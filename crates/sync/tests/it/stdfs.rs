//! [`StdFs`] on a real (temporary) folder.

use std::io;

use trs_sync::fs::{Fs, StdFs, read_all, write_all};
use trs_sync::path::probe_case_insensitive;
use trs_sync_proto::model::FileKind;

fn folder() -> (tempfile::TempDir, StdFs) {
    let dir = tempfile::tempdir().expect("temp dir");
    let fs = StdFs::new(dir.path());
    (dir, fs)
}

#[test]
fn files_dirs_and_metadata() {
    let (_d, fs) = folder();
    fs.create_dir("a").unwrap();
    fs.create_dir("a").unwrap();
    write_all(&fs, "a/f.txt", b"hello").unwrap();
    let m = fs.lstat("a/f.txt").unwrap().unwrap();
    assert_eq!((m.kind, m.size, m.exec, m.other), (FileKind::File, 5, false, false));
    assert!(!m.dataless, "a normal file is not a cloud placeholder");
    assert!(m.inode != 0 && m.mtime_ns > 0 && m.ctime_ns > 0);
    assert_eq!(fs.lstat("a").unwrap().unwrap().kind, FileKind::Dir);
    assert!(!fs.lstat("a").unwrap().unwrap().dataless);
    assert_eq!(fs.lstat("missing").unwrap(), None);
    assert_eq!(fs.lstat("a/f.txt/below").unwrap(), None, "below a file: not there");

    fs.set_exec("a/f.txt", true).unwrap();
    assert!(fs.lstat("a/f.txt").unwrap().unwrap().exec);
    fs.set_exec("a/f.txt", false).unwrap();
    assert!(!fs.lstat("a/f.txt").unwrap().unwrap().exec);
    let t = 1_600_000_000_123_456_789;
    fs.set_mtime("a/f.txt", t).unwrap();
    assert_eq!(fs.lstat("a/f.txt").unwrap().unwrap().mtime_ns, t);

    fs.symlink("f.txt", "a/link").unwrap();
    assert_eq!(fs.lstat("a/link").unwrap().unwrap().kind, FileKind::Symlink);
    assert_eq!(fs.read_link("a/link").unwrap(), "f.txt");
    let names: Vec<String> = fs.read_dir("a").unwrap().entries.into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["f.txt", "link"]);

    let h = fs.open_rw("a/f.txt").unwrap();
    h.write_at(10, b"!").unwrap();
    assert_eq!(h.len().unwrap(), 11);
    h.sync_all().unwrap();
    let mut buf = [0; 32];
    assert_eq!(h.read_at(0, &mut buf).unwrap(), 11, "short only at the end");
    drop(h);
    assert_eq!(fs.remove_dir("a").unwrap_err().kind(), io::ErrorKind::DirectoryNotEmpty);
    fs.remove_file("a/link").unwrap();
    fs.remove_file("a/f.txt").unwrap();
    fs.remove_dir("a").unwrap();
}

#[test]
fn copy_is_independent() {
    let (_d, fs) = folder();
    write_all(&fs, "orig", b"original").unwrap();
    fs.copy("orig", "copy").unwrap();
    assert_ne!(fs.lstat("orig").unwrap().unwrap().inode, fs.lstat("copy").unwrap().unwrap().inode);
    write_all(&fs, "copy", b"changed").unwrap();
    assert_eq!(read_all(&fs, "orig").unwrap(), b"original");
    assert_eq!(read_all(&fs, "copy").unwrap(), b"changed");
}

#[test]
fn hard_links_share_content() {
    let (_d, fs) = folder();
    write_all(&fs, "a", b"one").unwrap();
    fs.hard_link("a", "b").unwrap();
    assert_eq!(fs.lstat("a").unwrap().unwrap().inode, fs.lstat("b").unwrap().unwrap().inode);
    fs.open_rw("a").unwrap().write_at(0, b"two").unwrap();
    assert_eq!(read_all(&fs, "b").unwrap(), b"two");
    fs.remove_file("a").unwrap();
    assert_eq!(read_all(&fs, "b").unwrap(), b"two");
}

#[test]
fn directories_sync() {
    let (_d, fs) = folder();
    fs.create_dir("d").unwrap();
    write_all(&fs, "d/f", b"x").unwrap();
    fs.sync_dir("d").unwrap();
    fs.sync_dir("").unwrap();
    assert!(fs.sync_dir("missing").is_err());
}

#[test]
fn rename_replaces() {
    let (_d, fs) = folder();
    write_all(&fs, ".seedwell.x.tmp", b"new").unwrap();
    write_all(&fs, "x", b"old").unwrap();
    let ino = fs.lstat(".seedwell.x.tmp").unwrap().unwrap().inode;
    fs.rename(".seedwell.x.tmp", "x").unwrap();
    assert_eq!(fs.lstat(".seedwell.x.tmp").unwrap(), None);
    assert_eq!(fs.lstat("x").unwrap().unwrap().inode, ino);
    assert_eq!(read_all(&fs, "x").unwrap(), b"new");
}

#[test]
fn names_that_are_not_utf8_are_skipped() {
    let (d, fs) = folder();
    use std::os::unix::ffi::OsStrExt;
    let raw = std::ffi::OsStr::from_bytes(b"bad\xff");
    match std::fs::write(d.path().join(raw), b"x") {
        Ok(()) => {
            write_all(&fs, "good", b"").unwrap();
            let l = fs.read_dir("").unwrap();
            assert_eq!(l.entries.len(), 1);
            assert_eq!(l.entries[0].name, "good");
            assert_eq!(l.skipped, ["bad\u{FFFD}"]);
        }
        // APFS stores only UTF-8 names (EILSEQ); the listing logic is unit-tested in fs::disk.
        Err(e) if e.raw_os_error() == Some(92) || e.raw_os_error() == Some(84) => {
            eprintln!("this filesystem refuses non-UTF-8 names ({e}); nothing to skip");
        }
        Err(e) => panic!("creating a non-UTF-8 name: {e}"),
    }
}

#[test]
fn escaping_the_root_is_refused() {
    let (d, fs) = folder();
    let inner = d.path().join("inner");
    std::fs::create_dir(&inner).unwrap();
    std::fs::write(d.path().join("secret"), b"outside").unwrap();
    let fs_in = StdFs::new(&inner);
    for bad in ["../secret", "/etc/hosts", "a/../../secret", "./x", "a//b"] {
        assert_eq!(fs_in.lstat(bad).unwrap_err().kind(), io::ErrorKind::InvalidInput, "{bad}");
        assert!(fs_in.open_read(bad).is_err(), "{bad}");
        assert!(fs_in.open_rw(bad).is_err(), "{bad}");
    }
    assert!(fs_in.rename("../secret", "stolen").is_err());
    assert!(fs_in.copy("../secret", "stolen").is_err());
    assert!(fs_in.hard_link("../secret", "stolen").is_err());
    assert_eq!(fs_in.lstat("stolen").unwrap(), None);

    // A directory replaced by a symlink to the outside: writes and reads through it are refused.
    fs_in.symlink("..", "up").unwrap();
    assert_eq!(fs_in.open_read("up/secret").err().unwrap().kind(), io::ErrorKind::InvalidInput);
    assert_eq!(fs_in.open_rw("up/planted").err().unwrap().kind(), io::ErrorKind::InvalidInput);
    assert!(fs_in.remove_file("up/secret").is_err());
    assert!(d.path().join("secret").exists() && !d.path().join("planted").exists());
    drop(fs);
}

#[test]
fn case_probe_on_the_temp_volume() {
    let (d, fs) = folder();
    std::fs::create_dir(d.path().join(".seedwell")).unwrap();
    let ci = probe_case_insensitive(&fs).unwrap();
    // The temp volume is the boot volume: case-insensitive APFS on a default Mac.
    if cfg!(target_os = "macos") {
        eprintln!("temp volume case-insensitive: {ci}");
    }
    assert!(fs.read_dir(".seedwell").unwrap().entries.is_empty());
}
