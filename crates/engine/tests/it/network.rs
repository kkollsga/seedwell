//! Real-swarm test: downloads the Debian netinst ISO (a legal Linux distro torrent) with the
//! public tracker and DHT. Ignored by default; run with
//! `cargo test -p trs-engine --test it --release network:: -- --ignored --nocapture`.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_engine::*;

fn peak_rss_bytes() -> u64 {
    // SAFETY: getrusage only writes into the zeroed struct we pass.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    // macOS reports bytes, Linux kilobytes.
    if cfg!(target_os = "macos") { ru.ru_maxrss as u64 } else { ru.ru_maxrss as u64 * 1024 }
}

#[ignore = "downloads ~750 MB from the public Debian swarm"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_debian_netinst() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let torrent = std::fs::read(root.join("bench/torrents/debian-netinst.torrent")).expect("torrent file");
    let out = root.join("bench/downloads/trs-engine");
    std::fs::create_dir_all(&out).unwrap();
    let cfg = EngineConfig {
        default_save_dir: out.clone(),
        listen_addr: "0.0.0.0".parse().unwrap(),
        ..EngineConfig::default()
    };
    let engine = Engine::start(cfg, Arc::new(MemStore::new())).await.unwrap();
    let h = engine.handle();
    let added = h.add_torrent_file(torrent, AddOptions::default()).await.unwrap();
    eprintln!("[net] added {} (listening on {})", added.name, h.listen_port());
    let t0 = Instant::now();
    let mut last = (Instant::now(), 0u64);
    let mut peak_rate = 0u64;
    let deadline = Instant::now() + Duration::from_secs(45 * 60);
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let snap = h.snapshot().await.unwrap();
        let r = snap.rows.first().unwrap();
        peak_rate = peak_rate.max(r.down_rate);
        if last.0.elapsed() >= Duration::from_secs(10) {
            let inst = (r.done_bytes - last.1) as f64 / last.0.elapsed().as_secs_f64();
            eprintln!(
                "[net] {:?} {:.1}% peers={} rate={:.1} MB/s (window {:.1} MB/s) rss={} MB",
                r.state,
                100.0 * r.done_bytes as f64 / r.total_bytes.max(1) as f64,
                r.peers,
                r.down_rate as f64 / 1e6,
                inst / 1e6,
                peak_rss_bytes() / 1_000_000
            );
            last = (Instant::now(), r.done_bytes);
        }
        if matches!(r.state, TorrentState::Seeding | TorrentState::LowPower) {
            break;
        }
        assert!(Instant::now() < deadline, "download did not finish in 45 minutes");
        assert!(!matches!(r.state, TorrentState::Error | TorrentState::MissingData), "{:?}", r.error);
    }
    let secs = t0.elapsed().as_secs_f64();
    let total = h.snapshot().await.unwrap().rows[0].total_bytes;
    eprintln!(
        "[net] DONE {} bytes in {secs:.0} s = {:.2} MB/s average ({:.1} Mbit/s), peak instantaneous {:.1} MB/s, peak RSS {} MB",
        total,
        total as f64 / secs / 1e6,
        total as f64 * 8.0 / secs / 1e6,
        peak_rate as f64 / 1e6,
        peak_rss_bytes() / 1_000_000
    );
    let report = engine.shutdown(Duration::from_secs(30)).await;
    eprintln!("[net] shutdown: {report:?}");
    // Legal Linux ISO, but there is no reason to keep 750 MB around.
    let _ = std::fs::remove_dir_all(&out);
}
