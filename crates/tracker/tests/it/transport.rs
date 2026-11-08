//! HTTP and UDP transports against in-process mock trackers on localhost.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use seedwell_tracker::gate::{GateConfig, GatedRequest, HostGate};
use seedwell_tracker::http::{HttpConfig, HttpTransport};
use seedwell_tracker::machine::SendKind;
use seedwell_tracker::policy::DestinationPolicy;
use seedwell_tracker::response::{AnnounceError, TransportError};
use seedwell_tracker::udp::{UdpConfig, UdpTransport};
use seedwell_tracker::*;

/// A loopback port that refuses connections, and nothing else can listen at (as `seedwell_platform::net::closed_tcp_port`,
/// which this crate does not depend on). A port bound to port 0 and let go is in the system's range for ports it hands
/// out (from 32768 on Linux, 49152 on macOS and Windows), so another test of a whole-suite run can be given it and
/// listen there; this one is below those ranges, and checked: it binds, and a connection to it is refused (`std` binds
/// with `SO_REUSEADDR` on macOS and Linux, which lets `127.0.0.1:p` be bound while something listens on `0.0.0.0:p`).
fn closed_port() -> u16 {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().subsec_nanos() as u64 ^ (u64::from(std::process::id()) << 32);
    let closed = |p: u16| {
        let at = SocketAddr::from(([127, 0, 0, 1], p));
        if std::net::TcpListener::bind(at).is_err() {
            return false;
        }
        match std::net::TcpStream::connect_timeout(&at, Duration::from_millis(200)) {
            Ok(_) => false,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => true,
            // Windows says refused only after about 2 s of SYNs; there the bind found any listener already (no
            // `SO_REUSEADDR`).
            Err(e) => cfg!(windows) && e.kind() == std::io::ErrorKind::TimedOut,
        }
    };
    (0..10_000u64)
        .map(|i| 24_000 + ((seed ^ i).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48) as u16 % 8_000)
        .find(|p| closed(*p))
        .expect("a closed port below the systems' ephemeral ranges")
}

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
    let port = closed_port();
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
/// `spoof_first` sends a fake connect reply from another port before each real
/// one. Announces whose connection id is not the latest are answered with the
/// "connection id mismatch" error. A retransmitted connect (the same client and
/// transaction id) gets the id its first copy got: BEP 15 keeps an id valid for
/// two minutes, so the answer to the first copy stays good.
async fn udp_tracker(drop_first_connects: usize, spoof_first: bool) -> UdpTracker {
    udp_tracker_slow(drop_first_connects, spoof_first, Duration::ZERO).await
}

/// [`udp_tracker`], answering each connect only after `connect_delay` (a busy
/// tracker, or a loaded machine).
async fn udp_tracker_slow(drop_first_connects: usize, spoof_first: bool, connect_delay: Duration) -> UdpTracker {
    let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let addr = sock.local_addr().unwrap();
    let packets = Arc::new(Mutex::new(Vec::new()));
    let p = packets.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        let mut dropped = 0;
        let mut conn_counter = 0x1000_0000_0000u64;
        let mut valid: Vec<u64> = Vec::new();
        let mut last_connect: Option<(SocketAddr, [u8; 4], u64)> = None;
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
                    tokio::time::sleep(connect_delay).await;
                    let tid4: [u8; 4] = tid.try_into().unwrap();
                    let conn = match last_connect {
                        Some((a, t, c)) if a == from && t == tid4 => c,
                        _ => {
                            conn_counter += 1;
                            valid.clear();
                            valid.push(conn_counter);
                            last_connect = Some((from, tid4, conn_counter));
                            conn_counter
                        }
                    };
                    let mut r = vec![0, 0, 0, 0];
                    r.extend(tid);
                    r.extend(conn.to_be_bytes());
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
        bind: None,
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
    // It counts packets, so no retransmit may happen: fast_udp()'s 40 ms timer fires on a loaded machine before the
    // tracker's reply is read, and a second announce would be sent.
    let t = UdpTransport::bind(UdpConfig { base_timeout: Duration::from_secs(2), ..fast_udp() }).await.unwrap();
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
    // A loaded machine may answer a copy after the client's 40 ms retransmit: then one copy more goes out, so the
    // exchanges are checked by their shape (connects, then announces), not by an exact packet count.
    let first = actions(&tr);
    let (connects, announces) = connects_then_announces(&first);
    assert!(connects >= 3 && announces >= 1, "connect retransmitted past the two dropped copies, then announced: {first:?}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(t.announce(&gated(&url)).await.is_ok());
    let again = actions(&tr)[first.len()..].to_vec();
    let (connects, announces) = connects_then_announces(&again);
    assert!(connects >= 1 && announces >= 1, "the connection id expired, so we connected again: {again:?}");
}

/// How many connects (action 0) and then announces (action 1) a run of packets holds; panics when they are mixed (an
/// announce before its connect was answered, or a connect after the announce went out).
fn connects_then_announces(actions: &[u32]) -> (usize, usize) {
    let connects = actions.iter().take_while(|&&a| a == 0).count();
    let rest = &actions[connects..];
    assert!(rest.iter().all(|&a| a == 1), "connects, then announces: {actions:?}");
    (connects, rest.len())
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

/// The failure: a connect answered after the client retransmitted it (a slow tracker, a loaded machine) makes the
/// announce fail. The tracker answers both copies of the connect; the client announces with the first answer's id
/// while the second copy is still queued at the tracker. A late answer to the retransmission must not be taken for
/// the announce's reply either. (Seen on a loaded Windows PC in `udp_ignores_spoofed_replies_and_garbage`, whose
/// spoofing tracker binds a socket before each answer: "the spoofed connect reply was ignored" failed at 0.4 s.)
#[tokio::test]
async fn udp_announce_works_when_the_connect_was_answered_after_a_retransmit() {
    for spoof in [false, true] {
        // 100 ms per connect answer against a 40 ms first wait: the connect is always sent twice before its answer.
        let tr = udp_tracker_slow(0, spoof, Duration::from_millis(100)).await;
        let t = UdpTransport::bind(fast_udp()).await.unwrap();
        let url = format!("udp://127.0.0.1:{}/announce", tr.addr.port());
        let r = t.announce(&gated(&url)).await;
        assert!(r.is_ok(), "spoof {spoof}: {r:?}, packets {:?}", actions(&tr));
        assert_eq!(actions(&tr)[..2], [0, 0], "the connect was retransmitted before its answer");
    }
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
    // It counts packets, so no retransmit may happen (a 2 s timer, not fast_udp()'s 40 ms, which fires on a loaded
    // machine).
    let t = UdpTransport::bind(UdpConfig {
        connection_id_ttl: Duration::from_secs(60),
        base_timeout: Duration::from_secs(2),
        ..fast_udp()
    })
    .await
    .unwrap();
    let url = format!("udp://127.0.0.1:{}/announce", tr.addr.port());
    assert!(t.announce(&gated(&url)).await.is_ok());
    // A second client session makes the tracker forget our id (it keeps only the latest).
    let other = UdpTransport::bind(UdpConfig { base_timeout: Duration::from_secs(2), ..fast_udp() }).await.unwrap();
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
    assert!(gate.finish(first.ticket(), seedwell_tracker::gate::Outcome::Success { timing }, Millis(10)));
    gate.begin_quit();
    let stop = AnnounceParams { event: Some(Event::Stopped), numwant: 0, ..p };
    gate.submit((stop.info_hash, 0), RequestId(2), u, stop, SendKind::Event, Millis(20)).unwrap();
    gate.poll(Millis(20)).remove(0)
}

#[tokio::test]
async fn quit_stop_gives_the_answer_two_seconds_after_the_connection_is_up_and_no_more() {
    use seedwell_tracker::http::QuitLimits;
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
    use seedwell_tracker::http::QuitLimits;
    let limits = QuitLimits { connect: Duration::from_secs(3), answer: Duration::from_secs(2) };
    let s = serve(|_, _| Some(http_response("200 OK", &[], &bencode_ok(1800)))).await;
    let out = local_http("1").announce_quit(&gated_stopped(&url_for(&s, "/announce")), limits).await;
    assert!(out.result.is_ok());
    assert_eq!(out.extra_hops, 0);
}

#[tokio::test]
async fn quit_stop_to_a_refusing_host_is_never_reached() {
    use seedwell_tracker::http::QuitLimits;
    let port = closed_port();
    let limits = QuitLimits { connect: Duration::from_secs(3), answer: Duration::from_secs(2) };
    let out = local_http("1").announce_quit(&gated_stopped(&format!("http://127.0.0.1:{port}/announce")), limits).await;
    assert!(out.result.unwrap_err().never_reached_tracker());
}

/// A transport given one address listens there only (an engine bound to loopback stays off the network), and
/// one without listens on every address.
#[tokio::test]
async fn a_bound_transport_listens_on_its_address_only() {
    let t = UdpTransport::bind(UdpConfig { bind: Some(std::net::Ipv4Addr::LOCALHOST), ..fast_udp() }).await.unwrap();
    assert_eq!(t.local_addr_v4().unwrap().ip(), std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    let any = UdpTransport::bind(fast_udp()).await.unwrap();
    assert!(any.local_addr_v4().unwrap().ip().is_unspecified());
}

// ------------------------------------------------------------------ through a SOCKS5 proxy (SPEC §7H)

/// Failure: an announce through the proxy resolves the tracker's name on this computer (a DNS leak) or connects to
/// the tracker directly. The name `tracker.invalid` exists only at the fake proxy, so a local lookup would fail, and
/// the tracker counts one connection, the proxy's.
#[tokio::test]
async fn http_announce_through_the_proxy_sends_the_name_to_the_proxy() {
    let s = serve(|_, _| Some(http_response("200 OK", &[], &bencode_ok(1800)))).await;
    let fake = seedwell_socks::testkit::FakeProxy::start("user", "pass").await;
    fake.map_name("tracker.invalid", s.addr.ip());
    let t = HttpTransport::with_proxy("0.0.1", HttpConfig { policy: DestinationPolicy::LOCAL, ..HttpConfig::default() }, fake.proxy());
    let req = gated(&format!("http://tracker.invalid:{}/announce?passkey=k1", s.addr.port()));
    let out = t.announce(&req).await;
    assert_eq!(out.result.unwrap().peers.len(), 2);
    assert_eq!(fake.connects(), vec![format!("tracker.invalid:{}", s.addr.port())]);
    assert_eq!(s.connections.load(Ordering::SeqCst), 1);
    let got = s.requests.lock().unwrap()[0].clone();
    assert!(got.contains(&format!("Host: tracker.invalid:{}", s.addr.port())), "{got}");
    assert!(!got.to_ascii_lowercase().contains("proxy-authorization"), "SOCKS credentials never reach HTTP: {got}");
}

/// Failure: with the proxy down or the credentials wrong, the announce goes directly to the tracker (no kill
/// switch), or the failure counts against the tracker (its breaker) instead of saying the proxy is the problem.
#[tokio::test]
async fn http_announce_never_goes_direct_when_the_proxy_fails() {
    let s = serve(|_, _| Some(http_response("200 OK", &[], &bencode_ok(1800)))).await;
    let fake = seedwell_socks::testkit::FakeProxy::start("user", "pass").await;
    let mut wrong = fake.proxy();
    wrong.auth = Some(seedwell_socks::Credentials::new("user", "nope").unwrap());
    let cfg = HttpConfig { policy: DestinationPolicy::LOCAL, ..HttpConfig::default() };
    let t = HttpTransport::with_proxy("0.0.1", cfg, wrong);
    let out = t.announce(&gated(&url_for(&s, "/announce"))).await;
    let e = out.result.unwrap_err();
    assert_eq!(e, AnnounceError::Transport(TransportError::Proxy(ProxyBlock::Down)));
    assert!(e.never_reached_tracker() && !e.is_host_failure() && !e.is_unreachable(), "not the tracker's fault, not an outage");
    fake.stop().await;
    let t = HttpTransport::with_proxy("0.0.1", cfg, fake.proxy());
    let out = t.announce(&gated(&url_for(&s, "/announce"))).await;
    assert_eq!(out.result.unwrap_err(), AnnounceError::Transport(TransportError::Proxy(ProxyBlock::Down)));
    assert_eq!(s.connections.load(Ordering::SeqCst), 0, "nothing reached the tracker directly");
    // `localhost` would be the proxy's own: refused like a local lookup of it.
    let t = HttpTransport::with_proxy("0.0.1", HttpConfig::default(), fake.proxy());
    let out = t.announce(&gated("http://localhost:1/announce")).await;
    assert!(matches!(out.result.unwrap_err(), AnnounceError::Transport(TransportError::Forbidden(_))));
}
