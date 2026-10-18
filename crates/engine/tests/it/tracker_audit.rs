//! What a tracker sees (SPEC 7B, 7C, FINDINGS.md): event order, per-session counters, identity.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;

fn param<'a>(a: &'a Announce, k: &str) -> Option<&'a str> {
    a.raw_query.split('&').find_map(|kv| kv.strip_prefix(&format!("{k}=")))
}

fn order(a: &Announce) -> Vec<&str> {
    a.raw_query.split('&').map(|kv| kv.split('=').next().unwrap()).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn announce_sequence_counters_and_identity() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(5_000_000, Some(tracker.url()), false, 201);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let mut leech = node(|c| {
        c.trackers_enabled = true;
        c.down_limit = Some(1_200_000);
    })
    .await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    let events = |tr: &MockTracker| -> Vec<Announce> { tr.log_for(&t.id) };
    wait_for("started", 15, || async { (!events(&tracker).is_empty()).then_some(()) }).await;
    let first = events(&tracker)[0].clone();
    assert_eq!(first.event.as_deref(), Some("started"));
    // The gate may hold `started` for up to a second, so some bytes can already have arrived;
    // they belong to this session. `left` is exact either way.
    assert!(first.downloaded <= t.total() && first.left <= t.total(), "{first:?}");
    assert_eq!(first.uploaded, 0);
    assert!(first.numwant.unwrap_or(0) > 0);

    wait_done(&leech, &t.id, 60).await;
    wait_for("completed", 15, || async {
        events(&tracker).iter().any(|a| a.event.as_deref() == Some("completed")).then_some(())
    })
    .await;
    // Pause -> stopped, resume -> started with fresh session counters.
    leech.h.pause(&[t.id]);
    wait_for("stopped", 15, || async {
        events(&tracker).iter().any(|a| a.event.as_deref() == Some("stopped")).then_some(())
    })
    .await;
    leech.h.resume(&[t.id]);
    wait_for("second started", 15, || async {
        (events(&tracker).iter().filter(|a| a.event.as_deref() == Some("started")).count() == 2).then_some(())
    })
    .await;
    let report = leech.shutdown(Duration::from_secs(10)).await;
    assert_eq!(report.stopped_confirmed, 1, "{report:?}");
    wait_for("final stopped", 5, || async {
        (events(&tracker).iter().filter(|a| a.event.as_deref() == Some("stopped")).count() == 2).then_some(())
    })
    .await;

    let log = events(&tracker);
    let seq: Vec<&str> = log.iter().map(|a| a.event.as_deref().unwrap_or("regular")).collect();
    assert_eq!(seq, ["started", "completed", "stopped", "started", "stopped"], "{seq:?}");
    // completed carries the totals so far: left 0, all payload counted
    let completed = &log[1];
    assert_eq!(completed.left, 0);
    assert!(completed.downloaded >= t.total() && completed.downloaded < t.total() + 1_000_000, "{}", completed.downloaded);
    // stopped: numwant=0, session totals
    assert_eq!(log[2].numwant, Some(0));
    assert_eq!(log[4].numwant, Some(0));
    assert!(log[2].downloaded >= t.total());
    // resume: a new session, counters from zero, left exact, still no second `completed`
    assert_eq!((log[3].uploaded, log[3].downloaded, log[3].left), (0, 0, 0));
    assert_eq!((log[4].uploaded, log[4].downloaded, log[4].left), (0, 0, 0));
    // identity: one key and one peer id for the whole process, across pause/resume
    assert!(log.iter().all(|a| a.key == log[0].key && a.peer_id == log[0].peer_id));
    assert!(log[0].peer_id.starts_with(&trs_metainfo::PEER_ID_PREFIX));
    assert!(log[0].peer_id[8..].iter().all(u8::is_ascii_alphanumeric));
    // wire format (SPEC 12 #18): Transmission-style order, nothing exotic
    let o = order(&log[0]);
    assert_eq!(&o[..6], ["info_hash", "peer_id", "port", "uploaded", "downloaded", "left"]);
    for bad in ["corrupt", "redundant", "no_peer_id", "ip", "ipv4", "ipv6"] {
        assert!(!o.contains(&bad), "{bad} must not be sent: {o:?}");
    }
    assert!(param(&log[0], "compact") == Some("1"));
    assert!(param(&log[0], "key").is_some_and(|k| k.len() == 8 && k.chars().all(|c| c.is_ascii_hexdigit() && !c.is_lowercase())));

    // Restart: new process identity, `started` with left=0 and no `completed`.
    let store = leech.store.clone();
    let dir = leech.dir;
    let mut again = node_in(dir, store, |c| c.trackers_enabled = true).await;
    wait_for("restart started", 15, || async { (events(&tracker).len() == 6).then_some(())  }).await;
    let r = events(&tracker)[5].clone();
    assert_eq!(r.event.as_deref(), Some("started"));
    assert_eq!((r.left, r.uploaded, r.downloaded), (0, 0, 0));
    assert_ne!(r.peer_id, log[0].peer_id, "peer id is per process");
    assert_ne!(r.key, log[0].key, "key is per process");
    again.shutdown(Duration::from_secs(10)).await;
    wait_for("stopped after restart", 5, || async { (events(&tracker).len() == 7).then_some(()) }).await;
    assert_eq!(events(&tracker)[6].event.as_deref(), Some("stopped"));
    assert!(events(&tracker).iter().filter(|a| a.event.as_deref() == Some("completed")).count() == 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torrent_added_complete_never_sends_completed() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 211);
    let mut seeder = node(|c| c.trackers_enabled = true).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    let a = tracker.log_for(&t.id)[0].clone();
    assert_eq!((a.event.as_deref(), a.left, a.downloaded), (Some("started"), 0, 0));
    seeder.shutdown(Duration::from_secs(10)).await;
    let seq: Vec<String> =
        tracker.log_for(&t.id).iter().map(|a| a.event.clone().unwrap_or_else(|| "regular".into())).collect();
    assert_eq!(seq, ["started", "stopped"]);
}

/// Regular announces come at `interval`, never earlier (takes ~65 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn regular_announce_follows_the_interval() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 221);
    let mut seeder = node(|c| c.trackers_enabled = true).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("regular announce", 90, || async { (tracker.log_for(&t.id).len() >= 2).then_some(()) }).await;
    let log = tracker.log_for(&t.id);
    assert_eq!(log[0].event.as_deref(), Some("started"));
    assert_eq!(log[1].event, None);
    let gap = log[1].at.duration_since(log[0].at).as_secs_f64();
    assert!((59.0..75.0).contains(&gap), "interval was {gap:.1}s");
    seeder.shutdown(Duration::from_secs(10)).await;
}

/// The `network.encryption` policy reaches the HTTP announce: prefer = `supportcrypto=1`,
/// require = `supportcrypto=1&requirecrypto=1`, off = neither, always between `compact` and `event`.
async fn started_query(policy: trs_engine::config::EncryptionPolicy, seed: u64) -> String {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(0, Some(tracker.url()), false, seed);
    let mut n = node(|c| {
        c.trackers_enabled = true;
        c.encryption = policy;
    })
    .await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    let q = tracker.log_for(&t.id)[0].raw_query.clone();
    n.shutdown(Duration::from_secs(10)).await;
    q
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encryption_policy_is_advertised_to_http_trackers() {
    use trs_engine::config::EncryptionPolicy as Enc;
    let q = started_query(Enc::Prefer, 231).await;
    assert!(q.contains("&compact=1&supportcrypto=1&event=started"), "{q}");
    assert!(!q.contains("requirecrypto"), "{q}");
    let q = started_query(Enc::Require, 232).await;
    assert!(q.contains("&compact=1&supportcrypto=1&requirecrypto=1&event=started"), "{q}");
    let q = started_query(Enc::Off, 233).await;
    assert!(q.contains("&compact=1&event=started") && !q.contains("crypto"), "{q}");
}

/// A settings change is picked up by the next announce without causing one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encryption_change_applies_from_the_next_announce_without_an_extra_one() {
    use trs_engine::config::EncryptionPolicy as Enc;
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 234);
    let mut n = node(|c| {
        c.trackers_enabled = true;
        c.encryption = Enc::Prefer;
    })
    .await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    assert!(tracker.log_for(&t.id)[0].raw_query.contains("&supportcrypto=1&event=started"));
    n.h.update_config(|c| c.encryption = Enc::Require);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(tracker.log_for(&t.id).len(), 1, "a settings change must not announce");
    n.h.pause(&[t.id]);
    wait_for("stopped", 15, || async { (tracker.log_for(&t.id).len() >= 2).then_some(()) }).await;
    let log = tracker.log_for(&t.id);
    assert_eq!(log[1].event.as_deref(), Some("stopped"));
    assert!(log[1].raw_query.contains("&compact=1&supportcrypto=1&requirecrypto=1&event=stopped"), "{}", log[1].raw_query);
    n.shutdown(Duration::from_secs(10)).await;
}
