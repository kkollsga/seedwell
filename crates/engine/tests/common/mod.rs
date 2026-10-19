//! Shared test helpers: synthetic torrents, an in-test HTTP tracker, engine launchers.
#![allow(dead_code)]

pub mod scripted;

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use trs_engine::{Engine, EngineConfig, EngineHandle, MemStore, PolicyData, StateStore, TorrentId, TorrentState};

/// Deterministic xorshift for test data.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
}

/// A generated torrent plus its payload.
pub struct TestTorrent {
    pub bytes: Vec<u8>,
    pub id: TorrentId,
    pub name: String,
    pub piece_len: u32,
    pub pieces: u32,
    /// (relative path below the torrent root, content)
    pub files: Vec<(Vec<String>, Vec<u8>)>,
    pub multi: bool,
}

pub struct TorrentSpec<'a> {
    pub name: &'a str,
    pub piece_len: u32,
    pub files: &'a [(&'a str, usize)],
    pub private: bool,
    pub announce: Option<String>,
    pub seed: u64,
}

fn put_int(out: &mut Vec<u8>, key: &str, v: i64) {
    out.extend(format!("{}:{}i{}e", key.len(), key, v).bytes());
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend(format!("{}:", b.len()).bytes());
    out.extend(b);
}

pub fn make_torrent(spec: &TorrentSpec) -> TestTorrent {
    let mut rng = Rng(spec.seed | 1);
    let mut files = Vec::new();
    let mut stream = Vec::new();
    for (p, len) in spec.files {
        let mut data = vec![0u8; *len];
        rng.fill(&mut data);
        stream.extend_from_slice(&data);
        files.push((p.split('/').map(str::to_owned).collect::<Vec<_>>(), data));
    }
    let multi = spec.files.len() > 1 || spec.files.first().is_some_and(|f| f.0.contains('/'));
    let mut pieces = Vec::new();
    for chunk in stream.chunks(spec.piece_len as usize) {
        pieces.extend_from_slice(&trs_storage::hash::sha1(chunk));
    }
    let piece_count = (pieces.len() / 20) as u32;
    // info dictionary, keys sorted
    let mut info = Vec::new();
    info.push(b'd');
    if multi {
        info.extend(b"5:filesl");
        for (comps, data) in &files {
            info.push(b'd');
            put_int(&mut info, "length", data.len() as i64);
            info.extend(b"4:pathl");
            for c in comps {
                put_bytes(&mut info, c.as_bytes());
            }
            info.extend(b"ee");
        }
        info.push(b'e');
    } else {
        put_int(&mut info, "length", files[0].1.len() as i64);
    }
    info.extend(b"4:name");
    put_bytes(&mut info, spec.name.as_bytes());
    put_int(&mut info, "piece length", i64::from(spec.piece_len));
    info.extend(b"6:pieces");
    put_bytes(&mut info, &pieces);
    if spec.private {
        put_int(&mut info, "private", 1);
    }
    info.push(b'e');
    let id: [u8; 20] = trs_storage::hash::sha1(&info);
    let mut t = Vec::new();
    t.push(b'd');
    if let Some(a) = &spec.announce {
        t.extend(b"8:announce");
        put_bytes(&mut t, a.as_bytes());
    }
    t.extend(b"4:info");
    t.extend(&info);
    t.push(b'e');
    TestTorrent {
        bytes: t,
        id,
        name: spec.name.to_owned(),
        piece_len: spec.piece_len,
        pieces: piece_count,
        files,
        multi,
    }
}

/// The standard multi-file torrent: nested dirs, a zero-length file, an odd piece count.
pub fn standard_torrent(total_extra: usize, announce: Option<String>, private: bool, seed: u64) -> TestTorrent {
    standard_named("bundle", total_extra, announce, private, seed)
}

pub fn standard_named(name: &str, total_extra: usize, announce: Option<String>, private: bool, seed: u64) -> TestTorrent {
    let piece_len = 32 * 1024;
    let mut sizes = [700_001usize, 0, 1_234_567 + total_extra, 98_765];
    let count = |s: &[usize; 4]| (s.iter().sum::<usize>()).div_ceil(piece_len as usize);
    while count(&sizes) % 2 == 0 {
        sizes[3] += 1000;
    }
    let files = [
        ("a/one.bin", sizes[0]),
        ("a/b/empty.txt", sizes[1]),
        ("a/b/c/two.bin", sizes[2]),
        ("z.dat", sizes[3]),
    ];
    let t = make_torrent(&TorrentSpec { name, piece_len, files: &files, private, announce, seed });
    assert_eq!(t.pieces % 2, 1);
    t
}

impl TestTorrent {
    /// Write the payload below `dir` (as a finished download).
    pub fn write_to(&self, dir: &Path) {
        for (comps, data) in &self.files {
            let _ = comps;
            let p = self.file_path_of(dir, comps);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, data).unwrap();
        }
    }

    pub fn file_path(&self, dir: &Path, i: usize) -> PathBuf {
        self.file_path_of(dir, &self.files[i].0)
    }

    fn file_path_of(&self, dir: &Path, comps: &[String]) -> PathBuf {
        let mut p = dir.to_path_buf();
        if self.multi {
            p.push(&self.name);
            for c in comps {
                p.push(c);
            }
        } else {
            // A single-file torrent is the file named like the torrent.
            p.push(&self.name);
        }
        p
    }

    /// Assert that `dir` holds every file with exactly the right bytes (except `skip`).
    pub fn verify(&self, dir: &Path, skip: &[usize]) {
        for (i, (_, data)) in self.files.iter().enumerate() {
            if skip.contains(&i) {
                continue;
            }
            let p = self.file_path(dir, i);
            let got = std::fs::read(&p).unwrap_or_else(|e| panic!("{p:?}: {e}"));
            assert_eq!(got.len(), data.len(), "{p:?} length");
            assert!(got == *data, "{p:?} content differs");
        }
    }

    /// Like [`verify`](Self::verify) for a torrent that the organiser put in a folder with another
    /// name than the torrent's (`{name}` templates clean and title-case it: `app-one` becomes
    /// `App-One`). Case matters on Linux, so the test says which name it expects.
    pub fn verify_as(&self, dir: &Path, folder: &str, skip: &[usize]) {
        let renamed = TestTorrent { name: folder.to_owned(), files: self.files.clone(), bytes: Vec::new(), ..*self };
        renamed.verify(dir, skip);
    }

    pub fn total(&self) -> u64 {
        self.files.iter().map(|f| f.1.len() as u64).sum()
    }
}

pub fn test_config(dir: &Path) -> EngineConfig {
    EngineConfig {
        listen_addr: "127.0.0.1".parse().unwrap(),
        listen_port: 0,
        default_save_dir: dir.to_path_buf(),
        dht_enabled: false,
        // Plain handshakes unless a test is about encryption (`tests/encryption.rs`): several tests talk to
        // hand-written raw-wire peers, and an encrypted dial to one of those only times out.
        encryption: trs_engine::config::EncryptionPolicy::Off,
        trackers_enabled: false,
        io_threads: 2,
        hash_threads: 2,
        write_cache_bytes: 16 << 20,
        fd_cache: 128,
        max_active_downloads: 10,
        // The mock tracker lives on loopback.
        tracker_destinations: trs_tracker::policy::DestinationPolicy::LOCAL,
        ..EngineConfig::default()
    }
}

pub struct Node {
    pub engine: Option<Engine>,
    pub h: EngineHandle,
    pub store: MemStore,
    pub dir: tempfile::TempDir,
}

pub async fn node(cfg: impl FnOnce(&mut EngineConfig)) -> Node {
    let dir = tempfile::tempdir().unwrap();
    node_in(dir, MemStore::new(), cfg).await
}

pub async fn node_in(dir: tempfile::TempDir, store: MemStore, cfg: impl FnOnce(&mut EngineConfig)) -> Node {
    let mut c = test_config(dir.path());
    cfg(&mut c);
    let engine = Engine::start(c, Arc::new(store.clone())).await.unwrap();
    let h = engine.handle();
    Node { engine: Some(engine), h, store, dir }
}

impl Node {
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.h.listen_port()))
    }

    pub async fn state(&self, id: &TorrentId) -> Option<TorrentState> {
        self.h.snapshot().await.ok()?.rows.iter().find(|r| r.id == *id).map(|r| r.state)
    }

    pub async fn row(&self, id: &TorrentId) -> Option<trs_engine::TorrentRow> {
        self.h.snapshot().await.ok()?.rows.into_iter().find(|r| r.id == *id)
    }

    pub async fn shutdown(&mut self, d: Duration) -> trs_engine::ShutdownReport {
        match self.engine.take() {
            Some(e) => e.shutdown(d).await,
            None => Default::default(),
        }
    }
}

/// Poll `f` until it returns `Some`, or panic after `secs`.
pub async fn wait_for<T, F, Fut>(what: &str, secs: u64, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        if Instant::now() > end {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait until the torrent has all wanted data (Seeding / LowPower).
pub async fn wait_done(n: &Node, id: &TorrentId, secs: u64) {
    wait_done_h(&n.h, id, secs).await
}

pub async fn row_h(h: &EngineHandle, id: &TorrentId) -> Option<trs_engine::TorrentRow> {
    h.snapshot().await.ok()?.rows.into_iter().find(|r| r.id == *id)
}

pub async fn wait_done_h(h: &EngineHandle, id: &TorrentId, secs: u64) {
    let end = Instant::now() + Duration::from_secs(secs);
    let mut next_report = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(r) = row_h(h, id).await {
            if matches!(r.state, TorrentState::Seeding | TorrentState::LowPower) {
                return;
            }
            if Instant::now() > next_report {
                next_report += Duration::from_secs(5);
                eprintln!(
                    "[wait_done] {:?} {}/{} peers={} down={} tier={:?} err={:?}",
                    r.state, r.done_bytes, r.total_bytes, r.peers, r.down_rate, r.tier, r.error
                );
            }
        }
        assert!(Instant::now() < end, "timed out waiting for the download to finish");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A node whose state lives in a `trs-state` directory (redb), restartable on the same directory.
pub struct SNode {
    pub engine: Option<Engine>,
    pub h: EngineHandle,
    pub store: Arc<StateStore>,
    pub dl: PathBuf,
    pub state: PathBuf,
}

pub async fn snode(
    state: &Path,
    dl: &Path,
    policy: Option<PolicyData>,
    cfg: impl FnOnce(&mut EngineConfig),
) -> SNode {
    let mut c = test_config(dl);
    cfg(&mut c);
    let (store, _report) = StateStore::open(state).unwrap();
    let engine = match policy {
        Some(p) => Engine::start_with_policy(c, store.clone(), p).await.unwrap(),
        None => Engine::start(c, store.clone()).await.unwrap(),
    };
    let h = engine.handle();
    SNode { engine: Some(engine), h, store, dl: dl.to_path_buf(), state: state.to_path_buf() }
}

impl SNode {
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.h.listen_port()))
    }

    pub async fn row(&self, id: &TorrentId) -> Option<trs_engine::TorrentRow> {
        row_h(&self.h, id).await
    }

    pub async fn shutdown(&mut self, d: Duration) -> trs_engine::ShutdownReport {
        match self.engine.take() {
            Some(e) => e.shutdown(d).await,
            None => Default::default(),
        }
    }
}

// ---- mock tracker -------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Announce {
    pub at: Instant,
    pub info_hash: Vec<u8>,
    pub peer_id: Vec<u8>,
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    /// `corrupt=` when the client sent it (it only does when above zero).
    pub corrupt: Option<u64>,
    /// 200, or 503 while the tracker is set to fail (see `MockTracker::set_failing`).
    pub status: u16,
    pub event: Option<String>,
    pub numwant: Option<u32>,
    pub key: Option<String>,
    pub raw_query: String,
}

#[derive(Default)]
struct TrackerState {
    failing: bool,
    log: Vec<Announce>,
    peers: HashMap<Vec<u8>, HashMap<u16, (bool, Instant)>>,
}

#[derive(Clone)]
pub struct MockTracker {
    pub addr: SocketAddr,
    state: Arc<Mutex<TrackerState>>,
    /// interval/min interval seconds
    pub interval: u32,
}

fn pct_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("00");
            out.push(u8::from_str_radix(h, 16).unwrap_or(0));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

impl MockTracker {
    pub async fn start(interval: u32) -> MockTracker {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let state = Arc::new(Mutex::new(TrackerState::default()));
        let st = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut s, _)) = l.accept().await else { break };
                let st = st.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let mut n = 0;
                    loop {
                        match s.read(&mut buf[n..]).await {
                            Ok(0) | Err(_) => return,
                            Ok(k) => n += k,
                        }
                        if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") || n == buf.len() {
                            break;
                        }
                    }
                    let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let line = req.lines().next().unwrap_or("");
                    let query = line.split(' ').nth(1).and_then(|p| p.split_once('?')).map_or("", |x| x.1).to_owned();
                    let mut q: HashMap<String, String> = HashMap::new();
                    for kv in query.split('&') {
                        if let Some((k, v)) = kv.split_once('=') {
                            q.insert(k.to_owned(), v.to_owned());
                        }
                    }
                    let ih = pct_decode(q.get("info_hash").map_or("", |s| s));
                    let port: u16 = q.get("port").and_then(|p| p.parse().ok()).unwrap_or(0);
                    let a = Announce {
                        at: Instant::now(),
                        info_hash: ih.clone(),
                        peer_id: pct_decode(q.get("peer_id").map_or("", |s| s)),
                        port,
                        uploaded: q.get("uploaded").and_then(|p| p.parse().ok()).unwrap_or(0),
                        downloaded: q.get("downloaded").and_then(|p| p.parse().ok()).unwrap_or(0),
                        left: q.get("left").and_then(|p| p.parse().ok()).unwrap_or(0),
                        corrupt: q.get("corrupt").and_then(|p| p.parse().ok()),
                        status: 200,
                        event: q.get("event").cloned(),
                        numwant: q.get("numwant").and_then(|p| p.parse().ok()),
                        key: q.get("key").cloned(),
                        raw_query: query.clone(),
                    };
                    let mut a = a;
                    if st.lock().unwrap().failing {
                        a.status = 503;
                        st.lock().unwrap().log.push(a);
                        let _ = s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n").await;
                        return;
                    }
                    let (seeds, leech, compact) = {
                        let mut g = st.lock().unwrap();
                        let entry = g.peers.entry(ih.clone()).or_default();
                        if a.event.as_deref() == Some("stopped") {
                            entry.remove(&port);
                        } else {
                            entry.insert(port, (a.left == 0, Instant::now()));
                        }
                        let mut compact = Vec::new();
                        let (mut seeds, mut leech) = (0, 0);
                        for (p, (seed, _)) in entry.iter() {
                            if *seed { seeds += 1 } else { leech += 1 }
                            if *p != port && a.numwant != Some(0) {
                                compact.extend([127, 0, 0, 1]);
                                compact.extend(p.to_be_bytes());
                            }
                        }
                        g.log.push(a);
                        (seeds, leech, compact)
                    };
                    let mut body = format!(
                        "d8:completei{seeds}e10:incompletei{leech}e8:intervali{interval}e12:min intervali{interval}e5:peers{}:",
                        compact.len()
                    )
                    .into_bytes();
                    body.extend(&compact);
                    body.push(b'e');
                    let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n", body.len());
                    let _ = s.write_all(head.as_bytes()).await;
                    let _ = s.write_all(&body).await;
                });
            }
        });
        MockTracker { addr, state, interval }
    }

    pub fn url(&self) -> String {
        format!("http://{}/announce", self.addr)
    }

    /// Answer every announce with `503` (and log it with that status) until set back.
    pub fn set_failing(&self, failing: bool) {
        self.state.lock().unwrap().failing = failing;
    }

    pub fn log(&self) -> Vec<Announce> {
        self.state.lock().unwrap().log.clone()
    }

    pub fn log_for(&self, ih: &[u8]) -> Vec<Announce> {
        self.log().into_iter().filter(|a| a.info_hash == ih).collect()
    }
}

// ---- raw peers ----------------------------------------------------------------------------

use std::sync::atomic::{AtomicU64, Ordering};
use tokio::net::TcpStream;

/// A peer that serves garbage for every block it is asked for.
pub struct EvilPeer {
    pub addr: SocketAddr,
    pub blocks_served: Arc<AtomicU64>,
    pub connections: Arc<AtomicU64>,
}

pub async fn evil_peer(t: &TestTorrent) -> EvilPeer {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let served = Arc::new(AtomicU64::new(0));
    let conns = Arc::new(AtomicU64::new(0));
    let (ih, pieces) = (t.id, t.pieces);
    let (s2, c2) = (served.clone(), conns.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            let (served, conns) = (s2.clone(), c2.clone());
            conns.fetch_add(1, Ordering::Relaxed);
            tokio::spawn(async move {
                let mut hs = [0u8; 68];
                if s.read_exact(&mut hs).await.is_err() {
                    return;
                }
                let mut out = Vec::new();
                out.extend(b"\x13BitTorrent protocol");
                out.extend([0u8; 8]);
                out.extend(ih);
                out.extend(b"-EV0001-evilevilevil");
                // bitfield: every piece
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
                            let index = &body[1..5];
                            let begin = &body[5..9];
                            let length = u32::from_be_bytes([body[9], body[10], body[11], body[12]]) as usize;
                            let mut m = Vec::new();
                            m.extend(((9 + length) as u32).to_be_bytes());
                            m.push(7);
                            m.extend(index);
                            m.extend(begin);
                            m.extend(vec![0xABu8; length]);
                            served.fetch_add(1, Ordering::Relaxed);
                            if s.write_all(&m).await.is_err() {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
    });
    EvilPeer { addr, blocks_served: served, connections: conns }
}

/// Connect to an engine as a bare-bones peer and return its handshake reserved bytes and the
/// raw payload of its extended handshake.
pub async fn probe_ext_handshake(addr: SocketAddr, ih: TorrentId) -> ([u8; 8], Vec<u8>) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = Vec::new();
    out.extend(b"\x13BitTorrent protocol");
    out.extend([0, 0, 0, 0, 0, 0x10, 0, 0]);
    out.extend(ih);
    out.extend(b"-PR0001-probeprobepr");
    s.write_all(&out).await.unwrap();
    let mut hs = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut hs)).await.unwrap().unwrap();
    let mut reserved = [0u8; 8];
    reserved.copy_from_slice(&hs[20..28]);
    loop {
        let mut len = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(5), s.read_exact(&mut len)).await.unwrap().unwrap();
        let n = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; n];
        if n > 0 {
            s.read_exact(&mut body).await.unwrap();
        }
        if body.len() >= 2 && body[0] == 20 && body[1] == 0 {
            return (reserved, body[2..].to_vec());
        }
    }
}

/// Connect as a bare peer, express interest and time (handshake reply, first block).
pub async fn raw_first_block(addr: SocketAddr, ih: TorrentId) -> (Duration, Duration) {
    let t0 = Instant::now();
    let mut s = TcpStream::connect(addr).await.unwrap();
    let mut out = Vec::new();
    out.extend(b"\x13BitTorrent protocol");
    out.extend([0u8; 8]);
    out.extend(ih);
    out.extend(b"-RW0001-rawrawrawraw");
    // handshake + interested + request(0, 0, 16384): the request is only honoured after the unchoke,
    // so send it once the unchoke has arrived.
    out.extend([0, 0, 0, 1, 2]);
    s.write_all(&out).await.unwrap();
    let mut hs = [0u8; 68];
    tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut hs)).await.unwrap().unwrap();
    let t_hs = t0.elapsed();
    let mut requested = false;
    loop {
        let mut len = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(10), s.read_exact(&mut len)).await.unwrap().unwrap();
        let n = u32::from_be_bytes(len) as usize;
        let mut body = vec![0u8; n];
        if n > 0 {
            s.read_exact(&mut body).await.unwrap();
        }
        match body.first() {
            Some(1) if !requested => {
                requested = true;
                let mut r = vec![0, 0, 0, 13, 6];
                r.extend(0u32.to_be_bytes());
                r.extend(0u32.to_be_bytes());
                r.extend(16384u32.to_be_bytes());
                s.write_all(&r).await.unwrap();
            }
            Some(7) => return (t_hs, t0.elapsed()),
            _ => {}
        }
    }
}

pub fn hex_of(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Wait for an engine event matching `f`.
pub async fn wait_event(
    rx: &mut tokio::sync::broadcast::Receiver<trs_engine::EngineEvent>,
    secs: u64,
    mut f: impl FnMut(&trs_engine::EngineEvent) -> bool,
) -> trs_engine::EngineEvent {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match tokio::time::timeout(left, rx.recv()).await {
            Ok(Ok(e)) if f(&e) => return e,
            Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(_)) => panic!("event channel closed"),
            Err(_) => panic!("timed out waiting for an event"),
        }
    }
}

// ---- a small range-capable HTTP file server -------------------------------------------------

/// One file the test server can serve.
#[derive(Clone)]
pub struct ServedFile {
    pub path: String,
    pub body: Arc<Vec<u8>>,
    pub content_type: String,
    pub etag: Option<String>,
    /// Bytes per second (None = as fast as possible).
    pub rate: Option<u64>,
    pub ranges: bool,
}

impl ServedFile {
    pub fn new(path: &str, body: Vec<u8>) -> Self {
        ServedFile {
            path: path.to_owned(),
            body: Arc::new(body),
            content_type: "application/octet-stream".into(),
            etag: Some("\"v1\"".into()),
            rate: None,
            ranges: true,
        }
    }
}

#[derive(Clone, Debug)]
pub struct HttpHit {
    pub path: String,
    pub range: Option<String>,
}

pub struct FileServer {
    pub addr: SocketAddr,
    pub hits: Arc<Mutex<Vec<HttpHit>>>,
}

impl FileServer {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub fn hits(&self) -> Vec<HttpHit> {
        self.hits.lock().unwrap().clone()
    }
}

pub async fn serve_files(files: Vec<ServedFile>) -> FileServer {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let hits = Arc::new(Mutex::new(Vec::new()));
    let files = Arc::new(files);
    let h2 = hits.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = l.accept().await else { break };
            let (files, hits) = (files.clone(), h2.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let mut n = 0;
                loop {
                    match s.read(&mut buf[n..]).await {
                        Ok(0) | Err(_) => return,
                        Ok(k) => n += k,
                    }
                    if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") || n == buf.len() {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                let path = req.lines().next().and_then(|l| l.split(' ').nth(1)).unwrap_or("/").to_owned();
                let range = req.lines().find_map(|l| l.strip_prefix("Range: ").or_else(|| l.strip_prefix("range: "))).map(str::to_owned);
                hits.lock().unwrap().push(HttpHit { path: path.clone(), range: range.clone() });
                let Some(f) = files.iter().find(|f| f.path == path) else {
                    let _ = s.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                };
                let total = f.body.len();
                let (mut start, mut end) = (0usize, total.saturating_sub(1));
                let mut partial = false;
                if f.ranges
                    && let Some(r) = range.as_deref().and_then(|r| r.strip_prefix("bytes=")) {
                        let (a, b) = r.split_once('-').unwrap_or((r, ""));
                        start = a.parse().unwrap_or(0).min(total);
                        end = b.parse().unwrap_or(total.saturating_sub(1)).min(total.saturating_sub(1));
                        partial = true;
                    }
                if start >= total && total > 0 {
                    let _ = s.write_all(b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    return;
                }
                let len = if total == 0 { 0 } else { end + 1 - start };
                let mut head = if partial {
                    format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{total}\r\n")
                } else {
                    "HTTP/1.1 200 OK\r\n".to_owned()
                };
                head.push_str(&format!("Content-Length: {len}\r\nContent-Type: {}\r\nConnection: close\r\n", f.content_type));
                if f.ranges {
                    head.push_str("Accept-Ranges: bytes\r\n");
                }
                if let Some(e) = &f.etag {
                    head.push_str(&format!("ETag: {e}\r\n"));
                }
                head.push_str("\r\n");
                if s.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let chunk = 16 * 1024;
                let mut pos = start;
                while pos < start + len {
                    let to = (pos + chunk).min(start + len);
                    if s.write_all(&f.body[pos..to]).await.is_err() {
                        return;
                    }
                    pos = to;
                    if let Some(rate) = f.rate {
                        tokio::time::sleep(Duration::from_secs_f64(chunk as f64 / rate as f64)).await;
                    }
                }
                let _ = s.shutdown().await;
            });
        }
    });
    FileServer { addr, hits }
}

pub fn pattern_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut v = vec![0u8; n];
    Rng(seed | 1).fill(&mut v);
    v
}

/// A TCP port that is free right now (bound and released again).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}
