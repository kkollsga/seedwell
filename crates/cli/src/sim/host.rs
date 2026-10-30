//! The sim host: one small process per app/daemon that serves every simulated torrent's seeders and
//! tracker. It is started by the first `sim add` and ends when the app (or daemon) it serves is gone,
//! after `sim clear`, or when it is told to. Control is a Unix socket in the sim folder (same user only).
#![allow(missing_docs)] // variant fields are named after the options they hold


use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use super::content::{Kind, LayoutSpec, build, fill, hex, layout};
use super::swarm::{Limiter, Registry, SimState, TrackerMode, spawn_seeders, spawn_tracker};

/// What `sim add` asks the host to build. Sent as JSON.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SimSpec {
    /// `sim1`, ...
    pub id: String,
    /// Kind word.
    pub kind: String,
    /// Total bytes.
    pub size: Option<u64>,
    /// Seasons.
    pub seasons: u32,
    /// Episodes per season.
    pub episodes: u32,
    /// Files (archive).
    pub files: u32,
    /// Extras.
    pub extras: bool,
    /// Name.
    pub name: Option<String>,
    /// Private flag in the torrent (the announce URL then carries a fake passkey).
    pub private: bool,
    /// Upload cap, bytes/s (0 = unlimited).
    pub speed: u64,
    /// Seeders.
    pub peers: u32,
    /// Tracker mode word.
    pub tracker: String,
    /// Stop answering after this percentage of the content was served.
    pub stall_after_pct: Option<u32>,
    /// Pieces that are served corrupt once.
    pub corrupt: u32,
    /// Ordinal (the number in `simN`).
    pub ordinal: u32,
    /// Folder for this sim's files.
    pub dir: PathBuf,
}

struct Entry {
    sim: Arc<SimState>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    spec: SimSpec,
}

struct Host {
    reg: Registry,
    tracker: std::net::SocketAddr,
    sims: Mutex<HashMap<String, Entry>>,
    quit: watch::Sender<bool>,
}

fn pick<'a>(sims: &'a HashMap<String, Entry>, id: Option<&str>) -> Result<Vec<&'a Entry>, String> {
    match id {
        Some(i) => sims.get(i).map(|e| vec![e]).ok_or_else(|| format!("no sim `{i}` (see `seedwell sim list`)")),
        None => Ok(sims.values().collect()),
    }
}

async fn create(h: &Arc<Host>, spec: SimSpec) -> Result<Value, String> {
    let kind = Kind::parse(&spec.kind).ok_or_else(|| format!("unknown kind `{}`", spec.kind))?;
    let mode = TrackerMode::parse(&spec.tracker).ok_or_else(|| format!("unknown tracker mode `{}`", spec.tracker))?;
    let ls = LayoutSpec { kind, size: spec.size, seasons: spec.seasons, episodes: spec.episodes, files: spec.files, extras: spec.extras, name: spec.name.clone(), ordinal: spec.ordinal };
    let lay = layout(&ls);
    // The seed is what makes this torrent unique: ordinal and the clock.
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    let seed = now ^ (u64::from(spec.ordinal) << 40) ^ 0x5EED_5171;
    let mut key = [0u8; 16];
    fill(seed, 0, &mut key);
    let passkey = hex(&key);
    let announce = if spec.private {
        format!("http://127.0.0.1:{}/{passkey}/announce", h.tracker.port())
    } else {
        format!("http://127.0.0.1:{}/announce", h.tracker.port())
    };
    let comment = format!("Seedwell simulated torrent ({}); the data is generated and the swarm runs on 127.0.0.1", spec.id);
    let announce2 = if kind == Kind::Magnet { String::new() } else { announce.clone() };
    let private = spec.private;
    let built = tokio::task::spawn_blocking(move || build(lay, seed, &announce2, private, &comment)).await.map_err(|e| e.to_string())?;
    let built = Arc::new(built);
    std::fs::create_dir_all(&spec.dir).map_err(|e| format!("cannot create {}: {e}", spec.dir.display()))?;
    let torrent_path = spec.dir.join(format!("{}.torrent", spec.id));
    std::fs::write(&torrent_path, &built.torrent).map_err(|e| e.to_string())?;
    // Pieces that are served corrupt the first time they are asked for.
    let mut bad = HashMap::new();
    for i in 0..spec.corrupt.min(built.pieces) {
        let mut b = [0u8; 4];
        fill(seed ^ 0xBAD, u64::from(i) * 4, &mut b);
        bad.insert(u32::from_le_bytes(b) % built.pieces.max(1), 1u32);
    }
    let (stop_tx, _) = watch::channel(false);
    let sim = Arc::new(SimState {
        id: spec.id.clone(),
        kind,
        limiter: Limiter::new(spec.speed),
        stopped: stop_tx,
        mode: Mutex::new(mode),
        served: Default::default(),
        stall_after: spec.stall_after_pct.map(|p| built.total / 100 * u64::from(p.min(100))),
        corrupt: Mutex::new(bad),
        announces: Default::default(),
        last_event: Mutex::new(String::new()),
        conns: Default::default(),
        peers: Mutex::new(Vec::new()),
        built: built.clone(),
    });
    let tasks = spawn_seeders(sim.clone(), spec.peers.clamp(1, 400)).await.map_err(|e| format!("cannot listen on loopback: {e}"))?;
    h.reg.lock().unwrap_or_else(|e| e.into_inner()).insert(built.info_hash, sim.clone());
    let peers: Vec<String> = sim.peers.lock().unwrap_or_else(|e| e.into_inner()).iter().map(ToString::to_string).collect();
    let magnet = if kind == Kind::Magnet {
        Some(format!("magnet:?xt=urn:btih:{}&dn={}&tr={}", hex(&built.info_hash), built.layout.name, announce.replace(':', "%3A").replace('/', "%2F")))
    } else {
        None
    };
    let out = json!({
        "ok": true, "id": spec.id, "name": built.layout.name, "kind": spec.kind, "info_hash": hex(&built.info_hash),
        "torrent_path": torrent_path, "magnet": magnet, "total": built.total, "pieces": built.pieces,
        "files": built.layout.files.iter().map(|f| json!({"path": f.0, "length": f.1})).collect::<Vec<_>>(),
        "peers": peers,
    });
    h.sims.lock().unwrap_or_else(|e| e.into_inner()).insert(spec.id.clone(), Entry { sim, tasks, spec });
    Ok(out)
}

fn describe(e: &Entry) -> Value {
    let s = &e.sim;
    json!({
        "id": s.id, "kind": s.kind.as_str(), "name": s.built.layout.name, "info_hash": hex(&s.built.info_hash),
        "total": s.built.total, "speed": s.limiter.rate(), "tracker": s.mode.lock().unwrap_or_else(|x| x.into_inner()).word(),
        "peers": s.peers.lock().unwrap_or_else(|x| x.into_inner()).len(), "running": !s.is_stopped(),
        "connections": s.conns.load(Ordering::Relaxed), "served": s.served.load(Ordering::Relaxed),
        "announces": s.announces.load(Ordering::Relaxed), "last_event": s.last_event.lock().unwrap_or_else(|x| x.into_inner()).clone(),
        "private": e.spec.private, "dir": e.spec.dir,
    })
}

async fn handle(h: &Arc<Host>, req: Value) -> Value {
    let op = req["op"].as_str().unwrap_or("");
    let id = req["id"].as_str();
    let err = |m: String| json!({ "ok": false, "error": m });
    match op {
        "ping" => json!({ "ok": true, "pid": std::process::id(), "tracker": h.tracker.to_string() }),
        "create" => match serde_json::from_value::<SimSpec>(req["spec"].clone()) {
            Ok(spec) => create(h, spec).await.unwrap_or_else(err),
            Err(e) => err(format!("bad spec: {e}")),
        },
        "list" => {
            let sims = h.sims.lock().unwrap_or_else(|e| e.into_inner());
            let mut v: Vec<Value> = sims.values().map(describe).collect();
            v.sort_by_key(|x| x["id"].as_str().unwrap_or("").len() * 1000 + x["id"].as_str().unwrap_or("").bytes().last().unwrap_or(0) as usize);
            json!({ "ok": true, "sims": v })
        }
        "stop" | "resume" => {
            let sims = h.sims.lock().unwrap_or_else(|e| e.into_inner());
            match pick(&sims, id) {
                Ok(list) => {
                    for e in &list {
                        e.sim.stopped.send_replace(op == "stop");
                    }
                    json!({ "ok": true, "changed": list.len() })
                }
                Err(m) => err(m),
            }
        }
        "speed" => {
            let sims = h.sims.lock().unwrap_or_else(|e| e.into_inner());
            match pick(&sims, id) {
                Ok(list) => {
                    let bps = req["bps"].as_u64().unwrap_or(0);
                    list.iter().for_each(|e| e.sim.limiter.set(bps));
                    json!({ "ok": true, "changed": list.len(), "speed": bps })
                }
                Err(m) => err(m),
            }
        }
        "tracker" => {
            let Some(mode) = req["mode"].as_str().and_then(TrackerMode::parse) else { return err("unknown tracker mode".into()) };
            let sims = h.sims.lock().unwrap_or_else(|e| e.into_inner());
            match pick(&sims, id) {
                Ok(list) => {
                    list.iter().for_each(|e| *e.sim.mode.lock().unwrap_or_else(|x| x.into_inner()) = mode);
                    json!({ "ok": true, "changed": list.len(), "tracker": mode.word() })
                }
                Err(m) => err(m),
            }
        }
        "clear" | "shutdown" => {
            let mut sims = h.sims.lock().unwrap_or_else(|e| e.into_inner());
            for (_, e) in sims.drain() {
                e.sim.stopped.send_replace(true);
                e.tasks.iter().for_each(tokio::task::JoinHandle::abort);
            }
            h.reg.lock().unwrap_or_else(|e| e.into_inner()).clear();
            let _ = h.quit.send(true);
            json!({ "ok": true })
        }
        other => err(format!("unknown op `{other}`")),
    }
}

/// `seedwell sim host --dir D --app-socket S`: run until the app is gone or told to stop.
pub async fn run_host(dir: &Path, app_socket: &Path) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let sock_path = dir.join("host.sock");
    let _ = std::fs::remove_file(&sock_path);
    let reg: Registry = Arc::default();
    let (tracker, _tracker_task) = match spawn_tracker(reg.clone()).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("sim host: cannot listen on loopback: {e}");
            return 1;
        }
    };
    let mut listener = match trs_control::transport::bind_in_private_dir(&sock_path).await {
        Ok(b) => b.listener,
        Err(e) => {
            eprintln!("sim host: cannot bind {}: {e}", sock_path.display());
            return 1;
        }
    };
    let (quit, mut quit_rx) = watch::channel(false);
    let host = Arc::new(Host { reg, tracker, sims: Mutex::new(HashMap::new()), quit });
    // Follow the app: two failed connects in a row and the host is done (no orphans).
    let app = app_socket.to_path_buf();
    let watcher = host.quit.clone();
    tokio::spawn(async move {
        let mut misses = 0;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if trs_control::transport::connect(&app).await.is_ok() {
                misses = 0;
            } else {
                misses += 1;
                if misses >= 2 {
                    let _ = watcher.send(true);
                    return;
                }
            }
        }
    });
    loop {
        tokio::select! {
            _ = quit_rx.changed() => break,
            Ok(sock) = listener.accept() => {
                let h = host.clone();
                tokio::spawn(async move {
                    let (rd, mut wr) = tokio::io::split(sock);
                    let mut lines = BufReader::new(rd).lines();
                    while let Ok(Some(l)) = lines.next_line().await {
                        let out = match serde_json::from_str::<Value>(&l) {
                            Ok(req) => handle(&h, req).await,
                            Err(e) => json!({ "ok": false, "error": format!("bad request: {e}") }),
                        };
                        if wr.write_all(format!("{out}\n").as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        }
    }
    // Let the last answer (to `clear`) leave before the socket goes.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = std::fs::remove_file(&sock_path);
    0
}
