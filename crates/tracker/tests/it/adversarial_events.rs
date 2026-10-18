//! Adversarial audit: event order, `completed` semantics, counters.

use crate::adversarial_support;

use adversarial_support::*;
use trs_tracker::{Event, Millis, TorrentEvent};

const A: &str = "http://a.test/announce?passkey=SECRET";
const B: &str = "http://b.test/announce?passkey=SECRET";

fn lab_ok() -> Lab {
    Lab::new(always(|| ok(1800, None)))
}

fn start_download(lab: &mut Lab, n: u32, tiers: &[&[&str]]) {
    lab.add(spec(n, tiers));
    lab.stats(n, 0, 0, 1000, 1000);
    lab.ev(n, TorrentEvent::Start);
}

fn completed_total(lab: &Lab) -> usize {
    lab.sent
        .iter()
        .filter(|x| x.event() == Some(Event::Completed))
        .count()
}

// ---------------------------------------------------------------- completed

#[test]
fn completed_is_sent_once_with_left_zero() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 5000, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(10));
    // the engine (wrongly) repeats the notification a few times
    for _ in 0..5 {
        lab.ev(1, TorrentEvent::Complete);
        lab.run_for(d(1));
    }
    lab.run_for(d(3 * 3600));
    assert_eq!(completed_total(&lab), 1);
    let c = lab
        .sent
        .iter()
        .find(|x| x.event() == Some(Event::Completed))
        .unwrap();
    assert_eq!(c.p.left, 0);
    assert_eq!(c.p.downloaded, 1000);
    assert_eq!(c.p.uploaded, 5000);
}

#[test]
fn completed_never_twice_for_a_crash_at_any_instant() {
    // Crash every 50 ms from 0 to 30 s after the completion notification.
    let mut missed = 0;
    for step in 0..600u64 {
        let mut lab = lab_ok();
        start_download(&mut lab, 1, &[&[A]]);
        lab.run_for(d(10));
        lab.stats(1, 10, 1000, 0, 1000);
        lab.ev(1, TorrentEvent::Complete);
        lab.run_until(Millis(lab.now.0 + step * 50));
        lab.crash();
        // the engine knows the torrent is complete now
        lab.stats(1, 0, 0, 0, 1000);
        lab.ev(1, TorrentEvent::Start);
        lab.run_for(d(4 * 3600));
        let n = completed_total(&lab);
        assert!(
            n <= 1,
            "crash {} ms after Complete: {n} completed announces",
            step * 50
        );
        if n == 0 {
            missed += 1;
        }
        // a second and third crash/restart cycle must not resurrect it
        for _ in 0..2 {
            lab.crash();
            lab.stats(1, 0, 0, 0, 1000);
            lab.ev(1, TorrentEvent::Start);
            lab.run_for(d(3600));
        }
        assert!(completed_total(&lab) <= 1);
    }
    // At-most-once is the deliberate trade-off; a lost `completed` is only
    // possible while the request is on the wire (100 ms in this lab).
    assert!(missed <= 3, "{missed} of 600 crash points lost completed");
}

#[test]
fn completed_survives_failure_after_success_without_a_resend() {
    // the 4.0.6 shape: the second address family fails after the first succeeded
    let mut lab = Lab::new(Box::new(|_| {
        vec![(d(1), ok(1800, None)), (d(2), timeout())]
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(6 * 3600));
    assert_eq!(completed_total(&lab), 1);
}

/// FIXED audit defect (CORRECTNESS, host-wide outage): the README tells the engine that "late
/// or duplicate answers for a ticket are fine to forward". A *second successful*
/// answer for the same ticket (the other address family, 4 s after the first) is
/// ignored by the machine (first success wins, schedule anchored at the first
/// answer) but the gate hands it to the `Auditor` (`HostGate::finish`, unknown
/// ticket branch), which moves its own anchor to the later time. The machine's
/// next regular announce is then "earlier than interval" for the auditor, and
/// the gate trips the whole host (fail closed): every torrent on that host
/// stops announcing until `reset_host` + re-add.
#[test]
fn bug_duplicate_late_success_trips_the_host() {
    let mut lab = Lab::new(Box::new(|_| {
        vec![(d(1), ok(1800, None)), (d(5), ok(1800, None))]
    }));
    lab.panic_on_trip = false;
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(4 * 3600));
    assert!(
        lab.gate_events
            .iter()
            .all(|e| !matches!(e, trs_tracker::gate::GateEvent::HostTripped { .. })),
        "host tripped: {:?}",
        lab.gate_events
    );
    assert!(
        lab.sent.len() >= 8,
        "only {} requests in 4 h",
        lab.sent.len()
    );
}

/// Same root cause, no duplicate needed: the gate timed a regular announce out
/// (60 s), the machine retried (#2) and #2 failed; then the late success of #1
/// arrives. The machine cannot use it (something newer was sent) but the
/// Auditor re-anchors on it, so the next retry looks "early" and the host trips.
/// (With the real transports this needs the engine to deliver an answer >60 s
/// late, e.g. after a long stall or system sleep.)
#[test]
fn bug_late_success_after_newer_request_trips_the_host() {
    let mut lab = Lab::new(Box::new(|s| match s.n {
        0 => vec![(d(1), ok(1800, None))],
        1 => vec![(d(130), ok(1800, None))],
        _ => vec![(d(1), timeout())],
    }));
    lab.panic_on_trip = false;
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(4 * 3600));
    assert!(
        lab.gate_events
            .iter()
            .all(|e| !matches!(e, trs_tracker::gate::GateEvent::HostTripped { .. })),
        "host tripped: {:?}",
        lab.gate_events
    );
}

#[test]
fn completed_failure_is_not_retried_by_design() {
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event == Some(Event::Completed) {
            vec![(d(1), timeout())]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(24 * 3600));
    assert_eq!(
        completed_total(&lab),
        1,
        "documented at-most-once trade-off"
    );
    // later regular announces carry left=0
    assert!(
        lab.sent
            .iter()
            .filter(|x| x.event().is_none())
            .all(|x| x.p.left == 0)
    );
}

#[test]
fn recheck_that_raises_left_after_completion_does_not_resend_completed() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(10));
    // recheck finds 300 bytes missing: left goes UP, then the torrent completes again
    lab.stats(1, 0, 1000, 300, 1000);
    lab.run_for(d(1900));
    assert!(
        lab.sent
            .iter()
            .any(|x| x.event().is_none() && x.p.left == 300)
    );
    lab.stats(1, 0, 1300, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.ev(1, TorrentEvent::Pause);
    lab.run_for(d(30));
    lab.ev(1, TorrentEvent::Resume);
    lab.run_for(d(3600));
    assert_eq!(completed_total(&lab), 1);
}

#[test]
fn removed_and_readded_torrent_does_not_get_a_second_completed() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(10));
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(10));
    assert!(lab.hub().is_quiescent(&ih(1)));
    assert!(lab.hub().remove_torrent(&ih(1)));
    // user re-adds the same torrent (data deleted, downloads again from 0)
    let sp = spec(1, &[&[A]]);
    let now = lab.now;
    lab.hub().add_torrent(sp, now).unwrap();
    lab.stats(1, 0, 0, 1000, 1000);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(10));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(3600));
    assert_eq!(
        completed_total(&lab),
        1,
        "SPEC 7C.3 #3: at most once per torrent, ever"
    );
}

#[test]
fn added_complete_never_sends_completed_even_if_engine_fires_complete() {
    let mut lab = lab_ok();
    let mut sp = spec(1, &[&[A]]);
    sp.added_complete = true;
    lab.add(sp);
    lab.stats(1, 0, 0, 0, 1000);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(10));
    lab.ev(1, TorrentEvent::Complete);
    // recheck says 10 bytes missing and then complete again
    lab.stats(1, 0, 0, 10, 1000);
    lab.run_for(d(4000));
    lab.stats(1, 0, 10, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(4000));
    assert_eq!(completed_total(&lab), 0);
    assert_eq!(lab.sent[0].p.left, 0);
}

#[test]
fn magnet_with_unknown_size_never_claims_completeness() {
    let mut lab = lab_ok();
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 0, 0); // metadata unknown
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(10));
    assert_eq!(lab.sent[0].p.event, Some(Event::Started));
    assert!(lab.sent[0].p.left >= 1, "started with left=0 on a magnet");
}

/// FIXED audit defect (CORRECTNESS): `left_positive_at_start` is frozen at `Start` time from
/// the last pushed stats. A magnet that is started before its metadata is known
/// (the normal order: stats are `{left: 0, size: 0}` or never pushed) announces
/// `started` with `left = 1`, then downloads normally, but `Complete` is
/// ignored because `left_positive_at_start == false`: the tracker never sees
/// `completed` (no snatch is recorded; on H&R-tracking sites this is
/// invisible, on others the user loses the credit).
#[test]
fn bug_magnet_started_before_metadata_never_sends_completed() {
    let mut lab = lab_ok();
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 0, 0);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(10));
    // metadata arrives, the real download happens
    lab.stats(1, 0, 0, 1000, 1000);
    lab.run_for(d(60));
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(60));
    assert_eq!(completed_total(&lab), 1);
}

/// Same root cause with a known-size torrent when the engine pushes stats a
/// moment after `Start` (README only demands "before Complete/Stop").
#[test]
fn bug_stats_pushed_after_start_lose_completed() {
    let mut lab = lab_ok();
    lab.add(spec(1, &[&[A]]));
    lab.ev(1, TorrentEvent::Start); // no stats yet
    lab.stats(1, 0, 0, 1000, 1000); // arrive before the 0..5 s start jitter elapsed
    lab.run_for(d(10));
    assert_eq!(lab.sent[0].p.left, 1000);
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(60));
    assert_eq!(completed_total(&lab), 1);
}

/// FIXED audit defect (CORRECTNESS, low): a download that finishes while a stop is being
/// processed (`Pause`/`Quit` then the last piece lands) is never reported.
/// `on_complete` ignores the event when `want_stop`, and the next session has
/// `left_positive_at_start == false`.
#[test]
fn bug_complete_during_stop_window_is_forgotten() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 0, 990, 10, 1000);
    lab.ev(1, TorrentEvent::Pause);
    // last piece lands while `stopped` is still queued/in flight
    lab.stats(1, 0, 1000, 0, 1000);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(60));
    lab.ev(1, TorrentEvent::Resume);
    lab.run_for(d(3600));
    assert_eq!(completed_total(&lab), 1);
}

// ------------------------------------------------------------ session grammar

#[test]
fn stop_before_started_leaves_the_gate_sends_nothing() {
    let mut lab = lab_ok();
    for n in 0..50 {
        start_download(&mut lab, n, &[&["http://big.test/announce?passkey=x"]]);
    }
    // bucket starts empty and releases 1/s: after 5 s most torrents have not sent started
    lab.run_for(d(5));
    for n in 0..50 {
        lab.ev(n, TorrentEvent::Stop);
    }
    lab.run_for(d(600));
    for n in 0..50 {
        let started = lab.count_event(n, Some(Event::Started));
        let stopped = lab.count_event(n, Some(Event::Stopped));
        assert!(stopped <= started, "torrent {n}: stopped without started");
        if started == 0 {
            assert!(
                lab.sent_for(n).is_empty(),
                "torrent {n} contacted although stopped before started"
            );
        }
    }
    assert!(
        lab.sent
            .iter()
            .filter(|x| x.event() == Some(Event::Started))
            .count()
            < 50
    );
}

#[test]
fn stopped_is_not_sent_to_trackers_we_never_announced_to() {
    let mut lab = lab_ok();
    // (same tier: tiers announce independently since SPEC 7C.4b, a failover in the tier is silent)
    start_download(&mut lab, 1, &[&[A, B]]);
    lab.run_for(d(20));
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(20));
    let silent = if lab.host_count("a.test") > 0 { "b.test" } else { "a.test" };
    assert_eq!(
        lab.host_count(silent),
        0,
        "the failover of a tier must be silent while the first works"
    );
    assert_eq!(lab.count_event(1, Some(Event::Stopped)), 1);
}

#[test]
fn stopped_goes_only_to_the_trackers_with_a_session() {
    // A is down: B takes over; when A recovers B is suspended with `stopped`.
    let down = std::rc::Rc::new(std::cell::Cell::new(true));
    let dn = down.clone();
    let mut lab = Lab::new(Box::new(move |sent| {
        if sent.host == "a.test" && dn.get() {
            vec![(d(1), timeout())]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start_download(&mut lab, 1, &[&[A], &[B]]);
    lab.run_for(d(30));
    assert!(lab.host_count("b.test") >= 1);
    down.set(false);
    let recovered = lab.now;
    lab.run_for(d(300));
    // everything stopped by now except the one healthy tracker
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(600));
    let mut view = View::new();
    for x in &lab.sent {
        view.check(x, 1000, !(x.host == "a.test" && x.t < recovered));
    }
    assert!(view.violations.is_empty(), "{:#?}", view.violations);
    let a_stopped = lab
        .sent
        .iter()
        .filter(|x| x.host == "a.test" && x.event() == Some(Event::Stopped))
        .count();
    let b_stopped = lab
        .sent
        .iter()
        .filter(|x| x.host == "b.test" && x.event() == Some(Event::Stopped))
        .count();
    assert!(
        a_stopped <= 4 && b_stopped <= 4,
        "a={a_stopped} b={b_stopped}"
    );
}

#[test]
fn stopped_to_a_dead_tracker_is_bounded() {
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event.is_none() || sent.p.event == Some(Event::Started) {
            vec![(d(1), ok(1800, None))]
        } else {
            vec![(d(1), http_status(503))]
        }
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(3 * 3600));
    let st = lab.count_event(1, Some(Event::Stopped));
    assert!(
        (1..=3).contains(&st),
        "{st} stopped announces to a tracker answering 503"
    );
}

/// Quitting is one `stopped` attempt per tracker: a dead tracker is told once (no retry after a
/// minute of backoff), and a healthy one hears it exactly once.
#[test]
fn quit_sends_one_stopped_attempt_and_never_retries() {
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event.is_none() || sent.p.event == Some(Event::Started) {
            vec![(d(1), ok(1800, None))]
        } else {
            vec![(d(1), http_status(503))]
        }
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.quit(1);
    lab.run_for(d(3 * 3600));
    assert_eq!(lab.count_event(1, Some(Event::Stopped)), 1, "{:?}", lab.events_for(1));

    let mut ok_lab = lab_ok();
    start_download(&mut ok_lab, 1, &[&[A], &[B]]);
    ok_lab.run_for(d(10));
    ok_lab.quit(1);
    ok_lab.run_for(d(60));
    // two tiers announce independently: one `stopped` for each tracker
    assert_eq!(ok_lab.count_event(1, Some(Event::Stopped)), 2);
    for h in ["a.test", "b.test"] {
        let n = ok_lab.sent.iter().filter(|x| x.host == h && x.event() == Some(Event::Stopped)).count();
        assert_eq!(n, 1, "{h}");
    }
}

/// A quit while the first `started` is hung still ends with exactly one `stopped`, after it.
#[test]
fn quit_with_started_in_flight_orders_stopped_after_it() {
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event == Some(Event::Started) && sent.n == 0 {
            vec![] // the first started never completes (the driver cuts it at quit)
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.quit(1);
    lab.run_for(d(3600));
    let ev = lab.events_for(1);
    assert_eq!(ev[0], Some(Event::Started));
    assert_eq!(ev.iter().filter(|e| **e == Some(Event::Stopped)).count(), 1, "{ev:?}");
}

#[test]
fn started_hung_then_stop_still_says_goodbye_and_nothing_follows() {
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event == Some(Event::Started) && sent.n == 0 {
            vec![] // first started hangs
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(3600));
    let ev = lab.events_for(1);
    assert_eq!(ev[0], Some(Event::Started));
    assert_eq!(*ev.last().unwrap(), Some(Event::Stopped), "{ev:?}");
    assert_eq!(ev.iter().filter(|e| **e == Some(Event::Stopped)).count(), 1);
}

#[test]
fn resume_while_stopped_is_in_flight_orders_stopped_then_started() {
    let mut lab = Lab::new(Box::new(|sent| {
        if sent.p.event == Some(Event::Stopped) {
            vec![(d(20), ok(1800, None))]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(30));
    lab.ev(1, TorrentEvent::Pause);
    lab.run_for(d(3));
    lab.ev(1, TorrentEvent::Resume);
    lab.run_for(d(3));
    lab.ev(1, TorrentEvent::Pause);
    lab.ev(1, TorrentEvent::Resume);
    lab.run_for(d(600));
    let mut view = View::new();
    for x in &lab.sent {
        view.check(x, 1000, true);
    }
    assert!(
        view.violations.is_empty(),
        "{:#?} in {:?}",
        view.violations,
        lab.events_for(1)
    );
}

#[test]
fn session_counters_reset_at_started_and_never_decrease_within_a_session() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 700, 300, 700, 1000);
    lab.run_for(d(1900));
    lab.ev(1, TorrentEvent::Pause);
    lab.run_for(d(10));
    // fresh session: engine counts from zero again
    lab.stats(1, 0, 0, 700, 1000);
    lab.ev(1, TorrentEvent::Resume);
    lab.run_for(d(10));
    let started: Vec<_> = lab
        .sent
        .iter()
        .filter(|x| x.event() == Some(Event::Started))
        .collect();
    assert_eq!(started.len(), 2);
    assert_eq!((started[1].p.uploaded, started[1].p.downloaded), (0, 0));
    let stopped = lab
        .sent
        .iter()
        .find(|x| x.event() == Some(Event::Stopped))
        .unwrap();
    assert_eq!(
        (stopped.p.uploaded, stopped.p.downloaded, stopped.p.left),
        (700, 300, 700)
    );
    assert_eq!(stopped.p.numwant, 0);
}

/// FIXED audit defect (CORRECTNESS, low): `open_session` resets the high-water marks but not
/// the stored `raw` stats, so if the engine has not pushed fresh (zero)
/// session counters by the time `started` is built, `started` carries the
/// previous session's totals, and the high-water mark then pins the reported
/// values at that number for the whole new session. Mainstream clients (qBittorrent, Transmission) always
/// send `started` with 0/0 (FINDINGS.md "Counters").
#[test]
fn bug_started_after_resume_carries_stale_counters() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 5000, 300, 700, 1000);
    lab.ev(1, TorrentEvent::Pause);
    lab.run_for(d(10));
    lab.ev(1, TorrentEvent::Resume); // engine has not pushed new stats yet
    lab.run_for(d(10));
    let last_started = lab
        .sent
        .iter()
        .rev()
        .find(|x| x.event() == Some(Event::Started))
        .unwrap();
    assert_eq!((last_started.p.uploaded, last_started.p.downloaded), (0, 0));
    // later the engine pushes the true (small) counters; they must be reported
    lab.stats(1, 40, 10, 690, 1000);
    lab.run_for(d(1900));
    let reg = lab.sent.iter().rev().find(|x| x.event().is_none()).unwrap();
    assert_eq!(reg.p.uploaded, 40);
}

#[test]
fn engine_counters_going_backwards_or_left_above_size_are_sanitised() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.stats(1, 900, 500, 500, 1000);
    lab.run_for(d(1900));
    lab.stats(1, 100, 50, 5000, 1000); // garbage: smaller totals, left > size
    lab.run_for(d(1900));
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(10));
    let mut last = (0, 0);
    for x in lab
        .sent
        .iter()
        .filter(|x| x.event() != Some(Event::Started))
    {
        assert!(x.p.uploaded >= last.0 && x.p.downloaded >= last.1);
        assert!(x.p.left <= 1000);
        last = (x.p.uploaded, x.p.downloaded);
    }
}

#[test]
fn file_priority_changes_move_left_without_extra_requests() {
    let mut lab = lab_ok();
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    let n0 = lab.sent.len();
    for i in 0..1000u64 {
        // user toggles files on/off: left jumps up and down
        lab.stats(1, i, i / 2, if i % 2 == 0 { 400 } else { 900 }, 1000);
        lab.run_for(d(1));
    }
    assert!(
        lab.sent[n0..].len() <= 1,
        "{} requests for 1000 s of stats churn",
        lab.sent[n0..].len()
    );
}

#[test]
fn u64_extremes_do_not_panic_or_break_monotonicity() {
    let mut lab = lab_ok();
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, u64::MAX, u64::MAX, u64::MAX, u64::MAX);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(10));
    lab.stats(1, u64::MAX - 5, 1, 0, u64::MAX);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(4000));
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(100));
    let mut last = (0u64, 0u64);
    for x in &lab.sent {
        assert!(x.p.uploaded >= last.0 && x.p.downloaded >= last.1);
        last = (x.p.uploaded, x.p.downloaded);
    }
}

#[test]
fn tracker_id_is_echoed_per_session_and_stable() {
    let mut lab = Lab::new(Box::new(|sent| {
        // the tracker changes its id on every answer
        let id = format!("id{}", sent.n);
        vec![(d(1), ok_with_id(1800, id.as_bytes()))]
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(3 * 3600));
    let ids: Vec<_> = lab
        .sent
        .iter()
        .skip(1)
        .map(|x| x.p.tracker_id.clone())
        .collect();
    assert!(!ids.is_empty());
    assert!(
        ids.windows(2).all(|w| w[0] == w[1]),
        "tracker id must not change within a session: {ids:?}"
    );
}

#[test]
fn identity_never_changes_inside_a_hub_lifetime() {
    let mut lab = lab_ok();
    for n in 0..5 {
        start_download(&mut lab, n, &[&[A], &[B]]);
    }
    for round in 0..20 {
        lab.run_for(d(700));
        for n in 0..5 {
            lab.ev(
                n,
                if round % 2 == 0 {
                    TorrentEvent::Pause
                } else {
                    TorrentEvent::Resume
                },
            );
        }
    }
    let mut seen = std::collections::HashMap::new();
    for x in &lab.sent {
        let e = seen.entry(x.ih).or_insert((x.p.key, x.p.peer_id));
        assert_eq!(*e, (x.p.key, x.p.peer_id));
    }
}

// ------------------------------------------------------- engine ordering traps

/// FIXED audit defect (CORRECTNESS low, but visible to a tracker as a leech slot): a torrent
/// that is `added_complete` (a seed) but started before its first `set_stats`
/// is announced with `left = 1`: the "unknown size" rule (`size == 0 => left >= 1`)
/// overrides the machine's own knowledge that it is complete. Slot-limited
/// private trackers count that `started` as a leech and may answer
/// `failure reason: no download slots` (which then halts the tracker).
#[test]
fn bug_added_complete_torrent_without_stats_announces_as_a_leecher() {
    let mut lab = lab_ok();
    let mut sp = spec(1, &[&[A]]);
    sp.added_complete = true;
    lab.add(sp);
    lab.ev(1, TorrentEvent::Start); // stats not pushed yet
    lab.run_for(d(10));
    assert_eq!(lab.sent[0].p.left, 0, "a seed announced as leecher");
}

#[test]
fn torrent_without_any_tracker_is_inert() {
    let mut lab = lab_ok();
    lab.add(spec(1, &[]));
    lab.add(spec(2, &[&[]]));
    for n in [1, 2] {
        lab.stats(n, 0, 0, 10, 10);
        lab.ev(n, TorrentEvent::Start);
        lab.ev(n, TorrentEvent::Complete);
        lab.ev(n, TorrentEvent::Pause);
        lab.manual(n);
    }
    lab.run_for(d(3600));
    assert!(lab.sent.is_empty());
    assert!(lab.hub().is_quiescent(&ih(1)));
    assert!(lab.hub().remove_torrent(&ih(1)));
}

/// After a `failure reason` that follows an *accepted* `started`, the machine
/// closes the session without `stopped` ("the tracker rejected us"). A user
/// pause/resume then sends `started` again although the tracker never saw a
/// `stopped` for the first session. The tracker-probe style detector of
/// SPEC 7C.4a (REPEATED_STARTED) would flag this sequence.
#[test]
fn bug_started_repeated_without_stopped_after_a_midsession_halt() {
    let flip = std::rc::Rc::new(std::cell::Cell::new(false));
    let f = flip.clone();
    let mut lab = Lab::new(Box::new(move |_| {
        if f.get() {
            vec![(d(1), reject("torrent not registered"))]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    flip.set(true); // staff deleted the torrent: next announce is rejected
    lab.run_for(d(2000));
    lab.ev(1, TorrentEvent::Pause);
    lab.run_for(d(10));
    flip.set(false);
    lab.ev(1, TorrentEvent::Resume);
    lab.run_for(d(60));
    let mut view = View::new();
    for x in &lab.sent {
        // answers: first started ok, rejected regular, later ok
        let ok_answer = x.n != 1;
        view.check(x, 1000, ok_answer);
    }
    assert!(
        view.violations.is_empty(),
        "{:?} -> {:#?}",
        lab.events_for(1),
        view.violations
    );
}

// ------------------------------------------------------- fail-closed recovery

/// FIXED audit defect (CORRECTNESS): `Started` is refused when the durable write fails
/// (hub.rs mark_dispatched), and the refusal trips the machine *permanently*
/// (`Input::GateRefused` -> `trip`). One transient write error (disk full,
/// SQLite busy) therefore silences that tracker for the rest of the process, on
/// every torrent that happened to start in that moment. A retry with backoff
/// would be as safe (nothing was sent).
#[test]
fn bug_transient_store_failure_permanently_disables_the_tracker() {
    let mut lab = lab_ok();
    lab.panic_on_trip = false;
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 10, 10);
    lab.hub().store_mut().fail_saves = true;
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(20));
    lab.hub().store_mut().fail_saves = false; // disk is fine again
    lab.run_for(d(3 * 3600));
    assert!(
        !lab.sent.is_empty(),
        "never announced after the store recovered"
    );
}

/// FIXED audit defect (CORRECTNESS): a tripped machine ignores every input, including `Stop`,
/// so its session stays open and `is_quiescent`/`remove_torrent` never succeed.
/// Consequences: the torrent can never be removed from the hub, the README's
/// recovery path for a tripped host ("the torrent is re-added" after
/// `reset_host`) is impossible, and a quit waits the full 120 s linger.
#[test]
fn bug_tripped_machine_blocks_removal_and_quiescence() {
    let mut lab = lab_ok();
    lab.panic_on_trip = false;
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 10, 10);
    lab.hub().store_mut().fail_saves = true;
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(20));
    lab.hub().store_mut().fail_saves = false;
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(60));
    assert!(
        lab.hub().is_quiescent(&ih(1)),
        "a stopped torrent whose machine tripped is not quiescent"
    );
    assert!(lab.hub().remove_torrent(&ih(1)));
}

/// The documented recovery from a host trip: `reset_host`, stop, remove the
/// torrent, add it again. No process restart.
#[test]
fn bug_host_trip_cannot_be_recovered_without_restart() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.panic_on_trip = false;
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(100));
    // a protocol audit failure is the usual cause; the engine may also pull the
    // plug itself. (Any spelling of the host name works.)
    lab.hub().trip_host("A.test.", "test: fail closed");
    lab.run_for(d(4000)); // the next regular announce is refused: the machine trips
    assert!(
        lab.gate_events
            .iter()
            .any(|e| matches!(e, trs_tracker::gate::GateEvent::HostTripped { .. }))
    );
    let sent_while_tripped = lab.sent.len();
    lab.run_for(d(3600));
    assert_eq!(lab.sent.len(), sent_while_tripped, "silent while tripped");
    lab.hub().reset_host("a.test");
    lab.ev(1, TorrentEvent::Stop);
    lab.run_for(d(60));
    assert!(lab.hub().is_quiescent(&ih(1)));
    assert!(
        lab.hub().remove_torrent(&ih(1)),
        "cannot remove the torrent to re-add it"
    );
    // re-add and the tracker works again
    lab.specs.clear();
    lab.add(spec(1, &[&[A]]));
    lab.stats(1, 0, 0, 1000, 1000);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(3 * 3600));
    assert!(lab.sent.len() > sent_while_tripped + 3, "announcing again");
    assert_eq!(lab.sent.last().map(|x| x.host.as_str()), Some("a.test"));
}

#[test]
fn tracker_id_is_per_session_and_not_carried_over_a_restart() {
    // Like libtorrent and Transmission: the `tracker id` lives in memory only.
    let mut lab = Lab::new(Box::new(|_| vec![(d(1), ok_with_id(1800, b"abc123"))]));
    start_download(&mut lab, 1, &[&[A]]);
    lab.run_for(d(10));
    lab.crash();
    lab.stats(1, 0, 0, 10, 1000);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(4000));
    let started: Vec<_> = lab
        .sent
        .iter()
        .filter(|x| x.event() == Some(Event::Started))
        .collect();
    assert_eq!(started.len(), 2);
    assert_eq!(started[0].p.tracker_id, None);
    assert_eq!(
        started[1].p.tracker_id, None,
        "a new process (and a new session) starts without a tracker id"
    );
    // ... and latches the id it is given in the new session
    assert!(
        lab.sent
            .iter()
            .rev()
            .find(|x| x.event().is_none())
            .is_some_and(|x| x.p.tracker_id.as_deref() == Some(&b"abc123"[..]))
    );
}
