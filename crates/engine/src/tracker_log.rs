//! Developer mode: turning tracker exchanges and hub decisions into `trs_devlog` entries.
//!
//! Everything here runs only while the log is on (`DevLog::enabled`). The entries carry the raw
//! tracker URL; `DevLog::log` masks every secret at the logging boundary.

use std::net::IpAddr;
use std::time::Duration;

use trs_devlog::{Entry, Kind, Next, Request, Response};
use trs_tracker::gate::GatedRequest;
use trs_tracker::http::{HttpTrace, HttpTransport};
use trs_tracker::hub::{HubNote, NoteKind};
use trs_tracker::response::{AnnounceError, AnnounceResponse, RetryIn, TransportError};
use trs_tracker::udp::UdpTrace;
use trs_tracker::url::{Scheme, build_announce_url};
use trs_tracker::{AnnounceParams, Crypto, Event};

/// What the transport learned about the wire.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Wire {
    /// Nothing (abandoned before it finished).
    None,
    Http(HttpTrace),
    Udp(UdpTrace),
}

/// An exchange as the task that ran it saw it; the driver adds the title and the schedule.
pub(crate) struct Finished {
    pub entry: Entry,
    /// Interval and min interval of a successful answer, seconds.
    pub timing: Option<(u64, Option<u64>)>,
    /// The error, for the backoff explanation.
    pub error: Option<AnnounceError>,
}

pub(crate) fn hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(D[usize::from(x >> 4)] as char);
        s.push(D[usize::from(x & 15)] as char);
    }
    s
}

fn printable(b: &[u8]) -> String {
    b.iter().map(|&c| if (0x20..0x7f).contains(&c) { c as char } else { '.' }).collect()
}

fn transport_name(s: Scheme) -> &'static str {
    match s {
        Scheme::Http => "http",
        Scheme::Https => "https",
        Scheme::Udp => "udp",
    }
}

pub(crate) fn event_name(e: Option<Event>) -> &'static str {
    e.map_or("regular", Event::as_str)
}

/// Words for a failure.
pub(crate) fn error_text(e: &AnnounceError) -> String {
    match e {
        AnnounceError::Rejected { reason, retry_in } => {
            let r = match retry_in {
                Some(RetryIn::Never) => " (retry in: never)".to_string(),
                Some(RetryIn::Minutes(m)) => format!(" (retry in {m} min)"),
                None => String::new(),
            };
            format!("tracker failure reason: {reason}{r}")
        }
        AnnounceError::Transport(t) => match t {
            TransportError::Timeout => "timeout: no answer in time (the tracker may have received it)".into(),
            TransportError::Dns => "DNS lookup failed".into(),
            TransportError::Connect => "connection failed or refused".into(),
            TransportError::ConnectTimeout => "connect timed out".into(),
            TransportError::Tls => "TLS handshake or certificate failure".into(),
            TransportError::Forbidden => "destination not allowed by the network policy".into(),
            TransportError::NotSent => "not sent (a local write failed)".into(),
            TransportError::Io => "connection reset or I/O error".into(),
            TransportError::Redirect => "too many or refused redirects".into(),
            TransportError::TooLarge => "answer too large".into(),
        },
        AnnounceError::Http { status, retry_after } => match retry_after {
            Some(d) => format!("HTTP status {status} (Retry-After {} s)", d.as_secs()),
            None => format!("HTTP status {status}"),
        },
        AnnounceError::Garbage(p) => format!("unreadable answer ({p:?})"),
    }
}

fn response_fields(r: &AnnounceResponse) -> Vec<(String, String)> {
    let mut f = vec![("interval".to_string(), r.interval.as_secs().to_string())];
    if let Some(m) = r.min_interval {
        f.push(("min_interval".into(), m.as_secs().to_string()));
    }
    if let Some(c) = r.complete {
        f.push(("complete".into(), c.to_string()));
    }
    if let Some(i) = r.incomplete {
        f.push(("incomplete".into(), i.to_string()));
    }
    if let Some(d) = r.downloaded {
        f.push(("downloaded".into(), d.to_string()));
    }
    let v6 = r.peers.iter().filter(|p| p.addr.is_ipv6()).count();
    f.push(("peers".into(), r.peers.len().to_string()));
    if v6 > 0 {
        f.push(("peers_ipv6".into(), v6.to_string()));
    }
    if let Some(w) = &r.warning {
        f.push(("warning".into(), w.clone()));
    }
    if let Some(t) = &r.tracker_id {
        f.push(("tracker_id".into(), printable(t)));
    }
    if let Some(ip) = r.external_ip {
        f.push(("external_ip".into(), ip.to_string()));
    }
    f
}

fn params_fields(p: &AnnounceParams) -> Vec<(String, String)> {
    let mut f = vec![
        ("info_hash".to_string(), hex(&p.info_hash)),
        ("peer_id".into(), printable(&p.peer_id)),
        ("port".into(), p.port.to_string()),
        ("uploaded".into(), p.uploaded.to_string()),
        ("downloaded".into(), p.downloaded.to_string()),
        ("left".into(), p.left.to_string()),
        ("numwant".into(), p.numwant.to_string()),
        ("key".into(), format!("{:08X}", p.key)),
        ("compact".into(), "1".into()),
    ];
    if p.crypto != Crypto::Off {
        f.push(("supportcrypto".into(), "1".into()));
    }
    if p.crypto == Crypto::Require {
        f.push(("requirecrypto".into(), "1".into()));
    }
    // As on the wire: only when above zero, between the crypto flags and `event` (UDP has no field).
    if p.corrupt > 0 {
        f.push(("corrupt".into(), p.corrupt.to_string()));
    }
    if let Some(e) = p.event {
        f.push(("event".into(), e.as_str().into()));
    }
    if let Some(t) = &p.tracker_id {
        f.push(("trackerid".into(), printable(t)));
    }
    if let Some(a) = p.ipv4 {
        f.push(("ipv4".into(), a.to_string()));
    }
    if let Some(a) = p.ipv6 {
        f.push(("ipv6".into(), a.to_string()));
    }
    f
}

/// Describe a finished exchange. `result` is what the transport returned (`None` when the task
/// gave up waiting: `abandoned` says why).
pub(crate) fn finished(
    req: &GatedRequest,
    http: &HttpTransport,
    wire: Wire,
    result: Option<&Result<AnnounceResponse, AnnounceError>>,
    abandoned: Option<&'static str>,
    elapsed: Duration,
    ts_ms: u64,
) -> Finished {
    let url = req.url();
    let udp = url.scheme() == Scheme::Udp;
    let mut e = Entry {
        ts_ms,
        kind: Kind::Announce,
        tracker: url.raw().to_owned(),
        transport: transport_name(url.scheme()),
        event: event_name(req.event()),
        duration_ms: Some(elapsed.as_millis() as u64),
        ..Entry::default()
    };
    if let Some(p) = req.announce_params() {
        e.torrent = hex(&p.info_hash);
        let mut rq = Request::default();
        if udp {
            rq.url = url.raw().to_owned();
            rq.fields = vec![("connect.action".into(), "0 (connect)".into()), ("connect.protocol_id".into(), "0x41727101980".into())];
            if let Wire::Udp(t) = wire {
                rq.fields.push(("connect.used".into(), if t.connection_reused { "cached connection id, no new handshake".into() } else { "new handshake".into() }));
                rq.fields.push(("connection_id".into(), format!("{:#018x}", t.connection_id)));
                rq.fields.push(("announce.transaction_id".into(), format!("{:#010x}", t.transaction_id)));
            }
            rq.fields.push(("announce.action".into(), "1 (announce)".into()));
            let mut f = params_fields(p);
            f.retain(|(k, _)| !matches!(k.as_str(), "compact" | "supportcrypto" | "requirecrypto" | "corrupt" | "event" | "trackerid"));
            for (k, v) in f {
                let k = if k == "numwant" { "num_want".to_string() } else { k };
                rq.fields.push((format!("announce.{k}"), v));
            }
            rq.fields.push(("announce.event".into(), p.event.map_or("0 (none)".to_string(), |ev| format!("{} ({})", ev.udp_code(), ev.as_str()))));
            rq.fields.push(("announce.ip".into(), "0 (tracker uses the source address)".into()));
        } else {
            let full = build_announce_url(url, p);
            rq.headers = http.request_headers(&full);
            rq.url = full;
            rq.fields = params_fields(p);
        }
        e.request = Some(rq);
    } else {
        e.kind = Kind::Scrape;
        e.event = "scrape";
    }

    let mut timing = None;
    let mut error = None;
    let mut resp = Response::default();
    match (&wire, udp) {
        (Wire::Http(t), _) => {
            resp.status = t.status;
            e.fallback = t.fallback;
            set_addr(&mut e, t.addr);
        }
        (Wire::Udp(t), _) => {
            e.fallback = t.fallback;
            set_addr(&mut e, t.addr.map(|a| a.ip()));
        }
        (Wire::None, _) => {}
    }
    match result {
        Some(Ok(r)) => {
            resp.fields = response_fields(r);
            timing = Some((r.interval.as_secs(), r.min_interval.map(|m| m.as_secs())));
        }
        Some(Err(err)) => {
            resp.error = Some(error_text(err));
            match err {
                AnnounceError::Rejected { reason, retry_in } => {
                    resp.fields.push(("failure_reason".into(), reason.clone()));
                    match retry_in {
                        Some(RetryIn::Never) => resp.fields.push(("retry_in".into(), "never".into())),
                        Some(RetryIn::Minutes(m)) => resp.fields.push(("retry_in".into(), format!("{m} min"))),
                        None => {}
                    }
                }
                AnnounceError::Http { status, retry_after } => {
                    resp.status = Some(*status);
                    if let Some(d) = retry_after {
                        resp.fields.push(("retry_after".into(), format!("{} s", d.as_secs())));
                    }
                }
                _ => {}
            }
            error = Some(err.clone());
        }
        None => resp.error = Some(abandoned.unwrap_or("abandoned").to_owned()),
    }
    e.response = Some(resp);
    if let Some(a) = abandoned {
        e.note = Some(a.to_owned());
    }
    Finished { entry: e, timing, error }
}

fn set_addr(e: &mut Entry, ip: Option<IpAddr>) {
    if let Some(ip) = ip {
        let ip = ip.to_canonical();
        e.family = Some(if ip.is_ipv4() { "IPv4" } else { "IPv6" });
        e.ip = Some(ip.to_string());
    }
}

/// Why the next announce is when it is, after an answer.
pub(crate) fn schedule_why_ok(interval: u64, min: Option<u64>) -> String {
    match min {
        Some(m) if m > interval => format!("min interval {m} s is above interval {interval} s: the floor wins"),
        Some(m) => format!("interval {interval} s (min interval {m} s)"),
        None => format!("interval {interval} s"),
    }
}

/// Why the next attempt is when it is, after a failure.
pub(crate) fn schedule_why_err(e: &AnnounceError, failures: u32, pace: &str) -> String {
    let hint = match e {
        AnnounceError::Http { retry_after: Some(d), .. } => format!(", server asked Retry-After {} s", d.as_secs()),
        AnnounceError::Rejected { retry_in: Some(RetryIn::Minutes(m)), .. } => format!(", tracker said retry in {m} min"),
        _ => String::new(),
    };
    format!("backoff step {failures} after failure ({pace} pace){hint}")
}

/// A hub note as a log entry; `title` and the raw URL are filled by the caller.
pub(crate) fn note_entry(n: &HubNote, tracker: &str) -> Entry {
    let (kind, code) = match n.kind {
        NoteKind::Retire => (Kind::Decision, "retire"),
        NoteKind::RetireSilent => (Kind::Decision, "retire_silent"),
        NoteKind::Suspend => (Kind::Quit, "suspend"),
        NoteKind::SessionOpened => (Kind::Decision, "session_opened"),
        NoteKind::KeptSchedule => (Kind::Decision, "skipped"),
        NoteKind::RetryPulled => (Kind::Decision, "retry_moved"),
        NoteKind::Deferred => (Kind::Gate, "deferred"),
        NoteKind::Released => (Kind::Gate, "released"),
        NoteKind::Refused => (Kind::Gate, "refused"),
    };
    let url_transport = tracker.split("://").next().unwrap_or("");
    Entry {
        ts_ms: n.at.0,
        kind,
        torrent: hex(&n.torrent),
        tracker: tracker.to_owned(),
        transport: match url_transport {
            "http" => "http",
            "https" => "https",
            "udp" => "udp",
            _ => "",
        },
        decision: Some(code),
        note: Some(n.detail.clone()),
        next: n.next.map(|t| Next { at_ms: t.0, why: next_why(n.kind) }),
        ..Entry::default()
    }
}

fn next_why(k: NoteKind) -> String {
    match k {
        NoteKind::KeptSchedule => "regular announce time unchanged".into(),
        NoteKind::RetryPulled => "retry time after the network change".into(),
        NoteKind::SessionOpened => "first announce of the session".into(),
        _ => String::new(),
    }
}
