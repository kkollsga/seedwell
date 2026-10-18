//! The disk engine: explicit I/O, hashing and maintenance thread pools, the write cache and the
//! per-torrent handle.
//!
//! # Data flow when downloading
//! ```text
//! write_block (engine thread)          copy 16 KiB into the piece buffer, bounded by the budget
//!   └─ piece complete ─▶ hash pool     SHA-1 over the whole buffer (HW accelerated)
//!                          ├─ ok ────▶ I/O pool: one pwrite per file segment, then Verified
//!                          └─ bad ───▶ buffer dropped, HashFailed reported, nothing written
//! over budget: blocks of *new* pieces are written through immediately (I/O pool); when such a
//!   piece completes it is re-read from disk in 256 KiB chunks and hashed (bounded memory).
//! ```
//! Back-pressure: the I/O queue is bounded; submissions from the engine use `try_send` and return
//! [`StorageError::Busy`] without side effects. The hash queue is unbounded in type but bounded in
//! practice: each queued job either owns a piece buffer (counted in the write-cache budget) or is
//! a tiny re-read request for a piece whose blocks are already on disk.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::thread::JoinHandle;

use crossbeam_channel as cb;

use crate::error::{Result, StorageError, is_not_found};
use crate::fdcache::{self, FdCache, FdCacheStats};
use crate::fsutil;
use crate::hash::{self, Sha1Stream};
use crate::layout::{BLOCK_SIZE, Layout, PathRef};
use crate::bitset::PieceSet;
use crate::oneshot::{self, Pending};

/// Largest single read accepted by [`TorrentHandle::read_block`].
pub const MAX_READ_LEN: usize = 1 << 20;
/// Chunk size when streaming pieces from disk for hashing.
pub(crate) const STREAM_CHUNK: usize = 256 * 1024;

/// Tuning knobs for [`DiskIo`].
#[derive(Clone, Debug)]
pub struct DiskConfig {
    /// Blocking I/O threads (`pread`/`pwrite`).
    pub io_threads: usize,
    /// Hash threads; defaults to the number of performance cores.
    pub hash_threads: usize,
    /// Threads for long jobs: recheck, scrub, finalize, fingerprinting.
    pub maintenance_threads: usize,
    /// Capacity of the bounded I/O request queue.
    pub queue_depth: usize,
    /// Global fd-cache capacity (SPEC §5.2: 512).
    pub fd_cache_capacity: usize,
    /// Global write-cache budget in bytes (SPEC §5.2: 32 MiB).
    pub write_cache_bytes: usize,
    /// Max partially received pieces per torrent (caps memory from a misbehaving picker).
    pub max_open_pieces: usize,
    /// A pool thread that finds nothing to do for this long exits (the next job starts one
    /// again). An idle engine holds no worker threads: each costs about 30 KiB of resident stack.
    pub idle_exit: Duration,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            io_threads: 4,
            hash_threads: fsutil::perf_core_count(),
            maintenance_threads: 3,
            queue_depth: 1024,
            fd_cache_capacity: 512,
            write_cache_bytes: 32 << 20,
            max_open_pieces: 1024,
            idle_exit: Duration::from_secs(30),
        }
    }
}

/// Whether a torrent may write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Downloading: files are created lazily and opened read-write.
    Download,
    /// Seeding: files are opened read-only and are **never created**.
    Seed,
}

/// Which tree the torrent's files currently live in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    /// Under the `.incomplete` staging root (no effect if no staging root is configured).
    Staging,
    /// At their final paths.
    Final,
}

/// Per-torrent placement configuration.
#[derive(Clone, Debug)]
pub struct TorrentConfig {
    /// Save root that relative layout paths are joined onto.
    pub root: PathBuf,
    /// `.incomplete` staging root (same volume as `root` ideally). `None`: write straight to final paths.
    pub staging: Option<PathBuf>,
    /// Initial mode.
    pub mode: Mode,
    /// Initial location.
    pub location: Location,
    /// Reserve disk space (`F_PREALLOCATE`) instead of sparse files.
    pub preallocate: bool,
}

impl TorrentConfig {
    /// Downloading into `root`, sparse, no staging.
    pub fn download(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            staging: None,
            mode: Mode::Download,
            location: Location::Final,
            preallocate: false,
        }
    }

    /// Complete data at final paths under `root`, read-only.
    pub fn seed(root: impl Into<PathBuf>) -> Self {
        Self {
            mode: Mode::Seed,
            ..Self::download(root)
        }
    }

    /// Writes to `<root>/.incomplete` first.
    pub fn with_default_staging(mut self) -> Self {
        self.staging = Some(self.root.join(".incomplete"));
        self.location = Location::Staging;
        self
    }
}

/// Result of a completed piece.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PieceOutcome {
    /// Hash matched and the piece is written to the file(s).
    Verified(u32),
    /// Hash mismatch: nothing was kept; re-download the piece.
    HashFailed(u32),
}

/// What happened to a submitted block.
#[derive(Debug)]
pub enum WriteAck {
    /// Held in the write cache (or queued for write-through); piece not complete yet.
    Buffered,
    /// Block already received; ignored.
    Duplicate,
    /// This block completed the piece; the future resolves once it is hashed and (if valid) on disk.
    Complete(Pending<PieceOutcome>),
}

/// Snapshot of global counters.
#[derive(Clone, Copy, Debug, Default)]
pub struct DiskStats {
    /// Bytes currently held by piece buffers.
    pub cache_used: usize,
    /// Highest `cache_used` ever reached.
    pub cache_peak: usize,
    /// Configured budget.
    pub cache_limit: usize,
    /// Blocks that went straight to disk because the cache was full.
    pub write_through_blocks: u64,
    /// Pieces verified and written.
    pub pieces_verified: u64,
    /// Pieces that failed hash verification.
    pub hash_failures: u64,
    /// Bytes written to files.
    pub bytes_written: u64,
    /// Bytes read from files for block reads.
    pub bytes_read: u64,
    /// fd cache counters.
    pub fd: FdCacheStats,
}

#[derive(Default)]
struct Counters {
    write_through_blocks: AtomicU64,
    pieces_verified: AtomicU64,
    hash_failures: AtomicU64,
    bytes_written: AtomicU64,
    bytes_read: AtomicU64,
}

struct Budget {
    used: AtomicUsize,
    peak: AtomicUsize,
    limit: usize,
}

impl Budget {
    fn try_reserve(&self, n: usize) -> bool {
        let mut cur = self.used.load(Ordering::Relaxed);
        loop {
            let Some(new) = cur.checked_add(n).filter(|&v| v <= self.limit) else {
                return false;
            };
            match self
                .used
                .compare_exchange_weak(cur, new, Ordering::AcqRel, Ordering::Relaxed)
            {
                Ok(_) => {
                    self.peak.fetch_max(new, Ordering::Relaxed);
                    return true;
                }
                Err(v) => cur = v,
            }
        }
    }

    fn release(&self, n: usize) {
        self.used.fetch_sub(n, Ordering::AcqRel);
    }
}

// ---------------------------------------------------------------------------------------------
// Worker pools
// ---------------------------------------------------------------------------------------------

fn spawn_named(name: String, f: impl FnOnce() + Send + 'static) -> Option<JoinHandle<()>> {
    std::thread::Builder::new().name(name).spawn(f).ok()
}

/// A job queue with worker threads that exist only while there is work.
///
/// Nothing runs at construction. A submission that finds more queued jobs than idle workers
/// starts one more, up to `max`; a worker that waits `idle_exit` for a job exits. A seeding
/// engine nobody is reading from therefore owns no pool threads (each is about 30 KiB of
/// resident stack plus whatever its job buffers left behind), and a download ramps up to the full
/// pool within the first few jobs (a thread starts in tens of microseconds).
///
/// The hand-over between a worker retiring and a submission is closed by ordering: a retiring
/// worker leaves `idle` and `live` first and only then looks at the queue; a submitter enqueues first
/// and only then reads `idle` and `live`. Whichever of the two looks second sees the other's effect.
struct Pool<J> {
    name: &'static str,
    tx: cb::Sender<J>,
    rx: cb::Receiver<J>,
    max: usize,
    idle_exit: Duration,
    /// Threads that exist and have not decided to exit.
    live: AtomicUsize,
    /// Threads blocked waiting for a job.
    idle: AtomicUsize,
    closed: AtomicBool,
    started: AtomicUsize,
    handles: Mutex<Vec<JoinHandle<()>>>,
    spawn: Box<dyn Fn(String) -> Option<JoinHandle<()>> + Send + Sync>,
}

impl<J> Pool<J> {
    fn new(
        name: &'static str,
        bound: Option<usize>,
        max: usize,
        idle_exit: Duration,
        spawn: Box<dyn Fn(String) -> Option<JoinHandle<()>> + Send + Sync>,
    ) -> Self {
        let (tx, rx) = match bound {
            Some(n) => cb::bounded(n),
            None => cb::unbounded(),
        };
        Self {
            name,
            tx,
            rx,
            max,
            idle_exit,
            live: AtomicUsize::new(0),
            idle: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            started: AtomicUsize::new(0),
            handles: Mutex::new(Vec::new()),
            spawn,
        }
    }

    /// Queues a job, waiting for room if the queue is bounded and full.
    fn send(&self, job: J) -> std::result::Result<(), cb::SendError<J>> {
        self.ensure_before_wait();
        let r = self.tx.send(job);
        self.ensure();
        r
    }

    /// Queues a job or says why not (full, or shut down).
    fn try_send(&self, job: J) -> std::result::Result<(), cb::TrySendError<J>> {
        let r = self.tx.try_send(job);
        if r.is_ok() {
            self.ensure();
        }
        r
    }

    /// A blocking send into a full queue needs a worker to make room: make sure one exists.
    fn ensure_before_wait(&self) {
        if self.tx.is_full() {
            self.ensure();
        }
    }

    /// Starts a worker if the queue holds more jobs than idle workers and the pool is below `max`.
    fn ensure(&self) {
        if self.closed.load(Ordering::SeqCst) || self.rx.len() <= self.idle.load(Ordering::SeqCst) {
            return;
        }
        let mut live = self.live.load(Ordering::SeqCst);
        loop {
            if live >= self.max {
                return;
            }
            match self.live.compare_exchange(live, live + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break,
                Err(v) => live = v,
            }
        }
        let n = self.started.fetch_add(1, Ordering::Relaxed);
        match (self.spawn)(format!("{}-{}", self.name, n % self.max.max(1))) {
            Some(h) => {
                let mut hs = lock(&self.handles);
                hs.retain(|h| !h.is_finished());
                hs.push(h);
            }
            None => {
                self.live.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// The next job for a worker, or `None` when the thread must exit (it has already left `live`).
    fn next(&self) -> Option<J> {
        loop {
            self.idle.fetch_add(1, Ordering::SeqCst);
            let r = self.rx.recv_timeout(self.idle_exit);
            self.idle.fetch_sub(1, Ordering::SeqCst);
            match r {
                Ok(job) => return Some(job),
                Err(cb::RecvTimeoutError::Disconnected) => {
                    self.live.fetch_sub(1, Ordering::SeqCst);
                    return None;
                }
                Err(cb::RecvTimeoutError::Timeout) => {
                    self.live.fetch_sub(1, Ordering::SeqCst);
                    if self.rx.is_empty() || self.closed.load(Ordering::SeqCst) {
                        return None;
                    }
                    // A job slipped in while retiring: stay.
                    self.live.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
    }

    /// A worker that read its `Stop` leaves the pool.
    fn left(&self) {
        self.live.fetch_sub(1, Ordering::SeqCst);
    }

    /// Threads running now (idle or busy).
    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Lets queued work finish, then stops and joins every worker.
    fn shutdown(&self, stop: impl Fn() -> J) {
        // Work still queued needs someone to run it.
        if !self.rx.is_empty() && self.live() == 0 {
            self.ensure();
        }
        self.closed.store(true, Ordering::SeqCst);
        for _ in 0..self.live() {
            let _ = self.tx.send(stop());
        }
        let hs: Vec<_> = lock(&self.handles).drain(..).collect();
        for h in hs {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------------------------

enum IoJob {
    Read {
        t: Arc<TorrentShared>,
        layout: Arc<Layout>,
        offset: u64,
        len: usize,
        buf: Vec<u8>,
        tx: oneshot::Sender<Vec<u8>>,
    },
    WritePiece {
        t: Arc<TorrentShared>,
        layout: Arc<Layout>,
        piece: u32,
        buf: Box<[u8]>,
        tx: oneshot::Sender<PieceOutcome>,
    },
    WriteBlock {
        t: Arc<TorrentShared>,
        layout: Arc<Layout>,
        piece: u32,
        begin: u32,
        data: Box<[u8]>,
    },
    Stop,
}

enum HashJob {
    Buffer {
        t: Arc<TorrentShared>,
        layout: Arc<Layout>,
        piece: u32,
        buf: Box<[u8]>,
        expected: [u8; 20],
        tx: oneshot::Sender<PieceOutcome>,
    },
    Disk {
        t: Arc<TorrentShared>,
        layout: Arc<Layout>,
        piece: u32,
        expected: [u8; 20],
        tx: oneshot::Sender<PieceOutcome>,
    },
    Stop,
}

enum MaintJob {
    Run(Box<dyn FnOnce() + Send>),
    Stop,
}

// ---------------------------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------------------------

pub(crate) struct Shared {
    cfg: DiskConfig,
    pub(crate) fds: FdCache,
    budget: Budget,
    io: Pool<IoJob>,
    hash: Pool<HashJob>,
    maint: Pool<MaintJob>,
    next_torrent: AtomicU32,
    counters: Counters,
}

/// Owner of the thread pools. Dropping it drains the queues and joins the threads.
pub struct DiskIo {
    shared: Arc<Shared>,
}

impl DiskIo {
    /// Sets up the pools. No thread runs until the first job asks for one (see [`Pool`]).
    pub fn new(cfg: DiskConfig) -> Self {
        let io_n = cfg.io_threads.max(1);
        let hash_n = cfg.hash_threads.max(1);
        let maint_n = cfg.maintenance_threads.max(1);
        let idle = cfg.idle_exit;
        let depth = cfg.queue_depth.max(8);
        let shared = Arc::new_cyclic(|me: &std::sync::Weak<Shared>| {
            let (me_io, me_hash, me_maint) = (me.clone(), me.clone(), me.clone());
            Shared {
                fds: FdCache::new(cfg.fd_cache_capacity),
                budget: Budget {
                    used: AtomicUsize::new(0),
                    peak: AtomicUsize::new(0),
                    limit: cfg.write_cache_bytes,
                },
                io: Pool::new(
                    "trs-io",
                    Some(depth),
                    io_n,
                    idle,
                    Box::new(move |name| {
                        let sh = me_io.upgrade()?;
                        spawn_named(name, move || io_worker(&sh))
                    }),
                ),
                hash: Pool::new(
                    "trs-hash",
                    None,
                    hash_n,
                    idle,
                    Box::new(move |name| {
                        let sh = me_hash.upgrade()?;
                        spawn_named(name, move || hash_worker(&sh))
                    }),
                ),
                maint: Pool::new(
                    "trs-maint",
                    Some(256),
                    maint_n,
                    idle,
                    Box::new(move |name| {
                        let sh = me_maint.upgrade()?;
                        spawn_named(name, move || maint_worker(&sh))
                    }),
                ),
                next_torrent: AtomicU32::new(1),
                counters: Counters::default(),
                cfg,
            }
        });
        Self { shared }
    }

    /// Registers a torrent. Cheap: nothing is opened or created.
    pub fn add_torrent(&self, layout: Arc<Layout>, cfg: TorrentConfig) -> TorrentHandle {
        let id = self.shared.next_torrent.fetch_add(1, Ordering::Relaxed);
        let digest = layout.digest();
        TorrentHandle(Arc::new(TorrentShared {
            id,
            disk: self.shared.clone(),
            digest: AtomicU64::new(digest),
            structure: layout.structure_digest(),
            layout: RwLock::new(Some(layout)),
            paths: RwLock::new(Arc::new(Paths {
                root: cfg.root,
                staging: cfg.staging,
            })),
            mode: AtomicU8::new(cfg.mode as u8),
            location: AtomicU8::new(cfg.location as u8),
            preallocate: AtomicBool::new(cfg.preallocate),
            hashes: RwLock::new(None),
            pieces: Mutex::new(HashMap::new()),
            verified: Mutex::new(None),
            inflight_reads: AtomicU32::new(0),
            inflight_writes: AtomicU32::new(0),
            write_pause: AtomicBool::new(false),
            relocating: AtomicBool::new(false),
            path_gate: RwLock::new(()),
        }))
    }

    /// Counter snapshot.
    pub fn stats(&self) -> DiskStats {
        let s = &self.shared;
        DiskStats {
            cache_used: s.budget.used.load(Ordering::Relaxed),
            cache_peak: s.budget.peak.load(Ordering::Relaxed),
            cache_limit: s.budget.limit,
            write_through_blocks: s.counters.write_through_blocks.load(Ordering::Relaxed),
            pieces_verified: s.counters.pieces_verified.load(Ordering::Relaxed),
            hash_failures: s.counters.hash_failures.load(Ordering::Relaxed),
            bytes_written: s.counters.bytes_written.load(Ordering::Relaxed),
            bytes_read: s.counters.bytes_read.load(Ordering::Relaxed),
            fd: s.fds.stats(),
        }
    }

    /// Worker threads that exist right now (I/O, hashing and maintenance pools together). Zero
    /// for an engine that has had nothing to do for [`DiskConfig::idle_exit`].
    pub fn worker_threads(&self) -> usize {
        let s = &self.shared;
        s.io.live() + s.hash.live() + s.maint.live()
    }

    /// The configuration in effect.
    pub fn config(&self) -> &DiskConfig {
        &self.shared.cfg
    }
}

impl Drop for DiskIo {
    fn drop(&mut self) {
        // Stop messages queue behind real work, so accepted requests still complete.
        let s = &self.shared;
        s.io.shutdown(|| IoJob::Stop);
        s.hash.shutdown(|| HashJob::Stop);
        s.maint.shutdown(|| MaintJob::Stop);
    }
}

fn io_worker(sh: &Shared) {
    while let Some(job) = sh.io.next() {
        match job {
            IoJob::Stop => {
                sh.io.left();
                break;
            }
            IoJob::Read {
                t,
                layout,
                offset,
                len,
                mut buf,
                tx,
            } => {
                buf.clear();
                buf.resize(len, 0);
                let r = t.read_into(&layout, offset, &mut buf);
                t.inflight_reads.fetch_sub(1, Ordering::AcqRel);
                match r {
                    Ok(()) => {
                        sh.counters
                            .bytes_read
                            .fetch_add(len as u64, Ordering::Relaxed);
                        tx.send(Ok(buf));
                    }
                    Err(e) => tx.send(Err(e)),
                }
            }
            IoJob::WritePiece {
                t,
                layout,
                piece,
                buf,
                tx,
            } => {
                let r = t.write_piece(&layout, piece, &buf);
                t.inflight_writes.fetch_sub(1, Ordering::AcqRel);
                sh.budget.release(buf.len());
                match r {
                    Ok(()) => {
                        sh.counters
                            .bytes_written
                            .fetch_add(buf.len() as u64, Ordering::Relaxed);
                        sh.counters.pieces_verified.fetch_add(1, Ordering::Relaxed);
                        // Before the ack resolves, so a block that arrives after it is ignored.
                        t.mark_verified(piece);
                        tx.send(Ok(PieceOutcome::Verified(piece)));
                    }
                    Err(e) => tx.send(Err(e)),
                }
            }
            IoJob::WriteBlock {
                t,
                layout,
                piece,
                begin,
                data,
            } => {
                let off = layout.piece_offset(piece) + u64::from(begin);
                let r = t.write_at_stream(&layout, off, &data);
                if r.is_ok() {
                    sh.counters
                        .bytes_written
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                t.through_block_done(&layout, piece, r.err());
                t.inflight_writes.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

fn hash_worker(sh: &Shared) {
    let mut scratch: Vec<u8> = Vec::new();
    while let Some(job) = sh.hash.next() {
        match job {
            HashJob::Stop => {
                sh.hash.left();
                break;
            }
            HashJob::Buffer {
                t,
                layout,
                piece,
                buf,
                expected,
                tx,
            } => {
                if hash::sha1(&buf) == expected {
                    let job = IoJob::WritePiece {
                        t,
                        layout,
                        piece,
                        buf,
                        tx,
                    };
                    if let Err(cb::SendError(IoJob::WritePiece { buf, t, .. })) = sh.io.send(job) {
                        t.inflight_writes.fetch_sub(1, Ordering::AcqRel);
                        sh.budget.release(buf.len());
                    }
                } else {
                    t.inflight_writes.fetch_sub(1, Ordering::AcqRel);
                    sh.budget.release(buf.len());
                    sh.counters.hash_failures.fetch_add(1, Ordering::Relaxed);
                    tx.send(Ok(PieceOutcome::HashFailed(piece)));
                }
            }
            HashJob::Disk {
                t,
                layout,
                piece,
                expected,
                tx,
            } => {
                if scratch.is_empty() {
                    scratch = vec![0; STREAM_CHUNK];
                }
                match t.hash_piece_from_disk(&layout, piece, &mut scratch) {
                    Ok(d) if d == expected => {
                        sh.counters.pieces_verified.fetch_add(1, Ordering::Relaxed);
                        t.mark_verified(piece);
                        tx.send(Ok(PieceOutcome::Verified(piece)));
                    }
                    Ok(_) => {
                        sh.counters.hash_failures.fetch_add(1, Ordering::Relaxed);
                        tx.send(Ok(PieceOutcome::HashFailed(piece)));
                    }
                    Err(e) => tx.send(Err(e)),
                }
                t.inflight_writes.fetch_sub(1, Ordering::AcqRel);
                // Don't keep a 256 KiB scratch per idle hash thread.
                scratch = Vec::new();
            }
        }
    }
}

fn maint_worker(sh: &Shared) {
    while let Some(job) = sh.maint.next() {
        match job {
            MaintJob::Stop => {
                sh.maint.left();
                break;
            }
            MaintJob::Run(f) => f(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Per-torrent state
// ---------------------------------------------------------------------------------------------

pub(crate) struct Paths {
    pub(crate) root: PathBuf,
    pub(crate) staging: Option<PathBuf>,
}

// size: 16 bytes + bitmap (1 bit/block) + piece buffer while buffered
struct PieceEntry {
    have: Box<[u64]>,
    received: u32,
    nblocks: u32,
    /// `None` = this piece is being written through.
    buf: Option<Box<[u8]>>,
    pending_writes: u32,
    error: Option<StorageError>,
    done: Option<oneshot::Sender<PieceOutcome>>,
}

pub(crate) struct TorrentShared {
    pub(crate) id: u32,
    pub(crate) disk: Arc<Shared>,
    /// Full digest (paths included) of the layout currently installed or expected back.
    pub(crate) digest: AtomicU64,
    /// Geometry digest: constant for the life of the torrent; `switch_layout` checks against it.
    pub(crate) structure: u64,
    layout: RwLock<Option<Arc<Layout>>>,
    pub(crate) paths: RwLock<Arc<Paths>>,
    mode: AtomicU8,
    location: AtomicU8,
    preallocate: AtomicBool,
    hashes: RwLock<Option<Arc<[u8]>>>,
    pieces: Mutex<HashMap<u32, PieceEntry>>,
    /// Pieces already verified (and written) in this process: late duplicate blocks for them
    /// are ignored instead of starting a second buffer for the same piece. Allocated lazily,
    /// 1 bit per piece, only while downloading.
    verified: Mutex<Option<PieceSet>>,
    /// Read jobs submitted and not finished (relocation waits for zero before unlinking sources).
    pub(crate) inflight_reads: AtomicU32,
    /// Write/hash jobs queued or running for this torrent (`quiesce_writes` waits for zero).
    pub(crate) inflight_writes: AtomicU32,
    /// While set, `write_block` answers `Busy`.
    write_pause: AtomicBool,
    /// While set, a read that finds its file missing retries briefly with the then-current layout
    /// (a same-volume relocation renames files a moment before the layout switch).
    pub(crate) relocating: AtomicBool,
    /// Held shared while a file is opened (path resolution and `open` as one step) and
    /// exclusive while a file is renamed together with the layout switch that names its new
    /// path. So no open can see the old path after the rename, nor the new path before it.
    path_gate: RwLock<()>,
}

impl Drop for TorrentShared {
    fn drop(&mut self) {
        self.disk.fds.invalidate_torrent(self.id);
        // Return budget held by abandoned partial pieces.
        let map = self.pieces.get_mut().unwrap_or_else(|p| p.into_inner());
        for e in map.values() {
            if let Some(b) = &e.buf {
                self.disk.budget.release(b.len());
            }
        }
    }
}

fn rd<T>(l: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|p| p.into_inner())
}

fn wr<T>(l: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|p| p.into_inner())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl TorrentShared {
    /// Records that `piece` is verified and on disk (later blocks for it are ignored).
    pub(crate) fn mark_verified(&self, piece: u32) {
        let Ok(layout) = self.layout() else { return };
        if self.mode() != Mode::Download {
            return;
        }
        lock(&self.verified)
            .get_or_insert_with(|| PieceSet::new(layout.piece_count()))
            .set(piece, true);
    }

    /// Replaces the verified set with the result of a full recheck.
    pub(crate) fn replace_verified(&self, have: &PieceSet) {
        if self.mode() == Mode::Download {
            *lock(&self.verified) = Some(have.clone());
        }
    }

    pub(crate) fn is_verified(&self, piece: u32) -> bool {
        lock(&self.verified).as_ref().is_some_and(|v| v.get(piece))
    }

    pub(crate) fn layout(&self) -> Result<Arc<Layout>> {
        rd(&self.layout).clone().ok_or(StorageError::LayoutDropped)
    }

    pub(crate) fn mode(&self) -> Mode {
        if self.mode.load(Ordering::Acquire) == Mode::Seed as u8 {
            Mode::Seed
        } else {
            Mode::Download
        }
    }

    pub(crate) fn location(&self) -> Location {
        if self.location.load(Ordering::Acquire) == Location::Final as u8 {
            Location::Final
        } else {
            Location::Staging
        }
    }

    /// Absolute path of file `i` in the given tree, plus the base directory that must already
    /// exist before we create anything below it.
    pub(crate) fn resolve(
        &self,
        layout: &Layout,
        i: u32,
        loc: Location,
    ) -> (PathBuf, Option<PathBuf>) {
        let paths = rd(&self.paths).clone();
        Self::resolve_in(&paths, layout, i, loc)
    }

    /// [`Self::resolve`] against an explicit set of roots (what the paths *would* be).
    pub(crate) fn resolve_in(
        paths: &Paths,
        layout: &Layout,
        i: u32,
        loc: Location,
    ) -> (PathBuf, Option<PathBuf>) {
        let staged = loc == Location::Staging && paths.staging.is_some();
        let pref = layout.file_path(i as usize);
        match (staged, pref) {
            (false, PathRef::Relative(rel)) => (paths.root.join(rel), Some(paths.root.clone())),
            (false, PathRef::Absolute(abs)) => (PathBuf::from(abs), None),
            (true, PathRef::Relative(rel)) => {
                let s = paths.staging.clone().unwrap_or_default();
                (s.join(rel), Some(s))
            }
            (true, PathRef::Absolute(abs)) => {
                // Custom targets can be on another volume; stage them under a flat, collision-free name.
                let s = paths.staging.clone().unwrap_or_default();
                let name = Path::new(abs)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy();
                (s.join(".custom").join(format!("{i}-{name}")), Some(s))
            }
        }
    }

    pub(crate) fn staging_root(&self) -> Option<PathBuf> {
        rd(&self.paths).staging.clone()
    }

    pub(crate) fn set_location_final(&self) {
        self.location
            .store(Location::Final as u8, Ordering::Release);
        self.disk.fds.invalidate_torrent(self.id);
    }

    fn current_path(&self, layout: &Layout, i: u32) -> (PathBuf, Option<PathBuf>) {
        self.resolve(layout, i, self.location())
    }

    /// Path of file `i` in the layout installed right now. `layout` is only the fallback (a
    /// job queued before a switch holds the old `Arc`, whose paths are stale; geometry is the
    /// same in both).
    fn live_path(&self, layout: &Layout, i: u32) -> (PathBuf, Option<PathBuf>) {
        let cur = rd(&self.layout).clone();
        self.current_path(cur.as_deref().unwrap_or(layout), i)
    }

    fn open_for_read(&self, layout: &Layout, i: u32) -> io::Result<Arc<File>> {
        let key = fdcache::key(self.id, i);
        let _gate = rd(&self.path_gate);
        self.disk.fds.get_or_open(key, false, || {
            let (p, _) = self.live_path(layout, i);
            File::open(p)
        })
    }

    fn file_for_read(&self, layout: &Layout, i: u32) -> Result<Arc<File>> {
        let first = match self.open_for_read(layout, i) {
            Ok(f) => return Ok(f),
            Err(e) => e,
        };
        if !is_not_found(&first) || !self.relocating.load(Ordering::Acquire) {
            return Err(StorageError::from_io(i, first));
        }
        // A same-volume relocation renamed the file an instant before the layout switch: wait for
        // the switch (bounded) and look again with whatever layout is current then. The flag is
        // read *before* the open, so a miss with the flag already clear is a real miss.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut err = first;
        loop {
            std::thread::sleep(Duration::from_millis(2));
            let flagged = self.relocating.load(Ordering::Acquire);
            let Ok(cur) = self.layout() else { break };
            match self.open_for_read(&cur, i) {
                Ok(f) => return Ok(f),
                Err(e) if is_not_found(&e) => err = e,
                Err(e) => return Err(StorageError::from_io(i, e)),
            }
            if !flagged || Instant::now() >= deadline {
                break;
            }
        }
        Err(StorageError::from_io(i, err))
    }

    fn file_for_write(&self, layout: &Layout, i: u32) -> Result<Arc<File>> {
        if self.mode() != Mode::Download {
            return Err(StorageError::ReadOnly);
        }
        let key = fdcache::key(self.id, i);
        let want = layout.file_len(i as usize);
        let prealloc = self.preallocate.load(Ordering::Relaxed);
        let mut root_missing = false;
        let _gate = rd(&self.path_gate);
        let r = self.disk.fds.get_or_open(key, true, || {
            let (path, base) = self.live_path(layout, i);
            // Never rebuild a vanished save root: that is how data silently ends up on the boot
            // volume when an external drive is unplugged.
            if let Some(b) = &base {
                if !b.is_dir() {
                    root_missing = true;
                    return Err(io::Error::from(io::ErrorKind::NotFound));
                }
            }
            if let Some(parent) = path.parent() {
                fsutil::create_dirs_guarded(parent)?;
            }
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            if f.metadata()?.len() < want {
                if prealloc && f.metadata()?.len() == 0 {
                    fsutil::preallocate(&f, want)?;
                } else {
                    f.set_len(want)?;
                }
            }
            Ok(f)
        });
        match r {
            Ok(f) => Ok(f),
            Err(_) if root_missing => Err(StorageError::MissingRoot),
            Err(e) if is_not_found(&e) => Err(StorageError::MissingRoot),
            Err(e) => Err(StorageError::from_io(i, e)),
        }
    }

    /// Reads `buf.len()` bytes at stream offset `offset`; padding reads as zeros.
    pub(crate) fn read_into(&self, layout: &Layout, offset: u64, buf: &mut [u8]) -> Result<()> {
        let segs = layout
            .segments(offset, buf.len())
            .ok_or(StorageError::OutOfRange)?;
        for s in segs {
            let dst = &mut buf[s.data_offset..s.data_offset + s.len];
            if s.padding {
                dst.fill(0);
                continue;
            }
            let f = self.file_for_read(layout, s.file)?;
            f.read_exact_at(dst, s.file_offset)
                .map_err(|e| StorageError::from_io(s.file, e))?;
        }
        Ok(())
    }

    /// Writes `data` at stream offset `offset`, one `pwrite` per file segment; skips padding.
    fn write_at_stream(&self, layout: &Layout, offset: u64, data: &[u8]) -> Result<()> {
        let segs = layout
            .segments(offset, data.len())
            .ok_or(StorageError::OutOfRange)?;
        for s in segs {
            if s.padding {
                continue;
            }
            let f = self.file_for_write(layout, s.file)?;
            f.write_all_at(&data[s.data_offset..s.data_offset + s.len], s.file_offset)
                .map_err(|e| StorageError::from_io(s.file, e))?;
        }
        Ok(())
    }

    fn write_piece(&self, layout: &Layout, piece: u32, buf: &[u8]) -> Result<()> {
        self.write_at_stream(layout, layout.piece_offset(piece), buf)
    }

    /// SHA-1 of a piece read back in `scratch`-sized chunks.
    pub(crate) fn hash_piece_from_disk(
        &self,
        layout: &Layout,
        piece: u32,
        scratch: &mut [u8],
    ) -> Result<[u8; 20]> {
        let start = layout.piece_offset(piece);
        let size = layout.piece_size(piece) as usize;
        let mut h = Sha1Stream::new();
        let mut done = 0;
        while done < size {
            let n = scratch.len().min(size - done);
            self.read_into(layout, start + done as u64, &mut scratch[..n])?;
            h.update(&scratch[..n]);
            done += n;
        }
        Ok(h.finish())
    }

    fn expected_hash(&self, piece: u32) -> Result<[u8; 20]> {
        let g = rd(&self.hashes);
        let h = g.as_ref().ok_or(StorageError::NoHashes)?;
        let a = piece as usize * 20;
        h.get(a..a + 20)
            .and_then(|s| <[u8; 20]>::try_from(s).ok())
            .ok_or(StorageError::NoHashes)
    }

    /// Called by an I/O thread after a write-through block finished.
    fn through_block_done(
        self: &Arc<Self>,
        layout: &Arc<Layout>,
        piece: u32,
        err: Option<StorageError>,
    ) {
        let mut next: Option<HashJob> = None;
        let mut fail: Option<(oneshot::Sender<PieceOutcome>, StorageError)> = None;
        {
            let mut map = lock(&self.pieces);
            let Some(e) = map.get_mut(&piece) else { return };
            e.pending_writes = e.pending_writes.saturating_sub(1);
            if let Some(er) = err {
                e.error.get_or_insert(er);
            }
            if e.received == e.nblocks && e.pending_writes == 0 {
                if let Some(mut e) = map.remove(&piece) {
                    if let Some(tx) = e.done.take() {
                        match e.error.take() {
                            Some(er) => fail = Some((tx, er)),
                            None => match self.expected_hash(piece) {
                                Ok(expected) => {
                                    self.inflight_writes.fetch_add(1, Ordering::AcqRel);
                                    next = Some(HashJob::Disk {
                                        t: self.clone(),
                                        layout: layout.clone(),
                                        piece,
                                        expected,
                                        tx,
                                    });
                                }
                                Err(er) => fail = Some((tx, er)),
                            },
                        }
                    }
                }
            }
        }
        if let Some((tx, er)) = fail {
            tx.send(Err(er));
        }
        if let Some(job) = next {
            let _ = self.disk.hash.send(job);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Public handle
// ---------------------------------------------------------------------------------------------

/// Cheap, cloneable, thread-safe reference to one torrent's storage.
#[derive(Clone)]
pub struct TorrentHandle(pub(crate) Arc<TorrentShared>);

impl std::fmt::Debug for TorrentHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TorrentHandle#{}", self.0.id)
    }
}

impl TorrentHandle {
    /// Process-unique id used in the fd cache.
    pub fn id(&self) -> u32 {
        self.0.id
    }

    /// The layout, unless dropped for the Cold tier.
    pub fn layout(&self) -> Result<Arc<Layout>> {
        self.0.layout()
    }

    /// Current mode.
    pub fn mode(&self) -> Mode {
        self.0.mode()
    }

    /// Switches mode. Cached descriptors are dropped so they reopen with the right access
    /// (read-only for seeding, read-write for downloading).
    pub fn set_mode(&self, mode: Mode) {
        self.0.mode.store(mode as u8, Ordering::Release);
        self.0.disk.fds.invalidate_torrent(self.0.id);
        if mode != Mode::Download {
            // A seeder accepts no writes; do not keep a bitmap for it.
            *lock(&self.0.verified) = None;
        }
    }

    /// Declares `pieces` verified (resume data accepted by the caller, or a finished recheck), so
    /// blocks that still arrive for them are ignored. Replaces what was recorded before.
    pub fn set_verified(&self, pieces: impl IntoIterator<Item = u32>) {
        let Ok(layout) = self.0.layout() else { return };
        if self.0.mode() != Mode::Download {
            return;
        }
        let mut set = PieceSet::new(layout.piece_count());
        for p in pieces {
            set.set(p, true);
        }
        *lock(&self.0.verified) = Some(set);
    }

    /// Current location.
    pub fn location(&self) -> Location {
        self.0.location()
    }

    /// Declares where the files currently are (after finalize, or "Locate…").
    pub fn set_location(&self, loc: Location) {
        self.0.location.store(loc as u8, Ordering::Release);
        self.0.disk.fds.invalidate_torrent(self.0.id);
    }

    /// Re-points the torrent at a new save root/staging root ("Locate…", volume remounted elsewhere).
    pub fn set_paths(&self, root: PathBuf, staging: Option<PathBuf>) {
        *wr(&self.0.paths) = Arc::new(Paths { root, staging });
        self.0.disk.fds.invalidate_torrent(self.0.id);
        self.0.relocating.store(false, Ordering::Release);
    }

    /// Atomically swaps in a layout with identical geometry (piece length, total length, file
    /// offsets, lengths, padding flags) but different file paths (relative or absolute): the
    /// instant switch at the end of a relocation. Cached descriptors of this torrent are dropped;
    /// reads already running keep their `Arc<File>` (and their old layout `Arc`), so nothing
    /// blocks. Works in either mode, and while the layout is dropped (Cold): then only the
    /// expected digest changes and [`TorrentHandle::rebuild_layout`] must be given the new layout.
    /// Fails with [`StorageError::LayoutMismatch`] if the geometry differs.
    pub fn switch_layout(&self, new: Arc<Layout>) -> Result<()> {
        if new.structure_digest() != self.0.structure {
            return Err(StorageError::LayoutMismatch);
        }
        let _gate = wr(&self.0.path_gate);
        {
            let mut g = wr(&self.0.layout);
            self.0.digest.store(new.digest(), Ordering::Release);
            if g.is_some() {
                *g = Some(new);
            }
        }
        self.0.disk.fds.invalidate_torrent(self.0.id);
        self.0.relocating.store(false, Ordering::Release);
        Ok(())
    }

    /// [`TorrentHandle::switch_layout`] that also renames files, as one step for every reader
    /// and writer of this torrent: while it runs, no file of the torrent is opened, and nothing
    /// opens the old path afterwards (a write that was queued earlier resolves its path from the
    /// layout installed at that moment). This is what makes it safe to move a file that the
    /// torrent may be writing.
    ///
    /// * `absent`: paths that must not exist (a file the layout is about to name there was never
    ///   written, and nothing may have created it since). Otherwise an `AlreadyExists` error, nothing changed.
    /// * `renames`: `(from, to)`, executed in order with `rename_excl`; `to`'s folder is created.
    ///   A failure undoes the renames made so far and changes nothing else.
    /// * `before_switch`: called after the renames and before the new layout is installed, with
    ///   the gate held (tests use it to put a write exactly there).
    ///
    /// A crash between the renames and whatever the caller persists is repaired at the next
    /// start by comparing the record with the files (see the engine's `edgefix`).
    pub fn switch_layout_renaming(
        &self,
        new: Arc<Layout>,
        renames: &[(PathBuf, PathBuf)],
        absent: &[PathBuf],
        before_switch: Option<&dyn Fn()>,
    ) -> Result<()> {
        if new.structure_digest() != self.0.structure {
            return Err(StorageError::LayoutMismatch);
        }
        let t = &self.0;
        let _gate = wr(&t.path_gate);
        if absent.iter().any(|p| std::fs::symlink_metadata(p).is_ok()) {
            return Err(StorageError::io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a file appeared where the layout was about to point",
            )));
        }
        let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
        for r in renames {
            let step = (|| -> io::Result<()> {
                if let Some(dir) = r.1.parent() {
                    fsutil::create_dirs_guarded(dir)?;
                }
                fsutil::rename_excl(&r.0, &r.1)
            })();
            if let Err(e) = step {
                for (from, to) in done.iter().rev().map(|r| (&r.0, &r.1)) {
                    let _ = fsutil::rename_excl(to, from);
                }
                return Err(StorageError::io(e));
            }
            done.push(r);
        }
        if let Some(f) = before_switch {
            f();
        }
        {
            let mut g = wr(&t.layout);
            t.digest.store(new.digest(), Ordering::Release);
            if g.is_some() {
                *g = Some(new);
            }
        }
        t.disk.fds.invalidate_torrent(t.id);
        t.relocating.store(false, Ordering::Release);
        Ok(())
    }

    /// Where each file would be if the torrent were re-pointed with
    /// [`TorrentHandle::set_paths`]`(new_root, new_staging)` while in location `loc`. Padding files
    /// are `None`. Relative layout paths follow the roots; absolute ones stay (their staging names
    /// `.incomplete/.custom/<i>-<name>` follow the staging root). Input for relocation plans.
    pub fn plan_paths(
        &self,
        new_root: &Path,
        new_staging: Option<&Path>,
        loc: Location,
    ) -> Result<Vec<Option<PathBuf>>> {
        let layout = self.0.layout()?;
        let paths = Paths {
            root: new_root.to_path_buf(),
            staging: new_staging.map(Path::to_path_buf),
        };
        Ok((0..layout.file_count())
            .map(|i| {
                (!layout.file_is_padding(i))
                    .then(|| TorrentShared::resolve_in(&paths, &layout, i as u32, loc).0)
            })
            .collect())
    }

    /// Makes [`TorrentHandle::write_block`] answer `Busy` and resolves once no write or hash job of
    /// this torrent is queued or running (so every verified piece is on disk). Reads continue.
    /// Pair with [`TorrentHandle::resume_writes`]. Fails with `Busy` if jobs do not drain in 30 s.
    pub fn quiesce_writes(&self) -> Result<Pending<()>> {
        self.0.write_pause.store(true, Ordering::Release);
        let t = self.0.clone();
        self.maint(move || {
            let deadline = Instant::now() + Duration::from_secs(30);
            while t.inflight_writes.load(Ordering::Acquire) != 0 {
                if Instant::now() > deadline {
                    return Err(StorageError::Busy);
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        })
    }

    /// Re-enables writes after [`TorrentHandle::quiesce_writes`].
    pub fn resume_writes(&self) {
        self.0.write_pause.store(false, Ordering::Release);
    }

    /// Creates the save root (and staging root) if their volume is mounted. Call once when a new
    /// download starts; writes never create the root themselves.
    pub fn ensure_root(&self) -> Result<()> {
        let paths = rd(&self.0.paths).clone();
        for d in std::iter::once(&paths.root).chain(paths.staging.iter()) {
            fsutil::create_dirs_guarded(d).map_err(|e| {
                if is_not_found(&e) {
                    StorageError::MissingRoot
                } else {
                    StorageError::io(e)
                }
            })?;
        }
        Ok(())
    }

    /// Path of file `file` in the final or staging tree.
    pub fn file_path(&self, file: u32, loc: Location) -> Result<PathBuf> {
        let layout = self.0.layout()?;
        if file as usize >= layout.file_count() {
            return Err(StorageError::OutOfRange);
        }
        Ok(self.0.resolve(&layout, file, loc).0)
    }

    /// Attaches the flat concatenation of 20-byte SHA-1 piece hashes (Hot tier only).
    pub fn attach_hashes(&self, hashes: Arc<[u8]>) -> Result<()> {
        let layout = self.0.layout()?;
        if hashes.len() != layout.piece_count() as usize * 20 {
            return Err(StorageError::NoHashes);
        }
        *wr(&self.0.hashes) = Some(hashes);
        Ok(())
    }

    /// Drops the piece hashes (they are only needed while downloading/verifying).
    pub fn detach_hashes(&self) {
        *wr(&self.0.hashes) = None;
    }

    /// Number of partially received pieces currently tracked.
    pub fn open_pieces(&self) -> usize {
        lock(&self.0.pieces).len()
    }

    /// Submits one 16 KiB block (the last block of a piece may be shorter).
    ///
    /// Blocks are copied into a per-piece buffer that is hashed and written once complete. When
    /// the global budget is exhausted, blocks of pieces that did not fit are written through
    /// instead. Blocks must be aligned to [`BLOCK_SIZE`].
    pub fn write_block(&self, piece: u32, begin: u32, data: &[u8]) -> Result<WriteAck> {
        let t = &self.0;
        if t.mode() != Mode::Download {
            return Err(StorageError::ReadOnly);
        }
        if t.write_pause.load(Ordering::Acquire) {
            return Err(StorageError::Busy);
        }
        let layout = t.layout()?;
        let psize = layout.piece_size(piece);
        if psize == 0 {
            return Err(StorageError::OutOfRange);
        }
        if begin % BLOCK_SIZE != 0
            || begin >= psize
            || data.len() != BLOCK_SIZE.min(psize - begin) as usize
        {
            return Err(StorageError::InvalidBlock);
        }
        let expected = t.expected_hash(piece)?;
        let block = (begin / BLOCK_SIZE) as usize;
        // An endgame duplicate that lands after the piece was verified must not open a fresh
        // buffer (it would sit there until the torrent is dropped and could be "completed" again).
        if t.is_verified(piece) {
            return Ok(WriteAck::Duplicate);
        }

        let mut hash_job: Option<HashJob> = None;
        let mut ack = WriteAck::Buffered;
        {
            let mut map = lock(&t.pieces);
            if !map.contains_key(&piece) {
                if map.len() >= t.disk.cfg.max_open_pieces {
                    return Err(StorageError::TooManyPieces);
                }
                let nblocks = layout.block_count(piece);
                let buf = t
                    .disk
                    .budget
                    .try_reserve(psize as usize)
                    .then(|| vec![0u8; psize as usize].into_boxed_slice());
                map.insert(
                    piece,
                    PieceEntry {
                        have: vec![0u64; (nblocks as usize).div_ceil(64)].into_boxed_slice(),
                        received: 0,
                        nblocks,
                        buf,
                        pending_writes: 0,
                        error: None,
                        done: None,
                    },
                );
            }
            let Some(e) = map.get_mut(&piece) else {
                return Err(StorageError::OutOfRange);
            };
            if e.have[block / 64] >> (block % 64) & 1 == 1 {
                return Ok(WriteAck::Duplicate);
            }
            let mut queued_through = false;
            match e.buf.as_mut() {
                Some(buf) => {
                    let b = begin as usize;
                    buf[b..b + data.len()].copy_from_slice(data);
                }
                None => {
                    let job = IoJob::WriteBlock {
                        t: t.clone(),
                        layout: layout.clone(),
                        piece,
                        begin,
                        data: data.into(),
                    };
                    t.inflight_writes.fetch_add(1, Ordering::AcqRel);
                    match t.disk.io.try_send(job) {
                        Ok(()) => queued_through = true,
                        Err(err) => {
                            t.inflight_writes.fetch_sub(1, Ordering::AcqRel);
                            let empty = e.received == 0 && e.pending_writes == 0;
                            if empty {
                                map.remove(&piece);
                            }
                            return Err(match err {
                                cb::TrySendError::Full(_) => StorageError::Busy,
                                cb::TrySendError::Disconnected(_) => StorageError::Shutdown,
                            });
                        }
                    }
                }
            }
            e.have[block / 64] |= 1 << (block % 64);
            e.received += 1;
            if queued_through {
                e.pending_writes += 1;
                t.disk
                    .counters
                    .write_through_blocks
                    .fetch_add(1, Ordering::Relaxed);
            }
            if e.received == e.nblocks {
                let (tx, rx) = oneshot::channel();
                ack = WriteAck::Complete(rx);
                if e.buf.is_some() {
                    if let Some(mut e) = map.remove(&piece) {
                        if let Some(buf) = e.buf.take() {
                            t.inflight_writes.fetch_add(1, Ordering::AcqRel);
                            hash_job = Some(HashJob::Buffer {
                                t: t.clone(),
                                layout: layout.clone(),
                                piece,
                                buf,
                                expected,
                                tx,
                            });
                        }
                    }
                } else if e.pending_writes == 0 {
                    // Only reachable if every through-write already finished.
                    map.remove(&piece);
                    t.inflight_writes.fetch_add(1, Ordering::AcqRel);
                    hash_job = Some(HashJob::Disk {
                        t: t.clone(),
                        layout: layout.clone(),
                        piece,
                        expected,
                        tx,
                    });
                } else {
                    e.done = Some(tx);
                }
            }
        }
        if let Some(job) = hash_job {
            t.disk
                .hash
                .send(job)
                .map_err(|_| StorageError::Shutdown)?;
        }
        Ok(ack)
    }

    /// Forgets a partially received piece and frees its buffer (e.g. after a peer was banned).
    pub fn abort_piece(&self, piece: u32) {
        let mut map = lock(&self.0.pieces);
        if let Some(mut e) = map.remove(&piece) {
            if let Some(b) = e.buf.take() {
                self.0.disk.budget.release(b.len());
            }
            if let Some(tx) = e.done.take() {
                tx.send(Err(StorageError::Cancelled));
            }
        }
    }

    /// Reads `len` bytes at `begin` in `piece` into `buf` (reused: its capacity is kept).
    ///
    /// Only serve pieces whose [`PieceOutcome::Verified`] already resolved. While seeding, a
    /// missing file yields [`StorageError::MissingData`] and nothing is created.
    pub fn read_block(
        &self,
        piece: u32,
        begin: u32,
        len: usize,
        buf: Vec<u8>,
    ) -> Result<Pending<Vec<u8>>> {
        if len == 0 || len > MAX_READ_LEN {
            return Err(StorageError::InvalidBlock);
        }
        let layout = self.0.layout()?;
        if u64::from(begin) + len as u64 > u64::from(layout.piece_size(piece)) {
            return Err(StorageError::OutOfRange);
        }
        let offset = layout.piece_offset(piece) + u64::from(begin);
        let (tx, rx) = oneshot::channel();
        self.0.inflight_reads.fetch_add(1, Ordering::AcqRel);
        self.0
            .disk
            .io
            .try_send(IoJob::Read {
                t: self.0.clone(),
                layout,
                offset,
                len,
                buf,
                tx,
            })
            .map_err(|e| {
                self.0.inflight_reads.fetch_sub(1, Ordering::AcqRel);
                match e {
                    cb::TrySendError::Full(_) => StorageError::Busy,
                    cb::TrySendError::Disconnected(_) => StorageError::Shutdown,
                }
            })?;
        Ok(rx)
    }

    /// `fsync`s every existing file of the torrent.
    pub fn sync(&self) -> Result<Pending<()>> {
        let t = self.0.clone();
        let layout = t.layout()?;
        self.maint(move || {
            for i in 0..layout.file_count() as u32 {
                if layout.file_is_padding(i as usize) || layout.file_len(i as usize) == 0 {
                    continue;
                }
                let (p, _) = t.current_path(&layout, i);
                match File::open(&p) {
                    Ok(f) => f.sync_all().map_err(|e| StorageError::from_io(i, e))?,
                    Err(e) if is_not_found(&e) => {}
                    Err(e) => return Err(StorageError::from_io(i, e)),
                }
            }
            Ok(())
        })
    }

    /// Drops the layout (Cold tier). Fails with [`StorageError::Busy`] while pieces are in flight.
    /// Keeps only the 8-byte digest (and the paths) so [`TorrentHandle::rebuild_layout`] can verify.
    pub fn drop_layout(&self) -> Result<()> {
        if !lock(&self.0.pieces).is_empty() {
            return Err(StorageError::Busy);
        }
        *wr(&self.0.layout) = None;
        *wr(&self.0.hashes) = None;
        *lock(&self.0.verified) = None;
        self.0.disk.fds.invalidate_torrent(self.0.id);
        Ok(())
    }

    /// Re-installs a layout rebuilt from the stored `.torrent`; it must be identical to the original.
    pub fn rebuild_layout(&self, layout: Arc<Layout>) -> Result<()> {
        if layout.digest() != self.0.digest.load(Ordering::Acquire) {
            return Err(StorageError::LayoutMismatch);
        }
        *wr(&self.0.layout) = Some(layout);
        Ok(())
    }

    /// True when the layout is currently resident.
    pub fn has_layout(&self) -> bool {
        rd(&self.0.layout).is_some()
    }

    /// Estimated resident bytes of this torrent's storage state (see README "Memory").
    pub fn resident_bytes(&self) -> usize {
        let paths = rd(&self.0.paths).clone();
        let mut n = std::mem::size_of::<TorrentShared>() + std::mem::size_of::<Paths>();
        n += paths.root.as_os_str().len();
        n += paths.staging.as_ref().map_or(0, |s| s.as_os_str().len());
        if let Some(l) = rd(&self.0.layout).as_ref() {
            n += std::mem::size_of::<Layout>() + l.heap_size();
        }
        n += rd(&self.0.hashes).as_ref().map_or(0, |h| h.len());
        n
    }

    /// Runs `f` on a maintenance thread and returns its result as a future.
    pub(crate) fn maint<T: Send + 'static>(
        &self,
        f: impl FnOnce() -> Result<T> + Send + 'static,
    ) -> Result<Pending<T>> {
        let (tx, rx) = oneshot::channel();
        let job = MaintJob::Run(Box::new(move || tx.send(f())));
        self.0.disk.maint.try_send(job).map_err(|e| match e {
            cb::TrySendError::Full(_) => StorageError::Busy,
            cb::TrySendError::Disconnected(_) => StorageError::Shutdown,
        })?;
        Ok(rx)
    }

    pub(crate) fn shared(&self) -> &Arc<TorrentShared> {
        &self.0
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    #[test]
    fn report_struct_sizes() {
        println!(
            "TorrentShared {} B, Paths {} B, PieceEntry {} B, Layout {} B",
            std::mem::size_of::<TorrentShared>(),
            std::mem::size_of::<Paths>(),
            std::mem::size_of::<PieceEntry>(),
            std::mem::size_of::<Layout>()
        );
        assert!(std::mem::size_of::<TorrentShared>() <= 256);
    }
}
