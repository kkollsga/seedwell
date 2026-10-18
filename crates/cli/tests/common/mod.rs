//! Process-level test harness: real `seedwell daemon` processes and the CLI.
#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::Value;

pub const BIN: &str = env!("CARGO_BIN_EXE_seedwell");

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub struct Torrent {
    pub bytes: Vec<u8>,
    pub id: String,
    pub name: String,
    pub files: Vec<(String, Vec<u8>)>,
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    out.extend(format!("{}:", b.len()).bytes());
    out.extend(b);
}

/// A multi-file torrent of pseudo-random data (`files`: relative path, length).
pub fn make_torrent(name: &str, files: &[(&str, usize)], seed: u64) -> Torrent {
    make_torrent_ann(name, files, seed, None)
}

/// Like [`make_torrent`], with an announce URL.
pub fn make_torrent_ann(name: &str, files: &[(&str, usize)], seed: u64, announce: Option<&str>) -> Torrent {
    let mut x = seed | 1;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let piece_len = 32 * 1024usize;
    let mut stream = Vec::new();
    let mut data = Vec::new();
    for (p, len) in files {
        let d: Vec<u8> = (0..*len).map(|_| next() as u8).collect();
        stream.extend_from_slice(&d);
        data.push(((*p).to_owned(), d));
    }
    let mut pieces = Vec::new();
    for c in stream.chunks(piece_len) {
        pieces.extend_from_slice(&trs_storage::hash::sha1(c));
    }
    let mut info = b"d5:filesl".to_vec();
    for (p, d) in &data {
        info.extend(format!("d6:lengthi{}e4:pathl", d.len()).bytes());
        for c in p.split('/') {
            put_bytes(&mut info, c.as_bytes());
        }
        info.extend(b"ee");
    }
    info.extend(b"e4:name");
    put_bytes(&mut info, name.as_bytes());
    info.extend(format!("12:piece lengthi{piece_len}e6:pieces").bytes());
    put_bytes(&mut info, &pieces);
    info.push(b'e');
    let id = hex(&trs_storage::hash::sha1(&info));
    let mut t = Vec::new();
    if let Some(a) = announce {
        t.extend(format!("d8:announce{}:{a}4:info", a.len()).bytes());
    } else {
        t.extend(b"d4:info");
    }
    t.extend(&info);
    t.push(b'e');
    Torrent { bytes: t, id, name: name.to_owned(), files: data }
}

impl Torrent {
    pub fn write_payload(&self, dir: &Path) {
        for (p, d) in &self.files {
            let path = dir.join(&self.name).join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, d).unwrap();
        }
    }

    pub fn verify(&self, dir: &Path) {
        for (p, d) in &self.files {
            let path = dir.join(&self.name).join(p);
            let got = std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
            assert!(got == *d, "{path:?} differs");
        }
    }

    pub fn write_torrent(&self, dir: &Path) -> PathBuf {
        let p = dir.join(format!("{}.torrent", self.name));
        std::fs::write(&p, &self.bytes).unwrap();
        p
    }
}

pub struct Daemon {
    pub child: Child,
    pub socket: PathBuf,
    pub port: u16,
    pub state: PathBuf,
    pub dl: PathBuf,
}

impl Daemon {
    pub fn start(root: &Path, name: &str, permission: &str) -> Daemon {
        Self::start_with(root, name, permission, &[])
    }

    pub fn start_with(root: &Path, name: &str, permission: &str, extra: &[&str]) -> Daemon {
        Self::start_full(root, name, permission, extra, false)
    }

    /// `trackers`: let the daemon talk to trackers (the default of the tests is `--no-trackers`).
    pub fn start_full(root: &Path, name: &str, permission: &str, extra: &[&str], trackers: bool) -> Daemon {
        std::fs::create_dir_all(root.join("home")).unwrap();
        let state = root.join(name).join("state");
        let dl = root.join(name).join("dl");
        std::fs::create_dir_all(&state).unwrap();
        let mut cmd = Command::new(BIN);
        cmd.args(["daemon", "--state-dir"])
            .arg(&state)
            .arg("--download-dir")
            .arg(&dl)
            .args(["--bind", "127.0.0.1", "--no-dht", "--permission", permission])
            .args(if trackers { &[][..] } else { &["--no-trackers"][..] })
            .args(extra)
            .env_remove("SEEDWELL_SOCKET")
            // A stray `~` can only ever expand into the test directory, never the real home.
            .env("HOME", root.join("home"))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().expect("spawn seedwell daemon");
        let out = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            let mut line = String::new();
            let _ = r.read_line(&mut line);
            let _ = tx.send(line);
            // Keep draining so the daemon never blocks on a full pipe.
            let mut sink = String::new();
            while r.read_line(&mut sink).unwrap_or(0) > 0 {
                sink.clear();
            }
        });
        let line = rx.recv_timeout(Duration::from_secs(60)).expect("daemon did not become ready");
        assert!(line.starts_with("seedwell daemon ready"), "unexpected: {line:?}");
        let field = |k: &str| line.split_whitespace().find_map(|w| w.strip_prefix(k)).unwrap().to_owned();
        Daemon { child, socket: PathBuf::from(field("socket=")), port: field("port=").parse().unwrap(), state, dl }
    }

    /// Run the CLI against this daemon with `--json`.
    pub fn cli(&self, args: &[&str]) -> Out {
        cli_at(&self.socket, args)
    }

    pub fn json(&self, args: &[&str]) -> Value {
        let o = self.cli(args);
        assert_eq!(o.code, 0, "seedwell {args:?} failed: {}", o.stderr);
        serde_json::from_str(&o.stdout).unwrap_or_else(|e| panic!("seedwell {args:?}: not JSON ({e}): {}", o.stdout))
    }

    pub fn terminate(&mut self) {
        let _ = Command::new("kill").arg("-TERM").arg(self.child.id().to_string()).status();
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(20) {
            if self.child.try_wait().ok().flatten().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.terminate();
    }
}

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

pub fn cli_at(socket: &Path, args: &[&str]) -> Out {
    let o = Command::new(BIN)
        .arg("--socket")
        .arg(socket)
        .arg("--json")
        .args(args)
        .env_remove("SEEDWELL_SOCKET")
        .output()
        .expect("run seedwell");
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

/// The CLI with its own `HOME` and `TMPDIR` inside `root` (sims write below the temp directory).
pub fn cli_in(root: &Path, socket: &Path, args: &[&str]) -> Out {
    let o = Command::new(BIN)
        .arg("--socket")
        .arg(socket)
        .args(args)
        .env_remove("SEEDWELL_SOCKET")
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .output()
        .expect("run seedwell");
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

pub fn cli_text(socket: &Path, args: &[&str]) -> Out {
    let o = Command::new(BIN).arg("--socket").arg(socket).args(args).env_remove("SEEDWELL_SOCKET").output().expect("run seedwell");
    Out { code: o.status.code().unwrap_or(-1), stdout: String::from_utf8_lossy(&o.stdout).into_owned(), stderr: String::from_utf8_lossy(&o.stderr).into_owned() }
}

/// Poll `f` until it returns Some, or panic after `secs`.
pub fn wait_until<T>(secs: u64, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let t = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The row of `id` in `list`, if present.
pub fn row(d: &Daemon, id: &str) -> Option<Value> {
    let l = d.json(&["list", "--limit", "500"]);
    l["transfers"].as_array()?.iter().find(|t| t["id"] == id).cloned()
}
