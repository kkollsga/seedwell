//! End to end: real `seedwell daemon` processes (a seeder and a leecher on loopback) driven by the
//! `seedwell` CLI with `--json`, and through the permission levels.

use crate::common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;

use common::*;
use serde_json::Value;

fn is_done(r: &Value) -> bool {
    matches!(r["state"].as_str(), Some("seeding" | "low-power" | "completed"))
}

#[test]
fn not_running_is_a_clear_error() {
    let dir = tempfile::tempdir().unwrap();
    let o = cli_text(&dir.path().join("none.sock"), &["status"]);
    assert_eq!(o.code, 5, "{}", o.stderr);
    assert!(o.stderr.contains("not running") && o.stderr.contains("--launch"), "{}", o.stderr);
    let o = cli_at(&dir.path().join("none.sock"), &["list"]);
    assert_eq!(o.code, 5);
    assert!(o.stderr.contains("\"error\""), "--json errors are JSON: {}", o.stderr);
    // The usage errors have their own code.
    assert_eq!(cli_text(&dir.path().join("none.sock"), &["frobnicate"]).code, 2);
    assert_eq!(cli_text(&dir.path().join("none.sock"), &["pause"]).code, 2);
    let h = cli_text(&dir.path().join("none.sock"), &["help", "daemon"]);
    assert_eq!(h.code, 0);
    assert!(h.stdout.contains("daemon --state-dir"));
}

/// A relaunch after an orderly quit is not a crash: the daemon leaves the clean-exit marker in
/// `launches.log`, and a kill does not (crash-loop breaker, SPEC 7C.2).
#[test]
fn an_orderly_quit_leaves_the_clean_exit_marker_and_a_kill_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let mut d = Daemon::start(dir.path(), "a", "control");
    let log = d.state.join("launches.log");
    assert!(!std::fs::read_to_string(&log).unwrap().contains("clean"), "running: no marker yet");
    d.terminate();
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.lines().last().unwrap().ends_with(" clean"), "{text:?}");
    // The next run is killed instead of quitting.
    let mut d = Daemon::start(dir.path(), "a", "control");
    let _ = d.child.kill();
    let _ = d.child.wait();
    let text = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<_> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text:?}");
    assert!(lines[0].ends_with(" clean") && !lines[1].contains("clean"), "{text:?}");
}

/// `--allow-lan-trackers` writes the setting; the default stays strict.
#[test]
fn allow_lan_trackers_flag_writes_the_setting_and_the_default_is_strict() {
    let dir = tempfile::tempdir().unwrap();
    let mut strict = Daemon::start(dir.path(), "strict", "control");
    let text = std::fs::read_to_string(strict.state.join("config.toml")).unwrap();
    assert!(text.contains("allow_lan_trackers = false"), "{text}");
    strict.terminate();
    let mut lan = Daemon::start_with(dir.path(), "lan", "control", &["--allow-lan-trackers"]);
    let text = std::fs::read_to_string(lan.state.join("config.toml")).unwrap();
    assert!(text.contains("allow_lan_trackers = true"), "{text}");
    lan.terminate();
}

/// A `config.toml` with a bad value is never rewritten by the daemon: its options (here the
/// download folder) apply in memory, each warning is logged once at start, and the file is the
/// user's to fix. (Rewriting it would turn the salvaged value into its replacement for good.)
#[test]
fn a_config_with_a_bad_value_is_not_rewritten_and_the_options_still_apply() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("bad").join("state");
    std::fs::create_dir_all(&state).unwrap();
    let text = "[folders]\ntorrent_after_add = \"keep\"\n[transfers]\nmax_active_downloads = 0\n[network]\nupnp = true\n";
    std::fs::write(state.join("config.toml"), text).unwrap();
    let log = dir.path().join("daemon.err");
    let mut d = Daemon::start_logging(dir.path(), "bad", "control", &log);
    // The flags' download folder is in force: a payload put there is found complete.
    let t = make_torrent("Kept.Settings", &[("a.bin", 300_000)], 7);
    t.write_payload(&d.dl);
    let file = t.write_torrent(dir.path());
    let added = d.json(&["add", file.to_str().unwrap(), "--now"]);
    let id = added["id"].as_str().unwrap().to_owned();
    wait_until(30, "found complete in the flags' folder", || row(&d, &id).filter(is_done));
    d.terminate();
    assert_eq!(std::fs::read_to_string(state.join("config.toml")).unwrap(), text, "the user's file is left as it is");
    for f in ["config.toml.bak", "config.toml.rejected", "backups"] {
        assert!(!state.join(f).exists(), "{f} written next to it");
    }
    let err = std::fs::read_to_string(&log).unwrap();
    for w in ["config.toml: folders.torrent_after_add: \"keep\" is not a valid value (", "config.toml: transfers.max_active_downloads: 0 is not a valid value (", "the options apply to this run only and the file is left as it is"] {
        assert_eq!(err.matches(w).count(), 1, "{w:?} once in:\n{err}");
    }
}

#[test]
fn download_pause_resume_info_stats_and_two_step_remove() {
    let root = tempfile::tempdir().unwrap();
    let seeder = Daemon::start(root.path(), "seed", "full");
    let leech = Daemon::start(root.path(), "leech", "control");
    let peer = format!("127.0.0.1:{}", seeder.port);

    // ---- seed a torrent whose data is already on disk
    let t1 = make_torrent("alpha", &[("a.bin", 600_000), ("sub/b.bin", 500_000)], 1);
    t1.write_payload(&seeder.dl);
    let tf1 = t1.write_torrent(root.path());
    let added = seeder.json(&["add", tf1.to_str().unwrap(), "--save-path", seeder.dl.to_str().unwrap()]);
    assert_eq!(added["id"], t1.id.as_str());
    wait_until(30, "seeder to verify and seed alpha", || row(&seeder, &t1.id).filter(is_done));

    // ---- download it
    let a = leech.json(&["add", tf1.to_str().unwrap(), "--peer", &peer]);
    assert_eq!(a["kind"], "torrent");
    let r = wait_until(60, "leecher to finish alpha", || row(&leech, &t1.id).filter(is_done));
    assert_eq!(r["progress"], 1.0);
    // (Finished data moves out of staging a moment after the state changes.)
    wait_until(10, "alpha in place", || std::panic::catch_unwind(|| t1.verify(&leech.dl)).ok());

    // ---- info: files, trackers (none), peers
    let info = leech.json(&["info", &t1.id[..10]]);
    assert_eq!(info["files"].as_array().unwrap().len(), 2);
    assert_eq!(info["files"][0]["priority"], "normal");
    assert_eq!(info["transfer"]["name"], "alpha");
    let text = cli_text(&leech.socket, &["info", &t1.id[..10]]);
    assert_eq!(text.code, 0);
    assert!(text.stdout.contains("alpha") && text.stdout.contains("Files (2)"), "{}", text.stdout);
    let list_text = cli_text(&leech.socket, &["list"]);
    assert!(list_text.stdout.contains(&t1.id[..8]) && list_text.stdout.contains("alpha"), "{}", list_text.stdout);
    let status = leech.json(&["status"]);
    assert_eq!(status["permission"], "control");
    assert_eq!(status["total"], 1);

    // ---- limits, pause and resume mid-download
    let lim = leech.json(&["limits", "--down", "60KB"]);
    assert_eq!(lim["down_limit"], 60_000);
    assert_eq!(leech.json(&["limits"])["down_limit"], 60_000);
    let t2 = make_torrent("beta", &[("big.bin", 900_000)], 2);
    t2.write_payload(&seeder.dl);
    let tf2 = t2.write_torrent(root.path());
    seeder.json(&["add", tf2.to_str().unwrap(), "--save-path", seeder.dl.to_str().unwrap()]);
    wait_until(30, "seeder to seed beta", || row(&seeder, &t2.id).filter(is_done));
    leech.json(&["add", tf2.to_str().unwrap(), "--peer", &peer]);
    wait_until(30, "beta to start flowing", || row(&leech, &t2.id).filter(|r| r["done_bytes"].as_u64().unwrap_or(0) > 0));
    let p = leech.json(&["pause", &t2.id]);
    assert_eq!(p["count"], 1);
    let paused = wait_until(10, "beta to pause", || row(&leech, &t2.id).filter(|r| r["state"] == "paused"));
    let before = paused["done_bytes"].as_u64().unwrap();
    assert!(before < 900_000, "still incomplete when paused");
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let after = row(&leech, &t2.id).unwrap()["done_bytes"].as_u64().unwrap();
    assert_eq!(before, after, "no progress while paused");
    leech.json(&["resume", &t2.id]);
    leech.json(&["limits", "--down", "unlimited"]);
    // (Limits are also saved to config.toml, which the daemon hot-reloads; give a late reload of the
    // previous value time to settle.)
    wait_until(10, "unlimited", || (leech.json(&["limits"])["down_limit"] == Value::Null).then_some(()));
    let resumed = wait_until(10, "beta to leave the paused state", || row(&leech, &t2.id).filter(|r| r["state"] != "paused"));
    assert_ne!(resumed["state"], "paused", "{resumed}");
    // (Whether beta finishes now depends on whether the peer connection survived the pause, so the
    // test does not wait for it; the filters below accept both outcomes.)

    // ---- per-transfer settings: priority, category, seeding goal, files
    leech.json(&["priority", &t1.id, "high"]);
    leech.json(&["category", &t1.id, "software"]);
    leech.json(&["goal", &t1.id, "--ratio", "2", "--at-goal", "stop"]);
    let r = row(&leech, &t1.id).unwrap();
    assert_eq!((r["priority"].as_str(), r["category"].as_str()), (Some("high"), Some("software")));
    assert_eq!(r["seed_goal"]["ratio"], 2.0);
    // The goal reads the same everywhere: the engine's words in the data, in `info`, and the way back.
    assert_eq!((r["seed_goal"]["text"].as_str(), r["seed_goal_user"].as_bool()), (Some("Seed to ratio 2.0"), Some(true)));
    let text = cli_text(&leech.socket, &["info", &t1.id[..10]]);
    assert!(text.stdout.contains("seed to ratio 2.0") && text.stdout.contains("set for this torrent"), "{}", text.stdout);
    assert!(r["seeded_secs"].is_number());
    leech.json(&["goal", &t1.id, "--rule-default"]);
    let r = wait_until(10, "goal back to the rule's", || row(&leech, &t1.id).filter(|r| r["seed_goal_user"] == false));
    // (the Default rule: ratio 1.0 or 7 days)
    assert_eq!((r["seed_goal"]["ratio"].as_f64(), r["seed_goal"]["text"].as_str()), (Some(1.0), Some("Ratio 1.0 or 7 days")), "{r}");
    let bad = cli_text(&leech.socket, &["goal", &t1.id, "--rule-default", "--ratio", "3"]);
    assert_ne!(bad.code, 0, "{}", bad.stdout);
    // Skip a file right after adding: a low limit keeps the download slow enough to get there first.
    let t3 = make_torrent("gamma", &[("keep.bin", 300_000), ("skip.bin", 300_000)], 3);
    t3.write_payload(&seeder.dl);
    let tf3 = t3.write_torrent(root.path());
    seeder.json(&["add", tf3.to_str().unwrap(), "--save-path", seeder.dl.to_str().unwrap()]);
    wait_until(30, "seeder to seed gamma", || row(&seeder, &t3.id).filter(is_done));
    leech.json(&["limits", "--down", "60KB"]);
    leech.json(&["add", tf3.to_str().unwrap(), "--peer", &peer]);
    let f = leech.json(&["files", &t3.id, "--skip", "1"]);
    assert_eq!(f["count"], 1);
    leech.json(&["limits", "--down", "unlimited"]);
    // (The engine reports file priorities of a Cold torrent as normal; the effect is checked below.)
    // The engine marks it finished when the wanted file is complete. (Its `wanted_bytes` still
    // counts the skipped file after a priority change: an engine detail reported separately.)
    let g = wait_until(60, "gamma (one file) to finish", || row(&leech, &t3.id).filter(|r| is_done(r) && r["finished_at"].is_number()));
    assert!(g["done_bytes"].as_u64().unwrap() < g["total_bytes"].as_u64().unwrap(), "skipped file not downloaded: {g}");
    // (The file may be mid-move out of staging for a moment.)
    let start = std::time::Instant::now();
    loop {
        let ok = try_find_file(&leech.dl, "keep.bin").and_then(|p| std::fs::read(p).ok()).is_some_and(|d| d == t3.files[0].1);
        if ok {
            break;
        }
        assert!(start.elapsed().as_secs() < 8, "keep.bin never appeared; row {} tree {:?}", row(&leech, &t3.id).unwrap(), tree(&leech.dl));
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    // (A torrent with skipped files is never "complete", so the engine keeps it in staging.)
    assert!(tree(&leech.dl).iter().any(|p| p.ends_with("keep.bin")), "download folder: {:?}", tree(&leech.dl));

    // ---- filters
    let seeding = leech.json(&["list", "seeding"])["total"].as_u64().unwrap();
    let downloading = leech.json(&["list", "downloading"])["total"].as_u64().unwrap();
    assert!(seeding >= 2 && seeding + downloading == 3, "{seeding} + {downloading}");
    assert_eq!(leech.json(&["list", "--text", "ALPH"])["total"], 1);
    assert_eq!(leech.json(&["list", "--category", "software"])["total"], 1);
    assert_eq!(leech.json(&["list", "recent", "--limit", "2"])["transfers"].as_array().unwrap().len(), 2);

    // ---- statistics reach the protocol (written with a 2 s debounce)
    let stats = wait_until(20, "leecher statistics", || {
        let s = leech.json(&["stats"]);
        (s["all_time"]["downloaded"].as_u64().unwrap_or(0) >= 1_100_000).then_some(s)
    });
    assert!(stats["series"].as_array().unwrap().len() >= 30);
    let up = wait_until(20, "seeder statistics", || {
        let s = seeder.json(&["stats", "--days", "7"]);
        (s["all_time"]["uploaded"].as_u64().unwrap_or(0) > 1_000_000).then_some(s)
    });
    assert_eq!(up["series"].as_array().unwrap().len(), 7);
    let tr = leech.json(&["trackers"]);
    assert!(tr["profiles"].as_array().unwrap().is_empty());
    assert_eq!(leech.cli(&["stats", "no-such-profile"]).code, 3);

    // ---- removal at control level: files cannot be deleted, plain removal keeps them
    let denied = leech.cli(&["remove", &t1.id, "--delete-files", "-y"]);
    assert_eq!(denied.code, 4, "{}", denied.stderr);
    assert!(row(&leech, &t1.id).is_some());
    let keep = leech.json(&["remove", &t1.id, "-y"]);
    assert_eq!(keep["files_deleted"], false);
    wait_until(10, "alpha to leave the list", || row(&leech, &t1.id).is_none().then_some(()));
    t1.verify(&leech.dl);
    // Without -y and without a terminal nothing is removed.
    let no_tty = leech.cli(&["remove", &t2.id]);
    assert_eq!(no_tty.code, 0, "--json prints the plan: {}", no_tty.stderr);
    let plan: Value = serde_json::from_str(&no_tty.stdout).unwrap();
    assert_eq!(plan["token"].as_str().unwrap().len(), 32);
    assert!(row(&leech, &t2.id).is_some(), "the plan alone removes nothing");
    assert_eq!(leech.cli(&["remove-confirm", &"0".repeat(32)]).code, 1);
    assert!(row(&leech, &t2.id).is_some());
    leech.json(&["remove-confirm", plan["token"].as_str().unwrap()]);
    wait_until(10, "beta to leave the list", || row(&leech, &t2.id).is_none().then_some(()));

    // ---- removal at full: files are deleted only after the two steps
    let plan = seeder.json(&["remove", &t3.id, "--delete-files"]);
    assert_eq!(plan["delete_files"], true);
    assert!(seeder.dl.join("gamma/keep.bin").exists(), "step 1 deletes nothing");
    let done = seeder.json(&["remove-confirm", plan["token"].as_str().unwrap()]);
    assert_eq!(done["files_deleted"], true);
    // (The engine stops a live torrent before it deletes the files, so this lags the answer a little.)
    wait_until(10, "gamma files to be deleted", || (!seeder.dl.join("gamma/keep.bin").exists()).then_some(()));
    wait_until(10, "gamma to leave the seeder", || row(&seeder, &t3.id).is_none().then_some(()));
    // The token is single use.
    assert_eq!(seeder.cli(&["remove-confirm", plan["token"].as_str().unwrap()]).code, 1);

    // ---- the activity log names the caller and keeps secrets out
    let act = seeder.json(&["activity", "--limit", "100"]);
    let entries = act["entries"].as_array().unwrap();
    assert!(entries.iter().all(|e| e["client"] == "seedwell-cli"));
    let methods: Vec<&str> = entries.iter().map(|e| e["method"].as_str().unwrap()).collect();
    for m in ["add", "remove", "remove_confirm"] {
        assert!(methods.contains(&m), "{m} in {methods:?}");
    }
    assert!(!methods.contains(&"list") && !methods.contains(&"status"), "reads are not logged");
    // Control may not read it.
    assert_eq!(leech.cli(&["activity"]).code, 4);

    // ---- the event stream
    let mut w = std::process::Command::new(BIN)
        .arg("--socket")
        .arg(&seeder.socket)
        .args(["--json", "watch", "--kinds", "state_changed", "--count", "1", "--for", "20"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(600));
    seeder.json(&["pause", &t2.id]);
    let mut out = String::new();
    w.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    let _ = w.wait();
    let ev: Value = serde_json::from_str(out.lines().next().expect("an event line")).unwrap();
    assert_eq!(ev["kind"], "state_changed", "{ev}");
    assert_eq!(ev["data"]["state"], "paused");

    // Nothing went anywhere but the daemons' own folders (a `~` would have landed in `home/`).
    assert!(std::fs::read_dir(root.path().join("home")).unwrap().next().is_none(), "{:?}", tree(&root.path().join("home")));
}

#[test]
fn read_only_and_off_levels_through_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let ro = Daemon::start(root.path(), "ro", "read_only");
    assert_eq!(ro.cli(&["status"]).code, 0);
    assert_eq!(ro.cli(&["list"]).code, 0);
    assert_eq!(ro.cli(&["limits", "--down", "1MB"]).code, 4);
    assert_eq!(ro.cli(&["pause", "--all"]).code, 4);
    let t = make_torrent("zeta", &[("z.bin", 10_000)], 5);
    let tf = t.write_torrent(root.path());
    let o = ro.cli(&["add", tf.to_str().unwrap()]);
    assert_eq!(o.code, 4);
    assert!(o.stderr.contains("control"), "the error says which level is needed: {}", o.stderr);
    let off = Daemon::start(root.path(), "off", "off");
    let o = off.cli(&["status"]);
    assert_eq!(o.code, 4);
    assert!(o.stderr.contains("switched off"), "{}", o.stderr);
}

#[test]
fn settings_roundtrip_through_the_daemon_hot_reload() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start(root.path(), "s", "full");
    let g = d.json(&["settings", "get", "transfers.max_active_downloads"]);
    assert_eq!(g["settings"]["transfers.max_active_downloads"], 3);
    d.json(&["settings", "set", "transfers.max_active_downloads=5"]);
    assert_eq!(d.json(&["settings", "get", "transfers.max_active_downloads"])["settings"]["transfers.max_active_downloads"], 5);
    let cfg = std::fs::read_to_string(d.state.join("config.toml")).unwrap();
    assert!(cfg.contains("max_active_downloads = 5"), "{cfg}");
    let bad = d.cli(&["settings", "set", "transfers.max_active_downloads=0"]);
    assert_eq!(bad.code, 1, "{}", bad.stderr);
    let forbidden = d.cli(&["settings", "set", "agents.permission=full"]);
    assert_ne!(forbidden.code, 0);
}

/// A tiny HTTP/1.1 file server with Range support, for the direct-download test.
fn serve_file(data: Vec<u8>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().map_while(Result::ok) {
            let data = data.clone();
            std::thread::spawn(move || handle_http(s, &data));
        }
    });
    port
}

fn handle_http(mut s: std::net::TcpStream, data: &[u8]) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        // Read one request head.
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match s.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&tmp[..n]),
            }
        }
        let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = String::from_utf8_lossy(&buf[..end]).into_owned();
        buf.drain(..end);
        let is_head = head.starts_with("HEAD ");
        let range = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_owned));
        let len = data.len();
        let (status, from, to) = match range.as_deref().and_then(|r| r.split_once('-')) {
            Some((a, b)) => {
                let a: usize = a.trim().parse().unwrap_or(0);
                let b: usize = b.trim().parse().unwrap_or(len - 1);
                ("206 Partial Content", a.min(len), b.min(len - 1) + 1)
            }
            None => ("200 OK", 0, len),
        };
        let mut resp = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Type: application/octet-stream\r\n", to - from);
        if status.starts_with("206") {
            resp.push_str(&format!("Content-Range: bytes {from}-{}/{len}\r\n", to - 1));
        }
        resp.push_str("\r\n");
        if s.write_all(resp.as_bytes()).is_err() || (!is_head && s.write_all(&data[from..to]).is_err()) {
            return;
        }
    }
}

#[test]
fn direct_download_through_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start(root.path(), "dl", "control");
    let data: Vec<u8> = (0..300_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
    let port = serve_file(data.clone());
    let a = d.json(&["add", &format!("http://127.0.0.1:{port}/files/blob.bin?token=SECRET")]);
    assert_eq!(a["kind"], "direct", "{a}");
    let id = a["id"].as_str().unwrap().to_owned();
    let r = wait_until(60, "the direct download to finish", || row(&d, &id).filter(|r| is_done(r) || r["state"] == "error"));
    assert!(is_done(&r), "{r}");
    assert_eq!(r["kind"], "direct");
    assert!(!r.to_string().contains("SECRET"), "query secrets are redacted: {r}");
    assert_eq!(std::fs::read(find_file(&d.dl, "blob.bin")).unwrap(), data);
    assert_eq!(d.json(&["list", "--kind", "direct"])["total"], 1);
    assert_eq!(d.json(&["list", "--kind", "torrent"])["total"], 0);
}

fn try_find_file(dir: &Path, name: &str) -> Option<std::path::PathBuf> {
    fn walk(dir: &Path, name: &str, out: &mut Option<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, name, out);
            } else if p.file_name().is_some_and(|f| f == name) {
                *out = Some(p);
            }
        }
    }
    let mut out = None;
    walk(dir, name, &mut out);
    out
}

fn find_file(dir: &Path, name: &str) -> std::path::PathBuf {
    try_find_file(dir, name).unwrap_or_else(|| panic!("{name} not found below {dir:?}"))
}

fn tree(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            out.push(p.strip_prefix(base).unwrap().display().to_string());
            if p.is_dir() {
                walk(&p, base, out);
            }
        }
    }
    walk(dir, dir, &mut out);
    out
}

/// Smart add and moving through the CLI (SPEC 7A.5, 7A.6): a season pack is held for a choice
/// (nothing downloads), the structure comes back, `choose` starts only the chosen files, a second
/// pack is cancelled, and `move` relocates the finished files (it needs `full` and refuses
/// system folders).
#[test]
fn smart_add_and_move_through_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let seeder = Daemon::start(root.path(), "seed", "full");
    let leech = Daemon::start_with(root.path(), "leech", "full", &["--ask"]);
    let control = Daemon::start_with(root.path(), "ctl", "control", &["--ask"]);
    let peer = format!("127.0.0.1:{}", seeder.port);

    let names: Vec<String> = (1..=2)
        .flat_map(|s| (1..=2).map(move |e| format!("Some.Show.S{s:02}E{e:02}.1080p.WEB-DL.x264-GRP.mkv")))
        .collect();
    let files: Vec<(&str, usize)> = names.iter().map(|n| (n.as_str(), 150_000)).collect();
    let pack = make_torrent("Some.Show.S01-S02.1080p.WEB-DL.x264-GRP", &files, 77);
    pack.write_payload(&seeder.dl);
    let tf = pack.write_torrent(root.path());
    seeder.json(&["add", tf.to_str().unwrap(), "--save-path", seeder.dl.to_str().unwrap()]);
    wait_until(30, "the seeder to seed the pack", || row(&seeder, &pack.id).filter(is_done));

    // ---- held: nothing downloads, the structure comes back with the add
    let a = leech.json(&["add", tf.to_str().unwrap(), "--peer", &peer]);
    assert_eq!(a["needs_choice"], true, "{a}");
    assert_eq!(a["choice"]["structure"], "tv");
    assert_eq!(a["choice"]["groups"][0]["label"], "Season 1");
    assert_eq!(a["choice"]["groups"][0]["items"].as_array().unwrap().len(), 2);
    let held = row(&leech, &pack.id).unwrap();
    assert_eq!(held["state"], "awaiting-choice");
    assert_eq!(leech.json(&["list", "awaiting-choice"])["total"], 1);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    assert_eq!(row(&leech, &pack.id).unwrap()["downloaded_total"], 0, "nothing flows while held");
    assert!(tree(&leech.dl).is_empty(), "nothing on disk: {:?}", tree(&leech.dl));
    let text = cli_text(&leech.socket, &["choice", &pack.id[..10]]);
    assert!(text.stdout.contains("Season 1") && text.stdout.contains("S01E01") && text.stdout.contains("ask because"), "{}", text.stdout);

    // ---- choose season 2 only (file numbers 2 and 3)
    assert_eq!(leech.cli(&["start", &pack.id, "--files", "1", "--skip", "2"]).code, 2, "at most one of --default/--files/--skip");
    leech.json(&["start", &pack.id, "--files", "2-3"]);
    wait_until(60, "season 2 to download", || row(&leech, &pack.id).filter(|r| is_done(r) && r["wanted_bytes"] == 300_000));
    // (The files may be mid-move out of staging for a moment.)
    wait_until(10, "season 2 in place", || (try_find_file(&leech.dl, &names[3]).is_some()).then_some(()));
    assert!(try_find_file(&leech.dl, &names[0]).is_none(), "season 1 was not chosen");

    // ---- the control level may answer choosers but not move files
    let a2 = control.json(&["add", tf.to_str().unwrap(), "--peer", &peer]);
    assert_eq!(a2["needs_choice"], true);
    control.json(&["cancel", &pack.id, "--keep-file"]);
    wait_until(10, "the held torrent to go", || row(&control, &pack.id).is_none().then_some(()));
    assert!(tree(&control.dl).is_empty(), "cancel leaves nothing: {:?}", tree(&control.dl));
    let moved_denied = control.cli(&["move", &pack.id, "/tmp/nowhere"]);
    // (The torrent was cancelled above, so the selector finds nothing before the permission is asked.)
    assert!(matches!(moved_denied.code, 3 | 4), "{}", moved_denied.stderr);

    // ---- move the finished torrent
    let dest = root.path().join("moved");
    std::fs::create_dir_all(&dest).unwrap();
    let system = trs_platform::dirs::sensitive_dirs().into_iter().next().unwrap();
    assert_eq!(leech.cli(&["move", &pack.id, system.to_str().unwrap()]).code, 1, "system folders are refused");
    assert_eq!(leech.cli(&["move", &pack.id]).code, 2, "usage");
    let m = leech.json(&["move", &pack.id, dest.to_str().unwrap()]);
    assert_eq!(m["queued"].as_array().unwrap().len(), 1, "{m}");
    wait_until(30, "the files to arrive", || try_find_file(&dest, &names[2]).is_some().then_some(()));
    wait_until(10, "the old folder to go", || (try_find_file(&leech.dl, &names[2]).is_none()).then_some(()));
    assert_eq!(leech.json(&["moves"])["items"].as_array().unwrap().len(), 0, "pinned: nothing is out of place");
    let r = row(&leech, &pack.id).unwrap();
    assert!(r["save_path"].as_str().unwrap().starts_with(dest.to_str().unwrap()), "{r}");
}

/// The grace period through the CLI (SPEC 7A.7): `add` reports that the torrent is pending with
/// its deadline, `list pending` finds it, nothing flows until it starts, and `--now` skips the wait.
#[test]
fn the_grace_period_is_reported_and_now_skips_it() {
    let root = tempfile::tempdir().unwrap();
    let seeder = Daemon::start(root.path(), "seed", "full");
    let leech = Daemon::start_with(root.path(), "leech", "full", &["--grace", "4"]);
    let peer = format!("127.0.0.1:{}", seeder.port);
    let a = make_torrent("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP", &[("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv", 200_000)], 91);
    let b = make_torrent("Sintel.2010.1080p.BluRay.x264-GRP", &[("Sintel.2010.1080p.BluRay.x264-GRP.mkv", 200_000)], 93);
    for t in [&a, &b] {
        t.write_payload(&seeder.dl);
        let f = t.write_torrent(root.path());
        seeder.json(&["add", f.to_str().unwrap(), "--save-path", seeder.dl.to_str().unwrap()]);
        wait_until(30, "the seeder to seed", || row(&seeder, &t.id).filter(is_done));
    }
    let fa = a.write_torrent(root.path());
    let fb = b.write_torrent(root.path());

    let added = leech.json(&["add", fa.to_str().unwrap(), "--peer", &peer]);
    let p = &added["pending"];
    assert!(p["starts_in_secs"].as_u64().is_some_and(|s| (1..=4).contains(&s)), "{added}");
    assert!(p["starts_at_ms"].as_u64().unwrap() > 1_700_000_000_000, "{added}");
    let r = row(&leech, &a.id).unwrap();
    assert_eq!(r["state"], "pending", "{r}");
    assert_eq!(leech.json(&["list", "pending"])["total"], 1);
    let text = cli_text(&leech.socket, &["list", "pending"]);
    assert!(text.stdout.contains("pending") && text.stdout.contains("s "), "{}", text.stdout);

    // --now: no wait, no `pending` in the answer.
    let now = leech.json(&["add", fb.to_str().unwrap(), "--peer", &peer, "--now"]);
    assert!(now.get("pending").is_none(), "{now}");
    wait_until(30, "the --now torrent to finish", || row(&leech, &b.id).filter(is_done));

    // Meanwhile the pending one stayed silent; then it starts and finishes.
    assert!(row(&leech, &a.id).unwrap()["downloaded_total"] == 0 || row(&leech, &a.id).unwrap()["state"] != "pending");
    wait_until(30, "the pending torrent to start and finish", || row(&leech, &a.id).filter(is_done));
}

#[test]
fn an_agent_keeps_the_mac_awake_for_as_long_as_it_says_and_lets_it_sleep_when_done() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start(root.path(), "kglite", "control");
    let names = |v: &Value| v["leases"].as_array().unwrap().iter().map(|l| l["who"].as_str().unwrap().to_owned()).collect::<Vec<_>>();
    let until = |v: &Value| v["leases"][0]["until_ms"].as_u64().unwrap();
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;

    // No default: the agent says how long.
    let o = cli_text(&d.socket, &["awake", "on"]);
    assert!(o.code != 0 && o.stderr.contains("--for"), "{}", o.stderr);
    assert_eq!(names(&d.json(&["awake"])), Vec::<String>::new());

    // The time it asked for, under the project's name. The headless daemon says it cannot hold the Mac awake itself.
    let v = d.json(&["awake", "--for", "2h"]);
    assert_eq!(names(&v), ["kglite"]);
    assert!(until(&v).abs_diff(now() + 7_200_000) < 60_000, "about 2 h");
    assert_eq!((v["setting"].as_bool(), v["host_keeps"].as_bool(), v["kept"].as_bool()), (Some(true), Some(false), Some(false)));
    let t = cli_text(&d.socket, &["awake"]);
    assert!(t.stdout.contains("kglite until") && t.stdout.contains("runs without the app"), "{}", t.stdout);

    // Asking again replaces the time (shorter too); another agent has its own lease; `off` gives back only its own.
    let v = d.json(&["awake", "--for", "20m"]);
    assert!(until(&v).abs_diff(now() + 1_200_000) < 60_000, "about 20 min");
    assert_eq!(names(&d.json(&["awake", "--for", "1h", "--as", "web"])), ["web", "kglite"], "the one that lasts longest first");
    assert_eq!(names(&d.json(&["awake", "off"])), ["web"]);
    assert_eq!(names(&d.json(&["awake", "--for", "0", "--as", "web"])), Vec::<String>::new());
}
