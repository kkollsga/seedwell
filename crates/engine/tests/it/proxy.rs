//! Torrent traffic through a SOCKS5 proxy (SPEC §7H), against the loopback fake proxy of `trs-socks`: peers and
//! trackers go through it, nothing goes around it, the kill switch holds torrents while it is down, health is
//! measured.
use crate::common;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use common::*;
use trs_engine::*;
use trs_socks::testkit::FakeProxy;

fn via(fake: &FakeProxy) -> ProxyConfig {
    ProxyConfig { servers: vec![fake.addr().ip().to_string()], port: fake.addr().port(), auth: Some(proxy::Credentials::new("user", "pass").unwrap()), ..Default::default() }
}

/// A "peer" that only counts who connects to it over TCP, and keeps what reaches its UDP port of the same number
/// (uTP).
async fn decoy() -> (std::net::SocketAddr, Arc<AtomicUsize>, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let u = tokio::net::UdpSocket::bind(addr).await.unwrap();
    let datagrams = Arc::new(std::sync::Mutex::new(Vec::new()));
    let g = datagrams.clone();
    tokio::spawn(async move {
        let mut b = [0u8; 2048];
        while let Ok((n, _)) = u.recv_from(&mut b).await {
            g.lock().unwrap().push(b[..n].to_vec());
        }
    });
    let n = Arc::new(AtomicUsize::new(0));
    let c = n.clone();
    tokio::spawn(async move {
        while let Ok((s, _)) = l.accept().await {
            c.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(30)).await;
                drop(s);
            });
        }
    });
    (addr, n, datagrams)
}

/// A UDP socket that keeps what arrives.
async fn udp_watch() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
    let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = s.local_addr().unwrap();
    let got = Arc::new(std::sync::Mutex::new(Vec::new()));
    let g = got.clone();
    tokio::spawn(async move {
        let mut b = [0u8; 2048];
        while let Ok((n, _)) = s.recv_from(&mut b).await {
            g.lock().unwrap().push(b[..n].to_vec());
        }
    });
    (addr, got)
}

fn carrying(got: &std::sync::Mutex<Vec<Vec<u8>>>, ih: &[u8]) -> usize {
    got.lock().unwrap().iter().filter(|d| d.windows(ih.len()).any(|w| w == ih)).count()
}

async fn wait_health(h: &EngineHandle, want: ProxyHealth, secs: u64) -> ProxyStatus {
    wait_for(&format!("proxy {want:?}"), secs, || async {
        let s = h.proxy_status();
        (s.health == want).then_some(s)
    })
    .await
}

/// Failure: a peer or a tracker is reached directly instead of through the proxy. The tracker's name exists only at
/// the proxy (`tracker.invalid`), so an announce that arrives went through it and resolved nothing here; the decoy
/// peer counts every connection it gets and the proxy every connection it made to it: a direct dial would make the
/// decoy's count the larger.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn peers_and_trackers_go_through_the_proxy_and_nothing_around_it() {
    let tracker = MockTracker::start(60).await;
    let fake = FakeProxy::start("user", "pass").await;
    fake.map_name("tracker.invalid", tracker.addr.ip());
    let url = format!("http://tracker.invalid:{}/announce", tracker.addr.port());
    let t = standard_torrent(2_000_000, Some(url), false, 7101);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let (decoy_addr, decoy_hits, decoy_utp) = decoy().await;
    let leech = node(|c| {
        c.trackers_enabled = true;
        c.utp = true;
        c.proxy = Some(via(&fake));
    })
    .await;
    wait_health(&leech.h, ProxyHealth::Up, 15).await;
    let opts = AddOptions { peers: vec![seeder.addr(), decoy_addr], ..AddOptions::default() };
    leech.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    wait_for("the announce through the proxy", 15, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;

    let asked = fake.connects();
    assert!(asked.contains(&seeder.addr().to_string()), "{asked:?}");
    assert!(asked.contains(&format!("tracker.invalid:{}", tracker.addr.port())), "{asked:?}");
    let to_decoy = asked.iter().filter(|a| **a == decoy_addr.to_string()).count();
    assert!(to_decoy >= 1, "{asked:?}");
    assert_eq!(decoy_hits.load(Ordering::SeqCst), to_decoy, "the decoy saw only the proxy's connections");
    assert!(decoy_utp.lock().unwrap().is_empty(), "no uTP to the decoy");
    let st = leech.h.proxy_status();
    assert_eq!(st.server.as_deref(), Some(fake.addr().to_string().as_str()));
    assert!(st.handshake_ms.is_some() && st.connect_ms.is_some(), "{st:?}");
}

/// Failure: with the proxy on, something of a torrent still leaves outside it: a uTP dial (the peer's TCP port is
/// closed, which makes the engine try uTP without the proxy), a UDP tracker, a DHT lookup of the torrent, or an
/// incoming connection (the listener must be closed).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn udp_dht_utp_and_incoming_are_off_while_the_proxy_is_on() {
    let fake = FakeProxy::start("user", "pass").await;
    let (utp_peer, utp_hits) = udp_watch().await; // its TCP port is closed: TCP fails, uTP would be next
    let (udp_tracker, tracker_hits) = udp_watch().await;
    let (dht_node, dht_hits) = udp_watch().await;
    let leech = node(|c| {
        c.trackers_enabled = true;
        c.utp = true;
        c.dht_enabled = true;
        c.dht_allow_local = true;
        c.dht_bootstrap = vec![dht_node.to_string()];
        c.proxy = Some(via(&fake));
    })
    .await;
    wait_health(&leech.h, ProxyHealth::Up, 15).await;
    let t = standard_torrent(1_000_000, Some(format!("udp://{udp_tracker}/announce")), false, 7102);
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![utp_peer], ..AddOptions::default() }).await.unwrap();
    wait_for("the dial through the proxy", 15, || async { fake.connects().contains(&utp_peer.to_string()).then_some(()) }).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(utp_hits.lock().unwrap().len(), 0, "no uTP");
    assert_eq!(tracker_hits.lock().unwrap().len(), 0, "no UDP tracker");
    // (The DHT node itself may run for folder sync; no datagram of it names the torrent.)
    assert_eq!(carrying(&dht_hits, &t.id[..]), 0, "no DHT lookup or announce for the torrent");
    // The UDP tracker says why it is not asked, and its breaker stays closed: it is not the tracker's fault.
    let d = wait_for("the UDP tracker's reason", 10, || async {
        leech.h.torrent_detail(t.id).await.ok().filter(|d| d.trackers.iter().any(|x| x.status.to_lowercase().contains("proxy")))
    })
    .await;
    assert!(d.trackers.iter().all(|x| x.tripped.is_none()), "{:?}", d.trackers.iter().map(|x| x.tripped).collect::<Vec<_>>());
    assert!(tokio::net::TcpStream::connect(leech.addr()).await.is_err(), "nobody connects in");
    assert!(leech.h.public_dht().await.is_some(), "folder sync keeps the DHT to find its devices (sync never uses the proxy)");
    // Off again: the listener is back on the same port.
    leech.h.set_proxy(None);
    wait_for("listening again", 10, || async { tokio::net::TcpStream::connect(leech.addr()).await.ok().map(|_| ()) }).await;
    assert_eq!(leech.h.proxy_status().health, ProxyHealth::Off);
}

/// Failure: the kill switch leaks (a proxy that is down makes the engine dial the peer or the tracker directly), or
/// torrents stay stuck once the proxy is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_proxy_that_is_down_holds_torrents_and_they_go_on_when_it_is_back() {
    let tracker = MockTracker::start(60).await;
    let t = standard_torrent(2_000_000, Some(tracker.url()), false, 7103);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;

    let fake = FakeProxy::start("user", "pass").await;
    fake.stop().await;
    let (decoy_addr, decoy_hits, decoy_utp) = decoy().await;
    let leech = node(|c| {
        c.trackers_enabled = true;
        c.utp = true;
        c.proxy = Some(via(&fake));
    })
    .await;
    let st = wait_health(&leech.h, ProxyHealth::Down, 15).await;
    assert!(matches!(st.problem, Some(ProxyProblem::Unreachable(_))), "{st:?}");
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr(), decoy_addr], ..AddOptions::default() }).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(leech.row(&t.id).await.unwrap().downloaded_total, 0);
    assert_eq!(decoy_hits.load(Ordering::SeqCst), 0, "no direct dial");
    assert!(decoy_utp.lock().unwrap().is_empty(), "no uTP dial");
    assert!(tracker.log().is_empty(), "no direct announce");

    fake.resume().await;
    wait_health(&leech.h, ProxyHealth::Up, 30).await;
    wait_done(&leech, &t.id, 60).await;
    // (A network change brings a failed announce forward, not before 30 s after the failure: `NETWORK_RETRY_FLOOR`.)
    wait_for("the announce once the proxy is back", 60, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    assert_eq!(decoy_hits.load(Ordering::SeqCst), fake.connects().iter().filter(|a| **a == decoy_addr.to_string()).count());
}

/// Failure: wrong credentials read as "unreachable" (the user would look for a network problem), or a slow proxy
/// is not told apart from a healthy one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn wrong_credentials_and_a_slow_proxy_are_told_apart() {
    let fake = FakeProxy::start("user", "pass").await;
    let mut wrong = via(&fake);
    wrong.auth = Some(proxy::Credentials::new("user", "nope").unwrap());
    let n = node(|c| c.proxy = Some(wrong)).await;
    let st = wait_health(&n.h, ProxyHealth::Down, 15).await;
    assert_eq!(st.problem, Some(ProxyProblem::AuthRejected));
    // Each of the handshake's two answers comes 1 s late: over the 1.5 s threshold.
    fake.set_delay(Duration::from_millis(1000));
    n.h.set_proxy(Some(via(&fake)));
    let st = wait_health(&n.h, ProxyHealth::Slow, 20).await;
    assert!(st.handshake_ms.unwrap() >= 1500, "{st:?}");
    fake.set_delay(Duration::ZERO);
}

// ---- a DHT node on loopback that answers ---------------------------------------------------

/// The value of the transaction id (`1:t<len>:<tid>`) of a KRPC message.
fn krpc_tid(d: &[u8]) -> Option<Vec<u8>> {
    let i = d.windows(3).position(|w| w == b"1:t")? + 3;
    let colon = i + d[i..].iter().position(|&c| c == b':')?;
    let n: usize = std::str::from_utf8(&d[i..colon]).ok()?.parse().ok()?;
    d.get(colon + 1..colon + 1 + n).map(<[u8]>::to_vec)
}

fn krpc_reply(tid: &[u8], id: &[u8; 20], nodes: &[u8], token: bool) -> Vec<u8> {
    let mut r = b"d1:rd2:id20:".to_vec();
    r.extend_from_slice(id);
    r.extend_from_slice(format!("5:nodes{}:", nodes.len()).as_bytes());
    r.extend_from_slice(nodes);
    if token {
        r.extend_from_slice(b"5:token3:tok");
    }
    r.extend_from_slice(format!("e1:t{}:", tid.len()).as_bytes());
    r.extend_from_slice(tid);
    r.extend_from_slice(b"1:y1:re");
    r
}

/// A DHT node: answers pings and find_node at once (knowing no one), and a get_peers only once `release` is set,
/// with a token and `next` as the closer node to ask. Keeps every datagram it got.
async fn dht_node(id: [u8; 20], next: Option<([u8; 20], std::net::SocketAddrV4)>, release: Arc<std::sync::atomic::AtomicBool>) -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<Vec<u8>>>>) {
    let s = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let addr = s.local_addr().unwrap();
    let got = Arc::new(std::sync::Mutex::new(Vec::new()));
    let g = got.clone();
    tokio::spawn(async move {
        let mut b = [0u8; 2048];
        while let Ok((n, from)) = s.recv_from(&mut b).await {
            let d = b[..n].to_vec();
            g.lock().unwrap().push(d.clone());
            let Some(tid) = krpc_tid(&d) else { continue };
            if d.windows(9).any(|w| w == b"get_peers") {
                let nodes = next.map(|(nid, a)| {
                    let mut v = nid.to_vec();
                    v.extend_from_slice(&a.ip().octets());
                    v.extend_from_slice(&a.port().to_be_bytes());
                    v
                });
                let (s, release) = (s.clone(), release.clone());
                tokio::spawn(async move {
                    while !release.load(Ordering::SeqCst) {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    let _ = s.send_to(&krpc_reply(&tid, &id, &nodes.unwrap_or_default(), true), from).await;
                });
            } else if next.is_some() {
                let _ = s.send_to(&krpc_reply(&tid, &id, &[], false), from).await;
            }
        }
    });
    (addr, got)
}

/// Failure: a DHT lookup of a torrent that was running when the proxy came on goes on from this address (further
/// get_peers, then an announce_peer with the torrent's port). The first node answers only after the proxy is on and
/// names a second node: a live lookup would ask that one next.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dht_lookup_running_when_the_proxy_comes_on_stops() {
    let fake = FakeProxy::start("user", "pass").await;
    let release = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (b_addr, b_got) = dht_node([0xBB; 20], None, release.clone()).await;
    let std::net::SocketAddr::V4(b4) = b_addr else { unreachable!() };
    let (a_addr, a_got) = dht_node([0xAA; 20], Some(([0xBB; 20], b4)), release.clone()).await;
    let leech = node(|c| {
        c.dht_enabled = true;
        c.dht_allow_local = true;
        c.dht_bootstrap = vec![a_addr.to_string()];
    })
    .await;
    let t = standard_torrent(1_000_000, None, false, 7104);
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("the torrent's lookup at the first node", 30, || async { (carrying(&a_got, &t.id[..]) > 0).then_some(()) }).await;
    leech.h.set_proxy(Some(via(&fake)));
    wait_health(&leech.h, ProxyHealth::Up, 15).await;
    release.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(carrying(&b_got, &t.id[..]), 0, "the lookup went on after the proxy came on");
}

/// Failure: a direct connection made before the proxy came on survives it (the download goes on from this address).
/// The proxy is switched on mid-download, and it is down: the seeder must lose the leech's direct connection and no
/// byte may arrive after that.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switching_the_proxy_on_mid_download_closes_the_direct_connection() {
    let t = standard_torrent(30_000_000, None, false, 7105);
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 60).await;
    let leech = node(|c| c.down_limit = Some(300_000)).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..AddOptions::default() }).await.unwrap();
    wait_for("the direct download under way", 30, || async { leech.row(&t.id).await.filter(|r| r.downloaded_total > 300_000).map(|_| ()) }).await;

    let fake = FakeProxy::start("user", "pass").await;
    fake.stop().await;
    leech.h.set_proxy(Some(via(&fake)));
    wait_for("the seeder lost the direct connection", 10, || async {
        (seeder.h.snapshot().await.ok()?.connections == 0).then_some(())
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let before = leech.row(&t.id).await.unwrap().downloaded_total;
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(leech.row(&t.id).await.unwrap().downloaded_total, before, "nothing more came directly");
    assert_eq!(leech.h.snapshot().await.unwrap().connections, 0);
}

/// Failure: a torrent marked for `seedwell sim` skips the proxy for every tracker, not only for the mock tracker on
/// loopback: its announce to another tracker leaves directly. The tracker's name exists only at the proxy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_simulated_torrent_bypasses_the_proxy_only_for_loopback() {
    let tracker = MockTracker::start(60).await;
    let fake = FakeProxy::start("user", "pass").await;
    fake.map_name("tracker.invalid", tracker.addr.ip());
    let t = standard_torrent(0, Some(format!("http://tracker.invalid:{}/announce", tracker.addr.port())), false, 7106);
    let n = node(|c| {
        c.trackers_enabled = true;
        c.proxy = Some(via(&fake));
    })
    .await;
    wait_health(&n.h, ProxyHealth::Up, 15).await;
    n.h.set_simulated(t.id, true);
    t.write_to(n.dir.path());
    n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_for("the announce through the proxy", 20, || async { (!tracker.log_for(&t.id).is_empty()).then_some(()) }).await;
    assert!(fake.connects().contains(&format!("tracker.invalid:{}", tracker.addr.port())), "{:?}", fake.connects());
}

/// Failure: a `.torrent` link (Web sources; a private tracker's carries the passkey) is fetched directly while the
/// proxy is on: a local lookup of its name and a connection from this address. The name exists only at the proxy;
/// with the proxy down the fetch waits and the server hears nothing, then it goes once the proxy is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_torrent_link_is_fetched_through_the_proxy_and_waits_for_it() {
    let (t1, t2) = (standard_torrent(100_000, None, false, 7107), standard_torrent(100_000, None, false, 7108));
    let served = |path: &str, t: &TestTorrent| {
        let mut f = ServedFile::new(path, t.bytes.clone());
        f.content_type = "application/x-bittorrent".into();
        f
    };
    let srv = serve_files(vec![served("/one.torrent", &t1), served("/two.torrent", &t2)]).await;
    let fake = FakeProxy::start("user", "pass").await;
    fake.map_name("files.invalid", srv.addr.ip());
    let (state, dl) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let n = snode(state.path(), dl.path(), None, |c| c.proxy = Some(via(&fake))).await;
    wait_health(&n.h, ProxyHealth::Up, 15).await;
    let link = |p: &str| format!("http://files.invalid:{}{p}", srv.addr.port());
    n.h.add_download(&link("/one.torrent"), DownloadOptions::default()).await.unwrap();
    wait_for("the first torrent", 20, || async { n.row(&t1.id).await.filter(|r| r.kind == RowKind::Torrent).map(|_| ()) }).await;
    assert!(fake.connects().contains(&format!("files.invalid:{}", srv.addr.port())), "{:?}", fake.connects());

    // Down: the fetch fails through it (or waits, once the monitor has seen it) and is tried again later.
    fake.stop().await;
    let hits = srv.hits().len();
    n.h.add_download(&link("/two.torrent"), DownloadOptions::default()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(srv.hits().len(), hits, "nothing fetched while the proxy is down");
    fake.resume().await;
    wait_for("the second torrent once the proxy is back", 60, || async { n.row(&t2.id).await.filter(|r| r.kind == RowKind::Torrent).map(|_| ()) }).await;
}
