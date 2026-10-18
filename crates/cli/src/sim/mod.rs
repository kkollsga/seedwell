//! `seedwell sim …`: simulated torrents for testing. Synthetic data that is never stored, a swarm of
//! seeders and a mock tracker on 127.0.0.1, added to the running app or daemon like any torrent, so the
//! real engine paths run: tracker, peer wire, verify, picker, classification, the popover, Recent and
//! Sharing.
//!
//! Safety: sims need Developer mode (Settings > Advanced) or `--yes-i-know`; the announce goes only to
//! the loopback mock; the downloaded data lands in a sim folder under the temp directory, never in
//! Downloads; everything binds to 127.0.0.1; the host process ends with the app or on `sim clear`.
#![allow(missing_docs)] // variant fields are named after the options they hold


pub mod content;
pub mod host;
pub mod swarm;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use trs_control::client::Client;
use trs_control::human;
use trs_control::proto::*;
use trs_control::types::*;

use crate::args::{Opts, Usage, parse_rate, parse_size};
use crate::commands::{CliError, Global, R, ask, connect, fetch_rows, show};
use crate::sim::content::Kind;
use crate::sim::host::SimSpec;
use crate::sim::swarm::TrackerMode;

/// A `sim` subcommand.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// `sim add <kind> …`
    Add(AddOpts),
    /// `sim list`
    List,
    /// `sim stop [<sim>]`
    Stop(Option<String>),
    /// `sim resume [<sim>]`
    Resume(Option<String>),
    /// `sim speed <sim> R`
    Speed { sim: String, bps: u64 },
    /// `sim tracker <sim> MODE`
    Tracker { sim: String, mode: String },
    /// `sim clear`
    Clear,
    /// `sim scenario <name>`
    Scenario(String),
    /// `sim host --dir D --app-socket S` (internal: started by `sim add`)
    Host { dir: PathBuf, app_socket: PathBuf },
}

/// `sim add` options.
#[derive(Clone, Debug, PartialEq)]
pub struct AddOpts {
    /// Kind word.
    pub kind: String,
    /// `--size`.
    pub size: Option<u64>,
    /// `--speed` (0 = unlimited).
    pub speed: Option<u64>,
    /// `--peers`.
    pub peers: u32,
    /// `--private`.
    pub private: bool,
    /// `--tracker`.
    pub tracker: String,
    /// `--stall-after` percent.
    pub stall_after_pct: Option<u32>,
    /// `--corrupt`.
    pub corrupt: u32,
    /// `--extras`.
    pub extras: bool,
    /// `--name`.
    pub name: Option<String>,
    /// `--seasons`.
    pub seasons: u32,
    /// `--episodes`.
    pub episodes: u32,
    /// `--files`.
    pub files: u32,
    /// `--hold`: leave the start delay running.
    pub hold: bool,
    /// `--choose`: leave a multi-item torrent in the chooser.
    pub choose: bool,
}

const USAGE: &str = "usage: seedwell sim add <kind> [options] | list | stop [<sim>] | resume [<sim>] | speed <sim> R | tracker <sim> MODE | clear | scenario <name>";

/// The canned scenarios.
pub const SCENARIOS: [&str; 4] = ["redownload", "outage", "storm-300", "private-unregistered"];

fn pct(s: &str) -> Result<u32, Usage> {
    s.trim_end_matches('%').parse::<u32>().ok().filter(|p| *p <= 100).ok_or_else(|| Usage(format!("`{s}` is not a percentage (try 50%)")))
}

/// Words after `sim`.
pub fn parse(args: Vec<String>) -> Result<Cmd, Usage> {
    let mut it = args.into_iter();
    let sub = it.next().ok_or_else(|| Usage(USAGE.into()))?;
    let mut o = Opts::new(it.collect());
    Ok(match sub.as_str() {
        "add" => {
            let size = o.value(&["--size"])?.map(|v| parse_size(&v)).transpose()?;
            let speed = o.value(&["--speed"])?.map(|v| parse_rate(&v)).transpose()?;
            let peers = o.number::<u32>(&["--peers"])?.unwrap_or(3);
            let private = o.flag(&["--private"]);
            let tracker = o.value(&["--tracker"])?.unwrap_or_else(|| "ok".into());
            if TrackerMode::parse(&tracker).is_none() {
                return Err(Usage(format!("--tracker is ok, unregistered, down, slow, fail503 or interval=N, not `{tracker}`")));
            }
            let stall_after_pct = o.value(&["--stall-after"])?.map(|v| pct(&v)).transpose()?;
            let corrupt = o.number::<u32>(&["--corrupt"])?.unwrap_or(0);
            let extras = o.flag(&["--extras"]);
            let name = o.value(&["--name"])?;
            let seasons = o.number::<u32>(&["--seasons"])?.unwrap_or(1);
            let episodes = o.number::<u32>(&["--episodes"])?.unwrap_or(6);
            let files = o.number::<u32>(&["--files"])?.unwrap_or(2000);
            let hold = o.flag(&["--hold"]);
            let choose = o.flag(&["--choose"]);
            let mut pos = o.finish()?;
            if pos.len() != 1 {
                return Err(Usage(format!("usage: seedwell sim add <kind> [options]   kinds: {}", Kind::ALL)));
            }
            let kind = pos.remove(0);
            if Kind::parse(&kind).is_none() {
                return Err(Usage(format!("unknown kind `{kind}` (kinds: {})", Kind::ALL)));
            }
            if peers == 0 || peers > 400 {
                return Err(Usage("--peers is 1 to 400".into()));
            }
            if !(1..=20).contains(&seasons) || !(1..=200).contains(&episodes) || !(2..=100_000).contains(&files) {
                return Err(Usage("--seasons is 1-20, --episodes 1-200, --files 2-100000".into()));
            }
            Cmd::Add(AddOpts { kind, size, speed, peers, private, tracker, stall_after_pct, corrupt, extras, name, seasons, episodes, files, hold, choose })
        }
        "list" | "ls" => {
            no_more(o)?;
            Cmd::List
        }
        "stop" | "resume" => {
            let mut pos = o.finish()?;
            if pos.len() > 1 {
                return Err(Usage(format!("usage: seedwell sim {sub} [<sim>]")));
            }
            let sim = (!pos.is_empty()).then(|| pos.remove(0));
            if sub == "stop" { Cmd::Stop(sim) } else { Cmd::Resume(sim) }
        }
        "speed" => {
            let pos = o.finish()?;
            let [sim, rate] = <[String; 2]>::try_from(pos).map_err(|_| Usage("usage: seedwell sim speed <sim> 500KB/s".into()))?;
            Cmd::Speed { sim, bps: parse_rate(&rate)? }
        }
        "tracker" => {
            let pos = o.finish()?;
            let [sim, mode] = <[String; 2]>::try_from(pos).map_err(|_| Usage("usage: seedwell sim tracker <sim> ok|unregistered|down|slow|fail503|interval=N".into()))?;
            if TrackerMode::parse(&mode).is_none() {
                return Err(Usage(format!("tracker mode is ok, unregistered, down, slow, fail503 or interval=N, not `{mode}`")));
            }
            Cmd::Tracker { sim, mode }
        }
        "clear" => {
            no_more(o)?;
            Cmd::Clear
        }
        "scenario" => {
            let pos = o.finish()?;
            let [name] = <[String; 1]>::try_from(pos).map_err(|_| Usage(format!("usage: seedwell sim scenario <name>   scenarios: {}", SCENARIOS.join(" "))))?;
            if !SCENARIOS.contains(&name.as_str()) {
                return Err(Usage(format!("unknown scenario `{name}` (scenarios: {})", SCENARIOS.join(" "))));
            }
            Cmd::Scenario(name)
        }
        "host" => {
            let dir = o.value(&["--dir"])?.ok_or_else(|| Usage("sim host needs --dir".into()))?;
            let app_socket = o.value(&["--app-socket"])?.ok_or_else(|| Usage("sim host needs --app-socket".into()))?;
            no_more(o)?;
            Cmd::Host { dir: dir.into(), app_socket: app_socket.into() }
        }
        other => return Err(Usage(format!("unknown sim command `{other}`. {USAGE}"))),
    })
}

fn no_more(o: Opts) -> Result<(), Usage> {
    match o.finish()?.first() {
        None => Ok(()),
        Some(x) => Err(Usage(format!("unexpected argument `{x}`"))),
    }
}

// ----------------------------------------------------------------------------- state on disk

/// The sim folder for the app or daemon behind `socket`: under the temp directory, one per socket.
pub fn sim_dir(socket: &Path) -> PathBuf {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in socket.to_string_lossy().bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    std::env::temp_dir().join("seedwell-sim").join(format!("{h:016x}"))
}

#[derive(Default, Serialize, Deserialize)]
struct Manifest {
    next: u32,
    sims: Vec<ManifestSim>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ManifestSim {
    id: String,
    kind: String,
    name: String,
    torrent_id: String,
    dir: PathBuf,
}

fn load_manifest(dir: &Path) -> Manifest {
    std::fs::read_to_string(dir.join("sims.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_manifest(dir: &Path, m: &Manifest) -> R {
    std::fs::create_dir_all(dir).map_err(|e| CliError::plain(1, format!("cannot create {}: {e}", dir.display())))?;
    std::fs::write(dir.join("sims.json"), serde_json::to_string_pretty(m).unwrap_or_default()).map_err(|e| CliError::plain(1, format!("cannot write the sim list: {e}")))
}

// ----------------------------------------------------------------------------- the host

async fn host_call(dir: &Path, req: Value) -> Result<Value, CliError> {
    let sock = dir.join("host.sock");
    let mut s = UnixStream::connect(&sock).await.map_err(|_| CliError { code: 3, message: "no sims are running (start one with `seedwell sim add`)".into(), api_code: Some(ErrorCode::NotFound), data: None })?;
    s.write_all(format!("{req}\n").as_bytes()).await.map_err(|e| CliError::plain(1, format!("sim host: {e}")))?;
    let mut lines = BufReader::new(s).lines();
    let line = tokio::time::timeout(Duration::from_secs(900), lines.next_line())
        .await
        .map_err(|_| CliError::plain(1, "the sim host did not answer"))?
        .map_err(|e| CliError::plain(1, format!("sim host: {e}")))?
        .ok_or_else(|| CliError::plain(1, "the sim host closed the connection"))?;
    let v: Value = serde_json::from_str(&line).map_err(|e| CliError::plain(1, format!("sim host: {e}")))?;
    if v["ok"] == true { Ok(v) } else { Err(CliError::plain(1, v["error"].as_str().unwrap_or("sim host error").to_owned())) }
}

async fn ensure_host(g: &Global, dir: &Path) -> R {
    if host_call(dir, json!({"op":"ping"})).await.is_ok() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| CliError::plain(1, format!("cannot create {}: {e}", dir.display())))?;
    let exe = std::env::current_exe().map_err(|e| CliError::plain(1, format!("cannot find myself: {e}")))?;
    let log = std::fs::File::create(dir.join("host.log")).map_err(|e| CliError::plain(1, e.to_string()))?;
    // `trap '' HUP`: closing the terminal does not end the host; it ends when the app does.
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("trap '' HUP; exec \"$0\" \"$@\"")
        .arg(&exe)
        .args(["sim", "host", "--dir"])
        .arg(dir)
        .arg("--app-socket")
        .arg(&g.socket)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|e| CliError::plain(1, format!("cannot start the sim host: {e}")))?;
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if host_call(dir, json!({"op":"ping"})).await.is_ok() {
            return Ok(());
        }
    }
    Err(CliError::plain(1, format!("the sim host did not start (see {})", dir.join("host.log").display())))
}

/// Sims are for developers: Developer mode must be on, or the caller says `--yes-i-know`.
async fn allowed(g: &Global, c: &mut Client) -> R {
    if g.yes_i_know {
        return Ok(());
    }
    let (_, s): (_, SettingsDto) = ask(c, Request::SettingsGet(SettingsGetParams { keys: Some(vec!["advanced.developer_mode".into()]) })).await?;
    if s.settings.get("advanced.developer_mode") == Some(&Value::Bool(true)) {
        return Ok(());
    }
    Err(CliError {
        code: 4,
        message: "Turn on Developer mode: seedwell dev on (or pass --yes-i-know). Simulated torrents are fake torrents with a made-up swarm on 127.0.0.1.".into(),
        api_code: Some(ErrorCode::PermissionDenied),
        data: None,
    })
}

// ----------------------------------------------------------------------------- run

/// Run a `sim` command.
pub async fn run(cmd: Cmd, g: &Global) -> R {
    if let Cmd::Host { dir, app_socket } = &cmd {
        return match host::run_host(dir, app_socket).await {
            0 => Ok(()),
            n => Err(CliError::plain(n, "sim host failed")),
        };
    }
    let dir = sim_dir(&g.socket);
    match cmd {
        Cmd::Add(o) => {
            let mut c = connect(g).await?;
            allowed(g, &mut c).await?;
            let added = add_sim(g, &mut c, &dir, &o, true).await?;
            show(g, &added.json(), || added.text());
        }
        Cmd::List => list(g, &dir).await?,
        Cmd::Stop(sim) => {
            let v = host_call(&dir, json!({"op":"stop","id":sim.as_deref().map(sim_id)})).await?;
            show(g, &v, || format!("stopped {} sim(s): no seeders answer and the tracker lists no peers (the app shows \"Waiting for peers\")", v["changed"]));
        }
        Cmd::Resume(sim) => {
            let v = host_call(&dir, json!({"op":"resume","id":sim.as_deref().map(sim_id)})).await?;
            show(g, &v, || format!("resumed {} sim(s)", v["changed"]));
        }
        Cmd::Speed { sim, bps } => {
            let v = host_call(&dir, json!({"op":"speed","id":sim_id(&sim),"bps":bps})).await?;
            show(g, &v, || format!("{}: upload cap {}", sim_id(&sim), if bps == 0 { "unlimited".to_owned() } else { human::rate(bps) }));
        }
        Cmd::Tracker { sim, mode } => {
            let v = host_call(&dir, json!({"op":"tracker","id":sim_id(&sim),"mode":mode})).await?;
            show(g, &v, || format!("{}: the tracker now answers `{mode}`", sim_id(&sim)));
        }
        Cmd::Clear => clear(g, &dir).await?,
        Cmd::Scenario(name) => scenario(g, &dir, &name).await?,
        Cmd::Host { .. } => unreachable!("handled above"),
    }
    Ok(())
}

/// `3` and `sim3` both name the third sim.
fn sim_id(s: &str) -> String {
    if s.bytes().all(|b| b.is_ascii_digit()) { format!("sim{s}") } else { s.to_owned() }
}

/// A sim that was just added.
pub struct Added {
    /// `sim1`.
    pub sim: String,
    /// The torrent's id in the app.
    pub torrent_id: String,
    /// Release name.
    pub name: String,
    /// Kind.
    pub kind: String,
    /// Total bytes.
    pub total: u64,
    /// Files.
    pub files: Vec<(String, u64)>,
    /// What the app said about it.
    pub state: String,
    /// Seeders.
    pub peers: usize,
    /// Save folder.
    pub dir: PathBuf,
}

impl Added {
    fn json(&self) -> Value {
        json!({
            "sim": self.sim, "torrent_id": self.torrent_id, "name": self.name, "kind": self.kind, "total_bytes": self.total,
            "files": self.files.iter().map(|f| json!({"path": f.0, "length": f.1})).collect::<Vec<_>>(),
            "state": self.state, "peers": self.peers, "dir": self.dir, "added": true,
        })
    }

    fn text(&self) -> String {
        format!(
            "{}: {} ({}, {} files, {}) {}; {} seeder(s) on 127.0.0.1; data goes to {}",
            self.sim,
            self.name,
            self.kind,
            self.files.len(),
            human::bytes(self.total),
            self.state,
            self.peers,
            self.dir.display()
        )
    }
}

/// Create the sim in the host and add it to the app. `finish`: start a held multi-item torrent unless `--choose`.
pub async fn add_sim(g: &Global, c: &mut Client, dir: &Path, o: &AddOpts, finish: bool) -> Result<Added, CliError> {
    ensure_host(g, dir).await?;
    let mut manifest = load_manifest(dir);
    manifest.next += 1;
    let ordinal = manifest.next;
    let id = format!("sim{ordinal}");
    let data_dir = dir.join(&id);
    let spec = SimSpec {
        id: id.clone(),
        kind: o.kind.clone(),
        size: o.size,
        seasons: o.seasons,
        episodes: o.episodes,
        files: o.files,
        extras: o.extras,
        name: o.name.clone(),
        private: o.private,
        speed: o.speed.unwrap_or(0),
        peers: o.peers,
        tracker: o.tracker.clone(),
        stall_after_pct: o.stall_after_pct,
        corrupt: o.corrupt,
        ordinal,
        dir: data_dir.join("dl"),
    };
    eprintln!("seedwell: generating {id} ({})...", o.kind);
    let r = host_call(dir, json!({"op":"create","spec":spec})).await?;
    let torrent_path = r["torrent_path"].as_str().unwrap_or_default().to_owned();
    let peers: Vec<String> = r["peers"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_owned)).collect();
    let source = r["magnet"].as_str().map(str::to_owned).unwrap_or(torrent_path);
    let save = data_dir.join("data");
    std::fs::create_dir_all(&save).map_err(|e| CliError::plain(1, format!("cannot create {}: {e}", save.display())))?;
    // Only a torrent marked here (permission full and Developer mode, checked by the app) may announce to the
    // mock tracker on 127.0.0.1; nothing in a .torrent file can ask for that. Marked before the add so the
    // first announce is covered.
    let info_hash = r["info_hash"].as_str().unwrap_or_default().to_owned();
    if let Err(e) = ask::<Value>(c, Request::SimAllow(SimAllowParams { ids: vec![info_hash], allow: true })).await {
        eprintln!("seedwell: the tracker of this sim may be refused ({}); it still downloads through its direct peers", e.message);
    }
    let req = Request::Add(AddParams {
        source: Some(source),
        save_path: Some(save.to_string_lossy().into_owned()),
        now: !o.hold,
        peers: peers.iter().take(10).cloned().collect(),
        ..Default::default()
    });
    let (_, a): (_, AddedDto) = ask(c, req).await?;
    let mut state = if a.pending.is_some() { "held for its start delay".to_owned() } else { "added".to_owned() };
    if a.needs_choice {
        state = "waiting in the chooser".into();
        if finish && !o.choose && !o.hold {
            let (_, _): (_, ActionDto) = ask(c, Request::ChooseFiles(ChooseFilesParams { id: a.id.clone(), skip: Some(Vec::new()), ..Default::default() })).await?;
            state = "started".into();
        }
    } else if a.pending.is_none() {
        state = "started".into();
    }
    let files: Vec<(String, u64)> = r["files"].as_array().into_iter().flatten().map(|f| (f["path"].as_str().unwrap_or("").to_owned(), f["length"].as_u64().unwrap_or(0))).collect();
    manifest.sims.push(ManifestSim { id: id.clone(), kind: o.kind.clone(), name: r["name"].as_str().unwrap_or("").to_owned(), torrent_id: a.id.clone(), dir: data_dir.clone() });
    save_manifest(dir, &manifest)?;
    Ok(Added {
        sim: id,
        torrent_id: a.id,
        name: r["name"].as_str().unwrap_or("").to_owned(),
        kind: o.kind.clone(),
        total: r["total"].as_u64().unwrap_or(0),
        files,
        state,
        peers: peers.len(),
        dir: data_dir.join("data"),
    })
}

async fn list(g: &Global, dir: &Path) -> R {
    let manifest = load_manifest(dir);
    let host = host_call(dir, json!({"op":"list"})).await.ok();
    let live: Vec<Value> = host.as_ref().and_then(|h| h["sims"].as_array().cloned()).unwrap_or_default();
    let mut c = connect(g).await.ok();
    let rows = match c.as_mut() {
        Some(c) => fetch_rows(c, ListParams::default()).await.unwrap_or_default(),
        None => Vec::new(),
    };
    let sims: Vec<Value> = manifest
        .sims
        .iter()
        .map(|m| {
            let l = live.iter().find(|x| x["id"] == m.id.as_str());
            let row = rows.iter().find(|r| r.id == m.torrent_id);
            json!({
                "id": m.id, "kind": m.kind, "name": m.name, "torrent_id": m.torrent_id,
                "running": l.is_some_and(|x| x["running"] == true), "host": l.is_some(),
                "speed": l.map(|x| x["speed"].clone()), "tracker": l.map(|x| x["tracker"].clone()),
                "peers": l.map(|x| x["peers"].clone()), "connections": l.map(|x| x["connections"].clone()),
                "announces": l.map(|x| x["announces"].clone()), "served_bytes": l.map(|x| x["served"].clone()),
                "state": row.map(|r| r.state.clone()), "progress": row.map(|r| r.progress),
            })
        })
        .collect();
    let v = json!({ "sims": sims, "host_running": host.is_some(), "dir": dir });
    show(g, &v, || {
        if sims.is_empty() {
            return "No simulated torrents. `seedwell sim add movie` makes one.".to_owned();
        }
        let mut s = format!("{:<6} {:<12} {:<9} {:<13} {:>10} {:>5} {:>9}  NAME\n", "SIM", "KIND", "SWARM", "TRACKER", "SPEED", "CONNS", "STATE");
        for x in &sims {
            s.push_str(&format!(
                "{:<6} {:<12} {:<9} {:<13} {:>10} {:>5} {:>9}  {}\n",
                x["id"].as_str().unwrap_or(""),
                x["kind"].as_str().unwrap_or(""),
                if !x["host"].as_bool().unwrap_or(false) { "gone" } else if x["running"] == true { "running" } else { "stopped" },
                x["tracker"].as_str().unwrap_or("-"),
                x["speed"].as_u64().map_or("-".into(), |s| if s == 0 { "unlimited".into() } else { human::rate(s) }),
                x["connections"].as_u64().map_or("-".into(), |n| n.to_string()),
                x["state"].as_str().unwrap_or("-"),
                x["name"].as_str().unwrap_or("")
            ));
        }
        s.trim_end().to_owned()
    });
    Ok(())
}

async fn clear(g: &Global, dir: &Path) -> R {
    let manifest = load_manifest(dir);
    let _ = host_call(dir, json!({"op":"clear"})).await;
    let mut removed = 0usize;
    if !manifest.sims.is_empty()
        && let Ok(mut c) = connect(g).await {
            let rows = fetch_rows(&mut c, ListParams::default()).await.unwrap_or_default();
            // Take back the loopback permission of every sim torrent first.
            let _ = c.call(&Request::SimAllow(SimAllowParams { ids: manifest.sims.iter().map(|m| m.torrent_id.clone()).collect(), allow: false })).await;
            let ids: Vec<String> = manifest.sims.iter().filter(|m| rows.iter().any(|r| r.id == m.torrent_id)).map(|m| m.torrent_id.clone()).collect();
            if !ids.is_empty() {
                // Removal is two steps by design; this is the confirmed second step for our own sim torrents.
                let (_, plan): (_, RemovePlanDto) = ask(&mut c, Request::Remove(RemoveParams { ids: ids.clone(), delete_files: true })).await?;
                let (_, r): (_, RemovedDto) = ask(&mut c, Request::RemoveConfirm(RemoveConfirmParams { token: plan.token })).await?;
                removed = r.removed.len();
            }
        }
    let mut folders = 0usize;
    for m in &manifest.sims {
        if m.dir.starts_with(std::env::temp_dir()) && std::fs::remove_dir_all(&m.dir).is_ok() {
            folders += 1;
        }
    }
    // The host removes its socket just before it ends; give it a moment so the folder can go too.
    for _ in 0..30 {
        if !dir.join("host.sock").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let _ = std::fs::remove_file(dir.join("sims.json"));
    let _ = std::fs::remove_file(dir.join("host.log"));
    let _ = std::fs::remove_dir(dir);
    let v = json!({ "removed_torrents": removed, "removed_folders": folders, "sims": manifest.sims.len() });
    show(g, &v, || format!("cleared {} sim(s): {removed} torrent(s) removed with their data, {folders} folder(s) deleted, the swarm is stopped", manifest.sims.len()));
    Ok(())
}

// ----------------------------------------------------------------------------- scenarios

fn base(kind: &str) -> AddOpts {
    AddOpts {
        kind: kind.into(),
        size: None,
        speed: None,
        peers: 3,
        private: false,
        tracker: "ok".into(),
        stall_after_pct: None,
        corrupt: 0,
        extras: false,
        name: None,
        seasons: 1,
        episodes: 6,
        files: 2000,
        hold: false,
        choose: false,
    }
}

fn say(g: &Global, m: &str) {
    if !g.json {
        println!("{m}");
    }
}

async fn scenario(g: &Global, dir: &Path, name: &str) -> R {
    let mut c = connect(g).await?;
    allowed(g, &mut c).await?;
    let mut sims: Vec<Value> = Vec::new();
    match name {
        "redownload" => {
            // A private two-season pack: pick S01 only, let it finish, then tick S02 (it becomes a download again).
            let o = AddOpts { seasons: 2, episodes: 6, size: Some(180_000_000), speed: Some(6_000_000), private: true, choose: true, ..base("season-pack") };
            let a = add_sim(g, &mut c, dir, &o, true).await?;
            say(g, &format!("{}: waiting in the chooser; starting with Season 1 only", a.sim));
            let s1: Vec<u32> = (0..6).collect();
            let (_, _): (_, ActionDto) = ask(&mut c, Request::ChooseFiles(ChooseFilesParams { id: a.torrent_id.clone(), files: Some(s1), ..Default::default() })).await?;
            let (_, w): (_, WaitDto) = ask(&mut c, Request::WaitFor(WaitForParams { ids: vec![a.torrent_id.clone()], mode: Some(WaitMode::All), timeout_secs: Some(120) })).await?;
            say(g, if w.done { "Season 1 finished; ticking Season 2" } else { "Season 1 is not finished yet; ticking Season 2 anyway" });
            let set: Vec<FileSet> = (6..12).map(|index| FileSet { index, priority: FilePriorityParam::Normal }).collect();
            let (_, _): (_, ActionDto) = ask(&mut c, Request::SetFilePriorities(SetFilePrioritiesParams { id: a.torrent_id.clone(), priorities: None, set: Some(set) })).await?;
            say(g, "The torrent is a download again. Watch it: `seedwell list` / `seedwell debug perf`.");
            sims.push(a.json());
        }
        "outage" => {
            for _ in 0..3 {
                let o = AddOpts { size: Some(150_000_000), speed: Some(1_000_000), ..base("movie") };
                sims.push(add_sim(g, &mut c, dir, &o, true).await?.json());
            }
            let _ = host_call(dir, json!({"op":"tracker","mode":"down"})).await;
            let _ = host_call(dir, json!({"op":"stop"})).await;
            say(g, "Three downloads are running and now every tracker hangs up and every seeder is gone.\nWatch it: `seedwell debug health`, `seedwell debug trackers`. Recover: `seedwell sim tracker 1 ok` (and 2, 3) then `seedwell sim resume`.");
        }
        "storm-300" => {
            let o = AddOpts { size: Some(400_000_000), speed: Some(8_000_000), peers: 300, ..base("movie") };
            sims.push(add_sim(g, &mut c, dir, &o, true).await?.json());
            say(g, "300 seeders on loopback. Watch: `seedwell debug peers <torrent> --watch`, `seedwell debug perf`.");
        }
        "private-unregistered" => {
            let o = AddOpts { seasons: 2, size: Some(120_000_000), private: true, tracker: "unregistered".into(), ..base("season-pack") };
            sims.push(add_sim(g, &mut c, dir, &o, true).await?.json());
            say(g, "A private torrent whose tracker says \"Unregistered torrent\": the row halts with the tracker's words. `seedwell debug trackers`, then `seedwell debug announce <torrent>`.");
        }
        other => return Err(Usage(format!("unknown scenario `{other}`")).into()),
    }
    show(g, &json!({ "scenario": name, "sims": sims }), || format!("scenario {name}: {} sim(s) running; `seedwell sim list` shows them, `seedwell sim clear` removes them", sims.len()));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn parses_every_sim_command() {
        let Cmd::Add(a) = parse(w("add season-pack --seasons 2 --episodes 8 --speed 2MB/s --peers 5 --private --tracker interval=60 --stall-after 50% --corrupt 2 --extras --name X --hold")).unwrap() else { panic!() };
        assert_eq!((a.seasons, a.episodes, a.speed, a.peers, a.corrupt, a.stall_after_pct), (2, 8, Some(2_000_000), 5, 2, Some(50)));
        assert!(a.private && a.extras && a.hold && a.name.as_deref() == Some("X"));
        assert!(matches!(parse(w("add movie --size 1.5GB")).unwrap(), Cmd::Add(a) if a.size == Some(1_500_000_000)));
        assert_eq!(parse(w("list")).unwrap(), Cmd::List);
        assert_eq!(parse(w("stop")).unwrap(), Cmd::Stop(None));
        assert_eq!(parse(w("stop sim2")).unwrap(), Cmd::Stop(Some("sim2".into())));
        assert_eq!(parse(w("speed sim1 500KB/s")).unwrap(), Cmd::Speed { sim: "sim1".into(), bps: 500_000 });
        assert!(matches!(parse(w("tracker sim1 down")).unwrap(), Cmd::Tracker { .. }));
        assert_eq!(parse(w("clear")).unwrap(), Cmd::Clear);
        assert!(matches!(parse(w("scenario redownload")).unwrap(), Cmd::Scenario(_)));
        for bad in ["", "add", "add nothing", "add movie --tracker maybe", "add movie --peers 0", "speed sim1", "tracker sim1 sideways", "scenario nope", "frob", "add movie --stall-after lots", "add movie extra"] {
            assert!(parse(w(bad)).is_err(), "`sim {bad}` should be a usage error");
        }
    }

    #[test]
    fn sim_dir_is_per_socket_and_in_temp() {
        let a = sim_dir(Path::new("/x/control.sock"));
        assert_ne!(a, sim_dir(Path::new("/y/control.sock")));
        assert!(a.starts_with(std::env::temp_dir()));
        assert_eq!(sim_id("3"), "sim3");
        assert_eq!(sim_id("sim3"), "sim3");
    }
}
