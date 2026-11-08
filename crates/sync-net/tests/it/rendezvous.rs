//! The rendezvous (SPEC §7G.3) against a loopback fake of the Worker: a device found through it alone (LAN discovery
//! and the DHT off, the link's addresses dead), a network change that writes a new record, the status, turning it off,
//! and a rendezvous that is down.

use std::net::SocketAddr;

use seedwell_rendezvous::testkit::fake_rendezvous;
use seedwell_sync_proto::ids::FolderId;
use seedwell_sync_proto::members::{InviteLink, Role};
use seedwell_sync_proto::rendezvous::{Record, record_id_hex};

use crate::utp::{addr, device_with, until};

/// A port on loopback where nothing listens ([`crate::closed_addr`]).
fn dead() -> SocketAddr {
    crate::closed_addr()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_joins_through_the_rendezvous_alone_and_a_network_change_writes_a_new_record() {
    let rv = fake_rendezvous().await;
    let dir = tempfile::tempdir().unwrap();
    let a = device_with("Anna", |_| {}).await;
    let st = a.h.set_rendezvous(&format!("{}/", rv.url())).await.unwrap();
    assert_eq!((st.url.as_str(), st.last_error.as_str()), (rv.url().as_str(), ""));
    assert!(st.last_publish_s > 0);
    assert_eq!(rv.count("PUT", "/v1/r/"), 1, "written at once");
    // The loop's first look (and the same setting again) find the record just written: no second write.
    a.h.set_rendezvous(&rv.url()).await.unwrap();
    assert_eq!(rv.count("PUT", "/v1/r/"), 1, "a record moments old is not written again");

    // Anna's record there: sealed, signed, her address in it.
    let id = record_id_hex(&a.h.device_id());
    let sealed = seedwell_rendezvous::client::Rendezvous::new(&rv.url()).unwrap().get(&id).await.unwrap().unwrap();
    let rec = Record::open(&sealed, &a.h.device_id()).unwrap();
    assert_eq!(rec.lan, vec![addr(&a)]);

    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("seed.txt"), b"found through the rendezvous").unwrap();
    let folder: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let link = InviteLink::parse(&a.h.create_invite(&folder, Role::ReadWrite, 1, 7).unwrap().link).unwrap();
    let mut reach = link.reach.clone().expect("the link says where Anna is");
    assert_eq!(reach.rendezvous.as_deref(), Some(rv.url().as_str()), "and where she publishes");
    // Her addresses in the link are dead: only the rendezvous can find her.
    reach.lan = vec![dead()];
    reach.public = None;
    let link = InviteLink { reach: Some(reach), ..link }.format();

    // Bo has no rendezvous of his own: he looks Anna up in hers.
    let b = device_with("Bo", |_| {}).await;
    b.h.join_folder(&link, dir.path().join("B")).await.unwrap();
    until(30, "Bo has Anna's file, found through the rendezvous", || {
        std::fs::read(dir.path().join("B/seed.txt")).ok().as_deref() == Some(b"found through the rendezvous".as_slice())
    })
    .await;
    assert!(rv.count("GET", &format!("/v1/r/{id}")) >= 2, "Bo looked Anna up");
    assert_eq!(b.h.rendezvous_status().found, 1);
    assert_eq!(b.h.rendezvous_status().url, "", "Bo publishes nothing");

    // A network change: a new record, a few seconds later at most.
    let puts = rv.count("PUT", "/v1/r/");
    a.h.network_changed();
    until(15, "a new record after the network change", || rv.count("PUT", "/v1/r/") > puts).await;
    assert_eq!(a.h.test_network_changes(), 1);

    // The test, then off: the record is deleted and the setting cleared.
    a.h.test_rendezvous().await.unwrap();
    a.h.turn_off_rendezvous().await.unwrap();
    assert_eq!(a.h.rendezvous_status().url, "");
    assert!(rv.count("DELETE", &format!("/v1/r/{id}")) >= 1);
    assert_eq!(seedwell_rendezvous::client::Rendezvous::new(&rv.url()).unwrap().get(&id).await.unwrap(), None);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rendezvous_that_is_down_is_said_so_and_breaks_nothing() {
    let rv = fake_rendezvous().await;
    let dir = tempfile::tempdir().unwrap();
    let a = device_with("Anna", |_| {}).await;
    a.h.set_rendezvous(&rv.url()).await.unwrap();
    rv.down.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(a.h.test_rendezvous().await.is_err());
    a.h.network_changed();
    until(15, "the error is shown", || !a.h.rendezvous_status().last_error.is_empty()).await;
    assert!(a.h.rendezvous_status().last_error.starts_with("The rendezvous cannot be reached"), "{}", a.h.rendezvous_status().last_error);

    // Folders still sync the other ways.
    let docs = dir.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("x.txt"), b"x").unwrap();
    let folder: FolderId = a.h.add_folder(docs).await.unwrap().id.parse().unwrap();
    let b = device_with("Bo", |_| {}).await;
    b.h.join_folder(&a.h.create_invite(&folder, Role::ReadOnly, 1, 7).unwrap().link, dir.path().join("B")).await.unwrap();
    until(30, "Bo has the file", || std::fs::read(dir.path().join("B/x.txt")).is_ok()).await;
    a.h.shutdown().await;
    b.h.shutdown().await;
}
