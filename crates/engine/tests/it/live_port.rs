//! The listening port changes live: no restart, TCP peers stay, the old port closes, trackers
//! hear the new port with their next regular announce (no extra announce), a taken port keeps the
//! old one, and automatic (0) is a fresh random port.
use crate::common;
use std::net::SocketAddr;
use std::time::Duration;

use common::*;
use trs_engine::*;

fn at(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

async fn move_to(n: &Node, wanted: u16) -> u16 {
    let before = n.h.listen_port();
    n.h.update_config(move |c| c.listen_port = wanted);
    wait_for("the port changed", 15, || async {
        let now = n.h.listen_port();
        (now != before && (wanted == 0 || now == wanted)).then_some(now)
    })
    .await
}

/// Moves `n` to a manual port that is free: [`free_port`] checks TCP only and another test may take the port
/// meanwhile, and a taken port (TCP or UDP) leaves the engine where it was with a warning, so then the next one.
async fn move_to_free(n: &Node) -> u16 {
    for _ in 0..5 {
        let p = free_port();
        let before = n.h.listen_port();
        n.h.update_config(move |c| c.listen_port = p);
        let moved = wait_for("the port changed or was refused", 15, || async {
            if n.h.listen_port() == p && p != before {
                Some(true)
            } else {
                n.h.port_fallback().filter(|f| f.wanted == p).map(|_| false)
            }
        })
        .await;
        if moved {
            return p;
        }
    }
    panic!("no free port after five tries");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_port_change_moves_tcp_and_utp_closes_the_old_port_and_waits_for_the_next_announce() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(3_000_000, Some(tracker.url()), false, 8801);
    let seeder = node(|c| {
        c.trackers_enabled = true;
        c.up_limit = Some(1_000_000);
    })
    .await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let old = seeder.h.listen_port();
    wait_for("the first announce", 20, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    assert!(tracker.log_for(&t.id).iter().all(|a| a.port == old));

    // A leecher is mid-download over TCP when the port moves: its connection stays.
    let stay = node(|_| {}).await;
    stay.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![at(old)], ..Default::default() }).await.unwrap();
    wait_for("some data", 30, || async { (stay.row(&t.id).await?.done_bytes > 100_000).then_some(()) }).await;

    let announces_before = tracker.log_for(&t.id).len();
    let new = move_to_free(&seeder).await;
    assert!(seeder.h.port_fallback().is_none());

    // Right after the change no announce was made because of it.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let log = tracker.log_for(&t.id);
    assert_eq!(log.len(), announces_before, "no extra announce for a port change: {:?}", &log[announces_before..]);

    // The connection of the running download survived and finishes.
    wait_done(&stay, &t.id, 60).await;
    t.verify(stay.dir.path(), &[]);

    // New peers connect on the new port over TCP and over uTP.
    let tcp = node(|_| {}).await;
    tcp.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![at(new)], ..Default::default() }).await.unwrap();
    let utp = node(|c| c.outgoing_tcp = false).await;
    utp.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![at(new)], ..Default::default() }).await.unwrap();
    wait_done(&tcp, &t.id, 60).await;
    wait_done(&utp, &t.id, 60).await;
    assert!(seeder.h.diagnostics().utp_accepted >= 1, "uTP reached the new socket");
    t.verify(utp.dir.path(), &[]);

    // The old port is closed: TCP is refused, uTP never answers.
    wait_for("old TCP port closed", 5, || async {
        tokio::net::TcpStream::connect(at(old)).await.is_err().then_some(())
    })
    .await;
    let probe = trs_utp::UtpSocket::bind("127.0.0.1:0".parse().unwrap(), trs_utp::Config::default()).await.unwrap();
    let r = tokio::time::timeout(Duration::from_secs(4), probe.connect(at(old))).await;
    assert!(!matches!(r, Ok(Ok(_))), "nothing accepts uTP on the old port");

    // Whatever the tracker hears next carries the port in force when it is sent (the regular
    // announces are 60 s apart; a `stopped` is the first one a test can trigger).
    assert_eq!(tracker.log_for(&t.id).len(), announces_before, "still no extra announce");
    seeder.h.pause(&[t.id]);
    let stopped = wait_for("stopped", 20, || async {
        tracker.log_for(&t.id).into_iter().skip(announces_before).find(|a| a.event.as_deref() == Some("stopped"))
    })
    .await;
    assert_eq!(stopped.port, new);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_taken_port_keeps_the_old_one_and_says_so() {
    let n = node(|_| {}).await;
    let old = n.h.listen_port();
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();
    n.h.update_config(move |c| c.listen_port = taken);
    let f = wait_for("the fallback", 15, || async { n.h.port_fallback() }).await;
    assert_eq!((f.wanted, f.using), (taken, old));
    assert_eq!(f.message, format!("Port {taken} is in use by another app; using {old} for now"));
    assert_eq!(n.h.listen_port(), old, "the old port keeps running");
    assert!(tokio::net::TcpStream::connect(at(old)).await.is_ok());
    assert_eq!(n.h.snapshot().await.unwrap().port_fallback, Some(f));

    // A UDP-only squatter is a taken port as well.
    let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let udp_port = udp.local_addr().unwrap().port();
    n.h.update_config(move |c| c.listen_port = udp_port);
    wait_for("the second fallback", 15, || async { n.h.port_fallback().filter(|f| f.wanted == udp_port) }).await;
    assert_eq!(n.h.listen_port(), old);

    // Free again: the port moves and the warning goes away.
    drop(squatter);
    move_to_free(&n).await;
    assert!(n.h.port_fallback().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn automatic_is_a_random_port_at_every_start_and_at_every_live_change() {
    let a = node(|_| {}).await;
    let b = node(|_| {}).await;
    assert!(a.h.listen_port() >= 1024 && b.h.listen_port() >= 1024);
    assert_ne!(a.h.listen_port(), b.h.listen_port());
    assert_eq!(a.h.config().await.unwrap().listen_port, 0, "automatic stays automatic in the configuration");
    assert!(a.h.port_fallback().is_none());
    let manual = move_to_free(&a).await;
    let random = move_to(&a, 0).await;
    assert_ne!(random, manual);
    assert!(a.h.port_fallback().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dht_follows_the_port() {
    let dht = |bootstrap: String| {
        move |c: &mut EngineConfig| {
            c.dht_enabled = true;
            c.dht_allow_local = true;
            c.dht_bind = "127.0.0.1:0".parse().unwrap();
            c.dht_bootstrap = vec![bootstrap.clone()];
            c.pex_enabled = false;
        }
    };
    let router = node(dht("127.0.0.1:9".into())).await;
    let router_addr = format!("127.0.0.1:{}", router.h.listen_port());
    let t = standard_torrent(1_000_000, None, false, 8802);
    let seeder = node(dht(router_addr.clone())).await;
    move_to_free(&seeder).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    wait_for("the seeder announced", 60, || async { (seeder.h.diagnostics().dht_announces >= 1).then_some(()) }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let leech = node(dht(router_addr)).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&leech, &t.id, 120).await;
    t.verify(leech.dir.path(), &[]);
}

/// `public_dht` lends the DHT node to another service (folder sync, SPEC 7G): `None` with the DHT off, and
/// with it on an announce on a public topic from one node is found by a lookup from another, also after
/// the node moved to a new port.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dht_is_lent_for_public_topics() {
    let off = node(|c| c.dht_enabled = false).await;
    assert!(off.h.public_dht().await.is_none(), "no DHT, nothing to lend");
    let dht = |bootstrap: String| {
        move |c: &mut EngineConfig| {
            c.dht_enabled = true;
            c.dht_allow_local = true;
            c.dht_bind = "127.0.0.1:0".parse().unwrap();
            c.dht_bootstrap = vec![bootstrap.clone()];
            c.pex_enabled = false;
        }
    };
    let router = node(dht("127.0.0.1:9".into())).await;
    let router_addr = format!("127.0.0.1:{}", router.h.listen_port());
    let a = node(dht(router_addr.clone())).await;
    let b = node(dht(router_addr)).await;
    move_to_free(&a).await;
    let topic = [0x5a; 20];
    let topic = trs_dht::PublicInfoHash::new(topic, false).unwrap();
    let found = wait_for("the announced port is found", 60, || async {
        let dht_a = a.h.public_dht().await?;
        if let Ok(Ok(mut s)) = dht_a.announce(topic, 4242).await {
            while s.next().await.is_some() {}
        }
        let dht_b = b.h.public_dht().await?;
        let Ok(Ok(mut s)) = dht_b.get_peers(topic).await else { return None };
        let mut peers = Vec::new();
        while let Some(u) = s.next().await {
            if let trs_dht::LookupUpdate::Peers(v) = u {
                peers.extend(v);
            }
        }
        peers.into_iter().find(|p| p.port() == 4242)
    })
    .await;
    assert_eq!(found.ip(), &std::net::Ipv4Addr::LOCALHOST);
}
