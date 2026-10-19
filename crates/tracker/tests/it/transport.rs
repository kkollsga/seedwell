//! HTTP and UDP transports against in-process mock trackers on localhost.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use trs_tracker::gate::{GateConfig, GatedRequest, HostGate};
use trs_tracker::http::{HttpConfig, HttpTransport};
use trs_tracker::machine::SendKind;
use trs_tracker::policy::DestinationPolicy;
use trs_tracker::response::{AnnounceError, TransportError};
use trs_tracker::udp::{UdpConfig, UdpTransport};
use trs_tracker::*;

/// A transport that may talk to the loopback mock servers of this file (the
/// default policy refuses loopback: SSRF protection).
fn local_http(version: &str) -> HttpTransport {
    HttpTransport::with_config(
        version,
        HttpConfig {
            policy: DestinationPolicy::LOCAL,
            ..HttpConfig::default()
        },
    )
}

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

/// The only way to get a request: through a gate.
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

fn bencode_ok(interval: u32) -> Vec<u8> {
    let mut b = format!("d8:intervali{interval}e5:peers12:").into_bytes();
    b.extend([10, 0, 0, 1, 0x1A, 0xE1, 10, 0, 0, 2, 0x1A, 0xE2]);
    b.push(b'e');
    b
}

fn http_response(status: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut r = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len()).into_bytes();
    for (k, v) in headers {
        r.extend(format!("{k}: {v}\r\n").as_bytes());
    }
    r.extend(b"\r\n");
    r.extend(body);
    r
}

/// Serve requests on one listener; `handler(request_text, request_number)`
/// returns the raw response bytes (or None to hang up). Counts connections.
struct Server {
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<String>>>,
}

async fn serve(handler: impl Fn(&str, usize) -> Option<Vec<u8>> + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(handler);
    let (c, r) = (connections.clone(), requests.clone());
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
                    // read one request (GET, no body)
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
                        Some(resp) => {
                            if sock.write_all(&resp).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    }
                }
            });
        }
    });
    Server {
        addr,
        connections,
        requests,
    }
}

fn url_for(s: &Server, path: &str) -> String {
    format!("http://127.0.0.1:{}{}", s.addr.port(), path)
}

#[tokio::test]
async fn http_announce_parses_and_sends_exact_request() {
    let s = serve(|_, _| Some(http_response("200 OK", &[], &bencode_ok(1800)))).await;
    let t = local_http("0.0.1");
    let req = gated(&url_for(&s, "/PASSKEY/announce?passkey=k1"));
    let out = t.announce(&req).await;
    let resp = out.result.unwrap();
    assert_eq!(resp.interval, Duration::from_secs(1800));
    assert_eq!(resp.peers.len(), 2);
    assert_eq!(out.extra_hops, 0);
    let got = s.requests.lock().unwrap()[0].clone();
    let line = got.lines().next().unwrap();
    // Transmission-style order, no event-less extras, no dropped params.
    assert_eq!(
        line,
        "GET /PASSKEY/announce?passkey=k1&info_hash=%00%01abcdefghijklmnopq%FF&peer_id=-SX0001-abcdefghijkl&port=6881&uploaded=1&downloaded=2&left=3&numwant=50&key=DEADBEEF&compact=1&event=started HTTP/1.1"
    );
    // Exact header set and order (Transmission 4.1.3 capture): no `Connection`
    // header, so the connection is kept alive.
    let headers: Vec<&str> = got
        .split("\r\n")
        .skip(1)
        .take_while(|l| !l.is_empty())
        .collect();
    assert_eq!(
        headers,
        [
            format!("Host: 127.0.0.1:{}", s.addr.port()).as_str(),
            "User-Agent: Seedwell/0.0.1",
            "Accept: */*",
            "Accept-Encoding: gzip",
        ],
        "{got}"
    );
}

#[tokio::test]
async fn http_trace_reports_the_address_the_status_and_the_headers_that_are_sent() {
    let s = serve(|_, _| Some(http_response("200 OK", &[], &bencode_ok(1800)))).await;
    let t = local_http("0.0.1");
    let u = url_for(&s, "/announce?passkey=k1");
    let req = gated(&u);
    let out = t.announce(&req).await;
    assert!(out.result.is_ok());
    assert_eq!(out.trace.addr, Some(std::net::IpAddr::from([127, 0, 0, 1])));
    assert_eq!(out.trace.status, Some(200));
    assert!(!out.trace.fallback);
    // the headers the developer log shows are the ones on the wire
    let got = s.requests.lock().unwrap()[0].clone();
    let sent: Vec<&str> = got.split("\r\n").skip(1).take_while(|l| !l.is_empty()).collect();
    assert_eq!(t.request_headers(&u), sent);
    // an error status keeps its code in the trace
    let s = serve(|_, _| Some(http_response("503 Service Unavailable", &[], b""))).await;
    let out = t.announce(&gated(&url_for(&s, "/announce"))).await;
    assert_eq!(out.trace.status, Some(503));
}

#[tokio::test]
async fn http_gzip_body() {
    use flate2::{Compression, write::GzEncoder};
    use std::io::Write;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(&bencode_ok(900)).unwrap();
    let gz = enc.finish().unwrap();
    let s = serve(move |_, _| {
        Some(http_response(
            "200 OK",
            &[("Content-Encoding", "gzip".into())],
            &gz,
        ))
    })
    .await;
    let out = local_http("1")
        .announce(&gated(&url_for(&s, "/announce")))
        .await;
    assert_eq!(out.result.unwrap().interval, Duration::from_secs(900));
}

#[tokio::test]
async fn http_follows_same_host_redirect_and_reports_the_extra_hop() {
    let s = serve(|req, n| {
        if n == 1 {
            assert!(req.starts_with("GET /announce?"));
            Some(http_response(
                "302 Found",
                &[("Location", "/moved/announce?x=1".into())],
                b"",
            ))
        } else {
            assert!(req.starts_with("GET /moved/announce?x=1 "), "{req}");
            Some(http_response("200 OK", &[], &bencode_ok(1800)))
        }
    })
    .await;
    let out = local_http("1")
        .announce(&gated(&url_for(&s, "/announce")))
        .await;
    assert!(out.result.is_ok(), "{:?}", out.result);
    assert_eq!(out.extra_hops, 1);
}

#[tokio::test]
async fn http_refuses_cross_host_redirects_and_loops() {
    let s = serve(|_, _| {
        Some(http_response(
            "301 Moved",
            &[("Location", "http://evil.example/announce".into())],
            b"",
        ))
    })
    .await;
    let out = local_http("1")
        .announce(&gated(&url_for(&s, "/announce")))
        .await;
    assert_eq!(
        out.result.unwrap_err(),
        AnnounceError::Transport(TransportError::Redirect)
    );
    assert_eq!(
        s.requests.lock().unwrap().len(),
        1,
        "the other host was never contacted"
    );

    let s = serve(|_, n| {
        Some(http_response(
            "302 Found",
            &[("Location", format!("/loop{n}"))],
            b"",
        ))
    })
    .await;
    let out = local_http("1")
        .announce(&gated(&url_for(&s, "/announce")))
        .await;
    assert_eq!(
        out.result.unwrap_err(),
        AnnounceError::Transport(TransportError::Redirect)
    );
    assert_eq!(out.extra_hops, 3);
    assert_eq!(
        s.requests.lock().unwrap().len(),
        4,
        "original + 3 redirects, no more"
    );
}

#[tokio::test]
async fn http_error_statuses() {
    let s = serve(|_, n| match n {
        1 => Some(http_response(
            "503 Service Unavailable",
            &[("Retry-After", "120".into())],
            b"busy",
        )),
        2 => Some(http_response(
            "429 Too Many Requests",
            &[("Retry-After", "Sun, 06 Nov 2095 08:49:37 GMT".into())],
            b"",
        )),
        3 => Some(http_response(
            "400 Bad Request",
            &[],
            b"d14:failure reason11:bad requeste",
        )),
        4 => Some(http_response("200 OK", &[], b"<html>captive portal</html>")),
        5 => Some(http_response(
            "200 OK",
            &[],
            b"d14:failure reason20:Unregistered torrente",
        )),
        _ => Some(http_response("404 Not Found", &[], b"")),
    })
    .await;
    let t = local_http("1");
    let u = url_for(&s, "/announce");
    let r = t.announce(&gated(&u)).await.result.unwrap_err();
    assert_eq!(
        r,
        AnnounceError::Http {
            status: 503,
            retry_after: Some(Duration::from_secs(120))
        }
    );
    let r = t.announce(&gated(&u)).await.result.unwrap_err();
    match r {
        AnnounceError::Http {
            status: 429,
            retry_after: Some(d),
        } => assert!(d > Duration::from_secs(86_400 * 365), "{d:?}"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        t.announce(&gated(&u)).await.result.unwrap_err(),
        AnnounceError::Rejected { .. }
    ));
    assert!(matches!(
        t.announce(&gated(&u)).await.result.unwrap_err(),
        AnnounceError::Garbage(_)
    ));
    match t.announce(&gated(&u)).await.result.unwrap_err() {
        AnnounceError::Rejected { reason, .. } => assert_eq!(reason, "Unregistered torrent"),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        t.announce(&gated(&u)).await.result.unwrap_err(),
        AnnounceError::Http { status: 404, .. }
    ));
}

#[tokio::test]
async fn http_keep_alive_reuses_one_connection() {
    let s = serve(|_, _| Some(http_response("200 OK", &[], &bencode_ok(1800)))).await;
    let t = local_http("1");
    for _ in 0..4 {
        assert!(
            t.announce(&gated(&url_for(&s, "/announce")))
                .await
                .result
                .is_ok()
        );
    }
    assert_eq!(
        s.connections.load(Ordering::SeqCst),
        1,
        "one pooled connection per host"
    );
    assert_eq!(s.requests.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn http_connect_refused_and_total_timeout_and_oversize() {
    // refused
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let out = local_http("1")
        .announce(&gated(&format!("http://127.0.0.1:{port}/announce")))
        .await;
    let e = out.result.unwrap_err();
    assert_eq!(e, AnnounceError::Transport(TransportError::Connect));
    assert!(e.never_reached_tracker(), "refused: nothing was written");

    // accepts, never answers: total timeout
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((s, _)) = l.accept().await {
            held.push(s);
        }
    });
    let t = HttpTransport::with_config(
        "1",
        HttpConfig {
            policy: DestinationPolicy::LOCAL,
            total_timeout: Duration::from_millis(300),
            ..HttpConfig::default()
        },
    );
    let started = std::time::Instant::now();
    let out = t
        .announce(&gated(&format!("http://127.0.0.1:{port}/announce")))
        .await;
    let e = out.result.unwrap_err();
    assert_eq!(e, AnnounceError::Transport(TransportError::Timeout));
    assert!(
        !e.never_reached_tracker(),
        "a timeout after connecting is ambiguous: the request may have been read"
    );
    assert!(started.elapsed() < Duration::from_secs(5));

    // oversize body
    let big = vec![b'x'; 2 * 1024 * 1024];
    let s = serve(move |_, _| Some(http_response("200 OK", &[], &big))).await;
    let out = local_http("1")
        .announce(&gated(&url_for(&s, "/announce")))
        .await;
    assert!(
        matches!(
            out.result.unwrap_err(),
            AnnounceError::Transport(TransportError::TooLarge)
        ),
        "bounded memory"
    );
}

#[tokio::test]
async fn https_to_a_non_tls_server_fails_closed() {
    // A plaintext server on an https URL: the handshake must fail, nothing is
    // downgraded to plain HTTP, and no request is ever written.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::<u8>::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = l.accept().await {
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await;
            let mut b = [0u8; 1024];
            if let Ok(n) = sock.read(&mut b).await {
                s2.lock().unwrap().extend(&b[..n]);
            }
        }
    });
    let t = HttpTransport::with_config(
        "1",
        HttpConfig {
            policy: DestinationPolicy::LOCAL,
            total_timeout: Duration::from_secs(3),
            ..HttpConfig::default()
        },
    );
    let out = t
        .announce(&gated(&format!("https://127.0.0.1:{port}/announce")))
        .await;
    let err = out.result.unwrap_err();
    assert!(
        matches!(
            err,
            AnnounceError::Transport(
                TransportError::Tls | TransportError::Io | TransportError::Connect
            )
        ),
        "{err:?}"
    );
    assert!(
        err.never_reached_tracker(),
        "a failed handshake means the request was never written: {err:?}"
    );
    assert!(
        !seen.lock().unwrap().windows(4).any(|w| w == b"GET "),
        "no plaintext request was sent over the failed handshake"
    );
}

// ------------------------------------------------------------------- UDP

struct UdpTracker {
    addr: SocketAddr,
    packets: Arc<Mutex<Vec<Vec<u8>>>>,
}

/// BEP 15 mock: `drop_first_connects` ignores that many connect requests;
/// `error_on_announce_with_old_conn` answers announces whose connection id is
/// not the latest with the "connection id mismatch" error.
async fn udp_tracker(drop_first_connects: usize, spoof_first: bool) -> UdpTracker {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    let packets = Arc::new(Mutex::new(Vec::new()));
    let p = packets.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut dropped = 0;
        let mut conn_counter = 0x1000_0000_0000u64;
        let mut valid: Vec<u64> = Vec::new();
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf).await else {
                return;
            };
            let pkt = buf[..n].to_vec();
            p.lock().unwrap().push(pkt.clone());
            let action = u32::from_be_bytes(pkt[8..12].try_into().unwrap());
            let tid = &pkt[12..16];
            match action {
                0 => {
                    if dropped < drop_first_connects {
                        dropped += 1;
                        continue;
                    }
                    conn_counter += 1;
                    valid.clear();
                    valid.push(conn_counter);
                    let mut r = vec![0, 0, 0, 0];
                    r.extend(tid);
                    r.extend(conn_counter.to_be_bytes());
                    if spoof_first {
                        let evil = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                        let mut fake = r.clone();
                        fake[8..16].copy_from_slice(&0xBAD0_BAD0u64.to_be_bytes());
                        let _ = evil.send_to(&fake, from).await;
                    }
                    let _ = sock.send_to(&r, from).await;
                }
                1 => {
                    let conn = u64::from_be_bytes(pkt[0..8].try_into().unwrap());
                    if !valid.contains(&conn) {
                        let mut r = vec![0, 0, 0, 3];
                        r.extend(tid);
                        r.extend(b"connection id mismatch");
                        let _ = sock.send_to(&r, from).await;
                        continue;
                    }
                    let mut r = vec![0, 0, 0, 1];
                    r.extend(tid);
                    r.extend(1800u32.to_be_bytes());
                    r.extend(3u32.to_be_bytes());
                    r.extend(8u32.to_be_bytes());
                    r.extend([10, 0, 0, 9, 0x1A, 0xE1]);
                    let _ = sock.send_to(&r, from).await;
                }
                _ => {}
            }
        }
    });
    UdpTracker { addr, packets }
}

fn fast_udp() -> UdpConfig {
    UdpConfig {
        policy: DestinationPolicy::LOCAL,
        base_timeout: Duration::from_millis(40),
        max_exponent: 3,
        connection_id_ttl: Duration::from_millis(400),
        total_timeout: Duration::from_secs(5),
    }
}

fn actions(t: &UdpTracker) -> Vec<u32> {
    t.packets
        .lock()
        .unwrap()
        .iter()
        .map(|p| u32::from_be_bytes(p[8..12].try_into().unwrap()))
        .collect()
}

#[tokio::test]
async fn udp_announce_reuses_the_connection_id_and_sends_bep41_data() {
    let tr = udp_tracker(0, false).await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let url = format!("udp://127.0.0.1:{}/announce?passkey=XYZ", tr.addr.port());
    let r1 = t.announce(&gated(&url)).await.unwrap();
    assert_eq!(r1.interval, Duration::from_secs(1800));
    assert_eq!((r1.complete, r1.incomplete), (Some(8), Some(3)));
    assert_eq!(r1.peers[0].addr, "10.0.0.9:6881".parse().unwrap());
    let _ = t.announce(&gated(&url)).await.unwrap();
    assert_eq!(
        actions(&tr),
        vec![0, 1, 1],
        "one connect for two announces within 60 s"
    );
    let pkts = tr.packets.lock().unwrap();
    let a = &pkts[1];
    assert_eq!(&a[16..36], &params().info_hash);
    assert_eq!(a[83], 2, "event started");
    assert_eq!(&a[98..100], &[0x2, 21], "BEP 41 URLData option");
    assert_eq!(&a[100..121], b"/announce?passkey=XYZ");
}

#[tokio::test]
async fn udp_trace_reports_the_address_and_whether_the_connection_id_was_cached() {
    let tr = udp_tracker(0, false).await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let url = format!("udp://127.0.0.1:{}/announce", tr.addr.port());
    let (r1, t1) = t.announce_traced(&gated(&url)).await;
    assert!(r1.is_ok());
    assert_eq!(t1.addr, Some(tr.addr));
    assert!(!t1.connection_reused && !t1.fallback);
    assert_ne!(t1.transaction_id, 0);
    let (_, t2) = t.announce_traced(&gated(&url)).await;
    assert!(t2.connection_reused, "the second announce reuses the connection id");
    assert_eq!(t2.connection_id, t1.connection_id);
}

#[tokio::test]
async fn udp_retransmits_and_expires_connection_ids() {
    let tr = udp_tracker(2, false).await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let url = format!("udp://127.0.0.1:{}/announce", tr.addr.port());
    assert!(t.announce(&gated(&url)).await.is_ok());
    assert_eq!(
        actions(&tr),
        vec![0, 0, 0, 1],
        "connect retransmitted twice, same exchange"
    );
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(t.announce(&gated(&url)).await.is_ok());
    assert_eq!(
        actions(&tr)[4..],
        [0, 1],
        "the connection id expired, so we connected again"
    );
}

#[tokio::test]
async fn udp_ignores_spoofed_replies_and_garbage() {
    let tr = udp_tracker(0, true).await;
    let t = UdpTransport::bind(fast_udp()).await.unwrap();
    let url = format!("udp://127.0.0.1:{}/announce", tr.addr.port());
    assert!(
        t.announce(&gated(&url)).await.is_ok(),
        "the spoofed connect reply from another port was ignored"
    );
    // random junk straight at our socket does nothing
    let port = t.local_addr_v4().unwrap().port();
    let junk = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    for pkt in [
        vec![],
        vec![0; 3],
        vec![0, 0, 0, 9, 1, 2, 3, 4],
        vec![0, 0, 0, 1, 0, 0, 0, 1],
    ] {
        junk.send_to(&pkt, ("127.0.0.1", port)).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(t.announce(&gated(&url)).await.is_ok());
}

#[tokio::test]
async fn udp_times_out_and_maps_errors() {
    // silent tracker
    let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = silent.local_addr().unwrap().port();
    let cfg = UdpConfig {
        base_timeout: Duration::from_millis(10),
        max_exponent: 2,
        ..fast_udp()
    };
    let t = UdpTransport::bind(cfg).await.unwrap();
    let started = std::time::Instant::now();
    let e = t
        .announce(&gated(&format!("udp://127.0.0.1:{port}/announce")))
        .await
        .unwrap_err();
    // The connection-id handshake never completed: the announce was never
    // sent (provably undelivered), so this is a connect timeout.
    assert_eq!(e, AnnounceError::Transport(TransportError::ConnectTimeout));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "10 + 20 + 40 ms"
    );
    drop(silent);

    // error packet => rejection
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        while let Ok((_, from)) = sock.recv_from(&mut buf).await {
            let mut r = vec![0, 0, 0, 3];
            r.extend(&buf[12..16]);
            r.extend(b"unregistered torrent");
            let _ = sock.send_to(&r, from).await;
        }
    });
    let e = t
        .announce(&gated(&format!("udp://127.0.0.1:{}/announce", addr.port())))
        .await
        .unwrap_err();
    assert_eq!(
        e,
        AnnounceError::Rejected {
            reason: "unregistered torrent".into(),
            retry_in: None
        }
    );
}

#[tokio::test]
async fn udp_stale_connection_id_error_reconnects_once() {
    let tr = udp_tracker(0, false).await;
    let t = UdpTransport::bind(UdpConfig {
        connection_id_ttl: Duration::from_secs(60),
        ..fast_udp()
    })
    .await
    .unwrap();
    let url = format!("udp://127.0.0.1:{}/announce", tr.addr.port());
    assert!(t.announce(&gated(&url)).await.is_ok());
    // A second client session makes the tracker forget our id (it keeps only the latest).
    let other = UdpTransport::bind(fast_udp()).await.unwrap();
    assert!(other.announce(&gated(&url)).await.is_ok());
    assert!(
        t.announce(&gated(&url)).await.is_ok(),
        "mismatch error => fresh connect => success"
    );
    assert_eq!(actions(&tr), vec![0, 1, 0, 1, 1, 0, 1]);
}

/// A released `stopped`, as a quit produces it (after a `started` that was answered).
fn gated_stopped(url: &str) -> GatedRequest {
    let mut gate = HostGate::new(GateConfig { initial_tokens: 3, ..GateConfig::default() });
    let u = Arc::new(TrackerUrl::parse(url).unwrap());
    let p = params();
    gate.register_torrent(p.info_hash, vec![u.clone()], &[], Millis(0));
    gate.submit((p.info_hash, 0), RequestId(1), u.clone(), p.clone(), SendKind::Event, Millis(0)).unwrap();
    let first = gate.poll(Millis(0)).remove(0);
    let timing = clamp_timing(Some(1800), None);
    assert!(gate.finish(first.ticket(), trs_tracker::gate::Outcome::Success { timing }, Millis(10)));
    gate.begin_quit();
    let stop = AnnounceParams { event: Some(Event::Stopped), numwant: 0, ..p };
    gate.submit((stop.info_hash, 0), RequestId(2), u, stop, SendKind::Event, Millis(20)).unwrap();
    gate.poll(Millis(20)).remove(0)
}

#[tokio::test]
async fn quit_stop_gives_the_answer_two_seconds_after_the_connection_is_up_and_no_more() {
    use trs_tracker::http::QuitLimits;
    let limits = QuitLimits { connect: Duration::from_millis(1500), answer: Duration::from_millis(400) };
    // Accepts, reads the request, never answers.
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut sock, _)) = l.accept().await {
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            seen2.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_string());
            held.push(sock);
        }
    });
    let t0 = std::time::Instant::now();
    let out = local_http("1").announce_quit(&gated_stopped(&format!("http://127.0.0.1:{port}/announce")), limits).await;
    let took = t0.elapsed();
    assert_eq!(out.result.unwrap_err(), AnnounceError::Transport(TransportError::Timeout));
    assert!(took >= Duration::from_millis(380) && took < Duration::from_millis(1400), "answer wait ~400 ms, took {took:?}");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "the stopped did reach the tracker");
    assert!(seen[0].contains("event=stopped"));
}

#[tokio::test]
async fn quit_stop_over_a_fast_tracker_completes_and_reuses_nothing_special() {
    use trs_tracker::http::QuitLimits;
    let limits = QuitLimits { connect: Duration::from_secs(3), answer: Duration::from_secs(2) };
    let s = serve(|_, _| Some(http_response("200 OK", &[], &bencode_ok(1800)))).await;
    let out = local_http("1").announce_quit(&gated_stopped(&url_for(&s, "/announce")), limits).await;
    assert!(out.result.is_ok());
    assert_eq!(out.extra_hops, 0);
}

#[tokio::test]
async fn quit_stop_to_a_refusing_host_is_never_reached() {
    use trs_tracker::http::QuitLimits;
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let limits = QuitLimits { connect: Duration::from_secs(3), answer: Duration::from_secs(2) };
    let out = local_http("1").announce_quit(&gated_stopped(&format!("http://127.0.0.1:{port}/announce")), limits).await;
    assert!(out.result.unwrap_err().never_reached_tracker());
}
