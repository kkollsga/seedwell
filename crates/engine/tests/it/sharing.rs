//! The Sharing groups (SPEC 8.3): Popular / Active / Dormant / Error / Dead, last activity and
//! error start (persisted), tracker errors with their retry time.
use crate::common;
use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use trs_engine::persist::Placed;
use trs_engine::*;

fn now() -> u64 {
    trs_engine::util::unix_secs()
}

async fn group(h: &EngineHandle, id: &TorrentId) -> Option<SeedGroup> {
    row_h(h, id).await?.seed_group
}

/// A stored, finished torrent with the given activity and error start, as a previous run left it.
fn put_stored(s: &MemStore, t: &TestTorrent, dir: &std::path::Path, last_activity: u64, error_since: u64) {
    let rec = trs_engine::persist::TorrentRecord {
        id: t.id,
        name: t.name.clone(),
        has_metadata: true,
        magnet: None,
        announce: Some(vec![]),
        total_length: t.total(),
        wanted_length: t.total(),
        session_base: 0,
        piece_length: t.piece_len,
        private: false,
        save_path: dir.to_path_buf(),
        state: trs_engine::persist::StoredState::Active,
        file_priorities: vec![],
        priority: 1,
        priority_user: false,
        queue_pos: 0,
        category: None,
        category_user: false,
        tracker_profile: None,
        rule: None,
        added_at: now() - 5 * 86_400,
        finished_at: Some(now() - 4 * 86_400),
        seed_goal: SeedGoal::FOREVER,
        goal_user: false,
        added_complete: false,
        staged: false,
        complete: true,
        placed: Placed::No,
        last_activity,
        error_since,
        error_tracker: false,
        stage_path: None,
        pinned: false,
        awaiting_choice: false,
        pending_start: false,
        pending_stopped: false,
        title: None,
        idle: None,
    };
    s.put(
        rec,
        trs_engine::persist::LifetimeCounters { done_bytes: t.total(), ..Default::default() },
        Some(trs_engine::persist::ResumeRecord { complete: true, ..Default::default() }),
        Some(std::sync::Arc::from(t.bytes.as_slice())),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn activity_decides_active_or_dormant_and_survives_restarts() {
    let (a, b) = (standard_named("act", 0, None, false, 801), standard_named("dor", 0, None, false, 802));
    let dir = tempfile::tempdir().unwrap();
    a.write_to(dir.path());
    b.write_to(dir.path());
    let s = MemStore::new();
    put_stored(&s, &a, dir.path(), now() - 3600, 0);
    put_stored(&s, &b, dir.path(), now() - 2 * 86_400, 0);
    let n = node_in(tempfile::tempdir().unwrap(), s.clone(), |_| {}).await;
    assert_eq!(group(&n.h, &a.id).await, Some(SeedGroup::Active), "uploaded an hour ago");
    assert_eq!(group(&n.h, &b.id).await, Some(SeedGroup::Dormant), "nothing for two days");
    let r = row_h(&n.h, &b.id).await.unwrap();
    assert!(r.last_activity.is_some_and(|t| t < now() - 86_400) && r.error_since.is_none() && r.error.is_none());

    // A paused torrent is not in the Sharing view at all.
    n.h.pause(&[a.id]);
    wait_for("paused", 5, || async { (group(&n.h, &a.id).await.is_none()).then_some(()) }).await;
    n.h.resume(&[a.id]);
    wait_for("back", 5, || async { group(&n.h, &a.id).await.map(|_| ()) }).await;

    // Being asked for data makes a dormant torrent Active, and the activity is written to the store.
    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(b.bytes.clone(), AddOptions { peers: vec![n.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &b.id, 30).await;
    wait_for("dormant -> active", 20, || async { (group(&n.h, &b.id).await? == SeedGroup::Active).then_some(()) }).await;
    n.h.flush().await.unwrap();
    assert!(s.torrent(&b.id).unwrap().record.last_activity >= now() - 120, "persisted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_many_peers_download_from_is_popular() {
    let t = standard_torrent(3_000_000, None, false, 811);
    let seeder = node(|c| c.up_limit = Some(300_000)).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let mut leeches = Vec::new();
    for _ in 0..3 {
        let l = node(|_| {}).await;
        l.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
        leeches.push(l);
    }
    wait_for("popular", 30, || async { (group(&seeder.h, &t.id).await? == SeedGroup::Popular).then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_files_make_an_error_that_turns_dead_after_a_day() {
    let t = standard_torrent(0, None, false, 821);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    let mut a = node(|_| {}).await;
    t.write_to(a.dir.path());
    a.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&a, &t.id, 30).await;
    wait_for("fingerprints stored", 10, || async {
        a.h.flush().await.ok()?;
        let r = a.store.resume(&t.id)?;
        (r.complete && !r.fingerprints.is_empty()).then_some(())
    })
    .await;
    a.shutdown(Duration::from_secs(3)).await;
    std::fs::remove_file(t.file_path(a.dir.path(), 2)).unwrap();
    let store = a.store.clone();
    let b = node_in(a.dir, store.clone(), |_| {}).await;
    b.h.add_peers(t.id, vec![seeder.addr()]);
    let r = wait_for("missing", 20, || async {
        let r = row_h(&b.h, &t.id).await?;
        (r.state == TorrentState::MissingData).then_some(r)
    })
    .await;
    assert_eq!(r.seed_group, Some(SeedGroup::Error), "started just now");
    assert!(r.error.is_some() && r.error_since.is_some_and(|s| s + 60 > now()));
    // The error start is kept across restarts: a day later it is Dead.
    b.h.flush().await.unwrap();
    let mut st = store.torrent(&t.id).unwrap();
    assert!(st.record.error_since > 0, "persisted");
    st.record.error_since = now() - 30 * 3600;
    let resume = store.resume(&t.id);
    store.put(st.record, st.counters, resume, None);
    drop(b.h);
    let c = node_in(tempfile::tempdir().unwrap(), store, |_| {}).await;
    c.h.add_peers(t.id, vec![seeder.addr()]);
    wait_for("dead", 20, || async { (group(&c.h, &t.id).await? == SeedGroup::Dead).then_some(()) }).await;
}

/// A tracker that always answers with this `failure reason`.
async fn failing_tracker(body: &'static str) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(head.as_bytes()).await;
                let _ = s.write_all(body.as_bytes()).await;
            });
        }
    });
    addr
}

/// A tracker that answers 502 until `up` is set, then a normal announce answer.
async fn flaky_tracker(up: std::sync::Arc<std::sync::atomic::AtomicBool>) -> std::net::SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            let up = up.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let resp = if up.load(std::sync::atomic::Ordering::SeqCst) {
                    let body = "d8:completei1e10:incompletei0e8:intervali1800e5:peers0:e";
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
                } else {
                    "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()
                };
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_bad_gateway_is_calm_and_its_words_vanish_after_a_successful_retry() {
    let up = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let addr = flaky_tracker(up.clone()).await;
    let t = standard_torrent(0, Some(format!("http://{addr}/announce")), false, 861);
    let n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let r = wait_for("bad gateway", 30, || async {
        let r = row_h(&n.h, &t.id).await?;
        (r.error.as_deref() == Some("Bad gateway")).then_some(r)
    })
    .await;
    assert_eq!(r.tracker_detail.as_deref(), Some("The tracker's server is down or overloaded (it answered 502 Bad Gateway)."));
    assert!(r.error_transient, "temporary: no attention, no error count");
    assert!(r.tracker_failures >= 1 && r.next_retry_ms.is_some());
    // The tracker recovers; the user asks again (a seed would otherwise wait 15 minutes).
    up.store(true, std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(11)).await;
    let asked = n.h.reannounce(&[t.id]).await.unwrap();
    assert!(asked.iter().all(|a| a.sent), "{asked:?}");
    let r = wait_for("the problem is gone", 30, || async {
        let r = row_h(&n.h, &t.id).await?;
        (r.tracker_error.is_none() && r.error.is_none()).then_some(r)
    })
    .await;
    assert!(!r.error_transient && r.tracker_detail.is_none() && r.error_since.is_none(), "nothing tracker-related lingers: {r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unreachable_trackers_are_an_error_with_a_retry_time() {
    // Nothing listens on port 1: connection refused is a transport failure.
    let t = standard_torrent(0, Some("http://127.0.0.1:1/announce".into()), false, 831);
    let n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let r = wait_for("tracker error", 30, || async {
        let r = row_h(&n.h, &t.id).await?;
        (r.seed_group == Some(SeedGroup::Error)).then_some(r)
    })
    .await;
    assert_eq!(r.error.as_deref(), Some("Can't connect"));
    assert_eq!(r.tracker_detail.as_deref(), Some("The tracker can't be reached (the connection was refused or failed)."));
    assert!(r.error_transient, "a passing outage is calm");
    assert!(r.next_retry_ms.is_some_and(|ms| ms > now() * 1000), "a retry is scheduled: {:?}", r.next_retry_ms);
    assert!(r.error_since.is_some());
    // A user's ask is held by the ten second double-click floor at most, not by the backoff (a seed waits 15 minutes).
    let asked = n.h.reannounce(&[t.id]).await.unwrap();
    assert_eq!(asked.len(), 1);
    assert!(asked[0].waiting_after_failures.is_some(), "labelled as Seedwell's own wait: {asked:?}");
    assert!(asked[0].not_before_ms.is_some_and(|ms| ms <= now() * 1000 + 11_000), "{asked:?}");
    // Still seeding: the files are fine, so it keeps serving.
    assert!(matches!(r.state, TorrentState::LowPower | TorrentState::Seeding));
    // Pausing and resuming opens a new tracker session and clears the old verdict until it fails again.
    n.h.pause(&[t.id]);
    wait_for("not shared", 5, || async { group(&n.h, &t.id).await.is_none().then_some(()) }).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unregistered_torrent_is_dead_at_once_but_a_maintenance_message_is_not_an_error() {
    let dead = failing_tracker("d14:failure reason21:Unregistered torrent e").await;
    let t = standard_torrent(0, Some(format!("http://{dead}/announce")), false, 841);
    let n = node(|c| c.trackers_enabled = true).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let r = wait_for("dead", 30, || async {
        let r = row_h(&n.h, &t.id).await?;
        (r.seed_group == Some(SeedGroup::Dead)).then_some(r)
    })
    .await;
    assert!(r.error.as_deref().is_some_and(|e| e.to_lowercase().contains("unregistered")), "{:?}", r.error);

    let maint = failing_tracker(Box::leak(format!("d14:failure reason{}:{}e", "down for maintenance".len(), "down for maintenance").into_boxed_str())).await;
    let u = standard_named("maint", 0, Some(format!("http://{maint}/announce")), false, 842);
    u.write_to(n.dir.path());
    n.h.add_torrent_file(u.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("tracker answered", 20, || async {
        let d = n.h.torrent_detail(u.id).await.ok()?;
        d.trackers.iter().any(|t| t.status.starts_with("failed") || t.status.starts_with("warning") || t.status.starts_with("halted")).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let g = group(&n.h, &u.id).await;
    assert!(matches!(g, Some(SeedGroup::Active)), "a message from the tracker is not an error: {g:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_download_whose_trackers_halted_says_why_and_may_ask_again_only_once_in_a_while() {
    let dead = failing_tracker("d14:failure reason21:unregistered torrent e").await;
    let t = standard_torrent(0, Some(format!("http://{dead}/announce")), true, 851);
    let n = node(|c| c.trackers_enabled = true).await;
    // Nothing on disk and nobody to ask: a download that cannot start.
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let r = wait_for("halted", 30, || async {
        let r = row_h(&n.h, &t.id).await?;
        r.tracker_halted.then_some(r)
    })
    .await;
    assert_eq!(r.state, TorrentState::Downloading);
    assert_eq!(r.tracker_error.as_deref(), Some("Rejected: “Unregistered torrent”"));
    assert_eq!(r.waiting_note(), Some(RowNote { text: "Rejected: “Unregistered torrent”".into(), error: true }));
    assert_eq!(r.tracker_detail.as_deref(), Some("The tracker refused this torrent: “Unregistered torrent”."));
    assert!(!r.error_transient);

    // Asking again is allowed, but never twice in a row: the second answer is deferred.
    let _ = n.h.reannounce(&[t.id]).await.unwrap();
    let again = n.h.reannounce(&[t.id]).await.unwrap();
    assert!(!again.is_empty() && again.iter().all(|r| !r.sent && r.not_before_ms.is_some_and(|ms| ms > now() * 1000)), "{again:?}");
}
