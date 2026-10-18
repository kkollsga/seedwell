//! The worker pools exist only while there is work: an idle engine owns no threads, a burst ramps
//! up to the configured size, and everything still completes across the exits and restarts.

use crate::common;

use std::time::{Duration, Instant};

use common::*;
use trs_storage::{BLOCK_SIZE, DiskConfig, DiskIo, Mode, PieceOutcome, TorrentConfig, WriteAck};

fn disk(idle_exit: Duration) -> DiskIo {
    DiskIo::new(DiskConfig {
        io_threads: 3,
        hash_threads: 3,
        maintenance_threads: 2,
        fd_cache_capacity: 32,
        idle_exit,
        ..DiskConfig::default()
    })
}

fn wait_for(what: &str, limit: Duration, mut ok: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !ok() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn download(io: &DiskIo, s: &Synth, dir: &std::path::Path) -> trs_storage::TorrentHandle {
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir));
    h.attach_hashes(s.hashes.clone()).unwrap();
    let mut pending = Vec::new();
    for p in 0..s.layout.piece_count() {
        for (begin, data) in s.blocks(p) {
            if let WriteAck::Complete(f) = h.write_block(p, begin, &data).unwrap() {
                pending.push(f);
            }
        }
    }
    for f in pending {
        assert!(matches!(block_on(f).unwrap(), PieceOutcome::Verified(_)));
    }
    h
}

#[test]
fn an_idle_engine_has_no_worker_threads_and_work_brings_them_back() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(65_536, &[(&["a.bin"][..], 2_000_000, false)], 21);
    let io = disk(Duration::from_millis(150));
    assert_eq!(io.worker_threads(), 0, "nothing starts before the first job");

    let h = download(&io, &s, dir.path());
    assert!(io.worker_threads() >= 1, "a job starts a worker");
    assert!(io.worker_threads() <= 3 + 3 + 2, "never above the configured pool sizes");

    // Idle: every worker exits by itself.
    wait_for("idle exit", Duration::from_secs(5), || io.worker_threads() == 0);

    // The next jobs start workers again and complete.
    h.set_mode(Mode::Seed);
    let mut buf = Vec::new();
    for p in 0..s.layout.piece_count() {
        let size = s.layout.piece_size(p) as usize;
        let mut off = 0;
        while off < size {
            let n = (size - off).min(BLOCK_SIZE as usize);
            buf = h.read_block(p, off as u32, n, buf).unwrap().wait().unwrap();
            assert_eq!(&buf[..], &s.piece(p)[off..off + n]);
            off += n;
        }
    }
    wait_for("idle exit after reads", Duration::from_secs(5), || io.worker_threads() == 0);
}

#[test]
fn workers_that_exit_and_restart_in_a_tight_loop_lose_no_job() {
    // idle_exit of 1 ms: workers retire all the time, racing the submissions.
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(65_536, &[(&["a.bin"][..], 1_000_000, false)], 22);
    let io = disk(Duration::from_millis(1));
    let h = download(&io, &s, dir.path());
    h.set_mode(Mode::Seed);
    for round in 0..200 {
        let p = (round % s.layout.piece_count() as usize) as u32;
        let n = (s.layout.piece_size(p) as usize).min(BLOCK_SIZE as usize);
        let b = h.read_block(p, 0, n, Vec::new()).unwrap().wait().unwrap();
        assert_eq!(&b[..], &s.piece(p)[..n]);
        if round % 7 == 0 {
            std::thread::sleep(Duration::from_millis(3));
        }
    }
}

#[test]
fn dropping_joins_the_running_workers_and_an_unused_pool_drops_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(65_536, &[(&["a.bin"][..], 1_500_000, false)], 23);
    // Long idle time: the workers are still alive when the pool is dropped.
    let io = disk(Duration::from_secs(60));
    let _h = download(&io, &s, dir.path());
    assert!(io.worker_threads() >= 1);
    let t0 = Instant::now();
    drop(io);
    assert!(t0.elapsed() < Duration::from_secs(5), "drop joined the workers promptly");
    // Never used: nothing to stop.
    let t0 = Instant::now();
    drop(disk(Duration::from_secs(60)));
    assert!(t0.elapsed() < Duration::from_millis(500));
}
