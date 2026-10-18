//! Direct HTTP(S) downloads (SPEC 7D) inside the engine: the shared queue, the global limit,
//! restarts, category folders, and never being part of Sharing.
use crate::common;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::*;
use trs_engine::config::DetectionConfig;
use trs_engine::*;

/// Books finish in the plain folder `<lib>/Books`.
fn organizer(lib: &Path) -> impl FnOnce(&mut EngineConfig) + use<> {
    let lib = lib.to_path_buf();
    move |c| {
        c.detect = Some(DetectionConfig::default());
        c.category_locations[Category::Book.index()].finished = Some(lib.join("Books"));
    }
}

async fn wait_state(h: &EngineHandle, id: &TorrentId, st: TorrentState, secs: u64) -> TorrentRow {
    wait_for(&format!("{st:?}"), secs, || async {
        let r = row_h(h, id).await?;
        (r.state == st).then_some(r)
    })
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_direct_download_completes_goes_to_its_categorys_folder_and_is_not_sharing() {
    let body = pattern_bytes(400_000, 1);
    let srv = serve_files(vec![ServedFile::new("/Jane%20Austen%20-%20Emma.epub", body.clone())]).await;
    let (state, dl, lib) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let n = snode(state.path(), dl.path(), None, organizer(lib.path())).await;
    let mut ev = n.h.subscribe();
    let added = n
        .h
        .add_download(&srv.url("/Jane%20Austen%20-%20Emma.epub"), DownloadOptions { category: Some(Category::Book), ..DownloadOptions::default() })
        .await
        .unwrap();
    let s = n.h.snapshot().await.unwrap();
    let row = s.rows.iter().find(|r| r.id == added.id).unwrap();
    assert_eq!(row.kind, RowKind::Direct);
    assert_eq!(row.tracker_host, srv.addr.to_string());
    assert_eq!(row.url.as_deref(), Some(srv.url("/Jane%20Austen%20-%20Emma.epub").as_str()));
    wait_event(&mut ev, 30, |e| matches!(e, EngineEvent::Finished { id } if *id == added.id)).await;
    let dest = lib.path().join("Books/Jane Austen - Emma.epub");
    wait_for("organized", 10, || async { dest.exists().then_some(()) }).await;
    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert!(!dl.path().join("Jane Austen - Emma.epub").exists(), "moved, not copied");
    let s = n.h.snapshot().await.unwrap();
    let row = s.rows.iter().find(|r| r.id == added.id).unwrap();
    assert_eq!(row.state, TorrentState::Completed);
    assert_eq!(row.done_bytes, 400_000);
    assert_eq!(row.category, Some(Category::Book));
    assert_eq!((s.counts.seeding, s.counts.cold, s.counts.warm, s.counts.hot), (0, 0, 0, 0), "never Sharing, no tiers");
    assert_eq!(s.up_rate, 0);
}

/// A finished direct download in a plain folder moves when its category changes, like a torrent's
/// files do (SPEC 7A.2/7A.4): into the new category's own folder, and back to the download folder
/// when the new category has none. The row follows, and nothing is copied or left behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_finished_direct_download_in_a_plain_folder_follows_a_category_change() {
    use trs_engine::config::CategoryLocation;
    let body = pattern_bytes(300_000, 7);
    let srv = serve_files(vec![ServedFile::new("/report.bin", body.clone())]).await;
    let (state, dl, books, soft) =
        (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (b, sf) = (books.path().to_path_buf(), soft.path().to_path_buf());
    let n = snode(state.path(), dl.path(), None, move |c| {
        // No detection: only the categories' plain folders.
        c.category_locations[Category::Book.index()] = CategoryLocation { incomplete: None, finished: Some(b) };
        c.category_locations[Category::Software.index()] = CategoryLocation { incomplete: None, finished: Some(sf) };
    })
    .await;
    let mut ev = n.h.subscribe();
    let added = n.h.add_download(&srv.url("/report.bin"), DownloadOptions::default()).await.unwrap();
    wait_event(&mut ev, 30, |e| matches!(e, EngineEvent::Finished { id } if *id == added.id)).await;
    let home = dl.path().join("report.bin");
    wait_for("in the download folder", 10, || async { home.exists().then_some(()) }).await;

    let moved_to = |dir: PathBuf| {
        let n = &n;
        async move {
            let target = dir.join("report.bin");
            wait_for("moved", 15, || async { target.exists().then_some(()) }).await;
            assert_eq!(std::fs::read(&target).unwrap(), body_of(&target));
            let row = wait_for("row follows", 10, || async {
                row_h(&n.h, &added.id).await.filter(|r| r.save_path == target)
            })
            .await;
            assert_eq!(row.state, TorrentState::Completed);
            target
        }
    };
    fn body_of(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    n.h.set_category(&[added.id], Some(Category::Book));
    let t = moved_to(books.path().to_path_buf()).await;
    assert!(!home.exists(), "moved, not copied");
    assert_eq!(row_h(&n.h, &added.id).await.unwrap().category, Some(Category::Book));
    assert_eq!(std::fs::read(&t).unwrap(), body);

    n.h.set_category(&[added.id], Some(Category::Software));
    moved_to(soft.path().to_path_buf()).await;
    assert!(!books.path().join("report.bin").exists());

    // A category without a folder of its own: back where downloads go.
    n.h.set_category(&[added.id], Some(Category::Movie));
    moved_to(dl.path().to_path_buf()).await;
    assert!(!soft.path().join("report.bin").exists());
    assert_eq!(std::fs::read(&home).unwrap(), body);
}

/// "Max active downloads" counts torrents and direct downloads together.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_downloads_share_the_queue_with_torrents() {
    let mut f = ServedFile::new("/slow.bin", pattern_bytes(600_000, 2));
    f.rate = Some(250_000);
    let srv = serve_files(vec![f]).await;
    let t = standard_torrent(300_000, None, false, 601);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let n = snode(state.path(), dl.path(), None, |c| c.max_active_downloads = 1).await;
    let d = n.h.add_download(&srv.url("/slow.bin"), DownloadOptions::default()).await.unwrap();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    // One slot: the direct download holds it, the torrent waits.
    let s = n.h.snapshot().await.unwrap();
    let st = |id: &TorrentId| s.rows.iter().find(|r| r.id == *id).unwrap().state;
    assert_eq!(st(&d.id), TorrentState::Downloading);
    assert_eq!(st(&t.id), TorrentState::Queued);
    assert_eq!(s.counts.queued, 1);
    wait_state(&n.h, &d.id, TorrentState::Completed, 30).await;
    // The slot is free: the torrent runs and finishes.
    wait_done_h(&n.h, &t.id, 60).await;
    t.verify(dl.path(), &[]);
    assert_eq!(std::fs::read(dl.path().join("slow.bin")).unwrap(), pattern_bytes(600_000, 2));
}

/// Pause, restart the whole engine, resume: only the missing bytes are fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_downloads_survive_a_restart_and_resume_with_a_range() {
    let body = pattern_bytes(1_000_000, 3);
    let mut f = ServedFile::new("/big.bin", body.clone());
    f.rate = Some(400_000);
    let srv = serve_files(vec![f]).await;
    let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = snode(state.path(), dl.path(), None, |_| {}).await;
    let d = a.h.add_download(&srv.url("/big.bin"), DownloadOptions::default()).await.unwrap();
    wait_for("30%", 20, || async { (a.row(&d.id).await?.done_bytes >= 300_000).then_some(()) }).await;
    a.h.pause(&[d.id]);
    let paused = wait_state(&a.h, &d.id, TorrentState::Paused, 10).await;
    assert!(paused.done_bytes >= 300_000 && paused.done_bytes < 1_000_000);
    a.shutdown(Duration::from_secs(3)).await;
    drop(a);

    let b = snode(state.path(), dl.path(), None, |_| {}).await;
    let row = wait_for("the download is back", 10, || async { b.row(&d.id).await }).await;
    assert_eq!((row.kind, row.state), (RowKind::Direct, TorrentState::Paused));
    assert!(row.done_bytes >= 300_000, "progress kept: {}", row.done_bytes);
    b.h.resume(&[d.id]);
    wait_state(&b.h, &d.id, TorrentState::Completed, 30).await;
    assert_eq!(std::fs::read(dl.path().join("big.bin")).unwrap(), body);
    let hits = srv.hits();
    let resumed = hits.iter().filter_map(|h| h.range.as_deref()).filter(|r| !r.starts_with("bytes=0-")).count();
    assert!(resumed >= 1, "the second run asked for a range, not the whole file: {hits:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_global_and_per_download_limits_throttle_direct_downloads() {
    let srv = serve_files(vec![ServedFile::new("/a.bin", pattern_bytes(600_000, 4)), ServedFile::new("/b.bin", pattern_bytes(300_000, 5))]).await;
    let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let n = snode(state.path(), dl.path(), None, |c| c.down_limit = Some(250_000)).await;
    let t0 = Instant::now();
    let a = n.h.add_download(&srv.url("/a.bin"), DownloadOptions::default()).await.unwrap();
    wait_state(&n.h, &a.id, TorrentState::Completed, 30).await;
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(1200), "600 kB at 250 kB/s cannot take {took:?}");
    assert!(took < Duration::from_secs(10), "{took:?}");

    n.h.set_limits(None, None);
    let t0 = Instant::now();
    let b = n.h.add_download(&srv.url("/b.bin"), DownloadOptions { rate_limit: Some(100_000), ..Default::default() }).await.unwrap();
    wait_state(&n.h, &b.id, TorrentState::Completed, 30).await;
    assert!(t0.elapsed() >= Duration::from_millis(1800), "300 kB at 100 kB/s: {:?}", t0.elapsed());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_that_serves_a_torrent_becomes_a_torrent() {
    let t = standard_torrent(100_000, None, false, 602);
    let mut f = ServedFile::new("/linux.torrent", t.bytes.clone());
    f.content_type = "application/x-bittorrent".into();
    let srv = serve_files(vec![f]).await;
    let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let n = snode(state.path(), dl.path(), None, |_| {}).await;
    let d = n.h.add_download(&srv.url("/linux.torrent"), DownloadOptions::default()).await.unwrap();
    let row = wait_for("a torrent row", 20, || async {
        let r = n.row(&t.id).await?;
        (r.kind == RowKind::Torrent).then_some(r)
    })
    .await;
    assert_eq!(row.name, t.name);
    wait_for("direct row gone", 10, || async { n.row(&d.id).await.is_none().then_some(()) }).await;
    assert!(!dl.path().join("linux.torrent").exists(), "the downloaded .torrent is not left behind");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_resume_remove_and_bad_links() {
    let mut f = ServedFile::new("/slow.bin", pattern_bytes(2_000_000, 6));
    f.rate = Some(300_000);
    let srv = serve_files(vec![f]).await;
    let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let n = snode(state.path(), dl.path(), None, |_| {}).await;
    for bad in ["ftp://example.org/x", "file:///etc/passwd", "http://user:pw@example.org/x", "not a url", ""] {
        assert!(matches!(n.h.add_download(bad, DownloadOptions::default()).await, Err(EngineError::Invalid(_))), "{bad}");
    }
    let d = n.h.add_download(&srv.url("/slow.bin"), DownloadOptions { paused: true, ..Default::default() }).await.unwrap();
    wait_state(&n.h, &d.id, TorrentState::Paused, 5).await;
    n.h.resume(&[d.id]);
    wait_for("progress", 20, || async { (n.row(&d.id).await?.done_bytes > 100_000).then_some(()) }).await;
    n.h.pause_all();
    wait_state(&n.h, &d.id, TorrentState::Paused, 10).await;
    n.h.resume_all();
    wait_state(&n.h, &d.id, TorrentState::Downloading, 10).await;
    // Removing deletes the partial file.
    n.h.remove(&[d.id], true).await.unwrap();
    assert!(n.row(&d.id).await.is_none());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let leftovers: Vec<PathBuf> = walk(dl.path());
    assert!(leftovers.iter().all(|p| !p.to_string_lossy().ends_with(".part")), "{leftovers:?}");
    n.h.flush().await.unwrap();
    assert_eq!(n.store.load_directs().unwrap().len(), 0, "the record is gone");
}

fn walk(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            if e.path().is_dir() {
                out.extend(walk(&e.path()));
            } else {
                out.push(e.path());
            }
        }
    }
    out
}
