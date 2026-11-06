//! Links that say where their maker is (SPEC §7G.5): a device joins from the addresses in the link alone (LAN
//! discovery and the DHT off, no address added), a dead address in it does not hold the join up, a joining folder
//! says what it does and what went wrong, and the owner notes the attempts it refused.

use std::net::{Ipv4Addr, SocketAddr};

use trs_sync_net::status::FolderStatus;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::{InviteLink, Role};
use trs_sync_proto::reach::LinkReach;

use crate::utp::{Device, addr, device_with, read, until};

async fn device(name: &str) -> Device {
    // (Loopback, LAN discovery off and no DHT: the link's addresses are the only way.)
    device_with(name, |_| {}).await
}

fn folder(d: &Device, id: &FolderId) -> Option<FolderStatus> {
    d.h.snapshot().folders.into_iter().find(|f| f.id == id.to_string())
}

/// A port on loopback where nothing listens ([`crate::closed_addr`]).
fn dead() -> SocketAddr {
    crate::closed_addr()
}

/// `link` with its address block replaced by `reach`.
fn with_reach(link: &str, reach: LinkReach) -> String {
    let l = InviteLink::parse(link).unwrap();
    InviteLink { reach: Some(reach), ..l }.format()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_and_a_channel_are_joined_from_the_addresses_in_their_links_alone() {
    let dir = tempfile::tempdir().unwrap();
    let a = device("Anna").await;
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("seed.txt"), b"from Anna").unwrap();
    let id: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link;
    let reach = InviteLink::parse(&link).unwrap().reach.expect("the link says where Anna is");
    assert_eq!((reach.device, reach.name.as_str(), reach.lan.clone(), reach.public), (a.h.device_id(), "Anna", vec![addr(&a)], None));

    let b = device("Bo").await;
    b.h.join_folder(&link, dir.path().join("B")).await.unwrap();
    until(30, "B has Anna's file, from the link's address alone", || read(&dir.path().join("B/seed.txt")).as_deref() == Some(b"from Anna")).await;
    let f = folder(&b, &id).unwrap();
    assert_eq!((f.role, f.progress, f.problem), (Some(Role::ReadWrite), None, None), "admitted: nothing to say");

    let ch: FolderId = a.h.chat_new("Plans", false).await.unwrap().id.parse().unwrap();
    let c = device("Cy").await;
    c.h.chat_join(&a.h.create_invite(&ch, Role::ReadWrite, 1, 7).unwrap().link).await.unwrap();
    until(30, "C is in the channel", || c.h.channel_status(&ch).is_some_and(|s| s.role == Some(Role::ReadWrite) && s.progress.is_none())).await;
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_address_in_the_link_does_not_hold_up_the_join() {
    let dir = tempfile::tempdir().unwrap();
    let a = device("Anna").await;
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let id: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link;
    // Dead addresses first (nothing listens there), then Anna's.
    let reach = LinkReach { device: a.h.device_id(), name: "Anna".into(), lan: vec![dead(), dead(), addr(&a)], public: Some(dead()), rendezvous: None };
    let b = device("Bo").await;
    let t = std::time::Instant::now();
    b.h.join_folder(&with_reach(&link, reach), dir.path().join("B")).await.unwrap();
    until(20, "B is admitted", || folder(&b, &id).is_some_and(|f| f.role == Some(Role::ReadOnly))).await;
    assert!(t.elapsed() < std::time::Duration::from_secs(10), "took {:?}", t.elapsed());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_joining_folder_says_what_it_does_and_what_went_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let a = device("Anna").await;
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let id: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    // Cy is an ordinary member: it cannot admit anyone.
    let c = device("Cy").await;
    c.h.join_folder(&a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dir.path().join("C")).await.unwrap();
    until(30, "C is a member", || folder(&c, &id).is_some_and(|f| f.role == Some(Role::ReadWrite))).await;

    // B's link says Anna is where nothing answers.
    let gone = dead();
    let link = with_reach(&a.h.create_invite(&id, Role::ReadOnly, 1, 7).unwrap().link, LinkReach { device: a.h.device_id(), name: "Anna".into(), lan: vec![gone], public: None, rendezvous: None });
    let b = device("Bo").await;
    let st = b.h.join_folder(&link, dir.path().join("B")).await.unwrap();
    let first = st.progress.clone().unwrap();
    assert!(first == "Looking for Anna at the addresses in the link…" || first == format!("Trying Anna at {gone}…"), "{first}");
    until(20, "B says Anna does not answer there", || {
        folder(&b, &id).and_then(|f| f.problem).is_some_and(|p| p == format!("Anna at {gone}: nothing answers there (is Seedwell running on it?)"))
    })
    .await;

    // Then B reaches Cy, who cannot admit it.
    b.h.add_address(&id, addr(&c)).unwrap();
    until(20, "B says Cy cannot admit it", || {
        folder(&b, &id).is_some_and(|f| {
            f.progress.as_deref().is_some_and(|p| p.starts_with("Found Cy, which cannot admit this ")) && f.problem.as_deref().is_some_and(|p| p.starts_with("Cy answered but does not admit this "))
        })
    })
    .await;
    assert!(a.h.snapshot().folders[0].join_attempts.is_empty(), "nothing refused by the owner");

    // Anna is found: admitted, nothing more to say.
    b.h.add_address(&id, addr(&a)).unwrap();
    until(20, "B is admitted", || folder(&b, &id).is_some_and(|f| f.role == Some(Role::ReadOnly) && f.progress.is_none() && f.problem.is_none())).await;
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_owner_notes_who_tried_a_used_up_link_and_the_joining_side_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let a = device("Anna").await;
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let id: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let ch: FolderId = a.h.chat_new("Plans", false).await.unwrap().id.parse().unwrap();
    let link = a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link;
    let chat_link = a.h.create_invite(&ch, Role::ReadWrite, 1, 7).unwrap().link;
    let b = device("Bo").await;
    b.h.join_folder(&link, dir.path().join("B")).await.unwrap();
    b.h.chat_join(&chat_link).await.unwrap();
    until(30, "B is in both", || {
        folder(&b, &id).is_some_and(|f| f.role.is_some()) && b.h.channel_status(&ch).is_some_and(|c| c.role.is_some())
    })
    .await;

    // The links are used up: the MacBook that tries them next is refused, and the owner says so.
    let c = device("Kim's MacBook").await;
    c.h.join_folder(&link, dir.path().join("C")).await.unwrap();
    c.h.chat_join(&chat_link).await.unwrap();
    until(30, "C is told", || {
        folder(&c, &id).is_some_and(|f| f.problem.as_deref() == Some("this link has been used up") && f.progress.is_none())
            && c.h.channel_status(&ch).is_some_and(|s| s.problem.as_deref() == Some("this link has been used up"))
    })
    .await;
    let tried = folder(&a, &id).unwrap().join_attempts;
    let t = tried.last().unwrap_or_else(|| panic!("no attempt noted: {tried:?}"));
    assert_eq!((t.name.as_str(), t.client.as_str(), t.reason.as_str()), ("Kim's MacBook", "Seedwell test", "this link has been used up"));
    assert_eq!(t.device_id, c.h.device_id().to_string());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(t.at_s <= now && t.at_s + 60 > now);
    let in_chat = a.h.channel_status(&ch).unwrap().join_attempts;
    assert!(in_chat.iter().any(|t| t.name == "Kim's MacBook" && t.reason == "this link has been used up"), "{in_chat:?}");
    assert!(folder(&b, &id).unwrap().join_attempts.is_empty(), "only the device that refused notes it");
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_not_connected_shows_when_it_was_last_seen_and_why_it_cannot_be_reached() {
    let dir = tempfile::tempdir().unwrap();
    let a = device("Anna").await;
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let id: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let b = device("Bo").await;
    b.h.join_folder(&a.h.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, dir.path().join("B")).await.unwrap();
    until(30, "connected", || a.h.snapshot().devices.len() == 1).await;
    assert!(a.h.snapshot().offline.is_empty(), "connected, so not offline");
    let bid = b.h.device_id().to_string();
    let port = b.h.port();
    b.h.shutdown().await;
    // Bo's port stays taken (bound, not listening: a dial is refused): in a full test run another test's device could
    // otherwise get it, and Anna would find "another device answers" there instead.
    let hold = tokio::net::TcpSocket::new_v4().unwrap();
    hold.set_reuseaddr(true).unwrap();
    hold.bind(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).unwrap();
    let shown = |a: &Device| a.h.snapshot().offline.into_iter().find(|d| d.id == bid);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    // Why, in either form: refused ("nothing answers"), or timed out ("no answer": a dial made while Bo shut down, or
    // one that a loaded machine let run past its time, and the next dial is only after the retry wait).
    let why = |p: &str| p.starts_with("nothing answers at ") || p.starts_with("no answer at ");
    while !shown(&a).is_some_and(|d| d.name == "Bo" && d.last_seen_s.is_some() && d.problem.as_deref().is_some_and(why)) {
        assert!(std::time::Instant::now() < end, "Anna lists Bo as not connected, with why; she shows {:?}", shown(&a));
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let o = a.h.snapshot().offline.into_iter().find(|d| d.id == bid).unwrap();
    assert_eq!(o.folders, vec![id.to_string()]);
    a.h.shutdown().await;
}
