//! FSEvents only: resume tokens and history replay.

use trs_fswatch::{Backend, RawEvent, RescanReason, ResumeToken, WatchOptions, Watcher, watch};

use crate::common::*;

fn next_token(w: &Watcher) -> ResumeToken {
    let evs = wait_for(w, |e| matches!(e.last(), Some(RawEvent::Resume(_))));
    match evs.last() {
        Some(RawEvent::Resume(t)) => *t,
        _ => unreachable!(),
    }
}

#[test]
fn a_token_replays_changes_made_while_stopped() {
    let (_t, r) = root();
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    std::fs::write(r.join("before.txt"), b"1").expect("write");
    wait_for(&w, |e| covers(e, "before.txt"));
    // The first token after the event covers it.
    let token = next_token(&w);
    w.stop();
    assert_eq!(ResumeToken::from_bytes(&token.to_bytes()), Some(token));

    std::fs::write(r.join("while_stopped.txt"), b"2").expect("write");
    std::fs::create_dir(r.join("new_dir")).expect("mkdir");
    std::fs::write(r.join("new_dir/inner"), b"3").expect("write");
    std::fs::rename(r.join("before.txt"), r.join("after.txt")).expect("rename");

    let w = watch(&r, WatchOptions { since: Some(token), ..opts(Backend::Auto) }).expect("watch since");
    let evs = wait_for(&w, |e| e.contains(&RawEvent::HistoryDone));
    assert!(!evs.contains(&RawEvent::Rescan { rel: None, reason: RescanReason::NoHistory }), "{evs:#?}");
    // History keeps file-level detail (replayed per path, in path order rather than time order).
    for p in ["while_stopped.txt", "new_dir/inner", "before.txt", "after.txt"] {
        assert!(path_event(&evs, p).is_some(), "{p} not replayed: {evs:#?}");
    }
    assert_eq!(path_event(&evs, "after.txt").map(|(k, _)| k.is_removal()), Some(true));
    // Live events follow the replay, and tokens resume after it.
    std::fs::write(r.join("live.txt"), b"4").expect("write");
    wait_for(&w, |e| covers(e, "live.txt"));
    let later = next_token(&w);
    assert!(later.event_id > token.event_id);
    assert_eq!(later.volume_uuid, token.volume_uuid);
}

#[test]
fn a_token_from_the_future_means_a_full_rescan() {
    let (_t, r) = root();
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    let token = next_token(&w);
    w.stop();
    let future = ResumeToken { event_id: u64::MAX - 1, ..token };
    let w = watch(&r, WatchOptions { since: Some(future), ..opts(Backend::Auto) }).expect("watch since");
    let evs = wait_for(&w, |e| e.contains(&RawEvent::HistoryDone));
    assert_eq!(evs[0], RawEvent::Rescan { rel: None, reason: RescanReason::NoHistory });
}

#[test]
fn the_token_advances_while_the_root_is_quiet() {
    let (_t, r) = root();
    let elsewhere = tempfile::tempdir().expect("tempdir");
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    let first = next_token(&w);
    // Activity outside the root moves the host's event counter; the idle advance picks it up.
    let mut i = 0;
    let advanced = wait_for(&w, |e| {
        i += 1;
        let _ = std::fs::write(elsewhere.path().join(format!("n{}", i % 10)), b"x");
        matches!(e.last(), Some(RawEvent::Resume(t)) if t.event_id > first.event_id)
    });
    assert!(advanced.iter().all(|e| matches!(e, RawEvent::Resume(_))), "events outside the root: {advanced:#?}");
}
