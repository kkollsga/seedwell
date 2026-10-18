//! Bounded exhaustive model check (stateright) of `AnnounceMachine`.
//!
//! Every interleaving, up to the depth bound, of: start / stop / pause-resume
//! style events, completion, crash (restart from the persisted record), timer
//! ticks of various lengths, request dispatch, responses of every class
//! (success, timeout, 5xx with Retry-After, `failure reason`, BEP 31 retry-in),
//! *duplicate* answers for one logical request (the 4.0.6 scenario), manual
//! re-announce and network change. After every transition the independent
//! witness below checks SPEC 7C.3 / 7C.4 / 7C.4a.
//!
//! `MODEL_DEPTH` overrides the depth (default tuned for a few seconds in
//! release, under a minute in debug).

use std::hash::{Hash, Hasher};
use std::time::Duration;

use stateright::{Checker, Model, Property};
use trs_tracker::backoff::{nominal_delay, slow_nominal_delay};
use trs_tracker::response::{
    AnnounceError, AnnounceResponse, FailureClass, RetryIn, TransportError,
};
use trs_tracker::*;

const IH: InfoHash20 = [5; 20];

fn cfg(added_complete: bool) -> MachineConfig {
    let mut c = MachineConfig::new(IH, [6; 20], 0xBEEF, 6881);
    c.added_complete = added_complete;
    c.start_jitter = Duration::ZERO;
    c.wake_jitter = Duration::from_secs(10);
    c
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Res {
    Ok1800,
    Ok300Min60,
    Timeout,
    Http503Retry7200,
    /// Permanent-looking `failure reason`: halts.
    Rejected,
    /// Maintenance-style `failure reason`: slow retry schedule.
    Maintenance,
    RetryIn5,
    /// DNS failure: provably never reached the tracker.
    Dns,
}

impl Res {
    fn build(self) -> Result<AnnounceResponse, AnnounceError> {
        let ok = |i: i64, m: Option<i64>| {
            let t = clamp_timing(Some(i), m);
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
        };
        match self {
            Res::Ok1800 => ok(1800, None),
            Res::Ok300Min60 => ok(300, Some(60)),
            Res::Timeout => Err(AnnounceError::Transport(TransportError::Timeout)),
            Res::Http503Retry7200 => Err(AnnounceError::Http {
                status: 503,
                retry_after: Some(Duration::from_secs(7200)),
            }),
            Res::Rejected => Err(AnnounceError::Rejected {
                reason: "Torrent not registered".into(),
                retry_in: None,
            }),
            Res::Maintenance => Err(AnnounceError::Rejected {
                reason: "down for maintenance".into(),
                retry_in: None,
            }),
            Res::Dns => Err(AnnounceError::Transport(TransportError::Dns)),
            Res::RetryIn5 => Err(AnnounceError::Rejected {
                reason: "later".into(),
                retry_in: Some(RetryIn::Minutes(5)),
            }),
        }
    }
    fn is_ok(self) -> bool {
        matches!(self, Res::Ok1800 | Res::Ok300Min60)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Act {
    Start,
    UserStart,
    Stop,
    Complete,
    Crash,
    Advance(u32),
    Dispatch,
    Respond(Res),
    DupAnswer(Res),
    Manual,
    Network,
    Suspend,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sess {
    Closed,
    Open { acked: bool },
    Stopping,
}

/// What a correct observer believes about the schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Witness {
    sess: Sess,
    completed_total: u8,
    last_ok: Option<(Millis, Timing)>,
    fails: u32,
    last_fail: Option<Millis>,
    wait_after_fail: Duration,
    fail_was_network: bool,
    fail_was_slow: bool,
    net_earliest: Option<Millis>,
    halted: bool,
    rejected_in_session: bool,
    licence: bool,
    last_attempt: Option<Millis>,
    last_up: u64,
}

impl Witness {
    fn new() -> Self {
        Witness {
            sess: Sess::Closed,
            completed_total: 0,
            last_ok: None,
            fails: 0,
            last_fail: None,
            wait_after_fail: Duration::ZERO,
            fail_was_network: false,
            fail_was_slow: false,
            net_earliest: None,
            halted: false,
            rejected_in_session: false,
            licence: false,
            last_attempt: None,
            last_up: 0,
        }
    }
}

#[derive(Clone, Debug)]
struct Sys {
    m: AnnounceMachine,
    now: Millis,
    queued: Option<(RequestId, Option<Event>)>,
    /// Witness before the queued request was emitted (a withdrawn request
    /// never reached the wire, so it must leave no trace).
    before_queued: Option<Witness>,
    inflight: Option<(RequestId, Option<Event>)>,
    latest: Option<RequestId>,
    /// (id, event, succeeded, witness before it, still upgradable)
    resolved: Option<(RequestId, Option<Event>, bool, Witness, bool)>,
    persisted: TimingRecord,
    w: Witness,
    added_complete: bool,
    steps: u32,
    stop_requested: bool,
    uploaded: u64,
    bad: Option<String>,
    // for `sometimes` properties
    saw_completed: bool,
    saw_restart_after_stop: bool,
    saw_halt: bool,
    saw_upgrade: bool,
    stopped_once: bool,
}

impl Hash for Sys {
    fn hash<H: Hasher>(&self, h: &mut H) {
        format!(
            "{:?}",
            (
                &self.m,
                self.now,
                self.queued,
                &self.before_queued,
                self.inflight,
                self.latest,
                &self.resolved,
                &self.persisted,
                &self.w,
                self.uploaded
            )
        )
        .hash(h);
    }
}
impl PartialEq for Sys {
    fn eq(&self, o: &Self) -> bool {
        format!(
            "{:?}",
            (
                &self.m,
                self.now,
                self.queued,
                &self.before_queued,
                self.inflight,
                self.latest,
                &self.resolved,
                &self.persisted,
                &self.w,
                self.uploaded
            )
        ) == format!(
            "{:?}",
            (
                &o.m,
                o.now,
                o.queued,
                &o.before_queued,
                o.inflight,
                o.latest,
                &o.resolved,
                &o.persisted,
                &o.w,
                o.uploaded
            )
        )
    }
}

struct Model1 {
    depth: u32,
}

impl Sys {
    fn new(added_complete: bool) -> Sys {
        let mut m = AnnounceMachine::new(cfg(added_complete), None);
        m.handle(
            Input::Stats(Stats {
                uploaded: 0,
                downloaded: 0,
                left: if added_complete { 0 } else { 100 },
                size: 100,
            }),
            Millis::ZERO,
        );
        Sys {
            m,
            now: Millis::ZERO,
            queued: None,
            before_queued: None,
            inflight: None,
            latest: None,
            resolved: None,
            persisted: TimingRecord {
                completed_sent: added_complete,
                ..TimingRecord::default()
            },
            w: Witness::new(),
            added_complete,
            steps: 0,
            stop_requested: false,
            uploaded: 0,
            bad: None,
            saw_completed: false,
            saw_restart_after_stop: false,
            saw_halt: false,
            saw_upgrade: false,
            stopped_once: false,
        }
    }

    /// A late success must not revive a session the user already ended.
    fn end_session_for_late_answers(&mut self) {
        self.stop_requested = true;
        if let Some(r) = self.resolved.as_mut() {
            r.4 = false;
        }
    }

    /// A start before `stopped` was ever emitted just continues the session.
    fn start_cancels_pending_stop(&mut self) {
        if self.w.sess != Sess::Stopping {
            self.stop_requested = false;
        }
    }

    /// A user action over a halt or the slow retry schedule starts the backoff
    /// afresh. Also applied to the snapshot of a queued request: a withdrawn
    /// request restores the witness to that snapshot, and the user's reset
    /// must survive it.
    fn user_retry_reset(&mut self) {
        let reset = |w: &mut Witness| {
            if w.halted || w.fail_was_slow {
                w.fails = 0;
                w.last_fail = None;
                w.fail_was_slow = false;
            }
            w.halted = false;
        };
        reset(&mut self.w);
        if let Some(b) = self.before_queued.as_mut() {
            reset(b);
        }
    }

    fn violation(&mut self, msg: String) {
        if self.bad.is_none() {
            self.bad = Some(format!("{msg} (t={}s)", self.now.0 / 1000));
        }
    }

    fn apply_success(w: &mut Witness, now: Millis, ev: Option<Event>, t: Timing) {
        w.fails = 0;
        w.last_fail = None;
        w.net_earliest = None;
        w.halted = false;
        w.fail_was_slow = false;
        w.licence = false;
        match ev {
            Some(Event::Stopped) => {
                if w.sess == Sess::Stopping {
                    w.sess = Sess::Closed;
                }
            }
            Some(Event::Started) => {
                if matches!(w.sess, Sess::Open { .. }) {
                    w.sess = Sess::Open { acked: true };
                }
                w.last_ok = Some((now, t));
            }
            _ => w.last_ok = Some((now, t)),
        }
    }

    /// Feed the machine and check what it emits against the witness.
    fn feed(&mut self, input: Input) {
        let intents = self.m.handle(input, self.now);
        for it in intents {
            match it {
                Intent::SendAnnounce(s) => self.check_send(&s),
                Intent::CancelRequest(id) => {
                    if self.queued.map(|q| q.0) == Some(id) {
                        self.queued = None;
                        if let Some(w) = self.before_queued.take() {
                            self.w = w;
                        }
                    } else {
                        self.violation(format!("cancel of {id:?} which is not queued"));
                    }
                }
                Intent::Persist(rec) => self.persisted = rec,
                Intent::Report(Report::InvariantViolation(why)) => {
                    self.violation(format!("self-audit tripped: {why}"))
                }
                Intent::Report(Report::Halted { .. }) => self.saw_halt = true,
                Intent::Report(_) | Intent::ScheduleWake(_) => {}
            }
        }
        if let Some(t) = self.m.tripped() {
            self.violation(format!("machine tripped: {t}"));
        }
    }

    fn check_send(&mut self, s: &SendAnnounce) {
        let now = self.now;
        let p = &s.params;
        let before = self.w.clone();
        if self.queued.is_some() || self.inflight.is_some() {
            self.violation("second outstanding request (inv 5)".into());
        }
        if self.w.halted && p.event != Some(Event::Stopped) {
            self.violation("sent while halted by a failure reason".into());
        }
        if p.event != Some(Event::Stopped) {
            if let Some(lf) = self.w.last_fail {
                if self.w.fails > 0 {
                    let mut earliest = lf + self.w.wait_after_fail;
                    if let Some(ne) = self.w.net_earliest {
                        earliest = earliest.min(ne);
                    }
                    if now < earliest {
                        self.violation(format!(
                            "retry before backoff end ({} < {})",
                            now.0, earliest.0
                        ));
                    }
                }
            }
        }
        match p.event {
            Some(Event::Started) => {
                if matches!(self.w.sess, Sess::Open { acked: true }) && !self.w.rejected_in_session
                {
                    self.violation("second started in one session (inv 2)".into());
                }
                if self.w.sess != (Sess::Open { acked: false }) || self.w.rejected_in_session {
                    self.w.last_up = 0;
                }
                if matches!(self.w.sess, Sess::Closed | Sess::Stopping) && self.stopped_once {
                    self.saw_restart_after_stop = true;
                }
                self.w.rejected_in_session = false;
                self.stop_requested = false;
                self.w.sess = Sess::Open { acked: false };
            }
            Some(Event::Completed) => {
                if self.w.sess != (Sess::Open { acked: true }) {
                    self.violation("completed outside an acknowledged session (inv 2)".into());
                }
                if self.w.completed_total > 0 {
                    self.violation("second completed (inv 3)".into());
                }
                if self.added_complete {
                    self.violation("completed for a torrent added complete (inv 3)".into());
                }
                if p.left != 0 {
                    self.violation("completed with left > 0".into());
                }
            }
            Some(Event::Stopped) => {
                if !matches!(self.w.sess, Sess::Open { .. } | Sess::Stopping) {
                    self.violation("stopped without started (inv 2)".into());
                }
                self.w.sess = Sess::Stopping;
                self.stopped_once = true;
            }
            None => {
                if self.w.sess != (Sess::Open { acked: true }) {
                    self.violation(format!(
                        "announce outside an acknowledged session {:?} (inv 2)",
                        self.w.sess
                    ));
                } else if let Some((at, t)) = self.w.last_ok {
                    let regular = at + t.interval.max(t.min_interval.unwrap_or_default());
                    let min_eff = t
                        .min_interval
                        .unwrap_or(Duration::from_secs(300))
                        .min(t.interval);
                    if now >= regular || (self.w.licence && now >= at + min_eff) {
                        self.w.licence = false;
                    } else {
                        self.violation(format!(
                            "non-event announce {}s after success, interval {}s (inv 1)",
                            now.since(at).as_secs(),
                            t.interval.as_secs()
                        ));
                    }
                }
            }
        }
        if p.uploaded < self.w.last_up {
            self.violation("uploaded decreased (inv 4)".into());
        }
        if p.left > 100 {
            self.violation("left > size (inv 4)".into());
        }
        self.w.last_up = p.uploaded;
        self.w.last_attempt = Some(now);
        self.queued = Some((s.id, p.event));
        self.before_queued = Some(before);
        self.latest = Some(s.id);
        self.resolved = None;
    }

    fn deliver(&mut self, res: Res, id: RequestId, ev: Option<Event>, duplicate: bool) {
        let now = self.now;
        let result = res.build();
        let snapshot = self.w.clone();
        if duplicate {
            let Some((rid, rev, was_ok, snap, live)) = self.resolved.clone() else {
                return;
            };
            if rid != id {
                return;
            }
            if res.is_ok()
                && !was_ok
                && live
                && rev != Some(Event::Stopped)
                && self.latest == Some(id)
                && self.queued.is_none()
                && self.inflight.is_none()
            {
                // success wins over the failure already applied
                self.w = snap;
                let t = result.as_ref().map(|r| r.timing().sanitized()).expect("ok");
                Self::apply_success(&mut self.w, now, rev, t);
                self.resolved = Some((rid, rev, true, self.w.clone(), true));
                self.saw_upgrade = true;
            }
        } else {
            match &result {
                Ok(r) => Self::apply_success(&mut self.w, now, ev, r.timing().sanitized()),
                Err(e) if ev == Some(Event::Stopped) => {
                    let _ = e;
                }
                Err(e) => match e.class() {
                    FailureClass::Halt => {
                        self.w.halted = true;
                        self.w.rejected_in_session = true;
                        self.w.fail_was_slow = false;
                    }
                    FailureClass::Slow => {
                        self.w.fails += 1;
                        self.w.wait_after_fail = slow_nominal_delay(self.w.fails);
                        self.w.last_fail = Some(now);
                        self.w.fail_was_network = false;
                        self.w.fail_was_slow = true;
                        self.w.net_earliest = None;
                    }
                    FailureClass::Backoff { hint } => {
                        self.w.fails += 1;
                        let mut wait = nominal_delay(self.w.fails);
                        if let Some(h) = hint {
                            // a downloading torrent waits exactly the hint (1 min floor)
                            wait = h.max(Duration::from_secs(60));
                        }
                        self.w.wait_after_fail = wait;
                        self.w.last_fail = Some(now);
                        self.w.fail_was_network = e.is_network();
                        self.w.fail_was_slow = false;
                        self.w.net_earliest = None;
                    }
                },
            }
            // Provably undelivered `completed`: the tracker did not count it,
            // so exactly one more may follow.
            if ev == Some(Event::Completed) {
                if let Err(e) = &result {
                    if e.never_reached_tracker() {
                        self.w.completed_total = self.w.completed_total.saturating_sub(1);
                    }
                }
            }
            self.resolved = Some((id, ev, result.is_ok(), snapshot, !self.stop_requested));
        }
        self.feed(Input::Response { id, result });
    }
}

impl Model for Model1 {
    type State = Sys;
    type Action = Act;

    fn init_states(&self) -> Vec<Sys> {
        vec![Sys::new(false), Sys::new(true)]
    }

    fn actions(&self, s: &Sys, out: &mut Vec<Act>) {
        if s.bad.is_some() {
            return;
        }
        out.extend([
            Act::Start,
            Act::UserStart,
            Act::Stop,
            Act::Complete,
            Act::Crash,
            Act::Advance(61),
            Act::Advance(1801),
        ]);
        out.extend([Act::Manual, Act::Network, Act::Suspend]);
        if s.queued.is_some() {
            out.push(Act::Dispatch);
        }
        if s.inflight.is_some() {
            for r in [
                Res::Ok1800,
                Res::Ok300Min60,
                Res::Timeout,
                Res::Http503Retry7200,
                Res::Rejected,
                Res::Maintenance,
                Res::RetryIn5,
                Res::Dns,
            ] {
                out.push(Act::Respond(r));
            }
        }
        if s.resolved.is_some() {
            out.push(Act::DupAnswer(Res::Ok1800));
            out.push(Act::DupAnswer(Res::Timeout));
        }
    }

    fn next_state(&self, last: &Sys, a: Act) -> Option<Sys> {
        let mut s = last.clone();
        s.steps += 1;
        match a {
            Act::Start => {
                s.feed(Input::Torrent(TorrentEvent::Start));
                s.start_cancels_pending_stop();
            }
            Act::UserStart => {
                let was_halted = s.w.halted;
                s.user_retry_reset();
                let _ = was_halted;
                s.feed(Input::ClearHalt);
                s.feed(Input::Torrent(TorrentEvent::Start));
                s.start_cancels_pending_stop();
            }
            Act::Stop => {
                s.end_session_for_late_answers();
                s.feed(Input::Torrent(TorrentEvent::Stop));
            }
            Act::Suspend => {
                s.end_session_for_late_answers();
                s.feed(Input::Suspend);
            }
            Act::Complete => {
                if s.persisted.completed_sent && !s.m.completed_sent() {
                    return None;
                }
                s.feed(Input::Torrent(TorrentEvent::Complete));
                s.feed(Input::Stats(Stats {
                    uploaded: s.uploaded,
                    downloaded: 100,
                    left: 0,
                    size: 100,
                }));
            }
            Act::Crash => {
                let rec = s.persisted.clone();
                s.m = AnnounceMachine::new(cfg(s.added_complete), Some(rec));
                let left = if s.m.completed_sent() { 0 } else { 100 };
                s.m.handle(
                    Input::Stats(Stats {
                        uploaded: 0,
                        downloaded: 0,
                        left,
                        size: 100,
                    }),
                    s.now,
                );
                s.queued = None;
                s.before_queued = None;
                s.inflight = None;
                s.latest = None;
                s.resolved = None;
                s.w.sess = if s.w.sess == Sess::Closed {
                    Sess::Closed
                } else {
                    Sess::Stopping
                };
                s.w.fails = 0;
                s.w.last_fail = None;
                s.w.net_earliest = None;
                s.w.halted = false;
                s.w.licence = false;
                s.w.last_up = 0;
                s.w.rejected_in_session = false;
            }
            Act::Advance(secs) => {
                s.now = s.now + Duration::from_secs(u64::from(secs));
                s.feed(Input::Timer);
            }
            Act::Dispatch => {
                let (id, ev) = s.queued.take()?;
                s.before_queued = None;
                if ev == Some(Event::Completed) {
                    // `completed` counts once it is on the wire (a request that
                    // dies in the queue in a crash was never sent).
                    s.w.completed_total += 1;
                    s.saw_completed = true;
                }
                s.inflight = Some((id, ev));
                s.feed(Input::Dispatched(id));
            }
            Act::Respond(r) => {
                let (id, ev) = s.inflight.take()?;
                s.deliver(r, id, ev, false);
            }
            Act::DupAnswer(r) => {
                let (id, ev, _, _, _) = s.resolved.clone()?;
                s.deliver(r, id, ev, true);
            }
            Act::Manual => {
                s.user_retry_reset();
                s.w.licence = true;
                s.feed(Input::ManualReannounce);
            }
            Act::Network => {
                // a network change never licenses an early regular announce
                if let (true, true, Some(lf)) = (s.w.fails > 0, s.w.fail_was_network, s.w.last_fail)
                {
                    let floor = s.w.last_attempt.map_or(lf, |t| t + Duration::from_secs(30));
                    s.w.net_earliest = Some(floor.max(lf + Duration::from_secs(1)));
                }
                s.feed(Input::NetworkChanged);
            }
        }
        Some(s)
    }

    fn within_boundary(&self, s: &Sys) -> bool {
        s.steps <= self.depth && s.now.0 <= 3 * 3600 * 1000
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always(
                "no invariant violation (SPEC 7C.3 / 7C.4 / 7C.4a)",
                |_, s: &Sys| s.bad.is_none(),
            ),
            Property::sometimes("completed is reachable", |_, s: &Sys| s.saw_completed),
            Property::sometimes("a restart after stopped is reachable", |_, s: &Sys| {
                s.saw_restart_after_stop
            }),
            Property::sometimes("a failure-reason halt is reachable", |_, s: &Sys| {
                s.saw_halt
            }),
            Property::sometimes(
                "success-after-failure upgrade is reachable",
                |_, s: &Sys| s.saw_upgrade,
            ),
        ]
    }
}

#[test]
fn bounded_model_check() {
    let depth = std::env::var("MODEL_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(debug_assertions) { 8 } else { 9 });
    let checker = Model1 { depth }
        .checker()
        .threads(std::thread::available_parallelism().map_or(2, |n| n.get()))
        .spawn_bfs()
        .join();
    println!(
        "model check: depth {depth}, {} unique states, {} generated, max depth {}",
        checker.unique_state_count(),
        checker.state_count(),
        checker.max_depth()
    );
    checker.assert_properties();
}
