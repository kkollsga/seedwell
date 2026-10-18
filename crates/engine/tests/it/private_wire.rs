//! A private (BEP 27) torrent as a private tracker sees it (SPEC 7B.1, 7B.2, 7B.4): against a local mock only.
//! Checked: the announce URL is used as it is in the torrent (passkey included, nothing added), only the known
//! parameters are sent, identity is honest and stable inside a session, a torrent added complete never sends
//! `completed`, `stopped` follows pause and remove, and neither DHT nor PEX is used although both are switched on.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;

const PASSKEY: &str = "0123456789abcdef0123456789abcdef";

fn names(a: &Announce) -> Vec<&str> {
    a.raw_query.split('&').map(|kv| kv.split('=').next().unwrap()).collect()
}

fn ev(a: &Announce) -> &str {
    a.event.as_deref().unwrap_or("regular")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_torrent_announces_plainly_and_stays_off_dht_and_pex() {
    let tracker = MockTracker::start(60).await;
    let url = format!("{}?passkey={PASSKEY}", tracker.url());
    let t = standard_torrent(0, Some(url), true, 401);
    let mut node = node(|c| {
        c.trackers_enabled = true;
        c.pex_enabled = true;
        c.dht_enabled = true;
        c.dht_bind = "127.0.0.1:0".parse().unwrap();
        c.dht_allow_local = true;
        c.dht_bootstrap = vec!["127.0.0.1:9".into()];
    })
    .await;
    t.write_to(node.dir.path());
    node.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    let first = tracker.log_for(&t.id)[0].clone();

    // The announce URL is the one in the torrent: passkey first, then the client's parameters in the fixed order.
    assert!(first.raw_query.starts_with(&format!("passkey={PASSKEY}&info_hash=")), "{}", first.raw_query);
    let allowed = [
        "passkey", "info_hash", "peer_id", "port", "uploaded", "downloaded", "left", "numwant", "key", "compact",
        "supportcrypto", "requirecrypto", "event", "trackerid",
    ];
    for n in names(&first) {
        assert!(allowed.contains(&n), "unexpected parameter {n} in {}", first.raw_query);
    }
    // Honest identity: this client's own peer-id prefix and the real version, never another client's.
    assert!(first.peer_id.starts_with(b"-SX"), "{:?}", String::from_utf8_lossy(&first.peer_id));
    assert!(!first.peer_id.starts_with(b"-TR") && !first.peer_id.starts_with(b"-qB"));
    // Added complete: `started` with left=0, nothing downloaded, and no `completed` ever.
    assert_eq!((ev(&first), first.left, first.downloaded, first.uploaded), ("started", 0, 0, 0));
    assert!(first.numwant.unwrap_or(0) > 0 && first.numwant.unwrap_or(0) <= 100, "{:?}", first.numwant);

    // Pause: `stopped` with numwant=0; resume: a new `started`, the same key and peer_id inside the process.
    node.h.pause(&[t.id]);
    wait_for("stopped", 15, || async { tracker.log_for(&t.id).iter().any(|a| ev(a) == "stopped").then_some(()) }).await;
    node.h.resume(&[t.id]);
    wait_for("second started", 15, || async {
        (tracker.log_for(&t.id).iter().filter(|a| ev(a) == "started").count() == 2).then_some(())
    })
    .await;
    // Remove: `stopped` again.
    node.h.remove(&[t.id], false).await.unwrap();
    wait_for("stopped on remove", 15, || async {
        (tracker.log_for(&t.id).iter().filter(|a| ev(a) == "stopped").count() == 2).then_some(())
    })
    .await;

    let log = tracker.log_for(&t.id);
    let seq: Vec<&str> = log.iter().map(ev).collect();
    assert_eq!(seq, ["started", "stopped", "started", "stopped"], "{seq:?}");
    assert!(log.iter().all(|a| a.key == first.key && a.peer_id == first.peer_id), "key and peer_id are per process");
    assert_eq!(log[1].numwant, Some(0));
    assert_eq!(log[3].numwant, Some(0));
    assert!(log.iter().all(|a| a.left == 0 && a.downloaded == 0), "{log:?}");

    let d = node.h.diagnostics();
    assert_eq!((d.dht_lookups, d.dht_announces, d.pex_sent, d.pex_received), (0, 0, 0, 0), "{d:?}");
    node.shutdown(Duration::from_secs(5)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_torrent_keeps_exactly_its_own_trackers() {
    let tracker = MockTracker::start(60).await;
    let url = format!("{}?passkey={PASSKEY}", tracker.url());
    let t = standard_torrent(0, Some(url), true, 402);
    let node = node(|c| {
        c.trackers_enabled = true;
    })
    .await;
    t.write_to(node.dir.path());
    node.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    let d = node.h.torrent_detail(t.id).await.unwrap();
    assert_eq!(d.trackers.len(), 1, "{:?}", d.trackers);
    let host = tracker.url().trim_end_matches("announce").to_owned();
    assert!(d.trackers[0].url.starts_with(&host) && !d.trackers[0].url.contains(PASSKEY), "{}", d.trackers[0].url);
    assert!(!d.trackers[0].shown_url.contains(PASSKEY), "{}", d.trackers[0].shown_url);
    assert!(!format!("{:?}", d.trackers[0]).contains(PASSKEY), "Debug never prints the passkey");
}
