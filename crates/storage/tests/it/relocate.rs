//! Relocation (SPEC §7A.5): same-volume renames, verified cross-volume copies, crash recovery,
//! cancellation, space checks, and a real second volume (a temporary APFS disk image).
//!
//! Every test works in its own temp directory; nothing here reads or writes the real home.

use crate::common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use common::*;
use trs_storage::{
    CancelToken, DiskConfig, DiskIo, FilePath, Location, Mode, PieceSet, RecoverState,
    RelocateMethod, RelocateOptions, StorageError, TorrentConfig, TorrentHandle, WriteAck,
};

/// `wait` with a deadline: a lost reply fails the test in a minute instead of hanging the run.
trait WaitFast<T> {
    fn wait_fast(self) -> Result<T, StorageError>;
}

impl<T> WaitFast<T> for trs_storage::Pending<T> {
    fn wait_fast(self) -> Result<T, StorageError> {
        self.wait_timeout(Duration::from_secs(60))
            .expect("relocate reply never arrived (hung job)")
    }
}

fn disk() -> DiskIo {
    DiskIo::new(DiskConfig {
        io_threads: 2,
        hash_threads: 2,
        ..DiskConfig::default()
    })
}

/// Odd piece size, a padding file in the middle, an empty file, a 1-byte file, files that
/// straddle piece boundaries and are larger than the smallest copy buffer.
fn files() -> Vec<(&'static [&'static str], u64, bool)> {
    vec![
        (&["T", "a.bin"][..], 100_003, false),
        (&["T", ".pad", "1"][..], 29_997, true),
        (&["T", "sub", "b.bin"][..], 65_537, false),
        (&["T", "empty.txt"][..], 0, false),
        (&["T", "c.bin"][..], 1, false),
        (&["T", "d.bin"][..], 300_001, false),
    ]
}

const PIECE: u32 = 40_000;

struct Fx {
    _dir: tempfile::TempDir,
    base: PathBuf,
    src: PathBuf,
    dst: PathBuf,
    s: Synth,
}

fn fx() -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let src = base.join("src");
    let dst = base.join("lib").join("new");
    let s = synth_torrent(PIECE, &files(), 5);
    s.write_all_files(&src);
    Fx {
        _dir: dir,
        base,
        src,
        dst,
        s,
    }
}

impl Fx {
    fn handle(&self, io: &DiskIo) -> TorrentHandle {
        io.add_torrent(self.s.layout.clone(), TorrentConfig::seed(&self.src))
    }

    fn journal(&self) -> PathBuf {
        self.base.join("reloc.journal")
    }

    fn plan(&self, h: &TorrentHandle) -> Vec<Option<PathBuf>> {
        h.plan_paths(&self.dst, None, Location::Final).unwrap()
    }

    fn assert_original(&self) {
        for i in 0..self.s.layout.file_count() {
            if self.s.layout.file_is_padding(i) {
                continue;
            }
            let p = self.src.join(&self.s.names[i]);
            assert_eq!(std::fs::read(&p).unwrap(), self.s.file_bytes(i), "{p:?}");
        }
        self.assert_nothing_at_dst();
        assert!(!self.journal().exists(), "journal left behind");
    }

    fn assert_nothing_at_dst(&self) {
        assert!(!self.dst.exists(), "destination leftovers: {:?}", walk(&self.base.join("lib")));
        assert!(!self.base.join("lib").exists() || walk(&self.base.join("lib")).is_empty());
        let parts: Vec<_> = walk(&self.base)
            .into_iter()
            .filter(|p| p.to_string_lossy().ends_with(".trs-part"))
            .collect();
        assert!(parts.is_empty(), "{parts:?}");
    }

    fn assert_moved(&self) {
        for i in 0..self.s.layout.file_count() {
            if self.s.layout.file_is_padding(i) {
                continue;
            }
            let p = self.dst.join(&self.s.names[i]);
            assert_eq!(std::fs::read(&p).unwrap(), self.s.file_bytes(i), "{p:?}");
            assert!(!self.src.join(&self.s.names[i]).exists());
        }
        assert!(!self.journal().exists());
        let parts: Vec<_> = walk(&self.base)
            .into_iter()
            .filter(|p| p.to_string_lossy().ends_with(".trs-part"))
            .collect();
        assert!(parts.is_empty(), "{parts:?}");
    }
}

fn walk(p: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                out.extend(walk(&path));
            }
            out.push(path);
        }
    }
    out
}

fn opts(s: &Synth) -> RelocateOptions {
    RelocateOptions {
        hashes: Some(s.hashes.clone()),
        buffer_bytes: 64 << 10,
        reserve_bytes: 0,
        ..RelocateOptions::default()
    }
}

/// Reads every piece of the torrent through the handle and compares with the source of truth.
fn read_all_ok(h: &TorrentHandle, s: &Synth) {
    for p in 0..s.layout.piece_count() {
        let sz = s.layout.piece_size(p) as usize;
        let mut got = Vec::new();
        let mut off = 0;
        while off < sz {
            let n = (sz - off).min(16_384);
            let b = h
                .read_block(p, off as u32, n, Vec::new())
                .unwrap()
                .wait_fast()
                .unwrap_or_else(|e| panic!("piece {p} off {off}: {e}"));
            got.extend_from_slice(&b);
            off += n;
        }
        assert_eq!(got, s.piece(p), "piece {p}");
    }
}

// ---------------------------------------------------------------------------------------------

#[test]
fn same_volume_rename_with_concurrent_reader() {
    rename_with_reader(false);
}

/// Like above, but another thread keeps dropping the torrent's cached descriptors, so reads
/// during the rename-to-switch window must re-open files and ride out the window.
#[test]
fn same_volume_rename_with_concurrent_reader_and_fd_churn() {
    rename_with_reader(true);
}

fn rename_with_reader(churn: bool) {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    read_all_ok(&h, &f.s); // warm the fd cache: some reads use cached fds, some reopen
    let churner = churn.then(|| {
        let (h, stop) = (h.clone(), Arc::new(AtomicBool::new(false)));
        let s2 = stop.clone();
        let t = std::thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                h.set_mode(Mode::Seed); // drops cached fds
                std::thread::sleep(Duration::from_micros(150));
            }
        });
        (t, stop)
    });

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU32::new(0));
    let reader = {
        let (h, stop, reads) = (h.clone(), stop.clone(), reads.clone());
        let stream = f.s.stream.clone();
        let layout = f.s.layout.clone();
        std::thread::spawn(move || {
            let mut rng = Rng(77);
            while !stop.load(Ordering::Relaxed) {
                let p = (rng.next() % u64::from(layout.piece_count())) as u32;
                let sz = layout.piece_size(p);
                let begin = (rng.next() % u64::from(sz.div_ceil(16_384))) as u32 * 16_384;
                let n = (sz - begin).min(16_384) as usize;
                let b = h
                    .read_block(p, begin, n, Vec::new())
                    .unwrap()
                    .wait_fast()
                    .unwrap_or_else(|e| panic!("read failed during relocation: {e}"));
                let o = layout.piece_offset(p) as usize + begin as usize;
                assert_eq!(&b[..], &stream[o..o + n]);
                reads.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    std::thread::sleep(Duration::from_millis(20));

    let plan = f.plan(&h);
    let rep = h
        .relocate_prepare(f.journal(), plan, opts(&f.s))
        .unwrap()
        .wait_fast()
        .unwrap();
    assert_eq!(rep.method, RelocateMethod::Renamed);
    assert_eq!(rep.files, 5);
    assert_eq!(rep.bytes, 0);
    // The engine's switch.
    h.set_paths(f.dst.clone(), None);
    h.relocate_mark_switched(&f.journal()).unwrap();
    h.relocate_commit(f.journal(), Some(f.src.clone()))
        .unwrap()
        .wait_fast()
        .unwrap();
    std::thread::sleep(Duration::from_millis(20));
    stop.store(true, Ordering::Relaxed);
    reader.join().unwrap();
    if let Some((t, stop)) = churner {
        stop.store(true, Ordering::Relaxed);
        t.join().unwrap();
    }
    assert!(reads.load(Ordering::Relaxed) > 10);

    f.assert_moved();
    // Emptied source directories were pruned, the stop_at directory itself was kept.
    assert!(f.src.exists());
    assert!(walk(&f.src).is_empty(), "{:?}", walk(&f.src));
    read_all_ok(&h, &f.s);
    h.set_mode(Mode::Seed);
    // Idempotent.
    h.relocate_commit(f.journal(), Some(f.src.clone()))
        .unwrap()
        .wait_fast()
        .unwrap();
    h.relocate_rollback(f.journal()).unwrap().wait_fast().unwrap();
}

#[test]
fn collision_never_overwrites_and_moves_nothing() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let plan = f.plan(&h);
    let victim = plan[2].clone().unwrap();
    std::fs::create_dir_all(victim.parent().unwrap()).unwrap();
    std::fs::write(&victim, b"precious").unwrap();
    let r = h.relocate_prepare(f.journal(), plan, opts(&f.s)).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::DestinationExists { file: 2 })), "{r:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
    for i in [0, 2, 4, 5] {
        assert!(f.src.join(&f.s.names[i]).exists());
    }
    assert!(!f.journal().exists());
    assert_eq!(walk(&f.dst).into_iter().filter(|p| p.is_file()).count(), 1, "only the foreign file may exist");
    // Same through the copy path.
    let mut o = opts(&f.s);
    o.force_copy = true;
    let r = h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::DestinationExists { file: 2 })), "{r:?}");
    assert_eq!(std::fs::read(&victim).unwrap(), b"precious");
}

#[test]
fn collision_that_appears_midway_rolls_renames_back() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let plan = f.plan(&h);
    let late = plan[5].clone().unwrap();
    let first_src = f.src.join(&f.s.names[0]);
    let created = Arc::new(AtomicBool::new(false));
    let mut o = opts(&f.s);
    let c2 = created.clone();
    o.fault = Some(Arc::new(move |_| {
        // After the first file moved, someone drops a file onto a later destination.
        if !first_src.exists() && !c2.swap(true, Ordering::Relaxed) {
            std::fs::create_dir_all(late.parent().unwrap()).unwrap();
            std::fs::write(&late, b"racer").unwrap();
        }
        true
    }));
    let r = h.relocate_prepare(f.journal(), plan, o).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::DestinationExists { file: 5 })), "{r:?}");
    for i in [0, 2, 4, 5] {
        let p = f.src.join(&f.s.names[i]);
        assert_eq!(std::fs::read(&p).unwrap(), f.s.file_bytes(i), "{p:?} must be back");
    }
    assert_eq!(
        std::fs::read(f.dst.join(&f.s.names[5])).unwrap(),
        b"racer",
        "the foreign file is never touched"
    );
    assert!(!f.journal().exists());
    // Only the racer's own file and its directories remain at the destination.
    let left: Vec<_> = walk(&f.dst).into_iter().filter(|p| p.is_file()).collect();
    assert_eq!(left.len(), 1, "{left:?}");
}

#[test]
fn forced_copy_verifies_across_file_and_piece_boundaries() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut o = opts(&f.s);
    o.force_copy = true;
    let prog = Arc::new(AtomicU32::new(0));
    let p2 = prog.clone();
    o.progress = Some(Arc::new(move |d, t| {
        assert!(d <= t);
        p2.fetch_add(1, Ordering::Relaxed);
    }));
    let rep = h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast().unwrap();
    assert_eq!(rep.method, RelocateMethod::Copied);
    assert_eq!(rep.verified_pieces, f.s.layout.piece_count());
    assert_eq!(rep.files, 5);
    assert_eq!(rep.bytes, 100_003 + 65_537 + 1 + 300_001);
    assert!(prog.load(Ordering::Relaxed) >= 1);
    // The source is untouched and still serves until the switch.
    for i in [0, 2, 4, 5] {
        assert_eq!(std::fs::read(f.src.join(&f.s.names[i])).unwrap(), f.s.file_bytes(i));
        assert_eq!(std::fs::read(f.dst.join(&f.s.names[i])).unwrap(), f.s.file_bytes(i));
    }
    assert_eq!(std::fs::metadata(f.dst.join("T/empty.txt")).unwrap().len(), 0);
    assert!(!f.dst.join("T/.pad").exists(), "padding is never written");
    read_all_ok(&h, &f.s);
    // mtime is kept so stored fingerprints survive.
    let a = std::fs::metadata(f.src.join("T/a.bin")).unwrap().modified().unwrap();
    let b = std::fs::metadata(f.dst.join("T/a.bin")).unwrap().modified().unwrap();
    assert_eq!(a, b);
    h.set_paths(f.dst.clone(), None);
    read_all_ok(&h, &f.s);
    h.relocate_mark_switched(&f.journal()).unwrap();
    // After the switch a rollback must refuse.
    let r = h.relocate_rollback(f.journal()).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::InvalidState)), "{r:?}");
    h.relocate_commit(f.journal(), Some(f.src.clone())).unwrap().wait_fast().unwrap();
    f.assert_moved();
    assert!(walk(&f.src).is_empty());
}

#[test]
fn copy_without_hashes_and_with_cache_on() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let o = RelocateOptions {
        force_copy: true,
        nocache: false,
        reserve_bytes: 0,
        ..RelocateOptions::default()
    };
    let rep = h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast().unwrap();
    assert_eq!(rep.verified_pieces, 0);
    h.set_paths(f.dst.clone(), None);
    h.relocate_commit(f.journal(), None).unwrap().wait_fast().unwrap();
    f.assert_moved();
}

#[test]
fn corrupted_source_is_detected_by_the_running_hash() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    // Flip one byte in b.bin; find its piece.
    let p = f.src.join("T/sub/b.bin");
    let mut b = std::fs::read(&p).unwrap();
    b[40_000] ^= 0xff;
    std::fs::write(&p, &b).unwrap();
    let start = f.s.layout.file_start(2) + 40_000;
    let want_piece = (start / u64::from(PIECE)) as u32;
    let mut o = opts(&f.s);
    o.force_copy = true;
    let r = h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast();
    match r {
        Err(StorageError::VerifyFailed { piece }) => assert_eq!(piece, want_piece),
        other => panic!("{other:?}"),
    }
    // Destination cleaned, source exactly as it was (corrupt byte and all).
    f.assert_nothing_at_dst();
    assert_eq!(std::fs::read(&p).unwrap(), b);
    assert_eq!(std::fs::read(f.src.join("T/a.bin")).unwrap(), f.s.file_bytes(0));
    assert!(!f.journal().exists());
}

#[test]
fn truncated_or_missing_source_is_refused() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let p = f.src.join("T/d.bin");
    let full = std::fs::read(&p).unwrap();
    std::fs::write(&p, &full[..1000]).unwrap();
    let r = h.relocate_prepare(f.journal(), f.plan(&h), opts(&f.s)).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::Truncated { file: 5 })), "{r:?}");
    std::fs::remove_file(&p).unwrap();
    let r = h.relocate_prepare(f.journal(), f.plan(&h), opts(&f.s)).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::MissingData { file: 5 })), "{r:?}");
    assert!(!f.dst.exists());
}

#[test]
fn cancel_mid_copy_cleans_parts_and_keeps_source() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut o = opts(&f.s);
    o.force_copy = true;
    o.bandwidth = 400_000; // ~1 s for the 466 kB of data: plenty of time to cancel
    let cancel = CancelToken::new();
    o.cancel = cancel.clone();
    let c2 = cancel.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(250));
        c2.cancel();
    });
    let t0 = Instant::now();
    let r = h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast();
    t.join().unwrap();
    assert!(matches!(r, Err(StorageError::Cancelled)), "{r:?}");
    assert!(t0.elapsed() < Duration::from_millis(900), "cancel must be prompt");
    f.assert_original();
}

#[test]
fn insufficient_space_is_refused_before_anything_is_written() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut o = opts(&f.s);
    o.force_copy = true;
    o.reserve_bytes = u64::MAX / 2; // more than any disk has
    let r = h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast();
    match r {
        Err(StorageError::InsufficientSpace { needed, available }) => assert!(needed > available),
        other => panic!("{other:?}"),
    }
    f.assert_original();
}

#[test]
fn bandwidth_limit_paces_the_copy() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut o = opts(&f.s);
    o.force_copy = true;
    o.bandwidth = 1_000_000; // 466 kB at 1 MB/s => about 0.45 s
    let t0 = Instant::now();
    h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast().unwrap();
    let el = t0.elapsed();
    assert!(el >= Duration::from_millis(300), "{el:?}");
    assert!(el < Duration::from_secs(5), "{el:?}");
}

#[test]
fn unchanged_destinations_and_dst_none_are_left_alone() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut plan = f.plan(&h);
    plan[0] = Some(f.src.join(&f.s.names[0])); // already there
    plan[5] = None;
    let mut o = opts(&f.s);
    o.force_copy = true; // pieces spanning moved and unmoved files still verify
    let rep = h.relocate_prepare(f.journal(), plan, o).unwrap().wait_fast().unwrap();
    assert_eq!(rep.unchanged, 1);
    assert_eq!(rep.files, 3);
    assert_eq!(rep.verified_pieces, f.s.layout.piece_count());
    assert!(f.src.join("T/a.bin").exists() && f.src.join("T/d.bin").exists());
    h.relocate_rollback(f.journal()).unwrap().wait_fast().unwrap();
    assert!(!f.dst.exists());
}

/// Regression: a writer that dies (fault, disk error) while the reader/hash stages have filled
/// the pipeline used to leave the reader waiting for a free buffer forever, so the job never
/// replied. A slow fault hook keeps the pipeline full when the writer fails.
#[test]
fn writer_death_with_full_pipeline_resolves() {
    for k in 0..40u32 {
        let f = fx();
        let io = disk();
        let h = f.handle(&io);
        let mut o = opts(&f.s);
        o.force_copy = true;
        o.fault = Some(Arc::new(move |step| {
            std::thread::sleep(Duration::from_millis(3));
            step < k
        }));
        match h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast() {
            Err(StorageError::Interrupted) | Ok(_) => {}
            Err(e) => panic!("k={k}: unexpected {e}"),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Crash at every step
// ---------------------------------------------------------------------------------------------

fn crash_every_step(force_copy: bool) -> u32 {
    let mut k = 0u32;
    loop {
        let f = fx();
        let io = disk();
        let h = f.handle(&io);
        let mut o = opts(&f.s);
        o.force_copy = force_copy;
        o.fault = Some(Arc::new(move |step| step < k));
        let first = h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast();
        match first {
            Err(StorageError::Interrupted) => {
                // "Process restart": fresh pools, same files, the old layout/paths.
                drop(h);
                drop(io);
                let io2 = disk();
                let h2 = f.handle(&io2);
                let rec = h2.relocate_recover(&f.journal()).unwrap();
                match rec.state {
                    RecoverState::Nothing => {
                        f.assert_original();
                    }
                    RecoverState::Incomplete => {
                        assert!(!rec.entries.is_empty());
                        h2.relocate_rollback(f.journal()).unwrap().wait_fast().unwrap();
                        f.assert_original();
                        h2.relocate_rollback(f.journal()).unwrap().wait_fast().unwrap(); // idempotent
                    }
                    RecoverState::Prepared => {
                        // All verified and in place; the engine decides from its own record.
                        if k.is_multiple_of(2) {
                            h2.set_paths(f.dst.clone(), None);
                            h2.relocate_commit(f.journal(), Some(f.src.clone()))
                                .unwrap()
                                .wait_fast()
                                .unwrap();
                            f.assert_moved();
                            read_all_ok(&h2, &f.s);
                        } else {
                            h2.relocate_rollback(f.journal()).unwrap().wait_fast().unwrap();
                            f.assert_original();
                        }
                    }
                    s => panic!("step {k}: unexpected {s:?}"),
                }
                // After recovery a fresh relocation must work.
                if !f.dst.exists() {
                    let mut o = opts(&f.s);
                    o.force_copy = force_copy;
                    h2.relocate_prepare(f.journal(), f.plan(&h2), o)
                        .unwrap()
                        .wait_fast()
                        .unwrap_or_else(|e| panic!("redo after crash at step {k}: {e}"));
                    h2.set_paths(f.dst.clone(), None);
                    h2.relocate_commit(f.journal(), Some(f.src.clone())).unwrap().wait_fast().unwrap();
                    f.assert_moved();
                }
                k += 1;
            }
            Ok(_) => {
                h.set_paths(f.dst.clone(), None);
                h.relocate_commit(f.journal(), Some(f.src.clone())).unwrap().wait_fast().unwrap();
                f.assert_moved();
                return k;
            }
            Err(e) => panic!("step {k}: {e}"),
        }
    }
}

#[test]
fn crash_at_every_step_rename_path() {
    let n = crash_every_step(false);
    assert!(n >= 12, "only {n} crash points");
}

#[test]
fn crash_at_every_step_copy_path() {
    let n = crash_every_step(true);
    assert!(n >= 30, "only {n} crash points");
}

#[test]
fn recovery_after_switch_goes_forward_and_commit_is_resumable() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut o = opts(&f.s);
    o.force_copy = true;
    h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast().unwrap();
    // Crash before the engine persisted anything: Prepared.
    let rec = h.relocate_recover(&f.journal()).unwrap();
    assert_eq!(rec.state, RecoverState::Prepared);
    assert_eq!(rec.method, Some(RelocateMethod::Copied));
    assert_eq!(rec.entries.len(), 5); // four files and the empty one
    assert_eq!(rec.entries[0].dst, f.dst.join("T/a.bin"));
    // The engine persists its record, marks it, then crashes during the source deletion.
    h.relocate_mark_switched(&f.journal()).unwrap();
    h.relocate_mark_switched(&f.journal()).unwrap(); // idempotent
    std::fs::remove_file(f.src.join("T/a.bin")).unwrap(); // half-done deletion
    drop(h);
    drop(io);
    let io2 = disk();
    let h2 = f.handle(&io2);
    assert_eq!(h2.relocate_recover(&f.journal()).unwrap().state, RecoverState::Switched);
    let r = h2.relocate_rollback(f.journal()).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::InvalidState)), "{r:?}");
    h2.set_paths(f.dst.clone(), None);
    h2.relocate_commit(f.journal(), Some(f.src.clone())).unwrap().wait_fast().unwrap();
    f.assert_moved();
    read_all_ok(&h2, &f.s);
    // Commit refuses to delete a source whose destination is incomplete.
}

#[test]
fn commit_never_deletes_a_source_whose_destination_is_not_there() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut o = opts(&f.s);
    o.force_copy = true;
    h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast().unwrap();
    std::fs::remove_file(f.dst.join("T/c.bin")).unwrap(); // destination damaged
    let r = h.relocate_commit(f.journal(), None).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::MissingData { file: 4 })), "{r:?}");
    for i in [0, 2, 4, 5] {
        assert!(f.src.join(&f.s.names[i]).exists(), "source {i} must survive");
    }
    assert!(f.journal().exists());
}

#[test]
fn second_prepare_while_a_move_is_unfinished_is_refused() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    h.relocate_prepare(f.journal(), f.plan(&h), opts(&f.s)).unwrap().wait_fast().unwrap();
    let mut other = f.plan(&h);
    for p in other.iter_mut().flatten() {
        *p = f.base.join("elsewhere").join(p.strip_prefix(&f.dst).unwrap());
    }
    let r = h.relocate_prepare(f.journal(), other, opts(&f.s)).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::InvalidState)), "{r:?}");
    h.relocate_rollback(f.journal()).unwrap().wait_fast().unwrap();
    f.assert_original();
}

#[test]
fn foreign_or_garbage_journal() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    std::fs::write(f.journal(), b"TRSR\x01garbage-garbage-garbage").unwrap();
    assert!(matches!(h.relocate_recover(&f.journal()), Err(StorageError::CorruptJournal)));
    // A header-only (torn at creation) journal means nothing happened.
    std::fs::write(f.journal(), b"TRSR").unwrap();
    assert_eq!(h.relocate_recover(&f.journal()).unwrap().state, RecoverState::Nothing);
    assert!(!f.journal().exists());
}

// ---------------------------------------------------------------------------------------------
// switch_layout, plan_paths, quiesce
// ---------------------------------------------------------------------------------------------

#[test]
fn switch_layout_changes_paths_atomically_and_checks_geometry() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    read_all_ok(&h, &f.s);
    // Copy the data elsewhere by hand and point absolute per-file paths at it.
    let other = f.base.join("abs");
    f.s.write_all_files(&other);
    let new = f
        .s
        .layout
        .with_paths((0..f.s.layout.file_count()).map(|i| {
            FilePath::Absolute(other.join(&f.s.names[i]))
        }))
        .unwrap();
    assert_ne!(new.digest(), f.s.layout.digest());
    assert_eq!(new.structure_digest(), f.s.layout.structure_digest());
    // Remove the old data: only the new location can serve now.
    std::fs::remove_dir_all(&f.src).unwrap();
    // Cached fds still serve the old inodes until the switch; the switch drops them.
    h.switch_layout(Arc::new(new.clone())).unwrap();
    assert_eq!(h.layout().unwrap().digest(), new.digest());
    read_all_ok(&h, &f.s);
    // Cold round trip with the new digest.
    h.drop_layout().unwrap();
    assert!(h.rebuild_layout(f.s.layout.clone()).is_err(), "stale layout must be refused");
    h.rebuild_layout(Arc::new(new)).unwrap();
    read_all_ok(&h, &f.s);
    // Different geometry is refused.
    let other_geo = synth_torrent(PIECE, &files()[..3], 5).layout;
    assert!(matches!(h.switch_layout(other_geo), Err(StorageError::LayoutMismatch)));
    let g = synth_torrent(PIECE + 1, &files(), 5).layout;
    assert!(matches!(h.switch_layout(g), Err(StorageError::LayoutMismatch)));
}

#[test]
fn switch_layout_while_cold_updates_the_expected_digest() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    h.drop_layout().unwrap();
    let new = Arc::new(
        f.s.layout
            .with_paths((0..f.s.layout.file_count()).map(|i| {
                FilePath::Relative(vec!["x".into(), format!("{i}")])
            }))
            .unwrap(),
    );
    h.switch_layout(new.clone()).unwrap();
    assert!(!h.has_layout());
    assert!(h.rebuild_layout(f.s.layout.clone()).is_err());
    h.rebuild_layout(new).unwrap();
}

#[test]
fn quiesce_writes_blocks_new_writes_and_drains() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(32_768, &files(), 9);
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir.path()));
    h.attach_hashes(s.hashes.clone()).unwrap();
    let mut pend = Vec::new();
    for (b, d) in s.blocks(0) {
        if let WriteAck::Complete(p) = h.write_block(0, b, &d).unwrap() {
            pend.push(p);
        }
    }
    h.quiesce_writes().unwrap().wait_fast().unwrap();
    for p in pend {
        p.wait_fast().unwrap(); // already resolved: the drain waited for it
    }
    let (b, d) = s.blocks(1).remove(0);
    assert!(matches!(h.write_block(1, b, &d), Err(StorageError::Busy)));
    h.resume_writes();
    assert!(h.write_block(1, b, &d).is_ok());
}

#[test]
fn partial_download_relocates_and_continues_at_the_new_place() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (old, new) = (base.join("incomplete-a"), base.join("incomplete-b"));
    let fl: Vec<(&[&str], u64, bool)> = vec![
        (&["a.bin"][..], 98_304, false),
        (&["b.bin"][..], 98_304, false),
        (&["c.bin"][..], 98_304, false),
    ];
    let s = synth_torrent(32_768, &fl, 3);
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(&old));
    h.attach_hashes(s.hashes.clone()).unwrap();
    h.ensure_root().unwrap();
    // Pieces 0..3 are file a, piece 3 starts b: write 0,1,2 and 3 (verified) but not the rest.
    let mut have = PieceSet::new(s.layout.piece_count());
    for p in 0..4 {
        let mut pend = Vec::new();
        for (b, d) in s.blocks(p) {
            if let WriteAck::Complete(f) = h.write_block(p, b, &d).unwrap() {
                pend.push(f);
            }
        }
        for f in pend {
            f.wait_fast().unwrap();
        }
        have.set(p, true);
    }
    assert!(!old.join("c.bin").exists(), "file c was never created");
    h.quiesce_writes().unwrap().wait_fast().unwrap();
    std::fs::create_dir_all(&new).unwrap();
    let plan = h.plan_paths(&new, None, Location::Final).unwrap();
    let o = RelocateOptions {
        hashes: Some(s.hashes.clone()),
        have: Some(have),
        force_copy: true,
        reserve_bytes: 0,
        buffer_bytes: 64 << 10,
        ..RelocateOptions::default()
    };
    let journal = base.join("j");
    let rep = h.relocate_prepare(journal.clone(), plan, o).unwrap().wait_fast().unwrap();
    assert_eq!(rep.method, RelocateMethod::Copied);
    assert_eq!(rep.missing_skipped, 1);
    assert_eq!(rep.files, 2);
    assert_eq!(rep.verified_pieces, 4);
    h.set_paths(new.clone(), None);
    h.relocate_mark_switched(&journal).unwrap();
    h.relocate_commit(journal, Some(old.clone())).unwrap().wait_fast().unwrap();
    h.resume_writes();
    assert!(!old.join("a.bin").exists());
    assert_eq!(std::fs::metadata(new.join("b.bin")).unwrap().len(), 98_304);
    // Download continues at the new location and completes.
    let mut pend = Vec::new();
    for p in 4..s.layout.piece_count() {
        for (b, d) in s.blocks(p) {
            if let WriteAck::Complete(f) = h.write_block(p, b, &d).unwrap() {
                pend.push(f);
            }
        }
    }
    for f in pend {
        f.wait_fast().unwrap();
    }
    h.set_mode(Mode::Seed);
    read_all_ok(&h, &s);
    for i in 0..3 {
        assert_eq!(std::fs::read(new.join(&s.names[i])).unwrap(), s.file_bytes(i));
    }
}

// ---------------------------------------------------------------------------------------------
// A real second volume
// ---------------------------------------------------------------------------------------------

/// A temporary APFS disk image mounted below a temp dir; always detached on drop.
struct Image {
    mnt: PathBuf,
}

impl Image {
    fn create(dir: &Path, size: &str) -> Option<Self> {
        if cfg!(not(target_os = "macos")) {
            return None;
        }
        let img = dir.join("trsreloc.dmg");
        let mnt = dir.join("mnt");
        std::fs::create_dir_all(&mnt).ok()?;
        let run = |args: &[&str]| {
            std::process::Command::new("hdiutil")
                .env("HOME", dir)
                .args(args)
                .output()
        };
        let out = run(&[
            "create",
            "-size",
            size,
            "-fs",
            "APFS",
            "-volname",
            "trsreloc",
            "-ov",
            img.to_str()?,
        ])
        .ok()?;
        if !out.status.success() {
            eprintln!("hdiutil create failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        let guard = Self { mnt: mnt.clone() };
        let out = run(&[
            "attach",
            "-nobrowse",
            "-mountpoint",
            mnt.to_str()?,
            img.to_str()?,
        ])
        .ok()?;
        if !out.status.success() {
            eprintln!("hdiutil attach failed: {}", String::from_utf8_lossy(&out.stderr));
            return None;
        }
        Some(guard)
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        for _ in 0..3 {
            let ok = std::process::Command::new("hdiutil")
                .args(["detach", "-force"])
                .arg(&self.mnt)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if ok {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }
}

#[test]
fn real_cross_device_relocation_onto_a_disk_image() {
    let f = fx();
    let img_dir = f.base.join("img");
    std::fs::create_dir_all(&img_dir).unwrap();
    let Some(img) = Image::create(&img_dir, "64m") else {
        eprintln!("SKIP: cannot create/attach a disk image here");
        return;
    };
    let probe = img.mnt.join("probe");
    if std::fs::write(&probe, b"x").is_err() {
        eprintln!("SKIP: image not writable");
        return;
    }
    std::fs::remove_file(&probe).unwrap();
    let dst = img.mnt.join("Library").join("TV");
    assert!(
        !trs_storage::same_volume(&f.src, &dst).unwrap(),
        "image must be a different volume"
    );
    let io = disk();
    let h = f.handle(&io);
    let plan = h.plan_paths(&dst, None, Location::Final).unwrap();
    let mut o = opts(&f.s);
    o.reserve_bytes = 1 << 20; // 64 MB image: keep the default 1 GiB reserve out of this test
    let rep = h.relocate_prepare(f.journal(), plan, o).unwrap().wait_fast().unwrap();
    assert_eq!(rep.method, RelocateMethod::Copied, "different device selects the copy path by itself");
    assert_eq!(rep.verified_pieces, f.s.layout.piece_count());
    for i in [0, 2, 4, 5] {
        assert_eq!(std::fs::read(dst.join(&f.s.names[i])).unwrap(), f.s.file_bytes(i));
        assert!(f.src.join(&f.s.names[i]).exists());
    }
    read_all_ok(&h, &f.s); // still served from the source
    h.set_paths(dst.clone(), None);
    read_all_ok(&h, &f.s); // now from the image
    h.relocate_mark_switched(&f.journal()).unwrap();
    h.relocate_commit(f.journal(), Some(f.src.clone())).unwrap().wait_fast().unwrap();
    for i in [0, 2, 4, 5] {
        assert!(!f.src.join(&f.s.names[i]).exists());
        assert_eq!(std::fs::read(dst.join(&f.s.names[i])).unwrap(), f.s.file_bytes(i));
    }
    assert!(walk(&f.src).is_empty());
    read_all_ok(&h, &f.s);

    // Free-space refusal on the real small volume: 64 MB image, ask for more than it holds.
    let big: Vec<(&[&str], u64, bool)> = vec![(&["big.bin"][..], 90 << 20, false)];
    let bs = synth_torrent(1 << 20, &big, 1);
    let bsrc = f.base.join("bigsrc");
    bs.write_all_files(&bsrc);
    let h2 = io.add_torrent(bs.layout.clone(), TorrentConfig::seed(&bsrc));
    let plan = h2.plan_paths(&img.mnt.join("Big"), None, Location::Final).unwrap();
    let o = RelocateOptions {
        hashes: Some(bs.hashes.clone()),
        reserve_bytes: 0,
        ..RelocateOptions::default()
    };
    let r = h2.relocate_prepare(f.base.join("jbig"), plan, o).unwrap().wait_fast();
    assert!(matches!(r, Err(StorageError::InsufficientSpace { .. })), "{r:?}");
    assert!(!img.mnt.join("Big").exists());
}

// ---------------------------------------------------------------------------------------------
// Numbers (run with --release --ignored --nocapture)
// ---------------------------------------------------------------------------------------------

#[test]
#[ignore = "benchmark: cargo test --release -p trs-storage --test relocate -- --ignored --nocapture"]
fn bench_relocate() {
    // 1. Same-volume rename throughput.
    {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let n = 2000usize;
        let names: Vec<String> = (0..n).map(|i| format!("f{i:05}.bin")).collect();
        let fl: Vec<(Vec<&str>, u64, bool)> = Vec::new();
        let _ = fl;
        let specs: Vec<(FilePath, u64, bool)> = names
            .iter()
            .map(|nm| (FilePath::Relative(vec!["T".into(), nm.clone()]), 4096, false))
            .collect();
        let s = synth_with_paths(65_536, specs, 11);
        let src = base.join("src");
        s.write_all_files(&src);
        let io = disk();
        let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(&src));
        let dst = base.join("dst");
        let plan = h.plan_paths(&dst, None, Location::Final).unwrap();
        let j = base.join("j");
        let t0 = Instant::now();
        h.relocate_prepare(j.clone(), plan, RelocateOptions::default())
            .unwrap()
            .wait_fast()
            .unwrap();
        let prep = t0.elapsed();
        h.set_paths(dst.clone(), None);
        let t1 = Instant::now();
        h.relocate_commit(j, Some(src)).unwrap().wait_fast().unwrap();
        println!(
            "rename: {n} files in {:?} ({:.0} files/s), commit {:?}",
            prep,
            n as f64 / prep.as_secs_f64(),
            t1.elapsed()
        );
    }
    // 2. Copy throughput on the internal disk, 1 GiB, hashing / no hashing / nocache.
    let mb: u64 = std::env::var("RELOC_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(1024);
    let big: Vec<(&[&str], u64, bool)> = vec![
        (&["x", "big1.bin"][..], mb << 20 >> 1, false),
        (&["x", "big2.bin"][..], mb << 20 >> 1, false),
    ];
    let s = synth_torrent(4 << 20, &big, 21);
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let src = base.join("src");
    s.write_all_files(&src);
    let img_dir = base.join("img");
    std::fs::create_dir_all(&img_dir).unwrap();
    let img = Image::create(&img_dir, "3g");
    let mut targets: Vec<(&str, PathBuf)> = vec![("internal SSD", base.join("dstroot"))];
    if let Some(i) = &img {
        targets.push(("disk image (same SSD)", i.mnt.join("d")));
    }
    for (label, root) in targets {
        for (hashing, nocache, buf) in [
            (true, true, 4 << 20),
            (false, true, 4 << 20),
            (true, false, 4 << 20),
            (false, false, 4 << 20),
            (true, true, 8 << 20),
        ] {
            let io = disk();
            let h = io.add_torrent(s.layout.clone(), TorrentConfig::seed(&src));
            let dst = root.join(format!("{hashing}-{nocache}-{buf}"));
            let plan = h.plan_paths(&dst, None, Location::Final).unwrap();
            let o = RelocateOptions {
                hashes: hashing.then(|| s.hashes.clone()),
                nocache,
                buffer_bytes: buf,
                force_copy: true,
                reserve_bytes: 1 << 20,
                ..RelocateOptions::default()
            };
            let j = base.join("jb");
            let t0 = Instant::now();
            match h.relocate_prepare(j.clone(), plan, o).unwrap().wait_fast() {
                Ok(rep) => {
                    let el = t0.elapsed().as_secs_f64();
                    println!(
                        "copy {label}: {} MiB, hash={hashing} nocache={nocache} buf={} MiB: {:.0} MB/s ({:.2}s)",
                        mb,
                        buf >> 20,
                        rep.bytes as f64 / 1e6 / el,
                        el
                    );
                    h.relocate_rollback(j).unwrap().wait_fast().unwrap();
                }
                Err(e) => println!("copy {label}: failed: {e}"),
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// switch_layout_renaming: a rename plus the layout switch is one step for writers
// ---------------------------------------------------------------------------------------------

/// Three 96 KiB files with absolute paths; the middle one lives in a hidden folder.
fn hidden_fixture(base: &Path) -> (Synth, Vec<FilePath>, Vec<FilePath>) {
    let lib = base.join("lib");
    let hidden = lib.join(".hidden");
    let before = vec![
        FilePath::Absolute(lib.join("a.bin")),
        FilePath::Absolute(hidden.join("b.bin")),
        FilePath::Absolute(lib.join("c.bin")),
    ];
    let mut after = before.clone();
    after[1] = FilePath::Absolute(lib.join("b.bin"));
    let s = synth_with_paths(
        32_768,
        before.iter().cloned().map(|p| (p, 98_304, false)).collect(),
        11,
    );
    (s, before, after)
}

fn write_piece(h: &TorrentHandle, s: &Synth, p: u32) {
    let mut pend = Vec::new();
    for (b, d) in s.blocks(p) {
        if let WriteAck::Complete(f) = h.write_block(p, b, &d).unwrap() {
            pend.push(f);
        }
    }
    for f in pend {
        f.wait_fast().unwrap();
    }
}

#[test]
fn a_write_that_lands_between_the_rename_and_the_switch_goes_to_the_new_path() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (s, before, after) = hidden_fixture(&base);
    let lib = base.join("lib");
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(&base));
    h.attach_hashes(s.hashes.clone()).unwrap();
    // Piece 3 is the first of file b (pieces 0..3 are file a).
    write_piece(&h, &s, 3);
    assert!(lib.join(".hidden/b.bin").exists());
    // No descriptor is cached for it, as for a file that is opened lazily after a restart.
    h.set_mode(Mode::Download);
    let new = Arc::new(s.layout.with_paths(after.iter().cloned()).unwrap());
    let rename = (lib.join(".hidden/b.bin"), lib.join("b.bin"));
    let (h2, s2) = (h.clone(), &s);
    std::thread::scope(|sc| {
        // The hook runs after the rename, before the switch: put the next piece of b there.
        let hook = || {
            sc.spawn(|| write_piece(&h2, s2, 4));
            // Give the write job every chance to open the old path.
            std::thread::sleep(Duration::from_millis(150));
        };
        h.switch_layout_renaming(new, &[rename], &[], Some(&hook)).unwrap();
    });
    write_piece(&h, &s, 5);
    assert!(!lib.join(".hidden/b.bin").exists(), "the old path was recreated by a late write");
    let got = std::fs::read(lib.join("b.bin")).unwrap();
    assert_eq!(&got[..], &s.file_bytes(1)[..got.len()]);
    assert_eq!(got.len(), 98_304);
    let b = s.file_bytes(1);
    assert_eq!(&got[..], b, "pieces 3, 4 and 5 are all in the file at its new path");
    let _ = before;
}

#[test]
fn switch_layout_renaming_refuses_a_target_that_appeared_and_undoes_partial_renames() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (s, _, after) = hidden_fixture(&base);
    let lib = base.join("lib");
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(&base));
    h.attach_hashes(s.hashes.clone()).unwrap();
    write_piece(&h, &s, 3);
    let new = Arc::new(s.layout.with_paths(after.iter().cloned()).unwrap());
    // `absent` violated: nothing changes.
    std::fs::write(lib.join("b.bin"), b"user").unwrap();
    let err = h.switch_layout_renaming(new.clone(), &[], &[lib.join("b.bin")], None);
    assert!(err.is_err());
    assert_eq!(h.layout().unwrap().digest(), s.layout.digest());
    // The second rename collides: the first is undone and the layout is untouched.
    std::fs::write(lib.join("taken"), b"x").unwrap();
    let r = [
        (lib.join(".hidden/b.bin"), lib.join("moved")),
        (lib.join("moved"), lib.join("taken")),
    ];
    assert!(h.switch_layout_renaming(new, &r, &[], None).is_err());
    assert!(lib.join(".hidden/b.bin").exists());
    assert!(!lib.join("moved").exists());
    assert_eq!(h.layout().unwrap().digest(), s.layout.digest());
}

// ---------------------------------------------------------------------------------------------
// Metadata follows the data across volumes
// ---------------------------------------------------------------------------------------------

/// A binary plist holding the Finder tags ["Green\n2", "Seedwell"] (`com.apple.metadata:_kMDItemUserTags`).
const FINDER_TAGS: [u8; 63] = [
    98, 112, 108, 105, 115, 116, 48, 48, 162, 1, 2, 87, 71, 114, 101, 101, 110, 10, 50, 88, 83, 101, 101, 100, 119, 101, 108,
    108, 8, 11, 19, 0, 0, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28,
];

fn set_xattr(p: &Path, name: &str, value: &[u8]) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let name = std::ffi::CString::new(name).unwrap();
    // SAFETY: NUL-terminated strings and a readable buffer of the stated length.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0, 0) };
    // SAFETY: as above.
    #[cfg(not(target_os = "macos"))]
    let rc = unsafe { libc::setxattr(path.as_ptr(), name.as_ptr(), value.as_ptr().cast(), value.len(), 0) };
    assert_eq!(rc, 0, "setxattr {name:?}: {}", std::io::Error::last_os_error());
}

fn get_xattr(p: &Path, name: &str) -> Option<Vec<u8>> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
    let name = std::ffi::CString::new(name).unwrap();
    let mut buf = vec![0u8; 4096];
    // SAFETY: NUL-terminated strings and a writable buffer of the stated length.
    #[cfg(target_os = "macos")]
    let n = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0) };
    // SAFETY: as above.
    #[cfg(not(target_os = "macos"))]
    let n = unsafe { libc::getxattr(path.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    (n >= 0).then(|| {
        buf.truncate(n as usize);
        buf
    })
}

#[cfg(target_os = "macos")]
fn acl_listing(p: &Path) -> String {
    let out = std::process::Command::new("/bin/ls").arg("-le").arg(p).output().unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A cross-volume move (the copy path) keeps extended attributes, Finder tags, ACLs and the mode of
/// every file; an empty file and a file without any attribute are fine too.
#[test]
fn copy_keeps_xattrs_finder_tags_acls_and_mode() {
    use std::os::unix::fs::PermissionsExt;
    let f = fx();
    let a = f.src.join(&f.s.names[0]);
    let d = f.src.join(&f.s.names[5]);
    let empty = f.src.join(&f.s.names[3]);
    let tag_name = if cfg!(target_os = "macos") { "com.apple.metadata:_kMDItemUserTags" } else { "user.xdg.tags" };
    set_xattr(&a, tag_name, &FINDER_TAGS);
    let comment_name = if cfg!(target_os = "macos") { "com.apple.metadata:kMDItemFinderComment" } else { "user.xdg.comment" };
    set_xattr(&a, comment_name, b"bplist00 a comment");
    set_xattr(&empty, if cfg!(target_os = "macos") { "com.seedwell.test" } else { "user.seedwell.test" }, b"v");
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o640)).unwrap();
    #[cfg(target_os = "macos")]
    {
        let st = std::process::Command::new("/bin/chmod").arg("+a").arg("everyone allow read,readattr").arg(&a).status().unwrap();
        assert!(st.success());
        assert!(acl_listing(&a).contains("allow read,readattr"), "{}", acl_listing(&a));
    }

    let io = disk();
    let h = f.handle(&io);
    let mut o = opts(&f.s);
    o.force_copy = true;
    h.relocate_prepare(f.journal(), f.plan(&h), o).unwrap().wait_fast().unwrap();
    h.set_paths(f.dst.clone(), None);
    h.relocate_commit(f.journal(), None).unwrap().wait_fast().unwrap();
    f.assert_moved();

    let (na, nd, nempty) = (f.dst.join(&f.s.names[0]), f.dst.join(&f.s.names[5]), f.dst.join(&f.s.names[3]));
    assert_eq!(get_xattr(&na, tag_name).as_deref(), Some(&FINDER_TAGS[..]), "Finder tags");
    assert_eq!(get_xattr(&na, comment_name).as_deref(), Some(&b"bplist00 a comment"[..]), "comment");
    assert_eq!(std::fs::metadata(&na).unwrap().permissions().mode() & 0o7777, 0o640);
    assert_eq!(
        get_xattr(&nempty, if cfg!(target_os = "macos") { "com.seedwell.test" } else { "user.seedwell.test" }).as_deref(),
        Some(&b"v"[..]),
        "empty file"
    );
    #[cfg(target_os = "macos")]
    assert!(acl_listing(&na).contains("allow read,readattr"), "ACL lost: {}", acl_listing(&na));
    // A file that had no attributes has none now, and the data is byte-identical.
    assert_eq!(get_xattr(&nd, tag_name), None);
    let _ = d;
}

/// Finder drops a `.DS_Store` into every folder it shows: the folders a move empties still go,
/// litter and all, while a folder that holds anything else stays with it untouched.
#[test]
fn finder_litter_does_not_keep_an_emptied_folder() {
    let f = fx();
    let io = disk();
    let h = f.handle(&io);
    let mut dirs: Vec<PathBuf> = walk(&f.src).iter().filter_map(|p| p.parent().map(Path::to_path_buf)).collect();
    dirs.push(f.src.clone());
    dirs.sort();
    dirs.dedup();
    for d in &dirs {
        std::fs::write(d.join(".DS_Store"), b"finder").unwrap();
    }
    let keep = f.base.join("keep");
    std::fs::create_dir_all(&keep).unwrap();
    std::fs::write(keep.join(".DS_Store"), b"finder").unwrap();
    std::fs::write(keep.join("notes.txt"), b"mine").unwrap();
    let plan = f.plan(&h);
    h.relocate_prepare(f.journal(), plan, opts(&f.s)).unwrap().wait_fast().unwrap();
    h.set_paths(f.dst.clone(), None);
    h.relocate_mark_switched(&f.journal()).unwrap();
    h.relocate_commit(f.journal(), Some(f.base.clone())).unwrap().wait_fast().unwrap();
    f.assert_moved();
    assert!(!f.src.exists(), "left behind: {:?}", walk(&f.src));
    assert!(keep.join(".DS_Store").exists() && keep.join("notes.txt").exists());
    assert!(f.base.exists(), "stop_at itself stays");
}
