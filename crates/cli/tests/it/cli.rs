//! The CLI as an agent sees it: exit codes, the `"v":1` JSON shapes, ambiguous selectors, held torrents,
//! Developer mode, the debug commands (no passkey ever shows), the skill and simulated torrents, against
//! real `seedwell daemon` processes in temp directories. Nothing here touches the real home or app.

use crate::common;

use std::path::Path;

use common::*;
use serde_json::Value;

const PASSKEY: &str = "0123456789abcdef0123456789abcdef";
const SECRET2: &str = "AUTHSECRET99";

fn is_done(r: &Value) -> bool {
    matches!(r["state"].as_str(), Some("seeding" | "low-power" | "completed"))
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().expect("an object").keys().cloned().collect();
    k.sort();
    k
}

fn out_json(o: &Out) -> Value {
    assert_eq!(o.code, 0, "{}", o.stderr);
    serde_json::from_str(&o.stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {}", o.stdout))
}

#[test]
fn exit_codes_json_version_and_ambiguous_selectors() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start(root.path(), "a", "control");
    let a = make_torrent("Ubuntu.24.04.Desktop", &[("a.iso", 40_000)], 1);
    let b = make_torrent("Ubuntu.24.04.Server", &[("b.iso", 40_000)], 2);
    for t in [&a, &b] {
        let f = t.write_torrent(root.path());
        d.json(&["add", f.to_str().unwrap(), "--paused"]);
    }
    // usage 2, not found 3, ambiguous 3 with candidates
    assert_eq!(d.cli(&["pause"]).code, 2);
    assert_eq!(d.cli(&["frobnicate"]).code, 2);
    assert_eq!(d.cli(&["list", "nonsense"]).code, 2);
    let nf = d.cli(&["info", "no-such-torrent"]);
    assert_eq!(nf.code, 3, "{}", nf.stderr);
    let e: Value = serde_json::from_str(&nf.stderr).unwrap();
    assert_eq!((e["v"].as_u64(), e["error"]["code"].as_str(), e["error"]["exit"].as_u64()), (Some(1), Some("not_found"), Some(3)));
    let amb = d.cli(&["info", "ubuntu"]);
    assert_eq!(amb.code, 3, "{}", amb.stderr);
    let e: Value = serde_json::from_str(&amb.stderr).unwrap();
    assert_eq!(e["error"]["code"], "ambiguous");
    assert_eq!(e["error"]["candidates"].as_array().unwrap().len(), 2);
    let text = cli_text(&d.socket, &["info", "ubuntu"]);
    assert_eq!(text.code, 3);
    assert!(text.stderr.contains("matches 2 torrents") && text.stderr.contains("Ubuntu.24.04.Desktop"), "{}", text.stderr);
    // a unique name part and an id prefix both work
    assert_eq!(d.json(&["info", "desktop"])["transfer"]["id"], a.id.as_str());
    assert_eq!(d.json(&["info", &b.id[..8]])["transfer"]["id"], b.id.as_str());
    // filters select sets
    let p = d.json(&["resume", "--state", "paused"]);
    assert_eq!(p["count"], 2);

    // JSON: "v":1 first, stable key sets
    let o = d.cli(&["status"]);
    assert!(o.stdout.trim_start().starts_with("{\n  \"v\": 1"), "{}", o.stdout);
    let st = out_json(&o);
    assert_eq!(keys(&st), ["counts", "limits", "permission", "protocol", "reachability", "server", "summary", "total", "totals", "v"]);
    // Loopback daemon without port mapping: nothing has connected in, mapping off.
    assert_eq!(keys(&st["reachability"]), ["behind_cgnat", "incoming", "mapping", "port", "router_unavailable", "state", "summary"]);
    assert_eq!(st["reachability"]["state"], "unknown");
    assert_eq!(keys(&st["totals"]), ["cold", "connections", "down_rate", "hot", "listen_port", "up_rate", "warm"]);
    let l = d.json(&["list"]);
    assert_eq!(keys(&l), ["offset", "total", "transfers", "v"]);
    let row = &l["transfers"][0];
    for k in ["id", "name", "state", "progress", "total_bytes", "down_rate", "up_rate", "eta_secs", "tracker_host", "private", "save_path", "category", "seed_goal"] {
        assert!(row.get(k).is_some(), "row key {k}: {row}");
    }
    let i = d.json(&["info", "desktop"]);
    assert_eq!(keys(&i).iter().filter(|k| ["files", "peers", "trackers", "transfer", "v"].contains(&k.as_str())).count(), 5);
    assert_eq!(keys(&i["files"][0]), ["done", "index", "length", "padding", "path", "priority"]);
    let lim = d.json(&["limits"]);
    assert_eq!(keys(&lim), ["alt_active", "alt_down_limit", "alt_scheduled", "alt_up_limit", "down_limit", "up_limit", "v"]);

    // permission 4
    let ro = Daemon::start(root.path(), "ro", "read_only");
    assert_eq!(ro.cli(&["pause", "--all"]).code, 4);
    let off = Daemon::start(root.path(), "off", "off");
    assert_eq!(off.cli(&["status"]).code, 4);
    // 5: nothing listens
    assert_eq!(cli_at(&root.path().join("none.sock"), &["status"]).code, 5);
    // help
    let h = cli_text(&d.socket, &["pause", "--help"]);
    assert_eq!(h.code, 0);
    assert!(h.stdout.contains("EXAMPLES") && h.stdout.contains("seedwell pause --all") && h.stdout.contains("JSON"), "{}", h.stdout);
    let ag = cli_text(&d.socket, &["help", "agents"]);
    assert!(ag.stdout.contains("seedwell debug health") && ag.stdout.contains("seedwell sim add") && ag.stdout.contains("seedwell dev on"), "{}", ag.stdout);
}

#[test]
fn held_torrents_start_and_cancel() {
    let root = tempfile::tempdir().unwrap();
    let seeder = Daemon::start(root.path(), "seed", "full");
    let leech = Daemon::start_with(root.path(), "leech", "full", &["--ask"]);
    let peer = format!("127.0.0.1:{}", seeder.port);
    let names: Vec<String> = (1..=2).flat_map(|s| (1..=2).map(move |e| format!("Some.Show.S{s:02}E{e:02}.1080p.WEB-DL.x264-GRP.mkv"))).collect();
    let files: Vec<(&str, usize)> = names.iter().map(|n| (n.as_str(), 100_000)).collect();
    let pack = make_torrent("Some.Show.S01-S02.1080p.WEB-DL.x264-GRP", &files, 55);
    pack.write_payload(&seeder.dl);
    let tf = pack.write_torrent(root.path());
    seeder.json(&["add", tf.to_str().unwrap(), "--save-path", seeder.dl.to_str().unwrap()]);
    wait_until(30, "seed", || row(&seeder, &pack.id).filter(is_done));

    let text = cli_text(&leech.socket, &["add", tf.to_str().unwrap(), "--peer", &peer]);
    assert!(text.stdout.contains("seedwell start") && text.stdout.contains("seedwell cancel"), "{}", text.stdout);
    assert_eq!(row(&leech, &pack.id).unwrap()["state"], "awaiting-choice");
    // a running torrent cannot be started or cancelled; a held one can
    let c = leech.json(&["cancel", "Some.Show", "--keep-file"]);
    assert_eq!((c["action"].as_str(), c["torrent_file"].as_str()), (Some("cancelled"), Some("kept")));
    wait_until(10, "gone", || row(&leech, &pack.id).is_none().then_some(()));
    assert_eq!(leech.cli(&["cancel", "Some.Show"]).code, 3, "nothing matches any more");
    leech.json(&["add", tf.to_str().unwrap(), "--peer", &peer]);
    // Start = every file ticked
    let s = leech.json(&["start", "some.show"]);
    assert_eq!(s["action"], "started");
    wait_until(60, "all four files", || row(&leech, &pack.id).filter(|r| is_done(r) && r["wanted_bytes"] == 400_000));
    assert_eq!(leech.cli(&["start", "some.show"]).code, 1, "not held any more");
    assert_eq!(leech.cli(&["cancel", "some.show"]).code, 1, "not held any more");
    // `files --only` wants exactly those
    let f = leech.json(&["files", "some.show", "--only", "0"]);
    assert_eq!(f["count"], 1);
}

#[test]
fn dev_mode_from_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start(root.path(), "d", "full");
    let s = d.json(&["dev", "status"]);
    assert_eq!(s["developer_mode"], false);
    assert!(std::path::Path::new(s["log_path"].as_str().unwrap()).ends_with(std::path::Path::new("logs").join("trackers.log")));
    let on = d.json(&["dev", "on"]);
    assert_eq!(on["developer_mode"], true);
    let cfg = std::fs::read_to_string(d.state.join("config.toml")).unwrap();
    assert!(cfg.contains("developer_mode = true"), "{cfg}");
    assert!(cli_text(&d.socket, &["dev", "status"]).stdout.contains("Developer mode is ON"));
    assert_eq!(d.json(&["dev", "off"])["developer_mode"], false);
    assert_eq!(d.cli(&["dev", "maybe"]).code, 2);
    let ctl = Daemon::start(root.path(), "c", "control");
    assert_eq!(ctl.cli(&["dev", "on"]).code, 4, "changing a setting needs full");
}

/// A passkey in an announce URL must not appear in any command's output, text or JSON.
#[test]
fn passkeys_never_appear_in_any_debug_output() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start_full(root.path(), "t", "full", &["--dev-mode"], true);
    let announce = format!("http://tracker.invalid/{PASSKEY}/announce?passkey={PASSKEY}&authkey={SECRET2}");
    let t = make_torrent_ann("Hidden.Rivers.2016.1080p.BluRay", &[("h.mkv", 60_000)], 9, Some(&announce));
    let f = t.write_torrent(root.path());
    d.json(&["add", f.to_str().unwrap(), "--paused"]);
    d.json(&["resume", "hidden"]);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    let cmds: Vec<Vec<&str>> = vec![
        vec!["info", "hidden"],
        vec!["list"],
        vec!["status"],
        vec!["trackers"],
        vec!["debug", "health"],
        vec!["debug", "trackers"],
        vec!["debug", "trackers", "hidden"],
        vec!["debug", "trackers", "--tracker", "tracker.invalid"],
        vec!["debug", "peers", "hidden"],
        vec!["debug", "perf"],
        vec!["debug", "announce", "hidden"],
        vec!["reannounce", "hidden"],
        vec!["log", "trackers", "--limit", "100"],
        vec!["debug", "trackers", "log"],
        vec!["activity"],
        vec!["watch", "--count", "1", "--for", "1"],
    ];
    let mut shown = 0;
    for json in [false, true] {
        for c in &cmds {
            let o = if json { d.cli(c) } else { cli_text(&d.socket, c) };
            for s in [PASSKEY, SECRET2] {
                assert!(!o.stdout.contains(s) && !o.stderr.contains(s), "{c:?} (json {json}) leaked a secret:\n{}\n{}", o.stdout, o.stderr);
            }
            shown += o.stdout.len();
        }
    }
    assert!(shown > 1000);
    // the tracker row is there, masked
    let tr = d.json(&["debug", "trackers", "hidden"]);
    let row = &tr["transfers"][0]["trackers"][0];
    assert!(row["url"].as_str().unwrap().contains("tracker.invalid"), "{tr}");
    assert_eq!(keys(row), ["failures", "last_announce_at", "last_success_at", "leechers", "manual_ok_at", "next_announce_at", "next_reason", "seeders", "state", "status", "tier", "tripped", "url"]);
    // the log on disk is masked too
    let log = std::fs::read_to_string(d.state.join("logs/trackers.log")).unwrap_or_default();
    assert!(!log.contains(PASSKEY) && !log.contains(SECRET2), "{log}");
}

#[test]
fn debug_json_shapes() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start(root.path(), "s", "read_only");
    let h = d.json(&["debug", "health"]);
    assert_eq!(keys(&h), ["issues", "ok", "summary", "v"]);
    let p = d.json(&["debug", "perf"]);
    assert_eq!(keys(&p), ["limits", "peers", "probe", "process", "torrent_count", "torrents", "totals", "v"]);
    assert_eq!(keys(&p["process"]), ["cpu_percent", "open_fds", "pid", "rss_bytes", "threads"]);
    assert!(p["process"]["rss_bytes"].as_u64().unwrap_or(1) > 0);
    let probe = keys(&p["probe"]);
    for k in ["online", "listen_port", "verify_slots", "verify_free", "max_connections", "dht_running", "portmap_reported", "counters"] {
        assert!(probe.contains(&k.to_owned()), "probe key {k}: {probe:?}");
    }
    let t = d.json(&["debug", "trackers"]);
    assert_eq!(keys(&t), ["offline_since", "online", "transfers", "truncated", "v"]);
    assert_eq!(d.cli(&["debug", "peers", "nothing"]).code, 3);
    assert_eq!(d.cli(&["debug", "announce", "nothing"]).code, 3);
    // watch mode prints one JSON line per tick
    let o = std::process::Command::new(BIN).arg("--socket").arg(&d.socket).args(["--json", "debug", "perf", "--watch"]).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let mut o = o;
    let _ = o.kill();
    let out = o.wait_with_output().unwrap();
    let lines = String::from_utf8_lossy(&out.stdout).lines().count();
    assert!(lines >= 2, "{lines} lines");
}

#[test]
fn skill_commands_write_only_where_told() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("skills/seedwell");
    let d = Daemon::start(root.path(), "k", "read_only");
    let show = cli_text(&d.socket, &["skill", "show"]);
    assert_eq!(show.code, 0);
    assert!(show.stdout.starts_with("---\nname: seedwell\ndescription:") && show.stdout.contains("seedwell debug health"), "{}", &show.stdout[..200.min(show.stdout.len())]);
    let i = d.cli(&["skill", "install", "--dir", dir.to_str().unwrap()]);
    assert_eq!(i.code, 0, "{}", i.stderr);
    let v: Value = serde_json::from_str(&i.stdout).unwrap();
    assert_eq!(v["files"][0]["status"], "written");
    assert!(dir.join("SKILL.md").is_file() && dir.join("REFERENCE.md").is_file());
    std::fs::write(dir.join("SKILL.md"), "edited").unwrap();
    // no terminal: a modified file is not replaced without --force
    let again = d.cli(&["skill", "install", "--dir", dir.to_str().unwrap()]);
    assert_eq!(again.code, 1);
    assert_eq!(std::fs::read_to_string(dir.join("SKILL.md")).unwrap(), "edited");
    assert_eq!(d.cli(&["skill", "install", "--dir", dir.to_str().unwrap(), "--force"]).code, 0);
    assert_eq!(d.cli(&["skill", "uninstall", "--dir", dir.to_str().unwrap()]).code, 0);
    assert!(!dir.exists());
    // the home directory of the test stayed empty
    assert!(std::fs::read_dir(root.path().join("home")).map(|mut d| d.next().is_none()).unwrap_or(true));
}

// ----------------------------------------------------------------------------- sim

fn sim_daemon(root: &Path, name: &str) -> Daemon {
    std::fs::create_dir_all(root.join("tmp")).unwrap();
    Daemon::start_full(root, name, "full", &["--allow-loopback-trackers"], true)
}

fn sim(root: &Path, d: &Daemon, args: &[&str]) -> Out {
    let mut a = vec!["--json", "--yes-i-know"];
    a.extend_from_slice(args);
    cli_in(root, &d.socket, &a)
}

fn wait_row(d: &Daemon, id: &str, secs: u64, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    wait_until(secs, what, || row(d, id).filter(|r| f(r)))
}

#[test]
fn sim_needs_developer_mode_or_the_flag() {
    let root = tempfile::tempdir().unwrap();
    let d = sim_daemon(root.path(), "g");
    let o = cli_in(root.path(), &d.socket, &["sim", "add", "single", "--size", "100KB"]);
    assert_eq!(o.code, 4, "{}", o.stderr);
    assert!(o.stderr.contains("seedwell dev on"), "{}", o.stderr);
    assert_eq!(d.cli(&["dev", "on"]).code, 0);
    let o = cli_in(root.path(), &d.socket, &["--json", "sim", "add", "single", "--size", "100KB", "--speed", "unlimited"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    cli_in(root.path(), &d.socket, &["sim", "clear"]);
}

#[test]
fn sim_season_pack_downloads_through_the_real_engine_then_clears_everything() {
    let root = tempfile::tempdir().unwrap();
    let d = sim_daemon(root.path(), "s");
    let o = sim(root.path(), &d, &["sim", "add", "season-pack", "--seasons", "2", "--episodes", "2", "--size", "3MB", "--peers", "2", "--extras"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let added = out_json(&o);
    assert_eq!(added["v"], 1);
    let id = added["torrent_id"].as_str().unwrap().to_owned();
    assert!(added["files"].as_array().unwrap().len() >= 4);
    let r = wait_row(&d, &id, 90, "the pack to finish", is_done);
    assert_eq!(r["progress"], 1.0);
    let save = Path::new(added["dir"].as_str().unwrap()).to_owned();
    assert!(save.starts_with(root.path().join("tmp")) || save.starts_with(std::fs::canonicalize(root.path().join("tmp")).unwrap()), "{save:?}");
    // (The engine announces a moment after start, once it knows its port.)
    let l = wait_until(40, "the tracker to be asked", || {
        let l = out_json(&sim(root.path(), &d, &["sim", "list"]));
        (l["sims"][0]["announces"].as_u64().unwrap_or(0) >= 1).then_some(l)
    });
    assert_eq!(l["sims"][0]["id"], "sim1");
    assert_eq!(l["host_running"], true);
    // the host's tracker answers `debug announce`, and the gate holds the second one back
    let first = out_json(&d.cli(&["debug", "announce", &id[..10]]));
    let second = out_json(&d.cli(&["debug", "announce", &id[..10]]));
    let deferred = |v: &Value| v["transfers"][0]["trackers"].as_array().unwrap().iter().any(|t| t["not_before"].is_number());
    assert!(deferred(&second), "the second Ask Again inside the minimum interval is deferred: {first} {second}");
    // clear: torrent, data, folders and the host process are gone
    let sim_root = std::fs::read_dir(root.path().join("tmp").join("seedwell-sim")).unwrap().next().unwrap().unwrap().path();
    let c = out_json(&sim(root.path(), &d, &["sim", "clear"]));
    assert_eq!(c["removed_torrents"], 1);
    wait_until(20, "the torrent to leave", || row(&d, &id).is_none().then_some(()));
    wait_until(10, "the sim folder to go", || (!sim_root.exists()).then_some(()));
    assert!(tree_files(&d.dl).is_empty(), "no data left: {:?}", tree_files(&d.dl));
}

#[test]
fn sim_tracker_modes_halt_and_recover_and_speed_changes_the_rate() {
    let root = tempfile::tempdir().unwrap();
    let d = sim_daemon(root.path(), "t");
    // an unregistered tracker halts the row with the tracker's words
    let o = sim(root.path(), &d, &["sim", "add", "movie", "--size", "2MB", "--tracker", "unregistered", "--stall-after", "0%", "--private"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let added = out_json(&o);
    let id = added["torrent_id"].as_str().unwrap().to_owned();
    let halted = wait_until(40, "the halted tracker row", || {
        let t = d.json(&["debug", "trackers", &id[..10]]);
        (t["transfers"][0]["trackers"][0]["state"] == "halted").then_some(t)
    });
    assert!(halted["transfers"][0]["trackers"][0]["status"].as_str().unwrap().to_lowercase().contains("unregistered"), "{halted}");
    let h = out_json(&d.cli(&["debug", "health"]));
    assert!(h["issues"].as_array().unwrap().iter().any(|i| i["area"] == "tracker"), "{h}");
    assert!(!halted.to_string().contains("tracker_passkey"), "no secrets");
    // flip it live
    assert_eq!(sim(root.path(), &d, &["sim", "tracker", "sim1", "ok"]).code, 0);
    assert_eq!(sim(root.path(), &d, &["sim", "tracker", "sim1", "sideways"]).code, 2);
    sim(root.path(), &d, &["sim", "clear"]);

    // speed: 20 MB at 300 KB/s, then raised
    let o = sim(root.path(), &d, &["sim", "add", "single", "--size", "40MB", "--speed", "300KB/s", "--peers", "1"]);
    let id = out_json(&o)["torrent_id"].as_str().unwrap().to_owned();
    let slow = wait_until(40, "a slow rate", || row(&d, &id).filter(|r| r["down_rate"].as_u64().unwrap_or(0) > 50_000));
    assert!(slow["down_rate"].as_u64().unwrap() < 700_000, "capped near 300 KB/s: {slow}");
    assert_eq!(sim(root.path(), &d, &["sim", "speed", "sim1", "8MB/s"]).code, 0);
    let fast = wait_until(40, "a faster rate", || row(&d, &id).filter(|r| r["down_rate"].as_u64().unwrap_or(0) > 1_500_000));
    assert!(fast["down_rate"].as_u64().unwrap() > 1_500_000);
    // stop: the swarm goes away and the row says it is waiting
    assert_eq!(sim(root.path(), &d, &["sim", "stop", "sim1"]).code, 0);
    let w = wait_until(60, "waiting for peers", || row(&d, &id).filter(|r| r["down_rate"] == 0 && r["state"] == "downloading"));
    assert_eq!(w["state"], "downloading");
    sim(root.path(), &d, &["sim", "clear"]);
}

/// Developer mode does not open loopback trackers for everybody: a regular torrent whose tracker is on 127.0.0.1 is
/// still refused (nothing connects), while a torrent added by `sim` may announce there.
#[test]
fn only_sim_torrents_may_announce_to_a_loopback_tracker() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("tmp")).unwrap();
    // No --allow-loopback-trackers: the global policy is the app's.
    let d = Daemon::start_full(root.path(), "l", "full", &[], true);
    assert_eq!(d.cli(&["dev", "on"]).code, 0);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
    {
        let hits = hits.clone();
        std::thread::spawn(move || {
            for _s in listener.incoming().map_while(Result::ok) {
                hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        });
    }
    let t = make_torrent_ann("Regular.Torrent.2020", &[("r.bin", 50_000)], 21, Some(&format!("http://127.0.0.1:{port}/announce")));
    let f = t.write_torrent(root.path());
    d.json(&["add", f.to_str().unwrap(), "--paused"]);
    d.json(&["resume", "regular"]);
    let st = wait_until(40, "the regular torrent's tracker to be refused", || {
        let v = d.json(&["debug", "trackers", "regular"]);
        let s = v["transfers"][0]["trackers"][0]["state"].as_str().unwrap_or("").to_owned();
        (s == "backoff" || s == "halted").then_some(v)
    });
    assert_eq!(hits.load(std::sync::atomic::Ordering::Relaxed), 0, "nothing may connect to a loopback tracker: {st}");

    // A sim torrent in the same daemon announces to its mock tracker on 127.0.0.1.
    let o = sim(root.path(), &d, &["sim", "add", "single", "--size", "100KB"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let id = out_json(&o)["torrent_id"].as_str().unwrap().to_owned();
    let w = wait_until(40, "the sim tracker to work", || {
        let v = d.json(&["debug", "trackers", &id[..10]]);
        (v["transfers"][0]["trackers"][0]["state"] == "working").then_some(v)
    });
    assert!(w["transfers"][0]["trackers"][0]["last_success_at"].is_number(), "{w}");
    // Without Developer mode the app refuses to mark anything.
    assert_eq!(d.cli(&["dev", "off"]).code, 0);
    let mut c = std::process::Command::new(BIN);
    c.arg("--socket").arg(&d.socket).args(["--json", "--yes-i-know", "sim", "add", "single", "--size", "50KB"]).env("HOME", root.path().join("home")).env("TMPDIR", root.path().join("tmp")).env("TMP", root.path().join("tmp")).env("TEMP", root.path().join("tmp"));
    let o = c.output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("Developer mode"), "{}", String::from_utf8_lossy(&o.stderr));
    sim(root.path(), &d, &["sim", "clear"]);
}

#[test]
fn sim_host_ends_with_its_daemon() {
    let root = tempfile::tempdir().unwrap();
    let mut d = sim_daemon(root.path(), "o");
    assert_eq!(sim(root.path(), &d, &["sim", "add", "single", "--size", "100KB"]).code, 0);
    let host_sock = std::fs::read_dir(root.path().join("tmp").join("seedwell-sim")).unwrap().next().unwrap().unwrap().path().join("host.sock");
    assert!(trs_platform::ipc::is_served(&host_sock));
    d.terminate();
    wait_until(30, "the sim host to follow the daemon", || (!trs_platform::ipc::is_served(&host_sock)).then_some(()));
}

fn tree_files(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(d: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else {
                out.push(p.display().to_string());
            }
        }
    }
    walk(dir, &mut out);
    out
}
