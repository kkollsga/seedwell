//! BEP 12 tracker sets and the hub glue: fall-through, promotion, suspension,
//! persist-before-send, fail-closed on store errors, crash recovery.

use crate::common;

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use common::*;
use trs_tracker::gate::{GateConfig, GatedRequest};
use trs_tracker::hub::{TorrentSpec, TrackerHub};
use trs_tracker::machine::MachineConfig;
use trs_tracker::response::{AnnounceError, TransportError};
use trs_tracker::rng::SplitMix64;
use trs_tracker::set::{SetInput, TrackerSet};
use trs_tracker::store::{StoreError, TimingKey, TimingRecord, TimingStore};
use trs_tracker::*;

/// Host-scripted responder: hosts in `down` time out, everything else succeeds.
struct Scripted {
    down: Rc<RefCell<Vec<String>>>,
    interval: u64,
}
impl Responder for Scripted {
    fn respond(&mut self, _now: Millis, host: &str, _req: &GatedRequest) -> Reply {
        if self.down.borrow().iter().any(|h| h == host) {
            Reply::new(
                Duration::from_secs(5),
                Err(AnnounceError::Transport(TransportError::Timeout)),
            )
        } else {
            Reply::new(
                Duration::from_millis(100),
                Ok(ok_response(self.interval, None)),
            )
        }
    }
}

fn world(down: &[&str], interval: u64) -> (World<Scripted>, Rc<RefCell<Vec<String>>>) {
    let down = Rc::new(RefCell::new(
        down.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
    ));
    (
        World::new(
            Scripted {
                down: down.clone(),
                interval,
            },
            GateConfig::default(),
        ),
        down,
    )
}

fn hosts_contacted(w: &World<Scripted>) -> Vec<(String, u64)> {
    w.per_host_sent
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect()
}

#[test]
fn shuffle_happens_once_and_is_seeded() {
    let tiers = |_: ()| {
        vec![
            (0..8)
                .map(|i| TrackerUrl::parse(&format!("http://a{i}.test/announce")).unwrap())
                .collect::<Vec<_>>(),
            (0..8)
                .map(|i| TrackerUrl::parse(&format!("http://b{i}.test/announce")).unwrap())
                .collect::<Vec<_>>(),
        ]
    };
    let cfg = MachineConfig::new([1; 20], [2; 20], 3, 4);
    let a = TrackerSet::new(&cfg, tiers(()), |_| None, &mut SplitMix64::new(1));
    let b = TrackerSet::new(&cfg, tiers(()), |_| None, &mut SplitMix64::new(1));
    let c = TrackerSet::new(&cfg, tiers(()), |_| None, &mut SplitMix64::new(2));
    assert_eq!(a.order(), b.order(), "same seed, same shuffle");
    assert_ne!(a.order(), c.order(), "different seed, different shuffle");
    let natural: Vec<u16> = (0..16).collect();
    assert_ne!(a.order(), natural);
    // tiers stay tiers: first 8 slots are tier 0
    assert!(a.order()[..8].iter().all(|s| *s < 8));
    assert!(a.order()[8..].iter().all(|s| *s >= 8));
    // asking for the order never reshuffles
    assert_eq!(a.order(), a.order());
}

#[test]
fn duplicates_and_cap() {
    let cfg = MachineConfig::new([1; 20], [2; 20], 3, 4);
    let urls: Vec<TrackerUrl> = (0..100)
        .map(|i| TrackerUrl::parse(&format!("http://t{}.test/a", i % 50)).unwrap())
        .collect();
    let s = TrackerSet::new(&cfg, vec![urls], |_| None, &mut SplitMix64::new(1));
    assert_eq!(s.len(), trs_tracker::set::MAX_TRACKERS);
    let u: std::collections::HashSet<_> = s.urls().iter().map(|u| u.raw().to_owned()).collect();
    assert_eq!(u.len(), s.len());
}

#[test]
fn only_the_first_working_tracker_of_a_tier_is_used_and_tiers_run_alone() {
    let (mut w, _) = world(&[], 1800);
    w.add_torrent(
        spec(
            1,
            6881,
            &[
                &["http://a.test/announce", "http://b.test/announce"],
                &["http://c.test/announce"],
            ],
        ),
        1000,
        1000,
    );
    w.start(ih(1));
    w.run_for(Duration::from_secs(3 * 3600));
    let hosts = hosts_contacted(&w);
    // tier 1 uses one of a/b (the other is its failover), tier 2 is c: one tracker per tier
    assert_eq!(hosts.len(), 2, "one tracker per tier while it works: {hosts:?}");
    assert!(hosts.iter().any(|(h, _)| h == "c.test"));
    assert!(w.total_sent >= 12 && w.total_sent <= 16, "{}", w.total_sent);
}

#[test]
fn failure_falls_through_immediately_and_second_tier_is_last() {
    let (mut w, down) = world(&["a.test", "b.test"], 1800);
    w.add_torrent(
        spec(
            1,
            6881,
            &[
                &["http://a.test/announce", "http://b.test/announce"],
                &["http://c.test/announce"],
            ],
        ),
        1000,
        1000,
    );
    w.start(ih(1));
    w.run_for(Duration::from_secs(120));
    // all three were tried in the first two minutes, c.test carries the session
    let hosts = hosts_contacted(&w);
    assert_eq!(hosts.len(), 3, "{hosts:?}");
    assert!(w.per_host_sent["c.test"] >= 1);
    // c works: regular announces go to c only
    let before_ab = w.per_host_sent["a.test"] + w.per_host_sent["b.test"];
    w.run_for(Duration::from_secs(4 * 3600));
    let after_ab = w.per_host_sent["a.test"] + w.per_host_sent["b.test"];
    // a and b keep being retried with exponential backoff (<= 1/hour each, plus the first few)
    assert!(
        after_ab - before_ab <= 2 * 6 + 4,
        "failed trackers hammered: {}",
        after_ab - before_ab
    );
    assert!(
        w.per_host_sent["c.test"] >= 7,
        "c carries the load: {}",
        w.per_host_sent["c.test"]
    );
    // a recovers: tier 2 (c) is independent, it neither stops nor announces early
    down.borrow_mut().clear();
    let c_before = w.per_host_sent["c.test"];
    w.run_for(Duration::from_secs(3 * 3600));
    let c_after = w.per_host_sent["c.test"];
    assert!(
        (5..=7).contains(&(c_after - c_before)),
        "c keeps its own 30 min schedule: +{}",
        c_after - c_before
    );
}

#[test]
fn working_tracker_is_promoted_within_its_tier() {
    let (mut w, _) = world(&["a.test"], 1800);
    w.add_torrent(
        spec(
            1,
            6881,
            &[&["http://a.test/announce", "http://b.test/announce"]],
        ),
        1000,
        1000,
    );
    w.start(ih(1));
    w.run_for(Duration::from_secs(300));
    let set = w.hub().set(&ih(1)).unwrap();
    let order = set.order();
    let b_slot = (0..2u16)
        .find(|s| set.url(*s).unwrap().host() == "b.test")
        .unwrap();
    assert_eq!(
        order[0], b_slot,
        "the working tracker is at the front of its tier: {order:?}"
    );
}

#[test]
fn stopped_goes_to_every_tracker_with_an_open_session() {
    let (mut w, down) = world(&[], 300);
    w.add_torrent(
        spec(
            1,
            6881,
            &[&["http://a.test/announce", "http://b.test/announce"]],
        ),
        1000,
        1000,
    );
    // Whichever tracker the shuffle put first is down, so both get a `started`.
    let first = {
        let set = w.hub().set(&ih(1)).unwrap();
        set.url(set.order()[0]).unwrap().host().to_owned()
    };
    down.borrow_mut().push(first);
    w.start(ih(1));
    w.run_for(Duration::from_secs(200));
    assert_eq!(
        w.per_key_events.values().sum::<u64>(),
        2,
        "started to the dead one, started to the live one; the dead one is closed silently (it never answered)"
    );
    down.borrow_mut().clear();
    w.stop(ih(1));
    w.run_for(Duration::from_secs(3600));
    assert!(w.hub().is_quiescent(&ih(1)));
    assert!(
        w.per_key_events.values().sum::<u64>() >= 3,
        "stopped reached the live tracker too"
    );
}

#[test]
fn nothing_is_sent_for_a_stopped_torrent() {
    let (mut w, _) = world(&[], 300);
    w.add_torrent(spec(1, 6881, &[&["http://a.test/announce"]]), 1000, 1000);
    w.run_for(Duration::from_secs(10_000));
    assert_eq!(w.total_sent, 0, "torrents never started are silent");
    w.start(ih(1));
    w.run_for(Duration::from_secs(600));
    w.stop(ih(1));
    w.run_for(Duration::from_secs(600));
    let n = w.total_sent;
    w.run_for(Duration::from_secs(86_400));
    assert_eq!(w.total_sent, n, "silent after stopped");
}

// -------------------------------------------------------------- persistence

#[derive(Clone, Default)]
struct SpyStore {
    inner: Rc<RefCell<HashMap<TimingKey, TimingRecord>>>,
    fail_completed: Rc<RefCell<bool>>,
    log: Rc<RefCell<Vec<String>>>,
}
impl TimingStore for SpyStore {
    fn load(&self, key: &TimingKey) -> Option<TimingRecord> {
        self.inner.borrow().get(key).cloned()
    }
    fn save(&mut self, key: &TimingKey, rec: &TimingRecord) -> Result<(), StoreError> {
        if *self.fail_completed.borrow() && rec.completed_sent {
            self.log.borrow_mut().push("save-failed".into());
            return Err(StoreError("disk full".into()));
        }
        self.log
            .borrow_mut()
            .push(format!("save completed_sent={}", rec.completed_sent));
        self.inner.borrow_mut().insert(key.clone(), rec.clone());
        Ok(())
    }
}

fn drive(hub: &mut TrackerHub<SpyStore>, now: Millis, answer: bool) -> Vec<GatedRequest> {
    let reqs = hub.tick(now);
    if answer {
        for r in &reqs {
            hub.complete(r.ticket(), Ok(ok_response(1800, None)), now);
        }
    }
    reqs
}

#[test]
fn completed_flag_is_durable_before_the_request_is_released() {
    let store = SpyStore::default();
    let mut hub = TrackerHub::new(GateConfig::default(), store.clone(), 1);
    let s = spec(1, 6881, &[&["http://a.test/announce"]]);
    hub.add_torrent(s, Millis(0)).unwrap();
    let h = ih(1);
    hub.set_stats(
        &h,
        Stats {
            uploaded: 0,
            downloaded: 0,
            left: 100,
            size: 100,
        },
        Millis(0),
    );
    hub.torrent_event(&h, TorrentEvent::Start, Millis(0));
    let mut now = Millis(0);
    let mut started = false;
    for _ in 0..20 {
        now = hub.next_wake(now).unwrap_or(now).max(now);
        let reqs = drive(&mut hub, now, true);
        started |= reqs.iter().any(|r| r.event() == Some(Event::Started));
        if started {
            break;
        }
    }
    assert!(started);
    // (a few seconds of idle time refill the host's token bucket)
    now = now + Duration::from_secs(5);
    hub.set_stats(
        &h,
        Stats {
            uploaded: 0,
            downloaded: 100,
            left: 0,
            size: 100,
        },
        now,
    );
    hub.torrent_event(&h, TorrentEvent::Complete, now);
    let reqs = hub.tick(now);
    let c = reqs
        .iter()
        .find(|r| r.event() == Some(Event::Completed))
        .expect("completed released");
    // The moment the request is in our hands the flag is already durable.
    let key = TimingKey {
        info_hash: h,
        tracker_url: c.url().raw().to_owned(),
    };
    assert!(
        store.inner.borrow()[&key].completed_sent,
        "completed_sent persisted before release"
    );
    assert_eq!(c.announce_params().unwrap().left, 0);
}

#[test]
fn failed_persist_of_completed_is_never_released_but_retried_once_the_store_recovers() {
    let store = SpyStore::default();
    let mut hub = TrackerHub::new(GateConfig::default(), store.clone(), 1);
    hub.add_torrent(spec(1, 6881, &[&["http://a.test/announce"]]), Millis(0))
        .unwrap();
    let h = ih(1);
    hub.set_stats(
        &h,
        Stats {
            uploaded: 0,
            downloaded: 0,
            left: 100,
            size: 100,
        },
        Millis(0),
    );
    hub.torrent_event(&h, TorrentEvent::Start, Millis(0));
    let mut now = Millis(0);
    loop {
        now = hub.next_wake(now).unwrap_or(now).max(now);
        let reqs = drive(&mut hub, now, true);
        if reqs.iter().any(|r| r.event() == Some(Event::Started)) {
            break;
        }
    }
    *store.fail_completed.borrow_mut() = true;
    now = now + Duration::from_secs(5); // bucket refills
    hub.set_stats(
        &h,
        Stats {
            uploaded: 0,
            downloaded: 100,
            left: 0,
            size: 100,
        },
        now,
    );
    hub.torrent_event(&h, TorrentEvent::Complete, now);
    let reqs = hub.tick(now);
    assert!(
        reqs.iter().all(|r| r.event() != Some(Event::Completed)),
        "no completed without a durable flag"
    );
    assert!(hub.store_errors() >= 1);
    // Nothing was sent, so this is an ordinary failure with backoff, NOT a
    // reason to trip: the tracker keeps working once the disk does.
    let reports = hub.take_reports();
    assert!(
        reports
            .iter()
            .all(|r| !matches!(r.report, Report::InvariantViolation(_))),
        "{reports:?}"
    );
    assert!(
        reports.iter().any(|r| matches!(
            &r.report,
            Report::Failed {
                error: AnnounceError::Transport(TransportError::NotSent),
                ..
            }
        )),
        "{reports:?}"
    );
    // While the store is broken a completed is never released, however long we wait.
    for _ in 0..10 {
        now = now + Duration::from_secs(3600);
        assert!(
            hub.tick(now)
                .iter()
                .all(|r| r.event() != Some(Event::Completed))
        );
    }
    // The disk recovers: the owed completed goes out, exactly once.
    *store.fail_completed.borrow_mut() = false;
    let mut completed = 0;
    for _ in 0..40 {
        now = now + Duration::from_secs(1800);
        completed += drive(&mut hub, now, true)
            .iter()
            .filter(|r| r.event() == Some(Event::Completed))
            .count();
    }
    assert_eq!(completed, 1);
    assert!(
        store
            .inner
            .borrow()
            .values()
            .all(|r| r.completed_sent && !r.completion_owed),
        "the flag is durable once it was sent"
    );
}

#[test]
fn crash_and_restart_never_sends_completed_twice() {
    let (mut w, _) = world(&[], 300);
    w.add_torrent(spec(1, 6881, &[&["http://a.test/announce"]]), 1000, 1000);
    w.start(ih(1));
    w.run_for(Duration::from_secs(100));
    w.complete(ih(1));
    w.run_for(Duration::from_secs(1000));
    assert_eq!(
        w.per_key_events.get(&(ih(1), 0)).copied().unwrap_or(0),
        2,
        "started + completed"
    );
    for round in 0..5 {
        w.crash(round % 2 == 0);
        w.run_for(Duration::from_secs(3000));
        w.complete(ih(1));
        w.run_for(Duration::from_secs(1000));
    }
    // 1 completed overall; each restart sends exactly one `started`
    let events = w.per_key_events[&(ih(1), 0)];
    assert_eq!(events, 2 + 5, "started x6 + completed x1 = 7, got {events}");
}

#[test]
fn restart_does_not_announce_before_the_persisted_min_interval() {
    let (mut w, _) = world(&[], 1800);
    w.add_torrent(spec(1, 6881, &[&["http://a.test/announce"]]), 1000, 1000);
    w.start(ih(1));
    w.run_for(Duration::from_secs(100));
    let sent = w.total_sent;
    assert_eq!(sent, 1);
    // crash 100 s after the last success; interval 1800 => min eff 300 s
    w.crash(true);
    w.run_for(Duration::from_secs(150));
    assert_eq!(
        w.total_sent, sent,
        "post-crash `started` waits for the persisted floor"
    );
    w.run_for(Duration::from_secs(400));
    assert_eq!(w.total_sent, sent + 1);
}

#[test]
fn crash_loop_breaker_hold_delays_everything() {
    let (mut w, _) = world(&[], 1800);
    w.add_torrent(spec(1, 6881, &[&["http://a.test/announce"]]), 1000, 1000);
    let now = w.now;
    w.hub().hold_until(now + Duration::from_secs(600));
    w.start(ih(1));
    w.run_for(Duration::from_secs(599));
    assert_eq!(w.total_sent, 0);
    w.run_for(Duration::from_secs(20));
    assert_eq!(w.total_sent, 1);
}

#[test]
fn remove_requires_quiescence() {
    let (mut w, _) = world(&[], 1800);
    w.add_torrent(spec(1, 6881, &[&["http://a.test/announce"]]), 1000, 1000);
    w.start(ih(1));
    w.run_for(Duration::from_secs(100));
    let h = ih(1);
    assert!(!w.hub().remove_torrent(&h));
    w.stop(h);
    w.run_for(Duration::from_secs(100));
    assert!(w.hub().remove_torrent(&h));
    assert_eq!(w.hub().torrent_count(), 0);
}

#[test]
fn set_input_enum_is_usable_directly() {
    // The sans-I/O API is public: a TrackerSet can be driven without the hub.
    let cfg = MachineConfig::new([1; 20], [2; 20], 3, 4);
    let mut set = TrackerSet::new(
        &cfg,
        vec![vec![TrackerUrl::parse("http://a.test/x").unwrap()]],
        |_| None,
        &mut SplitMix64::new(1),
    );
    let mut out = Vec::new();
    set.handle(
        SetInput::Stats(Stats {
            uploaded: 0,
            downloaded: 0,
            left: 5,
            size: 5,
        }),
        Millis(0),
        &mut out,
    );
    set.handle(SetInput::Torrent(TorrentEvent::Start), Millis(0), &mut out);
    set.handle(SetInput::Timer, Millis(10_000), &mut out);
    assert!(out.iter().any(|i| matches!(&i.intent, trs_tracker::Intent::SendAnnounce(s) if s.params.event == Some(Event::Started))));
    let _: Option<TorrentSpec> = None;
}

#[test]
fn scrape_through_the_hub_is_explicit_gated_and_independent_of_announces() {
    let mut hub = TrackerHub::new(
        GateConfig {
            initial_tokens: 3,
            ..GateConfig::default()
        },
        SpyStore::default(),
        1,
    );
    hub.add_torrent(
        spec(1, 6881, &[&["http://a.test/announce?passkey=k"]]),
        Millis(0),
    )
    .unwrap();
    let h = ih(1);
    // nothing happens by itself
    assert!(hub.tick(Millis(10_000_000)).is_empty());
    hub.request_scrape(&h, 0, Millis(0)).unwrap();
    // an announce for the same pair is not disturbed while the scrape is in flight
    hub.set_stats(
        &h,
        Stats {
            uploaded: 0,
            downloaded: 0,
            left: 100,
            size: 100,
        },
        Millis(0),
    );
    hub.torrent_event(&h, TorrentEvent::Start, Millis(0));
    let first = hub.tick(Millis(0));
    assert_eq!(first.len(), 1, "scrape released");
    assert!(first[0].scrape_hashes().is_some());
    assert!(first[0].url().path_and_query().contains("announce"));
    let announce = hub.tick(Millis(10_000));
    assert_eq!(announce.len(), 1, "the started announce still goes out");
    assert_eq!(announce[0].event(), Some(Event::Started));
    assert!(
        hub.take_reports()
            .iter()
            .all(|r| !matches!(r.report, Report::InvariantViolation(_)))
    );
    hub.complete_scrape(first[0].ticket(), true, Millis(10_001));
    // rate limit: a second scrape right away is refused
    assert!(hub.request_scrape(&h, 0, Millis(10_002)).is_err());
}

#[test]
fn a_tracker_added_to_a_running_torrent_starts_through_the_gate_and_keeps_the_others() {
    let (mut w, _) = world(&[], 1800);
    w.add_torrent(spec(1, 6881, &[&["http://a.test/announce"]]), 1000, 1000);
    w.start(ih(1));
    w.run_for(Duration::from_secs(60));
    assert_eq!(hosts_contacted(&w).len(), 1);
    // The engine merged a tracker in: the oracle must accept the new URL.
    let set: std::collections::HashSet<String> =
        ["http://a.test/announce", "http://b.test/announce"].iter().map(|s| s.to_string()).collect();
    w.oracle.register(ih(1), set, 1000, false);
    let now = w.now;
    let added = w.hub().add_tracker(&ih(1), TrackerUrl::parse("http://b.test/announce").unwrap(), None, TorrentRole::Downloading, now);
    assert!(added);
    // Duplicates and the https twin are refused; a different torrent is unknown.
    assert!(!w.hub().add_tracker(&ih(1), TrackerUrl::parse("http://b.test/announce").unwrap(), None, TorrentRole::Downloading, now));
    assert!(!w.hub().add_tracker(&ih(1), TrackerUrl::parse("https://b.test/announce").unwrap(), None, TorrentRole::Downloading, now));
    assert!(!w.hub().add_tracker(&ih(9), TrackerUrl::parse("http://c.test/announce").unwrap(), None, TorrentRole::Downloading, now));
    assert_eq!(w.hub_ref().set(&ih(1)).unwrap().len(), 2);
    w.run_for(Duration::from_secs(120));
    let hosts = hosts_contacted(&w);
    assert!(hosts.iter().any(|(h, n)| h == "b.test" && *n >= 1), "new tracker announced: {hosts:?}");
    // Its first announce is `started`; the old tracker was not announced to again early.
    assert_eq!(w.per_host_sent["a.test"], 1, "{hosts:?}");
    // It then follows its own interval like any other tier.
    w.run_for(Duration::from_secs(2 * 3600));
    assert!(w.per_host_sent["b.test"] >= 3 && w.per_host_sent["b.test"] <= 5, "{:?}", hosts_contacted(&w));
}

#[test]
fn a_tracker_added_to_a_stopped_torrent_waits_for_start() {
    let (mut w, _) = world(&[], 1800);
    w.add_torrent(spec(2, 6881, &[&["http://a.test/announce"]]), 1000, 1000);
    let set: std::collections::HashSet<String> =
        ["http://a.test/announce", "http://b.test/announce"].iter().map(|s| s.to_string()).collect();
    w.oracle.register(ih(2), set, 1000, false);
    let now = w.now;
    assert!(w.hub().add_tracker(&ih(2), TrackerUrl::parse("http://b.test/announce").unwrap(), None, TorrentRole::Downloading, now));
    w.run_for(Duration::from_secs(300));
    assert!(w.total_sent == 0, "nothing is sent before start");
    w.start(ih(2));
    w.run_for(Duration::from_secs(60));
    assert_eq!(hosts_contacted(&w).len(), 2);
}
