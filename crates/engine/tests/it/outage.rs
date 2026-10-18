//! Outage handling in the running engine (SPEC 7C.4c): the OS path signal, the wake check, held
//! dials, and the inference from failing trackers. The timing arithmetic (probe schedule, spread,
//! pacing, 3-hour outages) is covered with a virtual clock in `crates/tracker/tests/outage.rs`.
use crate::common;
use std::time::{Duration, Instant};

use common::*;
use trs_engine::*;

async fn online(n: &Node) -> bool {
    n.h.snapshot().await.map(|s| s.online).unwrap_or(true)
}

async fn wait_online(n: &Node, want: bool, secs: u64) {
    wait_for(if want { "online" } else { "offline" }, secs, || async { (online(n).await == want).then_some(()) }).await;
}

fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unsatisfied_path_holds_announces_and_a_satisfied_one_releases_them() {
    let tracker = MockTracker::start(60).await;
    let n = node(|c| c.trackers_enabled = true).await;
    n.h.network_path_changed(false);
    wait_online(&n, false, 5).await;
    let snap = n.h.snapshot().await.unwrap();
    assert!(snap.offline_since_ms.is_some());

    let t = standard_torrent(0, Some(tracker.url()), false, 601);
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(7)).await;
    assert!(tracker.log().is_empty(), "nothing is sent while the path is down: {:?}", tracker.log().len());
    let d = n.h.torrent_detail(t.id).await.unwrap();
    assert!(d.trackers.iter().all(|x| x.failures == 0));

    n.h.network_path_changed(true);
    wait_for("started after the path came back", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    assert_eq!(tracker.log_for(&t.id)[0].event.as_deref(), Some("started"));
    wait_online(&n, true, 5).await;
    assert!(n.h.snapshot().await.unwrap().offline_since_ms.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wake_waits_for_the_network_then_goes_on_and_is_not_shown_as_an_outage() {
    let tracker = MockTracker::start(60).await;
    let n = node(|c| c.trackers_enabled = true).await;
    n.h.system_will_sleep();
    n.h.system_did_wake();
    let t = standard_torrent(0, Some(tracker.url()), false, 602);
    t.write_to(n.dir.path());
    let woke = Instant::now();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(tracker.log().is_empty(), "the wake check holds announces for a few seconds");
    assert!(online(&n).await, "a wake check is not an outage");
    wait_for("started after the wake check", 20, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    assert!(woke.elapsed() >= Duration::from_secs(4), "{:?}", woke.elapsed());
    assert!(online(&n).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dials_wait_for_the_network_and_no_peer_is_blamed() {
    let t = standard_torrent(2_000_000, None, false, 603);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let leech = node(|c| c.trackers_enabled = true).await;
    leech.h.network_path_changed(false);
    wait_online(&leech, false, 5).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let row = leech.row(&t.id).await.unwrap();
    assert_eq!(row.downloaded_total, 0, "no dial while the path is down");
    assert_eq!(leech.h.snapshot().await.unwrap().connections, 0);
    leech.h.network_path_changed(true);
    wait_done(&leech, &t.id, 30).await;
    t.verify(leech.dir.path(), &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trackers_on_two_hosts_that_cannot_be_reached_are_an_outage_not_a_backoff() {
    let (p1, p2) = (closed_port(), closed_port());
    let a = standard_named("Outage.A", 0, Some(format!("http://127.0.0.1:{p1}/announce")), false, 604);
    let b = standard_named("Outage.B", 0, Some(format!("http://[::1]:{p2}/announce")), false, 605);
    let n = node(|c| c.trackers_enabled = true).await;
    for t in [&a, &b] {
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    }
    wait_online(&n, false, 30).await;
    for t in [&a, &b] {
        let d = n.h.torrent_detail(t.id).await.unwrap();
        assert!(d.trackers.iter().all(|x| x.failures == 0), "{:?}", d.trackers.iter().map(|x| x.failures).collect::<Vec<_>>());
        let row = n.row(&t.id).await.unwrap();
        assert!(!matches!(row.state, TorrentState::Error), "an outage is not a torrent error");
    }
    // The OS says the path is fine, but nothing can be reached (a captive portal): still offline.
    n.h.network_path_changed(true);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(!online(&n).await);
}

/// Payload moving over connections that were made earlier proves the network works: a download
/// in progress, no new handshakes, and two tracker hosts that refuse connections (one tracker
/// being down) must not read as an outage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn steady_transfer_means_two_dead_tracker_hosts_are_not_an_outage() {
    let t = standard_torrent(30_000_000, None, false, 610);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let leech = node(|c| {
        c.trackers_enabled = true;
        c.down_limit = Some(400_000);
    })
    .await;
    let opts = AddOptions { peers: vec![seeder.addr()], ..AddOptions::default() };
    leech.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    wait_for("download under way", 30, || async {
        leech.row(&t.id).await.filter(|r| r.downloaded_total > 400_000).map(|_| ())
    })
    .await;

    // Two more torrents whose trackers (two hosts) refuse connections.
    let (p1, p2) = (closed_port(), closed_port());
    let a = standard_named("Outage.C", 0, Some(format!("http://127.0.0.1:{p1}/announce")), false, 611);
    let b = standard_named("Outage.D", 0, Some(format!("http://[::1]:{p2}/announce")), false, 612);
    for x in [&a, &b] {
        x.write_to(leech.dir.path());
        leech.h.add_torrent_file(x.bytes.clone(), AddOptions::default()).await.unwrap();
    }
    // Long enough for both announces to fail (the outage test above sees "offline" in < 30 s).
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(online(&leech).await, "payload is moving: the network is fine");
    }
    let row = leech.row(&t.id).await.unwrap();
    assert!(row.downloaded_total < t.total(), "the transfer was still running");
}
