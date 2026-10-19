//! Shared test harness: a virtual-time world around a `TrackerHub`, a fake
//! network driven by a `Responder`, and the independent `Oracle`.
#![allow(dead_code)]

pub mod oracle;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Duration;

use trs_tracker::gate::{GateConfig, GatedRequest, Ticket};
use trs_tracker::hub::{HubReport, TorrentSpec, TrackerHub};
use trs_tracker::response::{AnnounceError, AnnounceResponse, TransportError};
use trs_tracker::url::Scheme;
use trs_tracker::{InfoHash20, MemoryStore, Millis, Stats, TorrentEvent, TrackerUrl};

use oracle::{Oracle, User};

pub fn ok_response(interval: u64, min: Option<u64>) -> AnnounceResponse {
    let t = trs_tracker::clamp_timing(Some(interval as i64), min.map(|m| m as i64));
    AnnounceResponse {
        interval: t.interval,
        min_interval: t.min_interval,
        tracker_id: None,
        complete: None,
        incomplete: None,
        downloaded: None,
        peers: vec![],
        warning: None,
        external_ip: None,
    }
}

pub fn timeout_err() -> AnnounceError {
    AnnounceError::Transport(TransportError::Timeout)
}

/// What a fake tracker host does with a request.
pub struct Reply {
    pub latency: Duration,
    pub result: Result<AnnounceResponse, AnnounceError>,
    /// Further answers for the SAME logical request (e.g. the other address
    /// family of a dual-stack host), as (delay after the first, result).
    pub extra: Vec<(Duration, Result<AnnounceResponse, AnnounceError>)>,
}

impl Reply {
    pub fn new(latency: Duration, result: Result<AnnounceResponse, AnnounceError>) -> Reply {
        Reply {
            latency,
            result,
            extra: Vec::new(),
        }
    }
}

pub trait Responder {
    fn respond(&mut self, now: Millis, host: &str, req: &GatedRequest) -> Reply;
}

/// Outcome bookkeeping per (torrent, tracker), for the announce-count bound.
#[derive(Default, Clone, Debug)]
pub struct KeyStat {
    pub ok: u64,
    pub failed: u64,
    pub streaks: u64,
    pub fail_ms: u64,
    pub streak_start: Option<Millis>,
}

pub struct TorrentModel {
    pub run_since: Option<Millis>,
    pub run_ms: u64,
    pub spec: TorrentSpec,
    pub size: u64,
    pub left: u64,
    pub up: u64,
    pub down: u64,
    /// The user wants it running (so a crash restarts it).
    pub running: bool,
}

struct Pending {
    ticket: Ticket,
    host: String,
    seq: u64,
    due_at: Millis,
    result: Result<AnnounceResponse, AnnounceError>,
}

pub struct World<R: Responder> {
    pub hub: Option<TrackerHub<MemoryStore>>,
    pub now: Millis,
    pending: BTreeMap<(Millis, u64), Pending>,
    seq: u64,
    pub oracle: Oracle,
    pub responder: R,
    pub torrents: BTreeMap<InfoHash20, TorrentModel>,
    pub gate_cfg: GateConfig,
    pub reports: Vec<HubReport>,
    pub keep_reports: bool,
    pub per_host_sent: HashMap<String, u64>,
    pub per_key_sent: HashMap<(InfoHash20, u16), u64>,
    pub per_key_events: HashMap<(InfoHash20, u16), u64>,
    pub key_stats: HashMap<(InfoHash20, u16), KeyStat>,
    pub total_sent: u64,
    pub peer_id_gen: u8,
    /// Engine feeds garbage counters (decreasing, left > size) when set.
    pub sloppy_engine: bool,
    pub sloppy_flip: u64,
    pub crashes: u32,
    pub breaker_opens: u64,
    /// Until this time only `stopped` may be released (crash-loop hold).
    pub hold_until: Millis,
}

impl<R: Responder> World<R> {
    pub fn new(responder: R, gate_cfg: GateConfig) -> Self {
        let mut oracle = Oracle::new();
        oracle.burst = gate_cfg.burst;
        oracle.token_interval = gate_cfg.token_interval;
        oracle.max_per_host = gate_cfg.max_in_flight_per_host;
        oracle.http_deadline = gate_cfg.http_deadline;
        oracle.udp_deadline = gate_cfg.udp_deadline;
        World {
            hub: Some(TrackerHub::new(gate_cfg.clone(), MemoryStore::new(), 1)),
            now: Millis::ZERO,
            pending: BTreeMap::new(),
            seq: 0,
            oracle,
            responder,
            torrents: BTreeMap::new(),
            gate_cfg,
            reports: Vec::new(),
            keep_reports: false,
            per_host_sent: HashMap::new(),
            per_key_sent: HashMap::new(),
            per_key_events: HashMap::new(),
            key_stats: HashMap::new(),
            total_sent: 0,
            peer_id_gen: 1,
            sloppy_engine: false,
            sloppy_flip: 0,
            crashes: 0,
            breaker_opens: 0,
            hold_until: Millis::ZERO,
        }
    }

    fn user_action(&mut self, ih: InfoHash20, what: User) {
        // The hub has already expired everything due; the oracle expires lazily.
        self.oracle.advance(self.now, true);
        self.oracle.user(ih, what);
    }

    pub fn hub_ref(&self) -> &TrackerHub<MemoryStore> {
        self.hub.as_ref().expect("hub alive")
    }

    pub fn hub(&mut self) -> &mut TrackerHub<MemoryStore> {
        self.hub.as_mut().expect("hub alive")
    }

    pub fn peer_id(&self) -> [u8; 20] {
        let mut p = *b"-SX0001-............";
        p[8..].fill(self.peer_id_gen);
        p
    }

    pub fn add_torrent(&mut self, mut spec: TorrentSpec, size: u64, left: u64) {
        spec.peer_id = self.peer_id();
        spec.added_complete = spec.added_complete || left == 0;
        let urls: HashSet<String> = spec
            .tiers
            .iter()
            .flatten()
            .map(|u| u.raw().to_owned())
            .collect();
        self.oracle
            .register(spec.info_hash, urls, size, spec.added_complete);
        let ih = spec.info_hash;
        let now = self.now;
        self.hub()
            .add_torrent(spec.clone(), now)
            .expect("fresh torrent");
        self.torrents.insert(
            ih,
            TorrentModel {
                run_since: None,
                run_ms: 0,
                spec,
                size,
                left,
                up: 0,
                down: 0,
                running: false,
            },
        );
    }

    fn push_stats(&mut self, ih: InfoHash20) {
        let now = self.now;
        let sloppy = self.sloppy_engine;
        self.sloppy_flip = self
            .sloppy_flip
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let flip = self.sloppy_flip >> 33;
        let Some(t) = self.torrents.get(&ih) else {
            return;
        };
        let mut st = Stats {
            uploaded: t.up,
            downloaded: t.down,
            corrupt: 0,
            left: t.left,
            size: t.size,
        };
        if sloppy {
            match flip % 4 {
                0 => st.uploaded = st.uploaded.saturating_sub(flip % 1000),
                1 => st.left = st.size + flip % 1000,
                2 => st.downloaded = st.downloaded.saturating_sub(flip % 777),
                _ => {}
            }
        }
        self.hub().set_stats(&ih, st, now);
    }

    /// Engine-side progress: bytes move, then stats are pushed.
    pub fn progress(&mut self, ih: InfoHash20, up: u64, down: u64) {
        if let Some(t) = self.torrents.get_mut(&ih) {
            t.up += up;
            let d = down.min(t.left);
            t.down += d;
            t.left -= d;
        }
        self.push_stats(ih);
    }

    fn set_running(&mut self, ih: InfoHash20, on: bool) {
        let now = self.now;
        if let Some(t) = self.torrents.get_mut(&ih) {
            t.running = on;
            match (on, t.run_since) {
                (true, None) => t.run_since = Some(now),
                (false, Some(s)) => {
                    t.run_ms += now.0 - s.0;
                    t.run_since = None;
                }
                _ => {}
            }
        }
    }

    /// Total time the user had the torrent running, up to now.
    pub fn active_ms(&self, ih: &InfoHash20) -> u64 {
        self.torrents.get(ih).map_or(0, |t| {
            t.run_ms + t.run_since.map_or(0, |s| self.now.0 - s.0)
        })
    }

    pub fn start(&mut self, ih: InfoHash20) {
        self.set_running(ih, true);
        self.user_action(ih, User::StartOrResume);
        self.push_stats(ih);
        let now = self.now;
        self.hub().torrent_event(&ih, TorrentEvent::Start, now);
    }

    pub fn stop(&mut self, ih: InfoHash20) {
        self.set_running(ih, false);
        self.push_stats(ih);
        let now = self.now;
        self.hub().torrent_event(&ih, TorrentEvent::Stop, now);
    }

    pub fn pause(&mut self, ih: InfoHash20) {
        self.set_running(ih, false);
        let now = self.now;
        self.hub().torrent_event(&ih, TorrentEvent::Pause, now);
    }

    pub fn resume(&mut self, ih: InfoHash20) {
        self.set_running(ih, true);
        self.user_action(ih, User::StartOrResume);
        self.push_stats(ih);
        let now = self.now;
        self.hub().torrent_event(&ih, TorrentEvent::Resume, now);
    }

    pub fn complete(&mut self, ih: InfoHash20) {
        if let Some(t) = self.torrents.get_mut(&ih) {
            t.down += t.left;
            t.left = 0;
        }
        self.push_stats(ih);
        let now = self.now;
        self.hub().torrent_event(&ih, TorrentEvent::Complete, now);
    }

    pub fn manual(&mut self, ih: InfoHash20) {
        self.user_action(ih, User::Manual);
        self.push_stats(ih);
        let now = self.now;
        self.hub().manual_reannounce(&ih, now);
    }

    pub fn network_changed(&mut self) {
        let keys: Vec<_> = self.torrents.keys().copied().collect();
        for k in &keys {
            self.user_action(*k, User::Network);
            self.push_stats(*k);
        }
        let now = self.now;
        self.hub().network_changed(now);
    }

    /// Machine sleeps for `d`: nothing runs, then it wakes.
    pub fn sleep(&mut self, d: Duration) {
        self.now = self.now + d;
        self.oracle.woke(self.now);
        // Answers that arrived during the sleep are processed on wake-up
        // before any timer (the hub has not ticked yet).
        self.deliver_due();
        let keys: Vec<_> = self.torrents.keys().copied().collect();
        for k in &keys {
            self.oracle.user(*k, User::Wake);
            self.push_stats(*k);
        }
        let now = self.now;
        self.hub().wake(now);
        self.pump();
    }

    /// kill -9: in-flight work vanishes; persisted timing survives.
    pub fn crash(&mut self, new_identity: bool) {
        self.crashes += 1;
        let hub = self.hub.take().expect("hub alive");
        let store = hub.into_store();
        self.pending.clear();
        if new_identity {
            self.peer_id_gen = self.peer_id_gen.wrapping_add(1);
        }
        self.oracle.crash(self.now, new_identity);
        let mut hub = TrackerHub::new(self.gate_cfg.clone(), store, 1000 + u64::from(self.crashes));
        let pid = self.peer_id();
        let now = self.now;
        let running: Vec<InfoHash20> = self
            .torrents
            .iter()
            .filter(|(_, t)| t.running)
            .map(|(k, _)| *k)
            .collect();
        for t in self.torrents.values_mut() {
            if new_identity {
                t.spec.peer_id = pid;
            }
            // a fresh process starts counting a new session
            t.up = 0;
            t.down = 0;
            hub.add_torrent(t.spec.clone(), now).expect("fresh hub");
        }
        self.hub = Some(hub);
        for ih in running {
            self.user_action(ih, User::StartOrResume);
            self.push_stats(ih);
            let now = self.now;
            self.hub().torrent_event(&ih, TorrentEvent::Start, now);
        }
    }

    /// Run everything due at `self.now` until quiescent.
    pub fn pump(&mut self) {
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 100_000, "pump livelock at {:?}", self.now);
            let mut progressed = self.deliver_due();
            let now = self.now;
            self.oracle.advance(now, true);
            let reqs = self.hub().tick(now);
            self.drain_net();
            let reps = self.hub().take_reports();
            for r in &reps {
                if let trs_tracker::Report::InvariantViolation(why) = &r.report {
                    panic!("machine tripped its self-audit: {why}");
                }
            }
            if self.keep_reports {
                self.reports.extend(reps);
            }
            for ev in self.hub().take_gate_events() {
                match ev {
                    trs_tracker::gate::GateEvent::HostTripped { host, reason, .. } => {
                        panic!("gate tripped host {host}: {reason}")
                    }
                    trs_tracker::gate::GateEvent::HostPaused { .. } => self.breaker_opens += 1,
                    trs_tracker::gate::GateEvent::HostResumed { .. } => {}
                }
            }
            for r in reqs {
                progressed = true;
                self.on_release(r);
            }
            if !progressed {
                break;
            }
        }
    }

    /// Tell the oracle what the hub decided about connectivity (SPEC 7C.4c).
    fn drain_net(&mut self) {
        for ev in self.hub().take_net_events() {
            match ev {
                trs_tracker::outage::NetEvent::Offline { at, .. } => self.oracle.outage_started(at),
                trs_tracker::outage::NetEvent::Online { at, .. } => self.oracle.outage_ended(at),
                _ => {}
            }
        }
    }

    /// Deliver every answer that is due at `self.now`, in due order.
    fn deliver_due(&mut self) -> bool {
        let mut any = false;
        while let Some((&k, _)) = self.pending.iter().next() {
            if k.0 > self.now {
                break;
            }
            let p = self.pending.remove(&k).expect("present");
            let (ih, slot) = p.ticket.key();
            let timing = p
                .result
                .as_ref()
                .map(|r| r.timing().sanitized())
                .map_err(Clone::clone);
            if p.ticket.event() != Some(trs_tracker::Event::Stopped) {
                let ks = self.key_stats.entry((ih, slot)).or_default();
                if timing.is_ok() {
                    ks.ok += 1;
                    if let Some(s) = ks.streak_start.take() {
                        ks.fail_ms += self.now.0.saturating_sub(s.0);
                    }
                } else {
                    ks.failed += 1;
                    if ks.streak_start.is_none() {
                        ks.streak_start = Some(self.now);
                        ks.streaks += 1;
                    }
                }
            }
            self.oracle
                .completed(self.now, k.0, ih, slot, p.seq, p.ticket.event(), &timing);
            self.oracle.host_release(&p.host, p.seq);
            let now = self.now;
            let result = p.result;
            self.hub().complete(p.ticket, result, now);
            self.drain_net();
            any = true;
        }
        any
    }

    fn on_release(&mut self, r: GatedRequest) {
        let (ih, slot) = r.ticket().key();
        let url = r.url();
        let host = url.host().to_owned();
        let udp = url.scheme() == Scheme::Udp;
        let params = r.announce_params().expect("announce").clone();
        assert!(
            self.now >= self.hold_until || params.event == Some(trs_tracker::Event::Stopped),
            "request released during a crash-loop hold"
        );
        let oseq = self
            .oracle
            .sent(self.now, ih, slot, url.raw(), &host, udp, &params);
        self.total_sent += 1;
        match self.per_host_sent.get_mut(&host) {
            Some(c) => *c += 1,
            None => {
                self.per_host_sent.insert(host.clone(), 1);
            }
        }
        *self.per_key_sent.entry((ih, slot)).or_default() += 1;
        if params.event.is_some() {
            *self.per_key_events.entry((ih, slot)).or_default() += 1;
        }
        let reply = self.responder.respond(self.now, &host, &r);
        for (delay, result) in reply.extra {
            self.seq += 1;
            let at = self.now + reply.latency + delay;
            self.pending.insert(
                (at, self.seq),
                Pending {
                    ticket: r.ticket(),
                    host: host.clone(),
                    seq: oseq,
                    due_at: at,
                    result,
                },
            );
        }
        self.seq += 1;
        self.pending.insert(
            (self.now + reply.latency, self.seq),
            Pending {
                ticket: r.ticket(),
                host,
                seq: oseq,
                due_at: self.now + reply.latency,
                result: reply.result,
            },
        );
    }

    /// Next time anything is due.
    pub fn next_due(&mut self) -> Option<Millis> {
        let now = self.now;
        let a = self.pending.keys().next().map(|k| k.0);
        let b = self.hub().next_wake(now);
        match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        }
    }

    /// Advance to `t`, processing every due event in order.
    pub fn run_until(&mut self, t: Millis) {
        let mut stuck = 0;
        loop {
            self.pump();
            match self.next_due() {
                Some(n) if n <= t => {
                    stuck = if n <= self.now { stuck + 1 } else { 0 };
                    assert!(
                        stuck < 50,
                        "scheduler wants to run at {n:?} but nothing happens at {:?}",
                        self.now
                    );
                    self.now = n.max(self.now);
                }
                _ => break,
            }
        }
        if t > self.now {
            self.now = t;
        }
        self.pump();
    }

    pub fn run_for(&mut self, d: Duration) {
        let t = self.now + d;
        self.run_until(t);
    }
}

pub fn ih(n: u32) -> InfoHash20 {
    let mut h = [0u8; 20];
    h[..4].copy_from_slice(&n.to_be_bytes());
    h[4..8].copy_from_slice(&(n.wrapping_mul(2654435761)).to_be_bytes());
    h
}

pub fn spec(n: u32, port: u16, tiers: &[&[&str]]) -> TorrentSpec {
    let tiers: Vec<Vec<TrackerUrl>> = tiers
        .iter()
        .map(|t| {
            t.iter()
                .map(|u| TrackerUrl::parse(u).expect("url"))
                .collect()
        })
        .collect();
    TorrentSpec::new(
        ih(n),
        *b"-SX0001-............",
        n.wrapping_mul(0x9E3779B1),
        port,
        tiers,
    )
}

/// The tracker's-eye view of "announce spam" (SPEC 7C.4a): more than one
/// non-event announce within a tracker's `min interval` for the same
/// (torrent, tracker). This is the detector that flagged Transmission 4.0.6.
pub struct SpamDetector {
    min_eff: Duration,
    last: HashMap<InfoHash20, Millis>,
    pub spam: u64,
    pub non_event: u64,
    pub events: u64,
}

impl SpamDetector {
    pub fn new(min_interval: Duration) -> Self {
        SpamDetector {
            min_eff: min_interval,
            last: HashMap::new(),
            spam: 0,
            non_event: 0,
            events: 0,
        }
    }

    /// Record an announce *arriving at the tracker*.
    pub fn arrive(&mut self, now: Millis, ih: InfoHash20, event: Option<trs_tracker::Event>) {
        if event.is_some() {
            self.events += 1;
        } else {
            self.non_event += 1;
            if self
                .last
                .get(&ih)
                .is_some_and(|prev| now.since(*prev) < self.min_eff)
            {
                self.spam += 1;
            }
        }
        self.last.insert(ih, now);
    }
}
