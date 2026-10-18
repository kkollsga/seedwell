//! The tokio driver over real UDP sockets on loopback (no internet).

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;
use trs_dht::{Config, DhtHandle, DriverConfig, LookupUpdate, PublicInfoHash, Refused, spawn};

fn cfg(boot: Option<SocketAddr>, saved: Option<Vec<u8>>) -> DriverConfig {
    DriverConfig {
        // Every test node shares 127.0.0.1, so lift the per-IP response limit (and one-node-per-IP).
        core: Config {
            allow_local_addrs: true,
            unique_ip_per_table: false,
            secure_node_id: false,
            resp_ip_burst: 10_000,
            resp_ip_rate: 10_000,
            ..Config::default()
        },
        bind: SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
        bootstrap_hosts: boot.map(|b| vec![b.to_string()]).unwrap_or_default(),
        saved_state: saved,
        alive: None,
    }
}

async fn network(n: usize) -> Vec<DhtHandle> {
    let first = spawn(cfg(None, None)).await.unwrap();
    let boot = first.local_addr();
    let mut v = vec![first];
    for _ in 1..n {
        v.push(spawn(cfg(Some(boot), None)).await.unwrap());
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Everyone (including the bootstrap node) learns about the others through traffic.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    v
}

async fn drain(
    mut s: trs_dht::PeerStream,
) -> (
    Vec<SocketAddrV4>,
    Option<trs_dht::LookupStats>,
    Option<(u8, u8)>,
) {
    let mut peers = Vec::new();
    let mut fin = None;
    let mut ann = None;
    while let Ok(Some(u)) = tokio::time::timeout(Duration::from_secs(20), s.next()).await {
        match u {
            LookupUpdate::Peers(p) => peers.extend(p),
            LookupUpdate::Finished(st) => fin = Some(st),
            LookupUpdate::Announced {
                accepted,
                attempted,
            } => ann = Some((accepted, attempted)),
        }
    }
    (peers, fin, ann)
}

#[tokio::test(flavor = "current_thread")]
async fn nodes_bootstrap_announce_and_find_each_other_over_udp() {
    let nodes = network(12).await;
    for n in &nodes {
        let i = n.info().await.unwrap();
        assert!(i.table_nodes >= 3, "table {} {:?}", i.table_nodes, i);
    }
    let ih = PublicInfoHash::new([0x11; 20], false).unwrap();
    // Three nodes announce distinct "BitTorrent ports".
    for (k, n) in nodes[1..4].iter().enumerate() {
        let stream = n.announce(ih, 7001 + k as u16).await.unwrap().unwrap();
        let (_, fin, ann) = drain(stream).await;
        assert!(fin.is_some());
        let (accepted, attempted) = ann.expect("announce round finished");
        assert!(attempted >= 1 && accepted >= 1, "{accepted}/{attempted}");
    }
    // Another node finds them.
    let stream = nodes[9].get_peers(ih).await.unwrap().unwrap();
    let (peers, fin, ann) = drain(stream).await;
    assert!(ann.is_none());
    let fin = fin.unwrap();
    assert!(fin.queries > 0 && fin.replied > 0);
    let mut ports: Vec<u16> = peers.iter().map(|p| p.port()).collect();
    ports.sort_unstable();
    ports.dedup();
    assert_eq!(ports, vec![7001, 7002, 7003], "{peers:?}");
    // Asking again immediately is refused by the per-hash cooldown.
    match nodes[9].get_peers(ih).await.unwrap() {
        Err(Refused::Cooldown { .. }) => {}
        Err(other) => panic!("{other:?}"),
        Ok(_) => panic!("cooldown not enforced"),
    }
    // The same hash marked forbidden is refused outright.
    nodes[8].forbid([0x11; 20]).await.unwrap();
    assert!(matches!(
        nodes[8].get_peers(ih).await.unwrap(),
        Err(Refused::Forbidden)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn state_survives_restart() {
    let nodes = network(8).await;
    let saved = nodes[4].save_state().await.unwrap();
    assert!(saved.len() > 27);
    let id = nodes[4].info().await.unwrap().node_id;
    let boot_state = nodes[4].shutdown().await.unwrap();
    assert!(boot_state.len() > 27);
    // A fresh node with only the saved state (no bootstrap hosts) rejoins.
    let n = spawn(cfg(None, Some(saved))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let i = n.info().await.unwrap();
    assert_eq!(i.node_id, id, "node id restored");
    assert!(
        i.table_nodes >= 2,
        "rejoined via persisted nodes, table {}",
        i.table_nodes
    );
    // Handle to the stopped node reports an error instead of hanging.
    assert!(nodes[4].info().await.is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn lone_node_answers_ping_and_survives_garbage() {
    // A lone node with nowhere to bootstrap backs off; it must still answer a ping promptly.
    let a = spawn(cfg(None, None)).await.unwrap();
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    sock.send_to(
        b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe",
        a.local_addr(),
    )
    .await
    .unwrap();
    let mut buf = [0u8; 512];
    // The node answers and also pings us back to verify the sender; look for the answer.
    let mut answered = false;
    for _ in 0..2 {
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        let m = trs_dht::codec::parse(&buf[..n]).unwrap();
        if matches!(m.body, trs_dht::codec::Body::Response(_)) {
            assert_eq!(m.tid, b"aa");
            answered = true;
            break;
        }
    }
    assert!(answered);
    // Garbage does not kill the task.
    sock.send_to(b"\xff\xfe garbage", a.local_addr())
        .await
        .unwrap();
    assert!(a.info().await.is_ok());
}

#[tokio::test(flavor = "current_thread")]
async fn task_stops_and_releases_the_port_when_all_handles_drop() {
    let a = spawn(cfg(None, None)).await.unwrap();
    let addr = a.local_addr();
    drop(a);
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        if tokio::net::UdpSocket::bind(addr).await.is_ok() {
            return;
        }
    }
    panic!("port {addr} still held by the dht task");
}
