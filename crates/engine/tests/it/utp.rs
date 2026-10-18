//! uTP (BEP 29) in the session: one UDP socket on the listen port shared with the DHT, incoming
//! uTP accepted, outgoing uTP for peers that are known to support it, TCP otherwise.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;

async fn seeded_node(t: &TestTorrent, cfg: impl FnOnce(&mut EngineConfig)) -> Node {
    let n = node(cfg).await;
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&n, &t.id, 30).await;
    n
}

async fn peers_of(n: &Node, id: TorrentId) -> Vec<PeerInfo> {
    n.h.torrent_detail(id).await.map(|d| d.peers).unwrap_or_default()
}

/// A swarm that talks uTP and nothing else: two leechers dial the seeder over uTP only, every
/// byte arrives intact, and they trade pieces with each other over uTP as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn swarm_over_utp_only() {
    let t = standard_torrent(6_000_000, None, false, 201);
    let seeder = seeded_node(&t, |c| c.up_limit = Some(4_000_000)).await;
    let utp_only = |c: &mut EngineConfig| c.outgoing_tcp = false;
    let a = node(utp_only).await;
    let b = node(utp_only).await;
    for n in [&a, &b] {
        n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    }
    wait_done(&a, &t.id, 120).await;
    wait_done(&b, &t.id, 120).await;
    t.verify(a.dir.path(), &[]);
    t.verify(b.dir.path(), &[]);
    let (da, db, ds) = (a.h.diagnostics(), b.h.diagnostics(), seeder.h.diagnostics());
    // They have no TCP dial at all: whatever they connected to, they connected to over uTP.
    assert!(da.utp_dialed >= 1 && db.utp_dialed >= 1, "{da:?} {db:?}");
    assert!(ds.utp_accepted >= 2, "the seeder accepted both over uTP: {ds:?}");
    let row = seeder.row(&t.id).await.unwrap();
    assert!(row.uploaded_total >= t.total(), "uploaded {}", row.uploaded_total);
}

/// TCP and uTP side by side in one swarm: a default node dials TCP, a uTP-only node dials uTP,
/// and the seeder serves both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_tcp_and_utp_peers_share_a_swarm() {
    let t = standard_torrent(4_000_000, None, false, 202);
    let seeder = seeded_node(&t, |c| c.up_limit = Some(1_500_000)).await;
    let tcp = node(|_| {}).await;
    let utp = node(|c| c.outgoing_tcp = false).await;
    for n in [&tcp, &utp] {
        n.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    }
    // Both transports show up on the seeder (not necessarily at the same moment: under load one
    // leecher can be done before the other has connected, so remember what was seen).
    let seen = std::cell::Cell::new((false, false));
    wait_for("both transports on the seeder", 60, || async {
        let p = peers_of(&seeder, t.id).await;
        let (u, c) = seen.get();
        seen.set((u || p.iter().any(|x| x.utp), c || p.iter().any(|x| !x.utp)));
        let (u, c) = seen.get();
        (u && c).then_some(())
    })
    .await;
    wait_done(&tcp, &t.id, 120).await;
    wait_done(&utp, &t.id, 120).await;
    t.verify(tcp.dir.path(), &[]);
    t.verify(utp.dir.path(), &[]);
    // (The default node may well dial the uTP node over uTP later: the seeder's `ut_pex` flags it.)
    assert!(utp.h.diagnostics().utp_dialed >= 1);
    assert!(seeder.h.diagnostics().incoming_accepted >= 2);
}

/// A peer that connected to us over uTP is dialled over uTP next time (it is known to speak it,
/// for every torrent), while a peer we know nothing about is dialled over TCP.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_that_spoke_utp_are_dialled_over_utp() {
    let t1 = standard_named("first", 3_000_000, None, false, 203);
    let t2 = standard_named("second", 3_000_000, None, false, 213);
    let seeder = seeded_node(&t1, |_| {}).await;
    t2.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t2.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t2.id, 30).await;

    // L reaches the seeder over uTP for the first torrent, which tells the seeder that L speaks it.
    let l = node(|c| {
        c.outgoing_tcp = false;
        c.down_limit = Some(50_000);
    })
    .await;
    l.h.add_torrent_file(t1.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("L on the seeder over uTP", 30, || async {
        let p = peers_of(&seeder, t1.id).await;
        p.iter().any(|x| x.utp && !x.outgoing).then_some(())
    })
    .await;
    // M is a stranger: default node, no history with the seeder.
    let m = node(|_| {}).await;
    for n in [&l, &m] {
        n.h.add_torrent_file(t2.bytes.clone(), AddOptions::default()).await.unwrap();
    }
    // A torrent that is still being checked refuses connections, so let both get past that.
    for n in [&l, &m] {
        wait_for("the second torrent is checked", 30, || async {
            (n.state(&t2.id).await? == TorrentState::Downloading).then_some(())
        })
        .await;
    }
    // The seeder is told about both for the second torrent and dials them.
    seeder.h.add_peers(t2.id, vec![l.addr(), m.addr()]);
    wait_for("the seeder dialled L over uTP", 30, || async { (l.h.diagnostics().utp_accepted >= 1).then_some(()) }).await;
    wait_for("the seeder dialled M", 30, || async { (m.h.diagnostics().incoming_accepted >= 1).then_some(()) }).await;
    assert_eq!(m.h.diagnostics().utp_accepted, 0, "a stranger is dialled over TCP");
}

/// The DHT runs on the uTP socket (one UDP port): a node that announced on it is found by a
/// second node through a third, and the torrent downloads over the peer the DHT returned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_dht_works_on_the_shared_port() {
    let dht = |bootstrap: String| {
        move |c: &mut EngineConfig| {
            c.dht_enabled = true;
            c.dht_allow_local = true;
            c.dht_bind = "127.0.0.1:0".parse().unwrap();
            c.dht_bootstrap = vec![bootstrap.clone()];
            c.pex_enabled = false;
        }
    };
    // The "router": a node without torrents that everybody bootstraps from.
    let router = node(dht("127.0.0.1:9".into())).await;
    let router_addr = format!("127.0.0.1:{}", router.h.listen_port());
    let t = standard_torrent(1_500_000, None, false, 204);
    let seeder = node(dht(router_addr.clone())).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    wait_for("the seeder announced", 60, || async { (seeder.h.diagnostics().dht_announces >= 1).then_some(()) }).await;
    // Give the announce a moment to reach the router, then ask.
    tokio::time::sleep(Duration::from_secs(2)).await;
    let leech = node(dht(router_addr)).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&leech, &t.id, 120).await;
    t.verify(leech.dir.path(), &[]);
    assert!(leech.h.diagnostics().dht_lookups >= 1);
}

/// `network.utp` off: nothing listens on UDP for peers and every connection is TCP.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn utp_can_be_switched_off() {
    let t = standard_torrent(1_000_000, None, false, 205);
    let seeder = seeded_node(&t, |c| c.utp = false).await;
    let leech = node(|c| {
        c.utp = false;
        c.outgoing_tcp = true;
    })
    .await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    assert_eq!(seeder.h.diagnostics().utp_accepted + leech.h.diagnostics().utp_dialed, 0);
}
