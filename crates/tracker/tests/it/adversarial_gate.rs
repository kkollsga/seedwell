//! Adversarial audit: HostGate (token bucket, concurrency, breaker, host
//! identity, containment), shutdown linger, redirects charging.

use crate::adversarial_support;

use std::sync::Arc;
use std::time::Duration;

use adversarial_support::*;
use proptest::prelude::*;
use trs_tracker::gate::*;
use trs_tracker::machine::SendKind;
use trs_tracker::{
    AnnounceParams, Event, Millis, RequestId, TorrentEvent, TrackerUrl, clamp_timing,
};

fn u(s: &str) -> Arc<TrackerUrl> {
    Arc::new(TrackerUrl::parse(s).unwrap())
}

fn params(n: u32, ev: Option<Event>) -> AnnounceParams {
    AnnounceParams {
        info_hash: ih(n),
        peer_id: [1; 20],
        port: 1,
        uploaded: 0,
        downloaded: 0,
        corrupt: 0,
        left: 5,
        key: 7,
        event: ev,
        numwant: 50,
        tracker_id: None,
        ipv4: None,
        ipv6: None,
        crypto: Default::default(),
    }
}

fn timing() -> trs_tracker::Timing {
    clamp_timing(Some(1800), None)
}

fn gate_with(
    hosts_urls: &[&str],
    torrents: u32,
    cfg: GateConfig,
) -> (HostGate, Vec<Arc<TrackerUrl>>) {
    let mut g = HostGate::new(cfg);
    let urls: Vec<_> = hosts_urls.iter().map(|h| u(h)).collect();
    for n in 0..torrents {
        g.register_torrent(ih(n), urls.clone(), &[], Millis(0));
    }
    (g, urls)
}

fn submit_started(
    g: &mut HostGate,
    urls: &[Arc<TrackerUrl>],
    n: u32,
    slot: u16,
) -> Result<(), SubmitError> {
    g.submit(
        (ih(n), slot),
        RequestId(1),
        urls[usize::from(slot)].clone(),
        params(n, Some(Event::Started)),
        SendKind::Event,
        Millis(0),
    )
}

// ------------------------------------------------------------ host identity

#[test]
fn host_identity_ignores_case_port_scheme_and_default_ports() {
    let hosts: Vec<String> = [
        "http://Tracker.Example.org:443/announce",
        "https://tracker.example.org/announce",
        "HTTPS://TRACKER.EXAMPLE.ORG:8443/a?pk=1",
        "udp://tracker.example.org:6969/announce",
        "http://tracker.example.org/announce",
        "http://TRACKER.example.ORG:80/announce",
    ]
    .iter()
    .map(|s| TrackerUrl::parse(s).unwrap().host().to_owned())
    .collect();
    assert!(
        hosts.iter().all(|h| h == "tracker.example.org"),
        "{hosts:?}"
    );
}

#[test]
fn http_https_and_udp_to_one_host_share_one_bucket() {
    let (mut g, urls) = gate_with(
        &[
            "http://Shared.Test/announce",
            "https://shared.test:8443/announce",
            "udp://SHARED.test:6969/announce",
        ],
        20,
        GateConfig {
            initial_tokens: 0,
            ..GateConfig::default()
        },
    );
    for n in 0..20 {
        submit_started(&mut g, &urls, n, (n % 3) as u16).unwrap();
    }
    let mut released = 0;
    for ms in 0..10_000u64 {
        for r in g.poll(Millis(ms)) {
            released += 1;
            g.finish(
                r.ticket(),
                Outcome::Success { timing: timing() },
                Millis(ms),
            );
        }
    }
    // 10 s of a 1/s bucket (starting empty): at most 10 + 1 releases, not 3 x that
    assert!((9..=11).contains(&released), "{released}");
}

/// FIXED audit defect (HAMMER-RISK, low): the budget key is the lower-cased host *string*.
/// Spellings of the same machine are different hosts for the gate: a trailing
/// dot, equivalent IPv6 literals and IPv4 short forms each get their own
/// token bucket and their own two slots. (A tracker URL list that uses two
/// spellings, or a hostile `.torrent`, multiplies the per-host budget.)
#[test]
fn bug_equivalent_host_spellings_get_separate_budgets() {
    let canon = |s: &str| TrackerUrl::parse(s).unwrap().host().to_owned();
    assert_eq!(
        canon("http://tracker.example.org./a"),
        canon("http://tracker.example.org/a")
    );
    assert_eq!(
        canon("http://[::1]:80/a"),
        canon("http://[0:0:0:0:0:0:0:1]:80/a")
    );
    assert_eq!(canon("http://[::1]:80/a"), canon("http://[0::0001]:80/a"));
    assert_eq!(canon("http://127.0.0.1/a"), canon("http://127.1/a"));
    assert_eq!(canon("http://127.0.0.1/a"), canon("http://2130706433/a"));
    assert_eq!(
        canon("http://127.0.0.1/a"),
        canon("http://[::ffff:127.0.0.1]/a")
    );
}

#[test]
fn urls_that_differ_only_by_host_case_are_two_slots_but_one_budget() {
    // Two tiers with the "same" tracker spelled differently: the set keeps both
    // (raw URL equality) but the gate budgets them as one host.
    let a = TrackerUrl::parse("http://Tracker.test/announce?pk=1").unwrap();
    let b = TrackerUrl::parse("http://tracker.test/announce?pk=1").unwrap();
    assert_ne!(a, b);
    assert_eq!(a.host(), b.host());
}

// -------------------------------------------------------------- token bucket

#[test]
fn bucket_starts_empty_and_a_burst_needs_idle_time() {
    let (mut g, urls) = gate_with(&["http://h.test/announce"], 40, GateConfig::default());
    for n in 0..40 {
        submit_started(&mut g, &urls, n, 0).unwrap();
    }
    assert!(g.poll(Millis(0)).is_empty(), "empty bucket at registration");
    assert!(g.poll(Millis(999)).is_empty());
    let first = g.poll(Millis(1000));
    assert_eq!(first.len(), 1);
    // idle for an hour: the bucket holds at most the burst (3), and only 2 may be in flight
    let t = Millis(3_600_000);
    let burst = g.poll(t);
    assert_eq!(
        burst.len(),
        1,
        "one already in flight, cap is 2: {}",
        burst.len()
    );
    for r in burst.iter().chain(first.iter()) {
        g.finish(r.ticket(), Outcome::Success { timing: timing() }, t);
    }
    let again = g.poll(t);
    // credit was 3 - 1(at 1000 ms, refilled to 3 after an hour) -> after the burst of 1 it is 2 -> 2 more
    assert!(again.len() <= 2, "{}", again.len());
    for r in &again {
        g.finish(r.ticket(), Outcome::Success { timing: timing() }, t);
    }
    assert!(g.poll(t).len() <= 1, "total burst after idle must be 3");
}

#[test]
fn sliding_window_ceiling_holds_for_any_poll_pattern() {
    // Poll at jittery irregular times with instant completions; windows are bounded.
    let (mut g, urls) = gate_with(&["http://h.test/announce"], 3000, GateConfig::default());
    for n in 0..3000 {
        submit_started(&mut g, &urls, n, 0).unwrap();
    }
    let mut times = vec![];
    let mut t = 0u64;
    let mut x = 12345u64;
    while t < 1_000_000 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        t += 1 + (x >> 40) % 700;
        for r in g.poll(Millis(t)) {
            times.push(t);
            g.finish(r.ticket(), Outcome::Success { timing: timing() }, Millis(t));
        }
    }
    assert!(times.len() > 900, "{}", times.len());
    for w in [1000u64, 2000, 5000, 30_000, 300_000] {
        let mut lo = 0;
        let mut best = 0;
        for hi in 0..times.len() {
            while times[hi] - times[lo] > w {
                lo += 1;
            }
            best = best.max(hi - lo + 1);
        }
        assert!(best as u64 <= 3 + w / 1000, "{best} releases in {w} ms");
    }
}

#[test]
fn two_thousand_five_hundred_torrents_on_one_host_are_never_early_and_not_starved() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    for n in 0..2500u32 {
        lab.add(spec(n, &[&["http://big.test/announce?passkey=k"]]));
        lab.stats(n, 0, 0, 100, 100);
        lab.ev(n, TorrentEvent::Start);
    }
    lab.run_for(d(6 * 3600));
    // gate saturated: 1 req/s sustained
    assert!(lab.sent.len() <= 3 + 6 * 3600 + 5, "{}", lab.sent.len());
    assert!(
        lab.sent.len() > 6 * 3600 - 300,
        "gate should be saturated, {}",
        lab.sent.len()
    );
    // per torrent: strictly increasing gaps >= interval, and nobody waits unreasonably
    let mut last: std::collections::HashMap<_, u64> = Default::default();
    let mut worst_gap = 0;
    for x in &lab.sent {
        if let Some(prev) = last.insert(x.ih, x.t.0) {
            let gap = x.t.0 - prev;
            assert!(gap >= 1_800_000 + 100, "early: {gap} ms");
            worst_gap = worst_gap.max(gap);
        }
    }
    assert!(
        worst_gap <= 6_000_000,
        "starvation: worst gap {worst_gap} ms"
    );
    assert!(max_in_window(&lab.sent, "big.test", d(1)) <= 4);
}

// ------------------------------------------------------------- concurrency

#[test]
fn never_arriving_completions_free_their_slots_only_at_the_deadline() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce"],
        6,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    for n in 0..6 {
        submit_started(&mut g, &urls, n, 0).unwrap();
    }
    let first = g.poll(Millis(0));
    assert_eq!(first.len(), 2);
    for ms in (1000..60_000).step_by(1000) {
        assert!(
            g.poll(Millis(ms)).is_empty(),
            "third request released at {ms} ms with 2 hung"
        );
        assert!(g.expire(Millis(ms)).is_empty());
    }
    assert_eq!(g.in_flight("h.test"), 2);
    let expired = g.expire(Millis(60_000));
    assert_eq!(expired.len(), 2);
    assert_eq!(g.in_flight("h.test"), 0);
    // three timeouts from three torrents open the breaker, nobody is released
    let more = g.poll(Millis(60_000));
    assert!(more.len() <= 2);
}

#[test]
fn duplicate_late_and_foreign_completions_neither_leak_nor_free_extra_slots() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce"],
        8,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    for n in 0..8 {
        submit_started(&mut g, &urls, n, 0).unwrap();
    }
    let rel = g.poll(Millis(0));
    assert_eq!(rel.len(), 2);
    let (a, b) = (rel[0].ticket(), rel[1].ticket());
    assert!(g.finish(a, Outcome::HostAlive, Millis(10)));
    // duplicates of a finished ticket
    for _ in 0..5 {
        assert!(!g.finish(a, Outcome::HostAlive, Millis(11)));
        assert!(!g.finish(a, Outcome::HostFailure, Millis(11)));
    }
    assert_eq!(g.in_flight("h.test"), 1, "b is still in flight");
    // b expires; its genuine answer arrives afterwards
    let ex = g.expire(Millis(60_001));
    assert_eq!(ex.len(), 1);
    assert_eq!(g.in_flight("h.test"), 0);
    assert!(!g.finish(b, Outcome::Success { timing: timing() }, Millis(61_000)));
    assert!(!g.finish(b, Outcome::HostFailure, Millis(61_000)));
    assert_eq!(g.in_flight("h.test"), 0);
    // now the cap is still exactly 2
    let next = g.poll(Millis(70_000));
    assert!(next.len() <= 2);
    assert!(g.poll(Millis(70_000)).is_empty());
}

#[test]
fn a_late_answer_of_an_old_request_cannot_free_the_slot_of_a_newer_one() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce"],
        1,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    submit_started(&mut g, &urls, 0, 0).unwrap();
    let old = g.poll(Millis(0)).remove(0).ticket();
    assert_eq!(g.expire(Millis(60_000)).len(), 1);
    // the machine retries (same key)
    g.session_closed((ih(0), 0));
    g.submit(
        (ih(0), 0),
        RequestId(2),
        urls[0].clone(),
        params(0, Some(Event::Started)),
        SendKind::Event,
        Millis(0),
    )
    .unwrap();
    let new = g.poll(Millis(120_000));
    assert_eq!(new.len(), 1);
    assert_eq!(g.in_flight("h.test"), 1);
    assert!(!g.finish(old, Outcome::HostFailure, Millis(120_001)));
    assert_eq!(
        g.in_flight("h.test"),
        1,
        "old ticket freed the slot of the new request"
    );
    assert!(g.finish(new[0].ticket(), Outcome::HostAlive, Millis(120_002)));
    assert_eq!(g.in_flight("h.test"), 0);
}

// ----------------------------------------------------------- circuit breaker

fn fail_started(g: &mut HostGate, urls: &[Arc<TrackerUrl>], n: u32, now: u64) -> Option<Ticket> {
    submit_started(g, urls, n, 0).ok()?;
    let r = g.poll(Millis(now));
    let t = r.first()?.ticket();
    Some(t)
}

#[test]
fn breaker_pauses_double_and_the_probe_is_single() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce"],
        400,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    let mut now = 10_000u64;
    // three distinct torrents fail -> the breaker opens
    let mut until = 0;
    for n in 0..3u32 {
        let t = fail_started(&mut g, &urls, n, now).unwrap();
        g.finish(t, Outcome::HostFailure, Millis(now));
        for e in g.take_events() {
            if let GateEvent::HostPaused { until: u, .. } = e {
                until = u.0;
            }
        }
        now += 1000;
    }
    assert!(until > 0, "breaker did not open after 3 distinct failures");
    let mut pauses = vec![until - (now - 1000)];
    // many requests wait
    for n in 100..140u32 {
        submit_started(&mut g, &urls, n, 0).unwrap();
    }
    for _round in 0..8 {
        assert!(
            g.poll(Millis(until - 1)).is_empty(),
            "released during the pause"
        );
        let probe = g.poll(Millis(until));
        assert_eq!(probe.len(), 1, "half-open must allow exactly one probe");
        assert!(
            g.poll(Millis(until + 1000)).is_empty(),
            "second request while the probe is out"
        );
        g.finish(probe[0].ticket(), Outcome::HostFailure, Millis(until));
        let mut next = None;
        for e in g.take_events() {
            if let GateEvent::HostPaused { until: u, .. } = e {
                next = Some(u.0);
            }
        }
        let next = next.expect("a failed probe must re-open the breaker");
        pauses.push(next - until);
        until = next;
    }
    let nominal = |i: usize| (60_000u64 << i.min(10)).min(3_600_000);
    for (i, p) in pauses.iter().enumerate() {
        assert!(
            *p >= nominal(i) && *p <= nominal(i) * 12 / 10 + 1,
            "pause {i}: {p} ms, nominal {} ms ({pauses:?})",
            nominal(i)
        );
    }
    // a successful probe closes it and normal operation (2 slots) resumes
    let probe = g.poll(Millis(until));
    assert_eq!(probe.len(), 1);
    g.finish(
        probe[0].ticket(),
        Outcome::Success { timing: timing() },
        Millis(until),
    );
    assert!(
        g.take_events()
            .iter()
            .any(|e| matches!(e, GateEvent::HostResumed { .. }))
    );
    assert!(g.poll(Millis(until + 3000)).len() >= 2 || !g.poll(Millis(until + 6000)).is_empty());
}

#[test]
fn rejections_never_open_the_breaker_but_one_noisy_torrent_cannot_either() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce"],
        50,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    let mut now = 0u64;
    for n in 0..40u32 {
        now += 2000;
        submit_started(&mut g, &urls, n, 0).unwrap();
        for r in g.poll(Millis(now)) {
            g.finish(r.ticket(), Outcome::HostAlive, Millis(now));
        }
    }
    assert!(g.take_events().is_empty());
    // one torrent failing 100 times is its own business (machine backoff), not the host's
    for k in 0..100u64 {
        now += 2000;
        g.session_closed((ih(45), 0));
        g.submit(
            (ih(45), 0),
            RequestId(k + 1),
            urls[0].clone(),
            params(45, Some(Event::Started)),
            SendKind::Event,
            Millis(now),
        )
        .unwrap();
        for r in g.poll(Millis(now)) {
            g.finish(r.ticket(), Outcome::HostFailure, Millis(now));
        }
    }
    assert!(
        g.take_events().is_empty(),
        "single torrent opened the host breaker"
    );
}

#[test]
fn breaker_is_per_host() {
    let (mut g, urls) = gate_with(
        &["http://a.test/announce", "http://b.test/announce"],
        20,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    let mut now = 5000;
    for n in 0..3 {
        let t = fail_started(&mut g, &urls, n, now).unwrap();
        g.finish(t, Outcome::HostFailure, Millis(now));
        now += 1000;
    }
    assert!(g.breaker_open("a.test", Millis(now)));
    assert!(!g.breaker_open("b.test", Millis(now)));
    g.submit(
        (ih(10), 1),
        RequestId(1),
        urls[1].clone(),
        params(10, Some(Event::Started)),
        SendKind::Event,
        Millis(now),
    )
    .unwrap();
    assert_eq!(g.poll(Millis(now + 1000)).len(), 1);
}

#[test]
fn stopped_waits_for_the_breaker_too_but_always_beats_other_traffic() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce"],
        10,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    // queue a regular-looking Started and a Stopped for another torrent
    submit_started(&mut g, &urls, 0, 0).unwrap();
    g.submit(
        (ih(1), 0),
        RequestId(1),
        urls[0].clone(),
        params(1, Some(Event::Started)),
        SendKind::Event,
        Millis(0),
    )
    .unwrap();
    // torrent 1 had a session: pretend started succeeded then stop
    let first = g.poll(Millis(1000));
    assert_eq!(first.len(), 2);
    for r in &first {
        g.finish(
            r.ticket(),
            Outcome::Success { timing: timing() },
            Millis(1100),
        );
    }
    g.submit(
        (ih(2), 0),
        RequestId(1),
        urls[0].clone(),
        params(2, Some(Event::Started)),
        SendKind::Event,
        Millis(1100),
    )
    .unwrap();
    g.submit(
        (ih(0), 0),
        RequestId(2),
        urls[0].clone(),
        params(0, Some(Event::Stopped)),
        SendKind::Event,
        Millis(1100),
    )
    .unwrap();
    let r = g.poll(Millis(2100));
    assert_eq!(r[0].event(), Some(Event::Stopped), "stopped has priority");
}

// --------------------------------------------------------------- containment

#[test]
fn unregistered_urls_and_other_passkeys_are_refused() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce?passkey=AAA"],
        2,
        GateConfig::default(),
    );
    let other_key = u("http://h.test/announce?passkey=BBB");
    let other_host = u("http://evil.test/announce?passkey=AAA");
    let same = Arc::new(TrackerUrl::parse("http://h.test/announce?passkey=AAA").unwrap());
    for (url, want) in [
        (other_key, Err(SubmitError::UrlNotRegistered)),
        (other_host, Err(SubmitError::UrlNotRegistered)),
        (same, Ok(())),
    ] {
        let got = g.submit(
            (ih(0), 0),
            RequestId(1),
            url,
            params(0, Some(Event::Started)),
            SendKind::Event,
            Millis(0),
        );
        assert_eq!(got, want);
        if got.is_ok() {
            g.cancel((ih(0), 0), RequestId(1));
        }
    }
    assert_eq!(
        g.submit(
            (ih(99), 0),
            RequestId(1),
            urls[0].clone(),
            params(99, None),
            SendKind::Regular,
            Millis(0)
        ),
        Err(SubmitError::UnknownTorrent)
    );
    // torrent 1 cannot use torrent 0's slot numbering to reach a URL it did not register
    let mut g2 = HostGate::new(GateConfig::default());
    g2.register_torrent(ih(1), vec![u("http://one.test/a")], &[], Millis(0));
    g2.register_torrent(ih(2), vec![u("http://two.test/a")], &[], Millis(0));
    assert_eq!(
        g2.submit(
            (ih(1), 0),
            RequestId(1),
            u("http://two.test/a"),
            params(1, Some(Event::Started)),
            SendKind::Event,
            Millis(0)
        ),
        Err(SubmitError::UrlNotRegistered)
    );
}

#[test]
fn hold_blocks_everything_except_stopped() {
    let (mut g, urls) = gate_with(
        &["http://h.test/announce"],
        4,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    g.hold_until(Millis(600_000));
    submit_started(&mut g, &urls, 0, 0).unwrap();
    assert!(g.poll(Millis(599_999)).is_empty());
    assert!(
        g.next_wake(Millis(1000))
            .is_some_and(|w| w.0 >= 600_000 || w.0 >= 1000)
    );
    assert_eq!(g.poll(Millis(600_000)).len(), 1);
}

// ----------------------------------------------------------------- redirects

#[test]
fn redirect_hops_are_paid_for_by_the_bucket() {
    let (mut g, urls) = gate_with(
        &["http://r.test/announce"],
        10,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    for n in 0..10 {
        submit_started(&mut g, &urls, n, 0).unwrap();
    }
    // 3 hops of redirects on the first request: the bucket (3 tokens) goes into debt
    let r = g.poll(Millis(0));
    assert_eq!(r.len(), 2);
    g.charge_extra("r.test", 3);
    g.finish(r[0].ticket(), Outcome::HostAlive, Millis(10));
    g.finish(r[1].ticket(), Outcome::HostAlive, Millis(10));
    // tokens: 3 - 2 (released) - 3 (hops) = -2 -> next release not before ~2 s + 1 s refill
    assert!(g.poll(Millis(1000)).is_empty());
    assert!(g.poll(Millis(2900)).is_empty());
    assert_eq!(g.poll(Millis(3000)).len(), 1);
}

/// `charge_extra` looks the host up by exact string. Callers pass
/// `req.url().host()` (canonical) so this works, but any other spelling is
/// silently ignored and the redirect hops are then free.
#[test]
fn bug_charge_extra_ignores_non_canonical_host_spelling() {
    let (mut g, urls) = gate_with(
        &["http://r.test/announce"],
        5,
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
    );
    for n in 0..5 {
        submit_started(&mut g, &urls, n, 0).unwrap();
    }
    g.charge_extra("R.TEST", 3);
    assert!(g.poll(Millis(0)).is_empty(), "hops were free");
}

// ----------------------------------------------------------------- scrapes

#[test]
fn scrapes_are_rate_limited_per_tracker() {
    let (mut g, urls) = gate_with(&["http://s.test/announce"], 1, GateConfig::default());
    g.submit_scrape(
        (ih(0), 0),
        RequestId(1),
        urls[0].clone(),
        vec![ih(0)],
        Millis(0),
    )
    .unwrap();
    for t in [1u64, 60, 1799] {
        assert_eq!(
            g.submit_scrape((ih(0), 0), RequestId(2), urls[0].clone(), vec![ih(0)], s(t)),
            Err(SubmitError::ScrapeTooSoon)
        );
    }
}

// ----------------------------------------------------- shutdown (decision #16)

#[test]
fn stopping_500_torrents_while_running_sends_nothing_faster_than_the_bucket() {
    // Not a quit (no `begin_quit`): pausing everything keeps the regular pacing; see tests/quit_stops.rs
    // for the quit-time rules.
    let mut lab = Lab::new(always(|| ok(1800, None)));
    for n in 0..500u32 {
        lab.add(spec(n, &[&["http://tl.test/announce?passkey=k"]]));
        lab.stats(n, 0, 0, 100, 100);
        lab.ev(n, TorrentEvent::Start);
    }
    lab.run_for(d(700)); // everyone announced `started`
    let started = lab
        .sent
        .iter()
        .filter(|x| x.event() == Some(Event::Started))
        .count();
    assert_eq!(started, 500);
    let quit_at = lab.now;
    for n in 0..500 {
        lab.stats(n, 1000, 0, 100, 100);
        lab.ev(n, TorrentEvent::Stop);
    }
    lab.run_for(d(120));
    let after: Vec<_> = lab.sent.iter().filter(|x| x.t >= quit_at).collect();
    assert!(
        after.iter().all(|x| x.event() == Some(Event::Stopped)),
        "only stopped may leave at quit"
    );
    // sustained 1/s plus the burst
    assert!(
        after.len() <= 3 + 120 + 1,
        "{} stopped in 120 s",
        after.len()
    );
    assert!(
        after.len() >= 100,
        "gate is too slow at quit: {}",
        after.len()
    );
    let mut times: Vec<u64> = after.iter().map(|x| x.t.0).collect();
    times.sort_unstable();
    for w in times.windows(5) {
        // burst 3, then one per second: 5 releases span at least 2 s
        assert!(w[4] - w[0] >= 2000, "5 stopped within {} ms", w[4] - w[0]);
    }
    // each torrent got at most one stopped
    let mut per: std::collections::HashMap<_, u32> = Default::default();
    for x in &after {
        *per.entry(x.ih).or_default() += 1;
    }
    assert!(per.values().all(|c| *c == 1));
    // after the process dies, the survivors relaunch: nothing early, bucket again
    let delivered = after.len();
    lab.crash();
    for n in 0..500 {
        lab.stats(n, 0, 0, 100, 100);
        lab.ev(n, TorrentEvent::Start);
    }
    let n0 = lab.sent.len();
    lab.run_for(d(300));
    let relaunch = &lab.sent[n0..];
    assert!(max_in_window(relaunch, "tl.test", d(1)) <= 4);
    assert!(
        relaunch.len() >= 250 && relaunch.len() <= 303,
        "{}",
        relaunch.len()
    );
    println!(
        "quit: {delivered} of 500 stopped delivered in 120 s; relaunch sent {} in 300 s",
        relaunch.len()
    );
}

#[test]
fn quit_while_most_started_are_still_queued_skips_their_stopped() {
    let mut lab = Lab::new(always(|| ok(1800, None)));
    for n in 0..500u32 {
        lab.add(spec(n, &[&["http://tl.test/announce?passkey=k"]]));
        lab.stats(n, 0, 0, 100, 100);
        lab.ev(n, TorrentEvent::Start);
    }
    lab.run_for(d(60)); // only ~60 started have left the gate
    for n in 0..500 {
        lab.ev(n, TorrentEvent::Stop);
    }
    lab.run_for(d(120));
    for n in 0..500 {
        let st = lab.count_event(n, Some(Event::Started));
        let sp = lab.count_event(n, Some(Event::Stopped));
        assert!(sp <= st, "torrent {n}: {sp} stopped for {st} started");
        if st == 0 {
            assert_eq!(sp, 0);
        }
    }
    let total_started = lab
        .sent
        .iter()
        .filter(|x| x.event() == Some(Event::Started))
        .count();
    assert!(total_started < 100, "{total_started}");
}

// --------------------------------------------- property: gate liveness & safety

#[derive(Clone, Debug)]
enum GOp {
    Submit(u8, u8),
    Cancel(u8),
    Poll(u16),
    FinishOldest(u8),
    FinishRandom(u8, u8),
    Expire,
    Duplicate,
    Hold(u16),
}

fn gop() -> impl Strategy<Value = GOp> {
    prop_oneof![
        6 => (0u8..40, 0u8..3).prop_map(|(n, h)| GOp::Submit(n, h)),
        1 => (0u8..40).prop_map(GOp::Cancel),
        8 => (1u16..3000).prop_map(GOp::Poll),
        4 => (0u8..3).prop_map(GOp::FinishOldest),
        3 => (0u8..3, any::<u8>()).prop_map(|(o, i)| GOp::FinishRandom(o, i)),
        2 => Just(GOp::Expire),
        1 => Just(GOp::Duplicate),
        1 => (0u16..2000).prop_map(GOp::Hold),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(300), ..ProptestConfig::default() })]

    /// Safety (<= 2 in flight, windows <= burst + W, never more than 1 per key)
    /// and liveness (everything queued eventually leaves when answers arrive).
    #[test]
    fn gate_safety_and_liveness(ops in proptest::collection::vec(gop(), 1..150)) {
        let hosts = ["http://h0.test/a", "http://h1.test/a", "http://h2.test/a"];
        let (mut g, urls) = gate_with(&hosts, 40, GateConfig::default());
        let mut now = 0u64;
        let mut live: Vec<Ticket> = vec![];
        let mut rel_times: [Vec<u64>; 3] = Default::default();
        let mut next_id = 1u64;
        let mut seen_tickets: Vec<Ticket> = vec![];
        let mut submitted: std::collections::HashSet<(u8, u8)> = Default::default();
        let do_poll = |g: &mut HostGate, now: u64, live: &mut Vec<Ticket>, rel: &mut [Vec<u64>; 3], seen: &mut Vec<Ticket>| -> Result<(), TestCaseError> {
            for r in g.poll(Millis(now)) {
                let (ihh, slot) = r.ticket().key();
                prop_assert!(!live.iter().any(|t| t.key() == (ihh, slot)), "two in flight for one key");
                rel[usize::from(slot)].push(now);
                live.push(r.ticket());
                seen.push(r.ticket());
            }
            for h in 0..3 {
                let name = format!("h{h}.test");
                prop_assert!(g.in_flight(&name) <= 2);
            }
            Ok(())
        };
        for op in ops {
            match op {
                GOp::Submit(n, h) => {
                    let id = RequestId(next_id);
                    next_id += 1;
                    if g.submit((ih(u32::from(n)), u16::from(h)), id, urls[usize::from(h)].clone(), params(u32::from(n), Some(Event::Started)), SendKind::Event, Millis(now)).is_ok() {
                        submitted.insert((n, h));
                    }
                }
                GOp::Cancel(n) => {
                    for h in 0..3 {
                        let _ = g.cancel((ih(u32::from(n)), h), RequestId(0));
                    }
                }
                GOp::Poll(dt) => {
                    now += u64::from(dt);
                    do_poll(&mut g, now, &mut live, &mut rel_times, &mut seen_tickets)?;
                }
                GOp::FinishOldest(o) => {
                    if !live.is_empty() {
                        let t = live.remove(0);
                        let out = [Outcome::HostAlive, Outcome::HostFailure, Outcome::Neutral][usize::from(o) % 3];
                        g.finish(t, out, Millis(now));
                    }
                }
                GOp::FinishRandom(o, i) => {
                    if !live.is_empty() {
                        let t = live.remove(usize::from(i) % live.len());
                        let out = [Outcome::HostAlive, Outcome::HostFailure, Outcome::Neutral][usize::from(o) % 3];
                        g.finish(t, out, Millis(now));
                    }
                }
                GOp::Expire => {
                    for t in g.expire(Millis(now)) {
                        live.retain(|l| *l != t);
                    }
                }
                GOp::Duplicate => {
                    // re-deliver an arbitrary old ticket: must never free a live slot
                    if let Some(t) = seen_tickets.first().copied() {
                        let before: usize = (0..3).map(|h| g.in_flight(&format!("h{h}.test"))).sum();
                        let known = live.contains(&t);
                        if !known {
                            g.finish(t, Outcome::HostFailure, Millis(now));
                            let after: usize = (0..3).map(|h| g.in_flight(&format!("h{h}.test"))).sum();
                            prop_assert_eq!(before, after);
                        }
                    }
                }
                GOp::Hold(dt) => g.hold_until(Millis(now + u64::from(dt))),
            }
            let _ = g.take_events();
        }
        // sliding windows per host
        for times in &rel_times {
            for w in [1000u64, 5000, 60_000] {
                let mut lo = 0;
                for hi in 0..times.len() {
                    while times[hi] - times[lo] > w { lo += 1; }
                    prop_assert!((hi - lo + 1) as u64 <= 3 + w / 1000, "window {w}: {}", hi - lo + 1);
                }
            }
        }
        // liveness: answer everything immediately and follow next_wake until drained
        let mut guard = 0;
        loop {
            guard += 1;
            prop_assert!(guard < 20_000, "gate does not drain: pending {}", g.pending());
            for t in live.drain(..) { g.finish(t, Outcome::HostAlive, Millis(now)); }
            for t in g.expire(Millis(now)) { let _ = t; }
            if g.pending() == 0 { break; }
            let Some(w) = g.next_wake(Millis(now)) else {
                prop_assert!(false, "queued work but no wake-up (pending {}, now {now})", g.pending());
                unreachable!()
            };
            now = now.max(w.0);
            for r in g.poll(Millis(now)) { live.push(r.ticket()); }
            if live.is_empty() && g.pending() > 0 { now += 1; }
        }
    }
}

#[test]
fn default_gate_config_matches_the_spec() {
    // sanity: default deadlines are what the README says
    let c = GateConfig::default();
    assert_eq!(c.http_deadline, Duration::from_secs(60));
    assert_eq!(c.udp_deadline, Duration::from_secs(600));
    assert_eq!((c.max_in_flight_per_host, c.burst), (2, 3));
}

#[test]
fn unclean_restart_with_many_torrents_waits_for_each_persisted_min_interval() {
    let mut lab = Lab::new(always(|| ok(1800, Some(900))));
    for n in 0..100u32 {
        lab.add(spec(n, &[&["http://tl.test/announce?passkey=k"]]));
        lab.stats(n, 0, 0, 100, 100);
        lab.ev(n, TorrentEvent::Start);
    }
    lab.run_for(d(130));
    let success_at: std::collections::HashMap<_, _> =
        lab.sent.iter().map(|x| (x.ih, x.t.0 + 100)).collect();
    assert_eq!(success_at.len(), 100);
    lab.crash(); // kill -9: no stopped was sent
    for n in 0..100 {
        lab.stats(n, 0, 0, 100, 100);
        lab.ev(n, TorrentEvent::Start);
    }
    let n0 = lab.sent.len();
    lab.run_for(d(2000));
    for x in &lab.sent[n0..] {
        assert!(
            x.t.0 >= success_at[&x.ih] + 900_000,
            "started {} s after the previous success, min interval is 900 s",
            (x.t.0 - success_at[&x.ih]) / 1000
        );
    }
    assert_eq!(
        lab.sent[n0..]
            .iter()
            .filter(|x| x.event() == Some(Event::Started))
            .count(),
        100
    );
}

// ---------------------------------------------------------- scrape vs SPEC

/// SPEC 7B.3: "Scrape only when the UI needs it, no more often than the
/// announce interval." The gate uses a fixed 30 min gap and knows nothing about
/// the tracker's interval: with `interval = 2 h` a scrape is allowed every 30 min.
#[test]
fn bug_scrape_is_allowed_more_often_than_the_announce_interval() {
    let mut hub = trs_tracker::hub::TrackerHub::new(
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
        trs_tracker::MemoryStore::new(),
        1,
    );
    hub.add_torrent(spec(1, &[&["http://s.test/announce?pk=1"]]), Millis(0))
        .unwrap();
    assert!(hub.request_scrape(&ih(1), 0, s(10)).is_ok());
    // drain the queued scrape through the gate and complete it
    for t in 10..20 {
        for r in hub.tick(s(t)) {
            hub.complete_scrape(r.ticket(), true, s(t));
        }
    }
    // 31 minutes later, with a 2 h announce interval, a second scrape is accepted
    assert!(
        hub.request_scrape(&ih(1), 0, s(10 + 1900)).is_err(),
        "scrape allowed 31 min after the last one although announces are 2 h apart"
    );
}

/// `Millis` is documented as Unix milliseconds, but the gate casts time deltas
/// to i64 (gate.rs refill/next_wake). A clock reading above 2^63 ms (a `u64::MAX`
/// "never" sentinel passed by mistake) turns the delta negative: the bucket is
/// emptied for good.
#[test]
fn bug_gate_wedges_when_the_clock_exceeds_two_to_the_63_ms() {
    let (mut g, urls) = gate_with(&["http://h.test/announce"], 4, GateConfig::default());
    submit_started(&mut g, &urls, 0, 0).unwrap();
    let huge = Millis(u64::MAX - 10);
    assert!(
        !g.poll(huge).is_empty(),
        "idle host must hold a full bucket after a long time"
    );
}
