//! Redundant connections (SPEC 6): a torrent that wants nothing more closes the connections of
//! peers that can never want anything from it, and does not dial them again meanwhile.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;

const S01: [FilePriority; 4] = [FilePriority::Normal, FilePriority::Normal, FilePriority::Skip, FilePriority::Normal];

async fn seeder_of(t: &TestTorrent) -> Node {
    let s = node(|_| {}).await;
    t.write_to(s.dir.path());
    s.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&s, &t.id, 30).await;
    s
}

async fn no_peers_for(n: &Node, id: &TorrentId, ms: u64) {
    let end = tokio::time::Instant::now() + Duration::from_millis(ms);
    while tokio::time::Instant::now() < end {
        let r = n.row(id).await.unwrap();
        assert_eq!(r.peers, 0, "connection came back: {r:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A complete torrent that just finished downloading from a complete seed: both let go.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_complete_seeds_disconnect() {
    let t = standard_torrent(3_000_000, None, false, 501);
    let seeder = seeder_of(&t).await;
    let b = node(|_| {}).await;
    b.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&b, &t.id, 60).await;
    wait_for("both sides dropped", 20, || async {
        let (x, y) = (b.row(&t.id).await?, seeder.row(&t.id).await?);
        (x.peers == 0 && y.peers == 0).then_some(())
    })
    .await;
    no_peers_for(&b, &t.id, 1500).await;
}

/// A partial seed (every wanted piece held, some files skipped) drops a full seed too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_seed_disconnects_full_seed() {
    let t = standard_torrent(3_000_000, None, false, 502);
    let seeder = seeder_of(&t).await;
    let p = node(|_| {}).await;
    p.h.add_torrent_file(
        t.bytes.clone(),
        AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), ..Default::default() },
    )
    .await
    .unwrap();
    wait_done(&p, &t.id, 60).await;
    wait_for("partial seed dropped the seed", 20, || async {
        (p.row(&t.id).await?.peers == 0 && seeder.row(&t.id).await?.peers == 0).then_some(())
    })
    .await;
    no_peers_for(&p, &t.id, 1500).await;
}

/// A downloading torrent keeps its seeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn downloading_torrent_keeps_seeds() {
    let t = standard_torrent(6_000_000, None, false, 503);
    let seeder = seeder_of(&t).await;
    let l = node(|c| c.down_limit = Some(300_000)).await;
    l.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_for("connected", 20, || async { (l.row(&t.id).await?.peers == 1).then_some(()) }).await;
    for _ in 0..25 {
        let r = l.row(&t.id).await.unwrap();
        assert_eq!(r.peers, 1, "seed dropped while downloading: {r:?}");
        assert_eq!(r.state, TorrentState::Downloading);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// After the partial seed drops the full seed, ticking more files dials that address again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ticking_more_files_dials_the_seed_again() {
    let t = standard_torrent(3_000_000, None, false, 504);
    let seeder = seeder_of(&t).await;
    let p = node(|_| {}).await;
    p.h.add_torrent_file(
        t.bytes.clone(),
        AddOptions { peers: vec![seeder.addr()], file_priorities: S01.to_vec(), ..Default::default() },
    )
    .await
    .unwrap();
    wait_done(&p, &t.id, 60).await;
    wait_for("seed dropped", 20, || async { (p.row(&t.id).await?.peers == 0).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(p.row(&t.id).await.unwrap().peers, 0, "the seed is not dialled while upload-only");

    p.h.set_file_priorities(t.id, vec![FilePriority::Normal; 4]).await.unwrap();
    wait_for("downloads from the remembered seed", 30, || async {
        let r = p.row(&t.id).await?;
        (r.done_bytes == t.total()).then_some(())
    })
    .await;
}

/// Open a connection that completes the handshake and then says nothing (an idle, silent peer).
async fn silent_peer(addr: std::net::SocketAddr, t: &TestTorrent, tag: u8) -> tokio::net::TcpStream {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    let mut pid = *b"-XX0001-silentsilent";
    pid[19] = tag;
    hs.extend(pid);
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    s
}

/// An upload-only torrent whose slots are taken by idle peers still admits a newcomer: the idle
/// ones are evicted instead of the leecher being refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upload_only_torrent_evicts_idle_peers_for_a_newcomer() {
    let t = standard_torrent(2_000_000, None, false, 505);
    let seeder = node(|c| c.max_connections_per_torrent = 2).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let _a = silent_peer(seeder.addr(), &t, b'a').await;
    let _b = silent_peer(seeder.addr(), &t, b'b').await;
    wait_for("both slots taken", 10, || async { (seeder.row(&t.id).await?.peers == 2).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(10_500)).await;

    let l = node(|_| {}).await;
    l.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&l, &t.id, 30).await;
    t.verify(l.dir.path(), &[]);
}

/// A peer that is still downloading (it keeps announcing HAVEs, from anyone) is not evicted for a
/// newcomer even though it asks nothing of us right now: it stays until it is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_still_downloading_is_not_evicted() {
    use tokio::io::AsyncWriteExt;
    let t = standard_torrent(2_000_000, None, false, 507);
    let seeder = node(|c| c.max_connections_per_torrent = 2).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let mut a = silent_peer(seeder.addr(), &t, b'a').await;
    let mut b = silent_peer(seeder.addr(), &t, b'b').await;
    wait_for("both slots taken", 10, || async { (seeder.row(&t.id).await?.peers == 2).then_some(()) }).await;
    tokio::time::sleep(Duration::from_millis(10_500)).await;
    // HAVE piece 0: each got a piece from someone just now.
    for s in [&mut a, &mut b] {
        s.write_all(&[0, 0, 0, 5, 4, 0, 0, 0, 0]).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refused = seeder.h.diagnostics().incoming_refused;
    // The newcomer may be closed before or after the handshake reply: don't insist on one.
    let mut c = tokio::net::TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(*b"-XX0001-silentsilenc");
    let _ = c.write_all(&hs).await;
    wait_for("newcomer refused", 10, || async { (seeder.h.diagnostics().incoming_refused > refused).then_some(()) }).await;
    assert_eq!(seeder.row(&t.id).await.unwrap().peers, 2, "a downloading peer was evicted");
}

/// A tracker that reports no leechers: the seeding torrent dials nobody (neither of the two
/// seeds ever connects to the other), and no connection stays open.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn seed_dials_nobody_when_the_tracker_reports_no_leechers() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(1_000_000, Some(tracker.url()), false, 506);
    let mut nodes = Vec::new();
    for _ in 0..2 {
        let n = node(|c| c.trackers_enabled = true).await;
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
        wait_done(&n, &t.id, 30).await;
        let want = nodes.len() + 1;
        wait_for("announced", 10, || async { (tracker.log_for(&t.id).len() >= want).then_some(()) }).await;
        nodes.push(n);
    }
    tokio::time::sleep(Duration::from_millis(3000)).await;
    for n in &nodes {
        let d = n.h.diagnostics();
        assert_eq!(d.incoming_accepted, 0, "a seed was dialled");
        assert_eq!(n.row(&t.id).await.unwrap().peers, 0);
    }
}
