//! *Read only* (SPEC §7G.1): the folder's owners' setting. While it is on only the owners' changes count: every other
//! member (an admin, a member by link, one of the user's own devices) holds its changes back, and no device takes one
//! of theirs; members still pass the owners' versions on to each other. Nobody's role changes.

use std::path::Path;
use std::time::Duration;

use seedwell_sync_net::service::HELD_READ_ONLY;
use seedwell_sync_proto::ids::FolderId;
use seedwell_sync_proto::keys::Access;
use seedwell_sync_proto::members::Role;

use crate::utp::{Device, addr, device_with, read, until};

async fn device(name: &str) -> Device {
    device_with(name, |_| {}).await
}

/// The owner's folder (`doc.txt`, `other.txt`, `third.txt`), an admin (B) and a member (C) that have it, reaching each
/// other too.
struct Shared {
    a: Device,
    b: Device,
    c: Device,
    id: FolderId,
    dirs: Vec<tempfile::TempDir>,
}

impl Shared {
    fn root(&self, i: usize) -> std::path::PathBuf {
        if i == 0 { self.dirs[0].path().to_path_buf() } else { self.dirs[i].path().join("F") }
    }
}

async fn shared() -> Shared {
    let (a, b, c) = (device("Owner").await, device("Admin").await, device("Member").await);
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    for (n, body) in [("doc.txt", "v1"), ("other.txt", "other"), ("third.txt", "third")] {
        std::fs::write(dirs[0].path().join(n), body).unwrap();
    }
    let id: FolderId = a.h.add_folder(dirs[0].path().to_path_buf()).await.unwrap().id.parse().unwrap();
    for (d, dir, role) in [(&b, &dirs[1], Role::Admin), (&c, &dirs[2], Role::ReadWrite)] {
        d.h.join_folder(&a.h.create_invite(&id, role, 1, 7).unwrap().link, dir.path().join("F")).await.unwrap();
        d.h.add_address(&id, addr(&a)).unwrap();
    }
    for (d, dir) in [(&b, &dirs[1]), (&c, &dirs[2])] {
        until(40, "the member has the files", || read(&dir.path().join("F/third.txt")).is_some() && a.h.members(&id).is_ok_and(|m| m.iter().any(|x| x.device == d.h.device_id()))).await;
    }
    b.h.add_address(&id, addr(&c)).unwrap();
    c.h.add_address(&id, addr(&b)).unwrap();
    Shared { a, b, c, id, dirs }
}

fn status_of(d: &Device, id: &FolderId) -> seedwell_sync_net::status::FolderStatus {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).unwrap()
}

fn has(root: &Path, name: &str) -> Option<Vec<u8>> {
    read(&root.join(name))
}

async fn shutdown(devs: &[&Device]) {
    for d in devs {
        d.h.shutdown().await;
    }
}

/// Only the owner may switch Read only, and every member reads it the same. Fails when an admin or a member switches
/// it (on or off), or a member does not see the owner's setting.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_the_owner_switches_read_only_and_every_member_sees_it() {
    let s = shared().await;
    for d in [&s.b, &s.c] {
        assert!(d.h.set_read_only(&s.id, true).is_err(), "a member switched it on");
    }
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!([&s.a, &s.b, &s.c].iter().all(|d| !d.h.read_only(&s.id)));
    s.a.h.set_read_only(&s.id, true).unwrap();
    until(30, "every member sees it", || [&s.b, &s.c].iter().all(|d| d.h.read_only(&s.id))).await;
    assert!(s.b.h.set_read_only(&s.id, false).is_err(), "an admin switched it off");
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!([&s.a, &s.b, &s.c].iter().all(|d| d.h.read_only(&s.id)));
    for d in [&s.b, &s.c] {
        assert!(s.a.h.members(&s.id).unwrap().iter().any(|m| m.device == d.h.device_id() && m.role != Role::ReadOnly), "a role changed");
    }
    shutdown(&[&s.a, &s.b, &s.c]).await;
}

/// While it is on, a member's edit, new file, delete and rename reach neither the owner nor another member; the member
/// keeps them, says why, and still takes the owner's changes. Fails when any of them reaches another device, when the
/// member's own copy is changed back, or when its page does not say it holds back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn while_read_only_a_members_changes_stay_on_it_and_it_says_why() {
    let s = shared().await;
    s.a.h.set_read_only(&s.id, true).unwrap();
    until(30, "B holds back", || status_of(&s.b, &s.id).issues.iter().any(|(_, w)| w == HELD_READ_ONLY)).await;
    assert_eq!(status_of(&s.b, &s.id).access, Access::Read, "its page shows it writes nothing");
    let b = s.root(1);
    std::fs::write(b.join("doc.txt"), "B's edit").unwrap();
    std::fs::write(b.join("new.txt"), "B's file").unwrap();
    std::fs::remove_file(b.join("other.txt")).unwrap();
    std::fs::rename(b.join("third.txt"), b.join("renamed.txt")).unwrap();
    s.b.h.rescan(&s.id).unwrap();
    // The owner's changes still come.
    std::fs::write(s.root(0).join("owner.txt"), "the owner's").unwrap();
    until(40, "B and C have the owner's new file", || has(&b, "owner.txt").is_some() && has(&s.root(2), "owner.txt").is_some()).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    for i in [0, 2] {
        let r = s.root(i);
        assert_eq!(has(&r, "doc.txt").as_deref(), Some(&b"v1"[..]), "B's edit reached device {i}");
        assert!(has(&r, "new.txt").is_none() && has(&r, "renamed.txt").is_none(), "B's new file or rename reached device {i}");
        assert!(has(&r, "other.txt").is_some() && has(&r, "third.txt").is_some(), "B's delete or rename reached device {i}");
    }
    assert_eq!(has(&b, "doc.txt").as_deref(), Some(&b"B's edit"[..]), "B's own edit was undone");
    shutdown(&[&s.a, &s.b, &s.c]).await;
}

/// While it is on, the owner's change reaches every member, and a member passes it on (as a seeder): a device that
/// joins through an admin, the owner away, gets the owner's version from that admin; and an admin still admits it.
/// Fails when the owner's change does not reach a member, an admin cannot admit, or a member's relay of the owner's
/// version is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn while_read_only_the_owners_change_reaches_everyone_members_relay_it_and_an_admin_still_admits() {
    let s = shared().await;
    s.a.h.set_read_only(&s.id, true).unwrap();
    until(30, "B and C see it", || [&s.b, &s.c].iter().all(|d| d.h.read_only(&s.id))).await;
    std::fs::write(s.root(0).join("doc.txt"), "v2 by the owner").unwrap();
    until(40, "B and C have the owner's change", || [1, 2].iter().all(|i| has(&s.root(*i), "doc.txt").as_deref() == Some(&b"v2 by the owner"[..]))).await;
    // The owner goes away; the admin admits a new member, which hears only from the admin.
    s.a.h.shutdown().await;
    let d = device("New").await;
    let dir = tempfile::tempdir().unwrap();
    d.h.join_folder(&s.b.h.create_invite(&s.id, Role::ReadWrite, 1, 7).unwrap().link, dir.path().join("F")).await.unwrap();
    d.h.add_address(&s.id, addr(&s.b)).unwrap();
    until(40, "D has the owner's version, from the admin", || has(&dir.path().join("F"), "doc.txt").as_deref() == Some(&b"v2 by the owner"[..])).await;
    assert!(d.h.read_only(&s.id), "D sees Read only");
    shutdown(&[&s.b, &s.c, &d]).await;
}

/// While it is on, a member whose copy of a file differs from the owner's serves that file to nobody (only chunks that
/// hash to the owner's), so a device that hears only from that member never gets the member's content. Fails when a
/// member's changed copy reaches another device.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn while_read_only_a_members_changed_copy_is_served_to_nobody() {
    let s = shared().await;
    s.a.h.set_read_only(&s.id, true).unwrap();
    until(30, "B holds back", || s.b.h.read_only(&s.id)).await;
    std::fs::write(s.root(1).join("doc.txt"), "B's changed copy").unwrap();
    s.b.h.rescan(&s.id).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    s.a.h.shutdown().await;
    s.c.h.shutdown().await;
    let d = device("New").await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("F");
    d.h.join_folder(&s.b.h.create_invite(&s.id, Role::ReadWrite, 1, 7).unwrap().link, root.clone()).await.unwrap();
    d.h.add_address(&s.id, addr(&s.b)).unwrap();
    until(40, "D has the files B holds as the owner's", || has(&root, "other.txt").is_some()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_ne!(has(&root, "doc.txt").as_deref(), Some(&b"B's changed copy"[..]), "B's changed copy was served");
    shutdown(&[&s.b, &d]).await;
}

/// Turned off, every member sends what it held back, as ordinary changes; where the owner changed the same file the
/// usual conflict rule keeps both. Fails when a held change never arrives, or one of the two clashing versions is lost.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_read_only_off_sends_what_was_held_and_a_clash_keeps_both() {
    let s = shared().await;
    s.a.h.set_read_only(&s.id, true).unwrap();
    until(30, "B holds back", || status_of(&s.b, &s.id).issues.iter().any(|(_, w)| w == HELD_READ_ONLY)).await;
    std::fs::write(s.root(1).join("new.txt"), "held on B").unwrap();
    std::fs::write(s.root(1).join("doc.txt"), "B's doc").unwrap();
    s.b.h.rescan(&s.id).unwrap();
    std::fs::write(s.root(0).join("doc.txt"), "the owner's doc").unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(has(&s.root(0), "new.txt").is_none(), "held while on");
    s.a.h.set_read_only(&s.id, false).unwrap();
    until(40, "the owner and C have B's held file", || [0, 2].iter().all(|i| has(&s.root(*i), "new.txt").as_deref() == Some(&b"held on B"[..]))).await;
    let both = |root: &Path| {
        let all: Vec<Vec<u8>> = std::fs::read_dir(root).unwrap().flatten().filter_map(|e| std::fs::read(e.path()).ok()).collect();
        all.iter().any(|x| x == b"B's doc") && all.iter().any(|x| x == b"the owner's doc")
    };
    until(40, "the owner keeps both versions of doc.txt", || both(&s.root(0))).await;
    assert!(!status_of(&s.b, &s.id).issues.iter().any(|(_, w)| w == HELD_READ_ONLY), "B still says it holds back");
    shutdown(&[&s.a, &s.b, &s.c]).await;
}

/// Turned off, a member's held edit of a file it had received and a held delete reach every device, though nobody else
/// changed those files; also from a member that was away when it went off and starts again later. Fails when a held
/// edit or delete of an existing file stays on the member.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn turning_read_only_off_sends_held_edits_and_deletes_of_existing_files_also_after_a_restart() {
    let s = shared().await;
    s.a.h.set_read_only(&s.id, true).unwrap();
    for d in [&s.b, &s.c] {
        until(30, "the member holds back", || status_of(d, &s.id).issues.iter().any(|(_, w)| w == HELD_READ_ONLY)).await;
    }
    // B edits a received file; C deletes one, then goes away.
    std::fs::write(s.root(1).join("other.txt"), "B's held edit").unwrap();
    s.b.h.rescan(&s.id).unwrap();
    std::fs::remove_file(s.root(2).join("third.txt")).unwrap();
    s.c.h.rescan(&s.id).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let Shared { a, b, c, id, dirs } = s;
    // C is away (stopped, its port gone) while the owner turns Read only off.
    c.h.shutdown().await;
    a.h.set_read_only(&id, false).unwrap();
    let root = |i: usize| if i == 0 { dirs[0].path().to_path_buf() } else { dirs[i].path().join("F") };
    until(40, "B's held edit reaches the owner", || has(&root(0), "other.txt").as_deref() == Some(&b"B's held edit"[..])).await;
    // C starts again with Read only off.
    let c = c.restart("Member").await;
    c.h.add_address(&id, addr(&a)).unwrap();
    until(40, "C's held delete reaches the owner and B", || has(&root(0), "third.txt").is_none() && has(&root(1), "third.txt").is_none()).await;
    until(40, "C has B's edit", || has(&root(2), "other.txt").as_deref() == Some(&b"B's held edit"[..])).await;
    shutdown(&[&a, &b, &c]).await;
}
