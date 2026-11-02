//! Files in chat through the CLI, between two daemons on loopback (SPEC §7G.7 *Files*): the sender stops sharing
//! one (`chat stop-sharing`, `chat dm stop-sharing`) and the receiver's download of it ends as gone; the receiver
//! cancels a download (`chat attachment --cancel`), which deletes its partial file.

use crate::common;

use std::io::Write as _;
use std::process::{Command, Stdio};

use common::*;
use serde_json::Value;

/// `seedwell --json <args>` against `d` with `input` on standard input.
fn cli_stdin(d: &Daemon, args: &[&str], input: &str) -> Out {
    let mut child = Command::new(BIN)
        .arg("--socket")
        .arg(&d.socket)
        .arg("--json")
        .args(args)
        .env_remove("SEEDWELL_SOCKET")
        .env_remove("SEEDWELL_AGENT")
        .current_dir(d.state.parent().unwrap())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let o = child.wait_with_output().unwrap();
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

/// The attachment of `d`'s channel `Files` named `name`, once its message is there.
fn file_at(d: &Daemon, name: &str) -> Value {
    wait_until(30, name, || {
        let r = d.json(&["chat", "read", "Files"]);
        r["messages"].as_array()?.iter().flat_map(|m| m["attachments"].as_array().cloned().unwrap_or_default()).find(|a| a["name"] == name)
    })
}

#[test]
fn the_sender_stops_sharing_a_file_and_the_receiver_cancels_a_download() {
    let root = tempfile::tempdir().unwrap();
    // Below control, refused before anything is looked up: the error names the request that asks for control.
    let ro = Daemon::start(root.path(), "ro", "read_only");
    let o = ro.cli(&["chat", "stop-sharing", "Files", "0a1b2c3d"]);
    assert!(o.code == 4 && o.stderr.contains("seedwell agents request control"), "{}: {}", o.code, o.stderr);
    drop(ro);

    let a = Daemon::start(root.path(), "a", "control");
    let mut b = Daemon::start_with(root.path(), "b", "control", &["--chat-agents", "post"]);
    a.json(&["chat", "new", "Files"]);
    let link = a.json(&["chat", "share", "Files"])["link"].as_str().unwrap().to_owned();
    b.json(&["chat", "join", &link]);
    let port = a.json(&["sync", "list"])["port"].as_u64().unwrap();
    b.json(&["chat", "address", "Files", &format!("127.0.0.1:{port}")]);
    wait_until(30, "B admitted", || b.json(&["chat", "list"])["channels"][0]["role"].as_str().map(str::to_owned));

    // B shares two files.
    let (one, two) = (root.path().join("one.bin"), root.path().join("two.bin"));
    std::fs::write(&one, vec![1u8; 300_000]).unwrap();
    std::fs::write(&two, vec![2u8; 300_000]).unwrap();
    let sent = b.json(&["chat", "send", "Files", "--attach", one.to_str().unwrap(), "--attach", two.to_str().unwrap(), "the builds"]);
    assert_eq!(sent["attachments"][0]["state"].as_str(), Some("shared"), "{sent}");
    let h1 = file_at(&a, "one.bin")["hash"].as_str().unwrap().to_owned();
    let h2 = file_at(&a, "two.bin")["hash"].as_str().unwrap().to_owned();

    // B stops sharing the first, by the hash's first 8 characters: its card is unshared, the file stays.
    let o = cli_text(&b.socket, &["chat", "stop-sharing", "Files", &h1[..8]]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    assert!(o.stdout.starts_with("[file: one.bin, 300 KB, no longer shared: you stopped sharing it] Devices that ask for it"), "{}", o.stdout);
    let card = b.json(&["chat", "attachment", "Files", &h1[..12]]);
    assert_eq!(card["state"].as_str(), Some("unshared"), "{card}");
    assert!(one.exists());
    // Not a file sent from B: refused.
    let o = a.cli(&["chat", "stop-sharing", "Files", &h1[..8]]);
    assert!(o.code == 1 && o.stderr.contains("that file is not shared from this"), "{}: {}", o.code, o.stderr);

    // A's download of it ends as gone, on its card too.
    let to = root.path().join("got");
    std::fs::create_dir_all(&to).unwrap();
    let o = a.cli(&["chat", "attachment", "Files", &h1[..8], "--out", to.to_str().unwrap()]);
    assert!(o.code == 1 && o.stderr.contains("no longer available"), "{}: {}", o.code, o.stderr);
    assert_eq!(file_at(&a, "one.bin")["state"].as_str(), Some("gone"));
    assert!(!to.join("one.bin").exists() && !to.join("one.bin.seedwell-part").exists());

    // A private conversation: B sends a file to A and stops sharing it there.
    let a_dev = a.json(&["sync", "list"])["device_id"].as_str().unwrap().to_owned();
    let three = root.path().join("three.bin");
    std::fs::write(&three, vec![3u8; 1000]).unwrap();
    let pm = wait_until(30, "A's key at B", || {
        let o = cli_stdin(&b, &["chat", "dm", "send", &a_dev[..7], "--attach", three.to_str().unwrap(), "-"], "just for you");
        (o.code == 0).then(|| serde_json::from_str::<Value>(&o.stdout).unwrap())
    });
    let h3 = pm["attachments"][0]["hash"].as_str().unwrap().to_owned();
    let stopped = b.json(&["chat", "dm", "stop-sharing", &a_dev[..7], &h3[..8]]);
    assert_eq!((stopped["state"].as_str(), stopped["problem"].as_str()), (Some("unshared"), Some("you stopped sharing it")), "{stopped}");

    // With B away, A's download of the second file waits, a partial file beside its place; cancelling deletes it.
    b.terminate();
    let o = a.cli(&["chat", "attachment", "Files", &h2[..8], "--out", to.to_str().unwrap(), "--no-wait"]);
    assert_eq!(o.code, 0, "{}", o.stderr);
    let part = to.join("two.bin.seedwell-part");
    assert!(part.exists(), "{:?}", std::fs::read_dir(&to).unwrap().collect::<Vec<_>>());
    let state = file_at(&a, "two.bin")["state"].as_str().unwrap().to_owned();
    assert!(matches!(state.as_str(), "downloading" | "paused"), "{state}");
    let c = a.json(&["chat", "attachment", "Files", &h2[..8], "--cancel"]);
    assert_eq!((c["state"].as_str(), c["cancelled"].as_bool()), (Some("available"), Some(true)), "{c}");
    assert!(!part.exists() && !to.join("two.bin").exists(), "the partial file is gone");
    assert_eq!(file_at(&a, "two.bin")["state"].as_str(), Some("available"));
    // Again: nothing to cancel, and it says so.
    let o = cli_text(&a.socket, &["chat", "attachment", "Files", &h2[..8], "--cancel"]);
    assert!(o.code == 0 && o.stdout.contains("[file: two.bin, 300 KB] It is not being downloaded: nothing to cancel."), "{}{}", o.stdout, o.stderr);
}
