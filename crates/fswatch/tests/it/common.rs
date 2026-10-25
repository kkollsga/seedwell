//! Scenarios shared by every backend. They wait with timeouts (never fixed sleeps that assume a fast
//! machine) and accept a rescan wherever the contract allows one instead of a path event.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use trs_fswatch::{Backend, Hint, RawEvent, RescanReason, ResumeToken, WatchOptions, Watcher, watch};

/// Generous: on an idle machine everything arrives in well under a second.
pub const TIMEOUT: Duration = Duration::from_secs(20);

pub fn opts(backend: Backend) -> WatchOptions {
    WatchOptions { backend, latency: Duration::from_millis(50), resume_interval: Duration::from_millis(100), ..WatchOptions::default() }
}

/// Collects events until `done` says so. Panics with what was seen on timeout.
pub fn wait_for(w: &Watcher, mut done: impl FnMut(&[RawEvent]) -> bool) -> Vec<RawEvent> {
    let deadline = Instant::now() + TIMEOUT;
    let mut seen = Vec::new();
    while !done(&seen) {
        let left = deadline.saturating_duration_since(Instant::now());
        match w.events().recv_timeout(left) {
            Ok(ev) => seen.push(ev),
            Err(RecvTimeoutError::Timeout) => panic!("timed out; saw {seen:#?}"),
            Err(RecvTimeoutError::Disconnected) => panic!("watcher ended; saw {seen:#?}"),
        }
    }
    seen
}

/// Whether `rel` was reported, as a path event or as part of a rescan.
pub fn covers(evs: &[RawEvent], rel: &str) -> bool {
    let rel = Path::new(rel);
    evs.iter().any(|e| match e {
        RawEvent::Path { rel: r, .. } => r == rel,
        RawEvent::Rescan { rel: None, .. } | RawEvent::RootChanged => true,
        RawEvent::Rescan { rel: Some(d), .. } => rel.starts_with(d),
        _ => false,
    })
}

pub fn path_event<'a>(evs: &'a [RawEvent], rel: &str) -> Option<(&'a Hint, &'a Option<bool>)> {
    evs.iter().rev().find_map(|e| match e {
        RawEvent::Path { rel: r, kind, is_dir } if r == Path::new(rel) => Some((kind, is_dir)),
        _ => None,
    })
}

/// A root inside a fresh temporary folder (a subfolder, so the root itself can be renamed).
pub fn root() -> (tempfile::TempDir, PathBuf) {
    let t = tempfile::tempdir().expect("tempdir");
    let r = t.path().join("root");
    std::fs::create_dir(&r).expect("mkdir root");
    (t, r)
}

pub fn create_modify_rename_delete(backend: Backend) {
    let (_t, r) = root();
    let w = watch(&r, opts(backend)).expect("watch");

    std::fs::write(r.join("a.txt"), b"one").expect("write");
    wait_for(&w, |e| covers(e, "a.txt"));

    std::fs::write(r.join("a.txt"), b"one two").expect("rewrite");
    wait_for(&w, |e| covers(e, "a.txt"));

    std::fs::create_dir(r.join("sub")).expect("mkdir");
    let evs = wait_for(&w, |e| covers(e, "sub"));
    if let Some((_, is_dir)) = path_event(&evs, "sub") {
        assert_ne!(*is_dir, Some(false), "a directory reported as a file");
    }
    std::fs::write(r.join("sub/x"), b"x").expect("write in sub");
    wait_for(&w, |e| covers(e, "sub/x"));

    std::fs::rename(r.join("a.txt"), r.join("sub/b.txt")).expect("rename");
    wait_for(&w, |e| covers(e, "a.txt") && covers(e, "sub/b.txt"));

    std::fs::remove_file(r.join("sub/b.txt")).expect("remove");
    let evs = wait_for(&w, |e| covers(e, "sub/b.txt"));
    if let Some((kind, _)) = path_event(&evs, "sub/b.txt") {
        assert!(kind.is_removal(), "removal reported as {kind:?}");
    }
    w.stop();
}

pub fn root_changed_when_the_root_is_moved(backend: Backend) {
    let (t, r) = root();
    let w = watch(&r, opts(backend)).expect("watch");
    std::fs::rename(&r, t.path().join("moved")).expect("rename root");
    wait_for(&w, |e| e.contains(&RawEvent::RootChanged));
    w.stop();
}

pub fn a_burst_is_reported_or_rescanned(backend: Backend) {
    let (_t, r) = root();
    let w = watch(&r, opts(backend)).expect("watch");
    let n = 1500;
    for i in 0..n {
        std::fs::write(r.join(format!("f{i}")), b"x").expect("write");
    }
    let names: Vec<String> = (0..n).map(|i| format!("f{i}")).collect();
    let mut missing: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
    wait_for(&w, |e| {
        if let Some(last) = e.last() {
            match last {
                RawEvent::Path { rel, .. } => {
                    missing.remove(rel.to_str().unwrap_or(""));
                }
                RawEvent::Rescan { rel: None, .. } => missing.clear(),
                _ => {}
            }
        }
        missing.is_empty()
    });
    w.stop();
}

pub fn a_token_without_history_means_a_full_rescan(backend: Backend) {
    let (_t, r) = root();
    let since = Some(ResumeToken { event_id: 1, volume_uuid: [0xEE; 16] });
    let w = watch(&r, WatchOptions { since, ..opts(backend) }).expect("watch");
    let evs = wait_for(&w, |e| e.contains(&RawEvent::HistoryDone));
    assert_eq!(evs[0], RawEvent::Rescan { rel: None, reason: RescanReason::NoHistory });
    assert_eq!(evs[1], RawEvent::HistoryDone);
    w.stop();
}

pub fn stop_is_prompt_under_a_burst(backend: Backend) -> Duration {
    let (_t, r) = root();
    let w = watch(&r, opts(backend)).expect("watch");
    let running = Arc::new(AtomicBool::new(true));
    let writer = {
        let (r, running) = (r.clone(), running.clone());
        std::thread::spawn(move || {
            let mut i = 0u64;
            while running.load(Ordering::Relaxed) {
                let _ = std::fs::write(r.join(format!("b{}", i % 400)), i.to_le_bytes());
                i += 1;
            }
        })
    };
    // Stop while events are flowing.
    wait_for(&w, |e| e.len() >= 50);
    let t0 = Instant::now();
    w.stop();
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(5), "stop took {took:?}");
    let after = trs_fswatch::debug::callbacks();
    // The writer keeps going for a while: no callback may run any more.
    let until = Instant::now() + Duration::from_millis(300);
    while Instant::now() < until {
        std::thread::yield_now();
    }
    assert_eq!(trs_fswatch::debug::callbacks(), after, "a callback ran after stop returned");
    running.store(false, Ordering::Relaxed);
    writer.join().expect("writer");
    took
}
