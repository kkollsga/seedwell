//! A log line for people: one summary line and a few detail lines.

use serde_json::Value;

use crate::read::is_error;

/// A parsed line, ready for a list.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Unix milliseconds.
    pub ts_ms: u64,
    /// `announce`, `scrape`, `decision`, `gate`, `quit`.
    pub kind: String,
    /// Info-hash (40 hex).
    pub torrent: String,
    /// First 8 hex digits.
    pub id: String,
    /// Display title, or empty.
    pub title: String,
    /// Tracker URL with secrets masked.
    pub tracker: String,
    /// Tracker host.
    pub host: String,
    /// `HTTP`, `HTTPS`, `UDP`, or empty.
    pub transport: String,
    /// The event, or empty.
    pub event: String,
    /// The exchange failed.
    pub error: bool,
    /// One line: what happened.
    pub summary: String,
    /// The full text: request, response, schedule.
    pub detail: String,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(Value::as_str).unwrap_or("")
}

fn dur(secs: i64) -> String {
    let a = secs.unsigned_abs();
    let t = if a < 90 {
        format!("{a} s")
    } else if a < 5400 {
        format!("{} min", (a + 30) / 60)
    } else {
        format!("{:.1} h", a as f64 / 3600.0)
    };
    if secs < 0 { format!("{t} ago") } else { t }
}

/// Local wall-clock `HH:MM:SS`, matching the times the app's list shows (the file itself keeps UTC).
#[allow(unsafe_code)]
fn hms(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    // SAFETY: `localtime_r` writes only into the `tm` we own; a zeroed `tm` is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&secs, &mut tm) }.is_null() {
        let iso = crate::iso_time(ms);
        return iso.get(11..19).unwrap_or("").to_string();
    }
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

fn fields(v: &Value) -> Vec<(String, String)> {
    v.as_object()
        .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned))).collect())
        .unwrap_or_default()
}

fn field<'a>(f: &'a [(String, String)], k: &str) -> Option<&'a str> {
    f.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
}

impl Row {
    /// Parse one JSON line; `None` when it is not one of ours.
    pub fn parse(line: &str) -> Option<Row> {
        let v: Value = serde_json::from_str(line).ok()?;
        v.get("ts_ms")?;
        Some(Row::from_value(&v))
    }

    /// Build a row from a parsed line.
    pub fn from_value(v: &Value) -> Row {
        let ts_ms = v.get("ts_ms").and_then(Value::as_u64).unwrap_or(0);
        let transport = s(v, "transport").to_uppercase();
        let resp = v.get("response");
        let status = resp.and_then(|r| r.get("status")).and_then(Value::as_u64);
        let rf = resp.and_then(|r| r.get("fields")).map(fields).unwrap_or_default();
        let err = resp.and_then(|r| r.get("error")).and_then(Value::as_str);
        let ms = v.get("duration_ms").and_then(Value::as_u64);
        let next = v.get("next");
        let kind = s(v, "kind");

        let mut parts: Vec<String> = Vec::new();
        match kind {
            "announce" | "scrape" => {
                if let Some(e) = err {
                    parts.push(e.to_string());
                } else if let Some(c) = status {
                    parts.push(c.to_string());
                }
                if err.is_none() {
                    if let (Some(c), Some(i)) = (field(&rf, "complete"), field(&rf, "incomplete")) {
                        parts.push(format!("{c} seeders, {i} leechers"));
                    }
                    if let Some(p) = field(&rf, "peers") {
                        parts.push(format!("{p} peers"));
                    }
                }
                if let Some(w) = field(&rf, "warning") {
                    parts.push(format!("warning: {w}"));
                }
                if let Some(ms) = ms {
                    parts.push(format!("{ms} ms"));
                }
                if let Some(n) = next {
                    let in_s = n.get("in_s").and_then(Value::as_i64).unwrap_or(0);
                    parts.push(format!("next in {}", dur(in_s)));
                }
            }
            _ => {
                if let Some(d) = v.get("decision").and_then(Value::as_str) {
                    parts.push(d.to_string());
                }
                if let Some(n) = v.get("note").and_then(Value::as_str) {
                    parts.push(n.to_string());
                }
            }
        }
        let head = match (kind, s(v, "event")) {
            ("announce" | "scrape", "") => String::new(),
            ("announce" | "scrape", e) => format!("{e}  "),
            (k, _) => format!("{k}  "),
        };
        let summary = format!("{head}{}", parts.join("  ·  "));

        let mut d = String::new();
        d.push_str(&format!(
            "{}  {}  {}{}\n",
            hms(ts_ms),
            s(v, "id"),
            if s(v, "title").is_empty() { String::new() } else { format!("{}  ", s(v, "title")) },
            s(v, "tracker")
        ));
        if let Some(r) = v.get("request") {
            let url = s(r, "url");
            if !url.is_empty() {
                let verb = if transport == "UDP" { "UDP" } else { "GET" };
                d.push_str(&format!("{verb} {url}\n"));
            }
            for h in r.get("headers").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                d.push_str(&format!("  {h}\n"));
            }
            if transport == "UDP" {
                for (k, val) in fields(r.get("fields").unwrap_or(&Value::Null)) {
                    d.push_str(&format!("  {k} = {val}\n"));
                }
            }
        }
        if resp.is_some() {
            let mut line = String::from("->");
            if let Some(c) = status {
                line.push_str(&format!(" {c}"));
            }
            if let Some(e) = err {
                line.push_str(&format!(" {e}"));
            }
            if let Some(ms) = ms {
                line.push_str(&format!(" in {ms} ms"));
            }
            if let Some(f) = v.get("family").and_then(Value::as_str) {
                line.push_str(&format!(" via {f} {}", s(v, "ip")));
            }
            if v.get("fallback").and_then(Value::as_bool) == Some(true) {
                line.push_str(" (other address family tried)");
            }
            d.push_str(&line);
            d.push('\n');
            for (k, val) in rf {
                d.push_str(&format!("  {} = {val}\n", k.replace('_', " ")));
            }
        }
        if let Some(n) = next {
            let at = n.get("at_ms").and_then(Value::as_u64).unwrap_or(0);
            let in_s = n.get("in_s").and_then(Value::as_i64).unwrap_or(0);
            d.push_str(&format!("next: in {} ({}) - {}\n", dur(in_s), hms(at), s(n, "why")));
        }
        if let Some(dec) = v.get("decision").and_then(Value::as_str) {
            d.push_str(&format!("{dec}: {}\n", s(v, "note")));
        } else if let Some(n) = v.get("note").and_then(Value::as_str) {
            d.push_str(&format!("{n}\n"));
        }

        Row {
            ts_ms,
            kind: kind.to_string(),
            torrent: s(v, "torrent").to_string(),
            id: s(v, "id").to_string(),
            title: s(v, "title").to_string(),
            tracker: s(v, "tracker").to_string(),
            host: s(v, "host").to_string(),
            transport,
            event: s(v, "event").to_string(),
            error: is_error(v),
            summary,
            detail: d.trim_end().to_string(),
        }
    }
}

/// The plain-text rendering of a JSON line (the CLI's default output).
pub fn render(line: &str) -> String {
    Row::parse(line).map_or_else(|| line.to_string(), |r| r.detail)
}
