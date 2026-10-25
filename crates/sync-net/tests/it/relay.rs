//! Reaching a device that cannot be dialed: a member relay (through a third device that shares the
//! folder), a stand-alone relay server, and the relay's access rules. Everything on loopback; "cannot be
//! dialed" is the services' test switch that refuses direct sessions between two devices.

use tokio::net::TcpListener;
use trs_sync_net::session::Via;
use trs_sync_net::{RelayOptions, run_relay};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::members::Role;

use crate::utp::{Device, addr, device_with, read, until};

async fn device(name: &str) -> Device {
    device_with(name, |_| {}).await
}

fn via_to(d: &Device, other: DeviceId) -> Option<Via> {
    d.h.transports().into_iter().find(|(id, _)| *id == other).map(|(_, v)| v)
}

fn snapshot_via(d: &Device, other: DeviceId) -> Option<String> {
    d.h.snapshot().devices.into_iter().find(|x| x.id == other.to_string()).map(|x| x.via)
}

/// C shares a folder; A and B join it (B with `b_key`: "rw" or "ro"), all reach C, A and B meet directly,
/// then the test switch makes A and B unable to reach each other directly.
async fn three(b_key: &str) -> (Device, Device, Device, FolderId, Vec<tempfile::TempDir>) {
    let (a, b, c) = (device("Alpha").await, device("Beta").await, device("Hub").await);
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    std::fs::write(dirs[2].path().join("from-c.txt"), b"hub").unwrap();
    let st = c.h.add_folder(dirs[2].path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let invite = |role| c.h.create_invite(&id, role, 1, 1).unwrap().link;
    a.h.join_folder(&invite(Role::ReadWrite), dirs[0].path().join("F")).await.unwrap();
    let bk = if b_key == "rw" { invite(Role::ReadWrite) } else { invite(Role::ReadOnly) };
    b.h.join_folder(&bk, dirs[1].path().join("F")).await.unwrap();
    a.h.add_address(&id, addr(&c)).unwrap();
    b.h.add_address(&id, addr(&c)).unwrap();
    a.h.add_address(&id, addr(&b)).unwrap();
    let (ai, bi, ci) = (a.h.device_id(), b.h.device_id(), c.h.device_id());
    until(30, "A, B and C all connected", || {
        via_to(&a, bi).is_some() && via_to(&a, ci).is_some() && via_to(&b, ci).is_some() && via_to(&b, ai).is_some()
    })
    .await;
    until(30, "A and B have C's file", || {
        read(&dirs[0].path().join("F/from-c.txt")).is_some() && read(&dirs[1].path().join("F/from-c.txt")).is_some()
    })
    .await;
    a.h.test_refuse_direct(bi);
    b.h.test_refuse_direct(ai);
    (a, b, c, id, dirs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_relays_between_devices_that_cannot_reach_each_other() {
    let (a, b, c, _id, dirs) = three("rw").await;
    let (ai, bi) = (a.h.device_id(), b.h.device_id());
    until(60, "A and B connected through C", || via_to(&a, bi) == Some(Via::Relay) && via_to(&b, ai) == Some(Via::Relay)).await;
    assert_eq!(snapshot_via(&a, bi).as_deref(), Some("Relay"));
    assert_eq!(snapshot_via(&b, ai).as_deref(), Some("Relay"));
    assert!(c.h.relay_stats().relayed >= 1);
    assert!(c.h.relay_stats().forwarding >= 1);

    // A file goes from A to B (over the relayed session or through C; both are fine).
    std::fs::write(dirs[0].path().join("F/from-a.txt"), vec![9u8; 200_000]).unwrap();
    until(30, "B has A's file", || read(&dirs[1].path().join("F/from-a.txt")).map(|v| v.len()) == Some(200_000)).await;
    assert_eq!(via_to(&a, bi), Some(Via::Relay), "still relayed");
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_refuses_to_relay_without_the_access_it_requires() {
    // B has only Read access: "own devices" (Write on both sides) does not hold.
    let (a, b, c, _id, _dirs) = three("ro").await;
    let bi = b.h.device_id();
    until(40, "C refused a relay", || c.h.relay_stats().refused >= 1).await;
    assert_eq!(c.h.relay_stats().relayed, 0, "nothing relayed");
    assert_ne!(via_to(&a, bi), Some(Via::Relay));

    // Relaying for everyone in shared folders: now it is allowed.
    let mut s = c.h.settings();
    s.relay_everyone = true;
    c.h.set_settings(s).unwrap();
    until(90, "A and B connected through C", || via_to(&a, bi) == Some(Via::Relay)).await;
    assert!(c.h.relay_stats().relayed >= 1);
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_registered_at_a_relay_server_sync_through_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let opts = RelayOptions::new(DeviceKey::generate(&mut ring::rand::SystemRandom::new()).unwrap());
    let url = opts.url(&format!("127.0.0.1:{port}"));
    let server = tokio::spawn(run_relay(listener, opts));

    let (a, b) = (device("Alpha").await, device("Beta").await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("first.txt"), b"direct").unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let key = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Shared");
    b.h.join_folder(&key, dest.clone()).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B has the first file", || read(&dest.join("first.txt")).is_some()).await;

    a.h.add_relay(&id, &url).unwrap();
    b.h.add_relay(&id, &url).unwrap();
    let (ai, bi) = (a.h.device_id(), b.h.device_id());
    a.h.test_refuse_direct(bi);
    b.h.test_refuse_direct(ai);
    until(60, "A and B connected through the relay server", || {
        via_to(&a, bi) == Some(Via::Relay) && via_to(&b, ai) == Some(Via::Relay)
    })
    .await;
    assert_eq!(snapshot_via(&a, bi).as_deref(), Some("Relay"));

    // With direct sessions refused and no third device, this file can only come through the relay.
    std::fs::write(fa.path().join("second.bin"), vec![5u8; 1_000_000]).unwrap();
    until(30, "B has the second file", || read(&dest.join("second.bin")).map(|v| v.len()) == Some(1_000_000)).await;
    a.h.shutdown().await;
    b.h.shutdown().await;
    server.abort();
}

fn texts(d: &Device, id: &FolderId) -> Vec<String> {
    d.h.chat_read(id, None, None, None).unwrap_or_default().into_iter().map(|m| m.body).collect()
}

/// A channel only (no folder): C makes it, A and B join, all meet, then A and B cannot reach each other directly.
async fn three_in_a_channel() -> (Device, Device, Device, FolderId) {
    let (a, b, c) = (device("Alpha").await, device("Beta").await, device("Hub").await);
    let id: FolderId = c.h.chat_new("Plans").await.unwrap().id.parse().unwrap();
    for d in [&a, &b] {
        d.h.chat_join(&c.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link).await.unwrap();
        d.h.add_address(&id, addr(&c)).unwrap();
    }
    a.h.add_address(&id, addr(&b)).unwrap();
    let (ai, bi, ci) = (a.h.device_id(), b.h.device_id(), c.h.device_id());
    until(30, "A, B and C all connected", || {
        via_to(&a, bi).is_some() && via_to(&a, ci).is_some() && via_to(&b, ci).is_some() && via_to(&b, ai).is_some()
    })
    .await;
    a.h.test_refuse_direct(bi);
    b.h.test_refuse_direct(ai);
    (a, b, c, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_member_relays_a_channel_between_devices_that_cannot_reach_each_other() {
    let (a, b, c, id) = three_in_a_channel().await;
    let (ai, bi) = (a.h.device_id(), b.h.device_id());
    until(60, "A and B connected through C", || via_to(&a, bi) == Some(Via::Relay) && via_to(&b, ai) == Some(Via::Relay)).await;
    assert!(c.h.relay_stats().relayed >= 1);
    // C leaves the channel: it still relays the session (it shares nothing with them now, but the pipe stands),
    // and the message can only come over the relayed session.
    c.h.chat_leave(&id).unwrap();
    a.h.chat_send(&id, None, trs_sync_net::chat::MessageKind::Text, "over the relay", None, None).await.unwrap();
    until(30, "B has A's message", || texts(&b, &id) == ["over the relay"]).await;
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn devices_registered_at_a_relay_server_chat_through_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let opts = RelayOptions::new(DeviceKey::generate(&mut ring::rand::SystemRandom::new()).unwrap());
    let url = opts.url(&format!("127.0.0.1:{port}"));
    let server = tokio::spawn(run_relay(listener, opts));

    let (a, b) = (device("Alpha").await, device("Beta").await);
    let id: FolderId = a.h.chat_new("Plans").await.unwrap().id.parse().unwrap();
    b.h.chat_join(&a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();
    until(30, "B is admitted", || b.h.channel_status(&id).is_some_and(|c| c.role.is_some())).await;

    a.h.add_relay(&id, &url).unwrap();
    b.h.add_relay(&id, &url).unwrap();
    let (ai, bi) = (a.h.device_id(), b.h.device_id());
    a.h.test_refuse_direct(bi);
    b.h.test_refuse_direct(ai);
    until(60, "A and B connected through the relay server", || {
        via_to(&a, bi) == Some(Via::Relay) && via_to(&b, ai) == Some(Via::Relay)
    })
    .await;
    // With direct sessions refused and no third device, these can only come through the relay.
    a.h.chat_send(&id, None, trs_sync_net::chat::MessageKind::Text, "through the server", None, None).await.unwrap();
    until(30, "B has A's message", || texts(&b, &id) == ["through the server"]).await;
    b.h.chat_send(&id, None, trs_sync_net::chat::MessageKind::Text, "and back", None, None).await.unwrap();
    until(30, "A has B's answer", || texts(&a, &id).last().is_some_and(|t| t == "and back")).await;
    a.h.shutdown().await;
    b.h.shutdown().await;
    server.abort();
}
