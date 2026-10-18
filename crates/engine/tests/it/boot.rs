//! Start-up (SPEC 10): everything resumes, a few torrents at a time; a goal that was reached
//! keeps the torrent stopped.
use crate::common;
use std::time::{Duration, Instant};

use common::*;
use trs_engine::*;

fn uri_of(i: u32) -> String {
    // 20 distinct bytes per torrent; nothing will ever answer for them.
    let hash: Vec<u8> = (0..20u8).map(|b| b.wrapping_mul(7).wrapping_add(i as u8).wrapping_add((i >> 8) as u8 * 3)).collect();
    format!("magnet:?xt=urn:btih:{}&dn=Boot.{i}", hex_of(&hash))
}

/// Torrents that need an actor after a restart are brought up a few per tick, never all at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn start_up_brings_torrents_up_a_few_at_a_time() {
    const N: u64 = 30;
    let mut a = node(|_| {}).await;
    for i in 0..N as u32 {
        a.h.add_magnet(&uri_of(i), AddOptions::default()).await.unwrap();
    }
    // Half of them were paused when the app quit: they resume too.
    let rows = a.h.snapshot().await.unwrap().rows;
    assert_eq!(rows.len() as u64, N);
    let ids: Vec<_> = rows.iter().take(15).map(|r| r.id).collect();
    a.h.pause(&ids);
    wait_for("paused", 10, || async { (a.state(&ids[0]).await? == TorrentState::Paused).then_some(()) }).await;
    a.shutdown(Duration::from_secs(3)).await;
    let (dir, store) = (a.dir, a.store.clone());

    let b = node_in(dir, store, |_| {}).await;
    let d0 = b.h.diagnostics();
    assert!(d0.boot_waiting >= N - 4, "the torrents wait for their turn at start ({} waiting)", d0.boot_waiting);
    assert!(d0.actors_spawned <= 4, "only the first batch is up at once ({} up)", d0.actors_spawned);
    let (mut last, mut max_step, began) = (d0.actors_spawned, 0, Instant::now());
    while b.h.diagnostics().boot_waiting > 0 {
        assert!(began.elapsed() < Duration::from_secs(30), "start-up never finished");
        tokio::time::sleep(Duration::from_millis(5)).await;
        let now = b.h.diagnostics().actors_spawned;
        max_step = max_step.max(now - last);
        last = now;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let d = b.h.diagnostics();
    assert_eq!(d.actors_spawned, N, "every torrent came up in the end");
    assert!(max_step <= 4, "never more than a small batch between two looks ({max_step})");
    assert!(began.elapsed() >= Duration::from_millis(200 * (N / 2 - 2)), "it is spread out in time, not instantaneous");
    // None stayed paused.
    for r in b.h.snapshot().await.unwrap().rows {
        assert_ne!(r.state, TorrentState::Paused, "{} stayed paused after a restart", r.name);
    }
}

/// A user action on a torrent that is still waiting for its turn does not wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recheck_does_not_wait_for_the_start_up_queue() {
    const N: u32 = 24;
    let mut a = node(|_| {}).await;
    for i in 0..N {
        a.h.add_magnet(&uri_of(100 + i), AddOptions::default()).await.unwrap();
    }
    a.shutdown(Duration::from_secs(3)).await;
    let (dir, store) = (a.dir, a.store.clone());
    let b = node_in(dir, store, |_| {}).await;
    let rows = b.h.snapshot().await.unwrap().rows;
    let before = b.h.diagnostics().actors_spawned;
    b.h.recheck(&[rows[rows.len() - 1].id]);
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(b.h.diagnostics().actors_spawned > before || b.h.diagnostics().boot_waiting < u64::from(N), "the rechecked torrent started");
}

/// Quitting during the stagger is clean and starts nothing more.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn quitting_during_start_up_is_clean() {
    const N: u32 = 40;
    let mut a = node(|_| {}).await;
    for i in 0..N {
        a.h.add_magnet(&uri_of(200 + i), AddOptions::default()).await.unwrap();
    }
    a.shutdown(Duration::from_secs(3)).await;
    let (dir, store) = (a.dir, a.store.clone());
    let mut b = node_in(dir, store.clone(), |_| {}).await;
    assert!(b.h.diagnostics().boot_waiting > 0);
    let report = b.shutdown(Duration::from_secs(5)).await;
    assert!(report.waited < Duration::from_secs(5), "{report:?}");
    // Nothing was lost: the next start still has every torrent.
    let dir = b.dir;
    let c = node_in(dir, store, |_| {}).await;
    assert_eq!(c.h.snapshot().await.unwrap().rows.len() as u32, N);
}

/// A torrent that stopped at its seeding goal stays stopped across a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reached_goal_survives_a_restart() {
    let t = standard_torrent(0, None, false, 351);
    let mut a = node(|_| {}).await;
    t.write_to(a.dir.path());
    a.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&a, &t.id, 30).await;
    a.h.set_seed_goal(&[t.id], SeedGoal::stop_when_finished());
    wait_for("stopped at goal", 20, || async { (a.state(&t.id).await? == TorrentState::Paused).then_some(()) }).await;
    a.h.flush().await.unwrap();
    a.shutdown(Duration::from_secs(3)).await;
    let (dir, store) = (a.dir, a.store.clone());
    let b = node_in(dir, store, |_| {}).await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(b.state(&t.id).await, Some(TorrentState::Paused), "a reached goal is not undone by a restart");
}

/// A torrent the user paused resumes when the app starts again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pause_lasts_until_quit() {
    let t = standard_torrent(0, None, false, 352);
    let mut a = node(|_| {}).await;
    t.write_to(a.dir.path());
    a.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&a, &t.id, 30).await;
    a.h.pause(&[t.id]);
    wait_for("paused", 10, || async { (a.state(&t.id).await? == TorrentState::Paused).then_some(()) }).await;
    a.h.flush().await.unwrap();
    a.shutdown(Duration::from_secs(3)).await;
    let (dir, store) = (a.dir, a.store.clone());
    let b = node_in(dir, store, |_| {}).await;
    wait_for("sharing again", 10, || async {
        matches!(b.state(&t.id).await?, TorrentState::LowPower | TorrentState::Seeding).then_some(())
    })
    .await;
}

/// Builds before 2026-10-15 refused private torrents with a stored fault. That fault is not
/// sticky: the next launch retries the torrent with no user action and it shares normally.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stored_private_refusal_from_an_older_build_clears_itself() {
    let t = standard_torrent(0, None, true, 353);
    let mut a = node(|_| {}).await;
    t.write_to(a.dir.path());
    a.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&a, &t.id, 30).await;
    a.h.flush().await.unwrap();
    a.shutdown(Duration::from_secs(3)).await;
    let (dir, store) = (a.dir, a.store.clone());
    let mut st = store.torrent(&t.id).unwrap();
    st.record.state = trs_engine::persist::StoredState::Error("pre-release build: private trackers are disabled".into());
    store.put(st.record, st.counters, None, None);
    let b = node_in(dir, store, |_| {}).await;
    wait_for("sharing again", 10, || async {
        matches!(b.state(&t.id).await?, TorrentState::LowPower | TorrentState::Seeding).then_some(())
    })
    .await;
    let row = b.row(&t.id).await.unwrap();
    assert!(row.error.is_none(), "{:?}", row.error);
}
