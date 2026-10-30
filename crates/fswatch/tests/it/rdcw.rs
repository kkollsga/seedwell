//! The Windows backend (ReadDirectoryChangesW): what it reports that the others report differently.

use std::path::Path;

use trs_fswatch::{Backend, Hint, RawEvent, RescanReason, watch};

use crate::common::*;

/// A rename inside the root is two Renamed halves, without inodes to pair them.
#[test]
fn a_rename_is_two_renamed_halves() {
    let (_t, r) = root();
    std::fs::write(r.join("old"), b"data").expect("write");
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    std::fs::rename(r.join("old"), r.join("new")).expect("rename");
    let evs = wait_for(&w, |e| path_event(e, "old").is_some() && path_event(e, "new").is_some());
    assert_eq!(path_event(&evs, "old").map(|(k, _)| *k), Some(Hint::Renamed));
    assert_eq!(path_event(&evs, "new"), Some((&Hint::Renamed, &Some(false))));
}

/// A folder moved in from outside the root is reported, and so is a rescan of what it holds.
#[test]
fn a_folder_moved_in_is_rescanned() {
    let (t, r) = root();
    let outside = t.path().join("outside");
    std::fs::create_dir_all(outside.join("deep")).expect("mkdir");
    std::fs::write(outside.join("deep").join("f.txt"), b"x").expect("write");
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    std::fs::rename(&outside, r.join("came")).expect("move in");
    let evs = wait_for(&w, |e| e.iter().any(|ev| matches!(ev, RawEvent::Rescan { rel: Some(d), reason: RescanReason::NewDirectory } if d == Path::new("came"))));
    assert_eq!(path_event(&evs, "came").map(|(_, d)| *d), Some(Some(true)), "{evs:?}");
}

/// Paths below the top level arrive with this system's separators, deleted subtree included.
#[test]
fn a_nested_delete_is_reported_by_path() {
    let (_t, r) = root();
    std::fs::create_dir_all(r.join("a").join("b")).expect("mkdir");
    std::fs::write(r.join("a").join("b").join("f.txt"), b"x").expect("write");
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    std::fs::remove_dir_all(r.join("a").join("b")).expect("remove");
    let evs = wait_for(&w, |e| covers(e, "a/b/f.txt") && covers(e, "a/b"));
    if let Some((kind, _)) = path_event(&evs, "a/b/f.txt") {
        assert_eq!(*kind, Hint::Removed);
    }
}

/// A junction inside the root is reported as itself, never followed: a change in its target outside
/// the root makes no event (the scanner does not follow links either).
#[test]
fn a_junction_is_not_followed() {
    let (t, r) = root();
    let target = t.path().join("target");
    std::fs::create_dir(&target).expect("mkdir");
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    let o = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(r.join("j")).arg(&target).output().expect("mklink");
    assert!(o.status.success(), "{o:?}");
    // (mklink /J makes a folder and then turns it into a junction, so the first record may see a folder:
    // a hint only, the scanner stats it.)
    wait_for(&w, |e| path_event(e, "j").is_some());
    std::fs::write(target.join("inside.txt"), b"x").expect("write in target");
    std::fs::write(r.join("marker"), b"x").expect("marker");
    let evs = wait_for(&w, |e| path_event(e, "marker").is_some());
    assert!(path_event(&evs, "j/inside.txt").is_none(), "followed the junction: {evs:?}");
}

/// While no read is posted the system's buffer overflows under a burst; that is a full rescan, never a
/// silent loss.
#[test]
fn an_overflow_is_a_full_rescan() {
    let (_t, r) = root();
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    std::fs::write(r.join("first"), b"x").expect("write");
    trs_fswatch::debug::hold_reads(true);
    wait_for(&w, |e| path_event(e, "first").is_some());
    for i in 0..5000 {
        std::fs::write(r.join(format!("burst-{i:05}-with-a-long-enough-name.txt")), b"x").expect("write");
    }
    trs_fswatch::debug::hold_reads(false);
    wait_for(&w, |e| e.iter().any(|ev| matches!(ev, RawEvent::Rescan { rel: None, reason: RescanReason::Dropped })));
}
