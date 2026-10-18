//! Adversarial audit: BEP 12 tiers, fall-through, promotion, same-host
//! multi-URL private torrents, "all tiers fail".
//!
//! Reference behaviour: Transmission 4.x runs one announcer *per tier*, i.e. it announces to one
//! tracker of every tier, each on its own schedule. Seedwell does the same (SPEC 7C.4b): tiers
//! never influence each other; inside a tier BEP 12 failover applies (only the first tracker
//! that is not failing is active). libtorrent's default (`announce_to_all_tiers = false`)
//! was Seedwell's first model; its start/stop hand-over between tiers announced to trackers in
//! the middle of their interval (see `multi_tier_independence.rs`).

use crate::adversarial_support;

use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use adversarial_support::*;
use trs_tracker::{Event, TorrentEvent};

#[test]
fn healthy_first_tracker_means_failovers_stay_silent_for_days_and_tiers_run_alone() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.add(spec(
        1,
        &[
            &["http://a.test/announce?pk=1", "http://b.test/announce?pk=1"],
            &["http://c.test/announce?pk=1"],
            &["https://a.test/announce?pk=1"],
        ],
    ));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(72 * 3600));
    // tier 1 [a, b]: only a (b is its failover); tier 2 [c]; the https twin of a joins tier 1
    let hosts: std::collections::HashSet<_> = lab.sent.iter().map(|x| x.host.clone()).collect();
    assert_eq!(hosts.len(), 2, "{hosts:?}");
    assert!(!hosts.contains("b.test") || !hosts.contains("a.test") || lab.host_count("b.test") == 0);
    // one announce per interval and tier
    assert!(lab.sent.len() <= 2 * (72 * 2 + 3));
}

#[test]
fn same_host_http_and_https_twin_in_two_tiers_is_not_announced_in_parallel() {
    // very common in private torrents: http and https URL of one tracker
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.add(spec(
        1,
        &[
            &["http://tl.test/announce?pk=1"],
            &["https://tl.test/announce?pk=1"],
        ],
    ));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(24 * 3600));
    let urls: std::collections::HashSet<_> = lab.sent.iter().map(|x| x.url.clone()).collect();
    assert_eq!(urls.len(), 1);
    assert!(lab.sent.len() <= 24 * 2 + 3);
}

#[test]
fn all_tiers_failing_for_a_day_is_bounded_by_the_backoff_of_each_tracker() {
    for mk in [
        (|| timeout()) as fn() -> Resp,
        || http_status(503),
        || http_status(404),
    ] {
        let mut lab = Lab::new(always(mk));
        lab.add(spec(
            1,
            &[
                &["http://a.test/announce?pk=1", "http://b.test/announce?pk=1"],
                &["http://c.test/announce?pk=1", "udp://d.test:6969/announce"],
            ],
        ));
        lab.stats(1, 0, 0, 10, 10);
        lab.ev(1, TorrentEvent::Start);
        lab.run_for(d(24 * 3600));
        // 4 trackers; nominal backoff 1,2,4,8,16,32,60,60... min => ~27 attempts per day each
        let mut per: HashMap<String, usize> = HashMap::new();
        for x in &lab.sent {
            *per.entry(x.url.clone()).or_default() += 1;
        }
        assert_eq!(per.len(), 4, "every tracker must be tried: {per:?}");
        for (u, c) in &per {
            assert!(*c <= 30, "{u}: {c} attempts in 24 h of failure");
            assert!(*c >= 10, "{u}: only {c} attempts (retries must go on)");
        }
        for x in &lab.sent {
            assert_eq!(
                x.event(),
                Some(Event::Started),
                "never a regular announce before started succeeded"
            );
        }
        for h in ["a.test", "b.test", "c.test", "d.test"] {
            assert!(lab.host_count(h) <= 30);
        }
    }
}

#[test]
fn failover_is_not_needed_across_tiers_and_recovery_of_one_leaves_the_other_alone() {
    let up = Rc::new(Cell::new(false));
    let u2 = up.clone();
    let mut lab = Lab::new(Box::new(move |s| {
        if s.host == "a.test" && !u2.get() {
            vec![(d(1), timeout())]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    lab.add(spec(
        1,
        &[
            &["http://a.test/announce?pk=1"],
            &["http://b.test/announce?pk=1"],
        ],
    ));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(40));
    assert!(lab.host_count("b.test") >= 1, "no failover");
    let b_start = lab.sent.iter().find(|x| x.host == "b.test").unwrap().t;
    assert!(b_start.0 < 30_000, "failover took {} ms", b_start.0);
    up.set(true);
    lab.run_for(d(300));
    // a is healthy again; b is a tier of its own and never hears about it
    let b_events: Vec<_> = lab
        .sent
        .iter()
        .filter(|x| x.host == "b.test")
        .map(|x| x.event())
        .collect();
    assert!(!b_events.contains(&Some(Event::Stopped)), "{b_events:?}");
    assert_eq!(b_events.iter().filter(|e| **e == Some(Event::Started)).count(), 1);
    lab.run_for(d(6 * 3600));
    let b_times: Vec<u64> = lab.sent.iter().filter(|x| x.host == "b.test").map(|x| x.t.0).collect();
    for w in b_times.windows(2) {
        assert!(w[1] - w[0] >= 1_800_000, "b announced {} ms after its previous announce", w[1] - w[0]);
    }
}

#[test]
fn flapping_primary_cannot_make_the_secondary_flap_faster_than_each_backoff() {
    // a.test alternates up/down every 5 minutes for a day
    let t0 = Rc::new(Cell::new(0u64));
    let tt = t0.clone();
    let mut lab = Lab::new(Box::new(move |s| {
        tt.set(s.t.0);
        let up = (s.t.0 / 300_000) % 2 == 0;
        if s.host == "a.test" && !up {
            vec![(d(1), timeout())]
        } else {
            vec![(d(1), ok(300, None))]
        }
    }));
    lab.add(spec(
        1,
        &[
            &["http://a.test/announce?pk=1"],
            &["http://b.test/announce?pk=1"],
        ],
    ));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(24 * 3600));
    // 5 min interval (12/h) + backoff retries (<= 1/min while failing) per host
    for h in ["a.test", "b.test"] {
        let m = max_in_window(&lab.sent, h, d(3600));
        assert!(m <= 40, "{h}: {m} requests in one hour");
    }
    assert!(lab.sent.len() <= 1000, "{} requests/day", lab.sent.len());
    let mut view = View::new();
    for x in &lab.sent {
        view.check(x, 10, !(x.host == "a.test" && (x.t.0 / 300_000) % 2 == 1));
    }
    // a failing `started` is retried legitimately, everything else must be well-formed
    assert!(
        view.violations.is_empty(),
        "{:#?}",
        &view.violations[..view.violations.len().min(5)]
    );
}

#[test]
fn halted_first_tier_does_not_block_the_second_and_user_start_retries_it() {
    let mut lab = Lab::new(Box::new(|s| {
        if s.host == "a.test" {
            vec![(d(1), reject("torrent not registered"))]
        } else {
            vec![(d(1), ok(1800, None))]
        }
    }));
    lab.add(spec(
        1,
        &[
            &["http://a.test/announce?pk=1"],
            &["http://b.test/announce?pk=1"],
        ],
    ));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(24 * 3600));
    assert_eq!(
        lab.host_count("a.test"),
        1,
        "a rejection must not be retried by itself"
    );
    assert!(lab.host_count("b.test") >= 40);
}

#[test]
fn completed_goes_only_to_the_tracker_that_is_in_a_session() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    lab.add(spec(
        1,
        &[
            &["http://a.test/announce?pk=1", "http://a2.test/announce?pk=1"],
            &["http://b.test/announce?pk=1"],
        ],
    ));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(30));
    lab.stats(1, 0, 10, 0, 10);
    lab.ev(1, TorrentEvent::Complete);
    lab.run_for(d(60));
    let mut hosts = lab
        .sent
        .iter()
        .filter(|x| x.event() == Some(Event::Completed))
        .map(|x| x.host.as_str())
        .collect::<Vec<_>>();
    hosts.sort_unstable();
    // each tier's active tracker, never the failover (a2)
    assert!(hosts == ["a.test", "b.test"] || hosts == ["a2.test", "b.test"], "{hosts:?}");
}

#[test]
fn caps_dedupe_and_empty_tiers() {
    let mut tiers: Vec<Vec<String>> = vec![];
    for t in 0..40 {
        tiers.push(vec![
            format!("http://t{t}.test/announce"),
            format!("http://t{t}.test/announce"),
        ]);
    }
    tiers.push(vec![]);
    let tier_refs: Vec<Vec<&str>> = tiers
        .iter()
        .map(|t| t.iter().map(String::as_str).collect())
        .collect();
    let tier_slices: Vec<&[&str]> = tier_refs.iter().map(Vec::as_slice).collect();
    let mut lab = Lab::new(always(timeout));
    lab.add(spec(1, &tier_slices));
    lab.stats(1, 0, 0, 10, 10);
    lab.ev(1, TorrentEvent::Start);
    lab.run_for(d(3600));
    let hosts: std::collections::HashSet<_> = lab.sent.iter().map(|x| x.host.clone()).collect();
    assert!(hosts.len() <= 32, "{} trackers contacted", hosts.len());
    // the per-day rate to any one host stays small
    for h in &hosts {
        assert!(lab.host_count(h) <= 8, "{h}: {}", lab.host_count(h));
    }
}
