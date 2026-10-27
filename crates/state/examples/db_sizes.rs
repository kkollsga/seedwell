//! Prints database file sizes for N torrents (diagnostic for the README).
use trs_state::{Counters, OpenOptions, Store, TorrentRecord};

fn main() {
    let n: u32 = std::env::args().nth(1).and_then(|v| v.parse().ok()).unwrap_or(1000);
    let dir = std::env::temp_dir().join(format!("trs-state-sizes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let size = |d: &std::path::Path| std::fs::metadata(d.join("state.redb")).map(|m| m.len()).unwrap_or(0);
    {
        let s = Store::open_with(&dir, &OpenOptions::default()).unwrap().store;
        s.flush_now().unwrap();
        println!("empty database: {} KiB", size(&dir) / 1024);
        for k in 0..n {
            let mut id = [0u8; 20];
            id[..4].copy_from_slice(&k.to_be_bytes());
            let mut r = TorrentRecord::new(format!("Some.Show.S01E{k:04}.1080p.WEB-DL.DDP5.1.H.264-GROUP"), 1_500_000_000, 1_791_000_000);
            r.tracker_profile = Some("example".into());
            s.put_torrent(id, &r).unwrap();
            s.add_counters(id, Counters { downloaded: 1, uploaded: 2, seed_secs: 3 }).unwrap();
            if k % 100 == 99 {
                s.flush_now().unwrap(); // many small commits, like real life
            }
        }
        s.flush_now().unwrap();
    }
    let _ = std::process::Command::new("du").arg("-k").arg(dir.join("state.redb")).status();
    println!("{n} torrents (+counters), committed in batches of 100: {} KiB", size(&dir) / 1024);
    let _ = std::fs::remove_dir_all(&dir);
}
