use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;

use trs_bencode::{DecodeLimits, Value, decode_with_limits, raw_dict_get};

use crate::error::Error;
use crate::hash::InfoHash;
use crate::path::{MAX_PATH_BYTES, MAX_PATH_DEPTH, PathError, Repairs, sanitize_component};
use crate::warning::Warning;

/// Largest accepted piece length (256 MiB). Real torrents top out near 64 MiB;
/// the cap bounds the buffer a hostile torrent can make us allocate per piece.
pub const MAX_PIECE_LENGTH: u32 = 1 << 28;
/// Largest accepted file count. A torrent this big already needs ~100 MB to parse.
pub const MAX_FILES: usize = 1 << 20;
/// Cap on tracker URLs kept across all tiers.
pub const MAX_TRACKERS: usize = 256;
/// Cap on tiers kept.
pub const MAX_TIERS: usize = 64;
/// Cap on web-seed URLs kept.
pub const MAX_WEB_SEEDS: usize = 128;

/// Decode limits for `.torrent` files: 32 MiB and 2 Mi items cap transient
/// parse memory near 100 MB while still admitting ~250k-file torrents.
pub const TORRENT_LIMITS: DecodeLimits = DecodeLimits {
    max_items: 2 << 20,
    max_bytes: 32 << 20,
};

const FLAG_PADDING: u8 = 1;
const FLAG_EXECUTABLE: u8 = 2;
const FLAG_HIDDEN: u8 = 4;
const FLAG_SYMLINK: u8 = 8;

/// Which BitTorrent metadata format the torrent uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaVersion {
    /// BEP 3 only.
    V1,
    /// BEP 52 only (no `pieces`). Provisional support: files are listed, but
    /// pieces cannot be verified yet.
    V2,
    /// Both layouts in one info dictionary; the v1 layout is authoritative here.
    Hybrid,
}

/// One entry of the compact file table. size: 32 bytes. Paths live in a shared
/// arena so a 100k-file torrent costs one allocation for all path text.
#[derive(Debug, Clone, Copy)]
struct FileEntry {
    offset: u64,
    length: u64,
    path_off: u32,
    path_len: u32,
    flags: u8,
}

/// Borrowed view of one file in a [`Metainfo`].
#[derive(Debug, Clone, Copy)]
pub struct FileRef<'a> {
    /// Position in the torrent's file list.
    pub index: usize,
    /// Sanitized relative path, components joined by `/`. For multi-file
    /// torrents it excludes the root directory ([`Metainfo::name`]).
    pub path: &'a str,
    /// File size in bytes.
    pub length: u64,
    /// Byte offset of the file in the torrent's concatenated piece stream.
    pub offset: u64,
    flags: u8,
}

impl<'a> FileRef<'a> {
    /// The sanitized path components. None is empty, `.`, `..`, or contains `/`.
    pub fn components(&self) -> core::str::Split<'a, char> {
        self.path.split('/')
    }

    /// BEP 47 padding file: occupies piece-stream space but is never written to disk.
    pub fn is_padding(&self) -> bool {
        self.flags & FLAG_PADDING != 0
    }

    /// BEP 47 executable attribute.
    pub fn is_executable(&self) -> bool {
        self.flags & FLAG_EXECUTABLE != 0
    }

    /// BEP 47 hidden attribute.
    pub fn is_hidden(&self) -> bool {
        self.flags & FLAG_HIDDEN != 0
    }

    /// BEP 47 symlink entry. Never create a symlink from torrent data.
    pub fn is_symlink(&self) -> bool {
        self.flags & FLAG_SYMLINK != 0
    }
}

/// A parsed, validated, sanitized torrent.
///
/// Memory: the struct itself is 280 bytes on 64-bit targets (asserted by a
/// unit test). Heap use is dominated by `pieces` (20 B per piece) and the file
/// table (32 B per file plus path text). The decoded bencode tree is dropped
/// before `from_bytes` returns; the original bytes are *not* retained (see
/// [`Metainfo::info_range`] if you need them for BEP 9).
#[derive(Debug, Clone)]
pub struct Metainfo {
    /// v1 and/or v2 info-hash, computed over the original `info` bytes.
    pub info_hash: InfoHash,
    /// Metadata format.
    pub version: MetaVersion,
    /// Sanitized `name`: the file name (single-file) or root directory (multi-file).
    pub name: Box<str>,
    /// Bytes per piece (the last piece may be shorter).
    pub piece_length: u32,
    /// Size of the piece stream in bytes, padding included. For v2-only
    /// torrents files are piece-aligned, so this is the end of the last file.
    pub total_length: u64,
    /// Bytes of `total_length` that are not user data: BEP 47 padding files, or
    /// for v2-only torrents the gaps that piece alignment leaves between files.
    pub padding_length: u64,
    /// BEP 27 private flag: trackers only; no DHT, PEX or LSD.
    pub private: bool,
    /// BEP 12 tiers (fallback: single tier from `announce`), deduplicated and
    /// filtered to http(s)/udp.
    pub announce_tiers: Box<[Box<[Box<str>]>]>,
    /// BEP 19 web seeds (`url-list`), http(s) only.
    pub url_list: Box<[Box<str>]>,
    /// `info.source` (tracker-specific tag; part of the info-hash).
    pub source: Option<Box<str>>,
    /// Unix timestamp from `creation date`.
    pub creation_date: Option<i64>,
    /// Free-form `comment`.
    pub comment: Option<Box<str>>,
    /// `created by`.
    pub created_by: Option<Box<str>>,
    /// Byte range of the raw `info` dictionary inside the parsed buffer. Slice
    /// the original bytes with this to serve BEP 9 `ut_metadata` pieces.
    pub info_range: Range<usize>,
    /// Non-fatal oddities found while parsing.
    pub warnings: Vec<Warning>,
    multi_file: bool,
    pieces: Box<[u8]>,
    files: Box<[FileEntry]>,
    paths: Box<str>,
}

impl Metainfo {
    /// Parses a `.torrent` file with [`TORRENT_LIMITS`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Metainfo, Error> {
        Self::from_bytes_with_limits(bytes, TORRENT_LIMITS)
    }

    /// Parses a `.torrent` file with explicit decode limits.
    pub fn from_bytes_with_limits(bytes: &[u8], limits: DecodeLimits) -> Result<Metainfo, Error> {
        let root = decode_with_limits(bytes, limits)?;
        if root.as_dict().is_none() {
            return Err(Error::NotADictionary("torrent"));
        }
        let info = root.get(b"info").ok_or(Error::MissingKey("info"))?;
        if info.as_dict().is_none() {
            return Err(Error::NotADictionary("info"));
        }
        let raw = raw_dict_get(bytes, b"info")?.ok_or(Error::MissingKey("info"))?;
        // `raw` is a subslice of `bytes`, so the pointer difference is its offset.
        let start = (raw.as_ptr() as usize).saturating_sub(bytes.as_ptr() as usize);
        let mut m = parse_info(info, raw, start..start + raw.len())?;

        let mut warnings = Vec::new();
        m.announce_tiers = parse_trackers(&root, &mut warnings);
        m.url_list = parse_web_seeds(&root, &mut warnings);
        m.creation_date = root.get(b"creation date").and_then(Value::as_int);
        m.comment = lossy_opt(root.get(b"comment.utf-8").or_else(|| root.get(b"comment")));
        m.created_by = lossy_opt(root.get(b"created by"));
        m.warnings.extend(warnings);
        Ok(m)
    }

    /// Parses a bare `info` dictionary, as obtained through BEP 9 magnet metadata.
    /// Check it with [`InfoHash::verifies`] first. Trackers come from the magnet.
    pub fn from_info_bytes(info_bytes: &[u8]) -> Result<Metainfo, Error> {
        let info = decode_with_limits(info_bytes, TORRENT_LIMITS)?;
        if info.as_dict().is_none() {
            return Err(Error::NotADictionary("info"));
        }
        parse_info(&info, info_bytes, 0..info_bytes.len())
    }

    /// The raw `info` bytes inside the buffer this was parsed from.
    pub fn info_bytes<'a>(&self, original: &'a [u8]) -> Option<&'a [u8]> {
        original.get(self.info_range.clone())
    }

    /// True if the torrent has a root directory ([`Metainfo::name`]) holding its files.
    pub fn is_multi_file(&self) -> bool {
        self.multi_file
    }

    /// Number of files, padding files included.
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    fn file_ref(&self, index: usize, e: &FileEntry) -> FileRef<'_> {
        let start = e.path_off as usize;
        FileRef {
            index,
            // The table is built by us, so the range is always valid; `unwrap_or`
            // keeps this panic-free regardless.
            path: self
                .paths
                .get(start..start + e.path_len as usize)
                .unwrap_or(""),
            length: e.length,
            offset: e.offset,
            flags: e.flags,
        }
    }

    /// File `index`, if it exists.
    pub fn file(&self, index: usize) -> Option<FileRef<'_>> {
        Some(self.file_ref(index, self.files.get(index)?))
    }

    /// All files in piece-stream order.
    pub fn files(&self) -> impl ExactSizeIterator<Item = FileRef<'_>> + '_ {
        self.files
            .iter()
            .enumerate()
            .map(|(i, e)| self.file_ref(i, e))
    }

    /// Bytes the user actually receives: `total_length` minus padding.
    pub fn wanted_length(&self) -> u64 {
        self.total_length - self.padding_length
    }

    /// Number of v1 piece hashes (0 for v2-only torrents).
    pub fn piece_count(&self) -> usize {
        self.pieces.len() / 20
    }

    /// All v1 piece hashes concatenated (`20 * piece_count()` bytes).
    pub fn piece_hashes(&self) -> &[u8] {
        &self.pieces
    }

    /// SHA-1 of piece `index`.
    pub fn piece_hash(&self, index: usize) -> Option<&[u8; 20]> {
        let start = index.checked_mul(20)?;
        self.pieces
            .get(start..start.checked_add(20)?)?
            .try_into()
            .ok()
    }

    /// Length in bytes of piece `index` (the last piece may be short).
    pub fn piece_size(&self, index: usize) -> Option<u32> {
        if index >= self.piece_count() {
            return None;
        }
        let start = index as u64 * u64::from(self.piece_length);
        let left = self.total_length.checked_sub(start)?;
        Some(left.min(u64::from(self.piece_length)) as u32)
    }
}

fn lossy_opt(v: Option<&Value<'_>>) -> Option<Box<str>> {
    let s = String::from_utf8_lossy(v?.as_bytes()?);
    (!s.is_empty()).then(|| s.into_owned().into_boxed_str())
}

/// First of `keys` present in `dict` (used for the `*.utf-8` variants).
fn get_any<'v, 'a>(dict: &'v Value<'a>, keys: &[&[u8]]) -> Option<&'v Value<'a>> {
    keys.iter().find_map(|k| dict.get(k))
}

#[derive(Default)]
struct FileTable {
    entries: Vec<FileEntry>,
    arena: String,
    total: u64,
    padding: u64,
    data: u64,
    lossy: usize,
    colon: usize,
    symlinks: usize,
    /// V2 files start on piece boundaries; `Some(piece_length)` aligns offsets.
    align: Option<u64>,
}

impl FileTable {
    /// Gives distinct names to files whose paths differ from an earlier file's only by case or Unicode
    /// normalization: on a case-insensitive disk (APFS, NTFS) they would be one file, and the torrent could never
    /// complete. The later ones get " (2)", " (3)"… before the extension, chosen from the torrent alone so every
    /// system lays the torrent out the same way. Returns how many were renamed.
    fn disambiguate_folded(&mut self) -> usize {
        use unicode_normalization::UnicodeNormalization;
        let fold = |s: &str| s.to_lowercase().nfc().collect::<String>();
        let path = |t: &Self, e: &FileEntry| t.arena[e.path_off as usize..(e.path_off + e.path_len) as usize].to_owned();
        let real = |e: &FileEntry| e.flags & FLAG_PADDING == 0;
        let all: HashSet<String> = self.entries.iter().filter(|e| real(e)).map(|e| fold(&path(self, e))).collect();
        if all.len() == self.entries.iter().filter(|e| real(e)).count() {
            return 0;
        }
        let mut taken: HashSet<String> = HashSet::with_capacity(all.len());
        let mut renamed = 0;
        for i in 0..self.entries.len() {
            if !real(&self.entries[i]) {
                continue;
            }
            let p = path(self, &self.entries[i]);
            if taken.insert(fold(&p)) {
                continue;
            }
            let (dir, file) = p.rsplit_once('/').map_or(("", p.as_str()), |(d, f)| (d, f));
            let (stem, ext) = match file.rfind('.') {
                Some(dot) if dot > 0 => (&file[..dot], &file[dot..]),
                _ => (file, ""),
            };
            let at = |n: u32| {
                let name = format!("{stem} ({n}){ext}");
                if dir.is_empty() { name } else { format!("{dir}/{name}") }
            };
            let mut n = 2;
            while all.contains(&fold(&at(n))) || taken.contains(&fold(&at(n))) {
                n += 1;
            }
            let new = at(n);
            taken.insert(fold(&new));
            let e = &mut self.entries[i];
            e.path_off = self.arena.len() as u32;
            e.path_len = new.len() as u32;
            self.arena.push_str(&new);
            renamed += 1;
        }
        renamed
    }

    fn note(&mut self, r: Repairs) {
        self.lossy += usize::from(r.lossy);
        self.colon += usize::from(r.colon);
    }

    fn push(&mut self, comps: &[Cow<'_, str>], length: u64, flags: u8) -> Result<(), Error> {
        if self.entries.len() >= MAX_FILES {
            return Err(Error::TooManyFiles);
        }
        if comps.is_empty() {
            return Err(PathError::NoComponents.into());
        }
        if comps.len() > MAX_PATH_DEPTH {
            return Err(PathError::TooDeep.into());
        }
        let path_len = comps.iter().map(|c| c.len()).sum::<usize>() + comps.len() - 1;
        if path_len > MAX_PATH_BYTES {
            return Err(PathError::PathTooLong.into());
        }
        let path_off = u32::try_from(self.arena.len()).map_err(|_| Error::PathsTooLarge)?;
        for (i, c) in comps.iter().enumerate() {
            if i > 0 {
                self.arena.push('/');
            }
            self.arena.push_str(c);
        }
        if u32::try_from(self.arena.len()).is_err() {
            return Err(Error::PathsTooLarge);
        }
        let mut offset = self.total;
        if let (Some(a), true) = (self.align, length > 0) {
            offset = offset
                .div_ceil(a)
                .checked_mul(a)
                .ok_or(Error::LengthOverflow)?;
        }
        self.total = offset.checked_add(length).ok_or(Error::LengthOverflow)?;
        self.data = self.data.checked_add(length).ok_or(Error::LengthOverflow)?;
        if flags & FLAG_PADDING != 0 {
            self.padding += length;
        }
        if flags & FLAG_SYMLINK != 0 {
            self.symlinks += 1;
        }
        self.entries.push(FileEntry {
            offset,
            length,
            path_off,
            path_len: path_len as u32,
            flags,
        });
        Ok(())
    }
}

fn parse_attr(attr: Option<&Value<'_>>) -> u8 {
    let Some(a) = attr.and_then(Value::as_bytes) else {
        return 0;
    };
    let mut f = 0;
    for &c in a {
        f |= match c {
            b'p' => FLAG_PADDING,
            b'x' => FLAG_EXECUTABLE,
            b'h' => FLAG_HIDDEN,
            b'l' => FLAG_SYMLINK,
            _ => 0,
        };
    }
    f
}

fn length_of(v: Option<&Value<'_>>, key: &'static str) -> Result<u64, Error> {
    match v {
        None => Err(Error::MissingKey(key)),
        Some(Value::Int(n)) => u64::try_from(*n).map_err(|_| Error::NegativeLength),
        Some(_) => Err(Error::WrongType(key)),
    }
}

fn parse_info(info: &Value<'_>, raw: &[u8], info_range: Range<usize>) -> Result<Metainfo, Error> {
    let meta_version = match info.get(b"meta version") {
        None => 1,
        Some(Value::Int(v @ (1 | 2))) => *v,
        Some(Value::Int(v)) => return Err(Error::UnsupportedMetaVersion(*v)),
        Some(_) => return Err(Error::WrongType("meta version")),
    };

    let piece_length = match info.get(b"piece length") {
        None => return Err(Error::MissingKey("piece length")),
        Some(Value::Int(n)) => u32::try_from(*n)
            .ok()
            .filter(|n| (1..=MAX_PIECE_LENGTH).contains(n))
            .ok_or(Error::InvalidPieceLength)?,
        Some(_) => return Err(Error::WrongType("piece length")),
    };

    let name_raw = get_any(info, &[b"name.utf-8", b"name"])
        .ok_or(Error::MissingKey("name"))?
        .as_bytes()
        .ok_or(Error::WrongType("name"))?;
    let (name, name_repairs) = sanitize_component(name_raw)?;

    let pieces_v = info.get(b"pieces");
    let has_v1_files = info.get(b"files").is_some() || info.get(b"length").is_some();
    let tree = info.get(b"file tree");
    let (version, has_v1) = match (meta_version, pieces_v.is_some(), has_v1_files) {
        (1, _, _) => (MetaVersion::V1, true),
        (_, true, true) => (MetaVersion::Hybrid, true),
        (_, false, false) => (MetaVersion::V2, false),
        // v2 metadata with only half of the v1 layout is malformed.
        (_, false, true) => return Err(Error::MissingKey("pieces")),
        (_, true, false) => return Err(Error::MissingKey("length")),
    };
    if version != MetaVersion::V1 && tree.is_none() {
        return Err(Error::MissingKey("file tree"));
    }

    let mut table = FileTable {
        align: (!has_v1).then_some(u64::from(piece_length)),
        ..Default::default()
    };
    table.note(name_repairs);
    let multi_file = if has_v1 {
        parse_v1_files(info, &name, &mut table)?
    } else {
        let Some(tree) = tree else {
            return Err(Error::MissingKey("file tree"));
        };
        let dict = tree.as_dict().ok_or(Error::WrongType("file tree"))?;
        let mut stack = Vec::new();
        walk_tree(dict, &mut stack, &mut table)?;
        // A v2 single-file torrent is one file named like the torrent.
        !(table.entries.len() == 1 && table.arena == *name)
    };
    if table.entries.is_empty() || table.total == 0 {
        return Err(Error::EmptyTorrent);
    }
    if !has_v1 {
        // Alignment gaps carry no user data; count them as padding so that
        // `wanted_length` is the sum of the real file sizes.
        table.padding = table.total - table.data;
    }

    let pieces: Box<[u8]> = if has_v1 {
        let p = pieces_v
            .ok_or(Error::MissingKey("pieces"))?
            .as_bytes()
            .ok_or(Error::WrongType("pieces"))?;
        if p.len() % 20 != 0 {
            return Err(Error::BadPiecesLength);
        }
        let expected = table.total.div_ceil(u64::from(piece_length));
        let actual = (p.len() / 20) as u64;
        if expected != actual {
            return Err(Error::PieceCountMismatch { expected, actual });
        }
        p.into()
    } else {
        Box::default()
    };

    // Duplicate paths would let one file overwrite or alias another.
    let mut seen = HashSet::with_capacity(table.entries.len());
    for e in &table.entries {
        let s = &table.arena[e.path_off as usize..(e.path_off + e.path_len) as usize];
        if !seen.insert(s) {
            return Err(Error::DuplicatePath);
        }
    }
    drop(seen);
    let renamed = table.disambiguate_folded();

    let private = match info.get(b"private") {
        None => false,
        Some(Value::Int(n)) => *n != 0,
        Some(_) => return Err(Error::WrongType("private")),
    };

    let mut warnings = Vec::new();
    if table.lossy > 0 {
        warnings.push(Warning::LossyPathComponents(table.lossy));
    }
    if table.colon > 0 {
        warnings.push(Warning::ColonMappedComponents(table.colon));
    }
    if renamed > 0 {
        warnings.push(Warning::FoldedDuplicatesRenamed(renamed));
    }
    if table.symlinks > 0 {
        warnings.push(Warning::SymlinkEntries(table.symlinks));
    }
    if version != MetaVersion::V1 {
        warnings.push(Warning::V2PieceLayersIgnored);
    }

    Ok(Metainfo {
        info_hash: InfoHash::compute(raw, version != MetaVersion::V2, version != MetaVersion::V1),
        version,
        name: name.into_owned().into_boxed_str(),
        piece_length,
        total_length: table.total,
        padding_length: table.padding,
        private,
        announce_tiers: Box::default(),
        url_list: Box::default(),
        source: lossy_opt(info.get(b"source")),
        creation_date: None,
        comment: None,
        created_by: None,
        info_range,
        warnings,
        multi_file,
        pieces,
        files: table.entries.into_boxed_slice(),
        paths: table.arena.into_boxed_str(),
    })
}

/// Fills `table` from the v1 `length` / `files` layout; returns `is_multi_file`.
fn parse_v1_files(info: &Value<'_>, name: &str, table: &mut FileTable) -> Result<bool, Error> {
    match (info.get(b"files"), info.get(b"length")) {
        (Some(_), Some(_)) => Err(Error::AmbiguousLayout),
        (None, None) => Err(Error::MissingKey("length")),
        (None, Some(len)) => {
            let length = length_of(Some(len), "length")?;
            table.push(
                &[Cow::Borrowed(name)],
                length,
                parse_attr(info.get(b"attr")),
            )?;
            Ok(false)
        }
        (Some(files), None) => {
            let list = files.as_list().ok_or(Error::WrongType("files"))?;
            if list.len() > MAX_FILES {
                return Err(Error::TooManyFiles);
            }
            let mut comps: Vec<Cow<'_, str>> = Vec::new();
            for f in list {
                if f.as_dict().is_none() {
                    return Err(Error::NotADictionary("file entry"));
                }
                let length = length_of(f.get(b"length"), "length")?;
                let path = get_any(f, &[b"path.utf-8", b"path"])
                    .ok_or(Error::MissingKey("path"))?
                    .as_list()
                    .ok_or(Error::WrongType("path"))?;
                comps.clear();
                for c in path {
                    let raw = c.as_bytes().ok_or(Error::WrongType("path"))?;
                    let (s, r) = sanitize_component(raw)?;
                    table.note(r);
                    comps.push(s);
                }
                table.push(&comps, length, parse_attr(f.get(b"attr")))?;
            }
            Ok(true)
        }
    }
}

/// Depth-first walk of a BEP 52 `file tree`. Key `""` marks a leaf holding
/// `length`; every other key is a directory or file name.
fn walk_tree<'a>(
    dict: &[(&'a [u8], Value<'a>)],
    stack: &mut Vec<Cow<'a, str>>,
    table: &mut FileTable,
) -> Result<(), Error> {
    for (key, val) in dict {
        if key.is_empty() {
            // Leaf. A node that is a file cannot also have children.
            if dict.len() != 1 {
                return Err(Error::WrongType("file tree"));
            }
            let length = length_of(val.get(b"length"), "length")?;
            return table.push(stack, length, parse_attr(val.get(b"attr")));
        }
        let (comp, r) = sanitize_component(key)?;
        table.note(r);
        let child = val.as_dict().ok_or(Error::WrongType("file tree"))?;
        if stack.len() >= MAX_PATH_DEPTH {
            return Err(PathError::TooDeep.into());
        }
        stack.push(comp);
        walk_tree(child, stack, table)?;
        stack.pop();
    }
    Ok(())
}

/// True for the URL schemes a tracker URL may use. Case-insensitive, and the
/// remainder after `://` must be non-empty.
fn tracker_url_ok(url: &str) -> bool {
    has_scheme(url, &["http://", "https://", "udp://"])
}

fn web_seed_url_ok(url: &str) -> bool {
    has_scheme(url, &["http://", "https://"])
}

pub(crate) fn has_scheme(url: &str, schemes: &[&str]) -> bool {
    schemes.iter().any(|s| {
        url.len() > s.len()
            && url.is_char_boundary(s.len())
            && url[..s.len()].eq_ignore_ascii_case(s)
    })
}

/// Returns a usable tracker URL from raw bytes, or records why not.
fn clean_url(
    raw: &[u8],
    ok: fn(&str) -> bool,
    ignored: fn(Box<str>) -> Warning,
    warnings: &mut Vec<Warning>,
) -> Option<String> {
    let mut warn = |w: Warning| {
        // Bounded: a hostile torrent must not grow this list without limit.
        if warnings.len() < 64 {
            warnings.push(w);
        }
    };
    let Ok(text) = core::str::from_utf8(raw) else {
        warn(ignored(
            String::from_utf8_lossy(raw).into_owned().into_boxed_str(),
        ));
        return None;
    };
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    if !ok(t) {
        warn(ignored(t.into()));
        return None;
    }
    Some(t.to_owned())
}

/// Accumulates deduplicated URLs under a global cap.
struct UrlSet {
    seen: HashSet<String>,
    total: usize,
    capped: bool,
}

fn parse_trackers(root: &Value<'_>, warnings: &mut Vec<Warning>) -> Box<[Box<[Box<str>]>]> {
    let mut set = UrlSet {
        seen: HashSet::new(),
        total: 0,
        capped: false,
    };
    let mut tiers: Vec<Vec<Box<str>>> = Vec::new();

    let mut add = |set: &mut UrlSet, tier: &mut Vec<Box<str>>, raw: &[u8]| {
        let Some(url) = clean_url(raw, tracker_url_ok, Warning::IgnoredTracker, warnings) else {
            return;
        };
        if set.total >= MAX_TRACKERS {
            set.capped = true;
        } else if set.seen.insert(url.clone()) {
            set.total += 1;
            tier.push(url.into_boxed_str());
        }
    };

    if let Some(list) = root.get(b"announce-list").and_then(Value::as_list) {
        for t in list {
            if tiers.len() >= MAX_TIERS {
                set.capped = true;
                break;
            }
            let mut tier = Vec::new();
            match t {
                Value::List(urls) => {
                    for u in urls {
                        if let Some(b) = u.as_bytes() {
                            add(&mut set, &mut tier, b);
                        }
                    }
                }
                // Some generators emit a flat list of strings; treat each as its own tier entry.
                Value::Bytes(b) => add(&mut set, &mut tier, b),
                _ => {}
            }
            if !tier.is_empty() {
                tiers.push(tier);
            }
        }
    }
    if tiers.is_empty() {
        let mut tier = Vec::new();
        if let Some(b) = root.get(b"announce").and_then(Value::as_bytes) {
            add(&mut set, &mut tier, b);
        }
        if !tier.is_empty() {
            tiers.push(tier);
        }
    }
    if set.capped {
        warnings.push(Warning::TooManyUrls);
    }
    tiers.into_iter().map(Vec::into_boxed_slice).collect()
}

fn parse_web_seeds(root: &Value<'_>, warnings: &mut Vec<Warning>) -> Box<[Box<str>]> {
    let mut out: Vec<Box<str>> = Vec::new();
    let mut capped = false;
    let mut add = |raw: &[u8], warnings: &mut Vec<Warning>| {
        if let Some(url) = clean_url(raw, web_seed_url_ok, Warning::IgnoredWebSeed, warnings) {
            if out.len() >= MAX_WEB_SEEDS {
                capped = true;
            } else if !out.iter().any(|u| **u == *url) {
                out.push(url.into_boxed_str());
            }
        }
    };
    match root.get(b"url-list") {
        Some(Value::Bytes(b)) => add(b, warnings),
        Some(Value::List(l)) => {
            for v in l {
                if let Some(b) = v.as_bytes() {
                    add(b, warnings);
                }
            }
        }
        _ => {}
    }
    if capped {
        warnings.push(Warning::TooManyUrls);
    }
    out.into_boxed_slice()
}
