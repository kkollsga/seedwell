//! Multi-tier independence (SPEC 7C.4b). Soak regression: Ubuntu has two tiers, the first one
//! answers 503 to everything, the second works with `interval = 1800`. The second tracker was
//! announced to (`stopped`, then `started` again) in the middle of its interval because
//! the first tracker failed or recovered. A tracker's schedule must depend on its own
//! answers only; tiers never start, stop or reschedule each other.

use crate::adversarial_support;

use std::cell::RefCell;
use std::rc::Rc;

use adversarial_support::*;
use trs_tracker::{Event, TorrentEvent, TorrentRole};

const T1: &str = "http://t1.test/announce?pk=1";
const T2: &str = "http://t2.test/announce?pk=1";

/// One answered request, as the fake tracker saw it.
#[derive(Clone, Debug)]
struct Log {
    t: u64, // seconds
    host: String,
    event: Option<Event>,
    ok: bool,
    interval: u64,
}

type Shared = Rc<RefCell<Vec<Log>>>;

/// `behave(host, t_secs, n_th_request_to_host) -> answer` (`None` = 503).
fn lab_with(
    mut behave: impl FnMut(&str, u64, usize) -> Option<u64> + 'static,
) -> (Lab, Shared) {
    let log: Shared = Rc::default();
    let l2 = log.clone();
    let lab = Lab::new(Box::new(move |s| {
        let t = s.t.0 / 1000;
        let n = l2.borrow().iter().filter(|x| x.host == s.host).count();
        let a = behave(&s.host, t, n);
        l2.borrow_mut().push(Log {
            t,
            host: s.host.clone(),
            event: s.p.event,
            ok: a.is_some(),
            interval: a.unwrap_or(0),
        });
        vec![(d(1), a.map_or_else(|| http_status(503), |i| ok(i, None)))]
    }));
    (lab, log)
}

fn start(lab: &mut Lab, tiers: &[&[&str]], role: TorrentRole) {
    lab.add(spec(1, tiers));
    let (left, size) = if role == TorrentRole::Downloading { (10, 10) } else { (0, 10) };
    lab.stats(1, 0, 0, left, size);
    let now = lab.now;
    lab.hub().set_role(&ih(1), role, now);
    lab.ev(1, TorrentEvent::Start);
}

/// The rules a tracker operator would enforce, per host:
/// * after a success, nothing but the regular announce at `interval` (no event, no early one);
///   the only exception is a final `stopped`;
/// * after a failure, the next request waits for the backoff floor (`floor` seconds).
fn check_host(log: &[Log], host: &str, floor: u64) {
    let mine: Vec<&Log> = log.iter().filter(|x| x.host == host).collect();
    let dump = || mine.iter().map(|x| (x.t, x.event, x.ok)).collect::<Vec<_>>();
    for w in mine.windows(2) {
        let (a, b) = (w[0], w[1]);
        if a.ok && a.event != Some(Event::Stopped) {
            if b.event == Some(Event::Stopped) {
                continue;
            }
            assert!(b.event.is_none() || b.event == Some(Event::Completed), "{host}: {:?} after success: {:?}", b.event, dump());
            assert!(b.t >= a.t + a.interval, "{host}: early announce at {} after success at {} (interval {}): {:?}", b.t, a.t, a.interval, dump());
        } else if !a.ok && a.event != Some(Event::Stopped) && b.event != Some(Event::Stopped) {
            assert!(b.t >= a.t + floor, "{host}: retry at {} only {} s after failure at {}: {:?}", b.t, b.t - a.t, a.t, dump());
        }
    }
    // a second `started` while the first one is acknowledged is only legal behind a `stopped`
    let mut acked = false;
    for x in &mine {
        match x.event {
            Some(Event::Stopped) => acked = false,
            Some(Event::Started) => {
                assert!(!acked, "{host}: second started in one session: {:?}", dump());
                acked = x.ok;
            }
            _ => {}
        }
    }
}

fn count(log: &[Log], host: &str, ev: Option<Event>) -> usize {
    log.iter().filter(|x| x.host == host && x.event == ev).count()
}

#[test]
fn soak_two_tiers_first_always_503_second_recovers_first() {
    for role in [TorrentRole::Seeding, TorrentRole::Downloading] {
        let floor = if role == TorrentRole::Seeding { 890 } else { 55 };
        for down in [0u64, 400, 2000] {
            let (mut lab, log) = lab_with(move |host, t, _| {
                if host == "t1.test" || t < down { None } else { Some(1800) }
            });
            start(&mut lab, &[&[T1], &[T2]], role);
            lab.run_for(d(24 * 3600));
            let log = log.borrow();
            check_host(&log, "t1.test", floor);
            check_host(&log, "t2.test", floor);
            assert_eq!(count(&log, "t2.test", Some(Event::Stopped)), 0, "{role:?} {down}");
            assert!(count(&log, "t2.test", Some(Event::Started)) >= 1);
            // once t2 works it announces exactly once per interval: 24 h / 30 min, plus the
            // startup attempts
            let regular = log.iter().filter(|x| x.host == "t2.test" && x.ok).count();
            assert!(regular <= 49 + 1, "{regular}");
            assert!(count(&log, "t1.test", None) == 0, "t1 never succeeded, so never a regular announce");
        }
    }
}

#[test]
fn soak_first_tier_flaps_second_tier_is_not_touched() {
    // Tier 1: 503, 503, then a success, then 503 forever, then one more success. Every state
    // change of tier 1 happens while tier 2 sits in the middle of its interval.
    for role in [TorrentRole::Seeding, TorrentRole::Downloading] {
        let floor = if role == TorrentRole::Seeding { 890 } else { 55 };
        let (mut lab, log) = lab_with(|host, t, n| match host {
            "t1.test" => (n == 3 || (7_000..7_100).contains(&t)).then_some(1800),
            _ => (t >= 400).then_some(1800),
        });
        start(&mut lab, &[&[T1], &[T2]], role);
        lab.run_for(d(24 * 3600));
        let log = log.borrow();
        check_host(&log, "t1.test", floor);
        check_host(&log, "t2.test", floor);
        assert_eq!(count(&log, "t2.test", Some(Event::Stopped)), 0);
        assert!(count(&log, "t2.test", Some(Event::Started)) <= 5, "started only while t2 itself fails");
    }
}

#[test]
fn healthy_first_tier_starts_every_tier_independently() {
    // The Transmission model: both tiers announce, each on its own interval.
    let (mut lab, log) = lab_with(|host, _, _| Some(if host == "t1.test" { 1800 } else { 3600 }));
    start(&mut lab, &[&[T1], &[T2]], TorrentRole::Downloading);
    lab.run_for(d(12 * 3600));
    let log = log.borrow();
    check_host(&log, "t1.test", 55);
    check_host(&log, "t2.test", 55);
    assert_eq!(count(&log, "t1.test", Some(Event::Started)), 1);
    assert_eq!(count(&log, "t2.test", Some(Event::Started)), 1);
    assert!(count(&log, "t1.test", None) >= 22 && count(&log, "t1.test", None) <= 24);
    assert!(count(&log, "t2.test", None) >= 11 && count(&log, "t2.test", None) <= 12);
}

#[test]
fn http_and_https_twins_in_two_tiers_are_one_tracker() {
    let (mut lab, log) = lab_with(|_, _, _| Some(1800));
    start(
        &mut lab,
        &[&["http://tl.test/announce?pk=1"], &["https://tl.test/announce?pk=1"]],
        TorrentRole::Seeding,
    );
    lab.run_for(d(24 * 3600));
    let log = log.borrow();
    assert!(log.len() <= 24 * 2 + 3, "{}", log.len());
    let urls = lab.sent.iter().map(|x| x.url.clone()).collect::<std::collections::HashSet<_>>();
    assert_eq!(urls.len(), 1, "{urls:?}");
}

#[test]
fn failover_inside_a_tier_respects_every_trackers_own_schedule() {
    // One tier [a, b]: a is down for a long while, b works. b is promoted and a is silently
    // closed (it never acknowledged `started`): no `stopped` to a failing tracker, no request
    // to a outside its backoff, and b keeps its interval whatever a does.
    for role in [TorrentRole::Seeding, TorrentRole::Downloading] {
        let floor = if role == TorrentRole::Seeding { 890 } else { 55 };
        let (mut lab, log) = lab_with(|host, t, _| match host {
            "a.test" => (9_000..9_100).contains(&t).then_some(1800),
            _ => Some(1800),
        });
        start(
            &mut lab,
            &[&["http://a.test/announce?pk=1", "http://b.test/announce?pk=1"]],
            role,
        );
        lab.run_for(d(24 * 3600));
        let log = log.borrow();
        check_host(&log, "a.test", floor);
        check_host(&log, "b.test", floor);
        assert_eq!(count(&log, "a.test", Some(Event::Stopped)), 0, "{:?}", lab.sent.len());
    }
}

#[test]
fn in_tier_failover_back_and_forth_never_moves_the_healthy_trackers_schedule() {
    // [a, b], b works, a is flaky. Whichever is active, a tracker that is mid-interval after a
    // success is not announced to because the other one failed or recovered.
    for role in [TorrentRole::Seeding, TorrentRole::Downloading] {
        let floor = if role == TorrentRole::Seeding { 890 } else { 55 };
        let (mut lab, log) = lab_with(|host, t, n| match host {
            "a.test" => (n % 3 == 2).then_some(1800),
            "b.test" => (t % 20_000 < 12_000 || t > 60_000).then_some(1800),
            _ => None,
        });
        start(
            &mut lab,
            &[&["http://a.test/announce?pk=1", "http://b.test/announce?pk=1"]],
            role,
        );
        lab.run_for(d(48 * 3600));
        let log = log.borrow();
        check_host(&log, "a.test", floor);
        check_host(&log, "b.test", floor);
    }
}

#[test]
fn dual_stack_pair_of_tiers_with_blackholed_v6_never_disturbs_v4() {
    // torrent.example (v4) and ipv6.torrent.example (v6-only, blackholed here): two tiers.
    let (mut lab, log) = lab_with(|host, _, _| (host == "t1.test").then_some(1800));
    start(&mut lab, &[&[T1], &["http://ipv6.t2.test/announce?pk=1"]], TorrentRole::Seeding);
    lab.run_for(d(24 * 3600));
    let log = log.borrow();
    check_host(&log, "t1.test", 890);
    check_host(&log, "ipv6.t2.test", 890);
    assert!(count(&log, "t1.test", None) <= 49);
    assert_eq!(count(&log, "t1.test", Some(Event::Started)), 1);
    assert_eq!(count(&log, "t1.test", Some(Event::Stopped)), 0);
    assert!(count(&log, "ipv6.t2.test", Some(Event::Started)) <= 8, "{}", count(&log, "ipv6.t2.test", Some(Event::Started)));
}

#[test]
fn quit_sends_one_stopped_per_tracker() {
    let (mut lab, log) = lab_with(|host, _, _| (host != "t1.test").then_some(1800));
    start(
        &mut lab,
        &[&[T1], &[T2], &["http://t3.test/announce?pk=1", "http://t4.test/announce?pk=1"]],
        TorrentRole::Seeding,
    );
    lab.run_for(d(2000));
    lab.quit(1);
    lab.run_for(d(600));
    let log = log.borrow();
    for h in ["t1.test", "t2.test", "t3.test"] {
        assert!(count(&log, h, Some(Event::Stopped)) <= 1, "{h}");
    }
    assert_eq!(count(&log, "t2.test", Some(Event::Stopped)), 1);
    assert_eq!(count(&log, "t3.test", Some(Event::Stopped)), 1);
    // t4 is the unused failover of t3's tier: never told anything
    assert_eq!(count(&log, "t4.test", Some(Event::Started)), 0);
    assert_eq!(count(&log, "t4.test", Some(Event::Stopped)), 0);
    assert!(lab.hub().is_quiescent(&ih(1)));
}

#[test]
fn set_port_and_set_crypto_send_nothing_in_any_tier() {
    use trs_tracker::Crypto;
    let (mut lab, log) = lab_with(|host, _, _| (host != "t1.test").then_some(1800));
    start(&mut lab, &[&[T1], &[T2]], TorrentRole::Seeding);
    lab.run_for(d(1000));
    let before = lab.sent.len();
    let now = lab.now;
    lab.hub().set_port(7000);
    lab.hub().set_crypto(Crypto::Require);
    lab.pump();
    assert_eq!(lab.sent.len(), before, "set_port/set_crypto must not send");
    lab.run_for(d(3 * 3600));
    let log = log.borrow();
    check_host(&log, "t1.test", 890);
    check_host(&log, "t2.test", 890);
    // the next regular announces carry the new port
    let later: Vec<_> = lab.sent.iter().filter(|x| x.t > now && x.host == "t2.test").collect();
    assert!(!later.is_empty());
    assert!(later.iter().all(|x| x.p.port == 7000));
}

#[test]
fn network_change_and_wake_do_not_touch_a_healthy_tier() {
    for role in [TorrentRole::Seeding, TorrentRole::Downloading] {
        let floor = if role == TorrentRole::Seeding { 890 } else { 55 };
        let (mut lab, log) = lab_with(|host, t, _| (host != "t1.test" && t >= 100).then_some(1800));
        start(&mut lab, &[&[T1], &[T2]], role);
        lab.run_for(d(1000));
        for _ in 0..5 {
            lab.run_for(d(30));
            lab.network();
            lab.pump();
            lab.run_for(d(30));
            lab.sleep(d(1));
        }
        lab.run_for(d(24 * 3600));
        let log = log.borrow();
        check_host(&log, "t1.test", floor.min(55));
        check_host(&log, "t2.test", floor.min(55));
        assert_eq!(count(&log, "t2.test", Some(Event::Stopped)), 0);
    }
}

#[test]
fn promotion_after_success_only_reorders_inside_the_tier() {
    // [a, b] and [c]: b is promoted over a. c (other tier) is unaffected.
    let (mut lab, log) = lab_with(|host, _, _| (host != "a.test").then_some(1800));
    start(
        &mut lab,
        &[&["http://a.test/announce?pk=1", "http://b.test/announce?pk=1"], &["http://c.test/announce?pk=1"]],
        TorrentRole::Downloading,
    );
    lab.run_for(d(6 * 3600));
    let log = log.borrow();
    for h in ["a.test", "b.test", "c.test"] {
        check_host(&log, h, 55);
    }
    assert_eq!(count(&log, "c.test", Some(Event::Started)), 1);
    assert_eq!(count(&log, "c.test", Some(Event::Stopped)), 0);
}

#[test]
fn tiers_of_one_tracker_profile_fold_into_one_stream_with_failover() {
    use trs_tracker::TrackerUrl;
    use trs_tracker::set::fold_tiers;
    // main.tl.test and alt.tl.test are two names of one tracker (one profile); other.test is not.
    let tiers = |fold: bool| {
        let raw: Vec<Vec<TrackerUrl>> = ["http://main.tl.test/announce?pk=1", "http://alt.tl.test/announce?pk=1", "http://other.test/announce?pk=1"]
            .iter()
            .map(|u| vec![url(u)])
            .collect();
        if fold {
            fold_tiers(raw, |u| u.host().ends_with(".tl.test").then(|| "tl".to_owned()))
        } else {
            raw
        }
    };
    let run = |fold: bool, main_down: bool| {
        let (mut lab, log) = lab_with(move |host, _, _| (!(main_down && host == "main.tl.test")).then_some(1800));
        let mut sp = spec(1, &[]);
        sp.tiers = tiers(fold);
        lab.add(sp);
        lab.stats(1, 0, 0, 0, 10);
        let now = lab.now;
        lab.hub().set_role(&ih(1), TorrentRole::Seeding, now);
        lab.ev(1, TorrentEvent::Start);
        lab.run_for(d(12 * 3600));
        log.borrow().clone()
    };
    // with the profile: one stream for the tracker (main works => alt silent), other is its own
    let l = run(true, false);
    // (the shuffle inside the tier decides which of the two names is tried first)
    let (act, idle) = if count(&l, "main.tl.test", Some(Event::Started)) > 0 {
        ("main.tl.test", "alt.tl.test")
    } else {
        ("alt.tl.test", "main.tl.test")
    };
    assert_eq!(count(&l, idle, Some(Event::Started)), 0);
    assert!(count(&l, act, None) >= 22);
    assert!(count(&l, "other.test", None) >= 22);
    // failover: main down => alt carries the stream, one announce per interval
    let l = run(true, true);
    assert!(count(&l, "alt.tl.test", None) >= 22 && count(&l, "alt.tl.test", None) <= 24);
    check_host(&l, "alt.tl.test", 890);
    // without the profile: two independent streams
    let l = run(false, false);
    assert!(count(&l, "main.tl.test", None) >= 22);
    assert!(count(&l, "alt.tl.test", None) >= 22);
}
