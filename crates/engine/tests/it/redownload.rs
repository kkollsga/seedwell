//! A finished torrent whose wanted pieces are no longer all verified is a download again (SPEC 8.3,
//! 7B.2): files ticked after it finished, a recheck that finds pieces missing, a restart in the
//! middle. The tracker sees the true `left`, never a second `completed` and no extra `started`.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;

const S01: [FilePriority; 4] = [FilePriority::Normal, FilePriority::Normal, FilePriority::Skip, FilePriority::Normal];

fn seq(tr: &MockTracker, id: &TorrentId) -> Vec<String> {
    tr.log_for(id).iter().map(|a| a.event.clone().unwrap_or_else(|| "regular".into())).collect()
}

fn count(tr: &MockTracker, id: &TorrentId, ev: &str) -> usize {
    tr.log_for(id).iter().filter(|a| a.event.as_deref() == Some(ev)).count()
}

async fn seeder_on(tracker: &MockTracker, t: &TestTorrent) -> Node {
    let seeder = node(|c| c.trackers_enabled = true).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    wait_for("seeder started", 10, || async { (count(tracker, &t.id, "started") >= 1).then_some(()) }).await;
    seeder
}

/// The row of a download again: Downloads, not Recent, not Sharing.
fn assert_downloading(r: &TorrentRow, what: &str) {
    assert_eq!(r.state, TorrentState::Downloading, "{what}: {r:?}");
    assert!(r.seed_group.is_none(), "{what}: not in Sharing");
    assert!(r.finished_at.is_none(), "{what}: not in Recent");
}

/// S01 finished (partial selection), the user ticks S02: Downloading again, `left` is honest, no
/// `completed` until it really finishes, then exactly one; finished again with a new time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticking_more_files_after_finishing_downloads_again() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(7_000_000, Some(tracker.url()), true, 301);
    let seeder = seeded_peer(&tracker, &t).await;
    let leech = node(|c| {
        c.trackers_enabled = true;
        c.down_limit = Some(1_500_000);
    })
    .await;
    // A hit-and-run floor and a time goal: neither may act on a torrent that downloads again.
    let goal = SeedGoal { seed_secs: Some(1), min_seed_secs: 3600, ..SeedGoal::FOREVER };
    leech
        .h
        .add_torrent_file(
            t.bytes.clone(),
            AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), seed_goal: Some(goal), ..Default::default() },
        )
        .await
        .unwrap();
    wait_done(&leech, &t.id, 60).await;
    let first = leech.row(&t.id).await.unwrap();
    let t1 = first.finished_at.expect("finished_at after S01");
    assert!(first.seed_group.is_some(), "S01 is in Sharing");
    assert!(first.wanted_bytes < t.total());
    assert_eq!(count(&tracker, &t.id, "completed"), 0, "a partial selection never sends completed");

    let mut ev = leech.h.subscribe();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    leech.h.set_file_priorities(t.id, vec![FilePriority::Normal; 4]).await.unwrap();
    let r = wait_for("downloading again", 10, || async {
        let r = leech.row(&t.id).await?;
        (r.state == TorrentState::Downloading).then_some(r)
    })
    .await;
    assert_downloading(&r, "after ticking S02");
    assert_eq!(r.wanted_bytes, t.total());
    // Progress over the wanted bytes, a speed and a time left.
    let r = wait_for("speed and eta", 20, || async {
        let r = leech.row(&t.id).await?;
        (r.down_rate > 0 && r.eta_secs.is_some() && r.done_bytes > first.done_bytes).then_some(r)
    })
    .await;
    assert_downloading(&r, "while downloading");
    assert!(r.done_bytes < r.wanted_bytes);
    // The time goal (1 s) and the floor do not touch it.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let r = leech.row(&t.id).await.unwrap();
    assert_downloading(&r, "after the goal time");
    assert_eq!(count(&tracker, &t.id, "started"), 2, "seeder + leech, no extra started");

    // Leave: the `stopped` carries the true `left` (the missing bytes), and no `completed` went out.
    leech.h.pause(&[t.id]);
    wait_for("stopped", 15, || async { (count(&tracker, &t.id, "stopped") >= 1).then_some(()) }).await;
    let stopped = tracker.log_for(&t.id).into_iter().find(|a| a.event.as_deref() == Some("stopped")).unwrap();
    assert!(stopped.left > 0 && stopped.left < t.total(), "left {}", stopped.left);
    assert_eq!(count(&tracker, &t.id, "completed"), 0);
    leech.h.resume(&[t.id]);
    wait_done(&leech, &t.id, 90).await;
    t.verify(leech.dir.path(), &[]);
    let done = leech.row(&t.id).await.unwrap();
    assert!(done.finished_at.unwrap() > t1, "finished_at is the new time");
    assert!(done.seed_group.is_some());
    wait_for("completed", 15, || async { (count(&tracker, &t.id, "completed") == 1).then_some(()) }).await;
    let c = tracker.log_for(&t.id).into_iter().find(|a| a.event.as_deref() == Some("completed")).unwrap();
    assert_eq!(c.left, 0);
    let mut finished = 0;
    while let Ok(e) = ev.try_recv() {
        finished += usize::from(matches!(e, EngineEvent::Finished { .. }));
    }
    assert_eq!(finished, 1, "the finished notification again, once");
    assert_eq!(count(&tracker, &t.id, "completed"), 1);
}

async fn seeded_peer(tracker: &MockTracker, t: &TestTorrent) -> Node {
    seeder_on(tracker, t).await
}

/// A finished torrent whose data is damaged: the recheck finds pieces missing, it downloads them
/// (a download again, not Recent) and finishes without a second `completed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recheck_that_finds_missing_pieces_downloads_again_without_second_completed() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(5_000_000, Some(tracker.url()), true, 311);
    let seeder = seeder_on(&tracker, &t).await;
    let leech = node(|c| {
        c.trackers_enabled = true;
        c.down_limit = Some(1_500_000);
    })
    .await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    wait_for("completed", 15, || async { (count(&tracker, &t.id, "completed") == 1).then_some(()) }).await;
    let t1 = leech.row(&t.id).await.unwrap().finished_at.unwrap();

    // Damage the big file: zero its middle third, then recheck.
    let p = t.file_path(leech.dir.path(), 2);
    let mut bytes = std::fs::read(&p).unwrap();
    let third = bytes.len() / 3;
    bytes[third..2 * third].fill(0);
    std::fs::write(&p, &bytes).unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let mut ev = leech.h.subscribe();
    leech.h.recheck(&[t.id]);
    let r = wait_for("downloading again", 30, || async {
        let r = leech.row(&t.id).await?;
        (r.state == TorrentState::Downloading && r.done_bytes < t.total()).then_some(r)
    })
    .await;
    assert_downloading(&r, "after the recheck");
    assert_eq!(r.wanted_bytes, t.total());
    leech.h.pause(&[t.id]);
    wait_for("stopped", 15, || async { (count(&tracker, &t.id, "stopped") >= 1).then_some(()) }).await;
    let stopped = tracker.log_for(&t.id).into_iter().find(|a| a.event.as_deref() == Some("stopped") && a.left > 0);
    assert!(stopped.is_some_and(|s| s.left < t.total()), "stopped carries the true left");
    leech.h.resume(&[t.id]);
    wait_done(&leech, &t.id, 90).await;
    t.verify(leech.dir.path(), &[]);
    let done = leech.row(&t.id).await.unwrap();
    assert!(done.finished_at.unwrap() > t1);
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(count(&tracker, &t.id, "completed"), 1, "completed exactly once: {:?}", seq(&tracker, &t.id));
    let mut finished = 0;
    while let Ok(e) = ev.try_recv() {
        finished += usize::from(matches!(e, EngineEvent::Finished { .. }));
    }
    assert_eq!(finished, 1);
}

/// A restart in the middle of the re-download: still a download (not Recent), then it finishes
/// with `completed` once over the whole run, and `left` is the missing bytes at the new `started`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_in_the_middle_of_a_re_download() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(7_000_000, Some(tracker.url()), true, 321);
    let seeder = seeder_on(&tracker, &t).await;
    let mut leech = node(|c| {
        c.trackers_enabled = true;
        c.down_limit = Some(1_000_000);
        c.resume_interval = Duration::from_millis(500);
    })
    .await;
    leech
        .h
        .add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), ..Default::default() })
        .await
        .unwrap();
    wait_done(&leech, &t.id, 60).await;
    // The leech's own `started` (staggered at launch) is out before the files are ticked.
    wait_for("leech started", 15, || async { (count(&tracker, &t.id, "started") >= 2).then_some(()) }).await;
    leech.h.set_file_priorities(t.id, vec![FilePriority::Normal; 4]).await.unwrap();
    let mid = wait_for("some of S02", 30, || async {
        let r = leech.row(&t.id).await?;
        (r.state == TorrentState::Downloading && r.done_bytes > 2_500_000).then_some(r.done_bytes)
    })
    .await;
    assert!(mid < t.total());
    let store = leech.store.clone();
    leech.shutdown(Duration::from_secs(10)).await;
    wait_for("stopped at shutdown", 10, || async { (count(&tracker, &t.id, "stopped") >= 1).then_some(()) }).await;
    // What was persisted says: not finished.
    let rec = store.torrent(&t.id).unwrap();
    assert!(rec.record.finished_at.is_none(), "the record no longer says finished");
    let stopped = tracker.log_for(&t.id).into_iter().rev().find(|a| a.event.as_deref() == Some("stopped")).unwrap();
    assert!(stopped.left > 0);

    let leech_dir = leech.dir;
    let b = node_in(leech_dir, store, |c| {
        c.trackers_enabled = true;
        // Slow enough that the (staggered) `started` of the new session goes out mid-download.
        c.down_limit = Some(400_000);
    })
    .await;
    let r = wait_for("row after restart", 10, || async { b.row(&t.id).await }).await;
    assert!(r.finished_at.is_none() && r.seed_group.is_none(), "{r:?}");
    wait_for("downloading after restart", 30, || async {
        let r = b.row(&t.id).await?;
        (r.state == TorrentState::Downloading).then_some(())
    })
    .await;
    wait_done(&b, &t.id, 120).await;
    t.verify(b.dir.path(), &[]);
    wait_for("completed", 15, || async { (count(&tracker, &t.id, "completed") == 1).then_some(()) }).await;
    // The new session announced the true `left`.
    let started: Vec<Announce> = tracker.log_for(&t.id).into_iter().filter(|a| a.event.as_deref() == Some("started")).collect();
    assert_eq!(started.len(), 3, "seeder, leech, leech after the restart: {:?}", seq(&tracker, &t.id));
    let dbg: Vec<_> = tracker.log_for(&t.id).iter().map(|a| (a.event.clone(), a.left, a.downloaded, a.port)).collect();
    assert!(started[2].left > 0 && started[2].left < t.total(), "total {}: {dbg:?}", t.total());
    assert_eq!(count(&tracker, &t.id, "completed"), 1);
}

/// It takes a download slot like any download: with one slot, one of the two waits in the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_again_takes_a_queue_slot() {
    let a = standard_torrent(6_000_000, None, false, 331);
    let b = standard_named("other", 6_000_000, None, false, 332);
    let seeder = node(|_| {}).await;
    for t in [&a, &b] {
        t.write_to(seeder.dir.path());
        seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&seeder, &t.id, 30).await;
    }
    let leech = node(|c| {
        c.max_active_downloads = 1;
        c.down_limit = Some(1_000_000);
    })
    .await;
    leech.h.add_torrent_file(a.bytes.clone(), AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), ..Default::default() }).await.unwrap();
    wait_done(&leech, &a.id, 60).await;
    leech.h.add_torrent_file(b.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("b downloads", 10, || async { (leech.state(&b.id).await? == TorrentState::Downloading).then_some(()) }).await;
    leech.h.set_file_priorities(a.id, vec![FilePriority::Normal; 4]).await.unwrap();
    let (sa, sb) = wait_for("one slot", 10, || async {
        let (sa, sb) = (leech.state(&a.id).await?, leech.state(&b.id).await?);
        (sa != TorrentState::Seeding && sa != TorrentState::LowPower).then_some((sa, sb))
    })
    .await;
    let mut states = [sa, sb];
    states.sort_by_key(|s| format!("{s:?}"));
    assert_eq!(states, [TorrentState::Downloading, TorrentState::Queued], "{sa:?} {sb:?}");
    let ra = leech.row(&a.id).await.unwrap();
    assert!(ra.seed_group.is_none() && ra.finished_at.is_none());
}

/// Only `one.bin` wanted and finished; then the small `z.dat` is ticked while the big `two.bin` stays
/// skipped. The new download counts from where the torrent was: progress starts near 0 and reaches
/// 100 %, the time left is about the new file (not the skipped one), the base does not move when
/// more files are ticked during the download, and a skipped file that only holds the edge piece it
/// shares with a wanted neighbour shows nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reopened_download_counts_from_where_it_was() {
    let t = standard_torrent(6_000_000, None, false, 341);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let leech = node(|c| c.down_limit = Some(30_000)).await;
    let only_one = vec![FilePriority::Normal, FilePriority::Skip, FilePriority::Skip, FilePriority::Skip];
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], file_priorities: only_one, ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    let first = leech.row(&t.id).await.unwrap();
    assert_eq!((first.session_done, first.session_size), (first.wanted_bytes, first.wanted_bytes), "a finished torrent is whole");
    // two.bin shares the edge piece with one.bin: held, but not shown as downloaded.
    let d = leech.h.torrent_detail(t.id).await.unwrap();
    assert_eq!(d.files[2].done, 0, "an edge-only skipped file shows nothing: {:?}", d.files[2]);
    assert!(d.files[0].done == d.files[0].length);

    // Tick z.dat only: a download again that starts at ~0 for the new file.
    leech.h.set_file_priorities(t.id, vec![FilePriority::Normal, FilePriority::Skip, FilePriority::Skip, FilePriority::Normal]).await.unwrap();
    let r = wait_for("downloading again", 10, || async {
        let r = leech.row(&t.id).await?;
        (r.state == TorrentState::Downloading).then_some(r)
    })
    .await;
    let piece = 32 * 1024;
    let base = r.wanted_bytes - r.session_size;
    assert!(base + piece >= first.wanted_bytes && base <= first.wanted_bytes + piece, "base {base} vs {}", first.wanted_bytes);
    assert!(r.session_size < 200_000, "the session is the new file: {}", r.session_size);
    assert!(r.session_done * 2 < r.session_size, "starts near 0: {} of {}", r.session_done, r.session_size);
    let r = wait_for("eta", 20, || async {
        let r = leech.row(&t.id).await?;
        (r.down_rate > 0 && r.eta_secs.is_some()).then_some(r)
    })
    .await;
    // The time left is about the new file, not the whole skipped two.bin.
    let eta = r.eta_secs.unwrap();
    assert!(eta <= 20, "eta {eta} s for {} bytes at {} B/s", r.session_size - r.session_done, r.down_rate);
    // Ticking more while it downloads keeps the starting point.
    leech.h.set_file_priorities(t.id, vec![FilePriority::Normal; 4]).await.unwrap();
    let r2 = wait_for("still downloading", 10, || async {
        let r = leech.row(&t.id).await?;
        (r.state == TorrentState::Downloading && r.wanted_bytes == t.total()).then_some(r)
    })
    .await;
    let base2 = r2.wanted_bytes - r2.session_size;
    assert!(base2.abs_diff(base) <= piece, "base moved: {base} -> {base2}");
    assert!(r2.session_done < r2.session_size);
    // Skipped again, it finishes at 100 % and the session is whole.
    leech.h.set_file_priorities(t.id, vec![FilePriority::Normal, FilePriority::Skip, FilePriority::Skip, FilePriority::Normal]).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    let done = leech.row(&t.id).await.unwrap();
    assert_eq!((done.session_done, done.session_size), (done.wanted_bytes, done.wanted_bytes));
}
