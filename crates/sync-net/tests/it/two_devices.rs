//! Two or three services on loopback, each with its own state folder and synced folder (temp dirs only):
//! the whole stack, from the watcher to TLS and back.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::status::FolderState;
use trs_sync_net::{Keep, Options, SyncHandle};
use trs_sync_proto::members::Role;
use trs_sync_proto::ids::FolderId;

struct Device {
    h: SyncHandle,
    _state: tempfile::TempDir,
}

async fn device(name: &str) -> Device {
    let state = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
    let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
}

fn addr(d: &Device) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), d.h.port())
}

/// Waits (polling) until `ok` holds, or fails after `secs`.
async fn until(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn read(p: &Path) -> Option<Vec<u8>> {
    std::fs::read(p).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_file_syncs_both_ways_over_tls() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("hello.txt"), b"hello from A").unwrap();

    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let key = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Shared");
    b.h.join_folder(&key, dest.clone()).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();

    until(20, "B has A's file", || read(&dest.join("hello.txt")).as_deref() == Some(b"hello from A")).await;

    // A change on B comes back to A (found by B's watcher, announced over the session).
    std::fs::create_dir(dest.join("sub")).unwrap();
    std::fs::write(dest.join("sub/from-b.txt"), vec![7u8; 300_000]).unwrap();
    until(30, "A has B's file", || read(&fa.path().join("sub/from-b.txt")).map(|v| v.len()) == Some(300_000)).await;

    let snap = a.h.snapshot();
    assert_eq!(snap.devices.len(), 1, "one session");
    assert_eq!(snap.devices[0].name, "Beta");
    until(10, "both up to date", || {
        a.h.snapshot().folders[0].state == FolderState::UpToDate && b.h.snapshot().folders[0].state == FolderState::UpToDate
    })
    .await;
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_wrong_key_gets_nothing() {
    let (a, b) = (device("Alpha").await, device("Mallory").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("secret.txt"), b"not for Mallory").unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();

    // Mallory knows A's address and the folder id, but has another folder's key.
    let other = b.h.add_folder(fb.path().to_path_buf()).await.unwrap();
    let other_id: FolderId = other.id.parse().unwrap();
    b.h.add_address(&other_id, addr(&a)).unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(a.h.snapshot().devices.is_empty(), "no session without a shared folder");
    assert!(!fb.path().join("secret.txt").exists());
    let _ = id;
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_only_device_receives_and_its_changes_stay_local() {
    let (a, r) = (device("Alpha").await, device("Reader").await);
    let (fa, fr) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("doc.txt"), b"v1").unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let ro = a.h.create_invite(&id, Role::ReadOnly, 1, 1).unwrap().link;
    let dest = fr.path().join("Docs");
    r.h.join_folder(&ro, dest.clone()).await.unwrap();
    r.h.add_address(&id, addr(&a)).unwrap();
    until(20, "R has the file", || read(&dest.join("doc.txt")).as_deref() == Some(b"v1")).await;
    assert!(r.h.create_invite(&id, Role::ReadOnly, 1, 1).is_err(), "a reader cannot share");

    std::fs::write(dest.join("doc.txt"), b"reader's edit").unwrap();
    std::fs::write(fa.path().join("other.txt"), b"later from A").unwrap();
    until(20, "R still receives", || read(&dest.join("other.txt")).is_some()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(read(&fa.path().join("doc.txt")).as_deref(), Some(&b"v1"[..]), "A never takes the reader's edit");
    a.h.shutdown().await;
    r.h.shutdown().await;
}

/// Files in a folder's archive, below `.seedwell/Archive`.
fn archived(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.join(".seedwell/Archive")];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            if e.file_type().unwrap().is_dir() {
                stack.push(e.path());
            } else {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out
}

/// Both devices pull from each other at once. A pull holds its folder's core while it waits for the
/// other's answers; answering, and taking in the other's index, must not wait for that core, or each
/// waits for the other until requests time out (60 s).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_pulling_from_each_other_at_once_do_not_wait_on_each_other() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let key = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Shared");
    b.h.join_folder(&key, dest.clone()).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    std::fs::write(fa.path().join("ready"), b"1").unwrap();
    until(20, "B has the first file", || read(&dest.join("ready")).is_some()).await;

    // Big files on both while paused (distinct chunks: no local copy to reuse), then both go at once.
    for d in [&a, &b] {
        d.h.set_paused(&id, true).await.unwrap();
    }
    let big = |seed: u8| (0..48u32 << 20).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8 ^ seed).collect::<Vec<u8>>();
    std::fs::write(fa.path().join("from-a.bin"), big(1)).unwrap();
    std::fs::write(dest.join("from-b.bin"), big(2)).unwrap();
    let t = Instant::now();
    let (ra, rb) = tokio::join!(a.h.set_paused(&id, false), b.h.set_paused(&id, false));
    ra.unwrap();
    rb.unwrap();
    until(45, "both have both files", || {
        let len = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        len(&fa.path().join("from-b.bin")) == 48 << 20 && len(&dest.join("from-a.bin")) == 48 << 20
    })
    .await;
    assert!(t.elapsed() < Duration::from_secs(30), "took {:?}: the pulls waited on each other", t.elapsed());
    assert_eq!(read(&fa.path().join("from-b.bin")), Some(big(2)));
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_edits_make_one_conflict_copy_that_either_device_resolves() {
    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("plan.txt"), b"v1").unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let key = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Shared");
    b.h.join_folder(&key, dest.clone()).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(20, "B has the file", || read(&dest.join("plan.txt")).as_deref() == Some(b"v1")).await;

    // Both edit while B is paused; B's edit is newer, so A's becomes the conflict copy.
    b.h.set_paused(&id, true).await.unwrap();
    std::fs::write(fa.path().join("plan.txt"), b"from A").unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    std::fs::write(dest.join("plan.txt"), b"from B").unwrap();
    b.h.set_paused(&id, false).await.unwrap();

    let copy_on = |root: &Path| -> Option<String> {
        std::fs::read_dir(root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).find(|n| n.contains("(conflict, Alpha, "))
    };
    until(30, "one copy on both", || {
        copy_on(fa.path()).is_some_and(|c| read(&fa.path().join(&c)).as_deref() == Some(b"from A"))
            && copy_on(&dest) == copy_on(fa.path())
            && read(&fa.path().join("plan.txt")).as_deref() == Some(b"from B")
            && read(&dest.join("plan.txt")).as_deref() == Some(b"from B")
    })
    .await;
    let copy = copy_on(fa.path()).unwrap();
    for d in [&a, &b] {
        assert_eq!(d.h.conflicts(&id).await.unwrap(), vec![(copy.clone(), "plan.txt".to_string())]);
    }

    // B keeps A's version: it replaces the file everywhere, and B's own version is in B's archive.
    b.h.resolve_conflict(&id, &copy, Keep::Copy).await.unwrap();
    until(30, "A's version kept on both", || {
        [fa.path(), dest.as_path()].iter().all(|r| read(&r.join("plan.txt")).as_deref() == Some(b"from A") && copy_on(r).is_none())
    })
    .await;
    assert!(archived(&dest).iter().any(|n| n.starts_with("plan~")), "{:?}", archived(&dest));
    until(10, "no conflicts listed", || a.h.snapshot().folders[0].conflicts == 0 && b.h.snapshot().folders[0].conflicts == 0).await;
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keeping_both_gives_the_copy_a_plain_name_and_keeping_the_original_archives_the_copy() {
    let a = device("Alpha").await;
    let fa = tempfile::tempdir().unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    for (name, body) in [
        ("notes.txt", "mine"),
        ("notes (conflict, Beta, 2026-10-25 14.32).txt", "theirs"),
        ("todo", "mine"),
        ("todo (conflict, Beta, 2026-10-25 14.32)", "theirs"),
        ("todo (Beta, 2026-10-25 14.32)", "in the way"),
    ] {
        std::fs::write(fa.path().join(name), body).unwrap();
    }
    a.h.resolve_conflict(&id, "notes (conflict, Beta, 2026-10-25 14.32).txt", Keep::Both).await.unwrap();
    assert_eq!(read(&fa.path().join("notes (Beta, 2026-10-25 14.32).txt")).as_deref(), Some(&b"theirs"[..]));
    assert_eq!(read(&fa.path().join("notes.txt")).as_deref(), Some(&b"mine"[..]));
    a.h.resolve_conflict(&id, "todo (conflict, Beta, 2026-10-25 14.32)", Keep::Both).await.unwrap();
    assert_eq!(read(&fa.path().join("todo (Beta, 2026-10-25 14.32) 2")).as_deref(), Some(&b"theirs"[..]));

    std::fs::write(fa.path().join("plan (conflict, Beta, 2026-10-25 14.32).md"), "theirs").unwrap();
    a.h.resolve_conflict(&id, "plan (conflict, Beta, 2026-10-25 14.32).md", Keep::Original).await.unwrap();
    assert!(!fa.path().join("plan (conflict, Beta, 2026-10-25 14.32).md").exists());
    assert!(archived(fa.path()).iter().any(|n| n.starts_with("plan (conflict, Beta, 2026-10-25 14.32)~")));

    // Not a conflict copy, or not in the folder: refused.
    assert!(a.h.resolve_conflict(&id, "notes.txt", Keep::Copy).await.is_err());
    assert!(a.h.resolve_conflict(&id, "../x (conflict, B, 2026-10-25 14.32)", Keep::Copy).await.is_err());
    a.h.shutdown().await;
}
