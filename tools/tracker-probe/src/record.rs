//! The JSONL capture record shared by `proxy`, `mock`, `analyze` and `diff`.

use crate::bencode::{self, Value};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// One query parameter, in original position.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Param {
    /// Key exactly as sent (not decoded).
    pub k: String,
    /// Value exactly as sent (still percent-encoded).
    pub v: String,
    /// Decoded form: hex for `info_hash`/`peer_id`, lossy UTF-8 otherwise.
    pub dec: String,
}

/// Decoded summary of a tracker response body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RespSummary {
    /// `interval` seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval: Option<i64>,
    /// `min interval` seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_interval: Option<i64>,
    /// Seeders (`complete`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complete: Option<i64>,
    /// Leechers (`incomplete`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub incomplete: Option<i64>,
    /// Number of IPv4 peers returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers: Option<usize>,
    /// Number of IPv6 peers returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peers6: Option<usize>,
    /// `tracker id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker_id: Option<String>,
    /// `failure reason`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// `warning message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning_message: Option<String>,
    /// Any other top-level keys, as `key=short-value`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other: Vec<String>,
    /// Set when the body was not valid bencode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decode_error: Option<String>,
}

/// One logged request/response (or a run marker).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Record {
    /// `announce`, `scrape`, `other` or `marker`.
    pub kind: String,
    /// Unix time in milliseconds when the request was received.
    pub ts_ms: u64,
    /// Operator-supplied client label (e.g. `transmission-4.1.3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Client socket address.
    #[serde(default)]
    pub remote: String,
    /// Server-side socket address the client connected to (shows IPv4 vs IPv6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,
    /// Mapping id (`/t/<id>/...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Upstream URL without query (passkeys are never written: see `redact`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// HTTP method.
    #[serde(default)]
    pub method: String,
    /// Request path.
    #[serde(default)]
    pub path: String,
    /// Raw query string exactly as received.
    #[serde(default)]
    pub query: String,
    /// Parsed parameters in original order.
    #[serde(default)]
    pub params: Vec<Param>,
    /// Hex info-hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info_hash: Option<String>,
    /// Hex peer id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    /// Mock only: requests being served at the moment this one arrived (including itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inflight: Option<u32>,
    /// All request headers in received order.
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// Upstream HTTP status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    /// Upstream latency in ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Transport error text (timeout, connect failure, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Decoded response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<RespSummary>,
    /// Response body size on the wire (after content-encoding).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_size: Option<usize>,
    /// Response `Content-Encoding`, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_encoding: Option<String>,
    /// Marker name for `kind == "marker"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marker: Option<String>,
}

impl Record {
    /// Value of a parameter (decoded form) by key.
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.iter().find(|p| p.k == key).map(|p| p.dec.as_str())
    }

    /// Raw (still-encoded) value of a parameter by key.
    pub fn param_raw(&self, key: &str) -> Option<&str> {
        self.params.iter().find(|p| p.k == key).map(|p| p.v.as_str())
    }

    /// Value of a header (case-insensitive name).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// True when the response was a usable success (HTTP 200, no failure reason).
    pub fn succeeded(&self) -> bool {
        self.upstream_status == Some(200)
            && self.response.as_ref().is_some_and(|r| r.failure_reason.is_none() && r.decode_error.is_none())
    }
}

/// Current Unix time in milliseconds.
pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0))
}

/// Percent-decode; malformed escapes are passed through literally.
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && hex_val(b[i + 1]).is_some() && hex_val(b[i + 2]).is_some() {
            out.push(hex_val(b[i + 1]).unwrap_or(0) << 4 | hex_val(b[i + 2]).unwrap_or(0));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Lowercase hex encoding.
pub fn hex(b: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        let _ = write!(s, "{x:02x}");
    }
    s
}

/// Split a raw query into `(key, value)` pairs in order, without any
/// normalisation. A parameter with no `=` gets an empty value.
pub fn split_query(q: &str) -> Vec<(&str, &str)> {
    q.split('&').filter(|s| !s.is_empty()).map(|kv| kv.split_once('=').unwrap_or((kv, ""))).collect()
}

/// Parse a raw query into ordered, decoded parameters.
pub fn parse_params(q: &str) -> Vec<Param> {
    split_query(q)
        .into_iter()
        .map(|(k, v)| {
            let dec = if k == "info_hash" || k == "peer_id" {
                hex(&percent_decode(v))
            } else {
                String::from_utf8_lossy(&percent_decode(v)).into_owned()
            };
            Param { k: k.to_owned(), v: v.to_owned(), dec }
        })
        .collect()
}

/// Describe how a raw percent-encoded binary value was encoded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncodingInfo {
    /// Number of decoded bytes.
    pub bytes: usize,
    /// Bytes sent as `%XX`.
    pub escaped: usize,
    /// Bytes sent literally.
    pub literal: usize,
    /// Unreserved (RFC 3986) bytes that were needlessly escaped.
    pub unreserved_escaped: usize,
    /// Non-unreserved bytes sent literally (unsafe).
    pub reserved_literal: usize,
    /// Hex digit case used: `upper`, `lower`, `mixed` or `none`.
    pub hex_case: &'static str,
}

/// Analyse the percent-encoding style of a raw value.
pub fn encoding_info(raw: &str) -> EncodingInfo {
    let b = raw.as_bytes();
    let (mut up, mut low) = (false, false);
    let mut info = EncodingInfo::default();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() && hex_val(b[i + 1]).is_some() && hex_val(b[i + 2]).is_some() {
            let v = hex_val(b[i + 1]).unwrap_or(0) << 4 | hex_val(b[i + 2]).unwrap_or(0);
            for c in [b[i + 1], b[i + 2]] {
                up |= c.is_ascii_uppercase();
                low |= c.is_ascii_lowercase();
            }
            info.escaped += 1;
            if is_unreserved(v) {
                info.unreserved_escaped += 1;
            }
            i += 3;
        } else {
            info.literal += 1;
            if !is_unreserved(b[i]) {
                info.reserved_literal += 1;
            }
            i += 1;
        }
    }
    info.bytes = info.escaped + info.literal;
    info.hex_case = match (up, low) {
        (true, false) => "upper",
        (false, true) => "lower",
        (true, true) => "mixed",
        (false, false) => "none",
    };
    info
}

/// RFC 3986 unreserved set.
pub fn is_unreserved(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~')
}

/// Build a summary from a (possibly content-encoded) response body.
pub fn summarize_response(body: &[u8], content_encoding: Option<&str>) -> RespSummary {
    let mut out = RespSummary::default();
    let decoded: Vec<u8> = match content_encoding.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("gzip" | "x-gzip") => match gunzip(body, flate2::read::GzDecoder::new(body)) {
            Ok(v) => v,
            Err(e) => {
                out.decode_error = Some(format!("gzip: {e}"));
                return out;
            }
        },
        Some("deflate") => match gunzip(body, flate2::read::ZlibDecoder::new(body)) {
            Ok(v) => v,
            Err(e) => {
                out.decode_error = Some(format!("deflate: {e}"));
                return out;
            }
        },
        _ => body.to_vec(),
    };
    match bencode::decode(&decoded) {
        Ok(v) => fill_summary(&v, &mut out),
        Err(e) => out.decode_error = Some(e.to_string()),
    }
    out
}

/// Bounded decompression (guards against zip bombs from a hostile upstream).
fn gunzip<R: Read>(_raw: &[u8], mut r: R) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut limited = (&mut r).take(16 * 1024 * 1024);
    limited.read_to_end(&mut out)?;
    Ok(out)
}

fn fill_summary(v: &Value, out: &mut RespSummary) {
    let Value::Dict(d) = v else {
        out.decode_error = Some("response is not a dictionary".into());
        return;
    };
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    for (k, val) in d {
        let key = String::from_utf8_lossy(k).into_owned();
        match (key.as_str(), val) {
            ("interval", Value::Int(i)) => out.interval = Some(*i),
            ("min interval", Value::Int(i)) => out.min_interval = Some(*i),
            ("complete", Value::Int(i)) => out.complete = Some(*i),
            ("incomplete", Value::Int(i)) => out.incomplete = Some(*i),
            ("peers", Value::Bytes(b)) => out.peers = Some(b.len() / 6),
            ("peers", Value::List(l)) => out.peers = Some(l.len()),
            ("peers6", Value::Bytes(b)) => out.peers6 = Some(b.len() / 18),
            ("peers6", Value::List(l)) => out.peers6 = Some(l.len()),
            ("tracker id", Value::Bytes(b)) => out.tracker_id = Some(text(b)),
            ("failure reason", Value::Bytes(b)) => out.failure_reason = Some(text(b)),
            ("warning message", Value::Bytes(b)) => out.warning_message = Some(text(b)),
            _ => {
                let short = match val {
                    Value::Int(i) => i.to_string(),
                    Value::Bytes(b) if b.len() <= 40 => text(b),
                    Value::Bytes(b) => format!("<{} bytes>", b.len()),
                    Value::List(l) => format!("<list of {}>", l.len()),
                    Value::Dict(m) => format!("<dict of {}>", m.len()),
                };
                out.other.push(format!("{key}={short}"));
            }
        }
    }
}

/// Redact an upstream URL's query and any path segment that looks like a
/// passkey (long hex/alphanumeric token) before it is logged.
pub fn redact_url(url: &str) -> String {
    let no_query = url.split_once('?').map_or(url, |(a, _)| a);
    let (scheme, rest) = no_query.split_once("://").unwrap_or(("", no_query));
    let mut parts = rest.split('/');
    let host = parts.next().unwrap_or("");
    let path: Vec<String> = parts
        .map(|seg| if seg.len() >= 20 && seg.bytes().all(|c| c.is_ascii_alphanumeric()) { "<redacted>".into() } else { seg.to_owned() })
        .collect();
    let mut s = String::new();
    if !scheme.is_empty() {
        s.push_str(scheme);
        s.push_str("://");
    }
    s.push_str(host);
    for seg in path {
        s.push('/');
        s.push_str(&seg);
    }
    s
}

/// Read all records from a JSONL file; unparsable lines are skipped and counted.
pub fn read_records(path: &std::path::Path) -> anyhow::Result<(Vec<Record>, usize)> {
    use anyhow::Context;
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut recs = Vec::new();
    let mut bad = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Record>(line) {
            Ok(r) => recs.push(r),
            Err(_) => bad += 1,
        }
    }
    recs.sort_by_key(|r| r.ts_ms);
    Ok((recs, bad))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_split_preserves_order_and_raw_bytes() {
        let q = "info_hash=%12%34%AB%ab&peer_id=-TR4130-abcdefghijkl&port=51413&uploaded=0&event=started&weird&x=a%2Fb";
        let p = parse_params(q);
        let keys: Vec<&str> = p.iter().map(|p| p.k.as_str()).collect();
        assert_eq!(keys, ["info_hash", "peer_id", "port", "uploaded", "event", "weird", "x"]);
        assert_eq!(p[0].v, "%12%34%AB%ab");
        assert_eq!(p[0].dec, "1234abab");
        assert_eq!(p[1].dec, hex(b"-TR4130-abcdefghijkl"));
        assert_eq!(p[5].v, "");
        assert_eq!(p[6].dec, "a/b");
    }

    #[test]
    fn encoding_info_detects_case_and_needless_escapes() {
        let e = encoding_info("%12abc%41%7e");
        assert_eq!(e.escaped, 3);
        assert_eq!(e.literal, 3);
        assert_eq!(e.unreserved_escaped, 2); // 0x41 'A' and 0x7e '~'
        assert_eq!(e.hex_case, "lower");
        assert_eq!(encoding_info("%AB%12").hex_case, "upper");
        assert_eq!(encoding_info("%Ab").hex_case, "mixed");
        assert_eq!(encoding_info("a b").reserved_literal, 1);
    }

    #[test]
    fn percent_decode_is_lenient() {
        assert_eq!(percent_decode("%zz%4"), b"%zz%4");
        assert_eq!(percent_decode("%41%"), b"A%");
    }

    #[test]
    fn summarize_typical_response() {
        let body = b"d8:completei5e10:incompletei2e8:intervali1800e12:min intervali900e5:peers6:\x01\x02\x03\x04\x1a\xe16:peers60:e";
        let s = summarize_response(body, None);
        assert_eq!(s.interval, Some(1800));
        assert_eq!(s.min_interval, Some(900));
        assert_eq!(s.complete, Some(5));
        assert_eq!(s.peers, Some(1));
        assert_eq!(s.peers6, Some(0));
        assert!(s.decode_error.is_none());
    }

    #[test]
    fn summarize_failure_and_garbage() {
        let s = summarize_response(b"d14:failure reason4:nopee", None);
        assert_eq!(s.failure_reason.as_deref(), Some("nope"));
        let g = summarize_response(b"<html>", None);
        assert!(g.decode_error.is_some());
        let z = summarize_response(b"\x1f\x8b garbage", Some("gzip"));
        assert!(z.decode_error.is_some());
    }

    #[test]
    fn redaction_hides_passkeys() {
        assert_eq!(
            redact_url("https://t.example.org/0123456789abcdef0123456789abcdef/announce?x=1"),
            "https://t.example.org/<redacted>/announce"
        );
        assert_eq!(redact_url("http://bttracker.debian.org:6969/announce"), "http://bttracker.debian.org:6969/announce");
    }
}
