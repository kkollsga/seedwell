//! The `.part.json`-style sidecar: a tiny line-oriented text file next to the
//! `.part` that makes a download resumable across restarts and crashes.
//!
//! Format: first line `trs-http-part 1`, then `key value` lines (values
//! escape `\\`, `\n`, `\r`). Unknown keys are ignored (forward compatible).
//! It is parsed from disk, i.e. untrusted: size-capped, never panics.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::ranges::RangeSet;

const MAGIC: &str = "trs-http-part 1";
/// Sidecars are a few hundred bytes; refuse anything absurd.
const MAX_SIDECAR_BYTES: u64 = 256 * 1024;

/// Everything needed to resume a download.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sidecar {
    /// Caller's stable id (UUID in the app).
    pub id: String,
    /// URL as requested.
    pub url: String,
    /// URL after redirects (informational; resumes start from `url` again,
    /// because redirect targets are often short-lived signed links).
    pub final_url: String,
    /// Staged file name without `.part`.
    pub name: String,
    /// Strong ETag exactly as the server sent it (with quotes).
    pub etag: Option<String>,
    /// `Last-Modified` exactly as the server sent it.
    pub last_modified: Option<String>,
    /// Total size if the server told us.
    pub total: Option<u64>,
    /// Server honours ranges and we hold a validator, so segments are safe.
    pub segmentable: bool,
    /// `Content-Type` of the answer (lets the app spot `.torrent` payloads).
    pub content_type: Option<String>,
    /// Bytes known to be written to the `.part`.
    pub ranges: RangeSet,
}

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            c => o.push(c),
        }
    }
    o
}

fn unesc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            o.push(c);
            continue;
        }
        match it.next() {
            Some('n') => o.push('\n'),
            Some('r') => o.push('\r'),
            Some('\\') => o.push('\\'),
            Some(x) => {
                o.push('\\');
                o.push(x);
            }
            None => o.push('\\'),
        }
    }
    o
}

impl Sidecar {
    /// Serialize.
    pub fn encode(&self) -> String {
        let mut s = String::new();
        s.push_str(MAGIC);
        s.push('\n');
        let mut kv = |k: &str, v: &str| {
            s.push_str(k);
            s.push(' ');
            s.push_str(&esc(v));
            s.push('\n');
        };
        kv("id", &self.id);
        kv("url", &self.url);
        kv("final_url", &self.final_url);
        kv("name", &self.name);
        if let Some(v) = &self.etag {
            kv("etag", v);
        }
        if let Some(v) = &self.last_modified {
            kv("last_modified", v);
        }
        if let Some(v) = self.total {
            kv("total", &v.to_string());
        }
        kv("segmentable", if self.segmentable { "1" } else { "0" });
        if let Some(v) = &self.content_type {
            kv("content_type", v);
        }
        kv("ranges", &self.ranges.encode());
        s
    }

    /// Parse; `None` if it is not a sidecar we wrote.
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.lines();
        if lines.next()? != MAGIC {
            return None;
        }
        let mut sc = Sidecar::default();
        let mut have_ranges = false;
        for line in lines {
            let (k, v) = line.split_once(' ').unwrap_or((line, ""));
            let v = unesc(v);
            match k {
                "id" => sc.id = v,
                "url" => sc.url = v,
                "final_url" => sc.final_url = v,
                "name" => sc.name = v,
                "etag" => sc.etag = Some(v),
                "last_modified" => sc.last_modified = Some(v),
                "total" => sc.total = Some(v.parse().ok()?),
                "segmentable" => sc.segmentable = v == "1",
                "content_type" => sc.content_type = Some(v),
                "ranges" => {
                    sc.ranges = RangeSet::parse(&v)?;
                    have_ranges = true;
                }
                _ => {}
            }
        }
        if sc.url.is_empty() || sc.name.is_empty() || !have_ranges {
            return None;
        }
        Some(sc)
    }

    /// Does a validator exist that makes resuming safe?
    pub fn has_validator(&self) -> bool {
        self.etag.is_some() || self.last_modified.is_some()
    }

    /// Read from disk (bounded).
    pub fn read(path: &Path) -> Option<Self> {
        let f = File::open(path).ok()?;
        if f.metadata().ok()?.len() > MAX_SIDECAR_BYTES {
            return None;
        }
        let mut s = String::new();
        f.take(MAX_SIDECAR_BYTES).read_to_string(&mut s).ok()?;
        Self::parse(&s)
    }

    /// Crash-safe replace: write a temp file, fsync, rename over.
    pub fn write_atomic(&self, path: &Path) -> io::Result<()> {
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = std::path::PathBuf::from(tmp);
        {
            let mut f = File::create(&tmp)?;
            f.write_all(self.encode().as_bytes())?;
            f.sync_all()?;
        }
        fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Sidecar {
        let mut ranges = RangeSet::new();
        ranges.insert(0, 1000);
        ranges.insert(2000, 3000);
        Sidecar {
            id: "abc".into(),
            url: "https://example.com/a b?x=1".into(),
            final_url: "https://cdn.example.com/f".into(),
            name: "file\nname.iso".into(),
            etag: Some("\"xyz\"".into()),
            last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
            total: Some(5000),
            segmentable: true,
            content_type: None,
            ranges,
        }
    }

    #[test]
    fn roundtrip() {
        let s = sample();
        assert_eq!(Sidecar::parse(&s.encode()), Some(s));
    }

    #[test]
    fn garbage_is_rejected_not_panicking() {
        assert!(Sidecar::parse("").is_none());
        assert!(Sidecar::parse("trs-http-part 1\nurl x\n").is_none());
        assert!(Sidecar::parse("nope\n").is_none());
        assert!(Sidecar::parse("trs-http-part 1\nurl x\nname y\nranges 5-1\n").is_none());
        assert!(Sidecar::parse("trs-http-part 1\nurl x\nname y\nranges 0-1\ntotal zz\n").is_none());
    }
}
