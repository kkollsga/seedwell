//! Sessions over uTP: two services on loopback with TCP dialing off.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::session::Via;
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::ids::FolderId;

pub(crate) struct Device {
    pub(crate) h: SyncHandle,
    _state: tempfile::TempDir,
}

/// A service with its own temp state folder on loopback, LAN discovery off; `tune` changes the options.
pub(crate) async fn device_with(name: &str, tune: impl FnOnce(&mut Options)) -> Device {
    let state = tempfile::tempdir().unwrap();
    let secrets = Arc::new(FileSecrets::new(state.path().join("secrets")));
    let mut opts = Options::new(state.path().to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    tune(&mut opts);
    Device { h: SyncHandle::start(opts).await.unwrap(), _state: state }
}

pub(crate) fn addr(d: &Device) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), d.h.port())
}

/// Waits (polling) until `ok` holds, or fails after `secs`.
pub(crate) async fn until(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for: {what}");
}

pub(crate) fn read(p: &Path) -> Option<Vec<u8>> {
    std::fs::read(p).ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_sync_over_utp_only() {
    let no_tcp = |o: &mut Options| o.dial_tcp = false;
    let (a, b) = (device_with("Alpha", no_tcp).await, device_with("Beta", no_tcp).await);
    let (fa, fb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    std::fs::write(fa.path().join("hello.txt"), b"hello over uTP").unwrap();
    std::fs::write(fa.path().join("big.bin"), vec![3u8; 2_000_000]).unwrap();

    let st = a.h.add_folder(fa.path().to_path_buf()).await.unwrap();
    let id: FolderId = st.id.parse().unwrap();
    let key = a.h.shares(&id).unwrap().read_write_link.unwrap();
    let dest = fb.path().join("Shared");
    b.h.join_folder(&key, dest.clone()).await.unwrap();
    b.h.add_address(&id, addr(&a)).unwrap();

    until(30, "B has A's files", || {
        read(&dest.join("hello.txt")).as_deref() == Some(b"hello over uTP")
            && read(&dest.join("big.bin")).map(|v| v.len()) == Some(2_000_000)
    })
    .await;
    std::fs::write(dest.join("back.txt"), b"from B").unwrap();
    until(30, "A has B's file", || read(&fa.path().join("back.txt")).as_deref() == Some(b"from B")).await;

    for d in [&a, &b] {
        let t = d.h.transports();
        assert_eq!(t.len(), 1, "one session");
        assert_eq!(t[0].1, Via::Utp, "the session runs over uTP");
    }
    a.h.shutdown().await;
    b.h.shutdown().await;
}
