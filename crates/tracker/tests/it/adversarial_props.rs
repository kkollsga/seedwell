//! Adversarial audit: whole-hub property tests with an independent tracker-side
//! checker (`adversarial_support::View` plus the interval/backoff/concurrency/
//! bucket rules below, all derived from SPEC 7B/7C, not from the implementation).

use crate::adversarial_support;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adversarial_support::*;
use proptest::prelude::*;
use trs_tracker::response::AnnounceError;
use trs_tracker::{Event, InfoHash20, Millis, TorrentEvent, clamp_timing};

const HOSTS: [&str; 3] = ["a.test", "b.test", "c.test"];
const SIZE: u64 = 1000;

#[derive(Clone, Debug)]
enum Op {
    Adv(u32),
    Sleep(u32),
    Start(u8),
    Stop(u8),
    Pause(u8),
    Resume(u8),
    Complete(u8),
    Progress(u8, u16),
    Recheck(u8),
    Manual(u8),
    Network,
    Wake,
    Crash,
    Mode(u8, u8),
}

fn op_strategy(allow_user_pressure: bool) -> impl Strategy<Value = Op> {
    let base = prop_oneof![
        8 => (0u32..4000).prop_map(Op::Adv),
        2 => (1u32..40 * 3600).prop_map(Op::Sleep),
        3 => (0u8..3).prop_map(Op::Start),
        2 => (0u8..3).prop_map(Op::Stop),
        3 => (0u8..3).prop_map(Op::Pause),
        3 => (0u8..3).prop_map(Op::Resume),
        2 => (0u8..3).prop_map(Op::Complete),
        4 => (0u8..3, 0u16..300).prop_map(|(i, n)| Op::Progress(i, n)),
        1 => (0u8..3).prop_map(Op::Recheck),
        1 => Just(Op::Crash),
        5 => (0u8..3, 0u8..10).prop_map(|(h, m)| Op::Mode(h, m)),
    ];
    if allow_user_pressure {
        prop_oneof![
            10 => base,
            2 => (0u8..3).prop_map(Op::Manual),
            1 => Just(Op::Network),
            1 => Just(Op::Wake),
        ]
        .boxed()
    } else {
        base.boxed()
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Up,
    Timeout,
    Http5xx,
    Reject,
    Garbage,
    Hang,
    Slow,
    DualFail,
    Throttle,
}

fn mode_from(m: u8) -> Mode {
    [
        Mode::Up,
        Mode::Up,
        Mode::Timeout,
        Mode::Http5xx,
        Mode::Reject,
        Mode::Garbage,
        Mode::Hang,
        Mode::Slow,
        Mode::DualFail,
        Mode::Throttle,
    ][usize::from(m) % 10]
}

#[derive(Clone, Debug)]
struct Info {
    ok: Option<(u64, Option<u64>)>, // clamped (interval, min) seconds when the answer is a success
    rejected: bool,
    latency: Duration,
    hang: bool,
    min_wait: Duration,
    http_status: Option<u16>,
}

#[derive(Default)]
struct Shared {
    modes: [Option<Mode>; 3],
    info: HashMap<usize, Info>,
    session_up: HashMap<InfoHash20, u64>,
}

const IVS: [(u64, Option<u64>); 5] = [
    (1800, None),
    (900, Some(900)),
    (60, None),
    (120, Some(60)),
    (3600, Some(1800)),
];

fn responder(shared: Rc<RefCell<Shared>>) -> Responder {
    Box::new(move |sent| {
        let mut sh = shared.borrow_mut();
        let hi = HOSTS.iter().position(|h| *h == sent.host).unwrap();
        let mode = sh.modes[hi].unwrap_or(Mode::Up);
        let (iv, min) = IVS[(sent.n * 7 + hi) % IVS.len()];
        let t = clamp_timing(Some(iv as i64), min.map(|m| m as i64));
        let okinfo = Some((t.interval.as_secs(), t.min_interval.map(|m| m.as_secs())));
        let mut info = Info {
            ok: None,
            rejected: false,
            latency: d(1),
            hang: false,
            min_wait: Duration::ZERO,
            http_status: None,
        };
        let out = match mode {
            Mode::Up => {
                info.ok = okinfo;
                vec![(d(1), ok(iv, min))]
            }
            Mode::Timeout => vec![(d(20), timeout())],
            Mode::Http5xx => {
                info.http_status = Some(503);
                vec![(d(1), http_status(503))]
            }
            Mode::Reject => {
                info.rejected = true;
                vec![(d(1), reject("nope"))]
            }
            Mode::Garbage => vec![(
                d(1),
                Err(AnnounceError::Garbage(
                    trs_tracker::response::ParseError::NotBencode,
                )),
            )],
            Mode::Hang => {
                info.hang = true;
                vec![]
            }
            Mode::Slow => {
                info.ok = okinfo;
                info.latency = d(25);
                vec![(d(25), ok(iv, min))]
            }
            Mode::DualFail => {
                info.ok = okinfo;
                vec![(d(1), ok(iv, min)), (d(3), timeout())]
            }
            Mode::Throttle => {
                info.min_wait = d(600);
                info.http_status = Some(429);
                vec![(
                    d(1),
                    Err(AnnounceError::Http {
                        status: 429,
                        retry_after: Some(d(600)),
                    }),
                )]
            }
        };
        sh.info.insert(sent.n, info);
        out
    })
}

struct Engine {
    running: [bool; 3],
    left: [u64; 3],
    up: [u64; 3],
    down: [u64; 3],
}

fn tiers_for(i: u8) -> Vec<Vec<&'static str>> {
    match i {
        0 => vec![vec!["http://a.test/announce?pk=0"]],
        1 => vec![
            vec!["http://a.test/announce?pk=1"],
            vec!["http://b.test/announce?pk=1"],
        ],
        _ => vec![vec![
            "http://b.test/announce?pk=2",
            "udp://c.test:6969/announce",
        ]],
    }
}

struct Outcome {
    lab: Lab,
    shared: Rc<RefCell<Shared>>,
    crash_marks: Vec<usize>,
    relaxed: bool,
}

fn run_ops(ops: &[Op]) -> Outcome {
    let shared = Rc::new(RefCell::new(Shared::default()));
    let mut lab = Lab::new(responder(shared.clone()));
    let mut eng = Engine {
        running: [false; 3],
        left: [SIZE; 3],
        up: [0; 3],
        down: [0; 3],
    };
    for i in 0..3u8 {
        let t: Vec<Vec<&str>> = tiers_for(i);
        let tiers: Vec<&[&str]> = t.iter().map(|v| v.as_slice()).collect();
        lab.add(spec(u32::from(i) + 1, &tiers));
    }
    let mut crash_marks = vec![];
    let mut relaxed = false;
    let push = |lab: &mut Lab, eng: &Engine, i: usize| {
        lab.stats(i as u32 + 1, eng.up[i], eng.down[i], eng.left[i], SIZE);
        shared
            .borrow_mut()
            .session_up
            .insert(ih(i as u32 + 1), eng.up[i]);
    };
    for op in ops {
        match *op {
            Op::Adv(s) => lab.run_for(Duration::from_secs(u64::from(s))),
            Op::Sleep(s) => {
                lab.sleep(Duration::from_secs(u64::from(s)));
            }
            Op::Start(i) | Op::Resume(i) => {
                let i = usize::from(i);
                if !eng.running[i] {
                    eng.running[i] = true;
                    eng.up[i] = 0;
                    eng.down[i] = 0;
                    push(&mut lab, &eng, i);
                    lab.ev(i as u32 + 1, TorrentEvent::Start);
                }
            }
            Op::Stop(i) | Op::Pause(i) => {
                let i = usize::from(i);
                if eng.running[i] {
                    eng.running[i] = false;
                    push(&mut lab, &eng, i);
                    lab.ev(i as u32 + 1, TorrentEvent::Stop);
                }
            }
            Op::Complete(i) => {
                let i = usize::from(i);
                if eng.running[i] && eng.left[i] > 0 {
                    eng.down[i] += eng.left[i];
                    eng.left[i] = 0;
                    push(&mut lab, &eng, i);
                    lab.ev(i as u32 + 1, TorrentEvent::Complete);
                }
            }
            Op::Progress(i, n) => {
                let i = usize::from(i);
                if eng.running[i] {
                    eng.up[i] += u64::from(n);
                    let dl = (u64::from(n) / 3).min(eng.left[i]);
                    eng.down[i] += dl;
                    eng.left[i] -= dl;
                    push(&mut lab, &eng, i);
                    if eng.left[i] == 0 && dl > 0 {
                        lab.ev(i as u32 + 1, TorrentEvent::Complete);
                    }
                }
            }
            Op::Recheck(i) => {
                let i = usize::from(i);
                if eng.running[i] && eng.left[i] == 0 {
                    eng.left[i] = 100;
                    push(&mut lab, &eng, i);
                }
            }
            Op::Manual(i) => {
                relaxed = true;
                lab.manual(u32::from(i) + 1);
            }
            Op::Network => {
                relaxed = true;
                lab.network();
            }
            Op::Wake => {
                lab.wake();
            }
            Op::Crash => {
                crash_marks.push(lab.sent.len());
                lab.crash();
                for i in 0..3 {
                    eng.up[i] = 0;
                    eng.down[i] = 0;
                }
                for i in 0..3 {
                    if eng.running[i] {
                        push(&mut lab, &eng, i);
                        lab.ev(i as u32 + 1, TorrentEvent::Start);
                    }
                }
            }
            Op::Mode(h, m) => {
                shared.borrow_mut().modes[usize::from(h)] = Some(mode_from(m));
            }
        }
    }
    // let everything settle, then stop
    lab.run_for(Duration::from_secs(10));
    Outcome {
        lab,
        shared,
        crash_marks,
        relaxed,
    }
}

fn check_outcome(o: &Outcome) -> Result<(), String> {
    let sh = o.shared.borrow();
    let sent = &o.lab.sent;
    let mut view = View::new();
    let mut crash_iter = o.crash_marks.iter().peekable();
    struct KeyTrace {
        last_ok: Option<(u64, u64, Option<u64>)>, // deliver ms, interval s, min s
        fail_until: u64,                          // earliest ms for the next non-stopped send
        /// Failure time (ms) of a host-level failure with no server hint: a sibling's success on the
        /// host may pull the retry forward to 30 s after it.
        pullable: Option<u64>,
    }
    let mut keys: HashMap<(InfoHash20, u16), KeyTrace> = HashMap::new();
    let mut succ: HashMap<String, Vec<u64>> = HashMap::new();
    let mut inflight: HashMap<&str, Vec<(u64, u64)>> = HashMap::new();
    for (idx, x) in sent.iter().enumerate() {
        while crash_iter.peek().is_some_and(|m| **m == idx) {
            crash_iter.next();
            view.crash();
            keys.clear();
            succ.clear();
            inflight.clear();
        }
        let info = sh.info.get(&x.n).ok_or("no info")?;
        view.check(x, SIZE, info.ok.is_some());
        if info.rejected {
            view.rejected(x);
        }
        let kt = keys.entry((x.ih, x.slot)).or_insert(KeyTrace {
            last_ok: None,
            fail_until: 0,
            pullable: None,
        });
        // interval rule
        if x.p.event.is_none() {
            let (at, iv, min) = kt
                .last_ok
                .ok_or_else(|| format!("#{}: regular announce without a prior success", x.n))?;
            let gap = if o.relaxed {
                min.unwrap_or(300).min(iv)
            } else {
                iv.max(min.unwrap_or(0))
            };
            if x.t.0 < at + gap * 1000 {
                return Err(format!(
                    "#{} regular announce {} ms after a success (interval {iv} min {min:?}, required {gap} s)",
                    x.n,
                    x.t.0 - at
                ));
            }
        }
        // backoff rule (rejections are halts, handled by the user-action rule)
        // A success on the host after this failure (delivered before this send) may have pulled it forward.
        let pulled = kt.pullable.is_some_and(|f| succ.get(&x.host).is_some_and(|v| v.iter().any(|t| *t >= f && *t < x.t.0)));
        let until = if pulled { kt.fail_until.min(kt.pullable.unwrap_or(0) + 30_000) } else { kt.fail_until };
        if x.p.event != Some(Event::Stopped) && x.t.0 < until {
            return Err(format!(
                "#{} {:?} sent {} ms before the backoff ended",
                x.n,
                x.p.event,
                until - x.t.0
            ));
        }
        if let Some((iv, min)) = info.ok {
            if x.p.event != Some(Event::Stopped) {
                kt.last_ok = Some((x.t.0 + info.latency.as_millis() as u64, iv, min));
            }
            kt.fail_until = 0;
            kt.pullable = None;
        } else if !info.rejected {
            let deliver = x.t.0
                + if info.hang {
                    60_000
                } else {
                    info.latency.as_millis() as u64
                };
            let wait = if info.min_wait > Duration::ZERO {
                info.min_wait.as_millis() as u64
            } else {
                60_000
            };
            if x.p.event != Some(Event::Stopped) {
                kt.fail_until = deliver + wait;
                // README: a network change may shorten a network-class backoff,
                // but never below 30 s after the previous attempt.
                let network_class =
                    matches!(info.min_wait, Duration::ZERO) && info.http_status.is_none();
                if o.relaxed && network_class {
                    kt.fail_until = x.t.0 + 30_000;
                }
                // A user's ask (relaxed runs) beats Seedwell's own backoff, 10 s after the attempt, but
                // never what the tracker itself demanded (a hint).
                if o.relaxed && info.min_wait == Duration::ZERO {
                    kt.fail_until = kt.fail_until.min(x.t.0 + 10_000);
                }
                kt.pullable = (info.min_wait == Duration::ZERO && (network_class || info.http_status.is_some_and(|s| s >= 500))).then_some(deliver);
            }
        } else {
            kt.fail_until = 0;
        }
        if info.ok.is_some() && x.p.event != Some(Event::Stopped) {
            succ.entry(x.host.clone()).or_default().push(x.t.0 + info.latency.as_millis() as u64);
        }
        // concurrency per host: at most 2 unfinished requests
        let end = x.t.0
            + if info.hang {
                60_000
            } else {
                info.latency.as_millis() as u64
            };
        let list = inflight.entry(x.host.as_str()).or_default();
        list.retain(|(_, e)| *e > x.t.0);
        list.push((x.t.0, end));
        if list.len() > 2 {
            return Err(format!(
                "#{} more than 2 requests in flight to {}",
                x.n, x.host
            ));
        }
    }
    // token bucket windows (a crash resets the gate; the new gate starts empty,
    // so the sliding window over the whole log is still an upper bound)
    for host in HOSTS {
        for w in [1u64, 2, 5, 30, 300, 3600] {
            let m = max_in_window(sent, host, Duration::from_secs(w));
            if m as u64 > 3 + w {
                return Err(format!("{host}: {m} requests in a {w} s window"));
            }
        }
    }
    if !view.violations.is_empty() {
        return Err(view.violations.join("\n"));
    }
    // every URL we contacted is one of the torrent's own
    for x in sent {
        let i = if x.ih[..4] == 1u32.to_be_bytes() {
            0
        } else if x.ih[..4] == 2u32.to_be_bytes() {
            1
        } else {
            2
        };
        let allowed: Vec<String> = tiers_for(i as u8)
            .into_iter()
            .flatten()
            .map(str::to_owned)
            .collect();
        if !allowed.iter().any(|a| a.eq_ignore_ascii_case(&x.url)) {
            return Err(format!(
                "contacted {} which is not one of {allowed:?}",
                x.url
            ));
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(400), max_shrink_iters: 2000, ..ProptestConfig::default() })]

    /// Strict mode: no manual re-announce, no network change, so every regular
    /// announce must be >= max(interval, min interval) after the last success.
    #[test]
    fn strict_schedule_and_grammar(ops in proptest::collection::vec(op_strategy(false), 1..80)) {
        let o = run_ops(&ops);
        check_outcome(&o).map_err(TestCaseError::fail)?;
    }

    /// With manual re-announce / network / wake pressure: >= min interval.
    #[test]
    fn relaxed_schedule_and_grammar(ops in proptest::collection::vec(op_strategy(true), 1..80)) {
        let o = run_ops(&ops);
        check_outcome(&o).map_err(TestCaseError::fail)?;
    }
}

#[test]
fn deterministic_smoke_of_the_property_driver() {
    let ops = vec![
        Op::Start(0),
        Op::Start(1),
        Op::Start(2),
        Op::Adv(4000),
        Op::Mode(0, 2),
        Op::Adv(9000),
        Op::Complete(1),
        Op::Mode(0, 0),
        Op::Sleep(30 * 3600),
        Op::Crash,
        Op::Adv(4000),
        Op::Pause(1),
        Op::Adv(100),
    ];
    let o = run_ops(&ops);
    check_outcome(&o).unwrap();
    assert!(o.lab.sent.len() > 10);
    let _ = Millis::ZERO;
}

/// The checker must be able to fail: feed it logs a banned client would produce.
#[test]
fn the_tracker_side_checker_flags_known_bad_sequences() {
    use trs_tracker::AnnounceParams;
    let mk = |n: usize, t: u64, ev: Option<Event>, left: u64| Sent {
        n,
        t: Millis(t),
        ih: ih(1),
        slot: 0,
        url: "http://a.test/announce".into(),
        host: "a.test".into(),
        p: AnnounceParams {
            info_hash: ih(1),
            peer_id: [1; 20],
            port: 1,
            uploaded: 0,
            downloaded: 0,
            corrupt: 0,
            left,
            key: 1,
            event: ev,
            numwant: if ev == Some(Event::Stopped) { 0 } else { 50 },
            tracker_id: None,
            ipv4: None,
            ipv6: None,
            crypto: Default::default(),
        },
    };
    let run = |seq: Vec<(Option<Event>, u64, bool)>| {
        let mut v = View::new();
        for (i, (ev, left, ok)) in seq.into_iter().enumerate() {
            v.check(&mk(i, i as u64 * 1000, ev, left), 1000, ok);
        }
        v.violations.len()
    };
    use Event::*;
    assert_eq!(
        run(vec![
            (Some(Started), 5, true),
            (None, 5, true),
            (Some(Stopped), 5, true)
        ]),
        0
    );
    assert_eq!(
        run(vec![(Some(Started), 5, true), (Some(Started), 5, true)]),
        1,
        "repeated started"
    );
    assert_eq!(run(vec![(Some(Stopped), 5, true)]), 1, "stopped first");
    assert_eq!(run(vec![(None, 5, true)]), 1, "regular first");
    assert_eq!(
        run(vec![
            (Some(Started), 5, true),
            (Some(Stopped), 5, true),
            (None, 5, true)
        ]),
        1,
        "regular after stopped"
    );
    assert_eq!(
        run(vec![
            (Some(Started), 5, true),
            (Some(Completed), 0, true),
            (Some(Completed), 0, true)
        ]),
        1,
        "second completed"
    );
    assert_eq!(
        run(vec![(Some(Started), 5, true), (Some(Completed), 3, true)]),
        1,
        "completed with left"
    );
}
