//! Recheck, scrub, resume validation, missing data, fd cache, Cold-tier layout drop.

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::*;
use trs_storage::{
    CancelToken, DiskConfig, DiskIo, FileStatus, Mode, RecheckOptions, StorageError, TorrentConfig,
};

fn disk() -> DiskIo {
    DiskIo::new(DiskConfig {
        io_threads: 3,
        hash_threads: 2,
        ..DiskConfig::default()
    })
}

fn multi_files() -> Vec<(&'static [&'static str], u64, bool)> {
    vec![
        (&["t", "a.bin"][..], 300_000, false),
        (&[".pad", "0"][..], 4_288, true),
        (&["t", "z"][..], 0, false),
        (&["t", "d", "b.bin"][..], 777_777, false),
        (&["t", "c.bin"][..], 1, false),
    ]
}

#[test]
fn recheck_generated_multifile_torrent() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(32_768, &multi_files(), 42);
    s.write_all_files(dir.path());
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(dir.path()));

    let progress_calls = Arc::new(AtomicU32::new(0));
    let last_done = Arc::new(AtomicU32::new(0));
    let (pc, ld) = (progress_calls.clone(), last_done.clone());
    let mut opts = RecheckOptions::new(s.hashes.clone());
    opts.workers = 3;
    opts.progress = Some(Arc::new(move |p| {
        pc.fetch_add(1, Ordering::Relaxed);
        ld.fetch_max(p.pieces_done, Ordering::Relaxed);
        assert_eq!(p.pieces_total, 34);
    }));
    let r = h.recheck(opts).unwrap().wait().unwrap();
    let n = s.layout.piece_count();
    assert_eq!(n, 34);
    assert_eq!(r.valid, n);
    assert_eq!(r.have.count_ones(), n);
    assert_eq!(r.bytes_read, s.layout.total_length());
    assert!(r.missing_files.is_empty());
    assert!(progress_calls.load(Ordering::Relaxed) >= 1);
    assert_eq!(last_done.load(Ordering::Relaxed), n);

    // Corrupt one byte in b.bin: exactly the covering piece fails.
    let b_path = path_of(dir.path(), &s, 3);
    let mut data = std::fs::read(&b_path).unwrap();
    let pos = 500_000usize;
    data[pos] ^= 0x55;
    std::fs::write(&b_path, &data).unwrap();
    let bad_piece = ((s.layout.file_start(3) as usize + pos) / 32_768) as u32;
    let r = h
        .recheck(RecheckOptions::new(s.hashes.clone()))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(r.invalid, 1);
    assert!(!r.have.get(bad_piece));
    assert_eq!(r.valid, n - 1);

    // Delete a.bin: pieces needing it are skipped, the file is reported, nothing is recreated.
    std::fs::remove_file(path_of(dir.path(), &s, 0)).unwrap();
    let r = h
        .recheck(RecheckOptions::new(s.hashes.clone()))
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(r.missing_files, vec![0]);
    assert!(r.skipped_missing >= 10);
    assert!(!path_of(dir.path(), &s, 0).exists());

    // Truncated file counts as invalid pieces, not an error.
    std::fs::write(path_of(dir.path(), &s, 0), &s.file_bytes(0)[..1000]).unwrap();
    let r = h
        .recheck(RecheckOptions::new(s.hashes.clone()))
        .unwrap()
        .wait()
        .unwrap();
    assert!(r.invalid >= 9 && r.missing_files.is_empty());
}

#[test]
fn recheck_is_cancellable_and_scrub_is_throttled() {
    let dir = tempfile::tempdir().unwrap();
    let spec: Vec<(&[&str], u64, bool)> = vec![(&["big"][..], 4 << 20, false)];
    let s = synth_torrent(262_144, &spec, 11);
    s.write_all_files(dir.path());
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(dir.path()));

    // Scrub 4 MiB at 8 MiB/s must take >= ~0.4 s (smooth pacing), unthrottled recheck is fast.
    let mut o = RecheckOptions::new(s.hashes.clone());
    o.throttle_bytes_per_sec = Some(8 << 20);
    let t0 = Instant::now();
    let r = h.scrub(o).unwrap().wait().unwrap();
    let took = t0.elapsed();
    assert_eq!(r.valid, 16);
    assert!(
        took >= Duration::from_millis(350),
        "scrub too fast: {took:?}"
    );
    let t0 = Instant::now();
    h.recheck(RecheckOptions::new(s.hashes.clone()))
        .unwrap()
        .wait()
        .unwrap();
    assert!(t0.elapsed() < took);

    // Cancel mid-way.
    let cancel = CancelToken::new();
    let mut o = RecheckOptions::new(s.hashes.clone());
    o.throttle_bytes_per_sec = Some(1 << 20); // would take 4 s
    o.cancel = cancel.clone();
    let c2 = cancel.clone();
    o.progress = Some(Arc::new(move |_| c2.cancel()));
    let t0 = Instant::now();
    let r = h.scrub(o).unwrap().wait();
    assert!(matches!(r, Err(StorageError::Cancelled)));
    assert!(t0.elapsed() < Duration::from_secs(2));
}

#[test]
fn missing_data_never_recreates_files_when_seeding() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(32_768, &multi_files(), 5);
    s.write_all_files(dir.path());
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(dir.path()));
    let read = |p: u32| h.read_block(p, 0, 1000, Vec::new()).unwrap().wait();

    assert!(read(0).is_ok());
    // Missing file.
    std::fs::remove_file(path_of(dir.path(), &s, 0)).unwrap();
    // A cached fd of an unlinked file stays readable on POSIX; detecting deletion is the resume
    // check's job (see `resume_fingerprints_detect_changes`). Once the fd is dropped from the
    // cache (mode change, eviction, remount), the typed error appears.
    h.set_mode(Mode::Seed);
    assert!(matches!(
        read(0),
        Err(StorageError::MissingData { file: 0 })
    ));
    assert!(
        !path_of(dir.path(), &s, 0).exists(),
        "must not re-create the file"
    );

    // Missing directory (whole subtree gone).
    std::fs::remove_dir_all(dir.path().join("t").join("d")).unwrap();
    let first_b_piece = (s.layout.file_start(3) / 32_768) as u32 + 1;
    assert!(matches!(
        read(first_b_piece),
        Err(StorageError::MissingData { file: 3 })
    ));
    assert!(!dir.path().join("t").join("d").exists());

    // Entire root gone (unplugged drive).
    let gone = dir.path().join("vol");
    let h2 = io.add_torrent(s.layout.clone(), TorrentConfig::seed(&gone));
    assert!(matches!(
        h2.read_block(0, 0, 10, Vec::new()).unwrap().wait(),
        Err(StorageError::MissingData { .. })
    ));
    assert!(!gone.exists());

    // Truncated file.
    std::fs::write(path_of(dir.path(), &s, 4), b"").unwrap();
    let last = s.layout.piece_count() - 1;
    let r = h
        .read_block(last, s.layout.piece_size(last) - 1, 1, Vec::new())
        .unwrap()
        .wait();
    assert!(
        matches!(r, Err(StorageError::Truncated { file: 4 })),
        "{r:?}"
    );

    // Writing while seeding is refused.
    assert!(matches!(
        h.write_block(0, 0, &[0; 100]),
        Err(StorageError::ReadOnly | StorageError::NoHashes)
    ));
}

#[test]
fn downloading_into_vanished_root_fails_instead_of_recreating() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("Downloads");
    let s = synth_torrent(32_768, &multi_files(), 6);
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(&root));
    h.attach_hashes(s.hashes.clone()).unwrap();

    // Root does not exist: write must fail (reported via the piece future), not create it.
    let mut res = None;
    for (b, d) in s.blocks(0) {
        if let trs_storage::WriteAck::Complete(f) = h.write_block(0, b, &d).unwrap() {
            res = Some(f.wait());
        }
    }
    assert!(matches!(res.unwrap(), Err(StorageError::MissingRoot)));
    assert!(!root.exists());

    // ensure_root creates it (explicit, once), and a nonexistent volume is refused.
    h.ensure_root().unwrap();
    assert!(root.is_dir());
    let h2 = io.add_torrent(
        s.layout.clone(),
        TorrentConfig::download("/Volumes/__trs_unplugged__/dl"),
    );
    assert!(matches!(h2.ensure_root(), Err(StorageError::MissingRoot)));
    assert!(!std::path::Path::new("/Volumes/__trs_unplugged__").exists());
}

#[test]
fn fd_lru_stays_bounded_with_2000_files() {
    let dir = tempfile::tempdir().unwrap();
    let files: Vec<(Vec<String>, u64)> = (0..2000)
        .map(|i| (vec![format!("d{}", i % 20), format!("f{i}")], 128))
        .collect();
    // synth_torrent takes &str slices, so keep owned strings alive alongside.
    let comps: Vec<[String; 2]> = files
        .iter()
        .map(|(c, _)| [c[0].clone(), c[1].clone()])
        .collect();
    let refs: Vec<[&str; 2]> = comps
        .iter()
        .map(|c| [c[0].as_str(), c[1].as_str()])
        .collect();
    let spec: Vec<(&[&str], u64, bool)> = refs.iter().map(|r| (&r[..], 128u64, false)).collect();
    let s = synth_torrent(128, &spec, 77);
    s.write_all_files(dir.path());

    let io = DiskIo::new(DiskConfig {
        fd_cache_capacity: 64,
        io_threads: 4,
        ..DiskConfig::default()
    });
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(dir.path()));
    // Several passes from several threads.
    let h = Arc::new(h);
    let joins: Vec<_> = (0..4)
        .map(|k| {
            let h = h.clone();
            let s_hashes = s.stream.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                for pass in 0..2 {
                    for p in 0..2000u32 {
                        let p = (p * 7 + k * 13 + pass) % 2000;
                        buf = loop {
                            match h.read_block(p, 0, 128, std::mem::take(&mut buf)) {
                                Ok(f) => break f.wait().unwrap(),
                                Err(StorageError::Busy) => std::thread::yield_now(),
                                Err(e) => panic!("{e}"),
                            }
                        };
                        let o = p as usize * 128;
                        assert_eq!(&buf[..], &s_hashes[o..o + 128]);
                    }
                }
            })
        })
        .collect();
    for j in joins {
        j.join().unwrap();
    }
    let st = io.stats().fd;
    assert!(st.open <= 64, "{st:?}");
    assert!(st.evictions >= 2000 - 64, "{st:?}");
    assert!(st.misses >= 2000);
}

#[test]
fn resume_fingerprints_detect_changes() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(32_768, &multi_files(), 13);
    s.write_all_files(dir.path());
    // The empty file must exist for a complete torrent.
    std::fs::write(path_of(dir.path(), &s, 2), b"").unwrap();
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(dir.path()));
    let fps = h.fingerprints().unwrap().wait().unwrap();
    assert_eq!(fps.len(), 5);
    assert!(fps[1].is_none() && fps[0].is_some());
    let r = h.check_files(fps.clone()).unwrap().wait().unwrap();
    assert!(r.is_clean(), "{r:?}");

    // Same size, new mtime.
    let f = std::fs::OpenOptions::new()
        .write(true)
        .open(path_of(dir.path(), &s, 4))
        .unwrap();
    f.set_modified(std::time::SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    // Grown file.
    std::fs::OpenOptions::new()
        .append(true)
        .open(path_of(dir.path(), &s, 0))
        .unwrap()
        .set_len(300_001)
        .unwrap();
    // Deleted file.
    std::fs::remove_file(path_of(dir.path(), &s, 3)).unwrap();
    let r = h.check_files(fps.clone()).unwrap().wait().unwrap();
    assert_eq!(r.status[4], FileStatus::Modified);
    assert_eq!(r.status[0], FileStatus::SizeMismatch);
    assert_eq!(r.status[3], FileStatus::Missing);
    assert_eq!(r.status[1], FileStatus::Ok);
    assert!(!r.is_clean() && r.has_missing());
    let none = h.check_files(vec![None; 5]).unwrap().wait().unwrap();
    assert_eq!(none.status[4], FileStatus::NoRecord);
}

#[test]
fn cold_tier_layout_drop_and_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(32_768, &multi_files(), 1);
    s.write_all_files(dir.path());
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(dir.path()));
    h.attach_hashes(s.hashes.clone()).unwrap();
    let warm = h.resident_bytes();
    h.drop_layout().unwrap();
    let cold = h.resident_bytes();
    assert!(!h.has_layout() && cold < warm);
    assert!(matches!(
        h.read_block(0, 0, 10, Vec::new()),
        Err(StorageError::LayoutDropped)
    ));
    // A different layout is rejected, the identical one accepted.
    let other = synth_torrent(32_768, &multi_files()[..3], 1);
    assert!(matches!(
        h.rebuild_layout(other.layout.clone()),
        Err(StorageError::LayoutMismatch)
    ));
    h.rebuild_layout(s.layout.clone()).unwrap();
    assert!(h.read_block(0, 0, 10, Vec::new()).unwrap().wait().is_ok());
    println!("resident: warm {warm} B (5 files), cold {cold} B");
}

#[test]
fn resident_cost_per_torrent_is_small() {
    let io = disk();
    let spec: Vec<(&[&str], u64, bool)> = vec![(&["movie.mkv"][..], 1 << 30, false)];
    let l = synth_layout_only(&spec);
    let h = io.add_torrent(l, TorrentConfig::seed("/Users/x/Media"));
    let warm = h.resident_bytes();
    h.drop_layout().unwrap();
    let cold = h.resident_bytes();
    println!("1-file torrent: warm {warm} B, cold {cold} B");
    assert!(warm < 1024 && cold < 512, "warm {warm}, cold {cold}");
}

fn synth_layout_only(files: &[(&[&str], u64, bool)]) -> Arc<trs_storage::Layout> {
    use trs_storage::{FilePath, FileSpec, Layout};
    let specs: Vec<_> = files
        .iter()
        .map(|(c, l, p)| FileSpec {
            path: FilePath::Relative(c.iter().map(|s| s.to_string()).collect()),
            length: *l,
            is_padding: *p,
        })
        .collect();
    let total: u64 = specs.iter().map(|f| f.length).sum();
    Arc::new(Layout::new(1 << 20, total, total.div_ceil(1 << 20) as u32, specs).unwrap())
}
