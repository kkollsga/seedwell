//! A seed finds and reaches the leechers its tracker names as libtorrent does (SPEC §6, §7B.3): it asks for
//! 200 peers, dials the new addresses of a response at once (a boost, a bounded number at a time), does not
//! dial known seeds again after a demotion, and `debug peers` shows what each peer asked of it.
use crate::common;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use seedwell_engine::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// The most dials a seeding torrent has connecting at once (`torrent::events::SEED_MAX_CONNECTING`).
const MAX_CONNECTING: usize = 8;
/// The dials a seeding torrent makes at once for new addresses (`torrent::events::SEED_CONNECT_BOOST`).
const BOOST: usize = 24;

/// Loopback hosts that accept a connection, say nothing, and close it after `hold` (a leecher behind a
/// firewall that lets the SYN through, or a client that drops us). Counts what reached them.
struct Hosts {
    addrs: Vec<SocketAddr>,
    accepted: Arc<AtomicUsize>,
    max_open: Arc<AtomicUsize>,
}

impl Hosts {
    async fn start(n: usize, hold: Duration) -> Hosts {
        let accepted = Arc::new(AtomicUsize::new(0));
        let open = Arc::new(AtomicUsize::new(0));
        let max_open = Arc::new(AtomicUsize::new(0));
        let mut addrs = Vec::new();
        for _ in 0..n {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(l.local_addr().unwrap());
            let (accepted, open, max_open) = (accepted.clone(), open.clone(), max_open.clone());
            tokio::spawn(async move {
                while let Ok((s, _)) = l.accept().await {
                    accepted.fetch_add(1, Ordering::SeqCst);
                    let now = open.fetch_add(1, Ordering::SeqCst) + 1;
                    max_open.fetch_max(now, Ordering::SeqCst);
                    let open = open.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(hold).await;
                        open.fetch_sub(1, Ordering::SeqCst);
                        drop(s);
                    });
                }
            });
        }
        Hosts { addrs, accepted, max_open }
    }

    fn ports(&self) -> Vec<u16> {
        self.addrs.iter().map(SocketAddr::port).collect()
    }

    fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

fn quick_tiers(c: &mut EngineConfig) {
    c.tiers = TierTimings { hot_to_warm: Duration::from_millis(100), warm_to_cold: Duration::from_millis(300), ..TierTimings::default() };
}

/// A seed that asks its tracker for 50 peers learns of a quarter of the leechers libtorrent (200) finds in
/// a big swarm; and one that asks for 200 must keep all 200 it is given (a cap below what we ask, in the
/// response parser or the peer pool, would throw the rest away). `stopped` still asks for none.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seed_asks_its_tracker_for_200_peers_and_keeps_every_one() {
    let tracker = MockTracker::start(1800).await;
    let t = standard_torrent(300_000, Some(tracker.url()), false, 801);
    // Two hundred seeds (nobody to dial: the response says no one is downloading), on ports nothing listens on.
    let ports: Vec<u16> = (0..200).map(|i| 20_000 + i).collect();
    tracker.add_peers(&t.id, &ports, true);
    let seeder = node(|c| c.trackers_enabled = true).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    let first = wait_for("the first announce", 15, || async { tracker.log_for(&t.id).first().cloned() }).await;
    assert_eq!(first.event.as_deref(), Some("started"));
    assert_eq!(first.numwant, Some(200), "{}", first.raw_query);
    wait_for("all 200 peers known", 15, || async { (seeder.row(&t.id).await?.known_peers >= 200).then_some(()) }).await;

    seeder.h.pause(&[t.id]);
    let stopped = wait_for("stopped", 15, || async {
        tracker.log_for(&t.id).into_iter().find(|a| a.event.as_deref() == Some("stopped"))
    })
    .await;
    assert_eq!(stopped.numwant, Some(0), "{}", stopped.raw_query);
}

/// A seed dialled the leechers a tracker named at one attempt per 30 s after a burst of 4, two at a time:
/// fifty addresses took 23 minutes, and a leecher that cannot be dialled back was gone long before. The new
/// addresses of a response now get a boost of dials at once, never more than `MAX_CONNECTING` at a time,
/// and the rest follow at a steady pace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_seed_dials_the_leechers_its_tracker_names_at_once() {
    let hosts = Hosts::start(30, Duration::from_millis(500)).await;
    let tracker = MockTracker::start(1800).await;
    let t = standard_torrent(300_000, Some(tracker.url()), false, 802);
    tracker.add_peers(&t.id, &hosts.ports(), false);
    let seeder = node(|c| c.trackers_enabled = true).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let announced = wait_for("the first announce", 15, || async { tracker.log_for(&t.id).first().map(|a| a.at) }).await;
    wait_for("the boost dialled", 20, || async { (hosts.accepted() >= BOOST).then_some(()) }).await;
    let took = announced.elapsed();
    eprintln!("{BOOST} leechers dialled {took:?} after the announce; at most {} at once", hosts.max_open.load(Ordering::SeqCst));
    // Each host holds a dial for 0.5 s: 24 of them one or two at a time would take 6 s or more.
    assert!(took < Duration::from_secs(5), "the boost took {took:?}");
    assert!(hosts.max_open.load(Ordering::SeqCst) <= MAX_CONNECTING, "more dials at once than the cap");
    wait_for("the rest at the steady pace", 30, || async { (hosts.accepted() >= hosts.addrs.len()).then_some(()) }).await;
    assert!(hosts.max_open.load(Ordering::SeqCst) <= MAX_CONNECTING, "more dials at once than the cap");
    let totals = seeder.h.totals().await.unwrap();
    assert_eq!(totals.connections, 0, "a dial that never connected counts as a connection: {totals:?}");
}

/// A Cold seed has no actor: a tracker response that names leechers wakes it with those addresses. It got
/// them as cached peers and dialled four, then one per 30 s; it now dials them as a live seed does, at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cold_seed_woken_by_its_tracker_dials_the_leechers_at_once() {
    let hosts = Hosts::start(30, Duration::from_millis(500)).await;
    let tracker = MockTracker::start(1800).await;
    let t = standard_torrent(300_000, Some(tracker.url()), false, 803);
    let seeder = node(|c| {
        c.trackers_enabled = true;
        quick_tiers(c);
    })
    .await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    wait_for("the first announce", 15, || async { tracker.log_for(&t.id).first().map(|a| a.at) }).await;
    wait_for("the torrent went Cold", 30, || async { (seeder.row(&t.id).await?.tier == Tier::Cold).then_some(()) }).await;
    assert_eq!(hosts.accepted(), 0);

    tracker.add_peers(&t.id, &hosts.ports(), false);
    seeder.h.reannounce_with(&[t.id], true).await.unwrap();
    let woken = wait_for("the second announce", 30, || async { tracker.log_for(&t.id).get(1).map(|a| a.at) }).await;
    wait_for("the boost dialled", 20, || async { (hosts.accepted() >= BOOST).then_some(()) }).await;
    let took = woken.elapsed();
    eprintln!("{BOOST} leechers dialled {took:?} after the announce that woke the torrent");
    assert!(took < Duration::from_secs(5), "the boost took {took:?}");
    assert!(hosts.max_open.load(Ordering::SeqCst) <= MAX_CONNECTING, "more dials at once than the cap");
}

/// The seeds a torrent knew were forgotten with its actor at every demotion: each wake dialled them again
/// (the peer cache holds the download's seeds), only to find them redundant. The Cold record keeps them,
/// and a seed that is woken with their addresses dials only the others.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn known_seeds_are_not_dialled_again_after_a_demotion() {
    let t = standard_torrent(3_000_000, None, false, 804);
    let seed = node(|_| {}).await;
    t.write_to(seed.dir.path());
    seed.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seed, &t.id, 30).await;

    let me = node(quick_tiers).await;
    me.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seed.addr()], ..Default::default() }).await.unwrap();
    wait_done(&me, &t.id, 60).await;
    wait_for("the torrent went Cold", 30, || async { (me.row(&t.id).await?.tier == Tier::Cold).then_some(()) }).await;
    assert_eq!(seed.h.diagnostics().incoming_accepted, 1, "the download's one connection");

    let leecher = Hosts::start(1, Duration::from_millis(200)).await;
    me.h.add_peers(t.id, vec![seed.addr(), leecher.addrs[0]]);
    wait_for("the woken torrent dialled the other address", 15, || async { (leecher.accepted() >= 1).then_some(()) }).await;
    // A dial on loopback lands in milliseconds.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(seed.h.diagnostics().incoming_accepted, 1, "the known seed was dialled again after the wake");
}

async fn read_msg(s: &mut TcpStream, buf: &mut Vec<u8>) -> Option<u8> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await.ok()?;
    let n = u32::from_be_bytes(len) as usize;
    buf.resize(n, 0);
    s.read_exact(&mut buf[..n]).await.ok()?;
    Some(if n == 0 { u8::MAX } else { buf[0] })
}

fn request(piece: u32, begin: u32, len: u32) -> Vec<u8> {
    let mut m = vec![0, 0, 0, 13, 6];
    m.extend(piece.to_be_bytes());
    m.extend(begin.to_be_bytes());
    m.extend(len.to_be_bytes());
    m
}

/// "A leecher interested and unchoked, and nothing sent for 16 hours" could not be settled from our side:
/// `debug peers` showed only our own requests to the peer. It now counts what the peer asked of us, what
/// we served, what we declined, and when it last asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn debug_peers_counts_the_requests_a_peer_sent_and_what_became_of_them() {
    const BLOCK: u32 = 16 * 1024;
    let t = standard_torrent(300_000, None, false, 805);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    // A plain peer (no Fast extension, so no allowed-fast pieces): a request while choked is dropped.
    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    hs.extend([0u8; 8]);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-askaskaskask");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    let peer = || async {
        let d = seeder.h.torrent_detail(t.id).await.ok()?;
        d.peers.into_iter().next()
    };
    let p = wait_for("connected", 10, peer).await;
    assert_eq!((p.requests_received, p.blocks_served, p.requests_rejected, p.last_request_secs), (0, 0, 0, None));

    s.write_all(&request(0, 0, BLOCK)).await.unwrap();
    let p = wait_for("the choked request counted", 10, || async { peer().await.filter(|p| p.requests_received == 1) }).await;
    assert_eq!((p.blocks_served, p.requests_rejected), (0, 1), "a request while choked is declined");

    s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async { while read_msg(&mut s, &mut buf).await.expect("closed") != 1 {} })
        .await
        .expect("not unchoked");
    // Pieces are 32 KiB: two blocks of piece 0 and the first of piece 1.
    let mut reqs = Vec::new();
    for (piece, begin) in [(0, 0), (0, BLOCK), (1, 0)] {
        reqs.extend(request(piece, begin, BLOCK));
    }
    s.write_all(&reqs).await.unwrap();
    let mut pieces = 0;
    tokio::time::timeout(Duration::from_secs(10), async {
        while pieces < 3 {
            if read_msg(&mut s, &mut buf).await.expect("closed") == 7 {
                pieces += 1;
            }
        }
    })
    .await
    .expect("the blocks did not come");
    let p = peer().await.unwrap();
    assert_eq!((p.requests_received, p.blocks_served, p.requests_rejected), (4, 3, 1), "{p:?}");
    assert!(p.last_request_secs.is_some_and(|s| s < 10), "{p:?}");
    assert!(p.interested && !p.choked_by_us, "{p:?}");
}

/// With the Fast extension a choke no longer cancels requests: the ones we will not serve are rejected,
/// and a choked peer may still have its allowed-fast pieces (BEP 6). Requests for allowed-fast pieces still
/// queued when the peer was choked were dropped without a word: the peer waited for blocks that never
/// came, until its request timeout. They are served now.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn allowed_fast_requests_queued_at_a_choke_are_still_served() {
    const BLOCK: u32 = 16 * 1024;
    let t = standard_torrent(300_000, None, false, 806);
    let seeder = node(|c| {
        c.rechoke_interval = Duration::from_millis(200);
        // 16 KiB/s: the eight requests below are still queued when the choke comes.
        c.up_limit = Some(16 * 1024);
    })
    .await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let mut s = TcpStream::connect(seeder.addr()).await.unwrap();
    let mut hs = vec![19u8];
    hs.extend(b"BitTorrent protocol");
    let mut reserved = [0u8; 8];
    reserved[7] |= 0x04; // Fast
    hs.extend(reserved);
    hs.extend(t.id);
    hs.extend(*b"-TR4130-fastfastfast");
    s.write_all(&hs).await.unwrap();
    let mut theirs = [0u8; 68];
    s.read_exact(&mut theirs).await.unwrap();
    let mut buf = Vec::new();
    let mut fast = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while fast.len() < 4 {
            if read_msg(&mut s, &mut buf).await.expect("closed") == 17 {
                fast.push(u32::from_be_bytes(buf[1..5].try_into().unwrap()));
            }
        }
    })
    .await
    .expect("no allowed-fast set");

    s.write_all(&[0, 0, 0, 1, 2]).await.unwrap(); // interested
    tokio::time::timeout(Duration::from_secs(20), async { while read_msg(&mut s, &mut buf).await.expect("closed") != 1 {} })
        .await
        .expect("not unchoked");
    let mut reqs = Vec::new();
    for p in &fast {
        reqs.extend(request(*p, 0, BLOCK));
        reqs.extend(request(*p, BLOCK, BLOCK));
    }
    reqs.extend([0, 0, 0, 1, 3]); // not interested: the next rechoke chokes us
    s.write_all(&reqs).await.unwrap();
    let (mut pieces, mut rejects, mut choked) = (0, 0, false);
    let _ = tokio::time::timeout(Duration::from_secs(20), async {
        while pieces + rejects < 8 {
            match read_msg(&mut s, &mut buf).await.expect("closed") {
                0 => choked = true,
                7 => pieces += 1,
                16 => rejects += 1,
                _ => {}
            }
        }
    })
    .await;
    assert!(choked, "the peer was never choked: nothing tested");
    assert_eq!((pieces, rejects), (8, 0), "requests for allowed-fast pieces went unanswered after the choke");
}
