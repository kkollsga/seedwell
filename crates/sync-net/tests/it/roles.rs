//! Roles and invitations between services on loopback (SPEC §7G.1): who admits, what admins may do, removing a
//! device, co-owners, and links that are used up.

use std::path::Path;
use std::time::Duration;

use trs_sync_net::status::FolderState;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

use crate::utp::{Device, addr, device_with, read, until};

async fn device(name: &str) -> Device {
    device_with(name, |_| {}).await
}

fn state(d: &Device, id: &FolderId) -> Option<(FolderState, Option<String>)> {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).map(|f| (f.state, f.error))
}

fn role_of(d: &Device, id: &FolderId, who: &Device) -> Option<Role> {
    d.h.members(id).ok()?.into_iter().find(|m| m.device == who.h.device_id()).map(|m| m.role)
}

/// An owner A with a folder holding `seed.txt`, and its id.
async fn owner_with_folder(dir: &Path) -> (Device, FolderId) {
    let a = device("Owner").await;
    std::fs::write(dir.join("seed.txt"), b"from the owner").unwrap();
    let st = a.h.add_folder(dir.to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    (a, id)
}

/// `d` joins with `link` at `dir/F` and dials `via`.
async fn join(d: &Device, link: &str, dir: &Path, id: &FolderId, via: &Device) {
    d.h.join_folder(link, dir.join("F")).await.unwrap();
    d.h.add_address(id, addr(via)).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_one_use_link_admits_one_device_and_tells_the_next_why_not() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let (b, c) = (device("Beta").await, device("Gamma").await);
    let inv = a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap();
    assert_eq!((inv.max_uses, inv.uses, inv.revoked), (1, 0, false));
    assert!(inv.expires_s.is_some() && inv.link.starts_with("seedwell://sync/"));

    join(&b, &inv.link, dirs[1].path(), &id, &a).await;
    until(30, "B has the owner's file", || read(&dirs[1].path().join("F/seed.txt")).is_some()).await;
    assert_eq!(role_of(&a, &id, &b), Some(Role::ReadWrite));
    until(10, "the link counts one use", || a.h.invites(&id).unwrap().iter().any(|i| i.id == inv.id && i.uses == 1)).await;

    join(&c, &inv.link, dirs[2].path(), &id, &a).await;
    until(30, "C is told the link is used up", || {
        matches!(state(&c, &id), Some((FolderState::Error, Some(e))) if e.contains("used up"))
    })
    .await;
    assert!(read(&dirs[2].path().join("F/seed.txt")).is_none(), "C got nothing");
    assert_eq!(role_of(&a, &id, &c), None);
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admin_invites_but_cannot_change_roles_or_invite_admins() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let (b, c) = (device("Admin").await, device("Guest").await);
    join(&b, &a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[1].path(), &id, &a).await;
    until(30, "B joined", || role_of(&a, &id, &b).is_some()).await;
    a.h.set_role(&id, &b.h.device_id(), Role::Admin).unwrap();
    until(20, "B knows it is an admin", || role_of(&b, &id, &b) == Some(Role::Admin)).await;

    assert!(b.h.create_invite(&id, Role::Admin, 1, 7).is_err(), "an admin cannot invite admins");
    let link = b.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link;
    // C reaches only B: the admin admits it.
    join(&c, &link, dirs[2].path(), &id, &b).await;
    until(30, "C has the file through B", || read(&dirs[2].path().join("F/seed.txt")).is_some()).await;
    until(20, "the owner knows C, admitted by the admin", || role_of(&a, &id, &c) == Some(Role::ReadOnly)).await;
    let err = b.h.set_role(&id, &c.h.device_id(), Role::ReadWrite).unwrap_err().to_string();
    assert!(err.contains("only the folder's owner"), "{err}");
    assert!(b.h.remove_device(&id, &c.h.device_id()).is_err());
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_device_is_cut_off_and_its_changes_are_ignored() {
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let b = device("Beta").await;
    join(&b, &a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[1].path(), &id, &a).await;
    until(30, "B has the file", || read(&dirs[1].path().join("F/seed.txt")).is_some()).await;

    let err = a.h.remove_device(&id, &a.h.device_id()).unwrap_err().to_string();
    assert!(err.contains("remove the folder"), "{err}");
    a.h.remove_device(&id, &b.h.device_id()).unwrap();
    until(20, "B knows it was removed", || matches!(state(&b, &id), Some((FolderState::Error, Some(e))) if e.contains("removed"))).await;
    std::fs::write(dirs[1].path().join("F/after.txt"), b"from a removed device").unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(read(&dirs[0].path().join("after.txt")).is_none(), "the owner never takes it");
    assert_eq!(role_of(&a, &id, &b), None);
    assert!(a.h.snapshot().devices.iter().all(|d| d.id != b.h.device_id().to_string()), "the session ended");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_co_owner_receives_the_folder_key_and_can_then_change_roles() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let (b, c) = (device("Second Mac").await, device("Guest").await);
    join(&b, &a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[1].path(), &id, &a).await;
    join(&c, &a.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link, dirs[2].path(), &id, &a).await;
    until(30, "both joined", || role_of(&a, &id, &b).is_some() && role_of(&a, &id, &c).is_some()).await;
    // The only owner cannot step down.
    assert!(a.h.set_role(&id, &a.h.device_id(), Role::ReadWrite).is_err());

    a.h.set_role(&id, &b.h.device_id(), Role::Owner).unwrap();
    // B can change roles once it holds the folder key.
    until(20, "B changes C's role", || b.h.set_role(&id, &c.h.device_id(), Role::ReadWrite).is_ok()).await;
    until(20, "the owner sees B's change", || role_of(&a, &id, &c) == Some(Role::ReadWrite)).await;
    // Now A may step down: B is an owner too.
    a.h.set_role(&id, &a.h.device_id(), Role::Admin).unwrap();
    until(20, "B sees A as an admin", || role_of(&b, &id, &a) == Some(Role::Admin)).await;
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joining_waits_while_no_owner_or_admin_is_online() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let (b, c) = (device("Member").await, device("Newcomer").await);
    join(&b, &a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[1].path(), &id, &a).await;
    until(30, "B has the file", || read(&dirs[1].path().join("F/seed.txt")).is_some()).await;
    let link = a.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link;

    // The owner pauses the folder (as if offline); C reaches only B, an ordinary member.
    a.h.set_paused(&id, true).await.unwrap();
    join(&c, &link, dirs[2].path(), &id, &b).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(state(&c, &id).map(|s| s.0), Some(FolderState::WaitingForAdmin));
    assert!(read(&dirs[2].path().join("F/seed.txt")).is_none());

    // The owner comes back: C is admitted.
    a.h.set_paused(&id, false).await.unwrap();
    c.h.add_address(&id, addr(&a)).unwrap();
    until(30, "C has the file", || read(&dirs[2].path().join("F/seed.txt")).is_some()).await;
    assert_eq!(role_of(&c, &id, &c), Some(Role::ReadOnly));
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn when_the_folders_sharing_ends_the_others_are_cut_off_until_the_owner_extends_it() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let (b, c) = (device("Guest").await, device("Late").await);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(a.h.set_share_until(&id, now - 10).is_err(), "an end in the past is refused");
    a.h.set_share_until(&id, now + 30 * 86_400).unwrap();
    join(&b, &a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[1].path(), &id, &a).await;
    until(30, "B has the file", || read(&dirs[1].path().join("F/seed.txt")).is_some()).await;
    let ends = |d: &Device| d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).and_then(|f| f.share_until);
    assert_eq!((ends(&a), ends(&b)), (Some(now + 30 * 86_400), Some(now + 30 * 86_400)), "B knows the end too");
    let members = a.h.members(&id).unwrap();
    assert_eq!(members.iter().find(|m| m.device == b.h.device_id()).and_then(|m| m.expires_s), Some(now + 30 * 86_400));
    assert_eq!(members.iter().find(|m| m.is_me).and_then(|m| m.expires_s), None, "not for the owner");
    let err = b.h.set_share_until(&id, 0).unwrap_err().to_string();
    assert!(err.contains("Only the folder's owner"), "{err}");

    // The sharing ends (a second ago, instead of waiting 30 days): B is cut off and nobody joins.
    a.h.test_end_sharing(&id).unwrap();
    until(20, "B knows the sharing ended", || matches!(state(&b, &id), Some((FolderState::Error, Some(e))) if e.contains("no longer shared"))).await;
    assert_eq!(role_of(&a, &id, &b), None);
    std::fs::write(dirs[1].path().join("F/late.txt"), b"after the sharing ended").unwrap();
    join(&c, &a.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link, dirs[2].path(), &id, &a).await;
    until(20, "C is told why not", || matches!(state(&c, &id), Some((FolderState::Error, Some(e))) if e.contains("no longer shared"))).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(read(&dirs[0].path().join("late.txt")).is_none(), "the owner does not take B's change");

    // The owner extends it: B counts again and syncs.
    a.h.set_share_until(&id, 0).unwrap();
    std::fs::write(dirs[0].path().join("after.txt"), b"shared again").unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(60, "B has the new file", || read(&dirs[1].path().join("F/after.txt")).is_some()).await;
    assert_eq!((role_of(&a, &id, &b), ends(&b)), (Some(Role::ReadWrite), None));
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_still_in_use_is_not_moved_and_starts_again_where_it_is() {
    let a = device("Anna").await;
    let root = tempfile::tempdir().unwrap();
    let docs = root.path().join("Docs");
    std::fs::create_dir(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"hello").unwrap();
    let id: FolderId = a.h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    // Something still holds the folder (a scan on a loaded Mac): the move waits, then leaves the files alone.
    a.h.test_stop_wait(Duration::from_millis(300));
    let hold = a.h.test_hold_folder(&id).unwrap();
    let e = a.h.rename_folder(&id, "Papers").await.unwrap_err();
    assert!(e.to_string().contains("still busy"), "{e}");
    assert!(read(&docs.join("a.txt")).is_some() && !root.path().join("Papers").exists(), "the files stay");
    // Let go: the folder runs again at its place, and the move then works.
    drop(hold);
    a.h.test_stop_wait(Duration::from_secs(60));
    until(10, "the folder runs again", || a.h.snapshot().folders.iter().any(|f| f.id == id.to_string())).await;
    let moved = a.h.rename_folder(&id, "Papers").await.unwrap();
    assert!(Path::new(&moved.path).ends_with("Papers"), "{}", moved.path);
    assert_eq!(read(&root.path().join("Papers/a.txt")).as_deref(), Some(&b"hello"[..]));
}


/// A read-only member with a file only it has, made read & write while connected: the file reaches the others
/// then, not at its next local change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_made_read_write_while_connected_sends_the_files_only_it_had() {
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let b = device("Beta").await;
    std::fs::create_dir(dirs[1].path().join("F")).unwrap();
    std::fs::write(dirs[1].path().join("F/mine.txt"), b"only B had it").unwrap();
    join(&b, &a.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link, dirs[1].path(), &id, &a).await;
    until(30, "B has the owner's file", || read(&dirs[1].path().join("F/seed.txt")).is_some()).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(read(&dirs[0].path().join("mine.txt")).is_none(), "not while B may only read");

    a.h.set_role(&id, &b.h.device_id(), Role::ReadWrite).unwrap();
    until(20, "B's own file reaches the owner", || read(&dirs[0].path().join("mine.txt")).as_deref() == Some(&b"only B had it"[..])).await;
    for d in [&a, &b] {
        d.h.shutdown().await;
    }
}

/// Each device names itself in the membership (a signed profile): a new name reaches every member while the
/// sessions stay open (a Hello is not sent again), also a member that reaches the device only through another.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_new_device_name_reaches_every_member_through_the_membership() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, id) = owner_with_folder(dirs[0].path()).await;
    let (b, c) = (device("Beta").await, device("Gamma").await);
    for (d, dir) in [(&b, &dirs[1]), (&c, &dirs[2])] {
        join(d, &a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dir.path(), &id, &a).await;
        until(30, "joined", || role_of(&a, &id, d).is_some()).await;
    }
    let name_of = |on: &Device, who: &Device| on.h.members(&id).ok()?.into_iter().find(|m| m.device == who.h.device_id()).map(|m| m.name);
    until(20, "everyone knows Beta", || [&a, &c].iter().all(|d| name_of(d, &b).as_deref() == Some("Beta"))).await;
    let mut s = b.h.settings();
    s.device_name = "Beta Studio".into();
    b.h.set_settings(s).unwrap();
    until(20, "the new name went round", || [&a, &c].iter().all(|d| name_of(d, &b).as_deref() == Some("Beta Studio"))).await;
    // The user's name the same way; the device name stays.
    let user_of = |on: &Device, who: &Device| on.h.members(&id).ok()?.into_iter().find(|m| m.device == who.h.device_id()).map(|m| (m.name, m.user));
    let mut s = b.h.settings();
    s.user_name = "  Bea\n ".into();
    b.h.set_settings(s).unwrap();
    let want = Some(("Beta Studio".to_string(), "Bea".to_string()));
    until(20, "the user's name went round", || [&a, &b, &c].iter().all(|d| user_of(d, &b) == want)).await;
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}
