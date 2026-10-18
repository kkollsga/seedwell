//! `analyze` (human timeline + detector verdicts), `check` (verdicts only,
//! non-zero exit on violation) and the shared per-client profile model.

use crate::record::{EncodingInfo, Record, encoding_info, read_records};
use anyhow::Result;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

/// Arguments for `analyze`.
#[derive(clap::Args, Debug)]
pub struct AnalyzeArgs {
    /// Capture file(s) produced by `proxy` or `mock`.
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
    #[command(flatten)]
    pub opts: CheckOpts,
}

/// Arguments for `check`.
#[derive(clap::Args, Debug)]
pub struct CheckArgs {
    /// Capture file(s) produced by `proxy` or `mock`.
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
    /// Fail (exit 1) when the capture holds fewer announce records than this. A client that never
    /// reaches the tracker is "silent", not "well behaved": without this flag `check` passes an
    /// empty capture (default 0 = off).
    #[arg(long, default_value_t = 0)]
    pub expect_announces: usize,
    #[command(flatten)]
    pub opts: CheckOpts,
}

/// Detector thresholds.
#[derive(clap::Args, Debug, Clone)]
pub struct CheckOpts {
    /// Tolerance in ms before an early announce counts as a violation.
    #[arg(long, default_value_t = 2000)]
    pub tolerance_ms: u64,
    /// Maximum requests per 60 s sliding window per (client, tracker host).
    #[arg(long, default_value_t = 10)]
    pub max_per_minute: usize,
}

impl Default for CheckOpts {
    fn default() -> Self {
        Self { tolerance_ms: 2000, max_per_minute: 10 }
    }
}

/// Severity of a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Suspicious but not provably wrong.
    Warn,
    /// Protocol or politeness violation.
    Fail,
}

/// One detector finding.
#[derive(Debug, Clone)]
pub struct Finding {
    /// Severity.
    pub severity: Severity,
    /// Client label.
    pub client: String,
    /// Short info-hash (first 8 hex chars).
    pub torrent: String,
    /// Time (ms, Unix) of the offending request.
    pub ts_ms: u64,
    /// Rule id.
    pub rule: &'static str,
    /// Description.
    pub msg: String,
}

/// Client label for a record: explicit label, else the User-Agent product, else `unknown`.
pub fn client_of(r: &Record) -> String {
    if let Some(l) = r.label.as_ref().filter(|l| !l.is_empty()) {
        return l.clone();
    }
    r.header("user-agent").map_or_else(|| "unknown".to_owned(), |ua| ua.split_whitespace().next().unwrap_or(ua).to_owned())
}

/// Event name, with absent and `empty` normalised to `none`.
pub fn event_of(r: &Record) -> &str {
    match r.param("event") {
        None | Some("" | "empty") => "none",
        Some(e) => e,
    }
}

fn num(r: &Record, k: &str) -> Option<i64> {
    r.param(k).and_then(|v| v.parse().ok())
}

fn short(ih: &str) -> String {
    ih.chars().take(8).collect()
}

/// Announces of one (client, torrent, tracker), time-ordered.
pub struct Group<'a> {
    /// Client label.
    pub client: String,
    /// Hex info-hash.
    pub info_hash: String,
    /// Tracker mapping id.
    pub tracker: String,
    /// Announce records.
    pub recs: Vec<&'a Record>,
}

/// Group announce records by (client, info_hash, tracker id).
pub fn groups(all: &[Record]) -> Vec<Group<'_>> {
    let mut map: BTreeMap<(String, String, String), Vec<&Record>> = BTreeMap::new();
    for r in all.iter().filter(|r| r.kind == "announce") {
        map.entry((client_of(r), r.info_hash.clone().unwrap_or_default(), r.id.clone().unwrap_or_default())).or_default().push(r);
    }
    map.into_iter().map(|((client, info_hash, tracker), recs)| Group { client, info_hash, tracker, recs }).collect()
}

// ---------------------------------------------------------------- detector

/// Running state of the current session: uploaded, downloaded, left, key, peer_id.
type SessionState = (i64, i64, i64, Option<String>, Option<String>);

/// Run every detector over a capture.
pub fn detect(all: &[Record], opts: &CheckOpts) -> Vec<Finding> {
    let mut out = Vec::new();
    let tol = opts.tolerance_ms;
    for g in groups(all) {
        let ih = short(&g.info_hash);
        let mk = |sev, ts_ms, rule, msg: String| Finding { severity: sev, client: g.client.clone(), torrent: ih.clone(), ts_ms, rule, msg };
        let mut last_ok: Option<&Record> = None;
        let mut prev: Option<&Record> = None;
        let mut completed = 0usize;
        let mut session: Option<SessionState> = None;
        let mut spam_count = 0usize;
        let mut first_spam: Option<(u64, u64)> = None;
        for (i, r) in g.recs.iter().enumerate() {
            let ev = event_of(r);
            // R3: started first
            if i == 0 && ev != "started" {
                out.push(mk(Severity::Fail, r.ts_ms, "STARTED_FIRST", format!("first announce is event={ev}, not started")));
            }
            // R4: nothing after stopped except started
            if let Some(p) = prev {
                if event_of(p) == "stopped" && ev != "started" {
                    out.push(mk(Severity::Fail, r.ts_ms, "AFTER_STOPPED", format!("event={ev} sent after stopped without a new started")));
                }
            }
            if ev == "started" {
                if let Some(p) = prev.filter(|p| event_of(p) == "started") {
                    // A repeat after a *failed* started is a legitimate retry. A repeat after a
                    // successful one is the Transmission 4.0.6 pattern (a failure on the other
                    // address family overwrote the success): treat it like early announcing.
                    let gap = r.ts_ms.saturating_sub(p.ts_ms);
                    let required = p.response.as_ref().and_then(|x| x.interval.filter(|v| *v > 0).or(x.min_interval.filter(|v| *v > 0)));
                    let early = required.is_some_and(|req| gap + tol + p.latency_ms.unwrap_or(0) < u64::try_from(req).unwrap_or(0) * 1000);
                    if p.succeeded() && early {
                        spam_count += 1;
                        first_spam.get_or_insert((r.ts_ms, gap));
                        out.push(mk(
                            Severity::Fail,
                            r.ts_ms,
                            "REPEATED_STARTED",
                            format!("started re-sent {:.1}s after a successful started, without stopped in between", gap as f64 / 1000.0),
                        ));
                    } else if p.succeeded() {
                        out.push(mk(Severity::Warn, r.ts_ms, "REPEATED_STARTED", "started sent again without stopped in between".into()));
                    }
                }
            }
            // R1: spam / early re-announce
            if ev == "none" {
                if let Some(p) = last_ok {
                    let resp = p.response.as_ref();
                    let interval = resp.and_then(|x| x.interval).filter(|v| *v > 0);
                    let min = resp.and_then(|x| x.min_interval).filter(|v| *v > 0);
                    let required = min.or(interval);
                    let gap = r.ts_ms.saturating_sub(p.ts_ms);
                    if let Some(req) = required {
                        let req_ms = u64::try_from(req).unwrap_or(0) * 1000;
                        let slack = tol + p.latency_ms.unwrap_or(0);
                        if gap + slack < req_ms {
                            spam_count += 1;
                            first_spam.get_or_insert((r.ts_ms, gap));
                            out.push(mk(
                                Severity::Fail,
                                r.ts_ms,
                                "ANNOUNCE_SPAM",
                                format!(
                                    "regular announce {:.1}s after the previous successful response, tracker required >= {req}s ({})",
                                    gap as f64 / 1000.0,
                                    if min.is_some() { "min interval" } else { "interval" }
                                ),
                            ));
                        } else if let Some(iv) = interval {
                            let iv_ms = u64::try_from(iv).unwrap_or(0) * 1000;
                            if gap + slack < iv_ms {
                                out.push(mk(
                                    Severity::Warn,
                                    r.ts_ms,
                                    "EARLY_VS_INTERVAL",
                                    format!("regular announce {:.1}s after previous response; interval was {iv}s (min interval {} respected)", gap as f64 / 1000.0, min.unwrap_or(0)),
                                ));
                            }
                        }
                    }
                }
            }
            // R2: completed
            if ev == "completed" {
                completed += 1;
                if completed > 1 {
                    out.push(mk(Severity::Fail, r.ts_ms, "DUP_COMPLETED", format!("completed sent {completed} times for this torrent")));
                }
                if num(r, "left").is_some_and(|l| l > 0) {
                    out.push(mk(Severity::Warn, r.ts_ms, "COMPLETED_LEFT", "completed sent while left > 0".into()));
                }
            }
            // R5: counters within a session; key/peer_id stability
            let (up, down, left) = (num(r, "uploaded"), num(r, "downloaded"), num(r, "left"));
            if ev == "started" {
                session = Some((up.unwrap_or(0), down.unwrap_or(0), left.unwrap_or(0), r.param("key").map(str::to_owned), r.peer_id.clone()));
            } else if let Some(s) = session.as_mut() {
                if up.is_some_and(|u| u < s.0) {
                    out.push(mk(Severity::Fail, r.ts_ms, "COUNTER_DECREASE", format!("uploaded decreased {} -> {} within a session", s.0, up.unwrap_or(0))));
                }
                if down.is_some_and(|d| d < s.1) {
                    out.push(mk(Severity::Fail, r.ts_ms, "COUNTER_DECREASE", format!("downloaded decreased {} -> {} within a session", s.1, down.unwrap_or(0))));
                }
                if left.is_some_and(|l| l > s.2) {
                    out.push(mk(Severity::Warn, r.ts_ms, "LEFT_INCREASE", format!("left increased {} -> {} within a session", s.2, left.unwrap_or(0))));
                }
                s.0 = up.unwrap_or(s.0).max(s.0);
                s.1 = down.unwrap_or(s.1).max(s.1);
                s.2 = left.unwrap_or(s.2);
                if r.param("key").map(str::to_owned) != s.3 {
                    out.push(mk(Severity::Warn, r.ts_ms, "KEY_CHANGED", "key changed within a session".into()));
                    s.3 = r.param("key").map(str::to_owned);
                }
                if r.peer_id != s.4 {
                    out.push(mk(Severity::Warn, r.ts_ms, "PEER_ID_CHANGED", "peer_id changed within a session".into()));
                    s.4.clone_from(&r.peer_id);
                }
            }
            if r.succeeded() {
                last_ok = Some(r);
            }
            prev = Some(r);
        }
        if spam_count > 0 {
            if let Some((ts, gap)) = first_spam {
                out.push(mk(
                    Severity::Fail,
                    ts,
                    "ANNOUNCE_SPAM_SUMMARY",
                    format!("{spam_count} of {} announces were sent early (first gap {:.1}s)", g.recs.len(), gap as f64 / 1000.0),
                ));
            }
        }
    }
    // R6: per (client, tracker id) request rate across all torrents.
    let mut by_host: BTreeMap<(String, String), Vec<u64>> = BTreeMap::new();
    // Event announces (started/completed/stopped) are excluded: N torrents starting together is
    // legitimate and is bounded by the per-torrent rules; sustained regular traffic is the signal.
    for r in all.iter().filter(|r| (r.kind == "announce" && event_of(r) == "none") || r.kind == "scrape") {
        by_host.entry((client_of(r), r.id.clone().unwrap_or_default())).or_default().push(r.ts_ms);
    }
    for ((client, host), ts) in by_host {
        let mut lo = 0;
        let mut worst = (0usize, 0u64);
        for hi in 0..ts.len() {
            while ts[hi] - ts[lo] >= 60_000 {
                lo += 1;
            }
            if hi - lo + 1 > worst.0 {
                worst = (hi - lo + 1, ts[lo]);
            }
        }
        if worst.0 > opts.max_per_minute {
            out.push(Finding {
                severity: Severity::Fail,
                client,
                torrent: format!("host:{host}"),
                ts_ms: worst.1,
                rule: "HOST_RATE",
                msg: format!("{} requests within 60 s to tracker '{host}' (limit {})", worst.0, opts.max_per_minute),
            });
        }
    }
    out.sort_by_key(|f| (f.ts_ms, f.severity));
    out
}

/// Render verdict lines (PASS/FAIL per client) and findings.
pub fn verdicts(all: &[Record], findings: &[Finding], t0: u64) -> (String, bool) {
    let mut clients: Vec<String> = groups(all).into_iter().map(|g| g.client).collect();
    clients.sort();
    clients.dedup();
    let mut s = String::new();
    let mut any_fail = false;
    for c in &clients {
        let mine: Vec<&Finding> = findings.iter().filter(|f| &f.client == c).collect();
        let fails = mine.iter().filter(|f| f.severity == Severity::Fail).count();
        let warns = mine.len() - fails;
        any_fail |= fails > 0;
        let _ = writeln!(s, "{}  {c}   ({fails} violation(s), {warns} warning(s))", if fails > 0 { "FAIL" } else { "PASS" });
        // Show the first few findings of each rule, then a count: a spamming client produces hundreds.
        let mut shown: BTreeMap<&str, usize> = BTreeMap::new();
        let mut total: BTreeMap<&str, usize> = BTreeMap::new();
        for f in &mine {
            *total.entry(f.rule).or_default() += 1;
            let n = shown.entry(f.rule).or_default();
            if *n < 4 {
                *n += 1;
                let _ = writeln!(s, "    [{}] {} {} {}: {}", if f.severity == Severity::Fail { "FAIL" } else { "warn" }, fmt_t(f.ts_ms.saturating_sub(t0)), f.torrent, f.rule, f.msg);
            }
        }
        for (rule, n) in &total {
            if *n > 4 {
                let _ = writeln!(s, "    ... {rule}: {} more (total {n})", n - 4);
            }
        }
    }
    (s, any_fail)
}

// ---------------------------------------------------------------- profile

/// Format milliseconds as `+MM:SS.s`.
pub fn fmt_t(ms: u64) -> String {
    let s = ms as f64 / 1000.0;
    format!("+{:02}:{:04.1}", (s / 60.0).floor() as u64, s % 60.0)
}

/// Counter state at a session boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Handoff {
    /// Event that ended the previous session (`stopped`) or `<none>` if it ended without one.
    pub ended_with: String,
    /// `(uploaded, downloaded, left)` in the last announce of the previous session.
    pub before: (i64, i64, i64),
    /// `(uploaded, downloaded, left)` in the next `started`.
    pub after: (i64, i64, i64),
    /// Seconds between the two requests.
    pub gap_s: f64,
}

/// One `started`-delimited session.
#[derive(Debug, Clone, Default)]
pub struct Session {
    /// Start time (ms).
    pub start_ms: u64,
    /// Distinct `key` values seen.
    pub keys: Vec<String>,
    /// Distinct peer ids (hex) seen.
    pub peer_ids: Vec<String>,
    /// Announce count.
    pub n: usize,
    /// Events in order.
    pub events: Vec<String>,
}

/// Everything we can say about one client's handling of one torrent.
#[derive(Debug, Clone, Default)]
pub struct Profile {
    /// Client label.
    pub client: String,
    /// Info-hash hex.
    pub info_hash: String,
    /// Tracker id.
    pub tracker: String,
    /// Distinct User-Agent values with counts.
    pub user_agents: BTreeMap<String, usize>,
    /// Header names in order of the first request.
    pub header_order: Vec<String>,
    /// Parameter-name sequences by event with counts.
    pub order_by_event: BTreeMap<String, Vec<(Vec<String>, usize)>>,
    /// Event sequence (`none` for regular announces).
    pub events: Vec<String>,
    /// Aggregate encoding style of `info_hash`.
    pub enc_info_hash: EncodingInfo,
    /// Aggregate encoding style of `peer_id`.
    pub enc_peer_id: EncodingInfo,
    /// ASCII peer-id prefixes (first 8 chars) seen.
    pub peer_id_prefixes: Vec<String>,
    /// Values seen for every other parameter.
    pub param_values: BTreeMap<String, BTreeMap<String, usize>>,
    /// Sessions.
    pub sessions: Vec<Session>,
    /// Counter handoffs between sessions.
    pub handoffs: Vec<Handoff>,
    /// Regular-announce gaps `(gap_s, tracker interval, tracker min interval)`.
    pub gaps: Vec<(f64, Option<i64>, Option<i64>)>,
    /// Number of non-success responses.
    pub failures: usize,
}

fn merge(a: &mut EncodingInfo, b: &EncodingInfo) {
    a.bytes += b.bytes;
    a.escaped += b.escaped;
    a.literal += b.literal;
    a.unreserved_escaped += b.unreserved_escaped;
    a.reserved_literal += b.reserved_literal;
    if a.hex_case.is_empty() || a.hex_case == "none" {
        a.hex_case = b.hex_case;
    } else if b.hex_case != "none" && a.hex_case != b.hex_case {
        a.hex_case = "mixed";
    }
}

/// Reports are committed to the repository, so the client's own address
/// (`ip`, `ipv4`, `ipv6` parameters) is replaced by its kind.
pub fn redact_param(key: &str, value: &str) -> String {
    if !matches!(key, "ip" | "ipv4" | "ipv6") {
        return value.to_owned();
    }
    if value.parse::<std::net::Ipv4Addr>().is_ok() {
        "<IPv4 address>".into()
    } else if value.trim_matches(['[', ']']).parse::<std::net::Ipv6Addr>().is_ok() {
        "<IPv6 address>".into()
    } else {
        "<other>".into()
    }
}

/// Build the profile of a group.
pub fn profile(g: &Group<'_>) -> Profile {
    let mut p = Profile { client: g.client.clone(), info_hash: g.info_hash.clone(), tracker: g.tracker.clone(), ..Profile::default() };
    let mut last_ok: Option<&Record> = None;
    let mut prev: Option<&Record> = None;
    for r in &g.recs {
        if p.header_order.is_empty() {
            p.header_order = r.headers.iter().map(|(k, _)| k.clone()).collect();
        }
        *p.user_agents.entry(r.header("user-agent").unwrap_or("<none>").to_owned()).or_default() += 1;
        let ev = event_of(r).to_owned();
        let order: Vec<String> = r.params.iter().map(|x| x.k.clone()).collect();
        let slot = p.order_by_event.entry(ev.clone()).or_default();
        match slot.iter_mut().find(|(o, _)| *o == order) {
            Some((_, n)) => *n += 1,
            None => slot.push((order, 1)),
        }
        if let Some(v) = r.param_raw("info_hash") {
            merge(&mut p.enc_info_hash, &encoding_info(v));
        }
        if let Some(v) = r.param_raw("peer_id") {
            merge(&mut p.enc_peer_id, &encoding_info(v));
            let ascii = String::from_utf8_lossy(&crate::record::percent_decode(v)).chars().take(8).collect::<String>();
            if !p.peer_id_prefixes.contains(&ascii) {
                p.peer_id_prefixes.push(ascii);
            }
        }
        for x in &r.params {
            if matches!(x.k.as_str(), "info_hash" | "peer_id") {
                continue;
            }
            *p.param_values.entry(x.k.clone()).or_default().entry(redact_param(&x.k, &x.dec)).or_default() += 1;
        }
        if !r.succeeded() {
            p.failures += 1;
        }
        // Sessions and handoffs.
        let counters = (num(r, "uploaded").unwrap_or(0), num(r, "downloaded").unwrap_or(0), num(r, "left").unwrap_or(0));
        if ev == "started" || p.sessions.is_empty() {
            if ev == "started" {
                if let Some(pr) = prev {
                    p.handoffs.push(Handoff {
                        ended_with: if event_of(pr) == "stopped" { "stopped".into() } else { format!("<no stopped; last event={}>", event_of(pr)) },
                        before: (num(pr, "uploaded").unwrap_or(0), num(pr, "downloaded").unwrap_or(0), num(pr, "left").unwrap_or(0)),
                        after: counters,
                        gap_s: r.ts_ms.saturating_sub(pr.ts_ms) as f64 / 1000.0,
                    });
                }
            }
            p.sessions.push(Session { start_ms: r.ts_ms, ..Session::default() });
        }
        if let Some(s) = p.sessions.last_mut() {
            s.n += 1;
            s.events.push(ev.clone());
            if let Some(k) = r.param("key") {
                if !s.keys.iter().any(|x| x == k) {
                    s.keys.push(k.to_owned());
                }
            }
            if let Some(pid) = &r.peer_id {
                if !s.peer_ids.contains(pid) {
                    s.peer_ids.push(pid.clone());
                }
            }
        }
        if ev == "none" {
            if let (Some(l), Some(pr)) = (last_ok, prev) {
                // Only count gaps between consecutive regular activity (prev must be the last success).
                if std::ptr::eq(l, pr) {
                    let resp = l.response.as_ref();
                    p.gaps.push((r.ts_ms.saturating_sub(l.ts_ms) as f64 / 1000.0, resp.and_then(|x| x.interval), resp.and_then(|x| x.min_interval)));
                }
            }
        }
        p.events.push(ev);
        if r.succeeded() {
            last_ok = Some(r);
        }
        prev = Some(r);
    }
    p
}

/// Run-length encode an event list: `started, none x12, stopped`.
pub fn rle(events: &[String]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < events.len() {
        let mut j = i;
        while j < events.len() && events[j] == events[i] {
            j += 1;
        }
        out.push(if j - i > 1 { format!("{} x{}", events[i], j - i) } else { events[i].clone() });
        i = j;
    }
    out.join(", ")
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    if v.is_empty() { 0.0 } else { v[v.len() / 2] }
}

fn enc_line(e: &EncodingInfo) -> String {
    format!(
        "{} bytes: {} escaped, {} literal; needlessly escaped unreserved bytes: {}; unsafe literal bytes: {}; hex digit case: {}",
        e.bytes, e.escaped, e.literal, e.unreserved_escaped, e.reserved_literal, e.hex_case
    )
}

fn cell(r: &Record, k: &str) -> String {
    r.param(k).unwrap_or("-").to_owned()
}

fn resp_cell(r: &Record) -> String {
    if let Some(e) = &r.error {
        return format!("ERROR {e}");
    }
    let Some(x) = &r.response else { return "-".into() };
    let mut s = format!("{}", r.upstream_status.unwrap_or(0));
    if let Some(f) = &x.failure_reason {
        let _ = write!(s, " FAILURE '{f}'");
    }
    if let Some(d) = &x.decode_error {
        let _ = write!(s, " BAD-BENCODE ({d})");
    }
    if let Some(v) = x.interval {
        let _ = write!(s, " int={v}");
    }
    if let Some(v) = x.min_interval {
        let _ = write!(s, " min={v}");
    }
    if let (Some(c), Some(i)) = (x.complete, x.incomplete) {
        let _ = write!(s, " S/L={c}/{i}");
    }
    if let Some(v) = x.peers {
        let _ = write!(s, " peers={v}");
    }
    if let Some(v) = x.peers6.filter(|v| *v > 0) {
        let _ = write!(s, " peers6={v}");
    }
    if let Some(v) = &x.tracker_id {
        let _ = write!(s, " trackerid={v}");
    }
    if let Some(v) = &x.warning_message {
        let _ = write!(s, " WARNING '{v}'");
    }
    if !x.other.is_empty() {
        let _ = write!(s, " other[{}]", x.other.join(","));
    }
    let _ = write!(s, " ({}ms, {}B)", r.latency_ms.unwrap_or(0), r.response_size.unwrap_or(0));
    s
}

/// Render the full human-readable report for a capture.
pub fn report(all: &[Record], opts: &CheckOpts, title: &str) -> (String, bool) {
    let mut s = String::new();
    let t0 = all.first().map_or(0, |r| r.ts_ms);
    let n_ann = all.iter().filter(|r| r.kind == "announce").count();
    let n_scr = all.iter().filter(|r| r.kind == "scrape").count();
    let markers: Vec<&Record> = all.iter().filter(|r| r.kind == "marker").collect();
    let _ = writeln!(s, "# Capture report: {title}");
    let _ = writeln!(s, "records: {} total, {n_ann} announce, {n_scr} scrape, {} markers\n", all.len(), markers.len());
    let gs = groups(all);
    for g in &gs {
        let p = profile(g);
        let _ = writeln!(s, "## Client `{}`  torrent {}  tracker id `{}`", p.client, p.info_hash, p.tracker);
        let _ = writeln!(s, "User-Agent: {}", p.user_agents.iter().map(|(k, n)| format!("`{k}` x{n}")).collect::<Vec<_>>().join(", "));
        let _ = writeln!(s, "Request header order: {}", p.header_order.join(", "));
        let _ = writeln!(s, "\n### Timeline (t0 = first record; delta = since previous announce of this torrent)");
        let _ = writeln!(s, "{:<9} {:>8}  {:<10} {:>12} {:>12} {:>12} {:>7}  response", "time", "delta", "event", "uploaded", "downloaded", "left", "numwant");
        let last_ts = g.recs.last().map_or(0, |r| r.ts_ms);
        let mut items: Vec<(u64, Option<&Record>, Option<&Record>)> = g.recs.iter().map(|r| (r.ts_ms, Some(*r), None)).collect();
        items.extend(markers.iter().filter(|m| m.ts_ms <= last_ts + 300_000).map(|m| (m.ts_ms, None, Some(*m))));
        items.sort_by_key(|i| i.0);
        let mut prev_ts: Option<u64> = None;
        for (ts, rec, mk) in items {
            if let Some(m) = mk {
                let _ = writeln!(s, "{:<9} {:>8}  -- MARKER: {} --", fmt_t(ts.saturating_sub(t0)), "", m.marker.as_deref().unwrap_or("?"));
            } else if let Some(r) = rec {
                let delta = prev_ts.map_or(String::new(), |p| format!("{:+.1}s", ts.saturating_sub(p) as f64 / 1000.0));
                let _ = writeln!(
                    s,
                    "{:<9} {:>8}  {:<10} {:>12} {:>12} {:>12} {:>7}  {}",
                    fmt_t(ts.saturating_sub(t0)),
                    delta,
                    event_of(r),
                    cell(r, "uploaded"),
                    cell(r, "downloaded"),
                    cell(r, "left"),
                    cell(r, "numwant"),
                    resp_cell(r)
                );
                prev_ts = Some(ts);
            }
        }
        let _ = writeln!(s, "\n### Summary");
        let _ = writeln!(s, "event sequence: {}", rle(&p.events));
        let _ = writeln!(s, "responses that were not a clean success: {}", p.failures);
        let _ = writeln!(s, "\nparameter order by event (count):");
        for (ev, orders) in &p.order_by_event {
            for (o, n) in orders {
                let _ = writeln!(s, "  {ev:<9} x{n:<3} {}", o.join("&"));
            }
        }
        let _ = writeln!(s, "\nencoding:");
        let _ = writeln!(s, "  info_hash  {}", enc_line(&p.enc_info_hash));
        let _ = writeln!(s, "  peer_id    {}", enc_line(&p.enc_peer_id));
        let _ = writeln!(s, "  peer_id prefix(es) (ASCII, first 8): {:?}", p.peer_id_prefixes);
        let _ = writeln!(s, "\nparameter values seen (other than info_hash/peer_id):");
        for (k, vals) in &p.param_values {
            if matches!(k.as_str(), "uploaded" | "downloaded" | "left") {
                continue;
            }
            let shown = if vals.len() <= 6 {
                vals.iter().map(|(v, n)| format!("{v:?} x{n}")).collect::<Vec<_>>().join(", ")
            } else {
                format!("{} distinct values", vals.len())
            };
            let _ = writeln!(s, "  {k:<14} {shown}");
        }
        let _ = writeln!(s, "\nsessions (a session starts at event=started):");
        for (i, se) in p.sessions.iter().enumerate() {
            let _ = writeln!(
                s,
                "  #{}: {} announces, starts {}, key={:?}, peer_id={:?}",
                i + 1,
                se.n,
                fmt_t(se.start_ms.saturating_sub(t0)),
                se.keys,
                se.peer_ids.iter().map(|x| ascii_hex(x)).collect::<Vec<_>>()
            );
        }
        if p.sessions.len() > 1 {
            let keys: Vec<&String> = p.sessions.iter().flat_map(|x| x.keys.iter()).collect();
            let pids: Vec<&String> = p.sessions.iter().flat_map(|x| x.peer_ids.iter()).collect();
            let _ = writeln!(
                s,
                "  across sessions: key {}, peer_id {}",
                if keys.windows(2).all(|w| w[0] == w[1]) { "SAME" } else { "CHANGES" },
                if pids.windows(2).all(|w| w[0] == w[1]) { "SAME" } else { "CHANGES" }
            );
        }
        if !p.handoffs.is_empty() {
            let _ = writeln!(s, "\ncounters at session boundaries (uploaded/downloaded/left):");
            for h in &p.handoffs {
                let reset = h.after.0 == 0 && h.after.1 == 0 && (h.before.0 != 0 || h.before.1 != 0);
                let _ = writeln!(
                    s,
                    "  {} {:?} -> next started {:?} after {:.1}s: {}",
                    h.ended_with,
                    h.before,
                    h.after,
                    h.gap_s,
                    if reset { "uploaded/downloaded RESET to 0 (per-session counters)" } else if h.after.0 == h.before.0 && h.after.1 == h.before.1 { "carried over unchanged" } else { "changed" }
                );
            }
        }
        let _ = writeln!(s, "\ninterval adherence (regular announces following a successful response):");
        if p.gaps.is_empty() {
            let _ = writeln!(s, "  no consecutive regular announces observed");
        } else {
            let mut g: Vec<f64> = p.gaps.iter().map(|x| x.0).collect();
            let iv = p.gaps.iter().find_map(|x| x.1);
            let mi = p.gaps.iter().find_map(|x| x.2);
            let _ = writeln!(
                s,
                "  n={} gaps: min {:.1}s, median {:.1}s, max {:.1}s; tracker interval={:?} min interval={:?}",
                g.len(),
                g.iter().copied().fold(f64::MAX, f64::min),
                median(&mut g),
                g.iter().copied().fold(0.0, f64::max),
                iv,
                mi
            );
        }
        s.push('\n');
    }
    let findings = detect(all, opts);
    let _ = writeln!(s, "## Detector verdict (tolerance {} ms, max {} req/min/host)", opts.tolerance_ms, opts.max_per_minute);
    let (v, fail) = verdicts(all, &findings, t0);
    s.push_str(&v);
    if gs.is_empty() {
        s.push_str("(no announce records)\n");
    }
    (s, fail)
}

fn ascii_hex(h: &str) -> String {
    let bytes: Vec<u8> = (0..h.len() / 2).filter_map(|i| u8::from_str_radix(&h[2 * i..2 * i + 2], 16).ok()).collect();
    bytes.iter().map(|b| if b.is_ascii_graphic() { *b as char } else { '.' }).collect()
}

fn load_many(files: &[PathBuf]) -> Result<Vec<Record>> {
    let mut all = Vec::new();
    for f in files {
        let (mut recs, bad) = read_records(f)?;
        if bad > 0 {
            eprintln!("warning: {} unparsable line(s) skipped in {}", bad, f.display());
        }
        all.append(&mut recs);
    }
    all.sort_by_key(|r| r.ts_ms);
    Ok(all)
}

/// `analyze` entry point. Returns true when a violation was found.
pub fn run_analyze(a: &AnalyzeArgs) -> Result<bool> {
    let all = load_many(&a.files)?;
    // File names only: reports are committed and must not embed local absolute paths.
    let title = a.files.iter().map(|f| f.file_name().map_or_else(|| f.display().to_string(), |n| n.to_string_lossy().into_owned())).collect::<Vec<_>>().join(", ");
    let (text, fail) = report(&all, &a.opts, &title);
    print!("{text}");
    Ok(fail)
}

/// `check` entry point. Returns true when a violation was found.
pub fn run_check(a: &CheckArgs) -> Result<bool> {
    let all = load_many(&a.files)?;
    let t0 = all.first().map_or(0, |r| r.ts_ms);
    let findings = detect(&all, &a.opts);
    let (text, mut fail) = verdicts(&all, &findings, t0);
    if text.is_empty() {
        println!("no announce records found");
    }
    print!("{text}");
    let announces = all.iter().filter(|r| r.kind == "announce").count();
    if announces < a.expect_announces {
        println!("FAIL  NO_ANNOUNCES: {announces} announce record(s), expected at least {}", a.expect_announces);
        fail = true;
    }
    Ok(fail)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::record::{RespSummary, parse_params};

    /// Build a synthetic announce record for tests.
    pub fn rec(ts_s: f64, event: Option<&str>, up: i64, down: i64, left: i64, interval: i64, min: Option<i64>) -> Record {
        let mut q = format!("info_hash=%AA%BB&peer_id=-XX1000-abcdefghijkl&port=1&uploaded={up}&downloaded={down}&left={left}&key=ABCD");
        if let Some(e) = event {
            q.push_str(&format!("&event={e}"));
        }
        Record {
            kind: "announce".into(),
            ts_ms: (ts_s * 1000.0) as u64 + 1_000_000,
            label: Some("t".into()),
            id: Some("x".into()),
            params: parse_params(&q),
            info_hash: Some("aabb".into()),
            peer_id: Some("deadbeef".into()),
            upstream_status: Some(200),
            latency_ms: Some(10),
            response: Some(RespSummary { interval: Some(interval), min_interval: min, ..RespSummary::default() }),
            ..Record::default()
        }
    }

    fn rules(f: &[Finding]) -> Vec<&'static str> {
        f.iter().filter(|x| x.severity == Severity::Fail).map(|x| x.rule).collect()
    }

    #[test]
    fn well_behaved_client_passes() {
        let all = vec![
            rec(0.0, Some("started"), 0, 0, 100, 60, Some(30)),
            rec(60.0, None, 5, 10, 90, 60, Some(30)),
            rec(121.0, None, 6, 20, 80, 60, Some(30)),
            rec(130.0, Some("completed"), 6, 100, 0, 60, Some(30)),
            rec(190.0, None, 6, 100, 0, 60, Some(30)),
            rec(200.0, Some("stopped"), 7, 100, 0, 60, Some(30)),
        ];
        let f = detect(&all, &CheckOpts::default());
        assert!(rules(&f).is_empty(), "{f:?}");
    }

    #[test]
    fn spam_every_20s_fails() {
        let mut all = vec![rec(0.0, Some("started"), 0, 0, 100, 1800, None)];
        for i in 1..10 {
            all.push(rec(f64::from(i) * 20.0, None, 0, 0, 100, 1800, None));
        }
        let f = detect(&all, &CheckOpts::default());
        assert!(rules(&f).contains(&"ANNOUNCE_SPAM"));
        assert!(rules(&f).contains(&"ANNOUNCE_SPAM_SUMMARY"));
    }

    #[test]
    fn spam_relative_to_min_interval_and_warn_vs_interval() {
        // interval 60 / min 30: a 40 s gap violates interval only -> warning, not failure.
        let all = vec![rec(0.0, Some("started"), 0, 0, 1, 60, Some(30)), rec(40.0, None, 0, 0, 1, 60, Some(30))];
        let f = detect(&all, &CheckOpts::default());
        assert!(rules(&f).is_empty());
        assert!(f.iter().any(|x| x.rule == "EARLY_VS_INTERVAL"));
        // 20 s gap violates min interval -> spam.
        let all = vec![rec(0.0, Some("started"), 0, 0, 1, 60, Some(30)), rec(20.0, None, 0, 0, 1, 60, Some(30))];
        assert!(rules(&detect(&all, &CheckOpts::default())).contains(&"ANNOUNCE_SPAM"));
    }

    #[test]
    fn event_announces_are_exempt_from_spam() {
        let all = vec![
            rec(0.0, Some("started"), 0, 0, 1, 1800, None),
            rec(1.0, Some("completed"), 0, 1, 0, 1800, None),
            rec(2.0, Some("stopped"), 0, 1, 0, 1800, None),
            rec(3.0, Some("started"), 0, 0, 0, 1800, None),
        ];
        assert!(rules(&detect(&all, &CheckOpts::default())).is_empty());
    }

    #[test]
    fn ordering_violations() {
        let all = vec![rec(0.0, None, 0, 0, 1, 60, None)];
        assert!(rules(&detect(&all, &CheckOpts::default())).contains(&"STARTED_FIRST"));
        let all = vec![rec(0.0, Some("started"), 0, 0, 1, 60, None), rec(5.0, Some("stopped"), 0, 0, 1, 60, None), rec(900.0, None, 0, 0, 1, 60, None)];
        assert!(rules(&detect(&all, &CheckOpts::default())).contains(&"AFTER_STOPPED"));
        let all = vec![rec(0.0, Some("started"), 0, 0, 1, 60, None), rec(1.0, Some("completed"), 0, 1, 0, 60, None), rec(2.0, Some("completed"), 0, 1, 0, 60, None)];
        assert!(rules(&detect(&all, &CheckOpts::default())).contains(&"DUP_COMPLETED"));
    }

    #[test]
    fn repeated_started_after_success_is_spam_after_failure_is_retry() {
        let all = vec![rec(0.0, Some("started"), 0, 0, 1, 60, Some(30)), rec(20.0, Some("started"), 0, 0, 1, 60, Some(30))];
        assert!(rules(&detect(&all, &CheckOpts::default())).contains(&"REPEATED_STARTED"));
        let mut failed = rec(0.0, Some("started"), 0, 0, 1, 60, Some(30));
        failed.upstream_status = Some(500);
        let all = vec![failed, rec(20.0, Some("started"), 0, 0, 1, 60, Some(30))];
        assert!(rules(&detect(&all, &CheckOpts::default())).is_empty());
    }

    #[test]
    fn counter_decrease_within_session_only() {
        let all = vec![rec(0.0, Some("started"), 10, 10, 5, 60, None), rec(100.0, None, 9, 10, 5, 60, None)];
        assert!(rules(&detect(&all, &CheckOpts::default())).contains(&"COUNTER_DECREASE"));
        // A new started resets the baseline: not a violation.
        let all = vec![
            rec(0.0, Some("started"), 10, 10, 5, 60, None),
            rec(1.0, Some("stopped"), 10, 10, 5, 60, None),
            rec(2.0, Some("started"), 0, 0, 5, 60, None),
        ];
        assert!(rules(&detect(&all, &CheckOpts::default())).is_empty());
    }

    #[test]
    fn crypto_parameters_are_accepted_not_flagged() {
        // Seedwell with encryption prefer/require sends `compact&supportcrypto[&requirecrypto]&event`.
        for extra in ["&compact=1&supportcrypto=1", "&compact=1&supportcrypto=1&requirecrypto=1"] {
            let mut all = vec![
                rec(0.0, Some("started"), 0, 0, 100, 60, Some(30)),
                rec(60.0, None, 5, 10, 90, 60, Some(30)),
                rec(70.0, Some("stopped"), 5, 10, 90, 60, Some(30)),
            ];
            for r in &mut all {
                let mut q: Vec<String> = r.params.iter().map(|p| format!("{}={}", p.k, p.v)).collect();
                let at = q.iter().position(|kv| kv.starts_with("event=")).unwrap_or(q.len());
                q.insert(at, extra.trim_start_matches('&').to_owned());
                r.params = parse_params(&q.join("&"));
            }
            assert!(all.iter().all(|r| r.params.iter().any(|p| p.k == "supportcrypto")));
            let f = detect(&all, &CheckOpts::default());
            assert!(f.is_empty(), "{f:?}");
            let (text, fail) = report(&all, &CheckOpts::default(), "t");
            assert!(!fail && text.contains("PASS  t"), "{text}");
        }
    }

    #[test]
    fn host_rate_limit() {
        let mut all = vec![rec(0.0, Some("started"), 0, 0, 1, 10, None)];
        for i in 1..30 {
            all.push(rec(f64::from(i), None, 0, 0, 1, 10, None));
        }
        assert!(detect(&all, &CheckOpts { max_per_minute: 10, ..CheckOpts::default() }).iter().any(|f| f.rule == "HOST_RATE"));
    }

    #[test]
    fn own_address_is_redacted_in_reports() {
        assert_eq!(redact_param("ipv4", "80.1.2.3"), "<IPv4 address>");
        assert_eq!(redact_param("ipv6", "2001:db8::1"), "<IPv6 address>");
        assert_eq!(redact_param("numwant", "80"), "80");
    }

    #[test]
    fn rle_and_report_smoke() {
        let ev: Vec<String> = ["started", "none", "none", "stopped"].iter().map(ToString::to_string).collect();
        assert_eq!(rle(&ev), "started, none x2, stopped");
        let all = vec![rec(0.0, Some("started"), 0, 0, 100, 60, Some(30)), rec(60.0, None, 5, 10, 90, 60, Some(30))];
        let (text, fail) = report(&all, &CheckOpts::default(), "t");
        assert!(!fail);
        assert!(text.contains("PASS  t"), "{text}");
    }
}
