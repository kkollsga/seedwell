//! Play while downloading end to end (SPEC 7F): a throttled seeder, a leecher that streams over
//! HTTP `Range` from 127.0.0.1, bytes checked against the source, seeks served ahead of the
//! pieces in between, and clean ends when the torrent goes away.
use crate::common;
use std::time::{Duration, Instant};

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use trs_engine::*;

const PIECE: u32 = 256 * 1024;

fn movie(len: usize, seed: u64) -> TestTorrent {
    make_torrent(&TorrentSpec { name: "movie.mp4", piece_len: PIECE, files: &[("movie.mp4", len)], private: false, announce: None, seed })
}

struct Http {
    sock: TcpStream,
    buf: Vec<u8>,
}

impl Http {
    async fn get(url: &str, range: Option<&str>) -> (Http, u16) {
        let rest = url.strip_prefix("http://").unwrap();
        let (auth, path) = rest.split_at(rest.find('/').unwrap());
        let mut sock = TcpStream::connect(auth).await.unwrap();
        let r = range.map(|r| format!("Range: bytes={r}\r\n")).unwrap_or_default();
        sock.write_all(format!("GET {path} HTTP/1.1\r\nHost: {auth}\r\n{r}\r\n").as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..i]).to_string();
                let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
                buf.drain(..i + 4);
                return (Http { sock, buf }, status);
            }
            let n = sock.read(&mut tmp).await.unwrap();
            assert!(n > 0, "closed before the response head");
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    /// Read exactly `n` body bytes (fewer when the server ends the response).
    async fn take(&mut self, n: usize, secs: u64) -> Vec<u8> {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut tmp = vec![0u8; 64 * 1024];
        while self.buf.len() < n {
            let left = end.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.sock.read(&mut tmp)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(k)) => self.buf.extend_from_slice(&tmp[..k]),
                Err(_) => panic!("timed out after {} of {n} bytes", self.buf.len()),
            }
        }
        let k = n.min(self.buf.len());
        self.buf.drain(..k).collect()
    }

    /// Does the server hang up (EOF) within `secs`?
    async fn ends_within(&mut self, secs: u64) -> bool {
        let end = Instant::now() + Duration::from_secs(secs);
        let mut tmp = vec![0u8; 64 * 1024];
        loop {
            let left = end.saturating_duration_since(Instant::now());
            match tokio::time::timeout(left, self.sock.read(&mut tmp)).await {
                Ok(Ok(0)) | Ok(Err(_)) => return true,
                Ok(Ok(_)) => {}
                Err(_) => return false,
            }
        }
    }
}

async fn pair(len: usize, seed: u64, up: u64) -> (TestTorrent, Node, Node) {
    let t = movie(len, seed);
    let seeder = node(|c| c.up_limit = Some(up)).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let leech = node(|_| {}).await;
    let opts = AddOptions { peers: vec![seeder.addr()], ..AddOptions::default() };
    leech.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    (t, seeder, leech)
}

/// Bytes are exact, a seek to the middle is served long before the pieces in between exist, and
/// the rest of the download still completes (rarest-first, never whole-torrent sequential).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streams_exact_bytes_and_a_seek_jumps_the_queue() {
    let len = 24 << 20;
    let (t, _seeder, leech) = pair(len, 77, 3_000_000).await;
    let data = &t.files[0].1;
    let info = leech.h.stream_url(t.id, 0).await.unwrap();
    assert!(!info.complete);
    assert_eq!(info.container, StreamContainer::Mp4Family);
    assert_eq!(info.file_len, len as u64);
    let url = info.url.clone().unwrap();
    assert!(url.starts_with("http://127.0.0.1:"), "{url}");

    // The head of the file.
    let (mut r, status) = Http::get(&url, Some("0-")).await;
    assert_eq!(status, 206);
    let head = r.take(1 << 20, 20).await;
    assert_eq!(head, data[..1 << 20]);
    drop(r);

    // Seek to the middle: the first bytes there arrive while most of the file in between is absent.
    let mid = len / 2;
    let t0 = Instant::now();
    let (mut r, status) = Http::get(&url, Some(&format!("{mid}-"))).await;
    assert_eq!(status, 206);
    let got = r.take(512 * 1024, 20).await;
    let took = t0.elapsed();
    assert_eq!(got, data[mid..mid + 512 * 1024], "bytes at the seek target");
    let row = leech.row(&t.id).await.unwrap();
    eprintln!("seek served in {took:?}, done {} of {}", row.done_bytes, row.total_bytes);
    assert!(row.done_bytes < (len as u64) * 3 / 4, "the whole file was fetched before the seek was served");
    assert!(took < Duration::from_secs(8), "seek took {took:?}");
    drop(r);

    // The download still finishes and is exact.
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
}

/// Streaming a skipped file selects it for download and says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn streaming_an_unselected_file_selects_it() {
    let t = make_torrent(&TorrentSpec {
        name: "pack",
        piece_len: PIECE,
        files: &[("a.mp4", 2 << 20), ("b.mkv", 2 << 20)],
        private: false,
        announce: None,
        seed: 5,
    });
    let seeder = node(|_| {}).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let leech = node(|_| {}).await;
    let opts = AddOptions {
        peers: vec![seeder.addr()],
        file_priorities: vec![FilePriority::Normal, FilePriority::Skip],
        ..AddOptions::default()
    };
    leech.h.add_torrent_file(t.bytes.clone(), opts).await.unwrap();
    let mut ev = leech.h.subscribe();
    let info = leech.h.stream_url(t.id, 1).await.unwrap();
    assert!(info.selected || info.complete);
    assert_eq!(info.container, StreamContainer::Matroska);
    if let Some(url) = info.url {
        let (mut r, status) = Http::get(&url, Some("0-65535")).await;
        assert_eq!(status, 206);
        assert_eq!(r.take(65536, 20).await, t.files[1].1[..65536]);
        let saw = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(EngineEvent::FilesSelected { files, .. }) = ev.recv().await {
                    return files;
                }
            }
        })
        .await;
        assert_eq!(saw.ok(), Some(vec![1]));
    }
    let d = leech.h.torrent_detail(t.id).await.unwrap();
    assert_ne!(d.files[1].priority, FilePriority::Skip);
}

/// Removing the torrent while a response is in flight ends it cleanly (EOF, no hang, no garbage).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removing_the_torrent_ends_the_response() {
    let (t, _seeder, leech) = pair(16 << 20, 91, 1_000_000).await;
    let info = leech.h.stream_url(t.id, 0).await.unwrap();
    let url = info.url.unwrap();
    let (mut r, _) = Http::get(&url, Some("0-")).await;
    let first = r.take(256 * 1024, 20).await;
    assert_eq!(first, t.files[0].1[..256 * 1024]);
    leech.h.remove(&[t.id], true).await.unwrap();
    assert!(r.ends_within(5).await, "the response must end after the removal");
    // The stream no longer exists.
    let (_r2, status) = Http::get(&url, Some("0-10")).await;
    assert_eq!(status, 404);
}

/// `stop_stream` ends a response and a finished file needs no server.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stop_stream_and_complete_files() {
    let (t, _seeder, leech) = pair(6 << 20, 13, 2_000_000).await;
    let info = leech.h.stream_url(t.id, 0).await.unwrap();
    let token = info.token.clone().unwrap();
    let (mut r, _) = Http::get(&info.url.unwrap(), Some("0-")).await;
    let _ = r.take(1024, 20).await;
    assert!(leech.h.stop_stream(&token));
    assert!(r.ends_within(5).await);
    assert!(!leech.h.stop_stream(&token));
    wait_done(&leech, &t.id, 60).await;
    let again = leech.h.stream_url(t.id, 0).await.unwrap();
    assert!(again.complete && again.url.is_none());
}

/// A torrent without metadata gives a calm error.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_torrent_and_bad_file_are_errors() {
    let (t, _seeder, leech) = pair(2 << 20, 3, 5_000_000).await;
    assert!(matches!(leech.h.stream_url(t.id, 9).await, Err(EngineError::Invalid(_)) | Err(EngineError::NoMetadata)));
    assert!(leech.h.stream_url([9u8; 20], 0).await.is_err());
}

/// Unverified data is never returned: with only a peer that serves garbage, the player gets
/// no bytes at all (the response stalls) and certainly none of the garbage.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unverified_data_is_never_served() {
    let t = movie(2 << 20, 21);
    let evil = evil_peer(&t).await;
    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![evil.addr], ..AddOptions::default() }).await.unwrap();
    let info = leech.h.stream_url(t.id, 0).await.unwrap();
    let (mut r, status) = Http::get(&info.url.unwrap(), Some("0-")).await;
    assert_eq!(status, 206);
    wait_for("the evil peer was asked", 20, || async { (evil.blocks_served.load(std::sync::atomic::Ordering::Relaxed) > 0).then_some(()) }).await;
    let mut got = Vec::new();
    let mut tmp = [0u8; 4096];
    let end = Instant::now() + Duration::from_secs(3);
    while let Ok(Ok(n)) = tokio::time::timeout(end.saturating_duration_since(Instant::now()), r.sock.read(&mut tmp)).await {
        if n == 0 {
            break;
        }
        got.extend_from_slice(&tmp[..n]);
    }
    got.splice(0..0, r.buf.drain(..));
    assert!(got.is_empty(), "{} unverified bytes were served", got.len());
    assert_eq!(leech.row(&t.id).await.unwrap().done_bytes, 0);
}
