//! Adversarial audit: scheduling. Early/duplicate regular announces, manual and
//! network-change abuse, clock jumps, sleeps, persisted timing from another clock.
//!
//! Tests named `bug_*` reproduce defects found by the audit; they were
//! `#[ignore]`d until the fix and are permanent regressions now.

use crate::adversarial_support;

use std::time::Duration;

use adversarial_support::*;
use trs_tracker::gate::GateConfig;
use trs_tracker::hub::TrackerHub;
use trs_tracker::{Event, MemoryStore, Millis, TorrentEvent};

const A: &str = "http://a.test/announce";

fn lab_with(interval: u64, min: Option<u64>) -> Lab {
    let mut lab = Lab::new(always(move || ok(interval, min)));
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 1000, 1000);
    lab.ev(1, TorrentEvent::Start);
    lab
}

/// Regular (event-less) announces of torrent 1 with their predecessor's time
/// (every answer takes 100 ms in `always`).
fn regular_gaps(lab: &Lab) -> Vec<(Millis, Millis)> {
    let sent = lab.sent_for(1);
    let mut out = vec![];
    for w in sent.windows(2) {
        if w[1].event().is_none() {
            out.push((Millis(w[0].t.0 + 100), w[1].t));
        }
    }
    out
}

#[test]
fn regular_announces_follow_max_of_interval_and_min_interval() {
    for (iv, min) in [
        (1800u64, None),
        (1800, Some(900)),
        (600, Some(1800)),
        (60, None),
        (1, None),
        (0, None),
        (86_400 * 3, None),
        (120, Some(60)),
        (900, Some(0)),
    ] {
        let mut lab = lab_with(iv, min);
        lab.run_for(d(24 * 3600));
        let t = clamp(iv, min);
        let gaps = regular_gaps(&lab);
        assert!(
            !gaps.is_empty() || t > 12 * 3600,
            "no regular announces for {iv}/{min:?}"
        );
        for (resp, next) in gaps {
            assert!(
                next.0 - resp.0 >= t * 1000,
                "interval={iv} min={min:?}: announce {}ms after the response, needs {}s",
                next.0 - resp.0,
                t
            );
        }
        // never more than ceil(24h / t) + 1 announces in a day
        let n = lab.sent_for(1).len() as u64;
        assert!(n <= 86_400 / t + 3, "{iv}/{min:?}: {n} announces in 24 h");
    }
}

fn clamp(iv: u64, min: Option<u64>) -> u64 {
    let t = trs_tracker::clamp_timing(Some(iv as i64), min.map(|m| m as i64));
    t.interval.as_secs()
}

#[test]
fn repeated_timer_ticks_and_wake_storms_do_not_add_announces() {
    let mut lab = lab_with(1800, None);
    lab.run_for(d(10));
    let after_start = lab.sent.len();
    // 5 000 ticks at the same instant
    for _ in 0..5000 {
        lab.pump();
    }
    assert_eq!(lab.sent.len(), after_start);
    // a wake notification every 40 s for 3 hours must not produce early announces
    for _ in 0..(3 * 90) {
        lab.wake();
        lab.run_for(d(40));
    }
    for (resp, next) in regular_gaps(&lab) {
        assert!(
            next.0 - resp.0 >= 1_800_000,
            "wake produced an early announce"
        );
    }
    assert!(
        (5..=7).contains(&lab.count_event(1, None)),
        "3 h at 30 min interval"
    );
}

#[test]
fn manual_reannounce_mashing_is_limited_to_min_interval() {
    for (iv, min, floor) in [
        (1800u64, Some(900u64), 900u64),
        (1800, None, 300),
        (3600, Some(60), 60),
    ] {
        let mut lab = lab_with(iv, min);
        for _ in 0..(2 * 3600) {
            lab.manual(1);
            lab.run_for(d(1));
        }
        let gaps = regular_gaps(&lab);
        assert!(!gaps.is_empty());
        for (resp, next) in &gaps {
            assert!(
                next.0 - resp.0 >= floor * 1000,
                "{iv}/{min:?}: manual announce after {} ms, floor {floor} s",
                next.0 - resp.0
            );
        }
        assert!(lab.count_event(1, None) as u64 <= 2 * 3600 / floor + 2);
    }
}

#[test]
fn manual_reannounce_cannot_shorten_a_backoff() {
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event == Some(Event::Started) {
            vec![(d(1), ok(1800, Some(60)))]
        } else {
            vec![(d(1), timeout())]
        }
    }));
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(2000)); // first regular announce fails around t=1800
    let n0 = lab.sent.len();
    let last_fail = lab.sent.last().unwrap().t;
    for _ in 0..30 {
        lab.manual(1);
        lab.run_for(d(1));
    }
    // A user's ask beats the backoff, but not within 10 s of the last attempt (a double click).
    assert!(
        lab.sent.len() == n0 || lab.sent[n0].t.0 - last_fail.0 >= 10_000,
        "manual announce within 10 s of the last attempt"
    );
}

#[test]
fn halted_tracker_is_retried_by_manual_at_most_once_in_ten_seconds() {
    let mut lab = Lab::new(always(|| reject("torrent not registered")));
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(30));
    assert_eq!(lab.sent.len(), 1);
    for _ in 0..600 {
        lab.manual(1);
        lab.run_for(d(1));
    }
    assert!(
        lab.sent.len() <= 62,
        "{} requests in 10 min of manual mashing",
        lab.sent.len()
    );
    for w in lab.sent.windows(2) {
        assert!(w[1].t.0 - w[0].t.0 >= 10_000);
    }
}

/// FIXED audit defect (HAMMER-RISK, low): `Start`/`Resume` clears a `failure reason` halt with
/// no spacing at all (`Input::ClearHalt` has no floor, unlike `ManualReannounce`
/// which has `HALT_RETRY_FLOOR` = 60 s). An engine/agent loop that calls
/// `Resume` (or `Start`, idempotently, on every UI refresh) re-sends `started`
/// as fast as the gate allows (1/s) to a tracker that just rejected us.
#[test]
fn bug_start_resume_after_halt_has_no_floor() {
    let mut lab = Lab::new(always(|| reject("unregistered torrent")));
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(30));
    for _ in 0..300 {
        lab.ev(1, TorrentEvent::Start); // idempotent "make sure it runs"
        lab.run_for(d(2));
    }
    // 10 minutes of that must not be more than ~1 request per minute
    assert!(
        lab.sent.len() <= 12,
        "{} started announces to a rejecting tracker in 10 minutes",
        lab.sent.len()
    );
}

#[test]
fn pause_resume_mashing_is_bounded_by_the_gate() {
    let mut lab = lab_with(1800, None);
    lab.run_for(d(30));
    for _ in 0..600 {
        lab.ev(1, TorrentEvent::Pause);
        lab.run_for(Duration::from_millis(250));
        lab.ev(1, TorrentEvent::Resume);
        lab.run_for(Duration::from_millis(250));
    }
    // 5 minutes of toggling twice a second: at most the bucket rate to this host
    assert!(max_in_window(&lab.sent, "a.test", d(1)) <= 4);
    assert!(
        lab.sent.len() <= 3 + 300 + 5,
        "{} requests in 5 min",
        lab.sent.len()
    );
}

/// A flapping interface does not change the announce rate: a network change
/// never makes a regular announce earlier (see
/// `bug_network_flapping_announces_before_the_interval`), and it does not
/// starve it either: 9 h of flapping at a 30 min interval is still ~17 announces.
#[test]
fn network_flapping_keeps_the_normal_announce_rate() {
    let mut lab = lab_with(1800, None);
    lab.run_for(d(10));
    for _ in 0..(3 * 360) {
        lab.network();
        lab.run_for(d(30));
    }
    let gaps = regular_gaps(&lab);
    assert!(
        (15..=19).contains(&gaps.len()),
        "{} announces in 9 h at a 30 min interval",
        gaps.len()
    );
    for (resp, next) in gaps {
        assert!(next.0 - resp.0 >= 1_800_000);
    }
}

/// FIXED audit defect (CORRECTNESS, low): every network notification pushes `not_before` out
/// to `now + rand(0..30 s)` (`max` with the previous value), so a notification
/// every few seconds postpones even the first `started` indefinitely.
#[test]
fn bug_network_event_storm_starves_announces() {
    let mut lab = lab_with(1800, None);
    for _ in 0..600 {
        lab.network();
        lab.run_for(d(1));
    }
    assert!(
        !lab.sent.is_empty(),
        "no started in 10 minutes of 1 Hz network events"
    );
}

#[test]
fn bug_network_flapping_announces_before_the_interval() {
    let mut lab = lab_with(1800, None);
    lab.run_for(d(10));
    for _ in 0..(3 * 360) {
        lab.network();
        lab.run_for(d(30));
    }
    for (resp, next) in regular_gaps(&lab) {
        assert!(
            next.0 - resp.0 >= 1_800_000,
            "announce {} s after a success with a 1800 s interval",
            (next.0 - resp.0) / 1000
        );
    }
}

#[test]
fn sleeping_40_hours_costs_one_announce_per_torrent_and_respects_the_bucket() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    for n in 0..300 {
        lab.add(spec(n, &[&["http://big.test/announce?pk=abc"]]));
        lab.stats(n, 0, 0, 1000, 1000);
        lab.ev(n, TorrentEvent::Start);
    }
    lab.run_for(d(3600));
    let before = lab.sent.len();
    // a request is in flight when the lid closes
    lab.responder = Box::new(|_| vec![]);
    lab.run_for(d(1900));
    lab.responder = always(|| ok(1800, None));
    let slept_at = lab.now;
    lab.sleep(d(40 * 3600));
    lab.run_for(d(30 * 60));
    let after: Vec<_> = lab.sent[before..]
        .iter()
        .filter(|x| x.t.0 > slept_at.0 + 40 * 3_600_000)
        .collect();
    let mut per: std::collections::HashMap<_, u32> = Default::default();
    for x in &after {
        *per.entry(x.ih).or_default() += 1;
    }
    for (k, c) in &per {
        assert!(
            *c <= 2,
            "{k:?}: {c} announces in 30 min after a 40 h sleep (interval 30 min)"
        );
    }
    assert!(max_in_window(&lab.sent, "big.test", d(1)) <= 4);
    assert!(max_in_window(&lab.sent, "big.test", d(60)) <= 3 + 60 + 1);
}

#[test]
fn clock_stepping_back_one_hour_is_safe_and_recovers() {
    let mut lab = lab_with(1800, None);
    lab.run_for(d(7000));
    let n0 = lab.sent.len();
    // wall clock steps back by an hour (NTP step / manual change)
    lab.now = Millis(lab.now.0 - 3_600_000);
    lab.run_for(d(1800 + 3600 + 600));
    // all announces after the step respect the interval in elapsed wall time
    let mut last_resp: Option<Millis> = None;
    for x in &lab.sent[n0..] {
        if let (Some(r), None) = (last_resp, x.event()) {
            assert!(
                x.t.0 >= r.0 + 1_800_000 - 3_600_000,
                "must not send before the clamped clock"
            );
        }
        last_resp = Some(Millis(x.t.0 + 100));
    }
    // recovered: the tracker is announced to again within one hour + interval
    assert!(
        lab.sent.len() > n0,
        "never announced again after a backwards step"
    );
}

/// FIXED audit defect (CORRECTNESS/availability): the machines and the gate keep a high-water
/// mark of `now`. One bogus forward reading (RTC glitch, VM resume, a unix-ms
/// clock that was briefly wrong) pins every machine's `now` and every host
/// bucket's `last_refill` in the future; after the clock is corrected nothing is
/// ever announced again until the wall clock reaches the bogus value.
#[test]
fn bug_forward_clock_excursion_silences_trackers() {
    let mut lab = lab_with(1800, None);
    lab.run_for(d(100));
    let real = lab.now;
    // one tick with a clock 10 years ahead (e.g. a wrong RTC value) ...
    lab.now = Millis(real.0 + 10 * 365 * 86_400_000);
    lab.pump();
    // ... then the clock is right again
    lab.now = real;
    let n0 = lab.sent.len();
    lab.run_for(d(24 * 3600));
    assert!(
        lab.sent.len() >= n0 + 40,
        "only {} announces in the 24 h after the clock was corrected",
        lab.sent.len() - n0
    );
}

/// Same defect in the gate alone: a refill timestamp in the future starves the
/// host's bucket although machines (e.g. newly added ones) have sane clocks.
#[test]
fn bug_gate_bucket_starves_after_forward_excursion() {
    use std::sync::Arc;
    use trs_tracker::gate::*;
    use trs_tracker::machine::SendKind;
    use trs_tracker::{AnnounceParams, RequestId};
    let u = Arc::new(trs_tracker::TrackerUrl::parse("http://h.test/announce").unwrap());
    let mut gate = HostGate::new(GateConfig::default());
    for n in 0..4u32 {
        gate.register_torrent(ih(n), vec![u.clone()], &[], Millis(0));
    }
    let p = |n: u32| AnnounceParams {
        info_hash: ih(n),
        peer_id: [1; 20],
        port: 1,
        uploaded: 0,
        downloaded: 0,
        corrupt: 0,
        left: 5,
        key: 7,
        event: Some(Event::Started),
        numwant: 50,
        tracker_id: None,
        ipv4: None,
        ipv6: None,
        crypto: Default::default(),
    };
    let timing = trs_tracker::clamp_timing(Some(1800), None);
    // the glitch tick drains the (full) bucket: 3 releases at "year 2036"
    let glitch = Millis(10 * 365 * 86_400_000);
    let mut released = 0;
    for n in 0..3u32 {
        gate.submit(
            (ih(n), 0),
            RequestId(1),
            u.clone(),
            p(n),
            SendKind::Event,
            Millis(0),
        )
        .unwrap();
        for r in gate.poll(glitch) {
            gate.finish(r.ticket(), Outcome::Success { timing }, glitch);
            released += 1;
        }
    }
    assert_eq!(released, 3);
    // clock corrected: 10 s later the bucket must have refilled
    gate.submit(
        (ih(3), 0),
        RequestId(1),
        u.clone(),
        p(3),
        SendKind::Event,
        Millis(0),
    )
    .unwrap();
    let got = gate.poll(s(10));
    assert!(
        !got.is_empty(),
        "host never gets a token again after the excursion"
    );
}

#[test]
fn clock_near_u64_max_does_not_panic_or_spin() {
    let cfg = GateConfig {
        initial_tokens: 3,
        ..GateConfig::default()
    };
    let mut lab = Lab::with_cfg(always(|| ok(1800, None)), cfg);
    lab.now = Millis(u64::MAX - 10_000_000);
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_until(Millis(u64::MAX - 1000));
    let n = lab.sent.len();
    assert!(n >= 1);
    assert!(n < 10, "{n} requests in the last 10_000 s before u64::MAX");
}

#[test]
fn persisted_hold_from_another_clock_is_capped_to_one_hour() {
    use trs_tracker::store::{TimingKey, TimingRecord, TimingStore};
    for hold in [0u64, 5, u64::MAX / 2, u64::MAX - 1, u64::MAX] {
        let mut store = MemoryStore::new();
        let spec1 = spec(1, &[&[A]]);
        store
            .save(
                &TimingKey {
                    info_hash: ih(1),
                    tracker_url: A.to_owned(),
                },
                &TimingRecord {
                    session_open: true,
                    min_allowed_at: Some(Millis(hold)),
                    next_allowed_at: Some(Millis(hold)),
                    last_announce_at: Some(Millis(hold)),
                    ..Default::default()
                },
            )
            .unwrap();
        let mut hub = TrackerHub::new(GateConfig::default(), store, 1);
        let t0 = s(1_000_000);
        hub.add_torrent(spec1, t0).unwrap();
        hub.set_stats(
            &ih(1),
            trs_tracker::Stats {
                uploaded: 0,
                downloaded: 0,
                corrupt: 0,
                left: 5,
                size: 5,
            },
            t0,
        );
        hub.torrent_event(&ih(1), TorrentEvent::Start, t0);
        let mut first = None;
        let mut now = t0;
        for _ in 0..5000 {
            if !hub.tick(now).is_empty() {
                first = Some(now);
                break;
            }
            now = Millis(now.0 + 1000);
        }
        let first = first.expect("started never released within 5000 s + hold cap");
        let wait = first.0 - t0.0;
        assert!(wait <= 3_600_000 + 10_000, "hold {hold}: waited {wait} ms");
    }
}

#[test]
fn restart_after_crash_never_starts_before_persisted_min_allowed() {
    for crash_after in [1u64, 100, 800] {
        let mut lab = Lab::new(always(|| ok(1800, Some(900))));
        lab.add(spec(1, &[&[A]]));
        lab.stats(1, 0, 0, 10, 10);
        lab.ev(1, TorrentEvent::Start);
        lab.run_for(d(20));
        let success_at = lab.sent[0].t.0 + 100;
        lab.run_until(Millis(success_at + crash_after * 1000));
        lab.crash_and_relaunch();
        let n0 = lab.sent.len();
        lab.run_for(d(1000));
        let first = &lab.sent[n0];
        assert_eq!(first.event(), Some(Event::Started));
        assert!(
            first.t.0 >= success_at + 900_000,
            "started after a crash at +{crash_after}s was sent at +{} s (min interval 900 s)",
            (first.t.0 - success_at) / 1000
        );
    }
}

/// A torrent whose `completed` fails with a 5xx right after `started` succeeded with a 600 s min interval.
fn failed_completed_lab() -> Lab {
    let mut failed_once = false;
    let mut lab = Lab::new(Box::new(move |sent| match sent.p.event {
        Some(Event::Started) => vec![(d(1), ok(1800, Some(600)))],
        Some(Event::Completed) if !failed_once => {
            failed_once = true;
            vec![(d(1), http_status(503))]
        }
        _ => vec![(d(1), ok(1800, Some(600)))],
    }));
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(5));
    lab.stats(1, 5, 10, 0, 10);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(5)); // `completed` fails at about t=6
    lab
}

#[test]
fn a_forced_announce_inside_the_min_interval_after_a_failed_completed_does_not_trip_the_audit() {
    let mut lab = failed_completed_lab();
    assert_eq!(lab.count_event(1, Some(Event::Completed)), 1);
    let before = lab.count_event(1, None);
    lab.run_for(d(20));
    lab.force(1); // t is about 30 s, the min interval (600 s) is far from over
    lab.run_for(d(30));
    // (`Lab` panics when the machine or the gate trips.)
    assert_eq!(lab.count_event(1, None), before + 1, "the forced announce went out");
    let last = lab.sent_for(1).into_iter().rfind(|s| s.event().is_none()).unwrap();
    assert!(last.t.0 < 600_000, "inside the min interval: {:?}", last.t);
    assert_eq!((last.p.downloaded, last.p.left), (10, 0));
    // And the tracker keeps working: the regular schedule goes on.
    lab.run_for(d(2000));
    assert!(lab.count_event(1, None) >= before + 2, "regular announces continue");
}

#[test]
fn a_second_forced_announce_within_five_minutes_is_deferred_not_sent_and_not_tripping() {
    let mut lab = failed_completed_lab();
    lab.run_for(d(20));
    lab.force(1);
    lab.run_for(d(30));
    let n = lab.count_event(1, None);
    for _ in 0..120 {
        lab.force(1); // two minutes of mashing "Announce anyway"
        lab.run_for(d(1));
    }
    assert_eq!(lab.count_event(1, None), n, "no second early announce inside five minutes (and none by the min interval)");
    // The ordinary ask is bound by the min interval too.
    lab.manual(1);
    lab.run_for(d(10));
    assert_eq!(lab.count_event(1, None), n);
    // After the 5 minutes a new forced one is allowed again, still inside the 600 s min interval of the last success.
    lab.run_for(d(200));
    lab.force(1);
    lab.run_for(d(30));
    assert!(lab.count_event(1, None) <= n + 1);
}
