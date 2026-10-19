//! Outage detection and recovery (SPEC 7C.4c): a virtual-time simulation of 300 seeding torrents
//! on 3 hosts through outages of every kind. Checked: during an outage only probes leave, no
//! torrent's backoff escalates, recovery releases exactly the announces that came due, spread
//! and paced per host, and a single dead tracker is never an outage.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use trs_tracker::gate::{GateConfig, Ticket};
use trs_tracker::hub::{TorrentSpec, TrackerHub};
use trs_tracker::outage::{NetEvent, Reason};
use trs_tracker::response::{AnnounceError, AnnounceResponse, TransportError};
use trs_tracker::{InfoHash20, MemoryStore, Millis, Stats, TorrentEvent, TorrentRole, TrackerUrl};

const HOSTS: [&str; 3] = ["a.test", "b.test", "c.test"];
const N: usize = 300;
const INTERVAL: u64 = 1800;

fn secs(s: u64) -> Millis {
    Millis::from_secs(s)
}

fn ok(interval: u64) -> AnnounceResponse {
    let t = trs_tracker::clamp_timing(Some(interval as i64), None);
    AnnounceResponse {
        interval: t.interval,
        min_interval: t.min_interval,
        tracker_id: None,
        complete: None,
        incomplete: None,
        downloaded: None,
        peers: vec![],
        warning: None,
        external_ip: None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Net {
    Up,
    /// DNS fails at once.
    Dns,
    /// Connects time out (a captive portal or a dead uplink behind a "satisfied" path).
    ConnectTimeout,
}

type Pending = (Ticket, &'static str, Result<AnnounceResponse, AnnounceError>);

#[derive(Clone, Debug)]
struct Req {
    at: Millis,
    ih: InfoHash20,
    host: &'static str,
    event: Option<trs_tracker::Event>,
}

struct Sim {
    hub: TrackerHub<MemoryStore>,
    now: Millis,
    pend: BTreeMap<(Millis, u64), Pending>,
    seq: u64,
    log: Vec<Req>,
    net: Box<dyn FnMut(Millis, &'static str) -> Net>,
    in_flight: HashMap<&'static str, usize>,
    max_in_flight: usize,
    max_failures: u32,
    events: Vec<NetEvent>,
    ihs: Vec<InfoHash20>,
}

impl Sim {
    fn new(net: impl FnMut(Millis, &'static str) -> Net + 'static) -> Sim {
        Sim::with(N, net)
    }

    fn with(n: usize, net: impl FnMut(Millis, &'static str) -> Net + 'static) -> Sim {
        let mut hub = TrackerHub::new(GateConfig::default(), MemoryStore::new(), 7);
        let mut ihs = Vec::new();
        for i in 0..n {
            let mut ih = [0u8; 20];
            ih[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
            ih[8] = 0xAB;
            let host = HOSTS[i % 3];
            let url = TrackerUrl::parse(&format!("http://{host}/announce")).unwrap();
            let spec = TorrentSpec::new(ih, [9; 20], 42, 6881, vec![vec![url]]);
            hub.add_torrent(spec, Millis::ZERO).unwrap();
            ihs.push(ih);
        }
        Sim {
            hub,
            now: Millis::ZERO,
            pend: BTreeMap::new(),
            seq: 0,
            log: Vec::new(),
            net: Box::new(net),
            in_flight: HashMap::new(),
            max_in_flight: 0,
            max_failures: 0,
            events: Vec::new(),
            ihs,
        }
    }

    fn start_all(&mut self) {
        for ih in self.ihs.clone() {
            // A finished download: pure seeding.
            self.hub.set_stats(&ih, Stats { uploaded: 0, downloaded: 0, corrupt: 0, left: 0, size: 1 << 20 }, self.now);
            self.hub.set_role(&ih, TorrentRole::Seeding, self.now);
            self.hub.torrent_event(&ih, TorrentEvent::Start, self.now);
        }
    }

    fn failures(&self) -> u32 {
        let mut m = 0;
        for ih in &self.ihs {
            let set = self.hub.set(ih).unwrap();
            for i in 0..set.len() as u16 {
                m = m.max(set.machine(i).unwrap().failures());
            }
        }
        m
    }

    fn pump(&mut self) {
        // everything that can happen at `now`
        loop {
            let mut progress = false;
            while let Some((&k, _)) = self.pend.iter().next() {
                if k.0 > self.now {
                    break;
                }
                let (ticket, host, result) = self.pend.remove(&k).unwrap();
                *self.in_flight.get_mut(host).unwrap() -= 1;
                self.hub.complete(ticket, result, self.now);
                progress = true;
            }
            for req in self.hub.tick(self.now) {
                let ticket = req.ticket();
                let host: &'static str = HOSTS.iter().copied().find(|h| *h == req.url().host()).unwrap();
                let n = self.in_flight.entry(host).or_insert(0);
                *n += 1;
                self.max_in_flight = self.max_in_flight.max(*n);
                self.log.push(Req { at: self.now, ih: ticket.key().0, host, event: req.event() });
                let (lat, result) = match (self.net)(self.now, host) {
                    Net::Up => (Duration::from_millis(100), Ok(ok(INTERVAL))),
                    Net::Dns => (Duration::from_millis(200), Err(AnnounceError::Transport(TransportError::Dns))),
                    Net::ConnectTimeout => {
                        (Duration::from_secs(5), Err(AnnounceError::Transport(TransportError::ConnectTimeout)))
                    }
                };
                self.seq += 1;
                self.pend.insert((self.now + lat, self.seq), (ticket, host, result));
                progress = true;
            }
            self.events.extend(self.hub.take_net_events());
            self.max_failures = self.max_failures.max(self.failures());
            if !progress {
                break;
            }
        }
    }

    fn run_until(&mut self, t: Millis) {
        self.pump();
        loop {
            let a = self.pend.keys().next().map(|k| k.0);
            let b = self.hub.next_wake(self.now);
            let next = match (a, b) {
                (Some(x), Some(y)) => x.min(y),
                (Some(x), None) | (None, Some(x)) => x,
                (None, None) => t,
            };
            let next = next.max(self.now).min(t);
            self.now = next;
            self.pump();
            if self.now >= t {
                break;
            }
            // a wake at `now` that produces nothing must not spin
            if a.is_none() && b.is_some_and(|w| w <= self.now) && self.hub.next_wake(self.now).is_some_and(|w| w <= self.now) {
                self.now = Millis(self.now.0 + 1);
            }
        }
    }

    fn requests(&self, from: Millis, to: Millis) -> Vec<&Req> {
        self.log.iter().filter(|r| r.at >= from && r.at < to).collect()
    }

    fn probes(&self) -> Vec<Millis> {
        self.events
            .iter()
            .filter_map(|e| match e {
                NetEvent::Probe { at, .. } => Some(*at),
                _ => None,
            })
            .collect()
    }

    fn online_event(&self) -> Option<(Millis, Duration, u32)> {
        self.events.iter().rev().find_map(|e| match e {
            NetEvent::Online { at, after, due, .. } => Some((*at, *after, *due)),
            _ => None,
        })
    }
}

/// Per host: no more than `burst` requests may leave faster than one per second sustained, and
/// never more than 2 in flight.
fn assert_paced(sim: &Sim, reqs: &[&Req]) {
    for h in HOSTS {
        let times: Vec<u64> = reqs.iter().filter(|r| r.host == h).map(|r| r.at.0).collect();
        for k in 3..times.len() {
            // k+1 requests need (k+1-3) seconds at 1 req/s with a burst of 3
            let span = times[k] - times[0];
            assert!(span as i64 >= (k as i64 + 1 - 3) * 1000 - 1000, "host {h}: {} requests in {span} ms", k + 1);
        }
    }
    assert!(sim.max_in_flight <= 2, "max in flight {}", sim.max_in_flight);
}

fn outage_window(from: u64, to: u64, how: Net) -> impl FnMut(Millis, &'static str) -> Net {
    move |now, _| if now >= secs(from) && now < secs(to) { how } else { Net::Up }
}

#[test]
fn five_minute_outage_sends_only_probes_and_recovers_spread() {
    // Every announce is due around 1850-1950 s: the outage swallows them.
    let mut sim = Sim::new(outage_window(1700, 2000, Net::Dns));
    sim.start_all();
    sim.run_until(secs(1700));
    assert_eq!(sim.log.len(), N, "one `started` each");
    assert!(sim.events.is_empty());
    sim.run_until(secs(2000));
    let off_at = sim
        .events
        .iter()
        .find_map(|e| match e {
            NetEvent::Offline { at, reason: Reason::Inference, .. } => Some(*at),
            _ => None,
        })
        .expect("declared offline by inference");
    assert!(off_at >= secs(1800) && off_at < secs(1960), "{off_at:?}");
    // From the declaration on, only probes: at most one request per probe event.
    let probes = sim.probes();
    assert!(!probes.is_empty() && probes.len() <= 4, "{probes:?}");
    let after: Vec<_> = sim.requests(Millis(off_at.0 + 1), secs(2000));
    assert_eq!(after.len(), probes.len(), "requests after the declaration are exactly the probes");
    assert_eq!(sim.max_failures, 0, "no torrent's backoff escalated (before: rolled back, after: held)");
    let forgiven = sim.events.iter().find_map(|e| match e {
        NetEvent::Offline { forgiven, .. } => Some(*forgiven),
        _ => None,
    });
    eprintln!("offline declared at {off_at:?}, failures rolled back: {forgiven:?}");
    assert!(forgiven.unwrap() >= 1, "the failures that revealed the outage were taken back");
    // Network is back at 2000 s: the next probe finds it and everything due goes out.
    let t_back = secs(2000);
    sim.run_until(secs(2000 + 400));
    let (at, after_secs, due) = sim.online_event().expect("online again");
    assert!(at >= t_back && at <= secs(2000 + 301), "{at:?}");
    assert!(after_secs >= Duration::from_secs(60));
    assert!(due > 250, "nearly every torrent came due inside the outage: {due}");
    assert_eq!(sim.max_failures, 0);
    // The probe was one of the due announces; the rest leave inside the spread plus the pacing
    // of 1 request/s/host (100 per host cannot beat ~100 s).
    let burst = sim.requests(at, Millis(at.0 + 130_000));
    assert_eq!(burst.len() as u32, due - 1, "{} of {due}", burst.len());
    let mut per: HashMap<InfoHash20, u32> = HashMap::new();
    for r in sim.requests(Millis(at.0 - 20_000), secs(2400)) {
        *per.entry(r.ih).or_default() += 1;
    }
    assert!(per.values().all(|c| *c == 1), "no torrent announced twice");
    assert_eq!(per.len() as u32, due, "exactly the due torrents announced");
    assert_paced(&sim, &sim.requests(Millis::ZERO, secs(2400)));
}

#[test]
fn three_hour_outage_costs_one_request_per_probe_and_no_backoff() {
    let mut sim = Sim::new(outage_window(1700, 1700 + 3 * 3600, Net::Dns));
    sim.start_all();
    sim.run_until(secs(1700 + 3 * 3600));
    assert!(sim.events.iter().any(|e| matches!(e, NetEvent::Offline { .. })));
    let probes = sim.probes();
    // 30 s, 1 min, 2 min, then every 5 min (+-20 %) for ~3 h
    assert!((28..=42).contains(&probes.len()), "{} probes", probes.len());
    for w in probes.windows(2).skip(3) {
        let gap = w[1].0 - w[0].0;
        assert!((240_000..=420_000).contains(&gap), "gap {gap} ms");
    }
    let during: Vec<_> = sim.requests(secs(1960), secs(1700 + 3 * 3600));
    assert!(during.len() <= probes.len(), "{} requests vs {} probes", during.len(), probes.len());
    assert_eq!(sim.max_failures, 0, "no torrent's backoff escalated");
    // Back up: the next probe is within 5 min, all 300 announce within the pacing.
    let end = secs(1700 + 3 * 3600);
    sim.run_until(Millis(end.0 + 600_000));
    let (at, after, due) = sim.online_event().expect("online");
    assert!(after > Duration::from_secs(3 * 3600 - 400));
    assert_eq!(due as usize, N, "every torrent came due during 3 h");
    sim.run_until(Millis(at.0 + 200_000));
    let burst = sim.requests(at, Millis(at.0 + 200_000));
    assert_eq!(burst.len(), N - 1, "the probe was the 300th");
    // 100 per host at 1 request/s cannot beat ~100 s; the 60 s spread alone would not cap it.
    let last = burst.iter().map(|r| r.at.0).max().unwrap() - at.0;
    assert!(last <= 130_000, "{last} ms");
    assert_paced(&sim, &sim.requests(Millis::ZERO, Millis(at.0 + 200_000)));
    assert_eq!(sim.max_failures, 0);
    // Healthy again: the next interval is the tracker's.
    sim.run_until(Millis(at.0 + 200_000 + 3_000_000));
    assert!(sim.log.len() <= N * 3 + 40, "{}", sim.log.len());
}

#[test]
fn one_dead_tracker_is_not_an_outage() {
    let mut sim = Sim::new(|_, host| if host == "a.test" { Net::Dns } else { Net::Up });
    sim.start_all();
    sim.run_until(secs(3 * 3600));
    assert!(sim.events.is_empty(), "{:?}", sim.events);
    assert!(sim.hub.net_view().online);
    // Per-tracker backoff applies to the dead host's torrents: seeding 15 min doubling.
    assert!(sim.max_failures >= 1, "max failures {}", sim.max_failures);
    // The host's own circuit breaker and the per-tracker backoff keep the dead host quiet.
    let to_a = sim.log.iter().filter(|r| r.host == "a.test").count();
    assert!((4..=100 * 7).contains(&to_a), "{to_a}");
    // The others are untouched.
    assert!(sim.log.iter().filter(|r| r.host != "a.test").count() >= 200 * 6);
}

#[test]
fn unsatisfied_path_probes_nothing_until_it_changes() {
    // The network is really down for the first 2 h; the OS tells us.
    let mut sim = Sim::new(outage_window(1000, 1000 + 7200, Net::ConnectTimeout));
    sim.start_all();
    sim.run_until(secs(1000));
    sim.hub.path_changed(false, sim.now);
    sim.run_until(secs(1000 + 7200));
    let view = sim.hub.net_view();
    assert!(!view.online && view.reason == Some(Reason::Path));
    assert!(sim.requests(secs(1000), secs(1000 + 7200)).is_empty(), "no request while the path is down");
    assert!(sim.probes().is_empty());
    assert_eq!(sim.max_failures, 0);
    // The path comes back: exactly one probe, right away.
    let t = secs(1000 + 7200);
    sim.hub.path_changed(true, t);
    sim.run_until(Millis(t.0 + 99));
    let probes = sim.probes();
    assert_eq!(probes.len(), 1, "{probes:?}");
    assert!(probes[0].0 - t.0 <= 1000, "{probes:?}");
    let first = sim.requests(t, Millis(t.0 + 100));
    assert_eq!(first.len(), 1, "exactly one request before the probe is answered");
    sim.run_until(Millis(t.0 + 300_000));
    let (_, _, due) = sim.online_event().expect("online");
    assert!(due > 250, "{due}");
    let all = sim.requests(t, Millis(t.0 + 300_000));
    assert_eq!(all.len() as u32, due, "the released announces are exactly the due ones");
    assert_eq!(sim.max_failures, 0);
    assert_paced(&sim, &sim.requests(Millis::ZERO, Millis(t.0 + 300_000)));
}

#[test]
fn inference_works_without_any_os_signal_and_quiet_torrents_send_nothing_extra() {
    // Down 100 s only: shorter than any interval: nothing comes due, nothing is probed.
    let mut sim = Sim::new(outage_window(5000, 5100, Net::Dns));
    sim.start_all();
    sim.run_until(secs(5000));
    let before = sim.log.len();
    sim.run_until(secs(5400));
    // Regular announces fall in 1800+k and 3600+k; none lies in 5000..5400 except 5400-ish ones
    // (3600+1800 = 5400). So the log is unchanged by the short outage.
    assert_eq!(sim.log.len(), before, "nothing was due, so the outage cost nothing");
    assert!(sim.events.is_empty());
    assert_eq!(sim.max_failures, 0);
}

#[test]
fn wake_after_ten_hours_is_one_probe_then_a_paced_release() {
    let mut sim = Sim::new(|_, _| Net::Up);
    sim.start_all();
    sim.run_until(secs(600));
    // sleep 10 h: no ticks at all, then the wake notification arrives first
    let wake = Millis(secs(600).0 + 10 * 3600 * 1000);
    sim.now = wake;
    sim.hub.did_wake(wake);
    sim.run_until(Millis(wake.0 + 4000));
    assert!(sim.requests(wake, Millis(wake.0 + 4900)).is_empty(), "nothing before the check");
    sim.run_until(Millis(wake.0 + 300_000));
    let onlines = sim.events.iter().filter(|e| matches!(e, NetEvent::Online { .. })).count();
    assert_eq!(onlines, 1, "one recovery, one DHT bootstrap on the engine side");
    assert_eq!(sim.probes().len(), 1);
    let (at, _, due) = sim.online_event().unwrap();
    assert_eq!(due as usize, N);
    let after = sim.requests(wake, Millis(wake.0 + 300_000));
    assert_eq!(after.len(), N, "each torrent once");
    assert!(at.0 - wake.0 >= 5000 && at.0 - wake.0 < 8000, "{} ms after the wake", at.0 - wake.0);
    // Wake is not an outage for the UI.
    assert!(sim.hub.net_view().online);
    assert_eq!(sim.max_failures, 0);
    let mut spaced = sim.requests(Millis::ZERO, Millis(wake.0 + 300_000));
    spaced.retain(|r| r.at >= wake);
    assert_paced(&sim, &spaced);
}

#[test]
fn captive_portal_keeps_the_five_minute_cadence() {
    // The OS says the path is fine, but connects time out for two hours.
    let mut sim = Sim::new(outage_window(1700, 1700 + 7200, Net::ConnectTimeout));
    sim.start_all();
    sim.run_until(secs(1700));
    sim.hub.path_changed(true, sim.now);
    sim.run_until(secs(1700 + 7200));
    let view = sim.hub.net_view();
    assert!(!view.online && view.reason == Some(Reason::Inference), "{view:?}");
    let probes = sim.probes();
    let late: Vec<_> = probes.iter().filter(|p| p.0 > 2_400_000).collect();
    assert!((14..=26).contains(&(late.len())), "{} probes after the first 12 minutes", late.len());
    for w in late.windows(2) {
        assert!(w[1].0 - w[0].0 >= 240_000, "{:?}", w);
    }
    assert_eq!(sim.max_failures, 0);
}

#[test]
fn a_sign_of_life_probes_at_once_but_only_once_per_debounce() {
    let mut sim = Sim::new(outage_window(1700, 1700 + 7200, Net::Dns));
    sim.start_all();
    sim.run_until(secs(2400));
    assert!(!sim.hub.net_view().online);
    let n = sim.probes().len();
    sim.hub.net_hint(sim.now);
    sim.hub.net_hint(sim.now);
    sim.run_until(Millis(sim.now.0 + 3000));
    assert_eq!(sim.probes().len(), n + 1, "one probe for two hints");
}

#[test]
fn a_torrent_added_during_an_outage_waits_for_it() {
    let mut sim = Sim::new(outage_window(1700, 5000, Net::Dns));
    sim.start_all();
    sim.run_until(secs(2500));
    assert!(!sim.hub.net_view().online);
    let mut ih = [0u8; 20];
    ih[0] = 0xEE;
    let url = TrackerUrl::parse("http://a.test/announce").unwrap();
    sim.hub.add_torrent(TorrentSpec::new(ih, [9; 20], 42, 6881, vec![vec![url]]), sim.now).unwrap();
    sim.hub.torrent_event(&ih, TorrentEvent::Start, sim.now);
    sim.ihs.push(ih);
    let n = sim.log.len();
    sim.run_until(secs(3000));
    assert!(sim.log[n..].iter().all(|r| r.ih != ih), "no request for the new torrent while offline");
    sim.run_until(secs(5400));
    assert!(sim.log.iter().any(|r| r.ih == ih && r.event == Some(trs_tracker::Event::Started)));
    assert_eq!(sim.max_failures, 0);
}

#[test]
fn two_torrents_on_two_hosts_are_enough_evidence() {
    // The smallest user with two trackers: one torrent each. Both refuse connections.
    let mut sim = Sim::with(2, |_, _| Net::Dns);
    sim.hub.note_alive(Millis::ZERO); // a reading from the past changes nothing
    sim.start_all();
    sim.run_until(secs(120));
    assert!(!sim.hub.net_view().online, "{:?}", sim.events);
    // The first host's failure, seen before the second host failed, was taken back.
    assert_eq!(sim.failures(), 0);
    assert!(sim.max_failures <= 1);
}

#[test]
fn signs_of_life_elsewhere_mean_the_trackers_are_down_not_the_network() {
    // Peers keep handshaking (the engine feeds `note_alive`), yet both trackers refuse.
    let mut sim = Sim::with(2, |_, _| Net::Dns);
    sim.start_all();
    // A beat of one second (as the engine reports payload traffic): the failures of the two hosts
    // may land between two beats, but a failure within 2 s of a sign of life is no evidence.
    for t in 1..=600 {
        sim.run_until(secs(t));
        sim.hub.note_alive(sim.now);
    }
    assert!(sim.hub.net_view().online);
    assert!(sim.events.is_empty(), "{:?}", sim.events);
    assert!(sim.max_failures >= 1);
}
