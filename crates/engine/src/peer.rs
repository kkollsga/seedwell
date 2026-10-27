//! One peer connection: a tokio task that owns the socket.
//!
//! Division of labour (ENGINE.md 2, 4):
//! * The **peer task** frames messages with `trs-wire`, answers `request`s straight from storage,
//!   writes received blocks straight into storage (no hop through the torrent actor), enforces the
//!   rate limits, and keeps its read buffer leased only while bytes flow.
//! * The **torrent actor** makes every decision (what to request, who to unchoke) and sends the
//!   peer small commands.
//!
//! The torrent never awaits a peer: it uses `try_send`, and a peer whose command queue is full is
//! dropped. Peers do await the torrent's event queue, which is the back-pressure that keeps a
//! flooding peer from growing anything without bound.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use trs_utp::UtpSocket;
use trs_storage::{Pending, StorageError, TorrentHandle, WriteAck};
use trs_wire::{
    BLOCK_SIZE, BlockRequest, Decoder, Encoder, ExtPolicy, ExtendedMessage, HANDSHAKE_LEN,
    Handshake, LocalExtendedHandshake, Limits, Message, MessageRef, MetadataMessage, PeerState,
    Reserved,
};

use crate::api::Priority;
use crate::budget::{ConnPermit, HalfOpenPermit};
use crate::chan::{self, Receiver, Sender};
use crate::counters::Counters;
use crate::net::{Dial, PeerIo};
use crate::ratelimit::{Dir, RateLimiter};
use crate::torrent::TorrentMsg;

/// Identifies a peer inside one torrent.
pub(crate) type PeerKey = u32;

/// Read buffers leased while bytes flow (SPEC 5.2, ENGINE.md 4).
pub(crate) struct BufPool {
    free: Mutex<Vec<BytesMut>>,
    max_free: usize,
}

const LEASE_CAPACITY: usize = 32 * 1024;
/// Buffers that grew beyond this (a huge bitfield) are freed, not pooled.
const MAX_POOLED_CAPACITY: usize = 128 * 1024;
/// A connection with no traffic for this long gives its read buffer back.
const RX_RELEASE_AFTER: Duration = Duration::from_secs(5);

impl BufPool {
    pub(crate) fn new(max_free: usize) -> Self {
        Self { free: Mutex::new(Vec::new()), max_free }
    }

    fn lease(&self) -> BytesMut {
        let mut g = self.free.lock().unwrap_or_else(|p| p.into_inner());
        g.pop().unwrap_or_else(|| BytesMut::with_capacity(LEASE_CAPACITY))
    }

    fn give_back(&self, mut b: BytesMut) {
        b.clear();
        if b.capacity() > MAX_POOLED_CAPACITY {
            return;
        }
        let mut g = self.free.lock().unwrap_or_else(|p| p.into_inner());
        if g.len() < self.max_free {
            g.push(b);
        }
    }

    #[cfg(test)]
    pub(crate) fn pooled(&self) -> usize {
        self.free.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// Bits of verified pieces, readable by every peer task without asking the torrent.
// size: 24 bytes + 1 bit per piece
pub(crate) struct SharedHave {
    words: Box<[AtomicU64]>,
    count: u32,
}

impl SharedHave {
    pub(crate) fn new(count: u32) -> Self {
        let n = (count as usize).div_ceil(64);
        Self {
            words: (0..n).map(|_| AtomicU64::new(0)).collect(),
            count,
        }
    }

    pub(crate) fn get(&self, i: u32) -> bool {
        i < self.count
            && self
                .words
                .get((i / 64) as usize)
                .is_some_and(|w| w.load(Ordering::Acquire) >> (i % 64) & 1 == 1)
    }


    pub(crate) fn set(&self, i: u32) {
        if i < self.count
            && let Some(w) = self.words.get((i / 64) as usize) {
                w.fetch_or(1 << (i % 64), Ordering::Release);
            }
    }
}

/// What peers need from storage once the metadata is known.
pub(crate) struct DiskCtx {
    pub handle: TorrentHandle,
    pub have: SharedHave,
    pub piece_count: u32,
    pub piece_len: u32,
    pub total_len: u64,
    /// File geometry (for per-file upload accounting; a move keeps it).
    pub layout: Arc<trs_storage::Layout>,
}

/// Payload bytes served per file since the torrent went live. Allocated on the first block served
/// (a torrent nobody downloads from pays nothing) and gone with the live torrent.
#[derive(Default)]
pub(crate) struct FileUp {
    bytes: OnceLock<Box<[AtomicU64]>>,
}

impl FileUp {
    /// Credits a served block to the files it falls in.
    pub(crate) fn add(&self, layout: &trs_storage::Layout, piece: u32, begin: u32, len: u32) {
        let Some(segs) = layout.piece_segments(piece, begin, len as usize) else { return };
        let v = self.bytes.get_or_init(|| (0..layout.file_count()).map(|_| AtomicU64::new(0)).collect());
        for s in segs.filter(|s| !s.padding) {
            if let Some(c) = v.get(s.file as usize) {
                c.fetch_add(s.len as u64, Ordering::Relaxed);
            }
        }
    }

    /// Bytes served so far per file; `None` when nothing was served yet.
    pub(crate) fn snapshot(&self) -> Option<Vec<u64>> {
        self.bytes.get().map(|v| v.iter().map(|c| c.load(Ordering::Relaxed)).collect())
    }
}

impl DiskCtx {
    pub(crate) fn piece_size(&self, i: u32) -> u32 {
        if i >= self.piece_count {
            return 0;
        }
        if i + 1 == self.piece_count {
            (self.total_len - u64::from(i) * u64::from(self.piece_len)).min(u64::from(self.piece_len)) as u32
        } else {
            self.piece_len
        }
    }
}

/// Diagnostics counters shared with the engine.
#[derive(Default)]
pub(crate) struct Diag {
    pub pex_sent: AtomicU64,
    pub pex_received: AtomicU64,
    pub dht_lookups: AtomicU64,
    pub dht_announces: AtomicU64,
    pub hash_failures: AtomicU64,
    pub peers_banned: AtomicU64,
    pub promotions: AtomicU64,
    pub demotions: AtomicU64,
    pub wakeups: AtomicU64,
    pub incoming_accepted: AtomicU64,
    pub incoming_refused: AtomicU64,
    /// `incoming_refused` by [`Refusal`].
    pub refused: [AtomicU64; 4],
    pub rechecks: AtomicU64,
    pub utp_accepted: AtomicU64,
    pub utp_dialed: AtomicU64,
    pub mse_accepted: AtomicU64,
    pub mse_dialed: AtomicU64,
    pub mse_fallbacks: AtomicU64,
    pub actors_spawned: AtomicU64,
    pub boot_waiting: AtomicU64,
    pub verify_peak: AtomicU64,
    pub replans: AtomicU64,
}

/// Why an incoming connection was refused (`seedwell debug perf`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The connection cap, or too many handshakes pending.
    Busy,
    /// The handshake timed out or was not a BitTorrent one (plain or encrypted).
    Handshake,
    /// The torrent is unknown here, or not running (stopped, queued, moving, shutting down).
    Torrent,
    /// The torrent refused the peer: banned, checking, full, or its queue full.
    Peer,
}

impl Diag {
    /// Count a refused incoming connection.
    pub fn refuse(&self, why: Refusal) {
        self.incoming_refused.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.refused[why as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Per-torrent context shared by all its peer tasks.
pub(crate) struct PeerCtx {
    pub ih: [u8; 20],
    pub our_id: [u8; 20],
    /// `ut_pex` may be advertised, sent and used (the one rule: `policy::pex_allowed`). Live: the
    /// torrent flips it when the global switch or its tracker's setting changes.
    pub pex: AtomicBool,
    pub reserved: Reserved,
    pub disk: OnceLock<DiskCtx>,
    pub counters: Arc<Counters>,
    /// Served bytes per file (lazy, live torrents only).
    pub file_up: FileUp,
    pub limiter: Arc<RateLimiter>,
    /// The torrent's own upload cap from its rule.
    pub torrent_up: crate::ratelimit::TorrentLimit,
    /// The shared caps of the torrent's favorite tracker, if it has limits (SPEC 7E.4).
    pub group: crate::ratelimit::GroupSlot,
    pub priority: AtomicU8,
    pub pool: Arc<BufPool>,
    pub listen_port: Arc<AtomicU32>,
    pub version: String,
    pub metadata_size: AtomicU32,
    pub complete: AtomicBool,
    /// Set while finished files are being moved out of staging: nothing may be served.
    pub hold_uploads: AtomicBool,
    pub connect_timeout: Duration,
    pub handshake_timeout: Duration,
    pub diag: Arc<Diag>,
    /// Encryption policy and the peers that turned out not to speak it.
    pub encryption: crate::config::EncryptionPolicy,
    pub plain_peers: Arc<Mutex<crate::net::AddrSet>>,
    /// The shared uTP socket slot (the socket is replaced by a live port change).
    pub utp: Arc<std::sync::RwLock<Option<UtpSocket>>>,
    /// Connect timeout of the uTP leg when TCP is the fallback (a peer that speaks uTP answers fast).
    pub utp_probe_timeout: Duration,
}

impl PeerCtx {
    /// The current shared UDP socket (uTP), when uTP is on.
    pub(crate) fn utp(&self) -> Option<UtpSocket> {
        self.utp.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The extensions this torrent may use right now.
    pub(crate) fn policy(&self) -> ExtPolicy {
        ExtPolicy { allow_pex: self.pex.load(Ordering::Relaxed), ..ExtPolicy::for_torrent(false) }
    }

    fn prio(&self) -> Priority {
        Priority::from_u8(self.priority.load(Ordering::Relaxed))
    }
}

/// Counters the torrent samples once a second to derive rates.
#[derive(Default)]
pub(crate) struct PeerShared {
    pub down: AtomicU64,
    pub up: AtomicU64,
    /// Give up the connection while it is still being opened (the peer task reads no commands yet).
    pub abort: tokio::sync::Notify,
}

/// Our pieces, as told to a new peer.
pub(crate) enum HaveSummary {
    All,
    None,
    Bitfield(Bytes),
}

/// First command after `Connected`: everything the peer needs to start talking.
pub(crate) struct Welcome {
    pub have: HaveSummary,
    pub allowed_fast: Vec<u32>,
    pub num_pieces: Option<u32>,
}

/// Commands from the torrent actor.
pub(crate) enum PeerCmd {
    /// Boxed with the other big, rare variants: every queued command and every wake-up of the
    /// peer loop carries a `PeerCmd`, so its size is paid per connection.
    Welcome(Box<Welcome>),
    Choke,
    Unchoke,
    Interested(bool),
    Have(u32),
    Requests(Vec<BlockRequest>),
    Cancel(BlockRequest),
    Pex(Vec<u8>),
    /// Send a fresh extended handshake (the extensions we offer changed).
    ReadvertiseExt,
    Metadata(Box<MetaOut>),
    Disconnect,
}

/// A `ut_metadata` message to send.
pub(crate) enum MetaOut {
    Request(u32),
    Reject(u32),
    Data { piece: u32, total: u32, data: Bytes },
}

/// What a received `piece` message turned into inside storage.
pub(crate) enum BlockAck {
    Buffered,
    Duplicate,
    Complete(Pending<trs_storage::PieceOutcome>),
}

/// Messages from a peer.
pub(crate) enum PeerEvent {
    Connected { peer_id: [u8; 20], reserved: Reserved, utp: bool },
    Ext(ExtInfo),
    Choked,
    Unchoked,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Bytes),
    HaveAll,
    HaveNone,
    AllowedFast(u32),
    Block { piece: u32, begin: u32, len: u32, ack: BlockAck },
    Rejected(BlockRequest),
    Pex(Vec<(SocketAddr, bool)>),
    MetaRequest(u32),
    MetaData { piece: u32, total: u32, data: Bytes },
    MetaReject(u32),
    StorageFault(String, FaultKind),
}

/// Fields of the peer's extended handshake the torrent cares about.
pub(crate) struct ExtInfo {
    pub client: Option<String>,
    pub listen_port: Option<u16>,
    pub reqq: Option<u32>,
    pub metadata_size: Option<u32>,
    pub has_metadata: bool,
    pub has_pex: bool,
    pub upload_only: bool,
}

/// Why a peer task ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Close {
    /// Remote closed the connection.
    Eof,
    /// Socket error.
    Io(String),
    /// Malformed or forbidden message.
    Protocol(&'static str),
    /// A timeout elapsed.
    Timeout,
    /// The torrent asked us to leave.
    Local,
    /// Handshake named another torrent or was ours.
    Handshake(&'static str),
    /// The data on disk is gone.
    Storage(String),
}

pub(crate) struct PeerSpawn {
    pub key: PeerKey,
    pub addr: SocketAddr,
    /// An accepted socket together with the peer's handshake, or `None` to connect out.
    /// An accepted connection, its handshake, and whether ours went out already.
    pub incoming: Option<(PeerIo, Handshake, bool)>,
    /// How to connect out (ignored for incoming connections).
    pub dial: Dial,
    pub ctx: Arc<PeerCtx>,
    pub events: Sender<TorrentMsg>,
    pub cmds: Receiver<PeerCmd>,
    pub shared: Arc<PeerShared>,
    pub permit: ConnPermit,
    pub half_open: Option<HalfOpenPermit>,
}

/// Run a peer to completion and report how it ended.
// Takes a box: an async fn keeps its arguments in the task for its whole life, and `PeerSpawn` is
// 250 bytes that the connection no longer needs once it is established.
pub(crate) async fn run(p: Box<PeerSpawn>) {
    let key = p.key;
    let events = p.events.clone();
    // Boxed: the connect/handshake state is big and only needed for the first moments; it must
    // not be part of the future that an idle connection keeps for hours.
    let shared = p.shared.clone();
    let opening = Box::pin(Peer::establish(*p));
    let opened = tokio::select! {
        r = opening => r,
        () = shared.abort.notified() => Err(Close::Local),
    };
    drop(shared);
    let (why, connected) = match opened {
        Ok(mut peer) => {
            let r = peer.main_loop().await;
            peer.release_rx();
            (r.err().unwrap_or(Close::Eof), true)
        }
        Err(c) => (c, false),
    };
    let _ = events.send(TorrentMsg::PeerClosed { key, why, connected }).await;
}

/// Open the transport. TCP waits `connect_timeout`; a uTP leg that has TCP behind it is cut short
/// (a peer that speaks uTP answers the SYN in a round trip, one that does not never will).
async fn connect_transport(ctx: &PeerCtx, addr: SocketAddr, dial: Dial) -> Result<PeerIo, Close> {
    async fn tcp(ctx: &PeerCtx, addr: SocketAddr) -> Result<PeerIo, Close> {
        let s = tokio::time::timeout(ctx.connect_timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| Close::Timeout)?
            .map_err(|e| Close::Io(e.to_string()))?;
        Ok(PeerIo::Tcp(s))
    }
    async fn utp(ctx: &PeerCtx, addr: SocketAddr, limit: Duration) -> Result<PeerIo, Close> {
        let Some(sock) = ctx.utp() else { return Err(Close::Io("uTP is off".into())) };
        let s = tokio::time::timeout(limit, sock.connect(addr))
            .await
            .map_err(|_| Close::Timeout)?
            .map_err(|e| Close::Io(e.to_string()))?;
        Ok(PeerIo::Utp(s))
    }
    match dial {
        Dial::Tcp => tcp(ctx, addr).await,
        Dial::Utp => utp(ctx, addr, ctx.connect_timeout).await,
        Dial::UtpThenTcp => match utp(ctx, addr, ctx.utp_probe_timeout).await {
            Ok(s) => Ok(s),
            Err(_) => tcp(ctx, addr).await,
        },
        Dial::TcpThenUtp => match tcp(ctx, addr).await {
            Ok(s) => Ok(s),
            Err(first) => utp(ctx, addr, ctx.utp_probe_timeout).await.map_err(|_| first),
        },
    }
}

/// Connect and, per the encryption policy, open the encrypted handshake. Returns the connection and
/// whether our BitTorrent handshake has been sent already (as the initial payload of an encrypted
/// handshake). With `Prefer`, a peer that does not answer the encrypted handshake is dialled again
/// plain and remembered, so that the next dial goes straight to plain.
async fn dial_out(ctx: &PeerCtx, addr: SocketAddr, dial: Dial, ours: &Handshake) -> Result<(PeerIo, bool), Close> {
    use crate::config::EncryptionPolicy as Enc;
    let io = connect_transport(ctx, addr, dial).await?;
    let known_plain = ctx.plain_peers.lock().unwrap_or_else(|e| e.into_inner()).contains(&addr);
    if ctx.encryption == Enc::Off || (ctx.encryption == Enc::Prefer && known_plain) {
        return Ok((io, false));
    }
    // (A client that has no MSE usually closes on the first bytes; one that just sits there is
    // not worth more than a few seconds before the plain redial.)
    let attempt = tokio::time::timeout(ctx.handshake_timeout.min(Duration::from_secs(5)), crate::mse::initiate(io, &ctx.ih, &ours.to_bytes(), ctx.encryption)).await;
    match attempt {
        Ok(Ok(io)) => {
            ctx.diag.mse_dialed.fetch_add(1, Ordering::Relaxed);
            Ok((io, true))
        }
        _ if ctx.encryption == Enc::Require => Err(Close::Handshake("encrypted handshake failed")),
        _ => {
            // A client without MSE closes on the Diffie-Hellman bytes. Try again in the clear.
            ctx.diag.mse_fallbacks.fetch_add(1, Ordering::Relaxed);
            let io = connect_transport(ctx, addr, dial).await?;
            ctx.plain_peers.lock().unwrap_or_else(|e| e.into_inner()).insert(addr);
            Ok((io, false))
        }
    }
}

/// Result of [`Peer::emit`].
enum Emit<'a> {
    Done(Option<Result<(), Close>>),
    Wait(std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Close>> + Send + 'a>>),
}

impl std::future::Future for Emit<'_> {
    type Output = Result<(), Close>;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Self::Output> {
        match self.get_mut() {
            Emit::Done(r) => std::task::Poll::Ready(r.take().unwrap_or(Ok(()))),
            Emit::Wait(f) => f.as_mut().poll(cx),
        }
    }
}

/// What woke the peer loop.
enum Wake {
    Cmd(Option<PeerCmd>),
    Readable(std::io::Result<()>),
    Writable(std::io::Result<()>),
    ReadDone(trs_storage::Result<()>),
    Timer,
}

/// Sizes of the per-connection structures, for the `TRS_SIZES` diagnostic.
pub(crate) fn debug_sizes() -> String {
    format!(
        "Peer {} B, PeerSpawn {} B, ReadJob {} B, Run {} B, Decoder {} B, Encoder {} B, PeerState {} B, PeerIo {} B",
        std::mem::size_of::<Peer>(),
        std::mem::size_of::<PeerSpawn>(),
        std::mem::size_of::<ReadJob>(),
        std::mem::size_of::<Run>(),
        std::mem::size_of::<Decoder>(),
        std::mem::size_of::<Encoder>(),
        std::mem::size_of::<PeerState>(),
        std::mem::size_of::<PeerIo>(),
    )
}

/// Poll the read behind the oldest queued block; the result stays in its [`Run`].
fn poll_front(
    reads: &VecDeque<ReadJob>,
    runs: &mut VecDeque<Run>,
    cx: &mut std::task::Context<'_>,
) -> std::task::Poll<trs_storage::Result<()>> {
    use std::task::Poll;
    let Some(job) = reads.front() else { return Poll::Pending };
    let Some(first) = runs.front().map(|r| r.id) else { return Poll::Pending };
    let Some(run) = job.run.checked_sub(first).and_then(|i| usize::try_from(i).ok()).and_then(|i| runs.get_mut(i)) else {
        return Poll::Pending;
    };
    match &mut run.data {
        RunData::Ready(_) => Poll::Ready(Ok(())),
        RunData::Reading(f) => match std::pin::Pin::new(f).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(v)) => {
                run.data = RunData::Ready(Bytes::from(v));
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        },
    }
}

/// What kind of storage failure it was: it decides whether the torrent fails for good, waits for
/// its files to come back, or waits for the disk to accept writes again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FaultKind {
    /// Anything else: the torrent stops with an error until the user acts.
    Other,
    /// Files or their volume are gone (unplugged drive, deleted).
    Missing,
    /// Disk full, read-only volume, permission denied: may clear by itself.
    Blocked,
}

pub(crate) fn fault_kind(e: &StorageError) -> FaultKind {
    if matches!(e, StorageError::MissingData { .. } | StorageError::Truncated { .. } | StorageError::MissingRoot) {
        FaultKind::Missing
    } else if e.is_write_blocked() {
        FaultKind::Blocked
    } else {
        FaultKind::Other
    }
}

/// The same decision for an error that only survives as text ("... (os error 28)").
pub(crate) fn fault_kind_of_text(e: &str) -> FaultKind {
    if e.contains("MissingData") || e.contains("Truncated") || e.contains("MissingRoot") {
        return FaultKind::Missing;
    }
    let code = e
        .rsplit_once("(os error ")
        .and_then(|(_, rest)| rest.split_once(')'))
        .and_then(|(n, _)| n.parse::<i32>().ok());
    if code.is_some_and(trs_storage::os_error_is_write_blocked) { FaultKind::Blocked } else { FaultKind::Other }
}

/// A block a peer asked for, answered in request order.
struct ReadJob {
    req: BlockRequest,
    /// The [`Run`] that holds its bytes ([`NO_RUN`] until the batch of requests was grouped).
    run: u64,
}

const NO_RUN: u64 = u64::MAX;
/// Consecutive blocks of one piece that are read with a single `pread` (a leecher pipelines whole
/// runs of neighbouring blocks, so this turns 8 thread hand-offs into one).
const MAX_RUN: u32 = 128 * 1024;
/// Bytes of storage reads one peer keeps in flight or waiting for the socket. A leecher may queue up
/// to `reqq` blocks (16 MiB at 1000); reading them all at once would pin that much memory per peer,
/// so the rest of the queue is turned into reads as the earlier ones are sent.
const READ_AHEAD: u32 = 1024 * 1024;

/// One storage read that serves one or more [`ReadJob`]s.
struct Run {
    id: u64,
    begin: u32,
    /// Bytes of the read.
    len: u32,
    /// Jobs that have not been sent yet.
    users: u32,
    data: RunData,
}

enum RunData {
    Reading(Pending<Vec<u8>>),
    Ready(Bytes),
}

struct Peer {
    stream: PeerIo,
    ctx: Arc<PeerCtx>,
    key: PeerKey,
    addr: SocketAddr,
    events: Sender<TorrentMsg>,
    cmds: Receiver<PeerCmd>,
    shared: Arc<PeerShared>,
    _permit: ConnPermit,
    dec: Decoder,
    enc: Encoder,
    st: PeerState,
    fast: bool,
    rx: Option<BytesMut>,
    last_rx: Instant,
    last_tx: Instant,
    last_data: Instant,
    /// Buffers were used since they were last released (the timer then trims them once idle).
    bufs_dirty: bool,
    reads: VecDeque<ReadJob>,
    runs: VecDeque<Run>,
    next_run: u64,
    requested: Vec<(u32, u32)>,
    allowed_out: Vec<u32>,
    read_pause_until: Option<Instant>,
    up_pause_until: Option<Instant>,
    misbehaviour: u32,
    /// Our extended handshake to this peer listed `ut_pex` (so it may send us PEX).
    advertised_pex: bool,
    /// Bytes of the encoder's stream handed to the transport so far.
    enc_written: u64,
    /// Piece payload inside the encoder's queue, as `(start, end)` offsets in the same stream, in
    /// order. Uploaded bytes are counted when the transport takes them (SPEC 7B.2), not when queued.
    payload_marks: VecDeque<(u64, u64)>,
}

/// Peers may queue this many outstanding requests with us (twice our advertised `reqq`, for requests that
/// cross a cancel on the wire).
const MAX_QUEUED_READS: usize = 2 * trs_wire::extended::DEFAULT_REQQ as usize;
/// Blocks that arrive without a matching request before we hang up.
const MAX_UNWANTED_BLOCKS: u32 = 64;
const KEEPALIVE_AFTER: Duration = Duration::from_secs(90);
const IDLE_TIMEOUT: Duration = Duration::from_secs(240);
/// The encoder holds at most this much before we stop pulling finished reads.
const ENC_HIGH_WATER: usize = 256 * 1024;
/// Largest `requested` list we keep per peer.
const MAX_REQUESTED: usize = 512;

impl Peer {
    /// TCP connect (outgoing), handshake both ways, announce to the torrent and wait for its
    /// `Welcome`.
    async fn establish(p: PeerSpawn) -> Result<Box<Peer>, Close> {
        let PeerSpawn { key, addr, incoming, dial, ctx, events, mut cmds, shared, permit, half_open } = p;
        let ours = Handshake {
            reserved: ctx.reserved,
            info_hash: ctx.ih,
            peer_id: ctx.our_id,
        };
        // `sent`: an encrypted dial carries our handshake as its initial payload.
        let (mut stream, remote, is_incoming, sent) = match incoming {
            Some((s, hs, answered)) => (s, hs, true, answered),
            None => {
                let (s, sent) = dial_out(&ctx, addr, dial, &ours).await?;
                (s, Handshake::new([0; 20], [0; 20], false), false, sent)
            }
        };
        stream.set_nodelay();
        let hs_deadline = ctx.handshake_timeout;
        let remote = tokio::time::timeout(hs_deadline, async {
            if !sent {
                stream.write_all(&ours.to_bytes()).await.map_err(|e| Close::Io(e.to_string()))?;
            }
            if is_incoming {
                return Ok(remote);
            }
            let mut buf = [0u8; HANDSHAKE_LEN];
            stream.read_exact(&mut buf).await.map_err(|e| Close::Io(e.to_string()))?;
            match Handshake::decode(&buf) {
                Ok(Some(h)) => Ok(h),
                _ => Err(Close::Handshake("bad handshake")),
            }
        })
        .await
        .map_err(|_| Close::Timeout)??;
        if remote.info_hash != ctx.ih {
            return Err(Close::Handshake("wrong info hash"));
        }
        if remote.peer_id == ctx.our_id {
            return Err(Close::Handshake("connected to ourselves"));
        }
        drop(half_open);
        events
            .send(TorrentMsg::Peer {
                key,
                ev: PeerEvent::Connected { peer_id: remote.peer_id, reserved: remote.reserved, utp: stream.is_utp() },
            })
            .await
            .map_err(|_| Close::Local)?;
        let welcome = match tokio::time::timeout(Duration::from_secs(10), cmds.recv()).await {
            Ok(Some(PeerCmd::Welcome(w))) => *w,
            Ok(Some(PeerCmd::Disconnect)) | Ok(None) => return Err(Close::Local),
            _ => return Err(Close::Timeout),
        };
        let limits = welcome.num_pieces.map_or_else(Limits::default, Limits::for_pieces);
        let dec = Decoder::new(limits).negotiated(&ctx.reserved, &remote.reserved);
        let fast = ctx.reserved.both_fast(&remote.reserved);
        let ext = ctx.reserved.both_extension(&remote.reserved);
        let now = Instant::now();
        // On the heap, not in the task: the task's state machine would carry these 480 bytes in a
        // 128-byte-aligned cell for the connection's whole life; either way they are paid once.
        let mut peer = Box::new(Peer {
            stream,
            ctx,
            key,
            addr,
            events,
            cmds,
            shared,
            _permit: permit,
            dec,
            enc: Encoder::new(),
            st: PeerState::new(remote.reserved),
            fast,
            rx: None,
            last_rx: now,
            last_tx: now,
            last_data: now,
            bufs_dirty: true,
            reads: VecDeque::new(),
            runs: VecDeque::new(),
            next_run: 0,
            requested: Vec::new(),
            allowed_out: Vec::new(),
            read_pause_until: None,
            up_pause_until: None,
            misbehaviour: 0,
            advertised_pex: false,
            enc_written: 0,
            payload_marks: VecDeque::new(),
        });
        if ext {
            peer.send_ext_handshake();
        }
        match welcome.have {
            HaveSummary::All if fast => peer.enc.push(&Message::<&[u8]>::HaveAll),
            HaveSummary::None if fast => peer.enc.push(&Message::<&[u8]>::HaveNone),
            HaveSummary::Bitfield(b) => peer.enc.push(&Message::Bitfield(&b[..])),
            // Without Fast an empty bitfield is simply omitted; a full one is sent as bits by the
            // torrent (it never uses `All` for non-fast peers).
            _ => {}
        }
        for pc in welcome.allowed_fast.iter().take(16) {
            if fast {
                peer.enc.push(&Message::<&[u8]>::AllowedFast(*pc));
                peer.allowed_out.push(*pc);
            }
        }
        Ok(peer)
    }

    fn send_ext_handshake(&mut self) {
        let policy = self.ctx.policy();
        self.advertised_pex |= policy.allow_pex;
        let mut h = LocalExtendedHandshake::new(&self.ctx.version, policy);
        let lp = self.ctx.listen_port.load(Ordering::Relaxed);
        h.listen_port = u16::try_from(lp).ok().filter(|&p| p != 0);
        let ms = self.ctx.metadata_size.load(Ordering::Relaxed);
        h.metadata_size = (ms != 0).then_some(ms);
        h.upload_only = self.ctx.complete.load(Ordering::Relaxed);
        h.your_ip = Some(self.addr.ip());
        self.enc.push_extended(0, |b| h.encode_payload(b));
    }

    fn release_rx(&mut self) {
        if let Some(b) = self.rx.take() {
            self.ctx.pool.give_back(b);
        }
    }

    fn deadline(&self, now: Instant) -> Instant {
        let mut d = self.last_tx + KEEPALIVE_AFTER;
        d = d.min(self.last_rx + IDLE_TIMEOUT);
        // The buffer-release deadline only counts when the timer could act on it; otherwise (a
        // half-received frame, a write that does not drain) an expired term would re-arm the timer
        // to "now" on every pass and spin the task.
        let rx_idle = self.rx.as_ref().is_some_and(|b| b.is_empty());
        let trim = self.bufs_dirty && self.enc.is_empty() && self.reads.is_empty();
        if rx_idle || trim {
            d = d.min(self.last_data + RX_RELEASE_AFTER);
        }
        if let Some(t) = self.read_pause_until.filter(|t| *t > now) {
            d = d.min(t);
        }
        if let Some(t) = self.up_pause_until.filter(|t| *t > now) {
            d = d.min(t);
        }
        d.max(now)
    }

    /// The event loop. Hand-written with `poll_fn` over stateless `poll_*` calls instead of
    /// `select!`: the select macro keeps one future per branch (`Notified`, two readiness futures,
    /// ...) inside the task's state machine for as long as the connection lives, which made an idle
    /// peer cost 2 KiB of future alone.
    async fn main_loop(&mut self) -> Result<(), Close> {
        use std::task::Poll;
        let far = Instant::now() + Duration::from_secs(3600);
        let mut timer = Box::pin(tokio::time::sleep_until(far.into()));
        let mut armed = far;
        loop {
            let now = Instant::now();
            let want_read = self.read_pause_until.is_none_or(|t| now >= t);
            let want_write = !self.enc.is_empty() || self.stream.has_pending_write();
            let can_pop = self.reads.front().is_some_and(|j| j.run != NO_RUN)
                && self.up_pause_until.is_none_or(|t| now >= t)
                && self.enc.pending().len() < ENC_HIGH_WATER;
            let dl = self.deadline(now);
            if dl != armed {
                timer.as_mut().reset(dl.into());
                armed = dl;
            }
            // Same priority as the `biased` select it replaces: commands, reads from the socket,
            // writes, finished disk reads, timer.
            let wake = std::future::poll_fn(|cx| {
                if let Poll::Ready(c) = self.cmds.poll_recv(cx) {
                    return Poll::Ready(Wake::Cmd(c));
                }
                if want_read
                    && let Poll::Ready(r) = self.stream.poll_read_ready(cx) {
                        return Poll::Ready(Wake::Readable(r));
                    }
                if want_write
                    && let Poll::Ready(r) = self.stream.poll_write_ready(cx) {
                        return Poll::Ready(Wake::Writable(r));
                    }
                if can_pop
                    && let Poll::Ready(r) = poll_front(&self.reads, &mut self.runs, cx) {
                        return Poll::Ready(Wake::ReadDone(r));
                    }
                if timer.as_mut().poll(cx).is_ready() {
                    return Poll::Ready(Wake::Timer);
                }
                Poll::Pending
            })
            .await;
            match wake {
                Wake::Cmd(None) => return Err(Close::Local),
                Wake::Cmd(Some(c)) => self.on_cmd(c)?,
                Wake::Readable(r) => {
                    r.map_err(|e| Close::Io(e.to_string()))?;
                    Box::pin(self.on_readable()).await?;
                }
                Wake::Writable(r) => {
                    r.map_err(|e| Close::Io(e.to_string()))?;
                    self.on_writable()?;
                }
                Wake::ReadDone(r) => Box::pin(self.on_read_done(r)).await?,
                Wake::Timer => self.on_timer()?,
            }
        }
    }

    fn on_timer(&mut self) -> Result<(), Close> {
        let now = Instant::now();
        if now.saturating_duration_since(self.last_rx) >= IDLE_TIMEOUT {
            return Err(Close::Timeout);
        }
        if now.saturating_duration_since(self.last_tx) >= KEEPALIVE_AFTER {
            self.enc.push(&Message::<&[u8]>::KeepAlive);
            self.last_tx = now;
        }
        if now.saturating_duration_since(self.last_data) >= RX_RELEASE_AFTER {
            if self.rx.as_ref().is_some_and(|b| b.is_empty()) {
                self.release_rx();
            }
            // Idle for a while: the write buffer and the command inbox keep no allocation either
            // (ENGINE.md 4), so a seeded-and-forgotten connection costs its task and little else.
            if self.bufs_dirty && self.enc.is_empty() && self.reads.is_empty() {
                self.enc.release_if_idle();
                self.cmds.release_idle();
                // A deep request queue (up to `reqq`) grew these; both are empty here.
                if self.reads.capacity() > 0 {
                    self.reads = VecDeque::new();
                    self.runs = VecDeque::new();
                }
                self.bufs_dirty = false;
            }
        }
        Ok(())
    }

    fn on_writable(&mut self) -> Result<(), Close> {
        match self.stream.try_write(self.enc.pending()) {
            Ok(n) => {
                self.enc.advance(n);
                self.credit_written(n as u64);
                self.last_tx = Instant::now();
                self.last_data = Instant::now();
                self.bufs_dirty = true;
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(e) => Err(Close::Io(e.to_string())),
        }
    }

    /// `n` more bytes of the queue went to the transport: the piece payload among them is what
    /// we uploaded. Protocol overhead (headers, other messages) never counts, and a block that is
    /// still queued when the connection ends is never counted.
    fn credit_written(&mut self, n: u64) {
        let (from, to) = (self.enc_written, self.enc_written + n);
        self.enc_written = to;
        let mut sent = 0u64;
        while let Some(&(s, e)) = self.payload_marks.front() {
            if s >= to {
                break;
            }
            sent += e.min(to) - s.max(from);
            if e <= to {
                self.payload_marks.pop_front();
            } else {
                break;
            }
        }
        if sent > 0 {
            self.ctx.counters.add_up(sent);
        }
    }

    async fn on_readable(&mut self) -> Result<(), Close> {
        let mut rx = self.rx.take().unwrap_or_else(|| self.ctx.pool.lease());
        let mut rounds = 0;
        let mut result = Ok(());
        while rounds < 8 {
            rounds += 1;
            if rx.capacity() - rx.len() < BLOCK_SIZE as usize + 64 {
                rx.reserve(LEASE_CAPACITY);
            }
            match self.stream.try_read_buf(&mut rx) {
                Ok(0) => {
                    result = Err(Close::Eof);
                    break;
                }
                Ok(n) => {
                    let now = Instant::now();
                    self.last_rx = now;
                    self.last_data = now;
                    self.bufs_dirty = true;
                    if self.ctx.limiter.is_limited(Dir::Down) || self.ctx.group.is_limited(Dir::Down) {
                        let wait = self
                            .ctx
                            .limiter
                            .charge(Dir::Down, self.ctx.prio(), n as u64, now)
                            .max(self.ctx.group.charge(Dir::Down, n as u64, now));
                        if !wait.is_zero() {
                            self.read_pause_until = Some(now + wait);
                        }
                    }
                    if let Err(c) = self.process(&mut rx).await {
                        result = Err(c);
                        break;
                    }
                    if self.read_pause_until.is_some_and(|t| t > now) {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    result = Err(Close::Io(e.to_string()));
                    break;
                }
            }
        }
        if rx.is_empty() {
            // Nothing buffered: an idle peer holds no read buffer (SPEC 5.2).
            self.ctx.pool.give_back(rx);
        } else {
            self.rx = Some(rx);
        }
        result
    }

    /// Decode and dispatch every complete frame in `rx`.
    async fn process(&mut self, rx: &mut BytesMut) -> Result<(), Close> {
        loop {
            let (msg, used) = match self.dec.decode_ref(&rx[..]) {
                Ok(Some(x)) => x,
                Ok(None) => return self.assign_runs(),
                Err(_) => return Err(Close::Protocol("malformed message")),
            };
            self.on_message(msg).await?;
            rx.advance(used);
        }
    }

    /// Tell the torrent. The common case (room in its inbox) completes at once; only a full inbox
    /// waits, in a boxed future, so the send future's 150 bytes are not part of every idle
    /// connection's state machine.
    fn emit(&self, ev: PeerEvent) -> Emit<'_> {
        match self.events.try_send(TorrentMsg::Peer { key: self.key, ev }) {
            Ok(()) => Emit::Done(Some(Ok(()))),
            Err(chan::TrySendError::Closed(_)) => Emit::Done(Some(Err(Close::Local))),
            Err(chan::TrySendError::Full(m)) => Emit::Wait(Box::pin(async move {
                self.events.send(m).await.map_err(|_| Close::Local)
            })),
        }
    }

    async fn on_message(&mut self, msg: MessageRef<'_>) -> Result<(), Close> {
        self.st.on_received(&msg);
        match msg {
            Message::KeepAlive | Message::Port(_) | Message::Unknown(_) | Message::Suggest(_) => {}
            Message::Choke => {
                if !self.fast {
                    // Without Fast the peer dropped everything we asked for.
                    self.requested.clear();
                }
                self.emit(PeerEvent::Choked).await?
            }
            Message::Unchoke => self.emit(PeerEvent::Unchoked).await?,
            Message::Interested => self.emit(PeerEvent::Interested).await?,
            Message::NotInterested => self.emit(PeerEvent::NotInterested).await?,
            Message::Have(i) => self.emit(PeerEvent::Have(i)).await?,
            Message::Bitfield(b) => self.emit(PeerEvent::Bitfield(Bytes::copy_from_slice(b))).await?,
            Message::HaveAll => self.emit(PeerEvent::HaveAll).await?,
            Message::HaveNone => self.emit(PeerEvent::HaveNone).await?,
            Message::AllowedFast(i) => self.emit(PeerEvent::AllowedFast(i)).await?,
            Message::RejectRequest(r) => {
                self.requested.retain(|&(i, b)| !(i == r.index && b == r.begin));
                self.emit(PeerEvent::Rejected(r)).await?;
            }
            Message::Request(r) => self.on_request(r)?,
            Message::Cancel(r) => self.on_peer_cancel(r),
            // Boxed like `on_extended`: a seeder never receives pieces, and the frames of these rare paths
            // would otherwise sit in every idle connection's future.
            Message::Piece { index, begin, data } => Box::pin(self.on_piece(index, begin, data)).await?,
            Message::Extended { id, payload } => Box::pin(self.on_extended(id, payload)).await?,
        }
        Ok(())
    }

    fn on_peer_cancel(&mut self, r: BlockRequest) {
        // Only blocks whose read has not completed can be withdrawn; the rest is already in flight.
        if let Some(pos) = self
            .reads
            .iter()
            .position(|j| j.req == r)
            && let Some(job) = self.reads.remove(pos) {
                self.release_job(&job);
            }
    }

    fn on_request(&mut self, r: BlockRequest) -> Result<(), Close> {
        let Some(d) = self.ctx.disk.get() else {
            return Ok(());
        };
        let reject = |this: &mut Self, r: BlockRequest| {
            if this.fast {
                this.enc.push(&Message::<&[u8]>::RejectRequest(r));
            }
        };
        let size = d.piece_size(r.index);
        if size == 0 || u64::from(r.begin) + u64::from(r.length) > u64::from(size) || r.length == 0 {
            return Err(Close::Protocol("request out of range"));
        }
        if self.st.am_choking && !(self.fast && self.allowed_out.contains(&r.index)) {
            reject(self, r);
            return Ok(());
        }
        if !d.have.get(r.index) || self.ctx.hold_uploads.load(Ordering::Relaxed) {
            reject(self, r);
            return Ok(());
        }
        if self.reads.len() >= MAX_QUEUED_READS {
            return Err(Close::Protocol("too many queued requests"));
        }
        // Not read yet: the reads of one batch of requests are grouped in `assign_runs`.
        self.reads.push_back(ReadJob { req: r, run: NO_RUN });
        Ok(())
    }

    /// Group the requests that arrived in one batch into storage reads: neighbouring blocks of a
    /// piece share one `pread` (up to [`MAX_RUN`]). Transmission and libtorrent pipeline dozens of
    /// consecutive blocks, so this cuts the disk-thread hand-offs by up to 8x.
    fn assign_runs(&mut self) -> Result<(), Close> {
        let Some(d) = self.ctx.disk.get() else { return Ok(()) };
        let mut i = self.reads.len();
        while i > 0 && self.reads[i - 1].run == NO_RUN {
            i -= 1;
        }
        let mut ahead: u32 = self.runs.iter().map(|r| r.len).sum();
        while i < self.reads.len() {
            let first = self.reads[i].req;
            let mut next_begin = first.begin.saturating_add(first.length);
            let mut total = first.length;
            let mut j = i + 1;
            while let Some(job) = self.reads.get(j) {
                let r = job.req;
                if r.index == first.index && r.begin == next_begin && total + r.length <= MAX_RUN {
                    next_begin += r.length;
                    total += r.length;
                    j += 1;
                } else {
                    break;
                }
            }
            if ahead > 0 && ahead.saturating_add(total) > READ_AHEAD {
                // Enough is on its way; `on_read_done` comes back here as blocks leave.
                break;
            }
            match d.handle.read_block(first.index, first.begin, total as usize, Vec::new()) {
                Ok(fut) => {
                    let id = self.next_run;
                    self.next_run += 1;
                    self.runs.push_back(Run { id, begin: first.begin, len: total, users: (j - i) as u32, data: RunData::Reading(fut) });
                    ahead += total;
                    for k in i..j {
                        self.reads[k].run = id;
                    }
                    i = j;
                }
                Err(StorageError::Busy) if ahead > 0 => break,
                Err(StorageError::Busy) => {
                    // The disk queue is full: tell the peer (Fast) and let it ask again.
                    let dropped: Vec<ReadJob> = self.reads.drain(i..j).collect();
                    if self.fast {
                        for job in dropped {
                            self.enc.push(&Message::<&[u8]>::RejectRequest(job.req));
                        }
                    }
                }
                Err(e) => return Err(Close::Storage(e.to_string())),
            }
        }
        Ok(())
    }

    /// Forget a job that will not be sent (cancel, choke) and free its run once nobody needs it.
    fn release_job(&mut self, job: &ReadJob) {
        if job.run == NO_RUN {
            return;
        }
        if let Some(i) = self.run_index(job.run)
            && let Some(r) = self.runs.get_mut(i) {
                r.users = r.users.saturating_sub(1);
            }
        self.trim_runs();
    }

    fn trim_runs(&mut self) {
        while self.runs.front().is_some_and(|r| r.users == 0) {
            self.runs.pop_front();
        }
    }

    fn run_index(&self, id: u64) -> Option<usize> {
        let first = self.runs.front()?.id;
        usize::try_from(id.checked_sub(first)?).ok().filter(|&i| i < self.runs.len())
    }

    /// A storage read finished (or failed): send every block that is ready, in request order.
    async fn on_read_done(&mut self, res: trs_storage::Result<()>) -> Result<(), Close> {
        self.send_ready(res).await?;
        // Blocks left, so there is room to start reads for requests that were held back.
        if self.reads.back().is_some_and(|j| j.run == NO_RUN) {
            self.assign_runs()?;
        }
        Ok(())
    }

    async fn send_ready(&mut self, res: trs_storage::Result<()>) -> Result<(), Close> {
        if let Err(e) = res {
            let _ = self.emit(PeerEvent::StorageFault(e.to_string(), fault_kind(&e))).await;
            return Err(Close::Storage(e.to_string()));
        }
        loop {
            let now = Instant::now();
            if self.enc.pending().len() >= ENC_HIGH_WATER || self.up_pause_until.is_some_and(|t| t > now) {
                return Ok(());
            }
            let Some(job) = self.reads.front() else { return Ok(()) };
            let (req, run_id) = (job.req, job.run);
            if run_id == NO_RUN {
                return Ok(());
            }
            let Some(i) = self.run_index(run_id) else {
                self.reads.pop_front();
                continue;
            };
            let run = &mut self.runs[i];
            if let RunData::Reading(f) = &mut run.data {
                let polled = {
                    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                    std::pin::Pin::new(f).poll(&mut cx)
                };
                match polled {
                    std::task::Poll::Pending => return Ok(()),
                    std::task::Poll::Ready(Ok(v)) => run.data = RunData::Ready(Bytes::from(v)),
                    std::task::Poll::Ready(Err(e)) => {
                        let _ = self.emit(PeerEvent::StorageFault(e.to_string(), fault_kind(&e))).await;
                        return Err(Close::Storage(e.to_string()));
                    }
                }
            }
            let RunData::Ready(bytes) = &run.data else { return Ok(()) };
            let off = req.begin.checked_sub(run.begin).unwrap_or(u32::MAX) as usize;
            let Some(chunk) = bytes.get(off..off.saturating_add(req.length as usize)) else {
                return Err(Close::Storage("short read".into()));
            };
            let at = self.enc_written + self.enc.pending().len() as u64 + 13;
            self.payload_marks.push_back((at, at + chunk.len() as u64));
            self.enc.push_piece(req.index, req.begin, chunk);
            run.users = run.users.saturating_sub(1);
            self.reads.pop_front();
            self.trim_runs();
            let n = u64::from(req.length);
            if let Some(d) = self.ctx.disk.get() {
                self.ctx.file_up.add(&d.layout, req.index, req.begin, req.length);
            }
            self.shared.up.fetch_add(n, Ordering::Relaxed);
            if self.ctx.limiter.is_limited(Dir::Up) || self.ctx.torrent_up.is_limited() || self.ctx.group.is_limited(Dir::Up) {
                let now = Instant::now();
                let global = self.ctx.limiter.charge(Dir::Up, self.ctx.prio(), n, now);
                let own = self.ctx.torrent_up.charge(n, now);
                let tracker = self.ctx.group.charge(Dir::Up, n, now);
                let wait = global.max(own).max(tracker);
                if !wait.is_zero() {
                    self.up_pause_until = Some(now + wait);
                }
            }
        }
    }

    async fn on_piece(&mut self, index: u32, begin: u32, data: &[u8]) -> Result<(), Close> {
        let Some(pos) = self.requested.iter().position(|&(i, b)| i == index && b == begin) else {
            // Late block after a cancel, or unsolicited. Tolerate a few, then hang up.
            self.misbehaviour += 1;
            return if self.misbehaviour > MAX_UNWANTED_BLOCKS {
                Err(Close::Protocol("unsolicited blocks"))
            } else {
                Ok(())
            };
        };
        self.requested.swap_remove(pos);
        let Some(d) = self.ctx.disk.get() else {
            return Ok(());
        };
        if self.ctx.complete.load(Ordering::Relaxed) || d.have.get(index) {
            // A duplicate (endgame) that was in flight when the piece or the torrent verified.
            // Writing it would make storage hash and write the piece a second time.
            return Ok(());
        }
        let size = d.piece_size(index);
        let want = BLOCK_SIZE.min(size.saturating_sub(begin)) as usize;
        if data.len() != want || want == 0 {
            return Err(Close::Protocol("block has the wrong length"));
        }
        let mut tries = 0u32;
        let ack = loop {
            match d.handle.write_block(index, begin, data) {
                Ok(a) => break a,
                Err(StorageError::Busy) if tries < 500 => {
                    tries += 1;
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                Err(StorageError::Busy) => return Err(Close::Storage("disk busy".into())),
                // Storage switched to seeding between our check and the write: harmless.
                Err(StorageError::ReadOnly) => return Ok(()),
                Err(StorageError::TooManyPieces) => {
                    // The torrent opened more pieces than storage allows; give the block back.
                    self.emit(PeerEvent::Rejected(BlockRequest { index, begin, length: want as u32 })).await?;
                    return Ok(());
                }
                Err(e) => {
                    let _ = self.emit(PeerEvent::StorageFault(e.to_string(), fault_kind(&e))).await;
                    return Err(Close::Storage(e.to_string()));
                }
            }
        };
        let ack = match ack {
            WriteAck::Buffered => BlockAck::Buffered,
            WriteAck::Duplicate => BlockAck::Duplicate,
            WriteAck::Complete(p) => BlockAck::Complete(p),
        };
        if !matches!(ack, BlockAck::Duplicate) {
            // Rates only. `downloaded` (announced, ratio) is credited when the piece passes its hash
            // check (`Torrent::piece_verified`), so hash-failed data and pieces in progress never
            // count; duplicates and unsolicited data never reach here.
            self.shared.down.fetch_add(want as u64, Ordering::Relaxed);
        }
        self.emit(PeerEvent::Block { piece: index, begin, len: want as u32, ack }).await
    }

    async fn on_extended(&mut self, id: u8, payload: &[u8]) -> Result<(), Close> {
        let parsed = match ExtendedMessage::parse(id, payload, self.ctx.policy()) {
            Ok(m) => m,
            // PEX switched off after we advertised it: the peer is only late to notice.
            Err(trs_wire::Error::ExtensionDisabled(_)) if self.advertised_pex && id == trs_wire::extended::LOCAL_UT_PEX_ID => return Ok(()),
            Err(trs_wire::Error::ExtensionDisabled(_)) => {
                // e.g. PEX on a private torrent: ignore, but a peer that keeps doing it goes.
                self.misbehaviour += 8;
                return if self.misbehaviour > MAX_UNWANTED_BLOCKS {
                    Err(Close::Protocol("forbidden extension"))
                } else {
                    Ok(())
                };
            }
            Err(_) => return Err(Close::Protocol("malformed extended message")),
        };
        match parsed {
            ExtendedMessage::Handshake(h) => {
                self.st.apply_extended_handshake(&h);
                let info = ExtInfo {
                    client: h.client_lossy().map(|c| c.into_owned()),
                    listen_port: h.listen_port,
                    reqq: h.reqq,
                    metadata_size: h.metadata_size,
                    has_metadata: h.ut_metadata.is_some(),
                    // What the peer offers; whether we use it is the torrent's call (it can change).
                    has_pex: h.ut_pex.is_some(),
                    upload_only: h.upload_only.unwrap_or(false),
                };
                self.emit(PeerEvent::Ext(info)).await
            }
            ExtendedMessage::Metadata(m) => match m {
                MetadataMessage::Request { piece } => self.emit(PeerEvent::MetaRequest(piece)).await,
                MetadataMessage::Data { piece, total_size, data } => {
                    self.emit(PeerEvent::MetaData {
                        piece,
                        total: total_size,
                        data: Bytes::copy_from_slice(data),
                    })
                    .await
                }
                MetadataMessage::Reject { piece } => self.emit(PeerEvent::MetaReject(piece)).await,
                MetadataMessage::Unknown { .. } => Ok(()),
            },
            ExtendedMessage::Pex(m) => {
                if !self.ctx.pex.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let addrs: Vec<(SocketAddr, bool)> =
                    m.added().map(|p| (p.addr, p.flags.has(trs_wire::PexFlags::UTP))).take(200).collect();
                if addrs.is_empty() {
                    return Ok(());
                }
                self.emit(PeerEvent::Pex(addrs)).await
            }
            ExtendedMessage::Unknown { .. } => Ok(()),
        }
    }

    fn on_cmd(&mut self, c: PeerCmd) -> Result<(), Close> {
        self.last_data = Instant::now();
        self.bufs_dirty = true;
        match c {
            PeerCmd::Welcome(_) => {}
            PeerCmd::Choke => {
                if !self.st.am_choking {
                    let m = Message::<&[u8]>::Choke;
                    self.st.on_sent(&m);
                    self.enc.push(&m);
                    // Queued reads are dropped; with Fast the peer is told which ones.
                    while let Some(j) = self.reads.pop_front() {
                        if self.fast && !self.allowed_out.contains(&j.req.index) {
                            self.enc.push(&Message::<&[u8]>::RejectRequest(j.req));
                        }
                    }
                    self.runs.clear();
                }
            }
            PeerCmd::Unchoke => {
                if self.st.am_choking {
                    let m = Message::<&[u8]>::Unchoke;
                    self.st.on_sent(&m);
                    self.enc.push(&m);
                }
            }
            PeerCmd::Interested(on) => {
                if self.st.am_interested != on {
                    let m = if on { Message::<&[u8]>::Interested } else { Message::NotInterested };
                    self.st.on_sent(&m);
                    self.enc.push(&m);
                }
            }
            PeerCmd::Have(i) => self.enc.push(&Message::<&[u8]>::Have(i)),
            PeerCmd::Requests(v) => {
                for r in v {
                    if self.requested.len() >= MAX_REQUESTED {
                        return Err(Close::Protocol("request queue overflow"));
                    }
                    self.requested.push((r.index, r.begin));
                    self.enc.push(&Message::<&[u8]>::Request(r));
                }
            }
            PeerCmd::Cancel(r) => {
                self.requested.retain(|&(i, b)| !(i == r.index && b == r.begin));
                self.enc.push(&Message::<&[u8]>::Cancel(r));
            }
            // PEX was switched on after the handshake: tell the peer (BEP 10 allows a new handshake).
            PeerCmd::ReadvertiseExt => {
                if !self.advertised_pex && self.ctx.pex.load(Ordering::Relaxed) {
                    self.send_ext_handshake();
                }
            }
            PeerCmd::Pex(payload) => {
                if self.st.remote_ut_pex != 0 && self.ctx.pex.load(Ordering::Relaxed) {
                    self.enc.push_extended(self.st.remote_ut_pex, |b| {
                        bytes::BufMut::put_slice(b, &payload);
                    });
                    self.ctx.diag.pex_sent.fetch_add(1, Ordering::Relaxed);
                }
            }
            PeerCmd::Metadata(m) => {
                if self.st.remote_ut_metadata != 0 {
                    let msg = match &*m {
                        MetaOut::Request(p) => MetadataMessage::Request { piece: *p },
                        MetaOut::Reject(p) => MetadataMessage::Reject { piece: *p },
                        MetaOut::Data { piece, total, data } => MetadataMessage::Data {
                            piece: *piece,
                            total_size: *total,
                            data,
                        },
                    };
                    self.enc.push_extended(self.st.remote_ut_metadata, |b| msg.encode_payload(b));
                }
            }
            PeerCmd::Disconnect => return Err(Close::Local),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn served_blocks_are_credited_to_the_files_they_fall_in() {
        use trs_storage::{FilePath, FileSpec, Layout};
        let spec = |n: &str, len: u64, pad: bool| FileSpec { path: FilePath::Relative(vec![n.into()]), length: len, is_padding: pad };
        // Pieces of 32: a is bytes 0..40, a padding file 40..64, b 64..100.
        let l = Layout::new(32, 100, 4, vec![spec("a", 40, false), spec("pad", 24, true), spec("b", 36, false)]).unwrap();
        let up = FileUp::default();
        assert!(up.snapshot().is_none(), "nothing allocated before the first block");
        up.add(&l, 0, 0, 32);
        // Piece 1 starts at 32: 8 bytes of a, then 24 of padding (not counted).
        up.add(&l, 1, 0, 32);
        // Piece 2 is all b; a block in the last piece (4 bytes) too.
        up.add(&l, 2, 16, 16);
        up.add(&l, 3, 0, 4);
        assert_eq!(up.snapshot().unwrap(), vec![40, 0, 16 + 4]);
        // Out of range blocks are ignored.
        up.add(&l, 9, 0, 16);
        assert_eq!(up.snapshot().unwrap(), vec![40, 0, 20]);
    }

    #[test]
    fn shared_have_sets_and_bounds() {
        let h = SharedHave::new(130);
        assert!(!h.get(0));
        h.set(0);
        h.set(129);
        h.set(130);
        assert!(h.get(0) && h.get(129));
        assert!(!h.get(130) && !h.get(1000));
    }

    #[test]
    fn pool_recycles_and_caps() {
        let p = BufPool::new(2);
        let a = p.lease();
        let b = p.lease();
        let c = p.lease();
        p.give_back(a);
        p.give_back(b);
        p.give_back(c);
        assert_eq!(p.pooled(), 2);
        let _ = p.lease();
        assert_eq!(p.pooled(), 1);
        let mut big = BytesMut::with_capacity(MAX_POOLED_CAPACITY + 1);
        big.extend_from_slice(&[1]);
        p.give_back(big);
        assert_eq!(p.pooled(), 1, "oversized buffers are not pooled");
    }
}
