//! Memory of Cold torrents when the store is `StateStore` (redb): the same marginal measurement as
//! `scale.rs`, so the cost of the adapter and of redb's capped page cache shows up.
mod common;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use common::*;
use trs_engine::persist::*;
use trs_engine::*;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards every call to `System` unchanged and only counts bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

fn live() -> usize {
    LIVE.load(Relaxed)
}

fn tiny(i: usize) -> TestTorrent {
    make_torrent(&TorrentSpec {
        name: &format!("t{i:04}"),
        piece_len: 16384,
        files: &[("f.bin", 3000 + i)],
        private: false,
        announce: None,
        seed: 1000 + i as u64,
    })
}

fn populate(state: &std::path::Path, n: usize) {
    let (store, _) = StateStore::open(state).unwrap();
    for chunk in (0..n).collect::<Vec<_>>().chunks(100) {
        let mut b = Batch::default();
        for &i in chunk {
            let t = tiny(i);
            b.torrent_files.push((t.id, Arc::from(t.bytes.as_slice())));
            b.torrents.push(TorrentRecord {
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
                save_path: "/nonexistent/trs-scale".into(),
                state: StoredState::Active,
                file_priorities: vec![],
                priority: 1,
                priority_user: false,
                queue_pos: i as u32,
                category: None,
                category_user: false,
                tracker_profile: None,
                rule: None,
                added_at: 1_700_000_000 + i as u64,
                finished_at: Some(1_700_000_100),
                seed_goal: SeedGoal::FOREVER,
                goal_user: false,
                added_complete: false,
                staged: false,
                complete: true,
                placed: Placed::No,
                last_activity: 0,
                error_since: 0,
                error_tracker: false,
                stage_path: None,
                pinned: false,
                awaiting_choice: false,
                pending_start: false,
                pending_stopped: false,
                title: None,
                idle: None,
            });
            b.resume.push((t.id, Some(ResumeRecord { complete: true, bitfield: vec![], fingerprints: vec![None], saved_at: 1 })));
        }
        store.commit(b).unwrap();
    }
}

async fn cold_cost(n: usize) -> usize {
    let state = tempfile::tempdir().unwrap();
    populate(state.path(), n);
    let before = live();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.max_active_downloads = 1000;
    let (store, _) = StateStore::open(state.path()).unwrap();
    let e = Engine::start(cfg, store.clone()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let snap = e.handle().snapshot().await.unwrap();
    assert_eq!(snap.counts.total as usize, n);
    assert_eq!(snap.counts.cold as usize, n, "all Cold, no actor");
    drop(snap);
    let after = live();
    e.shutdown(Duration::from_secs(1)).await;
    drop(store);
    after.saturating_sub(before)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_torrents_on_redb_stay_small() {
    let small = cold_cost(112).await;
    let big = cold_cost(896).await;
    let per = (big as f64 - small as f64) / 784.0;
    eprintln!("[scale-redb] Cold torrent on StateStore: {per:.0} B each (112: {small} B, 896: {big} B)");
    assert!(per < 1024.0, "{per}");
}
