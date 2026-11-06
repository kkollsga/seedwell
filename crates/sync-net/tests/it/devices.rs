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
use trs_sync_proto::members::{Body, FOLLOW_READ_ONLY, FOLLOW_REMOVED, MAX_FOLLOW_SERIAL, Role, Signer};
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

/// The bug (0.1.18): *Files on demand* on a folder whose files are here moved them into this device's cache. Now the
/// device that holds them keeps them where they are (nothing read, moved or renamed: the same file ids, a full folder
/// at its path), and a sibling without them still gets placeholders: every file listed, nothing downloaded.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn files_on_demand_keeps_the_holders_files_in_place_and_a_sibling_gets_placeholders() {
    use std::os::unix::fs::MetadataExt;
    let a = Dev::new("Anna's Mac").await;
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("Movies");
    std::fs::create_dir_all(src.join("V for Vendetta (2005)")).unwrap();
    let film = src.join("V for Vendetta (2005)/film.mkv");
    std::fs::write(&film, vec![5u8; 300_000]).unwrap();
    let f: FolderId = a.h.add_folder(src.clone()).await.unwrap().id.parse().unwrap();
    let stamp = |p: &Path| std::fs::metadata(p).map(|m| (m.ino(), m.mtime_nsec(), m.len())).unwrap();
    let before = (std::fs::metadata(&src).unwrap().ino(), stamp(&film));
    let c_state = tempfile::tempdir().unwrap();
    let c = start_at(c_state.path(), "Mac mini", |o| {
        o.forbidden_root = trs_platform::dirs::home();
        o.on_demand = true;
    })
    .await;
    let c = Dev { h: c, state: c_state, name: "Mac mini", features: FEATURES };
    pair(&a, &c).await;

    let st = a.h.set_files_on_demand(&f, true, None).await.unwrap();
    assert!(!st.on_demand && st.on_demand_others, "full here, on demand for the others");
    assert_eq!(PathBuf::from(&st.path), src);
    assert_eq!((std::fs::metadata(&src).unwrap().ino(), stamp(&film)), before, "the same folder and file, untouched");
    assert!(a.h.offers_on_demand(&f));
    assert!(snapshot_folder(&a, &f).is_some_and(|s| !s.on_demand && s.on_demand_others));

    // The sibling, without the files: placeholders.
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "C lists the film", || c.h.od_item_at(&f, "V for Vendetta (2005)/film.mkv").ok().flatten().is_some()).await;
    let cs = snapshot_folder(&c, &f).unwrap();
    assert!(cs.on_demand, "joined on demand");
    assert_eq!(c.h.od_item_at(&f, "V for Vendetta (2005)/film.mkv").unwrap().unwrap().state, ItemState::OnlineOnly);
    assert!(!c.h.od_cache_path(&f, "V for Vendetta (2005)/film.mkv").unwrap().exists(), "nothing downloaded");
    assert_eq!((std::fs::metadata(&src).unwrap().ino(), stamp(&film)), before, "still untouched on A");

    // Off takes only the mark back.
    let st = a.h.set_files_on_demand(&f, false, None).await.unwrap();
    assert!(!st.on_demand && !st.on_demand_others && !a.h.offers_on_demand(&f));
    assert_eq!(stamp(&film), before.1);
    shutdown(&[&a, &c]).await;
}

/// Review of the on-demand fix, #3: the *On all my devices* statement was signed anew only when the mark of a full
/// folder changed, so freeing the copies or going back to full left it saying the old mode, and another holder turning
/// the switch off left this device showing it on. Fails when a mode switch leaves the statement as it was, or a device
/// shows its own mark instead of what the statement says.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_on_all_my_devices_statement_follows_every_mode_switch_and_every_holder_shows_it() {
    let a = Dev::new("Anna's Mac").await;
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("Docs");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"from Anna").unwrap();
    let f: FolderId = a.h.add_folder(src.clone()).await.unwrap().id.parse().unwrap();
    let c = Dev::new("Mac mini").await;
    pair(&a, &c).await;
    a.h.set_follow(&f, true).await.unwrap();
    assert_eq!(a.h.follow_on_demand(&f), Some(false), "full on all devices");
    until(40, "the sibling holds the folder, as an admin", || {
        read(&c.sync_dir().join("Docs/a.txt")).is_some() && folder_role(&c, &f, &c).is_some_and(Role::admits)
    })
    .await;

    // Freeing this device's copies makes it on demand here: devices that join later get placeholders.
    a.h.free_local_copies(&f, None).await.unwrap();
    assert_eq!(a.h.follow_on_demand(&f), Some(true), "signed anew after freeing the copies");
    // Full again: so for the devices that join too.
    let st = a.h.set_files_on_demand(&f, false, None).await.unwrap();
    assert!(!st.on_demand && !st.on_demand_others);
    assert_eq!(a.h.follow_on_demand(&f), Some(false), "signed anew after going back to full");

    // The switch on here; the sibling, which holds the files too, turns it off: both show it off. (The sibling has each
    // statement before the next: one it has not seen yet would rank above its own.)
    until(30, "the sibling has the statement of full", || c.h.follow_on_demand(&f) == Some(false)).await;
    assert!(a.h.set_files_on_demand(&f, true, None).await.unwrap().on_demand_others);
    until(30, "the sibling has the new statement", || c.h.follow_on_demand(&f) == Some(true)).await;
    assert!(snapshot_folder(&c, &f).is_some_and(|s| !s.on_demand && s.on_demand_others), "the sibling shows the statement too");
    c.h.set_files_on_demand(&f, false, None).await.unwrap();
    until(30, "Anna's Mac has the sibling's statement", || a.h.follow_on_demand(&f) == Some(false)).await;
    assert!(snapshot_folder(&a, &f).is_some_and(|s| !s.on_demand && !s.on_demand_others), "shown from the statement, not this device's mark");
    shutdown(&[&a, &c]).await;
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

/// The Follow that counted was the latest of any of the user's devices, so a sibling that is only a member of the
/// folder (made Read only by the user, or on another build) saying the folder was removed on all devices made the owner
/// remove its own folder from Seedwell, its folder key included, and one saying it was off stopped every admission.
/// Fails when a statement of a device that is not owner of the folder removes it anywhere, or one of a device that is
/// neither owner nor admin keeps a new device from joining.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_read_only_siblings_removal_or_off_never_removes_the_folder_nor_stops_admissions() {
    let (a, b, c) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await, Dev::new("Studio PC").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Films").await;
    pair(&a, &b).await;
    pair(&a, &c).await;
    a.h.set_follow(&f, true).await.unwrap();
    for s in [&b, &c] {
        until(40, "an admin, with the files", || folder_role(&a, &f, s) == Some(Role::Admin) && read(&s.sync_dir().join("Films/seed.txt")).is_some()).await;
    }
    // The user makes B Read only.
    a.h.set_role(&f, &b.id(), Role::ReadOnly).unwrap();
    until(30, "B is Read only", || folder_role(&b, &f, &b) == Some(Role::ReadOnly)).await;
    // B says the folder was removed on all devices.
    assert!(b.h.test_say_follow(&f, false, FOLLOW_REMOVED, None).await);
    for d in [&a, &c] {
        until(30, "it has B's statement", || d.h.test_has_follow(&f, &b.id(), FOLLOW_REMOVED)).await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert!(has_folder(&a, &f) && has_folder(&c, &f), "removed by a Read only sibling's statement");
    assert_eq!(folder_role(&a, &f, &a), Some(Role::Owner), "the owner keeps the folder and its key");
    assert!(a.h.is_followed(&f) && c.h.is_followed(&f), "still on all devices");
    // B says it is off: a device paired now still joins it.
    assert!(b.h.test_say_follow(&f, false, 0, None).await);
    until(30, "A has B's statement", || a.h.test_has_follow(&f, &b.id(), 0)).await;
    let d = Dev::new("New Mac").await;
    pair(&a, &d).await;
    until(40, "D joined", || folder_role(&a, &f, &d) == Some(Role::Admin) && read(&d.sync_dir().join("Films/seed.txt")).is_some()).await;
    // And the folder syncs on.
    std::fs::write(work.path().join("Films/after.txt"), b"A").unwrap();
    until(40, "C has the owner's new file", || read(&c.sync_dir().join("Films/after.txt")).is_some()).await;
    shutdown(&[&a, &b, &c, &d]).await;
}

/// The next Follow's serial was one above the highest any member said, unchecked, so a sibling saying `u64::MAX`
/// panicked a debug build (and wrapped to 0 in a release one), freezing "On all my devices". Fails when such a serial
/// panics, or keeps the owner from turning it off and on again (also once that sibling is an admin again, its
/// statement at the highest serial counting: the owner's next one must still rank above it).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_huge_serial_from_a_sibling_neither_panics_nor_freezes_on_all_my_devices() {
    let (a, b) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Films").await;
    pair(&a, &b).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "B is an admin", || folder_role(&b, &f, &b) == Some(Role::Admin)).await;
    a.h.set_role(&f, &b.id(), Role::ReadOnly).unwrap();
    until(40, "B is Read only", || folder_role(&b, &f, &b) == Some(Role::ReadOnly)).await;
    // (Counting for nothing, it changes nothing here; it goes with the next statement.)
    b.h.test_say_follow(&f, true, 0, Some(u64::MAX)).await;
    assert!(b.h.test_say_follow(&f, true, 0, Some(MAX_FOLLOW_SERIAL)).await);
    until(30, "A has B's statements", || a.h.test_has_follow(&f, &b.id(), 0)).await;
    a.h.set_follow(&f, false).await.unwrap();
    assert!(!a.h.is_followed(&f), "turned off");
    // B an admin again: its statement at the highest serial counts; the owner's next one still ranks above it.
    a.h.set_role(&f, &b.id(), Role::Admin).unwrap();
    until(40, "B is an admin again", || folder_role(&a, &f, &b) == Some(Role::Admin) && folder_role(&b, &f, &b) == Some(Role::Admin)).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "on all devices again, on A and B", || a.h.is_followed(&f) && b.h.is_followed(&f)).await;
    shutdown(&[&a, &b]).await;
}

/// Where `by` said, in `d`'s group as it is there now, that the group moved (its Moved statement's group id).
fn moved_by(d: &Dev, by: &Dev) -> Option<FolderId> {
    let by = by.id();
    d.h.test_group_statements().into_iter().find_map(|st| match (st.body, st.signer) {
        (Body::Moved { target_pk, .. }, Signer::Device(pk)) if DeviceId::of_public_key(&pk) == by => Some(FolderId::of_public_key(&target_pk)),
        _ => None,
    })
}

/// A device left the user's devices and the two others both changed the key at once, so two new groups: the one whose
/// group wins (the lowest id) told the other of its move while that one switched to its own new group, and what
/// reached the old group then was not what the switching device read of it afterwards. Neither learned anything new
/// later, so they stayed in two groups for good and stopped sharing what is on all their devices ("Read only" on on
/// one, off on the other). Fails when the two do not settle on one group, or a change on one does not reach the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_changing_the_key_at_once_settle_on_one_group() {
    let (a, b, h) = (Dev::new("Owner Mac").await, Dev::new("MacBook").await, Dev::new("Host Mac").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Films").await;
    pair(&a, &h).await;
    pair(&a, &b).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "B is an admin of the folder", || folder_role(&a, &f, &b) == Some(Role::Admin) && folder_role(&b, &f, &b) == Some(Role::Admin)).await;
    // The host leaves; A and B each change the key themselves (held until they do it on cue).
    for x in [&a, &b] {
        x.h.test_hold_key_changes(true);
    }
    h.h.devices_leave().await.unwrap();
    until(30, "A and B know the host left", || [&a, &b].iter().all(|x| listed(x) == ids(&[&a, &b]))).await;
    // B makes its new group and is held while it switches to it; A, which learned of B's move, makes one too, whose
    // id is lower (so it wins), and tells B while B is still switching.
    b.h.test_hold_switch(true);
    let bh = b.h.clone();
    let b_turn = tokio::spawn(async move { bh.test_rotate(None).await });
    until(20, "B is switching", || b.h.test_switches_waiting() == 1).await;
    until(20, "A knows of B's move", || moved_by(&a, &b).is_some()).await;
    let gb = moved_by(&a, &b).unwrap();
    let ga = a.h.test_rotate(Some(gb)).await.unwrap();
    // Everything A says of its move reaches B before B goes on: the new name, sent after it, comes in the same order.
    let mut s = a.h.settings();
    s.user_name = "Kim".into();
    a.h.set_settings(s).unwrap();
    until(20, "A's move and name reached B while it switches", || {
        moved_by(&b, &a) == Some(ga) && b.h.members(&f).is_ok_and(|m| m.iter().any(|x| x.device == a.id() && x.user == "Kim"))
    })
    .await;
    b.h.test_hold_switch(false);
    assert_eq!(b_turn.await.unwrap().unwrap(), gb);
    for x in [&a, &b] {
        x.h.test_hold_key_changes(false);
    }
    until(60, "one group, the two of them", || settled(&a, &b, &ids(&[&a, &b])) && settled(&b, &a, &ids(&[&a, &b]))).await;
    assert_eq!(a.h.test_group_id(), Some(ga), "the lowest group id wins");
    // They share what is on all their devices again: a change on one reaches the other.
    a.h.set_follow(&f, false).await.unwrap();
    until(40, "off all devices, on A and B", || !a.h.is_followed(&f) && !b.h.is_followed(&f)).await;
    shutdown(&[&a, &b, &h]).await;
}

/// Three devices left after one leaves, all three changing the key at once: B and C switch to their new groups at the
/// same moment while A makes the one that wins (the lowest id) and tells both meanwhile. Fails when what reached the old
/// group while B and C switched is lost to them (they would settle apart from A, or on two groups), or a change on one
/// does not reach the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_devices_changing_the_key_at_once_settle_on_one_group() {
    let (a, b, c, h) = (Dev::new("Owner Mac").await, Dev::new("MacBook").await, Dev::new("Studio PC").await, Dev::new("Host Mac").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Films").await;
    for x in [&h, &b, &c] {
        pair(&a, x).await;
    }
    a.h.set_follow(&f, true).await.unwrap();
    for x in [&b, &c] {
        until(40, "an admin of the folder", || folder_role(&a, &f, x) == Some(Role::Admin) && folder_role(x, &f, x) == Some(Role::Admin)).await;
    }
    let all = ids(&[&a, &b, &c]);
    for x in [&a, &b, &c] {
        x.h.test_hold_key_changes(true);
    }
    h.h.devices_leave().await.unwrap();
    until(30, "the three know the host left", || [&a, &b, &c].iter().all(|x| listed(x) == all)).await;
    // B and C make their new groups together and are held while they switch.
    for x in [&b, &c] {
        x.h.test_hold_switch(true);
    }
    let turns: Vec<_> = [&b, &c].iter().map(|x| {
        let h = x.h.clone();
        tokio::spawn(async move { h.test_rotate(None).await })
    }).collect();
    until(20, "B and C are switching", || b.h.test_switches_waiting() == 1 && c.h.test_switches_waiting() == 1).await;
    until(20, "A knows of both moves", || moved_by(&a, &b).is_some() && moved_by(&a, &c).is_some()).await;
    let below = moved_by(&a, &b).min(moved_by(&a, &c));
    let ga = a.h.test_rotate(below).await.unwrap();
    let mut s = a.h.settings();
    s.user_name = "Kim".into();
    a.h.set_settings(s).unwrap();
    until(20, "A's move and name reached B and C while they switch", || {
        [&b, &c].iter().all(|x| moved_by(x, &a) == Some(ga) && x.h.members(&f).is_ok_and(|m| m.iter().any(|m| m.device == a.id() && m.user == "Kim")))
    })
    .await;
    for x in [&b, &c] {
        x.h.test_hold_switch(false);
    }
    for t in turns {
        t.await.unwrap().unwrap();
    }
    for x in [&a, &b, &c] {
        x.h.test_hold_key_changes(false);
    }
    until(60, "one group, the three of them", || [&a, &b, &c].iter().all(|x| settled(x, &a, &all) && x.h.test_group_id() == Some(ga))).await;
    a.h.set_follow(&f, false).await.unwrap();
    until(40, "off all devices, on all three", || [&a, &b, &c].iter().all(|x| !x.h.is_followed(&f))).await;
    shutdown(&[&a, &b, &c, &h]).await;
}

/// 0.1.19's *Read only* lowered the user's other devices to Read only (owner grants) and only its own off gave them
/// back; that machinery is gone. Once this version runs, an owner must give each sibling it lowered its role back, once,
/// and a Read only that still counts on the group (the latest trusted Follow has the flag) must become the owner's
/// folder setting. Fails when a lowered sibling stays Read only, the old Read only is not carried over, the step runs
/// again (a Read only the user chooses later is undone), or this owner's own record of the old Read only turns it on
/// although the user turned it off on another device meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_old_read_only_gives_its_lowered_devices_back_once_and_carries_over_only_while_on() {
    let (a, b, c) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await, Dev::new("Studio PC").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Films").await;
    pair(&a, &b).await;
    pair(&a, &c).await;
    a.h.set_follow(&f, true).await.unwrap();
    for s in [&b, &c] {
        until(40, "an admin", || folder_role(&a, &f, s) == Some(Role::Admin)).await;
    }
    // What 0.1.19 left: its flag on the group's Follow, and B lowered after it came on (a later second).
    assert!(a.h.test_say_follow(&f, true, FOLLOW_READ_ONLY, None).await);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    a.h.set_role(&f, &b.id(), Role::ReadOnly).unwrap();
    until(30, "B lowered, as 0.1.19 did", || folder_role(&a, &f, &b) == Some(Role::ReadOnly)).await;
    assert!(!a.h.read_only(&f), "not the owners' setting yet");
    a.h.test_forget_read_only_migration(&f, None, None);
    until(30, "B has its role back, and the folder is Read only", || folder_role(&a, &f, &b) == Some(Role::Admin) && a.h.read_only(&f)).await;
    assert_eq!(folder_role(&a, &f, &c), Some(Role::Admin), "C was never lowered");
    // Once: Read only the user chooses later stays, whatever the next rounds do.
    a.h.set_role(&f, &c.id(), Role::ReadOnly).unwrap();
    a.h.set_follow(&f, false).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(folder_role(&a, &f, &c), Some(Role::ReadOnly), "the migration ran again");

    // This owner's own record says it was on (0.1.19's devices_read_only), but the user turned it off on another
    // device while this one was away (a trusted Follow without the flag): the lowered device comes back, and the folder
    // does not become Read only.
    a.h.set_read_only(&f, false).unwrap();
    assert!(a.h.test_say_follow(&f, true, 0, None).await);
    let came_on = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    a.h.set_role(&f, &b.id(), Role::ReadOnly).unwrap();
    until(30, "B lowered again", || folder_role(&a, &f, &b) == Some(Role::ReadOnly)).await;
    a.h.test_forget_read_only_migration(&f, Some((true, came_on)), None);
    until(30, "B has its role back", || folder_role(&a, &f, &b) == Some(Role::Admin)).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(!a.h.read_only(&f), "an owner's old record turned Read only on against the user's later off");
    shutdown(&[&a, &b, &c]).await;
}

/// Two owner devices update one after the other: the first gives the lowered sibling its role back, the user then
/// makes that sibling Read only, and the second owner's migration must leave that choice alone (it acts only on
/// decisions it held when it first opened the folder in this version). Fails when the second migration raises the
/// sibling again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_owners_migration_keeps_a_read_only_the_user_chose_after_the_first() {
    let (a, o2, s) = (Dev::new("Anna's Mac").await, Dev::new("Mac mini").await, Dev::new("Studio PC").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Films").await;
    pair(&a, &o2).await;
    pair(&a, &s).await;
    a.h.set_follow(&f, true).await.unwrap();
    for d in [&o2, &s] {
        until(40, "an admin", || folder_role(&a, &f, d) == Some(Role::Admin)).await;
    }
    a.h.set_role(&f, &o2.id(), Role::Owner).unwrap();
    until(40, "the second owner has the owner key", || folder_role(&o2, &f, &o2) == Some(Role::Owner) && o2.h.set_read_only(&f, false).is_ok()).await;
    // 0.1.19's state, on both owners: the flag, and S lowered after it came on.
    assert!(a.h.test_say_follow(&f, true, FOLLOW_READ_ONLY, None).await);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    a.h.set_role(&f, &s.id(), Role::ReadOnly).unwrap();
    until(30, "both owners see S lowered", || [&a, &o2].iter().all(|o| folder_role(o, &f, &s) == Some(Role::ReadOnly))).await;
    // The second owner updates now (what it holds is all 0.1.19's), but runs its migration later.
    let o2_upto = o2.h.test_max_serial(&f);
    // The first owner migrates: S gets its role back; then the user makes S Read only again.
    a.h.test_forget_read_only_migration(&f, None, None);
    until(30, "S raised by the first owner", || folder_role(&o2, &f, &s) == Some(Role::Admin)).await;
    a.h.set_role(&f, &s.id(), Role::ReadOnly).unwrap();
    until(30, "the second owner sees the user's choice", || folder_role(&o2, &f, &s) == Some(Role::ReadOnly)).await;
    // The second owner's migration.
    o2.h.test_forget_read_only_migration(&f, None, Some(o2_upto));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(folder_role(&a, &f, &s), Some(Role::ReadOnly), "the second owner's migration undid the user's choice");
    shutdown(&[&a, &o2, &s]).await;
}

/// *Read only* is the folder's own setting, in its membership: a restart, or the user's devices moving to a new group
/// key (after a removal), must keep it, and keep the user's other devices holding back. Fails when either loses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_holds_over_a_restart_and_a_new_group_key() {
    let (a, b, x) = (Dev::new("Anna's Mac").await, Dev::new("MacBook").await, Dev::new("Old Laptop").await);
    let work = tempfile::tempdir().unwrap();
    let f = folder(&a, work.path(), "Films").await;
    pair(&a, &b).await;
    pair(&a, &x).await;
    a.h.set_follow(&f, true).await.unwrap();
    until(40, "B is an admin, with the files", || folder_role(&a, &f, &b) == Some(Role::Admin) && read(&b.sync_dir().join("Films/seed.txt")).is_some()).await;
    a.h.set_read_only(&f, true).unwrap();
    until(30, "B holds back", || b.h.read_only(&f) && b.h.test_held_read_only(&f)).await;
    assert_eq!(folder_role(&a, &f, &b), Some(Role::Admin), "nobody's role changes");
    let b = b.restart().await;
    until(30, "B holds back after a restart", || b.h.read_only(&f) && b.h.test_held_read_only(&f)).await;
    let a = a.restart().await;
    assert!(a.h.read_only(&f), "the owner keeps it over a restart");
    // Both started anew, on new ports: B is told where A is now.
    b.h.add_address(&f, at(&a.h)).unwrap();
    until(30, "A and B meet again",|| a.h.devices_status().devices.iter().any(|d| d.id == b.id() && d.online)).await;
    // A new group key: the owner removes the old laptop.
    let old = a.h.test_group_id();
    a.h.devices_remove(&x.id(), &[]).await.unwrap();
    until(40, "B moved to the new key", || a.h.test_group_id() != old && settled(&b, &a, &ids(&[&a, &b]))).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(a.h.read_only(&f) && b.h.read_only(&f) && b.h.test_held_read_only(&f), "a new group key lost Read only");
    shutdown(&[&a, &b, &x]).await;
}
