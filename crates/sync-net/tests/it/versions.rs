//! Devices of different versions on loopback: one that speaks a newer protocol too (and has features this build
//! does not know) still syncs at the version both speak; one that no longer speaks this build's is refused, and
//! the joining side says which needs an update.

use trs_sync_net::status::FolderState;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;
use trs_sync_proto::msg::{MIN_PROTOCOL, PROTOCOL};

use crate::utp::{Device, addr, device_with, read, until};

fn state(d: &Device, id: &FolderId) -> Option<(FolderState, Option<String>)> {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).map(|f| (f.state, f.error))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_newer_device_syncs_at_the_version_both_speak_and_one_too_new_is_told_to_update() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = device_with("Owner", |_| {}).await;
    std::fs::write(dirs[0].path().join("seed.txt"), b"from the owner").unwrap();
    let id: FolderId = a.h.add_folder(dirs[0].path().to_path_buf()).await.unwrap().id.parse().unwrap();

    // Speaks this version and the next, with features this build does not have.
    let newer = device_with("Newer", |o| {
        o.protocol = (MIN_PROTOCOL, PROTOCOL + 1);
        o.features = 0b1010;
    })
    .await;
    newer.h.join_folder(&a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[1].path().join("F")).await.unwrap();
    newer.h.add_address(&id, addr(&a)).unwrap();
    until(30, "the newer device has the file", || read(&dirs[1].path().join("F/seed.txt")).is_some()).await;
    std::fs::write(dirs[1].path().join("F/back.txt"), b"from the newer one").unwrap();
    until(30, "the owner has its change", || read(&dirs[0].path().join("back.txt")).is_some()).await;

    // Speaks only a later version: refused, and the joining side says the owner's Seedwell needs an update.
    let too_new = device_with("Future", |o| o.protocol = (PROTOCOL + 1, PROTOCOL + 1)).await;
    too_new.h.join_folder(&a.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link, dirs[2].path().join("F")).await.unwrap();
    too_new.h.add_address(&id, addr(&a)).unwrap();
    until(20, "told why", || {
        matches!(state(&too_new, &id), Some((FolderState::Error, Some(e))) if e.contains("Owner runs an older Seedwell that cannot sync with this one: update it"))
    })
    .await;
    assert!(read(&dirs[2].path().join("F/seed.txt")).is_none());
    for d in [&a, &newer, &too_new] {
        d.h.shutdown().await;
    }
}

/// A service on a state folder that outlives it (to start it again, as another version).
async fn start_at(state: &std::path::Path, name: &str, protocol: (u32, u32)) -> trs_sync_net::SyncHandle {
    let secrets = std::sync::Arc::new(trs_sync_net::secrets::FileSecrets::new(state.join("secrets")));
    let mut opts = trs_sync_net::Options::new(state.to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    opts.protocol = protocol;
    trs_sync_net::SyncHandle::start(opts).await.unwrap()
}

/// Stops `h` and starts its state folder again in this process, as `protocol`: its `folders` open at once
/// (`shutdown` returns once they are closed).
async fn restart(h: trs_sync_net::SyncHandle, state: &std::path::Path, name: &str, protocol: (u32, u32), folders: usize) -> trs_sync_net::SyncHandle {
    h.shutdown().await;
    let h = start_at(state, name, protocol).await;
    assert_eq!(h.snapshot().folders.len(), folders, "{name}'s folders open again right after shutdown");
    h
}

fn issues(h: &trs_sync_net::SyncHandle, id: &FolderId) -> Vec<String> {
    h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).map(|f| f.issues.into_iter().map(|(_, r)| r).collect()).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_that_cannot_talk_any_more_is_listed_with_the_folders_issues_until_it_can() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let now = (MIN_PROTOCOL, PROTOCOL);
    let a = start_at(dirs[0].path(), "Owner", now).await;
    let folder = dirs[2].path().join("A");
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::write(folder.join("seed.txt"), b"x").unwrap();
    let id: FolderId = a.add_folder(folder).await.unwrap().id.parse().unwrap();
    let b = start_at(dirs[1].path(), "Beta", now).await;
    b.join_folder(&a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[2].path().join("B")).await.unwrap();
    b.add_address(&id, std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port())).unwrap();
    until(30, "B has the file", || read(&dirs[2].path().join("B/seed.txt")).is_some()).await;

    // B comes back as a Seedwell that only speaks a later version: both folders say so.
    let b = restart(b, dirs[1].path(), "Beta", (PROTOCOL + 1, PROTOCOL + 1), 1).await;
    b.add_address(&id, std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port())).unwrap();
    until(30, "B's folder lists the owner", || issues(&b, &id).iter().any(|i| i == "Owner runs an older Seedwell that cannot sync with this one: update it")).await;
    until(30, "the owner's folder lists B", || issues(&a, &id).iter().any(|i| i == "Beta runs a newer Seedwell that this one cannot sync with: update this Mac")).await;

    // Back to a version both speak: the issues go.
    let b = restart(b, dirs[1].path(), "Beta", now, 1).await;
    b.add_address(&id, std::net::SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), a.port())).unwrap();
    std::fs::write(dirs[2].path().join("A/again.txt"), b"y").unwrap();
    until(60, "B syncs again", || read(&dirs[2].path().join("B/again.txt")).is_some()).await;
    until(10, "no version issues", || issues(&a, &id).is_empty() && issues(&b, &id).iter().all(|i| !i.contains("Seedwell"))).await;
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn folders_of_the_test_builds_are_forgotten_at_start_and_keep_their_files() {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let now = (MIN_PROTOCOL, PROTOCOL);
    let h = start_at(state.path(), "Mac", now).await;
    let mut ids = Vec::new();
    for name in ["Old file", "Old link", "Fine"] {
        let dir = work.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"kept").unwrap();
        ids.push(h.add_folder(dir).await.unwrap().id.parse::<FolderId>().unwrap());
    }
    h.shutdown().await;
    drop(h);
    // One with a membership file of the earlier format, one with a link of the earlier format (no version byte).
    std::fs::write(state.path().join("sync").join(format!("{}.members", ids[0])), b"SWM2").unwrap();
    use trs_sync_net::secrets::SecretStore as _;
    let secrets = trs_sync_net::secrets::FileSecrets::new(state.path().join("secrets"));
    secrets.put(&format!("links-{}", ids[1]), b"seedwell://sync/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();

    let h = start_at(state.path(), "Mac", now).await;
    assert!(h.snapshot().folders.len() <= 1, "the two of the test builds are forgotten at once");
    let h = restart(h, state.path(), "Mac", now, 1).await;
    let left: Vec<String> = h.snapshot().folders.into_iter().map(|f| f.name).collect();
    assert_eq!(left, vec!["Fine".to_string()]);
    for name in ["Old file", "Old link"] {
        assert!(work.path().join(name).join("a.txt").exists(), "{name}: the files stay");
    }
    assert!(!state.path().join("sync").join(format!("{}.members", ids[0])).exists());
    h.shutdown().await;
}
