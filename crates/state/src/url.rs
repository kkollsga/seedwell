//! Announce-URL handling that never leaks passkeys.
//!
//! Private trackers put the passkey in the query (`?passkey=...`) or in a path segment
//! (`/a/<passkey>/announce`). Nothing here keeps the query, and long opaque path segments are
//! masked whenever a URL is displayed. [`AnnounceUrl`] has no field that could hold a secret
//! (the query is dropped while parsing), so its `Debug` and `Display` are safe by construction.

use std::fmt;

/// Longest URL we look at.
pub const MAX_URL_LEN: usize = 2048;

/// A path segment that looks like a passkey: 20+ ASCII alphanumerics containing a digit.
pub(crate) fn looks_secret(seg: &str) -> bool {
    seg.len() >= 20 && seg.bytes().all(|b| b.is_ascii_alphanumeric()) && seg.bytes().any(|b| b.is_ascii_digit())
}

/// Syntax pieces of a URL; the query and fragment are discarded.
pub(crate) struct Parts<'a> {
    pub scheme: Option<String>,
    pub host: String,
    pub port: Option<u16>,
    pub segments: Vec<&'a str>,
    /// The path text had a trailing/lone slash (distinguishes `host` from `host/`).
    pub has_path: bool,
    pub has_query: bool,
}

/// Split `raw` into its parts. With `allow_glob`, a leading `*.` in the host is kept.
pub(crate) fn split(raw: &str, allow_glob: bool) -> Result<Parts<'_>, &'static str> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty URL");
    }
    if raw.len() > MAX_URL_LEN {
        return Err("URL too long");
    }
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("URL contains whitespace or control characters");
    }
    let (scheme, rest) = match raw.find("://") {
        Some(i) => {
            let s = &raw[..i];
            let ok = !s.is_empty()
                && s.len() <= 16
                && s.as_bytes()[0].is_ascii_alphabetic()
                && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'));
            if !ok {
                return Err("bad URL scheme");
            }
            (Some(s.to_ascii_lowercase()), &raw[i + 3..])
        }
        None => (None, raw),
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let (h, after) = inner.split_once(']').ok_or("unterminated IPv6 literal")?;
        let port = match after {
            "" => None,
            a => Some(a.strip_prefix(':').ok_or("bad port")?),
        };
        (h.to_string(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (hostport.to_string(), None),
        }
    };
    let port = match port {
        None => None,
        Some(p) => {
            let n: u16 = p.parse().map_err(|_| "bad port")?;
            if n == 0 {
                return Err("port 0");
            }
            Some(n)
        }
    };
    let mut host = host.to_ascii_lowercase();
    while host.ends_with('.') {
        host.pop();
    }
    if host.is_empty() {
        return Err("missing host");
    }
    let body = if allow_glob { host.strip_prefix("*.").unwrap_or(&host) } else { host.as_str() };
    if body.is_empty()
        || !body.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
    {
        return Err("host contains invalid characters");
    }
    if body.contains("..") || body.starts_with('.') {
        return Err("malformed host");
    }
    let has_query = tail.contains('?');
    let path_end = tail.find(['?', '#']).unwrap_or(tail.len());
    let path = &tail[..path_end];
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() > 32 {
        return Err("too many path segments");
    }
    Ok(Parts { scheme, host, port, segments, has_path: path.starts_with('/'), has_query })
}

pub(crate) fn strip_default_port(scheme: Option<&str>, port: Option<u16>) -> Option<u16> {
    match (scheme, port) {
        (Some("http"), Some(80)) | (Some("https"), Some(443)) => None,
        (_, p) => p,
    }
}

/// An announce URL as found in a torrent, reduced to what matching needs.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct AnnounceUrl {
    /// Lower-case scheme (`http`, `https`, `udp`).
    pub scheme: String,
    /// Lower-case host without brackets or trailing dot.
    pub host: String,
    /// Explicit port, unless it is the scheme default.
    pub port: Option<u16>,
    /// Non-empty path segments.
    pub path: Vec<String>,
    /// The URL carried a query string (which was discarded).
    pub had_query: bool,
}

impl AnnounceUrl {
    /// Parse an announce URL. Errors are static strings that never echo the input.
    pub fn parse(raw: &str) -> Result<AnnounceUrl, &'static str> {
        let p = split(raw, false)?;
        let scheme = p.scheme.ok_or("missing URL scheme")?;
        let port = strip_default_port(Some(&scheme), p.port);
        Ok(AnnounceUrl {
            scheme,
            host: p.host,
            port,
            path: p.segments.iter().map(|s| (*s).to_string()).collect(),
            had_query: p.has_query,
        })
    }
}

impl fmt::Display for AnnounceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://", self.scheme)?;
        if self.host.contains(':') {
            write!(f, "[{}]", self.host)?;
        } else {
            f.write_str(&self.host)?;
        }
        if let Some(p) = self.port {
            write!(f, ":{p}")?;
        }
        for seg in &self.path {
            f.write_str("/")?;
            f.write_str(if looks_secret(seg) { "***" } else { seg })?;
        }
        if self.path.is_empty() {
            f.write_str("/")?;
        }
        if self.had_query {
            f.write_str("?***")?;
        }
        Ok(())
    }
}

impl fmt::Debug for AnnounceUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AnnounceUrl({self})")
    }
}

/// Log-safe rendering of a raw URL: query replaced by `?***`, passkey-looking path segments by
/// `***`. Unparseable input yields a constant instead of being echoed.
pub fn redact_url(raw: &str) -> String {
    match AnnounceUrl::parse(raw) {
        Ok(u) => u.to_string(),
        Err(_) => "<unparseable url>".to_string(),
    }
}

/// What a masked secret looks like on screen.
pub const MASK: &str = "••••••";

/// Query keys whose value is a credential on private trackers.
const SECRET_KEYS: &[&str] = &[
    "passkey", "pass_key", "pid", "authkey", "auth_key", "torrent_pass", "torrentpass", "key", "token", "secret",
    "apikey", "api_key", "auth", "uid", "rsskey", "hash", "sign", "signature", "k",
];

fn secret_value(v: &str) -> bool {
    v.len() >= 20 && v.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'%'))
}

/// A path segment that is a credential: [`looks_secret`], 32+ hex digits, or a base64 / base64url
/// looking token (24+ characters of `A-Za-z0-9+=_-`, with letters and digits, no dot).
fn secret_segment(seg: &str) -> bool {
    looks_secret(seg)
        || (seg.len() >= 32 && seg.bytes().all(|b| b.is_ascii_hexdigit()))
        || base64_token(seg)
}

fn base64_token(s: &str) -> bool {
    s.len() >= 24
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'=' | b'_' | b'-'))
        && s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes().any(|b| b.is_ascii_uppercase())
        && s.bytes().any(|b| b.is_ascii_lowercase())
}

/// A value of exactly 20 bytes, raw (unreserved characters) or percent-encoded: what our own
/// `info_hash` and `peer_id` parameters look like.
fn is_20_bytes(v: &str) -> bool {
    let b = v.as_bytes();
    let (mut i, mut n) = (0, 0);
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() || !(b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit()) {
                return false;
            }
            i += 3;
        } else if b[i].is_ascii_alphanumeric() || matches!(b[i], b'-' | b'.' | b'_' | b'~') {
            i += 1;
        } else {
            return false;
        }
        n += 1;
    }
    n == 20
}

/// The full URL for the screen: scheme, host, port, path and query kept, every secret part
/// (a passkey-like path segment, the value of `passkey=`, `pid=`, `authkey=`, `torrent_pass=` ... or
/// any long opaque query value) replaced by [`MASK`]. Meant for display only; the input is not
/// logged and unparseable input yields a constant rather than being echoed.
pub fn mask_secrets(raw: &str) -> String {
    mask_impl(raw, false)
}

/// [`mask_secrets`] for the developer log: our own `info_hash` and `peer_id` parameters (exactly 20
/// bytes, percent-encoded) are kept readable, everything else is masked as for the screen. The
/// `key` parameter is masked: it identifies this client across IP changes.
pub fn mask_secrets_for_log(raw: &str) -> String {
    mask_impl(raw, true)
}

fn mask_impl(raw: &str, log: bool) -> String {
    let raw = raw.trim();
    let Some(scheme_end) = raw.find("://") else { return "<unparseable url>".into() };
    if raw.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return "<unparseable url>".into();
    }
    let rest = &raw[scheme_end + 3..];
    let e = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    // Userinfo (`user:pass@`) is a credential too.
    let authority = match rest[..e].rsplit_once('@') {
        Some((_, host)) => format!("{MASK}@{host}"),
        None => rest[..e].to_string(),
    };
    // A fragment is never sent to a tracker: dropped.
    let path_q = rest[e..].split('#').next().unwrap_or("");
    let (path, query) = match path_q.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_q, None),
    };
    let mut out = String::with_capacity(raw.len());
    out.push_str(&raw[..scheme_end + 3]);
    out.push_str(&authority);
    for (i, seg) in path.split('/').enumerate() {
        if i > 0 {
            out.push('/');
        }
        out.push_str(if secret_segment(seg) { MASK } else { seg });
    }
    if let Some(q) = query {
        out.push('?');
        for (i, pair) in q.split('&').enumerate() {
            if i > 0 {
                out.push('&');
            }
            match pair.split_once('=') {
                Some((k, v)) if log && (k == "info_hash" || k == "peer_id") && is_20_bytes(v) => out.push_str(pair),
                Some((k, v)) if !v.is_empty() && (SECRET_KEYS.iter().any(|s| k.eq_ignore_ascii_case(s)) || secret_value(v)) => {
                    out.push_str(k);
                    out.push('=');
                    out.push_str(MASK);
                }
                _ => out.push_str(pair),
            }
        }
    }
    out
}

/// Free text (a tracker's error message) with every `scheme://...` word run through [`mask_secrets`].
pub fn mask_urls_in_text(text: &str) -> String {
    text.split_inclusive(char::is_whitespace)
        .map(|word| {
            let core = word.trim_end();
            if core.contains("://") {
                let tail = &word[core.len()..];
                // Quotes and trailing punctuation around the URL are not part of it.
                let url = core.trim_matches(|c: char| matches!(c, '"' | '\'' | '(' | ')' | '<' | '>' | '`'));
                let url = url.trim_end_matches(['.', ',', ';', ':']);
                let start = core.find(url).unwrap_or(0);
                format!("{}{}{}{}", &core[..start], mask_secrets(url), &core[start + url.len()..], tail)
            } else {
                word.to_string()
            }
        })
        .collect()
}

/// Free text for the developer log (tracker messages, titles, error text): URLs inside it are
/// masked, `name=value` words with a credential name are masked, and so is any word that is a
/// passkey-looking token (20+ alphanumerics with a digit, 32+ hex, base64-looking).
pub fn mask_text(text: &str) -> String {
    let t = mask_urls_in_text(text);
    t.split_inclusive(char::is_whitespace)
        .map(|word| {
            let core = word.trim_end();
            let tail = &word[core.len()..];
            format!("{}{}", mask_word(core), tail)
        })
        .collect()
}

fn mask_word(core: &str) -> String {
    if core.contains(MASK) {
        return core.to_string();
    }
    if let Some((k, v)) = core.split_once('=') {
        let k2 = k.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
        if !v.is_empty() && SECRET_KEYS.iter().any(|s| k2.eq_ignore_ascii_case(s)) {
            return format!("{k}={MASK}");
        }
    }
    let punct = |c: char| !c.is_ascii_alphanumeric() && !matches!(c, '+' | '=' | '_' | '-');
    let lead = core.len() - core.trim_start_matches(punct).len();
    let trimmed = core.trim_matches(punct);
    if !trimmed.is_empty() && secret_segment(trimmed) {
        return format!("{}{}{}", &core[..lead], MASK, &core[lead + trimmed.len()..]);
    }
    core.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_query_and_path_secrets() {
        let u = AnnounceUrl::parse("https://Tracker.Example.org:443/a/0123456789abcdef0123456789abcdef/announce?passkey=SECRET&x=1").unwrap();
        let shown = format!("{u} {u:?}");
        assert!(!shown.contains("SECRET") && !shown.contains("0123456789abcdef"), "{shown}");
        assert_eq!(u.to_string(), "https://tracker.example.org/a/***/announce?***");
    }

    #[test]
    fn redact_url_never_echoes_garbage() {
        assert_eq!(redact_url("not a url with passkey=abc"), "<unparseable url>");
        assert_eq!(redact_url("udp://t.org:6969/announce"), "udp://t.org:6969/announce");
    }

    #[test]
    fn parser_rejects_malformed() {
        for bad in ["", "http://", "http://:80/", "ftp//x", "http://a b/", "http://h:0/", "http://h:99999/", "1http://h/", "http://[::1/", "http://.x/", "http://a..b/", "http://h/\u{1}"] {
            assert!(AnnounceUrl::parse(bad).is_err(), "{bad:?}");
        }
        assert!(AnnounceUrl::parse(&format!("http://h/{}", "a/".repeat(40))).is_err());
        assert!(AnnounceUrl::parse(&format!("http://{}/", "a".repeat(3000))).is_err());
    }

    #[test]
    fn userinfo_ipv6_and_default_ports() {
        let u = AnnounceUrl::parse("http://user:pw@[2001:db8::1]:80/announce").unwrap();
        assert_eq!(u.host, "2001:db8::1");
        assert_eq!(u.port, None);
        assert!(!u.to_string().contains("pw"));
        assert_eq!(u.to_string(), "http://[2001:db8::1]/announce");
    }

    #[test]
    fn masks_secret_parts_for_display() {
        let hex = "0123456789abcdef0123456789abcdef";
        let cases = [
            (format!("https://t.example.org/announce/{hex}"), "https://t.example.org/announce/••••••".to_string()),
            (format!("https://t.example.org/{hex}/announce"), "https://t.example.org/••••••/announce".to_string()),
            ("https://t.example.org/announce?passkey=SECRET1&x=1".into(), "https://t.example.org/announce?passkey=••••••&x=1".into()),
            ("https://t.example.org/announce.php?pid=abc".into(), "https://t.example.org/announce.php?pid=••••••".into()),
            ("https://t.example.org/announce?authkey=a1&torrent_pass=b2".into(), "https://t.example.org/announce?authkey=••••••&torrent_pass=••••••".into()),
            ("https://t.example.org/announce?PassKey=Zz".into(), "https://t.example.org/announce?PassKey=••••••".into()),
            ("udp://tracker.opentrackr.org:1337/announce".into(), "udp://tracker.opentrackr.org:1337/announce".into()),
            ("http://u:pw@t.org/announce".into(), "http://••••••@t.org/announce".into()),
        ];
        for (raw, want) in cases {
            let got = mask_secrets(&raw);
            assert_eq!(got, want, "{raw}");
            assert!(!got.contains("SECRET1") && !got.contains("0123456789abcdef"), "{got}");
        }
        // a long opaque value under an unknown key
        assert_eq!(mask_secrets("https://t.org/a?foo=0123456789abcdefghij&n=2"), "https://t.org/a?foo=••••••&n=2");
        // plain words survive
        assert_eq!(mask_secrets("https://t.org/announce?compact=1"), "https://t.org/announce?compact=1");
        assert_eq!(mask_secrets("not a url passkey=abc"), "<unparseable url>");
    }

    #[test]
    fn urls_inside_error_text_are_masked() {
        let t = mask_urls_in_text("error sending request for url (https://t.org/announce?passkey=SECRET&x=1): timed out, see https://t.org/a/0123456789abcdef0123456789abcdef.");
        assert!(!t.contains("SECRET") && !t.contains("0123456789abcdef"), "{t}");
        assert!(t.contains("timed out") && t.contains("passkey=••••••"), "{t}");
        assert_eq!(mask_urls_in_text("connection refused"), "connection refused");
    }
    const HEX: &str = "0123456789abcdef0123456789abcdef";
    const B64: &str = "Zm9vYmFyQmF6MTIzNDU2Nzg5MEFCQw";

    #[test]
    fn log_masking_keeps_info_hash_and_peer_id_and_hides_key_and_passkeys() {
        let ih = "%12%34%56%78%9A%BC%DE%F0%12%34%56%78%9A%BC%DE%F0%12%34%56%78";
        let raw = format!("https://t.example.org/announce?passkey={HEX}&info_hash={ih}&peer_id=-SW0001-abcdefghijkl&port=51413&uploaded=0&key=DEADBEEF&compact=1");
        let got = mask_secrets_for_log(&raw);
        assert!(got.contains(&format!("info_hash={ih}")), "{got}");
        assert!(got.contains("peer_id=-SW0001-abcdefghijkl"), "{got}");
        assert!(got.contains("key=••••••") && !got.contains("DEADBEEF"), "{got}");
        assert!(got.contains("passkey=••••••") && !got.contains(HEX), "{got}");
        // an info_hash that is not 20 bytes is not ours: masked like any long value
        let fake = format!("https://t.org/a?info_hash={HEX}{HEX}");
        assert!(!mask_secrets_for_log(&fake).contains(HEX));
    }

    #[test]
    fn private_tracker_shapes_never_survive_masking() {
        let shapes = [
            format!("https://t.example.org/announce?passkey={HEX}"),
            format!("https://t.example.org/announce.php?authkey={B64}&torrent_pass={HEX}"),
            format!("http://t.example.org:2710/{HEX}/announce"),
            format!("https://t.example.org/announce/{HEX}"),
            format!("https://t.example.org/a/{B64}/announce"),
            format!("https://t.example.org/announce?pid={HEX}&uid=4242"),
            format!("https://u:{HEX}@t.example.org/announce"),
            "https://t.example.org/announce?k=SECRETVALUE1".to_string(),
            format!("https://t.example.org/announce?foo={B64}&bar=1"),
        ];
        for raw in shapes {
            for got in [mask_secrets(&raw), mask_secrets_for_log(&raw), mask_text(&format!("failed: GET {raw} reset")), mask_text(&raw)] {
                for secret in [HEX, B64, "SECRETVALUE1", "4242"] {
                    assert!(!got.contains(secret), "{secret} survived in {got} (from {raw})");
                }
            }
        }
    }

    #[test]
    fn text_masking_hides_tokens_and_keeps_prose() {
        let t = mask_text(&format!("bad passkey={HEX}, token {B64} unknown; Ubuntu 24.04 desktop amd64 is fine"));
        assert!(!t.contains(HEX) && !t.contains(B64), "{t}");
        assert!(t.contains("Ubuntu 24.04 desktop amd64 is fine"), "{t}");
        assert_eq!(mask_text("Unregistered torrent"), "Unregistered torrent");
    }
}
