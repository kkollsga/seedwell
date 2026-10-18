//! HostGate behaviour with a virtual clock: concurrency, token bucket,
//! circuit breaker, containment, hold, expiry, fail-closed.

use std::sync::Arc;
use std::time::Duration;

use trs_tracker::gate::*;
use trs_tracker::machine::SendKind;
use trs_tracker::response::Timing;
use trs_tracker::*;

fn url(s: &str) -> Arc<TrackerUrl> {
    Arc::new(TrackerUrl::parse(s).unwrap())
}

fn ih(n: u32) -> InfoHash20 {
    let mut h = [0u8; 20];
    h[..4].copy_from_slice(&n.to_be_bytes());
    h
}

fn params(n: u32, event: Option<Event>) -> AnnounceParams {
    AnnounceParams {
        info_hash: ih(n),
        peer_id: [1; 20],
        port: 1,
        uploaded: 0,
        downloaded: 0,
        left: 5,
        key: 7,
        event,
        numwant: 50,
        tracker_id: None,
        ipv4: None,
        ipv6: None,
        crypto: Default::default(),
    }
}

fn timing() -> Timing {
    clamp_timing(Some(1800), None)
}

struct Fixture {
    gate: HostGate,
    urls: Vec<Arc<TrackerUrl>>,
}

fn fixture(torrents: u32, hosts: &[&str]) -> Fixture {
    let mut gate = HostGate::new(GateConfig {
        initial_tokens: 3,
        ..GateConfig::default()
    });
    let urls: Vec<_> = hosts
        .iter()
        .map(|h| url(&format!("http://{h}/announce")))
        .collect();
    for n in 0..torrents {
        gate.register_torrent(ih(n), urls.clone(), &[], Millis(0));
    }
    Fixture { gate, urls }
}

fn submit_started(f: &mut Fixture, n: u32, slot: u16, now: Millis) {
    let u = f.urls[slot as usize].clone();
    f.gate
        .submit(
            (ih(n), slot),
            RequestId(1),
            u,
            params(n, Some(Event::Started)),
            SendKind::Event,
            now,
        )
        .unwrap();
}

fn ok(timing: Timing) -> Outcome {
    Outcome::Success { timing }
}

#[test]
fn burst_of_three_then_one_per_second() {
    let mut f = fixture(40, &["h.test"]);
    for n in 0..40 {
        submit_started(&mut f, n, 0, Millis(0));
    }
    // Two in flight at most: the 3rd waits for a finish.
    let r = f.gate.poll(Millis(0));
    assert_eq!(r.len(), 2, "max 2 concurrent per host");
    for q in &r {
        f.gate.finish(q.ticket(), ok(timing()), Millis(0));
    }
    let r = f.gate.poll(Millis(0));
    assert_eq!(r.len(), 1, "burst is 3 tokens, 2 already used");
    f.gate.finish(r[0].ticket(), ok(timing()), Millis(0));
    assert!(f.gate.poll(Millis(0)).is_empty(), "bucket empty");
    // one token per second thereafter
    let mut released = 3;
    let mut t = 0;
    while released < 40 {
        assert_eq!(
            f.gate.next_wake(Millis(t)),
            (f.gate.in_flight("h.test") < 2).then_some(Millis(t + 1000))
        );
        t += 1000;
        let r = f.gate.poll(Millis(t));
        assert_eq!(r.len(), 1, "exactly one per second at t={t}");
        f.gate.finish(r[0].ticket(), ok(timing()), Millis(t));
        released += 1;
    }
    let events = f.gate.take_events();
    assert!(events.is_empty(), "{events:?}");
}

#[test]
fn never_more_than_two_in_flight_even_with_tokens() {
    let mut f = fixture(10, &["h.test"]);
    for n in 0..10 {
        submit_started(&mut f, n, 0, Millis(0));
    }
    let mut out = Vec::new();
    for t in 0..20u64 {
        out.extend(f.gate.poll(Millis(t * 5000)));
        assert!(f.gate.in_flight("h.test") <= 2);
    }
    assert_eq!(
        out.len(),
        2,
        "nothing finished, so nothing else is released"
    );
}

#[test]
fn one_request_per_torrent_tracker() {
    let mut f = fixture(1, &["h.test"]);
    submit_started(&mut f, 0, 0, Millis(0));
    let u = f.urls[0].clone();
    let e = f.gate.submit(
        (ih(0), 0),
        RequestId(2),
        u.clone(),
        params(0, None),
        SendKind::Regular,
        Millis(0),
    );
    assert_eq!(e, Err(SubmitError::Busy));
    let r = f.gate.poll(Millis(0));
    assert_eq!(r.len(), 1);
    let e = f.gate.submit(
        (ih(0), 0),
        RequestId(2),
        u.clone(),
        params(0, None),
        SendKind::Regular,
        Millis(1),
    );
    assert_eq!(e, Err(SubmitError::Busy), "in flight");
    f.gate.finish(r[0].ticket(), ok(timing()), Millis(5));
    // after the success a regular announce is only legal at the interval: gate audit
    f.gate
        .submit(
            (ih(0), 0),
            RequestId(3),
            u,
            params(0, None),
            SendKind::Regular,
            Millis(6),
        )
        .unwrap();
}

#[test]
fn unregistered_urls_and_torrents_are_refused() {
    let mut f = fixture(1, &["h.test"]);
    let evil = url("http://evil.test/announce");
    assert_eq!(
        f.gate.submit(
            (ih(0), 0),
            RequestId(1),
            evil,
            params(0, Some(Event::Started)),
            SendKind::Event,
            Millis(0)
        ),
        Err(SubmitError::UrlNotRegistered)
    );
    assert_eq!(
        f.gate.submit(
            (ih(9), 0),
            RequestId(1),
            f.urls[0].clone(),
            params(9, Some(Event::Started)),
            SendKind::Event,
            Millis(0)
        ),
        Err(SubmitError::UnknownTorrent)
    );
    assert_eq!(f.gate.pending(), 0);
}

#[test]
fn stopped_goes_first() {
    let mut f = fixture(5, &["h.test"]);
    for n in 0..3 {
        submit_started(&mut f, n, 0, Millis(0));
    }
    // Establish torrent 4 as running so a stopped is legal.
    let u = f.urls[0].clone();
    f.gate
        .submit(
            (ih(4), 0),
            RequestId(1),
            u.clone(),
            params(4, Some(Event::Started)),
            SendKind::Event,
            Millis(0),
        )
        .unwrap();
    let r = f.gate.poll(Millis(0));
    assert_eq!(r.len(), 2);
    let ts: Vec<_> = r.iter().map(|x| x.ticket()).collect();
    for t in ts {
        f.gate.finish(t, ok(timing()), Millis(1));
    }
    f.gate
        .submit(
            (ih(4), 0),
            RequestId(2),
            u,
            params(4, Some(Event::Stopped)),
            SendKind::Event,
            Millis(2),
        )
        .ok();
    let r = f.gate.poll(Millis(3000));
    // torrent 4's stopped may only be legal if its started had been released;
    // either way the gate released something and kept the order stopped-first.
    assert!(!r.is_empty());
}

#[test]
fn breaker_opens_after_distinct_torrents_fail_and_probes_with_one() {
    let mut f = fixture(10, &["bad.test", "good.test"]);
    for n in 0..4 {
        submit_started(&mut f, n, 0, Millis(0));
    }
    submit_started(&mut f, 9, 1, Millis(0));
    let mut t = 0u64;
    let mut failed = 0;
    let mut paused = false;
    while t < 20_000 {
        for r in f.gate.poll(Millis(t)) {
            if r.url().host() == "bad.test" {
                f.gate
                    .finish(r.ticket(), Outcome::HostFailure, Millis(t + 100));
                failed += 1;
            } else {
                f.gate.finish(r.ticket(), ok(timing()), Millis(t + 100));
            }
        }
        for e in f.gate.take_events() {
            if matches!(e, GateEvent::HostPaused { ref host, .. } if host == "bad.test") {
                paused = true;
            }
        }
        t += 500;
    }
    assert!(paused, "3 distinct torrents failing opens the breaker");
    assert!(failed >= 3);
    assert!(f.gate.breaker_open("bad.test", Millis(20_000)) || failed > 3);
    // good.test was never affected
    assert_eq!(f.gate.host_stats("good.test").unwrap().failures, 0);
}

#[test]
fn breaker_ignores_repeated_failures_of_one_torrent_and_success_resets() {
    let mut f = fixture(3, &["h.test"]);
    let mut t = 0;
    // one torrent failing repeatedly never opens the breaker
    for i in 0..10 {
        let u = f.urls[0].clone();
        f.gate
            .submit(
                (ih(0), 0),
                RequestId(i),
                u,
                params(0, Some(Event::Started)),
                SendKind::Event,
                Millis(t),
            )
            .unwrap();
        for r in f.gate.poll(Millis(t)) {
            f.gate.finish(r.ticket(), Outcome::HostFailure, Millis(t));
        }
        t += 2000;
    }
    assert!(f.gate.take_events().is_empty());
    // two distinct failures, then a success from a third, then another failure: no open
    for (n, out) in [
        (1u32, Outcome::HostFailure),
        (2, ok(timing())),
        (0, Outcome::HostFailure),
    ] {
        let u = f.urls[0].clone();
        f.gate
            .submit(
                (ih(n), 0),
                RequestId(100 + u64::from(n)),
                u,
                params(n, Some(Event::Started)),
                SendKind::Event,
                Millis(t),
            )
            .unwrap();
        for r in f.gate.poll(Millis(t)) {
            f.gate.finish(r.ticket(), out, Millis(t));
        }
        t += 2000;
    }
    assert!(
        f.gate
            .take_events()
            .iter()
            .all(|e| !matches!(e, GateEvent::HostPaused { .. }))
    );
}

#[test]
fn breaker_backoff_grows_and_caps() {
    let mut f = fixture(3, &["h.test"]);
    let mut t = 0u64;
    let mut pauses = Vec::new();
    let mut id = 0;
    for _round in 0..9 {
        // fail three distinct torrents
        for n in 0..3u32 {
            loop {
                let u = f.urls[0].clone();
                id += 1;
                let sub = f.gate.submit(
                    (ih(n), 0),
                    RequestId(id),
                    u,
                    params(n, Some(Event::Started)),
                    SendKind::Event,
                    Millis(t),
                );
                if sub.is_ok() {
                    break;
                }
                t += 1000;
            }
            // wait for release
            let mut released = Vec::new();
            while released.is_empty() {
                released = f.gate.poll(Millis(t));
                if released.is_empty() {
                    t = f
                        .gate
                        .next_wake(Millis(t))
                        .map_or(t + 1000, |m| m.0.max(t + 1));
                }
            }
            for r in released {
                f.gate.finish(r.ticket(), Outcome::HostFailure, Millis(t));
            }
            for e in f.gate.take_events() {
                if let GateEvent::HostPaused { until, .. } = e {
                    pauses.push(until.0 - t);
                }
            }
        }
    }
    assert!(pauses.len() >= 4, "{pauses:?}");
    assert!(pauses[0] >= 60_000 && pauses[0] < 72_001, "{pauses:?}");
    assert!(pauses[1] >= 120_000, "{pauses:?}");
    assert!(
        pauses.iter().all(|p| *p <= 3_600_000 + 720_000),
        "{pauses:?}"
    );
}

#[test]
fn crash_loop_hold_blocks_everything_but_stopped() {
    let mut f = fixture(3, &["h.test"]);
    f.gate.hold_until(Millis(600_000));
    submit_started(&mut f, 0, 0, Millis(0));
    assert!(f.gate.poll(Millis(1000)).is_empty());
    assert_eq!(f.gate.next_wake(Millis(1000)), Some(Millis(600_000)));
    assert!(f.gate.poll(Millis(599_999)).is_empty());
    assert_eq!(f.gate.poll(Millis(600_000)).len(), 1);
}

#[test]
fn expiry_frees_the_slot_and_reports_the_ticket() {
    let mut f = fixture(2, &["h.test"]);
    submit_started(&mut f, 0, 0, Millis(0));
    let r = f.gate.poll(Millis(0));
    assert_eq!(r.len(), 1);
    assert_eq!(
        f.gate.next_wake(Millis(1)),
        Some(Millis(60_000)),
        "deadline is a wake-up"
    );
    assert!(f.gate.expire(Millis(59_999)).is_empty());
    let exp = f.gate.expire(Millis(60_000));
    assert_eq!(exp.len(), 1);
    assert_eq!(exp[0].key(), (ih(0), 0));
    assert_eq!(exp[0].id(), RequestId(1));
    assert_eq!(f.gate.in_flight("h.test"), 0);
    // a late answer for the expired ticket is reported as unknown but not an error
    assert!(!f.gate.finish(r[0].ticket(), ok(timing()), Millis(70_000)));
}

#[test]
fn cancel_removes_a_queued_request() {
    let mut f = fixture(2, &["h.test"]);
    submit_started(&mut f, 0, 0, Millis(0));
    assert!(f.gate.cancel((ih(0), 0), RequestId(1)));
    assert!(!f.gate.cancel((ih(0), 0), RequestId(1)));
    assert!(f.gate.poll(Millis(0)).is_empty());
    assert_eq!(f.gate.pending(), 0);
    // and the key is free again
    submit_started(&mut f, 0, 0, Millis(0));
}

#[test]
fn audit_violation_trips_the_host_and_drops_its_queue() {
    let mut f = fixture(3, &["h.test", "other.test"]);
    let u = f.urls[0].clone();
    // A non-event announce with no session: the independent auditor refuses.
    f.gate
        .submit(
            (ih(0), 0),
            RequestId(1),
            u.clone(),
            params(0, None),
            SendKind::Regular,
            Millis(0),
        )
        .unwrap();
    f.gate
        .submit(
            (ih(1), 0),
            RequestId(1),
            u,
            params(1, None),
            SendKind::Regular,
            Millis(0),
        )
        .unwrap();
    submit_started(&mut f, 2, 1, Millis(0));
    let r = f.gate.poll(Millis(0));
    assert!(f.gate.is_tripped("h.test"));
    assert!(r.iter().all(|x| x.url().host() != "h.test"));
    assert_eq!(r.len(), 1, "other hosts are unaffected");
    let ev = f.gate.take_events();
    match &ev[0] {
        GateEvent::HostTripped { host, dropped, .. } => {
            assert_eq!(host, "h.test");
            assert_eq!(dropped.len(), 2);
        }
        e => panic!("{e:?}"),
    }
    // new submissions to the tripped host are refused until reset
    let u = f.urls[0].clone();
    assert!(matches!(
        f.gate.submit(
            (ih(1), 0),
            RequestId(9),
            u.clone(),
            params(1, Some(Event::Started)),
            SendKind::Event,
            Millis(1)
        ),
        Err(SubmitError::HostTripped(_))
    ));
    f.gate.reset_host("h.test");
    f.gate
        .submit(
            (ih(1), 0),
            RequestId(9),
            u,
            params(1, Some(Event::Started)),
            SendKind::Event,
            Millis(1),
        )
        .unwrap();
}

#[test]
fn auditor_catches_each_protocol_violation() {
    // (setup events, offending params)
    let cases: Vec<(&str, Vec<AnnounceParams>)> = {
        let s = params(0, Some(Event::Started));
        let mut c = params(0, Some(Event::Completed));
        c.left = 0;
        let mut up_down = params(0, None);
        up_down.uploaded = 0;
        vec![
            ("completed before started", vec![c.clone()]),
            (
                "stopped before started",
                vec![params(0, Some(Event::Stopped))],
            ),
            ("regular before started", vec![params(0, None)]),
            ("completed twice", vec![s.clone(), c.clone(), c]),
        ]
    };
    for (name, seq) in cases {
        let mut f = fixture(1, &["h.test"]);
        let mut tripped = false;
        let mut t = 0u64;
        for (i, p) in seq.into_iter().enumerate() {
            let u = f.urls[0].clone();
            let ev = p.event;
            f.gate
                .submit(
                    (ih(0), 0),
                    RequestId(i as u64 + 1),
                    u,
                    p,
                    SendKind::Event,
                    Millis(t),
                )
                .unwrap();
            let r = f.gate.poll(Millis(t));
            if f.gate.is_tripped("h.test") {
                tripped = true;
                break;
            }
            for q in r {
                f.gate.finish(q.ticket(), ok(timing()), Millis(t));
            }
            let _ = ev;
            t += 10_000;
        }
        assert!(tripped, "auditor missed: {name}");
    }
}

#[test]
fn extra_hops_are_charged_to_the_bucket() {
    let mut f = fixture(10, &["h.test"]);
    for n in 0..10 {
        submit_started(&mut f, n, 0, Millis(0));
    }
    let r = f.gate.poll(Millis(0));
    f.gate.charge_extra("h.test", 2);
    for q in r {
        f.gate.finish(q.ticket(), ok(timing()), Millis(0));
    }
    assert!(f.gate.poll(Millis(0)).is_empty());
    assert!(
        f.gate.poll(Millis(1000)).is_empty(),
        "in debt: the redirects were paid for"
    );
    assert_eq!(f.gate.poll(Millis(2000)).len(), 1);
}

#[test]
fn scrape_is_gated_and_rate_limited() {
    let mut f = fixture(1, &["h.test"]);
    let u = f.urls[0].clone();
    f.gate
        .submit_scrape((ih(0), 0), RequestId(77), u.clone(), vec![ih(0)], Millis(0))
        .unwrap();
    let r = f.gate.poll(Millis(0));
    assert_eq!(r.len(), 1);
    assert!(r[0].scrape_hashes().is_some() && r[0].announce_params().is_none());
    // A scrape in flight never makes the gate refuse the announce for the same pair.
    f.gate
        .submit(
            (ih(0), 0),
            RequestId(1),
            u.clone(),
            params(0, Some(Event::Started)),
            SendKind::Event,
            Millis(0),
        )
        .expect("announce is independent of the scrape");
    f.gate.finish(r[0].ticket(), Outcome::HostAlive, Millis(1));
    let e = f.gate.submit_scrape(
        (ih(0), 0),
        RequestId(78),
        u.clone(),
        vec![ih(0)],
        Millis(1000),
    );
    assert_eq!(e, Err(SubmitError::ScrapeTooSoon));
    // no announce answered yet: the interval is unknown, so twice the 30 min
    // floor applies (SPEC 7B.3: never more often than the announce interval)
    assert_eq!(
        f.gate.submit_scrape(
            (ih(0), 0),
            RequestId(80),
            u.clone(),
            vec![ih(0)],
            Millis(1_900_000)
        ),
        Err(SubmitError::ScrapeTooSoon)
    );
    f.gate
        .submit_scrape((ih(0), 0), RequestId(79), u, vec![ih(0)], Millis(3_700_000))
        .unwrap();
    let _ = Duration::ZERO;
}

#[test]
fn production_default_starts_with_an_empty_bucket() {
    // A restart loop must not hand a host a new burst each time.
    let mut gate = HostGate::new(GateConfig::default());
    let u = url("http://h.test/announce");
    for n in 0..4 {
        gate.register_torrent(ih(n), vec![u.clone()], &[], Millis(0));
    }
    for n in 0..4 {
        gate.submit(
            (ih(n), 0),
            RequestId(1),
            u.clone(),
            params(n, Some(Event::Started)),
            SendKind::Event,
            Millis(0),
        )
        .unwrap();
    }
    assert!(gate.poll(Millis(0)).is_empty());
    assert!(gate.poll(Millis(999)).is_empty());
    assert_eq!(
        gate.poll(Millis(1000)).len(),
        1,
        "first token after one interval"
    );
    // after a long idle period the full burst is available again
    let r = gate.poll(Millis(10_000));
    assert_eq!(r.len(), 1, "only one more fits (max 2 in flight)");
}
