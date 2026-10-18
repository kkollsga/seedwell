use crate::common;
use std::time::{Duration, Instant};

use common::*;
use trs_engine::*;

/// 1 seeder + 3 leechers on loopback with a mock tracker: every byte arrives intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seeder_and_three_leechers_with_tracker() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(24_000_000, Some(tracker.url()), false, 21);
    let seeder = node(|c| c.trackers_enabled = true).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    // The seeder's `started` must be registered before the leechers ask.
    wait_for("seeder announce", 10, || async {
        tracker.log_for(&t.id).iter().any(|a| a.event.as_deref() == Some("started")).then_some(())
    })
    .await;

    let started = Instant::now();
    let mut leechers = Vec::new();
    for _ in 0..3 {
        let n = node(|c| c.trackers_enabled = true).await;
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        leechers.push(n);
    }
    for n in &leechers {
        wait_done(n, &t.id, 120).await;
    }
    eprintln!("3 x {} MB in {:?}", t.total() / 1_000_000, started.elapsed());
    for n in &leechers {
        t.verify(n.dir.path(), &[]);
        let row = n.row(&t.id).await.unwrap();
        assert_eq!(row.done_bytes, t.total());
        assert_eq!(row.total_bytes, t.total());
    }
    let seed_row = seeder.row(&t.id).await.unwrap();
    assert!(seed_row.uploaded_total >= t.total(), "seeder uploaded {}", seed_row.uploaded_total);
}

/// Per-file priority 0 skips a file; un-skipping later completes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn skipped_file_is_not_downloaded() {
    let t = standard_torrent(3_000_000, None, false, 31);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let leech = node(|_| {}).await;
    let prios = vec![FilePriority::Normal, FilePriority::Normal, FilePriority::Skip, FilePriority::Normal];
    let opts = AddOptions { peers: vec![seeder.addr()], file_priorities: prios, ..AddOptions::default() };
    leech.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    // Everything but file 2 is exact; the skipped file is absent or mostly holes.
    t.verify(leech.dir.path(), &[2]);
    let row = leech.row(&t.id).await.unwrap();
    assert!(row.done_bytes < t.total(), "skipped bytes still count as left");
    assert!(row.downloaded_total < t.total() - 2_000_000, "downloaded {}", row.downloaded_total);
    let skipped = t.file_path(leech.dir.path(), 2);
    if let Ok(got) = std::fs::read(&skipped) {
        let same = got.iter().zip(&t.files[2].1).filter(|(a, b)| a == b).count();
        assert!(same < got.len() / 10, "skipped file must be (nearly) empty, {same} of {} bytes match", got.len());
    }
    let detail = leech.h.torrent_detail(t.id).await.unwrap();
    assert_eq!(detail.files[2].priority, FilePriority::Skip);

    // Un-skip: the rest arrives.
    leech.h.set_file_priorities(t.id, vec![FilePriority::Normal; 4]).await.unwrap();
    wait_for("full download after un-skipping", 60, || async {
        (leech.row(&t.id).await?.done_bytes == t.total()).then_some(())
    })
    .await;
    wait_done(&leech, &t.id, 30).await;
    t.verify(leech.dir.path(), &[]);
}

/// Pause mid-download, nothing moves, resume completes; a global limit shapes the rate.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_resume_and_rate_limit() {
    let t = standard_torrent(6_000_000, None, false, 41);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let leech = node(|c| c.down_limit = Some(1_500_000)).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    let t0 = Instant::now();
    wait_for("some progress", 30, || async {
        let r = leech.row(&t.id).await?;
        (r.done_bytes > 1_000_000).then_some(())
    })
    .await;
    leech.h.pause(&[t.id]);
    wait_for("paused", 10, || async { (leech.state(&t.id).await? == TorrentState::Paused).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let a = leech.row(&t.id).await.unwrap().done_bytes;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let b = leech.row(&t.id).await.unwrap().done_bytes;
    assert_eq!(a, b, "no progress while paused");
    assert!(b < t.total());
    leech.h.resume(&[t.id]);
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    let elapsed = t0.elapsed().as_secs_f64();
    // ~9 MB at 1.5 MB/s cannot finish much faster than 4 s (token bucket allows a short burst).
    assert!(elapsed > 4.0, "limit not applied: {elapsed:.1}s for {} bytes", t.total());
}

/// Priorities changed while the download runs (Get Info > Files): a file skipped mid-download is not
/// fetched any more, checking it again completes it, and a finished torrent with a newly checked file
/// downloads only that file.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn priorities_changed_mid_download_and_after_finishing() {
    let t = standard_torrent(8_000_000, None, false, 32);
    let seeder = node(|c| c.up_limit = Some(1_500_000)).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..AddOptions::default() }).await.unwrap();
    wait_for("some data", 30, || async { (leech.row(&t.id).await?.done_bytes > 500_000).then_some(()) }).await;

    // Skip the big file while the download runs.
    let n = t.files.len();
    let big = 2; // the 9 MB file
    let mut prios = vec![FilePriority::Normal; n];
    prios[big] = FilePriority::Skip;
    leech.h.set_file_priorities(t.id, prios.clone()).await.unwrap();
    let row = leech.row(&t.id).await.unwrap();
    assert_eq!(row.wanted_bytes, t.total() - t.files[big].1.len() as u64, "wanted shrinks at once");
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[big]);
    let d = leech.h.torrent_detail(t.id).await.unwrap();
    assert_eq!(d.files[big].priority, FilePriority::Skip);
    assert!(d.files[big].done < d.files[big].length, "the skipped file is not complete");
    let before = leech.row(&t.id).await.unwrap();
    assert!(before.done_bytes < t.total());

    // Finished (seeding) with one file unwanted: checking it resumes downloading just that file.
    prios[big] = FilePriority::High;
    leech.h.set_file_priorities(t.id, prios).await.unwrap();
    wait_for("the checked file arrives", 60, || async { (leech.row(&t.id).await?.done_bytes == t.total()).then_some(()) }).await;
    wait_done(&leech, &t.id, 30).await;
    t.verify(leech.dir.path(), &[]);
    let after = leech.row(&t.id).await.unwrap();
    let extra = after.downloaded_total - before.downloaded_total;
    assert!(extra <= t.files[big].1.len() as u64 + 2 * 1_048_576, "only the new file was fetched, got {extra} bytes");
}
