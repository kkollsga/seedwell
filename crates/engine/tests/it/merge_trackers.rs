//! Adding a public torrent that is already there merges its tracker URLs into the existing one
//! (SPEC 7B.2): deduplicated, at most 8 added over the torrent's life, never for private torrents,
//! and the new trackers announce through the normal hub pacing.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;

fn magnet(id: &[u8], trackers: &[String]) -> String {
    let mut m = format!("magnet:?xt=urn:btih:{}", hex_of(id));
    for t in trackers {
        m.push_str("&tr=");
        for b in t.bytes() {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                m.push(b as char);
            } else {
                m.push_str(&format!("%{b:02X}"));
            }
        }
    }
    m
}

fn announce_count(n: &Node, id: &[u8; 20]) -> usize {
    n.store.torrent(id).unwrap().record.announce.unwrap_or_default().iter().flatten().count()
}

async fn started(t: &MockTracker, id: &[u8]) {
    wait_for("started", 20, || async { t.log_for(id).iter().any(|a| a.event.as_deref() == Some("started")).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_torrent_file_brings_its_tracker_and_it_announces() {
    let (a, b) = (MockTracker::start(1800).await, MockTracker::start(1800).await);
    let t = standard_torrent(0, Some(a.url()), false, 1301);
    let again = standard_torrent(0, Some(b.url()), false, 1301);
    assert_eq!(t.id, again.id, "same info dictionary");
    let n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    started(&a, &t.id).await;
    assert!(b.log_for(&t.id).is_empty());

    let r = n.h.add_torrent_file(again.bytes.clone(), AddOptions::default()).await;
    match r {
        Err(EngineError::TrackersMerged { id, added, .. }) => assert_eq!((id, added), (t.id, 1)),
        other => panic!("{other:?}"),
    }
    assert_eq!(announce_count(&n, &t.id), 2);
    started(&b, &t.id).await;
    // The same file again: nothing new, the plain "already added" answer.
    assert!(matches!(n.h.add_torrent_file(again.bytes.clone(), AddOptions::default()).await, Err(EngineError::Duplicate(_))));
    assert_eq!(announce_count(&n, &t.id), 2);
    // The old tracker was not announced to again by the merge.
    assert_eq!(a.log_for(&t.id).iter().filter(|x| x.event.as_deref() == Some("started")).count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn magnet_trackers_merge_dedupe_and_cap_at_eight() {
    let a = MockTracker::start(1800).await;
    let b = MockTracker::start(1800).await;
    let t = standard_torrent(0, Some(a.url()), false, 1302);
    let n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    started(&a, &t.id).await;

    // The torrent's own tracker and a repeat do not count; the https twin of A is the same tracker.
    let m = magnet(&t.id, &[a.url(), b.url(), b.url(), a.url().replace("http://", "https://"), "not a url".into()]);
    match n.h.add_magnet(&m, AddOptions::default()).await {
        Err(EngineError::TrackersMerged { added, name, .. }) => {
            assert_eq!(added, 1);
            assert!(!name.is_empty());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(announce_count(&n, &t.id), 2);
    started(&b, &t.id).await;

    // Twelve more: only seven fit (eight added in all).
    let many: Vec<String> = (0..12).map(|i| format!("http://127.0.0.1:{}/announce", 40_000 + i)).collect();
    match n.h.add_magnet(&magnet(&t.id, &many), AddOptions::default()).await {
        Err(EngineError::TrackersMerged { added, .. }) => assert_eq!(added, 7),
        other => panic!("{other:?}"),
    }
    assert_eq!(announce_count(&n, &t.id), 1 + 8);
    // The cap is for the torrent's life.
    let more = vec!["http://127.0.0.1:41000/announce".to_owned()];
    assert!(matches!(n.h.add_magnet(&magnet(&t.id, &more), AddOptions::default()).await, Err(EngineError::Duplicate(_))));
    assert_eq!(announce_count(&n, &t.id), 9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn private_torrents_never_merge() {
    let (a, b) = (MockTracker::start(1800).await, MockTracker::start(1800).await);
    let t = standard_torrent(0, Some(a.url()), true, 1303);
    let again = standard_torrent(0, Some(b.url()), true, 1303);
    assert_eq!(t.id, again.id);
    let n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    started(&a, &t.id).await;
    // Another .torrent file, and a magnet naming another tracker: both are "already added".
    assert!(matches!(n.h.add_torrent_file(again.bytes.clone(), AddOptions::default()).await, Err(EngineError::Duplicate(_))));
    assert!(matches!(n.h.add_magnet(&magnet(&t.id, &[b.url()]), AddOptions::default()).await, Err(EngineError::Duplicate(_))));
    assert_eq!(announce_count(&n, &t.id), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(b.log_for(&t.id).is_empty(), "the private torrent never contacted the new tracker");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_magnet_whose_metadata_has_not_arrived_does_not_merge() {
    // Nobody can say whether it is private yet.
    let t = standard_torrent(0, None, false, 1304);
    let n = node(|c| c.trackers_enabled = true).await;
    let m1 = magnet(&t.id, &[]);
    n.h.add_magnet(&m1, AddOptions::default()).await.unwrap();
    let b = MockTracker::start(1800).await;
    assert!(matches!(n.h.add_magnet(&magnet(&t.id, &[b.url()]), AddOptions::default()).await, Err(EngineError::Duplicate(_))));
}
