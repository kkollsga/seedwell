//! Adversarial audit: the HTTP and UDP transports against hostile loopback
//! servers. No traffic leaves the machine.
#![cfg(feature = "transport")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use trs_tracker::gate::{GateConfig, GatedRequest, HostGate};
use trs_tracker::http::{HttpConfig, HttpTransport};
use trs_tracker::machine::SendKind;
use trs_tracker::policy::DestinationPolicy;
use trs_tracker::response::{AnnounceError, ParseError, TransportError};
use trs_tracker::udp::{UdpConfig, UdpTransport};
use trs_tracker::*;

fn params() -> AnnounceParams {
    AnnounceParams {
        info_hash: *b"\x00\x01abcdefghijklmnopq\xff",
        peer_id: *b"-SX0001-abcdefghijkl",
        port: 6881,
        uploaded: 1,
        downloaded: 2,
        corrupt: 0,
        left: 3,
        key: 0xDEADBEEF,
        event: Some(Event::Started),
        numwant: 50,
        tracker_id: None,
        ipv4: None,
        ipv6: None,
        crypto: Default::default(),
    }
}

fn gated(url: &str) -> GatedRequest {
    let mut gate = HostGate::new(GateConfig {
        initial_tokens: 3,
        ..GateConfig::default()
    });
    let u = Arc::new(TrackerUrl::parse(url).unwrap());
    let p = params();
    gate.register_torrent(p.info_hash, vec![u.clone()], &[], Millis(0));
    gate.submit(
        (p.info_hash, 0),
        RequestId(1),
        u,
        p,
        SendKind::Event,
        Millis(0),
    )
    .unwrap();
    gate.poll(Millis(0)).remove(0)
}

fn ok_body() -> Vec<u8> {
    b"d8:intervali1800e5:peers6:\n\x00\x00\x01\x1a\xe1e".to_vec()
}

fn resp(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len()).into_bytes();
    for (k, v) in headers {
        r.extend(format!("{k}: {v}\r\n").as_bytes());
    }
    r.extend(b"\r\n");
    r.extend(body);
    r
}

enum Act {
    Raw(Vec<u8>),
    /// Send headers promising a lot, then the body slowly forever.
    Trickle,
    /// Send nothing, keep the socket open.
    Hang,
    /// Chunked body that never ends, as fast as the client reads.
    EndlessChunked,
}

struct Server {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
}

async fn serve(handler: impl Fn(&str, usize) -> Act + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(handler);
    let (r, c) = (requests.clone(), connections.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            c.fetch_add(1, Ordering::SeqCst);
            let (h, r) = (handler.clone(), r.clone());
            tokio::spawn(async move {
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
                    let text = String::from_utf8_lossy(&buf[..end]).to_string();
                    buf.drain(..end);
                    let n = {
                        let mut g = r.lock().unwrap();
                        g.push(text.clone());
                        g.len()
                    };
                    match h(&text, n) {
                        Act::Raw(b) => {
                            if sock.write_all(&b).await.is_err() {
                                return;
                            }
                        }
                        Act::Trickle => {
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n")
                                .await;
                            loop {
                                if sock.write_all(b"d").await.is_err() {
                                    return;
                                }
                                tokio::time::sleep(Duration::from_millis(30)).await;
                            }
                        }
                        Act::Hang => {
                            tokio::time::sleep(Duration::from_secs(3600)).await;
                            return;
                        }
                        Act::EndlessChunked => {
                            let _ = sock
                                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                                .await;
                            let chunk = format!("{:x}\r\n{}\r\n", 60000, "x".repeat(60000));
                            loop {
                                if sock.write_all(chunk.as_bytes()).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
            });
        }
    });
    Server {
        addr,
        requests,
        connections,
    }
}

fn url_for(s: &Server, path: &str) -> String {
    format!("http://127.0.0.1:{}{}", s.addr.port(), path)
}

/// Fast timeouts, loopback allowed (the hostile servers of this file live on
/// 127.0.0.1; the default policy would refuse them, see
/// `tracker_url_cannot_target_local_services`).
fn quick() -> HttpTransport {
    HttpTransport::with_config(
        "0.0.1",
        HttpConfig {
            connect_timeout: Duration::from_millis(500),
            total_timeout: Duration::from_millis(1500),
            pool_idle_timeout: Duration::from_secs(5),
            policy: DestinationPolicy::LOCAL,
            ..HttpConfig::default()
        },
    )
}

/// The same with the production policy.
fn quick_strict() -> HttpTransport {
    HttpTransport::with_config(
        "0.0.1",
        HttpConfig {
            connect_timeout: Duration::from_millis(500),
            total_timeout: Duration::from_millis(1500),
            pool_idle_timeout: Duration::from_secs(5),
            policy: DestinationPolicy::STRICT,
            ..HttpConfig::default()
        },
    )
}

// ---------------------------------------------------------------- HTTP

#[tokio::test]
async fn gzip_bomb_is_refused_quickly() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    // 300 MiB of zeros compress to ~300 KB: under the 1 MiB wire cap, far over the decoded cap
    let mut enc = GzEncoder::new(Vec::new(), Compression::best());
    let zeros = vec![0u8; 1 << 20];
    for _ in 0..300 {
        enc.write_all(&zeros).unwrap();
    }
    let gz = enc.finish().unwrap();
    assert!(gz.len() < (1 << 20), "test setup: {} bytes", gz.len());
    let s =
        serve(move |_, _| Act::Raw(resp("200 OK", &[("Content-Encoding", "gzip".into())], &gz)))
            .await;
    let started = Instant::now();
    let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
    assert!(out.result.is_err(), "bomb parsed as success?");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn trickling_and_endless_bodies_are_cut_off_by_the_total_timeout_and_the_size_cap() {
    let s = serve(|_, _| Act::Trickle).await;
    let t0 = Instant::now();
    let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
    assert_eq!(
        out.result.unwrap_err(),
        AnnounceError::Transport(TransportError::Timeout)
    );
    assert!(t0.elapsed() < Duration::from_secs(4));

    let s = serve(|_, _| Act::EndlessChunked).await;
    let t0 = Instant::now();
    let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
    assert!(matches!(
        out.result.unwrap_err(),
        AnnounceError::Transport(
            TransportError::TooLarge | TransportError::Timeout | TransportError::Io
        )
    ),);
    assert!(t0.elapsed() < Duration::from_secs(4));

    let s = serve(|_, _| Act::Hang).await;
    let t0 = Instant::now();
    let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
    assert_eq!(
        out.result.unwrap_err(),
        AnnounceError::Transport(TransportError::Timeout)
    );
    assert!(t0.elapsed() < Duration::from_secs(4));
}

#[tokio::test]
async fn declared_huge_content_length_with_small_body_is_an_error_not_a_hang() {
    let s = serve(|_, _| {
        Act::Raw(b"HTTP/1.1 200 OK\r\nContent-Length: 5000000\r\n\r\nd8:intervali900ee".to_vec())
    })
    .await;
    let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
    assert!(out.result.is_err());
}

#[tokio::test]
async fn unsupported_or_lying_content_encodings_are_garbage_never_success() {
    for enc in ["br", "deflate", "compress", "gzip, gzip", "zstd"] {
        let e = enc.to_string();
        let s = serve(move |_, _| {
            Act::Raw(resp(
                "200 OK",
                &[("Content-Encoding", e.clone())],
                &ok_body(),
            ))
        })
        .await;
        let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
        assert_eq!(
            out.result.unwrap_err(),
            AnnounceError::Garbage(ParseError::NotBencode),
            "Content-Encoding: {enc}"
        );
    }
    // claims gzip but sends plain bencode
    let s = serve(|_, _| {
        Act::Raw(resp(
            "200 OK",
            &[("Content-Encoding", "gzip".into())],
            &ok_body(),
        ))
    })
    .await;
    assert!(
        quick()
            .announce(&gated(&url_for(&s, "/announce")))
            .await
            .result
            .is_err()
    );
}

#[tokio::test]
async fn one_gated_request_buys_at_most_four_wire_requests_and_reports_the_hops() {
    // A server that redirects to itself forever.
    let s = serve(|req, _| {
        let path = req
            .lines()
            .next()
            .unwrap_or("")
            .split(' ')
            .nth(1)
            .unwrap_or("/")
            .to_string();
        Act::Raw(resp("302 Found", &[("Location", format!("{path}x"))], b""))
    })
    .await;
    let out = quick()
        .announce(&gated(&url_for(&s, "/announce?pk=1")))
        .await;
    assert_eq!(
        out.result.unwrap_err(),
        AnnounceError::Transport(TransportError::Redirect)
    );
    assert_eq!(
        out.extra_hops, 3,
        "the engine must charge these to the bucket"
    );
    assert_eq!(s.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn redirect_hops_are_reported_even_when_the_final_answer_is_a_success() {
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let s = serve(move |_, _| {
        if h.fetch_add(1, Ordering::SeqCst) < 2 {
            Act::Raw(resp("301 Moved", &[("Location", "/second".into())], b""))
        } else {
            Act::Raw(resp("200 OK", &[], &ok_body()))
        }
    })
    .await;
    let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
    assert!(out.result.is_ok());
    assert_eq!(out.extra_hops, 2);
}

#[tokio::test]
async fn request_shape_has_no_credentials_cookies_or_extra_headers() {
    let s =
        serve(|_, _| Act::Raw(resp("200 OK", &[("Set-Cookie", "a=b".into())], &ok_body()))).await;
    let t = quick();
    for _ in 0..3 {
        assert!(
            t.announce(&gated(&url_for(&s, "/announce?passkey=zzz")))
                .await
                .result
                .is_ok()
        );
    }
    let reqs = s.requests.lock().unwrap();
    assert_eq!(reqs.len(), 3);
    for r in reqs.iter() {
        let names: Vec<&str> = r
            .lines()
            .skip(1)
            .filter_map(|l| l.split(':').next())
            .filter(|n| !n.is_empty())
            .collect();
        assert_eq!(
            names,
            ["Host", "User-Agent", "Accept", "Accept-Encoding"],
            "{r}"
        );
        assert!(r.contains("User-Agent: Seedwell/0.0.1"));
        assert!(!r.to_ascii_lowercase().contains("cookie"));
    }
    assert_eq!(
        s.connections.load(Ordering::SeqCst),
        1,
        "keep-alive: one connection for three announces"
    );
}

/// FIXED audit defect (SECURITY, medium): nothing stops a `.torrent`/magnet tracker URL from
/// pointing at loopback / RFC 1918 / link-local / cloud-metadata addresses with
/// an attacker-chosen path and query. The client then issues a GET with
/// attacker-controlled path+query to LAN devices and local services (a CSRF /
/// SSRF primitive via a public torrent). libtorrent has `ssrf_mitigation` for
/// exactly this (FINDINGS.md: "libtorrent refuses a localhost tracker unless
/// ssrf_mitigation is off"); the HTTP transport has no equivalent.
#[tokio::test]
async fn tracker_url_cannot_target_local_services() {
    let s = serve(|_, _| Act::Raw(resp("200 OK", &[], &ok_body()))).await;
    // path is not "announce": it is a request an attacker wants a local service to see
    let evil = url_for(&s, "/cgi-bin/luci/admin/system/reboot?confirm=1");
    let out = quick_strict().announce(&gated(&evil)).await;
    assert!(
        s.requests.lock().unwrap().is_empty(),
        "the client sent GET {:?} to a loopback service",
        s.requests
            .lock()
            .unwrap()
            .first()
            .map(|r| r.lines().next().unwrap_or("").to_string())
    );
    assert!(
        matches!(out.result.unwrap_err(), AnnounceError::Transport(TransportError::Forbidden(_))),
        "a policy refusal is its own error and provably sent nothing"
    );
    assert_eq!(
        s.connections.load(Ordering::SeqCst),
        0,
        "not even a connect"
    );
}

#[tokio::test]
async fn every_spelling_of_a_local_address_is_refused_before_any_connect() {
    let s = serve(|_, _| Act::Raw(resp("200 OK", &[], &ok_body()))).await;
    let port = s.addr.port();
    let t = quick_strict();
    for host in [
        "127.0.0.1",
        "127.1",
        "2130706433",
        "0x7f.0.0.1",
        "0177.0.0.1",
        "[::1]",
        "[::ffff:127.0.0.1]",
        "[0:0:0:0:0:0:0:1]",
        "localhost",
        "localhost.",
        "169.254.169.254",
        "192.168.1.1",
        "10.0.0.1",
        "0.0.0.0",
    ] {
        let out = t
            .announce(&gated(&format!("http://{host}:{port}/announce")))
            .await;
        let e = out.result.unwrap_err();
        assert!(
            matches!(
                e,
                AnnounceError::Transport(TransportError::Forbidden(_) | TransportError::Dns)
            ),
            "{host}: {e:?}"
        );
        assert!(
            e.never_reached_tracker(),
            "{host}: a refused destination must count as provably undelivered"
        );
    }
    assert!(s.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn lan_opt_in_still_refuses_loopback_and_link_local() {
    let s = serve(|_, _| Act::Raw(resp("200 OK", &[], &ok_body()))).await;
    let t = HttpTransport::with_config(
        "0.0.1",
        HttpConfig {
            policy: DestinationPolicy {
                allow_lan_trackers: true,
                allow_loopback: false,
                allow_public: true,
            },
            ..HttpConfig::default()
        },
    );
    for host in ["127.0.0.1", "169.254.169.254", "[fe80::1]"] {
        let out = t
            .announce(&gated(&format!("http://{host}:{}/announce", s.addr.port())))
            .await;
        assert!(
            matches!(out.result.unwrap_err(), AnnounceError::Transport(TransportError::Forbidden(_))),
            "{host}"
        );
    }
    assert!(s.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_redirect_to_a_forbidden_address_is_not_followed() {
    // The origin is allowed (opted-in loopback); the redirect target is a cloud
    // metadata address with the same "host" check bypassed by being an IP
    // literal. resolve_redirect refuses other hosts already; the policy is the
    // second line of defence on every hop.
    let s = serve(|_, _| {
        Act::Raw(resp(
            "302 Found",
            &[(
                "Location",
                "http://169.254.169.254/latest/meta-data/".into(),
            )],
            b"",
        ))
    })
    .await;
    let out = quick().announce(&gated(&url_for(&s, "/announce"))).await;
    assert_eq!(
        out.result.unwrap_err(),
        AnnounceError::Transport(TransportError::Redirect)
    );
    assert_eq!(s.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn udp_tracker_url_cannot_target_local_services() {
    let (addr, pk) = udp_server(|_, tid| Some(announce_ok(tid, [1, 1, 1, 1, 0x1a, 0xe1]))).await;
    let t = UdpTransport::bind(UdpConfig {
        policy: DestinationPolicy::STRICT,
        ..fast_udp()
    })
    .await
    .unwrap();
    for host in ["127.0.0.1", "169.254.169.254", "192.168.0.1", "2130706433"] {
        let e = t
            .announce(&gated(&format!("udp://{host}:{}/announce", addr.port())))
            .await
            .unwrap_err();
        assert!(
            matches!(e, AnnounceError::Transport(TransportError::Forbidden(_))),
            "{host}"
        );
    }
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        pk.lock().unwrap().is_empty(),
        "no datagram left the process"
    );
}

// ----------------------------------------------------------------- UDP

fn fast_udp() -> UdpConfig {
    UdpConfig {
        policy: DestinationPolicy::LOCAL,
        base_timeout: Duration::from_millis(30),
        max_exponent: 3,
        connection_id_ttl: Duration::from_secs(60),
        total_timeout: Duration::from_secs(5),
        bind: None,
    }
}

/// Minimal BEP 15 server; `on_announce(packet_number, tid) -> Option<reply>`.
async fn udp_server(
    on_announce: impl Fn(usize, &[u8]) -> Option<Vec<u8>> + Send + 'static,
) -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    let pk = Arc::new(Mutex::new(Vec::new()));
    let p = pk.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut announces = 0usize;
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                return;
            };
            let pkt = buf[..n].to_vec();
            p.lock().unwrap().push(pkt.clone());
            let action = u32::from_be_bytes(pkt[8..12].try_into().unwrap());
            let tid = pkt[12..16].to_vec();
            match action {
                0 => {
                    let mut r = vec![0, 0, 0, 0];
                    r.extend(&tid);
                    r.extend(0xAABB_CCDDu64.to_be_bytes());
                    let _ = sock.send_to(&r, from).await;
                }
                1 => {
                    announces += 1;
                    if let Some(r) = on_announce(announces, &tid) {
                        let _ = sock.send_to(&r, from).await;
                    }
                }
                _ => {}
            }
        }
    });
    (addr, pk)
}

fn announce_ok(tid: &[u8], peer: [u8; 6]) -> Vec<u8> {
    let mut r = vec![0, 0, 0, 1];
    r.extend(tid);
    r.extend(1800u32.to_be_bytes());
    r.extend(1u32.to_be_bytes());
    r.extend(2u32.to_be_bytes());
    r.extend(peer);
    r
}

fn announce_count(pk: &Arc<Mutex<Vec<Vec<u8>>>>) -> usize {
    pk.lock()
        .unwrap()
        .iter()
        .filter(|p| u32::from_be_bytes(p[8..12].try_into().unwrap()) == 1)
        .count()
}

#[tokio::test]
async fn udp_reply_flood_with_right_tid_from_the_wrong_source_never_wins() {
    let (addr, _) = udp_server(|_, tid| {
        // the real answer comes late
        Some(announce_ok(tid, [1, 1, 1, 1, 0x1a, 0xe1]))
    })
    .await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let port = t.local_addr_v4().unwrap().port();
    // attacker on 127.0.0.1 but another source port, hammering every tid it can guess
    let evil = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let e2 = evil.clone();
    let flood = tokio::spawn(async move {
        let mut tid = 0u32;
        loop {
            let mut pkt = vec![0, 0, 0, 1];
            pkt.extend(tid.to_be_bytes());
            pkt.extend(1800u32.to_be_bytes());
            pkt.extend([0, 0, 0, 1, 0, 0, 0, 1]);
            pkt.extend([66, 66, 66, 66, 0x1a, 0xe1]);
            let _ = e2.send_to(&pkt, ("127.0.0.1", port)).await;
            let mut c = vec![0, 0, 0, 0];
            c.extend(tid.to_be_bytes());
            c.extend(0xBADu64.to_be_bytes());
            let _ = e2.send_to(&c, ("127.0.0.1", port)).await;
            tid = tid.wrapping_add(1);
            if tid.is_multiple_of(64) {
                tokio::task::yield_now().await;
            }
        }
    });
    let r = t
        .announce(&gated(&format!("udp://127.0.0.1:{}/announce", addr.port())))
        .await
        .unwrap();
    flood.abort();
    assert_eq!(
        r.peers[0].addr,
        "1.1.1.1:6881".parse().unwrap(),
        "a spoofed peer list was accepted"
    );
}

#[tokio::test]
async fn udp_dead_tracker_costs_a_bounded_number_of_datagrams() {
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = silent.local_addr().unwrap().port();
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    tokio::spawn(async move {
        let mut b = [0u8; 512];
        while silent.recv_from(&mut b).await.is_ok() {
            c.fetch_add(1, Ordering::SeqCst);
        }
    });
    let t = UdpTransport::bind(UdpConfig {
        base_timeout: Duration::from_millis(10),
        max_exponent: 4,
        ..fast_udp()
    })
    .await
    .unwrap();
    let e = t
        .announce(&gated(&format!("udp://127.0.0.1:{port}/announce")))
        .await
        .unwrap_err();
    // never got past the connection-id handshake: the announce was not sent
    assert_eq!(e, AnnounceError::Transport(TransportError::ConnectTimeout));
    tokio::time::sleep(Duration::from_millis(50)).await;
    // n = 0..=4 => 5 connect datagrams for the whole announce, nothing else
    assert_eq!(counter.load(Ordering::SeqCst), 5);
}

/// A rejection that merely *mentions* "connection" ("Too many connections from
/// your IP") is the tracker's answer, not a stale connection id: the hidden
/// second connect + announce happens only when a *cached* id was reused.
#[tokio::test]
async fn udp_error_mentioning_connection_on_a_fresh_handshake_is_not_retried() {
    let (addr, pk) = udp_server(|_, tid| {
        let mut r = vec![0, 0, 0, 3];
        r.extend(tid);
        r.extend(b"Too many connections from your IP");
        Some(r)
    })
    .await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let e = t
        .announce(&gated(&format!("udp://127.0.0.1:{}/announce", addr.port())))
        .await
        .unwrap_err();
    assert!(matches!(e, AnnounceError::Rejected { .. }));
    assert_eq!(
        announce_count(&pk),
        1,
        "a fresh connection id cannot be stale: the error is the tracker's answer"
    );
}

#[tokio::test]
async fn udp_unrelated_error_is_not_retried() {
    let (addr, pk) = udp_server(|_, tid| {
        let mut r = vec![0, 0, 0, 3];
        r.extend(tid);
        r.extend(b"Too many connections from your IP");
        Some(r)
    })
    .await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let _ = t
        .announce(&gated(&format!("udp://127.0.0.1:{}/announce", addr.port())))
        .await;
    assert_eq!(announce_count(&pk), 1);
}

#[tokio::test]
async fn udp_truncated_and_oversized_replies_do_not_panic_or_loop() {
    for variant in 0..4 {
        let (addr, pk) = udp_server(move |_, tid| {
            let mut r = announce_ok(tid, [9, 9, 9, 9, 0x1a, 0xe1]);
            match variant {
                0 => r.truncate(19),
                1 => r.truncate(8),
                2 => r.extend(vec![7u8; 8000]), // bigger than the recv buffer
                _ => r[3] = 77,                 // unknown action
            }
            Some(r)
        })
        .await;
        let t = UdpTransport::bind(fast_udp()).await.unwrap();
        let started = Instant::now();
        let r = t
            .announce(&gated(&format!("udp://127.0.0.1:{}/announce", addr.port())))
            .await;
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "variant {variant}"
        );
        if variant == 2 {
            // datagram truncated by the socket buffer: still well-formed prefix
            let _ = r;
        } else {
            assert!(r.is_err(), "variant {variant} parsed as success");
        }
        assert!(
            announce_count(&pk) <= 4,
            "variant {variant}: {} announce datagrams",
            announce_count(&pk)
        );
    }
}

#[tokio::test]
async fn udp_connection_id_cache_is_shared_but_expires() {
    let (addr, pk) = udp_server(|_, tid| Some(announce_ok(tid, [2, 2, 2, 2, 0x1a, 0xe1]))).await;
    let t = UdpTransport::bind(UdpConfig {
        connection_id_ttl: Duration::from_millis(300),
        ..fast_udp()
    })
    .await
    .unwrap();
    let url = format!("udp://127.0.0.1:{}/announce?pk=1", addr.port());
    for _ in 0..5 {
        t.announce(&gated(&url)).await.unwrap();
    }
    let connects = |pk: &Arc<Mutex<Vec<Vec<u8>>>>| {
        pk.lock()
            .unwrap()
            .iter()
            .filter(|p| p[8..12] == [0, 0, 0, 0])
            .count()
    };
    assert_eq!(connects(&pk), 1);
    tokio::time::sleep(Duration::from_millis(350)).await;
    t.announce(&gated(&url)).await.unwrap();
    assert_eq!(connects(&pk), 2);
}

// ------------------------------------------- "was the request written?"

#[tokio::test]
async fn connection_reset_after_the_request_was_read_is_ambiguous() {
    // The server reads the whole request and hangs up without answering: the
    // tracker may well have counted the announce.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = l.accept().await {
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf).await;
            drop(sock);
        }
    });
    let out = quick()
        .announce(&gated(&format!("http://127.0.0.1:{port}/announce")))
        .await;
    let e = out.result.unwrap_err();
    assert!(
        !e.never_reached_tracker(),
        "reset after the request was written must not look undelivered: {e:?}"
    );
}

#[tokio::test]
async fn http_5xx_and_garbage_answers_are_ambiguous() {
    let s = serve(|_, _| Act::Raw(resp("503 Service Unavailable", &[], b"busy"))).await;
    let e = quick()
        .announce(&gated(&url_for(&s, "/announce")))
        .await
        .result
        .unwrap_err();
    assert!(!e.never_reached_tracker(), "{e:?}");
    let s = serve(|_, _| Act::Raw(resp("200 OK", &[], b"<html>"))).await;
    let e = quick()
        .announce(&gated(&url_for(&s, "/announce")))
        .await
        .result
        .unwrap_err();
    assert!(!e.never_reached_tracker(), "{e:?}");
}

#[tokio::test]
async fn http_connect_timeout_is_provably_undelivered() {
    // 192.0.2.1 (TEST-NET-1): nothing answers. Sandboxes may refuse at once
    // instead; either way nothing was written.
    let t = HttpTransport::with_config(
        "0.0.1",
        HttpConfig {
            connect_timeout: Duration::from_millis(300),
            total_timeout: Duration::from_secs(3),
            pool_idle_timeout: Duration::from_secs(5),
            policy: DestinationPolicy::LOCAL,
            ..HttpConfig::default()
        },
    );
    let e = t
        .announce(&gated("http://192.0.2.1:81/announce"))
        .await
        .result
        .unwrap_err();
    assert!(
        matches!(
            e,
            AnnounceError::Transport(TransportError::ConnectTimeout | TransportError::Connect)
        ),
        "{e:?}"
    );
    assert!(e.never_reached_tracker());
}

#[tokio::test]
async fn udp_silence_after_the_announce_was_sent_is_ambiguous() {
    // connect is answered, the announce never is: the tracker may have it
    let (addr, _) = udp_server(|_, _| None).await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let e = t
        .announce(&gated(&format!("udp://127.0.0.1:{}/announce", addr.port())))
        .await
        .unwrap_err();
    assert_eq!(e, AnnounceError::Transport(TransportError::Timeout));
    assert!(!e.never_reached_tracker());
}

#[tokio::test]
async fn udp_transaction_ids_are_unpredictable_and_distinct() {
    let (addr, pk) = udp_server(|_, tid| Some(announce_ok(tid, [2, 2, 2, 2, 0x1a, 0xe1]))).await;
    let url = format!("udp://127.0.0.1:{}/announce", addr.port());
    let mut all: Vec<u32> = Vec::new();
    for _ in 0..3 {
        // a fresh transport each time: a seeded generator keyed on the clock or
        // the port would repeat or correlate
        let t = UdpTransport::bind(fast_udp()).await.unwrap();
        for _ in 0..100 {
            t.announce(&gated(&url)).await.unwrap();
        }
    }
    for p in pk.lock().unwrap().iter() {
        all.push(u32::from_be_bytes(p[12..16].try_into().unwrap()));
    }
    assert!(all.len() >= 300);
    let uniq: std::collections::HashSet<_> = all.iter().collect();
    assert_eq!(uniq.len(), all.len(), "transaction ids repeated");
    // no constant stride (a counter) and well spread over the 32-bit range
    let diffs: std::collections::HashSet<_> =
        all.windows(2).map(|w| w[1].wrapping_sub(w[0])).collect();
    assert!(diffs.len() > all.len() / 2, "ids look sequential");
    let top_nibbles: std::collections::HashSet<_> = all.iter().map(|t| t >> 28).collect();
    assert!(
        top_nibbles.len() >= 12,
        "ids are not spread: {top_nibbles:?}"
    );
}
