//! Reading the log back: tail with filters, and following new lines.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::FILE_NAME;

/// How much of one file a tail reads at most.
const TAIL_BYTES: u64 = 8 * 1024 * 1024;

/// Which lines to return.
#[derive(Clone, Debug, Default)]
pub struct Query {
    /// A torrent: info-hash prefix, or a word of its title.
    pub torrent: Option<String>,
    /// A tracker: part of its host or masked URL.
    pub tracker: Option<String>,
    /// One event: `started`, `regular`, `completed`, `stopped`, `scrape`.
    pub event: Option<String>,
    /// Only exchanges that failed (and decisions that report a failure).
    pub errors_only: bool,
    /// Any of: id, title, host.
    pub text: Option<String>,
    /// At most this many lines (the newest).
    pub limit: usize,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

/// True when the line reports a failure.
pub fn is_error(v: &Value) -> bool {
    let r = v.get("response");
    r.and_then(|r| r.get("error")).is_some_and(|e| !e.is_null())
        || r.and_then(|r| r.get("status")).and_then(Value::as_u64).is_some_and(|c| c >= 400)
        || matches!(s(v, "decision"), "halted" | "tripped" | "refused")
}

impl Query {
    /// Does `v` pass the filters?
    pub fn matches(&self, v: &Value) -> bool {
        let has = |hay: &str, needle: &str| hay.to_lowercase().contains(&needle.to_lowercase());
        if let Some(t) = self.torrent.as_deref().filter(|t| !t.is_empty()) {
            let (id, title) = (s(v, "torrent"), s(v, "title"));
            if !(id.starts_with(&t.to_lowercase()) || has(title, t)) {
                return false;
            }
        }
        if let Some(t) = self.tracker.as_deref().filter(|t| !t.is_empty())
            && !(has(s(v, "host"), t) || has(s(v, "tracker"), t)) {
                return false;
            }
        if let Some(e) = self.event.as_deref().filter(|e| !e.is_empty())
            && s(v, "event") != e {
                return false;
            }
        if let Some(t) = self.text.as_deref().filter(|t| !t.is_empty())
            && !(has(s(v, "torrent"), t) || has(s(v, "title"), t) || has(s(v, "host"), t)) {
                return false;
            }
        !self.errors_only || is_error(v)
    }
}

/// The log files, newest first.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![dir.join(FILE_NAME)];
    out.push(dir.join(format!("{FILE_NAME}.1")));
    out.push(dir.join(format!("{FILE_NAME}.2")));
    out.retain(|p| p.exists());
    out
}

/// Complete lines of the last `max` bytes of a file, oldest first.
fn tail_lines(path: &Path, max: u64) -> Vec<String> {
    let Ok(mut f) = File::open(path) else { return Vec::new() };
    let len = f.metadata().map_or(0, |m| m.len());
    let start = len.saturating_sub(max);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.take(max).read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    if start > 0 && !lines.is_empty() {
        lines.remove(0); // a partial first line
    }
    lines
}

/// The newest lines that pass `q`, oldest first. Reads the files from the newest backwards and
/// stops as soon as `q.limit` lines were found.
pub fn tail(dir: &Path, q: &Query) -> Vec<String> {
    let limit = if q.limit == 0 { 200 } else { q.limit };
    let mut out: Vec<String> = Vec::new();
    for p in files(dir) {
        for line in tail_lines(&p, TAIL_BYTES).into_iter().rev() {
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && q.matches(&v) {
                    out.push(line);
                    if out.len() >= limit {
                        out.reverse();
                        return out;
                    }
                }
        }
    }
    out.reverse();
    out
}

/// Follows the log as it grows (the CLI's `--follow`). Survives rotation.
pub struct Follower {
    dir: PathBuf,
    pos: u64,
    pending: String,
}

impl Follower {
    /// Start at the end of the current file.
    pub fn new(dir: &Path) -> Follower {
        let pos = std::fs::metadata(dir.join(FILE_NAME)).map_or(0, |m| m.len());
        Follower { dir: dir.to_path_buf(), pos, pending: String::new() }
    }

    fn read_from(path: &Path, pos: u64) -> (String, u64) {
        let Ok(mut f) = File::open(path) else { return (String::new(), pos) };
        if f.seek(SeekFrom::Start(pos)).is_err() {
            return (String::new(), pos);
        }
        let mut b = Vec::new();
        let _ = f.read_to_end(&mut b);
        (String::from_utf8_lossy(&b).into_owned(), pos + b.len() as u64)
    }

    /// New complete lines since the last call.
    pub fn poll(&mut self) -> Vec<String> {
        let cur = self.dir.join(FILE_NAME);
        let len = std::fs::metadata(&cur).map_or(0, |m| m.len());
        let mut text = String::new();
        if len < self.pos {
            // Rotated (or cleared): what was left in the old file is now `.1`.
            let (rest, _) = Self::read_from(&self.dir.join(format!("{FILE_NAME}.1")), self.pos);
            text.push_str(&rest);
            self.pos = 0;
        }
        let (more, np) = Self::read_from(&cur, self.pos);
        text.push_str(&more);
        self.pos = np;
        self.pending.push_str(&text);
        let mut out = Vec::new();
        while let Some(i) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=i).collect();
            let line = line.trim_end().to_owned();
            if !line.is_empty() {
                out.push(line);
            }
        }
        out
    }
}
