//! Real players against the real engine: a seeder throttled to `TRS_RATE_MBS` (default 6) MB/s and
//! a Seedwell leecher streaming a ~300 MB 1080p file from it over loopback HTTP. All `#[ignore]`:
//! they need ffmpeg, swiftc (AVFoundation) and VLC, and print numbers for SPEC 7F.4.
//!
//!   TRS_FX_MP4=<movie.mp4> TRS_FX_MKV=<movie.mkv> TRS_VLC=<VLC binary> \
//!   cargo test -p trs-engine --test streaming_players --release -- --ignored --nocapture --test-threads=1
//!
//! Budgets (SPEC 7F.4, healthy swarm at >= 5 MB/s): first frame <= 5 s, seek <= 4 s (pieces <= 8 MiB).
//! `TRS_PIECE_MIB` (default 4) sets the piece size.
use crate::common;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use trs_engine::*;

const FFMPEG: &str = "/opt/homebrew/bin/ffmpeg";

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|s| !s.is_empty())
}

fn env_u64(k: &str, d: u64) -> u64 {
    env(k).and_then(|v| v.parse().ok()).unwrap_or(d)
}

fn ms(a: Instant, b: Instant) -> f64 {
    b.duration_since(a).as_secs_f64() * 1000.0
}

fn verdict(name: &str, v: f64, budget: f64) -> String {
    format!("RESULT {name}: {v:.0} ms (budget {budget:.0} ms) {}", if v <= budget { "OK" } else { "OVER" })
}

struct Lab {
    /// Kept alive: the seeder.
    _seeder: Node,
    leech: Node,
    id: TorrentId,
    url: String,
    file_len: u64,
    duration: f64,
    piece_mib: u64,
}

/// A fresh seeder (throttled) and a fresh leecher streaming `path`; `secs` is the media duration.
async fn lab(path: &str, name: &str, secs: f64) -> Lab {
    let data = std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let piece_mib = env_u64("TRS_PIECE_MIB", 4);
    let rate = env_u64("TRS_RATE_MBS", 6) * 1_000_000;
    let t = make_torrent(&TorrentSpec { name, piece_len: (piece_mib << 20) as u32, files: &[(name, 0)], private: false, announce: None, seed: 1 });
    // `make_torrent` fills random bytes; build the real one from the file instead.
    drop(t);
    let t = torrent_of(name, (piece_mib << 20) as u32, data);
    let seeder = node(|c| c.up_limit = Some(rate)).await;
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 120).await;
    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..AddOptions::default() }).await.unwrap();
    let info = leech.h.stream_url(t.id, 0).await.unwrap();
    Lab { _seeder: seeder, leech, id: t.id, url: info.url.unwrap(), file_len: info.file_len, duration: secs, piece_mib }
}

/// A single-file torrent over the bytes of a real file.
fn torrent_of(name: &str, piece_len: u32, data: Vec<u8>) -> TestTorrent {
    let mut pieces = Vec::new();
    for chunk in data.chunks(piece_len as usize) {
        pieces.extend_from_slice(&trs_storage::hash::sha1(chunk));
    }
    let mut info = Vec::new();
    info.extend(b"d6:lengthi");
    info.extend(data.len().to_string().bytes());
    info.extend(b"e4:name");
    info.extend(format!("{}:{}", name.len(), name).bytes());
    info.extend(b"12:piece lengthi");
    info.extend(piece_len.to_string().bytes());
    info.extend(b"e6:pieces");
    info.extend(format!("{}:", pieces.len()).bytes());
    info.extend(&pieces);
    info.push(b'e');
    let id = trs_storage::hash::sha1(&info);
    let mut bytes = b"d4:info".to_vec();
    bytes.extend(&info);
    bytes.push(b'e');
    TestTorrent {
        bytes,
        id,
        name: name.to_owned(),
        piece_len,
        pieces: (pieces.len() / 20) as u32,
        files: vec![(vec![name.to_owned()], data)],
        multi: false,
    }
}

impl Lab {
    fn banner(&self, who: &str) {
        eprintln!(
            "\n=== {who}: {} MiB, {:.0} s, pieces {} MiB, swarm {} MB/s (real engine, loopback HTTP) ===",
            self.file_len >> 20,
            self.duration,
            self.piece_mib,
            env_u64("TRS_RATE_MBS", 6)
        );
    }

    async fn done_pct(&self) -> f64 {
        let r = self.leech.row(&self.id).await.unwrap();
        r.done_bytes as f64 * 100.0 / r.total_bytes.max(1) as f64
    }
}

async fn ffmpeg_run(url: &str, ss: Option<f64>) -> (Instant, Instant, bool) {
    let t0 = Instant::now();
    let mut c = Command::new(FFMPEG);
    c.args(["-nostdin", "-loglevel", "error"]);
    if let Some(s) = ss {
        c.args(["-ss", &s.to_string()]);
    }
    c.args(["-i", url, "-map", "0:v:0", "-frames:v", "1", "-f", "null", "-"]);
    c.kill_on_drop(true);
    let st = tokio::time::timeout(Duration::from_secs(90), c.status()).await.expect("ffmpeg timed out").unwrap();
    (t0, Instant::now(), st.success())
}

async fn libav_case(var: &str, name: &str) {
    let Some(path) = env(var) else { return eprintln!("{var} not set: skipping") };
    let lab = lab(&path, name, 600.0).await;
    lab.banner(&format!("libavformat/ffmpeg (mpv, IINA engine), {name}"));
    let (t0, t1, ok) = ffmpeg_run(&lab.url, None).await;
    assert!(ok);
    eprintln!("{}", verdict("libav start -> first decoded frame", ms(t0, t1), 5000.0));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let before = lab.done_pct().await;
    let (t0, t1, ok) = ffmpeg_run(&lab.url, Some(lab.duration * 0.7)).await;
    assert!(ok);
    eprintln!("{} (downloaded {before:.1}% at the seek)", verdict("libav seek to 70% -> first frame (new process, includes reopening)", ms(t0, t1), 4000.0));
    // A second seek, further down, with the first still streaming in the background.
    let (t0, t1, ok) = ffmpeg_run(&lab.url, Some(lab.duration * 0.35)).await;
    assert!(ok);
    eprintln!("{}", verdict("libav seek to 35% -> first frame", ms(t0, t1), 4000.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg and TRS_FX_MP4; prints latency numbers"]
async fn real_engine_libavformat_mp4() {
    libav_case("TRS_FX_MP4", "movie.mp4").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg and TRS_FX_MKV; prints latency numbers"]
async fn real_engine_libavformat_mkv() {
    libav_case("TRS_FX_MKV", "movie.mkv").await;
}

fn json_num(line: &str, key: &str) -> Option<f64> {
    let i = line.find(&format!("\"{key}\""))?;
    let rest = line[i + key.len() + 2..].trim_start_matches([':', ' ']);
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs swiftc, AVFoundation and TRS_FX_MP4; prints latency numbers"]
async fn real_engine_avfoundation_mp4() {
    let Some(path) = env("TRS_FX_MP4") else { return eprintln!("TRS_FX_MP4 not set: skipping") };
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../stream/tests/players/avf.swift");
    let bin = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("avf-probe");
    let st = Command::new("swiftc").arg("-O").arg(&src).arg("-o").arg(&bin).status().await.expect("swiftc");
    assert!(st.success());
    let lab = lab(&path, "movie.mp4", 600.0).await;
    lab.banner("AVFoundation (QuickTime Player's engine), MP4 moov at end");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let seek_to = lab.duration * 0.7;
    let mut child = Command::new(&bin).args([&lab.url, &seek_to.to_string(), "90"]).stdout(Stdio::piped()).kill_on_drop(true).spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let (mut start, mut dur, mut first, mut si, mut sp) = (None, None, None, None, None);
    while let Ok(Some(l)) = tokio::time::timeout(Duration::from_secs(100), lines.next_line()).await.unwrap_or(Ok(None)) {
        eprintln!("  avf: {l}");
        start = start.or(json_num(&l, "start_ms"));
        dur = dur.or(json_num(&l, "duration_loaded_ms"));
        first = first.or(json_num(&l, "first_play_ms"));
        si = si.or(json_num(&l, "seek_issued_ms"));
        sp = sp.or(json_num(&l, "seek_playing_ms"));
        assert!(!l.contains("\"error\""), "{l}");
        if l.contains("done_ms") {
            break;
        }
    }
    let _ = child.kill().await;
    let _ = UNIX_EPOCH.duration_since(UNIX_EPOCH);
    let _ = SystemTime::now();
    let (s, d, f, si, sp) = (start.unwrap(), dur.unwrap(), first.unwrap(), si.unwrap(), sp.unwrap());
    eprintln!("{}", verdict("AVFoundation start -> duration known", d - s, 5000.0));
    eprintln!("{}", verdict("AVFoundation start -> first frame playing", f - s, 5000.0));
    eprintln!("{}", verdict("AVFoundation seek to 70% -> playing again", sp - si, 4000.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a VLC binary that runs on this CPU (TRS_VLC) and TRS_FX_MP4/TRS_FX_MKV; prints latency numbers"]
async fn real_engine_vlc() {
    let Some(vlc) = env("TRS_VLC") else { return eprintln!("TRS_VLC not set: skipping") };
    for (var, name) in [("TRS_FX_MP4", "movie.mp4"), ("TRS_FX_MKV", "movie.mkv")] {
        let Some(path) = env(var) else { continue };
        let lab = lab(&path, name, 600.0).await;
        lab.banner(&format!("VLC headless (rc interface), {name}"));
        let mut c = Command::new(&vlc)
            .args(["-I", "rc", "--rc-fake-tty", "--no-video-title-show", "--vout=dummy", "--aout=dummy", "--play-and-exit"])
            .arg(&lab.url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdin = c.stdin.take().unwrap();
        let mut out = BufReader::new(c.stdout.take().unwrap()).lines();
        let t0 = Instant::now();
        let mut t_first = None;
        let end = Instant::now() + Duration::from_secs(60);
        while Instant::now() < end && t_first.is_none() {
            stdin.write_all(b"get_time\n").await.unwrap();
            if let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_millis(500), out.next_line()).await
                && l.trim().trim_start_matches("> ").parse::<i64>().is_ok_and(|t| t >= 1)
            {
                t_first = Some(Instant::now());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let t_first = t_first.expect("VLC never advanced");
        eprintln!("{}", verdict(&format!("VLC {name} start -> playback time advancing"), ms(t0, t_first), 5000.0));
        tokio::time::sleep(Duration::from_secs(3)).await;
        let target = (lab.duration * 0.7) as i64;
        let before = lab.done_pct().await;
        let t_seek = Instant::now();
        stdin.write_all(format!("seek {target}\n").as_bytes()).await.unwrap();
        let end = Instant::now() + Duration::from_secs(40);
        let mut t_after = None;
        while Instant::now() < end && t_after.is_none() {
            stdin.write_all(b"get_time\n").await.unwrap();
            if let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_millis(500), out.next_line()).await
                && l.trim().trim_start_matches("> ").parse::<i64>().is_ok_and(|t| t > target)
            {
                t_after = Some(Instant::now());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = stdin.write_all(b"quit\n").await;
        let _ = tokio::time::timeout(Duration::from_secs(5), c.wait()).await;
        let _ = c.kill().await;
        let t_after = t_after.expect("VLC never resumed after the seek");
        eprintln!("{} (downloaded {before:.1}% at the seek)", verdict(&format!("VLC {name} seek to 70% -> playing at target"), ms(t_seek, t_after), 4000.0));
    }
}
