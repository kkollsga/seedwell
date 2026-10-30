//! F1 (Linux/trackers report): a dual-stack tracker whose first address
//! accepts the TCP connection and then closes it or never answers.
//!
//! The name `dual.test` resolves (from a fixed table, no DNS) to `::1` and
//! `127.0.0.1`, in that order, with one mock per family on the same port:
//! the IPv6 mock misbehaves, the IPv4 mock is a normal tracker. Before the fix
//! the announce stayed on IPv6 and the tracker never heard from us.
//!
//! Part 1 drives the transport directly (what is sent where, how often, and
//! the `completed` rule). Part 2 puts the real transport behind the real hub,
//! gate and machines on a virtual clock and checks the cadence the IPv4
//! tracker sees (the lab's `v6close` / `v6blackhole` / `v6blackhole-i300`).

use crate::common;

use std::cell::RefCell;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use trs_tracker::gate::{GateConfig, GatedRequest, HostGate};
use trs_tracker::http::{HttpConfig, HttpTransport};
use trs_tracker::machine::SendKind;
use trs_tracker::policy::DestinationPolicy;
use trs_tracker::response::AnnounceError;
use trs_tracker::*;

const OK: u8 = 0;
/// Accept, read the request, close without answering.
const CLOSE: u8 = 1;
/// Accept and close at once, without reading.
const CLOSE_UNREAD: u8 = 2;
/// Accept, read the request, never answer.
const BLACKHOLE: u8 = 3;

/// One mock tracker on one address family.
struct Mock {
    mode: Arc<AtomicU8>,
    connections: Arc<AtomicUsize>,
    /// Complete requests received (request head fully read).
    requests: Arc<AtomicUsize>,
    /// The `event=` of every request received.
    events: Arc<Mutex<Vec<String>>>,
    /// Stops accepting (the listener is dropped: connections are refused).
    stop: tokio::task::AbortHandle,
}

impl Mock {
    fn refuse_from_now_on(&self) {
        self.stop.abort();
    }
    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }
    fn conns(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
    fn reqs(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

fn body(interval: u32, min: Option<u32>) -> Vec<u8> {
    let mut b = format!("d8:intervali{interval}e").into_bytes();
    if let Some(m) = min {
        b.extend(format!("12:min intervali{m}e").as_bytes());
    }
    b.extend(b"5:peers0:e");
    b
}

fn spawn_mock(listener: TcpListener, mode: u8, interval: u32, min: Option<u32>) -> Mock {
    let mode = Arc::new(AtomicU8::new(mode));
    let conns = Arc::new(AtomicUsize::new(0));
    let reqs = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let (mode_c, conns_c, reqs_c, events_c) =
        (mode.clone(), conns.clone(), reqs.clone(), events.clone());
    let task = tokio::spawn(async move {
        let (mode, conns, reqs, events) = (mode_c, conns_c, reqs_c, events_c);
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            conns.fetch_add(1, Ordering::SeqCst);
            let (mode, reqs, events) = (mode.clone(), reqs.clone(), events.clone());
            tokio::spawn(async move {
                if mode.load(Ordering::SeqCst) == CLOSE_UNREAD {
                    return; // dropped: FIN/RST right after accept
                }
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                loop {
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut tmp).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend(&tmp[..n]),
                        }
                    }
                    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    buf.drain(..end);
                    reqs.fetch_add(1, Ordering::SeqCst);
                    let event = head
                        .split(['?', '&', ' '])
                        .find_map(|p| p.strip_prefix("event="))
                        .unwrap_or("")
                        .to_owned();
                    events.lock().unwrap().push(event);
                    match mode.load(Ordering::SeqCst) {
                        CLOSE => return,
                        BLACKHOLE => {
                            std::future::pending::<()>().await;
                        }
                        _ => {
                            let b = body(interval, min);
                            let mut r =
                                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", b.len())
                                    .into_bytes();
                            r.extend(b);
                            if sock.write_all(&r).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
    });
    Mock {
        mode,
        connections: conns,
        requests: reqs,
        events,
        stop: task.abort_handle(),
    }
}

/// A v4 and a v6 listener on the same port, or `None` without IPv6 loopback.
async fn dual_listeners() -> Option<(u16, TcpListener, TcpListener)> {
    for _ in 0..50 {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.ok()?;
        let port = v4.local_addr().ok()?.port();
        match TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await {
            Ok(v6) => return Some((port, v4, v6)),
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(e) => {
                eprintln!("no IPv6 loopback ({e}): skipping");
                return None;
            }
        }
    }
    None
}

struct Lab {
    port: u16,
    v4: Mock,
    v6: Mock,
}

async fn lab(v6_mode: u8, interval: u32, min: Option<u32>) -> Option<Lab> {
    let (port, l4, l6) = dual_listeners().await?;
    Some(Lab {
        port,
        v4: spawn_mock(l4, OK, interval, min),
        v6: spawn_mock(l6, v6_mode, interval, min),
    })
}

fn transport(cfg: HttpConfig, v6_first: bool) -> HttpTransport {
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let ips = if v6_first { vec![v6, v4] } else { vec![v4, v6] };
    HttpTransport::with_static_hosts(
        "0.0.1",
        HttpConfig {
            policy: DestinationPolicy::LOCAL,
            ..cfg
        },
        &[("dual.test", ips)],
    )
}

fn fast_cfg() -> HttpConfig {
    HttpConfig {
        address_timeout: Duration::from_millis(400),
        total_timeout: Duration::from_secs(6),
        ..HttpConfig::default()
    }
}

fn params(event: Option<Event>) -> AnnounceParams {
    AnnounceParams {
        info_hash: *b"\x00\x01abcdefghijklmnopq\xff",
        peer_id: *b"-SX0001-abcdefghijkl",
        port: 6881,
        uploaded: 0,
        downloaded: 0,
        corrupt: 0,
        left: 3,
        key: 0xDEADBEEF,
        event,
        numwant: 50,
        tracker_id: None,
        ipv4: None,
        ipv6: None,
        crypto: Default::default(),
    }
}

/// The only way to get a request: through a gate. The gate's own audit wants
/// `started` first, so another event is released after a `started` that is
/// answered inside the gate (it never reaches the transport).
fn gated(port: u16, event: Option<Event>) -> GatedRequest {
    gated_url(&format!("http://dual.test:{port}/announce"), event)
}

fn gated_url(url: &str, event: Option<Event>) -> GatedRequest {
    let mut gate = HostGate::new(GateConfig {
        initial_tokens: 3,
        ..GateConfig::default()
    });
    let u = Arc::new(TrackerUrl::parse(url).unwrap());
    let p = params(Some(Event::Started));
    gate.register_torrent(p.info_hash, vec![u.clone()], &[], Millis(0));
    let mut now = Millis(0);
    if event != Some(Event::Started) {
        gate.submit(
            (p.info_hash, 0),
            RequestId(1),
            u.clone(),
            p.clone(),
            SendKind::Event,
            now,
        )
        .unwrap();
        let first = gate.poll(now).remove(0);
        gate.finish(
            first.ticket(),
            trs_tracker::gate::Outcome::Success {
                timing: clamp_timing(Some(900), None),
            },
            now,
        );
        now = Millis(3_600_000);
    }
    let mut p = p;
    p.event = event;
    if event == Some(Event::Completed) {
        p.left = 0;
    }
    gate.submit((p.info_hash, 0), RequestId(2), u, p, SendKind::Event, now)
        .unwrap();
    gate.poll(now).remove(0)
}

macro_rules! lab_or_skip {
    ($e:expr) => {
        match $e.await {
            Some(l) => l,
            None => return,
        }
    };
}

// ---------------------------------------------------------------- part 1

#[tokio::test]
async fn accept_then_close_falls_back_to_ipv4_once_and_remembers() {
    for mode in [CLOSE, CLOSE_UNREAD] {
        let lab = lab_or_skip!(lab(mode, 900, None));
        let t = transport(fast_cfg(), true);
        let out = t.announce(&gated(lab.port, Some(Event::Started))).await;
        assert!(out.result.is_ok(), "mode {mode}: {:?}", out.result.err());
        assert_eq!(out.extra_hops, 0, "the fallback hop is not a redirect hop");
        assert_eq!(lab.v6.conns(), 1);
        assert_eq!(lab.v4.conns(), 1);
        // The tracker that answered got exactly one complete announce.
        assert_eq!(lab.v4.reqs(), 1);
        assert_eq!(lab.v4.events.lock().unwrap().as_slice(), ["started"]);
        // Later announces go to the family that worked, without touching IPv6.
        for _ in 0..3 {
            let out = t.announce(&gated(lab.port, None)).await;
            assert!(out.result.is_ok());
        }
        assert_eq!(lab.v6.conns(), 1, "mode {mode}: IPv6 is not tried again");
        assert_eq!(lab.v4.reqs(), 4);
    }
}

#[tokio::test]
async fn the_trace_says_which_address_answered_and_that_the_other_family_was_tried() {
    let lab = lab_or_skip!(lab(CLOSE, 900, None));
    let t = transport(fast_cfg(), true);
    let out = t.announce(&gated(lab.port, Some(Event::Started))).await;
    assert!(out.result.is_ok());
    assert_eq!(out.trace.addr, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert!(out.trace.fallback, "the IPv6 address failed first");
    assert_eq!(out.trace.status, Some(200));
    // remembered: the next one goes straight to IPv4, no fallback
    let out = t.announce(&gated(lab.port, None)).await;
    assert_eq!(out.trace.addr, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    assert!(!out.trace.fallback);
}

#[tokio::test]
async fn accept_then_blackhole_falls_back_after_the_address_timeout() {
    let lab = lab_or_skip!(lab(BLACKHOLE, 900, None));
    let t = transport(fast_cfg(), true);
    let t0 = Instant::now();
    let out = t.announce(&gated(lab.port, Some(Event::Started))).await;
    let took = t0.elapsed();
    assert!(out.result.is_ok(), "{:?}", out.result.err());
    assert!(
        took >= Duration::from_millis(350),
        "waited for the IPv6 address first: {took:?}"
    );
    assert!(took < Duration::from_secs(3), "{took:?}");
    assert_eq!((lab.v6.conns(), lab.v6.reqs()), (1, 1));
    assert_eq!((lab.v4.conns(), lab.v4.reqs()), (1, 1));
    // Remembered: the next announce is immediate and IPv4 only.
    let t0 = Instant::now();
    assert!(t.announce(&gated(lab.port, None)).await.result.is_ok());
    assert!(t0.elapsed() < Duration::from_millis(300));
    assert_eq!(lab.v6.conns(), 1);
}

#[tokio::test]
async fn ipv4_first_with_a_broken_ipv4_falls_back_to_ipv6() {
    let (port, l4, l6) = match dual_listeners().await {
        Some(x) => x,
        None => return,
    };
    let v4 = spawn_mock(l4, CLOSE, 900, None);
    let v6 = spawn_mock(l6, OK, 900, None);
    let t = transport(fast_cfg(), false);
    assert!(t.announce(&gated(port, None)).await.result.is_ok());
    assert_eq!((v4.conns(), v6.reqs()), (1, 1));
}

#[tokio::test]
async fn completed_is_never_sent_to_a_second_address() {
    // The machine keeps `completed` set after an ambiguous failure
    // (at-most-once), so a second send here could double-count it.
    for mode in [CLOSE, BLACKHOLE] {
        let lab = lab_or_skip!(lab(mode, 900, None));
        let t = transport(
            HttpConfig {
                total_timeout: Duration::from_millis(1500),
                ..fast_cfg()
            },
            true,
        );
        let out = t.announce(&gated(lab.port, Some(Event::Completed))).await;
        let err = out.result.expect_err("no answer from IPv6");
        assert!(!err.never_reached_tracker(), "ambiguous: {err:?}");
        assert_eq!(lab.v6.conns(), 1);
        assert_eq!(
            lab.v4.conns(),
            0,
            "mode {mode}: completed must not move to IPv4"
        );
        assert_eq!(lab.v4.reqs(), 0);
    }
}

#[tokio::test]
async fn completed_moves_on_when_the_remembered_family_refuses_the_connection() {
    // Nothing was written, so this is safe for `completed` too (hyper's own
    // connector would have gone on to the next address).
    let lab = lab_or_skip!(lab(CLOSE, 900, None));
    let t = transport(
        HttpConfig {
            pool_idle_timeout: Duration::from_millis(30),
            ..fast_cfg()
        },
        true,
    );
    assert!(
        t.announce(&gated(lab.port, Some(Event::Started)))
            .await
            .result
            .is_ok()
    ); // remembers v4
    lab.v4.refuse_from_now_on();
    lab.v6.set(OK);
    tokio::time::sleep(Duration::from_millis(150)).await; // the kept-alive connection is gone
    let out = t.announce(&gated(lab.port, Some(Event::Completed))).await;
    assert!(out.result.is_ok(), "{:?}", out.result.err());
    assert_eq!(lab.v4.events.lock().unwrap().as_slice(), ["started"]);
    assert_eq!(
        lab.v6.events.lock().unwrap().as_slice(),
        ["started", "completed"]
    );
}

#[tokio::test]
async fn completed_uses_the_remembered_family() {
    let lab = lab_or_skip!(lab(BLACKHOLE, 900, None));
    let t = transport(fast_cfg(), true);
    assert!(
        t.announce(&gated(lab.port, Some(Event::Started)))
            .await
            .result
            .is_ok()
    );
    let out = t.announce(&gated(lab.port, Some(Event::Completed))).await;
    assert!(out.result.is_ok());
    assert_eq!(lab.v6.reqs(), 1, "only the started went to IPv6");
    assert_eq!(
        lab.v4.events.lock().unwrap().as_slice(),
        ["started", "completed"]
    );
}

#[tokio::test]
async fn stopped_falls_back_like_started() {
    let lab = lab_or_skip!(lab(CLOSE, 900, None));
    let t = transport(fast_cfg(), true);
    let out = t.announce(&gated(lab.port, Some(Event::Stopped))).await;
    assert!(out.result.is_ok());
    assert_eq!(lab.v4.events.lock().unwrap().as_slice(), ["stopped"]);
}

#[tokio::test]
async fn both_families_broken_gives_exactly_two_tries_and_an_ambiguous_error() {
    let lab = lab_or_skip!(lab(CLOSE, 900, None));
    lab.v4.set(CLOSE);
    let t = transport(fast_cfg(), true);
    let out = t.announce(&gated(lab.port, Some(Event::Started))).await;
    let err = out.result.expect_err("both fail");
    assert!(!err.never_reached_tracker(), "{err:?}");
    assert_eq!(lab.v6.conns(), 1);
    assert_eq!(lab.v4.conns(), 1, "one fallback, never a loop");
    assert_eq!((lab.v6.reqs(), lab.v4.reqs()), (1, 1));
}

#[tokio::test]
async fn a_remembered_family_that_breaks_hands_over_once_and_forgets() {
    let lab = lab_or_skip!(lab(CLOSE, 900, None));
    let t = transport(fast_cfg(), true);
    assert!(t.announce(&gated(lab.port, None)).await.result.is_ok()); // remembers v4
    // Now IPv4 breaks and IPv6 recovers.
    lab.v4.set(CLOSE);
    lab.v6.set(OK);
    let before = (lab.v4.reqs(), lab.v6.conns());
    assert!(t.announce(&gated(lab.port, None)).await.result.is_ok());
    // (the kept-alive IPv4 connection was reused, got the request and died)
    assert_eq!(lab.v4.reqs(), before.0 + 1, "IPv4 saw the request once");
    assert_eq!(lab.v6.conns(), before.1 + 1, "then IPv6 once");
    assert_eq!(lab.v6.reqs(), 2, "and answered it");
    // IPv6 is now the remembered family.
    assert!(t.announce(&gated(lab.port, None)).await.result.is_ok());
    assert_eq!(lab.v4.reqs(), before.0 + 1);
}

#[tokio::test]
async fn the_remembered_family_expires() {
    let lab = lab_or_skip!(lab(CLOSE, 900, None));
    let t = transport(
        HttpConfig {
            family_memory: Duration::from_millis(300),
            ..fast_cfg()
        },
        true,
    );
    assert!(t.announce(&gated(lab.port, None)).await.result.is_ok());
    assert_eq!(lab.v6.conns(), 1);
    assert!(t.announce(&gated(lab.port, None)).await.result.is_ok());
    assert_eq!(lab.v6.conns(), 1);
    tokio::time::sleep(Duration::from_millis(350)).await;
    // IPv6 is probed again once, fails, and IPv4 answers.
    assert!(t.announce(&gated(lab.port, None)).await.result.is_ok());
    assert_eq!(lab.v6.conns(), 2);
}

#[tokio::test]
async fn a_working_first_family_is_used_without_any_second_try() {
    let lab = lab_or_skip!(lab(OK, 900, None));
    let t = transport(fast_cfg(), true);
    for _ in 0..3 {
        assert!(t.announce(&gated(lab.port, None)).await.result.is_ok());
    }
    assert_eq!(lab.v6.reqs(), 3);
    assert_eq!((lab.v4.conns(), lab.v4.reqs()), (0, 0));
}

#[tokio::test]
async fn http_status_errors_do_not_fall_back() {
    // A 5xx is an answer: the tracker was reached. No second send.
    let (port, l4, l6) = match dual_listeners().await {
        Some(x) => x,
        None => return,
    };
    let _v6 = spawn_mock(l6, OK, 900, None);
    drop(l4);
    // (re-bind v4 as a 500 server)
    let l4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l4.accept().await else {
                return;
            };
            h.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut b = [0u8; 4096];
                let _ = s.read(&mut b).await;
                let _ = s
                    .write_all(b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n")
                    .await;
            });
        }
    });
    // IPv4 first so the 500 is the first answer
    let t = transport(fast_cfg(), false);
    let out = t.announce(&gated(port, None)).await;
    assert!(matches!(
        out.result,
        Err(AnnounceError::Http { status: 500, .. })
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------- part 2

/// (torrent number, virtual release time, event) of one announce.
type Released = (u32, Millis, Option<Event>);

/// What a run saw.
struct Run {
    /// Regular (non-event) gaps in seconds, all torrents.
    gaps: Vec<f64>,
    /// Announces released per torrent.
    per: Vec<usize>,
    v6_conns: usize,
    v4_reqs: usize,
    /// Failures the client saw.
    failures: u32,
}

/// Real transport behind the real hub/gate/machines on a virtual clock.
struct Real {
    rt: Arc<tokio::runtime::Runtime>,
    t: HttpTransport,
    detector: Rc<RefCell<SpamDetector>>,
    /// Virtual release time of every announce (all of them went through the transport).
    released: Rc<RefCell<Vec<Released>>>,
    failures: Rc<RefCell<u32>>,
}

impl Responder for Real {
    fn respond(&mut self, now: Millis, _host: &str, req: &GatedRequest) -> Reply {
        let p = req.announce_params().unwrap();
        let n = u32::from_be_bytes(p.info_hash[..4].try_into().unwrap());
        self.released.borrow_mut().push((n, now, p.event));
        let out = self.rt.block_on(self.t.announce(req));
        match &out.result {
            Ok(_) => self.detector.borrow_mut().arrive(now, p.info_hash, p.event),
            Err(_) => *self.failures.borrow_mut() += 1,
        }
        Reply::new(Duration::from_millis(100), out.result)
    }
}

/// Run `torrents` torrents for 15 virtual minutes against the broken-IPv6 lab.
/// Returns (regular gaps in seconds, announces per torrent, IPv6 connections,
/// IPv4 requests, failures seen by the client).
fn cadence(v6_mode: u8, interval: u32, min: Option<u32>, torrents: u32) -> Option<Run> {
    let rt = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    let lab = rt.block_on(lab(v6_mode, interval, min))?;
    let detector = Rc::new(RefCell::new(SpamDetector::new(Duration::from_secs(
        u64::from(min.unwrap_or(0)).max(30),
    ))));
    let released = Rc::new(RefCell::new(Vec::new()));
    let failures = Rc::new(RefCell::new(0));
    let responder = Real {
        rt: rt.clone(),
        t: transport(fast_cfg(), true),
        detector: detector.clone(),
        released: released.clone(),
        failures: failures.clone(),
    };
    let mut w = World::new(responder, GateConfig::default());
    w.oracle.trace_on = false;
    let url = format!("http://dual.test:{}/announce", lab.port);
    for n in 1..=torrents {
        w.add_torrent(spec(n, 6881, &[&[url.as_str()]]), 1000, 1000);
    }
    for n in 1..=torrents {
        w.start(ih(n));
    }
    w.run_for(Duration::from_secs(900));
    drop(w);

    let rel = released.borrow();
    let mut gaps = Vec::new();
    let mut per = Vec::new();
    for n in 1..=torrents {
        let mine: Vec<_> = rel.iter().filter(|(i, _, _)| *i == n).collect();
        per.push(mine.len());
        let regular: Vec<Millis> = mine
            .iter()
            .filter(|(_, _, e)| e.is_none())
            .map(|(_, t, _)| *t)
            .collect();
        gaps.extend(regular.windows(2).map(|p| p[1].since(p[0]).as_secs_f64()));
    }
    assert_eq!(detector.borrow().spam, 0, "tracker-side announce spam");
    assert!(lab.v4.reqs() > 0);
    let failures = *failures.borrow();
    Some(Run {
        gaps,
        per,
        v6_conns: lab.v6.conns(),
        v4_reqs: lab.v4.reqs(),
        failures,
    })
}

fn check(name: &str, interval: f64, got: Option<Run>) {
    let Some(Run {
        gaps,
        per,
        v6_conns,
        v4_reqs,
        failures,
    }) = got
    else {
        eprintln!("{name}: no IPv6 loopback, skipped");
        return;
    };
    println!(
        "{name}: announces per torrent {per:?}, regular gaps min {:.1} max {:.1} s, IPv6 connections {v6_conns}, IPv4 requests {v4_reqs}, failures {failures}",
        gaps.iter().cloned().fold(f64::MAX, f64::min),
        gaps.iter().cloned().fold(0.0, f64::max),
    );
    // "Normal cadence": started + one per interval, never early.
    let lo = (900.0 / interval).floor() as usize;
    let hi = (900.0 / interval).ceil() as usize + 2;
    for (i, n) in per.iter().enumerate() {
        assert!(
            (lo..=hi).contains(n),
            "{name}: torrent {} announced {n} times, expected {lo}..={hi}",
            i + 1
        );
    }
    assert!(!gaps.is_empty());
    for g in &gaps {
        assert!(
            *g >= interval - 0.5,
            "{name}: early regular announce after {g} s"
        );
        assert!(
            *g <= interval * 1.3,
            "{name}: late regular announce after {g} s"
        );
    }
    // Not hammering IPv6: one probe in the whole run (remembered family).
    assert!(v6_conns <= 2, "{name}: {v6_conns} IPv6 connections");
    // Every announce that left reached the IPv4 tracker exactly once.
    assert_eq!(v4_reqs, per.iter().sum::<usize>(), "{name}");
    assert_eq!(failures, 0, "{name}: the client saw failures");
}

#[test]
fn v6_accepts_and_closes_ipv4_gets_the_normal_cadence() {
    check("v6close 60/30", 60.0, cadence(CLOSE, 60, Some(30), 3));
}

#[test]
fn v6_accepts_and_never_answers_ipv4_gets_the_normal_cadence() {
    check(
        "v6blackhole 60/30",
        60.0,
        cadence(BLACKHOLE, 60, Some(30), 3),
    );
}

#[test]
fn v6_blackhole_with_interval_300_min_120_follows_the_300_s_interval() {
    check(
        "v6blackhole 300/120",
        300.0,
        cadence(BLACKHOLE, 300, Some(120), 3),
    );
}

// ---------------------------------------------------------------- UDP

use tokio::net::UdpSocket;
use trs_tracker::udp::{UdpConfig, UdpTransport};

/// A BEP 15 mock. `answer_connect` / `answer_announce` select what it answers;
/// every packet's action is logged.
struct UdpMock {
    actions: Arc<Mutex<Vec<u32>>>,
    events: Arc<Mutex<Vec<u32>>>,
}

impl UdpMock {
    fn count(&self, action: u32) -> usize {
        self.actions
            .lock()
            .unwrap()
            .iter()
            .filter(|a| **a == action)
            .count()
    }
}

fn spawn_udp(sock: UdpSocket, answer_connect: bool, answer_announce: bool) -> UdpMock {
    let m = UdpMock {
        actions: Arc::new(Mutex::new(Vec::new())),
        events: Arc::new(Mutex::new(Vec::new())),
    };
    let (log, events) = (m.actions.clone(), m.events.clone());
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                return;
            };
            if n < 16 {
                continue;
            }
            let pkt = buf[..n].to_vec();
            let action = u32::from_be_bytes(pkt[8..12].try_into().unwrap());
            log.lock().unwrap().push(action);
            let tid = &pkt[12..16];
            match action {
                0 if answer_connect => {
                    let mut r = vec![0, 0, 0, 0];
                    r.extend(tid);
                    r.extend(0x1234_5678_9abcu64.to_be_bytes());
                    let _ = sock.send_to(&r, from).await;
                }
                1 => {
                    // event field of the announce packet
                    events
                        .lock()
                        .unwrap()
                        .push(u32::from_be_bytes(pkt[80..84].try_into().unwrap()));
                    if answer_announce {
                        let mut r = vec![0, 0, 0, 1];
                        r.extend(tid);
                        r.extend(1800u32.to_be_bytes());
                        r.extend(0u32.to_be_bytes());
                        r.extend(1u32.to_be_bytes());
                        let _ = sock.send_to(&r, from).await;
                    }
                }
                _ => {}
            }
        }
    });
    m
}

async fn udp_lab(v4: (bool, bool), v6: (bool, bool)) -> Option<(u16, UdpMock, UdpMock)> {
    for _ in 0..50 {
        let s4 = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.ok()?;
        let port = s4.local_addr().ok()?.port();
        match UdpSocket::bind((Ipv6Addr::LOCALHOST, port)).await {
            Ok(s6) => {
                return Some((port, spawn_udp(s4, v4.0, v4.1), spawn_udp(s6, v6.0, v6.1)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(_) => return None,
        }
    }
    None
}

fn fast_udp() -> UdpConfig {
    UdpConfig {
        policy: DestinationPolicy::LOCAL,
        base_timeout: Duration::from_millis(40),
        max_exponent: 3,
        connection_id_ttl: Duration::from_secs(30),
        total_timeout: Duration::from_secs(8),
        bind: None,
    }
}

async fn udp_transport() -> UdpTransport {
    UdpTransport::bind_with_static_hosts(
        fast_udp(),
        &[(
            "udp.test",
            vec![
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ],
        )],
    )
    .await
    .unwrap()
}

fn udp_url(port: u16) -> String {
    format!("udp://udp.test:{port}/announce")
}

#[tokio::test]
async fn udp_unanswered_handshake_falls_back_to_the_other_family() {
    let (port, v4, v6) = lab_or_skip!(udp_lab((false, false), (true, true)));
    let t = udp_transport().await;
    // Even `completed`: only a handshake was abandoned, nothing announce-shaped was sent.
    let r = t
        .announce(&gated_url(&udp_url(port), Some(Event::Completed)))
        .await;
    assert!(r.is_ok(), "{:?}", r.err());
    assert_eq!(v4.count(1), 0, "IPv4 never saw an announce");
    assert!(
        (1..=3).contains(&v4.count(0)),
        "handshake tried at most 3 times: {}",
        v4.count(0)
    );
    assert_eq!((v6.count(0), v6.count(1)), (1, 1));
    assert_eq!(
        v6.events.lock().unwrap().as_slice(),
        [1],
        "one completed, once"
    );
    // Remembered: the next announce does not touch IPv4 again.
    let before = v4.count(0);
    assert!(t.announce(&gated_url(&udp_url(port), None)).await.is_ok());
    assert_eq!(v4.count(0), before);
}

#[tokio::test]
async fn udp_an_unanswered_announce_is_not_resent_elsewhere() {
    // IPv4 completes the handshake but never answers the announce: it may have
    // processed it, so IPv6 is not tried.
    let (port, v4, v6) = lab_or_skip!(udp_lab((true, false), (true, true)));
    let t = udp_transport().await;
    let err = t
        .announce(&gated_url(&udp_url(port), Some(Event::Started)))
        .await
        .expect_err("no announce answer");
    assert!(!err.never_reached_tracker(), "{err:?}");
    assert!(v4.count(1) >= 1);
    assert_eq!((v6.count(0), v6.count(1)), (0, 0));
}

#[tokio::test]
async fn udp_both_silent_is_a_connect_timeout_and_each_family_is_tried_once_per_schedule() {
    let (port, v4, v6) = lab_or_skip!(udp_lab((false, false), (false, false)));
    let t = udp_transport().await;
    let err = t
        .announce(&gated_url(&udp_url(port), Some(Event::Started)))
        .await
        .expect_err("silent");
    assert!(err.never_reached_tracker(), "{err:?}");
    assert!(v4.count(0) <= 3, "{}", v4.count(0));
    assert!(v6.count(0) <= 4, "{}", v6.count(0));
    assert_eq!(v4.count(1) + v6.count(1), 0);
}
