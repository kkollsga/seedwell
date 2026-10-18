//! Size and load-time budgets (docs/ENGINEERING.md memory discipline, task: 1000 torrents < 50 ms).
//!
//! Run `cargo test -p trs-state --release --test perf -- --nocapture` for the real numbers; the
//! debug build only checks a 10x looser bound.

use crate::common;

use std::time::Instant;

use common::{TempDir, fast, id};
use trs_classify::Category;
use trs_state::rules::{Priority, SeedingGoal};
use trs_state::{Counters, Store, TorrentRecord};

fn rec(n: u32) -> TorrentRecord {
    let mut r = TorrentRecord::new(
        format!("Some.Show.S{:02}E{:02}.1080p.WEB-DL.DDP5.1.H.264-GROUP{n:04}", n % 20, n % 30),
        1_500_000_000 + u64::from(n) * 1_000,
        1_791_000_000 + i64::from(n),
    );
    r.piece_count = 1500;
    r.file_count = 2;
    r.completed_at = Some(1_791_100_000 + i64::from(n));
    r.save_root = 1 + n % 3;
    r.category = Some(Category::Tv);
    r.tracker_profile = Some("example".into());
    if n % 10 == 0 {
        r.priority = Some(Priority::High);
        r.goal = Some(SeedingGoal::Forever);
    }
    r
}

#[test]
fn thousand_torrents_load_fast_and_stay_small() {
    let dir = TempDir::new("perf");
    let mut encoded = 0usize;
    {
        let s = Store::open_with(dir.path(), &fast()).unwrap().store;
        for n in 0..1000 {
            let r = rec(n);
            encoded += r.encode().len();
            s.put_torrent(id(n), &r).unwrap();
            s.add_counters(id(n), Counters { downloaded: 1_500_000_000, uploaded: 3_000_000_000 + u64::from(n), seed_secs: 86_400 * 30 }).unwrap();
        }
        s.flush_now().unwrap();
    }
    let size = std::fs::metadata(dir.path().join("state.redb")).unwrap().len();

    let t0 = Instant::now();
    let opened = Store::open_with(dir.path(), &fast()).unwrap();
    let open = t0.elapsed();
    let s = opened.store;
    let t1 = Instant::now();
    let loaded = s.load_torrents().unwrap();
    let load = t1.elapsed();
    let t2 = Instant::now();
    let counters = s.load_counters().unwrap();
    let cload = t2.elapsed();
    assert_eq!(loaded.records.len(), 1000);
    assert_eq!(counters.len(), 1000);
    assert!(loaded.skipped.is_empty());
    eprintln!(
        "1000 torrents: record avg {} B (+20 B key), file {} KiB ({} B/torrent incl. counters); open+integrity check {:?}, load_torrents {:?}, load_counters {:?}",
        encoded / 1000,
        size / 1024,
        size / 1000,
        open,
        load,
        cload
    );
    let budget = if cfg!(debug_assertions) { 500 } else { 50 };
    assert!(load.as_millis() < budget, "load_torrents took {load:?}");
    assert!(encoded / 1000 < 100, "average record is {} bytes", encoded / 1000);
}
