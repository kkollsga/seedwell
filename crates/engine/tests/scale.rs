//! Memory per tier and idle wake-ups with hundreds of torrents (SPEC 2, ENGINE.md 3, 9).
//!
//! A counting global allocator measures live heap bytes. Per-torrent cost is the *marginal* cost
//! between two engine sizes, so thread stacks, the runtime and the disk pools cancel out.
mod common;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use common::*;
use trs_engine::persist::*;
use trs_engine::*;

struct Counting;
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// Live allocation count per size class (power of two, 1 B .. 8 MiB), to see what an idle peer
/// is made of when `[scale]` numbers move.
static CLASSES: [AtomicIsize; 24] = [const { AtomicIsize::new(0) }; 24];

fn class(size: usize) -> usize {
    (usize::BITS - size.max(1).saturating_sub(1).leading_zeros()).min(23) as usize
}

// SAFETY: forwards every call to `System` unchanged and only counts bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            LIVE.fetch_add(l.size(), Relaxed);
            CLASSES[class(l.size())].fetch_add(1, Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        LIVE.fetch_sub(l.size(), Relaxed);
        CLASSES[class(l.size())].fetch_sub(1, Relaxed);
    }
}

#[global_allocator]
static A: Counting = Counting;

fn live() -> usize {
    LIVE.load(Relaxed)
}

fn record(t: &TestTorrent, dir: &std::path::Path, announce: Option<String>) -> TorrentRecord {
    TorrentRecord {
        id: t.id,
        name: t.name.clone(),
        has_metadata: true,
        magnet: None,
        announce: Some(announce.into_iter().map(|a| vec![a]).collect()),
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
        finished_at: Some(1_700_000_100),
        seed_goal: SeedGoal::FOREVER,
        goal_user: false,
        added_complete: false,
        staged: false,
        complete: false,
        placed: trs_engine::persist::Placed::No,
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
    }
}

fn tiny(i: usize, announce: Option<String>) -> TestTorrent {
    let name = format!("t{i:04}");
    make_torrent(&TorrentSpec {
        name: &name,
        piece_len: 16384,
        files: &[("f.bin", 3000 + i)],
        private: false,
        announce,
        seed: 1000 + i as u64,
    })
}

fn populate(n: usize, announce: Option<String>) -> MemStore {
    let s = MemStore::new();
    let dir = std::path::Path::new("/nonexistent/trs-scale");
    for i in 0..n {
        let t = tiny(i, announce.clone());
        let rec = record(&t, dir, announce.clone());
        let done = t.total();
        s.put(
            rec,
            LifetimeCounters { done_bytes: done, ..Default::default() },
            Some(ResumeRecord { complete: true, ..Default::default() }),
            Some(Arc::from(t.bytes.as_slice())),
        );
    }
    s
}

/// Bytes the engine holds for `n` Cold torrents (after load settles), minus the baseline.
async fn cold_cost(n: usize, tracker: Option<&MockTracker>) -> usize {
    let url = tracker.map(|t| t.url());
    let store = populate(n, url);
    let before = live();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = test_config(dir.path());
    cfg.trackers_enabled = tracker.is_some();
    cfg.max_active_downloads = 1000;
    let e = Engine::start(cfg, Arc::new(store.clone())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(700)).await;
    let h = e.handle();
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.counts.total as usize, n);
    assert_eq!(snap.counts.cold as usize, n, "all Cold, no actor");
    drop(snap);
    let after = live();
    e.shutdown(Duration::from_secs(1)).await;
    drop(store);
    after.saturating_sub(before)
}

async fn five_hundred_cold_torrents() {
    // --- memory, no trackers -------------------------------------------------------------------
    // 112 and 896 fill their hash tables completely (7/8 load), so the slope is the table's
    // worst-case per-entry cost; a half-full table costs about half as much again in slack.
    let small = cold_cost(112, None).await;
    let big = cold_cost(896, None).await;
    let per_cold = (big as f64 - small as f64) / 784.0;
    eprintln!("[scale] Cold torrent, no tracker: {per_cold:.0} B each (112: {small} B, 896: {big} B)");

    // --- with one tracker each (hub machines + gate registry) ---------------------------------
    let tr = MockTracker::start(1800).await;
    let small_t = cold_cost(112, Some(&tr)).await;
    let big_t = cold_cost(896, Some(&tr)).await;
    let per_cold_t = (big_t as f64 - small_t as f64) / 784.0;
    eprintln!("[scale] Cold torrent with 1 tracker: {per_cold_t:.0} B each (tracker share: {:.0} B)", per_cold_t - per_cold);

    // The table alone, for attribution: key + a 192-byte record per entry.
    {
        use std::collections::HashMap;
        let mk = |n: usize| {
            let before = live();
            type Entry = ([u8; 192], Option<Box<u8>>);
            let mut m: HashMap<[u8; 20], Entry> = HashMap::new();
            for i in 0..n {
                let mut k = [0u8; 20];
                k[..8].copy_from_slice(&(i as u64).to_le_bytes());
                m.insert(k, ([0; 192], None));
            }
            let used = live().saturating_sub(before); // (other threads may free while we measure)
            drop(m);
            used
        };
        let (a, b) = (mk(112), mk(896));
        eprintln!("[scale] bare HashMap<id, 200-byte entry>: {:.0} B per entry at full load", (b as f64 - a as f64) / 784.0);
    }

    // trs-tracker alone: one TrackerSet + machine + gate registration per torrent.
    {
        use trs_tracker::{TrackerUrl, gate::GateConfig, hub::{TorrentSpec, TrackerHub}, store::MemoryStore, Millis};
        let mk = |n: usize| {
            let before = live();
            let mut hub = TrackerHub::new(GateConfig::default(), MemoryStore::new(), 1);
            for i in 0..n {
                let mut ih = [0u8; 20];
                ih[..8].copy_from_slice(&(i as u64).to_le_bytes());
                let url = TrackerUrl::parse("http://tracker.example.org:6969/announce").unwrap();
                hub.add_torrent(TorrentSpec::new(ih, [1; 20], 7, 6881, vec![vec![url]]), Millis(0)).unwrap();
            }
            let used = live().saturating_sub(before); // (other threads may free while we measure)
            drop(hub);
            used
        };
        let (a, b) = (mk(112), mk(896));
        eprintln!("[scale] trs-tracker hub alone: {:.0} B per (torrent, tracker)", (b as f64 - a as f64) / 784.0);
    }

    // --- idle wake-ups -------------------------------------------------------------------------
    let store = populate(500, None);
    let dir = tempfile::tempdir().unwrap();
    let e = Engine::start(test_config(dir.path()), Arc::new(store.clone())).await.unwrap();
    let h = e.handle();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let w0 = h.diagnostics().wakeups;
    let secs = 20;
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let w1 = h.diagnostics().wakeups;
    let rate = (w1 - w0) as f64 / secs as f64;
    eprintln!("[scale] idle wake-ups with 500 Cold torrents: {} in {secs} s = {rate:.3}/s", w1 - w0);
    let snap = h.snapshot().await.unwrap();
    assert_eq!(snap.counts.cold, 500);
    e.shutdown(Duration::from_secs(1)).await;

    assert!(per_cold < 1024.0, "{per_cold}");
    assert!(per_cold_t < 8192.0, "{per_cold_t}");
    assert!(rate <= 1.0, "{rate}");
}

/// Marginal heap per torrent in each tier, and per idle peer.
async fn tier_and_peer_costs() {
    let k = 40;
    let torrents: Vec<TestTorrent> = (0..k).map(|i| tiny(i, None)).collect();
    async fn run(torrents: &[TestTorrent], tiers: TierTimings) -> usize {
        let dir = tempfile::tempdir().unwrap();
        for t in torrents {
            t.write_to(dir.path());
        }
        let before = live();
        let mut cfg = test_config(dir.path());
        cfg.tiers = tiers;
        cfg.max_active_downloads = 1000;
        let e = Engine::start(cfg, Arc::new(MemStore::new())).await.unwrap();
        let h = e.handle();
        for t in torrents {
            h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        }
        for _ in 0..200 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let s = h.snapshot().await.unwrap();
            if s.rows.iter().all(|r| matches!(r.state, TorrentState::LowPower | TorrentState::Seeding)) {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
        let c = h.snapshot().await.unwrap().counts;
        eprintln!("[scale] tiers: cold={} warm={} hot={}", c.cold, c.warm, c.hot);
        let used = live().saturating_sub(before);
        e.shutdown(Duration::from_secs(1)).await;
        used
    }
    let hour = Duration::from_secs(3600);
    let hot = run(&torrents, TierTimings { hot_to_warm: hour, warm_to_cold: hour, ..Default::default() }).await;
    let warm = run(&torrents, TierTimings { hot_to_warm: Duration::from_millis(100), warm_to_cold: hour, ..Default::default() }).await;
    let cold = run(
        &torrents,
        TierTimings { hot_to_warm: Duration::from_millis(100), warm_to_cold: Duration::from_millis(300), ..Default::default() },
    )
    .await;
    let kf = k as f64;
    let warm_extra = (warm as f64 - cold as f64) / kf;
    let hot_extra = (hot as f64 - warm as f64) / kf;
    eprintln!("[scale] Warm costs {warm_extra:.0} B more than Cold per seeding torrent; Hot {hot_extra:.0} B more than Warm");
    assert!(warm_extra < 8192.0, "{warm_extra}");

    // --- idle peers on one Warm torrent ----------------------------------------------------------
    let dir = tempfile::tempdir().unwrap();
    torrents[0].write_to(dir.path());
    let e = Engine::start(test_config(dir.path()), Arc::new(MemStore::new())).await.unwrap();
    let h = e.handle();
    h.add_torrent_file(torrents[0].bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("seeding", 10, || async {
        matches!(h.snapshot().await.ok()?.rows.first()?.state, TorrentState::LowPower | TorrentState::Seeding).then_some(())
    })
    .await;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], h.listen_port()));
    // Peers are added in two steps, 16 and then 32 more, so that one-off allocations (the first
    // page of tokio's I/O registrations, hash table growth) cancel out of the slope. Each step
    // waits until the idle trimming (5 s) has run: an *idle* peer holds no buffers.
    let mut socks = Vec::new();
    let connect = |n: usize, from: usize| {
        let torrent = torrents[0].id;
        async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut out = Vec::new();
            for i in from..from + n {
                let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
                let mut hs = Vec::new();
                hs.extend(b"\x13BitTorrent protocol");
                hs.extend([0u8; 8]);
                hs.extend(torrent);
                hs.extend(format!("-ID0001-idle{i:08}").bytes());
                s.write_all(&hs).await.unwrap();
                let mut buf = [0u8; 68];
                s.read_exact(&mut buf).await.unwrap();
                out.push(s);
            }
            out
        }
    };
    let settle = || tokio::time::sleep(Duration::from_millis(6500));
    tokio::time::sleep(Duration::from_millis(300)).await;
    // 14 and 56 peers: both fill the torrent's hash table of peers to its resize threshold (7/8 of
    // a power of two), so the slope is the table's worst-case per-entry cost and does not depend on
    // where a doubling happens to fall. Two independent rounds; noise from other threads only adds
    // allocations, so the smaller slope is the robust one.
    socks.extend(connect(14, 0).await);
    settle().await;
    let p14 = (live(), snapshot_classes());
    socks.extend(connect(42, 14).await);
    settle().await;
    let p56 = (live(), snapshot_classes());
    let first_slope = (p56.0 as f64 - p14.0 as f64) / 42.0;
    // Second round on top: drop to 14 again is not possible without waiting for the engine to
    // notice, so measure the same span once more later instead (allocator state has settled).
    settle().await;
    let p56b = live();
    let second_slope = (p56b as f64 - p14.0 as f64) / 42.0;
    let per_peer = first_slope.min(second_slope);
    eprintln!("[scale] per idle peer connection (engine heap, socket buffers excluded): {per_peer:.0} B (slopes {first_slope:.0}, {second_slope:.0})");
    for (i, (a, b)) in p14.1.iter().zip(&p56.1).enumerate() {
        let d = (*b - *a) as f64 / 42.0;
        if d.abs() >= 0.5 {
            eprintln!("[scale]   allocations of {} B..{} B per idle peer: {d:+.1}", (1usize << i) / 2 + 1, 1usize << i);
        }
    }
    let d = h.torrent_detail(torrents[0].id).await.unwrap();
    assert_eq!(d.peers.len(), 56);

    // The same sockets without the engine: what two ends of a loopback connection cost in this
    // process (tokio registrations, socket handles), measured the same way.
    let control = {
        async fn pairs(l: &tokio::net::TcpListener, n: usize) -> Vec<(tokio::net::TcpStream, tokio::net::TcpStream)> {
            let la = l.local_addr().unwrap();
            let mut v = Vec::new();
            for _ in 0..n {
                let c = tokio::net::TcpStream::connect(la).await.unwrap();
                let (s, _) = l.accept().await.unwrap();
                v.push((c, s));
            }
            v
        }
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let v1 = pairs(&l, 16).await;
        let a = live();
        let v2 = pairs(&l, 32).await;
        let b = live();
        drop((v1, v2));
        (b as f64 - a as f64) / 32.0
    };
    eprintln!("[scale] a plain loopback socket pair (both ends) costs {control:.0} B of the above");
    drop(socks);
    e.shutdown(Duration::from_secs(1)).await;
    assert!(per_peer < PER_PEER_LIMIT, "{per_peer}");
    assert!(per_peer - control < ENGINE_PER_PEER_LIMIT, "engine part {}", per_peer - control);
}

fn snapshot_classes() -> Vec<isize> {
    CLASSES.iter().map(|c| c.load(Relaxed)).collect()
}

/// Engine heap per idle peer, socket buffers excluded, tokio's socket registrations included (about
/// 720 B for the two ends of a loopback connection). Measured 2.7 KB; the limit leaves room for
/// allocator and machine-load noise.
const PER_PEER_LIMIT: f64 = 3300.0;
/// The same without the two socket ends' registrations (measured in the same run): ENGINE.md 4's
/// "2 KiB of protocol state per idle peer", plus about 15 % for noise.
const ENGINE_PER_PEER_LIMIT: f64 = 2350.0;

/// One test so the allocator counts are not disturbed by a second test running in parallel.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scale() {
    five_hundred_cold_torrents().await;
    tier_and_peer_costs().await;
}
