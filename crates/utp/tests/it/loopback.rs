//! Real UDP sockets on 127.0.0.1 through the tokio driver.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use trs_utp::{Config, ForeignDatagram, UtpSocket, UtpStream};

fn lo() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}

/// Deterministic stream content addressed by byte position, so MBs can be verified without
/// holding a second copy.
fn fill(seed: u64, offset: u64, buf: &mut [u8]) {
    let mut i = 0;
    while i < buf.len() {
        let pos = offset + i as u64;
        let block = pos / 8;
        let mut z = (block ^ seed).wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        let bytes = z.to_le_bytes();
        let skip = (pos % 8) as usize;
        let n = (8 - skip).min(buf.len() - i);
        buf[i..i + n].copy_from_slice(&bytes[skip..skip + n]);
        i += n;
    }
}

async fn send_stream(mut s: impl tokio::io::AsyncWrite + Unpin, seed: u64, total: u64) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut off = 0;
    while off < total {
        let n = buf.len().min((total - off) as usize);
        fill(seed, off, &mut buf[..n]);
        s.write_all(&buf[..n]).await?;
        off += n as u64;
    }
    Ok(())
}

async fn verify_stream(mut s: impl tokio::io::AsyncRead + Unpin, seed: u64, total: u64) -> std::io::Result<()> {
    let mut buf = vec![0u8; 64 * 1024];
    let mut want = vec![0u8; 64 * 1024];
    let mut off = 0u64;
    loop {
        let n = s.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        fill(seed, off, &mut want[..n]);
        assert!(buf[..n] == want[..n], "corruption at offset {off}");
        off += n as u64;
    }
    assert_eq!(off, total, "short stream");
    Ok(())
}

async fn pair(cfg: Config) -> (UtpSocket, UtpSocket, UtpStream, UtpStream) {
    let a = UtpSocket::bind(lo(), cfg.clone()).await.unwrap();
    let b = UtpSocket::bind(lo(), cfg).await.unwrap();
    let (c, s) = tokio::join!(a.connect(b.local_addr()), b.accept());
    (a, b, c.unwrap(), s.unwrap().0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn echo_and_close_semantics() {
    let (_a, _b, mut c, mut s) = pair(Config::default()).await;
    c.write_all(b"\x13BitTorrent protocol").await.unwrap();
    let mut buf = [0u8; 20];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"\x13BitTorrent protocol");
    s.write_all(b"pong").await.unwrap();
    let mut r = [0u8; 4];
    c.read_exact(&mut r).await.unwrap();
    assert_eq!(&r, b"pong");
    // half-close: c stops writing but can still read
    c.shutdown().await.unwrap();
    let mut rest = Vec::new();
    s.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty());
    s.write_all(b"late reply").await.unwrap();
    drop(s);
    let mut rest = Vec::new();
    c.read_to_end(&mut rest).await.unwrap();
    assert_eq!(rest, b"late reply");
    assert_eq!(c.peer_addr(), _b.local_addr());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_socket_serves_many_concurrent_streams() {
    const N: usize = 200;
    const BYTES: u64 = 256 * 1024;
    // All 200 peers share 127.0.0.1, so the per-IP cap must be raised for this test.
    let cfg = Config { max_per_ip: 512, max_half_open: 512, ..Config::default() };
    let server = UtpSocket::bind(lo(), cfg.clone()).await.unwrap();
    let client = UtpSocket::bind(lo(), cfg).await.unwrap();
    let addr = server.local_addr();
    let srv = tokio::spawn({
        let server = server.clone();
        async move {
            let mut tasks = Vec::new();
            for _ in 0..N {
                let (s, _) = server.accept().await.unwrap();
                tasks.push(tokio::spawn(async move {
                    let (r, mut w) = tokio::io::split(s);
                    // echo-and-verify: receive client data, send our own stream back
                    let (a, b) = tokio::join!(verify_stream(r, 77, BYTES), async {
                        send_stream(&mut w, 88, BYTES).await?;
                        w.shutdown().await
                    });
                    a.unwrap();
                    b.unwrap();
                }));
            }
            for t in tasks {
                t.await.unwrap();
            }
        }
    });
    let start = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..N {
        let client = client.clone();
        tasks.push(tokio::spawn(async move {
            let s = client.connect(addr).await.unwrap();
            let (r, mut w) = tokio::io::split(s);
            let (a, b) = tokio::join!(verify_stream(r, 88, BYTES), async {
                send_stream(&mut w, 77, BYTES).await?;
                w.shutdown().await
            });
            a.unwrap();
            b.unwrap();
        }));
    }
    for t in tasks {
        tokio::time::timeout(Duration::from_secs(120), t).await.expect("timeout").unwrap();
    }
    srv.await.unwrap();
    let secs = start.elapsed().as_secs_f64();
    let (sa, sb) = (server.stats(), client.stats());
    let lost = (sa.datagrams_out + sb.datagrams_out).saturating_sub(sa.datagrams_in + sb.datagrams_in);
    println!("datagrams sent {} received {} (lost in the kernel: {lost})", sa.datagrams_out + sb.datagrams_out, sa.datagrams_in + sb.datagrams_in);
    println!(
        "{N} concurrent streams x {} KiB each way: {:.2} s, {:.1} MB/s aggregate",
        BYTES / 1024,
        secs,
        2.0 * N as f64 * BYTES as f64 / 1e6 / secs
    );
}

async fn bulk(cfg: Config, total: u64) -> f64 {
    let (_a, _b, c, s) = pair(cfg).await;
    let start = Instant::now();
    let tx = tokio::spawn(async move {
        let mut c = c;
        send_stream(&mut c, 5, total).await.unwrap();
        c.shutdown().await.unwrap();
        c
    });
    verify_stream(s, 5, total).await.unwrap();
    let secs = start.elapsed().as_secs_f64();
    let _c = tx.await.unwrap();
    total as f64 / 1e6 / secs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_transfer_is_byte_exact_default_mtu() {
    let mbs = bulk(Config::default(), 128 * 1024 * 1024).await;
    println!("loopback bulk, 1400 B payload: 128 MiB at {mbs:.1} MB/s");
    // A sanity floor, not a benchmark: it must hold on a loaded machine (3.5 MB/s seen at load 8).
    assert!(mbs > 1.0, "{mbs:.1} MB/s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn loopback_grows_the_datagram_past_1400_by_probing() {
    let (_a, _b, c, s) = pair(Config::default()).await;
    let tx = tokio::spawn(async move {
        let mut c = c;
        send_stream(&mut c, 9, 32 * 1024 * 1024).await.unwrap();
        let mtu = c.stats().map(|s| s.path_mtu);
        c.shutdown().await.unwrap();
        (c, mtu)
    });
    verify_stream(s, 9, 32 * 1024 * 1024).await.unwrap();
    let (_c, mtu) = tx.await.unwrap();
    let mtu = mtu.expect("stats");
    println!("loopback settled at a {mtu} B datagram");
    assert!(mtu > 1454 && mtu <= 4000, "{mtu}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn large_transfer_is_byte_exact_jumbo_payload() {
    let cfg = Config { max_payload: 8000, send_buffer: 2 << 20, recv_buffer: 2 << 20, ..Config::default() };
    let mbs = bulk(cfg, 256 * 1024 * 1024).await;
    println!("loopback bulk, 8000 B payload: 256 MiB at {mbs:.1} MB/s");
    // A sanity floor, not a benchmark: it must hold on a loaded machine (3.5 MB/s seen at load 8).
    assert!(mbs > 1.0, "{mbs:.1} MB/s");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shares_the_port_with_dht_style_traffic() {
    let (tx, mut rx) = mpsc::channel::<ForeignDatagram>(64);
    let a = UtpSocket::bind_with_foreign(lo(), Config::default(), tx).await.unwrap();
    let b = UtpSocket::bind(lo(), Config::default()).await.unwrap();
    let (c, s) = tokio::join!(b.connect(a.local_addr()), a.accept());
    let (mut c, mut s) = (c.unwrap(), s.unwrap().0);

    // A DHT-style ping arrives on the same port while a uTP stream is active.
    let raw = tokio::net::UdpSocket::bind(lo()).await.unwrap();
    let ping = b"d1:ad2:id20:abcdefghij0123456789e1:q4:ping1:t2:aa1:y1:qe";
    raw.send_to(ping, a.local_addr()).await.unwrap();
    c.write_all(b"hello over utp").await.unwrap();
    let mut buf = [0u8; 14];
    s.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello over utp");
    let dg = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.unwrap().unwrap();
    assert_eq!(&dg.data[..], ping);
    assert_eq!(dg.from, raw.local_addr().unwrap());
    // The reply goes out through the shared socket, from the shared port.
    a.send_foreign(dg.from, b"d1:rd2:id20:01234567890123456789e1:t2:aa1:y1:re").await.unwrap();
    let mut rbuf = [0u8; 128];
    let (n, from) = tokio::time::timeout(Duration::from_secs(2), raw.recv_from(&mut rbuf)).await.unwrap().unwrap();
    assert_eq!(from, a.local_addr());
    assert_eq!(rbuf[0], b'd');
    assert!(n > 10);
    let st = a.stats();
    assert_eq!(st.foreign_in, 1);
    assert_eq!(st.live_connections, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_failures() {
    // Listener that does not accept: SYN is ignored, connect times out quickly with tuned retries.
    let cfg = Config {
        syn_retries: 1,
        initial_rto: Duration::from_millis(100),
        ..Config::default()
    };
    let deaf = UtpSocket::bind(lo(), Config { accept_incoming: false, ..Config::default() }).await.unwrap();
    let c = UtpSocket::bind(lo(), cfg).await.unwrap();
    let start = Instant::now();
    let err = c.connect(deaf.local_addr()).await.err().expect("must fail");
    assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    assert!(start.elapsed() < Duration::from_secs(3));
    // After the failure nothing lingers
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(c.stats().live_connections, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_and_drop_propagate() {
    let (_a, _b, c, mut s) = pair(Config::default()).await;
    c.reset();
    let mut buf = [0u8; 8];
    let e = tokio::time::timeout(Duration::from_secs(3), s.read(&mut buf)).await.unwrap().unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset);
    drop(c);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_stream_flushes_buffered_data_then_fin() {
    let (_a, _b, mut c, mut s) = pair(Config::default()).await;
    let data = vec![0xA5u8; 300_000];
    c.write_all(&data).await.unwrap();
    drop(c); // graceful: data first, then FIN
    let mut got = Vec::new();
    s.read_to_end(&mut got).await.unwrap();
    assert_eq!(got.len(), data.len());
    assert!(got == data);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_task_exits_when_all_handles_are_gone() {
    let a = UtpSocket::bind(lo(), Config::default()).await.unwrap();
    let b = UtpSocket::bind(lo(), Config::default()).await.unwrap();
    let (c, s) = tokio::join!(a.connect(b.local_addr()), b.accept());
    drop((c.unwrap(), s.unwrap()));
    let probe = a.clone();
    drop(a);
    drop(b);
    // lingering connections finish, then the task and socket are released
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(probe.stats().live_connections <= 1);
}

/// The readiness API the engine's peer loop uses (`readable` / `try_read` / `writable` /
/// `try_write`), the same shape as `TcpStream`'s: `WouldBlock` when idle, ready on data and on EOF.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_api_behaves_like_a_tcp_stream() {
    let server = UtpSocket::bind(lo(), Config::default()).await.unwrap();
    let client = UtpSocket::bind(lo(), Config::default()).await.unwrap();
    let saddr = server.local_addr();
    let (c, s) = tokio::join!(client.connect(saddr), server.accept());
    let c = c.unwrap();
    let (s, _) = s.unwrap();
    let mut buf = [0u8; 64];
    // Nothing yet: not readable, a read says WouldBlock; there is room to write.
    assert!(tokio::time::timeout(Duration::from_millis(100), s.readable()).await.is_err());
    assert_eq!(s.try_read(&mut buf).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    tokio::time::timeout(Duration::from_secs(1), c.writable()).await.unwrap().unwrap();
    assert_eq!(c.try_write(b"hello").unwrap(), 5);
    tokio::time::timeout(Duration::from_secs(5), s.readable()).await.unwrap().unwrap();
    assert_eq!(s.try_read(&mut buf).unwrap(), 5);
    assert_eq!(&buf[..5], b"hello");
    // The peer closes: readable again, and the next read is EOF (`Ok(0)`).
    drop(c);
    tokio::time::timeout(Duration::from_secs(5), s.readable()).await.unwrap().unwrap();
    assert_eq!(s.try_read(&mut buf).unwrap(), 0);
    // A full send buffer says `WouldBlock`, and `writable` completes once the peer has read.
    let (c2, s2) = tokio::join!(client.connect(saddr), server.accept());
    let (c2, (s2, _)) = (c2.unwrap(), s2.unwrap());
    let chunk = vec![7u8; 64 * 1024];
    let mut queued = 0usize;
    loop {
        match c2.try_write(&chunk) {
            Ok(n) => queued += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("{e}"),
        }
        assert!(queued < 64 << 20, "the send buffer must be bounded");
    }
    assert_eq!(c2.try_write(&chunk).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    let mut sink = vec![0u8; 256 * 1024];
    let mut got = 0;
    while got < queued {
        s2.readable().await.unwrap();
        got += s2.try_read(&mut sink).unwrap();
    }
    tokio::time::timeout(Duration::from_secs(5), c2.writable()).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_peer_on_one_of_our_own_addresses_counts_as_this_host() {
    // The benchmark harness dials the machine's LAN address, which the kernel routes over lo0.
    let Some(lan) = lan_address() else { return };
    let a = UtpSocket::bind("0.0.0.0:0".parse().unwrap(), Config::default()).await.unwrap();
    let b = UtpSocket::bind("0.0.0.0:0".parse().unwrap(), Config::default()).await.unwrap();
    let to = SocketAddr::new(lan, b.local_addr().port());
    let (c, s) = tokio::join!(a.connect(to), b.accept());
    let (c, s) = (c.unwrap(), s.unwrap().0);
    let tx = tokio::spawn(async move {
        let mut c = c;
        send_stream(&mut c, 3, 16 * 1024 * 1024).await.unwrap();
        let mtu = c.stats().map(|s| s.path_mtu);
        c.shutdown().await.unwrap();
        (c, mtu)
    });
    verify_stream(s, 3, 16 * 1024 * 1024).await.unwrap();
    let (_c, mtu) = tx.await.unwrap();
    assert_eq!(mtu, Some(4000), "own-address peers are probed like loopback");
}

/// This machine's outbound IPv4 address (what a UDP socket connected to the Internet would use),
/// if it has one; `None` when offline.
fn lan_address() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}
