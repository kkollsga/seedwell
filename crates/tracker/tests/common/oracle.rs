//! Independent invariant checker (SPEC 7C.3). It is written separately from the
//! crate's own `Auditor`/machine audit: it sees only what a tracker would see
//! on the wire (requests and the answers) plus the user-level inputs.
//! Any violation panics with context so proptest can shrink it.
#![allow(dead_code, clippy::too_many_arguments)]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::time::Duration;

use trs_tracker::backoff::{nominal_delay, slow_nominal_delay};
use trs_tracker::response::{AnnounceError, FailureClass};
use trs_tracker::{AnnounceParams, Event, InfoHash20, Millis, Timing};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sess {
    Closed,
    Open { acked: bool },
    Stopping,
}

#[derive(Clone, Debug)]
struct KeyState {
    sess: Sess,
    completed: u32,
    last_up: u64,
    last_down: u64,
    ident: Option<(u32, [u8; 20])>,
    tid: Option<Vec<u8>>,
    last_success: Option<(Millis, Timing)>,
    manual_licence: bool,
    net_earliest: Option<Millis>,
    fails: u32,
    /// The failure state before each of the latest failures (newest last), so that an outage can
    /// take back exactly the failures inside its window. An entry is valid only while
    /// `snap.fails + 1` is the current count.
    hist: Vec<Snap>,
    last_fail: Option<Millis>,
    /// When the hub could first see the last failure. Equal to `last_fail` unless the machine slept
    /// through the request's deadline: the timeout is then noticed (and counted) at the wake.
    last_fail_seen: Option<Millis>,
    wait_after_fail: Duration,
    last_fail_network: bool,
    /// The last failure was a non-permanent `failure reason` (slow schedule).
    last_fail_slow: bool,
    last_attempt: Option<Millis>,
    halted: bool,
    /// A `failure reason` ended the tracker's willingness: the machine may
    /// close its session without `stopped` (nothing to stop).
    rejected_in_session: bool,
    in_flight: Option<(u64, Millis, Option<Event>)>, // (seq, deadline, event)
    /// Failure applied for `seq`, with the state before it, so that a success
    /// for the same logical request can still win (SPEC 7C.4a rule 3).
    resolved: Option<(u64, Undo)>,
}

impl KeyState {
    fn new() -> Self {
        KeyState {
            sess: Sess::Closed,
            completed: 0,
            last_up: 0,
            last_down: 0,
            ident: None,
            tid: None,
            last_success: None,
            manual_licence: false,
            net_earliest: None,
            fails: 0,
            hist: Vec::new(),
            last_fail: None,
            last_fail_seen: None,
            wait_after_fail: Duration::ZERO,
            last_fail_network: false,
            last_fail_slow: false,
            last_attempt: None,
            halted: false,
            rejected_in_session: false,
            in_flight: None,
            resolved: None,
        }
    }
}

/// Failure state at one instant (see `KeyState::hist`).
#[derive(Clone, Debug)]
struct Snap {
    fails: u32,
    last_fail: Option<Millis>,
    last_fail_seen: Option<Millis>,
    wait_after_fail: Duration,
    last_fail_network: bool,
    last_fail_slow: bool,
}

impl Snap {
    fn of(ks: &KeyState) -> Snap {
        Snap {
            fails: ks.fails,
            last_fail: ks.last_fail,
            last_fail_seen: ks.last_fail_seen,
            wait_after_fail: ks.wait_after_fail,
            last_fail_network: ks.last_fail_network,
            last_fail_slow: ks.last_fail_slow,
        }
    }
    fn restore(self, ks: &mut KeyState) {
        ks.fails = self.fails;
        ks.last_fail = self.last_fail;
        ks.last_fail_seen = self.last_fail_seen;
        ks.wait_after_fail = self.wait_after_fail;
        ks.last_fail_network = self.last_fail_network;
        ks.last_fail_slow = self.last_fail_slow;
        ks.net_earliest = None;
    }
}

#[derive(Clone, Debug)]
struct Undo {
    fails: u32,
    last_fail: Option<Millis>,
    last_fail_seen: Option<Millis>,
    wait_after_fail: Duration,
    last_fail_network: bool,
    last_fail_slow: bool,
    halted: bool,
    rejected_in_session: bool,
    net_earliest: Option<Millis>,
    completed: u32,
}

impl Undo {
    fn of(ks: &KeyState) -> Undo {
        Undo {
            fails: ks.fails,
            last_fail: ks.last_fail,
            last_fail_seen: ks.last_fail_seen,
            wait_after_fail: ks.wait_after_fail,
            last_fail_network: ks.last_fail_network,
            last_fail_slow: ks.last_fail_slow,
            halted: ks.halted,
            rejected_in_session: ks.rejected_in_session,
            net_earliest: ks.net_earliest,
            completed: ks.completed,
        }
    }
    fn restore(&self, ks: &mut KeyState) {
        ks.fails = self.fails;
        ks.last_fail = self.last_fail;
        ks.last_fail_seen = self.last_fail_seen;
        ks.wait_after_fail = self.wait_after_fail;
        ks.last_fail_network = self.last_fail_network;
        ks.last_fail_slow = self.last_fail_slow;
        ks.halted = self.halted;
        ks.rejected_in_session = self.rejected_in_session;
        ks.net_earliest = self.net_earliest;
        ks.completed = self.completed;
    }
}

/// What the user did (for licences).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum User {
    Manual,
    Network,
    Wake,
    StartOrResume,
}

#[derive(Default)]
struct HostLog {
    sends: VecDeque<Millis>,
    in_flight: Vec<(u64, Millis)>, // (seq, deadline)
    total: u64,
}

pub struct Oracle {
    keys: HashMap<(InfoHash20, u16), KeyState>,
    hosts: HashMap<String, HostLog>,
    allowed_urls: HashMap<InfoHash20, HashSet<String>>,
    added_complete: HashSet<InfoHash20>,
    sizes: HashMap<InfoHash20, u64>,
    expiries: BTreeMap<(Millis, u64), (InfoHash20, u16)>,
    seq: u64,
    pub http_deadline: Duration,
    pub udp_deadline: Duration,
    pub burst: u32,
    pub token_interval: Duration,
    pub max_per_host: usize,
    pub sent_total: u64,
    pub max_sends_in_1s_window: usize,
    /// Keep a rolling textual trace for failure diagnostics (slow; tests only).
    pub trace_on: bool,
    /// An outage is declared (SPEC 7C.4c).
    offline: bool,
    /// The latest wake from sleep (a timeout that fell due during the sleep is noticed then).
    wake_at: Option<Millis>,
}

struct Ctx {
    now: Millis,
    ih3: u8,
    slot: u16,
    event: Option<Event>,
}

impl std::fmt::Display for Ctx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "t={}s torrent={:02x} slot={} event={:?}",
            self.now.0 / 1000,
            self.ih3,
            self.slot,
            self.event
        )
    }
}

thread_local! {
    static TRACE: std::cell::RefCell<VecDeque<String>> = const { std::cell::RefCell::new(VecDeque::new()) };
}

/// When the hub first sees a timeout whose deadline was `deadline`: at the deadline, unless the
/// machine slept through it, in which case at the wake (nothing ticks while it sleeps). SPEC 7C.4c
/// rolls back the failures that revealed an outage by the time they were seen.
fn seen_at(deadline: Millis, wake_at: Option<Millis>, now: Millis) -> Millis {
    match wake_at {
        Some(w) if w > deadline && w <= now => w,
        _ => deadline,
    }
}

fn trace(line: String) {
    TRACE.with(|t| {
        let mut t = t.borrow_mut();
        if t.len() > 60 {
            t.pop_front();
        }
        t.push_back(line);
    });
}

fn fail(msg: String) -> ! {
    let tr = TRACE.with(|t| t.borrow().iter().cloned().collect::<Vec<_>>().join("\n  "));
    panic!("INVARIANT VIOLATION: {msg}\nrecent oracle trace:\n  {tr}");
}

impl Oracle {
    pub fn new() -> Self {
        TRACE.with(|t| t.borrow_mut().clear());
        Oracle {
            keys: HashMap::new(),
            hosts: HashMap::new(),
            allowed_urls: HashMap::new(),
            added_complete: HashSet::new(),
            sizes: HashMap::new(),
            expiries: BTreeMap::new(),
            seq: 0,
            http_deadline: Duration::from_secs(60),
            udp_deadline: Duration::from_secs(600),
            burst: 3,
            token_interval: Duration::from_secs(1),
            max_per_host: 2,
            sent_total: 0,
            max_sends_in_1s_window: 0,
            trace_on: true,
            offline: false,
            wake_at: None,
        }
    }

    /// The machine woke at `now`.
    pub fn woke(&mut self, now: Millis) {
        self.wake_at = Some(now);
    }

    pub fn register(
        &mut self,
        ih: InfoHash20,
        urls: HashSet<String>,
        size: u64,
        added_complete: bool,
    ) {
        self.allowed_urls.insert(ih, urls);
        self.sizes.insert(ih, size);
        if added_complete {
            self.added_complete.insert(ih);
        }
    }

    pub fn host_total(&self, host: &str) -> u64 {
        self.hosts.get(host).map_or(0, |h| h.total)
    }

    /// Process deadline expiries up to `now`. The harness calls this right before every hub
    /// tick, which is when the hub itself notices a timeout (so a timeout is judged against the
    /// connectivity state the hub is in at that moment, and an answer delivered before the tick
    /// still beats it); it is also called when a request is sent.
    pub fn advance(&mut self, now: Millis, inclusive: bool) {
        let upper = if inclusive {
            (now, u64::MAX)
        } else {
            (Millis(now.0.saturating_sub(1)), u64::MAX)
        };
        let due: Vec<_> = self
            .expiries
            .range(..=upper)
            .map(|(k, v)| (*k, *v))
            .collect();
        let offline = self.offline;
        let wake_at = self.wake_at;
        for ((deadline, seq), key) in due {
            self.expiries.remove(&(deadline, seq));
            if let Some(ks) = self.keys.get_mut(&key)
                && let Some((s, _, ev)) = ks.in_flight
                    && s == seq {
                        ks.in_flight = None;
                        // The hub declares a timeout at (or after) the deadline.
                        if ev != Some(Event::Stopped) && !offline {
                            Self::record_failure(
                                ks,
                                deadline,
                                &AnnounceError::Transport(trs_tracker::TransportError::Timeout),
                            );
                            ks.last_fail_seen = Some(seen_at(deadline, wake_at, now));
                        }
                    }
        }
    }

    fn push_snap(ks: &mut KeyState) {
        ks.hist.push(Snap::of(ks));
        if ks.hist.len() > 4 {
            ks.hist.remove(0);
        }
    }

    fn record_failure(ks: &mut KeyState, at: Millis, e: &AnnounceError) {
        match e.class() {
            FailureClass::Halt => {
                ks.halted = true;
                ks.rejected_in_session = true;
                ks.last_fail_slow = false;
            }
            FailureClass::Slow => {
                // non-permanent `failure reason`: slow schedule, never halts
                Self::push_snap(ks);
                ks.fails += 1;
                let mut wait = slow_nominal_delay(ks.fails);
                if let AnnounceError::Rejected {
                    retry_in: Some(trs_tracker::response::RetryIn::Minutes(m)),
                    ..
                } = e
                {
                    wait = wait.max(Duration::from_secs(u64::from(*m) * 60));
                }
                ks.wait_after_fail = wait;
                ks.last_fail = Some(at);
                ks.last_fail_seen = Some(at);
                ks.last_fail_network = false;
                ks.last_fail_slow = true;
                ks.net_earliest = None;
            }
            FailureClass::Backoff { hint } => {
                Self::push_snap(ks);
                ks.fails += 1;
                let mut wait = nominal_delay(ks.fails);
                if let Some(h) = hint {
                    // a downloading torrent waits exactly the hint (1 min floor)
                    wait = h.max(Duration::from_secs(60));
                }
                ks.wait_after_fail = wait;
                ks.last_fail = Some(at);
                ks.last_fail_seen = Some(at);
                ks.last_fail_network = e.is_network();
                ks.last_fail_slow = false;
                ks.net_earliest = None;
            }
        }
    }

    fn apply_success(ks: &mut KeyState, now: Millis, event: Option<Event>, t: Timing) {
        ks.fails = 0;
        ks.last_fail = None;
        ks.net_earliest = None;
        ks.halted = false;
        ks.last_fail_slow = false;
        ks.manual_licence = false;
        match event {
            Some(Event::Stopped) => {
                if ks.sess == Sess::Stopping {
                    ks.sess = Sess::Closed;
                }
            }
            Some(Event::Started) => {
                if matches!(ks.sess, Sess::Open { .. }) {
                    ks.sess = Sess::Open { acked: true };
                }
                ks.last_success = Some((now, t));
            }
            _ => ks.last_success = Some((now, t)),
        }
    }

    pub fn user(&mut self, ih: InfoHash20, what: User) {
        if self.trace_on {
            trace(format!("            USER {:02x} {what:?}", ih[3]));
        }
        for ((i, _), ks) in self.keys.iter_mut() {
            if *i != ih {
                continue;
            }
            match what {
                User::Manual => {
                    ks.manual_licence = true;
                    if ks.halted || ks.last_fail_slow {
                        // a user retry after a halt (or during the slow
                        // schedule) starts the backoff afresh
                        ks.fails = 0;
                        ks.last_fail = None;
                        ks.last_fail_slow = false;
                    }
                    ks.halted = false;
                    // a user retry after a halt waits at least the floor; the
                    // machine tracks that itself, the wire rule below covers it
                }
                User::Network | User::Wake => {
                    // Network change and wake never license an early regular
                    // announce: they only reschedule failed ones.
                    if let (true, true, Some(lf)) =
                        (ks.fails > 0, ks.last_fail_network, ks.last_fail)
                    {
                        // A network change may shorten a network-class backoff,
                        // but never below 30 s after the previous attempt.
                        let floor = ks.last_attempt.map_or(lf, |t| t + Duration::from_secs(30));
                        ks.net_earliest = Some(floor.max(lf + Duration::from_secs(1)));
                    }
                }
                User::StartOrResume => {
                    ks.halted = false;
                    if ks.last_fail_slow {
                        ks.fails = 0;
                        ks.last_fail = None;
                        ks.last_fail_slow = false;
                    }
                }
            }
        }
    }

    /// A request is about to hit the wire.
    pub fn sent(
        &mut self,
        now: Millis,
        ih: InfoHash20,
        slot: u16,
        url_raw: &str,
        host: &str,
        udp: bool,
        p: &AnnounceParams,
    ) -> u64 {
        self.advance(now, true);
        self.sent_total += 1;
        self.seq += 1;
        let seq = self.seq;
        if self.trace_on {
            trace(format!(
                "{:>9.1}s SENT {:02x}/{slot} {:?} up={} down={} left={} tid={:?}",
                now.0 as f64 / 1000.0,
                ih[3],
                p.event,
                p.uploaded,
                p.downloaded,
                p.left,
                p.tracker_id
            ));
        }
        let ctx = Ctx {
            now,
            ih3: ih[3],
            slot,
            event: p.event,
        };
        // inv 7
        match self.allowed_urls.get(&ih) {
            Some(set) if set.contains(url_raw) => {}
            _ => fail(format!("inv7: request to an unregistered URL ({ctx})")),
        }
        let deadline = now
            + if udp {
                self.udp_deadline
            } else {
                self.http_deadline
            };
        // inv 5: host budgets
        if !self.hosts.contains_key(host) {
            self.hosts.insert(host.to_owned(), HostLog::default());
        }
        let h = self.hosts.get_mut(host).expect("inserted");
        h.in_flight.retain(|(_, d)| *d > now);
        if h.in_flight.len() >= self.max_per_host {
            fail(format!(
                "inv5: more than {} concurrent requests to {host} ({ctx})",
                self.max_per_host
            ));
        }
        h.in_flight.push((seq, deadline));
        h.total += 1;
        h.sends.push_back(now);
        while h.sends.len() > 4096 {
            h.sends.pop_front();
        }
        // windows: sends in any closed window of W seconds <= burst + W
        for w in [1u64, 2, 5, 30, 300] {
            let lo = now.0.saturating_sub(w * 1000);
            let cnt = h.sends.len() - h.sends.partition_point(|t| t.0 < lo);
            let allowed = self.burst as u64 + w * 1000 / self.token_interval.as_millis() as u64;
            if cnt as u64 > allowed {
                let recent: Vec<u64> = h.sends.iter().rev().take(8).map(|t| t.0).collect();
                fail(format!(
                    "inv5: {cnt} requests to {host} within {w}s (> {allowed}) ({ctx}) recent send times (ms): {recent:?}"
                ));
            }
            if w == 1 {
                self.max_sends_in_1s_window = self.max_sends_in_1s_window.max(cnt);
            }
        }
        let size = self.sizes.get(&ih).copied().unwrap_or(u64::MAX);
        let added_complete = self.added_complete.contains(&ih);
        let ks = self.keys.entry((ih, slot)).or_insert_with(KeyState::new);
        // inv 5: one in flight per (torrent, tracker)
        if let Some((_, d, _)) = ks.in_flight
            && d > now {
                fail(format!(
                    "inv5: second request in flight for one (torrent, tracker) ({ctx})"
                ));
            }
        ks.in_flight = Some((seq, deadline, p.event));
        ks.resolved = None;
        self.expiries.insert((deadline, seq), (ih, slot));
        // inv 6
        match ks.ident {
            None => ks.ident = Some((p.key, p.peer_id)),
            Some((k, id)) => {
                if (k, id) != (p.key, p.peer_id) {
                    fail(format!("inv6: key/peer_id changed ({ctx})"));
                }
            }
        }
        // halted keys do not talk (a single best-effort `stopped` after an
        // accepted `started` excepted)
        if ks.halted && p.event != Some(Event::Stopped) {
            fail(format!("failure reason did not halt announcing ({ctx})"));
        }
        // backoff rule (every send except stopped)
        if p.event != Some(Event::Stopped)
            && let Some(lf) = ks.last_fail
                && ks.fails > 0 {
                    let mut earliest = lf + ks.wait_after_fail;
                    if let Some(ne) = ks.net_earliest {
                        earliest = earliest.min(ne);
                    }
                    if now < earliest {
                        fail(format!(
                            "backoff: retry at {}s but earliest is {}s after {} failures ({ctx})",
                            now.0 / 1000,
                            earliest.0 / 1000,
                            ks.fails
                        ));
                    }
                }
        // inv 2 / 3 / 1
        match p.event {
            Some(Event::Started) => {
                if matches!(ks.sess, Sess::Open { acked: true }) && !ks.rejected_in_session {
                    fail(format!("inv2: second started in one session ({ctx})"));
                }
                // After a rejection the machine may have closed its session
                // silently and begun a new one (counters restart).
                // A tracker that never acknowledged `started` and was retired by the set (the
                // failover of its tier took over) is closed without a request; its next
                // `started` is a new session with fresh counters.
                let retired_unacked = ks.sess == (Sess::Open { acked: false })
                    && (p.uploaded < ks.last_up || p.downloaded < ks.last_down);
                if ks.sess != (Sess::Open { acked: false })
                    || ks.rejected_in_session
                    || retired_unacked
                {
                    ks.last_up = 0;
                    ks.last_down = 0;
                    ks.tid = None;
                }
                ks.rejected_in_session = false;
                ks.sess = Sess::Open { acked: false };
            }
            Some(Event::Completed) => {
                if ks.sess != (Sess::Open { acked: true }) {
                    fail(format!(
                        "inv2: completed outside an acknowledged session ({ctx})"
                    ));
                }
                if ks.completed > 0 {
                    fail(format!("inv3: second completed ({ctx})"));
                }
                if added_complete {
                    fail(format!(
                        "inv3: completed for a torrent added complete ({ctx})"
                    ));
                }
                if p.left != 0 {
                    fail(format!("completed with left>0 ({ctx})"));
                }
                ks.completed += 1;
            }
            Some(Event::Stopped) => {
                if !matches!(ks.sess, Sess::Open { .. } | Sess::Stopping) {
                    fail(format!("inv2: stopped without started ({ctx})"));
                }
                ks.sess = Sess::Stopping;
            }
            None => {
                if ks.sess != (Sess::Open { acked: true }) {
                    fail(format!(
                        "inv2: announce outside an acknowledged session: {:?} ({ctx})",
                        ks.sess
                    ));
                }
                let (s, t) = ks.last_success.expect("acked implies success");
                let regular = s + t.interval.max(t.min_interval.unwrap_or_default());
                let min_eff = t
                    .min_interval
                    .unwrap_or(Duration::from_secs(300))
                    .min(t.interval);
                let manual = s + min_eff;
                if now >= regular {
                    // fine
                } else if ks.manual_licence && now >= manual {
                    ks.manual_licence = false;
                } else {
                    fail(format!(
                        "inv1: non-event announce at {}s, last success {}s, interval {}s min {:?} ({ctx})",
                        now.0 / 1000,
                        s.0 / 1000,
                        t.interval.as_secs(),
                        t.min_interval.map(|d| d.as_secs())
                    ));
                }
                if now >= regular {
                    ks.manual_licence = false;
                }
            }
        }
        // inv 4
        if p.uploaded < ks.last_up || p.downloaded < ks.last_down {
            fail(format!("inv4: counters decreased ({ctx})"));
        }
        if p.left > size {
            fail(format!("inv4: left {} > size {size} ({ctx})", p.left));
        }
        ks.last_up = p.uploaded;
        ks.last_down = p.downloaded;
        // inv 6: tracker id stable within the session
        match (&ks.tid, &p.tracker_id) {
            (Some(a), Some(b)) if a.as_slice() != &b[..] => {
                fail(format!("inv6: tracker id changed ({ctx})"))
            }
            (Some(_), None) => fail(format!("inv6: tracker id dropped ({ctx})")),
            (None, Some(b)) => ks.tid = Some(b.to_vec()),
            _ => {}
        }
        ks.last_attempt = Some(now);
        seq
    }

    /// The answer (or failure) reached the machine.
    /// The hub declared an outage (SPEC 7C.4c): the network failures of the last two minutes
    /// revealed it and are taken back; until it ends, network failures are not the tracker's.
    pub fn outage_started(&mut self, now: Millis) {
        self.offline = true;
        if self.trace_on {
            trace(format!("{:>9.1}s OUTAGE declared", now.0 as f64 / 1000.0));
        }
        let since = Millis(now.0.saturating_sub(120_000));
        for ks in self.keys.values_mut() {
            // Newest first, contiguous, at most the latest two (as the machine does): the network
            // failures inside the window go; an older real failure stays counted.
            for _ in 0..2 {
                let seen = ks.last_fail_seen.or(ks.last_fail);
                if !(ks.fails > 0 && ks.last_fail_network && seen.is_some_and(|t| t >= since)) {
                    break;
                }
                match ks.hist.pop() {
                    Some(snap) if snap.fails + 1 == ks.fails => snap.restore(ks),
                    _ => {
                        ks.hist.clear();
                        ks.fails = 0;
                        ks.last_fail = None;
                        ks.last_fail_seen = None;
                        ks.net_earliest = None;
                        ks.last_fail_network = false;
                        break;
                    }
                }
            }
        }
    }

    /// The outage is over.
    pub fn outage_ended(&mut self, now: Millis) {
        self.offline = false;
        if self.trace_on {
            trace(format!("{:>9.1}s OUTAGE over", now.0 as f64 / 1000.0));
        }
    }

    pub fn completed(
        &mut self,
        now: Millis,
        due_at: Millis,
        ih: InfoHash20,
        slot: u16,
        seq: u64,
        event: Option<Event>,
        result: &Result<Timing, AnnounceError>,
    ) {
        let offline = self.offline;
        let Some(ks) = self.keys.get_mut(&(ih, slot)) else {
            return;
        };
        // A request that already expired (its failure was counted at the
        // deadline) or was superseded is stale: it cannot change anything.
        let mut current = ks.in_flight.is_some_and(|(s, _, _)| s == seq);
        if let Some((s, deadline, ev)) = ks.in_flight
            && s == seq && due_at > deadline {
                // The answer was ready only after the gate's deadline: the hub
                // has timed it out already (it ticks at every deadline).
                ks.in_flight = None;
                if ev != Some(Event::Stopped) && !offline {
                    Self::record_failure(
                        ks,
                        deadline,
                        &AnnounceError::Transport(trs_tracker::TransportError::Timeout),
                    );
                    ks.last_fail_seen = Some(seen_at(deadline, self.wake_at, now));
                }
                current = false;
            }
        if self.trace_on {
            trace(format!(
                "{:>9.1}s DONE {:02x}/{slot} {:?} current={current} {:?}",
                now.0 as f64 / 1000.0,
                ih[3],
                event,
                result
                    .as_ref()
                    .map(|t| (t.interval.as_secs(), t.min_interval.map(|d| d.as_secs())))
                    .map_err(|e| format!("{e}"))
            ));
        }
        if !current {
            // A further answer for a request that already resolved: a success
            // still wins while nothing newer has been sent (never for `stopped`).
            if let (Ok(t), Some((rseq, undo))) = (result, ks.resolved.take())
                && rseq == seq && event != Some(Event::Stopped) {
                    undo.restore(ks);
                    Self::apply_success(ks, now, event, *t);
                }
            return;
        }
        ks.in_flight = None;
        let already_resolved = false;
        // remember how to undo a failure
        if result.is_err() && event != Some(Event::Stopped) {
            ks.resolved = Some((seq, Undo::of(ks)));
        }
        if let (Err(e), Some(Event::Completed)) = (result, event) {
            // Provably never reached the tracker: it did not count it.
            if e.never_reached_tracker() {
                ks.completed = ks.completed.saturating_sub(1);
            }
        }
        match result {
            Ok(t) => Self::apply_success(ks, now, event, *t),
            Err(e) => {
                // A failed `stopped` is handled by its own bounded retry
                // counter and never halts or backs off the next session.
                if !already_resolved && event != Some(Event::Stopped) && !(offline && e.is_network()) {
                    Self::record_failure(ks, now, e);
                }
            }
        }
    }

    /// Free a host slot when a request completes.
    pub fn host_release(&mut self, host: &str, seq: u64) {
        if let Some(h) = self.hosts.get_mut(host) {
            h.in_flight.retain(|(s, _)| *s != seq);
        }
    }

    /// The process died: nothing in flight, sessions end uncleanly, failure
    /// state is lost, but `completed` history survives (persisted).
    pub fn crash(&mut self, now: Millis, new_identity: bool) {
        self.offline = false;
        if self.trace_on {
            trace(format!("{:>9.1}s CRASH", now.0 as f64 / 1000.0));
        }
        self.expiries.clear();
        for h in self.hosts.values_mut() {
            h.in_flight.clear();
        }
        for ks in self.keys.values_mut() {
            ks.sess = if matches!(ks.sess, Sess::Closed) {
                Sess::Closed
            } else {
                Sess::Stopping
            };
            ks.in_flight = None;
            ks.fails = 0;
            ks.last_fail = None;
            ks.halted = false;
            ks.manual_licence = false;
            ks.net_earliest = None;
            if new_identity {
                ks.ident = None;
            }
        }
    }
}
