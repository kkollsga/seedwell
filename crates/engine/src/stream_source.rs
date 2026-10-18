//! Play while downloading (SPEC 7F), the engine side of `trs-stream`'s [`PieceSource`] contract.
//!
//! A torrent that nobody streams owns none of this: the actor's `stream` slot is `None`, the
//! picker's hint list is empty and no task or allocation exists. The first `stream_url` creates a
//! [`StreamBus`] inside the torrent actor; every stream of that torrent shares it through its own
//! [`TorrentSource`].
//!
//! * `has` reads the lock-free `SharedHave` bits the peer tasks already read.
//! * `subscribe` hands out the epoch counter. The actor sets the `have` bit first and bumps the
//!   epoch second (`piece_verified`), the ordering `PieceSource` requires.
//! * `read` goes through the storage handle (fd cache, write cache), split at piece boundaries.
//! * `piece_progress` is a small per-piece counter of received blocks the actor keeps for open
//!   pieces, so a slow piece is never cut off by the 60 s stall rule.
//! * `set_urgent`/`clear` never block: the stream's picture is parked in a mutex-protected map and
//!   the actor is woken with one coalesced message ([`TorrentCmd::StreamHints`]).
//!
//! Web seeds (BEP 19) are NOT part of this: the engine has no web seed client yet. When it gets
//! one, urgent pieces with no fast peer are the ones it should fetch (SPEC 7F.2, see
//! `crates/stream/ENGINE_INTEGRATION.md` section 6).

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::watch;
use trs_stream::{FileRange, Geometry, PieceSource, StreamId, UrgentRange};
use trs_wire::BLOCK_SIZE;

use crate::chan::Sender;
use crate::peer::PeerCtx;
use crate::torrent::{TorrentCmd, TorrentMsg};

/// One stream's picture waiting to be applied by the actor. `ranges == None` clears the stream.
pub(crate) struct PendingHints {
    pub at: Instant,
    pub ranges: Option<Box<[UrgentRange]>>,
}

/// Per-torrent state shared by the actor and the adapters of its streams. Created on the first stream.
pub(crate) struct StreamBus {
    epoch: watch::Sender<u64>,
    /// Blocks received so far of each piece (only open pieces are non-zero).
    progress: Box<[AtomicU16]>,
    pending: Mutex<HashMap<StreamId, PendingHints>>,
    /// A wake-up message is queued (or being handled).
    queued: AtomicBool,
    tx: Sender<TorrentMsg>,
}

impl StreamBus {
    pub(crate) fn new(piece_count: u32, tx: Sender<TorrentMsg>) -> Arc<Self> {
        let (epoch, _) = watch::channel(0u64);
        Arc::new(Self {
            epoch,
            progress: (0..piece_count).map(|_| AtomicU16::new(0)).collect(),
            pending: Mutex::new(HashMap::new()),
            queued: AtomicBool::new(false),
            tx,
        })
    }

    /// A piece became verified (call after the `have` bit is set).
    pub(crate) fn bump(&self) {
        self.epoch.send_modify(|e| *e = e.wrapping_add(1));
    }

    /// Record how many blocks of `piece` have arrived (0 forgets it).
    pub(crate) fn note_progress(&self, piece: u32, blocks: u16) {
        if let Some(a) = self.progress.get(piece as usize) {
            a.store(blocks, Ordering::Relaxed);
        }
    }

    fn park(&self, id: StreamId, h: PendingHints) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id, h);
        if !self.queued.swap(true, Ordering::AcqRel) && self.tx.try_send(TorrentMsg::Cmd(TorrentCmd::StreamHints)).is_err() {
            // Queue full or actor gone: the next call tries again, the picture stays parked.
            self.queued.store(false, Ordering::Release);
        }
    }

    /// The actor takes what streams parked since the last time.
    pub(crate) fn take_pending(&self) -> Vec<(StreamId, PendingHints)> {
        // Clear the flag first: a `park` after the drain below queues a fresh wake-up.
        self.queued.store(false, Ordering::Release);
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).drain().collect()
    }
}

/// The `PieceSource` of one torrent, shared by all its streams.
pub(crate) struct TorrentSource {
    ctx: Arc<PeerCtx>,
    bus: Arc<StreamBus>,
}

impl TorrentSource {
    pub(crate) fn new(ctx: Arc<PeerCtx>, bus: Arc<StreamBus>) -> Self {
        Self { ctx, bus }
    }
}

impl PieceSource for TorrentSource {
    fn geometry(&self) -> Geometry {
        match self.ctx.disk.get() {
            Some(d) => Geometry { piece_length: d.piece_len, total_len: d.total_len },
            None => Geometry { piece_length: 0, total_len: 0 },
        }
    }

    fn has(&self, piece: u32) -> bool {
        self.ctx.disk.get().is_some_and(|d| d.have.get(piece))
    }

    fn subscribe(&self) -> watch::Receiver<u64> {
        self.bus.epoch.subscribe()
    }

    fn piece_progress(&self, piece: u32) -> u32 {
        let Some(d) = self.ctx.disk.get() else { return 0 };
        let blocks = self.bus.progress.get(piece as usize).map_or(0, |a| u32::from(a.load(Ordering::Relaxed)));
        (blocks * BLOCK_SIZE).min(d.piece_size(piece))
    }

    fn read(&self, offset: u64, len: usize, buf: Vec<u8>) -> impl Future<Output = io::Result<Vec<u8>>> + Send {
        let ctx = self.ctx.clone();
        async move {
            let Some(d) = ctx.disk.get() else {
                return Err(io::Error::new(io::ErrorKind::NotFound, "storage is not attached"));
            };
            let mut out = buf;
            out.clear();
            let (mut off, mut left) = (offset, len);
            while left > 0 {
                if off >= d.total_len {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past the end of the torrent"));
                }
                let piece = (off / u64::from(d.piece_len)) as u32;
                let begin = (off - u64::from(piece) * u64::from(d.piece_len)) as u32;
                let n = left.min((d.piece_size(piece) - begin) as usize);
                let pending = d
                    .handle
                    .read_block(piece, begin, n, if out.is_empty() { std::mem::take(&mut out) } else { Vec::new() })
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let got = pending.await.map_err(|e| io::Error::other(e.to_string()))?;
                if got.len() != n {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
                }
                if out.is_empty() {
                    out = got;
                } else {
                    out.extend_from_slice(&got);
                }
                off += n as u64;
                left -= n;
            }
            Ok(out)
        }
    }

    fn set_urgent(&self, stream: StreamId, ranges: &[UrgentRange]) {
        self.bus.park(stream, PendingHints { at: Instant::now(), ranges: Some(ranges.into()) });
    }

    fn clear(&self, stream: StreamId) {
        self.bus.park(stream, PendingHints { at: Instant::now(), ranges: None });
    }
}

/// What the torrent actor hands the session when a stream is opened.
pub(crate) struct StreamParts {
    pub source: Arc<TorrentSource>,
    pub file: FileRange,
}

/// Why the actor could not open a stream.
#[derive(Debug)]
pub(crate) enum OpenError {
    /// The actor is still starting (or rechecking); try again in a moment.
    NotReady,
    /// The torrent has no metadata.
    NoMetadata,
    /// Every piece is present already: nothing to stream.
    Complete,
    /// The file index is out of range, a padding file, or empty.
    BadFile(String),
}
