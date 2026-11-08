//! Adversarial audit: whole-hub property tests with an independent tracker-side
//! checker (`adversarial_support::View` plus the interval/backoff/concurrency/
//! bucket rules below, all derived from SPEC 7B/7C, not from the implementation).

use crate::adversarial_support;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adversarial_support::*;
use proptest::prelude::*;
use seedwell_tracker::outage::NetEvent;
use seedwell_tracker::response::AnnounceError;
use seedwell_tracker::{AnnounceParams, Event, InfoHash20, Millis, TorrentEvent, clamp_timing};

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
    /// A flapping interface: a network change and a wake in turn every 10 s, this many times.
    Flap(u8),
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
            1 => (1u8..90).prop_map(Op::Flap),
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
                    seedwell_tracker::response::ParseError::NotBencode,
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
    /// The user asked the tracker again at least once (`Op::Manual`).
    relaxed: bool,
    /// When the user asked the tracker again (ms), and for which torrent: each ask beats that
    /// torrent's own backoff, 10 s after its failed attempt (SPEC 7C.4).
    manuals: Vec<(u64, InfoHash20)>,
    /// When a network change or a wake reached the hub (ms), in order: each may pull a failed
    /// announce's network-class retry in (SPEC 7C.4, 7C.4b rule 6).
    net_events: Vec<u64>,
    /// When the hub declared an outage (ms). Its rollback may forgive a failure series whole,
    /// so the next failure begins a new series with an early try of its own.
    outages: Vec<u64>,
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
    let mut manuals = vec![];
    let mut net_events = vec![];
    let mut outages = vec![];
    let mut note_outages = |lab: &mut Lab| {
        for e in lab.hub().take_net_events() {
            if let NetEvent::Offline { at, .. } = e {
                outages.push(at.0);
            }
        }
    };
    let push = |lab: &mut Lab, eng: &Engine, i: usize| {
        lab.stats(i as u32 + 1, eng.up[i], eng.down[i], eng.left[i], SIZE);
        shared
            .borrow_mut()
            .session_up
            .insert(ih(i as u32 + 1), eng.up[i]);
    };
    for op in ops {
        if matches!(op, Op::Crash) {
            note_outages(&mut lab);
        }
        match *op {
            Op::Adv(s) => lab.run_for(Duration::from_secs(u64::from(s))),
            Op::Sleep(s) => {
                // `Lab::sleep` wakes the hub when the time has jumped.
                net_events.push((lab.now + Duration::from_secs(u64::from(s))).0);
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
                manuals.push((lab.now.0, ih(u32::from(i) + 1)));
                lab.manual(u32::from(i) + 1);
            }
            Op::Network => {
                net_events.push(lab.now.0);
                lab.network();
            }
            Op::Wake => {
                net_events.push(lab.now.0);
                lab.wake();
            }
            Op::Flap(n) => {
                for k in 0..n {
                    net_events.push(lab.now.0);
                    if k % 2 == 0 {
                        lab.network();
                    } else {
                        lab.wake();
                    }
                    lab.run_for(Duration::from_secs(10));
                }
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
        note_outages(&mut lab);
    }
    // let everything settle, then stop
    lab.run_for(Duration::from_secs(10));
    note_outages(&mut lab);
    Outcome {
        lab,
        shared,
        crash_marks,
        relaxed,
        manuals,
        net_events,
        outages,
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
        /// Attempt time (ms) of a network-class failure (timeout, hang, no server hint, no HTTP
        /// status): a network change or wake after it may pull the retry in, but never to within
        /// 30 s of that attempt (SPEC 7C.4, 7C.4b rule 6; README "Wake/network-change").
        net_failed_at: Option<u64>,
        /// When a send of this failure series (failures since the last success) could only
        /// have come from a network change or wake pulling it in: that happens once per series,
        /// a second one is a flapping network hammering a dead tracker.
        net_pulled_at: Option<u64>,
        /// Attempt time (ms) of a failure without a server hint: a user's ask for this torrent
        /// after it may bring the retry to 10 s after that attempt.
        manual_from: Option<u64>,
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
            net_failed_at: None,
            net_pulled_at: None,
            manual_from: None,
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
        let mut until_without_net = if pulled { kt.fail_until.min(kt.pullable.unwrap_or(0) + 30_000) } else { kt.fail_until };
        // A user's ask for this torrent after the failure beats Seedwell's own backoff.
        if let Some(f) = kt.manual_from
            && o.manuals.iter().any(|(t, i)| *i == x.ih && *t >= f && *t <= x.t.0)
        {
            until_without_net = until_without_net.min(f + 10_000);
        }
        let mut until = until_without_net;
        // A network change or wake between that failure and this send may have pulled it in.
        if let Some(f) = kt.net_failed_at
            && o.net_events.iter().any(|w| *w >= f && *w <= x.t.0)
        {
            until = until.min(f + 30_000);
        }
        if x.p.event != Some(Event::Stopped) && x.t.0 < until {
            return Err(format!(
                "#{} {:?} sent {} ms before the backoff ended",
                x.n,
                x.p.event,
                until - x.t.0
            ));
        }
        // Only a network change or wake explains this send: once per failure series (SPEC 7C.4).
        // An outage declared since the last such pull may have forgiven that series.
        if x.p.event != Some(Event::Stopped) && x.t.0 < until_without_net {
            if let Some(p) = kt.net_pulled_at
                && !o.outages.iter().any(|a| *a > p && *a <= x.t.0)
            {
                return Err(format!(
                    "#{} {:?}: a network change or wake pulled a retry in again {} ms after the last pull of the same failure series",
                    x.n,
                    x.p.event,
                    x.t.0 - p
                ));
            }
            kt.net_pulled_at = Some(x.t.0);
        }
        if let Some((iv, min)) = info.ok {
            if x.p.event != Some(Event::Stopped) {
                kt.last_ok = Some((x.t.0 + info.latency.as_millis() as u64, iv, min));
            }
            kt.fail_until = 0;
            kt.pullable = None;
            kt.net_failed_at = None;
            kt.net_pulled_at = None;
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
                // README: a network change or wake may shorten a network-class backoff,
                // but never below 30 s after the previous attempt (checked at the next send).
                let network_class =
                    matches!(info.min_wait, Duration::ZERO) && info.http_status.is_none();
                kt.net_failed_at = network_class.then_some(x.t.0);
                // A user's ask beats Seedwell's own backoff, 10 s after the attempt, but never what
                // the tracker itself demanded (a hint): checked at the next send.
                kt.manual_from = (info.min_wait == Duration::ZERO).then_some(x.t.0);
                kt.pullable = (info.min_wait == Duration::ZERO && (network_class || info.http_status.is_some_and(|s| s >= 500))).then_some(deliver);
            }
        } else {
            kt.fail_until = 0;
            kt.net_failed_at = None;
            kt.net_pulled_at = None;
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

    /// Strict mode: no manual re-announce, no explicit network change or wake (a `Sleep` still
    /// wakes the hub), so every regular announce must be >= max(interval, min interval) after the
    /// last success.
    #[test]
    fn strict_schedule_and_grammar(ops in proptest::collection::vec(op_strategy(false), 1..80)) {
        let o = run_ops(&ops);
        check_outcome(&o).map_err(TestCaseError::fail)?;
    }

    /// With manual re-announce / network / wake pressure. A manual re-announce may come at the
    /// min interval; a network change or wake moves no regular announce and only pulls a
    /// network-class retry in, never within 30 s of its attempt (SPEC 7C.4, 7C.4b rule 6).
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
    let mk = fake_sent;
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

/// One request of torrent 1 to `a.test`, as the tracker would log it.
fn fake_sent(n: usize, t: u64, ev: Option<Event>, left: u64) -> Sent {
    Sent {
        n,
        t: Millis(t),
        ih: ih(1),
        slot: 0,
        url: "http://a.test/announce?pk=0".into(),
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
    }
}

/// Found by the PC's run (`proptest-regressions/adversarial_props.txt`): a wake pulled a
/// resumed torrent's timed-out `started` in from a 38-minute backoff to 9 s after the wake, and
/// the checker called it "sent before the backoff ended" because it let only an explicit network
/// change pull a retry in. The scheduler was right (SPEC 7C.4: sleep/wake reschedule failed
/// announces; 7C.4b rule 6). This targets the two ways that pull could hammer a tracker: a retry
/// within 30 s of the failed attempt, and a wake cutting short a tracker's own `Retry-After`.
#[test]
fn after_a_wake_a_resumed_torrents_started_retry_still_waits_30_s_and_any_retry_after() {
    // Twenty torrents, so twenty draws of the wake jitter: one draw can land past 30 s by luck.
    for n in 1..=20 {
        // A tracker that times out 1 s after each request: without the 30 s floor the wake's
        // quick retry (1-16 s from the wake) would land well inside it.
        let (attempt, retry) = woken_after_failures(n, 4, Box::new(|_| vec![(d(1), timeout())]));
        let retry = retry.expect("retried");
        assert_eq!(retry.event(), Some(Event::Started));
        assert!(
            retry.t.0 >= attempt + 30_000,
            "torrent {n}: retried {} ms after the failed attempt; a wake never pulls within 30 s",
            retry.t.0 - attempt
        );
        // The backoff after four failures is 8 min or more: the wake pulled the retry in, as
        // SPEC 7C.4 promises a download whose network came back.
        assert!(
            retry.t.0 < attempt + 120_000,
            "torrent {n}: the wake did not pull the failed started in ({} s)",
            (retry.t.0 - attempt) / 1000
        );

        // A tracker that answers 429 with Retry-After 600 s: a wake does not move that wait.
        let throttle: Responder = Box::new(|_| {
            vec![(
                d(1),
                Err(AnnounceError::Http {
                    status: 429,
                    retry_after: Some(d(600)),
                }),
            )]
        });
        let (attempt, retry) = woken_after_failures(n, 1, throttle);
        let retry = retry.expect("retried after the Retry-After");
        assert!(
            retry.t.0 >= attempt + 600_000,
            "torrent {n}: retried {} s after a Retry-After of 600 s",
            (retry.t.0 - attempt) / 1000
        );
    }
}

/// Torrent `n` (one tracker) is resumed after a long sleep; once its `started` has failed
/// `failures` times, the Mac wakes 2 s after the last failure was answered. Returns the last
/// failed attempt's time and the first request after the wake (within 15 min).
fn woken_after_failures(n: u32, failures: usize, responder: Responder) -> (u64, Option<Sent>) {
    let mut lab = Lab::new(responder);
    lab.add(spec(n, &[&["http://a.test/announce"]]));
    lab.sleep(d(30 * 3600));
    lab.stats(n, 0, 0, SIZE, SIZE);
    lab.ev(n, TorrentEvent::Resume);
    while lab.sent.len() < failures {
        lab.run_for(d(1));
    }
    let attempt = lab.sent[failures - 1].t.0;
    lab.run_for(d(2)); // its answer (1 s) has arrived
    assert_eq!(lab.sent.len(), failures, "torrent {n}: nothing before the backoff");
    assert!(lab.now.0 < attempt + 30_000);
    lab.wake();
    lab.run_for(d(900));
    (attempt, lab.sent.get(failures).cloned())
}

/// The checker's wake rule must still be able to fail: a wake licenses a timed-out retry from
/// 30 s after its attempt, not sooner, not before a tracker's Retry-After, and a wake that came
/// before the failure licenses nothing.
#[test]
fn the_checker_flags_a_wake_pulled_retry_too_soon_or_before_a_retry_after() {
    let timed_out = Info {
        ok: None,
        rejected: false,
        latency: d(1),
        hang: false,
        min_wait: Duration::ZERO,
        http_status: None,
    };
    let throttled = Info {
        min_wait: d(600),
        http_status: Some(429),
        ..timed_out.clone()
    };
    let check = |first: &Info, retry_at: u64, wake_at: u64| {
        let shared = Rc::new(RefCell::new(Shared::default()));
        shared.borrow_mut().info.insert(0, first.clone());
        shared.borrow_mut().info.insert(1, timed_out.clone());
        let mut lab = Lab::new(Box::new(|_| vec![]));
        lab.sent = vec![
            fake_sent(0, 1_000, Some(Event::Started), SIZE),
            fake_sent(1, retry_at, Some(Event::Started), SIZE),
        ];
        check_outcome(&Outcome {
            lab,
            shared,
            crash_marks: vec![],
            relaxed: false,
            manuals: vec![],
            net_events: vec![wake_at],
            outages: vec![],
        })
    };
    check(&timed_out, 31_000, 2_000).expect("30 s after the attempt, after a wake");
    assert!(check(&timed_out, 20_000, 2_000).is_err(), "19 s after the attempt");
    assert!(check(&timed_out, 31_000, 500).is_err(), "the wake came before the failure");
    assert!(check(&throttled, 31_000, 2_000).is_err(), "inside a Retry-After");
}

/// The checker's once-per-series rule must be able to fail: two network-pulled retries of one
/// failure series are flagged, while an outage declared between them (its rollback may forgive
/// the series) licenses the second.
#[test]
fn the_checker_flags_a_second_network_pull_in_one_failure_series() {
    let timed_out = Info {
        ok: None,
        rejected: false,
        latency: d(1),
        hang: false,
        min_wait: Duration::ZERO,
        http_status: None,
    };
    let check = |outages: Vec<u64>| {
        let shared = Rc::new(RefCell::new(Shared::default()));
        for n in 0..3 {
            shared.borrow_mut().info.insert(n, timed_out.clone());
        }
        let mut lab = Lab::new(Box::new(|_| vec![]));
        // Each retry 31 s after the failed attempt before it: only a network change or wake
        // (at 2 s and 33 s) explains either one.
        lab.sent = vec![
            fake_sent(0, 1_000, Some(Event::Started), SIZE),
            fake_sent(1, 32_000, Some(Event::Started), SIZE),
            fake_sent(2, 63_000, Some(Event::Started), SIZE),
        ];
        check_outcome(&Outcome {
            lab,
            shared,
            crash_marks: vec![],
            relaxed: false,
            manuals: vec![],
            net_events: vec![2_000, 33_000],
            outages,
        })
    };
    let err = check(vec![]).expect_err("a second pull in one failure series");
    assert!(err.contains("pulled a retry in again"), "{err}");
    check(vec![40_000]).expect("an outage between the pulls may have begun a new series");
}

/// A network that keeps flapping (Wi-Fi dropping, a VPN reconnecting: a network change or a
/// wake every 10 s) pulls a dead tracker's retry in once, then the backoff grows. The failure
/// this catches: every network change or wake pulling a failed network-class retry in again,
/// so a flapping interface retried a dead tracker every 30-46 s for as long as it flapped, held
/// back only by the per-host limit (SPEC 7C.4, 7C.4b rule 6). Network changes and wakes share
/// the one pull; a success ends the series, and the next outage gets its own early try.
#[test]
fn a_network_that_keeps_flapping_pulls_a_dead_trackers_retry_in_once_then_the_backoff_grows() {
    let up = Rc::new(Cell::new(false));
    let u = up.clone();
    let mut lab = Lab::new(Box::new(move |_| {
        vec![(d(1), if u.get() { ok(1800, None) } else { timeout() })]
    }));
    lab.add(spec(1, &[&["http://a.test/announce"]]));
    lab.stats(1, 0, 0, SIZE, SIZE);
    lab.ev(1, TorrentEvent::Start);
    // Fails until `n` requests were sent, with no network change; then its answer arrives.
    let fail_until_sent = |lab: &mut Lab, n: usize| {
        while lab.sent.len() < n {
            lab.run_for(d(1));
        }
        lab.run_for(d(2));
        assert_eq!(lab.sent.len(), n, "nothing before the backoff");
    };
    // Alternating network changes and wakes, every 10 s.
    let flap = |lab: &mut Lab, secs: u64| {
        for i in 0..secs / 10 {
            if i % 2 == 0 {
                lab.network();
            } else {
                lab.wake();
            }
            lab.run_for(d(10));
        }
    };
    let gaps_s = |lab: &Lab, from: usize| -> Vec<u64> {
        lab.sent[from..].windows(2).map(|w| (w[1].t.0 - w[0].t.0) / 1000).collect()
    };

    // Four failures: the downloading backoff is 8 min or more. Then two hours of flapping.
    fail_until_sent(&mut lab, 4);
    flap(&mut lab, 2 * 3600);
    let gaps = gaps_s(&lab, 3);
    assert!(gaps.len() >= 3, "retries during two hours of flapping: {gaps:?}");
    // One early "are we back?" try: 30 s after the failed attempt at the earliest, well inside
    // the 8 min backoff.
    assert!((30..120).contains(&gaps[0]), "the first flap pulls the retry in: {gaps:?}");
    // Then the normal growing backoff (16 min, 32 min, capped at 1 h), whatever the network does.
    for (i, g) in gaps[1..].iter().enumerate() {
        let floor = ((16 * 60u64) << i).min(3600);
        assert!(*g >= floor, "retry {} came {g} s after the one before, under the {floor} s backoff: {gaps:?}", i + 2);
    }

    // The tracker comes back: a success ends the series.
    up.set(true);
    let before = lab.sent.len();
    while lab.sent.len() == before {
        lab.run_for(d(10));
    }
    lab.run_for(d(2));
    up.set(false);
    // It dies again: the regular announce and three retries fail, then the network flaps. The
    // new series gets its one early try, and only one.
    let first = lab.sent.len();
    fail_until_sent(&mut lab, first + 4);
    flap(&mut lab, 30 * 60);
    let gaps = gaps_s(&lab, first + 3);
    assert!(!gaps.is_empty() && (30..120).contains(&gaps[0]), "a new series is pulled in once: {gaps:?}");
    assert!(gaps[1..].iter().all(|g| *g >= 16 * 60), "and once only: {gaps:?}");
}
