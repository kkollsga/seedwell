//! The simulated swarm: seeders that speak just enough BitTorrent to serve generated pieces, and a
//! tracker that answers announces. Everything binds to 127.0.0.1 only. Memory stays bounded: a
//! request is answered with one block computed on the spot (at most 128 KiB), nothing is cached.
#![allow(missing_docs)] // variant fields are named after the options they hold


use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;

use super::content::{B, Built, Kind, fill};

/// What the tracker does with announces for one sim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerMode {
    /// Answers with the seeders.
    Ok,
    /// `failure reason: Unregistered torrent` (the tracker halts).
    Unregistered,
    /// Hangs up without an answer.
    Down,
    /// Answers after 15 s.
    Slow,
    /// HTTP 503.
    Fail503,
    /// Answers with this announce interval (seconds).
    Interval(u32),
}

impl TrackerMode {
    /// Parse `ok|unregistered|down|slow|fail503|interval=60`.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => Self::Ok,
            "unregistered" => Self::Unregistered,
            "down" => Self::Down,
            "slow" => Self::Slow,
            "fail503" | "503" => Self::Fail503,
            _ => Self::Interval(s.strip_prefix("interval=")?.parse().ok().filter(|n| (1..=86_400).contains(n))?),
        })
    }

    /// The word.
    pub fn word(self) -> String {
        match self {
            Self::Ok => "ok".into(),
            Self::Unregistered => "unregistered".into(),
            Self::Down => "down".into(),
            Self::Slow => "slow".into(),
            Self::Fail503 => "fail503".into(),
            Self::Interval(n) => format!("interval={n}"),
        }
    }
}

/// A shared token bucket. `0` bytes/s means unlimited. Changing the rate takes effect at once.
pub struct Limiter {
    rate: AtomicU64,
    state: Mutex<(f64, Instant)>,
}

impl Limiter {
    /// New limiter.
    pub fn new(rate: u64) -> Self {
        Self { rate: AtomicU64::new(rate), state: Mutex::new((0.0, Instant::now())) }
    }

    /// Change the rate (bytes/s, 0 = unlimited).
    pub fn set(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
    }

    /// The rate.
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Wait until `n` bytes may be sent.
    pub async fn acquire(&self, n: usize) {
        loop {
            let rate = self.rate();
            if rate == 0 {
                return;
            }
            let wait = {
                let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let now = Instant::now();
                let burst = (rate as f64 / 4.0).max(64.0 * 1024.0);
                s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * rate as f64).min(burst);
                s.1 = now;
                if s.0 >= n as f64 || s.0 >= burst {
                    s.0 -= n as f64;
                    return;
                }
                (n as f64 - s.0) / rate as f64
            };
            tokio::time::sleep(Duration::from_secs_f64(wait.clamp(0.001, 1.0))).await;
        }
    }
}

/// One simulated torrent in the host.
pub struct SimState {
    /// `sim1`, `sim2`, ...
    pub id: String,
    /// Kind.
    pub kind: Kind,
    /// The torrent.
    pub built: Arc<Built>,
    /// Upload cap.
    pub limiter: Limiter,
    /// Seeders refuse and the tracker answers without peers while true.
    pub stopped: watch::Sender<bool>,
    /// Tracker behaviour.
    pub mode: Mutex<TrackerMode>,
    /// Bytes of pieces served.
    pub served: AtomicU64,
    /// Stop answering after this many bytes (`--stall-after`).
    pub stall_after: Option<u64>,
    /// Pieces that are served corrupt the first N times.
    pub corrupt: Mutex<HashMap<u32, u32>>,
    /// Announces seen.
    pub announces: AtomicU64,
    /// The last announce's event (`started`, `completed`, ...), empty for a regular one.
    pub last_event: Mutex<String>,
    /// Open peer connections.
    pub conns: AtomicU32,
    /// Seeder addresses.
    pub peers: Mutex<Vec<SocketAddr>>,
}

impl SimState {
    /// Whether the swarm is stopped.
    pub fn is_stopped(&self) -> bool {
        *self.stopped.borrow()
    }
}

/// The torrents the tracker knows, by info hash.
pub type Registry = Arc<Mutex<HashMap<[u8; 20], Arc<SimState>>>>;

// ----------------------------------------------------------------------------- seeders

/// Bind `n` seeder listeners on loopback and serve until the task is aborted.
pub async fn spawn_seeders(sim: Arc<SimState>, n: u32) -> std::io::Result<Vec<tokio::task::JoinHandle<()>>> {
    let mut handles = Vec::new();
    for _ in 0..n.max(1) {
        let l = TcpListener::bind("127.0.0.1:0").await?;
        sim.peers.lock().unwrap_or_else(|e| e.into_inner()).push(l.local_addr()?);
        let s = sim.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = l.accept().await else { continue };
                if s.is_stopped() || s.conns.load(Ordering::Relaxed) >= 200 {
                    continue; // dropped: closed at once
                }
                let s2 = s.clone();
                tokio::spawn(async move {
                    s2.conns.fetch_add(1, Ordering::Relaxed);
                    let _ = serve_peer(sock, &s2).await;
                    s2.conns.fetch_sub(1, Ordering::Relaxed);
                });
            }
        }));
    }
    Ok(handles)
}

async fn send(w: &mut (impl AsyncWriteExt + Unpin), id: u8, parts: &[&[u8]]) -> std::io::Result<()> {
    let len: usize = 1 + parts.iter().map(|p| p.len()).sum::<usize>();
    let mut head = Vec::with_capacity(5);
    head.extend((len as u32).to_be_bytes());
    head.push(id);
    w.write_all(&head).await?;
    for p in parts {
        w.write_all(p).await?;
    }
    Ok(())
}

async fn serve_peer(mut sock: TcpStream, sim: &Arc<SimState>) -> std::io::Result<()> {
    let _ = sock.set_nodelay(true);
    let mut hs = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(10), sock.read_exact(&mut hs)).await.map_err(|_| std::io::ErrorKind::TimedOut)??;
    if hs[0] != 19 || &hs[1..20] != b"BitTorrent protocol" || hs[28..48] != sim.built.info_hash {
        return Ok(()); // an encrypted or foreign handshake: hang up, the client dials plain next
    }
    let ltep = hs[25] & 0x10 != 0;
    let mut ours = Vec::with_capacity(68);
    ours.push(19);
    ours.extend(b"BitTorrent protocol");
    let mut reserved = [0u8; 8];
    reserved[5] = 0x10;
    ours.extend(reserved);
    ours.extend(sim.built.info_hash);
    ours.extend(b"-SM0001-");
    let mut tail = [0u8; 12];
    fill(0x51_4D, u64::from(sim.conns.load(Ordering::Relaxed)) * 12, &mut tail);
    ours.extend(tail.iter().map(|b| b'a' + b % 26));
    sock.write_all(&ours).await?;
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::with_capacity(8192, rd);
    if ltep {
        let h = B::D(vec![
            ("m", B::D(vec![("ut_metadata", B::I(1))])),
            ("metadata_size", B::I(sim.built.info.len() as i64)),
            ("v", B::s("Seedwell sim")),
        ])
        .bytes();
        send(&mut wr, 20, &[&[0], &h]).await?;
    }
    let pieces = sim.built.pieces as usize;
    let mut bitfield = vec![0xFFu8; pieces.div_ceil(8)];
    if !pieces.is_multiple_of(8)
        && let Some(l) = bitfield.last_mut() {
            *l = 0xFFu8 << (8 - pieces % 8);
        }
    send(&mut wr, 5, &[&bitfield]).await?;
    let mut their_meta_id: u8 = 0;
    let mut stop_rx = sim.stopped.subscribe();
    loop {
        let mut lenb = [0u8; 4];
        tokio::select! {
            r = rd.read_exact(&mut lenb) => { r?; }
            _ = stop_rx.changed() => { if *stop_rx.borrow() { return Ok(()); } continue; }
        }
        let len = u32::from_be_bytes(lenb) as usize;
        if len == 0 {
            continue; // keep-alive
        }
        if len > 64 * 1024 {
            return Ok(());
        }
        let mut msg = vec![0u8; len];
        rd.read_exact(&mut msg).await?;
        match msg[0] {
            2 => send(&mut wr, 1, &[]).await?, // interested -> unchoke
            6 if len == 13 => {
                let idx = u32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]);
                let begin = u32::from_be_bytes([msg[5], msg[6], msg[7], msg[8]]);
                let want = u32::from_be_bytes([msg[9], msg[10], msg[11], msg[12]]);
                let b = &sim.built;
                let piece_start = u64::from(idx) * u64::from(b.piece_len);
                if idx >= b.pieces || want == 0 || want > 128 * 1024 || piece_start >= b.total {
                    return Ok(());
                }
                let piece_len = (b.total - piece_start).min(u64::from(b.piece_len));
                if u64::from(begin) + u64::from(want) > piece_len {
                    return Ok(());
                }
                if sim.is_stopped() {
                    return Ok(());
                }
                if sim.stall_after.is_some_and(|s| sim.served.load(Ordering::Relaxed) >= s) {
                    continue; // stalled: the request is never answered
                }
                sim.limiter.acquire(want as usize).await;
                let mut data = vec![0u8; want as usize];
                fill(b.seed, piece_start + u64::from(begin), &mut data);
                if begin == 0 {
                    let mut c = sim.corrupt.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(left) = c.get_mut(&idx)
                        && *left > 0 {
                            *left -= 1;
                            data[0] ^= 0xFF;
                        }
                }
                sim.served.fetch_add(u64::from(want), Ordering::Relaxed);
                send(&mut wr, 7, &[&msg[1..5], &msg[5..9], &data]).await?;
            }
            20 if len >= 2 => {
                if msg[1] == 0 {
                    // their extended handshake: where they want ut_metadata messages
                    if let Some(p) = find_int(&msg[2..], b"ut_metadatai") {
                        their_meta_id = p as u8;
                    }
                } else if msg[1] == 1 && their_meta_id != 0
                    && let Some(piece) = find_int(&msg[2..], b"5:piecei") {
                        let info = &sim.built.info;
                        let start = piece as usize * 16384;
                        if start < info.len() {
                            let end = (start + 16384).min(info.len());
                            let head = B::D(vec![("msg_type", B::I(1)), ("piece", B::I(piece)), ("total_size", B::I(info.len() as i64))]).bytes();
                            send(&mut wr, 20, &[&[their_meta_id], &head, &info[start..end]]).await?;
                        } else {
                            let head = B::D(vec![("msg_type", B::I(2)), ("piece", B::I(piece))]).bytes();
                            send(&mut wr, 20, &[&[their_meta_id], &head]).await?;
                        }
                    }
            }
            _ => {}
        }
    }
}

/// The integer after `key` in a bencoded blob (`...ut_metadatai3e...` -> 3), without a full parser.
fn find_int(blob: &[u8], key: &[u8]) -> Option<i64> {
    let pos = blob.windows(key.len()).position(|w| w == key)? + key.len();
    let end = blob[pos..].iter().position(|b| *b == b'e')?;
    std::str::from_utf8(&blob[pos..pos + end]).ok()?.parse().ok()
}

// ----------------------------------------------------------------------------- tracker

fn pct_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    out
}

fn query_param<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// Serve announces on a loopback port until aborted. Returns the address.
pub async fn spawn_tracker(reg: Registry) -> std::io::Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let l = TcpListener::bind("127.0.0.1:0").await?;
    let addr = l.local_addr()?;
    let h = tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = l.accept().await else { continue };
            let reg = reg.clone();
            tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(60), announce(sock, reg)).await;
            });
        }
    });
    Ok((addr, h))
}

async fn announce(sock: TcpStream, reg: Registry) -> std::io::Result<()> {
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);
    let mut line = String::new();
    rd.read_line(&mut line).await?;
    // drain the headers
    for _ in 0..64 {
        let mut h = String::new();
        if rd.read_line(&mut h).await? == 0 || h.trim().is_empty() {
            break;
        }
    }
    let target = line.split_whitespace().nth(1).unwrap_or("");
    let query = target.split_once('?').map_or("", |(_, q)| q);
    let hash = query_param(query, "info_hash").map(pct_decode).unwrap_or_default();
    let sim = <[u8; 20]>::try_from(hash.as_slice()).ok().and_then(|h| reg.lock().unwrap_or_else(|e| e.into_inner()).get(&h).cloned());
    let respond = |status: &str, body: Vec<u8>| {
        let mut o = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).into_bytes();
        o.extend(body);
        o
    };
    let Some(sim) = sim else {
        let body = B::D(vec![("failure reason", B::s("Unregistered torrent"))]).bytes();
        return wr.write_all(&respond("200 OK", body)).await;
    };
    sim.announces.fetch_add(1, Ordering::Relaxed);
    *sim.last_event.lock().unwrap_or_else(|e| e.into_inner()) = query_param(query, "event").unwrap_or("").to_owned();
    let mode = *sim.mode.lock().unwrap_or_else(|e| e.into_inner());
    let out = match mode {
        TrackerMode::Down => return Ok(()),
        TrackerMode::Fail503 => respond("503 Service Unavailable", Vec::new()),
        TrackerMode::Unregistered => respond("200 OK", B::D(vec![("failure reason", B::s("Unregistered torrent"))]).bytes()),
        m => {
            if m == TrackerMode::Slow {
                tokio::time::sleep(Duration::from_secs(15)).await;
            }
            let interval = if let TrackerMode::Interval(n) = m { i64::from(n) } else { 1800 };
            let peers: Vec<u8> = if sim.is_stopped() {
                Vec::new()
            } else {
                sim.peers
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .iter()
                    .filter_map(|a| match a {
                        SocketAddr::V4(v) => Some([v.ip().octets().as_slice(), &v.port().to_be_bytes()].concat()),
                        _ => None,
                    })
                    .flatten()
                    .collect()
            };
            let seeders = (peers.len() / 6) as i64;
            respond(
                "200 OK",
                B::D(vec![
                    ("interval", B::I(interval)),
                    ("min interval", B::I(interval.min(60))),
                    ("complete", B::I(seeders)),
                    ("incomplete", B::I(0)),
                    ("peers", B::S(peers)),
                ])
                .bytes(),
            )
        }
    };
    wr.write_all(&out).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_modes_parse() {
        assert_eq!(TrackerMode::parse("ok"), Some(TrackerMode::Ok));
        assert_eq!(TrackerMode::parse("interval=60"), Some(TrackerMode::Interval(60)));
        assert_eq!(TrackerMode::parse("interval=0"), None);
        assert_eq!(TrackerMode::parse("fail503"), Some(TrackerMode::Fail503));
        assert_eq!(TrackerMode::parse("maybe"), None);
        assert_eq!(TrackerMode::Interval(5).word(), "interval=5");
    }

    #[test]
    fn percent_decoding_keeps_binary() {
        assert_eq!(pct_decode("%00%ff%41+x"), vec![0, 255, b'A', b' ', b'x']);
        assert_eq!(pct_decode("100%"), b"100%".to_vec());
    }

    #[tokio::test]
    async fn limiter_paces() {
        let l = Limiter::new(1_000_000);
        let t = Instant::now();
        for _ in 0..40 {
            l.acquire(50_000).await;
        }
        // 2 MB at 1 MB/s with a 250 KB burst: about 1.7 s
        assert!(t.elapsed() > Duration::from_millis(1200), "{:?}", t.elapsed());
        l.set(0);
        let t = Instant::now();
        l.acquire(100_000_000).await;
        assert!(t.elapsed() < Duration::from_millis(50));
    }
}
