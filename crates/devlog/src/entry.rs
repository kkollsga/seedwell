//! One log entry and its masked JSON rendering.

use std::fmt::Write as _;

use trs_state::url::{MASK, mask_secrets_for_log, mask_text, mask_urls_in_text};

/// What kind of line this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Kind {
    /// One announce request and its answer (or error).
    #[default]
    Announce,
    /// One scrape request and its answer.
    Scrape,
    /// A decision that sent nothing (retire, suspend, deferred, skipped, halted).
    Decision,
    /// The rate gate: a request deferred, a host paused, resumed or tripped.
    Gate,
    /// Quit handling.
    Quit,
}

impl Kind {
    /// The word written to the file.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Announce => "announce",
            Kind::Scrape => "scrape",
            Kind::Decision => "decision",
            Kind::Gate => "gate",
            Kind::Quit => "quit",
        }
    }
}

/// The request as sent.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// HTTP: the whole URL with every parameter. UDP: `udp://host:port/announce`.
    pub url: String,
    /// HTTP request headers, `Name: value`.
    pub headers: Vec<String>,
    /// Decoded fields: HTTP parameters, or the UDP connect / announce packet fields.
    pub fields: Vec<(String, String)>,
}

/// The answer, or what went wrong.
#[derive(Clone, Debug, Default)]
pub struct Response {
    /// HTTP status.
    pub status: Option<u16>,
    /// Decoded fields (interval, min interval, complete, incomplete, peers, warning, failure reason, tracker id).
    pub fields: Vec<(String, String)>,
    /// timeout, DNS, TLS, connection reset, tracker failure ...
    pub error: Option<String>,
}

/// When the next announce is and why.
#[derive(Clone, Debug, Default)]
pub struct Next {
    /// Unix milliseconds.
    pub at_ms: u64,
    /// interval, min-interval floor, backoff step N, the gate, a failover ...
    pub why: String,
}

/// Everything about one logged event. Strings may hold secrets (the raw tracker URL): they are
/// masked by [`Entry::to_line`], which is the only way to turn an entry into text.
#[derive(Clone, Debug, Default)]
pub struct Entry {
    /// Unix milliseconds.
    pub ts_ms: u64,
    /// Kind of line.
    pub kind: Kind,
    /// Info-hash, 40 hex digits.
    pub torrent: String,
    /// Display title, when known.
    pub title: Option<String>,
    /// Tracker URL as announced (raw, may hold the passkey).
    pub tracker: String,
    /// `http`, `https`, `udp` or empty.
    pub transport: &'static str,
    /// `started`, `regular`, `completed`, `stopped`, `scrape` or empty.
    pub event: &'static str,
    /// The request.
    pub request: Option<Request>,
    /// The answer.
    pub response: Option<Response>,
    /// Round trip.
    pub duration_ms: Option<u64>,
    /// `IPv4` or `IPv6`.
    pub family: Option<&'static str>,
    /// Address that was used.
    pub ip: Option<String>,
    /// The other address family was tried after a failure (dual-stack fallback).
    pub fallback: bool,
    /// The resulting schedule.
    pub next: Option<Next>,
    /// Short code of a decision (`retire`, `suspend`, `deferred`, `skipped`, ...).
    pub decision: Option<&'static str>,
    /// Free text.
    pub note: Option<String>,
}

fn esc(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parameter and field names whose value is a credential or identifies the client across IPs.
const SECRET_FIELDS: &[&str] = &[
    "key", "passkey", "pass_key", "pid", "authkey", "auth_key", "torrent_pass", "torrentpass", "token", "secret", "apikey",
    "api_key", "auth", "uid", "rsskey", "sign", "signature", "k",
];

/// Our own identifiers: an info-hash (40 hex digits, or 20 percent-encoded bytes) is not a secret.
fn is_info_hash(k: &str, v: &str) -> bool {
    (k == "info_hash" || k.ends_with(".info_hash")) && v.len() == 40 && v.bytes().all(|b| b.is_ascii_hexdigit())
}

fn field_value(k: &str, v: &str) -> String {
    if is_info_hash(k, v) {
        v.to_string()
    } else if SECRET_FIELDS.iter().any(|s| k.rsplit('.').next().unwrap_or(k).eq_ignore_ascii_case(s)) {
        MASK.to_string()
    } else {
        mask_text(v)
    }
}

fn fields_obj(out: &mut String, fields: &[(String, String)]) {
    out.push('{');
    for (i, (k, v)) in fields.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        esc(out, k);
        out.push(':');
        esc(out, &field_value(k, v));
    }
    out.push('}');
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC) for Unix milliseconds.
pub fn iso_time(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // civil from days (Howard Hinnant)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem % 3600 / 60, rem % 60, ms % 1000)
}

impl Entry {
    /// The masked JSON line (no trailing newline). Every string that can hold a secret goes
    /// through the masking functions of `trs_state::url` here.
    pub fn to_line(&self) -> String {
        let tracker = if self.tracker.contains("://") { mask_secrets_for_log(&self.tracker) } else { mask_text(&self.tracker) };
        let host = host_of(&tracker);
        let mut o = String::with_capacity(512);
        let _ = write!(o, "{{\"v\":1,\"ts_ms\":{},\"ts\":\"{}\",\"kind\":\"{}\"", self.ts_ms, iso_time(self.ts_ms), self.kind.as_str());
        o.push_str(",\"torrent\":");
        esc(&mut o, &self.torrent);
        o.push_str(",\"id\":");
        esc(&mut o, self.torrent.get(..8).unwrap_or(&self.torrent));
        if let Some(t) = &self.title {
            o.push_str(",\"title\":");
            esc(&mut o, &mask_urls_in_text(t));
        }
        o.push_str(",\"tracker\":");
        esc(&mut o, &tracker);
        o.push_str(",\"host\":");
        esc(&mut o, &host);
        if !self.transport.is_empty() {
            let _ = write!(o, ",\"transport\":\"{}\"", self.transport);
        }
        if !self.event.is_empty() {
            let _ = write!(o, ",\"event\":\"{}\"", self.event);
        }
        if let Some(r) = &self.request {
            o.push_str(",\"request\":{\"url\":");
            esc(&mut o, &mask_secrets_for_log(&r.url));
            o.push_str(",\"headers\":[");
            for (i, h) in r.headers.iter().enumerate() {
                if i > 0 {
                    o.push(',');
                }
                esc(&mut o, &mask_text(h));
            }
            o.push_str("],\"fields\":");
            fields_obj(&mut o, &r.fields);
            o.push('}');
        }
        if let Some(r) = &self.response {
            o.push_str(",\"response\":{");
            if let Some(s) = r.status {
                let _ = write!(o, "\"status\":{s},");
            }
            o.push_str("\"fields\":");
            fields_obj(&mut o, &r.fields);
            if let Some(e) = &r.error {
                o.push_str(",\"error\":");
                esc(&mut o, &mask_text(e));
            }
            o.push('}');
        }
        if let Some(d) = self.duration_ms {
            let _ = write!(o, ",\"duration_ms\":{d}");
        }
        if let Some(f) = self.family {
            let _ = write!(o, ",\"family\":\"{f}\"");
        }
        if let Some(ip) = &self.ip {
            o.push_str(",\"ip\":");
            esc(&mut o, ip);
        }
        if self.fallback {
            o.push_str(",\"fallback\":true");
        }
        if let Some(n) = &self.next {
            let in_s = (n.at_ms as i64 - self.ts_ms as i64) / 1000;
            let _ = write!(o, ",\"next\":{{\"at_ms\":{},\"in_s\":{in_s},\"why\":", n.at_ms);
            esc(&mut o, &mask_text(&n.why));
            o.push('}');
        }
        if let Some(d) = self.decision {
            let _ = write!(o, ",\"decision\":\"{d}\"");
        }
        if let Some(n) = &self.note {
            o.push_str(",\"note\":");
            esc(&mut o, &mask_text(n));
        }
        o.push('}');
        o
    }
}

fn host_of(masked_url: &str) -> String {
    let rest = masked_url.split_once("://").map_or(masked_url, |(_, r)| r);
    let auth = rest.split(['/', '?']).next().unwrap_or("");
    auth.rsplit_once('@').map_or(auth, |(_, h)| h).to_string()
}
