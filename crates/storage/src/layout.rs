//! Piece/block ↔ file mapping.
//!
//! The hot path ([`Layout::segments`]) is allocation-free: one binary search over the cumulative
//! file offsets, then a linear walk. Zero-length files own no bytes and are skipped by the
//! mapping (they are only created/moved by finalize). BEP 47 padding files are flagged so the
//! I/O layer never writes them and reads synthesise zeros.
//!
//! # Resident cost
//! ```text
//! size_of::<Layout>()                = 48 bytes
//! per file                           = 16 bytes (start offset + path end + flags) + path bytes
//! ```
//! A 1,000-file torrent with ~60-byte paths costs ≈ 76 KB; a 1-file torrent ≈ 150 bytes.
//! Cold torrents drop the whole thing ([`TorrentHandle::drop_layout`](crate::TorrentHandle)) and
//! keep only the 8-byte [`Layout::digest`].

use std::ffi::OsStr;
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

/// Standard BitTorrent block size.
pub const BLOCK_SIZE: u32 = 16 * 1024;
/// Hard cap on files per torrent (input comes from `.torrent` files).
pub const MAX_FILES: usize = 1 << 20;

/// Where one file lives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilePath {
    /// Sanitised components relative to the torrent's save root.
    Relative(Vec<String>),
    /// Per-file custom target (organize/placement); used as-is when the torrent is finalized.
    Absolute(PathBuf),
}

/// One file of the torrent, in torrent order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileSpec {
    /// Placement.
    pub path: FilePath,
    /// Length in bytes.
    pub length: u64,
    /// BEP 47 padding file: never written, reads as zeros.
    pub is_padding: bool,
}

/// Why a [`Layout`] could not be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayoutError {
    /// Piece length is zero.
    ZeroPieceLength,
    /// More than [`MAX_FILES`] files.
    TooManyFiles,
    /// Sum of file lengths differs from the declared total (or overflows).
    TotalMismatch,
    /// Declared piece count differs from `ceil(total / piece_length)`.
    PieceCountMismatch,
    /// A path component is empty, `.`/`..`, or contains `/` or NUL; or an absolute path is not
    /// absolute / contains `..`.
    BadPath(u32),
    /// Total path bytes exceed 4 GiB.
    PathsTooLarge,
}

impl fmt::Display for LayoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroPieceLength => write!(f, "piece length is zero"),
            Self::TooManyFiles => write!(f, "too many files"),
            Self::TotalMismatch => write!(f, "file lengths do not sum to the total length"),
            Self::PieceCountMismatch => write!(f, "piece count does not match total length"),
            Self::BadPath(i) => write!(f, "unsafe path for file #{i}"),
            Self::PathsTooLarge => write!(f, "paths too large"),
        }
    }
}

impl std::error::Error for LayoutError {}

const FLAG_ABSOLUTE: u8 = 1;
const FLAG_PADDING: u8 = 2;

// size: 16 bytes per file (+1 sentinel record per layout).
#[derive(Clone, Copy, Debug)]
struct FileRec {
    /// Offset of the file's first byte in the torrent's byte stream.
    start: u64,
    /// End of this file's path inside `Layout::paths`.
    path_end: u32,
    flags: u8,
}

/// Immutable mapping between the torrent byte stream, pieces and files.
// size: 48 bytes + 16 bytes/file + path bytes
#[derive(Clone, Debug)]
pub struct Layout {
    piece_length: u32,
    piece_count: u32,
    total_length: u64,
    /// `n + 1` records; the last is a sentinel with `start == total_length`.
    files: Box<[FileRec]>,
    /// Paths concatenated; relative components are joined with `/`.
    paths: Box<[u8]>,
}

/// Borrowed view of a file's configured location.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRef<'a> {
    /// `/`-joined relative path (safe to `Path::join` onto a root).
    Relative(&'a OsStr),
    /// Absolute custom target.
    Absolute(&'a OsStr),
}

/// A contiguous piece of a request that lies within one file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// File index.
    pub file: u32,
    /// Offset inside that file.
    pub file_offset: u64,
    /// Offset inside the caller's buffer / the request.
    pub data_offset: usize,
    /// Length in bytes (never zero).
    pub len: usize,
    /// True for BEP 47 padding: don't touch the disk.
    pub padding: bool,
}

/// Allocation-free iterator over the [`Segment`]s of a byte range.
#[derive(Clone, Debug)]
pub struct Segments<'a> {
    layout: &'a Layout,
    pos: u64,
    end: u64,
    idx: usize,
    data_off: usize,
}

impl Layout {
    /// Validates and builds a layout.
    ///
    /// `total_length` and `piece_count` are redundant with the files and piece length, and are
    /// checked against them so a corrupt resume record is caught here instead of at read time.
    pub fn new(
        piece_length: u32,
        total_length: u64,
        piece_count: u32,
        files: impl IntoIterator<Item = FileSpec>,
    ) -> Result<Self, LayoutError> {
        if piece_length == 0 {
            return Err(LayoutError::ZeroPieceLength);
        }
        let mut recs: Vec<FileRec> = Vec::new();
        let mut paths: Vec<u8> = Vec::new();
        let mut offset: u64 = 0;
        for (i, f) in files.into_iter().enumerate() {
            if i >= MAX_FILES {
                return Err(LayoutError::TooManyFiles);
            }
            let bad = || LayoutError::BadPath(i as u32);
            let mut flags = 0;
            match &f.path {
                FilePath::Relative(comps) => {
                    if comps.is_empty() {
                        return Err(bad());
                    }
                    for (k, c) in comps.iter().enumerate() {
                        let b = c.as_bytes();
                        if b.is_empty()
                            || b == b"."
                            || b == b".."
                            || b.contains(&b'/')
                            || b.contains(&0)
                        {
                            return Err(bad());
                        }
                        if k > 0 {
                            paths.push(b'/');
                        }
                        paths.extend_from_slice(b);
                    }
                }
                FilePath::Absolute(p) => {
                    let b = p.as_os_str().as_encoded_bytes();
                    if !p.is_absolute()
                        || b.contains(&0)
                        || p.components()
                            .any(|c| matches!(c, std::path::Component::ParentDir))
                    {
                        return Err(bad());
                    }
                    flags |= FLAG_ABSOLUTE;
                    paths.extend_from_slice(b);
                }
            }
            if f.is_padding {
                flags |= FLAG_PADDING;
            }
            let path_end = u32::try_from(paths.len()).map_err(|_| LayoutError::PathsTooLarge)?;
            recs.push(FileRec {
                start: offset,
                path_end,
                flags,
            });
            offset = offset
                .checked_add(f.length)
                .ok_or(LayoutError::TotalMismatch)?;
        }
        if offset != total_length {
            return Err(LayoutError::TotalMismatch);
        }
        if total_length.div_ceil(u64::from(piece_length)) != u64::from(piece_count) {
            return Err(LayoutError::PieceCountMismatch);
        }
        recs.push(FileRec {
            start: total_length,
            path_end: paths.len() as u32,
            flags: 0,
        });
        Ok(Self {
            piece_length,
            piece_count,
            total_length,
            files: recs.into_boxed_slice(),
            paths: paths.into_boxed_slice(),
        })
    }

    /// Nominal piece length.
    pub fn piece_length(&self) -> u32 {
        self.piece_length
    }

    /// Number of pieces.
    pub fn piece_count(&self) -> u32 {
        self.piece_count
    }

    /// Sum of all file lengths.
    pub fn total_length(&self) -> u64 {
        self.total_length
    }

    /// Number of files (including padding and empty files).
    pub fn file_count(&self) -> usize {
        self.files.len() - 1
    }

    /// Byte offset of the start of `piece` in the torrent stream.
    pub fn piece_offset(&self, piece: u32) -> u64 {
        u64::from(piece) * u64::from(self.piece_length)
    }

    /// Size of `piece` (the last one may be short); 0 when out of range.
    pub fn piece_size(&self, piece: u32) -> u32 {
        if piece >= self.piece_count {
            return 0;
        }
        let start = self.piece_offset(piece);
        (self.total_length - start).min(u64::from(self.piece_length)) as u32
    }

    /// Number of 16 KiB blocks in `piece`.
    pub fn block_count(&self, piece: u32) -> u32 {
        self.piece_size(piece).div_ceil(BLOCK_SIZE)
    }

    /// Start offset of file `i` in the torrent stream.
    pub fn file_start(&self, i: usize) -> u64 {
        self.files[i].start
    }

    /// Length of file `i`.
    pub fn file_len(&self, i: usize) -> u64 {
        self.files[i + 1].start - self.files[i].start
    }

    /// Whether file `i` is a BEP 47 padding file.
    pub fn file_is_padding(&self, i: usize) -> bool {
        self.files[i].flags & FLAG_PADDING != 0
    }

    /// Configured location of file `i`.
    pub fn file_path(&self, i: usize) -> PathRef<'_> {
        let begin = if i == 0 {
            0
        } else {
            self.files[i - 1].path_end as usize
        };
        // SAFETY: `paths` holds only whole `str`s, `/` and whole `as_encoded_bytes` of paths (see
        // `new`), cut at those boundaries: valid encoded bytes on every system.
        let s = unsafe { OsStr::from_encoded_bytes_unchecked(&self.paths[begin..self.files[i].path_end as usize]) };
        if self.files[i].flags & FLAG_ABSOLUTE != 0 {
            PathRef::Absolute(s)
        } else {
            PathRef::Relative(s)
        }
    }

    /// Index of the (non-empty) file containing byte `offset`.
    pub fn file_at(&self, offset: u64) -> Option<usize> {
        if offset >= self.total_length {
            return None;
        }
        // Last record with start <= offset. Zero-length files share a start with their successor
        // and sort before it, so the *last* match is always the non-empty one.
        Some(self.files.partition_point(|r| r.start <= offset) - 1)
    }

    /// Iterates the file segments of `len` bytes starting at torrent offset `offset`.
    ///
    /// Returns `None` if the range exceeds the torrent.
    pub fn segments(&self, offset: u64, len: usize) -> Option<Segments<'_>> {
        let end = offset.checked_add(len as u64)?;
        if end > self.total_length {
            return None;
        }
        let idx = if len == 0 { 0 } else { self.file_at(offset)? };
        Some(Segments {
            layout: self,
            pos: offset,
            end,
            idx,
            data_off: 0,
        })
    }

    /// Segments of `len` bytes at `begin` inside `piece`.
    pub fn piece_segments(&self, piece: u32, begin: u32, len: usize) -> Option<Segments<'_>> {
        let size = self.piece_size(piece);
        if u64::from(begin) + len as u64 > u64::from(size) {
            return None;
        }
        self.segments(self.piece_offset(piece) + u64::from(begin), len)
    }

    /// Torrent offset of byte `file_offset` in file `i`.
    pub fn to_stream_offset(&self, i: usize, file_offset: u64) -> u64 {
        self.files[i].start + file_offset
    }

    /// Pieces that contain any byte of file `i` (empty for zero-length files).
    pub fn pieces_of_file(&self, i: usize) -> Range<u32> {
        let len = self.file_len(i);
        if len == 0 {
            return 0..0;
        }
        let pl = u64::from(self.piece_length);
        let first = self.files[i].start / pl;
        let last = (self.files[i].start + len - 1) / pl;
        first as u32..(last + 1) as u32
    }

    /// Indices of the non-empty files overlapping `piece`.
    pub fn files_of_piece(&self, piece: u32) -> Range<usize> {
        let size = u64::from(self.piece_size(piece));
        if size == 0 {
            return 0..0;
        }
        let start = self.piece_offset(piece);
        let first = self.file_at(start).unwrap_or(0);
        let last = self.file_at(start + size - 1).unwrap_or(first);
        first..last + 1
    }

    /// Stable 64-bit fingerprint of the structure (offsets, flags, paths).
    ///
    /// Lets a Cold torrent drop its layout and verify a rebuilt one in 8 bytes of state.
    pub fn digest(&self) -> u64 {
        // FNV-1a: deterministic across runs/versions (unlike `DefaultHasher`).
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut feed = |b: &[u8]| {
            for &x in b {
                h = (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(&self.piece_length.to_le_bytes());
        feed(&self.total_length.to_le_bytes());
        for r in self.files.iter() {
            feed(&r.start.to_le_bytes());
            feed(&[r.flags]);
            feed(&r.path_end.to_le_bytes());
        }
        feed(&self.paths);
        h
    }

    /// Fingerprint of the *geometry* only: piece length, total length and every file's offset and
    /// padding flag. Paths (and whether they are relative or absolute) are not part of it, so a
    /// layout whose files were moved has the same value. Relocation journals and
    /// [`TorrentHandle::switch_layout`](crate::TorrentHandle::switch_layout) use it.
    pub fn structure_digest(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ 0x5a5a;
        let mut feed = |b: &[u8]| {
            for &x in b {
                h = (h ^ u64::from(x)).wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        feed(&self.piece_length.to_le_bytes());
        feed(&self.total_length.to_le_bytes());
        for r in self.files.iter() {
            feed(&r.start.to_le_bytes());
            feed(&[r.flags & FLAG_PADDING]);
        }
        h
    }

    /// A copy of this layout with new per-file paths (same geometry). `paths` must yield exactly
    /// one entry per file; padding files keep a path too (it is never touched).
    pub fn with_paths(
        &self,
        paths: impl IntoIterator<Item = FilePath>,
    ) -> Result<Layout, LayoutError> {
        let mut it = paths.into_iter();
        let mut specs = Vec::with_capacity(self.file_count());
        for i in 0..self.file_count() {
            let path = it.next().ok_or(LayoutError::BadPath(i as u32))?;
            specs.push(FileSpec {
                path,
                length: self.file_len(i),
                is_padding: self.file_is_padding(i),
            });
        }
        if it.next().is_some() {
            return Err(LayoutError::TooManyFiles);
        }
        Layout::new(self.piece_length, self.total_length, self.piece_count, specs)
    }

    /// Heap bytes owned by this layout (excluding the struct itself).
    pub fn heap_size(&self) -> usize {
        std::mem::size_of_val(&*self.files) + self.paths.len()
    }
}

impl Iterator for Segments<'_> {
    type Item = Segment;

    fn next(&mut self) -> Option<Segment> {
        if self.pos >= self.end {
            return None;
        }
        let files = &self.layout.files;
        // Skip finished and zero-length files.
        while files[self.idx + 1].start <= self.pos {
            self.idx += 1;
        }
        let rec = files[self.idx];
        let take = self.end.min(files[self.idx + 1].start) - self.pos;
        let seg = Segment {
            file: self.idx as u32,
            file_offset: self.pos - rec.start,
            data_offset: self.data_off,
            len: take as usize,
            padding: rec.flags & FLAG_PADDING != 0,
        };
        self.pos += take;
        self.data_off += take as usize;
        Some(seg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn spec(name: &str, len: u64, pad: bool) -> FileSpec {
        FileSpec {
            path: FilePath::Relative(vec![name.to_string()]),
            length: len,
            is_padding: pad,
        }
    }

    /// A Unix-style absolute test path made absolute on this system (a drive in front on Windows).
    fn abs(p: &str) -> String {
        if cfg!(windows) { format!("C:{p}") } else { p.to_string() }
    }

    fn build(piece_len: u32, files: Vec<FileSpec>) -> Layout {
        let total: u64 = files.iter().map(|f| f.length).sum();
        let count = total.div_ceil(u64::from(piece_len)) as u32;
        Layout::new(piece_len, total, count, files).unwrap()
    }

    #[test]
    fn basic_mapping_and_boundaries() {
        let l = build(
            10,
            vec![
                spec("a", 7, false),
                spec("b", 0, false),
                spec("c", 15, false),
            ],
        );
        assert_eq!(l.piece_count(), 3);
        assert_eq!(l.piece_size(2), 2);
        assert_eq!(l.piece_size(3), 0);
        let segs: Vec<_> = l.piece_segments(0, 0, 10).unwrap().collect();
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].file, segs[0].file_offset, segs[0].len), (0, 0, 7));
        assert_eq!(
            (
                segs[1].file,
                segs[1].file_offset,
                segs[1].len,
                segs[1].data_offset
            ),
            (2, 0, 3, 7)
        );
        assert_eq!(l.file_at(7), Some(2));
        assert_eq!(l.file_at(22), None);
        assert_eq!(l.pieces_of_file(2), 0..3);
        assert_eq!(l.pieces_of_file(1), 0..0);
        assert_eq!(l.files_of_piece(0), 0..3);
        assert!(l.piece_segments(2, 0, 3).is_none());
        assert!(l.segments(0, 23).is_none());
        assert_eq!(l.segments(22, 0).unwrap().count(), 0);
    }

    #[test]
    fn validation() {
        let ok = spec("a", 5, false);
        assert_eq!(
            Layout::new(0, 5, 1, [ok.clone()]).unwrap_err(),
            LayoutError::ZeroPieceLength
        );
        assert_eq!(
            Layout::new(4, 6, 2, [ok.clone()]).unwrap_err(),
            LayoutError::TotalMismatch
        );
        assert_eq!(
            Layout::new(4, 5, 3, [ok.clone()]).unwrap_err(),
            LayoutError::PieceCountMismatch
        );
        for bad in ["", ".", "..", "a/b", "a\0b"] {
            let f = FileSpec {
                path: FilePath::Relative(vec![bad.into()]),
                length: 1,
                is_padding: false,
            };
            assert_eq!(
                Layout::new(4, 1, 1, [f]).unwrap_err(),
                LayoutError::BadPath(0),
                "{bad:?}"
            );
        }
        let rel_abs = FileSpec {
            path: FilePath::Absolute("rel/x".into()),
            length: 1,
            is_padding: false,
        };
        assert!(Layout::new(4, 1, 1, [rel_abs]).is_err());
        let dotdot = FileSpec {
            path: FilePath::Absolute(abs("/a/../x").into()),
            length: 1,
            is_padding: false,
        };
        assert!(Layout::new(4, 1, 1, [dotdot]).is_err());
        let empty = Layout::new(4, 0, 0, std::iter::empty()).unwrap();
        assert_eq!(empty.file_count(), 0);
        assert!(empty.file_at(0).is_none());
    }

    #[test]
    fn paths_and_flags() {
        let files = vec![
            FileSpec {
                path: FilePath::Relative(vec!["dir".into(), "f.bin".into()]),
                length: 3,
                is_padding: false,
            },
            FileSpec {
                path: FilePath::Absolute(abs("/Volumes/X/lib/f2.bin").into()),
                length: 2,
                is_padding: false,
            },
            spec(".pad", 5, true),
        ];
        let l = build(4, files);
        assert_eq!(l.file_path(0), PathRef::Relative(OsStr::new("dir/f.bin")));
        assert_eq!(
            l.file_path(1),
            PathRef::Absolute(OsStr::new(&abs("/Volumes/X/lib/f2.bin")))
        );
        assert!(l.file_is_padding(2) && !l.file_is_padding(0));
        let segs: Vec<_> = l.segments(0, 10).unwrap().collect();
        assert!(segs[2].padding);
    }

    #[test]
    fn digest_detects_changes() {
        let a = build(8, vec![spec("a", 10, false)]);
        let b = build(8, vec![spec("b", 10, false)]);
        let c = build(8, vec![spec("a", 10, false)]);
        assert_ne!(a.digest(), b.digest());
        assert_eq!(a.digest(), c.digest());
    }

    fn arb_layout() -> impl Strategy<Value = Layout> {
        (
            1u32..200,
            prop::collection::vec((0u64..400, any::<bool>(), 0u8..4), 0..24),
        )
            .prop_map(|(pl, fs)| {
                let files = fs
                    .into_iter()
                    .enumerate()
                    .map(|(i, (len, pad, kind))| FileSpec {
                        path: if kind == 0 {
                            FilePath::Absolute(abs(&format!("/x/{i}")).into())
                        } else {
                            FilePath::Relative(vec![format!("f{i}")])
                        },
                        length: if kind == 1 { 0 } else { len },
                        is_padding: pad,
                    })
                    .collect();
                build(pl, files)
            })
    }

    proptest! {
        // Every range maps to segments that tile it exactly, in order, inside file bounds, and
        // map back to the same stream offsets.
        #[test]
        fn segments_tile_and_round_trip(l in arb_layout(), a in 0u64..10_000, b in 0usize..10_000) {
            let total = l.total_length();
            let off = if total == 0 { 0 } else { a % (total + 1) };
            let len = (b as u64 % (total - off + 1)) as usize;
            let mut covered = 0usize;
            let mut stream = off;
            for s in l.segments(off, len).unwrap() {
                prop_assert!(s.len > 0);
                prop_assert_eq!(s.data_offset, covered);
                prop_assert!(s.file_offset + s.len as u64 <= l.file_len(s.file as usize));
                prop_assert_eq!(l.to_stream_offset(s.file as usize, s.file_offset), stream);
                prop_assert_eq!(l.file_at(stream), Some(s.file as usize));
                prop_assert_eq!(s.padding, l.file_is_padding(s.file as usize));
                covered += s.len;
                stream += s.len as u64;
            }
            prop_assert_eq!(covered, len);
            prop_assert!(l.segments(off, len + (total - off) as usize + 1).is_none());
        }

        // Pieces tile the stream; piece segments equal stream segments; file<->piece relations agree.
        #[test]
        fn piece_relations(l in arb_layout()) {
            let mut sum = 0u64;
            for p in 0..l.piece_count() {
                let size = l.piece_size(p);
                prop_assert!(size > 0 && size <= l.piece_length());
                sum += u64::from(size);
                let direct: Vec<_> = l.piece_segments(p, 0, size as usize).unwrap().collect();
                let via: Vec<_> = l.segments(l.piece_offset(p), size as usize).unwrap().collect();
                prop_assert_eq!(&direct, &via);
                let fr = l.files_of_piece(p);
                prop_assert_eq!(direct.first().map(|s| s.file as usize), Some(fr.start));
                prop_assert_eq!(direct.last().map(|s| s.file as usize + 1), Some(fr.end));
                for s in &direct {
                    prop_assert!(l.pieces_of_file(s.file as usize).contains(&p));
                }
            }
            prop_assert_eq!(sum, l.total_length());
            for i in 0..l.file_count() {
                for p in l.pieces_of_file(i) {
                    prop_assert!(l.files_of_piece(p).contains(&i));
                }
            }
        }

        // Sub-block requests at any position inside a piece never leave it.
        #[test]
        fn block_requests(l in arb_layout(), p in 0u32..64, begin in 0u32..300, len in 0usize..300) {
            if l.piece_count() == 0 { return Ok(()); }
            let p = p % l.piece_count();
            let size = l.piece_size(p);
            let r = l.piece_segments(p, begin, len);
            if u64::from(begin) + len as u64 <= u64::from(size) {
                let n: usize = r.unwrap().map(|s| s.len).sum();
                prop_assert_eq!(n, len);
            } else {
                prop_assert!(r.is_none());
            }
        }
    }
}
