//! Diagnostics for `seedwell debug …` that need no engine: the server process measured on demand,
//! the words for "why is the next announce then", and the health findings. Everything is derived from
//! what the protocol already returns, so the same rules serve the app, the daemon and the tests.

use std::process::Command;

use crate::types::*;

/// Measure this process with `ps` and the file-descriptor table. Run it on a blocking thread; it
/// spawns one short child process.
pub fn process_stats() -> ProcessDto {
    let pid = std::process::id();
    let mut out = ProcessDto { pid, ..Default::default() };
    let ps = |args: &[&str]| -> Option<String> {
        // The C locale: in a Norwegian one `ps` writes "0,0".
        let o = Command::new("ps").env("LC_ALL", "C").args(args).arg("-p").arg(pid.to_string()).output().ok()?;
        o.status.success().then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    if let Some(t) = ps(&["-o", "rss=", "-o", "%cpu="]) {
        let mut it = t.split_whitespace();
        out.rss_bytes = it.next().and_then(|v| v.parse::<u64>().ok()).map(|kb| kb * 1024);
        out.cpu_percent = it.next().and_then(decimal);
    }
    out.threads = if cfg!(target_os = "macos") {
        // One line per thread plus a header.
        ps(&["-M"]).map(|t| t.lines().count().saturating_sub(1) as u32)
    } else {
        ps(&["-o", "nlwp="]).and_then(|t| t.trim().parse().ok())
    };
    let fd_dir = if cfg!(target_os = "macos") { "/dev/fd" } else { "/proc/self/fd" };
    // The directory handle used for the listing is itself one descriptor.
    out.open_fds = std::fs::read_dir(fd_dir).ok().map(|d| d.count().saturating_sub(1) as u32);
    out
}

/// "12.5", or "12,5" from a `ps` that kept a locale's decimal comma.
fn decimal(v: &str) -> Option<f64> {
    v.replace(',', ".").parse().ok()
}

/// Why the next announce is when it is.
pub fn next_reason(t: &TrackerDto, transfer_state: &str, now: u64) -> String {
    match t.state.as_str() {
        "halted" => "halted: nothing is sent until you ask (`seedwell debug announce`)".into(),
        "backoff" => match t.retry_at {
            Some(r) if r > now => format!("retry #{} after a failure, in {}", t.failures + 1, crate::human::duration(r - now)),
            _ => format!("retry #{} after a failure, due now", t.failures + 1),
        },
        _ if matches!(transfer_state, "paused" | "completed" | "error" | "missing-data") => format!("none: the torrent is {transfer_state}"),
        _ if t.next_announce_at.is_none() => "no announce scheduled (waiting for the first one, or the network is down)".into(),
        _ => "regular interval from the tracker".into(),
    }
}

/// The findings of `debug health`.
/// `tracker_trouble`: `(short id, name, reason)` of torrents whose every tracker is halted or backing off.
pub fn health(probe: &ProbeDto, rows: &[TransferDto], limits: &LimitsDto, tracker_trouble: &[(String, String, String)]) -> DebugHealthDto {
    let mut issues: Vec<HealthIssue> = Vec::new();
    let mut add = |area: &str, severity: &str, message: String, next: String| {
        issues.push(HealthIssue { area: area.into(), severity: severity.into(), message, next });
    };
    if !probe.online {
        add("offline", "problem", "The network looks down; trackers and DHT are held until it is back.".into(), "seedwell debug trackers".into());
    }
    if let Some(p) = &probe.port_fallback {
        add("port", "notice", format!("The listen port was taken ({p}); incoming peers may not find you."), "seedwell debug perf".into());
    }
    if probe.portmap_reported && probe.portmap_unavailable {
        add("port", "notice", "No port mapping (UPnP / NAT-PMP) worked; incoming connections depend on a manual forward.".into(), "seedwell debug perf".into());
    } else if probe.portmap_reported && probe.portmap_behind_cgnat {
        add("port", "notice", "The router's address is not reachable from the Internet (CGNAT); only outgoing connections work.".into(), "seedwell debug perf".into());
    }
    let tracker_bad: Vec<&TransferDto> = rows
        .iter()
        .filter(|r| r.waiting_note.as_deref().is_some_and(|n| n.starts_with("Tracker")) || r.error.as_deref().is_some_and(|e| e.to_lowercase().contains("tracker")) || tracker_trouble.iter().any(|t| r.id.starts_with(&t.0)))
        .collect();
    if let Some(first) = tracker_bad.first() {
        let detail = tracker_trouble.iter().find(|t| first.id.starts_with(&t.0)).map(|t| t.2.clone()).or_else(|| first.waiting_note.clone()).or_else(|| first.error.clone()).unwrap_or_default();
        add(
            "tracker",
            "problem",
            format!("{} torrent(s) have tracker trouble, e.g. {}: {}", tracker_bad.len(), first.name, detail),
            format!("seedwell debug trackers {}", first.short_id()),
        );
    }
    let disk_bad: Vec<&TransferDto> = rows.iter().filter(|r| matches!(r.state.as_str(), "error" | "missing-data") && !tracker_bad.iter().any(|t| t.id == r.id)).collect();
    if let Some(first) = disk_bad.first() {
        add(
            "disk",
            "problem",
            format!("{} torrent(s) in error, e.g. {}: {}", disk_bad.len(), first.name, first.error.as_deref().unwrap_or(&first.state)),
            format!("seedwell info {}", first.short_id()),
        );
    }
    let stalled: Vec<&TransferDto> = rows
        .iter()
        .filter(|r| r.state == "downloading" && r.down_rate < 2048 && !tracker_bad.iter().any(|t| t.id == r.id))
        .collect();
    if let Some(first) = stalled.first() {
        add(
            "stalled",
            "notice",
            format!("{} download(s) are barely moving, e.g. {}{}", stalled.len(), first.name, first.waiting_note.as_deref().map(|n| format!(" ({n})")).unwrap_or_default()),
            format!("seedwell debug peers {}", first.short_id()),
        );
    }
    let unmoved: Vec<&TransferDto> = rows.iter().filter(|r| r.move_note.is_some()).collect();
    if let Some(first) = unmoved.first() {
        add(
            "move",
            "notice",
            format!("{} torrent(s) could not be moved into their status folder and keep seeding where they are, e.g. {}: {}", unmoved.len(), first.name, first.move_note.as_deref().unwrap_or_default()),
            format!("seedwell info {}", first.short_id()),
        );
    }
    let downloading = rows.iter().any(|r| r.state == "downloading");
    if downloading && probe.down_limited {
        add("limits", "notice", format!("Downloads are capped at {}.", limits.down_limit.map_or("a limit".into(), crate::human::rate)), "seedwell limits".into());
    }
    if probe.max_connections > 0 && probe.connections_open as f64 >= f64::from(probe.max_connections) * 0.95 {
        add("limits", "notice", format!("The connection budget is almost used ({} of {}).", probe.connections_open, probe.max_connections), "seedwell debug perf".into());
    }
    let ok = !issues.iter().any(|i| i.severity == "problem");
    let summary = if issues.is_empty() {
        "Nothing wrong found.".to_owned()
    } else {
        format!("{} problem(s), {} notice(s).", issues.iter().filter(|i| i.severity == "problem").count(), issues.iter().filter(|i| i.severity == "notice").count())
    };
    DebugHealthDto { ok, issues, summary }
}

/// `203.0.113.45:6881` to `203.0.113.x:6881` (IPv6: the last three groups).
pub fn redact_addr(addr: &str) -> String {
    let Some((host, port)) = addr.rsplit_once(':') else { return addr.to_owned() };
    let host = host.trim_matches(|c| c == '[' || c == ']');
    if host.contains(':') {
        let groups: Vec<&str> = host.split(':').collect();
        let keep = groups.len().saturating_sub(3);
        let masked: Vec<&str> = groups.iter().take(keep).copied().chain(std::iter::repeat_n("x", groups.len() - keep)).collect();
        format!("[{}]:{port}", masked.join(":"))
    } else {
        let mut o: Vec<&str> = host.split('.').collect();
        if let Some(last) = o.last_mut() {
            *last = "x";
        }
        format!("{}:{port}", o.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_is_measured_whatever_the_locale() {
        assert_eq!((decimal("12.5"), decimal("0,0"), decimal("x")), (Some(12.5), Some(0.0), None));
        let p = process_stats();
        assert!(p.cpu_percent.is_some() && p.rss_bytes.is_some_and(|b| b > 0), "{p:?}");
    }

    #[test]
    fn addresses_are_masked_on_request() {
        assert_eq!(redact_addr("203.0.113.45:6881"), "203.0.113.x:6881");
        assert_eq!(redact_addr("[2001:db8:0:0:0:0:1:2]:51413"), "[2001:db8:0:0:0:x:x:x]:51413");
    }

    #[test]
    fn health_is_quiet_when_nothing_is_wrong() {
        let p = ProbeDto { online: true, ..Default::default() };
        let h = health(&p, &[], &LimitsDto::default(), &[]);
        assert!(h.ok && h.issues.is_empty());
        let p = ProbeDto { online: false, ..Default::default() };
        let h = health(&p, &[], &LimitsDto::default(), &[]);
        assert!(!h.ok && h.issues[0].area == "offline");
    }
}
