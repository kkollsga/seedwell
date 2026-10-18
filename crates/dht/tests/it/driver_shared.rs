//! The driver on somebody else's socket (`spawn_shared`): datagrams in through a channel, out
//! through a callback. Three nodes are wired together by a tiny in-memory "network" keyed by
//! address, so the test needs no sockets at all.

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;
use trs_dht::{Config, DriverConfig, LookupUpdate, PublicInfoHash, SharedIo, spawn_shared};

type Inbox = mpsc::Sender<(SocketAddrV4, Box<[u8]>)>;

fn addr(last: u8) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, last), 6881)
}

fn cfg(boot: Option<SocketAddrV4>) -> DriverConfig {
    DriverConfig {
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
        saved_state: None,
        alive: None,
    }
}

fn node(net: &Arc<Mutex<HashMap<SocketAddrV4, Inbox>>>, me: SocketAddrV4, boot: Option<SocketAddrV4>) -> trs_dht::DhtHandle {
    let (tx, rx) = mpsc::channel(256);
    net.lock().unwrap().insert(me, tx);
    let n = net.clone();
    let io = SharedIo {
        incoming: rx,
        send: Arc::new(move |to, data| {
            if let Some(inbox) = n.lock().unwrap().get(&to) {
                let _ = inbox.try_send((me, data.into()));
            }
        }),
        local: SocketAddr::V4(me),
    };
    spawn_shared(cfg(boot), io)
}

#[tokio::test(flavor = "current_thread")]
async fn nodes_on_shared_io_find_each_other() {
    let net = Arc::new(Mutex::new(HashMap::new()));
    let first = node(&net, addr(1), None);
    let mut nodes = vec![first];
    for i in 2..=8 {
        nodes.push(node(&net, addr(i), Some(addr(1))));
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(nodes[3].local_addr(), SocketAddr::V4(addr(4)));
    for n in &nodes {
        assert!(n.info().await.unwrap().table_nodes >= 2);
    }
    let ih = PublicInfoHash::new([0x42; 20], false).unwrap();
    let mut s = nodes[2].announce(ih, 7001).await.unwrap().unwrap();
    while let Ok(Some(u)) = tokio::time::timeout(Duration::from_secs(10), s.next()).await {
        if matches!(u, LookupUpdate::Announced { .. }) {
            break;
        }
    }
    let mut s = nodes[6].get_peers(ih).await.unwrap().unwrap();
    let mut found = Vec::new();
    while let Ok(Some(u)) = tokio::time::timeout(Duration::from_secs(10), s.next()).await {
        match u {
            LookupUpdate::Peers(p) => found.extend(p),
            LookupUpdate::Finished(_) => break,
            LookupUpdate::Announced { .. } => {}
        }
    }
    assert!(found.contains(&SocketAddrV4::new(*addr(3).ip(), 7001)), "{found:?}");
    // The node ends when the socket owner drops its end of the feed.
    drop(net);
    for n in nodes {
        let _ = n.shutdown().await;
    }
}
