//! Real players against the server and a fake swarm at a fixed rate. All `#[ignore]`: they need
//! the players installed (and a GUI session for IINA), take seconds each, and print a report.
//!
//!   cargo test -p trs-stream --test players -- --ignored --nocapture --test-threads=1
//!
//! Budgets (SPEC 7F.4, healthy swarm at >= 5 MB/s): first frame <= 5 s, seek <= 4 s.
//! Environment: `TRS_PIECE_MIB` (default 4) piece size, `TRS_RATE_MBS` (default 6) swarm MB/s.

use crate::common;

use common::client::eventually;
use common::fake::FakeSource;
use common::fixtures::*;
use common::*;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use trs_stream::{Config, EndReason, Event, FileRange, StreamHandle, StreamServer};

/// VLC to run: `TRS_VLC` (e.g. a universal build unpacked elsewhere) or the one in /Applications.
fn vlc() -> String {
    std::env::var("TRS_VLC").unwrap_or_else(|_| "/Applications/VLC.app/Contents/MacOS/VLC".into())
}

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}

struct Rec {
    ev: Event,
    /// Was the first piece of the request's range already verified when it arrived?
    start_piece_present: Option<bool>,
}

struct Lab {
    srv: StreamServer,
    src: Arc<FakeSource>,
    h: StreamHandle,
    recs: Arc<Mutex<Vec<Rec>>>,
    file_len: u64,
    duration: f64,
    piece: u64,
    rate: u64,
    anchor: (Instant, f64),
}

async fn lab(kind: Fx, name: &str, secs: u32) -> Option<Lab> {
    let path: PathBuf = fixture(kind, secs)?;
    let piece = env_u64("TRS_PIECE_MIB", 4) * MIB;
    let rate = env_u64("TRS_RATE_MBS", 6) * 1_000_000;
    let src = FakeSource::from_file(&path, piece as u32, rate);
    let recs: Arc<Mutex<Vec<Rec>>> = Arc::new(Mutex::new(Vec::new()));
    let (r2, w): (_, Weak<FakeSource>) = (recs.clone(), Arc::downgrade(&src));
    let cfg = Config {
        on_event: Some(Arc::new(move |e| {
            let present = match (e, w.upgrade()) {
                (Event::Request { start, status, .. }, Some(s)) if *status < 300 => {
                    use trs_stream::PieceSource;
                    Some(s.has((*start / piece) as u32))
                }
                _ => None,
            };
            r2.lock().unwrap().push(Rec { ev: e.clone(), start_piece_present: present });
        })),
        ..Config::default()
    };
    let srv = StreamServer::start(cfg).await.unwrap();
    let file_len = std::fs::metadata(&path).unwrap().len();
    let h = srv.open(src.clone(), FileRange { index: 0, name: name.into(), offset: 0, len: file_len }, 1).unwrap();
    let duration = secs as f64;
    let anchor = (Instant::now(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64() * 1000.0);
    Some(Lab { srv, src, h, recs, file_len, duration, piece, rate, anchor })
}

impl Lab {
    fn unix_ms(&self, t: Instant) -> f64 {
        self.anchor.1 + t.duration_since(self.anchor.0).as_secs_f64() * 1000.0
    }
    fn since(&self, t: Instant) -> f64 {
        t.duration_since(self.anchor.0).as_secs_f64() * 1000.0
    }

    fn report(&self) -> String {
        let recs = self.recs.lock().unwrap();
        let mut out = String::new();
        let mut sent: std::collections::HashMap<u64, u64> = Default::default();
        for r in recs.iter() {
            match &r.ev {
                Event::Request { req, start, end_excl, range, status, at, head_only, .. } => out.push_str(&format!(
                    "  +{:>7.0} ms  req#{req} {} {:?} -> {status} {}..{} ({:.1}%) present={:?}\n",
                    self.since(*at),
                    if *head_only { "HEAD" } else { "GET " },
                    range.as_deref().unwrap_or("-"),
                    start,
                    end_excl,
                    *start as f64 * 100.0 / self.file_len as f64,
                    r.start_piece_present
                )),
                Event::Chunk { req, len, .. } => *sent.entry(*req).or_default() += *len as u64,
                Event::End { req, reason, sent: s, at } => out.push_str(&format!("  +{:>7.0} ms  req#{req} end {reason:?} after {s} B\n", self.since(*at))),
            }
        }
        out
    }

    /// First request time.
    fn first_request(&self) -> Option<Instant> {
        self.recs.lock().unwrap().iter().find_map(|r| if let Event::Request { at, .. } = &r.ev { Some(*at) } else { None })
    }

    /// Time of the first chunk that starts at or beyond `min_offset` and before the tail region.
    fn first_chunk_past(&self, min_offset: u64) -> Option<Instant> {
        let tail = self.file_len.saturating_sub(4 * MIB);
        self.recs.lock().unwrap().iter().find_map(|r| match &r.ev {
            Event::Chunk { offset, at, .. } if *offset >= min_offset && *offset < tail => Some(*at),
            _ => None,
        })
    }

    /// The request that is the seek: the last GET starting in `lo..hi` (file offsets). Returns
    /// (request time, time the request had streamed >= `bytes`, start piece was present).
    fn seek_request(&self, lo: u64, hi: u64, bytes: u64) -> Option<(Instant, Option<Instant>, bool)> {
        let recs = self.recs.lock().unwrap();
        let (req, at, present) = recs.iter().rev().find_map(|r| match &r.ev {
            Event::Request { req, start, at, head_only: false, status, .. } if *status < 300 && (lo..hi).contains(start) => {
                Some((*req, *at, r.start_piece_present.unwrap_or(false)))
            }
            _ => None,
        })?;
        let mut cum = 0u64;
        let mut steady = None;
        for r in recs.iter() {
            if let Event::Chunk { req: q, len, at, .. } = &r.ev
                && *q == req
            {
                cum += *len as u64;
                if cum >= bytes && steady.is_none() {
                    steady = Some(*at);
                }
            }
        }
        Some((at, steady, present))
    }

    /// A seek target (fraction of the file) whose piece and neighbours are not yet downloaded.
    fn unfetched_fraction(&self) -> f64 {
        use trs_stream::PieceSource;
        for f in [0.70, 0.66, 0.74, 0.62, 0.78, 0.58, 0.82, 0.5, 0.9] {
            let p = (self.file_len as f64 * f / self.piece as f64) as u32;
            if (p.saturating_sub(1)..=p + 1).all(|q| !self.src.has(q)) {
                return f;
            }
        }
        0.7
    }

    fn bytes_per_sec(&self) -> f64 {
        self.file_len as f64 / self.duration
    }
}

fn ms(a: Instant, b: Instant) -> f64 {
    b.duration_since(a).as_secs_f64() * 1000.0
}

fn verdict(name: &str, value_ms: f64, budget_ms: f64) -> String {
    format!("{name}: {value_ms:.0} ms (budget {budget_ms:.0} ms) {}", if value_ms <= budget_ms { "OK" } else { "OVER" })
}

fn banner(lab: &Lab, player: &str) {
    eprintln!(
        "\n=== {player}: {} MiB file, {:.0} s, {:.2} MB/s stream, pieces {} MiB, swarm {:.1} MB/s ===",
        lab.file_len / MIB,
        lab.duration,
        lab.bytes_per_sec() / 1e6,
        lab.piece / MIB,
        lab.rate as f64 / 1e6
    );
}

// ------------------------------------------------------------------------------------------
// libavformat (the engine inside mpv and IINA) via ffmpeg: always available where ffmpeg is.
// ------------------------------------------------------------------------------------------

async fn ffmpeg_run(url: &str, ss: Option<f64>, frames: u32) -> (Instant, Instant, bool) {
    let t0 = Instant::now();
    let mut c = Command::new(FFMPEG);
    c.args(["-nostdin", "-loglevel", "error"]);
    if let Some(s) = ss {
        c.args(["-ss", &s.to_string()]);
    }
    c.args(["-i", url, "-map", "0:v:0", "-frames:v", &frames.to_string(), "-f", "null", "-"]);
    c.kill_on_drop(true);
    let st = tokio::time::timeout(Duration::from_secs(60), c.status()).await.expect("ffmpeg timed out").unwrap();
    (t0, Instant::now(), st.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg; prints latency numbers"]
async fn libavformat_first_frame_and_seek_mp4_moov_at_end() {
    let Some(lab) = lab(Fx::Mp4End, "movie.mp4", 600).await else { return };
    banner(&lab, "libavformat/ffmpeg (mpv, IINA engine), MP4 moov at end");
    let (t0, t1, ok) = ffmpeg_run(&lab.h.url, None, 1).await;
    assert!(ok);
    let first = lab.first_request().unwrap();
    eprintln!("{}", verdict("start -> first decoded frame", ms(t0, t1), 5000.0));
    if let Some(t) = lab.first_chunk_past(2 * MIB) {
        eprintln!("first request -> first byte served past the head index: {:.0} ms", ms(first, t));
    }
    // Let the swarm run a while so the seek is to a point that is mostly unfetched.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let frac = lab.unfetched_fraction();
    let lo = (lab.file_len as f64 * (frac - 0.08)) as u64;
    let hi = (lab.file_len as f64 * (frac + 0.08)) as u64;
    let t0 = Instant::now();
    let (_, t1, ok) = ffmpeg_run(&lab.h.url, Some(lab.duration * frac), 1).await;
    assert!(ok);
    if let Some((req, steady, present)) = lab.seek_request(lo, hi, (lab.bytes_per_sec() * 2.0) as u64) {
        eprintln!("seek request arrived {:.0} ms after the process started (start piece already present: {present})", ms(t0, req));
        if let Some(s) = steady {
            eprintln!("{}", verdict("seek request -> 2 s of media served", ms(req, s), 4000.0));
        }
        eprintln!("{}", verdict("seek request -> first frame decoded (process exit)", ms(req, t1), 4000.0));
    } else {
        eprintln!("no ranged request found near the seek target");
    }
    eprintln!("{}", lab.report());
    eprintln!("pieces have: {} arrival {:?}", lab.src.piece_count_have(), lab.src.arrival_order());
    assert_eq!(lab.src.unverified_reads(), 0);
    lab.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs ffmpeg; prints latency numbers"]
async fn libavformat_first_frame_and_seek_mkv_cues_at_end() {
    let Some(lab) = lab(Fx::MkvCues, "movie.mkv", 600).await else { return };
    banner(&lab, "libavformat/ffmpeg (mpv, IINA engine), MKV cues at end");
    let (t0, t1, ok) = ffmpeg_run(&lab.h.url, None, 1).await;
    assert!(ok);
    eprintln!("{}", verdict("start -> first decoded frame", ms(t0, t1), 5000.0));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let frac = lab.unfetched_fraction();
    let lo = (lab.file_len as f64 * (frac - 0.08)) as u64;
    let hi = (lab.file_len as f64 * (frac + 0.08)) as u64;
    let (_, t1, ok) = ffmpeg_run(&lab.h.url, Some(lab.duration * frac), 1).await;
    assert!(ok);
    if let Some((req, steady, present)) = lab.seek_request(lo, hi, (lab.bytes_per_sec() * 2.0) as u64) {
        eprintln!("(start piece already present: {present})");
        if let Some(s) = steady {
            eprintln!("{}", verdict("seek request -> 2 s of media served", ms(req, s), 4000.0));
        }
        eprintln!("{}", verdict("seek request -> first frame decoded (process exit)", ms(req, t1), 4000.0));
    }
    eprintln!("{}", lab.report());
    lab.srv.shutdown().await;
}

// ------------------------------------------------------------------------------------------
// AVFoundation (QuickTime Player's engine) through a small Swift program.
// ------------------------------------------------------------------------------------------

async fn build_avf() -> Option<PathBuf> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/players/avf.swift");
    let out = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("avf-probe");
    if out.exists() && std::fs::metadata(&out).and_then(|m| m.modified()).ok() > std::fs::metadata(&src).and_then(|m| m.modified()).ok() {
        return Some(out);
    }
    let st = Command::new("swiftc").arg("-O").arg(&src).arg("-o").arg(&out).status().await.ok()?;
    st.success().then_some(out)
}

fn json_num(line: &str, key: &str) -> Option<f64> {
    let i = line.find(&format!("\"{key}\""))?;
    let rest = &line[i + key.len() + 2..];
    let rest = rest.trim_start_matches([':', ' ']);
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    rest[..end].trim().parse().ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs swiftc and AVFoundation; prints latency numbers"]
async fn avfoundation_plays_and_seeks_mp4_moov_at_end() {
    avf_case(Fx::Mp4End, "movie.mp4").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs swiftc and AVFoundation; prints latency numbers"]
async fn avfoundation_plays_and_seeks_mp4_faststart() {
    avf_case(Fx::Mp4Fast, "movie.mp4").await;
}

async fn avf_case(kind: Fx, name: &str) {
    let Some(bin) = build_avf().await else { return eprintln!("swiftc unavailable: skipping") };
    let Some(lab) = lab(kind, name, 600).await else { return };
    banner(&lab, &format!("AVFoundation ({kind:?})"));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let seek_to = lab.duration * lab.unfetched_fraction();
    let mut child = Command::new(&bin)
        .args([&lab.h.url, &seek_to.to_string(), "60"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let (mut start, mut dur_loaded, mut first_play, mut seek_issued, mut seek_playing) = (None, None, None, None, None);
    let mut err = None;
    while let Ok(Some(l)) = tokio::time::timeout(Duration::from_secs(70), lines.next_line()).await.unwrap_or(Ok(None)) {
        eprintln!("  avf: {l}");
        start = start.or(json_num(&l, "start_ms"));
        dur_loaded = dur_loaded.or(json_num(&l, "duration_loaded_ms"));
        first_play = first_play.or(json_num(&l, "first_play_ms"));
        seek_issued = seek_issued.or(json_num(&l, "seek_issued_ms"));
        seek_playing = seek_playing.or(json_num(&l, "seek_playing_ms"));
        if l.contains("\"error\"") {
            err = Some(l.clone());
        }
        if l.contains("done_ms") {
            break;
        }
    }
    let _ = child.kill().await;
    eprintln!("{}", lab.report());
    assert!(err.is_none(), "{err:?}");
    let (s, d, f, si, sp) = (start.unwrap(), dur_loaded.unwrap(), first_play.unwrap(), seek_issued.unwrap(), seek_playing.unwrap());
    eprintln!("{}", verdict("start -> duration known", d - s, 5000.0));
    eprintln!("{}", verdict("start -> first frame playing", f - s, 5000.0));
    eprintln!("{}", verdict(&format!("seek to {seek_to:.0} s -> playing again"), sp - si, 4000.0));
    let _ = lab.unix_ms(Instant::now());
    // Every response AVFoundation got was exact, and none of it was unverified.
    assert_eq!(lab.src.unverified_reads(), 0);
    lab.srv.shutdown().await;
}

// ------------------------------------------------------------------------------------------
// VLC (headless). The binary in /Applications is x86_64-only on some installs; the test skips
// when it cannot run (no Rosetta).
// ------------------------------------------------------------------------------------------

async fn vlc_runs() -> bool {
    matches!(Command::new(vlc()).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().await, Ok(s) if s.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs VLC that can execute on this CPU; prints latency numbers"]
async fn vlc_headless_first_frame_and_rc_seek() {
    if !vlc_runs().await {
        return eprintln!("VLC cannot run here ({}): skipping", vlc());
    }
    let Some(lab) = lab(Fx::Mp4End, "movie.mp4", 600).await else { return };
    banner(&lab, "VLC headless (rc interface)");
    let mut c = Command::new(vlc())
        .args(["-I", "rc", "--rc-fake-tty", "--no-video-title-show", "--vout=dummy", "--aout=dummy", "--play-and-exit"])
        .arg(&lab.h.url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = c.stdin.take().unwrap();
    let mut out = BufReader::new(c.stdout.take().unwrap()).lines();
    let t0 = Instant::now();
    // Poll get_time until playback has advanced.
    let mut t_first = None;
    let mut deadline = Instant::now() + Duration::from_secs(40);
    let mut last_time = 0i64;
    while Instant::now() < deadline && t_first.is_none() {
        stdin.write_all(b"get_time\n").await.unwrap();
        if let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_millis(500), out.next_line()).await
            && let Ok(t) = l.trim().trim_start_matches("> ").parse::<i64>()
        {
            last_time = t;
            if t >= 1 {
                t_first = Some(Instant::now());
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let t_first = t_first.expect("VLC never advanced");
    eprintln!("{}", verdict("start -> playback time advancing", ms(t0, t_first), 5000.0));
    tokio::time::sleep(Duration::from_secs(3)).await;
    let target = (lab.duration * 0.7) as i64;
    let t_seek = Instant::now();
    stdin.write_all(format!("seek {target}\n").as_bytes()).await.unwrap();
    deadline = Instant::now() + Duration::from_secs(30);
    let mut t_after = None;
    while Instant::now() < deadline && t_after.is_none() {
        stdin.write_all(b"get_time\n").await.unwrap();
        if let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_millis(500), out.next_line()).await
            && let Ok(t) = l.trim().trim_start_matches("> ").parse::<i64>()
            && t > target
        {
            t_after = Some(Instant::now());
            last_time = t;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = stdin.write_all(b"quit\n").await;
    let _ = tokio::time::timeout(Duration::from_secs(5), c.wait()).await;
    let _ = c.kill().await;
    eprintln!("{}", lab.report());
    let t_after = t_after.expect("VLC never resumed after the seek");
    eprintln!("{} (last time {last_time})", verdict("seek command -> playing at target", ms(t_seek, t_after), 4000.0));
    lab.srv.shutdown().await;
}

// ------------------------------------------------------------------------------------------
// IINA (GUI). Launched the way the app will (`iina://weblink?url=`); reports the requests it
// issues, then quits it. Needs a logged-in GUI session: without one IINA starts but never
// requests anything, and the test reports that instead of failing.
// ------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "launches the IINA app (needs a GUI session); prints requests"]
async fn iina_plays_url_and_issues_ranges() {
    if !std::path::Path::new("/Applications/IINA.app").exists() {
        return eprintln!("IINA not installed: skipping");
    }
    let Some(lab) = lab(Fx::Mp4End, "movie.mp4", 600).await else { return };
    banner(&lab, "IINA (iina://weblink)");
    let enc: String = lab.h.url.bytes().map(|b| if b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_' { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    let t0 = Instant::now();
    let st = Command::new("open").arg(format!("iina://weblink?url={enc}")).status().await.unwrap();
    assert!(st.success());
    let played = eventually(30.0, || lab.recs.lock().unwrap().iter().any(|r| matches!(&r.ev, Event::Chunk { offset, .. } if *offset >= 2 * MIB))).await;
    if played {
        eprintln!("IINA requested data; first byte past the index {:.0} ms after launch", ms(t0, lab.first_chunk_past(2 * MIB).unwrap_or(Instant::now())));
        // Let it play a few seconds so seeks/reads show up, then close it.
        tokio::time::sleep(Duration::from_secs(8)).await;
    } else {
        eprintln!("IINA made no request within 30 s (no GUI session, or it is waiting on a dialog)");
    }
    let _ = Command::new("osascript").args(["-e", "tell application \"IINA\" to quit"]).status().await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let _ = Command::new("pkill").args(["-f", "IINA.app/Contents/MacOS/IINA"]).status().await;
    eprintln!("{}", lab.report());
    if played {
        let ranged = lab.recs.lock().unwrap().iter().filter(|r| matches!(&r.ev, Event::Request { status: 206, .. })).count();
        assert!(ranged >= 2, "IINA issued {ranged} ranged requests");
        assert!(lab.recs.lock().unwrap().iter().all(|r| !matches!(&r.ev, Event::End { reason: EndReason::Error, .. })));
    }
    assert_eq!(lab.src.unverified_reads(), 0);
    lab.srv.shutdown().await;
}

/// Serve the 10-minute MP4 for `TRS_SERVE_SECS` (default 60) and print the URL and requests, so a
/// player can be pointed at it by hand.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "manual: prints a URL and serves it"]
async fn manual_serve() {
    let Some(lab) = lab(Fx::Mp4End, "movie.mp4", 600).await else { return };
    eprintln!("URL: {}", lab.h.url);
    tokio::time::sleep(Duration::from_secs(env_u64("TRS_SERVE_SECS", 60))).await;
    eprintln!("{}", lab.report());
}
