//! LAN discovery on loopback: each service listens on its own discovery port and announces to the others by
//! unicast (the test hook), so no broadcast or multicast leaves the machine.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use trs_sync_net::lan::{Announcement, unix_hour};
use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::keys::{FolderKey, FolderSecret};
use trs_sync_proto::members::{InviteLink, Role};

struct Device {
    h: SyncHandle,
    _state: tempfile::TempDir,
}

fn lo(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// A UDP port nobody uses right now (bound and released).
fn free_udp_port() -> u16 {
    UdpSocket::bind(lo(0)).unwrap().local_addr().unwrap().port()
}

async fn device(name: &str, lan_port: u16, targets: Vec<SocketAddr>) -> Device {
    let state = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
    let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = true;
    opts.lan_port = lan_port;
    opts.lan_targets = targets;
    Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
}

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
async fn devices_find_each_other_on_the_lan_and_sync() {
    let (pa, pb) = (free_udp_port(), free_udp_port());
    let a = device("Alpha", pa, vec![lo(pb)]).await;
    let b = device("Beta", pb, vec![lo(pa)]).await;
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("hello.txt"), b"found on the LAN").unwrap();

    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let key = a.h.create_invite(&id, Role::ReadWrite, 1, 1).unwrap().link;
    let dest = fb.path().join("Shared");
    // No address typed in: only the announcements bring them together.
    b.h.join_folder(&key, dest.clone()).await.unwrap();

    until(30, "B has A's file", || read(&dest.join("hello.txt")).as_deref() == Some(b"found on the LAN")).await;
    let snap = a.h.snapshot();
    assert_eq!(snap.devices.len(), 1, "one session");
    assert_eq!(snap.devices[0].name, "Beta");
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A service with one folder, listening for announcements on its own port; and that folder's key.
async fn listener_with_folder() -> (Device, u16, FolderKey, tempfile::TempDir) {
    let pa = free_udp_port();
    let a = device("Alpha", pa, Vec::new()).await;
    let fa = tempfile::tempdir().unwrap();
    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let link = InviteLink::parse(&a.h.create_invite(&id, Role::ReadOnly, 1, 1).unwrap().link).unwrap();
    let key = FolderKey::Member(link.invite.folder_pk);
    (a, pa, key, fa)
}

/// An announcement by `stranger` for the folder of `key`, pointing at `port`.
fn announcement(stranger: &DeviceKey, key: &FolderKey, hour: u32, port: u16) -> Announcement {
    let nonce = [5u8; 8];
    Announcement { device_pk: *stranger.public_key(), instance: 42, port, hour, nonce, tags: vec![key.lan_tag(hour, &nonce)] }
}

fn send(to: u16, pkt: &[u8]) {
    let s = UdpSocket::bind(lo(0)).unwrap();
    s.send_to(pkt, lo(to)).unwrap();
}

/// Is `tcp` dialed within `secs`?
async fn dialed(tcp: &TcpListener, secs: u64) -> bool {
    tokio::time::timeout(Duration::from_secs(secs), tcp.accept()).await.is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_badly_signed_announcement_is_ignored() {
    let (a, pa, key, _fa) = listener_with_folder().await;
    let tcp = TcpListener::bind(lo(0)).await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let stranger = DeviceKey::from_seed(&[7; 32]);
    let ann = announcement(&stranger, &key, unix_hour(), port);

    // Signed by another key than the one it names; and a flipped signature byte.
    send(pa, &ann.encode(&DeviceKey::from_seed(&[8; 32])));
    let mut flipped = ann.encode(&stranger);
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    send(pa, &flipped);
    assert!(!dialed(&tcp, 3).await, "a bad signature leads to no dial");

    // The same announcement, signed properly, is dialed (the test can see a dial).
    send(pa, &ann.encode(&stranger));
    assert!(dialed(&tcp, 10).await, "a good announcement is dialed");
    a.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_announcement_for_another_folder_or_hour_is_not_dialed() {
    let (a, pa, key, _fa) = listener_with_folder().await;
    let tcp = TcpListener::bind(lo(0)).await.unwrap();
    let port = tcp.local_addr().unwrap().port();
    let stranger = DeviceKey::from_seed(&[9; 32]);
    let hour = unix_hour();

    let other = FolderKey::Owner(FolderSecret::generate(&mut ring::rand::SystemRandom::new()).unwrap());
    send(pa, &announcement(&stranger, &other, hour, port).encode(&stranger));
    // Our folder, but three hours old (a replay).
    send(pa, &announcement(&stranger, &key, hour - 3, port).encode(&stranger));
    assert!(!dialed(&tcp, 3).await, "no dial for a folder we do not have, or an old hour");

    // An hour off is still accepted (clocks differ around the hour).
    send(pa, &announcement(&stranger, &key, hour - 1, port).encode(&stranger));
    assert!(dialed(&tcp, 10).await, "our folder is dialed");
    a.h.shutdown().await;
}
