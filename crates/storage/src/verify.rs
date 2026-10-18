//! Resume validation, full recheck and throttled scrub.
//!
//! Recheck/scrub stream each piece through a small scratch buffer (256 KiB per worker, ≤ 8
//! workers), so memory in flight is ≤ 2 MiB regardless of piece size or torrent size. Files that
//! do not exist are *never* created: their pieces simply count as invalid and the file is listed
//! in the report.

use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::bitset::PieceSet;
use crate::disk::{STREAM_CHUNK, TorrentHandle, TorrentShared};
use crate::error::{Result, StorageError, is_not_found};
use crate::fsutil;
use crate::hash::Sha1Stream;
use crate::layout::Layout;
use crate::oneshot::Pending;

/// Cooperative cancellation shared between the caller and a long-running job.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    /// New, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests cancellation; the job stops within one chunk.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// Progress snapshot passed to the callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Progress {
    /// Pieces examined so far.
    pub pieces_done: u32,
    /// Pieces in the torrent.
    pub pieces_total: u32,
    /// Bytes read so far.
    pub bytes_done: u64,
}

/// Progress callback; invoked from worker threads (at most ~1000 times per run plus once at the end).
pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

/// Default scrub rate: gentle enough to be invisible on an SSD, 1 TB in ~17 h.
pub const DEFAULT_SCRUB_BYTES_PER_SEC: u64 = 16 << 20;
const MAX_WORKERS: usize = 8;
const MAX_LISTED_MISSING: usize = 256;

/// Parameters of [`TorrentHandle::recheck`] / [`TorrentHandle::scrub`].
#[derive(Clone)]
pub struct RecheckOptions {
    /// Flat 20-byte SHA-1 piece hashes (the caller loads them from the `.torrent`).
    pub hashes: Arc<[u8]>,
    /// Parallel readers/hashers (default 2, max 8).
    pub workers: usize,
    /// Read-rate limit in bytes/s; `None` = unthrottled.
    pub throttle_bytes_per_sec: Option<u64>,
    /// Cancellation.
    pub cancel: CancelToken,
    /// Optional progress callback.
    pub progress: Option<ProgressFn>,
    /// Lower the I/O priority of the worker threads (macOS `PRIO_DARWIN_BG`).
    pub background_io: bool,
}

impl RecheckOptions {
    /// Unthrottled, 2 workers, no callback.
    pub fn new(hashes: Arc<[u8]>) -> Self {
        Self {
            hashes,
            workers: 2,
            throttle_bytes_per_sec: None,
            cancel: CancelToken::new(),
            progress: None,
            background_io: false,
        }
    }
}

/// Outcome of a recheck/scrub.
#[derive(Clone, Debug)]
pub struct RecheckReport {
    /// Pieces whose hash matched.
    pub have: PieceSet,
    /// Number of matching pieces.
    pub valid: u32,
    /// Pieces that were read but did not match (or were short).
    pub invalid: u32,
    /// Pieces not examined because a file they need is missing.
    pub skipped_missing: u32,
    /// Up to 256 missing file indices.
    pub missing_files: Vec<u32>,
    /// Total number of missing files.
    pub missing_file_count: u32,
    /// Bytes read.
    pub bytes_read: u64,
}

/// Smooth pacing limiter: each acquire schedules the next slot, so reads are spread evenly
/// instead of bursting.
pub(crate) struct RateLimiter {
    bytes_per_sec: f64,
    next_free: Mutex<Instant>,
}

impl RateLimiter {
    pub(crate) fn new(rate: u64) -> Self {
        Self {
            bytes_per_sec: rate.max(1024) as f64,
            next_free: Mutex::new(Instant::now()),
        }
    }

    pub(crate) fn acquire(&self, bytes: usize, cancel: &CancelToken) {
        let wait = {
            let mut g = self.next_free.lock().unwrap_or_else(|p| p.into_inner());
            let now = Instant::now();
            let start = (*g).max(now);
            *g = start + Duration::from_secs_f64(bytes as f64 / self.bytes_per_sec);
            start - now
        };
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || cancel.is_cancelled() {
                return;
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }
}

fn atomic_words(n: u32) -> Vec<AtomicU64> {
    (0..(n as usize).div_ceil(64))
        .map(|_| AtomicU64::new(0))
        .collect()
}

fn atomic_set(w: &[AtomicU64], i: u32) {
    w[(i / 64) as usize].fetch_or(1 << (i % 64), Ordering::Relaxed);
}

fn atomic_get(w: &[AtomicU64], i: u32) -> bool {
    w[(i / 64) as usize].load(Ordering::Relaxed) >> (i % 64) & 1 == 1
}

/// Hashes one piece in chunks, pacing reads through `limiter`.
fn hash_piece_paced(
    t: &TorrentShared,
    layout: &Layout,
    piece: u32,
    scratch: &mut [u8],
    limiter: Option<&RateLimiter>,
    cancel: &CancelToken,
    bytes: &AtomicU64,
) -> Result<[u8; 20]> {
    let start = layout.piece_offset(piece);
    let size = layout.piece_size(piece) as usize;
    let mut h = Sha1Stream::new();
    let mut done = 0;
    while done < size {
        if cancel.is_cancelled() {
            return Err(StorageError::Cancelled);
        }
        let n = scratch.len().min(size - done);
        if let Some(l) = limiter {
            l.acquire(n, cancel);
        }
        t.read_into(layout, start + done as u64, &mut scratch[..n])?;
        h.update(&scratch[..n]);
        bytes.fetch_add(n as u64, Ordering::Relaxed);
        done += n;
    }
    Ok(h.finish())
}

fn run_recheck(
    t: &Arc<TorrentShared>,
    layout: &Arc<Layout>,
    o: RecheckOptions,
) -> Result<RecheckReport> {
    // Drop cached fds first: a replaced or deleted file would otherwise keep serving its old
    // (unlinked) inode and the recheck would verify data that is no longer on disk.
    t.disk.fds.invalidate_torrent(t.id);
    let n = layout.piece_count();
    let have = atomic_words(n);
    let missing = atomic_words(layout.file_count() as u32);
    let next = AtomicU32::new(0);
    let done = AtomicU32::new(0);
    let valid = AtomicU32::new(0);
    let invalid = AtomicU32::new(0);
    let skipped = AtomicU32::new(0);
    let bytes = AtomicU64::new(0);
    let failure: Mutex<Option<StorageError>> = Mutex::new(None);
    let limiter = o.throttle_bytes_per_sec.map(RateLimiter::new);
    let workers = o.workers.clamp(1, MAX_WORKERS);
    let report_every = (n / 1000).max(1);

    let report = |d: u32| {
        if let Some(cb) = &o.progress {
            cb(Progress {
                pieces_done: d,
                pieces_total: n,
                bytes_done: bytes.load(Ordering::Relaxed),
            });
        }
    };

    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                if o.background_io {
                    fsutil::set_background_io();
                }
                let mut scratch = vec![0u8; STREAM_CHUNK];
                loop {
                    if o.cancel.is_cancelled()
                        || failure.lock().unwrap_or_else(|p| p.into_inner()).is_some()
                    {
                        break;
                    }
                    let p = next.fetch_add(1, Ordering::Relaxed);
                    if p >= n {
                        break;
                    }
                    let blocked = layout.files_of_piece(p).any(|f| {
                        !layout.file_is_padding(f)
                            && layout.file_len(f) > 0
                            && atomic_get(&missing, f as u32)
                    });
                    if blocked {
                        skipped.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let a = p as usize * 20;
                        let expected = o.hashes.get(a..a + 20);
                        match hash_piece_paced(
                            t,
                            layout,
                            p,
                            &mut scratch,
                            limiter.as_ref(),
                            &o.cancel,
                            &bytes,
                        ) {
                            Ok(d) if Some(&d[..]) == expected => {
                                atomic_set(&have, p);
                                valid.fetch_add(1, Ordering::Relaxed);
                            }
                            Ok(_) | Err(StorageError::Truncated { .. }) => {
                                invalid.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(StorageError::MissingData { file }) => {
                                atomic_set(&missing, file);
                                skipped.fetch_add(1, Ordering::Relaxed);
                            }
                            Err(StorageError::Cancelled) => break,
                            Err(e) => {
                                failure
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .get_or_insert(e);
                                break;
                            }
                        }
                    }
                    let d = done.fetch_add(1, Ordering::Relaxed) + 1;
                    if d.is_multiple_of(report_every) {
                        report(d);
                    }
                }
            });
        }
    });

    if let Some(e) = failure.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return Err(e);
    }
    if o.cancel.is_cancelled() {
        return Err(StorageError::Cancelled);
    }
    report(done.load(Ordering::Relaxed));

    let missing_files: Vec<u32> = (0..layout.file_count() as u32)
        .filter(|&f| atomic_get(&missing, f))
        .collect();
    let have_set = PieceSet::from_words(have.iter().map(|w| w.load(Ordering::Relaxed)).collect(), n);
    t.replace_verified(&have_set);
    Ok(RecheckReport {
        have: have_set,
        valid: valid.load(Ordering::Relaxed),
        invalid: invalid.load(Ordering::Relaxed),
        skipped_missing: skipped.load(Ordering::Relaxed),
        missing_file_count: missing_files.len() as u32,
        missing_files: missing_files.into_iter().take(MAX_LISTED_MISSING).collect(),
        bytes_read: bytes.load(Ordering::Relaxed),
    })
}

/// Size and modification time of a file, stored when a torrent completes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileFingerprint {
    /// Length in bytes.
    pub size: u64,
    /// Seconds since the Unix epoch.
    pub mtime_secs: i64,
    /// Nanosecond part.
    pub mtime_nanos: u32,
}

impl FileFingerprint {
    /// Serialised size.
    pub const ENCODED_LEN: usize = 20;

    /// Fixed little-endian encoding for the resume store.
    pub fn to_bytes(&self) -> [u8; Self::ENCODED_LEN] {
        let mut b = [0; Self::ENCODED_LEN];
        b[..8].copy_from_slice(&self.size.to_le_bytes());
        b[8..16].copy_from_slice(&self.mtime_secs.to_le_bytes());
        b[16..].copy_from_slice(&self.mtime_nanos.to_le_bytes());
        b
    }

    /// Inverse of [`FileFingerprint::to_bytes`]; `None` unless exactly 20 bytes.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != Self::ENCODED_LEN {
            return None;
        }
        Some(Self {
            size: u64::from_le_bytes(b[..8].try_into().ok()?),
            mtime_secs: i64::from_le_bytes(b[8..16].try_into().ok()?),
            mtime_nanos: u32::from_le_bytes(b[16..].try_into().ok()?),
        })
    }
}

/// Result of comparing one file with its stored fingerprint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    /// Matches (or is a padding file).
    Ok,
    /// File does not exist.
    Missing,
    /// Exists with a different size (or not the size the layout requires).
    SizeMismatch,
    /// Same size, different modification time: recheck before serving.
    Modified,
    /// No stored fingerprint for a file that needs one.
    NoRecord,
}

/// Per-file results of a resume check.
#[derive(Clone, Debug)]
pub struct ResumeReport {
    /// One entry per layout file.
    pub status: Box<[FileStatus]>,
}

impl ResumeReport {
    /// True when every file matches: the torrent may resume without a rehash.
    pub fn is_clean(&self) -> bool {
        self.status.iter().all(|s| *s == FileStatus::Ok)
    }

    /// True when at least one file is missing (→ `Missing data`).
    pub fn has_missing(&self) -> bool {
        self.status.contains(&FileStatus::Missing)
    }
}

fn fingerprint_of(m: &std::fs::Metadata) -> FileFingerprint {
    FileFingerprint {
        size: m.len(),
        mtime_secs: m.mtime(),
        mtime_nanos: m.mtime_nsec() as u32,
    }
}

impl TorrentHandle {
    /// Full verification of every piece against `opts.hashes`, with bounded memory.
    pub fn recheck(&self, opts: RecheckOptions) -> Result<Pending<RecheckReport>> {
        let layout = self.layout()?;
        if opts.hashes.len() != layout.piece_count() as usize * 20 {
            return Err(StorageError::NoHashes);
        }
        let t = self.shared().clone();
        self.maint(move || run_recheck(&t, &layout, opts))
    }

    /// Like [`recheck`](Self::recheck) but paced (default 16 MiB/s) at background I/O priority.
    ///
    /// Occupies one maintenance thread for the whole run; schedule one scrub at a time.
    pub fn scrub(&self, mut opts: RecheckOptions) -> Result<Pending<RecheckReport>> {
        opts.throttle_bytes_per_sec
            .get_or_insert(DEFAULT_SCRUB_BYTES_PER_SEC);
        opts.background_io = true;
        opts.workers = opts.workers.min(2);
        self.recheck(opts)
    }

    /// Stats every file (`None` for padding or missing files) to build resume fingerprints.
    pub fn fingerprints(&self) -> Result<Pending<Vec<Option<FileFingerprint>>>> {
        let t = self.shared().clone();
        let layout = self.layout()?;
        self.maint(move || {
            let mut out = Vec::with_capacity(layout.file_count());
            for i in 0..layout.file_count() as u32 {
                if layout.file_is_padding(i as usize) {
                    out.push(None);
                    continue;
                }
                let (p, _) = t.resolve(&layout, i, t.location());
                match std::fs::metadata(&p) {
                    Ok(m) => out.push(Some(fingerprint_of(&m))),
                    Err(e) if is_not_found(&e) => out.push(None),
                    Err(e) => return Err(StorageError::from_io(i, e)),
                }
            }
            Ok(out)
        })
    }

    /// Compares files on disk with fingerprints stored earlier (size + mtime, SPEC §10).
    ///
    /// Only meaningful for complete torrents: files of an unfinished download change constantly.
    pub fn check_files(
        &self,
        stored: Vec<Option<FileFingerprint>>,
    ) -> Result<Pending<ResumeReport>> {
        let t = self.shared().clone();
        let layout = self.layout()?;
        self.maint(move || {
            let mut status = Vec::with_capacity(layout.file_count());
            for i in 0..layout.file_count() {
                if layout.file_is_padding(i) {
                    status.push(FileStatus::Ok);
                    continue;
                }
                let (p, _) = t.resolve(&layout, i as u32, t.location());
                let m = match std::fs::metadata(&p) {
                    Ok(m) => m,
                    Err(e) if is_not_found(&e) => {
                        status.push(FileStatus::Missing);
                        continue;
                    }
                    Err(e) => return Err(StorageError::from_io(i as u32, e)),
                };
                let fp = fingerprint_of(&m);
                status.push(match stored.get(i).copied().flatten() {
                    _ if fp.size != layout.file_len(i) => FileStatus::SizeMismatch,
                    None => FileStatus::NoRecord,
                    Some(s) if s.size != fp.size => FileStatus::SizeMismatch,
                    Some(s) if s != fp => FileStatus::Modified,
                    Some(_) => FileStatus::Ok,
                });
            }
            Ok(ResumeReport {
                status: status.into_boxed_slice(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_codec() {
        let f = FileFingerprint {
            size: 1 << 40,
            mtime_secs: -5,
            mtime_nanos: 999,
        };
        assert_eq!(FileFingerprint::from_bytes(&f.to_bytes()), Some(f));
        assert_eq!(FileFingerprint::from_bytes(&[0; 3]), None);
    }

    #[test]
    fn limiter_paces() {
        let l = RateLimiter::new(1 << 20);
        let c = CancelToken::new();
        let t0 = Instant::now();
        for _ in 0..4 {
            l.acquire(64 << 10, &c); // 256 KiB total at 1 MiB/s ≈ 190 ms after the first free slot
        }
        assert!(
            t0.elapsed() >= Duration::from_millis(150),
            "{:?}",
            t0.elapsed()
        );
        c.cancel();
        let t1 = Instant::now();
        l.acquire(100 << 20, &c);
        assert!(t1.elapsed() < Duration::from_millis(100));
    }
}
