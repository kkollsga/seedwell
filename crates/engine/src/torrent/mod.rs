//! The torrent actor: one tokio task per awake (Warm or Hot) torrent.
//!
//! It owns the picker, the choker and the peer table, and talks to
//! * its **peer tasks** through bounded command queues (`try_send`, never awaited) and one shared
//!   event queue (peers await it, which gives back-pressure),
//! * the **session** through [`TorrentReport`]s, and
//! * **storage** through the `trs-storage` handle.
//!
//! Files: `startup` (load, validate, recheck), `events` (peer events, requests), `maintenance`
//! (ticks, choking, tiers, resume), `magnet` (BEP 9), `pex` (BEP 11).

mod events;
mod magnet;
mod maintenance;
mod pex;
mod startup;
mod streaming;

use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::oneshot;
use trs_storage::{Layout, PieceOutcome};
use trs_wire::{Bitfield, BlockRequest, ExtPolicy, Handshake, Reserved};

use crate::api::{FileInfo, PeerInfo, Priority, TorrentId};
use crate::budget::ConnPermit;
use crate::chan::{Receiver, Sender};
use crate::choker::{Choker, Mode};
use crate::config::EngineConfig;
use crate::counters::Counters;
use crate::peer::{Close, PeerCmd, PeerCtx, PeerEvent, PeerKey, PeerShared};
use crate::persist::ResumeRecord;
use crate::picker::Picker;
use crate::session::SessionMsg;
use crate::shared::Shared;
use crate::tier::Tier;
use crate::util::Rng;

pub(crate) use magnet::MetaFetch;
pub(crate) use startup::build_layout;

/// Messages a torrent actor receives.
pub(crate) enum TorrentMsg {
    Peer { key: PeerKey, ev: PeerEvent },
    PeerClosed { key: PeerKey, why: Close, connected: bool },
    PieceHashed { piece: u32, result: Result<PieceOutcome, String> },
    Incoming(Box<IncomingConn>),
    AddPeers(Vec<SocketAddr>),
    /// Peers from our own announce, with the leecher count the tracker reported.
    TrackerPeers(Vec<SocketAddr>, Option<u64>),
    Cmd(TorrentCmd),
    InfoBytes(Result<Arc<[u8]>, String>),
    Fingerprints(Vec<Option<[u8; 20]>>),
    Finalized(Result<(), String>),
    /// The network is back after an outage or a wake (SPEC 7C.4c): a torrent with nobody
    /// connected forgets its dial backoff and dials again.
    NetRestored,
}

/// An accepted socket whose handshake was read; boxed so idle inboxes stay small.
pub(crate) struct IncomingConn {
    pub stream: crate::net::PeerIo,
    pub hs: Handshake,
    pub addr: SocketAddr,
    pub permit: ConnPermit,
}

/// Commands from the session.
pub(crate) enum TorrentCmd {
    /// Stop everything and reply with what the session needs to persist.
    Shutdown(oneshot::Sender<ShutdownInfo>),
    SetFilePriorities(Vec<u8>),
    SetPriority(Priority),
    /// Per-torrent upload cap (rule `up_limit`), bytes/s.
    SetUpLimit(Option<u64>),
    /// Join or leave the shared caps of a favorite tracker.
    SetGroup(Option<Arc<crate::ratelimit::GroupBucket>>),
    ConfigChanged,
    Detail(oneshot::Sender<DetailParts>),
    /// The files live somewhere else now (a move finished, or the placement changed): use the
    /// new layout from the next I/O on.
    SwitchLocation(Box<Switch>),
    /// The storage handle of the running torrent (a same-volume move uses it, so reads that
    /// meet a file in transit wait for the switch instead of failing).
    Handle(oneshot::Sender<Option<trs_storage::TorrentHandle>>),
    /// Open a stream of a file (SPEC 7F): answers with the source the streaming server reads from.
    StreamOpen { file: usize, reply: oneshot::Sender<Result<crate::stream_source::StreamParts, crate::stream_source::OpenError>> },
    /// A stream parked a new picture of the pieces it needs (coalesced; see `StreamBus`).
    StreamHints,
}

/// A change of where the torrent's files are (SPEC 7A.5). The session already persisted it.
pub(crate) struct Switch {
    /// The layout with the new paths (same geometry), built like a fresh start would build it.
    pub layout: Arc<Layout>,
    /// Folder of the torrent's own layout.
    pub save_path: PathBuf,
    /// Staging folder (`<root>/<info-hash>`) of unfinished data.
    pub staging: Option<PathBuf>,
    /// The organizer's per-file destinations.
    pub placement: Option<crate::persist::Placement>,
    /// Files to rename as part of the switch (edge files, `edgefix`): done by storage under the
    /// same gate that installs the layout, so no write can open the old path in between.
    pub renames: Vec<(PathBuf, PathBuf)>,
    /// Paths that must still be absent when the switch happens (a file that was never written
    /// and is about to be named elsewhere).
    pub absent: Vec<PathBuf>,
    /// Answered once the switch is made (`Err` = the storage layer refused).
    pub reply: Option<oneshot::Sender<Result<(), String>>>,
}

/// The reason a switch that renames files is refused while the finished files are being moved
/// (the session tries again a little later).
pub(crate) const SWITCH_BUSY: &str = "the torrent is moving its finished files";

impl Switch {
    /// A switch that only re-points the layout (a move).
    pub fn plain(
        layout: Arc<Layout>,
        save_path: PathBuf,
        staging: Option<PathBuf>,
        placement: Option<crate::persist::Placement>,
        reply: Option<oneshot::Sender<Result<(), String>>>,
    ) -> Self {
        Self { layout, save_path, staging, placement, renames: Vec::new(), absent: Vec::new(), reply }
    }
}

/// Final state handed back when a torrent actor stops.
pub(crate) struct ShutdownInfo {
    pub resume: Option<ResumeRecord>,
    pub peers: Vec<SocketAddr>,
}

/// Parts of `TorrentDetail` only the actor knows.
pub(crate) struct DetailParts {
    pub files: Vec<FileInfo>,
    pub peers: Vec<PeerInfo>,
    pub piece_count: u32,
    pub piece_length: u32,
    pub comment: Option<String>,
}

/// What the session learns about a torrent that finished loading.
pub(crate) struct ReadyInfo {
    pub name: String,
    pub total_length: u64,
    pub wanted_length: u64,
    pub piece_length: u32,
    pub private: bool,
    pub complete: bool,
    pub wanted_done: bool,
    pub announce: Vec<Vec<String>>,
    pub staged: bool,
}

/// Reports to the session.
pub(crate) enum TorrentReport {
    Ready(Box<ReadyInfo>),
    /// The wanted files are done; `complete` says every piece is verified.
    Finished { complete: bool, files_completed: u32, files_bytes: u64 },
    /// The wanted set grew after the wanted files were done (files ticked again): the torrent is
    /// downloading again until [`TorrentReport::Finished`] says otherwise.
    Unfinished,
    Resume(ResumeRecord),
    MetadataReady { info: Vec<u8>, peers: Vec<SocketAddr> },
    Fault { kind: crate::peer::FaultKind, message: String },
    /// A write failed because the disk is full, read-only or not permitted. The actor stays up:
    /// it stops asking for blocks and keeps serving the pieces it has; the session retries later.
    DiskBlocked { message: String },
    Demote,
    WantPeers,
    Banned,
    /// The finished files were moved out of `.incomplete`.
    Finalized,
}

/// Numbers the session reads without asking.
// size: ~120 bytes
pub(crate) struct LiveStats {
    pub down_rate: AtomicU64,
    pub up_rate: AtomicU64,
    pub peers: AtomicU32,
    pub seeds: AtomicU32,
    /// Peer addresses known (connected or still to try).
    pub known: AtomicU32,
    pub interested: AtomicU32,
    pub uploading: AtomicU32,
    pub done_bytes: AtomicU64,
    pub wanted_done: AtomicBool,
    pub verifying: AtomicBool,
    /// Needs to verify its files and waits for one of the verification slots (`Shared::verify_slot`).
    pub verify_wait: AtomicBool,
    pub ready: AtomicBool,
    pub tier: AtomicU8,
}

impl LiveStats {
    pub(crate) fn new(tier: Tier) -> Self {
        Self {
            down_rate: AtomicU64::new(0),
            up_rate: AtomicU64::new(0),
            peers: AtomicU32::new(0),
            seeds: AtomicU32::new(0),
            known: AtomicU32::new(0),
            interested: AtomicU32::new(0),
            uploading: AtomicU32::new(0),
            done_bytes: AtomicU64::new(0),
            wanted_done: AtomicBool::new(false),
            verifying: AtomicBool::new(false),
            verify_wait: AtomicBool::new(false),
            ready: AtomicBool::new(false),
            tier: AtomicU8::new(tier_to_u8(tier)),
        }
    }

    pub(crate) fn tier(&self) -> Tier {
        match self.tier.load(Ordering::Relaxed) {
            0 => Tier::Cold,
            1 => Tier::Warm,
            _ => Tier::Hot,
        }
    }
}

fn tier_to_u8(t: Tier) -> u8 {
    match t {
        Tier::Cold => 0,
        Tier::Warm => 1,
        Tier::Hot => 2,
    }
}

/// Everything needed to start an actor.
pub(crate) struct TorrentInit {
    pub id: TorrentId,
    pub epoch: u64,
    pub shared: Arc<Shared>,
    pub private: bool,
    pub has_metadata: bool,
    pub save_path: PathBuf,
    pub staged: bool,
    pub file_prios: Vec<u8>,
    pub priority: Priority,
    pub resume: Option<ResumeRecord>,
    pub counters: Arc<Counters>,
    pub download_allowed: bool,
    pub fresh: bool,
    pub force_recheck: bool,
    pub peers: Vec<SocketAddr>,
    pub first: Vec<TorrentMsg>,
    pub up_limit: Option<u64>,
    /// The shared caps of the torrent's favorite tracker, if any.
    pub group: Option<Arc<crate::ratelimit::GroupBucket>>,
    /// Library paths chosen by the organizer (place & rename).
    pub placement: Option<crate::persist::Placement>,
    /// Files that exist under two edge names (see `edgefix`).
    pub edge_conflicts: Vec<crate::edgefix::Conflict>,
    /// Where unfinished data lives while `staged`.
    pub staging: Option<PathBuf>,
    pub tier: Tier,
    pub rx: Receiver<TorrentMsg>,
    pub tx: Sender<TorrentMsg>,
    pub live: Arc<LiveStats>,
}

/// Peers we refuse (bounded; see `events::MAX_BANNED`).
#[derive(Default)]
pub(crate) struct Banned {
    pub ips: HashSet<IpAddr>,
    pub addrs: HashSet<SocketAddr>,
    pub ids: HashSet<[u8; 20]>,
}

/// Candidate peer addresses (bounded).
pub(crate) struct PeerPool {
    queue: VecDeque<SocketAddr>,
    known: HashSet<SocketAddr>,
    fails: HashMap<SocketAddr, (u8, Instant)>,
    /// Addresses that were complete peers when we had nothing to download: not dialled while we
    /// are upload-only (nothing to trade), usable again the moment we want pieces.
    seeds: HashSet<SocketAddr>,
}

/// Most addresses remembered per torrent.
const MAX_KNOWN_PEERS: usize = 512;
/// Connection attempts per address before it is forgotten.
const MAX_ATTEMPTS: u8 = 4;

impl PeerPool {
    fn new() -> Self {
        Self { queue: VecDeque::new(), known: HashSet::new(), fails: HashMap::new(), seeds: HashSet::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.known.len()
    }

    /// Add a candidate; returns whether it was new and acceptable.
    pub(crate) fn add(&mut self, a: SocketAddr) -> bool {
        let ip = a.ip();
        if a.port() == 0 || ip.is_multicast() || ip.is_unspecified() {
            return false;
        }
        if self.known.len() >= MAX_KNOWN_PEERS || !self.known.insert(a) {
            return false;
        }
        self.queue.push_back(a);
        true
    }

    /// An address whose dial failed for want of a network: back in the queue, no strike.
    pub(crate) fn requeue(&mut self, a: SocketAddr) {
        if self.known.contains(&a) && !self.queue.contains(&a) {
            self.queue.push_back(a);
        }
    }

    /// Forget every strike and wait (the network is back).
    pub(crate) fn clear_backoff(&mut self) {
        self.fails.clear();
    }

    /// Put an address back for a later retry.
    fn retry_later(&mut self, a: SocketAddr, after: Duration, now: Instant) {
        let e = self.fails.entry(a).or_insert((0, now));
        e.0 += 1;
        e.1 = now + after * u32::from(e.0);
        if e.0 >= MAX_ATTEMPTS {
            self.fails.remove(&a);
            self.known.remove(&a);
            self.seeds.remove(&a);
        } else if self.known.contains(&a) && !self.queue.contains(&a) {
            self.queue.push_back(a);
        }
    }

    /// Next address that is allowed to be tried now.
    fn next(
        &mut self,
        now: Instant,
        skip: &HashSet<SocketAddr>,
        banned: &HashSet<IpAddr>,
        upload_only: bool,
    ) -> Option<SocketAddr> {
        for _ in 0..self.queue.len() {
            let a = self.queue.pop_front()?;
            if !self.known.contains(&a) || banned.contains(&a.ip()) {
                self.known.remove(&a);
                self.seeds.remove(&a);
                continue;
            }
            if upload_only && self.seeds.contains(&a) {
                // Dropped from the queue; `release_seeds` puts it back when we want pieces again.
                continue;
            }
            if skip.contains(&a) || self.fails.get(&a).is_some_and(|f| f.1 > now) {
                self.queue.push_back(a);
                continue;
            }
            return Some(a);
        }
        None
    }

    /// Addresses remembered as seeds.
    pub(crate) fn seed_count(&self) -> usize {
        self.seeds.len()
    }

    fn add_front(&mut self, a: SocketAddr) {
        if self.known.contains(&a) {
            self.queue.push_front(a);
        }
    }

    fn forget(&mut self, a: &SocketAddr) {
        self.known.remove(a);
        self.fails.remove(a);
        self.seeds.remove(a);
    }

    /// Remember that this address holds everything we have (no use to us while upload-only).
    pub(crate) fn mark_seed(&mut self, a: SocketAddr) {
        if self.known.contains(&a) {
            self.seeds.insert(a);
        }
    }

    /// We want pieces again: every remembered seed is a candidate once more.
    pub(crate) fn release_seeds(&mut self) {
        let seeds = std::mem::take(&mut self.seeds);
        for a in seeds {
            if self.known.contains(&a) && !self.queue.contains(&a) {
                self.queue.push_back(a);
            }
        }
    }
}

/// What we know about the pieces a peer has.
pub(crate) enum PeerHave {
    /// No bitfield yet.
    Unknown,
    /// Everything.
    All,
    /// A set we track (only while we download).
    Set(Bitfield),
    /// Only a count (we are complete and need nothing from them).
    Count(u32),
}

/// An exponentially smoothed byte rate.
#[derive(Clone, Copy, Default)]
pub(crate) struct Rate {
    last: u64,
    pub per_sec: f64,
}

impl Rate {
    pub(crate) fn update(&mut self, total: u64, secs: f64) {
        let inst = total.saturating_sub(self.last) as f64 / secs.max(0.001);
        self.last = total;
        self.per_sec = if self.per_sec == 0.0 { inst } else { self.per_sec * 0.6 + inst * 0.4 };
        // The smoothing only approaches zero: an idle connection reads exactly 0 again.
        if self.per_sec < 1.0 {
            self.per_sec = 0.0;
        }
    }
}

/// A connected (or connecting) peer, torrent side.
// size: ~300 bytes + inflight list + optional bitfield
pub(crate) struct PeerEntry {
    pub tx: Sender<PeerCmd>,
    pub shared: Arc<PeerShared>,
    pub addr: SocketAddr,
    pub listen_addr: Option<SocketAddr>,
    pub incoming: bool,
    /// The connection runs over uTP.
    pub utp: bool,
    pub connected: bool,
    pub peer_id: [u8; 20],
    pub fast: bool,
    pub am_choking: bool,
    pub am_interested: bool,
    pub peer_choking: bool,
    pub peer_interested: bool,
    pub have: PeerHave,
    pub inflight: Vec<BlockRequest>,
    pub allowed_in: Vec<u32>,
    pub depth: u32,
    pub down: Rate,
    pub up: Rate,
    pub connected_at: Instant,
    pub last_block: Instant,
    /// The peer's last HAVE (it is still downloading, from anyone, while these keep coming).
    pub last_have: Option<Instant>,
    pub last_unchoked: Option<Instant>,
    pub snubbed_at: Option<Instant>,
    pub strikes: u32,
    pub client: Option<Box<str>>,
    pub upload_only: bool,
    pub has_pex: bool,
    pub has_metadata: bool,
    pub meta_size: u32,
    pub reqq: u32,
    pub pex_seq: u64,
    pub next_pex: Instant,
    pub no_refill_until: Option<Instant>,
}

/// Things that exist once the metadata is known.
pub(crate) struct Meta {
    pub layout: Arc<Layout>,
    pub comment: Option<String>,
    pub piece_count: u32,
    pub piece_len: u32,
    pub total_len: u64,
    /// Bytes of the wanted files (non-padding, priority above zero).
    pub wanted_len: u64,
    pub file_prio: Vec<u8>,
    pub picker: Option<Picker>,
    pub hashes: Option<Arc<[u8]>>,
    pub staged: bool,
}

/// The actor.
pub(crate) struct Torrent {
    pub id: TorrentId,
    pub epoch: u64,
    pub sh: Arc<Shared>,
    pub cfg: Arc<EngineConfig>,
    pub tx: Sender<TorrentMsg>,
    pub rx: Receiver<TorrentMsg>,
    pub ctx: Arc<PeerCtx>,
    pub live: Arc<LiveStats>,
    pub counters: Arc<Counters>,
    pub private: bool,
    pub save_path: PathBuf,
    pub staging: Option<PathBuf>,
    pub placement: Option<crate::persist::Placement>,
    pub priority: Priority,
    pub meta: Option<Meta>,
    pub peers: HashMap<PeerKey, PeerEntry>,
    pub next_key: PeerKey,
    pub addrs_in_use: HashSet<SocketAddr>,
    pub pool: PeerPool,
    pub banned: Banned,
    pub download_allowed: bool,
    /// A write failed (disk full / read-only / denied): no more requests until the session restarts us.
    pub disk_blocked: bool,
    pub downloading: bool,
    pub finished_reported: bool,
    pub tier: Tier,
    pub choker: Choker,
    pub slots: usize,
    /// Outgoing attempts a seeding torrent may still make now, and when it last earned one.
    pub seed_dials: u8,
    pub seed_dial_at: Instant,
    /// The last tracker response said nobody is downloading: an upload-only torrent dials no one.
    pub no_leechers: bool,
    /// When `slots` was last taken from the allocator.
    pub slots_at: Instant,
    pub last_rechoke: Instant,
    pub contrib: HashMap<u32, Vec<PeerKey>>,
    pub last_demand: Instant,
    pub uploading_since: Option<Instant>,
    pub rng: Rng,
    pub last_tick: Instant,
    pub last_resume: Instant,
    pub resume_dirty: bool,
    pub demote_sent: bool,
    /// The finished files are being moved out of `.incomplete`.
    pub finalizing: bool,
    pub shutting_down: bool,
    pub meta_fetch: Option<MetaFetch>,
    pub info_cache: Option<(Instant, Arc<[u8]>)>,
    pub info_loading: bool,
    pub meta_requests: Vec<(PeerKey, u32)>,
    pub pex_log: pex::PexLog,
    pub last_want_peers: Option<Instant>,
    pub checking: bool,
    pub start: Option<startup::StartParams>,
    pub pending_prios: Option<Vec<u8>>,
    /// A location switch that arrived while the actor was still starting up.
    pub pending_switch: Option<Box<Switch>>,
    /// Last known file fingerprints, kept so periodic resume data does not lose them.
    pub fps: Option<Vec<Option<[u8; 20]>>>,
    /// Present from the first stream on; a torrent nobody streams has `None` and pays nothing.
    pub stream: Option<std::sync::Arc<crate::stream_source::StreamBus>>,
    /// A stream has live hints: bandwidth weight High.
    pub stream_boost: bool,
}

impl Torrent {
    fn new(init: TorrentInit) -> Self {
        let cfg = init.shared.cfg();
        let private = init.private;
        let policy = ExtPolicy::for_torrent(private || !cfg.pex_enabled);
        let up_limit = init.up_limit;
        let ctx = Arc::new(PeerCtx {
            ih: init.id,
            our_id: init.shared.our_id,
            policy,
            reserved: Reserved::ours_with(private, cfg.dht_enabled && init.shared.dht_running.load(Ordering::Relaxed)),
            disk: std::sync::OnceLock::new(),
            counters: init.counters.clone(),
            file_up: Default::default(),
            limiter: init.shared.limiter.clone(),
            torrent_up: crate::ratelimit::TorrentLimit::default(),
            group: crate::ratelimit::GroupSlot::default(),
            priority: AtomicU8::new(init.priority.to_u8()),
            pool: init.shared.pool.clone(),
            listen_port: AtomicU32::new(u32::from(init.shared.port())),
            version: cfg.client_version.clone(),
            metadata_size: AtomicU32::new(0),
            complete: AtomicBool::new(false),
            hold_uploads: AtomicBool::new(false),
            connect_timeout: cfg.connect_timeout,
            handshake_timeout: cfg.handshake_timeout,
            diag: init.shared.diag.clone(),
            encryption: cfg.encryption,
            plain_peers: init.shared.plain_peers.clone(),
            utp: init.shared.utp.get().cloned(),
            utp_probe_timeout: cfg.utp_probe_timeout,
        });
        ctx.torrent_up.set(up_limit);
        ctx.group.assign(init.group.clone());
        let now = Instant::now();
        let mut seed_bytes = [0u8; 8];
        seed_bytes.copy_from_slice(&init.id[..8]);
        let seed = u64::from_le_bytes(seed_bytes) ^ init.epoch;
        let mut t = Torrent {
            id: init.id,
            epoch: init.epoch,
            sh: init.shared,
            cfg,
            tx: init.tx,
            rx: init.rx,
            ctx,
            live: init.live,
            counters: init.counters,
            private,
            save_path: init.save_path,
            staging: init.staging,
            placement: init.placement,
            priority: init.priority,
            meta: None,
            peers: HashMap::new(),
            next_key: 1,
            addrs_in_use: HashSet::new(),
            pool: PeerPool::new(),
            banned: Banned::default(),
            download_allowed: init.download_allowed,
            disk_blocked: false,
            downloading: false,
            finished_reported: false,
            tier: init.tier,
            choker: Choker::new(seed),
            slots: 4,
            seed_dials: 4,
            seed_dial_at: now,
            no_leechers: false,
            slots_at: now,
            last_rechoke: now,
            contrib: HashMap::new(),
            last_demand: now,
            uploading_since: None,
            rng: Rng::seeded(seed ^ 0x9E37_79B9_7F4A_7C15),
            last_tick: now,
            last_resume: now,
            resume_dirty: false,
            demote_sent: false,
            finalizing: false,
            shutting_down: false,
            meta_fetch: None,
            info_cache: None,
            info_loading: false,
            meta_requests: Vec::new(),
            pex_log: pex::PexLog::new(),
            last_want_peers: None,
            checking: false,
            start: Some(startup::StartParams {
                resume: init.resume,
                fresh: init.fresh,
                force_recheck: init.force_recheck,
                staged: init.staged,
                file_prios: init.file_prios,
                has_metadata: init.has_metadata,
                first: init.first,
                edge_conflicts: init.edge_conflicts,
            }),
            pending_prios: None,
            pending_switch: None,
            fps: None,
            stream: None,
            stream_boost: false,
        };
        for a in init.peers {
            t.pool.add(a);
        }
        t
    }

    /// Send a report to the session without ever blocking the actor.
    pub(crate) fn report(&self, r: TorrentReport) {
        let msg = SessionMsg::Torrent { id: self.id, epoch: self.epoch, report: r };
        if let Err(tokio::sync::mpsc::error::TrySendError::Full(m)) = self.sh.session.try_send(msg) {
            let tx = self.sh.session.clone();
            tokio::spawn(async move {
                let _ = tx.send(m).await;
            });
        }
    }

    fn peer_cap(&self) -> usize {
        self.cfg.peer_cap(self.tier == Tier::Hot)
    }

    fn mode(&self) -> Mode {
        if self.downloading { Mode::Leeching } else { Mode::Seeding }
    }

    fn complete(&self) -> bool {
        self.ctx.complete.load(Ordering::Relaxed)
    }
}

/// Entry point of the actor task.
pub(crate) async fn run(init: TorrentInit) {
    let mut t = Torrent::new(init);
    match t.startup().await {
        Ok(startup::Flow::Run) => {}
        Ok(startup::Flow::Exit) => return,
        Err(fail) => {
            t.fault(fail);
            t.wait_for_shutdown().await;
            return;
        }
    }
    t.main_loop().await;
}

impl Torrent {
    async fn main_loop(&mut self) {
        let mut timer = Box::pin(tokio::time::sleep_until((Instant::now() + Duration::from_secs(3600)).into()));
        // The deadline the timer is set to; `None` once it fired and must be re-armed.
        let mut armed: Option<Instant> = None;
        loop {
            // Only ever move the deadline *earlier*. Re-arming on every message would push it out
            // forever while blocks stream in, and the periodic work would never run.
            let want = Instant::now() + self.next_wait();
            if armed.is_none_or(|a| want < a) {
                timer.as_mut().reset(want.into());
                armed = Some(want);
            }
            tokio::select! {
                msg = self.rx.recv() => {
                    let Some(m) = msg else { break };
                    if self.handle(m).await {
                        break;
                    }
                }
                _ = timer.as_mut() => {
                    self.sh.diag.wakeups.fetch_add(1, Ordering::Relaxed);
                    armed = None;
                    self.tick().await;
                }
            }
        }
        self.drop_storage();
    }

    /// After a fault the actor does nothing but wait to be told to stop.
    async fn wait_for_shutdown(&mut self) {
        while let Some(m) = self.rx.recv().await {
            if let TorrentMsg::Cmd(TorrentCmd::Shutdown(reply)) = m {
                let _ = reply.send(self.shutdown_info());
                break;
            }
        }
        self.drop_storage();
    }

    /// Returns true when the actor should exit.
    async fn handle(&mut self, m: TorrentMsg) -> bool {
        match m {
            TorrentMsg::Peer { key, ev } => self.on_peer_event(key, ev).await,
            TorrentMsg::PeerClosed { key, why, connected } => self.on_peer_closed(key, why, connected),
            TorrentMsg::PieceHashed { piece, result } => self.on_piece_hashed(piece, result),
            TorrentMsg::Incoming(c) => self.accept_incoming(c.stream, c.hs, c.addr, c.permit),
            TorrentMsg::NetRestored => {
                if !self.peers.values().any(|p| p.connected) {
                    self.pool.clear_backoff();
                }
                self.connect_more(Instant::now());
            }
            TorrentMsg::AddPeers(v) => {
                for a in v {
                    self.pool.add(a);
                }
                self.connect_more(Instant::now());
            }
            TorrentMsg::TrackerPeers(v, incomplete) => {
                self.no_leechers = incomplete == Some(0);
                for a in v {
                    self.pool.add(a);
                }
                self.connect_more(Instant::now());
            }
            TorrentMsg::InfoBytes(r) => self.on_info_bytes(r),
            TorrentMsg::Fingerprints(f) => self.on_fingerprints(f),
            TorrentMsg::Finalized(r) => self.on_finalized(r),
            TorrentMsg::Cmd(c) => return self.on_cmd(c),
        }
        false
    }

    fn on_cmd(&mut self, c: TorrentCmd) -> bool {
        match c {
            TorrentCmd::Shutdown(reply) => {
                self.shutting_down = true;
                let info = self.shutdown_info();
                for p in self.peers.values() {
                    let _ = p.tx.try_send(PeerCmd::Disconnect);
                }
                let _ = reply.send(info);
                return true;
            }
            TorrentCmd::SetFilePriorities(p) => self.set_file_priorities(p),
            TorrentCmd::SetPriority(p) => {
                self.priority = p;
                self.publish_priority();
            }
            TorrentCmd::SetUpLimit(l) => self.ctx.torrent_up.set(l),
            TorrentCmd::SetGroup(g) => self.ctx.group.assign(g),
            TorrentCmd::ConfigChanged => {
                self.cfg = self.sh.cfg();
            }
            TorrentCmd::Detail(reply) => {
                let _ = reply.send(self.detail());
            }
            TorrentCmd::SwitchLocation(sw) => self.switch_location(*sw),
            TorrentCmd::Handle(reply) => {
                let _ = reply.send(self.ctx.disk.get().map(|d| d.handle.clone()));
            }
            TorrentCmd::StreamOpen { file, reply } => self.on_stream_open(file, reply),
            TorrentCmd::StreamHints => self.apply_stream_hints(),
        }
        false
    }

    /// Release storage state held for this torrent (partial pieces return their cache budget).
    fn drop_storage(&mut self) {
        let (Some(d), Some(m)) = (self.ctx.disk.get(), &self.meta) else { return };
        if let Some(p) = &m.picker {
            for piece in p.open_piece_ids() {
                d.handle.abort_piece(piece);
            }
        }
        d.handle.detach_hashes();
    }

    fn fault(&mut self, f: startup::Fail) {
        use crate::peer::{FaultKind, fault_kind_of_text};
        let (kind, message) = match f {
            startup::Fail::Missing(m) => (FaultKind::Missing, m),
            startup::Fail::Blocked(m) => (FaultKind::Blocked, m),
            startup::Fail::Error(m) => (fault_kind_of_text(&m), m),
        };
        self.report(TorrentReport::Fault { kind, message });
    }

    /// Peers worth reconnecting to after a restart.
    fn good_peers(&self) -> Vec<SocketAddr> {
        let mut v: Vec<(f64, SocketAddr)> = self
            .peers
            .values()
            .filter(|p| p.connected)
            .filter_map(|p| p.listen_addr.map(|a| (p.down.per_sec + p.up.per_sec, a)))
            .collect();
        v.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        v.into_iter().take(50).map(|(_, a)| a).collect()
    }

    fn shutdown_info(&self) -> ShutdownInfo {
        ShutdownInfo {
            resume: self.resume_record(None),
            peers: self.good_peers(),
        }
    }

    /// Resume data from the current state. `fps` carries file fingerprints when known.
    fn resume_record(&self, fps: Option<Vec<Option<[u8; 20]>>>) -> Option<ResumeRecord> {
        let m = self.meta.as_ref()?;
        let complete = self.complete();
        let bitfield = match (&m.picker, complete) {
            (_, true) => Vec::new(),
            (Some(p), false) => p.have().as_bytes().to_vec(),
            (None, false) => return None,
        };
        Some(ResumeRecord {
            complete,
            bitfield,
            fingerprints: fps.or_else(|| self.fps.clone()).unwrap_or_default(),
            saved_at: crate::util::unix_secs(),
        })
    }

    fn on_fingerprints(&mut self, fps: Vec<Option<[u8; 20]>>) {
        self.fps = Some(fps.clone());
        if let Some(r) = self.resume_record(Some(fps)) {
            self.report(TorrentReport::Resume(r));
        }
    }

    /// Bytes helper used by several submodules.
    pub(crate) fn bitfield_bytes(&self) -> (Bytes, bool, bool) {
        match &self.meta {
            Some(m) => match &m.picker {
                _ if self.complete() => (Bytes::new(), true, false),
                Some(p) if p.have().is_none() => (Bytes::new(), false, true),
                Some(p) => (Bytes::copy_from_slice(p.have().as_bytes()), false, false),
                None => (Bytes::new(), false, true),
            },
            None => (Bytes::new(), false, true),
        }
    }
}

#[cfg(test)]
mod rate_tests {
    use super::Rate;

    #[test]
    fn an_idle_rate_fades_to_exactly_zero() {
        let mut r = Rate::default();
        r.update(10_000_000, 1.0);
        assert!(r.per_sec > 0.0);
        let mut ticks = 0;
        while r.per_sec > 0.0 {
            r.update(10_000_000, 1.0);
            ticks += 1;
            assert!(ticks < 60, "rate never reached zero");
        }
    }
}
