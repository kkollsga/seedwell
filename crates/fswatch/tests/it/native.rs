//! The native backend (FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on Windows).

use trs_fswatch::{ActiveBackend, Backend, watch};

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

#[test]
fn watching_a_file_or_a_missing_path_fails() {
    let (_t, r) = root();
    std::fs::write(r.join("f"), b"").expect("write");
    assert!(watch(&r.join("f"), opts(Backend::Auto)).is_err());
    assert!(watch(&r.join("missing"), opts(Backend::Auto)).is_err());
}
