//! Behaviour of the redb-backed store: persistence, batching, removal, statistics, timing,
//! schema migrations and recovery.

use crate::common;

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use common::{TempDir, fast, id, open};
use trs_classify::Category;
use trs_state::record::{FileStamp, PartialPiece};
use trs_state::rules::{Priority, SeedingGoal};
use trs_state::store::{Migration, Schema};
use trs_state::{Counters, Day, OpenOptions, ResumeData, StateError, StatsBucket, StatsScope, Store, TorrentRecord};
use trs_tracker::store::{TimingKey, TimingRecord, TimingStore};
use trs_tracker::time::Millis;

fn record(n: u32) -> TorrentRecord {
    let mut r = TorrentRecord::new(format!("Torrent number {n}"), 1_000_000 * u64::from(n + 1), 1_791_000_000 + i64::from(n));
    r.piece_count = 64;
    r.tracker_profile = Some("tl".into());
    r.category = Some(Category::Tv);
    r.priority = Some(Priority::High);
    r.goal = Some(SeedingGoal::Forever);
    r
}

fn resume(pieces: u32) -> ResumeData {
    let len = (pieces as usize).div_ceil(8);
    let mut bitfield = vec![0u8; len];
    bitfield[0] = 0xff;
    let spare = len * 8 - pieces as usize;
    if spare > 0 {
        let last = bitfield.len() - 1;
        bitfield[last] &= !((1u8 << spare) - 1);
    }
    ResumeData {
        piece_count: pieces,
        bitfield: bitfield.into_boxed_slice(),
        partial: vec![PartialPiece { index: 9, blocks: vec![0b1010].into_boxed_slice() }],
        files: vec![FileStamp { size: 5, mtime_ns: 7 }],
        saved_at: 42,
    }
}

#[test]
fn everything_survives_a_reopen() {
    let dir = TempDir::new("reopen");
    let peers: Vec<SocketAddr> = vec!["10.0.0.1:6881".parse().unwrap(), "[::1]:51413".parse().unwrap()];
    {
        let o = open(dir.path());
        assert!(o.report.created && o.report.recovered.is_none() && o.report.migrated.is_none());
        let s = o.store;
        let root = s.intern_root("/Volumes/Seed").unwrap();
        assert_eq!(s.intern_root("/Volumes/Seed").unwrap(), root, "interning is idempotent");
        let mut rec = record(1);
        rec.save_root = root;
        s.put_torrent_file(&id(1), b"d4:infod4:name1:aee").unwrap();
        s.put_torrent(id(1), &rec).unwrap();
        s.put_resume(id(1), &resume(70)).unwrap();
        s.add_counters(id(1), Counters { downloaded: 10, uploaded: 20, seed_secs: 30 }).unwrap();
        s.put_peers(id(1), &peers).unwrap();
        s.put_dht_table(b"dht-blob").unwrap();
        s.record_stats(&StatsScope::Profile("tl".into()), Day(20_000), &StatsBucket { uploaded: 5, ..Default::default() }).unwrap();
        // No explicit flush: dropping the store must commit what was accepted.
    }
    let o = open(dir.path());
    assert!(!o.report.created);
    let s = o.store;
    let loaded = s.load_torrents().unwrap();
    assert!(loaded.skipped.is_empty());
    assert_eq!(loaded.records.len(), 1);
    assert_eq!(loaded.records[0].0, id(1));
    assert_eq!(loaded.records[0].1.name, "Torrent number 1");
    assert_eq!(s.root_path(loaded.records[0].1.save_root).as_deref(), Some("/Volumes/Seed"));
    assert_eq!(s.get_resume(id(1)).unwrap(), Some(resume(70)));
    assert_eq!(s.get_counters(id(1)).unwrap(), Counters { downloaded: 10, uploaded: 20, seed_secs: 30 });
    assert_eq!(s.get_peers(id(1)).unwrap(), peers);
    assert_eq!(s.get_dht_table().unwrap().as_deref(), Some(&b"dht-blob"[..]));
    assert_eq!(s.stats_total(&StatsScope::All).unwrap().uploaded, 5);
    assert_eq!(s.read_torrent_file(&id(1)).unwrap(), b"d4:infod4:name1:aee");
    assert_eq!(s.list_torrent_files().unwrap(), vec![id(1)]);
    // Interning continues after the highest stored id.
    assert!(s.intern_root("/other").unwrap() > loaded.records[0].1.save_root);
}

#[test]
fn writes_are_debounced_then_committed_without_a_flush() {
    let dir = TempDir::new("debounce");
    let o = Store::open_with(dir.path(), &trs_state::OpenOptions { debounce: Duration::from_millis(200), ..Default::default() }).unwrap();
    let s = o.store;
    s.put_torrent(id(5), &record(5)).unwrap();
    assert_eq!(s.pending_calls(), 1);
    let t = Instant::now();
    while s.pending_calls() > 0 {
        assert!(t.elapsed() < Duration::from_secs(5), "never committed");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(t.elapsed() >= Duration::from_millis(150), "committed too early: {:?}", t.elapsed());
    assert!(s.last_write_error().is_none());
}

#[test]
fn many_calls_coalesce_into_one_row_per_key() {
    let dir = TempDir::new("coalesce");
    let s = open(dir.path()).store;
    for i in 0..1000u64 {
        s.add_counters(id(1), Counters { downloaded: 1, uploaded: 2, seed_secs: 0 }).unwrap();
        let mut r = record(1);
        r.total_size = i;
        s.put_torrent(id(1), &r).unwrap();
    }
    s.flush_now().unwrap();
    assert_eq!(s.get_counters(id(1)).unwrap(), Counters { downloaded: 1000, uploaded: 2000, seed_secs: 0 });
    assert_eq!(s.get_torrent(id(1)).unwrap().unwrap().total_size, 999, "last write wins");
    assert_eq!(s.load_torrents().unwrap().records.len(), 1);
}

#[test]
fn counters_only_grow() {
    let dir = TempDir::new("counters");
    let s = open(dir.path()).store;
    s.set_counters_at_least(id(2), Counters { downloaded: 100, uploaded: 50, seed_secs: 9 }).unwrap();
    s.flush_now().unwrap();
    s.set_counters_at_least(id(2), Counters { downloaded: 10, uploaded: 500, seed_secs: 1 }).unwrap();
    s.flush_now().unwrap();
    assert_eq!(s.get_counters(id(2)).unwrap(), Counters { downloaded: 100, uploaded: 500, seed_secs: 9 });
    s.add_counters(id(2), Counters { downloaded: u64::MAX, uploaded: 0, seed_secs: 0 }).unwrap();
    s.flush_now().unwrap();
    assert_eq!(s.get_counters(id(2)).unwrap().downloaded, u64::MAX, "saturates, never wraps");
    assert_eq!(s.load_counters().unwrap().len(), 1);
}

#[test]
fn remove_deletes_rows_and_file_but_keeps_stats() {
    let dir = TempDir::new("remove");
    let s = open(dir.path()).store;
    let key = TimingKey { info_hash: id(3), tracker_url: "https://t.org/announce?pk=secret".into() };
    let other = TimingKey { info_hash: id(4), tracker_url: "https://t.org/announce?pk=secret".into() };
    let mut timing = s.timing_handle();
    timing.save(&key, &TimingRecord { completed_sent: true, ..Default::default() }).unwrap();
    timing.save(&other, &TimingRecord { completed_sent: true, ..Default::default() }).unwrap();
    for n in [3, 4] {
        s.put_torrent_file(&id(n), b"x").unwrap();
        s.put_torrent(id(n), &record(n)).unwrap();
        s.put_resume(id(n), &resume(16)).unwrap();
        s.put_peers(id(n), &["1.1.1.1:1".parse().unwrap()]).unwrap();
        s.add_counters(id(n), Counters { downloaded: 1, uploaded: 1, seed_secs: 1 }).unwrap();
    }
    s.record_stats(&StatsScope::Public, Day(100), &StatsBucket { uploaded: 77, torrents_removed: 1, ..Default::default() }).unwrap();
    s.flush_now().unwrap();

    s.remove_torrent(id(3)).unwrap();
    s.flush_now().unwrap();
    assert!(s.get_torrent(id(3)).unwrap().is_none());
    assert!(s.get_resume(id(3)).unwrap().is_none());
    assert_eq!(s.get_counters(id(3)).unwrap(), Counters::default());
    assert!(s.get_peers(id(3)).unwrap().is_empty());
    assert!(timing.load(&key).is_none(), "timing rows purged");
    assert!(timing.load(&other).is_some(), "other torrents untouched");
    assert!(s.get_torrent(id(4)).unwrap().is_some());
    // The file is unlinked right after the commit.
    let t = Instant::now();
    while s.read_torrent_file(&id(3)).is_ok() {
        assert!(t.elapsed() < Duration::from_secs(2), "file still there");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.read_torrent_file(&id(4)).is_ok());
    assert_eq!(s.stats_total(&StatsScope::Public).unwrap().uploaded, 77, "history survives removal");
}

#[test]
fn re_adding_right_after_removal_keeps_the_file() {
    let dir = TempDir::new("readd");
    let s = open(dir.path()).store;
    s.put_torrent_file(&id(7), b"orig").unwrap();
    s.put_torrent(id(7), &record(7)).unwrap();
    s.flush_now().unwrap();
    s.remove_torrent(id(7)).unwrap();
    s.put_torrent_file(&id(7), b"again").unwrap();
    s.put_torrent(id(7), &record(7)).unwrap();
    s.flush_now().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(s.read_torrent_file(&id(7)).unwrap(), b"again");
    assert!(s.get_torrent(id(7)).unwrap().is_some());
}

#[test]
fn failed_commits_are_kept_merged_and_retried() {
    let dir = TempDir::new("retry");
    let s = open(dir.path()).store;
    s.add_counters(id(1), Counters { downloaded: 1, ..Default::default() }).unwrap();
    s.__inject_commit_failures(1);
    let err = s.flush_now().unwrap_err();
    assert!(matches!(err, StateError::WriteFailed(_)), "{err}");
    assert!(s.last_write_error().is_some());
    // New work arrives while the failed batch is pending: both must land, exactly once.
    s.add_counters(id(1), Counters { downloaded: 10, ..Default::default() }).unwrap();
    s.put_torrent(id(1), &record(1)).unwrap();
    s.flush_now().unwrap();
    assert_eq!(s.get_counters(id(1)).unwrap().downloaded, 11);
    assert!(s.get_torrent(id(1)).unwrap().is_some());
    assert!(s.last_write_error().is_none());
}

#[test]
fn background_retry_succeeds_without_a_caller() {
    let dir = TempDir::new("bgretry");
    let s = open(dir.path()).store;
    s.__inject_commit_failures(2);
    s.put_torrent(id(1), &record(1)).unwrap();
    let t = Instant::now();
    while s.pending_calls() > 0 {
        assert!(t.elapsed() < Duration::from_secs(5), "never recovered");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(s.get_torrent(id(1)).unwrap().is_some());
}

/// Statistics belong to the automatic tracker (its domain), so saving a favorite from it or
/// removing the favorite never changes the history (SPEC 7E.2).
#[test]
fn favorite_keeps_the_trackers_history_and_unfavorite_too() {
    use trs_state::stats::scopes_for;
    use trs_state::trackers::TrackerProfiles;
    let dir = TempDir::new("fav-stats");
    let s = open(dir.path()).store;
    let today = Day::from_ymd(2026, 10, 11).unwrap();
    let b = |down, up| StatsBucket { downloaded: down, uploaded: up, torrents_completed: 1, ..Default::default() };
    // Two torrents on two hosts of one domain accumulate under the domain.
    s.record_stats(&StatsScope::domain("example.org"), today.minus(3), &b(5, 50)).unwrap();
    s.record_stats(&StatsScope::domain("example.org"), today, &b(1, 10)).unwrap();
    s.record_stats(&StatsScope::domain("other.org"), today, &b(100, 100)).unwrap();
    s.flush_now().unwrap();
    let total = |scopes: &[StatsScope]| {
        let mut t = StatsBucket::default();
        for sc in scopes {
            t.add(&s.stats_total(sc).unwrap());
        }
        t
    };

    let mut profiles = TrackerProfiles::default();
    let auto = scopes_for(&profiles, "example.org");
    assert_eq!(auto, [StatsScope::domain("example.org")]);
    let before = total(&auto);
    assert_eq!((before.downloaded, before.uploaded, before.torrents_completed), (6, 60, 2), "auto totals accumulate");

    // Save as favorite: the same history, plus anything recorded before domains existed.
    let id = profiles.add_favorite("", &["tracker.example.org".into(), "bt.example.org".into()]).unwrap();
    s.record_stats(&StatsScope::Profile(id.clone()), today.minus(9), &b(2, 20)).unwrap();
    s.flush_now().unwrap();
    let fav = scopes_for(&profiles, &id);
    assert_eq!(fav, [StatsScope::Profile(id.clone()), StatsScope::domain("example.org")], "one domain counts once");
    let t = total(&fav);
    assert_eq!((t.downloaded, t.uploaded), (6 + 2, 60 + 20));

    // New traffic after saving still lands under the domain, and shows in the favorite.
    s.record_stats(&StatsScope::domain("example.org"), today, &b(4, 40)).unwrap();
    s.flush_now().unwrap();
    assert_eq!(total(&fav).downloaded, 6 + 2 + 4);

    // Unfavorite: the history stays under the domain.
    profiles.remove(&id);
    let after = total(&scopes_for(&profiles, "example.org"));
    assert_eq!((after.downloaded, after.uploaded), (6 + 4, 60 + 40));
    assert_eq!(total(&[StatsScope::domain("other.org")]).downloaded, 100, "other trackers untouched");
}

#[test]
fn stats_series_summary_and_scopes() {
    let dir = TempDir::new("stats");
    let s = open(dir.path()).store;
    let today = Day::from_ymd(2026, 10, 11).unwrap(); // Sunday
    let tl = StatsScope::Profile("tl".into());
    let b = |down, up| StatsBucket { downloaded: down, uploaded: up, torrents_added: 1, ..Default::default() };
    // today, yesterday (Saturday, same week), the Sunday before (previous week, same month), previous month.
    s.record_stats(&tl, today, &b(1, 10)).unwrap();
    s.record_stats(&tl, today, &b(1, 10)).unwrap();
    s.record_stats(&tl, today.minus(1), &b(2, 20)).unwrap();
    s.record_stats(&tl, today.minus(7), &b(4, 40)).unwrap();
    s.record_stats(&tl, Day::from_ymd(2026, 9, 30).unwrap(), &b(8, 80)).unwrap();
    s.record_stats(&StatsScope::Profile("other".into()), today, &b(100, 1000)).unwrap();
    s.record_stats(&StatsScope::Public, today.minus(2), &b(1000, 0)).unwrap();
    s.flush_now().unwrap();

    let sum = s.stats_summary(&tl, today).unwrap();
    assert_eq!((sum.today.downloaded, sum.today.uploaded), (2, 20));
    assert_eq!(sum.this_week.downloaded, 2 + 2, "Monday 5th .. Sunday 11th");
    assert_eq!(sum.this_month.downloaded, 2 + 2 + 4, "Oct 1 .. Oct 11");
    assert_eq!(sum.all_time.downloaded, 2 + 2 + 4 + 8);
    assert_eq!(sum.all_time.torrents_added, 5);
    assert_eq!(sum.all_time.ratio(), Some(10.0));

    let series = s.stats_series(&tl, today, 30).unwrap();
    assert_eq!(series.len(), 30);
    assert_eq!(series.last().unwrap().0, today);
    assert_eq!(series[0].0, today.minus(29));
    assert!(series.windows(2).all(|w| w[0].0.0 + 1 == w[1].0.0), "dense and ordered");
    assert_eq!(series.iter().map(|(_, b)| b.downloaded).sum::<u64>(), 2 + 2 + 4 + 8, "Sep 30 is within 30 days");
    assert_eq!(s.stats_series(&tl, today, 3).unwrap().iter().map(|(_, b)| b.downloaded).collect::<Vec<_>>(), [0, 2, 2]);
    assert!(s.stats_series(&tl, today, 0).unwrap().is_empty());

    let all = s.stats_series(&StatsScope::All, today, 30).unwrap();
    assert_eq!(all.iter().map(|(_, b)| b.downloaded).sum::<u64>(), 16 + 100 + 1000);
    assert_eq!(s.stats_total(&StatsScope::All).unwrap().downloaded, 16 + 100 + 1000);
    assert_eq!(s.stats_summary(&StatsScope::Public, today).unwrap().this_week.downloaded, 1000);
    let mut scopes = s.stats_scopes().unwrap();
    scopes.sort();
    assert_eq!(scopes, ["@public", "other", "tl"]);
    assert!(matches!(s.record_stats(&StatsScope::All, today, &b(1, 1)), Err(StateError::Invalid(_))));

    // Daily rows always add up to the lifetime total.
    let full = s.stats_series(&tl, today, 3000).unwrap();
    let mut sum_all = StatsBucket::default();
    for (_, b) in &full {
        sum_all.add(b);
    }
    assert_eq!(sum_all, s.stats_total(&tl).unwrap());
}

#[test]
fn stats_day_boundary_uses_the_supplied_offset() {
    let dir = TempDir::new("tz");
    let s = open(dir.path()).store;
    let scope = StatsScope::Public;
    // The same instant lands on different local days depending on the offset.
    let t = 1_791_761_400; // 2026-10-11 23:30 UTC
    s.record_stats(&scope, Day::from_unix(t, 2 * 3600), &StatsBucket { uploaded: 1, ..Default::default() }).unwrap();
    s.record_stats(&scope, Day::from_unix(t, -5 * 3600), &StatsBucket { uploaded: 10, ..Default::default() }).unwrap();
    s.flush_now().unwrap();
    let d11 = Day::from_ymd(2026, 10, 11).unwrap();
    let series = s.stats_series(&scope, Day(d11.0 + 1), 2).unwrap();
    assert_eq!(series.iter().map(|(d, b)| (d.iso(), b.uploaded)).collect::<Vec<_>>(), [("2026-10-11".into(), 10), ("2026-10-12".into(), 1)]);
}

#[test]
fn timing_store_is_durable_without_a_flush() {
    let dir = TempDir::new("timing");
    let key = TimingKey { info_hash: id(9), tracker_url: "https://t.org/announce?passkey=SECRET".into() };
    let rec = TimingRecord {
        last_announce_at: Some(Millis(1_791_000_000_000)),
        next_allowed_at: Some(Millis(1_791_000_900_000)),
        min_allowed_at: None,
        completed_sent: true,
        completion_owed: false,
        tracker_id: Some(b"tid".to_vec().into_boxed_slice()),
        session_open: true,
    };
    {
        let s = open(dir.path()).store;
        let mut h = s.timing_handle();
        assert_eq!(h.load(&key), None);
        h.save(&key, &rec).unwrap();
        // Simulate a crash right after `save` returned: leak the store so Drop never flushes.
        std::mem::forget(s);
        // (the leaked handle keeps the file locked in this process, so read through the same handle)
        assert_eq!(h.load(&key), Some(rec.clone()));
    }
}

#[test]
fn timing_survives_reopen_and_closed_store_fails_safely() {
    let dir = TempDir::new("timing2");
    let key = TimingKey { info_hash: id(9), tracker_url: "udp://t.org:6969/announce".into() };
    let rec = TimingRecord { completed_sent: true, ..Default::default() };
    let mut h = {
        let s = open(dir.path()).store;
        let mut h = s.timing_handle();
        h.save(&key, &rec).unwrap();
        h
    };
    // Store dropped: the handle must not resurrect it, and `load` must fail towards "completed already sent".
    assert!(h.save(&key, &rec).is_err());
    assert_eq!(h.load(&key), Some(TimingRecord { completed_sent: true, ..Default::default() }));
    let s = open(dir.path()).store;
    assert_eq!(s.timing_handle().load(&key), Some(rec));
    let long = TimingKey { info_hash: id(9), tracker_url: "x".repeat(5000) };
    assert!(s.timing_handle().save(&long, &TimingRecord::default()).is_err());
}

#[test]
fn undecodable_records_are_reported_not_deleted() {
    let dir = TempDir::new("badrecord");
    {
        let s = open(dir.path()).store;
        s.put_torrent(id(1), &record(1)).unwrap();
        s.put_torrent(id(2), &record(2)).unwrap();
        s.flush_now().unwrap();
    }
    // Overwrite one record with garbage through a raw redb handle.
    {
        let db = redb::Database::create(dir.path().join("state.redb")).unwrap();
        let w = db.begin_write().unwrap();
        {
            let t: redb::TableDefinition<[u8; 20], &[u8]> = redb::TableDefinition::new("torrents");
            let mut t = w.open_table(t).unwrap();
            t.insert(id(2), [9u8, 9, 9].as_slice()).unwrap();
        }
        w.commit().unwrap();
    }
    let s = open(dir.path()).store;
    let l = s.load_torrents().unwrap();
    assert_eq!(l.records.len(), 1);
    assert_eq!(l.skipped.len(), 1);
    assert_eq!(l.skipped[0].0, id(2));
    assert!(s.get_torrent(id(2)).is_err());
}

#[test]
fn a_second_open_of_the_same_directory_fails_without_touching_it() {
    let dir = TempDir::new("twice");
    let _first = open(dir.path());
    let err = Store::open_with(dir.path(), &fast()).err().expect("second open must fail");
    assert!(matches!(err, StateError::Db(_)), "{err}");
    assert!(dir.path().join("state.redb").exists());
    let leftovers = std::fs::read_dir(dir.path()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().contains("corrupt")).count();
    assert_eq!(leftovers, 0, "a locked database is not a corrupt one");
}

// ---------------------------------------------------------------- schema

fn migrate_1_to_2(w: &redb::WriteTransaction) -> Result<(), StateError> {
    let def: redb::TableDefinition<&str, &[u8]> = redb::TableDefinition::new("dht_table");
    let mut t = w.open_table(def).map_err(redb::Error::from)?;
    t.insert("migrated", b"yes".as_slice()).map_err(redb::Error::from)?;
    Ok(())
}

fn failing(_: &redb::WriteTransaction) -> Result<(), StateError> {
    Err(StateError::Invalid("boom"))
}

static V2: Schema = Schema { current: 2, migrations: &[Migration { from: 1, to: 2, name: "add marker", run: migrate_1_to_2 }] };
static V2_BAD: Schema = Schema { current: 2, migrations: &[Migration { from: 1, to: 2, name: "always fails", run: failing }] };
static V3_NO_PATH: Schema = Schema { current: 3, migrations: &[Migration { from: 2, to: 3, name: "only 2 to 3", run: migrate_1_to_2 }] };
static V1: Schema = Schema { current: 1, migrations: &[] };

fn with_schema(schema: &'static Schema) -> OpenOptions {
    OpenOptions { schema, ..fast() }
}

#[test]
fn migration_backs_up_first_then_upgrades_atomically() {
    let dir = TempDir::new("migrate");
    {
        let s = Store::open_with(dir.path(), &with_schema(&V1)).unwrap().store;
        s.put_torrent(id(1), &record(1)).unwrap();
        s.flush_now().unwrap();
    }
    let o = Store::open_with(dir.path(), &with_schema(&V2)).unwrap();
    let m = o.report.migrated.clone().expect("migrated");
    assert_eq!((m.from, m.to, o.report.schema_version), (1, 2, 2));
    // The backup is a complete, openable v1 database holding the pre-migration data.
    let probe = TempDir::new("migrate-backup");
    std::fs::copy(&m.backup, probe.path().join("state.redb")).unwrap();
    let old = Store::open_with(probe.path(), &with_schema(&V1)).unwrap();
    assert!(old.report.migrated.is_none() && old.report.recovered.is_none());
    assert_eq!(old.store.load_torrents().unwrap().records.len(), 1);
    assert_eq!(old.store.get_dht_table().unwrap(), None, "the marker row was added after the backup");
    assert_eq!(o.store.get_dht_table().unwrap(), None);
    assert_eq!(o.store.load_torrents().unwrap().records.len(), 1);
    drop(o);
    // Reopening at the same version does nothing.
    let o = Store::open_with(dir.path(), &with_schema(&V2)).unwrap();
    assert!(o.report.migrated.is_none());
    assert_eq!(std::fs::read_dir(dir.path().join("backups")).unwrap().count(), 1);
}

#[test]
fn newer_schema_is_refused_and_the_file_is_untouched() {
    let dir = TempDir::new("newer");
    {
        let s = Store::open_with(dir.path(), &with_schema(&V2)).unwrap().store;
        s.put_torrent(id(1), &record(1)).unwrap();
        s.flush_now().unwrap();
    }
    let size_before = std::fs::metadata(dir.path().join("state.redb")).unwrap().len();
    let err = Store::open_with(dir.path(), &with_schema(&V1)).err().unwrap();
    match &err {
        StateError::NewerSchema { found: 2, supported: 1, .. } => {}
        other => panic!("{other}"),
    }
    assert!(err.to_string().contains("not modified"), "{err}");
    assert_eq!(std::fs::metadata(dir.path().join("state.redb")).unwrap().len(), size_before);
    let moved = std::fs::read_dir(dir.path()).unwrap().flatten().any(|e| e.file_name().to_string_lossy().contains("corrupt"));
    assert!(!moved);
    // And the newer build still opens it.
    assert_eq!(Store::open_with(dir.path(), &with_schema(&V2)).unwrap().store.load_torrents().unwrap().records.len(), 1);
}

#[test]
fn failed_migration_leaves_the_database_as_it_was() {
    let dir = TempDir::new("badmig");
    {
        let s = Store::open_with(dir.path(), &with_schema(&V1)).unwrap().store;
        s.put_torrent(id(1), &record(1)).unwrap();
        s.flush_now().unwrap();
    }
    let err = Store::open_with(dir.path(), &with_schema(&V2_BAD)).err().unwrap();
    assert!(matches!(&err, StateError::Migration { name: "always fails", .. }), "{err}");
    let err = Store::open_with(dir.path(), &with_schema(&V3_NO_PATH)).err().unwrap();
    assert!(matches!(err, StateError::NoMigration { from: 1 }), "{err}");
    // Still a v1 database with its data.
    let o = Store::open_with(dir.path(), &with_schema(&V1)).unwrap();
    assert_eq!(o.report.schema_version, 1);
    assert_eq!(o.store.load_torrents().unwrap().records.len(), 1);
}

// ---------------------------------------------------------------- corruption

fn seed_db(dir: &std::path::Path) {
    let s = open(dir).store;
    for n in 0..50 {
        s.put_torrent(id(n), &record(n)).unwrap();
        s.put_resume(id(n), &resume(128)).unwrap();
    }
    s.flush_now().unwrap();
}

fn assert_recovered(dir: &std::path::Path, what: &str) {
    let o = Store::open_with(dir, &fast()).unwrap_or_else(|e| panic!("{what}: {e}"));
    let rec = o.report.recovered.clone().unwrap_or_else(|| panic!("{what}: expected a recovery"));
    assert!(rec.moved_to.exists(), "{what}: damaged file kept for inspection");
    assert!(o.report.created);
    assert_eq!(o.store.load_torrents().unwrap().records.len(), 0, "{what}: fresh database");
    // The fresh database works.
    o.store.put_torrent(id(1), &record(1)).unwrap();
    o.store.flush_now().unwrap();
    assert_eq!(o.store.load_torrents().unwrap().records.len(), 1);
}

#[test]
fn garbage_file_is_moved_aside_and_replaced() {
    let dir = TempDir::new("garbage");
    std::fs::write(dir.path().join("state.redb"), vec![0xA5u8; 100_000]).unwrap();
    assert_recovered(dir.path(), "garbage");
}

#[test]
fn text_file_is_moved_aside_and_replaced() {
    let dir = TempDir::new("text");
    std::fs::write(dir.path().join("state.redb"), b"this is not a database\n".repeat(500)).unwrap();
    assert_recovered(dir.path(), "text");
}

#[test]
fn truncated_file_never_panics_and_never_loses_the_original() {
    for frac in [0.05, 0.3, 0.5, 0.9, 0.999] {
        let dir = TempDir::new("trunc");
        seed_db(dir.path());
        let p = dir.path().join("state.redb");
        let bytes = std::fs::read(&p).unwrap();
        let cut = ((bytes.len() as f64) * frac) as usize;
        std::fs::write(&p, &bytes[..cut]).unwrap();
        match Store::open_with(dir.path(), &fast()) {
            Ok(o) => {
                // Either redb repaired it (fine) or we started fresh and kept the damaged file.
                if let Some(r) = &o.report.recovered {
                    assert_eq!(std::fs::metadata(&r.moved_to).unwrap().len() as usize, cut);
                }
                let _ = o.store.load_torrents();
            }
            Err(e) => panic!("truncation to {frac} must be survivable, got: {e}"),
        }
    }
}

#[test]
fn random_byte_damage_is_always_survivable() {
    // Deterministic xorshift so failures reproduce.
    let seed: u64 = std::env::var("STATE_DAMAGE_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0x9E37_79B9_7F4A_7C15);
    let rounds: u64 = std::env::var("STATE_DAMAGE_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let dir0 = TempDir::new("flip-src");
    seed_db(dir0.path());
    let original = std::fs::read(dir0.path().join("state.redb")).unwrap();
    for round in 0..rounds {
        let dir = TempDir::new("flip");
        let mut bytes = original.clone();
        for _ in 0..(1 + next() % 40) {
            let i = (next() as usize) % bytes.len();
            bytes[i] ^= (next() % 255 + 1) as u8;
        }
        std::fs::write(dir.path().join("state.redb"), &bytes).unwrap();
        match Store::open_with(dir.path(), &fast()) {
            Ok(o) => {
                // Opens (possibly after recovery); reading must not panic whatever is inside.
                let _ = o.store.load_torrents();
                let _ = o.store.stats_scopes();
                let _ = o.store.load_counters();
                o.store.put_torrent(id(1), &record(1)).ok();
                let _ = o.store.flush_now();
            }
            Err(e) => {
                // Failing to open is acceptable only if nothing was destroyed.
                assert!(dir.path().join("state.redb").exists(), "round {round}: {e}");
            }
        }
    }
}

#[test]
fn empty_file_is_just_a_new_database() {
    let dir = TempDir::new("empty");
    std::fs::write(dir.path().join("state.redb"), b"").unwrap();
    let o = open(dir.path());
    assert!(o.report.created && o.report.recovered.is_none());
}

#[test]
fn integrity_check_passes_on_a_healthy_file() {
    let dir = TempDir::new("integrity");
    seed_db(dir.path());
    assert!(Store::check_file(dir.path().join("state.redb")).unwrap());
}
