//! A favorite tracker's shared speed caps (SPEC 7E.4): one bucket for all of the tracker's
//! torrents together, on top of the global limit, changing live.
use crate::common;
use std::time::{Duration, Instant};

use common::*;
use trs_engine::*;
use trs_state::rules::{DefaultRule, SeedingGoal};
use trs_state::trackers::{Profile, UrlPattern};
use trs_state::{RuleSet, TrackerProfiles};

const ANNOUNCE: &str = "http://127.0.0.1:1/announce";

fn policy(down: Option<u64>, up: Option<u64>) -> PolicyData {
    let mut rs = RuleSet::default();
    rs.set_default(DefaultRule { goal: SeedingGoal::Forever, ..DefaultRule::default() }).unwrap();
    let mut tp = TrackerProfiles::default();
    tp.add(Profile {
        id: "fav".into(),
        name: "Fav".into(),
        urls: vec![UrlPattern::parse("127.0.0.1").unwrap()],
        down_limit: down,
        up_limit: up,
        ..Profile::default()
    })
    .unwrap();
    PolicyData { rules: rs, profiles: tp, private_min_seed: None, idle: None }
}

/// Three 8 MB torrents of the favorite's tracker, seeded by one node and leeched by another.
struct Rig {
    seeder: Node,
    leech: Node,
    ids: Vec<TorrentId>,
}

async fn rig(seed: u64, pol: PolicyData, seeder_cfg: impl FnOnce(&mut EngineConfig)) -> Rig {
    let seeder = node(seeder_cfg).await;
    seeder.h.apply_policy(pol);
    let leech = node(|_| {}).await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let t = standard_named(&format!("bundle{i}"), 8_000_000, Some(ANNOUNCE.into()), false, seed + i);
        t.write_to(seeder.dir.path());
        seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&seeder, &t.id, 60).await;
        assert_eq!(seeder.row(&t.id).await.unwrap().tracker_profile.as_deref(), Some("fav"));
        ids.push(t.id);
        leech
            .h
            .add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() })
            .await
            .unwrap();
    }
    Rig { seeder, leech, ids }
}

impl Rig {
    async fn uploaded(&self) -> u64 {
        let mut n = 0;
        for id in &self.ids {
            n += self.seeder.row(id).await.map_or(0, |r| r.uploaded_total);
        }
        n
    }

    async fn downloaded(&self) -> u64 {
        let mut n = 0;
        for id in &self.ids {
            n += self.leech.row(id).await.map_or(0, |r| r.done_bytes);
        }
        n
    }

    /// Bytes per second the seeder uploads over the next `secs`.
    async fn up_rate(&self, secs: u64) -> f64 {
        let (b0, t0) = (self.uploaded().await, Instant::now());
        tokio::time::sleep(Duration::from_secs(secs)).await;
        (self.uploaded().await - b0) as f64 / t0.elapsed().as_secs_f64()
    }

    async fn down_rate(&self, secs: u64) -> f64 {
        let (b0, t0) = (self.downloaded().await, Instant::now());
        tokio::time::sleep(Duration::from_secs(secs)).await;
        (self.downloaded().await - b0) as f64 / t0.elapsed().as_secs_f64()
    }
}

/// Seconds of each measurement (`up_rate`, `down_rate`).
const WINDOW_S: f64 = 5.0;

/// A rate measured over `WINDOW_S` under a cap of `cap` B/s: never under 90 % of it, and never over the cap plus the
/// one second of burst the bucket may hold (`TorrentLimit`, `RateLimiter`), spent at once when the transfer fell
/// behind just before the window (a loaded CI runner), with 5 % to spare. Far from what an unshared cap of three
/// torrents gives (three times the cap).
fn capped(got: f64, cap: f64) -> bool {
    got >= cap * 0.90 && got <= cap * (WINDOW_S + 1.0) / WINDOW_S * 1.05
}

/// The cap is for the three torrents together, and a changed cap reaches the running torrents.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trackers_upload_cap_is_shared_by_its_torrents_and_changes_live() {
    let r = rig(7100, policy(None, Some(1_000_000)), |_| {}).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let a = r.up_rate(WINDOW_S as u64).await;
    eprintln!("tracker cap 1.0 MB/s -> {a:.0} B/s over 3 torrents");
    assert!(capped(a, 1_000_000.0), "{a}");
    // Halve it live.
    r.seeder.h.apply_policy(policy(None, Some(500_000)));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let b = r.up_rate(WINDOW_S as u64).await;
    eprintln!("tracker cap 0.5 MB/s -> {b:.0} B/s");
    assert!(capped(b, 500_000.0), "{b}");
    // Remove it: the favorite has no bucket any more and the torrents run free.
    // About 20 MB are left: 40 s at the old cap, a moment without one.
    r.seeder.h.apply_policy(policy(None, None));
    let t0 = Instant::now();
    for id in &r.ids {
        wait_done(&r.leech, id, 10).await;
    }
    eprintln!("unlimited: the rest arrived in {:?}", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_secs(8));
}

/// The global limit still applies on top: the stricter of the two wins.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_global_cap_applies_on_top_of_the_trackers() {
    let r = rig(7200, policy(None, Some(2_000_000)), |c| c.up_limit = Some(600_000)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let a = r.up_rate(WINDOW_S as u64).await;
    eprintln!("global 0.6 MB/s under tracker 2 MB/s -> {a:.0} B/s");
    assert!(capped(a, 600_000.0), "{a}");
    // A tracker cap below the global one wins.
    r.seeder.h.apply_policy(policy(None, Some(300_000)));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let b = r.up_rate(WINDOW_S as u64).await;
    eprintln!("tracker 0.3 MB/s under global 0.6 MB/s -> {b:.0} B/s");
    assert!(capped(b, 300_000.0), "{b}");
}

/// The download cap is shared too (measured at the downloading node).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_trackers_download_cap_is_shared_by_its_torrents() {
    let seeder = node(|_| {}).await;
    let leech = node(|_| {}).await;
    leech.h.apply_policy(policy(Some(800_000), None));
    let mut ids = Vec::new();
    for i in 0..3 {
        let t = standard_named(&format!("bundle{i}"), 8_000_000, Some(ANNOUNCE.into()), false, 7300 + i);
        t.write_to(seeder.dir.path());
        seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&seeder, &t.id, 60).await;
        leech
            .h
            .add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() })
            .await
            .unwrap();
        ids.push(t.id);
    }
    let r = Rig { seeder, leech, ids };
    tokio::time::sleep(Duration::from_secs(2)).await;
    let a = r.down_rate(WINDOW_S as u64).await;
    eprintln!("tracker download cap 0.8 MB/s -> {a:.0} B/s over 3 torrents");
    assert!(capped(a, 800_000.0), "{a}");
}

/// A favorite whose addresses stop matching lets its torrents out of the cap, and one that starts
/// matching takes them in: torrents move between groups live.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn torrents_move_between_groups_when_the_favorites_addresses_change() {
    let r = rig(7400, policy(None, Some(400_000)), |_| {}).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let a = r.up_rate(WINDOW_S as u64).await;
    eprintln!("capped -> {a:.0} B/s");
    assert!(capped(a, 400_000.0), "{a}");
    // The favorite now claims another host: the torrents are nobody's and run free.
    let mut moved = policy(None, Some(400_000));
    moved.profiles.profiles[0].urls = vec![UrlPattern::parse("elsewhere.test").unwrap()];
    r.seeder.h.apply_policy(moved);
    let t0 = Instant::now();
    for id in &r.ids {
        wait_done(&r.leech, id, 20).await;
    }
    eprintln!("left the group: the rest arrived in {:?}", t0.elapsed());
    assert!(t0.elapsed() < Duration::from_secs(10));
    assert_eq!(r.seeder.row(&r.ids[0]).await.unwrap().tracker_profile, None);
}
