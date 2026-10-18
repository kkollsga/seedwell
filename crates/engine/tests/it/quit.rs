//! Quit time (SPEC 12 #16): the removal of port mappings, the DHT's last save and the tracker
//! `stopped` announces are independent, so they run side by side and the common quit is fast.
//! Loopback only: a NAT-PMP gateway, a silent DHT bootstrap node and mock trackers.
use crate::common;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use tokio::net::{TcpListener, UdpSocket};
use trs_engine::config::PortmapConfig;
use trs_engine::*;

/// A UDP socket that swallows everything: a DHT node that never answers.
async fn blackhole() -> std::net::SocketAddrV4 {
    let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let std::net::SocketAddr::V4(a) = s.local_addr().unwrap() else { unreachable!() };
    tokio::spawn(async move {
        let mut b = [0u8; 2048];
        while s.recv_from(&mut b).await.is_ok() {}
    });
    a
}

/// A NAT-PMP gateway. Mapping requests are answered at once; once `slow` is set every answer
/// takes `slow_delay` (a router that is busy or half asleep at quit time), and with `mute` set it
/// does not answer at all.
#[derive(Default)]
struct GwState {
    mapped: usize,
    deletes: usize,
    slow: Option<Duration>,
    mute: bool,
}

async fn gateway() -> (std::net::SocketAddrV4, Arc<Mutex<GwState>>) {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let std::net::SocketAddr::V4(addr) = sock.local_addr().unwrap() else { unreachable!() };
    let st = Arc::new(Mutex::new(GwState::default()));
    let s2 = st.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 64];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else { break };
            if n < 2 {
                continue;
            }
            let (version, op) = (buf[0], buf[1]);
            let reply: Vec<u8> = if version != 0 {
                vec![0, 128 | (op & 0x7f), 0, 1, 0, 0, 0, 1]
            } else if op == 0 {
                vec![0, 128, 0, 0, 0, 0, 0, 1, 203, 0, 113, 9]
            } else if (op == 1 || op == 2) && n >= 12 {
                let internal = u16::from_be_bytes([buf[4], buf[5]]);
                let life = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                {
                    let mut g = s2.lock().unwrap();
                    if life == 0 {
                        g.deletes += 1;
                    } else {
                        g.mapped += 1;
                    }
                }
                let mut r = vec![0, 128 | op, 0, 0, 0, 0, 0, 1];
                r.extend(internal.to_be_bytes());
                r.extend(if life == 0 { 0 } else { internal }.to_be_bytes());
                r.extend(life.to_be_bytes());
                r
            } else {
                continue;
            };
            let (mute, slow) = {
                let g = s2.lock().unwrap();
                (g.mute, g.slow)
            };
            if mute {
                continue;
            }
            if let Some(d) = slow {
                tokio::time::sleep(d).await;
            }
            let _ = sock.send_to(&reply, from).await;
        }
    });
    (addr, st)
}

/// A tracker that accepts connections and never answers.
async fn tarpit() -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    format!("http://{a}/announce")
}

struct Quit {
    took: Duration,
    report: ShutdownReport,
    stopped: usize,
    deletes: usize,
}

/// Three seeding torrents on a good tracker (and, with `dead_tracker`, a fourth on a tarpit), a
/// mapped listen port, a running DHT; then the quit with the gateway answering as `gw_at_quit`.
async fn quit(dead_tracker: bool, gw_at_quit: impl FnOnce(&mut GwState)) -> Quit {
    let tracker = MockTracker::start(60).await;
    let (gw, gws) = gateway().await;
    let boot = blackhole().await;
    let mut n = node(|c| {
        c.trackers_enabled = true;
        c.dht_enabled = true;
        c.dht_bind = "127.0.0.1:0".parse().unwrap();
        c.dht_bootstrap = vec![boot.to_string()];
        c.dht_allow_local = true;
        c.portmap = PortmapConfig { enabled: true, upnp: false, natpmp: true, test_gateway: Some(gw) };
    })
    .await;
    let mut ts = Vec::new();
    for i in 0..3 {
        ts.push(standard_named(&format!("quit-{i}"), 200_000, Some(tracker.url()), false, 900 + i));
    }
    if dead_tracker {
        ts.push(standard_named("quit-dead", 200_000, Some(tarpit().await), false, 950));
    }
    for t in &ts {
        t.write_to(n.dir.path());
        n.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    }
    for t in &ts {
        wait_done(&n, &t.id, 30).await;
    }
    wait_for("started announces", 20, || async { (tracker.log().len() >= 3).then_some(()) }).await;
    wait_for("mapped", 20, || async { (gws.lock().unwrap().mapped >= 2).then_some(()) }).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    gw_at_quit(&mut gws.lock().unwrap());
    let t0 = Instant::now();
    let report = n.shutdown(Duration::from_secs(120)).await;
    let took = t0.elapsed();
    let stopped = tracker.log().iter().filter(|a| a.event.as_deref() == Some("stopped")).count();
    let deletes = gws.lock().unwrap().deletes;
    Quit { took, report, stopped, deletes }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_common_quit_with_mapping_and_dht_on_is_fast() {
    let q = quit(false, |_| {}).await;
    eprintln!("QUIT common: {:?} {:?} stopped={} deletes={}", q.took, q.report, q.stopped, q.deletes);
    assert_eq!(q.stopped, 3, "{:?}", q.report);
    assert_eq!(q.deletes, 2);
    assert!(q.took < Duration::from_secs(3), "{:?}", q.took);
}

/// A router that stopped answering: its two deletions share one 3 s window with everything else
/// instead of coming after the tracker wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_mute_gateway_costs_its_own_window_only() {
    let q = quit(false, |g| g.mute = true).await;
    eprintln!("QUIT mute gateway: {:?} {:?} stopped={}", q.took, q.report, q.stopped);
    assert_eq!(q.stopped, 3);
    assert!(q.took < Duration::from_secs(5), "{:?}", q.took);
}

/// A dead tracker (accepts, never answers) holds the quit for its own bounded tail only: before,
/// the in-flight `started` (30 s) and then a three-attempt `stopped` with a minute of backoff
/// between the attempts ran into the 120 s cap; now about 5 s (3 s grace + 2 s answer wait).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_tracker_does_not_hold_the_quit_for_a_minute() {
    let q = quit(true, |_| {}).await;
    eprintln!("QUIT dead tracker: {:?} {:?} stopped={}", q.took, q.report, q.stopped);
    assert_eq!(q.stopped, 3);
    // The dead tracker's request in flight at quit is cut after 3 s, then its `stopped` (the
    // tracker accepts, so the connection is up at once) gets 2 s for an answer; the answered ones
    // were not held up behind it.
    assert_eq!(q.report.stopped_confirmed + q.report.stopped_abandoned, 4, "{:?}", q.report);
    assert_eq!(q.deletes, 2);
    assert!(q.took < Duration::from_secs(8), "{:?}", q.took);
    assert!(q.took > Duration::from_secs(4), "3 s grace for the request in flight plus the 2 s answer wait: {:?}", q.took);
}
