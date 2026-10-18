//! Property tests over the whole stack (machines + sets + gate + hub) with
//! random user actions, faults, crashes, sleeps and many torrents. The
//! independent `Oracle` asserts SPEC 7C.3 invariants 1-7 after every request.

use crate::common;

use std::time::Duration;

use common::*;
use proptest::prelude::*;
use trs_tracker::Millis;
use trs_tracker::gate::{GateConfig, GatedRequest};
use trs_tracker::response::{AnnounceError, RetryIn, TransportError};

/// Random fault injection driven by a seed so proptest can shrink the knobs.
struct Faulty {
    state: u64,
    p_fail: u64,
    changing_tracker_ids: bool,
}

impl Faulty {
    fn next(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.state >> 33
    }
}

impl Responder for Faulty {
    fn respond(&mut self, _now: Millis, _host: &str, req: &GatedRequest) -> Reply {
        let r = self.next();
        let secs = Duration::from_secs;
        if r % 100 < self.p_fail {
            let kind = self.next() % 9;
            let (lat, res) = match kind {
                0 => (secs(30), Err(timeout_err())),
                1 => (
                    secs(1),
                    Err(AnnounceError::Http {
                        status: 503,
                        retry_after: None,
                    }),
                ),
                2 => (
                    secs(1),
                    Err(AnnounceError::Http {
                        status: 429,
                        retry_after: Some(secs(60 + self.next() % 7200)),
                    }),
                ),
                3 => (
                    secs(1),
                    Err(AnnounceError::Garbage(
                        trs_tracker::response::ParseError::NotBencode,
                    )),
                ),
                4 => (secs(0), Err(AnnounceError::Transport(TransportError::Dns))),
                5 => (
                    secs(1),
                    Err(AnnounceError::Rejected {
                        reason: "nope".into(),
                        retry_in: None,
                    }),
                ),
                6 => (
                    secs(1),
                    Err(AnnounceError::Rejected {
                        reason: "later".into(),
                        retry_in: Some(RetryIn::Minutes(1 + (self.next() % 90) as u32)),
                    }),
                ),
                7 => (
                    secs(1),
                    Err(AnnounceError::Transport(TransportError::Connect)),
                ),
                _ => {
                    // hang: the gate times it out; the late answer is an error
                    let late = Err(timeout_err());
                    (secs(5000), late)
                }
            };
            return Reply::new(lat, res);
        }
        let iv = [30u64, 120, 300, 900, 1800, 3600, 90_000][(self.next() % 7) as usize];
        let min = match self.next() % 4 {
            0 => Some(30),
            1 => Some(iv / 2),
            2 => Some(iv),
            _ => None,
        };
        let mut resp = ok_response(iv, min);
        if self.changing_tracker_ids && self.next() % 3 == 0 {
            resp.tracker_id = Some(format!("tid{}", self.next() % 5).into_bytes().into());
        }
        if self.next() % 10 == 0 {
            resp.warning = Some("slow down".into());
        }
        let _ = req;
        Reply::new(Duration::from_millis(50 + self.next() % 2000), Ok(resp))
    }
}

#[derive(Debug, Clone)]
enum Act {
    Advance(u32),
    Start(usize),
    Stop(usize),
    Pause(usize),
    Resume(usize),
    Complete(usize),
    Manual(usize),
    Progress(usize, u32, u32),
    Network,
    Sleep(u32),
    Crash(bool),
    StartAll,
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        5 => (0u32..400).prop_map(Act::Advance),
        4 => (0u32..40_000).prop_map(Act::Advance),
        4 => (0usize..8).prop_map(Act::Start),
        2 => (0usize..8).prop_map(Act::Stop),
        2 => (0usize..8).prop_map(Act::Pause),
        2 => (0usize..8).prop_map(Act::Resume),
        2 => (0usize..8).prop_map(Act::Complete),
        3 => (0usize..8).prop_map(Act::Manual),
        3 => (0usize..8, 0u32..100_000, 0u32..100_000).prop_map(|(i, u, d)| Act::Progress(i, u, d)),
        1 => Just(Act::Network),
        1 => (0u32..400_000).prop_map(Act::Sleep),
        1 => any::<bool>().prop_map(Act::Crash),
        1 => Just(Act::StartAll),
    ]
}

const LAYOUTS: usize = 7;

fn layout(i: usize) -> Vec<Vec<String>> {
    let v = |t: &[&[&str]]| {
        t.iter()
            .map(|x| x.iter().map(|s| s.to_string()).collect())
            .collect()
    };
    match i % LAYOUTS {
        0 => v(&[&["http://h1.test/announce"]]),
        1 => v(&[&[
            "http://h1.test/announce?passkey=k1",
            "http://h2.test/announce",
        ]]),
        2 => v(&[
            &["http://h2.test/announce"],
            &["udp://h3.test:1337/announce"],
        ]),
        3 => vec![vec![format!(
            "http://p{i}.private.test/ann/PASS{i}/announce"
        )]],
        4 => v(&[&[
            "http://h1.test/announce",
            "http://h2.test/announce",
            "http://h3.test/announce",
        ]]),
        5 => v(&[&["http://h3.test/announce"], &["https://h1.test/announce"]]),
        _ => v(&[&["http://h2.test:8080/announce"]]),
    }
}

fn build_world(
    n: usize,
    seed: u64,
    p_fail: u64,
    sloppy: bool,
    tids: bool,
    complete_at_add: u8,
) -> World<Faulty> {
    let mut w = World::new(
        Faulty {
            state: seed,
            p_fail,
            changing_tracker_ids: tids,
        },
        GateConfig::default(),
    );
    w.sloppy_engine = sloppy;
    for i in 0..n {
        let tiers = layout(i);
        let refs: Vec<Vec<&str>> = tiers
            .iter()
            .map(|t| t.iter().map(String::as_str).collect())
            .collect();
        let slices: Vec<&[&str]> = refs.iter().map(Vec::as_slice).collect();
        let mut s = spec(i as u32 + 1, 6881 + i as u16, &slices);
        s.private = i % LAYOUTS == 3;
        let size = 1_000_000u64 * (i as u64 + 1);
        let left = if (complete_at_add as usize) % (n + 1) == i {
            0
        } else {
            size
        };
        w.add_torrent(s, size, left);
    }
    w
}

fn run_script(w: &mut World<Faulty>, n: usize, acts: &[Act]) {
    let ihs: Vec<_> = (0..n).map(|i| ih(i as u32 + 1)).collect();
    for a in acts {
        match a {
            Act::Advance(s) => w.run_for(Duration::from_secs(u64::from(*s))),
            Act::Start(i) => w.start(ihs[i % n]),
            Act::Stop(i) => w.stop(ihs[i % n]),
            Act::Pause(i) => w.pause(ihs[i % n]),
            Act::Resume(i) => w.resume(ihs[i % n]),
            Act::Complete(i) => w.complete(ihs[i % n]),
            Act::Manual(i) => w.manual(ihs[i % n]),
            Act::Progress(i, u, d) => w.progress(ihs[i % n], u64::from(*u), u64::from(*d)),
            Act::Network => w.network_changed(),
            Act::Sleep(s) => w.sleep(Duration::from_secs(u64::from(*s))),
            Act::Crash(id) => w.crash(*id),
            Act::StartAll => {
                for h in &ihs {
                    w.start(*h);
                }
            }
        }
        w.pump();
    }
}

/// Case count from `PROPTEST_CASES`, defaulting to `default`.
fn cfg(default: u32) -> ProptestConfig {
    let cases = std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default);
    ProptestConfig {
        cases,
        max_shrink_iters: 4000,
        ..ProptestConfig::default()
    }
}

/// One whole scenario: the script, then a wind-down; the oracle asserts SPEC 7C.3 after every
/// request on the way.
fn check_case(
    n: usize,
    seed: u64,
    p_fail: u64,
    sloppy: bool,
    tids: bool,
    complete_at_add: u8,
    acts: &[Act],
) {
    let mut w = build_world(n, seed, p_fail, sloppy, tids, complete_at_add);
    run_script(&mut w, n, acts);
    // wind down: stop everything, give stop retries time, and require quiescence
    for i in 0..n {
        w.stop(ih(i as u32 + 1));
    }
    w.run_for(Duration::from_secs(2 * 86_400));
    for i in 0..n {
        let h = ih(i as u32 + 1);
        assert!(w.hub().is_quiescent(&h), "torrent {:02x} did not wind down", h[0]);
    }
    // every host stayed inside its budget (the oracle panics otherwise);
    // sanity: the per-host 1-second window never exceeded burst + 1
    assert!(w.oracle.max_sends_in_1s_window <= 4);
}

proptest! {
    #![proptest_config(cfg(300))]

    /// Invariants 1-7 under random events, responses, crashes and clock jumps.
    #[test]
    fn invariants_hold(
        n in 1usize..8,
        seed in any::<u64>(),
        p_fail in 0u64..70,
        sloppy in any::<bool>(),
        tids in any::<bool>(),
        complete_at_add in 0u8..10,
        acts in proptest::collection::vec(act(), 0..70),
    ) {
        check_case(n, seed, p_fail, sloppy, tids, complete_at_add, &acts);
    }
}

/// Counterexample 1 of the outage work (SPEC 7C.4c). A `completed` was sent, the machine slept,
/// and the request's timeout was noticed only at the wake. Two connect failures on other hosts
/// then declared an outage, which takes back the failures it revealed - this late timeout among
/// them, because the hub counts it when it sees it (at the wake), not at the deadline. The
/// oracle used to date it at the deadline, hours before, and expected a two-failure backoff.
#[test]
fn regression_timeout_noticed_at_wake_is_rolled_back_with_the_outage() {
    use Act::*;
    check_case(
        5,
        7354476410921757212,
        59,
        false,
        true,
        1,
        &[
            Resume(2),
            Sleep(21730),
            Advance(24292),
            Stop(2),
            StartAll,
            Advance(18533),
            Pause(2),
            Manual(6),
            Complete(0),
            Sleep(129747),
            Start(2),
            Advance(98),
        ],
    );
}

/// Counterexample 2. A UDP request (10 min deadline) went unanswered; its deadline fell inside
/// an outage that lasted under a second. "A network failure while Offline is not counted"
/// (SPEC 7C.4c), so the hub counted nothing and the next announce left 645 s after the first.
/// The oracle used to settle the expiry lazily at the next send, when the outage was over, and
/// counted it.
#[test]
fn regression_timeout_inside_an_outage_is_not_counted() {
    use Act::*;
    check_case(
        7,
        3399949271910287880,
        29,
        false,
        false,
        0,
        &[
            Start(6),
            Start(3),
            Advance(63),
            Sleep(163519),
            Start(4),
            Start(5),
            Resume(2),
            Advance(951),
        ],
    );
}

/// Guard for the oracle's ordering rule that counterexample 2 forced: a timeout is judged at the
/// hub's tick, not earlier. Here a request sent just before an outage is answered with a success
/// only when the machine wakes days later, before the tick. A success always wins (SPEC 7C.4a
/// rule 3): the failure count starts over and the next announce is an interval after that answer.
/// Settling the expiry at the end of the outage instead (the first attempt at the fix) ignored
/// the answer and still expected the seven failures from before.
#[test]
fn regression_success_after_a_sleep_resets_the_backoff() {
    use Act::*;
    check_case(
        7,
        16424581077155485989,
        58,
        false,
        true,
        8,
        &[
            Advance(347),
            StartAll,
            Advance(310),
            StartAll,
            Advance(192),
            Stop(7),
            Advance(47),
            Pause(5),
            Start(5),
            Advance(0),
            Advance(13706),
            Sleep(88638),
            Sleep(586),
            Start(0),
            Pause(1),
            Advance(70),
            Advance(0),
            Advance(0),
            Pause(1),
            Advance(0),
            Advance(0),
            StartAll,
            Advance(152),
            Advance(0),
            Advance(0),
        ],
    );
}

/// SPEC 7C.4a at system level: many torrents announce at once after a long
/// idle period (sleep/wake) - each tracker still sees at most one announce per
/// torrent and the per-host ceiling holds.
#[test]
fn thundering_herd_after_wake() {
    for seed in 0..5u64 {
        let mut w = build_world(7, seed, 0, false, false, 99);
        // 100 extra torrents on the same three hosts
        for i in 7..107usize {
            let tiers = layout(i % 3);
            let refs: Vec<Vec<&str>> = tiers
                .iter()
                .map(|t| t.iter().map(String::as_str).collect())
                .collect();
            let slices: Vec<&[&str]> = refs.iter().map(Vec::as_slice).collect();
            w.add_torrent(spec(i as u32 + 1, 7000, &slices), 1_000_000, 500_000);
        }
        let all: Vec<_> = (0..107).map(|i| ih(i as u32 + 1)).collect();
        for h in &all {
            w.start(*h);
        }
        w.run_for(Duration::from_secs(3600));
        let before = w.total_sent;
        w.sleep(Duration::from_secs(10 * 3600));
        w.run_for(Duration::from_secs(1800));
        let sent_after = w.total_sent - before;
        // One announce per torrent after waking (plus nothing else: no failures here).
        assert!(sent_after <= 107 + 107, "herd: {sent_after} requests");
        assert!(w.oracle.max_sends_in_1s_window <= 4);
        assert_eq!(w.breaker_opens, 0);
    }
}
