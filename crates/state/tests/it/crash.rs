//! Crash safety: a child process hammers the store and is SIGKILLed at random moments
//! (including while it is opening the database and while a commit is in flight). After every
//! kill the parent reopens the database and checks that
//!
//! * it opens without any recovery (a SIGKILL must never corrupt it) and passes the integrity check,
//! * everything acknowledged by `flush_now` / the durable timing path is there,
//! * counters never go backwards and cross-table invariants hold (they are written in one transaction).
//!
//! `CRASH_ITERS` sets the number of kills (default 40; the README records a run of 1000).
//! The child is this same test binary re-run with `TRS_CRASH_CHILD_DIR` set.

use crate::common;

use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TempDir, id};
use trs_state::{Counters, Day, OpenOptions, StatsBucket, StatsScope, Store, TorrentRecord};
use trs_tracker::store::{TimingKey, TimingRecord, TimingStore};

const CHILD_ENV: &str = "TRS_CRASH_CHILD_DIR";

fn timing_key(k: u64) -> TimingKey {
    TimingKey { info_hash: id(100_000 + (k % 997) as u32), tracker_url: format!("https://t{}.example.org/announce?passkey=SECRET{k}", k % 3) }
}

/// The workload. Runs until killed.
#[test]
fn crash_child_entry() {
    let Ok(dir) = std::env::var(CHILD_ENV) else { return };
    let opts = OpenOptions { debounce: Duration::from_millis(15), ..OpenOptions::default() };
    let opened = Store::open_with(&dir, &opts).expect("child: open");
    let store = opened.store;
    let mut timing = store.timing_handle();
    let mut k = store.get_counters(id(1)).expect("child: counters").uploaded;
    loop {
        k += 1;
        // One logical event touches counters and statistics: they must always agree.
        store.add_counters(id(1), Counters { downloaded: 2, uploaded: 1, seed_secs: 1 }).unwrap();
        store
            .record_stats(&StatsScope::Profile("p".into()), Day(200 + (k % 5) as u32), &StatsBucket { downloaded: 2, uploaded: 1, torrents_added: 1, ..Default::default() })
            .unwrap();
        if k.is_multiple_of(7) {
            let mut r = TorrentRecord::new(format!("torrent {k}"), k, k as i64);
            r.piece_count = 8;
            store.put_torrent(id(1000 + (k % 50) as u32), &r).unwrap();
        }
        if k.is_multiple_of(11) {
            let rec = TimingRecord { completed_sent: true, session_open: k.is_multiple_of(2), ..Default::default() };
            if timing.save(&timing_key(k), &rec).is_ok() {
                println!("timing {k}");
            }
        }
        if k.is_multiple_of(13) && store.flush_now().is_ok() {
            println!("flushed {k}");
        }
        if k.is_multiple_of(5) {
            std::thread::sleep(Duration::from_micros(300));
        }
    }
}

struct Acks {
    flushed: u64,
    timing: HashSet<u64>,
}

fn rng(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn survives_sigkill_at_random_moments() {
    if std::env::var(CHILD_ENV).is_ok() {
        return; // we are the child's harness: only crash_child_entry runs
    }
    let iters: u64 = std::env::var("CRASH_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(40);
    let dir = TempDir::new("crash");
    let exe = std::env::current_exe().unwrap();
    let mut seed = std::env::var("CRASH_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(0xC0FF_EE12_3456_789Bu64) | 1;
    let mut prev_uploaded = 0u64;
    let mut all_timing: HashSet<u64> = HashSet::new();
    let mut flushed_floor = 0u64;
    let (mut mid_open, mut opened_runs) = (0u32, 0u32);

    for iter in 0..iters {
        let mut child = Command::new(&exe)
            .args(["--exact", "crash::crash_child_entry", "--nocapture", "--test-threads=1"])
            .env(CHILD_ENV, dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");
        let acks = Arc::new(Mutex::new(Acks { flushed: 0, timing: HashSet::new() }));
        let reader = {
            let acks = Arc::clone(&acks);
            let out = child.stdout.take().unwrap();
            std::thread::spawn(move || {
                for line in BufReader::new(out).lines().map_while(Result::ok) {
                    let mut parts = line.split_whitespace();
                    let (Some(kind), Some(Ok(n))) = (parts.next(), parts.next().map(str::parse::<u64>)) else { continue };
                    let mut a = acks.lock().unwrap();
                    match kind {
                        "flushed" => a.flushed = a.flushed.max(n),
                        "timing" => {
                            a.timing.insert(n);
                        }
                        _ => {}
                    }
                }
            })
        };
        // Random lifetime: from "dies while opening" to "ran several flush cycles".
        let ms = 5 + rng(&mut seed) % 400;
        std::thread::sleep(Duration::from_millis(ms));
        child.kill().expect("SIGKILL");
        child.wait().unwrap();
        reader.join().unwrap();
        let acks = acks.lock().unwrap();
        flushed_floor = flushed_floor.max(acks.flushed);
        all_timing.extend(acks.timing.iter().copied());
        if acks.flushed == 0 && acks.timing.is_empty() {
            mid_open += 1;
        } else {
            opened_runs += 1;
        }

        // ---- reopen and verify
        let opened = Store::open(dir.path()).unwrap_or_else(|e| panic!("iter {iter}: reopen failed: {e}"));
        assert!(opened.report.recovered.is_none(), "iter {iter}: SIGKILL corrupted the database: {:?}", opened.report.recovered);
        assert!(opened.report.migrated.is_none());
        let s = opened.store;
        let c = s.get_counters(id(1)).unwrap();
        assert_eq!(c.downloaded, 2 * c.uploaded, "iter {iter}: torn counters {c:?}");
        assert_eq!(c.seed_secs, c.uploaded, "iter {iter}: torn counters {c:?}");
        assert!(c.uploaded >= prev_uploaded, "iter {iter}: counter went backwards {} -> {}", prev_uploaded, c.uploaded);
        assert!(c.uploaded >= flushed_floor, "iter {iter}: acknowledged flush {flushed_floor} lost, have {}", c.uploaded);
        prev_uploaded = c.uploaded;

        let scope = StatsScope::Profile("p".into());
        let total = s.stats_total(&scope).unwrap();
        assert_eq!(total.uploaded, c.uploaded, "iter {iter}: stats and counters were committed together");
        assert_eq!(total.downloaded, 2 * total.uploaded);
        let series = s.stats_series(&scope, Day(210), 100).unwrap();
        let mut sum = StatsBucket::default();
        series.iter().for_each(|(_, b)| sum.add(b));
        assert_eq!(sum, total, "iter {iter}: daily rows and lifetime total disagree");

        let loaded = s.load_torrents().unwrap();
        assert!(loaded.skipped.is_empty(), "iter {iter}: undecodable records {:?}", loaded.skipped);

        let h = s.timing_handle();
        for k in &all_timing {
            let rec = h.try_load(&timing_key(*k)).unwrap().unwrap_or_else(|| panic!("iter {iter}: durable timing write {k} lost"));
            assert!(rec.completed_sent, "iter {iter}: completed_sent lost for {k}");
        }
        drop(s);
        if iter % 10 == 9 {
            assert!(Store::check_file(dir.path().join("state.redb")).unwrap(), "iter {iter}: integrity check failed");
        }
    }
    eprintln!(
        "crash test: {iters} SIGKILLs ({opened_runs} after the child was running, {mid_open} before its first ack), final lifetime counter {prev_uploaded}, last acknowledged flush {flushed_floor}, {} durable timing writes verified",
        all_timing.len()
    );
    assert!(prev_uploaded > 0, "the child never made progress; the test proved nothing");
}
