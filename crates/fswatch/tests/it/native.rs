//! The native backend (FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on Windows).

use trs_fswatch::{ActiveBackend, Backend, RawEvent, watch};

use crate::common::*;

#[test]
fn uses_the_native_backend() {
    let (_t, r) = root();
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    let want = if cfg!(target_os = "macos") {
        ActiveBackend::FsEvents
    } else if cfg!(windows) {
        ActiveBackend::ReadDirectoryChanges
    } else {
        ActiveBackend::Inotify
    };
    assert_eq!(w.backend(), want);
    assert_eq!(w.root(), std::fs::canonicalize(&r).expect("canonicalize"));
}

#[test]
fn create_modify_rename_delete_are_reported() {
    create_modify_rename_delete(Backend::Auto);
}

#[test]
fn moving_the_root_is_reported() {
    root_changed_when_the_root_is_moved(Backend::Auto);
}

#[test]
fn a_burst_is_reported_or_rescanned_natively() {
    a_burst_is_reported_or_rescanned(Backend::Auto);
}

#[test]
fn a_foreign_token_means_a_full_rescan() {
    a_token_without_history_means_a_full_rescan(Backend::Auto);
}

#[test]
fn stop_is_clean_under_a_burst() {
    stop_is_prompt_under_a_burst(Backend::Auto);
    // FSEvents drops its last reference to the context on its own queue, a moment after the stream is
    // released when the machine is busy: wait for it, but not forever (that would be a leak).
    #[cfg(target_os = "macos")]
    {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while trs_fswatch::debug::live_streams() != 0 && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(trs_fswatch::debug::live_streams(), 0, "the stream still holds its state after stop");
    }
}

/// A link to the root elsewhere (a symlink; on Windows a directory junction, as `%USERPROFILE%\.claude\skills` was made
/// to a synced folder): what is written through it is reported under the root, relative to it, never under the
/// link's path. So the folder matches its ignore patterns against the right path, whichever way a file was written.
#[test]
fn changes_made_through_a_link_to_the_root_are_reported_relative_to_the_root() {
    let (t, r) = root();
    let view = t.path().join("view");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&r, &view).expect("symlink");
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd").arg("/C").arg("mklink").arg("/J").arg(&view).arg(&r).output().expect("mklink");
        assert!(out.status.success(), "mklink /J: {out:?}");
    }
    let w = watch(&r, opts(Backend::Auto)).expect("watch");
    std::fs::create_dir_all(view.join("synced").join("u_u")).expect("mkdir through the link");
    std::fs::write(view.join("synced").join("u_u").join("a.md"), b"written through the link").expect("write through the link");
    let evs = wait_for(&w, |e| covers(e, "synced/u_u/a.md"));
    for e in &evs {
        let rel = match e {
            RawEvent::Path { rel, .. } | RawEvent::Rescan { rel: Some(rel), .. } => rel,
            _ => continue,
        };
        assert!(rel.is_relative(), "{e:?}");
        assert!(rel.components().all(|c| matches!(c, std::path::Component::Normal(_))), "{e:?}");
        let first = rel.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned());
        assert!(first.is_none_or(|f| f == "synced"), "reported under the root, not the link: {e:?}");
    }
    w.stop();
}

#[test]
fn watching_a_file_or_a_missing_path_fails() {
    let (_t, r) = root();
    std::fs::write(r.join("f"), b"").expect("write");
    assert!(watch(&r.join("f"), opts(Backend::Auto)).is_err());
    assert!(watch(&r.join("missing"), opts(Backend::Auto)).is_err());
}

#[test]
fn a_split_watcher_reports_on_its_channel_and_dropping_the_guard_ends_it() {
    let (_t, r) = root();
    let (events, guard) = watch(&r, opts(Backend::Auto)).expect("watch").split();
    assert_eq!(guard.root(), std::fs::canonicalize(&r).expect("canonicalize"));
    std::fs::write(r.join("latest.json"), b"{}").expect("write");
    let got = events.recv_timeout(std::time::Duration::from_secs(10));
    assert!(got.is_ok(), "an event for the new file: {got:?}");
    drop(guard);
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match events.recv_timeout(std::time::Duration::from_millis(200)) {
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            _ => assert!(std::time::Instant::now() < until, "the channel stays open after the guard is dropped"),
        }
    }
}
