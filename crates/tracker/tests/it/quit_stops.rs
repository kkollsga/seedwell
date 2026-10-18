//! Quit-time `stopped` (SPEC 7B.3, 7C): after `begin_quit` the stops of hundreds of torrents on
//! one host leave together (up to `quit_max_in_flight` over all hosts, outside the per-host cap and
//! the token bucket); while the app runs, the per-host cap of 2 and the bucket are untouched.

use crate::adversarial_support;

use std::time::Duration;

use adversarial_support::*;
use trs_tracker::gate::GateConfig;
use trs_tracker::{Event, TorrentEvent};

const HOST: &str = "tl.test";

fn lab_with_started(n: u32, responder: Responder, cfg: GateConfig) -> Lab {
    let mut lab = Lab::with_cfg(responder, cfg);
    for i in 0..n {
        lab.add(spec(i, &[&["http://tl.test/announce?passkey=k"]]));
        lab.stats(i, 0, 0, 100, 100);
        lab.ev(i, TorrentEvent::Start);
    }
    lab.run_for(Duration::from_secs(u64::from(n) + 60)); // everyone announced `started`
    let started = lab.sent.iter().filter(|x| x.event() == Some(Event::Started)).count();
    assert_eq!(started, n as usize);
    lab
}

/// A tracker that answers regular announces at once and holds every `stopped` for `hold`.
fn slow_stops(hold: Duration) -> Responder {
    Box::new(move |s| {
        let delay = if s.event() == Some(Event::Stopped) { hold } else { Duration::from_millis(50) };
        vec![(delay, ok(1800, None))]
    })
}

/// Step the virtual clock in 20 ms slices for `total`, returning the highest (quit, host) in-flight seen.
fn watch(lab: &mut Lab, total: Duration) -> (usize, usize) {
    let (mut quit_max, mut host_max) = (0, 0);
    let end = lab.now + total;
    while lab.now < end {
        let t = lab.now + Duration::from_millis(20);
        lab.run_until(t);
        quit_max = quit_max.max(lab.hub_ref().gate().quit_in_flight());
        host_max = host_max.max(lab.hub_ref().gate().in_flight(HOST));
    }
    (quit_max, host_max)
}

fn stopped_after(lab: &Lab, t: trs_tracker::Millis) -> Vec<&Sent> {
    lab.sent.iter().filter(|x| x.t >= t && x.event() == Some(Event::Stopped)).collect()
}

#[test]
fn at_quit_300_torrents_on_one_host_exceed_the_host_cap_but_never_the_global_one() {
    let mut lab = lab_with_started(300, slow_stops(Duration::from_millis(1500)), GateConfig::default());
    let quit_at = lab.now;
    lab.hub().begin_quit();
    for n in 0..300 {
        lab.stats(n, 1000, 0, 100, 100);
        lab.quit(n);
    }
    let (quit_max, _) = watch(&mut lab, Duration::from_secs(20));
    assert!(quit_max > 2, "quit stops must not be held to the host cap of 2: {quit_max}");
    assert_eq!(quit_max, 50, "the global cap is the only limit, and it is reached");
    let stops = stopped_after(&lab, quit_at);
    assert_eq!(stops.len(), 300, "every stop is attempted exactly once");
    let mut per: std::collections::HashSet<_> = Default::default();
    assert!(stops.iter().all(|x| per.insert(x.ih)), "one stopped per torrent");
    // 300 stops, 50 at a time, 1.5 s each: six waves.
    let last = stops.iter().map(|x| x.t.0).max().unwrap() - quit_at.0;
    assert!((7_000..=9_500).contains(&last), "waves of 50: last stop sent {last} ms after the quit");
    // The first wave left at once, not at 1 per second.
    let first_wave = stops.iter().filter(|x| x.t.0 - quit_at.0 < 100).count();
    assert_eq!(first_wave, 50, "50 stops in the first 100 ms");
}

#[test]
fn quit_stops_that_hang_free_their_slot_at_the_quit_deadline() {
    let cfg = GateConfig { quit_stop_deadline: Duration::from_secs(2), ..GateConfig::default() };
    // `stopped` is never answered; the gate declares it timed out after 2 s.
    let responder: Responder = Box::new(|s| {
        if s.event() == Some(Event::Stopped) { vec![] } else { vec![(Duration::from_millis(50), ok(1800, None))] }
    });
    let mut lab = lab_with_started(300, responder, cfg);
    let quit_at = lab.now;
    lab.hub().begin_quit();
    for n in 0..300 {
        lab.quit(n);
    }
    let (quit_max, _) = watch(&mut lab, Duration::from_secs(14));
    assert_eq!(quit_max, 50);
    let stops = stopped_after(&lab, quit_at);
    assert_eq!(stops.len(), 300, "timeouts of the stops never open the host breaker: all are attempted");
    let last = stops.iter().map(|x| x.t.0).max().unwrap() - quit_at.0;
    assert!((10_000..=12_500).contains(&last), "six waves of 2 s: last stop sent {last} ms after the quit");
    assert!(!lab.hub_ref().gate().breaker_open(HOST, lab.now));
    assert_eq!(lab.hub_ref().gate().quit_in_flight(), 0, "all slots are free again");
}

#[test]
fn quit_stops_use_the_global_cap_across_hosts() {
    let mut lab = Lab::new(slow_stops(Duration::from_secs(1)));
    for i in 0..120u32 {
        let h = i % 3;
        let u = format!("http://t{h}.test/announce?passkey=k");
        lab.add(spec(i, &[&[u.as_str()]]));
        lab.stats(i, 0, 0, 100, 100);
        lab.ev(i, TorrentEvent::Start);
    }
    lab.run_for(Duration::from_secs(300));
    let quit_at = lab.now;
    lab.hub().begin_quit();
    for n in 0..120 {
        lab.quit(n);
    }
    let (quit_max, _) = watch(&mut lab, Duration::from_secs(6));
    assert_eq!(quit_max, 50, "50 over all hosts together");
    assert_eq!(stopped_after(&lab, quit_at).len(), 120);
}

#[test]
fn while_running_the_host_cap_of_two_and_the_bucket_still_hold() {
    let mut lab = lab_with_started(60, slow_stops(Duration::from_millis(1500)), GateConfig::default());
    let t0 = lab.now;
    // The user pauses everything (not a quit): no begin_quit.
    for n in 0..60 {
        lab.stats(n, 1000, 0, 100, 100);
        lab.ev(n, TorrentEvent::Stop);
    }
    let (quit_max, host_max) = watch(&mut lab, Duration::from_secs(30));
    assert_eq!(quit_max, 0, "the quit rules are off");
    assert_eq!(host_max, 2, "per-host cap of 2");
    let stops = stopped_after(&lab, t0);
    let mut times: Vec<u64> = stops.iter().map(|x| x.t.0).collect();
    times.sort_unstable();
    for w in times.windows(5) {
        assert!(w[4] - w[0] >= 2000, "bucket: 5 stopped within {} ms", w[4] - w[0]);
    }
}

#[test]
fn regular_announces_keep_their_pacing_while_the_app_quits() {
    // 100 torrents not yet started when the quit begins: their `started` still obeys the cap.
    let mut lab = Lab::new(slow_stops(Duration::from_secs(1)));
    for i in 0..100u32 {
        lab.add(spec(i, &[&["http://tl.test/announce?passkey=k"]]));
        lab.stats(i, 0, 0, 100, 100);
    }
    lab.hub().begin_quit();
    for i in 0..100 {
        lab.ev(i, TorrentEvent::Start);
    }
    let (quit_max, host_max) = watch(&mut lab, Duration::from_secs(20));
    assert_eq!(quit_max, 0);
    assert!(host_max <= 2, "{host_max}");
}
