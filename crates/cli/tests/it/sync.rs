//! Folder sync through the daemon and the CLI, and `seedwell relay` (temp dirs and loopback only).

use crate::common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;

#[test]
fn folder_sync_runs_in_the_daemon() {
    let root = tempfile::tempdir().unwrap();
    let d = Daemon::start(root.path(), "s", "full");
    let l = d.json(&["sync", "list"]);
    assert!(l["port"].as_u64().unwrap() > 0, "{l}");
    assert_eq!(l["folders"].as_array().unwrap().len(), 0);
    assert_eq!(l["v"], 1);

    let docs = root.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"hello").unwrap();
    let f = d.json(&["sync", "add", docs.to_str().unwrap()]);
    assert_eq!((f["name"].as_str(), f["access"].as_str()), (Some("Docs"), Some("read_write")), "{f}");
    let group = f["id"].as_str().unwrap().split('-').next().unwrap().to_owned();
    assert_eq!(d.json(&["sync", "status", &group])["name"], "Docs");
    assert_eq!(d.json(&["sync", "pause", "docs"])["state"], "paused");
    let sh = d.json(&["sync", "share", "Docs"]);
    assert!(sh["read_write"].as_str().unwrap().starts_with("SWW-"), "{sh}");
    let text = cli_text(&d.socket, &["sync", "list"]);
    assert_eq!(text.code, 0, "{}", text.stderr);
    assert!(text.stdout.contains("Docs") && text.stdout.contains(&group), "{}", text.stdout);
    // The keys are in a file only the user can read, never in sync.toml.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(d.state.join("sync").join("secrets")).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let toml = std::fs::read_to_string(d.state.join("sync.toml")).unwrap();
    assert!(!toml.contains(sh["read_write"].as_str().unwrap()) && !toml.contains("SWW-"), "{toml}");
    // A script gets the plan and the token; -y removes (the files stay).
    let plan = d.json(&["sync", "remove", "Docs"]);
    assert!(plan["token"].as_str().is_some_and(|t| t.len() == 32), "{plan}");
    let r = d.json(&["sync", "remove", "Docs", "-y"]);
    assert_eq!(r["files"], "kept", "{r}");
    assert!(docs.join("a.txt").exists());
    assert_eq!(d.json(&["sync", "list"])["folders"].as_array().unwrap().len(), 0);
    let o = d.cli(&["sync", "status", "Docs"]);
    assert_eq!(o.code, 3, "not found is exit 3: {}", o.stderr);

    // A daemon without sync says so.
    let n = Daemon::start_with(root.path(), "n", "full", &["--no-sync"]);
    let o = n.cli(&["sync", "list"]);
    assert_eq!(o.code, 1);
    assert!(o.stderr.contains("not running"), "{}", o.stderr);
    // Nothing landed in the (temporary) home folder.
    assert!(std::fs::read_dir(root.path().join("home")).unwrap().next().is_none());
}

/// Starts `seedwell relay` and returns it with its ready line.
fn relay(key: &std::path::Path) -> (std::process::Child, String) {
    let mut child = Command::new(BIN)
        .args(["relay", "--port", "0", "--bind", "127.0.0.1", "--key-file"])
        .arg(key)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(out).read_line(&mut line);
        let _ = tx.send(line);
    });
    let line = rx.recv_timeout(Duration::from_secs(30)).expect("relay did not become ready");
    (child, line)
}

#[test]
fn the_relay_starts_and_keeps_its_id() {
    let root = tempfile::tempdir().unwrap();
    let key = root.path().join("relay.key");
    let (mut a, line) = relay(&key);
    assert!(line.starts_with("seedwell relay ready address=relay://127.0.0.1:"), "{line:?}");
    let id = line.split('#').nth(1).unwrap().split_whitespace().next().unwrap().to_owned();
    let _ = Command::new("kill").arg("-TERM").arg(a.id().to_string()).status();
    let status = a.wait().unwrap();
    assert!(status.success(), "a signal stops it cleanly: {status:?}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
    let (mut b, line) = relay(&key);
    assert!(line.contains(&format!("#{id}")), "the same key, the same address: {line:?}");
    let _ = b.kill();
    let _ = b.wait();
}
