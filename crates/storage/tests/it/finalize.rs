//! Staging, finalize (atomic moves + cross-volume copy fallback) and crash recovery.

use crate::common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use common::*;
use trs_storage::{
    DiskConfig, DiskIo, FilePath, FinalizeOptions, Location, Mode, StorageError, TorrentConfig,
    TorrentHandle, WriteAck,
};

fn disk() -> DiskIo {
    DiskIo::new(DiskConfig {
        io_threads: 2,
        hash_threads: 2,
        ..DiskConfig::default()
    })
}

/// 4 files: relative, relative-in-subdir, a custom absolute target, an empty file.
fn staged_torrent(base: &Path, big: u64) -> (Synth, PathBuf) {
    let custom_dir = base.join("library").join("Movies");
    let files = vec![
        (
            FilePath::Relative(vec!["Show".into(), "a.bin".into()]),
            70_000,
            false,
        ),
        (
            FilePath::Relative(vec!["Show".into(), "sub".into(), "b.bin".into()]),
            big,
            false,
        ),
        (
            FilePath::Absolute(custom_dir.join("Movie (2020).mkv")),
            40_000,
            false,
        ),
        (
            FilePath::Relative(vec!["Show".into(), "empty.txt".into()]),
            0,
            false,
        ),
    ];
    (synth_with_paths(32_768, files, 31), custom_dir)
}

/// Writes the staged files exactly where the handle expects them (as a finished download would).
fn populate_staging(h: &TorrentHandle, s: &Synth) {
    for i in 0..s.layout.file_count() {
        if s.layout.file_len(i) == 0 {
            continue;
        }
        let p = h.file_path(i as u32, Location::Staging).unwrap();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s.file_bytes(i)).unwrap();
    }
}

fn final_path(h: &TorrentHandle, i: usize) -> PathBuf {
    h.file_path(i as u32, Location::Final).unwrap()
}

fn assert_finalized(h: &TorrentHandle, s: &Synth, root: &Path) {
    for i in 0..s.layout.file_count() {
        let p = final_path(h, i);
        assert_eq!(std::fs::read(&p).unwrap(), s.file_bytes(i), "{p:?}");
        let st = h.file_path(i as u32, Location::Staging).unwrap();
        assert!(!st.exists(), "staged file left behind: {st:?}");
    }
    assert!(
        !root.join(".incomplete").exists(),
        "empty staging tree must be pruned"
    );
    assert_eq!(h.location(), Location::Final);
    let mut leftovers = Vec::new();
    for e in walk(root) {
        if e.to_string_lossy().ends_with(".trs-part") || e.to_string_lossy().ends_with(".journal") {
            leftovers.push(e);
        }
    }
    assert!(leftovers.is_empty(), "{leftovers:?}");
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

#[test]
fn download_to_staging_then_finalize() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dl");
    std::fs::create_dir_all(&root).unwrap();
    let (s, custom_dir) = staged_torrent(dir.path(), 200_000);
    let io = disk();
    let h = io.add_torrent(
        s.layout.clone(),
        TorrentConfig::download(&root).with_default_staging(),
    );
    h.attach_hashes(s.hashes.clone()).unwrap();
    h.ensure_root().unwrap();

    let mut pend = Vec::new();
    for p in 0..s.layout.piece_count() {
        for (b, d) in s.blocks(p) {
            if let WriteAck::Complete(f) = h.write_block(p, b, &d).unwrap() {
                pend.push(f);
            }
        }
    }
    for f in pend {
        f.wait().unwrap();
    }
    // Everything is in staging, nothing at the final paths yet (media apps never see partials).
    assert!(root.join(".incomplete/Show/a.bin").exists());
    assert!(root.join(".incomplete/.custom").is_dir());
    assert!(!root.join("Show").exists() && !custom_dir.exists());

    let journal = root.join(".incomplete").join("t.journal");
    let rep = h
        .finalize(journal.clone(), FinalizeOptions::default())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(rep.renamed, 3);
    assert_eq!(rep.empty_created, 1);
    assert_finalized(&h, &s, &root);
    assert_eq!(
        std::fs::metadata(root.join("Show/empty.txt"))
            .unwrap()
            .len(),
        0
    );
    assert!(custom_dir.join("Movie (2020).mkv").exists());
    assert!(!journal.exists());

    // Switch to seeding and serve a block from the final location.
    h.set_mode(Mode::Seed);
    let blk = h
        .read_block(0, 0, 16_384, Vec::new())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(&blk[..], &s.piece(0)[..16_384]);
    // Idempotent.
    h.finalize(journal, FinalizeOptions::default())
        .unwrap()
        .wait()
        .unwrap();
}

fn crash_and_resume(force_copy: bool, big: u64) -> u32 {
    let mut k = 0u32;
    loop {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dl");
        let (s, _) = staged_torrent(dir.path(), big);
        let journal = root.join(".incomplete").join("t.journal");
        let cfg = || TorrentConfig::seed(&root).with_default_staging();

        let io = disk();
        let h = io.add_torrent(s.layout.clone(), cfg());
        populate_staging(&h, &s);
        let calls = Arc::new(AtomicU32::new(0));
        let c2 = calls.clone();
        let opts = FinalizeOptions {
            force_copy,
            fault: Some(Arc::new(move |step| {
                c2.fetch_add(1, Ordering::Relaxed);
                step < k
            })),
        };
        let first = h.finalize(journal.clone(), opts).unwrap().wait();
        match first {
            Err(StorageError::Interrupted) => {
                // "Process restart": brand-new pools and handle, same on-disk state.
                drop(h);
                drop(io);
                let io2 = disk();
                let h2 = io2.add_torrent(s.layout.clone(), cfg());
                assert_eq!(h2.location(), Location::Staging);
                let opts = FinalizeOptions {
                    force_copy,
                    fault: None,
                };
                h2.finalize(journal.clone(), opts)
                    .unwrap()
                    .wait()
                    .unwrap_or_else(|e| panic!("resume after crash at step {k} failed: {e}"));
                assert_finalized(&h2, &s, &root);
                k += 1;
            }
            Ok(_) => {
                assert_finalized(&h, &s, &root);
                return k; // crash points exhausted: first run finished without interruption
            }
            Err(e) => panic!("step {k}: {e}"),
        }
    }
}

#[test]
fn interrupted_finalize_resumes_at_every_step_rename_path() {
    let steps = crash_and_resume(false, 90_000);
    assert!(steps >= 10, "only {steps} crash points exercised");
}

#[test]
fn interrupted_finalize_resumes_at_every_step_copy_path() {
    // 2.5 MiB file => several copy chunks, so crashes also land mid-copy with a partial .trs-part.
    let steps = crash_and_resume(true, 2_621_440);
    assert!(steps >= 25, "only {steps} crash points exercised");
}

#[test]
fn never_overwrites_foreign_destination() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dl");
    let (s, _) = staged_torrent(dir.path(), 90_000);
    let io = disk();
    let h = io.add_torrent(
        s.layout.clone(),
        TorrentConfig::seed(&root).with_default_staging(),
    );
    populate_staging(&h, &s);
    let dst = final_path(&h, 1);
    std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
    std::fs::write(&dst, b"precious").unwrap();

    let r = h
        .finalize(root.join("j"), FinalizeOptions::default())
        .unwrap()
        .wait();
    assert!(
        matches!(r, Err(StorageError::DestinationExists { file: 1 })),
        "{r:?}"
    );
    assert_eq!(std::fs::read(&dst).unwrap(), b"precious");
    assert!(
        h.file_path(1, Location::Staging).unwrap().exists(),
        "source must stay intact"
    );
    assert_eq!(h.location(), Location::Staging);
}

#[test]
fn missing_staged_file_and_unplugged_destination_volume() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dl");
    let (s, _) = staged_torrent(dir.path(), 90_000);
    let io = disk();
    let h = io.add_torrent(
        s.layout.clone(),
        TorrentConfig::seed(&root).with_default_staging(),
    );
    populate_staging(&h, &s);
    std::fs::remove_file(h.file_path(0, Location::Staging).unwrap()).unwrap();
    let r = h
        .finalize(root.join("j1"), FinalizeOptions::default())
        .unwrap()
        .wait();
    assert!(
        matches!(r, Err(StorageError::MissingData { file: 0 })),
        "{r:?}"
    );

    // Custom target on a volume that is not mounted: refuse, keep the staged data.
    let files = vec![
        (FilePath::Relative(vec!["a".into()]), 1000, false),
        (
            FilePath::Absolute("/Volumes/__trs_unplugged__/Media/x.mkv".into()),
            1000,
            false,
        ),
    ];
    let s2 = synth_with_paths(512, files, 3);
    let root2 = dir.path().join("dl2");
    let h2 = io.add_torrent(
        s2.layout.clone(),
        TorrentConfig::seed(&root2).with_default_staging(),
    );
    populate_staging(&h2, &s2);
    let r = h2
        .finalize(root2.join("j2"), FinalizeOptions::default())
        .unwrap()
        .wait();
    assert!(matches!(r, Err(StorageError::MissingRoot)), "{r:?}");
    assert!(!Path::new("/Volumes/__trs_unplugged__").exists());
    assert!(h2.file_path(1, Location::Staging).unwrap().exists());
}

#[test]
fn foreign_journal_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dl");
    let (s, _) = staged_torrent(dir.path(), 90_000);
    let io = disk();
    let h = io.add_torrent(
        s.layout.clone(),
        TorrentConfig::seed(&root).with_default_staging(),
    );
    populate_staging(&h, &s);
    let j = root.join("foreign.journal");
    std::fs::write(&j, b"TRSJ\x01garbage-garbage-garbage").unwrap();
    let r = h.finalize(j, FinalizeOptions::default()).unwrap().wait();
    assert!(matches!(r, Err(StorageError::CorruptJournal)), "{r:?}");
}

/// Finishing a download across volumes (the copy path of finalize) keeps a staged file's extended
/// attributes, such as a Finder tag a watch-folder or an app put on it.
#[test]
fn finalize_by_copy_keeps_extended_attributes() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dl");
    std::fs::create_dir_all(&root).unwrap();
    let (s, _custom) = staged_torrent(dir.path(), 100_000);
    let io = disk();
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(&root).with_default_staging());
    h.ensure_root().unwrap();
    populate_staging(&h, &s);
    let staged = h.file_path(0, Location::Staging).unwrap();
    let cpath = std::ffi::CString::new(staged.as_os_str().as_bytes()).unwrap();
    let (name, tag) = if cfg!(target_os = "macos") {
        ("com.apple.metadata:_kMDItemUserTags", &b"bplist00 tags"[..])
    } else {
        ("user.xdg.tags", &b"bplist00 tags"[..])
    };
    let cname = std::ffi::CString::new(name).unwrap();
    // SAFETY: NUL-terminated strings, readable value buffer.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::setxattr(cpath.as_ptr(), cname.as_ptr(), tag.as_ptr().cast(), tag.len(), 0, 0) };
    // SAFETY: as above.
    #[cfg(not(target_os = "macos"))]
    let rc = unsafe { libc::setxattr(cpath.as_ptr(), cname.as_ptr(), tag.as_ptr().cast(), tag.len(), 0) };
    assert_eq!(rc, 0);

    let journal = root.join(".incomplete").join("t.journal");
    let opts = FinalizeOptions { force_copy: true, ..FinalizeOptions::default() };
    h.finalize(journal, opts).unwrap().wait().unwrap();
    let fin = final_path(&h, 0);
    let cfin = std::ffi::CString::new(fin.as_os_str().as_bytes()).unwrap();
    let mut buf = [0u8; 64];
    // SAFETY: NUL-terminated strings, writable buffer.
    #[cfg(target_os = "macos")]
    let n = unsafe { libc::getxattr(cfin.as_ptr(), cname.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0) };
    // SAFETY: as above.
    #[cfg(not(target_os = "macos"))]
    let n = unsafe { libc::getxattr(cfin.as_ptr(), cname.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) };
    assert_eq!(&buf[..n.max(0) as usize], tag, "xattr after the finalize copy");
    assert_eq!(std::fs::read(&fin).unwrap(), s.file_bytes(0));
}
