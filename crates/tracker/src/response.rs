//! Tracker response model, defensive parsing and the interval clamping rules
//! of SPEC 7C.1. Nothing here panics on any input.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

use trs_bencode::{DecodeLimits, Value};

use crate::types::{Peer, PeerId20};

/// Interval used when the tracker sent none, or garbage (SPEC 7C.1).
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Smallest interval we accept when the tracker sent no `min interval`.
pub const MIN_INTERVAL_FLOOR: Duration = Duration::from_secs(5 * 60);
/// Smallest `min interval` we respect.
pub const MIN_INTERVAL_MIN: Duration = Duration::from_secs(60);
/// Largest interval/min interval we accept.
pub const MAX_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Peers kept from one response (compact + dict + v6 combined).
pub const MAX_PEERS: usize = 200;
/// Largest response body we parse.
pub const MAX_BODY: usize = 1 << 20;
/// Longest tracker-supplied text (failure reason, warning) kept for the UI.
pub const MAX_TEXT_CHARS: usize = 512;
/// Longest `tracker id` we echo.
pub const MAX_TRACKER_ID: usize = 256;

/// The scheduling numbers of a response after clamping.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Timing {
    /// Effective announce interval (always >= `min_interval` when present).
    pub interval: Duration,
    /// Effective `min interval` (60 s ..= 24 h), if the tracker sent one.
    pub min_interval: Option<Duration>,
}

/// Apply the SPEC 7C.1 clamping rules to raw tracker values (seconds).
///
/// * missing / non-positive interval: 30 min;
/// * `min interval` is kept within 60 s ..= 24 h (non-positive = absent);
/// * interval < 5 min is raised to 5 min unless a `min interval` is present,
///   in which case `max(interval, min interval)` is used;
/// * interval is never below `min interval` and never above 24 h.
///
/// Idempotent: clamping an already clamped [`Timing`] changes nothing.
pub fn clamp_timing(interval: Option<i64>, min_interval: Option<i64>) -> Timing {
    let min = min_interval.filter(|&m| m > 0).map(|m| {
        Duration::from_secs((m as u64).clamp(MIN_INTERVAL_MIN.as_secs(), MAX_INTERVAL.as_secs()))
    });
    let mut iv = match interval {
        Some(i) if i > 0 => Duration::from_secs((i as u64).min(MAX_INTERVAL.as_secs())),
        _ => DEFAULT_INTERVAL,
    };
    match min {
        None => iv = iv.max(MIN_INTERVAL_FLOOR),
        Some(m) => iv = iv.max(m),
    }
    Timing {
        interval: iv,
        min_interval: min,
    }
}

impl Timing {
    /// Re-clamp (defence in depth for values that did not come from the parser).
    pub fn sanitized(self) -> Timing {
        let secs = |d: Duration| i64::try_from(d.as_secs()).unwrap_or(i64::MAX);
        clamp_timing(Some(secs(self.interval)), self.min_interval.map(secs))
    }
}

/// A successfully parsed announce response.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AnnounceResponse {
    /// Effective interval after clamping.
    pub interval: Duration,
    /// Effective min interval after clamping.
    pub min_interval: Option<Duration>,
    /// `tracker id` to echo in later announces.
    pub tracker_id: Option<Box<[u8]>>,
    /// Seeders (`complete`).
    pub complete: Option<u64>,
    /// Leechers (`incomplete`).
    pub incomplete: Option<u64>,
    /// Completed downloads (`downloaded`), if reported.
    pub downloaded: Option<u64>,
    /// Peers, capped at [`MAX_PEERS`].
    pub peers: Vec<Peer>,
    /// `warning message`, sanitised, to show to the user.
    pub warning: Option<String>,
    /// BEP 24 external IP.
    pub external_ip: Option<IpAddr>,
}

impl AnnounceResponse {
    /// The clamped timing pair.
    pub fn timing(&self) -> Timing {
        Timing {
            interval: self.interval,
            min_interval: self.min_interval,
        }
    }
}

/// BEP 31 `retry in`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetryIn {
    /// "never": do not retry automatically.
    Never,
    /// Retry after this many minutes (capped at 24 h).
    Minutes(u32),
}

/// Transport-level failure classes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportError {
    /// The request was (or may have been) written and no answer came in time.
    /// *Ambiguous*: the tracker may have received the announce.
    Timeout,
    /// Name resolution failed. The request was never written.
    Dns,
    /// TCP/UDP connect failed or was refused. The request was never written.
    Connect,
    /// TCP connect or the UDP connection-id handshake timed out. The announce
    /// itself was never written.
    ConnectTimeout,
    /// TLS handshake or certificate failure (never downgraded). The request
    /// was never written.
    Tls,
    /// The destination address is not allowed (loopback, link-local, cloud
    /// metadata, a private network without `allow_lan_trackers`, a public one in
    /// an isolated test run), and why. Nothing was sent (SSRF protection, see
    /// [`crate::policy`]).
    Forbidden(crate::policy::Refusal),
    /// The request was never put on the wire for a local reason (the durable
    /// write it depends on failed). Nothing was sent.
    NotSent,
    /// I/O error after connecting (reset, truncated). *Ambiguous*.
    Io,
    /// Too many redirects, or a redirect we refuse (other host, downgrade).
    Redirect,
    /// Response body over the size cap.
    TooLarge,
}

/// Why a body could not be understood.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseError {
    /// Not valid bencode (or empty).
    NotBencode,
    /// Valid bencode but not a dictionary.
    NotDict,
    /// Over [`MAX_BODY`].
    TooLarge,
}

/// Everything that can go wrong with one announce.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AnnounceError {
    /// The tracker answered with `failure reason` (or a UDP error packet).
    Rejected {
        /// Sanitised reason, shown verbatim.
        reason: String,
        /// BEP 31 `retry in`, if any.
        retry_in: Option<RetryIn>,
    },
    /// The request itself failed.
    Transport(TransportError),
    /// Non-2xx HTTP status.
    Http {
        /// Status code.
        status: u16,
        /// Parsed `Retry-After`, if any.
        retry_after: Option<Duration>,
    },
    /// 2xx body we could not parse.
    Garbage(ParseError),
}

/// How the scheduler reacts to an error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailureClass {
    /// Halt announcing for this (torrent, tracker) until the user acts. Only
    /// for `failure reason`s that look permanent (see [`is_permanent_reason`]).
    Halt,
    /// Any other `failure reason` (maintenance, "try later", ...): retry on the
    /// slow schedule [`SLOW_RETRY_SCHEDULE`], never give up.
    Slow,
    /// Exponential backoff, optionally lengthened by a server hint.
    Backoff {
        /// Minimum wait demanded by the server (Retry-After / retry in).
        hint: Option<Duration>,
    },
}

impl AnnounceError {
    /// Scheduler classification (SPEC 7C.4).
    pub fn class(&self) -> FailureClass {
        match self {
            AnnounceError::Rejected {
                retry_in: Some(RetryIn::Minutes(m)),
                ..
            } => FailureClass::Backoff {
                hint: Some(Duration::from_secs(u64::from(*m) * 60)),
            },
            // BEP 31 "retry in: never" is the tracker saying it is permanent.
            AnnounceError::Rejected {
                retry_in: Some(RetryIn::Never),
                ..
            } => FailureClass::Halt,
            AnnounceError::Rejected { reason, .. } => {
                if is_permanent_reason(reason) {
                    FailureClass::Halt
                } else {
                    FailureClass::Slow
                }
            }
            // 429/503 demand at least the server's time; other statuses rarely
            // carry the header, but it is honoured whenever present.
            AnnounceError::Http { retry_after, .. } => FailureClass::Backoff {
                hint: retry_after.map(|d| d.min(MAX_INTERVAL)),
            },
            AnnounceError::Transport(_) | AnnounceError::Garbage(_) => {
                FailureClass::Backoff { hint: None }
            }
        }
    }

    /// True when the failure suggests our own connectivity (DNS, connect,
    /// timeout) rather than the tracker refusing us. A network change may
    /// shorten the wait for these.
    pub fn is_network(&self) -> bool {
        matches!(
            self,
            AnnounceError::Transport(
                TransportError::Timeout
                    | TransportError::ConnectTimeout
                    | TransportError::Dns
                    | TransportError::Connect
                    | TransportError::Io
            )
        )
    }

    /// True when the failure says the tracker's host is in trouble (a 5xx, a timeout, a refused or
    /// reset connection), so another torrent's success on the same host is good news for this one.
    /// A `failure reason` or a 4xx is about the torrent and never counts.
    pub fn is_host_level(&self) -> bool {
        self.is_network() || matches!(self, AnnounceError::Http { status, .. } if *status >= 500)
    }

    /// True when the request could not even reach the tracker's network: DNS failure, connect
    /// refused or unreachable, connect timeout. This is the evidence the outage detector counts
    /// (SPEC 7C.4c); a slow or resetting tracker (`Timeout`, `Io`) and HTTP errors are not.
    pub fn is_unreachable(&self) -> bool {
        matches!(
            self,
            AnnounceError::Transport(
                TransportError::Dns | TransportError::Connect | TransportError::ConnectTimeout
            )
        )
    }

    /// True when a server answered, whatever it said (a `failure reason`, an HTTP status, a
    /// body we could not parse): the network works.
    pub fn answered(&self) -> bool {
        match self {
            AnnounceError::Rejected { .. } | AnnounceError::Http { .. } | AnnounceError::Garbage(_) => true,
            AnnounceError::Transport(t) => matches!(
                t,
                TransportError::Tls | TransportError::Redirect | TransportError::TooLarge
            ),
        }
    }

    /// True when the request *provably never reached the tracker*: DNS failure,
    /// connect refused/failed/timed out, TLS handshake failure (all before the
    /// request is written), a refusal by the destination policy, or a local
    /// refusal. A `completed` whose answer is one of these is re-sent with the
    /// next announce; every other failure is ambiguous and is not (the tracker
    /// may have counted it). Transports must only return these variants when
    /// nothing was written.
    pub fn never_reached_tracker(&self) -> bool {
        matches!(
            self,
            AnnounceError::Transport(
                TransportError::Dns
                    | TransportError::Connect
                    | TransportError::ConnectTimeout
                    | TransportError::Tls
                    | TransportError::Forbidden(_)
                    | TransportError::NotSent
            )
        )
    }

    /// True when this says something about the *host's* health (feeds the
    /// circuit breaker). A tracker-level rejection means the host is alive.
    pub fn is_host_failure(&self) -> bool {
        !matches!(
            self,
            AnnounceError::Rejected { .. }
                | AnnounceError::Transport(TransportError::Forbidden(_) | TransportError::NotSent)
        )
    }
}

/// Substrings (matched case-insensitively) of a `failure reason` that look
/// *permanent*: the tracker does not know the torrent, or it has banned us.
/// Only these halt announcing until the user acts (SPEC 7B.3). Anything else
/// (maintenance, "try again later", "database error", "too many requests" ...)
/// is retried on the slow schedule: seeding must never silently stop for
/// years because of a maintenance message.
///
/// The table is deliberately short and conservative. A wrongly *halted* torrent
/// costs the user a click; a wrongly *retried* one costs one request per
/// 30 min to 24 h, which no tracker minds.
pub const PERMANENT_REASONS: [&str; 16] = [
    "unregistered torrent",
    "torrent not registered",
    "not registered",
    "torrent not found",
    "not found",
    "unknown torrent",
    "info hash not found",
    "banned client",
    "client is banned",
    "client banned",
    "banned",
    "invalid passkey",
    "passkey invalid",
    "passkey is invalid",
    "bad passkey",
    "unknown passkey",
];

/// Does this `failure reason` look permanent? See [`PERMANENT_REASONS`].
pub fn is_permanent_reason(reason: &str) -> bool {
    let lower = reason.to_ascii_lowercase();
    PERMANENT_REASONS.iter().any(|p| lower.contains(p))
}

/// Retry delays after the 1st, 2nd, ... non-permanent `failure reason`:
/// 30 min, 1 h, 2 h, 4 h, 8 h, then every 24 h (the last entry repeats).
/// Jitter of 0-20 % is added on top and never subtracted.
pub const SLOW_RETRY_SCHEDULE: [Duration; 6] = [
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
    Duration::from_secs(2 * 60 * 60),
    Duration::from_secs(4 * 60 * 60),
    Duration::from_secs(8 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];

impl fmt::Display for AnnounceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnnounceError::Rejected { reason, .. } => {
                write!(f, "tracker rejected the announce: {reason}")
            }
            AnnounceError::Transport(TransportError::Forbidden(r)) => write!(f, "tracker address not allowed: {r}"),
            AnnounceError::Transport(e) => write!(f, "tracker request failed: {e:?}"),
            AnnounceError::Http { status, .. } => write!(f, "tracker returned HTTP {status}"),
            AnnounceError::Garbage(e) => write!(f, "tracker sent an unusable response: {e:?}"),
        }
    }
}
impl std::error::Error for AnnounceError {}

/// Lossy UTF-8, control characters replaced by spaces, truncated.
pub fn sanitize_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_TEXT_CHARS)
        .collect()
}

fn uint(v: Option<&Value<'_>>) -> Option<u64> {
    v.and_then(Value::as_int)
        .and_then(|i| u64::try_from(i).ok())
}

/// Append IPv4 compact peers (6 bytes each); trailing partial entries are ignored.
pub fn push_compact_v4(out: &mut Vec<Peer>, data: &[u8]) {
    for chunk in data.as_chunks::<6>().0 {
        if out.len() >= MAX_PEERS {
            return;
        }
        let ip = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        if port != 0 && !ip.is_unspecified() {
            out.push(Peer {
                addr: SocketAddr::new(IpAddr::V4(ip), port),
                peer_id: None,
            });
        }
    }
}

/// Append IPv6 compact peers (18 bytes each).
pub fn push_compact_v6(out: &mut Vec<Peer>, data: &[u8]) {
    for chunk in data.as_chunks::<18>().0 {
        if out.len() >= MAX_PEERS {
            return;
        }
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&chunk[..16]);
        let ip = Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        if port != 0 && !ip.is_unspecified() {
            out.push(Peer {
                addr: SocketAddr::new(IpAddr::V6(ip), port),
                peer_id: None,
            });
        }
    }
}

fn push_dict_peers(out: &mut Vec<Peer>, list: &[Value<'_>]) {
    for entry in list {
        if out.len() >= MAX_PEERS {
            return;
        }
        let Some(ip) = entry
            .get(b"ip")
            .and_then(Value::as_bytes)
            .and_then(|b| std::str::from_utf8(b).ok())
        else {
            continue;
        };
        let Ok(ip) = ip.trim().parse::<IpAddr>() else {
            continue;
        };
        let Some(port) = entry
            .get(b"port")
            .and_then(Value::as_int)
            .and_then(|p| u16::try_from(p).ok())
        else {
            continue;
        };
        if port == 0 || ip.is_unspecified() {
            continue;
        }
        let peer_id = entry
            .get(b"peer id")
            .and_then(Value::as_bytes)
            .and_then(|b| PeerId20::try_from(b).ok());
        out.push(Peer {
            addr: SocketAddr::new(ip, port),
            peer_id,
        });
    }
}

fn parse_external_ip(b: &[u8]) -> Option<IpAddr> {
    match b.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(b[0], b[1], b[2], b[3]))),
        16 => {
            let mut o = [0u8; 16];
            o.copy_from_slice(b);
            Some(IpAddr::V6(Ipv6Addr::from(o)))
        }
        _ => std::str::from_utf8(b).ok()?.trim().parse().ok(),
    }
}

fn parse_retry_in(v: &Value<'_>) -> Option<RetryIn> {
    match v {
        Value::Int(i) if *i > 0 => Some(RetryIn::Minutes(
            u32::try_from((*i).min(1440)).unwrap_or(1440),
        )),
        Value::Bytes(b) if b.eq_ignore_ascii_case(b"never") => Some(RetryIn::Never),
        Value::Bytes(b) => {
            let n: i64 = std::str::from_utf8(b).ok()?.trim().parse().ok()?;
            (n > 0).then(|| RetryIn::Minutes(u32::try_from(n.min(1440)).unwrap_or(1440)))
        }
        _ => None,
    }
}

/// Parse an HTTP announce response body.
///
/// Strict about the bencode framing (via `trs-bencode`), defensive about
/// content: unknown keys are ignored, bad optional values are dropped, trailing
/// bytes after the top-level dictionary are tolerated, and timing is clamped.
pub fn parse_announce_response(body: &[u8]) -> Result<AnnounceResponse, AnnounceError> {
    if body.len() > MAX_BODY {
        return Err(AnnounceError::Garbage(ParseError::TooLarge));
    }
    let limits = DecodeLimits {
        max_items: 100_000,
        max_bytes: MAX_BODY,
    };
    let (value, _rest) = trs_bencode::decode_prefix_with_limits(body, limits)
        .map_err(|_| AnnounceError::Garbage(ParseError::NotBencode))?;
    if value.as_dict().is_none() {
        return Err(AnnounceError::Garbage(ParseError::NotDict));
    }
    if let Some(reason) = value.get(b"failure reason").and_then(Value::as_bytes) {
        let retry_in = value.get(b"retry in").and_then(parse_retry_in);
        return Err(AnnounceError::Rejected {
            reason: sanitize_text(reason),
            retry_in,
        });
    }
    let timing = clamp_timing(
        value.get(b"interval").and_then(Value::as_int),
        value.get(b"min interval").and_then(Value::as_int),
    );
    let mut peers = Vec::new();
    match value.get(b"peers") {
        Some(Value::Bytes(b)) => push_compact_v4(&mut peers, b),
        Some(Value::List(l)) => push_dict_peers(&mut peers, l),
        _ => {}
    }
    if let Some(Value::Bytes(b)) = value.get(b"peers6") {
        push_compact_v6(&mut peers, b);
    }
    let tracker_id = value
        .get(b"tracker id")
        .and_then(Value::as_bytes)
        .filter(|b| !b.is_empty() && b.len() <= MAX_TRACKER_ID)
        .map(Box::<[u8]>::from);
    Ok(AnnounceResponse {
        interval: timing.interval,
        min_interval: timing.min_interval,
        tracker_id,
        complete: uint(value.get(b"complete")),
        incomplete: uint(value.get(b"incomplete")),
        downloaded: uint(value.get(b"downloaded")),
        peers,
        warning: value
            .get(b"warning message")
            .and_then(Value::as_bytes)
            .map(sanitize_text)
            .filter(|s| !s.trim().is_empty()),
        external_ip: value
            .get(b"external ip")
            .and_then(Value::as_bytes)
            .and_then(parse_external_ip),
    })
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_refused_address_says_why_not_forbidden() {
        use crate::policy::Refusal;
        let e = AnnounceError::Transport(TransportError::Forbidden(Refusal::Public));
        assert_eq!(e.to_string(), "tracker address not allowed: public address (an isolated test run reaches only local trackers)");
        assert!(e.never_reached_tracker() && !e.is_host_failure());
    }

    use super::*;

    fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    /// (interval, min interval) -> (effective interval, effective min), seconds
    type Case = (Option<i64>, Option<i64>, u64, Option<u64>);

    #[test]
    fn clamp_table() {
        let cases: &[Case] = &[
            (None, None, 1800, None),
            (Some(0), None, 1800, None),
            (Some(-5), None, 1800, None),
            (Some(i64::MIN), None, 1800, None),
            (Some(1), None, 300, None),
            (Some(299), None, 300, None),
            (Some(300), None, 300, None),
            (Some(1800), None, 1800, None),
            (Some(86_400), None, 86_400, None),
            (Some(86_401), None, 86_400, None),
            (Some(i64::MAX), None, 86_400, None),
            // min interval present: interval below 5 min is respected down to min (60 s floor)
            (Some(120), Some(60), 120, Some(60)),
            (Some(30), Some(10), 60, Some(60)),
            (Some(30), Some(90), 90, Some(90)),
            (Some(100), Some(1000), 1000, Some(1000)),
            (Some(1800), Some(900), 1800, Some(900)),
            (Some(600), Some(1800), 1800, Some(1800)),
            (None, Some(120), 1800, Some(120)),
            (None, Some(7200), 7200, Some(7200)),
            (Some(1800), Some(0), 1800, None),
            (Some(1800), Some(-1), 1800, None),
            (Some(1800), Some(i64::MAX), 86_400, Some(86_400)),
        ];
        for &(i, m, ei, em) in cases {
            let t = clamp_timing(i, m);
            assert_eq!(
                (t.interval, t.min_interval),
                (s(ei), em.map(s)),
                "input ({i:?},{m:?})"
            );
            assert_eq!(t.sanitized(), t, "idempotent for ({i:?},{m:?})");
            assert!(t.interval >= t.min_interval.unwrap_or_default());
            assert!(t.interval <= MAX_INTERVAL);
        }
    }

    fn bencode_dict(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut e: Vec<_> = entries.to_vec();
        e.sort_by(|a, b| a.0.cmp(b.0));
        let mut out = b"d".to_vec();
        for (k, v) in e {
            out.extend(format!("{}:{}", k.len(), k).as_bytes());
            out.extend(v);
        }
        out.push(b'e');
        out
    }
    fn int(i: i64) -> Vec<u8> {
        format!("i{i}e").into_bytes()
    }
    fn bytes(b: &[u8]) -> Vec<u8> {
        let mut v = format!("{}:", b.len()).into_bytes();
        v.extend(b);
        v
    }

    #[test]
    fn full_response_every_key() {
        let mut peers = Vec::new();
        peers.extend([10, 0, 0, 1, 0x1A, 0xE1]);
        peers.extend([192, 168, 1, 2, 0xC8, 0xD5]);
        let mut peers6 = Vec::new();
        peers6.extend("2001:db8::5".parse::<Ipv6Addr>().unwrap().octets());
        peers6.extend([0x1A, 0xE1]);
        let body = bencode_dict(&[
            ("interval", int(1800)),
            ("min interval", int(900)),
            ("tracker id", bytes(b"tid-1")),
            ("complete", int(12)),
            ("incomplete", int(34)),
            ("downloaded", int(56)),
            ("peers", bytes(&peers)),
            ("peers6", bytes(&peers6)),
            ("warning message", bytes(b"be nice")),
            ("external ip", bytes(&[203, 0, 113, 9])),
            ("unknown key", int(1)),
        ]);
        let r = parse_announce_response(&body).unwrap();
        assert_eq!(r.interval, s(1800));
        assert_eq!(r.min_interval, Some(s(900)));
        assert_eq!(r.tracker_id.as_deref(), Some(&b"tid-1"[..]));
        assert_eq!(
            (r.complete, r.incomplete, r.downloaded),
            (Some(12), Some(34), Some(56))
        );
        assert_eq!(r.peers.len(), 3);
        assert_eq!(r.peers[0].addr, "10.0.0.1:6881".parse().unwrap());
        assert_eq!(r.peers[1].addr, "192.168.1.2:51413".parse().unwrap());
        assert_eq!(r.peers[2].addr, "[2001:db8::5]:6881".parse().unwrap());
        assert_eq!(r.warning.as_deref(), Some("be nice"));
        assert_eq!(r.external_ip, Some("203.0.113.9".parse().unwrap()));
    }

    #[test]
    fn dict_peers_and_bad_entries() {
        let mut peer1 = bencode_dict(&[
            ("ip", bytes(b"1.2.3.4")),
            ("port", int(6881)),
            ("peer id", bytes(&[7; 20])),
        ]);
        let peer2 = bencode_dict(&[("ip", bytes(b"2001:db8::1")), ("port", int(80))]);
        let bad_port = bencode_dict(&[("ip", bytes(b"1.2.3.5")), ("port", int(70000))]);
        let hostname = bencode_dict(&[("ip", bytes(b"example.com")), ("port", int(80))]);
        let zero = bencode_dict(&[("ip", bytes(b"0.0.0.0")), ("port", int(80))]);
        let port0 = bencode_dict(&[("ip", bytes(b"1.2.3.6")), ("port", int(0))]);
        let mut list = b"l".to_vec();
        for p in [
            &mut peer1,
            &mut peer2.clone(),
            &mut bad_port.clone(),
            &mut hostname.clone(),
            &mut zero.clone(),
            &mut port0.clone(),
        ] {
            list.extend(p.iter());
        }
        list.extend(int(5));
        list.push(b'e');
        let body = bencode_dict(&[("interval", int(1800)), ("peers", list)]);
        let r = parse_announce_response(&body).unwrap();
        assert_eq!(r.peers.len(), 2);
        assert_eq!(r.peers[0].peer_id, Some([7; 20]));
        assert_eq!(r.peers[1].peer_id, None);
    }

    #[test]
    fn failure_reason_and_retry_in() {
        let body = bencode_dict(&[("failure reason", bytes(b"torrent \x07 not registered"))]);
        match parse_announce_response(&body) {
            Err(AnnounceError::Rejected {
                reason,
                retry_in: None,
            }) => assert_eq!(reason, "torrent   not registered"),
            other => panic!("{other:?}"),
        }
        let body = bencode_dict(&[("failure reason", bytes(b"later")), ("retry in", int(10))]);
        assert!(matches!(
            parse_announce_response(&body),
            Err(AnnounceError::Rejected {
                retry_in: Some(RetryIn::Minutes(10)),
                ..
            })
        ));
        let body = bencode_dict(&[
            ("failure reason", bytes(b"no")),
            ("retry in", bytes(b"never")),
        ]);
        assert!(matches!(
            parse_announce_response(&body),
            Err(AnnounceError::Rejected {
                retry_in: Some(RetryIn::Never),
                ..
            })
        ));
        let body = bencode_dict(&[
            ("failure reason", bytes(b"no")),
            ("retry in", int(10_000_000)),
        ]);
        assert!(matches!(
            parse_announce_response(&body),
            Err(AnnounceError::Rejected {
                retry_in: Some(RetryIn::Minutes(1440)),
                ..
            })
        ));
        let body = bencode_dict(&[("failure reason", bytes(b"no")), ("retry in", int(-3))]);
        assert!(matches!(
            parse_announce_response(&body),
            Err(AnnounceError::Rejected { retry_in: None, .. })
        ));
        // Failure reason wins over everything else in the dict.
        let body = bencode_dict(&[
            ("failure reason", bytes(b"no")),
            ("interval", int(60)),
            ("peers", bytes(&[1, 1, 1, 1, 0, 80])),
        ]);
        assert!(parse_announce_response(&body).is_err());
    }

    #[test]
    fn garbage_never_panics() {
        for g in [
            &b""[..],
            b"hello",
            b"<html>503</html>",
            b"d",
            b"de",
            b"i5e",
            b"le",
            b"d8:intervali",
            b"d8:intervali1800e",
            b"\x00\x01\x02",
            b"d1:ai1e1:ai2ee",
            b"li1ee",
            b"0:",
        ] {
            let r = parse_announce_response(g);
            if g == b"de" {
                // empty dict: a success with defaults
                let r = r.unwrap();
                assert_eq!(r.interval, DEFAULT_INTERVAL);
            } else {
                assert!(
                    matches!(r, Err(AnnounceError::Garbage(_))),
                    "{g:?} -> {r:?}"
                );
            }
        }
        assert!(matches!(
            parse_announce_response(&vec![b'x'; MAX_BODY + 1]),
            Err(AnnounceError::Garbage(ParseError::TooLarge))
        ));
        let deep = [vec![b'l'; 100_000], vec![b'e'; 100_000]].concat();
        assert!(parse_announce_response(&deep).is_err());
    }

    #[test]
    fn weird_values_are_handled() {
        for (iv, expect) in [
            (0i64, 1800u64),
            (-1, 1800),
            (1, 300),
            (99_999_999_999, 86_400),
        ] {
            let r = parse_announce_response(&bencode_dict(&[("interval", int(iv))])).unwrap();
            assert_eq!(r.interval, s(expect), "interval {iv}");
        }
        // interval as a string => invalid => default
        let r = parse_announce_response(&bencode_dict(&[("interval", bytes(b"900"))])).unwrap();
        assert_eq!(r.interval, DEFAULT_INTERVAL);
        // negative counters are dropped
        let r = parse_announce_response(&bencode_dict(&[
            ("complete", int(-5)),
            ("incomplete", bytes(b"x")),
        ]))
        .unwrap();
        assert_eq!((r.complete, r.incomplete), (None, None));
        // trailing newline tolerated
        let mut body = bencode_dict(&[("interval", int(900))]);
        body.extend(b"\r\n");
        assert_eq!(parse_announce_response(&body).unwrap().interval, s(900));
        // oversized tracker id dropped; textual external ip accepted; odd-length ip dropped
        let r = parse_announce_response(&bencode_dict(&[
            ("tracker id", bytes(&[b'a'; MAX_TRACKER_ID + 1])),
            ("external ip", bytes(b"198.51.100.7")),
        ]))
        .unwrap();
        assert_eq!(r.tracker_id, None);
        assert_eq!(r.external_ip, Some("198.51.100.7".parse().unwrap()));
        let r =
            parse_announce_response(&bencode_dict(&[("external ip", bytes(&[1, 2, 3]))])).unwrap();
        assert_eq!(r.external_ip, None);
    }

    #[test]
    fn peers_are_capped_and_partial_entries_ignored() {
        let mut compact = Vec::new();
        for i in 0..1000u32 {
            compact.extend([10, (i >> 8) as u8, i as u8, 1, 0x1A, 0xE1]);
        }
        compact.extend([1, 2, 3]); // partial entry
        let r = parse_announce_response(&bencode_dict(&[("peers", bytes(&compact))])).unwrap();
        assert_eq!(r.peers.len(), MAX_PEERS);
        // v4 + v6 share the cap
        let mut v6 = Vec::new();
        for _ in 0..500 {
            v6.extend([
                0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0x1A, 0xE1,
            ]);
        }
        let r = parse_announce_response(&bencode_dict(&[
            ("peers", bytes(&compact)),
            ("peers6", bytes(&v6)),
        ]))
        .unwrap();
        assert_eq!(r.peers.len(), MAX_PEERS);
    }

    #[test]
    fn error_classification() {
        let rej = AnnounceError::Rejected {
            reason: "Unregistered torrent".into(),
            retry_in: None,
        };
        assert_eq!(rej.class(), FailureClass::Halt);
        assert!(!rej.is_host_failure());
        // any other failure reason is retried on the slow schedule
        let maint = AnnounceError::Rejected {
            reason: "Tracker is down for maintenance".into(),
            retry_in: None,
        };
        assert_eq!(maint.class(), FailureClass::Slow);
        assert!(!maint.is_host_failure());
        // BEP 31 "retry in: never" is permanent whatever the text says
        let never = AnnounceError::Rejected {
            reason: "x".into(),
            retry_in: Some(RetryIn::Never),
        };
        assert_eq!(never.class(), FailureClass::Halt);
        let e = AnnounceError::Http {
            status: 503,
            retry_after: Some(s(10 * 86_400)),
        };
        assert_eq!(
            e.class(),
            FailureClass::Backoff {
                hint: Some(MAX_INTERVAL)
            }
        );
        assert!(e.is_host_failure() && !e.is_network());
        assert!(AnnounceError::Transport(TransportError::Dns).is_network());
        assert!(!AnnounceError::Transport(TransportError::Tls).is_network());
    }
}
