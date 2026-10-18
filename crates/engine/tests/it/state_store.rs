//! The engine on the redb-backed store: the same scenarios as the in-memory tests, plus restart,
//! counters, statistics buckets, launch history and recovery.
use crate::common;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use trs_engine::persist::{Batch, LifetimeCounters, Placed, Placement, ResumeRecord, StatDelta, Store, StoredState, TorrentRecord};
use trs_engine::*;
use trs_state::{Day, StatsScope};

/// A file fingerprint as trs-storage encodes it: size, mtime seconds, mtime nanoseconds.
fn fp(size: u64, secs: i64, nanos: u32) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..8].copy_from_slice(&size.to_le_bytes());
    b[8..16].copy_from_slice(&secs.to_le_bytes());
    b[16..20].copy_from_slice(&nanos.to_le_bytes());
    b
}

fn rec(t: &TestTorrent, dir: &std::path::Path) -> TorrentRecord {
    TorrentRecord {
        id: t.id,
        name: t.name.clone(),
        has_metadata: true,
        magnet: None,
        announce: None,
        total_length: t.total(),
        wanted_length: t.total(),
        session_base: 0,
        piece_length: t.piece_len,
        private: false,
        save_path: dir.to_path_buf(),
        state: StoredState::Active,
        file_priorities: vec![],
        priority: 1,
        priority_user: false,
        queue_pos: 0,
        category: None,
        category_user: false,
        tracker_profile: None,
        rule: None,
        added_at: 1_700_000_000,
        finished_at: None,
        seed_goal: SeedGoal::FOREVER,
        goal_user: false,
        added_complete: false,
        staged: false,
        complete: false,
        placed: Placed::No,
        last_activity: 0,
        error_since: 0,
        error_tracker: false,
        stage_path: None,
        pinned: false,
        awaiting_choice: false,
        pending_start: false,
        title: None,
        idle: None,
    }
}

async fn seeded(state: &std::path::Path, dl: &std::path::Path, t: &TestTorrent) -> SNode {
    let n = snode(state, dl, None, |_| {}).await;
    t.write_to(dl);
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done_h(&n.h, &t.id, 30).await;
    n
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_download_through_redb() {
    let t = standard_torrent(0, None, false, 211);
    let (sd, dd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let (ls, ld) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let seeder = seeded(sd.path(), dd.path(), &t).await;
    let leech = snode(ls.path(), ld.path(), None, |_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done_h(&leech.h, &t.id, 60).await;
    t.verify(ld.path(), &[]);
    let row = leech.row(&t.id).await.unwrap();
    assert!(row.downloaded_total >= t.total());
    leech.h.flush().await.unwrap();
    // The original .torrent is in torrents/, the record and counters in the database.
    let hex = hex_of(&t.id);
    assert!(ls.path().join("torrents").join(format!("{hex}.torrent")).exists());
    let loaded = leech.store.load_torrents().unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].counters.downloaded >= t.total());
}

/// Restart mid-download: verified pieces are kept, nothing is rechecked or re-downloaded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restart_resumes_without_redownloading_on_redb() {
    let t = standard_torrent(8_000_000, None, false, 221);
    let (sd, dd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let seeder = {
        let n = snode(sd.path(), dd.path(), None, |c| c.up_limit = Some(2_000_000)).await;
        t.write_to(dd.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done_h(&n.h, &t.id, 30).await;
        n
    };
    let (ls, ld) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = snode(ls.path(), ld.path(), None, |c| c.resume_interval = Duration::from_millis(500)).await;
    a.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("30% done", 30, || async { (a.row(&t.id).await?.done_bytes * 100 / t.total() >= 30).then_some(()) }).await;
    a.shutdown(Duration::from_secs(3)).await;
    drop(a);

    // A fresh process: the database is opened again from disk.
    let b = snode(ls.path(), ld.path(), None, |_| {}).await;
    b.h.add_peers(t.id, vec![seeder.addr()]);
    let first = wait_for("row after restart", 10, || async { b.row(&t.id).await }).await;
    assert!(first.done_bytes > 0 || first.state == TorrentState::Checking, "{first:?}");
    wait_done_h(&b.h, &t.id, 60).await;
    t.verify(ld.path(), &[]);
    assert_eq!(b.h.diagnostics().rechecks, 0, "a clean restart must not recheck");
    let row = b.row(&t.id).await.unwrap();
    assert!(row.downloaded_total <= t.total() + 3_000_000, "downloaded {} for {}", row.downloaded_total, t.total());
}

/// A complete torrent comes back Cold: no recheck, counters and ratio intact, and it serves a peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn complete_torrent_restarts_cold_with_counters() {
    let t = standard_torrent(0, None, false, 231);
    let (sd, dd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = seeded(sd.path(), dd.path(), &t).await;
    // Let someone download so the upload counter is not zero.
    let (ls, ld) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let leech = snode(ls.path(), ld.path(), None, |_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![a.addr()], ..Default::default() }).await.unwrap();
    wait_done_h(&leech.h, &t.id, 60).await;
    let up = wait_for("upload counted", 10, || async {
        let r = a.row(&t.id).await?;
        (r.uploaded_total >= t.total()).then_some(r.uploaded_total)
    })
    .await;
    wait_for("fingerprints stored", 10, || async {
        a.h.flush().await.ok()?;
        let r = a.store.load_resume(&t.id).ok()??;
        (r.complete && !r.fingerprints.is_empty()).then_some(())
    })
    .await;
    a.shutdown(Duration::from_secs(3)).await;
    drop(a);

    let b = snode(sd.path(), dd.path(), None, |_| {}).await;
    let row = wait_for("row", 10, || async { b.row(&t.id).await }).await;
    assert_eq!(row.tier, Tier::Cold);
    assert_eq!(row.done_bytes, t.total());
    assert!(row.uploaded_total >= up, "lifetime counters survive: {} >= {up}", row.uploaded_total);
    assert!(row.finished_at.is_some());
    // A second leecher is served by the woken torrent.
    let (l2s, l2d) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let l2 = snode(l2s.path(), l2d.path(), None, |_| {}).await;
    l2.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![b.addr()], ..Default::default() }).await.unwrap();
    wait_done_h(&l2.h, &t.id, 60).await;
    t.verify(l2d.path(), &[]);
    assert_eq!(b.h.diagnostics().rechecks, 0, "file stamps matched, so no recheck");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_files_are_reported_on_redb() {
    let t = standard_torrent(0, None, false, 241);
    let (sd, dd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = seeded(sd.path(), dd.path(), &t).await;
    wait_for("fingerprints stored", 10, || async {
        a.h.flush().await.ok()?;
        let r = a.store.load_resume(&t.id).ok()??;
        (r.complete && !r.fingerprints.is_empty()).then_some(())
    })
    .await;
    a.shutdown(Duration::from_secs(3)).await;
    drop(a);
    let victim = t.file_path(dd.path(), 2);
    std::fs::remove_file(&victim).unwrap();
    let b = snode(sd.path(), dd.path(), None, |_| {}).await;
    b.h.add_peers(t.id, vec!["127.0.0.1:9".parse().unwrap()]);
    wait_for("missing data", 20, || async { (b.row(&t.id).await?.state == TorrentState::MissingData).then_some(()) }).await;
    assert!(!victim.exists(), "never silently re-created");
}

/// What the engine wrote is what comes back, field by field.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn records_round_trip_through_the_adapter() {
    let t = standard_torrent(0, Some("http://tracker.example/announce".into()), true, 251);
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    let mut r = rec(&t, dl.path());
    r.private = true;
    r.state = StoredState::Paused;
    r.priority = 0;
    r.priority_user = true;
    r.queue_pos = 7;
    r.category = Some(Category::Tv);
    r.category_user = true;
    r.tracker_profile = Some("tl".into());
    r.rule = Some("tz".into());
    r.finished_at = Some(1_700_000_100);
    r.seed_goal = SeedGoal { ratio: Some(2.0), seed_secs: Some(3600), min_seed_secs: 0, any: false, at_goal: AtGoal::Stop, stop_on_finish: false, idle_mb: 0, idle_hours: 0 };
    r.goal_user = true;
    r.added_complete = true;
    r.staged = true;
    r.complete = true;
    r.placed = Placed::Renamed;
    r.file_priorities = vec![2, 0, 2, 0];
    store
        .commit(Batch {
            torrent_files: vec![(t.id, Arc::from(t.bytes.clone()))],
            torrents: vec![r.clone()],
            counters: vec![(t.id, LifetimeCounters { uploaded: 77, downloaded: 88, seeded_secs: 99, done_bytes: 0, wanted_done: None })],
            ..Batch::default()
        })
        .unwrap();
    drop(store);
    let (store, rep) = StateStore::open(state.path()).unwrap();
    assert!(!rep.created && rep.recovered.is_none());
    let got = store.load_torrents().unwrap();
    assert_eq!(got.len(), 1);
    let g = &got[0].record;
    assert_eq!(g.name, r.name);
    assert_eq!(g.state, StoredState::Paused);
    assert!(g.private && g.priority_user && g.category_user && g.goal_user && g.added_complete && g.staged && g.complete);
    assert_eq!(g.placed, Placed::Renamed);
    assert_eq!((g.priority, g.queue_pos, g.category), (0, 7, Some(Category::Tv)));
    assert_eq!((g.tracker_profile.as_deref(), g.rule.as_deref()), (Some("tl"), Some("tz")));
    assert_eq!(g.finished_at, Some(1_700_000_100));
    assert_eq!(g.file_priorities, vec![2, 0, 2, 0]);
    assert_eq!(g.save_path, dl.path());
    assert_eq!(g.total_length, t.total());
    assert_eq!(g.piece_length, t.piece_len);
    assert_eq!(g.announce, Some(vec![vec!["http://tracker.example/announce".to_owned()]]), "tiers come from the stored .torrent");
    assert_eq!(g.seed_goal.ratio, Some(2.0));
    assert_eq!(g.seed_goal.seed_secs, Some(3600));
    assert!(!g.seed_goal.any);
    assert_eq!((got[0].counters.uploaded, got[0].counters.downloaded, got[0].counters.seeded_secs), (77, 88, 99));
    assert_eq!(got[0].counters.done_bytes, t.total(), "complete");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn magnet_record_keeps_its_uri_until_metadata_arrives() {
    let t = standard_torrent(0, None, false, 252);
    let state = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    let uri = format!("magnet:?xt=urn:btih:{}&dn=x&tr=http%3A%2F%2Ft.example%2Fa", hex_of(&t.id));
    let mut r = rec(&t, std::path::Path::new("/tmp"));
    r.has_metadata = false;
    r.magnet = Some(uri.clone());
    r.total_length = 0;
    r.piece_length = 0;
    store.commit(Batch { torrents: vec![r], ..Batch::default() }).unwrap();
    let got = store.load_torrents().unwrap();
    assert_eq!(got[0].record.magnet.as_deref(), Some(uri.as_str()));
    assert!(!got[0].record.has_metadata);
    assert_eq!(got[0].record.announce, Some(vec![vec!["http://t.example/a".to_owned()]]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_data_and_placements_round_trip() {
    let t = standard_torrent(0, None, false, 253);
    let state = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    store
        .commit(Batch {
            torrents: vec![rec(&t, std::path::Path::new("/tmp"))],
            resume: vec![(
                t.id,
                Some(ResumeRecord {
                    complete: false,
                    bitfield: vec![0b1010_0000; (t.pieces as usize).div_ceil(8)].into_iter().enumerate().map(|(i, b)| if i == 0 { b } else { 0 }).collect(),
                    fingerprints: vec![Some(fp(100, 1_700_000_000, 123)), None, Some(fp(0, 1_700_000_001, 999_999_999))],
                    saved_at: 1234,
                }),
            )],
            placements: vec![(t.id, Some(Placement { files: vec![Some("/lib/a/b.mkv".into()), None], library_root: "/lib".into() }))],
            ..Batch::default()
        })
        .unwrap();
    let r = store.load_resume(&t.id).unwrap().unwrap();
    assert!(!r.complete);
    assert_eq!(r.bitfield[0], 0b1010_0000);
    assert_eq!(r.fingerprints, vec![Some(fp(100, 1_700_000_000, 123)), None, Some(fp(0, 1_700_000_001, 999_999_999))]);
    assert_eq!(r.saved_at, 1234);
    let p = store.load_placement(&t.id).unwrap().unwrap();
    assert_eq!(p.library_root, std::path::PathBuf::from("/lib"));
    assert_eq!(p.files[0].as_deref(), Some(std::path::Path::new("/lib/a/b.mkv")));
    assert_eq!(p.files[1], None);
    // Complete resume data: no bitfield, stamps only.
    store
        .commit(Batch {
            resume: vec![(t.id, Some(ResumeRecord { complete: true, bitfield: vec![], fingerprints: vec![Some(fp(5, 1_700_000_002, 0))], saved_at: 5 }))],
            ..Batch::default()
        })
        .unwrap();
    let r = store.load_resume(&t.id).unwrap().unwrap();
    assert!(r.complete && r.bitfield.is_empty());
    assert_eq!(r.fingerprints, vec![Some(fp(5, 1_700_000_002, 0))]);
    // Peers and the DHT table.
    let peers: Vec<std::net::SocketAddr> = vec!["1.2.3.4:6881".parse().unwrap(), "[2001:db8::1]:51413".parse().unwrap()];
    store.commit(Batch { peers: vec![(t.id, peers.clone())], dht_state: Some(vec![1, 2, 3]), ..Batch::default() }).unwrap();
    assert_eq!(store.load_peers(&t.id).unwrap(), peers);
    assert_eq!(store.load_dht_state().unwrap(), Some(vec![1, 2, 3]));
    // Removing the torrent removes everything attached.
    store.commit(Batch { removed: vec![t.id], ..Batch::default() }).unwrap();
    assert!(store.load_resume(&t.id).unwrap().is_none());
    assert!(store.load_placement(&t.id).unwrap().is_none());
    assert!(store.load_torrents().unwrap().is_empty());
}

/// Statistics land in the tracker domain's own bucket, `@public` or `@other-private`, on the local day.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stat_deltas_are_bucketed_by_profile_and_local_day() {
    let state = tempfile::tempdir().unwrap();
    let opened = trs_state::Store::open(state.path()).unwrap();
    // A zone 2 hours ahead of UTC: 23:30 UTC is already the next local day.
    let store = StateStore::with_time_zone(opened.store, |_| 7200);
    let at = 1_791_000_000 - (1_791_000_000 % 86_400) + 23 * 3600 + 1800; // 23:30 UTC
    let d = |domain: Option<&str>, private: bool| StatDelta {
        domain: domain.map(str::to_owned),
        private,
        at,
        downloaded: 100,
        uploaded: 50,
        torrents_added: 1,
        torrents_completed: 1,
        files_completed: 3,
        files_bytes: 90,
        seed_secs: 10,
        ..StatDelta::default()
    };
    store
        .commit(Batch { stat_deltas: vec![d(Some("example.org"), true), d(None, true), d(None, false), d(Some("example.org"), true)], ..Batch::default() })
        .unwrap();
    let s = store.state();
    let tl = s.stats_total(&StatsScope::domain("example.org")).unwrap();
    assert_eq!((tl.downloaded, tl.uploaded, tl.torrents_added, tl.torrents_completed, tl.files_completed, tl.files_bytes, tl.seed_secs), (200, 100, 2, 2, 6, 180, 20));
    assert_eq!(s.stats_total(&StatsScope::OtherPrivate).unwrap().downloaded, 100);
    assert_eq!(s.stats_total(&StatsScope::Public).unwrap().downloaded, 100);
    assert_eq!(s.stats_total(&StatsScope::All).unwrap().downloaded, 400);
    let local_day = Day::from_unix(at as i64, 7200);
    let series = s.stats_series(&StatsScope::domain("example.org"), local_day, 2).unwrap();
    assert_eq!(series.last().unwrap().1.downloaded, 200, "bucketed on the local day");
    assert_eq!(series[0].1.downloaded, 0, "and not on the UTC day before");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn launch_history_feeds_the_crash_loop_guard() {
    let state = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    let now = 1_791_000_000_000u64;
    assert_eq!(store.record_launch(now).unwrap(), vec![now]);
    assert_eq!(store.record_launch(now + 60_000).unwrap(), vec![now, now + 60_000]);
    assert_eq!(store.record_launch(now + 120_000).unwrap().len(), 3);
    // Old launches fall out of the history.
    assert_eq!(store.record_launch(now + 10_000_000).unwrap(), vec![now + 10_000_000]);
    // The history survives a reopen.
    drop(store);
    let (store, _) = StateStore::open(state.path()).unwrap();
    assert_eq!(store.record_launch(now + 10_060_000).unwrap().len(), 2);
}

/// Only launches that followed an unclean exit feed the breaker: relaunching the app does not hold
/// announces, a crash loop still does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graceful_relaunches_do_not_trip_the_crash_loop_guard() {
    use trs_tracker::{Millis, crash::CrashLoopGuard};
    let state = tempfile::tempdir().unwrap();
    let guard = CrashLoopGuard::default();
    let now = 1_791_000_000_000u64;
    let hold = |launches: Vec<u64>, at: u64| guard.evaluate(&launches.into_iter().map(Millis).collect::<Vec<_>>(), Millis(at));
    // Five quit-and-relaunch cycles, each reopening the same state directory.
    for i in 0..5u64 {
        let (store, _) = StateStore::open(state.path()).unwrap();
        let at = now + i * 20_000;
        assert_eq!(hold(store.record_launch(at).unwrap(), at), None, "launch {i}");
        store.record_clean_exit().unwrap();
    }
    // Crashes (no marker) still count, three in a row trip it.
    let (store, _) = StateStore::open(state.path()).unwrap();
    for i in 0..3u64 {
        let at = now + 200_000 + i * 20_000;
        let h = hold(store.record_launch(at).unwrap(), at);
        assert_eq!(h.is_some(), i == 2, "crash launch {i}: {h:?}");
    }
}

/// The tracker's `completed_sent` is durable and shared with the engine's timing store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tracker_timing_goes_through_the_durable_handle() {
    use trs_tracker::store::{TimingKey, TimingRecord};
    let state = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    let key = TimingKey { info_hash: [3; 20], tracker_url: "http://x.example/a".into() };
    let mut h = store.timing_store();
    h.save(&key, &TimingRecord { completed_sent: true, ..Default::default() }).unwrap();
    drop(h);
    drop(store);
    let (store, _) = StateStore::open(state.path()).unwrap();
    assert!(store.timing_store().load(&key).unwrap().completed_sent);
}

/// A damaged database is set aside and the torrent list is rebuilt from `torrents/`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn damaged_database_is_rebuilt_from_torrent_files() {
    let t = standard_torrent(0, None, false, 261);
    let (sd, dd) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let mut a = seeded(sd.path(), dd.path(), &t).await;
    a.h.flush().await.unwrap();
    a.shutdown(Duration::from_secs(3)).await;
    drop(a);
    std::fs::write(sd.path().join("state.redb"), b"this is not a database at all").unwrap();
    let (store, report) = StateStore::open(sd.path()).unwrap();
    assert!(report.recovered.is_some(), "set aside: {report:?}");
    assert_eq!(store.rebuild_from_torrent_files(dd.path()).unwrap(), 1);
    drop(store);
    // The engine finds the data by itself (a recheck) and seeds again.
    let b = snode(sd.path(), dd.path(), None, |_| {}).await;
    wait_done_h(&b.h, &t.id, 30).await;
    assert_eq!(b.row(&t.id).await.unwrap().done_bytes, t.total());
}

/// The queue order survives a restart (it lives in `torrent-extra.bin`, not in the redb record).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queue_order_survives_a_restart() {
    let (a, b, c) = (standard_named("qa", 0, None, false, 271), standard_named("qb", 0, None, false, 272), standard_named("qc", 0, None, false, 273));
    let state = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    let mk = |t: &TestTorrent, pos: u32| {
        let mut r = rec(t, std::path::Path::new("/tmp"));
        r.queue_pos = pos;
        r
    };
    store.commit(Batch { torrents: vec![mk(&a, 5), mk(&b, 2), mk(&c, 9)], ..Batch::default() }).unwrap();
    // `b` moves to the front, `c` is removed.
    store.commit(Batch { torrents: vec![mk(&b, 0)], removed: vec![c.id], ..Batch::default() }).unwrap();
    drop(store);
    let (store, _) = StateStore::open(state.path()).unwrap();
    let mut got: Vec<(u32, String)> = store.load_torrents().unwrap().into_iter().map(|t| (t.record.queue_pos, t.record.name)).collect();
    got.sort();
    assert_eq!(got, vec![(0, "qb".to_owned()), (5, "qa".to_owned())]);
}

/// Removed and added again before the debounced write: one batch carries both, and the torrent
/// must survive with its .torrent file (QA saw "the stored .torrent file is missing" after a quick
/// remove and re-add).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_then_readd_in_one_batch_keeps_the_torrent() {
    let t = standard_torrent(0, None, false, 254);
    let state = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    let file: std::sync::Arc<[u8]> = std::sync::Arc::from(&t.bytes[..]);
    store
        .commit(Batch { torrents: vec![rec(&t, std::path::Path::new("/tmp"))], torrent_files: vec![(t.id, file.clone())], ..Batch::default() })
        .unwrap();
    store
        .commit(Batch {
            removed: vec![t.id],
            torrents: vec![rec(&t, std::path::Path::new("/tmp"))],
            torrent_files: vec![(t.id, file)],
            ..Batch::default()
        })
        .unwrap();
    assert_eq!(store.load_torrents().unwrap().len(), 1, "the re-added torrent is stored");
    assert!(store.load_torrent_file(&t.id).unwrap().is_some(), "with its .torrent file");
}

/// The same through the engine: remove, re-add at once, restart; the torrent is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quick_remove_and_readd_survives_a_restart() {
    let t = standard_torrent(0, None, false, 255);
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let mut n = snode(state.path(), dl.path(), None, |_| {}).await;
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await; // the add is written
    n.h.remove(&[t.id], false).await.unwrap();
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    n.shutdown(Duration::from_secs(3)).await;
    drop(n);
    let n = snode(state.path(), dl.path(), None, |_| {}).await;
    let row = n.row(&t.id).await.expect("the torrent is back after the restart");
    assert!(row.error.is_none(), "no error: {:?}", row.error);
}

/// The inactive condition's upload history is a side file (SPEC §7.5): it comes back with the
/// record after a restart, and goes when the condition is switched off or the torrent removed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_inactive_condition_history_survives_a_restart() {
    use trs_engine::idle::IdleRing;
    let t = standard_torrent(0, None, false, 262);
    let state = tempfile::tempdir().unwrap();
    let dl = tempfile::tempdir().unwrap();
    let (store, _) = StateStore::open(state.path()).unwrap();
    let mut ring = IdleRing::new(150 * 3600, 0, 0);
    ring.observe(3600, 700_000_000);
    ring.observe(7200, 700_000_000);
    ring.stopped = true;
    let mut r = rec(&t, dl.path());
    r.idle = Some(ring.clone());
    store
        .commit(Batch { torrent_files: vec![(t.id, Arc::from(t.bytes.clone()))], torrents: vec![r.clone()], ..Batch::default() })
        .unwrap();
    drop(store);
    let (store, _) = StateStore::open(state.path()).unwrap();
    let got = store.load_torrents().unwrap();
    let back = got[0].record.idle.clone().expect("ring restored");
    assert_eq!((back.uploaded(), back.arms_in(), back.stopped), (ring.uploaded(), ring.arms_in(), true));
    assert!(back.uploaded() >= 690_000_000);
    r.idle = None;
    store.commit(Batch { torrents: vec![r], ..Batch::default() }).unwrap();
    assert!(store.load_torrents().unwrap()[0].record.idle.is_none(), "switched off: the history is dropped");
}
