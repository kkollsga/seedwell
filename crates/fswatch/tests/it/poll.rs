//! The polling backend, on every OS.

use std::time::Duration;

use trs_fswatch::{ActiveBackend, Backend, watch};

use crate::common::*;

const FAST: Backend = Backend::Poll { interval: Duration::from_millis(50) };

#[test]
fn polling_reports_create_modify_rename_delete() {
    create_modify_rename_delete(FAST);
}

#[test]
fn polling_reports_a_moved_root() {
    root_changed_when_the_root_is_moved(FAST);
}

#[test]
fn polling_reports_a_burst() {
    a_burst_is_reported_or_rescanned(FAST);
}

#[test]
fn polling_has_no_history() {
    a_token_without_history_means_a_full_rescan(FAST);
}

#[test]
fn polling_stops_promptly() {
    stop_is_prompt_under_a_burst(FAST);
}

#[test]
fn polling_reports_a_rename_as_two_halves() {
    let (_t, r) = root();
    std::fs::write(r.join("old"), b"data").expect("write");
    let w = watch(&r, opts(FAST)).expect("watch");
    assert_eq!(w.backend(), ActiveBackend::Poll);
    std::fs::rename(r.join("old"), r.join("new")).expect("rename");
    let evs = wait_for(&w, |e| path_event(e, "old").is_some() && path_event(e, "new").is_some());
    // The poller pairs the halves by inode; Windows gives it none, so there it is a remove and a create
    // (hints either way: the consumer stats both paths).
    use trs_fswatch::Hint::{Created, Removed, Renamed};
    let (old, new) = if cfg!(unix) { (Renamed, Renamed) } else { (Removed, Created) };
    assert_eq!(path_event(&evs, "old").map(|(k, _)| *k), Some(old));
    assert_eq!(path_event(&evs, "new").map(|(k, _)| *k), Some(new));
}

#[test]
fn polling_reports_the_root_coming_back() {
    let (t, r) = root();
    let w = watch(&r, opts(FAST)).expect("watch");
    let aside = t.path().join("aside");
    std::fs::rename(&r, &aside).expect("move root away");
    wait_for(&w, |e| e.contains(&trs_fswatch::RawEvent::RootChanged));
    std::fs::rename(&aside, &r).expect("move root back");
    wait_for(&w, |e| e.contains(&trs_fswatch::RawEvent::RootChanged));
    // Watching goes on from a fresh baseline.
    std::fs::write(r.join("after"), b"x").expect("write");
    wait_for(&w, |e| covers(e, "after"));
}
