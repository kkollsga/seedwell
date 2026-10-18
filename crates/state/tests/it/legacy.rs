//! Opening a `state.redb` written by Seedwell 0.1 (redb 2.6, file format v2): it is backed up,
//! converted to the current redb, and everything in it is still there.

use crate::common;

use std::net::SocketAddr;
use std::path::Path;

use common::{TempDir, id, open};
use trs_state::{Counters, Day, StatsBucket, StatsScope, TorrentRecord};
use trs_tracker::store::{TimingKey, TimingRecord, TimingStore};

/// Rewrite the current-format file at `dir/state.redb` as redb 2.6 would have written it, row for
/// row. The rows were encoded by the store itself, so this is exactly what Seedwell 0.1 left on disk.
fn downgrade_to_redb2(dir: &Path) {
    let path = dir.join("state.redb");
    let v2 = dir.join("state.redb.v2");
    {
        let src = redb::Database::open(&path).unwrap();
        let rtx = redb::ReadableDatabase::begin_read(&src).unwrap();
        let dst = redb2::Database::create(&v2).unwrap();
        let w = dst.begin_write().unwrap();
        macro_rules! copy {
            ($name:literal, $k:ty, $v:ty) => {{
                let s: redb::TableDefinition<$k, $v> = redb::TableDefinition::new($name);
                let d: redb2::TableDefinition<$k, $v> = redb2::TableDefinition::new($name);
                let s = rtx.open_table(s).unwrap();
                let mut d = w.open_table(d).unwrap();
                for row in redb::ReadableTable::iter(&s).unwrap() {
                    let (k, v) = row.unwrap();
                    d.insert(k.value(), v.value()).unwrap();
                }
            }};
        }
        copy!("meta", &str, &[u8]);
        copy!("torrents", [u8; 20], &[u8]);
        copy!("resume", [u8; 20], &[u8]);
        copy!("counters", [u8; 20], &[u8]);
        copy!("tracker_timing", &[u8], &[u8]);
        copy!("stats_daily", (&str, u32), &[u8]);
        copy!("stats_totals", &str, &[u8]);
        copy!("peers_cache", [u8; 20], &[u8]);
        copy!("dht_table", &str, &[u8]);
        copy!("save_roots", u32, &str);
        w.commit().unwrap();
    }
    std::fs::rename(&v2, &path).unwrap();
    // The new redb really cannot open it: this is the case the conversion exists for.
    assert!(matches!(redb::Database::open(&path), Err(redb::DatabaseError::UpgradeRequired(_))));
}

fn set_schema_redb2(dir: &Path, v: u32) {
    let db = redb2::Database::open(dir.join("state.redb")).unwrap();
    let w = db.begin_write().unwrap();
    {
        let t: redb2::TableDefinition<&str, &[u8]> = redb2::TableDefinition::new("meta");
        w.open_table(t).unwrap().insert("schema", v.to_le_bytes().as_slice()).unwrap();
    }
    w.commit().unwrap();
}

fn record(n: u32, root: u32) -> TorrentRecord {
    let mut r = TorrentRecord::new(format!("Torrent number {n}"), 1_000_000 * u64::from(n + 1), 1_791_000_000 + i64::from(n));
    r.save_root = root;
    r.piece_count = 64;
    r
}

fn timing_key(n: u32) -> TimingKey {
    TimingKey { info_hash: id(n), tracker_url: format!("udp://tracker{n}.example:6969/announce") }
}

const TODAY: Day = Day(20_380);

/// A 0.1 data folder with one of everything the store keeps.
fn seed(dir: &Path) {
    let s = open(dir).store;
    let a = s.intern_root("/Volumes/Seed").unwrap();
    let b = s.intern_root("/Users/x/Downloads").unwrap();
    for n in 0..20 {
        s.put_torrent(id(n), &record(n, if n % 2 == 0 { a } else { b })).unwrap();
        s.add_counters(id(n), Counters { downloaded: u64::from(n), uploaded: 2 * u64::from(n), seed_secs: 60 }).unwrap();
    }
    let peers: Vec<SocketAddr> = vec!["10.0.0.1:6881".parse().unwrap(), "[::1]:51413".parse().unwrap()];
    s.put_peers(id(3), &peers).unwrap();
    s.put_dht_table(b"dht-blob").unwrap();
    for (scope, day, up) in [
        (StatsScope::Profile("tl".into()), TODAY, 5),
        (StatsScope::Profile("tl".into()), TODAY.minus(1), 7),
        (StatsScope::Profile("a much longer profile name".into()), TODAY.minus(40), 11),
        (StatsScope::Public, TODAY, 13),
    ] {
        s.record_stats(&scope, day, &StatsBucket { uploaded: up, torrents_added: 1, ..Default::default() }).unwrap();
    }
    let mut h = s.timing_handle();
    h.save(&timing_key(1), &TimingRecord { completed_sent: true, ..Default::default() }).unwrap();
    s.flush_now().unwrap();
}

fn assert_seeded(dir: &Path) {
    let s = open(dir).store;
    let loaded = s.load_torrents().unwrap();
    assert!(loaded.skipped.is_empty(), "{:?}", loaded.skipped);
    assert_eq!(loaded.records.len(), 20);
    for (h, r) in &loaded.records {
        let n = u32::from_be_bytes(h[..4].try_into().unwrap());
        assert_eq!(r.name, format!("Torrent number {n}"));
        let want = if n % 2 == 0 { "/Volumes/Seed" } else { "/Users/x/Downloads" };
        assert_eq!(s.root_path(r.save_root).as_deref(), Some(want));
        assert_eq!(s.get_counters(*h).unwrap(), Counters { downloaded: u64::from(n), uploaded: 2 * u64::from(n), seed_secs: 60 });
    }
    assert_eq!(s.get_peers(id(3)).unwrap().len(), 2);
    assert_eq!(s.get_dht_table().unwrap().as_deref(), Some(&b"dht-blob"[..]));
    // The daily rows have a (name, day) tuple key, whose encoding changed in redb 3.
    let tl = StatsScope::Profile("tl".into());
    assert_eq!(s.stats_series(&tl, TODAY, 2).unwrap().iter().map(|(_, b)| b.uploaded).collect::<Vec<_>>(), [7, 5]);
    let long = StatsScope::Profile("a much longer profile name".into());
    assert_eq!(s.stats_series(&long, TODAY, 60).unwrap().iter().map(|(_, b)| b.uploaded).sum::<u64>(), 11);
    assert_eq!(s.stats_total(&StatsScope::All).unwrap().uploaded, 5 + 7 + 11 + 13);
    let mut scopes = s.stats_scopes().unwrap();
    scopes.sort();
    assert_eq!(scopes, ["@public", "a much longer profile name", "tl"]);
    assert_eq!(s.timing_handle().load(&timing_key(1)).map(|r| r.completed_sent), Some(true));
    // The roots table came across: interning finds the stored ids and continues after them.
    let seed_root = loaded.records.iter().find(|(h, _)| h[3] == 0).unwrap().1.save_root;
    assert_eq!(s.intern_root("/Volumes/Seed").unwrap(), seed_root);
    assert!(s.intern_root("/elsewhere").unwrap() > 2);
}

#[test]
fn a_0_1_database_is_backed_up_converted_and_keeps_everything() {
    let dir = TempDir::new("legacy");
    seed(dir.path());
    downgrade_to_redb2(dir.path());
    let original = std::fs::read(dir.path().join("state.redb")).unwrap();

    let o = open(dir.path());
    let backup = o.report.converted.clone().expect("converted");
    assert!(o.report.recovered.is_none() && o.report.migrated.is_none() && !o.report.created, "{:?}", o.report);
    assert!(backup.starts_with(dir.path().join("backups")));
    assert_eq!(std::fs::read(&backup).unwrap(), original, "the backup is the untouched 0.1 file");
    drop(o);
    assert!(!dir.path().join("state.redb.convert").exists());

    assert_seeded(dir.path());
    let again = open(dir.path());
    assert!(again.report.converted.is_none(), "converted once");
    drop(again);
    assert!(redb::Database::open(dir.path().join("state.redb")).is_ok());
}

#[test]
fn an_old_schema_in_a_0_1_file_is_converted_then_migrated() {
    let dir = TempDir::new("legacy-v1");
    seed(dir.path());
    downgrade_to_redb2(dir.path());
    set_schema_redb2(dir.path(), 1);
    let o = open(dir.path());
    assert!(o.report.converted.is_some());
    let m = o.report.migrated.clone().expect("schema 1 -> current");
    assert_eq!(m.from, 1);
    assert_eq!(o.report.schema_version, trs_state::store::SCHEMA.current);
    drop(o);
    assert_seeded(dir.path());
}

#[test]
fn a_damaged_0_1_file_is_never_lost() {
    let base = TempDir::new("legacy-base");
    seed(base.path());
    downgrade_to_redb2(base.path());
    let original = std::fs::read(base.path().join("state.redb")).unwrap();
    // Cut the file at several points; each open either converts it or sets it aside, and the
    // original bytes always survive somewhere in the folder.
    for cut in [original.len() / 2, original.len() - 4096, 8192] {
        let dir = TempDir::new("legacy-cut");
        std::fs::write(dir.path().join("state.redb"), &original[..cut]).unwrap();
        match trs_state::Store::open_with(dir.path(), &common::fast()) {
            Ok(o) => assert!(o.report.converted.is_some() || o.report.recovered.is_some(), "cut {cut}: {:?}", o.report),
            Err(e) => assert!(dir.path().join("state.redb").exists(), "cut {cut}: {e}"),
        }
        let kept = walk(dir.path()).into_iter().any(|p| std::fs::read(&p).map(|b| b == original[..cut]).unwrap_or(false));
        assert!(kept, "cut {cut}: the damaged original was kept");
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() { out.extend(walk(&p)) } else { out.push(p) }
    }
    out
}
