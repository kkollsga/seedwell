//! Moving a torrent's files to new paths after a location change (SPEC §7A.5), crash-safely.
//!
//! The engine drives three phases, so that the source is removed only after the engine's own record
//! points at the destination:
//!
//! ```text
//! relocate_prepare   same volume: rename_excl per file.  other volume: streaming copy, hashed
//!                    against the piece hashes, fsynced, published with rename_excl.  Sources of a
//!                    copy are NOT touched.           journal: Plan Started (Publishing Done)* Prepared
//! -- engine: quiesce, switch_layout / set_paths, persist its record --
//! relocate_mark_switched                                                       journal: Switched
//! relocate_commit    delete the sources (copy), prune emptied directories, drop the journal
//! relocate_rollback  (instead of the switch) undo: delete our copies / rename files back
//! relocate_recover   after a crash: which of the above states was reached
//! ```
//!
//! # Journal (`TRSR`, v1)
//! `TRSR | version | structure digest (u64) | file count (u32)`, then records. The first is the
//! **Plan** (`tag 1 | len u32 | payload | fnv32`): method, every `(file, src, dst)` and the
//! directories the move is about to create (topmost missing ancestors). It is fsynced, then
//! `Started` is fsynced, and only then does the first filesystem change happen: a journal without
//! `Started` means nothing was touched. After the plan come 5-byte records `(tag, u32)`:
//! `Started`, `Publishing(file)` (copy only, fsynced *before* the rename that publishes the file),
//! `Done(file)`, `Prepared` (fsynced), `Switched` (fsynced), `SourcesGone`. A torn trailing record is
//! ignored. The journal identifies the torrent by [`Layout::structure_digest`], so it stays valid
//! across the path change.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crossbeam_channel as cb;

use crate::bitset::PieceSet;
use crate::disk::{TorrentHandle, TorrentShared};
use crate::error::{Result, StorageError, is_not_found};
use crate::finalize::FaultHook;
use crate::fsutil;
use crate::hash::Sha1Stream;
use crate::layout::{Layout, MAX_FILES};
use crate::oneshot::Pending;
use crate::verify::{CancelToken, RateLimiter};

const MAGIC: &[u8; 4] = b"TRSR";
const VERSION: u8 = 1;
const HEADER_LEN: usize = 17;

const T_PLAN: u8 = 1;
const T_STARTED: u8 = 2;
const T_PUBLISHING: u8 = 3;
const T_DONE: u8 = 4;
const T_PREPARED: u8 = 5;
const T_SWITCHED: u8 = 6;
const T_GONE: u8 = 7;

const S_PUBLISHING: u8 = 1;
const S_DONE: u8 = 2;

/// Default copy buffer (three are in flight: reader, queue, writer).
pub const DEFAULT_COPY_BUFFER: usize = 4 << 20;
/// Largest accepted copy buffer.
pub const MAX_COPY_BUFFER: usize = 8 << 20;
/// Default free-space reserve kept on the destination volume (SPEC §7A.5: largest + 1 GB).
pub const DEFAULT_RESERVE: u64 = 1 << 30;
const NBUF: usize = 3; // plus one buffer being filled by the reader
const MAX_PATH_BYTES: usize = 8192;
const READ_RETRY_WAIT: Duration = Duration::from_secs(5);

/// Progress callback: `(bytes processed, bytes total)`; called from the worker thread, at most
/// about every 100 ms plus once at the end.
pub type RelocateProgressFn = Arc<dyn Fn(u64, u64) + Send + Sync>;

/// How the files were moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelocateMethod {
    /// `rename_excl` per file (same volume). Sources are gone after `relocate_prepare`.
    Renamed,
    /// Streaming copy (different volume, or `force_copy`). Sources stay until `relocate_commit`.
    Copied,
}

/// Parameters of [`TorrentHandle::relocate_prepare`].
#[derive(Clone)]
pub struct RelocateOptions {
    /// Flat 20-byte SHA-1 piece hashes; when set, every piece is verified as it passes the copy.
    pub hashes: Option<Arc<[u8]>>,
    /// For a torrent that is still downloading: only pieces marked here are verified (the others
    /// are copied as they are, all-zero blocks become holes), and sources of files that were never
    /// created are skipped instead of being an error. `None` = the torrent is complete.
    pub have: Option<PieceSet>,
    /// Copy buffer size (clamped to 64 KiB..=8 MiB; default 4 MiB).
    pub buffer_bytes: usize,
    /// `F_NOCACHE` on source and destination descriptors (default true).
    pub nocache: bool,
    /// Copy rate limit in bytes/s, 0 = unlimited.
    pub bandwidth: u64,
    /// Use the copy path even when source and destination are on the same volume (tests).
    pub force_copy: bool,
    /// `fsync` copied files and directories (default true).
    pub fsync: bool,
    /// Free space that must remain on the destination volume after the copy (default 1 GiB).
    pub reserve_bytes: u64,
    /// Cancellation; the copy stops within one buffer and cleans up.
    pub cancel: CancelToken,
    /// Progress callback.
    pub progress: Option<RelocateProgressFn>,
    /// Crash simulation for tests: called with a step number before every filesystem mutation;
    /// `false` stops the run with [`StorageError::Interrupted`] and leaves everything as it is.
    pub fault: Option<FaultHook>,
}

impl Default for RelocateOptions {
    fn default() -> Self {
        Self {
            hashes: None,
            have: None,
            buffer_bytes: DEFAULT_COPY_BUFFER,
            nocache: true,
            bandwidth: 0,
            force_copy: false,
            fsync: true,
            reserve_bytes: DEFAULT_RESERVE,
            cancel: CancelToken::new(),
            progress: None,
            fault: None,
        }
    }
}

/// One planned file move (as recorded in the journal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelocateEntry {
    /// File index in the layout.
    pub file: u32,
    /// Where the file was.
    pub src: PathBuf,
    /// Where it goes.
    pub dst: PathBuf,
    /// The source did not exist and the file is empty: an empty file is created at `dst`.
    pub create_empty: bool,
}

/// Result of [`TorrentHandle::relocate_prepare`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelocateReport {
    /// What was done.
    pub method: RelocateMethod,
    /// Files moved or created at the destination.
    pub files: u32,
    /// Files whose destination equals their current path.
    pub unchanged: u32,
    /// Sources that did not exist (partial download): nothing moved, the destination is created
    /// lazily by later writes.
    pub missing_skipped: u32,
    /// Bytes copied (0 for renames).
    pub bytes: u64,
    /// Pieces verified while copying.
    pub verified_pieces: u32,
}

/// What a journal found after a crash says about the move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoverState {
    /// No journal, or one without a started plan: nothing was changed (a stale journal is removed).
    Nothing,
    /// Started, not finished: the engine's record still points at the old paths. Roll back.
    Incomplete,
    /// All files are at the destination and verified; the switch may or may not have been
    /// persisted. The engine compares its record: at `dst` → commit (or `mark_switched` + commit),
    /// at `src` → roll back, or redo the switch and commit.
    Prepared,
    /// The engine had switched; the sources may still exist. Commit.
    Switched,
    /// Sources deleted; only the journal remains. Commit removes it.
    Done,
}

/// Parsed journal returned by [`TorrentHandle::relocate_recover`].
#[derive(Clone, Debug)]
pub struct Recovered {
    /// State reached.
    pub state: RecoverState,
    /// Method of the plan (when there is one).
    pub method: Option<RelocateMethod>,
    /// The plan: compare with the engine's record.
    pub entries: Vec<RelocateEntry>,
}

// ---------------------------------------------------------------------------------------------
// Journal
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Parsed {
    method: Option<RelocateMethod>,
    entries: Vec<RelocateEntry>,
    dirs: Vec<PathBuf>,
    started: bool,
    prepared: bool,
    switched: bool,
    gone: bool,
    /// Per file index: `S_PUBLISHING` | `S_DONE` bits.
    state: Vec<u8>,
}

fn fnv32(b: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &x in b {
        h = (h ^ u32::from(x)).wrapping_mul(0x0100_0193);
    }
    h
}

fn header(count: usize, digest: u64) -> [u8; HEADER_LEN] {
    let mut h = [0u8; HEADER_LEN];
    h[..4].copy_from_slice(MAGIC);
    h[4] = VERSION;
    h[5..13].copy_from_slice(&digest.to_le_bytes());
    h[13..].copy_from_slice(&(count as u32).to_le_bytes());
    h
}

fn put_path(out: &mut Vec<u8>, p: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let b = p.as_os_str().as_bytes();
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn encode_plan(method: RelocateMethod, entries: &[RelocateEntry], dirs: &[PathBuf]) -> Vec<u8> {
    let mut p = vec![match method {
        RelocateMethod::Renamed => 1,
        RelocateMethod::Copied => 2,
    }];
    p.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    p.extend_from_slice(&(dirs.len() as u32).to_le_bytes());
    for e in entries {
        p.extend_from_slice(&e.file.to_le_bytes());
        p.push(u8::from(e.create_empty));
        put_path(&mut p, &e.src);
        put_path(&mut p, &e.dst);
    }
    for d in dirs {
        put_path(&mut p, d);
    }
    let mut out = vec![T_PLAN];
    out.extend_from_slice(&(p.len() as u32).to_le_bytes());
    out.extend_from_slice(&p);
    out.extend_from_slice(&fnv32(&p).to_le_bytes());
    out
}

struct Rd<'a>(&'a [u8]);

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (a, b) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn path(&mut self) -> Option<PathBuf> {
        use std::os::unix::ffi::OsStrExt;
        let n = self.u32()? as usize;
        if n > MAX_PATH_BYTES {
            return None;
        }
        Some(PathBuf::from(std::ffi::OsStr::from_bytes(self.take(n)?)))
    }
}

fn decode_plan(payload: &[u8], count: usize, p: &mut Parsed) -> Option<()> {
    let mut r = Rd(payload);
    let method = match r.u8()? {
        1 => RelocateMethod::Renamed,
        2 => RelocateMethod::Copied,
        _ => return None,
    };
    let ne = r.u32()? as usize;
    let nd = r.u32()? as usize;
    if ne > MAX_FILES || nd > MAX_FILES {
        return None;
    }
    for _ in 0..ne {
        let file = r.u32()?;
        let flags = r.u8()?;
        let src = r.path()?;
        let dst = r.path()?;
        if file as usize >= count {
            return None;
        }
        p.entries.push(RelocateEntry {
            file,
            src,
            dst,
            create_empty: flags & 1 != 0,
        });
    }
    for _ in 0..nd {
        p.dirs.push(r.path()?);
    }
    p.method = Some(method);
    Some(())
}

/// Reads and parses a journal. `Ok(None)`: no file, or too short to hold a header.
fn read_journal(path: &Path, count: usize, digest: u64) -> Result<Option<Parsed>> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if is_not_found(&e) => return Ok(None),
        Err(e) => return Err(StorageError::io(e)),
    };
    if bytes.len() < HEADER_LEN {
        return Ok(None);
    }
    if bytes[..HEADER_LEN] != header(count, digest) {
        return Err(StorageError::CorruptJournal);
    }
    let mut p = Parsed {
        state: vec![0; count],
        ..Parsed::default()
    };
    let mut rest = &bytes[HEADER_LEN..];
    while let Some((&tag, tail)) = rest.split_first() {
        if tag == T_PLAN {
            if p.method.is_some() {
                return Err(StorageError::CorruptJournal);
            }
            let mut r = Rd(tail);
            let Some(len) = r.u32() else { break };
            let len = len as usize;
            let (Some(payload), Some(sum)) = (r.take(len.min(r.0.len())), r.u32()) else {
                break; // torn plan: nothing was started
            };
            if payload.len() != len || sum != fnv32(payload) {
                break;
            }
            decode_plan(payload, count, &mut p).ok_or(StorageError::CorruptJournal)?;
            rest = r.0;
            continue;
        }
        if p.method.is_none() || tail.len() < 4 {
            if p.method.is_none() {
                return Err(StorageError::CorruptJournal);
            }
            break; // torn trailing record
        }
        let arg = u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]);
        match tag {
            T_STARTED => p.started = true,
            T_PUBLISHING | T_DONE => {
                let Some(s) = p.state.get_mut(arg as usize) else {
                    return Err(StorageError::CorruptJournal);
                };
                *s |= if tag == T_DONE { S_DONE } else { S_PUBLISHING };
            }
            T_PREPARED => p.prepared = true,
            T_SWITCHED => p.switched = true,
            T_GONE => p.gone = true,
            _ => return Err(StorageError::CorruptJournal),
        }
        rest = &tail[4..];
    }
    Ok(Some(p))
}

struct Jw {
    file: File,
    path: PathBuf,
}

impl Jw {
    /// New journal (truncating a leftover one that never got a plan) with header only.
    fn create(path: &Path, count: usize, digest: u64) -> Result<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty() {
                fsutil::create_dirs_guarded(parent).map_err(StorageError::io)?;
            }
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .map_err(StorageError::io)?;
        file.write_all(&header(count, digest))
            .map_err(StorageError::io)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    fn append(path: &Path) -> Result<Self> {
        let file = OpenOptions::new()
            .append(true)
            .open(path)
            .map_err(StorageError::io)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    fn write_sync(&mut self, bytes: &[u8]) -> Result<()> {
        self.file.write_all(bytes).map_err(StorageError::io)?;
        self.file.sync_data().map_err(StorageError::io)
    }

    fn plan(&mut self, bytes: &[u8]) -> Result<()> {
        self.write_sync(bytes)?;
        sync_parent(&self.path);
        Ok(())
    }

    fn rec(&mut self, tag: u8, arg: u32, sync: bool) -> Result<()> {
        let mut r = [0u8; 5];
        r[0] = tag;
        r[1..].copy_from_slice(&arg.to_le_bytes());
        // One write(2) on an O_APPEND fd: the record is there or it is not.
        if sync {
            self.write_sync(&r)
        } else {
            self.file.write_all(&r).map_err(StorageError::io)
        }
    }
}

fn sync_dir(p: &Path) {
    if let Ok(d) = File::open(p) {
        let _ = d.sync_all();
    }
}

fn sync_parent(p: &Path) {
    if let Some(d) = p.parent()
        && !d.as_os_str().is_empty() {
            sync_dir(d);
        }
}

fn exists(p: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

fn remove_if_exists(p: &Path) -> io::Result<bool> {
    match fs::remove_file(p) {
        Ok(()) => Ok(true),
        Err(e) if is_not_found(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

fn part_path(dst: &Path) -> PathBuf {
    let mut s = dst.as_os_str().to_os_string();
    s.push(".trs-part");
    PathBuf::from(s)
}

/// Removes empty directories below `dir`, and `dir` itself if it ends up empty. Finder's litter
/// (`.DS_Store`, `._*`) does not count and goes with its folder; anything else keeps it.
fn prune_empty_tree(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    let mut empty = true;
    for e in rd.flatten() {
        let p = e.path();
        let ft = e.file_type();
        if ft.as_ref().is_ok_and(|t| t.is_dir()) && prune_empty_tree(&p) {
            continue;
        }
        if ft.is_ok_and(|t| t.is_file()) && crate::fsutil::is_finder_litter(&e.file_name()) {
            continue;
        }
        empty = false;
    }
    empty && fsutil::remove_dir_if_empty(dir)
}

/// Removes `dir` and its empty ancestors, stopping below `stop_at` (which is never removed).
fn prune_up(dir: &Path, stop_at: &Path) {
    let mut d = dir.to_path_buf();
    while d != stop_at && d.starts_with(stop_at) {
        if !fsutil::remove_dir_if_empty(&d) || !d.pop() {
            break;
        }
    }
}

struct Ctx {
    step: AtomicU32,
    fault: Option<FaultHook>,
}

impl Ctx {
    fn step(&self) -> Result<()> {
        let n = self.step.fetch_add(1, Ordering::Relaxed);
        match &self.fault {
            Some(f) if !f(n) => Err(StorageError::Interrupted),
            _ => Ok(()),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Piece verification while copying
// ---------------------------------------------------------------------------------------------

enum Feed<'a> {
    Data(&'a [u8]),
    Zeros,
    Skip,
}

struct Verifier<'a> {
    layout: &'a Layout,
    hashes: &'a [u8],
    have: Option<&'a PieceSet>,
    piece: u32,
    filled: u64,
    h: Sha1Stream,
    verified: u32,
}

static ZEROS: [u8; 64 * 1024] = [0; 64 * 1024];

impl<'a> Verifier<'a> {
    fn new(layout: &'a Layout, hashes: &'a [u8], have: Option<&'a PieceSet>) -> Self {
        Self {
            layout,
            hashes,
            have,
            piece: 0,
            filled: 0,
            h: Sha1Stream::new(),
            verified: 0,
        }
    }

    fn want(&self, p: u32) -> bool {
        self.have.is_none_or(|h| h.get(p))
    }

    /// Advances the stream position by `len` bytes. `Err(piece)` = hash mismatch.
    fn advance(&mut self, mut len: u64, mut feed: Feed<'_>) -> std::result::Result<(), u32> {
        while len > 0 && self.piece < self.layout.piece_count() {
            let psize = u64::from(self.layout.piece_size(self.piece));
            let take = len.min(psize - self.filled);
            let want = self.want(self.piece);
            if want {
                match &mut feed {
                    Feed::Data(d) => {
                        let (a, b) = d.split_at(take as usize);
                        self.h.update(a);
                        *d = b;
                    }
                    Feed::Zeros => {
                        let mut left = take as usize;
                        while left > 0 {
                            let n = left.min(ZEROS.len());
                            self.h.update(&ZEROS[..n]);
                            left -= n;
                        }
                    }
                    // Data for a piece we must verify is not there: cannot be vouched for.
                    Feed::Skip => return Err(self.piece),
                }
            } else if let Feed::Data(d) = &mut feed {
                *d = &d[take as usize..];
            }
            self.filled += take;
            len -= take;
            if self.filled == psize {
                if want {
                    let got = std::mem::take(&mut self.h).finish();
                    let a = self.piece as usize * 20;
                    if self.hashes.get(a..a + 20) != Some(&got[..]) {
                        return Err(self.piece);
                    }
                    self.verified += 1;
                }
                self.piece += 1;
                self.filled = 0;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Prepare
// ---------------------------------------------------------------------------------------------

/// What the reader does with one file of the stream.
enum Job {
    /// Padding: zeros for the hasher, nothing on disk.
    Padding(u64),
    /// Not on disk (partial download) or empty: advance only.
    Nothing(u64),
    /// Read `path`; copy into entry `entry` (if any), hash if verifying.
    Read {
        file: u32,
        path: PathBuf,
        len: u64,
        entry: Option<usize>,
    },
}

/// Set when a downstream stage (writer or hash) has exited. Buffers that were in flight inside
/// a stage that died are lost, so the reader would wait for a free buffer forever while the
/// surviving stage waits for its next item; the reader polls this flag and gives up instead.
#[derive(Default)]
struct StageDown(std::sync::atomic::AtomicBool);

struct StageDownGuard<'a>(&'a StageDown);

impl StageDown {
    /// Marks the stage down when dropped (return, error or panic).
    fn guard(&self) -> StageDownGuard<'_> {
        StageDownGuard(self)
    }

    fn is_down(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

impl Drop for StageDownGuard<'_> {
    fn drop(&mut self) {
        (self.0).0.store(true, std::sync::atomic::Ordering::Release);
    }
}

enum Msg {
    Open {
        /// The source file: its extended attributes (Finder tags, comments), ACLs and mode follow
        /// the data to the destination.
        src: PathBuf,
        part: PathBuf,
        mode: u32,
        mtime: Option<SystemTime>,
        len: u64,
    },
    Data {
        buf: Vec<u8>,
        n: usize,
        off: u64,
    },
    Close,
}

fn is_zero(b: &[u8]) -> bool {
    b.iter().all(|&x| x == 0)
}

#[allow(clippy::too_many_arguments)]
fn writer(
    rx: cb::Receiver<Msg>,
    free: cb::Sender<Vec<u8>>,
    ctx: &Ctx,
    sparse: bool,
    nocache: bool,
    fsync: bool,
    stage_down: &StageDown,
) -> Result<()> {
    let _down = stage_down.guard();
    struct Cur {
        f: File,
        src: PathBuf,
        len: u64,
        mtime: Option<SystemTime>,
    }
    let mut cur: Option<Cur> = None;
    while let Ok(m) = rx.recv() {
        match m {
            Msg::Open {
                src,
                part,
                mode,
                mtime,
                len,
            } => {
                ctx.step()?;
                let f = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(mode & 0o7777 | 0o600)
                    .open(&part)
                    .map_err(StorageError::io)?;
                if nocache {
                    fsutil::set_nocache(&f);
                }
                cur = Some(Cur { f, src, len, mtime });
            }
            Msg::Data { buf, n, off } => {
                ctx.step()?;
                if let Some(c) = &cur
                    && !(sparse && is_zero(&buf[..n])) {
                        c.f.write_all_at(&buf[..n], off).map_err(StorageError::io)?;
                    }
                let _ = free.send(buf);
            }
            Msg::Close => {
                ctx.step()?;
                if let Some(c) = cur.take() {
                    c.f.set_len(c.len).map_err(StorageError::io)?;
                    // Extended attributes (Finder tags and comments), ACLs and mode, before the
                    // fsync and before the rename that publishes the file. A source that cannot be
                    // reopened or a refused attribute never fails the move: the bytes are verified.
                    if let Ok(sf) = File::open(&c.src) {
                        let _ = fsutil::copy_metadata(&sf, &c.f);
                    }
                    if fsync {
                        c.f.sync_all().map_err(StorageError::io)?;
                    }
                    // Keep the mtime so stored fingerprints stay valid after the move.
                    if let Some(m) = c.mtime {
                        let _ = c.f.set_modified(m);
                    }
                }
            }
        }
    }
    Ok(())
}

struct CopyOut {
    bytes: u64,
    verified: u32,
}

/// What the reader hands to the hash stage, in stream order.
enum Item {
    /// Padding file: zeros for the hasher.
    Pad(u64),
    /// Bytes that are not on disk (partial download): advance only.
    Skip(u64),
    /// Start of a file that is being copied.
    Open(Msg),
    /// A chunk; `copy` false = read only to be hashed (the file stays where it is).
    Data {
        buf: Vec<u8>,
        n: usize,
        off: u64,
        copy: bool,
    },
    /// End of the file being copied.
    Close,
}

/// Stage 2: verifies pieces as the stream passes, then forwards copy chunks to the writer. Hashing
/// runs on its own thread so reading, hashing and writing overlap.
fn hash_stage(
    rx: cb::Receiver<Item>,
    tx: cb::Sender<Msg>,
    free: cb::Sender<Vec<u8>>,
    mut verifier: Option<Verifier<'_>>,
    stage_down: &StageDown,
) -> Result<u32> {
    let _down = stage_down.guard();
    let fail = |piece| StorageError::VerifyFailed { piece };
    let broken = || StorageError::io(io::Error::other("copy pipeline broke"));
    while let Ok(it) = rx.recv() {
        match it {
            Item::Pad(len) => {
                if let Some(v) = verifier.as_mut() {
                    v.advance(len, Feed::Zeros).map_err(fail)?;
                }
            }
            Item::Skip(len) => {
                if let Some(v) = verifier.as_mut() {
                    v.advance(len, Feed::Skip).map_err(fail)?;
                }
            }
            Item::Open(m) => tx.send(m).map_err(|_| broken())?,
            Item::Data { buf, n, off, copy } => {
                if let Some(v) = verifier.as_mut() {
                    v.advance(n as u64, Feed::Data(&buf[..n])).map_err(fail)?;
                }
                if copy {
                    tx.send(Msg::Data { buf, n, off }).map_err(|_| broken())?;
                } else {
                    let _ = free.send(buf);
                }
            }
            Item::Close => tx.send(Msg::Close).map_err(|_| broken())?,
        }
    }
    Ok(verifier.map_or(0, |v| v.verified))
}

fn copy_all(
    layout: &Layout,
    jobs: &[Job],
    entries: &[RelocateEntry],
    opts: &RelocateOptions,
    ctx: &Ctx,
    total: u64,
) -> Result<CopyOut> {
    let buf_len = opts.buffer_bytes.clamp(64 << 10, MAX_COPY_BUFFER);
    let limiter = (opts.bandwidth > 0).then(|| RateLimiter::new(opts.bandwidth));
    let verifier = opts
        .hashes
        .as_deref()
        .map(|h| Verifier::new(layout, h, opts.have.as_ref()));
    let (item_tx, item_rx) = cb::bounded::<Item>(NBUF);
    let (msg_tx, msg_rx) = cb::bounded::<Msg>(NBUF);
    let (free_tx, free_rx) = cb::bounded::<Vec<u8>>(NBUF + 1);
    for _ in 0..=NBUF {
        let _ = free_tx.send(vec![0u8; buf_len]);
    }
    let sparse = opts.have.is_some();
    let (nocache, fsync) = (opts.nocache, opts.fsync);
    let mut done_bytes = 0u64;
    let mut copied = 0u64;
    let mut last_report = Instant::now();

    let down = StageDown::default();
    let down = &down;
    let (reader_err, broken, hash_res, writer_res) = std::thread::scope(|s| {
        let free2 = free_tx.clone();
        let w = s.spawn(move || writer(msg_rx, free_tx, ctx, sparse, nocache, fsync, down));
        let h = s.spawn(move || hash_stage(item_rx, msg_tx, free2, verifier, down));
        let mut broken = false;
        let r: Result<()> = (|| {
            macro_rules! send {
                ($it:expr) => {
                    if item_tx.send($it).is_err() {
                        broken = true;
                        return Ok(());
                    }
                };
            }
            for job in jobs {
                let (file, path, len, entry) = match job {
                    Job::Padding(len) => {
                        send!(Item::Pad(*len));
                        continue;
                    }
                    Job::Nothing(len) => {
                        send!(Item::Skip(*len));
                        continue;
                    }
                    Job::Read {
                        file,
                        path,
                        len,
                        entry,
                    } => (*file, path, *len, *entry),
                };
                let src = File::open(path).map_err(|e| StorageError::from_io(file, e))?;
                if opts.nocache {
                    fsutil::set_nocache(&src);
                }
                if let Some(ei) = entry {
                    let meta = src.metadata().map_err(|e| StorageError::from_io(file, e))?;
                    send!(Item::Open(Msg::Open {
                        src: path.clone(),
                        part: part_path(&entries[ei].dst),
                        mode: meta.mode(),
                        mtime: meta.modified().ok(),
                        len,
                    }));
                }
                let mut off = 0u64;
                while off < len {
                    if opts.cancel.is_cancelled() {
                        return Err(StorageError::Cancelled);
                    }
                    let n = (len - off).min(buf_len as u64) as usize;
                    let mut buf = loop {
                        match free_rx.recv_timeout(Duration::from_millis(100)) {
                            Ok(b) => break b,
                            Err(cb::RecvTimeoutError::Timeout) => {
                                if opts.cancel.is_cancelled() {
                                    return Err(StorageError::Cancelled);
                                }
                                if down.is_down() {
                                    broken = true;
                                    return Ok(());
                                }
                            }
                            Err(cb::RecvTimeoutError::Disconnected) => {
                                broken = true;
                                return Ok(());
                            }
                        }
                    };
                    if buf.len() < n {
                        buf.resize(n, 0);
                    }
                    src.read_exact_at(&mut buf[..n], off)
                        .map_err(|e| StorageError::from_io(file, e))?;
                    if let Some(l) = &limiter {
                        l.acquire(n, &opts.cancel);
                    }
                    send!(Item::Data {
                        buf,
                        n,
                        off,
                        copy: entry.is_some(),
                    });
                    if entry.is_some() {
                        copied += n as u64;
                    }
                    off += n as u64;
                    done_bytes += n as u64;
                    if let Some(p) = &opts.progress
                        && last_report.elapsed() >= Duration::from_millis(100) {
                            last_report = Instant::now();
                            p(done_bytes, total);
                        }
                }
                if entry.is_some() {
                    send!(Item::Close);
                }
            }
            Ok(())
        })();
        drop(item_tx);
        let died = || StorageError::io(io::Error::other("copy stage panicked"));
        let hr = h.join().unwrap_or_else(|_| Err(died()));
        let wr = w.join().unwrap_or_else(|_| Err(died()));
        (r, broken, hr, wr)
    });

    // Root cause first: the reader's own error, then a verification failure, then the writer
    // (whose failure makes the hash stage see a closed pipeline), then the hash stage.
    if !broken {
        reader_err?;
    }
    if let Err(e @ StorageError::VerifyFailed { .. }) = hash_res {
        return Err(e);
    }
    writer_res?;
    let verified = hash_res?;
    if broken {
        return Err(StorageError::io(io::Error::other("copy pipeline broke")));
    }
    if opts.cancel.is_cancelled() {
        return Err(StorageError::Cancelled);
    }
    if let Some(p) = &opts.progress {
        p(done_bytes, total);
    }
    Ok(CopyOut {
        bytes: copied,
        verified,
    })
}

/// Topmost missing ancestor of `parent` (the directory a `create_dir_all` would create first).
fn topmost_missing(parent: &Path) -> Option<PathBuf> {
    let mut top = None;
    for a in parent.ancestors() {
        if a.as_os_str().is_empty() || a.exists() {
            break;
        }
        top = Some(a.to_path_buf());
    }
    top
}

fn has_parent_dir(p: &Path) -> bool {
    p.components().any(|c| matches!(c, Component::ParentDir))
}

struct Prepared {
    parsed: Parsed,
    jobs: Vec<Job>,
    unchanged: u32,
    missing_skipped: u32,
    total_bytes: u64,
    /// (path, mtime-less) sources to re-check after the copy: (path, len, mtime).
    src_stamp: Vec<(PathBuf, u64, Option<SystemTime>)>,
}

/// Can a folder be changed (entries created in it or removed from it)? `access(W_OK)` also
/// reports a locked folder (`uchg`) and a read-only volume.
fn dir_writable(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
    // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
    unsafe { libc::access(c.as_ptr(), libc::W_OK) == 0 }
}

/// The lock flags that stop a file from being renamed or removed (Finder "Locked" is `uchg`).
#[cfg(target_os = "macos")]
fn locked_flags(m: &fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt as _;
    const LOCKED: u32 = libc::UF_IMMUTABLE | libc::UF_APPEND | libc::SF_IMMUTABLE | libc::SF_APPEND;
    m.st_flags() & LOCKED != 0
}
#[cfg(not(target_os = "macos"))]
fn locked_flags(_: &fs::Metadata) -> bool {
    false
}

/// The cheap check before a move touches anything: no source file is locked, the folders the
/// sources leave can be changed, and so can the nearest existing folder of every destination.
/// Pairs are `(file index, source, destination)`; a source that is not there is skipped (the
/// plan decides about missing data). Stats and `access` only; no file is opened or changed.
pub fn preflight_move<'a>(pairs: impl IntoIterator<Item = (u32, &'a Path, &'a Path)>) -> Result<()> {
    let mut checked: HashSet<PathBuf> = HashSet::new();
    let mut dir_ok = |d: &Path| -> bool { !checked.insert(d.to_path_buf()) || dir_writable(d) };
    for (file, src, dst) in pairs {
        if let Ok(m) = fs::symlink_metadata(src) {
            if locked_flags(&m) {
                return Err(StorageError::Locked { file, path: src.to_path_buf() });
            }
            if let Some(p) = src.parent()
                && !dir_ok(p) {
                    return Err(StorageError::NoPermission { path: p.to_path_buf() });
                }
        }
        // The nearest folder that exists above the destination is the one that gets a new entry.
        if let Some(top) = dst.parent().and_then(|p| p.ancestors().find(|a| a.is_dir()))
            && !dir_ok(top) {
                return Err(StorageError::NoPermission { path: top.to_path_buf() });
            }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn plan(
    t: &TorrentShared,
    layout: &Layout,
    dst: &[Option<PathBuf>],
    opts: &RelocateOptions,
) -> Result<Prepared> {
    if dst.len() != layout.file_count() {
        return Err(StorageError::OutOfRange);
    }
    if let Some(h) = &opts.hashes
        && h.len() != layout.piece_count() as usize * 20 {
            return Err(StorageError::NoHashes);
        }
    let mut entries: Vec<RelocateEntry> = Vec::new();
    let mut jobs: Vec<Job> = Vec::with_capacity(layout.file_count());
    let mut unchanged = 0u32;
    let mut missing_skipped = 0u32;
    let mut seen: HashSet<&Path> = HashSet::new();
    let mut src_stamp = Vec::new();
    let hashing = opts.hashes.is_some();

    #[allow(clippy::needless_range_loop)]
    for i in 0..layout.file_count() {
        let len = layout.file_len(i);
        let idx = i as u32;
        if layout.file_is_padding(i) {
            jobs.push(Job::Padding(len));
            continue;
        }
        let (src, _) = t.resolve(layout, idx, t.location());
        if let Some(d) = &dst[i]
            && (!d.is_absolute() || has_parent_dir(d)) {
                return Err(StorageError::Layout(crate::layout::LayoutError::BadPath(idx)));
            }
        let meta = match fs::symlink_metadata(&src) {
            Ok(m) => Some(m),
            Err(e) if is_not_found(&e) => None,
            Err(e) => return Err(StorageError::from_io(idx, e)),
        };
        let d = match &dst[i] {
            Some(d) if *d != src => d,
            other => {
                if other.is_some() {
                    unchanged += 1;
                }
                // Stays where it is. Still read (not copied) so a piece spanning moved and
                // unmoved files verifies.
                if hashing && len > 0 {
                    if meta.is_none() {
                        let needed = opts
                            .have
                            .as_ref()
                            .is_none_or(|h| layout.pieces_of_file(i).any(|p| h.get(p)));
                        if needed {
                            return Err(StorageError::MissingData { file: idx });
                        }
                        jobs.push(Job::Nothing(len));
                    } else {
                        jobs.push(Job::Read {
                            file: idx,
                            path: src,
                            len,
                            entry: None,
                        });
                    }
                } else {
                    jobs.push(Job::Nothing(len));
                }
                continue;
            }
        };
        match meta {
            Some(m) => {
                if !m.is_file() {
                    return Err(StorageError::MissingData { file: idx });
                }
                if m.len() < len {
                    return Err(StorageError::Truncated { file: idx });
                }
                src_stamp.push((src.clone(), m.len(), m.modified().ok()));
                entries.push(RelocateEntry {
                    file: idx,
                    src: src.clone(),
                    dst: d.clone(),
                    create_empty: false,
                });
                jobs.push(Job::Read {
                    file: idx,
                    path: src,
                    len,
                    entry: Some(entries.len() - 1),
                });
            }
            None if len == 0 => {
                entries.push(RelocateEntry {
                    file: idx,
                    src,
                    dst: d.clone(),
                    create_empty: true,
                });
                jobs.push(Job::Nothing(0));
            }
            None if opts.have.is_some() => {
                // A partial download that never wrote this file. Fine unless a piece we vouch
                // for needs it.
                if let Some(h) = &opts.have
                    && layout.pieces_of_file(i).any(|p| h.get(p)) {
                        return Err(StorageError::MissingData { file: idx });
                    }
                missing_skipped += 1;
                jobs.push(Job::Nothing(len));
            }
            None => return Err(StorageError::MissingData { file: idx }),
        }
    }

    // Destinations: distinct, absent, and their volume mounted.
    for e in &entries {
        if !seen.insert(e.dst.as_path()) {
            return Err(StorageError::DestinationExists { file: e.file });
        }
        if exists(&e.dst).map_err(|er| StorageError::from_io(e.file, er))? {
            return Err(StorageError::DestinationExists { file: e.file });
        }
        if let Some(parent) = e.dst.parent()
            && let Some(mp) = fsutil::volume_mount_point(parent)
                && !mp.is_dir() {
                    return Err(StorageError::MissingRoot);
                }
    }

    // Before anything is touched: locked files and folders that cannot be changed.
    preflight_move(entries.iter().map(|e| (e.file, e.src.as_path(), e.dst.as_path())))?;

    let moves: Vec<&RelocateEntry> = entries.iter().filter(|e| !e.create_empty).collect();
    let mut cross = opts.force_copy && !moves.is_empty();
    if !cross {
        for e in &moves {
            let sd = fsutil::dev_of_nearest_existing(&e.src)
                .map_err(|er| StorageError::from_io(e.file, er))?;
            let parent = e.dst.parent().unwrap_or(&e.dst);
            let dd = fsutil::dev_of_nearest_existing(parent)
                .map_err(|er| StorageError::from_io(e.file, er))?;
            if sd != dd {
                cross = true;
                break;
            }
        }
    }
    let method = if cross {
        RelocateMethod::Copied
    } else {
        RelocateMethod::Renamed
    };

    // Free space per destination volume.
    let mut total_bytes = 0u64;
    if cross {
        let mut per_dev: Vec<(u64, u64, PathBuf)> = Vec::new();
        for e in &moves {
            let len = layout.file_len(e.file as usize);
            total_bytes += len;
            let parent = e.dst.parent().unwrap_or(&e.dst).to_path_buf();
            let dev = fsutil::dev_of_nearest_existing(&parent).map_err(StorageError::io)?;
            match per_dev.iter_mut().find(|x| x.0 == dev) {
                Some(x) => x.1 += len,
                None => per_dev.push((dev, len, parent)),
            }
        }
        for (_, need, sample) in per_dev {
            let needed = need.saturating_add(opts.reserve_bytes);
            let available = fsutil::available_space(&sample).map_err(StorageError::io)?;
            if available < needed {
                return Err(StorageError::InsufficientSpace { needed, available });
            }
        }
        // Hash-only reads count towards progress too.
        total_bytes += jobs
            .iter()
            .map(|j| match j {
                Job::Read {
                    len, entry: None, ..
                } => *len,
                _ => 0,
            })
            .sum::<u64>();
    }

    // Directories we are about to create (topmost missing ancestors, deduplicated).
    let mut dirs: Vec<PathBuf> = Vec::new();
    for e in &entries {
        if let Some(top) = e.dst.parent().and_then(topmost_missing)
            && !dirs.iter().any(|d| top.starts_with(d)) {
                dirs.retain(|d| !d.starts_with(&top));
                dirs.push(top);
            }
    }

    let n = layout.file_count();
    Ok(Prepared {
        parsed: Parsed {
            method: Some(method),
            entries,
            dirs,
            state: vec![0; n],
            ..Parsed::default()
        },
        jobs,
        unchanged,
        missing_skipped,
        total_bytes,
        src_stamp,
    })
}

fn run_prepare(
    t: &Arc<TorrentShared>,
    layout: &Arc<Layout>,
    journal_path: &Path,
    dst: &[Option<PathBuf>],
    opts: &RelocateOptions,
) -> Result<RelocateReport> {
    let count = layout.file_count();
    let digest = layout.structure_digest();
    // A journal that holds a plan belongs to an unfinished move: recover it first.
    if let Some(p) = read_journal(journal_path, count, digest)?
        && p.method.is_some() {
            return Err(StorageError::InvalidState);
        }
    let Prepared {
        mut parsed,
        jobs,
        unchanged,
        missing_skipped,
        total_bytes,
        src_stamp,
    } = plan(t, layout, dst, opts)?;
    let method = parsed.method.unwrap_or(RelocateMethod::Renamed);
    let ctx = Ctx {
        step: AtomicU32::new(0),
        fault: opts.fault.clone(),
    };

    if parsed.entries.is_empty() {
        // Nothing to move (all unchanged or nothing on disk yet).
        return Ok(RelocateReport {
            method,
            files: 0,
            unchanged,
            missing_skipped,
            bytes: 0,
            verified_pieces: 0,
        });
    }

    ctx.step()?;
    let mut jw = Jw::create(journal_path, count, digest)?;
    jw.plan(&encode_plan(method, &parsed.entries, &parsed.dirs))?;
    ctx.step()?;
    jw.rec(T_STARTED, 0, true)?;
    parsed.started = true;

    let run = (|| -> Result<(u64, u32)> {
        for d in &parsed.dirs {
            ctx.step()?;
            fsutil::create_dirs_guarded(d).map_err(|e| {
                if is_not_found(&e) {
                    StorageError::MissingRoot
                } else {
                    StorageError::io(e)
                }
            })?;
        }
        // Parents deeper than the topmost created dir (and pre-existing ones) may still be missing.
        for e in &parsed.entries {
            if let Some(p) = e.dst.parent() {
                ctx.step()?;
                fsutil::create_dirs_guarded(p).map_err(|er| StorageError::from_io(e.file, er))?;
            }
        }
        match method {
            RelocateMethod::Renamed => {
                t.relocating.store(true, Ordering::Release);
                for e in &parsed.entries {
                    ctx.step()?;
                    if e.create_empty {
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&e.dst)
                            .map_err(|er| map_exists(e.file, er))?;
                    } else {
                        fsutil::rename_excl(&e.src, &e.dst).map_err(|er| map_exists(e.file, er))?;
                    }
                    jw.rec(T_DONE, e.file, false)?;
                    if let Some(s) = parsed.state.get_mut(e.file as usize) {
                        *s |= S_DONE;
                    }
                }
                Ok((0, 0))
            }
            RelocateMethod::Copied => {
                for e in &parsed.entries {
                    if !e.create_empty {
                        remove_if_exists(&part_path(&e.dst))
                            .map_err(|er| StorageError::from_io(e.file, er))?;
                    }
                }
                let out = copy_all(layout, &jobs, &parsed.entries, opts, &ctx, total_bytes)?;
                // Sources must be what we read: a file changed under us is not "verified".
                for (p, len, mtime) in &src_stamp {
                    let m = fs::metadata(p).map_err(StorageError::io)?;
                    if m.len() != *len || m.modified().ok() != *mtime {
                        return Err(StorageError::io(io::Error::other(
                            "source file changed while copying",
                        )));
                    }
                }
                // Publish: everything is written, fsynced and verified.
                for e in &parsed.entries {
                    ctx.step()?;
                    if e.create_empty {
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .open(&e.dst)
                            .map_err(|er| map_exists(e.file, er))?;
                    } else {
                        jw.rec(T_PUBLISHING, e.file, true)?;
                        if let Some(s) = parsed.state.get_mut(e.file as usize) {
                            *s |= S_PUBLISHING;
                        }
                        ctx.step()?;
                        fsutil::rename_excl(&part_path(&e.dst), &e.dst)
                            .map_err(|er| map_exists(e.file, er))?;
                    }
                    jw.rec(T_DONE, e.file, false)?;
                    if let Some(s) = parsed.state.get_mut(e.file as usize) {
                        *s |= S_DONE;
                    }
                }
                if opts.fsync {
                    let mut dirs: Vec<&Path> =
                        parsed.entries.iter().filter_map(|e| e.dst.parent()).collect();
                    dirs.sort();
                    dirs.dedup();
                    for d in dirs {
                        sync_dir(d);
                    }
                }
                Ok((out.bytes, out.verified))
            }
        }
    })();

    match run {
        Ok((bytes, verified)) => {
            ctx.step().inspect_err(|_| t.relocating.store(false, Ordering::Release))?;
            jw.rec(T_PREPARED, 0, true)?;
            Ok(RelocateReport {
                method,
                files: parsed.entries.len() as u32,
                unchanged,
                missing_skipped,
                bytes,
                verified_pieces: verified,
            })
        }
        Err(StorageError::Interrupted) => {
            // Simulated crash: leave the disk as it is.
            t.relocating.store(false, Ordering::Release);
            Err(StorageError::Interrupted)
        }
        Err(e) => {
            // Undo what this call did; the source is untouched (copy) or renamed back (rename).
            let _ = rollback_parsed(t, &parsed, journal_path);
            Err(e)
        }
    }
}

fn map_exists(file: u32, e: io::Error) -> StorageError {
    if e.kind() == io::ErrorKind::AlreadyExists {
        StorageError::DestinationExists { file }
    } else {
        StorageError::from_io(file, e)
    }
}

// ---------------------------------------------------------------------------------------------
// Rollback, commit, recover
// ---------------------------------------------------------------------------------------------

fn rollback_parsed(t: &TorrentShared, p: &Parsed, journal_path: &Path) -> Result<()> {
    let method = p.method.unwrap_or(RelocateMethod::Renamed);
    if method == RelocateMethod::Renamed {
        t.relocating.store(true, Ordering::Release);
    }
    let r = (|| -> Result<()> {
        for e in p.entries.iter().rev() {
            let io = |er: io::Error| StorageError::from_io(e.file, er);
            if e.create_empty {
                // Ours: the precheck guarantees the destination did not exist before.
                if let Ok(m) = fs::symlink_metadata(&e.dst)
                    && m.is_file() && m.len() == 0 {
                        remove_if_exists(&e.dst).map_err(io)?;
                    }
                continue;
            }
            match method {
                RelocateMethod::Copied => {
                    let existed = remove_if_exists(&part_path(&e.dst)).map_err(io)?;
                    let st = p.state.get(e.file as usize).copied().unwrap_or(0);
                    // The part is gone only if the publishing rename happened: then dst is ours.
                    if !existed && st & (S_PUBLISHING | S_DONE) != 0 {
                        remove_if_exists(&e.dst).map_err(io)?;
                    }
                }
                RelocateMethod::Renamed => {
                    if !exists(&e.src).map_err(io)? && exists(&e.dst).map_err(io)? {
                        if let Some(parent) = e.src.parent() {
                            fsutil::create_dirs_guarded(parent).map_err(io)?;
                        }
                        fsutil::rename_excl(&e.dst, &e.src).map_err(|er| map_exists(e.file, er))?;
                    }
                }
            }
        }
        for d in p.dirs.iter().rev() {
            prune_empty_tree(d);
        }
        Ok(())
    })();
    t.relocating.store(false, Ordering::Release);
    r?;
    for e in &p.entries {
        if let Some(parent) = e.src.parent() {
            sync_dir(parent);
        }
    }
    remove_if_exists(journal_path).map_err(StorageError::io)?;
    sync_parent(journal_path);
    Ok(())
}

fn run_rollback(t: &Arc<TorrentShared>, layout: &Layout, journal_path: &Path) -> Result<()> {
    let Some(p) = read_journal(journal_path, layout.file_count(), layout.structure_digest())?
    else {
        let _ = remove_if_exists(journal_path);
        return Ok(());
    };
    if p.method.is_none() || !p.started {
        let _ = remove_if_exists(journal_path);
        return Ok(());
    }
    if p.switched || p.gone {
        return Err(StorageError::InvalidState);
    }
    rollback_parsed(t, &p, journal_path)
}

fn run_commit(
    t: &Arc<TorrentShared>,
    layout: &Layout,
    journal_path: &Path,
    stop_at: Option<&Path>,
) -> Result<()> {
    let Some(p) = read_journal(journal_path, layout.file_count(), layout.structure_digest())?
    else {
        return Ok(()); // already committed
    };
    if p.method.is_none() || !p.started {
        let _ = remove_if_exists(journal_path);
        return Ok(());
    }
    if !p.prepared {
        return Err(StorageError::InvalidState);
    }
    let method = p.method.unwrap_or(RelocateMethod::Renamed);
    // The files that were in flight on the old layout finish first; then forget old descriptors.
    let deadline = Instant::now() + READ_RETRY_WAIT;
    while t.inflight_reads.load(Ordering::Acquire) != 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    t.disk.fds.invalidate_torrent(t.id);
    t.relocating.store(false, Ordering::Release);

    if method == RelocateMethod::Copied && !p.gone {
        // The source goes only if the destination is really there and complete.
        for e in p.entries.iter().filter(|e| !e.create_empty) {
            let want = layout.file_len(e.file as usize);
            let ok = fs::metadata(&e.dst).is_ok_and(|m| m.is_file() && m.len() == want);
            if !ok {
                return Err(StorageError::MissingData { file: e.file });
            }
        }
        for e in p.entries.iter().filter(|e| !e.create_empty) {
            remove_if_exists(&e.src).map_err(|er| StorageError::from_io(e.file, er))?;
        }
    }
    let mut jw = Jw::append(journal_path)?;
    if !p.gone {
        jw.rec(T_GONE, 0, true)?;
    }
    if let Some(stop) = stop_at {
        for e in &p.entries {
            if let Some(parent) = e.src.parent() {
                prune_up(parent, stop);
            }
        }
    }
    for e in &p.entries {
        if let Some(parent) = e.src.parent() {
            sync_dir(parent);
        }
    }
    drop(jw);
    remove_if_exists(journal_path).map_err(StorageError::io)?;
    sync_parent(journal_path);
    Ok(())
}

impl TorrentHandle {
    /// First phase of a relocation: moves (same volume) or copies and verifies (other volume)
    /// every file whose destination in `dst` differs from where it is now. `dst` has one entry per
    /// file index (`None` and padding files are left alone); destinations must be absolute and not
    /// exist yet. The engine picks collision-free names beforehand; nothing is ever overwritten
    /// ([`StorageError::DestinationExists`], with everything already done undone).
    ///
    /// Same volume: directories are created, then `rename_excl` per file; the files are at the
    /// destination when this resolves, so the engine must switch ([`TorrentHandle::switch_layout`]
    /// or [`TorrentHandle::set_paths`]) right away; until then reads that miss a file wait for the
    /// switch (up to 3 s). Other volume: sources stay untouched and keep serving reads. For a
    /// torrent that is still downloading call [`TorrentHandle::quiesce_writes`] first.
    ///
    /// Runs on one maintenance thread for its whole duration. `journal_path` must not hold the
    /// journal of an unfinished move ([`StorageError::InvalidState`]: recover it first).
    pub fn relocate_prepare(
        &self,
        journal_path: PathBuf,
        dst: Vec<Option<PathBuf>>,
        opts: RelocateOptions,
    ) -> Result<Pending<RelocateReport>> {
        let t = self.shared().clone();
        let layout = self.layout()?;
        self.maint(move || run_prepare(&t, &layout, &journal_path, &dst, &opts))
    }

    /// Records that the engine has switched to the new paths and persisted its record. After this
    /// the move can only go forward ([`TorrentHandle::relocate_rollback`] refuses).
    pub fn relocate_mark_switched(&self, journal_path: &Path) -> Result<()> {
        let layout = self.layout()?;
        let p = read_journal(journal_path, layout.file_count(), layout.structure_digest())?
            .ok_or(StorageError::InvalidState)?;
        if !p.prepared {
            return Err(StorageError::InvalidState);
        }
        if p.switched {
            return Ok(());
        }
        Jw::append(journal_path)?.rec(T_SWITCHED, 0, true)
    }

    /// Final phase: after the engine's record points at the destination, deletes the sources of a
    /// copy (only when every destination file exists with the right size), prunes the
    /// directories that became empty but never `stop_at` or anything above it (`None`: prunes
    /// nothing), and removes the journal. Valid from `Prepared`, `Switched` and `Done`;
    /// idempotent; a missing journal means "already committed". Waits (≤ 5 s) for reads submitted
    /// before the switch so none of them looks for a deleted file.
    pub fn relocate_commit(
        &self,
        journal_path: PathBuf,
        stop_at: Option<PathBuf>,
    ) -> Result<Pending<()>> {
        let t = self.shared().clone();
        let layout = self.layout()?;
        self.maint(move || run_commit(&t, &layout, &journal_path, stop_at.as_deref()))
    }

    /// Undoes a relocation that was not switched: deletes the copies and parts this move made, or
    /// renames the files back, removes the directories it created (when empty) and the journal.
    /// Refuses ([`StorageError::InvalidState`]) once `relocate_mark_switched` was recorded.
    /// Idempotent; a missing journal is success.
    pub fn relocate_rollback(&self, journal_path: PathBuf) -> Result<Pending<()>> {
        let t = self.shared().clone();
        let layout = self.layout()?;
        self.maint(move || run_rollback(&t, &layout, &journal_path))
    }

    /// Reads the journal after a restart and says how far the move got (see the recovery matrix
    /// in the README). A journal without a started plan is removed and reported as
    /// [`RecoverState::Nothing`]. Cheap, synchronous (one small file read).
    pub fn relocate_recover(&self, journal_path: &Path) -> Result<Recovered> {
        let layout = self.layout()?;
        let none = Recovered {
            state: RecoverState::Nothing,
            method: None,
            entries: Vec::new(),
        };
        let Some(p) = read_journal(journal_path, layout.file_count(), layout.structure_digest())?
        else {
            let _ = remove_if_exists(journal_path);
            return Ok(none);
        };
        if p.method.is_none() || !p.started {
            let _ = remove_if_exists(journal_path);
            return Ok(none);
        }
        let state = if p.gone {
            RecoverState::Done
        } else if p.switched {
            RecoverState::Switched
        } else if p.prepared {
            RecoverState::Prepared
        } else {
            RecoverState::Incomplete
        };
        Ok(Recovered {
            state,
            method: p.method,
            entries: p.entries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_codec_roundtrip_and_torn() {
        let entries = vec![
            RelocateEntry {
                file: 1,
                src: "/a/b".into(),
                dst: "/c/d".into(),
                create_empty: false,
            },
            RelocateEntry {
                file: 3,
                src: "/a/e".into(),
                dst: "/c/f".into(),
                create_empty: true,
            },
        ];
        let dirs = vec![PathBuf::from("/c")];
        let dir = tempfile::tempdir().unwrap();
        let jp = dir.path().join("j");
        let mut jw = Jw::create(&jp, 4, 99).unwrap();
        jw.plan(&encode_plan(RelocateMethod::Copied, &entries, &dirs))
            .unwrap();
        jw.rec(T_STARTED, 0, true).unwrap();
        jw.rec(T_PUBLISHING, 1, true).unwrap();
        let p = read_journal(&jp, 4, 99).unwrap().unwrap();
        assert_eq!(p.entries, entries);
        assert_eq!(p.dirs, dirs);
        assert!(p.started && !p.prepared);
        assert_eq!(p.state[1], S_PUBLISHING);
        // Torn trailing record and torn plan are tolerated.
        let mut bytes = fs::read(&jp).unwrap();
        bytes.extend_from_slice(&[T_DONE, 1]);
        fs::write(&jp, &bytes).unwrap();
        assert_eq!(read_journal(&jp, 4, 99).unwrap().unwrap().state[1], S_PUBLISHING);
        let cut = HEADER_LEN + 9;
        fs::write(&jp, &bytes[..cut]).unwrap();
        let p = read_journal(&jp, 4, 99).unwrap().unwrap();
        assert!(p.method.is_none() && !p.started);
        // Foreign journal.
        fs::write(&jp, &bytes).unwrap();
        assert!(read_journal(&jp, 5, 99).is_err());
        assert!(read_journal(&jp, 4, 98).is_err());
        // Garbage records after a valid header are rejected, not trusted.
        let mut bad = header(4, 99).to_vec();
        bad.extend_from_slice(&[T_DONE, 0, 0, 0, 0]);
        fs::write(&jp, &bad).unwrap();
        assert!(read_journal(&jp, 4, 99).is_err());
    }

    #[test]
    fn done_state_is_recovered_and_commit_finishes() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let (src, dst) = (base.join("s"), base.join("d"));
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("f"), vec![7u8; 5000]).unwrap();
        let layout = Arc::new(
            Layout::new(
                4096,
                5000,
                2,
                [crate::FileSpec {
                    path: crate::FilePath::Relative(vec!["f".into()]),
                    length: 5000,
                    is_padding: false,
                }],
            )
            .unwrap(),
        );
        let io = crate::DiskIo::new(crate::DiskConfig::default());
        let h = io.add_torrent(layout, crate::TorrentConfig::seed(&src));
        let j = base.join("j");
        let o = RelocateOptions {
            force_copy: true,
            reserve_bytes: 0,
            ..RelocateOptions::default()
        };
        let plan = h.plan_paths(&dst, None, crate::Location::Final).unwrap();
        h.relocate_prepare(j.clone(), plan, o).unwrap().wait().unwrap();
        h.set_paths(dst.clone(), None);
        h.relocate_mark_switched(&j).unwrap();
        // The crash: sources were deleted and SourcesGone recorded, journal not yet removed.
        fs::remove_file(src.join("f")).unwrap();
        Jw::append(&j).unwrap().rec(T_GONE, 0, true).unwrap();
        assert_eq!(h.relocate_recover(&j).unwrap().state, RecoverState::Done);
        assert!(matches!(
            h.relocate_rollback(j.clone()).unwrap().wait(),
            Err(StorageError::InvalidState)
        ));
        h.relocate_commit(j.clone(), Some(src.clone())).unwrap().wait().unwrap();
        assert!(!j.exists());
        assert_eq!(fs::read(dst.join("f")).unwrap(), vec![7u8; 5000]);
    }

    #[test]
    fn verifier_spans_files() {
        // 3 pieces of 10 bytes over a 25-byte stream.
        let stream: Vec<u8> = (0..25u8).collect();
        let layout = Layout::new(
            10,
            25,
            3,
            [
                crate::FileSpec {
                    path: crate::FilePath::Relative(vec!["a".into()]),
                    length: 7,
                    is_padding: false,
                },
                crate::FileSpec {
                    path: crate::FilePath::Relative(vec!["p".into()]),
                    length: 5,
                    is_padding: true,
                },
                crate::FileSpec {
                    path: crate::FilePath::Relative(vec!["b".into()]),
                    length: 13,
                    is_padding: false,
                },
            ],
        )
        .unwrap();
        let mut s = stream.clone();
        s[7..12].fill(0);
        let mut hashes = Vec::new();
        for p in 0..3 {
            let a = layout.piece_offset(p) as usize;
            hashes.extend_from_slice(&crate::hash::sha1(&s[a..a + layout.piece_size(p) as usize]));
        }
        let mut v = Verifier::new(&layout, &hashes, None);
        v.advance(7, Feed::Data(&s[..7])).unwrap();
        v.advance(5, Feed::Zeros).unwrap();
        v.advance(13, Feed::Data(&s[12..])).unwrap();
        assert_eq!(v.verified, 3);
        let mut v = Verifier::new(&layout, &hashes, None);
        v.advance(7, Feed::Data(&s[..7])).unwrap();
        v.advance(5, Feed::Zeros).unwrap();
        let mut bad = s[12..].to_vec();
        bad[9] ^= 1; // byte 21 -> piece 2
        assert_eq!(v.advance(13, Feed::Data(&bad)), Err(2));
    }
}
