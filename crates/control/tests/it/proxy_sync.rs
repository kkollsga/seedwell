//! The torrent proxy (SPEC §7H) is for torrents only: folder sync and chat between devices never go through it and
//! never wait for it, also while it holds every torrent.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use trs_engine::{Engine, EngineConfig, MemStore, ProxyConfig, ProxyHealth};
use trs_socks::testkit::FakeProxy;
use trs_sync_net::SyncHandle;
use trs_sync_net::chat::MessageKind;
use trs_sync_net::secrets::FileSecrets;

struct Device {
    root: tempfile::TempDir,
    engine: Option<Engine>,
    h: SyncHandle,
}

async fn device(fake: &FakeProxy, name: &str) -> Device {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let ec = EngineConfig {
        listen_addr: Ipv4Addr::LOCALHOST,
        default_save_dir: root.path().join("dl"),
        dht_enabled: false,
        trackers_enabled: false,
        proxy: Some(ProxyConfig {
            servers: vec![fake.addr().ip().to_string()],
            port: fake.addr().port(),
            auth: Some(trs_engine::proxy::Credentials::new("user", "pass").unwrap()),
            ..Default::default()
        }),
        ..EngineConfig::default()
    };
    let engine = Engine::start(ec, Arc::new(MemStore::new())).await.unwrap();
    let secrets = Arc::new(FileSecrets::new(state.join("secrets")));
    let name = name.to_owned();
    let h = trs_control::sync::start_sync_with(&state, "Seedwell test".into(), secrets, Some(&engine.handle()), move |o| {
        o.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        o.lan_discovery = false;
        o.default_device_name = name;
    })
    .await
    .unwrap();
    Device { root, engine: Some(engine), h }
}

async fn wait_until(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
    let end = tokio::time::Instant::now() + Duration::from_secs(secs);
    while !f() {
        assert!(tokio::time::Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Failure: chat or a synced folder's files go through the torrent proxy (the user's decision: NordVPN never sees
/// folder sync or chats), or wait with the torrents when the proxy is down.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn chat_and_folder_sync_between_devices_never_go_through_the_torrent_proxy() {
    let fake = FakeProxy::start("user", "pass").await;
    let mut mac = device(&fake, "Mac").await;
    let mut pc = device(&fake, "PC").await;
    let id: trs_sync_proto::ids::FolderId = mac.h.chat_new("Proxy test", false).await.unwrap().id.parse().unwrap();
    let link = mac.h.create_invite(&id, trs_sync_proto::members::Role::ReadWrite, 2, 7).unwrap().link;
    pc.h.chat_join(&link).await.unwrap();
    pc.h.add_address(&id, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), mac.h.port())).unwrap();
    wait_until(30, "the PC is admitted", || pc.h.channel_status(&id).is_some_and(|c| c.role.is_some())).await;
    mac.h.chat_send(&id, None, MessageKind::Text, "through no proxy", None, None).await.unwrap();
    wait_until(20, "the message reaches the PC", || pc.h.chat_read(&id, None, None, None).is_ok_and(|m| m.len() == 1)).await;
    // A folder with a file, synced to the PC.
    let docs = mac.root.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"first file").unwrap();
    let folder: trs_sync_proto::ids::FolderId = mac.h.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    let flink = mac.h.create_invite(&folder, trs_sync_proto::members::Role::ReadWrite, 2, 7).unwrap().link;
    let there = pc.root.path().join("Docs");
    pc.h.join_folder(&flink, there.clone()).await.unwrap();
    pc.h.add_address(&folder, SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), mac.h.port())).unwrap();
    wait_until(60, "the file reaches the PC", || std::fs::read(there.join("a.txt")).is_ok_and(|b| b == b"first file")).await;
    // (The engines test the proxy with handshakes; a connection anywhere through it would be a CONNECT.)
    assert!(fake.connects().is_empty(), "nothing of sync went through the proxy: {:?}", fake.connects());

    // The proxy goes down: torrents are held, the devices still talk.
    fake.stop().await;
    for d in [&mac, &pc] {
        let e = d.engine.as_ref().unwrap().handle();
        e.set_proxy(Some(ProxyConfig {
            servers: vec![fake.addr().ip().to_string()],
            port: fake.addr().port(),
            auth: Some(trs_engine::proxy::Credentials::new("user", "other").unwrap()),
            ..Default::default()
        }));
        wait_until(20, "the proxy is down", || e.proxy_status().health == ProxyHealth::Down).await;
    }
    pc.h.chat_send(&id, None, MessageKind::Text, "still here", None, None).await.unwrap();
    wait_until(20, "the reply reaches the Mac", || mac.h.chat_read(&id, None, None, None).is_ok_and(|m| m.len() == 2)).await;
    std::fs::write(docs.join("b.txt"), b"while the proxy is down").unwrap();
    wait_until(60, "the second file reaches the PC", || std::fs::read(there.join("b.txt")).is_ok_and(|b| b == b"while the proxy is down")).await;

    for d in [&mut mac, &mut pc] {
        d.h.shutdown().await;
        if let Some(e) = d.engine.take() {
            e.shutdown(Duration::from_secs(1)).await;
        }
    }
}
