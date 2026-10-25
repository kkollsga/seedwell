//! Port mapping of the listen port against a NAT-PMP gateway on loopback.
use crate::common;
use std::collections::BTreeMap;
use std::net::SocketAddrV4;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use tokio::net::UdpSocket;
use trs_engine::config::PortmapConfig;
use trs_engine::*;

#[derive(Default)]
struct Gateway {
    /// (protocol 1 = UDP, 2 = TCP, internal port) -> external port
    table: BTreeMap<(u8, u16), u16>,
    deletes: Vec<(u8, u16)>,
    requests: usize,
}

/// A NAT-PMP-only gateway: version-2 (PCP) requests are answered with "unsupported version".
async fn gateway() -> (SocketAddrV4, Arc<Mutex<Gateway>>) {
    gateway_shifted(0).await
}

/// Like [`gateway`], but it grants `asked + shift` as the external port (a taken port).
async fn gateway_shifted(shift: u16) -> (SocketAddrV4, Arc<Mutex<Gateway>>) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = match sock.local_addr().unwrap() {
        std::net::SocketAddr::V4(a) => a,
        _ => unreachable!(),
    };
    let state = Arc::new(Mutex::new(Gateway::default()));
    let st = state.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else { break };
            if n < 2 {
                continue;
            }
            st.lock().unwrap().requests += 1;
            let (version, op) = (buf[0], buf[1]);
            let reply: Vec<u8> = if version != 0 {
                vec![0, 128 | (op & 0x7f), 0, 1, 0, 0, 0, 1]
            } else if op == 0 {
                vec![0, 128, 0, 0, 0, 0, 0, 1, 203, 0, 113, 9]
            } else if (op == 1 || op == 2) && n >= 12 {
                let internal = u16::from_be_bytes([buf[4], buf[5]]);
                let asked = u16::from_be_bytes([buf[6], buf[7]]);
                let life = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                let mut g = st.lock().unwrap();
                let ext = if asked == 0 { internal } else { asked }.wrapping_add(shift);
                if life == 0 {
                    g.table.remove(&(op, internal));
                    g.deletes.push((op, internal));
                } else {
                    g.table.insert((op, internal), ext);
                }
                let mut r = vec![0, 128 | op, 0, 0, 0, 0, 0, 1];
                r.extend(internal.to_be_bytes());
                r.extend(if life == 0 { 0 } else { ext }.to_be_bytes());
                r.extend(life.to_be_bytes());
                r
            } else {
                continue;
            };
            let _ = sock.send_to(&reply, from).await;
        }
    });
    (addr, state)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_listen_port_is_mapped_and_unmapped_within_the_shutdown_deadline() {
    let (gw_addr, gw) = gateway().await;
    let mut n = node(|c| {
        c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw_addr) };
    })
    .await;
    let mut ev = n.h.subscribe();
    let port = n.h.listen_port();
    let status = wait_event(&mut ev, 20, |e| matches!(e, EngineEvent::PortMap(s) if s.external_tcp_port.is_some() && s.external_udp_port.is_some())).await;
    let EngineEvent::PortMap(s) = status else { unreachable!() };
    assert_eq!(s.method, Some("nat-pmp"));
    assert_eq!((s.external_tcp_port, s.external_udp_port), (Some(port), Some(port)));
    assert_eq!(s.external_ip, Some("203.0.113.9".parse().unwrap()));
    assert!(!s.behind_cgnat);
    {
        let g = gw.lock().unwrap();
        assert_eq!(g.table.get(&(2, port)), Some(&port), "TCP");
        assert_eq!(g.table.get(&(1, port)), Some(&port), "UDP");
    }
    // A network change makes the driver look again; it must not storm the gateway.
    let before = gw.lock().unwrap().requests;
    n.h.network_changed();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(gw.lock().unwrap().requests - before <= 6, "polite after a network change");

    let t0 = Instant::now();
    n.shutdown(Duration::from_secs(10)).await;
    assert!(t0.elapsed() < Duration::from_secs(5), "{:?}", t0.elapsed());
    let g = gw.lock().unwrap();
    assert!(g.table.is_empty(), "mappings deleted at shutdown: {:?}", g.table);
    assert_eq!(g.deletes.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nothing_is_sent_to_the_gateway_unless_enabled_and_it_follows_the_settings() {
    let (gw_addr, gw) = gateway().await;
    let n = node(|c| {
        c.portmap = PortmapConfig { enabled: false, upnp: true, natpmp: true, test_gateway: Some(gw_addr) };
    })
    .await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(gw.lock().unwrap().requests, 0);
    // Turning it on at runtime starts the mapping; turning it off deletes it.
    let mut ev = n.h.subscribe();
    n.h.update_config(move |c| c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw_addr) });
    wait_event(&mut ev, 20, |e| matches!(e, EngineEvent::PortMap(s) if s.external_tcp_port.is_some() && s.external_udp_port.is_some())).await;
    assert_eq!(gw.lock().unwrap().table.len(), 2);
    n.h.update_config(move |c| c.portmap = PortmapConfig { enabled: false, upnp: false, natpmp: false, test_gateway: Some(gw_addr) });
    wait_for("mappings removed", 10, || async { gw.lock().unwrap().table.is_empty().then_some(()) }).await;
}

/// A live port change moves the mapping: the old one is deleted, the new one added.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mapping_follows_a_live_port_change() {
    let (gw_addr, gw) = gateway().await;
    let n = node(|c| {
        c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw_addr) };
    })
    .await;
    let old = n.h.listen_port();
    wait_for("mapped", 20, || async { (gw.lock().unwrap().table.len() == 2).then_some(()) }).await;
    let new = free_port();
    n.h.update_config(move |c| c.listen_port = new);
    wait_for("moved", 20, || async {
        let g = gw.lock().unwrap();
        (g.table.get(&(2, new)) == Some(&new) && g.table.get(&(1, new)) == Some(&new) && !g.table.contains_key(&(2, old))).then_some(())
    })
    .await;
    assert_eq!(gw.lock().unwrap().table.len(), 2, "{:?}", gw.lock().unwrap().table);
}

/// A router that grants another external port than the one asked for: the trackers must be told
/// the external one (the listen port is useless to peers on the Internet), including for the
/// torrents loaded at start-up, which are registered before the mapping is known.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trackers_are_told_the_mapped_external_port() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(200_000, Some(tracker.url()), false, 91);
    let store = MemStore::new();
    let dir = tempfile::tempdir().unwrap();
    t.write_to(dir.path());
    // First run: no port mapping; the torrent is added, verified and remembered.
    let mut first = node_in(dir, store.clone(), |c| c.trackers_enabled = true).await;
    first.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&first, &t.id, 30).await;
    let listen = first.h.listen_port();
    wait_for("first started", 10, || async {
        tracker.log_for(&t.id).iter().any(|a| a.event.as_deref() == Some("started") && a.port == listen).then_some(())
    })
    .await;
    first.shutdown(Duration::from_secs(5)).await;
    let first_dir = std::mem::replace(&mut first.dir, tempfile::tempdir().unwrap());

    // Second run on the same state: the router maps the listen port to port + 1000.
    let (gw_addr, _gw) = gateway_shifted(1000).await;
    let seen = tracker.log_for(&t.id).len();
    let second = node_in(first_dir, store.clone(), |c| {
        c.trackers_enabled = true;
        c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw_addr) };
    })
    .await;
    let port = second.h.listen_port();
    wait_for("a started announce with the external port", 30, || async {
        let log = tracker.log_for(&t.id);
        log[seen..].iter().any(|a| a.event.as_deref() == Some("started")).then_some(())
    })
    .await;
    let log = tracker.log_for(&t.id);
    let started = log[seen..].iter().find(|a| a.event.as_deref() == Some("started")).unwrap();
    assert_eq!(started.port, port.wrapping_add(1000), "announced port {} vs listen {port}", started.port);
}

/// `reachability` shows what the router answered, forgets it when mapping is switched off, and
/// counts no incoming connection from loopback (only Internet peers prove the port is open).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reachability_reports_the_mapping_and_clears_it_when_switched_off() {
    let (gw_addr, _gw) = gateway().await;
    let n = node(|c| {
        c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw_addr) };
    })
    .await;
    let port = n.h.listen_port();
    let r = wait_for("mapped", 20, || async {
        let r = n.h.reachability();
        r.portmap.as_ref().is_some_and(|s| s.external_tcp_port.is_some()).then_some(r)
    })
    .await;
    assert!(r.mapping_enabled);
    assert_eq!(r.port, port);
    let s = r.portmap.unwrap();
    assert_eq!((s.method, s.external_tcp_port, s.external_ip), (Some("nat-pmp"), Some(port), Some("203.0.113.9".parse().unwrap())));
    assert_eq!((r.incoming, r.last_incoming_at), (0, None));

    n.h.update_config(move |c| c.portmap = PortmapConfig { enabled: false, upnp: false, natpmp: false, test_gateway: Some(gw_addr) });
    wait_for("status cleared", 10, || async {
        let r = n.h.reachability();
        (!r.mapping_enabled && r.portmap.is_none()).then_some(())
    })
    .await;
    // The stopped mapper's last word (its mappings deleted) must not bring a status back.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(n.h.reachability().portmap, None);
}

/// Folder sync's port (SPEC 7G) is mapped, TCP and UDP, by the same mapper as the listen port. Setting the
/// same port again sends nothing; clearing it deletes its mapping and keeps the listen port's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_extra_port_is_mapped_next_to_the_listen_port() {
    let (gw_addr, gw) = gateway().await;
    let n = node(|c| {
        c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw_addr) };
    })
    .await;
    let port = n.h.listen_port();
    wait_for("listen port mapped", 20, || async { (gw.lock().unwrap().table.len() == 2).then_some(()) }).await;
    let extra = free_port();
    n.h.set_extra_mapped_port(Some(extra));
    let s = wait_for("extra port mapped", 20, || async {
        let s = n.h.reachability().portmap?;
        let all = [s.extra_external_tcp_port, s.extra_external_udp_port, s.external_tcp_port, s.external_udp_port];
        all.iter().all(Option::is_some).then_some(s)
    })
    .await;
    assert_eq!((s.external_tcp_port, s.external_udp_port), (Some(port), Some(port)));
    assert_eq!((s.extra_external_tcp_port, s.extra_external_udp_port), (Some(extra), Some(extra)));
    {
        let g = gw.lock().unwrap();
        assert_eq!(g.table.get(&(2, extra)), Some(&extra), "TCP for the extra port");
        assert_eq!(g.table.get(&(1, extra)), Some(&extra), "UDP for the extra port");
        assert_eq!(g.table.get(&(2, port)), Some(&port), "the listen port's TCP survives the restart");
        assert_eq!(g.table.get(&(1, port)), Some(&port), "and its UDP");
        assert_eq!(g.table.len(), 4);
    }
    let before = gw.lock().unwrap().requests;
    n.h.set_extra_mapped_port(Some(extra));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(gw.lock().unwrap().requests, before, "the same port again asks the router nothing");

    n.h.set_extra_mapped_port(None);
    wait_for("extra port unmapped", 20, || async {
        let g = gw.lock().unwrap();
        (g.table.len() == 2 && g.table.contains_key(&(2, port)) && g.table.contains_key(&(1, port))).then_some(())
    })
    .await;
    wait_for("status without the extra port", 20, || async {
        let s = n.h.reachability().portmap?;
        (s.external_tcp_port.is_some() && s.extra_external_tcp_port.is_none() && s.extra_external_udp_port.is_none()).then_some(())
    })
    .await;
}

/// With mapping off the extra port is only remembered; switching mapping on maps it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_extra_port_waits_for_mapping_to_be_switched_on() {
    let (gw_addr, gw) = gateway().await;
    let n = node(|c| {
        c.portmap = PortmapConfig { enabled: false, upnp: false, natpmp: true, test_gateway: Some(gw_addr) };
    })
    .await;
    let extra = free_port();
    n.h.set_extra_mapped_port(Some(extra));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(gw.lock().unwrap().requests, 0);
    n.h.update_config(move |c| c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw_addr) });
    wait_for("all four mapped", 20, || async {
        let g = gw.lock().unwrap();
        (g.table.get(&(2, extra)) == Some(&extra) && g.table.get(&(1, extra)) == Some(&extra) && g.table.len() == 4).then_some(())
    })
    .await;
}
