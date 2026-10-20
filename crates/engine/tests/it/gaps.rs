//! Behaviours the agent interface relies on: wanted bytes after priorities, Cold detail,
//! re-announce answers, remove that waits, peers surviving pause/resume.
use crate::common;
use std::time::{Duration, Instant};

use common::*;
use trs_engine::*;

async fn seeder_of(t: &TestTorrent, rate: Option<u64>) -> Node {
    let n = node(|c| c.up_limit = rate).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wanted_bytes_and_cold_detail_follow_file_priorities() {
    let t = standard_torrent(1_000_000, None, false, 901);
    let n = node(|_| {}).await;
    // Paused, so it has no actor: everything comes from the Cold record and the stored torrent.
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { paused: true, ..Default::default() }).await.unwrap();
    let before = n.row(&t.id).await.unwrap();
    assert_eq!(before.wanted_bytes, t.total());
    let mut prios = vec![FilePriority::Normal; t.files.len()];
    prios[2] = FilePriority::Skip;
    prios[3] = FilePriority::High;
    n.h.set_file_priorities(t.id, prios.clone()).await.unwrap();
    let after = n.row(&t.id).await.unwrap();
    assert_eq!(after.wanted_bytes, t.total() - t.files[2].1.len() as u64, "the skipped file is not wanted");
    let d = n.h.torrent_detail(t.id).await.unwrap();
    let got: Vec<FilePriority> = d.files.iter().map(|f| f.priority).collect();
    assert_eq!(got, prios, "a Cold torrent reports the priorities it has");
    // Back to all Normal.
    n.h.set_file_priorities(t.id, vec![FilePriority::Normal; t.files.len()]).await.unwrap();
    assert_eq!(n.row(&t.id).await.unwrap().wanted_bytes, t.total());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_given_to_a_paused_add_are_used_on_resume() {
    let t = standard_torrent(300_000, None, false, 902);
    let seeder = seeder_of(&t, None).await;
    let n = node(|_| {}).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { paused: true, peers: vec![seeder.addr()], ..Default::default() })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(n.state(&t.id).await, Some(TorrentState::Paused));
    n.h.resume(&[t.id]);
    wait_done(&n, &t.id, 20).await;
    t.verify(n.dir.path(), &[]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_redials_known_peers_at_once() {
    let t = standard_torrent(6_000_000, None, false, 903);
    let seeder = seeder_of(&t, Some(600_000)).await;
    let n = node(|_| {}).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("connected and downloading", 20, || async { (n.row(&t.id).await?.done_bytes > 200_000).then_some(()) }).await;
    n.h.pause(&[t.id]);
    wait_for("paused", 5, || async { (n.state(&t.id).await? == TorrentState::Paused).then_some(()) }).await;
    let at = n.row(&t.id).await.unwrap().done_bytes;
    n.h.resume(&[t.id]);
    // No tracker, no DHT, no new hint: only what the engine remembers can bring the peer back.
    let t0 = Instant::now();
    wait_for("data flows again", 10, || async { (n.row(&t.id).await?.done_bytes > at + 100_000).then_some(()) }).await;
    assert!(t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_replies_after_the_files_are_gone() {
    let t = standard_torrent(1_000_000, None, false, 904);
    let n = seeded_node(&t).await;
    let root = n.dir.path().join(&t.name);
    assert!(root.exists());
    // Hot torrent with a live actor.
    let leech_dir = tempfile::tempdir().unwrap();
    let _ = leech_dir;
    n.h.remove(&[t.id], true).await.unwrap();
    // No waiting: the call returned only when the deletion was done.
    assert!(!root.exists(), "files deleted before the reply");
    assert!(n.row(&t.id).await.is_none());
    // Removing something that is not there is not an error and does not hang.
    n.h.remove(&[t.id], true).await.unwrap();
}

async fn seeded_node(t: &TestTorrent) -> Node {
    seeder_of(t, None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remove_of_a_downloading_torrent_waits_for_the_actor_too() {
    let t = standard_torrent(6_000_000, None, false, 905);
    let seeder = seeder_of(&t, Some(500_000)).await;
    let n = node(|_| {}).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("some data", 20, || async { (n.row(&t.id).await?.done_bytes > 200_000).then_some(()) }).await;
    n.h.remove(&[t.id], true).await.unwrap();
    assert!(!n.dir.path().join(&t.name).exists(), "deleted when the call returns");
    assert!(n.row(&t.id).await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reannounce_reports_each_tracker_and_the_gate() {
    let tracker = MockTracker::start(1800).await;
    let t = standard_torrent(0, Some(tracker.url()), false, 906);
    let n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("started", 20, || async {
        tracker.log_for(&t.id).iter().any(|a| a.event.as_deref() == Some("started")).then_some(())
    })
    .await;
    // The engine has the answer too (the tracker logs the request before it replies).
    wait_for("answer seen", 20, || async {
        let d = n.h.torrent_detail(t.id).await.ok()?;
        d.trackers.iter().any(|x| x.status.starts_with("announced")).then_some(())
    })
    .await;
    // The session is open and the min interval (1800 s) is running: the answer says so.
    let r = n.h.reannounce(&[t.id]).await.unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].id, t.id);
    assert!(!r[0].url_redacted.contains("127.0.0.1:0"), "{}", r[0].url_redacted);
    assert!(!r[0].sent, "{r:?}");
    let until = r[0].not_before_ms.expect("a time");
    assert!(until > trs_engine::util::unix_ms() + 1_000_000, "about 30 minutes away: {until}");
    // Unknown torrents and a disabled tracker give an empty answer, not an error.
    assert!(n.h.reannounce(&[[9; 20]]).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_given_to_a_paused_magnet_are_used_on_resume() {
    let t = standard_torrent(200_000, None, false, 907);
    let seeder = seeder_of(&t, None).await;
    let n = node(|_| {}).await;
    let uri = format!("magnet:?xt=urn:btih:{}&dn={}", hex_of(&t.id), t.name);
    n.h.add_magnet(&uri, AddOptions { paused: true, peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    n.h.resume(&[t.id]);
    wait_done(&n, &t.id, 30).await;
    t.verify(n.dir.path(), &[]);
}
