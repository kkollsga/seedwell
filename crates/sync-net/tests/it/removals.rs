//! An admin's removal of a user (SPEC §7G.1 *Removing a device*): only owners sign membership changes. The admin's
//! device asks the owners, cuts the user off at once, and keeps asking until an owner's device has signed the removal,
//! which it does by itself, with no question to its user.

use std::time::Duration;

use seedwell_sync_net::removals::Asked;
use seedwell_sync_proto::ids::FolderId;
use seedwell_sync_proto::members::Role;

use crate::utp::{Device, addr, device_with, read, until};

async fn device(name: &str) -> Device {
    device_with(name, |_| {}).await
}

fn role_of(d: &Device, id: &FolderId, who: &Device) -> Option<Role> {
    d.h.members(id).ok()?.into_iter().find(|m| m.device == who.h.device_id()).map(|m| m.role)
}

fn asked(d: &Device, id: &FolderId, who: &Device) -> Option<Asked> {
    d.h.members(id).ok()?.into_iter().find(|m| m.device == who.h.device_id()).and_then(|m| m.removal)
}

/// B (admin) and C (user) talk to each other as well as to A.
fn connected(d: &Device, other: &Device) -> bool {
    d.h.snapshot().devices.iter().any(|x| x.id == other.h.device_id().to_string())
}

/// An owner A with a folder holding `seed.txt`, an admin B and a user C (Read & write), each reaching the others.
async fn folder_with_admin_and_user(dirs: &[tempfile::TempDir]) -> (Device, Device, Device, FolderId) {
    let a = device("Owner").await;
    std::fs::write(dirs[0].path().join("seed.txt"), b"from the owner").unwrap();
    let id: FolderId = a.h.add_folder(dirs[0].path().to_path_buf()).await.unwrap().id.parse().unwrap();
    let (b, c) = (device("Admin").await, device("User").await);
    for (d, dir, role) in [(&b, &dirs[1], Role::Admin), (&c, &dirs[2], Role::ReadWrite)] {
        d.h.join_folder(&a.h.create_invite(&id, role, 1, 7).unwrap().link, dir.path().join("F")).await.unwrap();
        d.h.add_address(&id, addr(&a)).unwrap();
        until(40, "admitted, with the files", || role_of(&a, &id, d) == Some(role) && read(&dir.path().join("F/seed.txt")).is_some()).await;
    }
    b.h.add_address(&id, addr(&c)).unwrap();
    c.h.add_address(&id, addr(&b)).unwrap();
    until(40, "B knows C, and they are connected", || role_of(&b, &id, &c) == Some(Role::ReadWrite) && connected(&b, &c)).await;
    (a, b, c, id)
}

async fn shutdown(devs: &[&Device]) {
    for d in devs {
        d.h.shutdown().await;
    }
}

/// An admin's Remove asks the owner: the owner's device removes the user by itself (nothing is done on it), the
/// admin's device cuts the user off at once, and demoting the admin afterwards changes nothing (the removal is the
/// owner's). Fails when the owner's device does not sign the removal on its own, the admin's device keeps syncing
/// with the user meanwhile, or the demotion lets the user back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admins_request_makes_the_owner_remove_the_user_and_a_later_demotion_changes_nothing() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c, id) = folder_with_admin_and_user(&dirs).await;
    b.h.remove_device(&id, &c.h.device_id()).unwrap();
    until(5, "the admin's device cuts the user off at once", || !connected(&b, &c)).await;
    // No call on A: its device decides by itself.
    until(30, "the owner's device removed the user", || role_of(&a, &id, &c).is_none() && role_of(&b, &id, &c).is_none()).await;
    until(20, "the admin's request is done", || asked(&b, &id, &c).is_none()).await;
    // The admin is demoted: the removal stays.
    a.h.set_role(&id, &b.h.device_id(), Role::ReadWrite).unwrap();
    until(20, "B demoted", || role_of(&b, &id, &b) == Some(Role::ReadWrite)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(role_of(&a, &id, &c).is_none() && role_of(&b, &id, &c).is_none(), "the demotion let the user back");
    shutdown(&[&a, &b, &c]).await;
}

/// The owner's device refuses, with a reason the admin's device shows, a request from a device that is not an admin
/// and one against an owner or an admin; the admin's own device refuses to ask for those. Fails when such a request is
/// carried out, or comes back without a reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_from_a_non_admin_or_against_an_owner_or_admin_is_refused_with_a_reason() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c, id) = folder_with_admin_and_user(&dirs).await;
    // Asked locally: refused before anything is sent.
    let err = b.h.remove_device(&id, &a.h.device_id()).unwrap_err().to_string();
    assert!(err.contains("not owners or admins"), "{err}");
    assert!(c.h.remove_device(&id, &b.h.device_id()).is_err(), "a user asked");
    // Sent anyway (another build): the owner's device refuses, with the reason.
    c.h.test_request_removal(&id, &b.h.device_id()).await.unwrap();
    until(20, "the user is told why not", || matches!(asked(&c, &id, &b), Some(Asked::Refused(why)) if why.contains("only an admin"))).await;
    b.h.test_request_removal(&id, &a.h.device_id()).await.unwrap();
    until(20, "the admin is told why not", || matches!(asked(&b, &id, &a), Some(Asked::Refused(why)) if why.contains("not owners or admins"))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!((role_of(&a, &id, &a), role_of(&a, &id, &b)), (Some(Role::Owner), Some(Role::Admin)), "a refused request was carried out");
    shutdown(&[&a, &b, &c]).await;
}

/// The owner is offline when the admin asks, and the admin's device starts again before the owner comes back: the
/// request is kept and sent when the owner's device is reachable, which then removes the user. Fails when a request is
/// lost with the owner away or over a restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_made_while_the_owner_is_away_is_carried_out_when_it_comes_back_also_after_a_restart() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c, id) = folder_with_admin_and_user(&dirs).await;
    let a = a.stop().await;
    b.h.remove_device(&id, &c.h.device_id()).unwrap();
    until(5, "the admin's device cuts the user off at once", || !connected(&b, &c)).await;
    assert_eq!(asked(&b, &id, &c), Some(Asked::Waiting));
    let b = b.restart("Admin").await;
    assert_eq!(asked(&b, &id, &c), Some(Asked::Waiting), "the request was lost over a restart");
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!connected(&b, &c), "the user came back while the request waits");
    let a = a.start("Owner").await;
    b.h.add_address(&id, addr(&a)).unwrap();
    until(40, "the owner's device removed the user", || role_of(&a, &id, &c).is_none()).await;
    until(20, "the admin's device knows", || role_of(&b, &id, &c).is_none() && asked(&b, &id, &c).is_none()).await;
    shutdown(&[&a, &b, &c]).await;
}

/// Only an owner's device answers a removal request: a member that is not one forges a "refused" answer, and the
/// admin's device must ignore it (keep asking, keep the user cut off). Fails when a non-owner's answer calls off the
/// request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reply_from_a_device_that_is_not_an_owner_is_ignored() {
    let dirs: Vec<tempfile::TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c, id) = folder_with_admin_and_user(&dirs[..3]).await;
    let d = device("Other User").await;
    d.h.join_folder(&a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dirs[3].path().join("F")).await.unwrap();
    d.h.add_address(&id, addr(&a)).unwrap();
    until(40, "D admitted", || role_of(&b, &id, &d) == Some(Role::ReadWrite)).await;
    d.h.add_address(&id, addr(&b)).unwrap();
    until(40, "B and D connected", || connected(&b, &d)).await;
    // The owner is away: the request waits.
    let a = a.stop().await;
    b.h.remove_device(&id, &c.h.device_id()).unwrap();
    assert_eq!(asked(&b, &id, &c), Some(Asked::Waiting));
    d.h.test_reply_removal(&id, &c.h.device_id(), Some("not today".into())).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(asked(&b, &id, &c), Some(Asked::Waiting), "a non-owner's answer called the request off");
    assert!(!connected(&b, &c), "the user was let back in");
    drop(a);
    shutdown(&[&b, &c, &d]).await;
}

/// A request names the owners' last word about the device that its admin knew; the owners decide about it again while
/// the request waits (here: a new role), so their device must keep the user, and say why. A request made after that
/// is carried out. Fails when a stale request removes a device the owners decided about since, or a current one is
/// refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_request_the_owner_decided_about_since_is_not_carried_out_and_a_current_one_is() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c, id) = folder_with_admin_and_user(&dirs).await;
    let a = a.stop().await;
    b.h.remove_device(&id, &c.h.device_id()).unwrap();
    assert_eq!(asked(&b, &id, &c), Some(Asked::Waiting));
    // Back, before it hears from the admin, the owner gives the user a new role.
    let a = a.start("Owner").await;
    a.h.set_role(&id, &c.h.device_id(), Role::ReadOnly).unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "the admin is told the owner decided since", || matches!(asked(&b, &id, &c), Some(Asked::Refused(why)) if why.contains("decided about this device since"))).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(role_of(&a, &id, &c), Some(Role::ReadOnly), "a stale request removed the user");
    // Asked again now: carried out.
    until(20, "B knows the new role", || role_of(&b, &id, &c) == Some(Role::ReadOnly)).await;
    b.h.remove_device(&id, &c.h.device_id()).unwrap();
    until(30, "the owner's device removed the user", || role_of(&a, &id, &c).is_none()).await;
    shutdown(&[&a, &b, &c]).await;
}

/// While an admin's request waits, its device keeps the user away on purpose, so the folder must not wait for it: with
/// the owner away too, the admin's status said "Waiting for 2 devices: Owner (just now), User (just now)" (the Windows
/// agent saw it). Once the owner's device refuses, the user is a member like any other and is waited for again. Fails
/// when a device cut off pending removal is counted or listed among those the folder waits for, or a refused one is
/// still left out.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_cut_off_pending_removal_is_not_waited_for_until_the_owner_refuses() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let (a, b, c, id) = folder_with_admin_and_user(&dirs).await;
    let user = c.h.device_id();
    let waiting = |d: &Device| d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).and_then(|f| f.waiting).unwrap_or_default();
    let asked_user = |d: &Device| d.h.members(&id).ok().and_then(|m| m.into_iter().find(|m| m.device == user)).and_then(|m| m.removal);
    let _c = c.stop().await;
    let a = a.stop().await;
    until(30, "the admin waits for both while nothing is asked", || waiting(&b).starts_with("Waiting for 2 devices: ")).await;

    b.h.remove_device(&id, &user).unwrap();
    assert_eq!(asked_user(&b), Some(Asked::Waiting));
    until(10, "the admin waits for the owner alone", || waiting(&b).starts_with("Waiting for Owner (last seen ")).await;
    let w = waiting(&b);
    assert!(!w.contains("User") && !w.contains("2 devices"), "the device cut off on purpose is waited for: {w}");

    // The owner decides about the user before it hears the request (a new role): its device refuses.
    let a = a.start("Owner").await;
    a.h.set_role(&id, &user, Role::ReadOnly).unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "the admin is told the owner refused", || matches!(asked_user(&b), Some(Asked::Refused(_)))).await;
    let _a = a.stop().await;
    until(30, "the user is waited for again", || {
        let w = waiting(&b);
        w.starts_with("Waiting for 2 devices: ") && w.contains("Owner") && w.contains("User")
    })
    .await;
    b.h.shutdown().await;
}

/// A channel's removal request made while the owner is away is carried out when the owner comes back: a channel's
/// link must send the waiting requests as a folder's does. Fails when a channel's link with an owner never sends them
/// (the request waits until the channel's membership happens to change; found by the PC's QA, 2026-11-06).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_channel_request_made_while_the_owner_is_away_is_carried_out_when_it_comes_back() {
    let a = device("Owner").await;
    let id: FolderId = a.h.chat_new("Team", false).await.unwrap().id.parse().unwrap();
    let (b, c) = (device("Admin").await, device("User").await);
    for (d, role) in [(&b, Role::Admin), (&c, Role::ReadWrite)] {
        d.h.chat_join(&a.h.create_invite(&id, role, 1, 7).unwrap().link).await.unwrap();
        d.h.add_address(&id, addr(&a)).unwrap();
        until(40, "admitted to the channel", || role_of(&a, &id, d) == Some(role)).await;
    }
    until(40, "B knows C", || role_of(&b, &id, &c) == Some(Role::ReadWrite)).await;
    let a = a.stop().await;
    b.h.remove_device(&id, &c.h.device_id()).unwrap();
    assert_eq!(asked(&b, &id, &c), Some(Asked::Waiting));
    let a = a.start("Owner").await;
    b.h.add_address(&id, addr(&a)).unwrap();
    until(40, "the owner's device removed the user from the channel", || role_of(&a, &id, &c).is_none()).await;
    until(20, "the admin's device knows", || role_of(&b, &id, &c).is_none() && asked(&b, &id, &c).is_none()).await;
    shutdown(&[&a, &b, &c]).await;
}
