//! Hand-written wire peers that count the payload bytes crossing their own socket, so a test can
//! compare what an engine announced with what really went over the wire (SPEC 7B.2).
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::TestTorrent;

const BLOCK: u32 = 16384;

/// The torrent's payload as one stream (files in order).
pub fn stream_of(t: &TestTorrent) -> Arc<Vec<u8>> {
    Arc::new(t.files.iter().flat_map(|f| f.1.iter().copied()).collect())
}

pub fn piece_size(t: &TestTorrent, total: u64, piece: u32) -> u32 {
    let start = u64::from(piece) * u64::from(t.piece_len);
    (total - start).min(u64::from(t.piece_len)) as u32
}

/// A seeder that serves every piece, and serves the pieces in `bad` once as garbage.
pub struct ScriptedSeeder {
    pub addr: SocketAddr,
    /// Correct piece payload written to the socket.
    pub good: Arc<AtomicU64>,
    /// Garbage piece payload written to the socket.
    pub bad: Arc<AtomicU64>,
    pub connections: Arc<AtomicU64>,
}

/// `block_delay` paces the transfer so a test can act while it runs.
pub async fn scripted_seeder(t: &TestTorrent, bad: &[u32], block_delay: Duration) -> ScriptedSeeder {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let stream = stream_of(t);
    let total = stream.len() as u64;
    let (good, badc, conns) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    // Garbage still owed per bad piece (bytes), shared by all connections.
    let owed: Arc<std::sync::Mutex<std::collections::HashMap<u32, u64>>> = Arc::new(std::sync::Mutex::new(
        bad.iter().map(|&p| (p, u64::from(piece_size(t, total, p)))).collect(),
    ));
    let (ih, pieces, plen) = (t.id, t.pieces, t.piece_len);
    let (g2, b2, c2) = (good.clone(), badc.clone(), conns.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            c2.fetch_add(1, Ordering::Relaxed);
            let (stream, owed, good, badc) = (stream.clone(), owed.clone(), g2.clone(), b2.clone());
            tokio::spawn(async move {
                let mut hs = [0u8; 68];
                if s.read_exact(&mut hs).await.is_err() {
                    return;
                }
                let mut out = Vec::new();
                out.extend(b"\x13BitTorrent protocol");
                out.extend([0u8; 8]);
                out.extend(ih);
                out.extend(b"-SC0001-scriptseeder");
                let nbytes = (pieces as usize).div_ceil(8);
                let mut bf = vec![0xFFu8; nbytes];
                let spare = nbytes * 8 - pieces as usize;
                if spare > 0 {
                    bf[nbytes - 1] <<= spare;
                }
                out.extend(((nbytes + 1) as u32).to_be_bytes());
                out.push(5);
                out.extend(bf);
                if s.write_all(&out).await.is_err() {
                    return;
                }
                loop {
                    let mut len = [0u8; 4];
                    if s.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let n = u32::from_be_bytes(len) as usize;
                    let mut body = vec![0u8; n];
                    if n > 0 && s.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    match body.first() {
                        Some(2) => {
                            if s.write_all(&[0, 0, 0, 1, 1]).await.is_err() {
                                return;
                            }
                        }
                        Some(6) if n == 13 => {
                            let index = u32::from_be_bytes([body[1], body[2], body[3], body[4]]);
                            let begin = u32::from_be_bytes([body[5], body[6], body[7], body[8]]);
                            let length = u32::from_be_bytes([body[9], body[10], body[11], body[12]]);
                            let at = u64::from(index) * u64::from(plen) + u64::from(begin);
                            let Some(data) = stream.get(at as usize..(at + u64::from(length)) as usize) else { return };
                            let mut payload = data.to_vec();
                            let garbage = {
                                let mut o = owed.lock().unwrap();
                                match o.get_mut(&index) {
                                    Some(left) if *left > 0 => {
                                        *left = left.saturating_sub(u64::from(length));
                                        true
                                    }
                                    _ => false,
                                }
                            };
                            if garbage {
                                payload.iter_mut().for_each(|b| *b ^= 0xA5);
                            }
                            let mut m = Vec::with_capacity(13 + payload.len());
                            m.extend((9 + length).to_be_bytes());
                            m.push(7);
                            m.extend(index.to_be_bytes());
                            m.extend(begin.to_be_bytes());
                            m.extend(&payload);
                            if !block_delay.is_zero() {
                                tokio::time::sleep(block_delay).await;
                            }
                            if s.write_all(&m).await.is_err() {
                                return;
                            }
                            if garbage { &badc } else { &good }.fetch_add(u64::from(length), Ordering::Relaxed);
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    ScriptedSeeder { addr, good, bad: badc, connections: conns }
}

/// A leecher that downloads the blocks of `pieces` from an engine, one request at a time, and counts
/// the piece payload it reads from its socket. The connection stays open until it is dropped.
pub struct Leech {
    /// Piece payload read from the socket.
    pub payload: u64,
    /// Everything else read (headers, bitfield, have, unchoke ...).
    pub overhead: u64,
    pub stream: TcpStream,
}

pub async fn leech(addr: SocketAddr, t: &TestTorrent, pieces: std::ops::Range<u32>) -> Leech {
    let data = stream_of(t);
    let total = data.len() as u64;
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = Vec::new();
    out.extend(b"\x13BitTorrent protocol");
    out.extend([0u8; 8]);
    out.extend(t.id);
    out.extend(b"-SL0001-scriptleechr");
    out.extend([0, 0, 0, 1, 2]);
    s.write_all(&out).await.unwrap();
    let mut hs = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut hs)).await.unwrap().unwrap();
    let mut res = Leech { payload: 0, overhead: 68, stream: s };
    let mut unchoked = false;
    let mut todo: std::collections::VecDeque<(u32, u32, u32)> = pieces
        .flat_map(|p| {
            let size = piece_size(t, total, p);
            (0..size.div_ceil(BLOCK)).map(move |b| (p, b * BLOCK, BLOCK.min(size - b * BLOCK)))
        })
        .collect();
    let mut inflight: Option<(u32, u32, u32)> = None;
    while !(todo.is_empty() && inflight.is_none()) {
        if unchoked && inflight.is_none() {
            let (p, b, l) = todo.pop_front().unwrap();
            let mut r = vec![0, 0, 0, 13, 6];
            r.extend(p.to_be_bytes());
            r.extend(b.to_be_bytes());
            r.extend(l.to_be_bytes());
            res.stream.write_all(&r).await.unwrap();
            inflight = Some((p, b, l));
        }
        let mut len = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(15), res.stream.read_exact(&mut len)).await.expect("leech: timeout").unwrap();
        let n = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; n];
        if n > 0 {
            res.stream.read_exact(&mut body).await.unwrap();
        }
        match body.first() {
            Some(1) => {
                unchoked = true;
                res.overhead += 4 + n as u64;
            }
            Some(7) => {
                let (p, b) = (u32::from_be_bytes([body[1], body[2], body[3], body[4]]), u32::from_be_bytes([body[5], body[6], body[7], body[8]]));
                let got = &body[9..];
                let at = u64::from(p) * u64::from(t.piece_len) + u64::from(b);
                assert_eq!(got, &data[at as usize..at as usize + got.len()], "wrong data for piece {p} at {b}");
                res.payload += got.len() as u64;
                res.overhead += 4 + 9;
                if inflight.is_some_and(|(ip, ib, il)| (ip, ib, il) == (p, b, got.len() as u32)) {
                    inflight = None;
                }
            }
            _ => res.overhead += 4 + n as u64,
        }
    }
    res
}
