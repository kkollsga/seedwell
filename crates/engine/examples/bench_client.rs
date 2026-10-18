//! Headless Seedwell for the comparison benchmark harness (`bench/README.md`, "Seedwell adapter").
//!
//! ```text
//! bench_client add <file.torrent>... [--add <file.torrent>]... \
//!     --download-dir <dir> --state-dir <dir> --listen-port <p> \
//!     [--no-dht] [--no-pex] [--no-lsd] [--no-portmap] [--no-utp]
//! ```
//!
//! Runs in the foreground, prints one JSON object per second on stdout (everything else goes to
//! stderr) and shuts down gracefully (stopped announces, state flushed) on SIGTERM or SIGINT.
#![deny(unsafe_code)]

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use std::io::Write as _;
use tokio::signal::unix::{SignalKind, signal};
use trs_engine::{AddOptions, EngineConfig, EngineHandle, Host, HostOptions, Totals};

/// Live heap bytes, counted only when `TRS_BENCH_DIAG=1` (measurement aid; with it, a diagnostics line
/// goes to stderr every 10 s).
/// Everything the process allocates goes through it, so the number includes tokio, redb and the
/// runtime, and can be set against `phys_footprint` to see what is not heap.
mod heap {
    use std::alloc::{GlobalAlloc, Layout};
    // The allocator the apps install (`trs_storage::MappedAlloc`), so that the benchmark measures it.
    use trs_storage::MappedAlloc as System;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

    pub static ON: AtomicBool = AtomicBool::new(false);
    pub static LIVE: AtomicUsize = AtomicUsize::new(0);
    /// Live allocation count per size class (power of two), printed with the diagnostics line.
    pub static CLASSES: [std::sync::atomic::AtomicIsize; 24] = [const { std::sync::atomic::AtomicIsize::new(0) }; 24];

    /// Cumulative allocation count and bytes per size class, and the peak of `LIVE` (diagnostics).
    pub static CUM: [AtomicUsize; 24] = [const { AtomicUsize::new(0) }; 24];
    pub static PEAK: AtomicUsize = AtomicUsize::new(0);

    pub fn class(size: usize) -> usize {
        (usize::BITS - size.max(1).saturating_sub(1).leading_zeros()).min(23) as usize
    }

    pub struct Counting;

    thread_local! { static IN_PROF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    pub static PROF_LO: AtomicUsize = AtomicUsize::new(usize::MAX);
    pub static PROF_HI: AtomicUsize = AtomicUsize::new(0);
    static PROF_N: AtomicUsize = AtomicUsize::new(0);

    /// Allocation profiler (diagnostics): prints a backtrace for every 8th allocation whose size lies in
    /// `TRS_BENCH_PROF=lo-hi`.
    fn profile(size: usize) {
        if size < PROF_LO.load(Relaxed) || size > PROF_HI.load(Relaxed) || PROF_N.fetch_add(1, Relaxed) % 8 != 0 {
            return;
        }
        if IN_PROF.with(|f| f.replace(true)) {
            return;
        }
        let bt = std::backtrace::Backtrace::force_capture().to_string();
        let mut out = format!("PROF size={size}");
        for l in bt.lines() {
            let l = l.trim();
            if l.starts_with("at ") && !l.contains("/rustc/") && !l.contains("bench_client.rs") && out.len() < 600 {
                out.push(' ');
                out.push_str(l.rsplit('/').next().unwrap_or(l));
            }
        }
        eprintln!("{out}");
        IN_PROF.with(|f| f.set(false));
    }

    // SAFETY: every call is forwarded to `System` unchanged; the wrapper only adds to two counters.
    #[allow(unsafe_code)]
    unsafe impl GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = unsafe { System.alloc(l) };
            if ON.load(Relaxed) && !p.is_null() {
                let now = LIVE.fetch_add(l.size(), Relaxed) + l.size();
                PEAK.fetch_max(now, Relaxed);
                CLASSES[class(l.size())].fetch_add(1, Relaxed);
                CUM[class(l.size())].fetch_add(l.size(), Relaxed);
                profile(l.size());
            }
            p
        }

        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe { System.dealloc(p, l) };
            if ON.load(Relaxed) {
                LIVE.fetch_sub(l.size(), Relaxed);
                CLASSES[class(l.size())].fetch_sub(1, Relaxed);
            }
        }

        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            let q = unsafe { System.realloc(p, l, new) };
            if ON.load(Relaxed) && !q.is_null() {
                LIVE.fetch_add(new, Relaxed);
                LIVE.fetch_sub(l.size(), Relaxed);
                CLASSES[class(l.size())].fetch_sub(1, Relaxed);
                CLASSES[class(new)].fetch_add(1, Relaxed);
            }
            q
        }
    }
}

#[global_allocator]
static ALLOC: heap::Counting = heap::Counting;

struct Args {
    torrents: Vec<PathBuf>,
    download_dir: PathBuf,
    state_dir: PathBuf,
    listen_port: u16,
    dht: bool,
    pex: bool,
    lsd: bool,
    portmap: bool,
    utp: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: bench_client add <file.torrent>... [--add <file.torrent>]... --download-dir <dir> \
         --state-dir <dir> --listen-port <port> [--no-dht] [--no-pex] [--no-lsd] [--no-portmap] [--no-utp]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut a = Args {
        torrents: Vec::new(),
        download_dir: PathBuf::new(),
        state_dir: PathBuf::new(),
        listen_port: 0,
        dht: true,
        pex: true,
        lsd: true,
        portmap: true,
        utp: true,
    };
    let mut started = false;
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--version" => {
                println!("trs-bench-client {} (trs-engine)", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "add" if !started => started = true,
            "--add" => a.torrents.push(it.next().unwrap_or_else(|| usage()).into()),
            "--download-dir" => a.download_dir = it.next().unwrap_or_else(|| usage()).into(),
            "--state-dir" => a.state_dir = it.next().unwrap_or_else(|| usage()).into(),
            "--listen-port" => a.listen_port = it.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage()),
            "--no-dht" => a.dht = false,
            "--no-pex" => a.pex = false,
            "--no-lsd" => a.lsd = false,
            "--no-portmap" => a.portmap = false,
            "--no-utp" => a.utp = false,
            s if s.starts_with("--") => usage(),
            s if started => a.torrents.push(s.into()),
            _ => usage(),
        }
    }
    if a.torrents.is_empty() || a.download_dir.as_os_str().is_empty() || a.state_dir.as_os_str().is_empty() {
        usage();
    }
    a
}

/// One output line: aggregates over every torrent.
fn status_line(t: &Totals) -> String {
    let n = t.torrents;
    let state = if t.checking > 0 {
        "checking"
    } else if t.errors > 0 && t.other == 0 && t.seeding == 0 {
        "error"
    } else if t.other > 0 {
        "downloading"
    } else if t.seeding == n && n > 0 {
        "seeding"
    } else {
        "error"
    };
    let mut out = String::with_capacity(200);
    let _ = write!(
        out,
        "{{\"done_bytes\":{},\"total_bytes\":{},\"down_rate\":{},\"up_rate\":{},\"peers\":{},\
         \"state\":\"{state}\",\"torrents\":{n},\"seeding\":{},\"checking\":{}}}",
        t.done_bytes, t.total_bytes, t.down_rate, t.up_rate, t.peers, t.seeding, t.checking
    );
    out
}

async fn run(a: Args) -> Result<(), String> {
    std::fs::create_dir_all(&a.download_dir).map_err(|e| format!("download dir: {e}"))?;
    std::fs::create_dir_all(&a.state_dir).map_err(|e| format!("state dir: {e}"))?;
    // The benchmark contract is "seed until SIGTERM". The shipped default rule (ratio 1.0 or 7 days,
    // then stop) would stop a torrent the moment the first leecher has taken it once, and a stopped
    // torrent refuses every later handshake: the root cause of the "second leecher never connects"
    // failure of the first hot campaign. Only written when there is no rules.toml yet.
    let rules = a.state_dir.join("rules.toml");
    if !rules.exists() {
        std::fs::write(&rules, "version = 1\n\n[default]\npriority = \"normal\"\non_goal = \"stop_keep\"\ngoal = { kind = \"forever\" }\n")
            .map_err(|e| format!("rules.toml: {e}"))?;
    }
    let (dir, port) = (a.download_dir.clone(), a.listen_port);
    let (dht, pex, portmap, utp) = (a.dht, a.pex, a.portmap, a.utp);
    // The real stack from a state directory (state.redb, config.toml, ...), with the engine
    // settings the benchmark contract asks for. Nothing outside the two directories is touched.
    let host = Host::start(
        &a.state_dir,
        HostOptions {
            hot_reload: false,
            persist_listen_port: false,
            tweak: Some(Box::new(move |c: &mut EngineConfig| {
                c.listen_port = port;
                c.dht_bind = std::net::SocketAddrV4::new(std::net::Ipv4Addr::UNSPECIFIED, port);
                c.default_save_dir = dir;
                // Straight into the download folder: no staging, no organizing, no watch folders.
                c.use_staging = false;
                c.incomplete_dir = None;
                c.detect = None;
                c.watch.clear();
                // No queueing: every added torrent is active.
                c.max_active_downloads = 100_000;
                // Peer limits equalised with the other clients in the harness (bench/README.md).
                c.max_connections = 200;
                c.max_connections_per_torrent = 100;
                c.down_limit = None;
                c.up_limit = None;
                c.alt_schedule = None;
                c.alt_speed = false;
                // The local benchmarks use a mock tracker on loopback (production refuses that).
                c.tracker_destinations = trs_tracker::policy::DestinationPolicy::LOCAL;
                c.dht_enabled = dht;
                c.pex_enabled = pex;
                c.utp = utp;
                // Measurement aid: shorter tier timers, so that an idle seeder reaches its steady
                // Cold state in minutes instead of ten (`TRS_BENCH_TIER_SECS=15`).
                if let Some(secs) = std::env::var("TRS_BENCH_TIER_SECS").ok().and_then(|v| v.parse::<u64>().ok()) {
                    c.tiers.hot_to_warm = Duration::from_secs(secs);
                    c.tiers.warm_to_cold = Duration::from_secs(secs);
                }
                c.portmap.enabled = portmap && (c.portmap.upnp || c.portmap.natpmp);
            })),
            ..HostOptions::default()
        },
    )
    .await
    .map_err(|e| e.to_string())?;
    let _ = a.lsd; // the engine has no LSD yet; accepted for the contract
    let h: EngineHandle = host.handle();
    for p in &a.torrents {
        let bytes = std::fs::read(p).map_err(|e| format!("{}: {e}", p.display()))?;
        match h.add_torrent_file(bytes, AddOptions::default()).await {
            Ok(_) | Err(trs_engine::EngineError::Duplicate(_)) => {}
            Err(e) => eprintln!("{}: {e}", p.display()),
        }
    }
    eprintln!("bench_client: {} torrents added, listening on {}", a.torrents.len(), h.listen_port());

    let heap_log = std::env::var_os("TRS_BENCH_DIAG").is_some();
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stdout = std::io::stdout();
    let mut heap_tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tokio::select! {
            _ = tick.tick() => {
                // `totals`, not `snapshot`: no row (and no string) is built per torrent, so the
                // once-a-second status line costs nothing measurable with 500 idle torrents.
                if let Ok(t) = h.totals().await {
                    let mut line = status_line(&t);
                    line.push('\n');
                    // A closed pipe means the harness is gone; keep running until signalled.
                    let _ = stdout.write_all(line.as_bytes());
                    let _ = stdout.flush();
                }
            }
            _ = heap_tick.tick(), if heap_log => {
                let t = h.totals().await.unwrap_or_default();
                let d = h.diagnostics();
                eprintln!(
                    "bench_client: heap_live_bytes={} connections={} half_open={} peers={} accepted={} refused={} utp_in={} utp_out={}",
                    heap::LIVE.load(std::sync::atomic::Ordering::Relaxed),
                    t.connections,
                    t.half_open,
                    t.peers,
                    d.incoming_accepted,
                    d.incoming_refused,
                    d.utp_accepted,
                    d.utp_dialed
                );
                let classes: Vec<String> = heap::CLASSES
                    .iter()
                    .enumerate()
                    .map(|(i, c)| format!("{i}:{}", c.load(std::sync::atomic::Ordering::Relaxed)))
                    .collect();
                eprintln!("bench_client: heap_classes {}", classes.join(" "));
                let cum: Vec<String> = heap::CUM.iter().enumerate().map(|(i, c)| format!("{i}:{}", c.load(std::sync::atomic::Ordering::Relaxed) >> 10)).collect();
                eprintln!("bench_client: heap_peak={} cum_kib_by_class {}", heap::PEAK.load(std::sync::atomic::Ordering::Relaxed), cum.join(" "));
            }
            _ = term.recv() => break,
            _ = int.recv() => break,
        }
    }
    eprintln!("bench_client: shutting down");
    let r = host.shutdown(Duration::from_secs(20)).await;
    eprintln!("bench_client: stopped confirmed={} abandoned={} waited={:?}", r.stopped_confirmed, r.stopped_abandoned, r.waited);
    Ok(())
}

fn main() {
    if let Some((lo, hi)) = std::env::var("TRS_BENCH_PROF").ok().and_then(|v| v.split_once('-').map(|(a, b)| (a.parse().unwrap_or(0), b.parse().unwrap_or(0)))) {
        heap::PROF_LO.store(lo, std::sync::atomic::Ordering::Relaxed);
        heap::PROF_HI.store(hi, std::sync::atomic::Ordering::Relaxed);
    }
    if std::env::var_os("TRS_BENCH_DIAG").is_some() {
        heap::ON.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let a = parse_args();
    let rt = match trs_engine::build_runtime() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("bench_client: {e}");
            std::process::exit(1);
        }
    };
    let code = match rt.block_on(run(a)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("bench_client: {e}");
            1
        }
    };
    rt.shutdown_timeout(Duration::from_secs(2));
    std::process::exit(code);
}
