//! The session actor: owns the torrent table and everything global.
//!
//! Cold torrents are [`cold::ColdTorrent`] records in `table`; Warm and Hot torrents additionally
//! have a [`Live`] (an actor task). The session never awaits a torrent: it uses `try_send` and
//! spawns helper tasks for replies, so no cycle of waiting actors can form.

mod add;
mod boot;
pub(crate) mod cold;
mod cmds;
mod direct;
mod load;
pub(crate) mod moves;
pub(crate) mod pending;
mod policy_apply;
mod recover;
mod reports;
mod settings;
mod sharing;
mod titles;
mod snapshot;
mod status;
pub(crate) mod streams;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use trs_wire::Handshake;

use self::cold::*;
pub(crate) use self::sharing::TrackerErr;
use crate::api::*;
use crate::budget::ConnPermit;
use crate::counters::Counters;
use crate::organize::Prepared;
use crate::persist::{Batch, DirectRecord, LifetimeCounters, Placement, ResumeRecord, StatDelta, Writer};
use crate::policy::{IdTable, PolicyData, ProfileMatch};
use crate::shared::Shared;
use crate::torrent::{self, LiveStats, TorrentMsg, TorrentReport};
use crate::tracker_driver::{TrackerCmd, TrackerHandle};
use crate::util::unix_secs;

/// Everything the session receives.
pub(crate) enum SessionMsg {
    Cmd(Cmd),
    /// Which torrent has `H("req2", info-hash) == target`? (the encrypted handshake names it that way)
    MseLookup { target: [u8; 20], reply: oneshot::Sender<Option<TorrentId>> },
    Incoming { stream: crate::net::PeerIo, hs: Handshake, addr: SocketAddr, permit: ConnPermit },
    Torrent { id: TorrentId, epoch: u64, report: TorrentReport },
    Exited { id: TorrentId, epoch: u64, info: Option<torrent::ShutdownInfo> },
    TrackerPeers { id: TorrentId, peers: Vec<SocketAddr>, incomplete: Option<u64> },
    /// Seeders and leechers from the tracker's last announce response (for the row's "(4 of 250)").
    /// An announce left (`Some(since_ms)`) or the last one in flight was answered (`None`).
    TrackerBusy { id: TorrentId, since_ms: Option<u64> },
    TrackerSwarm { id: TorrentId, seeders: Option<u64>, leechers: Option<u64> },
    DhtPeers { id: TorrentId, peers: Vec<SocketAddr> },
    DetailReady(Box<DetailJob>),
    /// Magnet metadata has been classified and placed (off the actor).
    MetadataPlanned(Box<add::MetaPlanned>),
    /// Result of re-matching tracker profiles after `trackers.toml` changed.
    Rematched(Vec<(TorrentId, ProfileMatch)>),
    /// Display titles computed off the actor: `(torrent, category they were computed for, title)`.
    Titled(Vec<(TorrentId, Option<trs_classify::Category>, String)>),
    /// Something happened to a direct download.
    Direct(direct::DirectMsg),
    /// A live listen-port change finished binding (`gen` tells a stale answer from the current one).
    Rebound { serial: u64, wanted: u16, result: Result<Box<crate::engine::Bound>, String> },
    /// The DHT node was moved to the new UDP socket after a live port change.
    DhtMoved { serial: u64, dht: trs_dht::DhtHandle },
    /// The listen port was mapped (or not). `serial` tells a stopped mapper's last word from the
    /// current one's.
    PortMap { serial: u64, status: crate::api::PortMapStatus },
    /// The previous mapper has deleted its mappings: start the one for mapper generation `serial`.
    PortMapRestart { serial: u64 },
    /// Which of the folders torrents save to are there (`volumes_changed`).
    Volumes(Vec<(u32, bool)>),
    /// The outage state changed (SPEC 7C.4c): `held` while the hub holds everything.
    Net { held: bool },
    /// The trackers of a torrent started or stopped failing.
    TrackerHealth { id: TorrentId, health: crate::tracker_driver::Health },
    /// A queued move was planned (off the actor).
    MovePlanned { job: moves::MoveJob, result: Result<Option<Box<crate::moveplan::MovePlan>>, String> },
    /// The exec task of a move asks the session to persist the new place and tell the actor.
    MoveSwitch { id: TorrentId, plan: Arc<crate::moveplan::MovePlan>, reply: oneshot::Sender<Result<(), String>> },
    /// A move ended.
    MoveDone { id: TorrentId, end: moves::ExecEnd },
    /// A finishing torrent is tried again.
    MoveRetry(moves::MoveJob),
    /// The actor of a moved torrent could not switch its layout and restarts from the record.
    MoveRestartActor(TorrentId),
    /// The actor answered an edge-file rename (`apply_edge_policy`): persist the placement.
    EdgeApplied { id: TorrentId, placement: Box<crate::persist::Placement>, result: Result<(), String>, tries: u8 },
    /// Run the edge-file policy again (the first attempt met a busy or changed torrent).
    EdgeRetry { id: TorrentId, tries: u8 },
    /// The journal of an interrupted move was resolved at start-up.
    MoveRecovered { id: TorrentId, result: Result<String, String> },
}

/// Everything the chooser computation needs from the session (it runs off the actor).
pub(crate) struct ChoiceJob {
    pub opts: AddOptions,
    pub awaiting: bool,
    pub prios: Vec<u8>,
    pub save_path: PathBuf,
    pub stage_root: Option<PathBuf>,
    pub placed: bool,
    /// Category in effect (the chooser's structure follows it).
    pub category: Option<trs_classify::Category>,
    /// Rule id in effect (`None` = Default) and the goal the torrent has now.
    pub rule: Option<String>,
    pub goal: SeedGoal,
}

pub(crate) struct DetailJob {
    pub reply: oneshot::Sender<Result<TorrentDetail, EngineError>>,
    pub detail: Result<TorrentDetail, EngineError>,
}

/// Commands from [`crate::EngineHandle`].
pub(crate) enum Cmd {
    /// A parsed and planned `.torrent`.
    AddPrepared {
        p: Box<Prepared>,
        opts: Box<AddOptions>,
        origin: AddOrigin,
        reply: oneshot::Sender<Result<Added, EngineError>>,
    },
    AddMagnet { uri: String, opts: Box<AddOptions>, reply: oneshot::Sender<Result<Added, EngineError>> },
    Exists(TorrentId, oneshot::Sender<Option<String>>),
    Pause(Vec<TorrentId>),
    /// A pause the app makes by itself (on battery): it does not outlive the run.
    PauseByApp(Vec<TorrentId>),
    /// Ends the app's own pause of those it still holds.
    ResumeByApp(Vec<TorrentId>),
    Resume(Vec<TorrentId>),
    Remove { ids: Vec<TorrentId>, delete_files: bool, reply: oneshot::Sender<()> },
    Recheck(Vec<TorrentId>),
    MoveToTop(Vec<TorrentId>),
    SetPriority(Vec<TorrentId>, Priority),
    SetFilePriorities(TorrentId, Vec<FilePriority>, oneshot::Sender<Result<(), EngineError>>),
    SetSeedGoal(Vec<TorrentId>, SeedGoal),
    ClearSeedGoal(Vec<TorrentId>),
    SetCategory(Vec<TorrentId>, Option<Category>),
    /// What the chooser needs to know about a torrent (smart add).
    ChoiceInfo(TorrentId, oneshot::Sender<Result<ChoiceJob, EngineError>>),
    /// Answer a chooser: set the priorities, and start the torrent if it was held.
    ConfirmChoice { id: TorrentId, prios: Vec<FilePriority>, reply: oneshot::Sender<Result<(), EngineError>> },
    /// Queue moves of torrents to the configured location or below a folder (SPEC 7A.5).
    MoveTorrents { ids: Vec<TorrentId>, target: crate::moveplan::MoveTarget, reply: oneshot::Sender<MoveReport> },
    /// Which torrents would move if the settings were applied to them (nothing is moved).
    ProposeMoves(oneshot::Sender<MoveProposal>),
    /// Cancel queued and running moves (a copy in progress is cleaned up; the torrent stays put).
    CancelMoves(Vec<TorrentId>),
    /// Remove a held torrent; nothing was written, nothing is left.
    CancelChoice { id: TorrentId, reply: oneshot::Sender<Result<(), EngineError>> },
    /// Pause, resume, start or cancel the grace period of a pending torrent (SPEC 7A.7).
    PendingAct { id: TorrentId, act: pending::PendingAct, reply: oneshot::Sender<Result<bool, EngineError>> },
    SetRule(Vec<TorrentId>, Option<String>),
    SetPolicy(Option<Arc<PolicyData>>),
    AddDownload { url: String, opts: Box<DownloadOptions>, reply: oneshot::Sender<Result<Added, EngineError>> },
    SetDownloadLimit(TorrentId, Option<u64>),
    ForgetFinished(Vec<TorrentId>),
    PauseAll,
    ResumeAll,
    /// A volume was mounted or unmounted (the app's observers).
    VolumesChanged,
    UpdateConfig(Box<dyn FnOnce(&mut crate::config::EngineConfig) + Send>),
    Reannounce(Vec<TorrentId>, bool, oneshot::Sender<Vec<ReannounceResult>>),
    AddPeers(TorrentId, Vec<SocketAddr>),
    NetworkChanged,
    /// The OS path monitor (SPEC 7C.4c).
    PathChanged(bool),
    WillSleep,
    DidWake,
    Snapshot(oneshot::Sender<Snapshot>),
    Totals(oneshot::Sender<Totals>),
    Detail(TorrentId, oneshot::Sender<Result<TorrentDetail, EngineError>>),
    Config(oneshot::Sender<crate::config::EngineConfig>),
    Flush(oneshot::Sender<()>),
    Shutdown { deadline: Instant, reply: oneshot::Sender<ShutdownReport> },
    /// Open a stream of one file (SPEC 7F); the server is the session-wide loopback server.
    StreamOpen {
        id: TorrentId,
        file: usize,
        server: Arc<trs_stream::StreamServer>,
        reply: streams::OpenReply,
    },
    /// A stream was closed by the caller: torrents nobody streams any more may go back to the queue.
    StreamsClosed,
    /// The DHT node, for public lookups by other services (folder sync); `None` while the DHT is off.
    PublicDht(oneshot::Sender<Option<trs_dht::DhtHandle>>),
    /// Map one more port (TCP and UDP) with the listen port's mapper (folder sync's port), or stop mapping it.
    SetExtraMappedPort(Option<u16>),
}

/// Timed work. Ordered so a `BTreeSet<(Instant, Job)>` is the scheduler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Job {
    Persist,
    CountersTick,
    GoalCheck,
    /// Re-evaluate the alt-speed schedule.
    AltCheck,
    /// Direct download bookkeeping (progress records).
    DirectTick,
    /// Sharing roles drift with time (Active -> Dormant).
    RoleCheck,
    /// Move finished torrents between the status folders (SPEC 7A.3).
    StatusCheck,
    DhtAnnounce(TorrentId),
    Retry(TorrentId),
    /// The grace period of a pending torrent may be over.
    PendingDue(TorrentId),
    /// Give the allocator's free memory back to the OS (after a burst of work).
    Trim,
    /// Bring up the next few torrents that need an actor after start-up (see `boot.rs`).
    BootStep,
    /// Look at the rate of every running download (`recover.rs`).
    StallCheck,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum After {
    Nothing,
    Respawn { force_recheck: bool },
    Remove { delete_files: bool },
}

/// A `remove` call waiting for its torrents to be gone: stopped, files deleted if asked.
pub(crate) struct RemoveWait {
    left: std::sync::atomic::AtomicUsize,
    tx: std::sync::Mutex<Option<oneshot::Sender<()>>>,
}

impl RemoveWait {
    pub(crate) fn new(n: usize, tx: oneshot::Sender<()>) -> Arc<Self> {
        let w = Arc::new(Self { left: std::sync::atomic::AtomicUsize::new(n), tx: std::sync::Mutex::new(Some(tx)) });
        if n == 0 {
            w.fire();
        }
        w
    }

    fn fire(&self) {
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
    }

    /// One torrent is finished with.
    pub(crate) fn done(&self) {
        if self.left.fetch_sub(1, Ordering::AcqRel) <= 1 {
            self.fire();
        }
    }
}

/// A running actor.
pub(crate) struct Live {
    pub tx: crate::chan::Sender<TorrentMsg>,
    pub stats: Arc<LiveStats>,
    pub epoch: u64,
    pub stopping: bool,
    pub after: After,
}

pub(crate) struct Entry {
    pub cold: ColdTorrent,
    pub live: Option<Box<Live>>,
}

#[derive(Default)]
struct Dirty {
    records: HashSet<TorrentId>,
    counters: HashSet<TorrentId>,
    resume: HashMap<TorrentId, Option<ResumeRecord>>,
    peers: HashMap<TorrentId, Vec<SocketAddr>>,
    removed: Vec<TorrentId>,
    files: Vec<(TorrentId, Arc<[u8]>)>,
    announce: HashMap<TorrentId, Vec<Vec<String>>>,
    stats: Vec<StatDelta>,
    placements: Vec<(TorrentId, Option<Placement>)>,
    directs: HashMap<TorrentId, DirectRecord>,
    removed_directs: Vec<TorrentId>,
}

/// Trims after the one that follows a burst (see `Session::trim_memory`).
const TRIM_FOLLOWUPS: u8 = 3;
/// Spacing of those follow-up trims.
const TRIM_FOLLOWUP_EVERY: Duration = Duration::from_secs(150);

pub(crate) struct Session {
    pub sh: Arc<Shared>,
    pub rx: mpsc::Receiver<SessionMsg>,
    pub table: HashMap<TorrentId, Entry>,
    pub roots: Vec<PathBuf>,
    pub ids: IdTable,
    pub directs: HashMap<TorrentId, direct::DirectEntry>,
    pub downloader: Option<trs_http::Downloader>,
    pub watcher: Option<tokio::task::JoinHandle<()>>,
    pub portmap: Option<crate::portmap::PortMapRunner>,
    /// Bumped at every mapper (re)start; answers of an older one are dropped.
    pub portmap_gen: u64,
    /// The policy the table was last evaluated against.
    pub applied_policy: Option<Arc<PolicyData>>,
    /// Magnet torrents whose metadata is being classified right now.
    pub planning: HashSet<TorrentId>,
    /// Trackers that are failing right now (only those; healthy torrents cost nothing).
    pub tracker_errs: HashMap<TorrentId, TrackerErr>,
    /// Seeders / leechers of each torrent's last tracker response.
    pub swarm: HashMap<TorrentId, (Option<u32>, Option<u32>)>,
    /// Torrents with an announce in flight, and since when (unix ms).
    pub announcing: HashMap<TorrentId, u64>,
    pub activity_tick: Option<Instant>,
    /// Peers of torrents the user paused, so that resume redials them at once (bounded; the
    /// persisted cache is written with a delay and is only the fallback after a restart).
    pub paused_peers: HashMap<TorrentId, Vec<SocketAddr>>,
    /// Callers of `remove` waiting for a torrent to be gone.
    pub remove_waits: HashMap<TorrentId, Vec<Arc<RemoveWait>>>,
    /// The status folder each torrent belonged in at the last status check (SPEC 7A.3): a move
    /// into a different folder waits until two checks in a row agree.
    pub status_seen: HashMap<TorrentId, crate::status::StatusFolder>,
    /// Torrents whose edge files are being renamed by a running actor (`apply_edge_policy`); the
    /// value says whether the policy must run once more when that finishes.
    pub edge_busy: HashMap<TorrentId, bool>,
    pub hosts: Vec<Box<str>>,
    pub tracker: Option<TrackerHandle>,
    pub dht: Option<trs_dht::DhtHandle>,
    pub writer: Writer,
    dirty: Dirty,
    /// Torrents and direct downloads removed whose removal may not be stored yet (waiting in `dirty`, or with the
    /// writer). A commit that bypasses the writer (`commit_now`, an add) for one of them first waits for it: else the
    /// removal, stored after, would delete what the add wrote (its `.torrent` file and record).
    removing: HashSet<TorrentId>,
    jobs: BTreeSet<(Instant, Job)>,
    pub epoch: u64,
    pub shutting_down: bool,
    flushed: HashMap<TorrentId, (u64, u64, u64)>,
    pub next_pos: u32,
    dht_inflight: HashSet<TorrentId>,
    pub hint_peers: HashMap<TorrentId, Vec<SocketAddr>>,
    pub listener: Option<tokio::task::JoinHandle<()>>,
    /// Accepts uTP connections on the shared UDP socket.
    pub utp_listener: Option<tokio::task::JoinHandle<()>>,
    /// The DHT node runs on the shared UDP socket (and so moves with it on a live port change).
    pub dht_shared: bool,
    /// Counts live port changes; an answer for an older one is dropped.
    pub rebind_gen: u64,
    /// A second port the port mapper maps (TCP and UDP): folder sync's (`EngineHandle::set_extra_mapped_port`).
    pub extra_mapped_port: Option<u16>,
    /// When the first request of the pending memory trim was made (see `request_trim`).
    trim_since: Option<Instant>,
    /// Follow-up trims still to run after the last burst (see `trim_memory`).
    trim_followups: u8,
    /// Moves of files to new locations (SPEC 7A.5).
    pub mover: moves::Mover,
    /// Torrents somebody is watching: they bypass the queue (empty unless someone streams).
    pub streaming: HashSet<TorrentId>,
    /// Countdowns of the torrents held for the grace period (SPEC 7A.7).
    pub pending: HashMap<TorrentId, pending::Countdown>,
    /// Torrents that need an actor after start-up, in the order they will be brought up.
    pub boot: boot::BootQueue,
    /// Torrents waiting to be tried again after a fault that may clear (`recover.rs`).
    pub retries: HashMap<TorrentId, recover::Retry>,
    /// Running downloads and whether they move (`recover.rs`); empty unless something downloads.
    pub stall: HashMap<TorrentId, crate::queue::Stall>,
    /// `finished_at` of finished torrents reopened while idle (files ticked): restored if the actor finds nothing missing.
    pub reopened_at: HashMap<TorrentId, u32>,
    /// Zero point of the stall clock.
    pub stall_clock: Instant,
    /// Whether a save folder was there when last looked at (`recover.rs`).
    pub root_seen: HashMap<u32, (bool, Instant)>,
}

impl Session {
    pub(crate) fn new(
        sh: Arc<Shared>,
        rx: mpsc::Receiver<SessionMsg>,
        tracker: Option<TrackerHandle>,
        dht: Option<trs_dht::DhtHandle>,
    ) -> Self {
        Self {
            writer: Writer::start(sh.store.clone()),
            applied_policy: sh.policy(),
            sh,
            rx,
            table: HashMap::new(),
            roots: Vec::new(),
            ids: IdTable::default(),
            directs: HashMap::new(),
            downloader: None,
            watcher: None,
            portmap: None,
            portmap_gen: 0,
            planning: HashSet::new(),
            status_seen: HashMap::new(),
            edge_busy: HashMap::new(),
            tracker_errs: HashMap::new(),
            swarm: HashMap::new(),
            announcing: HashMap::new(),
            remove_waits: HashMap::new(),
            paused_peers: HashMap::new(),
            activity_tick: None,
            hosts: vec!["".into()],
            tracker,
            dht,
            dirty: Dirty::default(),
            removing: HashSet::new(),
            jobs: BTreeSet::new(),
            epoch: 0,
            shutting_down: false,
            flushed: HashMap::new(),
            next_pos: 0,
            dht_inflight: HashSet::new(),
            hint_peers: HashMap::new(),
            listener: None,
            utp_listener: None,
            dht_shared: false,
            rebind_gen: 0,
            extra_mapped_port: None,
            trim_since: None,
            trim_followups: 0,
            mover: moves::Mover::default(),
            streaming: HashSet::new(),
            pending: HashMap::new(),
            boot: boot::BootQueue::default(),
            retries: HashMap::new(),
            stall: HashMap::new(),
            reopened_at: HashMap::new(),
            stall_clock: Instant::now(),
            root_seen: HashMap::new(),
        }
    }

    pub(crate) fn intern_root(&mut self, p: PathBuf) -> u32 {
        if let Some(i) = self.roots.iter().position(|r| *r == p) {
            return i as u32;
        }
        self.roots.push(p);
        (self.roots.len() - 1) as u32
    }

    /// Intern a tracker host (0 = none). Capped so a hostile torrent cannot grow the table.
    pub(crate) fn intern_host(&mut self, h: &str) -> u16 {
        if let Some(i) = self.hosts.iter().position(|x| &**x == h) {
            return i as u16;
        }
        if self.hosts.len() >= 4096 {
            return 0;
        }
        self.hosts.push(h.into());
        (self.hosts.len() - 1) as u16
    }

    pub(crate) fn save_path(&self, c: &ColdTorrent) -> PathBuf {
        self.roots.get(c.save_root as usize).cloned().unwrap_or_default()
    }

    /// The staging root recorded for `c` (`None` = the configured folder or `<save>/.incomplete`).
    pub(crate) fn stage_root(&self, c: &ColdTorrent) -> Option<PathBuf> {
        ColdTorrent::opt(c.stage_root).and_then(|i| self.roots.get(i as usize)).cloned()
    }

    /// The persisted record of `c`.
    pub(crate) fn record_of(&self, c: &ColdTorrent, announce: Option<Vec<Vec<String>>>) -> crate::persist::TorrentRecord {
        c.record(self.save_path(c), self.stage_root(c), announce, &self.ids)
    }

    pub(crate) fn next_epoch(&mut self) -> u64 {
        self.epoch += 1;
        self.epoch
    }

    // ---- scheduler ----------------------------------------------------------------------

    fn schedule(&mut self, at: Instant, job: Job) {
        self.jobs.insert((at, job));
    }

    fn jobs_retain(&mut self, keep: impl Fn(&Job) -> bool) {
        self.jobs.retain(|(_, j)| keep(j));
    }

    fn schedule_after(&mut self, d: Duration, job: Job) {
        // Only one pending copy of the singleton jobs.
        if matches!(job, Job::Persist | Job::CountersTick | Job::GoalCheck | Job::AltCheck | Job::DirectTick | Job::RoleCheck | Job::StatusCheck | Job::Trim | Job::BootStep | Job::StallCheck)
            && self.jobs.iter().any(|(_, j)| *j == job)
        {
            return;
        }
        self.schedule(Instant::now() + d, job);
    }

    fn run_jobs(&mut self, now: Instant) {
        while let Some(&(at, job)) = self.jobs.iter().next() {
            if at > now {
                break;
            }
            self.jobs.remove(&(at, job));
            match job {
                Job::Persist => self.flush_persist(),
                Job::CountersTick => self.counters_tick(),
                Job::GoalCheck => self.goal_check(),
                Job::AltCheck => self.alt_check(),
                Job::DirectTick => self.direct_tick(),
                Job::RoleCheck => self.role_check(),
                Job::StatusCheck => self.status_check(),
                Job::DhtAnnounce(id) => self.dht_announce_job(id),
                Job::Retry(id) => self.retry_fault(id),
                Job::PendingDue(id) => self.pending_due(id),
                Job::Trim => self.trim_memory(),
                Job::BootStep => self.boot_step(),
                Job::StallCheck => self.stall_check(),
            }
        }
    }

    pub(crate) async fn run(mut self) {
        loop {
            let wait = self
                .jobs
                .iter()
                .next()
                .map_or(Duration::from_secs(3600), |(at, _)| at.saturating_duration_since(Instant::now()));
            tokio::select! {
                m = self.rx.recv() => {
                    let Some(m) = m else { break };
                    self.sh.diag.wakeups.fetch_add(1, Ordering::Relaxed);
                    if self.handle(m).await {
                        break;
                    }
                }
                _ = tokio::time::sleep(wait) => {
                    self.sh.diag.wakeups.fetch_add(1, Ordering::Relaxed);
                    self.run_jobs(Instant::now());
                }
            }
        }
    }

    /// Returns true when the session is finished.
    async fn handle(&mut self, m: SessionMsg) -> bool {
        match m {
            SessionMsg::Cmd(Cmd::Shutdown { deadline, reply }) => {
                self.shutdown(deadline, reply).await;
                return true;
            }
            SessionMsg::Cmd(c) => self.on_cmd(c),
            SessionMsg::MseLookup { target, reply } => {
                let found = self.table.keys().find(|id| trs_wire::mse::hash(&[b"req2", &id[..]]) == target).copied();
                let _ = reply.send(found);
            }
            SessionMsg::Incoming { stream, hs, addr, permit } => self.on_incoming(stream, hs, addr, permit),
            SessionMsg::Torrent { id, epoch, report } => self.on_report(id, epoch, report),
            SessionMsg::Exited { id, epoch, info } => self.on_exited(id, epoch, info),
            SessionMsg::TrackerBusy { id, since_ms } => {
                match since_ms {
                    Some(t) => self.announcing.insert(id, t),
                    None => self.announcing.remove(&id),
                };
                self.sh.emit(EngineEvent::TrackerHealthChanged { id });
            }
            SessionMsg::TrackerSwarm { id, seeders, leechers } => {
                let cap = |n: Option<u64>| n.map(|n| n.min(u64::from(u32::MAX)) as u32);
                self.swarm.insert(id, (cap(seeders), cap(leechers)));
            }
            SessionMsg::TrackerPeers { id, peers, incomplete } => self.on_tracker_peers(id, peers, incomplete),
            SessionMsg::DhtPeers { id, peers } => self.on_dht_peers(id, peers),
            SessionMsg::DetailReady(j) => {
                let _ = j.reply.send(j.detail);
            }
            SessionMsg::MetadataPlanned(m) => self.on_metadata_planned(*m),
            SessionMsg::Rematched(v) => self.on_rematched(v),
            SessionMsg::Titled(v) => self.on_titled(v),
            SessionMsg::Direct(m) => self.on_direct(m),
            SessionMsg::Rebound { serial, wanted, result } => self.on_rebound(serial, wanted, result),
            SessionMsg::DhtMoved { serial, dht } => self.on_dht_moved(serial, dht),
            SessionMsg::PortMap { serial, status } => self.on_portmap(serial, status),
            SessionMsg::PortMapRestart { serial } => self.on_portmap_restart(serial),
            SessionMsg::TrackerHealth { id, health } => self.on_tracker_health(id, health),
            SessionMsg::Net { held } => self.on_net(held),
            SessionMsg::Volumes(res) => self.on_volumes(res),
            SessionMsg::MovePlanned { job, result } => self.on_move_planned(job, result),
            SessionMsg::MoveSwitch { id, plan, reply } => self.on_move_switch(id, plan, reply),
            SessionMsg::MoveDone { id, end } => self.on_move_done(id, end),
            SessionMsg::MoveRetry(job) => self.on_move_retry(job),
            SessionMsg::MoveRestartActor(id) => self.on_move_restart_actor(id),
            SessionMsg::EdgeApplied { id, placement, result, tries } => self.on_edge_applied(id, *placement, result, tries),
            SessionMsg::EdgeRetry { id, tries } => self.edge_policy(id, tries),
            SessionMsg::MoveRecovered { id, result } => self.on_move_recovered(id, result),
        }
        false
    }

    // ---- persistence --------------------------------------------------------------------

    pub(crate) fn mark_record(&mut self, id: TorrentId) {
        self.dirty.records.insert(id);
        self.schedule_persist();
    }

    pub(crate) fn mark_counters(&mut self, id: TorrentId) {
        self.dirty.counters.insert(id);
        self.schedule_persist();
    }

    fn schedule_persist(&mut self) {
        let d = self.sh.cfg().persist_debounce;
        self.schedule_after(d, Job::Persist);
    }

    /// The automatic tracker a torrent's statistics are recorded under: the registrable domain of
    /// its primary announce host.
    pub(crate) fn stat_domain(&self, c: &ColdTorrent) -> Option<String> {
        let h = self.hosts.get(usize::from(c.host))?;
        (!h.is_empty()).then(|| trs_state::trackers::registrable_domain(h))
    }

    pub(crate) fn push_stat(&mut self, d: StatDelta) {
        self.dirty.stats.push(d);
        self.schedule_persist();
    }

    pub(crate) fn flush_persist(&mut self) {
        let now = unix_secs();
        let mut b = Batch { torrent_files: std::mem::take(&mut self.dirty.files), ..Batch::default() };
        let mut records: Vec<TorrentId> = self.dirty.records.drain().collect();
        records.sort_unstable();
        for id in records {
            if let Some(e) = self.table.get(&id) {
                let announce = self.dirty.announce.remove(&id);
                b.torrents.push(self.record_of(&e.cold, announce));
            }
        }
        let mut counters: Vec<TorrentId> = self.dirty.counters.drain().collect();
        counters.sort_unstable();
        for id in counters {
            if let Some(e) = self.table.get(&id) {
                b.counters.push((id, lifetime(&e.cold, now)));
            }
        }
        for id in b.torrents.iter().map(|r| r.id).collect::<Vec<_>>() {
            if !b.counters.iter().any(|(i, _)| *i == id)
                && let Some(e) = self.table.get(&id) {
                    b.counters.push((id, lifetime(&e.cold, now)));
                }
        }
        b.resume = self.dirty.resume.drain().collect();
        b.peers = self.dirty.peers.drain().collect();
        b.removed = std::mem::take(&mut self.dirty.removed);
        b.stat_deltas = std::mem::take(&mut self.dirty.stats);
        b.placements = std::mem::take(&mut self.dirty.placements);
        b.directs = self.dirty.directs.drain().map(|(_, d)| d).collect();
        b.removed_directs = std::mem::take(&mut self.dirty.removed_directs);
        self.writer.submit(b);
    }

    /// Statistics deltas for torrents that moved bytes (or seeded) since the last call.
    fn collect_stat_deltas(&mut self) {
        let now = unix_secs();
        let mut out = Vec::new();
        for (id, e) in &self.table {
            let c = &e.cold;
            let (u, d, sd) = (c.counters.up_total(), c.counters.down_total(), c.seeded_secs(now));
            let prev = self.flushed.get(id).copied().unwrap_or((u, d, sd));
            if (u, d, sd) != prev {
                out.push(StatDelta {
                    domain: self.stat_domain(c),
                    private: c.private(),
                    at: now,
                    uploaded: u - prev.0.min(u),
                    downloaded: d - prev.1.min(d),
                    seed_secs: sd - prev.2.min(sd),
                    ..StatDelta::default()
                });
            }
            self.flushed.insert(*id, (u, d, sd));
        }
        for d in out {
            self.push_stat(d);
        }
    }

    fn counters_tick(&mut self) {
        self.update_activity();
        self.collect_stat_deltas();
        let live: Vec<TorrentId> = self.table.iter().filter(|(_, e)| e.live.is_some()).map(|(i, _)| *i).collect();
        for id in &live {
            self.mark_counters(*id);
        }
        self.goal_check();
        if !live.is_empty() {
            self.schedule_after(Duration::from_secs(60), Job::CountersTick);
        }
    }

    // ---- seeding goals ------------------------------------------------------------------

    pub(crate) fn goal_check(&mut self) {
        let now = unix_secs();
        let mut reached = Vec::new();
        let mut any_goal = false;
        // Wake when something can first be reached: at the end of a hit-and-run floor or a time
        // goal, otherwise every 20 s (ratio goals) or minute.
        let mut next = 60u64;
        let (idle_hit, idle_wait) = self.idle_check(now);
        next = next.min(idle_wait);
        for (id, e) in &self.table {
            let c = &e.cold;
            if c.want != Want::Active || !c.finished() || !c.seed_goal.is_set() {
                continue;
            }
            any_goal = true;
            // Files still in `.incomplete`: the `Finalized` report runs this check again.
            if c.has(F_STAGED) {
                continue;
            }
            let g = &c.seed_goal;
            let seeded = c.seeded_secs(now);
            if idle_hit.contains(id) {
                reached.push((*id, g.at_goal));
                continue;
            }
            if g.reached(c.ratio(), seeded) {
                reached.push((*id, g.at_goal));
                continue;
            }
            let floor = u64::from(g.min_seed_secs).saturating_sub(seeded);
            let wait = if floor > 0 {
                floor + 1
            } else {
                let base = if g.ratio.is_some() { 20 } else { 60 };
                match g.seed_secs {
                    Some(t) => base.min(u64::from(t).saturating_sub(seeded) + 1),
                    None => base,
                }
            };
            next = next.min(wait.max(1));
        }
        for (id, action) in reached {
            self.sh.emit(EngineEvent::SeedingGoalReached { id });
            if idle_hit.contains(&id) {
                if let Some(r) = self.table.get_mut(&id).and_then(|e| e.cold.idle.as_mut()) {
                    r.stopped = true;
                    r.unsaved = true;
                }
                self.mark_record(id);
            }
            match action {
                AtGoal::Stop => self.set_want(id, Want::GoalReached),
                AtGoal::Remove => self.remove_torrent(id, false),
                AtGoal::RemoveAndDelete => self.remove_torrent(id, true),
            }
        }
        if any_goal && !self.shutting_down {
            // A pending check may be later than what is needed now: replace it.
            self.jobs_retain(|j| *j != Job::GoalCheck);
            self.schedule_after(Duration::from_secs(next), Job::GoalCheck);
        }
    }

    /// The inactive stop condition (SPEC §7.5): feed every ring with the seeding time and uploads
    /// since the last check, create or drop rings as the condition appears or goes, and return the
    /// torrents it stops now plus the seconds until it needs looking at again. Runs inside
    /// [`goal_check`](Self::goal_check), so it costs nothing for torrents without the condition.
    fn idle_check(&mut self, now: u64) -> (std::collections::HashSet<TorrentId>, u64) {
        use crate::idle::IdleRing;
        const SAVE_EVERY: u64 = 300;
        let hour = u64::from(self.sh.cfg().idle_hour_secs.max(1));
        let mut hit = std::collections::HashSet::new();
        let mut dirty = Vec::new();
        let mut wait = u64::MAX;
        for (id, e) in self.table.iter_mut() {
            let c = &mut e.cold;
            let Some((below, hours)) = c.seed_goal.idle() else {
                if c.idle.take().is_some() {
                    dirty.push(*id);
                }
                continue;
            };
            if c.has(F_STAGED) {
                continue;
            }
            let window = u32::try_from(u64::from(hours) * hour).unwrap_or(u32::MAX);
            let seeding = c.want == Want::Active && c.finished() && !c.has(F_QUEUED);
            if c.idle.as_ref().is_some_and(|r| r.window_secs() != window) {
                c.idle = None;
            }
            let seeded = c.seeded_secs(now).min(u64::from(u32::MAX)) as u32;
            let up = c.counters.up_total();
            if c.idle.is_none() {
                if !seeding {
                    continue;
                }
                let mut r = IdleRing::new(window, seeded, up);
                r.saved_at = now as u32;
                c.idle = Some(Box::new(r));
                dirty.push(*id);
            }
            if !seeding {
                continue;
            }
            let min_seed = u64::from(c.seed_goal.min_seed_secs);
            let Some(r) = c.idle.as_mut() else { continue };
            r.observe(seeded, up);
            if r.stopped {
                r.stopped = false;
            }
            if r.unsaved && now.saturating_sub(u64::from(r.saved_at)) >= SAVE_EVERY {
                r.saved_at = now as u32;
                r.unsaved = false;
                dirty.push(*id);
            }
            if r.idle(below) {
                if u64::from(seeded) >= min_seed {
                    hit.insert(*id);
                } else {
                    wait = wait.min(min_seed - u64::from(seeded) + 1);
                }
            } else {
                let w = u64::from(r.window_secs());
                let w = if r.armed() { 60.min((w / 4).max(1)) } else { u64::from(r.arms_in()) + 1 };
                wait = wait.min(60.min(w.max(1)));
            }
        }
        for id in dirty {
            self.mark_record(id);
        }
        (hit, wait)
    }

    // ---- DHT ----------------------------------------------------------------------------

    pub(crate) fn dht_ok(&self, c: &ColdTorrent) -> bool {
        self.dht.is_some() && !c.private() && c.want == Want::Active && !c.has(F_QUEUED) && !c.awaiting() && self.sh.cfg().dht_enabled
    }

    /// Start a peer lookup (and optionally announce) for a public torrent.
    ///
    /// Returns `false` when a lookup for this torrent is already running (the caller may try
    /// again soon); nothing else is "busy".
    pub(crate) fn dht_lookup(&mut self, id: TorrentId, announce: bool) -> bool {
        let Some(e) = self.table.get(&id) else { return true };
        if !self.dht_ok(&e.cold) {
            return true;
        }
        let Some(dht) = self.dht.clone() else { return true };
        // The only way to build a PublicInfoHash is to state the private flag.
        let Some(h) = trs_dht::PublicInfoHash::new(id, e.cold.private()) else { return true };
        if !self.dht_inflight.insert(id) {
            return false;
        }
        let port = self.sh.port();
        let tx = self.sh.session.clone();
        let diag = self.sh.diag.clone();
        if announce {
            diag.dht_announces.fetch_add(1, Ordering::Relaxed);
        }
        diag.dht_lookups.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let res = if announce { dht.announce(h, port).await } else { dht.get_peers(h).await };
            if let Ok(Ok(mut stream)) = res {
                while let Some(u) = stream.next().await {
                    if let trs_dht::LookupUpdate::Peers(v) = u {
                        let peers = v.into_iter().map(SocketAddr::V4).collect();
                        let _ = tx.send(SessionMsg::DhtPeers { id, peers }).await;
                    }
                }
            }
            let _ = tx.send(SessionMsg::DhtPeers { id, peers: Vec::new() }).await;
        });
        true
    }

    fn dht_announce_job(&mut self, id: TorrentId) {
        let Some(e) = self.table.get(&id) else { return };
        if e.cold.want != Want::Active {
            return;
        }
        if !self.dht_lookup(id, true) {
            // The plain lookup that every torrent starts when it becomes ready is still running:
            // announce right after it instead of waiting for the next period.
            self.schedule_after(Duration::from_secs(10), Job::DhtAnnounce(id));
            return;
        }
        // 30 minutes with +/- 5 minutes of jitter, so 500 torrents spread out.
        let j = crate::util::unix_ms() % 600;
        self.schedule_after(Duration::from_secs(25 * 60 + j), Job::DhtAnnounce(id));
    }

    pub(crate) fn dht_schedule_first(&mut self, id: TorrentId, spread: Duration) {
        if self.dht.is_some() {
            self.schedule_after(spread, Job::DhtAnnounce(id));
        }
    }

    // ---- memory -------------------------------------------------------------------------

    /// A burst of work ended or a torrent went Cold: give freed memory back to the OS once things
    /// have been quiet for a while. Debounced (each call pushes the trim back by 20 s) but never
    /// later than two minutes after the first request, so a steady trickle of events cannot
    /// postpone it forever. This is what keeps an idle seeder of hundreds of torrents near its live
    /// heap instead of near the peak of its start-up verification.
    pub(crate) fn request_trim(&mut self) {
        self.trim_followups = TRIM_FOLLOWUPS;
        let now = Instant::now();
        let first = *self.trim_since.get_or_insert(now);
        if now.saturating_duration_since(first) < Duration::from_secs(120) {
            self.jobs_retain(|j| *j != Job::Trim);
            self.schedule_after(Duration::from_secs(20), Job::Trim);
        } else if !self.jobs.iter().any(|(_, j)| *j == Job::Trim) {
            self.schedule_after(Duration::from_secs(1), Job::Trim);
        }
    }

    /// The system allocator only gives back memory that has been free for a few minutes (measured on
    /// macOS 26: `malloc_zone_pressure_relief` called every 10 s released nothing for the first 150 s
    /// after a burst and 3.4 MB of 10.9 MB in the two minutes after that), so the trim 20 s after a
    /// burst finds little. A few follow-up trims, minutes apart, collect the rest; each is one cheap
    /// syscall off the actor, and a new burst starts the series again.
    fn trim_memory(&mut self) {
        self.trim_since = None;
        if !self.shutting_down {
            // A syscall that walks the allocator's free lists: off the actor.
            tokio::task::spawn_blocking(trs_storage::release_free_memory);
            if self.trim_followups > 0 {
                self.trim_followups -= 1;
                self.schedule_after(TRIM_FOLLOWUP_EVERY, Job::Trim);
            }
        }
    }

    // ---- accounting helpers ---------------------------------------------------------------

    /// Account the seeding stretch that is ending or starting.
    pub(crate) fn update_seed_clock(&mut self, id: TorrentId) {
        let now = unix_secs() as u32;
        let Some(e) = self.table.get_mut(&id) else { return };
        let c = &mut e.cold;
        let seeding = c.want == Want::Active && c.finished() && !c.has(F_QUEUED);
        if seeding && c.seed_since == 0 {
            c.seed_since = now.max(1);
        } else if !seeding && c.seed_since != 0 {
            c.seeded_secs = c.seeded_secs.saturating_add(now.saturating_sub(c.seed_since));
            c.seed_since = 0;
        }
    }

    pub(crate) fn tracker_send(&self, c: TrackerCmd) {
        if let Some(t) = &self.tracker {
            t.send(c);
        }
    }

    pub(crate) fn dirty_resume(&mut self, id: TorrentId, r: Option<ResumeRecord>) {
        self.dirty.resume.insert(id, r);
        self.schedule_persist();
    }

    pub(crate) fn dirty_peers(&mut self, id: TorrentId, p: Vec<SocketAddr>) {
        self.dirty.peers.insert(id, p);
        self.schedule_persist();
    }

    pub(crate) fn dirty_direct(&mut self, mut d: DirectRecord) {
        // What the app paused by itself (on battery) is stored as running: the pause ends with the run.
        if d.state == crate::persist::DirectState::Paused && self.directs.get(&d.id).is_some_and(|e| e.app_paused) {
            d.state = crate::persist::DirectState::Active;
        }
        self.dirty.removed_directs.retain(|x| *x != d.id);
        self.dirty.directs.insert(d.id, d);
        self.schedule_persist();
    }

    pub(crate) fn dirty_direct_removed(&mut self, id: TorrentId) {
        self.dirty.directs.remove(&id);
        self.dirty.removed_directs.push(id);
        self.note_removing(id);
        self.schedule_persist();
    }

    pub(crate) fn dirty_remove(&mut self, id: TorrentId) {
        self.dirty.records.remove(&id);
        self.dirty.counters.remove(&id);
        self.dirty.resume.remove(&id);
        self.dirty.placements.retain(|(i, _)| *i != id);
        self.dirty.files.retain(|(i, _)| *i != id);
        self.dirty.removed.push(id);
        self.note_removing(id);
        self.schedule_persist();
    }

    fn note_removing(&mut self, id: TorrentId) {
        // Bounded: after many removals with no add of one of them, store them all and forget the list.
        if self.removing.len() >= 4096 {
            self.store_removals();
        }
        self.removing.insert(id);
    }

    /// Stores every removal so far (hands the waiting batch to the writer and waits for it).
    pub(crate) fn store_removals(&mut self) {
        self.flush_persist();
        self.writer.flush();
        self.removing.clear();
    }

    /// Before a commit that bypasses the writer: a removal of one of `ids` still on its way is stored first.
    pub(crate) fn before_commit_now(&mut self, ids: impl IntoIterator<Item = TorrentId>) {
        if !self.removing.is_empty() && ids.into_iter().any(|id| self.removing.contains(&id)) {
            self.store_removals();
        }
    }

    pub(crate) fn ensure_counters_job(&mut self) {
        self.schedule_after(Duration::from_secs(60), Job::CountersTick);
    }

    pub(crate) fn ensure_goal_job(&mut self) {
        self.schedule_after(Duration::from_secs(60), Job::GoalCheck);
    }

    /// Run the goal check in a moment (torrents that were found complete, rule edits).
    pub(crate) fn goal_check_soon(&mut self) {
        let at = Instant::now() + Duration::from_millis(500);
        if self.jobs.iter().any(|(t, j)| *j == Job::GoalCheck && *t <= at) {
            return;
        }
        self.jobs_retain(|j| *j != Job::GoalCheck);
        self.schedule(at, Job::GoalCheck);
    }

    pub(crate) fn stat_baseline(&mut self, id: TorrentId) {
        if let Some(e) = self.table.get(&id) {
            let sd = e.cold.seeded_secs(unix_secs());
            self.flushed.entry(id).or_insert((e.cold.counters.up_total(), e.cold.counters.down_total(), sd));
        }
    }

    pub(crate) fn forget_stat(&mut self, id: &TorrentId) {
        self.flushed.remove(id);
    }

    pub(crate) fn dht_done(&mut self, id: &TorrentId) {
        self.dht_inflight.remove(id);
    }

    /// Final deltas before shutdown or removal.
    pub(crate) fn final_stats(&mut self) {
        self.collect_stat_deltas();
    }
}

fn lifetime(c: &ColdTorrent, now: u64) -> LifetimeCounters {
    LifetimeCounters {
        uploaded: c.counters.up_total(),
        downloaded: c.counters.down_total(),
        seeded_secs: c.seeded_secs(now),
        done_bytes: c.done_bytes(),
        wanted_done: None,
    }
}

/// Helper to build the `Counters` of a stored torrent.
pub(crate) fn restore_counters(total: u64, done: u64, c: &LifetimeCounters) -> Arc<Counters> {
    let counters = Counters::restore(c.uploaded, c.downloaded, total.saturating_sub(done));
    counters.set_wanted_done(c.wanted_done.unwrap_or(0));
    Arc::new(counters)
}
