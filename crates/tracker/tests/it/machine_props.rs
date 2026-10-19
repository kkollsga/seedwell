//! Property tests of a single `AnnounceMachine` driven directly: the
//! Transmission 4.0.6 regression (SPEC 7C.4a) and robustness against arbitrary
//! input sequences.

use std::time::Duration;

use proptest::prelude::*;
use trs_tracker::backoff::{nominal_delay, slow_nominal_delay};
use trs_tracker::response::{FailureClass, ParseError, TransportError};
use trs_tracker::*;

fn cfg() -> MachineConfig {
    let mut c = MachineConfig::new([3; 20], [4; 20], 0x1234, 6881);
    c.start_jitter = Duration::ZERO;
    c
}

fn ok(interval: u64, min: Option<u64>) -> AnnounceResponse {
    let t = clamp_timing(Some(interval as i64), min.map(|m| m as i64));
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

#[derive(Clone, Debug)]
enum Res {
    Ok(u64, Option<u64>),
    Timeout,
    Dns,
    Http(u16, Option<u32>),
    Garbage,
    /// A permanent-looking `failure reason` (halts).
    Rejected,
    /// A maintenance-style `failure reason` (slow retry schedule).
    Maintenance,
    RetryIn(u32),
}

impl Res {
    fn build(&self) -> Result<AnnounceResponse, AnnounceError> {
        match self {
            Res::Ok(i, m) => Ok(ok(*i, *m)),
            Res::Timeout => Err(AnnounceError::Transport(TransportError::Timeout)),
            Res::Dns => Err(AnnounceError::Transport(TransportError::Dns)),
            Res::Http(s, ra) => Err(AnnounceError::Http {
                status: *s,
                retry_after: ra.map(|r| Duration::from_secs(u64::from(r))),
            }),
            Res::Garbage => Err(AnnounceError::Garbage(ParseError::NotBencode)),
            Res::Rejected => Err(AnnounceError::Rejected {
                reason: "Torrent not registered with this tracker".into(),
                retry_in: None,
            }),
            Res::Maintenance => Err(AnnounceError::Rejected {
                reason: "down for maintenance".into(),
                retry_in: None,
            }),
            Res::RetryIn(m) => Err(AnnounceError::Rejected {
                reason: "later".into(),
                retry_in: Some(RetryIn::Minutes(*m)),
            }),
        }
    }
    fn is_ok(&self) -> bool {
        matches!(self, Res::Ok(..))
    }
}

fn res() -> impl Strategy<Value = Res> {
    prop_oneof![
        4 => (0u64..100_000, proptest::option::of(0u64..100_000)).prop_map(|(i, m)| Res::Ok(i, m)),
        2 => Just(Res::Timeout),
        1 => Just(Res::Dns),
        2 => (400u16..600, proptest::option::of(0u32..10_000)).prop_map(|(s, r)| Res::Http(s, r)),
        1 => Just(Res::Garbage),
        1 => Just(Res::Rejected),
        1 => Just(Res::Maintenance),
        1 => (1u32..120).prop_map(Res::RetryIn),
    ]
}

/// Drive helper that dispatches instantly and records sends.
struct Rig {
    m: AnnounceMachine,
    now: Millis,
    sends: Vec<(Millis, SendAnnounce)>,
    latest: Option<RequestId>,
}

impl Rig {
    fn new() -> Self {
        Rig {
            m: AnnounceMachine::new(cfg(), None),
            now: Millis::ZERO,
            sends: vec![],
            latest: None,
        }
    }
    fn input(&mut self, i: Input) -> Vec<SendAnnounce> {
        let mut out = Vec::new();
        let mut q = vec![i];
        while let Some(inp) = q.pop() {
            for it in self.m.handle(inp, self.now) {
                if let Intent::SendAnnounce(s) = it {
                    self.sends.push((self.now, s.clone()));
                    self.latest = Some(s.id);
                    q.push(Input::Dispatched(s.id));
                    out.push(s);
                }
            }
        }
        out
    }
}

/// The schedule model (what SPEC 7C.4a says must hold).
#[derive(Default)]
struct Model {
    deadline: Option<Millis>,
    retry_not_before: Option<Millis>,
    fails: u32,
}

/// Case count from `PROPTEST_CASES`, defaulting to `default`.
fn proptest_cfg(default: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    ProptestConfig {
        cases,
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(proptest_cfg(400))]

    /// 4.0.6: for ONE logical announce, several results (e.g. IPv4 success and
    /// IPv6 failure) delivered in any order, at the same instant: the success
    /// wins, nothing is counted twice, and the next announce comes one
    /// interval later - never "in 20 seconds".
    #[test]
    fn success_wins_in_any_order_batched(
        rounds in proptest::collection::vec(
            (proptest::collection::vec(res(), 1..5), 0u64..200_000), 1..12),
    ) {
        let mut r = Rig::new();
        r.input(Input::Stats(Stats { uploaded: 0, downloaded: 0, corrupt: 0, left: 10, size: 10 }));
        let mut pending = r.input(Input::Torrent(TorrentEvent::Start));
        let mut model = Model::default();
        for (results, wait) in rounds {
            let Some(req) = pending.pop() else {
                // nothing outstanding: let time pass until the machine sends
                r.now = r.m.next_wake().unwrap_or(r.now).max(r.now);
                pending = r.input(Input::Timer);
                continue;
            };
            // Is it halted? then only the user can proceed.
            let first_ok = results.iter().find(|x| x.is_ok()).cloned();
            let first = results[0].clone();
            let was_halted_before = r.m.health() == Health::Halted;
            prop_assert!(!was_halted_before);
            for x in &results {
                r.input(Input::Response { id: req.id, result: x.build() });
            }
            match first_ok {
                Some(Res::Ok(i, m)) => {
                    let t = clamp_timing(Some(i as i64), m.map(|m| m as i64));
                    prop_assert_eq!(r.m.failures(), 0, "success must erase a concurrent failure");
                    prop_assert_eq!(r.m.retry_at(), None);
                    prop_assert_eq!(r.m.regular_at(), Some(r.now + t.interval), "next announce one interval after the success");
                    prop_assert_eq!(r.m.health(), Health::Healthy);
                    model.deadline = Some(r.now + t.interval);
                    model.retry_not_before = None;
                    model.fails = 0;
                }
                _ => match first.build() {
                    Err(e) => match e.class() {
                        FailureClass::Halt => {
                            prop_assert_eq!(r.m.health(), Health::Halted);
                            // a user retry gets us going again
                            pending = r.input(Input::ManualReannounce);
                            // (the user's retry starts the backoff afresh)
                            model.fails = 0;
                            model.retry_not_before = r.m.retry_at();
                            continue;
                        }
                        FailureClass::Slow => {
                            model.fails += 1;
                            prop_assert_eq!(r.m.failures(), 1.max(model.fails), "duplicate failures count once");
                            model.retry_not_before = Some(r.now + slow_nominal_delay(model.fails));
                        }
                        FailureClass::Backoff { hint } => {
                            model.fails += 1;
                            prop_assert_eq!(r.m.failures(), 1.max(model.fails), "duplicate failures count once");
                            let mut wait_min = nominal_delay(model.fails);
                            if let Some(h) = hint {
                    // a downloading torrent waits exactly the hint (1 min floor)
                    wait_min = h.max(Duration::from_secs(60));
                }
                            model.retry_not_before = Some(r.now + wait_min);
                        }
                    },
                    Ok(_) => unreachable!(),
                },
            }
            // let time pass and collect what the machine does next
            let target = r.now + Duration::from_secs(wait);
            let mut sent_now = Vec::new();
            loop {
                let step = r.m.next_wake().filter(|w| *w <= target);
                match step {
                    Some(w) => {
                        r.now = w.max(r.now);
                        sent_now.extend(r.input(Input::Timer));
                        if !sent_now.is_empty() { break; }
                    }
                    None => { r.now = target; sent_now.extend(r.input(Input::Timer)); break; }
                }
            }
            for s in &sent_now {
                if s.params.event.is_none()
                    && let Some(d) = model.deadline {
                        prop_assert!(r.now >= d, "non-event announce at {:?} before the interval deadline {:?}", r.now, d);
                    }
                if let Some(nb) = model.retry_not_before {
                    prop_assert!(r.now >= nb, "announce at {:?} before backoff end {:?}", r.now, nb);
                }
            }
            pending = sent_now;
        }
    }

    /// Success after failure, delivered later but before any newer request
    /// was sent, still wins; delivered after a newer request, it is ignored.
    #[test]
    fn late_success_wins_only_while_nothing_newer_was_sent(delay in 0u64..400, iv in 300u64..5000) {
        let mut r = Rig::new();
        r.input(Input::Stats(Stats { uploaded: 0, downloaded: 0, corrupt: 0, left: 10, size: 10 }));
        let first = r.input(Input::Torrent(TorrentEvent::Start)).remove(0);
        r.input(Input::Response { id: first.id, result: Res::Timeout.build() });
        let retry_at = r.m.retry_at().unwrap();
        prop_assert!(retry_at.0 >= 60_000);
        r.now = r.now + Duration::from_secs(delay);
        let before_retry = r.now < retry_at;
        // timers may fire the retry first
        let sent = r.input(Input::Timer);
        r.input(Input::Response { id: first.id, result: Ok(ok(iv, None)) });
        if before_retry {
            prop_assert!(sent.is_empty());
            prop_assert_eq!(r.m.failures(), 0);
            prop_assert_eq!(r.m.regular_at(), Some(r.now + Duration::from_secs(iv)));
        } else {
            prop_assert_eq!(sent.len(), 1, "the retry went out");
            prop_assert_eq!(r.m.failures(), 1, "late success for an old request must not touch the new one");
            prop_assert_eq!(r.m.regular_at(), None);
        }
    }

    /// Arbitrary (even nonsensical) inputs never panic, never trip the
    /// self-audit, and keep the core invariants.
    #[test]
    fn arbitrary_inputs_are_safe(
        added_complete in any::<bool>(),
        ops in proptest::collection::vec(op(), 0..120),
    ) {
        let mut c = cfg();
        c.added_complete = added_complete;
        let mut r = Rig { m: AnnounceMachine::new(c, None), now: Millis::ZERO, sends: vec![], latest: None };
        let mut outstanding: Vec<RequestId> = Vec::new();
        // `completed` requests the tracker could have counted: sent ones, minus
        // those whose failure provably never reached it (the machine rolls its
        // flag back for exactly one re-send), plus those a late success revived.
        let mut completed: i32 = 0;
        let mut stopped_seen = false;
        for o in ops {
            let intents_before = r.sends.len();
            let flag_before = r.m.completed_sent();
            let sent = match o {
                Op::Ev(e) => r.input(Input::Torrent(e)),
                Op::Suspend => r.input(Input::Suspend),
                Op::ClearHalt => r.input(Input::ClearHalt),
                Op::Manual => r.input(Input::ManualReannounce),
                Op::Net => r.input(Input::NetworkChanged),
                Op::Wake => r.input(Input::Wake),
                Op::Timer(dt) => { r.now = r.now + Duration::from_secs(dt); r.input(Input::Timer) }
                Op::Stats(u, d, l, s) => r.input(Input::Stats(Stats { uploaded: u, downloaded: d, corrupt: 0, left: l, size: s })),
                Op::Respond(k, x) => {
                    let id = outstanding.get(k % outstanding.len().max(1)).copied().unwrap_or(RequestId(k as u64));
                    r.input(Input::Response { id, result: x.build() })
                }
                Op::Bogus(id) => { r.input(Input::Dispatched(RequestId(id))); r.input(Input::Response { id: RequestId(id), result: Res::Timeout.build() }) }
            };
            outstanding.extend(sent.iter().map(|s| s.id));
            let sent_completed = r.sends[intents_before..]
                .iter()
                .any(|(_, s)| s.params.event == Some(Event::Completed));
            if !sent_completed {
                match (flag_before, r.m.completed_sent()) {
                    (true, false) => completed -= 1,
                    (false, true) => completed += 1,
                    _ => {}
                }
            }
            for (_, s) in r.sends[intents_before..].iter() {
                match s.params.event {
                    Some(Event::Completed) => completed += 1,
                    Some(Event::Started) => stopped_seen = false,
                    Some(Event::Stopped) => stopped_seen = true,
                    None => prop_assert!(!stopped_seen, "announce after stopped"),
                }
                if s.params.event == Some(Event::Completed) {
                    prop_assert_eq!(s.params.left, 0);
                }
            }
            prop_assert!(completed <= 1, "completed more than once");
            if added_complete { prop_assert_eq!(completed, 0); }
            prop_assert!(completed >= 0);
            prop_assert!(r.m.tripped().is_none(), "self-audit tripped: {:?}", r.m.tripped());
        }
        // counters never decreased inside a session in what was sent
        let mut last: Option<(u64, u64)> = None;
        for (_, s) in &r.sends {
            if let Some((u, d)) = last {
                // (a `started` may begin a new session, where counters restart)
                if s.params.event != Some(Event::Started) {
                    prop_assert!(s.params.uploaded >= u && s.params.downloaded >= d);
                }
            }
            last = Some((s.params.uploaded, s.params.downloaded));
        }
    }
}

#[derive(Clone, Debug)]
enum Op {
    Ev(TorrentEvent),
    Suspend,
    ClearHalt,
    Manual,
    Net,
    Wake,
    Timer(u64),
    Stats(u64, u64, u64, u64),
    Respond(usize, Res),
    Bogus(u64),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => prop_oneof![
            Just(TorrentEvent::Start), Just(TorrentEvent::Stop), Just(TorrentEvent::Pause),
            Just(TorrentEvent::Resume), Just(TorrentEvent::Complete)
        ].prop_map(Op::Ev),
        1 => Just(Op::Suspend),
        1 => Just(Op::ClearHalt),
        1 => Just(Op::Manual),
        1 => Just(Op::Net),
        1 => Just(Op::Wake),
        6 => (0u64..100_000).prop_map(Op::Timer),
        3 => (0u64..1000, 0u64..1000, 0u64..2000, 0u64..1500).prop_map(|(a, b, c, d)| Op::Stats(a, b, c, d)),
        8 => (0usize..8, res()).prop_map(|(k, r)| Op::Respond(k, r)),
        1 => (0u64..50).prop_map(Op::Bogus),
    ]
}
