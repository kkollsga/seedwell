//! Support code for the `adversarial_*` test files.
//!
//! Deliberately independent of `tests/common`: a tiny virtual-time `Lab`
//! around a `TrackerHub` and a tracker-side `View` (what a tracker operator
//! would see) written from the SPEC, not from the implementation.
#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use trs_tracker::gate::{GateConfig, GateEvent, GatedRequest, Ticket};
use trs_tracker::hub::{HubReport, TorrentSpec, TrackerHub};
use trs_tracker::response::{AnnounceError, AnnounceResponse, TransportError};
use trs_tracker::{
    AnnounceParams, Event, InfoHash20, MemoryStore, Millis, Report, Stats, TorrentEvent,
    TrackerUrl, clamp_timing,
};

pub type Resp = Result<AnnounceResponse, AnnounceError>;

pub fn ms(n: u64) -> Millis {
    Millis(n)
}
pub fn s(n: u64) -> Millis {
    Millis::from_secs(n)
}
pub fn d(n: u64) -> Duration {
    Duration::from_secs(n)
}

pub fn ih(n: u32) -> InfoHash20 {
    let mut h = [0u8; 20];
    h[..4].copy_from_slice(&n.to_be_bytes());
    h[4..8].copy_from_slice(&n.wrapping_mul(2_654_435_761).to_be_bytes());
    h
}

pub fn url(u: &str) -> TrackerUrl {
    TrackerUrl::parse(u).expect("test url")
}

/// Spec with the given tiers (`&[&[url]]`), size/left are pushed separately.
pub fn spec(n: u32, tiers: &[&[&str]]) -> TorrentSpec {
    let tiers = tiers
        .iter()
        .map(|t| t.iter().map(|u| url(u)).collect())
        .collect();
    TorrentSpec::new(
        ih(n),
        *b"-SX0001-............",
        n.wrapping_mul(0x9E37_79B1) | 1,
        6881,
        tiers,
    )
}

pub fn ok(interval: u64, min: Option<u64>) -> Resp {
    let t = clamp_timing(Some(interval as i64), min.map(|m| m as i64));
    Ok(AnnounceResponse {
        interval: t.interval,
        min_interval: t.min_interval,
        tracker_id: None,
        complete: None,
        incomplete: None,
        downloaded: None,
        peers: vec![],
        warning: None,
        external_ip: None,
    })
}

pub fn ok_with_id(interval: u64, id: &[u8]) -> Resp {
    let mut r = ok(interval, None).unwrap();
    r.tracker_id = Some(id.to_vec().into_boxed_slice());
    Ok(r)
}

pub fn timeout() -> Resp {
    Err(AnnounceError::Transport(TransportError::Timeout))
}
pub fn http_status(code: u16) -> Resp {
    Err(AnnounceError::Http {
        status: code,
        retry_after: None,
    })
}
pub fn reject(reason: &str) -> Resp {
    Err(AnnounceError::Rejected {
        reason: reason.into(),
        retry_in: None,
    })
}

/// One request as a tracker sees it.
#[derive(Clone, Debug)]
pub struct Sent {
    pub n: usize,
    pub t: Millis,
    pub ih: InfoHash20,
    pub slot: u16,
    pub url: String,
    pub host: String,
    pub p: AnnounceParams,
}

impl Sent {
    pub fn event(&self) -> Option<Event> {
        self.p.event
    }
}

/// What the fake network does with a request: a list of (delay, answer).
/// An empty list means the request hangs (the gate times it out).
pub type Responder = Box<dyn FnMut(&Sent) -> Vec<(Duration, Resp)>>;

pub fn always(r: impl Fn() -> Resp + 'static) -> Responder {
    Box::new(move |_| vec![(Duration::from_millis(100), r())])
}

struct Pending {
    ticket: Ticket,
    result: Resp,
    sent_n: usize,
}

pub struct Lab {
    pub hub: Option<TrackerHub<MemoryStore>>,
    pub now: Millis,
    pub sent: Vec<Sent>,
    pub reports: Vec<HubReport>,
    pub gate_events: Vec<GateEvent>,
    pub responder: Responder,
    pending: BTreeMap<(Millis, u64), Pending>,
    seq: u64,
    pub cfg: GateConfig,
    pub specs: Vec<TorrentSpec>,
    pub running: HashMap<InfoHash20, bool>,
    pub hub_seed: u64,
    pub panic_on_trip: bool,
}

impl Lab {
    pub fn new(responder: Responder) -> Lab {
        Lab::with_cfg(responder, GateConfig::default())
    }

    pub fn with_cfg(responder: Responder, cfg: GateConfig) -> Lab {
        Lab {
            hub: Some(TrackerHub::new(cfg.clone(), MemoryStore::new(), 1)),
            now: Millis::ZERO,
            sent: vec![],
            reports: vec![],
            gate_events: vec![],
            responder,
            pending: BTreeMap::new(),
            seq: 0,
            cfg,
            specs: vec![],
            running: HashMap::new(),
            hub_seed: 1,
            panic_on_trip: true,
        }
    }

    pub fn hub(&mut self) -> &mut TrackerHub<MemoryStore> {
        self.hub.as_mut().expect("hub alive")
    }
    pub fn hub_ref(&self) -> &TrackerHub<MemoryStore> {
        self.hub.as_ref().expect("hub alive")
    }

    pub fn add(&mut self, spec: TorrentSpec) {
        let now = self.now;
        self.hub().add_torrent(spec.clone(), now).expect("add");
        self.specs.push(spec);
    }

    pub fn stats(&mut self, n: u32, up: u64, down: u64, left: u64, size: u64) {
        let now = self.now;
        self.hub().set_stats(
            &ih(n),
            Stats {
                uploaded: up,
                downloaded: down,
                corrupt: 0,
                left,
                size,
            },
            now,
        );
    }

    pub fn ev(&mut self, n: u32, ev: TorrentEvent) {
        let now = self.now;
        match ev {
            TorrentEvent::Start | TorrentEvent::Resume => {
                self.running.insert(ih(n), true);
            }
            TorrentEvent::Stop | TorrentEvent::Pause => {
                self.running.insert(ih(n), false);
            }
            TorrentEvent::Complete => {}
        }
        self.hub().torrent_event(&ih(n), ev, now);
    }

    /// The process quits: `stopped` once per tracker with an open session, no retry.
    pub fn quit(&mut self, n: u32) {
        let now = self.now;
        self.running.insert(ih(n), false);
        self.hub().quit(&ih(n), now);
    }

    pub fn manual(&mut self, n: u32) {
        let now = self.now;
        self.hub().manual_reannounce(&ih(n), now);
    }

    /// "Announce anyway".
    pub fn force(&mut self, n: u32) {
        let now = self.now;
        self.hub().manual_reannounce_forced(&ih(n), now);
    }

    pub fn network(&mut self) {
        let now = self.now;
        self.hub().network_changed(now);
    }

    pub fn wake(&mut self) {
        let now = self.now;
        self.hub().wake(now);
    }

    /// kill -9 and relaunch: in-flight work vanishes, persisted timing survives,
    /// running torrents are started again.
    pub fn crash_and_relaunch(&mut self) {
        self.crash();
        let running: Vec<InfoHash20> = self
            .running
            .iter()
            .filter(|(_, r)| **r)
            .map(|(k, _)| *k)
            .collect();
        for k in running {
            let now = self.now;
            self.hub().torrent_event(&k, TorrentEvent::Start, now);
        }
    }

    /// kill -9 and relaunch without starting anything yet: the caller pushes
    /// stats and `Start`s torrents like its engine would.
    pub fn crash(&mut self) {
        let hub = self.hub.take().expect("hub");
        let store = hub.into_store();
        self.pending.clear();
        self.hub_seed += 1;
        let mut hub = TrackerHub::new(self.cfg.clone(), store, self.hub_seed);
        let now = self.now;
        for sp in &self.specs {
            hub.add_torrent(sp.clone(), now).expect("re-add");
        }
        self.hub = Some(hub);
    }

    pub fn sent_for(&self, n: u32) -> Vec<&Sent> {
        self.sent.iter().filter(|x| x.ih == ih(n)).collect()
    }

    pub fn events_for(&self, n: u32) -> Vec<Option<Event>> {
        self.sent_for(n).iter().map(|x| x.event()).collect()
    }

    pub fn count_event(&self, n: u32, e: Option<Event>) -> usize {
        self.sent_for(n).iter().filter(|x| x.event() == e).count()
    }

    pub fn host_count(&self, host: &str) -> usize {
        self.sent.iter().filter(|x| x.host == host).count()
    }

    fn deliver_due(&mut self) -> bool {
        let mut any = false;
        while let Some((&k, _)) = self.pending.iter().next() {
            if k.0 > self.now {
                break;
            }
            let p = self.pending.remove(&k).expect("present");
            let now = self.now;
            let _ = p.sent_n;
            self.hub().complete(p.ticket, p.result, now);
            any = true;
        }
        any
    }

    pub fn pump(&mut self) {
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 200_000, "pump livelock at {:?}", self.now);
            let mut progressed = self.deliver_due();
            let now = self.now;
            let reqs = self.hub().tick(now);
            let reps = self.hub().take_reports();
            for r in &reps {
                if let Report::InvariantViolation(why) = &r.report
                    && self.panic_on_trip {
                        panic!("machine tripped its self-audit: {why}");
                    }
            }
            self.reports.extend(reps);
            for ev in self.hub().take_gate_events() {
                if let GateEvent::HostTripped { host, reason, .. } = &ev
                    && self.panic_on_trip {
                        panic!("gate tripped host {host}: {reason}");
                    }
                self.gate_events.push(ev);
            }
            for r in reqs {
                progressed = true;
                self.release(r);
            }
            if !progressed {
                break;
            }
        }
    }

    fn release(&mut self, r: GatedRequest) {
        let (i, slot) = r.ticket().key();
        let p = r.announce_params().expect("announce").clone();
        let sent = Sent {
            n: self.sent.len(),
            t: self.now,
            ih: i,
            slot,
            url: r.url().raw().to_owned(),
            host: r.url().host().to_owned(),
            p,
        };
        let answers = (self.responder)(&sent);
        let n = sent.n;
        self.sent.push(sent);
        for (delay, result) in answers {
            self.seq += 1;
            self.pending.insert(
                (self.now + delay, self.seq),
                Pending {
                    ticket: r.ticket(),
                    result,
                    sent_n: n,
                },
            );
        }
    }

    pub fn next_due(&mut self) -> Option<Millis> {
        let now = self.now;
        let a = self.pending.keys().next().map(|k| k.0);
        let b = self.hub().next_wake(now);
        match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        }
    }

    pub fn run_until(&mut self, t: Millis) {
        let mut stuck = 0;
        loop {
            self.pump();
            match self.next_due() {
                Some(n) if n <= t => {
                    if n <= self.now {
                        stuck += 1;
                        assert!(
                            stuck < 50,
                            "scheduler wants {n:?} but nothing happens at {:?}",
                            self.now
                        );
                        // make progress in 1 ms steps if the scheduler asks for "now" repeatedly
                        self.now = Millis(self.now.0 + 1);
                    } else {
                        stuck = 0;
                        self.now = n;
                    }
                }
                _ => break,
            }
        }
        if t > self.now {
            self.now = t;
        }
        self.pump();
    }

    pub fn run_for(&mut self, dur: Duration) {
        let t = self.now + dur;
        self.run_until(t);
    }

    /// Simulate a machine that slept: time jumps with nothing running, then
    /// everything that became due is processed on wake-up, like an app would.
    pub fn sleep(&mut self, dur: Duration) {
        self.now = self.now + dur;
        self.wake();
        self.pump();
    }
}

/// Maximum number of releases to one host in any `window` (sliding), computed
/// from the sent log.
pub fn max_in_window(sent: &[Sent], host: &str, window: Duration) -> usize {
    let times: Vec<u64> = sent
        .iter()
        .filter(|x| x.host == host)
        .map(|x| x.t.0)
        .collect();
    let w = window.as_millis() as u64;
    let mut best = 0;
    let mut lo = 0;
    for hi in 0..times.len() {
        while times[hi] - times[lo] > w {
            lo += 1;
        }
        best = best.max(hi - lo + 1);
    }
    best
}

/// Tracker-side grammar checker (independent of the repo's oracle).
///
/// It tracks, per (info-hash, tracker URL), what an operator can see and
/// returns a list of violations of SPEC 7B/7C wire rules.
#[derive(Default)]
pub struct View {
    keys: HashMap<(InfoHash20, String), KeyView>,
    completed_ever: HashMap<(InfoHash20, String), u32>,
    pub violations: Vec<String>,
}

#[derive(Default, Clone)]
struct KeyView {
    open: bool,
    stop_failed: bool,
    started_answered_ok: bool,
    last_up: u64,
    last_down: u64,
    first_started_left: Option<u64>,
}

impl View {
    pub fn new() -> View {
        View::default()
    }

    /// A process restart: tracker-side nothing changes, but "open" sessions
    /// may legitimately be followed by a new `started` (no `stopped` was sent).
    pub fn crash(&mut self) {
        for k in self.keys.values_mut() {
            k.started_answered_ok = false;
        }
    }

    /// The tracker answered the request just checked with `failure reason`.
    /// A halted machine closes its session without `stopped`, so a later
    /// `started` is not a "repeated started" any more.
    pub fn rejected(&mut self, s: &Sent) {
        if let Some(v) = self.keys.get_mut(&(s.ih, s.url.clone())) {
            v.started_answered_ok = false;
        }
    }

    pub fn check(&mut self, s: &Sent, size: u64, answered_ok: bool) {
        let key = (s.ih, s.url.clone());
        let v = self.keys.entry(key.clone()).or_default();
        let p = &s.p;
        let mut bad = |m: String| {
            self.violations.push(format!(
                "#{} t={:?} {:?} ev={:?}: {m}",
                s.n, s.t, s.host, p.event
            ))
        };
        if size > 0 && p.left > size {
            bad(format!("left {} > size {}", p.left, size));
        }
        match p.event {
            Some(Event::Started) => {
                if v.open && v.started_answered_ok {
                    bad("started again without stopped after a successful started".into());
                }
                v.open = true;
                v.stop_failed = false;
                v.started_answered_ok = answered_ok;
                v.last_up = p.uploaded;
                v.last_down = p.downloaded;
                if v.first_started_left.is_none() {
                    v.first_started_left = Some(p.left);
                }
            }
            Some(Event::Stopped) => {
                if !v.open && !v.stop_failed {
                    bad("stopped without an open session".into());
                }
                if p.numwant != 0 {
                    bad("stopped with numwant != 0".into());
                }
                v.open = false;
                v.stop_failed = !answered_ok;
                v.started_answered_ok = false;
            }
            Some(Event::Completed) => {
                if !v.open {
                    bad("completed outside a session".into());
                }
                if p.left != 0 {
                    bad("completed with left > 0".into());
                }
                let c = self.completed_ever.entry(key.clone()).or_default();
                *c += 1;
                if *c > 1 {
                    bad("second completed".into());
                }
            }
            None => {
                if !v.open {
                    bad("regular announce outside a session".into());
                }
            }
        }
        if p.event != Some(Event::Started) {
            if p.uploaded < v.last_up || p.downloaded < v.last_down {
                bad(format!(
                    "counters decreased: up {}->{} down {}->{}",
                    v.last_up, p.uploaded, v.last_down, p.downloaded
                ));
            }
            v.last_up = v.last_up.max(p.uploaded);
            v.last_down = v.last_down.max(p.downloaded);
        }
    }

    pub fn completed_count(&self, ih: InfoHash20, url: &str) -> u32 {
        self.completed_ever
            .get(&(ih, url.to_owned()))
            .copied()
            .unwrap_or(0)
    }
}
