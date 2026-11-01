//! Your own devices trust each other (SPEC §7G.1 "Your devices"), between services on loopback: pairing with a link
//! that the device which made it confirms, folders and channels on all your devices joining by themselves, the admin
//! path and the owner raising it, an older member accepting a sibling from the pushed grant, final removal with its
//! cascade, the skip list, and Start Over.

use std::path::{Path, PathBuf};
use std::time::Duration;

use trs_sync::ondemand::ItemState;
use trs_sync_net::SyncHandle;
use trs_sync_net::chat::MessageKind;
use trs_sync_net::devices::DevicesState;
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::members::Role;
use trs_sync_proto::msg::{FEATURE_DEVICES, FEATURES};

use crate::chat::{at, start_at};
use crate::utp::{read, until};

/// A device whose state folder outlives its service (to start it again).
struct Dev {
    h: SyncHandle,
    state: tempfile::TempDir,
    name: &'static str,
    features: u64,
}

impl Dev {
    async fn new(name: &'static str) -> Dev {
        Dev::with(name, FEATURES).await
    }

    async fn with(name: &'static str, features: u64) -> Dev {
        let state = tempfile::tempdir().unwrap();
        let h = start(state.path(), name, features).await;
        Dev { h, state, name, features }
    }

    /// Stops the service and starts it again on the same state.
    async fn restart(self) -> Dev {
        self.h.shutdown().await;
        let h = start(self.state.path(), self.name, self.features).await;
        Dev { h, ..self }
    }

    fn id(&self) -> DeviceId {
        self.h.device_id()
    }

    /// Where folders that follow go here (the run's own `Sync`, never the home folder).
    fn sync_dir(&self) -> PathBuf {
        self.state.path().join("Sync")
    }
}

async fn start(state: &Path, name: &str, features: u64) -> SyncHandle {
    start_at(state, name, |o| {
        o.features = features;
        // An isolated run: joined folders go to the state folder, never under the home folder.
        o.forbidden_root = trs_platform::dirs::home();
    })
    .await
}

/// `new` pairs with `owner`'s link, and `owner` confirms.
async fn pair(owner: &Dev, new: &Dev) {
    let (link, _) = owner.h.devices_pair_link().await.unwrap();
    new.h.devices_join(&link).await.unwrap();
    until(30, "the request reaches the link's maker", || owner.h.devices_status().requests.iter().any(|r| r.id == new.id())).await;
    owner.h.devices_confirm(&new.id()).await.unwrap();
    until(30, "paired, with the user key", || new.h.devices_status().state == DevicesState::Paired && new.h.test_has_user_key()).await;
}

fn folder_role(d: &Dev, id: &FolderId, who: &Dev) -> Option<Role> {
    d.h.members(id).ok()?.into_iter().find(|m| m.device == who.id()).map(|m| m.role)
}

fn folder_path(d: &Dev, id: &FolderId) -> Option<PathBuf> {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string()).map(|f| PathBuf::from(f.path))
}

fn has_folder(d: &Dev, id: &FolderId) -> bool {
    d.h.snapshot().folders.iter().any(|f| f.id == id.to_string())
}

/// An owner's folder holding `seed.txt` (under `dir/<name>`).
async fn folder(d: &Dev, dir: &Path, name: &str) -> FolderId {
    let p = dir.join(name);
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("seed.txt"), format!("from {}", d.name)).unwrap();
    d.h.add_folder(p).await.unwrap().id.parse().unwrap()
}

async fn shutdown(devs: &[&Dev]) {
    for d in devs {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pairing_waits_for_the_makers_confirmation_and_a_matching_user_name_gets_nothing() {
    let (a, b, c) = (Dev::new("Anna's Mac").await, Dev::new("Studio PC").await, Dev::new("Stranger").await);
    for d in [&a, &b, &c] {
        let mut s = d.h.settings();
        s.user_name = "Kim".into();
        d.h.set_settings(s).unwrap();
    }
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    let g = folder(&a, work.path(), "Shared").await;

    let (link, expires) = a.h.devices_pair_link().await.unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(link.starts_with("seedwell://devices/") && expires > now && expires <= now + 15 * 60 + 1, "{link} {expires}");
    assert_eq!(a.h.devices_status().state, DevicesState::Alone);
    assert!(a.h.devices_status().link.is_some_and(|(l, _)| l == link), "shown again while it admits");
    // Only the devices reader takes it.
    assert!(b.h.join_folder(&link, work.path().join("x")).await.unwrap_err().to_string().contains("pair a device"));
    assert!(a.h.devices_join(&link).await.is_err(), "not on the device that made it");
    a.h.set_follow(&f, true).await.unwrap();

    b.h.devices_join(&link).await.unwrap();
    let joining = b.h.devices_status();
    assert_eq!((joining.state, joining.me), (DevicesState::Joining, b.id()));
    until(30, "A shows the request, with B's name", || a.h.devices_status().requests.iter().any(|r| r.id == b.id() && r.name == "Studio PC")).await;
    until(10, "B says to confirm on A", || b.h.devices_status().progress.is_some_and(|p| p.contains("Anna's Mac"))).await;
    // Without the confirmation nothing is given.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(b.h.devices_status().state, DevicesState::Joining);
    assert!(!b.h.test_has_user_key() && !has_folder(&b, &f));
    assert_eq!(a.h.devices_status().devices.len(), 1);

    // The stranger with the same user name, in another folder with A, gets nothing of the group.
    c.h.join_folder(&a.h.create_invite(&g, Role::ReadWrite, 1, 7).unwrap().link, work.path().join("C")).await.unwrap();
    c.h.add_address(&g, at(&a.h)).unwrap();
    until(30, "C has the shared folder", || read(&work.path().join("C/seed.txt")).is_some()).await;

    a.h.devices_confirm(&b.id()).await.unwrap();
    until(30, "B is paired and holds the user key", || b.h.devices_status().state == DevicesState::Paired && b.h.test_has_user_key()).await;
    let list = a.h.devices_status().devices;
    assert_eq!(list.iter().map(|d| (d.id, d.is_me)).collect::<Vec<_>>(), vec![(a.id(), true), (b.id(), false)], "this device first");
    assert!(list[1].paired_by == Some(a.id()) && list[1].paired_by_name == "Anna's Mac");
    until(20, "B lists A by its name", || b.h.devices_status().devices.iter().any(|d| d.id == a.id() && d.name == "Anna's Mac" && d.online)).await;
    assert!(a.h.devices_status().link.is_none(), "the link is used up");
    // The folder on all devices came to B by itself, at its default location, as an admin.
    until(40, "B has the followed folder", || read(&b.sync_dir().join("Dev/seed.txt")).is_some()).await;
    assert_eq!(folder_role(&a, &f, &b), Some(Role::Admin));

    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(c.h.devices_status().state, DevicesState::None);
    assert!(!has_folder(&c, &f), "the stranger never gets the folder");
    assert_eq!(folder_role(&a, &f, &c), None);
    // A device connected only for the group is in no device list (agents read these).
    let snap = b.h.snapshot();
    assert!(snap.offline.iter().all(|o| o.id != c.id().to_string()));
    // The link admits nobody again.
    let d = Dev::new("Late").await;
    d.h.devices_join(&link).await.unwrap();
    until(30, "the late device is told", || d.h.devices_status().problem.is_some_and(|p| p.contains("used up"))).await;
    shutdown(&[&a, &b, &c, &d]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_pairing_admits_nobody_with_that_link() {
    let (a, b) = (Dev::new("Anna's Mac").await, Dev::new("Studio PC").await);
    let (link, _) = a.h.devices_pair_link().await.unwrap();
    b.h.devices_join(&link).await.unwrap();
    until(30, "A shows the request", || a.h.devices_status().requests.iter().any(|r| r.id == b.id())).await;
    a.h.devices_refuse(&b.id()).await.unwrap();
    assert!(a.h.devices_status().requests.is_empty());
    until(30, "B is told", || b.h.devices_status().problem.is_some_and(|p| p.contains("refused") || p.contains("revoked"))).await;
    assert!(!b.h.test_has_user_key());
    assert_eq!(a.h.devices_status().devices.len(), 1);
    shutdown(&[&a, &b]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_followed_folder_and_channels_come_to_a_sibling_by_themselves() {
    let (a, b) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Seedwell Dev").await;
    let yes: FolderId = a.h.chat_new("Seedwell Development", true).await.unwrap().id.parse().unwrap();
    let no: FolderId = a.h.chat_new("Private", false).await.unwrap().id.parse().unwrap();
    a.h.chat_send(&yes, None, MessageKind::Text, "hello from A", None, None).await.unwrap();
    pair(&a, &b).await;
    // Only an owner or admin may put it on all devices; nothing follows by itself.
    assert!(!a.h.follow_state(&f).unwrap().on && a.h.follow_state(&f).unwrap().may);
    assert_eq!(a.h.follow_state(&f).unwrap().others, vec!["MacBook".to_string()]);
    for id in [&f, &yes, &no] {
        a.h.set_follow(id, true).await.unwrap();
    }
    until(40, "the folder came, with its file", || read(&b.sync_dir().join("Seedwell Dev/seed.txt")).is_some()).await;
    until(30, "B is an admin of it", || folder_role(&b, &f, &b) == Some(Role::Admin)).await;
    assert!(b.h.follow_state(&f).unwrap().on, "B shows it on all devices too");
    until(30, "the channels came, B an admin", || [&yes, &no].iter().all(|c| b.h.channel_status(c).is_some_and(|s| s.role == Some(Role::Admin)))).await;
    until(20, "the message arrived", || b.h.chat_read(&yes, None, None, None).unwrap_or_default().iter().any(|m| m.body == "hello from A")).await;
    // Agents: the Yes channel is open to them on B, the No channel stays closed.
    assert!(b.h.channel_status(&yes).unwrap().agents);
    assert!(!b.h.channel_status(&no).unwrap().agents);
    // Changes go both ways.
    std::fs::write(b.sync_dir().join("Seedwell Dev/from-b.txt"), b"B").unwrap();
    until(30, "A has B's file", || read(&work.path().join("Seedwell Dev/from-b.txt")).is_some()).await;
    shutdown(&[&a, &b]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_paired_through_an_admin_gets_read_and_write_and_the_owner_raises_it() {
    let (a, b) = (Dev::new("Owner Mac").await, Dev::new("Admin Mac").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    pair(&a, &b).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "B is an admin of the folder", || folder_role(&b, &f, &b) == Some(Role::Admin) && read(&b.sync_dir().join("Dev/seed.txt")).is_some()).await;

    // The owner goes offline; N pairs through B, which can admit only at Read & write.
    a.h.shutdown().await;
    let n = Dev::new("New Mac").await;
    pair(&b, &n).await;
    until(40, "N has the folder through B", || read(&n.sync_dir().join("Dev/seed.txt")).is_some()).await;
    assert_eq!(folder_role(&n, &f, &n), Some(Role::ReadWrite));
    assert_eq!(folder_role(&b, &f, &n), Some(Role::ReadWrite));

    // The owner comes back: it raises N to Admin.
    let a = Dev { h: start(a.state.path(), a.name, a.features).await, ..a };
    until(60, "the owner raised N", || folder_role(&a, &f, &n) == Some(Role::Admin)).await;
    until(30, "N knows", || folder_role(&n, &f, &n) == Some(Role::Admin)).await;
    assert!(a.h.devices_status().devices.iter().any(|d| d.id == n.id() && d.paired_by == Some(b.id())), "paired by B");
    shutdown(&[&a, &b, &n]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_on_an_older_protocol_accepts_a_sibling_from_the_pushed_grant() {
    let a = Dev::new("Owner Mac").await;
    let old = Dev::with("Older Mac", FEATURES & !FEATURE_DEVICES).await;
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    old.h.join_folder(&a.h.create_invite(&f, Role::ReadWrite, 1, 7).unwrap().link, work.path().join("Old")).await.unwrap();
    old.h.add_address(&f, at(&a.h)).unwrap();
    until(30, "the older member has the folder", || read(&work.path().join("Old/seed.txt")).is_some()).await;
    let n = Dev::new("New Mac").await;
    assert!(a.h.set_follow(&f, true).await.is_err(), "not paired yet: nothing to put it on");
    pair(&a, &n).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "N joined the folder", || folder_role(&n, &f, &n) == Some(Role::Admin)).await;
    until(30, "the older member knows N from the owner's grant", || folder_role(&old, &f, &n) == Some(Role::Admin)).await;

    // Without the owner, N and the older member sync with each other.
    a.h.shutdown().await;
    n.h.add_address(&f, at(&old.h)).unwrap();
    std::fs::write(work.path().join("Old/from-old.txt"), b"old").unwrap();
    until(40, "N has the older member's file", || folder_path(&n, &f).is_some_and(|p| read(&p.join("from-old.txt")).is_some())).await;
    shutdown(&[&old, &n]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_a_device_is_final_and_takes_it_out_of_the_owners_places() {
    let (a, n) = (Dev::new("Owner Mac").await, Dev::new("Lost Laptop").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    let c: FolderId = a.h.chat_new("Team", true).await.unwrap().id.parse().unwrap();
    pair(&a, &n).await;
    a.h.set_follow(&f, true).await.unwrap();
    a.h.set_follow(&c, true).await.unwrap();
    until(40, "N joined both", || folder_role(&a, &f, &n) == Some(Role::Admin) && n.h.channel_status(&c).is_some_and(|s| s.role == Some(Role::Admin))).await;

    let plan = a.h.devices_remove_plan(&n.id()).unwrap();
    let mut owner: Vec<FolderId> = plan.owner.iter().map(|p| p.id).collect();
    owner.sort();
    let mut want = vec![f, c];
    want.sort();
    assert_eq!((owner, plan.admin.len(), plan.name.as_str()), (want.clone(), 0, "Lost Laptop"));
    assert!(a.h.devices_remove(&a.id(), &[]).await.is_err(), "not this device: it leaves instead");
    a.h.devices_remove(&n.id(), &want).await.unwrap();
    assert_eq!(a.h.devices_status().devices.len(), 1);
    assert_eq!(folder_role(&a, &f, &n), None);
    assert_eq!(a.h.members(&c).unwrap().iter().find(|m| m.device == n.id()).map(|m| m.role), None);
    until(30, "N knows it was removed", || n.h.devices_status().state == DevicesState::Removed).await;
    // It still holds the user key, but a grant it makes itself does not let it back in, and no sibling admits it.
    std::fs::write(work.path().join("Dev/after.txt"), b"after").unwrap();
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(folder_path(&n, &f).is_none_or(|p| read(&p.join("after.txt")).is_none()), "nothing new reaches it");
    assert_eq!(folder_role(&a, &f, &n), None);
    assert!(n.h.devices_pair_link().await.is_err(), "a removed device pairs nobody");
    shutdown(&[&a, &n]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_taken_off_one_device_stays_off_and_unfollowing_deletes_nothing() {
    let (a, b) = (Dev::new("Owner Mac").await, Dev::new("MacBook").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    let g = folder(&a, work.path(), "Photos").await;
    pair(&a, &b).await;
    a.h.set_follow(&f, true).await.unwrap();
    a.h.set_follow(&g, true).await.unwrap();
    until(40, "B has both", || has_folder(&b, &f) && read(&b.sync_dir().join("Photos/seed.txt")).is_some()).await;

    // Not on this Mac: it does not come back, also when the group changes.
    assert!(b.h.is_followed(&f));
    b.h.remove_folder(&f).await.unwrap();
    let other = folder(&a, work.path(), "Other").await;
    a.h.set_follow(&other, true).await.unwrap();
    until(40, "the next one came", || has_folder(&b, &other)).await;
    assert!(!has_folder(&b, &f), "the one taken off stays off");

    // Off on all devices: B keeps what it has.
    a.h.set_follow(&g, false).await.unwrap();
    until(20, "B sees it off", || !b.h.follow_state(&g).unwrap().on).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(has_folder(&b, &g) && read(&b.sync_dir().join("Photos/seed.txt")).is_some());
    assert_eq!(folder_role(&a, &g, &b), Some(Role::Admin));
    shutdown(&[&a, &b]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_that_removed_each_other_start_over_and_pair_again() {
    let (a, b) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await);
    pair(&a, &b).await;
    // Apart, each removes the other.
    b.h.shutdown().await;
    a.h.devices_remove(&b.id(), &[]).await.unwrap();
    let a = a.restart().await;
    a.h.shutdown().await;
    let b = Dev { h: start(b.state.path(), b.name, b.features).await, ..b };
    b.h.devices_remove(&a.id(), &[]).await.unwrap();
    let a = Dev { h: start(a.state.path(), a.name, a.features).await, ..a };
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(a.h.devices_status().state, DevicesState::Alone);
    assert_eq!(b.h.devices_status().state, DevicesState::Alone);

    // Start Over: a new group, the other pairs again (its own group, with nobody else in it, goes).
    let old = a.h.devices_status();
    let fresh = a.h.devices_start_over().await.unwrap();
    assert_eq!(fresh.state, DevicesState::Alone);
    assert!(a.h.test_has_user_key());
    pair(&a, &b).await;
    assert_eq!(a.h.devices_status().devices.len(), 2);
    assert_eq!(old.me, a.id());
    // Leave: the group is gone here, the folders stay.
    b.h.devices_leave().await.unwrap();
    assert_eq!(b.h.devices_status().state, DevicesState::None);
    assert!(!b.h.test_has_user_key());
    until(20, "A sees B gone", || a.h.devices_status().devices.len() == 1).await;
    shutdown(&[&a, &b]).await;
}

/// The devices in a device's list, by id.
fn listed(d: &Dev) -> Vec<DeviceId> {
    let mut v: Vec<DeviceId> = d.h.devices_status().devices.into_iter().map(|x| x.id).collect();
    v.sort();
    v
}

fn ids(devs: &[&Dev]) -> Vec<DeviceId> {
    let mut v: Vec<DeviceId> = devs.iter().map(|d| d.id()).collect();
    v.sort();
    v
}

/// In the same group as `other`, holding its key, and listing exactly `want`.
fn settled(d: &Dev, other: &Dev, want: &[DeviceId]) -> bool {
    d.h.test_group_id().is_some() && d.h.test_group_id() == other.h.test_group_id() && d.h.test_has_user_key() && listed(d) == want
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_device_keeps_the_old_key_but_a_device_it_mints_gets_nothing() {
    let (a, b, l) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await, Dev::new("Lost Laptop").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    pair(&a, &b).await;
    pair(&a, &l).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "B and L joined the folder", || folder_role(&a, &f, &b) == Some(Role::Admin) && folder_role(&a, &f, &l) == Some(Role::Admin)).await;
    let old = a.h.test_group_id().unwrap();

    // Removing L changes the key: A and B go on in a new group, without L.
    a.h.devices_remove(&l.id(), &[f]).await.unwrap();
    assert_ne!(a.h.test_group_id(), Some(old), "a new group at once");
    until(30, "B moved with A", || settled(&b, &a, &ids(&[&a, &b]))).await;
    assert!(settled(&a, &b, &ids(&[&a, &b])));
    until(20, "L knows it was removed", || l.h.devices_status().state == DevicesState::Removed).await;

    // L still holds the old key: it signs a fresh device into the old group, which knows where A and B were.
    let m = Dev::new("Minted").await;
    assert!(l.h.test_mint_into_group(&m.h).await);
    assert_eq!((m.h.test_group_id(), m.h.devices_status().state), (Some(old), DevicesState::Paired));
    std::fs::write(work.path().join("Dev/after.txt"), b"after").unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    // Nobody admits it: not to the devices, not to the folder on all of them.
    for d in [&a, &b] {
        assert!(!listed(d).contains(&m.id()), "{} does not list it", d.name);
        assert_eq!(folder_role(d, &f, &m), None, "{} did not admit it", d.name);
    }
    assert!(folder_path(&m, &f).is_none_or(|p| read(&p.join("after.txt")).is_none()), "it got nothing");
    assert_eq!(a.h.test_group_id(), b.h.test_group_id(), "and the group stayed where it was");
    shutdown(&[&a, &b, &l, &m]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_away_during_a_removal_moves_to_the_new_group_when_it_comes_back() {
    let (a, b, c, l) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await, Dev::new("Studio PC").await, Dev::new("Lost Laptop").await);
    for d in [&b, &c, &l] {
        pair(&a, d).await;
    }
    let old = a.h.test_group_id();
    c.h.shutdown().await;
    a.h.devices_remove(&l.id(), &[]).await.unwrap();
    until(30, "B moved", || settled(&b, &a, &ids(&[&a, &b, &c]))).await;

    // C comes back: it meets A or B, learns where the group went, and is admitted there by its own key.
    let c = Dev { h: start(c.state.path(), c.name, c.features).await, ..c };
    assert_eq!(c.h.test_group_id(), old, "still in the old group");
    until(40, "C moved too", || settled(&c, &a, &ids(&[&a, &b, &c]))).await;
    until(20, "A sees C online in the new group", || a.h.devices_status().devices.iter().any(|d| d.id == c.id() && d.online)).await;
    shutdown(&[&a, &b, &c, &l]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_removals_made_apart_settle_on_one_group_without_either_removed_device() {
    let (a, b, c, d) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await, Dev::new("Studio PC").await, Dev::new("Old Phone").await);
    for x in [&b, &c, &d] {
        pair(&a, x).await;
    }
    // A folder on all of them, through which A and B meet again (in a test there is no discovery).
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "B has the folder", || folder_role(&b, &f, &b) == Some(Role::Admin)).await;
    until(20, "every device lists the four", || [&a, &b].iter().all(|x| listed(x).len() == 4)).await;
    c.h.shutdown().await;
    d.h.shutdown().await;
    // Apart: A removes C while B is away, then B removes D while A is away. Two new groups.
    b.h.shutdown().await;
    a.h.devices_remove(&c.id(), &[]).await.unwrap();
    a.h.shutdown().await;
    let b = Dev { h: start(b.state.path(), b.name, b.features).await, ..b };
    b.h.devices_remove(&d.id(), &[]).await.unwrap();
    assert_ne!(b.h.test_group_id(), None);
    // Together again: they settle on one group, without C and without D.
    let a = Dev { h: start(a.state.path(), a.name, a.features).await, ..a };
    b.h.add_address(&f, at(&a.h)).unwrap();
    until(60, "one group, the two of them", || settled(&a, &b, &ids(&[&a, &b])) && settled(&b, &a, &ids(&[&a, &b]))).await;
    shutdown(&[&a, &b]).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn paired_devices_share_the_rendezvous_and_find_each_other_again_after_a_network_change() {
    let rv = trs_rendezvous::testkit::fake_rendezvous().await;
    let (a, b) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await);
    pair(&a, &b).await;
    // A sets one up: B, which has none, takes it and publishes there too.
    a.h.set_rendezvous(&rv.url()).await.unwrap();
    until(20, "B took A's rendezvous", || b.h.rendezvous_status().url == rv.url()).await;
    let b_record = trs_sync_proto::rendezvous::record_id_hex(&b.id());
    until(20, "B publishes there", || rv.count("PUT", &format!("/v1/r/{b_record}")) >= 1).await;
    // A pairing link made now says where its maker publishes.
    let (link, _) = a.h.devices_pair_link().await.unwrap();
    let reach = trs_sync_proto::members::InviteLink::parse_devices(&link).unwrap().reach.unwrap();
    assert_eq!(reach.rendezvous.as_deref(), Some(rv.url().as_str()));

    // Off on B only: A keeps it, and B does not take it back by itself.
    b.h.turn_off_rendezvous().await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!((a.h.rendezvous_status().url, b.h.rendezvous_status().url), (rv.url(), String::new()));

    // A network change on A: the session that carries only the group (no folder in common) comes back at once.
    let sessions = a.h.test_sessions();
    a.h.test_addresses_gone(&[std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]);
    a.h.network_changed();
    until(20, "A and B together again for the group", || {
        a.h.test_sessions() > sessions && a.h.devices_status().devices.iter().any(|d| d.id == b.id() && d.online)
    })
    .await;
    assert_eq!(b.h.rendezvous_status().url, "", "still off on B");
    shutdown(&[&a, &b]).await;
}

fn snapshot_folder(d: &Dev, id: &FolderId) -> Option<trs_sync_net::status::FolderStatus> {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string())
}

/// A folder on demand on the device that puts it on all devices comes on demand to a sibling: every file listed,
/// nothing downloaded until asked for. A device that keeps no folders on demand (Windows today) joins it in full and
/// says why. Its owner removing it removes it from Seedwell on every sibling, each keeping its files.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_on_demand_folder_follows_on_demand_and_its_owner_removing_it_removes_it_everywhere() {
    let a = Dev::new("Anna's Mac").await;
    // B keeps folders on demand wherever the test runs (a Mac's default; not a PC's).
    let mac_state = tempfile::tempdir().unwrap();
    let mac = start_at(mac_state.path(), "MacBook", |o| {
        o.forbidden_root = trs_platform::dirs::home();
        o.on_demand = true;
    })
    .await;
    let b = Dev { h: mac, state: mac_state, name: "MacBook", features: FEATURES };
    let full_state = tempfile::tempdir().unwrap();
    let pc = start_at(full_state.path(), "Studio PC", |o| {
        o.forbidden_root = trs_platform::dirs::home();
        o.on_demand = false;
    })
    .await;
    let pc = Dev { h: pc, state: full_state, name: "Studio PC", features: FEATURES };
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("Photos");
    std::fs::create_dir_all(src.join("2026")).unwrap();
    std::fs::write(src.join("2026/beach.jpg"), b"beach").unwrap();
    std::fs::write(src.join("big.bin"), vec![7u8; 1_000_000]).unwrap();
    let f: FolderId = a.h.add_folder_on_demand(src.clone()).await.unwrap().id.parse().unwrap();
    pair(&a, &b).await;
    pair(&a, &pc).await;
    a.h.set_follow(&f, true).await.unwrap();

    // B: on demand, its place the default location's (not made: the host's), every file listed and none fetched.
    until(40, "B lists the folder's files", || b.h.od_item_at(&f, "2026/beach.jpg").ok().flatten().is_some() && b.h.od_item_at(&f, "big.bin").ok().flatten().is_some())
        .await;
    let st = snapshot_folder(&b, &f).unwrap();
    assert!(st.on_demand, "joined on demand");
    assert_eq!(PathBuf::from(&st.path), b.sync_dir().join("Photos"));
    assert!(!b.sync_dir().join("Photos").exists(), "the place is the host's");
    assert_eq!(b.h.od_item_at(&f, "big.bin").unwrap().unwrap().state, ItemState::OnlineOnly);
    assert!(!b.h.od_cache_path(&f, "big.bin").unwrap().exists(), "nothing downloaded");
    assert!(st.issues.is_empty(), "{:?}", st.issues);
    let at = b.h.od_fetch(&f, "2026/beach.jpg", Duration::from_secs(30)).await.unwrap().path;
    assert_eq!(read(&at).as_deref(), Some(&b"beach"[..]));

    // The PC keeps no folders on demand: in full, saying why.
    until(40, "the PC has every file", || read(&pc.sync_dir().join("Photos/big.bin")).is_some_and(|b| b.len() == 1_000_000)).await;
    let st = snapshot_folder(&pc, &f).unwrap();
    assert!(!st.on_demand);
    assert!(st.issues.iter().any(|(p, why)| p.is_empty() && why == trs_sync_net::FULL_HERE), "{:?}", st.issues);
    // A switch on demand on the owner later is the owner's only: a device that joined keeps its own.
    assert!(a.h.removes_everywhere(&f) && !b.h.removes_everywhere(&f), "only the owner's removal goes everywhere");

    // A, the owner, removes it: it goes from Seedwell everywhere, every device keeping its files.
    a.h.remove_folder(&f).await.unwrap();
    assert_eq!(read(&src.join("2026/beach.jpg")).as_deref(), Some(&b"beach"[..]), "A's files back at its place");
    // (A folder leaves the list when it stops, before its files are where they go: the removal is over once they are.)
    let removed = |d: &Dev| !has_folder(d, &f) && d.sync_dir().join("Photos").exists() && !d.sync_dir().join("Photos/.seedwell").exists();
    until(30, "gone from B and the PC", || removed(&b) && removed(&pc)).await;
    assert_eq!(read(&b.sync_dir().join("Photos/2026/beach.jpg")).as_deref(), Some(&b"beach"[..]), "B's fetched file at its place");
    assert_eq!(read(&pc.sync_dir().join("Photos/big.bin")).map(|b| b.len()), Some(1_000_000), "the PC's files stay");
    assert!(!a.h.is_followed(&f) && b.h.follow_state(&f).is_err());
    shutdown(&[&a, &b, &pc]).await;
}

/// Removed on a sibling that is not its owner, a folder goes from that device only (its files stay there) and does
/// not come back; the owner removing it later reaches a sibling that was away at the time when it is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_siblings_removal_stays_local_and_an_away_sibling_removes_it_when_back() {
    let (a, b, c) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await, Dev::new("Mac mini").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Dev").await;
    pair(&a, &b).await;
    pair(&a, &c).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "B and C have the folder's file", || read(&b.sync_dir().join("Dev/seed.txt")).is_some() && read(&c.sync_dir().join("Dev/seed.txt")).is_some()).await;
    assert!(!b.h.removes_everywhere(&f), "B is an admin there, not its owner");

    // B removes it: from B only.
    b.h.remove_folder(&f).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(has_folder(&a, &f) && has_folder(&c, &f), "A and C keep it");
    assert!(a.h.is_followed(&f), "still on all devices");
    assert!(read(&b.sync_dir().join("Dev/seed.txt")).is_some(), "B's files stay");
    assert!(!has_folder(&b, &f), "and it does not come back to B");

    // The agent interface's removal on the owner is this device's only too (agents never change the other devices).
    let g = folder(&a, work.path(), "Notes").await;
    a.h.set_follow(&g, true).await.unwrap();
    until(40, "C has the second folder", || read(&c.sync_dir().join("Notes/seed.txt")).is_some()).await;
    a.h.remove_folder_here(&g, false).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(has_folder(&c, &g) && c.h.is_followed(&g), "C keeps it, still on all devices");

    // C is away when A removes it; back, it removes it too, keeping its files.
    c.h.shutdown().await;
    a.h.remove_folder(&f).await.unwrap();
    assert!(read(&work.path().join("Dev/seed.txt")).is_some(), "A's files stay");
    let c = c.restart().await;
    until(40, "C removed it once back", || !has_folder(&c, &f) && !c.sync_dir().join("Dev/.seedwell").exists()).await;
    assert!(read(&c.sync_dir().join("Dev/seed.txt")).is_some(), "C's files stay");
    shutdown(&[&a, &b, &c]).await;
}
