//! The container probes and the index-first behaviour against real ffmpeg output.

use crate::common;

use common::client::eventually;
use common::fake::FakeSource;
use common::fixtures::*;
use common::*;
use std::path::Path;
use std::process::Command;
use std::time::Duration;
use trs_stream::container::{Fetched, Probe, probe_mkv, probe_mp4};
use trs_stream::{Config, FileRange, PlanConfig, Priority};

fn ffprobe_duration(p: &Path) -> f64 {
    let out = Command::new("/opt/homebrew/bin/ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(p)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// Independent scan of top-level MP4 atoms: (kind, offset, size).
fn mp4_atoms(data: &[u8]) -> Vec<(String, u64, u64)> {
    let mut v = Vec::new();
    let mut off = 0usize;
    while off + 8 <= data.len() {
        let mut size = u32::from_be_bytes(data[off..off + 4].try_into().unwrap()) as u64;
        if size == 1 {
            size = u64::from_be_bytes(data[off + 8..off + 16].try_into().unwrap());
        }
        v.push((String::from_utf8_lossy(&data[off + 4..off + 8]).to_string(), off as u64, size));
        off += size as usize;
    }
    v
}

/// Drive a probe over a real file, returning the result and the (offset, len) fetches it asked for.
fn drive(path: &Path, p: fn(u64, &Fetched) -> Probe) -> (trs_stream::container::ContainerInfo, Vec<(u64, usize)>) {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::open(path).unwrap();
    let len = f.metadata().unwrap().len();
    let mut fetched = Fetched::new();
    let mut needs = Vec::new();
    for _ in 0..70 {
        match p(len, &fetched) {
            Probe::Done(i) => {
                assert!(fetched.bytes() < 256 * 1024, "the probe reads only headers ({} bytes)", fetched.bytes());
                return (i, needs);
            }
            Probe::Need { offset, len } => {
                needs.push((offset, len));
                let mut b = vec![0u8; len];
                f.read_exact_at(&mut b, offset).unwrap();
                fetched.add(offset, b);
            }
        }
    }
    panic!("probe did not terminate");
}

#[test]
fn mp4_moov_at_end_real_file() {
    let Some(p) = fixture(Fx::Mp4End, 600) else { return };
    let data = std::fs::read(&p).unwrap();
    let atoms = mp4_atoms(&data);
    let order: Vec<&str> = atoms.iter().map(|a| a.0.as_str()).collect();
    assert_eq!(order.last(), Some(&"moov"), "fixture must have moov at the end: {order:?}");
    let moov = atoms.iter().find(|a| a.0 == "moov").unwrap();
    let (info, needs) = drive(&p, probe_mp4);
    assert_eq!(info.index, vec![moov.1..moov.1 + moov.2]);
    let d = info.duration.unwrap().as_secs_f64();
    assert!((d - ffprobe_duration(&p)).abs() < 0.05, "{d}");
    assert!((d - 600.0).abs() < 1.0);
    // It skipped the whole mdat: the head was read at 0, then the tail at the moov.
    assert!(needs.iter().any(|n| n.0 == moov.1));
    assert!(needs.len() <= 6, "{needs:?}");
}

#[test]
fn mp4_faststart_real_file() {
    let Some(p) = fixture(Fx::Mp4Fast, 20) else { return };
    let data = std::fs::read(&p).unwrap();
    let atoms = mp4_atoms(&data);
    let moov = atoms.iter().find(|a| a.0 == "moov").unwrap();
    assert!(moov.1 < 100, "faststart puts moov first");
    let (info, needs) = drive(&p, probe_mp4);
    assert_eq!(info.index, vec![moov.1..moov.1 + moov.2]);
    assert!((info.duration.unwrap().as_secs_f64() - ffprobe_duration(&p)).abs() < 0.05);
    assert!(needs.iter().all(|n| n.0 < 4096), "everything is in the head: {needs:?}");
}

#[test]
fn mkv_cues_at_end_real_file() {
    let Some(p) = fixture(Fx::MkvCues, 600) else { return };
    let data = std::fs::read(&p).unwrap();
    let (info, needs) = drive(&p, probe_mkv);
    let d = info.duration.expect("Segment/Info/Duration").as_secs_f64();
    assert!((d - ffprobe_duration(&p)).abs() < 0.05, "{d}");
    assert_eq!(info.index.len(), 1, "one Cues range");
    let r = &info.index[0];
    assert_eq!(&data[r.start as usize..r.start as usize + 4], &[0x1C, 0x53, 0xBB, 0x6B], "range starts at the Cues element");
    assert!(r.start > data.len() as u64 / 2, "Cues are at the end of this file");
    assert_eq!(r.end, data.len() as u64);
    assert!(needs.len() <= 4, "{needs:?}");
}

#[test]
fn probes_on_truncated_real_files_do_not_panic() {
    let Some(p) = fixture(Fx::Mp4End, 20) else { return };
    let Some(m) = fixture(Fx::MkvCues, 20) else { return };
    for (path, probe) in [(&p, probe_mp4 as fn(u64, &Fetched) -> Probe), (&m, probe_mkv)] {
        let data = std::fs::read(path).unwrap();
        for cut in [0usize, 5, 11, 40, 100, 1000, 70_000, data.len() / 2] {
            let cutd = &data[..cut];
            let mut f = Fetched::new();
            for _ in 0..70 {
                match probe(cut as u64, &f) {
                    Probe::Done(_) => break,
                    Probe::Need { offset, len } => f.add(offset, cutd[offset as usize..offset as usize + len].to_vec()),
                }
            }
        }
    }
}

fn file_of(p: &Path, name: &str) -> FileRange {
    FileRange { index: 0, name: name.into(), offset: 0, len: std::fs::metadata(p).unwrap().len() }
}

/// Open a real file with a tiny head/tail so the index can only be found by parsing, and watch
/// what the stream asks for before any player connects.
async fn index_first(kind: Fx, name: &str, expect_start_of_index: impl Fn(&[u8], u64) -> u64) {
    let Some(p) = fixture(kind, 60) else { return };
    let data = std::fs::read(&p).unwrap();
    let cfg = Config { plan: PlanConfig { index_bytes: 64 * 1024, ..PlanConfig::default() }, ..Config::default() };
    let (srv, _ev) = server_with(cfg).await;
    let piece = 256 * 1024u32;
    let src = FakeSource::from_file(&p, piece, 6 * MIB);
    let f = file_of(&p, name);
    let h = srv.open(src.clone(), f.clone(), 1).unwrap();

    // Duration appears from the container, not from a guess.
    assert!(eventually(10.0, || srv.duration_of(&h.token).is_some()).await, "duration never found");
    let d = srv.duration_of(&h.token).unwrap().as_secs_f64();
    assert!((d - ffprobe_duration(&p)).abs() < 0.1, "{d}");

    // The index range was marked urgent with deadline 0, before any cursor existed.
    let idx_start = expect_start_of_index(&data, f.len);
    let idx_piece = (idx_start / u64::from(piece)) as u32;
    let mut seen = false;
    for e in src.hint_log() {
        if let common::fake::HintEvent::Set(_, rs) = e {
            seen |= rs.iter().any(|r| r.priority == Priority::Urgent && r.deadline == Duration::ZERO && r.range().contains(&idx_piece));
        }
    }
    assert!(seen, "moov/Cues piece {idx_piece} was never marked urgent");

    // ...and it arrived early: among the first pieces to land, ahead of the bulk of the file.
    assert!(eventually(10.0, || src.has_piece_at(idx_start)).await);
    let order = src.arrival_order();
    let pos = order.iter().position(|&q| q == idx_piece).unwrap();
    let total = f.len.div_ceil(u64::from(piece)) as usize;
    assert!(pos < 40 && pos < total / 4, "index piece landed {pos}th of {total}");
    assert_eq!(src.unverified_reads(), 0);

    // The bitrate now comes from size/duration: a cursor at 0 gets a window of that many seconds.
    let bps = f.len as f64 / d;
    let mut c = common::client::Client::connect(srv.addr()).await;
    c.send("GET", &common::client::path_of(&h.url), &[("Range", "bytes=0-")]).await;
    let _ = c.read_head().await.unwrap();
    assert!(eventually(2.0, || src.hints_of(h.id).iter().any(|r| r.priority == Priority::Urgent)).await);
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The cursor sits at the first byte the player has not been sent (it may have run ahead through
    // present pieces); its urgent window is that many seconds of playback long.
    let hints = src.hints_of(h.id);
    let w = hints.iter().filter(|r| r.priority == Priority::Urgent && r.deadline == Duration::ZERO).min_by_key(|r| r.pieces.0).unwrap();
    let want = (bps * 30.0).max(8.0 * 1048576.0);
    let got = f64::from(w.pieces.1 - w.pieces.0) * f64::from(piece);
    assert!((got - want).abs() <= f64::from(piece) * 3.0 + 1.0 || w.pieces.1 as usize == total, "window {got} vs {want}: {hints:?}");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_finds_moov_at_end_and_fetches_it_first() {
    index_first(Fx::Mp4End, "clip.mp4", |data, _| mp4_atoms(data).iter().find(|a| a.0 == "moov").unwrap().1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_finds_mkv_cues_at_end_and_fetches_them_first() {
    index_first(Fx::MkvCues, "clip.mkv", |data, len| {
        let p = data.windows(4).rposition(|w| w == [0x1C, 0x53, 0xBB, 0x6B]).unwrap() as u64;
        assert!(p < len);
        p
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_mp4_faststart_index_is_at_the_head() {
    let Some(p) = fixture(Fx::Mp4Fast, 20) else { return };
    let (srv, _ev) = server().await;
    let src = FakeSource::from_file(&p, 256 * 1024, 20 * MIB);
    let h = srv.open(src.clone(), file_of(&p, "clip.mp4"), 1).unwrap();
    assert!(eventually(5.0, || srv.duration_of(&h.token).is_some()).await);
    assert!(eventually(5.0, || src.active_ids().is_empty()).await, "index done, hints released");
    srv.shutdown().await;
}
