//! Tracker URLs and announce URL construction (RFC 3986 exact encoding).

use std::fmt::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use crate::types::{AnnounceParams, Crypto};

/// URL scheme of a tracker.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Scheme {
    /// Plain HTTP.
    Http,
    /// HTTP over TLS.
    Https,
    /// BEP 15 UDP.
    Udp,
}

/// Why a tracker URL was rejected.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UrlError {
    /// Not `http`, `https` or `udp`.
    UnsupportedScheme,
    /// Empty, too long, or containing non-ASCII/control/space characters.
    Malformed,
    /// Missing host.
    NoHost,
    /// Bad or missing port (UDP needs an explicit port).
    BadPort,
    /// `user:pass@` is not supported (and would leak credentials into logs).
    UserInfo,
}

impl fmt::Display for UrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            UrlError::UnsupportedScheme => "unsupported tracker URL scheme",
            UrlError::Malformed => "malformed tracker URL",
            UrlError::NoHost => "tracker URL has no host",
            UrlError::BadPort => "tracker URL has a bad port",
            UrlError::UserInfo => "tracker URL contains credentials",
        })
    }
}
impl std::error::Error for UrlError {}

/// Longest tracker URL we accept.
pub const MAX_URL_LEN: usize = 2048;

/// A parsed tracker URL.
///
/// Passkeys live in the path or query, so `Display`/`Debug` print only
/// `scheme://host:port/…`. The raw text is only available via [`raw`](Self::raw)
/// and must never be logged (SPEC 7B.4).
///
/// size: 24 bytes plus one heap block holding the raw text followed by the
/// canonical host (one allocation per URL; the hub shares equal URLs between
/// torrents).
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TrackerUrl {
    /// `raw` text immediately followed by the canonical host name.
    buf: Box<str>,
    /// Length of the raw text within `buf`.
    raw_len: u16,
    /// Byte offset in the raw text where the path starts (`raw_len` if none).
    path_start: u16,
    port: u16,
    scheme: Scheme,
}

impl TrackerUrl {
    /// Parse and validate. Fragments are dropped; the path and query are kept
    /// byte-for-byte.
    pub fn parse(input: &str) -> Result<TrackerUrl, UrlError> {
        let input = input.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n');
        if input.is_empty() || input.len() > MAX_URL_LEN {
            return Err(UrlError::Malformed);
        }
        if !input.bytes().all(|b| (0x21..0x7f).contains(&b)) {
            return Err(UrlError::Malformed);
        }
        let input = input.split('#').next().unwrap_or("");
        let (scheme_str, rest) = input.split_once("://").ok_or(UrlError::Malformed)?;
        let scheme = if scheme_str.eq_ignore_ascii_case("http") {
            Scheme::Http
        } else if scheme_str.eq_ignore_ascii_case("https") {
            Scheme::Https
        } else if scheme_str.eq_ignore_ascii_case("udp") {
            Scheme::Udp
        } else {
            return Err(UrlError::UnsupportedScheme);
        };
        let auth_end = rest.find(['/', '?']).unwrap_or(rest.len());
        let authority = &rest[..auth_end];
        if authority.contains('@') {
            return Err(UrlError::UserInfo);
        }
        let (host, port_str) = if let Some(stripped) = authority.strip_prefix('[') {
            let close = stripped.find(']').ok_or(UrlError::Malformed)?;
            let host = &stripped[..close];
            let after = &stripped[close + 1..];
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(UrlError::Malformed);
            }
            match after {
                "" => (host, None),
                a => (host, Some(a.strip_prefix(':').ok_or(UrlError::Malformed)?)),
            }
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(UrlError::NoHost);
        }
        if !authority.starts_with('[') && host.contains(':') {
            return Err(UrlError::Malformed);
        }
        let valid_host_char =
            |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_' | ':');
        if !host.chars().all(valid_host_char) {
            return Err(UrlError::Malformed);
        }
        let canonical = canonical_host(host).ok_or(UrlError::Malformed)?;
        let port = match port_str {
            Some(p) => {
                let n: u16 = p.parse().map_err(|_| UrlError::BadPort)?;
                if n == 0 {
                    return Err(UrlError::BadPort);
                }
                n
            }
            None => match scheme {
                Scheme::Http => 80,
                Scheme::Https => 443,
                Scheme::Udp => return Err(UrlError::BadPort),
            },
        };
        let prefix_len = scheme_str.len() + 3 + auth_end;
        let mut buf = String::with_capacity(input.len() + canonical.len());
        buf.push_str(&scheme_str.to_ascii_lowercase());
        buf.push_str("://");
        buf.push_str(authority);
        buf.push_str(&input[prefix_len..]);
        let raw_len = u16::try_from(buf.len()).map_err(|_| UrlError::Malformed)?;
        let path_start =
            u16::try_from(scheme_str.len() + 3 + auth_end).map_err(|_| UrlError::Malformed)?;
        buf.push_str(&canonical);
        Ok(TrackerUrl {
            buf: buf.into_boxed_str(),
            raw_len,
            path_start,
            port,
            scheme,
        })
    }

    /// Scheme.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }
    /// Canonical host (see [`canonical_host`]; IPv6 literals without
    /// brackets). This is the key the host gate budgets on, regardless of port
    /// or scheme, so every spelling of one machine shares one budget.
    pub fn host(&self) -> &str {
        &self.buf[usize::from(self.raw_len)..]
    }
    /// The host as an IP address when the URL names one by literal (any
    /// spelling: `127.1`, `2130706433`, `[::ffff:7f00:1]`).
    pub fn host_ip(&self) -> Option<IpAddr> {
        self.host().parse().ok()
    }
    /// Port (defaults 80/443 applied).
    pub fn port(&self) -> u16 {
        self.port
    }
    /// The full URL including any passkey. Never log this.
    pub fn raw(&self) -> &str {
        &self.buf[..usize::from(self.raw_len)]
    }
    /// Path and query exactly as given (`/` if there is none).
    pub fn path_and_query(&self) -> &str {
        let p = &self.raw()[usize::from(self.path_start)..];
        if p.is_empty() { "/" } else { p }
    }
    /// True when `self` is the https twin of the http URL `other` (same host).
    /// The only scheme change a redirect may make.
    pub fn is_https_upgrade_of(&self, other: &TrackerUrl) -> bool {
        other.scheme == Scheme::Http && self.scheme == Scheme::Https && self.host() == other.host()
    }
}

impl fmt::Display for TrackerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self.scheme {
            Scheme::Http => "http",
            Scheme::Https => "https",
            Scheme::Udp => "udp",
        };
        if self.host().contains(':') {
            write!(f, "{s}://[{}]:{}/…", self.host(), self.port)
        } else {
            write!(f, "{s}://{}:{}/…", self.host(), self.port)
        }
    }
}
impl fmt::Debug for TrackerUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TrackerUrl({self})")
    }
}

/// Canonical form of a URL host, the key every per-host budget uses:
///
/// * ASCII lower case;
/// * one trailing dot removed (`tracker.example.org.` is `tracker.example.org`);
/// * IPv4 literals in any `inet_aton` spelling (`127.1`, `2130706433`,
///   `0x7f.0.0.1`, `0177.0.0.1`) become dotted decimal;
/// * IPv6 literals become their shortest form, and IPv4-mapped IPv6
///   (`::ffff:127.0.0.1`) becomes the IPv4 address.
///
/// `None` for hosts that are not valid (a numeric last label that does not
/// parse as an IPv4 address, `a..b`-style empty labels, a bad IPv6 literal).
/// `host` is given without the brackets of an IPv6 literal.
pub fn canonical_host(host: &str) -> Option<String> {
    let lower = host.to_ascii_lowercase();
    if lower.contains(':') {
        let v6: Ipv6Addr = lower.parse().ok()?;
        return Some(match v6.to_ipv4_mapped() {
            Some(v4) => v4.to_string(),
            None => v6.to_string(),
        });
    }
    let name = lower.strip_suffix('.').unwrap_or(&lower);
    if name.is_empty() || name.starts_with('.') || name.contains("..") || name.ends_with('.') {
        return None;
    }
    let last = name.rsplit('.').next().unwrap_or("");
    if is_numeric_label(last) {
        return parse_inet_aton(name).map(|a| a.to_string());
    }
    Some(name.to_owned())
}

/// A label the WHATWG URL rules (and `getaddrinfo`) read as a number: all
/// decimal digits, or `0x` followed by hex digits.
fn is_numeric_label(l: &str) -> bool {
    if l.is_empty() {
        return false;
    }
    if let Some(h) = l.strip_prefix("0x") {
        return h.bytes().all(|b| b.is_ascii_hexdigit());
    }
    l.bytes().all(|b| b.is_ascii_digit())
}

/// `inet_aton`: 1-4 parts, each decimal / `0x` hex / leading-zero octal, the
/// last part filling the remaining bytes.
fn parse_inet_aton(s: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut nums = Vec::with_capacity(4);
    for p in &parts {
        let v = if let Some(h) = p.strip_prefix("0x") {
            if h.is_empty() {
                0
            } else {
                u64::from_str_radix(h, 16).ok()?
            }
        } else if p.len() > 1 && p.starts_with('0') {
            u64::from_str_radix(&p[1..], 8).ok()?
        } else {
            p.parse::<u64>().ok()?
        };
        nums.push(v);
    }
    let (last, head) = nums.split_last()?;
    if head.iter().any(|&v| v > 255) {
        return None;
    }
    let room = 1u64 << (8 * (5 - parts.len()) as u32);
    if *last >= room {
        return None;
    }
    let mut addr = *last;
    for (i, &v) in head.iter().enumerate() {
        addr |= v << (8 * (3 - i) as u32);
    }
    Some(Ipv4Addr::from(addr as u32))
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode every byte except RFC 3986 unreserved characters
/// (`A-Za-z0-9-._~`), with uppercase hex. Used for `info_hash`, `peer_id` and
/// `trackerid`: classic client bugs live exactly here.
pub fn percent_encode_into(out: &mut String, bytes: &[u8]) {
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 15) as usize] as char);
        }
    }
}

/// One announce parameter we may send. There is deliberately no variant for
/// `redundant`, `no_peer_id` or `ip`: they are optional extras that trackers ignore and
/// Transmission does not send either. `corrupt` is sent (as Transmission does) only when it is
/// above zero.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Param {
    /// `info_hash`
    InfoHash,
    /// `peer_id`
    PeerId,
    /// `port`
    Port,
    /// `uploaded`
    Uploaded,
    /// `downloaded`
    Downloaded,
    /// `left`
    Left,
    /// `numwant`
    Numwant,
    /// `key`
    Key,
    /// `compact`
    Compact,
    /// `supportcrypto` and, when required, `requirecrypto` (absent when encryption is off)
    Crypto,
    /// `corrupt`: bytes of pieces that failed their hash check this session (absent when zero)
    Corrupt,
    /// `event` (absent on regular announces)
    Event,
    /// `trackerid` (only after the tracker sent one)
    TrackerId,
    /// `ipv4` (opt-in)
    Ipv4,
    /// `ipv6` (opt-in)
    Ipv6,
}

/// THE parameter order, in one place: Transmission 4.x's, as captured from announces
/// (`info_hash, peer_id, port, uploaded,
/// downloaded, left, numwant, key, compact, [event]`). Transmission's source
/// (`announcer-http.cc`, `announce_url_new`) appends `trackerid` after `event`
/// and its captured `ipv4` last, so we do the same. Parameters whose value is
/// absent are skipped. SPEC §12 decision 18.
///
/// `supportcrypto` follows `compact` as in the 4.1.3 capture. `requirecrypto` is not in the
/// capture; libtransmission's `announce_url_new` (announcer-http.cc, 4.0.x) writes
/// `...&compact=1&supportcrypto=1`, then `&requirecrypto=1` when its encryption mode is
/// "required", then `corrupt`, `event` and `trackerid`, so it sits right after `supportcrypto`.
pub const PARAM_ORDER: [Param; 15] = [
    Param::InfoHash,
    Param::PeerId,
    Param::Port,
    Param::Uploaded,
    Param::Downloaded,
    Param::Left,
    Param::Numwant,
    Param::Key,
    Param::Compact,
    Param::Crypto,
    Param::Corrupt,
    Param::Event,
    Param::TrackerId,
    Param::Ipv4,
    Param::Ipv6,
];

/// Separator to emit before the next parameter.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sep {
    /// URL has no query yet.
    Question,
    /// Continue an existing query.
    Amp,
    /// URL already ends in `?` or `&`.
    Nothing,
}

fn push_param(out: &mut String, sep: &mut Sep, name: &str, write_value: impl FnOnce(&mut String)) {
    match *sep {
        Sep::Question => out.push('?'),
        Sep::Amp => out.push('&'),
        Sep::Nothing => {}
    }
    *sep = Sep::Amp;
    out.push_str(name);
    out.push('=');
    write_value(out);
}

/// Build the full HTTP(S) announce URL: the tracker URL's existing path and
/// query are preserved byte-for-byte and our parameters are appended (`&` when
/// the URL already has a query, e.g. `?passkey=...`).
pub fn build_announce_url(base: &TrackerUrl, p: &AnnounceParams) -> String {
    let mut out = String::with_capacity(base.raw().len() + 256);
    // Everything before the query, then decide the separator.
    let raw = base.raw();
    out.push_str(raw);
    // Existing query (passkey): continue it, and avoid "??" / "&&" when the
    // URL already ends in a separator.
    let mut sep = if !raw.contains('?') {
        Sep::Question
    } else if raw.ends_with('?') || raw.ends_with('&') {
        Sep::Nothing
    } else {
        Sep::Amp
    };
    let first = &mut sep;
    for param in PARAM_ORDER {
        match param {
            Param::InfoHash => push_param(&mut out, first, "info_hash", |o| {
                percent_encode_into(o, &p.info_hash)
            }),
            Param::PeerId => push_param(&mut out, first, "peer_id", |o| {
                percent_encode_into(o, &p.peer_id)
            }),
            Param::Port => push_param(&mut out, first, "port", |o| {
                let _ = write!(o, "{}", p.port);
            }),
            Param::Uploaded => push_param(&mut out, first, "uploaded", |o| {
                let _ = write!(o, "{}", p.uploaded);
            }),
            Param::Downloaded => push_param(&mut out, first, "downloaded", |o| {
                let _ = write!(o, "{}", p.downloaded);
            }),
            Param::Left => push_param(&mut out, first, "left", |o| {
                let _ = write!(o, "{}", p.left);
            }),
            Param::Numwant => push_param(&mut out, first, "numwant", |o| {
                let _ = write!(o, "{}", p.numwant);
            }),
            Param::Key => push_param(&mut out, first, "key", |o| {
                let _ = write!(o, "{:08X}", p.key);
            }),
            Param::Compact => push_param(&mut out, first, "compact", |o| o.push('1')),
            Param::Crypto => {
                if p.crypto != Crypto::Off {
                    push_param(&mut out, first, "supportcrypto", |o| o.push('1'));
                }
                if p.crypto == Crypto::Require {
                    push_param(&mut out, first, "requirecrypto", |o| o.push('1'));
                }
            }
            Param::Corrupt => {
                if p.corrupt > 0 {
                    push_param(&mut out, first, "corrupt", |o| {
                        let _ = write!(o, "{}", p.corrupt);
                    });
                }
            }
            Param::Event => {
                if let Some(e) = p.event {
                    push_param(&mut out, first, "event", |o| o.push_str(e.as_str()));
                }
            }
            Param::TrackerId => {
                if let Some(t) = &p.tracker_id {
                    push_param(&mut out, first, "trackerid", |o| percent_encode_into(o, t));
                }
            }
            Param::Ipv4 => {
                if let Some(a) = p.ipv4 {
                    push_param(&mut out, first, "ipv4", |o| {
                        percent_encode_into(o, a.to_string().as_bytes())
                    });
                }
            }
            Param::Ipv6 => {
                if let Some(a) = p.ipv6 {
                    push_param(&mut out, first, "ipv6", |o| {
                        percent_encode_into(o, a.to_string().as_bytes())
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Event;

    fn params() -> AnnounceParams {
        let mut ih = [0u8; 20];
        for (i, b) in ih.iter_mut().enumerate() {
            *b = i as u8;
        }
        AnnounceParams {
            info_hash: ih,
            peer_id: *b"-SX0001-abcdefghijkl",
            port: 6881,
            uploaded: 1,
            downloaded: 2,
            corrupt: 0,
            left: 3,
            key: 0xDEAD_BEEF,
            event: None,
            numwant: 50,
            tracker_id: None,
            ipv4: None,
            ipv6: None,
            crypto: Default::default(),
        }
    }

    fn enc(bytes: &[u8]) -> String {
        let mut s = String::new();
        percent_encode_into(&mut s, bytes);
        s
    }

    #[test]
    fn canonical_hosts() {
        let c = |s: &str| canonical_host(s);
        assert_eq!(
            c("Tracker.Example.ORG").as_deref(),
            Some("tracker.example.org")
        );
        assert_eq!(
            c("tracker.example.org.").as_deref(),
            Some("tracker.example.org")
        );
        assert_eq!(c("tracker.example.org..").as_deref(), None);
        assert_eq!(c(".example.org").as_deref(), None);
        assert_eq!(c("a..b").as_deref(), None);
        for v4 in [
            "127.0.0.1",
            "127.1",
            "2130706433",
            "0x7f.1",
            "0177.0.0.1",
            "0x7f000001",
            "127.0.1",
        ] {
            assert_eq!(c(v4).as_deref(), Some("127.0.0.1"), "{v4}");
        }
        assert_eq!(c("10.1.2.3").as_deref(), Some("10.1.2.3"));
        assert_eq!(c("256.1.1.1"), None);
        assert_eq!(c("1.2.3.4.5"), None);
        assert_eq!(c("08.1.1.1"), None, "invalid octal");
        assert_eq!(c("4294967296"), None);
        assert_eq!(c("4294967295").as_deref(), Some("255.255.255.255"));
        // names that merely contain digits stay names
        assert_eq!(c("example.to").as_deref(), Some("example.to"));
        assert_eq!(c("123.example").as_deref(), Some("123.example"));
        assert_eq!(c("::1").as_deref(), Some("::1"));
        assert_eq!(c("0:0:0:0:0:0:0:1").as_deref(), Some("::1"));
        assert_eq!(c("2001:DB8:0:0:0:0:0:1").as_deref(), Some("2001:db8::1"));
        assert_eq!(c("::ffff:127.0.0.1").as_deref(), Some("127.0.0.1"));
        assert_eq!(c("::ffff:7f00:1").as_deref(), Some("127.0.0.1"));
        assert_eq!(c("1::2::3"), None);
    }

    #[test]
    fn urls_share_one_host_key_across_spellings() {
        let h = |s: &str| TrackerUrl::parse(s).unwrap().host().to_owned();
        assert_eq!(h("http://t.example./a"), h("http://T.EXAMPLE/a"));
        assert_eq!(h("udp://[::ffff:10.0.0.1]:80/a"), h("udp://10.0.0.1:80/a"));
        assert_eq!(h("http://[0::0001]/a"), h("http://[::1]/a"));
        assert!(TrackerUrl::parse("http://999.1.1.1/a").is_err());
        assert_eq!(
            TrackerUrl::parse("http://2130706433:8080/a")
                .unwrap()
                .host_ip(),
            Some("127.0.0.1".parse().unwrap())
        );
        assert_eq!(
            TrackerUrl::parse("http://t.example/a").unwrap().host_ip(),
            None
        );
    }

    #[test]
    fn encodes_every_non_unreserved_byte_uppercase() {
        for b in 0u8..=255 {
            let e = enc(&[b]);
            let unreserved = b.is_ascii_alphanumeric() || b"-._~".contains(&b);
            if unreserved {
                assert_eq!(e, (b as char).to_string());
            } else {
                assert_eq!(e, format!("%{b:02X}"), "byte {b:#x}");
            }
        }
    }

    #[test]
    fn encoding_vectors() {
        assert_eq!(enc(b""), "");
        assert_eq!(enc(b"a b"), "a%20b", "space is %20, never '+'");
        assert_eq!(enc(b"~-._"), "~-._");
        assert_eq!(enc(b"+/=&?#%"), "%2B%2F%3D%26%3F%23%25");
        assert_eq!(enc(&[0xff, 0x00, 0x7f, 0x80]), "%FF%00%7F%80");
        assert_eq!(enc("é".as_bytes()), "%C3%A9");
        assert_eq!(enc(b"*'()!$,;:@"), "%2A%27%28%29%21%24%2C%3B%3A%40");
    }

    #[test]
    fn exact_url_plain() {
        let base = TrackerUrl::parse("http://tracker.example.com:6969/announce").unwrap();
        let url = build_announce_url(&base, &params());
        assert_eq!(
            url,
            "http://tracker.example.com:6969/announce?info_hash=%00%01%02%03%04%05%06%07%08%09%0A%0B%0C%0D%0E%0F%10%11%12%13\
             &peer_id=-SX0001-abcdefghijkl&port=6881&uploaded=1&downloaded=2&left=3&numwant=50&key=DEADBEEF\
             &compact=1"
        );
    }

    #[test]
    fn passkey_in_query_is_preserved_and_extended_with_ampersand() {
        let base = TrackerUrl::parse("https://t.example/announce?passkey=0123abcd").unwrap();
        let url = build_announce_url(&base, &params());
        assert!(
            url.starts_with("https://t.example/announce?passkey=0123abcd&info_hash=%00%01"),
            "{url}"
        );
        assert_eq!(url.matches('?').count(), 1);
    }

    #[test]
    fn passkey_in_path_is_preserved() {
        let base =
            TrackerUrl::parse("http://t.example:2710/0123456789abcdef0123456789abcdef/announce")
                .unwrap();
        let url = build_announce_url(&base, &params());
        assert!(
            url.starts_with(
                "http://t.example:2710/0123456789abcdef0123456789abcdef/announce?info_hash="
            ),
            "{url}"
        );
    }

    #[test]
    fn trailing_separators_are_not_doubled() {
        for (input, prefix) in [
            (
                "http://t.example/announce?",
                "http://t.example/announce?info_hash=",
            ),
            (
                "http://t.example/announce?a=1&",
                "http://t.example/announce?a=1&info_hash=",
            ),
            (
                "http://t.example/announce?a=1&b=%20",
                "http://t.example/announce?a=1&b=%20&info_hash=",
            ),
        ] {
            let url = build_announce_url(&TrackerUrl::parse(input).unwrap(), &params());
            assert!(url.starts_with(prefix), "{url}");
            assert!(!url.contains("??") && !url.contains("&&"));
        }
    }

    #[test]
    fn events_trackerid_and_opt_in_ips() {
        let base = TrackerUrl::parse("http://t.example/a").unwrap();
        let mut p = params();
        p.event = Some(Event::Started);
        p.tracker_id = Some(b"id/1 ~".to_vec().into());
        let url = build_announce_url(&base, &p);
        assert!(
            url.ends_with(
                "&left=3&numwant=50&key=DEADBEEF&compact=1&event=started&trackerid=id%2F1%20~"
            ),
            "{url}"
        );
        // ipv4/ipv6 only when explicitly configured, always last
        p.ipv4 = Some("192.0.2.7".parse().unwrap());
        p.ipv6 = Some("2001:db8::1".parse().unwrap());
        let url = build_announce_url(&base, &p);
        assert!(
            url.ends_with("&trackerid=id%2F1%20~&ipv4=192.0.2.7&ipv6=2001%3Adb8%3A%3A1"),
            "{url}"
        );
        p.ipv4 = None;
        p.ipv6 = None;
        p.event = Some(Event::Stopped);
        p.numwant = 0;
        let url = build_announce_url(&base, &p);
        assert!(
            url.contains("&numwant=0&key=DEADBEEF&compact=1&event=stopped"),
            "{url}"
        );
        p.event = Some(Event::Completed);
        assert!(build_announce_url(&base, &p).contains("&compact=1&event=completed"));
    }

    #[test]
    fn corrupt_is_sent_only_when_non_zero_after_crypto_and_before_event() {
        let base = TrackerUrl::parse("http://t.example/a").unwrap();
        let mut p = params();
        p.crypto = Crypto::Require;
        p.event = Some(Event::Stopped);
        p.corrupt = 0;
        assert!(!build_announce_url(&base, &p).contains("corrupt"));
        p.corrupt = 4_194_304;
        let url = build_announce_url(&base, &p);
        assert!(url.contains("&compact=1&supportcrypto=1&requirecrypto=1&corrupt=4194304&event=stopped"), "{url}");
    }

    #[test]
    fn regular_announce_has_no_event_and_nothing_we_dropped() {
        let base = TrackerUrl::parse("http://t.example/a").unwrap();
        let url = build_announce_url(&base, &params());
        for banned in [
            "event=",
            "corrupt",
            "redundant",
            "no_peer_id",
            "supportcrypto",
            "requirecrypto",
            "ip=",
            "ipv4",
            "ipv6",
            "trackerid",
            "+",
        ] {
            assert!(!url.contains(banned), "{banned} in {url}");
        }
        assert!(url.ends_with("&compact=1"));
        // key is 8 upper-case hex digits, also when small
        let mut p = params();
        p.key = 0xAB;
        assert!(build_announce_url(&base, &p).contains("&key=000000AB&"));
    }

    #[test]
    fn crypto_parameters_follow_the_policy_right_after_compact() {
        let base = TrackerUrl::parse("http://t.example/a").unwrap();
        let with = |crypto: Crypto, event: Option<Event>| {
            let mut p = params();
            p.crypto = crypto;
            p.event = event;
            build_announce_url(&base, &p)
        };
        // Off (default): nothing.
        let off = with(Crypto::Off, None);
        assert!(off.ends_with("&compact=1") && !off.contains("crypto"), "{off}");
        // Prefer: supportcrypto=1 only.
        let prefer = with(Crypto::Support, None);
        assert!(prefer.ends_with("&compact=1&supportcrypto=1"), "{prefer}");
        assert!(!prefer.contains("requirecrypto"), "{prefer}");
        // Require: both, supportcrypto first (libtransmission order).
        let require = with(Crypto::Require, None);
        assert!(require.ends_with("&compact=1&supportcrypto=1&requirecrypto=1"), "{require}");
        // Before `event`, `trackerid` and the opt-in addresses.
        let mut p = params();
        p.crypto = Crypto::Require;
        p.event = Some(Event::Started);
        p.tracker_id = Some(b"id".to_vec().into_boxed_slice());
        p.ipv4 = Some("192.0.2.7".parse().unwrap());
        let url = build_announce_url(&base, &p);
        assert!(
            url.contains("&compact=1&supportcrypto=1&requirecrypto=1&event=started&trackerid=id&ipv4=192.0.2.7"),
            "{url}"
        );
        // A passkey in the base URL does not disturb it.
        let pk = TrackerUrl::parse("https://t.example/announce?passkey=abc").unwrap();
        let mut p = params();
        p.crypto = Crypto::Support;
        assert!(build_announce_url(&pk, &p).ends_with("&compact=1&supportcrypto=1"));
    }

    /// Full URL for the Debian netinst info-hash, compared with what
    /// Transmission 4.1.3 actually sent (captured announce,
    /// `transmission-4.1.3-debian.txt`: parameter order
    /// `info_hash&peer_id&port&uploaded&downloaded&left&numwant&key&compact&supportcrypto&event&ipv4`,
    /// upper-case hex, `numwant` 80 on `started` and 0 on `stopped`).
    /// Allowed differences: `peer_id`, `key`, `port` values and `ipv4` (opt-in, so
    /// we drop it). `supportcrypto` is sent (default policy `prefer`) in the same place.
    #[test]
    fn matches_the_transmission_capture_modulo_identity_and_dropped_params() {
        const DEBIAN: [u8; 20] = [
            0x7a, 0xcf, 0x8f, 0xb5, 0x90, 0xb2, 0x06, 0x0d, 0xd9, 0xc3, 0x14, 0x6e, 0xf7, 0x70,
            0x16, 0x9d, 0x59, 0x34, 0x33, 0xb0,
        ];
        let base = TrackerUrl::parse("http://bttracker.debian.org:6969/announce").unwrap();
        let ours = |event: Event, down: u64, left: u64, numwant: u32| {
            build_announce_url(
                &base,
                &AnnounceParams {
                    info_hash: DEBIAN,
                    peer_id: *b"-SX0001-abcdefghijkl",
                    port: 6881,
                    uploaded: 0,
                    downloaded: down,
                    corrupt: 0,
                    left,
                    key: 0x0A1B_2C3D,
                    event: Some(event),
                    numwant,
                    tracker_id: None,
                    ipv4: None,
                    ipv6: None,
                    crypto: Crypto::Support,
                },
            )
        };
        // Exactly what our client sends:
        assert_eq!(
            ours(Event::Started, 0, 792_723_456, 80),
            "http://bttracker.debian.org:6969/announce?info_hash=z%CF%8F%B5%90%B2%06%0D%D9%C3%14n%F7p%16%9DY43%B0\
             &peer_id=-SX0001-abcdefghijkl&port=6881&uploaded=0&downloaded=0&left=792723456\
             &numwant=80&key=0A1B2C3D&compact=1&supportcrypto=1&event=started"
        );
        // What Transmission 4.1.3 sent (the `started` at +00:00.7 and the
        // `stopped` at +00:15.9 of the capture; `<ipv4>` stands for the
        // redacted public address).
        let transmission = [
            (
                "http://bttracker.debian.org:6969/announce?info_hash=z%CF%8F%B5%90%B2%06%0D%D9%C3%14n%F7p%16%9DY43%B0\
                 &peer_id=-TR4130-zjcuzsuibwhl&port=51423&uploaded=0&downloaded=0&left=792723456\
                 &numwant=80&key=DCBD59FD&compact=1&supportcrypto=1&event=started&ipv4=<ipv4>",
                ours(Event::Started, 0, 792_723_456, 80),
            ),
            (
                "http://bttracker.debian.org:6969/announce?info_hash=z%CF%8F%B5%90%B2%06%0D%D9%C3%14n%F7p%16%9DY43%B0\
                 &peer_id=-TR4130-zjcuzsuibwhl&port=51423&uploaded=0&downloaded=201326592&left=584695808\
                 &numwant=0&key=DCBD59FD&compact=1&supportcrypto=1&event=stopped&ipv4=<ipv4>",
                ours(Event::Stopped, 201_326_592, 584_695_808, 0),
            ),
        ];
        let normalise = |u: &str| -> String {
            u.split(['?', '&'])
                .filter(|kv| !kv.starts_with("ipv4="))
                .map(|kv| match kv.split_once('=') {
                    Some(("peer_id" | "key" | "port", _)) => {
                        format!("{}=<ours-or-theirs>", kv.split('=').next().unwrap())
                    }
                    _ => kv.to_owned(),
                })
                .collect::<Vec<_>>()
                .join("&")
        };
        for (theirs, ours) in transmission {
            assert_eq!(
                normalise(&ours),
                normalise(theirs),
                "\nours:   {ours}\ntheirs: {theirs}"
            );
        }
    }

    #[test]
    fn parse_accepts_and_rejects() {
        let u = TrackerUrl::parse("HTTP://Tracker.Example.COM/Announce?x=Y#frag").unwrap();
        assert_eq!(u.host(), "tracker.example.com");
        assert_eq!(u.port(), 80);
        assert_eq!(
            u.path_and_query(),
            "/Announce?x=Y",
            "case of path/query is preserved, fragment dropped"
        );
        assert_eq!(
            TrackerUrl::parse("https://t.example/a").unwrap().port(),
            443
        );
        let u = TrackerUrl::parse("udp://tracker.opentrackr.org:1337/announce").unwrap();
        assert_eq!((u.scheme(), u.port()), (Scheme::Udp, 1337));
        let u = TrackerUrl::parse("http://[2001:DB8::1]:6969/announce").unwrap();
        assert_eq!((u.host(), u.port()), ("2001:db8::1", 6969));
        assert_eq!(
            TrackerUrl::parse("http://t.example")
                .unwrap()
                .path_and_query(),
            "/"
        );
        for bad in [
            "",
            "ftp://x/y",
            "http://",
            "http:///a",
            "http://u:p@host/a",
            "udp://host/announce",
            "http://host:0/a",
            "http://host:99999/a",
            "http://ho st/a",
            "http://host/a b",
            "http://a:b:80/",
            "http://[::1/a",
            "nonsense",
            "http://exämple.com/a",
            "wss://x/y",
        ] {
            assert!(TrackerUrl::parse(bad).is_err(), "{bad}");
        }
        assert!(TrackerUrl::parse(&format!("http://t.example/{}", "a".repeat(3000))).is_err());
    }

    #[test]
    fn display_never_contains_the_passkey() {
        let u = TrackerUrl::parse("http://t.example:80/SECRETPASSKEY/announce?passkey=SECRET2")
            .unwrap();
        for s in [format!("{u}"), format!("{u:?}")] {
            assert!(!s.contains("SECRET"), "{s}");
            assert!(s.contains("t.example"));
        }
    }

    #[test]
    fn https_upgrade_detection() {
        let a = TrackerUrl::parse("http://t.example/a").unwrap();
        let b = TrackerUrl::parse("https://t.example/a").unwrap();
        let c = TrackerUrl::parse("https://other.example/a").unwrap();
        assert!(b.is_https_upgrade_of(&a));
        assert!(!a.is_https_upgrade_of(&b));
        assert!(!c.is_https_upgrade_of(&a));
    }
}
