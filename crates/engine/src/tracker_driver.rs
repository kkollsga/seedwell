//! One task drives `trs_tracker::TrackerHub` for every torrent, exactly as the tracker crate's
//! README prescribes:
//!
//! * the clock is Unix milliseconds (`Millis`);
//! * counters are pushed with `set_stats` before every tick (session counters, exact `left`);
//! * `key` and `peer_id` are generated once per process and reused for every torrent, across
//!   pause and resume;
//! * `Start`/`Stop` are the only lifecycle events the engine sends, plus `Complete` once;
//! * on shutdown every open session gets `Stop` and the task waits for quiescence or the deadline.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};
use trs_tracker::gate::GateConfig;
use trs_tracker::http::HttpTransport;
use trs_tracker::hub::{TorrentSpec, TrackerHub};
use trs_tracker::machine::Report;
use trs_tracker::store::{StoreError, TimingKey, TimingRecord, TimingStore};
use trs_tracker::udp::{UdpConfig, UdpTransport};
use trs_tracker::url::Scheme;
use trs_tracker::{Millis, Stats, TorrentEvent, TrackerUrl};

use crate::api::{ReannounceResult, SecretUrl, ShutdownReport, TorrentId, TrackerInfo, TrackerState};
use crate::counters::Counters;
use crate::peer::Diag;
use crate::session::SessionMsg;
use crate::shared::Shared;
use crate::tracker_log::{self, Wire};
use crate::util::unix_ms;
use trs_devlog::{DevLog, Entry, Kind, Next};

/// Adapter so a boxed store can back the hub.
struct BoxedTiming(Box<dyn TimingStore + Send>);

impl TimingStore for BoxedTiming {
    fn load(&self, key: &TimingKey) -> Option<TimingRecord> {
        self.0.load(key)
    }

    fn save(&mut self, key: &TimingKey, record: &TimingRecord) -> Result<(), StoreError> {
        self.0.save(key, record)
    }
}

/// What the driver needs to know to register a torrent.
pub(crate) struct TrackerTorrent {
    pub id: TorrentId,
    pub private: bool,
    pub added_complete: bool,
    pub tiers: Vec<Vec<String>>,
    pub counters: Arc<Counters>,
    pub size: u64,
}

/// What a torrent is doing, for the retry pace of failed announces (SPEC 7C.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Incomplete or fetching metadata: needs peers soon.
    Downloading,
    /// Seeding, Popular or Active.
    Seeding,
    /// Seeding, Dormant (no upload and no interested peer for 24 h).
    SeedingDormant,
}

/// Aggregate health of a torrent's trackers, as the Sharing groups need it (SPEC 8.3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Health {
    /// At least one tracker answers, or nothing has failed (yet).
    Ok,
    /// Every tracker is failing (not a mere `warning message` or maintenance reason).
    Failing { reason: String, retry_at_ms: Option<u64> },
    /// Every tracker halted on a permanent-looking reason (unregistered, banned, bad passkey).
    Dead { reason: String },
}

/// Commands from the session.
pub(crate) enum TrackerCmd {
    /// The torrent's role changed.
    Role(TorrentId, Role),
    Add(TrackerTorrent),
    /// A duplicate add brought new announce URLs for a registered torrent: each becomes a tier
    /// of its own, announced through the normal hub pacing when a session is open.
    AddTrackers(TorrentId, Vec<String>),
    Start(TorrentId),
    Stop(TorrentId),
    Complete(TorrentId),
    Remove(TorrentId),
    /// Re-announce now; the reply says, per tracker, what the min-interval gate made of it.
    Reannounce(TorrentId, oneshot::Sender<Vec<ReannounceResult>>),
    SetSize(TorrentId, u64),
    Info(TorrentId, oneshot::Sender<Vec<TrackerInfo>>),
    NetworkChanged,
    /// The OS reports whether a usable network path exists (SPEC 7C.4c).
    Path(bool),
    /// The Mac is about to sleep.
    WillSleep,
    /// The Mac woke.
    DidWake,
    /// A sign of life from outside the tracker code (an accepted connection, a DHT datagram)
    /// while the hub holds everything.
    Alive,
    /// The display title of a torrent (developer log only; sent while the log is on).
    Title(TorrentId, String),
    /// The port mapper settled or changed: announce `Some(port)` (the external TCP port, when it
    /// differs from the listen port; `None` = the listen port) from now on, replaying the
    /// commands held back meanwhile. Torrents already registered carry it from their next regular
    /// announce on.
    PortKnown(Option<u16>),
    /// `network.encryption` changed: announce `supportcrypto` / `requirecrypto` accordingly from
    /// each torrent's next announce (no extra request, no schedule change).
    Crypto(crate::config::EncryptionPolicy),
    Shutdown { deadline: Instant, reply: oneshot::Sender<ShutdownReport> },
}

/// Cloneable handle.
#[derive(Clone)]
pub(crate) struct TrackerHandle {
    tx: mpsc::UnboundedSender<TrackerCmd>,
}

impl TrackerHandle {
    pub(crate) fn send(&self, c: TrackerCmd) {
        let _ = self.tx.send(c);
    }
}

#[derive(Default, Clone, PartialEq, Eq)]
enum SlotState {
    /// Nothing heard yet.
    #[default]
    Unknown,
    Ok,
    /// A warning or a non-permanent `failure reason`: shown, but not an error.
    Warn,
    Failed { reason: String, retry_at_ms: u64 },
    Halted(String),
}

#[derive(Default, Clone)]
struct SlotStat {
    status: String,
    seeds: Option<u64>,
    leechers: Option<u64>,
    state: SlotState,
}

/// The per-tracker status of one torrent. Torrents have one to three trackers, so a vector with a
/// linear search costs about 110 bytes per tracker; a `HashMap` allocated a four-bucket table
/// (about 450 bytes) the moment the first announce answered.
#[derive(Default)]
struct Slots(Vec<(u16, SlotStat)>);

impl Slots {
    fn entry(&mut self, slot: u16) -> &mut SlotStat {
        let i = match self.0.iter().position(|(s, _)| *s == slot) {
            Some(i) => i,
            None => {
                self.0.push((slot, SlotStat::default()));
                self.0.len() - 1
            }
        };
        &mut self.0[i].1
    }

    fn get(&self, slot: &u16) -> Option<&SlotStat> {
        self.0.iter().find(|(s, _)| s == slot).map(|(_, v)| v)
    }

    fn values(&self) -> impl Iterator<Item = &SlotStat> {
        self.0.iter().map(|(_, v)| v)
    }

    fn values_mut(&mut self) -> impl Iterator<Item = &mut SlotStat> {
        self.0.iter_mut().map(|(_, v)| v)
    }
}

struct Tracked {
    counters: Arc<Counters>,
    size: u64,
    /// A tracker session is open (Start sent, Stop not yet).
    active: bool,
    tiers: usize,
    slots: Slots,
    /// Last health sent to the session.
    health: Health,
    role: Role,
}

struct ShutdownState {
    deadline: Instant,
    reply: oneshot::Sender<ShutdownReport>,
    started: Instant,
    opened: u32,
}

struct Driver {
    hub: TrackerHub<BoxedTiming>,
    torrents: HashMap<TorrentId, Tracked>,
    sh: Arc<Shared>,
    key: u32,
    numwant: u32,
    /// What HTTP announces say about encryption (follows `network.encryption`).
    crypto: trs_tracker::Crypto,
    removing: Vec<TorrentId>,
    /// Reports drained while answering a manual re-announce; handled with the next batch.
    stash: Vec<trs_tracker::hub::HubReport>,
    /// Port announced to trackers: the listen port, or the external port a mapper obtained.
    ann_port: Option<u16>,
    /// While a port mapper is starting, commands wait (at most [`PORT_SETTLE`]) so that the
    /// torrents loaded at start-up are registered with the port that peers can reach.
    settling: Option<Instant>,
    held: std::collections::VecDeque<TrackerCmd>,
    /// Developer log (SPEC 8.7).
    log: Arc<DevLog>,
    /// The hub is tracing decisions because the log is on.
    traced: bool,
    /// Titles for the log lines.
    titles: HashMap<TorrentId, String>,
}

/// How long start-up waits for the port mapper before trackers are told the listen port.
const PORT_SETTLE: Duration = Duration::from_secs(5);

/// Quit-time `stopped` (SPEC 7B.3, 7C, §12 #16). The process is leaving and the answer to a
/// `stopped` is irrelevant, so at quit they leave together (the hub lifts the per-host cap and the
/// token bucket, up to 50 in flight over all hosts) and each waits only briefly: the connection
/// gets [`QUIT_STOP_LIMITS`]`.connect`, the answer `.answer` after the request is written.
const QUIT_STOP_LIMITS: trs_tracker::http::QuitLimits = trs_tracker::http::QuitLimits {
    connect: Duration::from_secs(3),
    answer: Duration::from_secs(2),
};
/// Everything sent or given up within this long after the quit begins, whatever deadline the
/// caller asked for (the former 120 s linger is gone).
const QUIT_TOTAL: Duration = Duration::from_secs(10);
/// Extra time a request already in flight gets once the quit has begun: the machine cannot send
/// `stopped` while a request is outstanding, and a slow regular announce is not worth a long wait.
const QUIT_INFLIGHT_GRACE: Duration = Duration::from_secs(3);

/// What the quit-time `stopped` requests did (all tasks add to it).
#[derive(Default)]
struct QuitTally {
    released: std::sync::atomic::AtomicU32,
    /// Never reached the tracker (DNS, connect, TLS failure, connect timeout).
    unreached: std::sync::atomic::AtomicU32,
    /// Written, but not answered in time.
    timed_out: std::sync::atomic::AtomicU32,
}

/// Start the driver task.
pub(crate) fn spawn(
    sh: Arc<Shared>,
    timing: Box<dyn TimingStore + Send>,
    key: u32,
    hold_until: Option<Millis>,
) -> TrackerHandle {
    let cfg = sh.cfg();
    let (numwant, version) = (cfg.numwant, cfg.client_version.clone());
    let wakeups = sh.diag.clone();
    let (tx, rx) = mpsc::unbounded_channel();
    let mut hub = TrackerHub::new(GateConfig::default(), BoxedTiming(timing), unix_ms() ^ 0xA5A5);
    if let Some(t) = hold_until {
        hub.hold_until(t);
    }
    let pm = cfg.portmap;
    let sh_log = sh.devlog.clone();
    let settling = (pm.enabled && (pm.upnp || pm.natpmp)).then(|| Instant::now() + PORT_SETTLE);
    let d = Driver {
        hub,
        torrents: HashMap::new(),
        sh,
        key,
        numwant,
        crypto: crypto_of(cfg.encryption),
        removing: Vec::new(),
        stash: Vec::new(),
        ann_port: None,
        settling,
        held: std::collections::VecDeque::new(),
        log: sh_log,
        traced: false,
        titles: HashMap::new(),
    };
    let handle = TrackerHandle { tx };
    d.sh.net.set_tracker(handle.clone());
    tokio::spawn(run(d, rx, version, wakeups));
    handle
}

/// The announce parameters for an encryption policy: prefer = `supportcrypto=1`, require adds
/// `requirecrypto=1`, off sends neither.
pub(crate) fn crypto_of(p: crate::config::EncryptionPolicy) -> trs_tracker::Crypto {
    use crate::config::EncryptionPolicy as E;
    match p {
        E::Off => trs_tracker::Crypto::Off,
        E::Prefer => trs_tracker::Crypto::Support,
        E::Require => trs_tracker::Crypto::Require,
    }
}

/// Tiers whose URLs belong to one tracker profile become one tier (SPEC 7C.4b): the alternate
/// domains of a private tracker are failovers of each other, never a second announce stream.
/// Uses the same matcher that attributes torrents to profiles. Without a shared profile, tiers
/// stay independent.
pub(crate) fn fold_profile_tiers(
    tiers: Vec<Vec<TrackerUrl>>,
    policy: Option<&crate::policy::PolicyData>,
) -> Vec<Vec<TrackerUrl>> {
    let Some(pol) = policy else { return tiers };
    trs_tracker::set::fold_tiers(tiers, |u| crate::policy::match_profile(&pol.profiles, [u.raw()]).profile)
}

/// The wall clock moved on by much more than the monotonic one: the machine slept in between.
/// (Both are read once per pass of the driver loop, so a pass that was merely late is not a sleep:
/// the margin is 30 s.)
pub(crate) fn slept(wall_then: u64, mono_then: Instant, wall_now: u64, mono_now: Instant) -> bool {
    let wall = wall_now.saturating_sub(wall_then);
    let mono = mono_now.saturating_duration_since(mono_then).as_millis() as u64;
    wall > mono + 30_000
}

/// "14:03:07" in UTC (the log line says what time the outage began; the file has the full stamp).
fn fmt_clock(ms: u64) -> String {
    let s = ms / 1000 % 86_400;
    format!("{:02}:{:02}:{:02} UTC", s / 3600, s / 60 % 60, s % 60)
}

/// "3 h 12 min", "4 min 5 s", "30 s".
fn fmt_span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s} s"),
        60..=3599 => format!("{} min {} s", s / 60, s % 60),
        _ => format!("{} h {} min", s / 3600, s / 60 % 60),
    }
}

fn clock() -> Millis {
    Millis(unix_ms())
}

async fn run(
    mut d: Driver,
    mut cmds: mpsc::UnboundedReceiver<TrackerCmd>,
    version: String,
    wakeups: Arc<Diag>,
) {
    let policy = d.sh.cfg().tracker_destinations;
    let http = HttpTransport::with_config(&version, trs_tracker::http::HttpConfig { policy, ..Default::default() });
    let udp = UdpTransport::bind(UdpConfig { policy, ..UdpConfig::default() }).await.ok();
    // Torrents added by `seedwell sim` (marked by the control layer) announce to the mock tracker on 127.0.0.1;
    // every other torrent keeps the configured policy.
    let http_sim = HttpTransport::with_config(&version, trs_tracker::http::HttpConfig { policy: trs_tracker::policy::DestinationPolicy { allow_loopback: true, ..policy }, ..Default::default() });
    let (done_tx, mut done_rx) = mpsc::unbounded_channel();
    let (extra_tx, mut extra_rx) = mpsc::unbounded_channel::<(String, u32)>();
    let mut shutdown: Option<ShutdownState> = None;
    let (quit_tx, _) = tokio::sync::watch::channel(false);
    let tally = Arc::new(QuitTally::default());
    // Wake detection without the OS (the headless daemon has no path monitor): the monotonic
    // clock stands still while the machine sleeps and the wall clock does not.
    let mut last_pass: Option<(u64, Instant)> = None;
    loop {
        if shutdown.is_some() && !*quit_tx.borrow() {
            quit_tx.send_replace(true);
        }
        if d.settling.is_some_and(|t| Instant::now() >= t) {
            // No mapping in time: trackers hear the listen port, as before.
            if let Some(s) = d.apply(TrackerCmd::PortKnown(None), shutdown.is_some()) {
                shutdown = Some(s);
            }
        }
        let now = clock();
        // Developer mode: one relaxed atomic load per pass; everything else below is skipped when off.
        let logging = d.log.enabled();
        if logging != d.traced {
            d.traced = logging;
            d.hub.set_trace(logging);
        }
        let mono = Instant::now();
        if let Some((wall, then)) = last_pass {
            if slept(wall, then, now.0, mono) {
                d.log_net("did_wake", "the clock jumped while the process was asleep: treated as a wake".into());
                d.hub.did_wake(now);
            }
        }
        last_pass = Some((now.0, mono));
        d.push_stats(now);
        // Signs of life from outside the tracker code (peers, DHT, inbound connections).
        d.hub.note_alive(Millis(d.sh.net.alive_ms()));
        let quitting = shutdown.is_some();
        for req in d.hub.tick(now) {
            let simulated = req.announce_params().is_some_and(|p| d.sh.simulated.lock().unwrap_or_else(|e| e.into_inner()).contains(&p.info_hash));
            let (http, udp, tx, extra) = (if simulated { http_sim.clone() } else { http.clone() }, udp.clone(), done_tx.clone(), extra_tx.clone());
            let ts_ms = now.0;
            // At quit a `stopped` that has not been answered in `QUIT_STOP_TIMEOUT` is given up
            // (the normal request timeout is 30 s), and a request that is still in flight when the
            // quit begins gets `QUIT_INFLIGHT_GRACE` more: the machine cannot send `stopped` while
            // a request is outstanding, and a tracker that is slow on a regular announce is not
            // worth a 30 s wait. The abandoned request counts as a timeout (ambiguous: a `completed`
            // stays marked as sent). Every other request keeps its own timeouts.
            let quit_stop = quitting && req.event() == Some(trs_tracker::Event::Stopped);
            if quit_stop {
                tally.released.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let tally = tally.clone();
            let mut quit_rx = quit_tx.subscribe();
            tokio::spawn(async move {
                let ticket = req.ticket();
                let started = Instant::now();
                let call = async {
                    match req.url().scheme() {
                        Scheme::Udp => match &udp {
                            Some(u) => {
                                let (r, t) = u.announce_traced(&req).await;
                                (r, Wire::Udp(t))
                            }
                            None => (Err(trs_tracker::AnnounceError::Transport(trs_tracker::TransportError::Connect)), Wire::None),
                        },
                        _ => {
                            let o = if quit_stop { http.announce_quit(&req, QUIT_STOP_LIMITS).await } else { http.announce(&req).await };
                            let _ = extra.send((req.url().host().to_owned(), o.extra_hops));
                            (o.result, Wire::Http(o.trace))
                        }
                    }
                };
                let patience = async {
                    if quit_stop {
                        // Backstop (HTTP limits itself; UDP has no finer hook): connect plus answer.
                        tokio::time::sleep(QUIT_STOP_LIMITS.total()).await;
                    } else if quit_rx.wait_for(|q| *q).await.is_ok() {
                        tokio::time::sleep(QUIT_INFLIGHT_GRACE).await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                };
                let mut abandoned = None;
                let (result, wire) = tokio::select! {
                    r = call => r,
                    () = patience => {
                        abandoned = Some(if quit_stop { "given up: the quit wait for stopped ran out" } else { "given up: the quit grace for a request in flight ran out" });
                        (Err(trs_tracker::AnnounceError::Transport(trs_tracker::TransportError::Timeout)), Wire::None)
                    }
                };
                if quit_stop {
                    use std::sync::atomic::Ordering::Relaxed;
                    match &result {
                        Err(e) if e.never_reached_tracker() => tally.unreached.fetch_add(1, Relaxed),
                        Err(trs_tracker::AnnounceError::Transport(trs_tracker::TransportError::Timeout)) => tally.timed_out.fetch_add(1, Relaxed),
                        _ => 0,
                    };
                }
                let fin = logging.then(|| Box::new(tracker_log::finished(&req, &http, wire, Some(&result), abandoned, started.elapsed(), ts_ms)));
                let _ = tx.send((ticket, result, fin));
            });
        }
        d.drain_net().await;
        d.forward_reports().await;
        d.drain_log();
        if let Some(ShutdownState { deadline, .. }) = &shutdown {
            let all_quiet = d.torrents.keys().all(|id| d.hub.is_quiescent(id));
            if all_quiet || Instant::now() >= *deadline {
                let Some(ShutdownState { reply, started, opened, .. }) = shutdown.take() else { break };
                let total = opened;
                let abandoned = d.torrents.keys().filter(|id| !d.hub.is_quiescent(id)).count() as u32;
                use std::sync::atomic::Ordering::Relaxed;
                let (released, unreached, timed_out) = (tally.released.load(Relaxed), tally.unreached.load(Relaxed), tally.timed_out.load(Relaxed));
                d.log_quit(format!("quit done: {} stopped confirmed, {abandoned} abandoned, waited {} ms", total.saturating_sub(abandoned), started.elapsed().as_millis()), "quit_done");
                d.log_quit(
                    format!(
                        "quit: {} stopped sent, {timed_out} timed out ({unreached} never reached the tracker, {abandoned} torrent(s) still open)",
                        released.saturating_sub(unreached)
                    ),
                    "quit_summary",
                );
                let _ = reply.send(ShutdownReport {
                    stopped_confirmed: total.saturating_sub(abandoned),
                    stopped_abandoned: abandoned,
                    waited: started.elapsed(),
                });
                break;
            }
        }
        let wait = match (d.hub.next_wake(now), &shutdown) {
            (w, Some(ShutdownState { deadline, .. })) => {
                let until = deadline.saturating_duration_since(Instant::now());
                // While stopping, poll quickly so confirmed stops are noticed promptly.
                w.map_or(until, |w| w.since(now).min(until)).min(Duration::from_millis(200))
            }
            (Some(w), None) => w.since(now),
            (None, None) => Duration::from_secs(3600),
        };
        let wait = if d.removing.is_empty() { wait } else { wait.min(Duration::from_millis(250)) };
        let wait = d.settling.map_or(wait, |t| wait.min(t.saturating_duration_since(Instant::now())));
        tokio::select! {
            biased;
            c = cmds.recv() => match c {
                None => break,
                Some(c) => {
                    if let Some(s) = d.apply(c, shutdown.is_some()) {
                        shutdown = Some(s);
                    }
                }
            },
            Some((ticket, result, fin)) = done_rx.recv() => {
                d.hub.complete(ticket, result, clock());
                if let Some(f) = fin {
                    d.log_finished(ticket, *f);
                }
            }
            Some((host, hops)) = extra_rx.recv() => d.hub.charge_extra(&host, hops),
            _ = tokio::time::sleep(wait.max(Duration::from_millis(5))) => {
                wakeups.wakeups.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

impl Driver {
    fn push_stats(&mut self, now: Millis) {
        for (id, t) in &self.torrents {
            if !t.active {
                continue;
            }
            self.hub.set_stats(
                id,
                Stats {
                    uploaded: t.counters.session_up(),
                    downloaded: t.counters.session_down(),
                    left: t.counters.left(),
                    size: t.size,
                },
                now,
            );
        }
    }

    /// Returns a shutdown state to install.
    fn apply(&mut self, c: TrackerCmd, stopping: bool) -> Option<ShutdownState> {
        let now = clock();
        let mut opened = 0u32;
        if stopping {
            if let TrackerCmd::Shutdown { reply, .. } = c {
                let _ = reply.send(ShutdownReport::default());
            }
            return None;
        }
        if self.settling.is_some()
            && !matches!(
                c,
                TrackerCmd::PortKnown(_)
                    | TrackerCmd::Shutdown { .. }
                    | TrackerCmd::NetworkChanged
                    | TrackerCmd::Path(_)
                    | TrackerCmd::WillSleep
                    | TrackerCmd::DidWake
                    | TrackerCmd::Alive
            )
        {
            self.held.push_back(c);
            return None;
        }
        match c {
            TrackerCmd::PortKnown(p) => {
                self.ann_port = p;
                self.settling = None;
                // Torrents already registered hear it with their next regular announce (no extra
                // request, no earlier schedule): the mapper may renew with another external port
                // or fall back to the listen port at any time.
                self.hub.set_port(p.unwrap_or_else(|| self.sh.port()));
                let mut out = None;
                while let Some(h) = self.held.pop_front() {
                    if let Some(s) = self.apply(h, false) {
                        out = Some(s);
                    }
                }
                return out;
            }
            TrackerCmd::Crypto(p) => {
                self.crypto = crypto_of(p);
                self.hub.set_crypto(self.crypto);
            }
            TrackerCmd::Role(id, role) => {
                if let Some(t) = self.torrents.get_mut(&id) {
                    t.role = role;
                    // The retry pace follows the role (SPEC 7C.4): Downloading 1 min..1 h, Seeding
                    // 15 min..6 h, SeedingDormant 30 min..12 h. The session sends it on every
                    // change: finishing, or the Sharing group drifting between Active and Dormant.
                    let r = match role {
                        Role::Downloading => trs_tracker::TorrentRole::Downloading,
                        Role::Seeding => trs_tracker::TorrentRole::Seeding,
                        Role::SeedingDormant => trs_tracker::TorrentRole::SeedingDormant,
                    };
                    self.hub.set_role(&id, r, now);
                }
            }
            TrackerCmd::Add(t) => {
                let tiers: Vec<Vec<TrackerUrl>> = t
                    .tiers
                    .iter()
                    .map(|tier| tier.iter().filter_map(|u| TrackerUrl::parse(u).ok()).collect())
                    .filter(|tier: &Vec<TrackerUrl>| !tier.is_empty())
                    .collect();
                let tiers = fold_profile_tiers(tiers, self.sh.policy().as_deref());
                let mut spec = TorrentSpec::new(
                    t.id,
                    self.sh.our_id,
                    self.key,
                    self.ann_port.unwrap_or_else(|| self.sh.port()),
                    tiers,
                );
                spec.numwant = self.numwant;
                spec.crypto = self.crypto;
                spec.added_complete = t.added_complete;
                spec.private = t.private;
                let ntiers = spec.tiers.len();
                if self.hub.add_torrent(spec, now).is_ok() {
                    self.torrents.insert(
                        t.id,
                        Tracked {
                            counters: t.counters,
                            size: t.size,
                            active: false,
                            tiers: ntiers,
                            slots: Slots::default(),
                            health: Health::Ok,
                            role: Role::Downloading,
                        },
                    );
                }
            }
            TrackerCmd::AddTrackers(id, urls) => {
                let t = self.torrents.get_mut(&id)?;
                let stats = Stats {
                    uploaded: t.counters.session_up(),
                    downloaded: t.counters.session_down(),
                    left: t.counters.left(),
                    size: t.size,
                };
                let role = match t.role {
                    Role::Downloading => trs_tracker::TorrentRole::Downloading,
                    Role::Seeding => trs_tracker::TorrentRole::Seeding,
                    Role::SeedingDormant => trs_tracker::TorrentRole::SeedingDormant,
                };
                for u in urls {
                    if let Ok(url) = TrackerUrl::parse(&u) {
                        if self.hub.add_tracker(&id, url, Some(stats), role, now) {
                            t.tiers += 1;
                        }
                    }
                }
            }
            TrackerCmd::Start(id) => {
                if let Some(t) = self.torrents.get_mut(&id) {
                    if !t.active {
                        // A new tracker session: the session counters restart from zero.
                        t.counters.begin_session();
                        t.active = true;
                        // A new session: what the old one learned is stale.
                        t.slots.values_mut().for_each(|s| s.state = SlotState::Unknown);
                        if t.health != Health::Ok {
                            t.health = Health::Ok;
                            let _ = self.sh.session.try_send(SessionMsg::TrackerHealth { id, health: Health::Ok });
                        }
                        self.hub.set_stats(
                            &id,
                            Stats { uploaded: 0, downloaded: 0, left: t.counters.left(), size: t.size },
                            now,
                        );
                        self.hub.torrent_event(&id, TorrentEvent::Start, now);
                    }
                }
            }
            TrackerCmd::Stop(id) => {
                if let Some(t) = self.torrents.get_mut(&id) {
                    if t.active {
                        t.active = false;
                        self.hub.set_stats(
                            &id,
                            Stats {
                                uploaded: t.counters.session_up(),
                                downloaded: t.counters.session_down(),
                                left: t.counters.left(),
                                size: t.size,
                            },
                            now,
                        );
                        self.hub.torrent_event(&id, TorrentEvent::Stop, now);
                    }
                }
            }
            TrackerCmd::Complete(id) => {
                if let Some(t) = self.torrents.get(&id) {
                    self.hub.set_stats(
                        &id,
                        Stats {
                            uploaded: t.counters.session_up(),
                            downloaded: t.counters.session_down(),
                            left: 0,
                            size: t.size,
                        },
                        now,
                    );
                    self.hub.torrent_event(&id, TorrentEvent::Complete, now);
                }
            }
            TrackerCmd::Remove(id) => {
                // Removal waits for the `stopped` to be answered (or given up); retried by tick.
                if let Some(t) = self.torrents.get_mut(&id) {
                    if t.active {
                        t.active = false;
                        self.hub.torrent_event(&id, TorrentEvent::Stop, now);
                    }
                }
                self.pending_removal(id);
            }
            TrackerCmd::Reannounce(id, reply) => {
                let _ = reply.send(self.reannounce(id, now));
            }
            TrackerCmd::SetSize(id, size) => {
                if let Some(t) = self.torrents.get_mut(&id) {
                    t.size = size;
                }
            }
            TrackerCmd::Info(id, reply) => {
                let _ = reply.send(self.info(&id, now));
            }
            TrackerCmd::NetworkChanged => self.hub.network_changed(now),
            TrackerCmd::Path(ok) => {
                self.log_net("path", format!("the OS reports the network path is {}", if ok { "satisfied" } else { "unsatisfied" }));
                self.hub.path_changed(ok, now);
            }
            TrackerCmd::WillSleep => self.log_net("will_sleep", "the Mac is going to sleep: nothing to do".into()),
            TrackerCmd::DidWake => {
                self.log_net("did_wake", "the Mac woke: announces wait for the network (or 5 s), then one probe decides".into());
                self.hub.did_wake(now);
            }
            TrackerCmd::Alive => self.hub.net_hint(now),
            TrackerCmd::Title(id, t) => {
                self.titles.insert(id, t);
            }
            TrackerCmd::Shutdown { deadline, reply } => {
                // However long the caller would wait, the stops get QUIT_TOTAL at most.
                let deadline = deadline.min(Instant::now() + QUIT_TOTAL);
                // From here `stopped` leaves outside the per-host cap and the bucket.
                self.hub.begin_quit();
                for (id, t) in self.torrents.iter_mut() {
                    if t.active {
                        opened += 1;
                        t.active = false;
                        self.hub.set_stats(
                            id,
                            Stats {
                                uploaded: t.counters.session_up(),
                                downloaded: t.counters.session_down(),
                                left: t.counters.left(),
                                size: t.size,
                            },
                            now,
                        );
                        // One `stopped` attempt per tracker: a retry would only come after the first
                        // backoff step (a minute or more), and the process is leaving.
                        self.hub.quit(id, now);
                    }
                }
                self.log_quit(
                    format!("quit: {opened} open session(s) get one stopped attempt each (up to 50 at once), {} s to finish", deadline.saturating_duration_since(Instant::now()).as_secs()),
                    "quit_begin",
                );
                return Some(ShutdownState { deadline, reply, started: Instant::now(), opened });
            }
        }
        None
    }

    /// The tracker URL of a slot (raw: masked by the log).
    fn raw_url(&self, ih: &TorrentId, slot: u16) -> String {
        self.hub.set(ih).and_then(|s| s.url(slot)).map(|u| u.raw().to_owned()).unwrap_or_default()
    }

    fn pace(&self, ih: &TorrentId) -> &'static str {
        match self.torrents.get(ih).map(|t| t.role) {
            Some(Role::Seeding) => "seeding",
            Some(Role::SeedingDormant) => "dormant seed",
            _ => "downloading",
        }
    }

    fn log_net(&self, code: &'static str, note: String) {
        if !self.traced {
            return;
        }
        self.log.log(&Entry { ts_ms: unix_ms(), kind: Kind::Decision, decision: Some(code), note: Some(note), ..Entry::default() });
    }

    /// What the hub decided about connectivity (SPEC 7C.4c): publish it, tell the session, log it.
    async fn drain_net(&mut self) {
        use trs_tracker::outage::{NetEvent, Reason};
        for ev in self.hub.take_net_events() {
            match ev {
                NetEvent::Offline { at, reason, forgiven, due } => {
                    self.sh.net.set_held(true);
                    let _ = self.sh.session.try_send(SessionMsg::Net { held: true });
                    if reason == Reason::Wake {
                        self.log_net("checking", format!("woke: holding announces, dials and DHT until the network answers; {due} announce(s) already due"));
                        continue;
                    }
                    self.sh.net.set_shown(Some(at.0));
                    // The failures that revealed it were taken back by the hub: they are not the trackers'.
                    let mut ids = Vec::new();
                    for (id, t) in self.torrents.iter_mut() {
                        let mut touched = false;
                        for s in t.slots.values_mut() {
                            if matches!(&s.state, SlotState::Failed { reason, .. } if reason == "Tracker unreachable") {
                                s.state = SlotState::Unknown;
                                s.status = "waiting for the network".into();
                                touched = true;
                            }
                        }
                        if touched {
                            ids.push(*id);
                        }
                    }
                    for id in ids {
                        self.update_health(id).await;
                    }
                    self.log_net(
                        "offline",
                        format!(
                            "offline: since {}, reason {}; {forgiven} torrent(s)' recent failures taken back, {due} announce(s) already due; nothing is sent but probes",
                            fmt_clock(at.0),
                            reason.word()
                        ),
                    );
                }
                NetEvent::Probe { torrent, slot, host, n, .. } => {
                    let title = self.titles.get(&torrent).cloned();
                    if self.traced {
                        self.log.log(&Entry {
                            ts_ms: unix_ms(),
                            kind: Kind::Decision,
                            torrent: tracker_log::hex(&torrent),
                            title,
                            tracker: self.raw_url(&torrent, slot),
                            decision: Some("probe"),
                            note: Some(format!("probe {n}: this torrent's next announce to {host} tests the network")),
                            ..Entry::default()
                        });
                    }
                }
                NetEvent::ProbeSkipped { .. } => self.log_net("probe_skipped", "a probe was due but no announce was waiting; nothing to test with".into()),
                NetEvent::Online { after, due, spread, reason, .. } => {
                    self.sh.net.set_held(false);
                    self.sh.net.set_shown(None);
                    let _ = self.sh.session.try_send(SessionMsg::Net { held: false });
                    self.log_net(
                        "online",
                        format!(
                            "online after {}: {due} announce(s) due, released over {} s through the host gate{}",
                            fmt_span(after),
                            spread.as_secs(),
                            if reason == Reason::Wake { " (wake check)" } else { "" }
                        ),
                    );
                }
            }
        }
    }

    /// Hub decisions (retire, suspend, deferred, skipped ...) as log lines.
    fn drain_log(&mut self) {
        if !self.traced {
            return;
        }
        for n in self.hub.take_notes() {
            let url = self.raw_url(&n.torrent, n.slot);
            let mut e = tracker_log::note_entry(&n, &url);
            e.title = self.titles.get(&n.torrent).cloned();
            self.log.log(&e);
        }
    }

    fn log_quit(&self, note: String, code: &'static str) {
        if !self.traced {
            return;
        }
        self.log.log(&Entry { ts_ms: unix_ms(), kind: Kind::Quit, decision: Some(code), note: Some(note), ..Entry::default() });
    }

    fn log_gate(&self, ev: &trs_tracker::gate::GateEvent) {
        use trs_tracker::gate::GateEvent as G;
        let now = unix_ms();
        let (host, code, note, next) = match ev {
            G::HostPaused { host, until } => (
                host,
                "host_paused",
                format!("circuit breaker opened for {host}: no requests to it until the half-open probe"),
                Some(Next { at_ms: until.0, why: "breaker reopens for one probe request".into() }),
            ),
            G::HostResumed { host } => (host, "host_resumed", format!("circuit breaker closed for {host}: a probe succeeded"), None),
            G::HostTripped { host, reason, dropped } => (
                host,
                "host_tripped",
                format!("the gate stopped all requests to {host} ({reason}); {} queued request(s) dropped", dropped.len()),
                None,
            ),
        };
        self.log.log(&Entry { ts_ms: now, kind: Kind::Gate, tracker: host.clone(), decision: Some(code), note: Some(note), next, ..Entry::default() });
    }

    /// Reports that carry a decision (the exchanges themselves are logged by `log_finished`).
    fn log_report(&self, r: &trs_tracker::hub::HubReport) {
        let (code, note, next) = match &r.report {
            Report::ManualDeferred { until } => (
                "manual_deferred",
                "manual re-announce not sent: min interval / backoff floor not reached yet".to_string(),
                Some(Next { at_ms: until.0, why: "earliest time a manual re-announce is honoured".into() }),
            ),
            Report::Halted { reason } => ("halted", format!("tracker rejected us permanently ({reason}); announcing stops until you re-announce or restart the torrent"), None),
            Report::StopAbandoned => ("stop_abandoned", "stopped could not be delivered; given up".to_string(), None),
            Report::SessionClosed => ("session_closed", "tracker session closed".to_string(), None),
            Report::StaleResponse => ("stale_response", "an answer arrived that matches no request; ignored".to_string(), None),
            Report::InvariantViolation(m) => ("tripped", format!("self-check stopped this tracker: {m}"), None),
            Report::Announced { .. } | Report::Warning(_) | Report::Failed { .. } => return,
        };
        self.log.log(&Entry {
            ts_ms: unix_ms(),
            kind: Kind::Decision,
            torrent: tracker_log::hex(&r.torrent),
            title: self.titles.get(&r.torrent).cloned(),
            tracker: self.raw_url(&r.torrent, r.slot),
            decision: Some(code),
            note: Some(note),
            next,
            ..Entry::default()
        });
    }

    /// Add the title and the resulting schedule to a finished exchange and log it.
    fn log_finished(&mut self, ticket: trs_tracker::gate::Ticket, mut f: tracker_log::Finished) {
        let (ih, slot) = ticket.key();
        f.entry.title = self.titles.get(&ih).cloned();
        let stopped = f.entry.event == "stopped";
        if let Some(m) = self.hub.set(&ih).and_then(|s| s.machine(slot)) {
            if stopped {
                f.entry.next = None;
            } else if let Some((interval, min)) = f.timing {
                f.entry.next = m.regular_at().map(|at| Next { at_ms: at.0, why: tracker_log::schedule_why_ok(interval, min) });
            } else if let Some(err) = &f.error {
                if m.health() == trs_tracker::machine::Health::Halted {
                    let n = f.entry.note.take().map(|n| format!("{n}; ")).unwrap_or_default();
                    f.entry.note = Some(format!("{n}halted: announcing stops until you re-announce or restart the torrent"));
                } else if let Some(at) = m.retry_at() {
                    f.entry.next = Some(Next { at_ms: at.0, why: tracker_log::schedule_why_err(err, m.failures(), self.pace(&ih)) });
                }
            }
        }
        if stopped {
            let n = f.entry.note.take().map(|n| format!("{n}; ")).unwrap_or_default();
            f.entry.note = Some(format!("{n}stopped is best effort: no next announce for this session"));
        }
        self.log.log(&f.entry);
    }

    fn pending_removal(&mut self, id: TorrentId) {
        if self.hub.remove_torrent(&id) {
            self.torrents.remove(&id);
            self.titles.remove(&id);
        } else if let Some(t) = self.torrents.get_mut(&id) {
            // Keep it registered until the stop is through; the loop retries.
            let _ = t;
            self.removing.push(id);
        }
    }

    async fn forward_reports(&mut self) {
        // Retry removals whose `stopped` is now answered.
        let ids: Vec<TorrentId> = std::mem::take(&mut self.removing);
        for id in ids {
            self.pending_removal(id);
        }
        let mut touched: Vec<TorrentId> = Vec::new();
        let mut batch = std::mem::take(&mut self.stash);
        batch.extend(self.hub.take_reports());
        for r in batch {
            if self.traced {
                self.log_report(&r);
            }
            let Some(t) = self.torrents.get_mut(&r.torrent) else { continue };
            let slot = t.slots.entry(r.slot);
            if !touched.contains(&r.torrent) {
                touched.push(r.torrent);
            }
            match r.report {
                Report::Announced { response, .. } => {
                    slot.state = SlotState::Ok;
                    slot.status = "announced".into();
                    slot.seeds = response.complete;
                    slot.leechers = response.incomplete;
                    if let Some(w) = &response.warning {
                        slot.status = format!("announced (warning: {w})");
                    }
                    if response.complete.is_some() || response.incomplete.is_some() {
                        let _ = self
                            .sh
                            .session
                            .send(SessionMsg::TrackerSwarm { id: r.torrent, seeders: response.complete, leechers: response.incomplete })
                            .await;
                    }
                    let peers: Vec<SocketAddr> = response.peers.iter().map(|p| p.addr).collect();
                    if !peers.is_empty() {
                        let _ = self
                            .sh
                            .session
                            .send(SessionMsg::TrackerPeers { id: r.torrent, peers, incomplete: response.incomplete })
                            .await;
                    }
                }
                Report::Warning(w) => slot.status = format!("warning: {w}"),
                Report::Failed { error, retry_at, .. } => {
                    slot.status = format!("failed: {error}");
                    slot.state = match &error {
                        // The tracker answered and said something: that is a message, not an outage.
                        trs_tracker::AnnounceError::Rejected { .. } => SlotState::Warn,
                        trs_tracker::AnnounceError::Transport(_) => {
                            SlotState::Failed { reason: "Tracker unreachable".into(), retry_at_ms: retry_at.0 }
                        }
                        trs_tracker::AnnounceError::Http { status, .. } => {
                            SlotState::Failed { reason: format!("Tracker answered {status}"), retry_at_ms: retry_at.0 }
                        }
                        trs_tracker::AnnounceError::Garbage(_) => {
                            SlotState::Failed { reason: "Tracker sent an unreadable answer".into(), retry_at_ms: retry_at.0 }
                        }
                    };
                }
                Report::Halted { reason } => {
                    slot.status = format!("halted: {reason}");
                    slot.state = SlotState::Halted(reason);
                }
                Report::ManualDeferred { .. } => slot.status = "re-announce deferred".into(),
                Report::SessionClosed | Report::StopAbandoned | Report::StaleResponse => {}
                Report::InvariantViolation(m) => slot.status = format!("stopped by self-check: {m}"),
            }
        }
        for id in touched {
            self.update_health(id).await;
        }
        // Gate events (breaker, trips) are consumed so the log cannot grow.
        for ev in self.hub.take_gate_events() {
            if self.traced {
                self.log_gate(&ev);
            }
        }
    }

    /// Ask the hub for a manual re-announce and report, per tracker, whether the gate lets it
    /// through now or defers it (`ManualDeferred`: min interval, backoff).
    fn reannounce(&mut self, id: TorrentId, now: Millis) -> Vec<ReannounceResult> {
        self.hub.manual_reannounce(&id, now);
        let mut deferred: HashMap<u16, u64> = HashMap::new();
        for r in self.hub.take_reports() {
            match (&r.report, r.torrent == id) {
                (Report::ManualDeferred { until }, true) => {
                    deferred.insert(r.slot, until.0);
                    if self.traced {
                        self.log_report(&r);
                    }
                }
                _ => self.stash.push(r),
            }
        }
        let Some(set) = self.hub.set(&id) else { return Vec::new() };
        let mut out = Vec::new();
        for slots in set.tiers() {
            for &slot in slots {
                let Some(url) = set.url(slot) else { continue };
                // A halted tracker is not "deferred" by the gate: the machine re-arms its retry no
                // sooner than a minute after the last attempt. Both are "not now".
                let until = deferred
                    .get(&slot)
                    .copied()
                    .or_else(|| set.machine(slot).and_then(|m| m.retry_at()).map(|t| t.0).filter(|t| *t > now.0));
                out.push(ReannounceResult { id, url_redacted: url.to_string(), sent: until.is_none(), not_before_ms: until });
            }
        }
        out
    }

    /// Tell the session when the aggregate health of a torrent's trackers changes: an error is
    /// "every tracker failing", never one failing tracker among several or a warning.
    async fn update_health(&mut self, id: TorrentId) {
        let total: usize = self.hub.set(&id).map_or(0, |s| s.tiers().iter().map(|t| t.len()).sum());
        let Some(t) = self.torrents.get_mut(&id) else { return };
        let states: Vec<&SlotState> = t.slots.values().map(|s| &s.state).collect();
        let any_ok = states.iter().any(|s| matches!(s, SlotState::Ok | SlotState::Warn));
        let all_known = total > 0 && states.len() >= total && states.iter().all(|s| **s != SlotState::Unknown);
        let health = if any_ok || !all_known {
            Health::Ok
        } else if states.iter().all(|s| matches!(s, SlotState::Halted(_))) {
            match states.first() {
                Some(SlotState::Halted(r)) => Health::Dead { reason: format!("Tracker: {}", sentence_case(r)) },
                _ => Health::Ok,
            }
        } else {
            let retry = states.iter().filter_map(|s| if let SlotState::Failed { retry_at_ms, .. } = s { Some(*retry_at_ms) } else { None }).min();
            let reason = states
                .iter()
                .find_map(|s| if let SlotState::Failed { reason, .. } = s { Some(reason.clone()) } else { None })
                .unwrap_or_else(|| "Tracker unreachable".into());
            Health::Failing { reason, retry_at_ms: retry }
        };
        if health != t.health {
            t.health = health.clone();
            let _ = self.sh.session.send(SessionMsg::TrackerHealth { id, health }).await;
        }
    }

    fn info(&self, id: &TorrentId, now: Millis) -> Vec<TrackerInfo> {
        let (Some(set), Some(t)) = (self.hub.set(id), self.torrents.get(id)) else {
            return Vec::new();
        };
        let _ = now;
        let mut out = Vec::new();
        for (tier_idx, slots) in set.tiers().iter().enumerate() {
            for &slot in slots {
                let Some(url) = set.url(slot) else { continue };
                let (last, next, failures) = match set.machine(slot) {
                    Some(m) => (m.record().last_announce_at, m.regular_at(), m.failures()),
                    None => (None, None, 0),
                };
                let st = t.slots.get(&slot).cloned().unwrap_or_default();
                let (last_success, retry_at, manual_ok, tripped) = match set.machine(slot) {
                    Some(m) => (m.last_success(), m.retry_at(), m.record().min_allowed_at, m.tripped()),
                    None => (None, None, None, None),
                };
                out.push(TrackerInfo {
                    url: url.to_string(),
                    shown_url: trs_state::url::mask_secrets(url.raw()),
                    full_url: SecretUrl::new(url.raw()),
                    tier: tier_idx as u32,
                    status: if st.status.is_empty() { "waiting".into() } else { trs_state::url::mask_urls_in_text(&st.status) },
                    last_announce_ms: last.map(|m| m.0),
                    next_announce_ms: next.map(|m| m.0),
                    seeds: st.seeds,
                    leechers: st.leechers,
                    failures,
                    state: match &st.state {
                        SlotState::Unknown => TrackerState::Waiting,
                        SlotState::Ok => TrackerState::Working,
                        SlotState::Warn => TrackerState::Warn,
                        SlotState::Failed { .. } => TrackerState::Backoff,
                        SlotState::Halted(_) => TrackerState::Halted,
                    },
                    last_success_ms: last_success.map(|m| m.0),
                    retry_at_ms: match &st.state {
                        SlotState::Failed { retry_at_ms, .. } => Some(*retry_at_ms),
                        _ => retry_at.map(|m| m.0),
                    },
                    manual_ok_ms: manual_ok.map(|m| m.0),
                    tripped,
                });
            }
        }
        let _ = t.tiers;
        out
    }
}

#[cfg(test)]
mod wake_tests {
    use super::*;

    #[test]
    fn a_clock_jump_without_monotonic_time_is_a_sleep() {
        let t = Instant::now();
        let later = t + Duration::from_secs(60);
        // an ordinary minute
        assert!(!slept(1_000_000, t, 1_060_000, later));
        // a pass that was 20 s late is not a sleep
        assert!(!slept(1_000_000, t, 1_080_000, later));
        // ten hours of wall clock in one minute of uptime
        assert!(slept(1_000_000, t, 1_000_000 + 10 * 3600 * 1000, later));
        // the clock set backwards is not a sleep
        assert!(!slept(2_000_000, t, 1_000_000, later));
    }
}

#[cfg(test)]
mod fold_tests {
    use super::*;
    use crate::policy::PolicyData;
    use trs_state::trackers::{Profile, TrackerProfiles, UrlPattern};

    fn tiers(urls: &[&str]) -> Vec<Vec<TrackerUrl>> {
        urls.iter().map(|u| vec![TrackerUrl::parse(u).unwrap()]).collect()
    }

    fn policy(patterns: &[&str]) -> PolicyData {
        let mut profiles = TrackerProfiles::default();
        if !patterns.is_empty() {
            profiles
                .add(Profile {
                    id: "tl".into(),
                    name: "TL".into(),
                    urls: patterns.iter().map(|p| UrlPattern::parse(p).unwrap()).collect(),
                    ..Profile::default()
                })
                .unwrap();
        }
        PolicyData { rules: Default::default(), profiles, private_min_seed: None, idle: None }
    }

    #[test]
    fn urls_of_one_profile_share_a_tier_and_others_stay_apart() {
        let t = tiers(&["http://a.tl.test/announce", "http://ipv6.other.test/announce", "http://b.tl.test/announce", "http://other.test/announce"]);
        let p = policy(&["a.tl.test", "b.tl.test"]);
        let out = fold_profile_tiers(t.clone(), Some(&p));
        let hosts: Vec<Vec<&str>> = out.iter().map(|t| t.iter().map(|u| u.host()).collect()).collect();
        assert_eq!(hosts, [vec!["a.tl.test", "b.tl.test"], vec!["ipv6.other.test"], vec!["other.test"]]);
        // no profile, or no policy: untouched (torrent.ubuntu.com / ipv6.torrent.ubuntu.com stay two streams)
        assert_eq!(fold_profile_tiers(t.clone(), Some(&policy(&[]))).len(), 4);
        assert_eq!(fold_profile_tiers(t, None).len(), 4);
    }
}

/// "unregistered torrent" -> "Unregistered torrent" (trackers write their reasons in lower case).
fn sentence_case(s: &str) -> String {
    let s = s.trim();
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}
