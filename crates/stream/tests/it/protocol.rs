//! HTTP behaviour against a fake swarm: ranges, errors, keep-alive, disconnects, seek storms,
//! stalls, > 4 GB offsets, several streams.

use crate::common;

use common::client::*;
use common::fake::FakeSource;
use common::*;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use trs_stream::{Config, EndReason, Event, FileRange, Priority, StreamServer};

fn file(name: &str, offset: u64, len: u64) -> FileRange {
    FileRange { index: 0, name: name.into(), offset, len }
}

fn ends(ev: &Events) -> Vec<(u64, EndReason, u64)> {
    ev.lock().unwrap().iter().filter_map(|e| if let Event::End { req, reason, sent, .. } = e { Some((*req, *reason, *sent)) } else { None }).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn range_forms_and_headers() {
    let (srv, _ev) = server().await;
    let len = 5 * MIB + 123;
    let src = FakeSource::synthetic(len, 256 * 1024, 300 * MIB);
    let h = srv.open(src.clone(), file("clip.mp4", 0, len), 1).unwrap();
    assert!(h.url.starts_with(&format!("http://127.0.0.1:{}/{}/clip.mp4", srv.addr().port(), h.token)));
    assert_eq!(h.token.len(), 32);
    assert!(h.is_mp4_family);
    let path = path_of(&h.url);
    let a = srv.addr();

    // Whole file.
    let r = get(a, &path, &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.h("content-length"), Some(len.to_string().as_str()));
    assert_eq!(r.h("accept-ranges"), Some("bytes"));
    assert_eq!(r.h("content-type"), Some("video/mp4"));
    assert!(r.h("content-range").is_none());
    assert_eq!(r.body, src.expected(0, len as usize));

    // Open-ended.
    let r = get(a, &path, &[("Range", "bytes=1000-")]).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.h("content-range"), Some(format!("bytes 1000-{}/{len}", len - 1).as_str()));
    assert_eq!(r.h("content-length"), Some((len - 1000).to_string().as_str()));
    assert_eq!(r.body, src.expected(1000, (len - 1000) as usize));

    // Closed range, clamped end.
    let r = get(a, &path, &[("Range", "bytes=10-19")]).await;
    assert_eq!((r.status, r.body.len()), (206, 10));
    assert_eq!(r.h("content-range"), Some(format!("bytes 10-19/{len}").as_str()));
    assert_eq!(r.body, src.expected(10, 10));
    let r = get(a, &path, &[("Range", "bytes=5242880-99999999999")]).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.body.len() as u64, len - 5 * MIB);

    // Suffix.
    let r = get(a, &path, &[("Range", "bytes=-100")]).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.h("content-range"), Some(format!("bytes {}-{}/{len}", len - 100, len - 1).as_str()));
    assert_eq!(r.body, src.expected(len - 100, 100));
    let r = get(a, &path, &[("Range", "bytes=-999999999")]).await;
    assert_eq!((r.status, r.body.len() as u64), (206, len));

    // AVFoundation probe: exact headers.
    let r = get(a, &path, &[("Range", "bytes=0-1")]).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.h("content-range"), Some(format!("bytes 0-1/{len}").as_str()));
    assert_eq!(r.h("content-length"), Some("2"));
    assert_eq!(r.body, src.expected(0, 2));

    // Past the end.
    for spec in [format!("bytes={len}-"), format!("bytes={}-{}", len + 5, len + 50), "bytes=-0".to_string()] {
        let r = get(a, &path, &[("Range", &spec)]).await;
        assert_eq!(r.status, 416, "{spec}");
        assert_eq!(r.h("content-range"), Some(format!("bytes */{len}").as_str()));
        assert_eq!(r.h("content-length"), Some("0"));
    }

    // Several ranges: the first is served, as a single-part 206.
    let r = get(a, &path, &[("Range", "bytes=100-199,300-399,500-")]).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.h("content-range"), Some(format!("bytes 100-199/{len}").as_str()));
    assert_eq!(r.body, src.expected(100, 100));
    assert!(r.h("content-type").unwrap().starts_with("video/"));

    // Garbage Range headers are ignored (200).
    for spec in ["bytes=9-2", "items=0-5", "bytes=x-"] {
        let r = get(a, &path, &[("Range", spec)]).await;
        assert_eq!((r.status, r.body.len() as u64), (200, len), "{spec}");
    }

    // HEAD: same headers, no body.
    let r = head(a, &path, &[]).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.h("content-length"), Some(len.to_string().as_str()));
    assert_eq!(r.h("accept-ranges"), Some("bytes"));
    let r = head(a, &path, &[("Range", "bytes=-10")]).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.h("content-length"), Some("10"));
    assert_eq!(r.h("content-range"), Some(format!("bytes {}-{}/{len}", len - 10, len - 1).as_str()));

    // If-Range: matching validator keeps the range, a stale one yields the full file.
    let etag = r.h("etag").unwrap().to_string();
    let r = get(a, &path, &[("Range", "bytes=0-9"), ("If-Range", &etag)]).await;
    assert_eq!(r.status, 206);
    let r = get(a, &path, &[("Range", "bytes=0-9"), ("If-Range", "\"stale\"")]).await;
    assert_eq!((r.status, r.body.len() as u64), (200, len));

    assert_eq!(src.unverified_reads(), 0);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn errors_wrong_token_methods_host() {
    let (srv, _ev) = server().await;
    let src = FakeSource::synthetic(MIB, 64 * 1024, 100 * MIB);
    let h = srv.open(src, file("a.mkv", 0, MIB), 1).unwrap();
    let a = srv.addr();
    let path = path_of(&h.url);

    assert_eq!(get(a, "/00000000000000000000000000000000/a.mkv", &[]).await.status, 404);
    assert_eq!(get(a, &format!("/{}x/a.mkv", h.token), &[]).await.status, 404);
    assert_eq!(get(a, "/", &[]).await.status, 404);
    assert_eq!(get(a, "/a.mkv", &[]).await.status, 404);
    // The name is cosmetic: only the token authorises.
    assert_eq!(get(a, &format!("/{}/whatever", h.token), &[("Range", "bytes=0-0")]).await.status, 206);
    assert_eq!(get(a, &format!("{path}?x=1"), &[("Range", "bytes=0-0")]).await.status, 206);

    let mut c = Client::connect(a).await;
    c.send("POST", &path, &[("Content-Length", "0")]).await;
    let r = c.read_response(true).await.unwrap();
    assert_eq!(r.status, 405);
    assert_eq!(r.h("allow"), Some("GET, HEAD"));

    // Foreign Host header (DNS rebinding): refused even with the right token.
    let mut c = Client::connect(a).await;
    c.sock.write_all(format!("GET {path} HTTP/1.1\r\nHost: evil.example.com\r\n\r\n").as_bytes()).await.unwrap();
    assert_eq!(c.read_response(true).await.unwrap().status, 403);

    // Garbage and oversize heads.
    let mut c = Client::connect(a).await;
    c.sock.write_all(b"NOT HTTP\r\n\r\n").await.unwrap();
    assert_eq!(c.read_response(true).await.unwrap().status, 400);
    let mut c = Client::connect(a).await;
    c.sock.write_all(&vec![b'a'; 20_000]).await.unwrap();
    assert_eq!(c.read_response(true).await.unwrap().status, 431);

    // Close: token stops working.
    assert!(srv.close(&h.token));
    assert!(!srv.close(&h.token));
    assert_eq!(get(a, &path, &[]).await.status, 404);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_loopback_refused() {
    // bind() on a non-loopback address is rejected outright.
    let bad = StreamServer::start(Config { bind: "0.0.0.0:0".parse().unwrap(), ..Config::default() }).await;
    assert!(bad.is_err());
    assert!(trs_stream::peer_allowed(&"127.0.0.1:1".parse().unwrap()));
    assert!(trs_stream::peer_allowed(&"[::1]:1".parse().unwrap()));
    assert!(trs_stream::peer_allowed(&"[::ffff:127.0.0.1]:1".parse().unwrap()));
    assert!(!trs_stream::peer_allowed(&"192.168.1.10:1".parse().unwrap()));
    assert!(!trs_stream::peer_allowed(&"[2001:db8::1]:1".parse().unwrap()));

    // End to end: serve on the machine's LAN address and connect to it from there.
    let lan = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.connect("192.0.2.1:9")?;
        s.local_addr()
    });
    let Ok(lan) = lan else { return eprintln!("no LAN address: skipping") };
    if lan.ip().is_loopback() || lan.ip().is_unspecified() {
        return eprintln!("no LAN address: skipping");
    }
    let l = tokio::net::TcpListener::bind((lan.ip(), 0)).await.unwrap();
    let addr = l.local_addr().unwrap();
    let srv = StreamServer::start_with_listener(Config::default(), l).await.unwrap();
    let src = FakeSource::synthetic(MIB, 64 * 1024, 100 * MIB);
    let h = srv.open(src, file("a.mkv", 0, MIB), 1).unwrap();
    let token_path = format!("/{}/a.mkv", h.token);
    let mut c = Client::connect(addr).await;
    c.send("GET", &token_path, &[("Range", "bytes=0-9")]).await;
    assert!(c.read_head().await.is_none(), "a non-loopback peer must get no response");
    assert_eq!(srv.stats().connections, 0);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keep_alive_and_pipelining() {
    let (srv, _ev) = server().await;
    let len = 2 * MIB;
    let src = FakeSource::synthetic(len, 128 * 1024, 200 * MIB);
    let h = srv.open(src.clone(), file("a.mp4", 0, len), 1).unwrap();
    let path = path_of(&h.url);
    let mut c = Client::connect(srv.addr()).await;
    for i in 0..5u64 {
        c.send("GET", &path, &[("Range", &format!("bytes={}-{}", i * 1000, i * 1000 + 99))]).await;
        let r = c.read_response(false).await.unwrap();
        assert_eq!(r.status, 206);
        assert_eq!(r.h("connection"), Some("keep-alive"));
        assert_eq!(r.body, src.expected(i * 1000, 100));
        c.send("HEAD", &path, &[]).await;
        assert_eq!(c.read_response(true).await.unwrap().status, 200);
    }
    c.send("GET", &path, &[("Range", "bytes=99999999-")]).await;
    assert_eq!(c.read_response(true).await.unwrap().status, 416);
    // Two requests in one write.
    let mut both = req_bytes("GET", &path, &[("Range", "bytes=0-9")]);
    both.extend(req_bytes("GET", &path, &[("Range", "bytes=10-19")]));
    c.sock.write_all(&both).await.unwrap();
    assert_eq!(c.read_response(false).await.unwrap().body, src.expected(0, 10));
    assert_eq!(c.read_response(false).await.unwrap().body, src.expected(10, 10));
    // Connection: close is honoured.
    c.send("GET", &path, &[("Range", "bytes=0-9"), ("Connection", "close")]).await;
    let r = c.read_response(false).await.unwrap();
    assert_eq!(r.h("connection"), Some("close"));
    assert!(c.closed_within(Duration::from_secs(2)).await);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bytes_are_gated_by_verification() {
    let (srv, ev) = server().await;
    let len = 6 * MIB;
    let src = FakeSource::synthetic(len, 512 * 1024, 3 * MIB);
    let h = srv.open(src.clone(), file("a.bin", 0, len), 1).unwrap();
    let path = path_of(&h.url);
    let t0 = Instant::now();
    let mut c = Client::connect(srv.addr()).await;
    c.send("GET", &path, &[]).await;
    let head = c.read_head().await.unwrap();
    assert_eq!(head.status, 200);
    let first = c.read_body(1000).await;
    let t_first = t0.elapsed();
    assert_eq!(first, src.expected(0, 1000));
    // At 3 MB/s the first 512 KiB piece is there after ~0.2 s, the whole file after ~2 s.
    assert!(t_first < Duration::from_millis(900), "first byte {t_first:?}");
    let rest = c.read_body(len as usize - 1000).await;
    let total = t0.elapsed();
    assert_eq!(rest, src.expected(1000, len as usize - 1000));
    assert!(total > Duration::from_millis(1500), "whole file in {total:?}: data cannot arrive faster than the swarm");
    assert_eq!(src.unverified_reads(), 0);
    // Every Chunk event was within data that was verified when sent: read count equals chunk count.
    let chunks = ev.lock().unwrap().iter().filter(|e| matches!(e, Event::Chunk { .. })).count();
    assert!(chunks >= 6 * 4, "{chunks} chunks");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn client_disconnect_drops_urgency_at_once() {
    let (srv, ev) = server().await;
    let len = 64 * MIB;
    let src = FakeSource::synthetic(len, MIB as u32, 2 * MIB);
    src.prefill(0..2 * MIB);
    src.prefill(len - 2 * MIB..len);
    let h = srv.open(src.clone(), file("a.bin", 0, len), 1).unwrap();
    let path = path_of(&h.url);
    assert!(eventually(2.0, || src.active_ids().is_empty()).await, "index hints go away once head and tail are present");

    // Case 1: parked waiting for a piece, the client leaves before any byte.
    let mut c = Client::connect(srv.addr()).await;
    c.send("GET", &path, &[("Range", "bytes=20971520-")]).await;
    let _ = c.read_head().await.unwrap();
    assert!(eventually(2.0, || !src.active_ids().is_empty()).await);
    let hints = src.hints_of(h.id);
    assert_eq!(hints[0].priority, Priority::Urgent);
    assert_eq!(hints[0].pieces.0, 20);
    assert_eq!(hints[0].deadline, Duration::ZERO);
    let t = Instant::now();
    drop(c);
    assert!(eventually(1.0, || src.active_ids().is_empty()).await, "urgency still set after hang-up");
    assert!(t.elapsed() < Duration::from_millis(500));
    assert!(eventually(1.0, || srv.stats().cursors == 0 && srv.stats().connections == 0).await);

    // Case 2: mid-body.
    let mut c = Client::connect(srv.addr()).await;
    c.send("GET", &path, &[("Range", "bytes=30408704-")]).await;
    let _ = c.read_head().await.unwrap();
    let got = c.read_body(300_000).await;
    assert_eq!(got.len(), 300_000);
    drop(c);
    assert!(eventually(1.0, || src.active_ids().is_empty() && srv.stats().cursors == 0).await);
    let e = ends(&ev);
    assert!(e.iter().filter(|x| x.1 == EndReason::ClientGone).count() >= 2, "{e:?}");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_piece_ends_response_cleanly_after_timeout() {
    let (srv, ev) = server_with(Config { stall_timeout: Duration::from_millis(400), ..Config::default() }).await;
    let len = 4 * MIB;
    let src = FakeSource::synthetic(len, 256 * 1024, 200 * MIB);
    src.stall(8);
    let h = srv.open(src.clone(), file("a.bin", 0, len), 1).unwrap();
    let path = path_of(&h.url);
    let mut c = Client::connect(srv.addr()).await;
    let t = Instant::now();
    c.send("GET", &path, &[]).await;
    let head = c.read_head().await.unwrap();
    assert_eq!(head.h("content-length"), Some(len.to_string().as_str()));
    let body = c.read_body(len as usize).await;
    let took = t.elapsed();
    assert_eq!(body.len() as u64, 8 * 256 * 1024, "everything before the stalled piece, nothing after");
    assert_eq!(body, src.expected(0, body.len()));
    assert!(took >= Duration::from_millis(350) && took < Duration::from_secs(3), "{took:?}");
    assert!(c.closed_within(Duration::from_secs(1)).await, "connection closed after the cut-off");
    assert!(ends(&ev).iter().any(|e| e.1 == EndReason::Stalled && e.2 == body.len() as u64));
    // The server is fine afterwards, and the piece unblocks a new request.
    let r = get(srv.addr(), &path, &[("Range", "bytes=0-99")]).await;
    assert_eq!(r.status, 206);
    src.unstall(8);
    let r = get(srv.addr(), &path, &[("Range", "bytes=2097152-2097251")]).await;
    assert_eq!(r.body, src.expected(2 * MIB, 100));
    assert_eq!(src.unverified_reads(), 0);
    assert!(eventually(2.0, || srv.stats().cursors == 0).await);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_but_progressing_piece_does_not_time_out() {
    // 1 MiB pieces at 1 MB/s take ~1 s each; the stall limit is 300 ms, but the partial piece grows.
    let (srv, ev) = server_with(Config { stall_timeout: Duration::from_millis(300), ..Config::default() }).await;
    let len = 3 * MIB;
    let src = FakeSource::synthetic(len, MIB as u32, MIB);
    let h = srv.open(src.clone(), file("a.bin", 0, len), 1).unwrap();
    let r = get(srv.addr(), &path_of(&h.url), &[]).await;
    assert_eq!(r.body.len() as u64, len);
    assert!(ends(&ev).iter().all(|e| e.1 != EndReason::Stalled));
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn over_4gb_offsets() {
    let (srv, _ev) = server().await;
    let total = 6 * 1024 * MIB + 3 * MIB;
    let f = file("big.bin", 5 * MIB, 5 * 1024 * MIB + 512 * MIB);
    let src = FakeSource::synthetic(total, 4 * MIB as u32, 600 * MIB);
    let h = srv.open(src.clone(), f.clone(), 1).unwrap();
    assert_eq!(h.file_len, f.len);
    let path = path_of(&h.url);
    let a = srv.addr();

    let start = 4_294_967_290u64; // 2^32 - 6
    let r = get(a, &path, &[("Range", &format!("bytes={start}-{}", start + 99))]).await;
    assert_eq!(r.status, 206);
    assert_eq!(r.h("content-range"), Some(format!("bytes {start}-{}/{}", start + 99, f.len).as_str()));
    assert_eq!(r.body, src.expected(f.offset + start, 100));

    let r = get(a, &path, &[("Range", "bytes=-1000")]).await;
    assert_eq!(r.h("content-range"), Some(format!("bytes {}-{}/{}", f.len - 1000, f.len - 1, f.len).as_str()));
    assert_eq!(r.body, src.expected(f.offset + f.len - 1000, 1000));

    let r = get(a, &path, &[("Range", &format!("bytes={}-", f.len))]).await;
    assert_eq!(r.status, 416);
    assert_eq!(r.h("content-range"), Some(format!("bytes */{}", f.len).as_str()));

    let r = head(a, &path, &[]).await;
    assert_eq!(r.h("content-length"), Some(f.len.to_string().as_str()));
    // A read across the 5 GiB mark inside the file.
    let start = (5u64 << 30) - 50;
    let r = get(a, &path, &[("Range", &format!("bytes={start}-{}", start + 99))]).await;
    assert_eq!(r.body, src.expected(f.offset + start, 100));
    assert_eq!(src.unverified_reads(), 0);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 6)]
async fn two_streams_share_the_swarm_fairly() {
    let (srv, ev) = server().await;
    let each = 12 * MIB;
    let src = FakeSource::synthetic(2 * each, MIB as u32, 8 * MIB);
    let a = srv.open(src.clone(), file("ep1.mkv", 0, each), 7).unwrap();
    let b = srv.open(src.clone(), file("ep2.mkv", each, each), 7).unwrap();
    assert_ne!(a.token, b.token);
    assert_ne!(a.id, b.id);
    let addr = srv.addr();
    let t0 = Instant::now();
    let mut tasks = Vec::new();
    for h in [&a, &b] {
        let p = path_of(&h.url);
        let s = src.clone();
        let base = if h.token == a.token { 0 } else { each };
        tasks.push(tokio::spawn(async move {
            let r = get(addr, &p, &[]).await;
            assert_eq!(r.body, s.expected(base, each as usize));
            t0.elapsed()
        }));
    }
    let ta = tasks.remove(0).await.unwrap();
    let tb = tasks.remove(0).await.unwrap();
    let (lo, hi) = (ta.min(tb), ta.max(tb));
    assert!(hi > Duration::from_millis(2200), "24 MiB at 8 MB/s needs ~3 s, got {hi:?}");
    assert!(lo.as_secs_f64() > 0.7 * hi.as_secs_f64(), "unfair: {ta:?} vs {tb:?}");
    assert_eq!(ends(&ev).iter().filter(|e| e.1 == EndReason::Complete).count(), 2);

    // The same file in a second player: its own token and cursors; closing one leaves the other.
    let a2 = srv.open(src.clone(), file("ep1.mkv", 0, each), 7).unwrap();
    assert_ne!(a.token, a2.token);
    assert!(srv.close(&a.token));
    assert_eq!(get(addr, &path_of(&a.url), &[]).await.status, 404);
    assert_eq!(get(addr, &path_of(&a2.url), &[("Range", "bytes=0-9")]).await.status, 206);
    assert_eq!(get(addr, &path_of(&b.url), &[("Range", "bytes=0-9")]).await.status, 206);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removal_closes_streams_mid_play() {
    let (srv, ev) = server().await;
    let len = 32 * MIB;
    let src = FakeSource::synthetic(len, MIB as u32, MIB / 2);
    let s1 = srv.open(src.clone(), file("a.mkv", 0, len), 1).unwrap();
    let s2 = srv.open(src.clone(), file("a.mkv", 0, len), 1).unwrap();
    let other = FakeSource::synthetic(len, MIB as u32, MIB / 2);
    let s3 = srv.open(other.clone(), file("b.mkv", 0, len), 2).unwrap();
    let mut c1 = Client::connect(srv.addr()).await;
    c1.send("GET", &path_of(&s1.url), &[("Range", "bytes=10485760-")]).await;
    let _ = c1.read_head().await.unwrap();
    let mut c2 = Client::connect(srv.addr()).await;
    c2.send("GET", &path_of(&s3.url), &[("Range", "bytes=10485760-")]).await;
    let _ = c2.read_head().await.unwrap();
    assert!(eventually(2.0, || !src.active_ids().is_empty()).await);
    assert_eq!(srv.close_owner(1), 2);
    assert_eq!(srv.stats().streams, 1);
    assert!(c1.closed_within(Duration::from_secs(2)).await, "response of a removed torrent ends");
    assert!(eventually(1.0, || src.active_ids().is_empty()).await);
    assert!(ends(&ev).iter().any(|e| e.1 == EndReason::Closed));
    assert_eq!(get(srv.addr(), &path_of(&s2.url), &[]).await.status, 404);
    // Owner 2 is untouched.
    assert!(!other.active_ids().is_empty());
    // The engine dropping a source (torrent gone) also ends the response.
    other.kill();
    assert!(c2.closed_within(Duration::from_secs(2)).await);
    assert!(eventually(1.0, || ends(&ev).iter().any(|e| e.1 == EndReason::SourceGone)).await);
    srv.shutdown().await;
    assert!(src.active_ids().is_empty());
}

/// AVFoundation / QuickTime pattern: a `bytes=0-1` probe, then many small ranged requests, several
/// in parallel, on keep-alive connections.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn avfoundation_small_parallel_ranges_coalesce() {
    let cfg = Config { linger: Duration::from_millis(400), ..Config::default() };
    let (srv, _ev) = server_with(cfg).await;
    let len = 200 * MIB;
    let src = FakeSource::synthetic(len, MIB as u32, 600 * MIB);
    src.prefill(0..2 * MIB);
    src.prefill(len - 2 * MIB..len);
    let h = srv.open(src.clone(), file("movie.mp4", 0, len), 1).unwrap();
    let path = path_of(&h.url);
    let addr = srv.addr();

    let r = get(addr, &path, &[("Range", "bytes=0-1")]).await;
    assert_eq!((r.status, r.body.len()), (206, 2));
    assert_eq!(r.h("content-range"), Some(format!("bytes 0-1/{len}").as_str()));

    // Let the probe's linger lapse so the batch is judged on its own.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let base = 50 * MIB;
    let chunk = 128 * 1024u64;
    let mut conns = Vec::new();
    for _ in 0..4 {
        conns.push(Client::connect(addr).await);
    }
    let log_start = src.hint_log().len();
    for round in 0..8u64 {
        let mut tasks = Vec::new();
        for (i, mut c) in conns.drain(..).enumerate() {
            let off = base + (round * 4 + i as u64) * chunk;
            let path = path.clone();
            let src = src.clone();
            tasks.push(tokio::spawn(async move {
                c.send("GET", &path, &[("Range", &format!("bytes={off}-{}", off + chunk - 1))]).await;
                let r = c.read_response(false).await.unwrap();
                assert_eq!(r.status, 206);
                assert_eq!(r.h("content-length"), Some(chunk.to_string().as_str()));
                assert_eq!(r.body, src.expected(off, chunk as usize));
                c
            }));
        }
        for t in tasks {
            conns.push(t.await.unwrap());
        }
        // The picture is one window: no piece twice, near the front, never a jump back to 0.
        let hints = srv.last_urgency(&h.token).unwrap();
        let mut seen = std::collections::HashSet::new();
        for r in &hints {
            for p in r.range() {
                assert!(seen.insert(p), "piece {p} in two ranges: {hints:?}");
            }
        }
        assert!(hints.len() <= 6, "at most one window per live or lingering request plus the read-ahead: {hints:?}");
        assert!(hints.iter().any(|r| r.priority == Priority::Urgent && r.pieces.0 >= 49 && r.pieces.0 <= 58), "{hints:?}");
    }
    // No tiny request wiped the hints in between: every Set after the probe starts at the playhead area.
    for e in &src.hint_log()[log_start..] {
        if let common::fake::HintEvent::Set(_, rs) = e {
            let first_urgent = rs.iter().find(|r| r.priority == Priority::Urgent).unwrap();
            assert!(first_urgent.pieces.0 >= 49, "{rs:?}");
        }
    }
    // 32 requests produced a handful of updates, not one per request per advance.
    assert!(src.hint_log().len() - log_start < 32 * 4, "{}", src.hint_log().len() - log_start);
    // Once the requests stop and the linger passes, the stream asks for nothing.
    assert!(eventually(3.0, || src.active_ids().is_empty()).await, "{:?} {:?}", srv.stats(), srv.last_urgency(&h.token));
    srv.shutdown().await;
}

/// A player that stops reading (paused, or seeked away on a connection it keeps open) stops
/// claiming urgency after `stale_after`; reading again brings it back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_cursor_goes_stale_then_recovers() {
    let cfg = Config { stale_after: Duration::from_millis(300), ..Config::default() };
    let (srv, _ev) = server_with(cfg).await;
    let len = 512 * MIB;
    let src = FakeSource::synthetic(len, MIB as u32, 2000 * MIB);
    src.prefill(0..2 * MIB);
    src.prefill(len - 2 * MIB..len);
    let h = srv.open(src.clone(), file("a.bin", 0, len), 1).unwrap();
    let mut c = Client::connect(srv.addr()).await;
    c.send("GET", &path_of(&h.url), &[("Range", "bytes=104857600-")]).await;
    let _ = c.read_head().await.unwrap();
    // Don't read. The kernel buffers fill, the server's write blocks, the window lapses.
    assert!(eventually(5.0, || src.active_ids().is_empty() && srv.stats().cursors == 1).await, "still urgent: {:?}", src.active_ids());
    // Read again: the window comes back at the new position.
    let got = c.read_body(8 * MIB as usize).await;
    assert_eq!(got.len(), 8 * MIB as usize);
    assert!(eventually(2.0, || !src.active_ids().is_empty()).await);
    let hints = src.hints_of(h.id);
    assert!(hints[0].pieces.0 >= 100, "{hints:?}");
    drop(c);
    assert!(eventually(2.0, || srv.stats().cursors == 0 && src.active_ids().is_empty()).await);
    srv.shutdown().await;
}
