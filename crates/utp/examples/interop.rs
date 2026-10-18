//! Minimal BitTorrent-over-uTP peer used by `interop/transmission.sh`.
//!
//! * `download <peer> <info_hash_hex> <file> <piece_len>`: connect over uTP to a seeder, do the BEP 3
//!   handshake, request every block, and compare the bytes with `file`.
//! * `serve <listen> <info_hash_hex> <file> <piece_len> [tracker_tcp_port]`: accept uTP peers and
//!   seed `file`; with a tracker port it also answers HTTP announces by returning itself, so a
//!   Transmission *leecher* will dial us over uTP.
//!
//! This is test tooling, not an engine: no piece verification, no pipelining beyond a fixed depth.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use trs_utp::{Config, UtpSocket};

const BLOCK: u32 = 16 * 1024;
const PSTR: &[u8] = b"\x13BitTorrent protocol";

fn unhex(s: &str) -> [u8; 20] {
    let mut out = [0u8; 20];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).expect("hex info hash");
    }
    out
}

fn handshake(info_hash: &[u8; 20], id_tag: u8) -> Vec<u8> {
    let mut h = Vec::with_capacity(68);
    h.extend_from_slice(PSTR);
    h.extend_from_slice(&[0u8; 8]);
    h.extend_from_slice(info_hash);
    let mut pid = *b"-TRSUTP-interop-0000";
    pid[19] = b'0' + id_tag % 10;
    h.extend_from_slice(&pid);
    h
}

async fn read_msg<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<(u8, Vec<u8>)>> {
    loop {
        let mut len = [0u8; 4];
        r.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 {
            continue; // keep-alive
        }
        if len > 4 * 1024 * 1024 {
            return Err(std::io::Error::other("oversized message"));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        return Ok(Some((body[0], body[1..].to_vec())));
    }
}

async fn send_msg<W: AsyncWrite + Unpin>(w: &mut W, id: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut m = Vec::with_capacity(5 + payload.len());
    m.extend_from_slice(&(1 + payload.len() as u32).to_be_bytes());
    m.push(id);
    m.extend_from_slice(payload);
    w.write_all(&m).await
}

async fn download_utp(peer: SocketAddr, ih: [u8; 20], file: &str, piece_len: u32, hold: u64) {
    let sock = UtpSocket::bind("127.0.0.1:0".parse().unwrap(), Config::default()).await.unwrap();
    let t0 = Instant::now();
    let s = tokio::time::timeout(Duration::from_secs(20), sock.connect(peer))
        .await
        .expect("uTP connect timed out")
        .expect("uTP connect failed");
    println!("uTP connected to {peer} in {:?} (local {})", t0.elapsed(), s.local_addr());
    let sk = sock.clone();
    let s = download(s, "uTP", ih, file, piece_len, move |s| format!("{:?} socket {:?}", s.stats(), sk.stats())).await;
    // Keep the connection open so a script can inspect the peer's view of it.
    tokio::time::sleep(Duration::from_secs(hold)).await;
    drop(s);
}

async fn download_tcp(peer: SocketAddr, ih: [u8; 20], file: &str, piece_len: u32) {
    let s = tokio::net::TcpStream::connect(peer).await.expect("tcp connect");
    download(s, "TCP", ih, file, piece_len, |_| String::new()).await;
}

async fn download<S: AsyncRead + AsyncWrite + Unpin>(
    mut s: S,
    label: &str,
    ih: [u8; 20],
    file: &str,
    piece_len: u32,
    stats: impl Fn(&S) -> String,
) -> S {
    let expect = std::fs::read(file).expect("read expected file");
    let total = expect.len() as u32;
    s.write_all(&handshake(&ih, 1)).await.unwrap();
    let mut hs = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(20), s.read_exact(&mut hs))
        .await
        .expect("handshake reply timed out")
        .expect("handshake read");
    assert_eq!(&hs[..20], PSTR, "peer replied with a BitTorrent handshake");
    assert_eq!(&hs[28..48], &ih, "info hash echoed");
    println!("handshake OK: reserved {:02x?} peer_id {:?}", &hs[20..28], String::from_utf8_lossy(&hs[48..68]));
    send_msg(&mut s, 2, &[]).await.unwrap(); // interested
    let mut out = vec![0u8; total as usize];
    let mut next_block = 0u32; // block index not yet requested
    let blocks = total.div_ceil(BLOCK);
    let mut got = 0u32;
    let mut inflight = 0u32;
    let mut unchoked = false;
    let began = Instant::now();
    let mut started = Instant::now();
    let mut first_piece_at = None;
    let mut ext_seen = false;
    let mut last_print = Instant::now();
    while got < blocks {
        if std::env::var_os("UTP_TRACE").is_some() && last_print.elapsed() > Duration::from_millis(500) {
            last_print = Instant::now();
            println!("  t={:.1}s got {} blocks, inflight {inflight}, {}", started.elapsed().as_secs_f64(), got, stats(&s));
        }
        let (id, p) = tokio::time::timeout(Duration::from_secs(30), read_msg(&mut s))
            .await
            .expect("stalled")
            .unwrap()
            .unwrap();
        match id {
            1 => unchoked = true,
            0 => unchoked = false,
            20 => ext_seen = true,
            7 => {
                if first_piece_at.is_none() {
                    first_piece_at = Some(began.elapsed());
                    started = Instant::now();
                }
                let idx = u32::from_be_bytes(p[0..4].try_into().unwrap());
                let begin = u32::from_be_bytes(p[4..8].try_into().unwrap());
                let data = &p[8..];
                let off = (idx * piece_len + begin) as usize;
                out[off..off + data.len()].copy_from_slice(data);
                got += 1;
                inflight -= 1;
            }
            _ => {}
        }
        while unchoked && inflight < 32 && next_block < blocks {
            let off = next_block * BLOCK;
            let len = BLOCK.min(total - off);
            let mut req = Vec::new();
            req.extend_from_slice(&(off / piece_len).to_be_bytes());
            req.extend_from_slice(&(off % piece_len).to_be_bytes());
            req.extend_from_slice(&len.to_be_bytes());
            send_msg(&mut s, 6, &req).await.unwrap();
            next_block += 1;
            inflight += 1;
        }
    }
    let secs = started.elapsed().as_secs_f64();
    assert!(out == expect, "downloaded bytes differ from the seeded file");
    println!("our connection stats at the end: {}", stats(&s));
    println!(
        "DOWNLOAD OK over {label}: {} bytes byte-exact in {:.2} s = {:.2} MB/s after the first block (peer took {:?} to unchoke us; extension msgs seen: {ext_seen})",
        total,
        secs,
        total as f64 / 1e6 / secs,
        first_piece_at.unwrap_or_default()
    );
    s
}

async fn serve_peer(mut s: trs_utp::UtpStream, ih: [u8; 20], data: Arc<Vec<u8>>, piece_len: u32) {
    let mut hs = [0u8; 68];
    if s.read_exact(&mut hs).await.is_err() || &hs[..20] != PSTR || hs[28..48] != ih {
        println!("rejecting peer: bad handshake");
        return;
    }
    println!("accepted uTP peer {}: handshake OK, reserved {:02x?}", s.peer_addr(), &hs[20..28]);
    s.write_all(&handshake(&ih, 2)).await.unwrap();
    let pieces = (data.len() as u32).div_ceil(piece_len) as usize;
    let mut bf = vec![0u8; pieces.div_ceil(8)];
    for i in 0..pieces {
        bf[i / 8] |= 0x80 >> (i % 8);
    }
    send_msg(&mut s, 5, &bf).await.unwrap();
    let (mut r, mut w) = tokio::io::split(s);
    let mut sent_bytes = 0u64;
    loop {
        match read_msg(&mut r).await {
            Ok(Some((2, _))) => send_msg(&mut w, 1, &[]).await.unwrap(), // interested -> unchoke
            Ok(Some((6, p))) => {
                let idx = u32::from_be_bytes(p[0..4].try_into().unwrap());
                let begin = u32::from_be_bytes(p[4..8].try_into().unwrap());
                let len = u32::from_be_bytes(p[8..12].try_into().unwrap());
                let off = (idx * piece_len + begin) as usize;
                let Some(chunk) = data.get(off..off + len as usize) else { continue };
                let mut pl = Vec::with_capacity(8 + chunk.len());
                pl.extend_from_slice(&idx.to_be_bytes());
                pl.extend_from_slice(&begin.to_be_bytes());
                pl.extend_from_slice(chunk);
                send_msg(&mut w, 7, &pl).await.unwrap();
                sent_bytes += len as u64;
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    println!("peer finished; served {sent_bytes} bytes over uTP");
}

async fn tracker(port: u16, peer: SocketAddr) {
    // `peer` is what we tell clients to dial (Transmission ignores loopback peers from trackers).
    let l = tokio::net::TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    let SocketAddr::V4(v4) = peer else { panic!("IPv4 only") };
    loop {
        let Ok((mut c, _)) = l.accept().await else { continue };
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let _ = c.read(&mut buf).await;
            let mut body = b"d8:completei1e10:incompletei0e8:intervali30e5:peers6:".to_vec();
            body.extend_from_slice(&v4.ip().octets());
            body.extend_from_slice(&v4.port().to_be_bytes());
            body.push(b'e');
            let head = format!("HTTP/1.0 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n", body.len());
            let _ = c.write_all(head.as_bytes()).await;
            let _ = c.write_all(&body).await;
            println!("tracker: answered an announce with {peer}");
        });
    }
}

async fn serve(listen: SocketAddr, ih: [u8; 20], file: &str, piece_len: u32, tracker_port: Option<u16>, advertise: Option<std::net::IpAddr>) {
    let data = Arc::new(std::fs::read(file).expect("read file"));
    let sock = UtpSocket::bind(listen, Config::default()).await.unwrap();
    println!("seeding {} bytes over uTP on {}", data.len(), sock.local_addr());
    if let Some(p) = tracker_port {
        let mut adv = sock.local_addr();
        if let Some(ip) = advertise {
            adv.set_ip(ip);
        }
        tokio::spawn(tracker(p, adv));
    }
    loop {
        let (s, _) = sock.accept().await.unwrap();
        let data = data.clone();
        tokio::spawn(serve_peer(s, ih, data, piece_len));
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    match a.first().map(String::as_str) {
        Some("download-tcp") => {
            download_tcp(a[1].parse().unwrap(), unhex(&a[2]), &a[3], a[4].parse().unwrap()).await;
        }
        Some("download") => {
            download_utp(a[1].parse().unwrap(), unhex(&a[2]), &a[3], a[4].parse().unwrap(), a.get(5).and_then(|h| h.parse().ok()).unwrap_or(0)).await
        }
        Some("serve") => {
            serve(a[1].parse().unwrap(), unhex(&a[2]), &a[3], a[4].parse().unwrap(), a.get(5).and_then(|p| p.parse().ok()), a.get(6).and_then(|p| p.parse().ok()))
                .await
        }
        _ => eprintln!("usage: interop download <peer> <ih> <file> <piece_len> | serve <listen> <ih> <file> <piece_len> [tracker_port [advertise_ip]]"),
    }
}
