//! `seedwell debug …`: diagnostics for trackers, peers, connections and performance. All of it is
//! read-only (permission `read`) except `announce` (Ask Tracker Again, `control`, gated exactly as in
//! the app). Developer mode itself is `seedwell dev`. Passkeys never reach this
//! output: the server masks every URL and status text before it answers.
#![allow(missing_docs)] // variant fields are named after the options they hold


use std::fmt::Write as _;
use std::io::IsTerminal;
use std::time::Duration;

use serde_json::Value;
use trs_control::client::Client;
use trs_control::human;
use trs_control::proto::*;
use trs_control::types::*;

use crate::args::{Opts, Usage};
use crate::cmd::{LogArgs, parse_log_args};
use crate::commands::{CliError, Global, R, connect, json_text, log_trackers, reannounce, resolve, resolve_one, show};
use crate::select::Selection;

/// A `debug` subcommand.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// `debug health`
    Health { watch: bool },
    /// `debug trackers [<torrent>] [--tracker X]`
    Trackers { torrent: Option<String>, tracker: Option<String>, watch: bool },
    /// `debug trackers log …`
    TrackersLog { log: LogArgs },
    /// `debug announce <torrent> [--tracker X]`
    Announce { sel: Selection, tracker: Option<String>, force: bool, wait: bool },
    /// `debug peers <torrent>`
    Peers { torrent: String, redact: bool, watch: bool },
    /// `debug perf`
    Perf { watch: bool },
}

const USAGE: &str = "usage: seedwell debug health | trackers [<torrent>] [--tracker X] | trackers log [...] | announce <torrent> | peers <torrent> | perf   (--watch refreshes every second)";

/// Words after `debug`.
pub fn parse(args: Vec<String>) -> Result<Cmd, Usage> {
    let mut it = args.into_iter();
    let sub = it.next().ok_or_else(|| Usage(USAGE.into()))?;
    let mut o = Opts::new(it.collect());
    let watch = o.flag(&["--watch", "-w"]);
    Ok(match sub.as_str() {
        "health" => {
            no_more(o)?;
            Cmd::Health { watch }
        }
        "perf" => {
            no_more(o)?;
            Cmd::Perf { watch }
        }
        "trackers" => {
            let log = parse_log_args(&mut o)?;
            let tracker = log.tracker.clone();
            let pos = o.finish()?;
            if pos.first().map(String::as_str) == Some("log") {
                if pos.len() > 1 {
                    return Err(Usage(USAGE.into()));
                }
                // `-f` means follow in the log, `--watch` too.
                let mut log = log;
                log.follow |= watch;
                return Ok(Cmd::TrackersLog { log });
            }
            if pos.len() > 1 {
                return Err(Usage("debug trackers takes at most one torrent".into()));
            }
            Cmd::Trackers { torrent: pos.into_iter().next(), tracker, watch: watch || log.follow }
        }
        "announce" | "reannounce" => {
            let force = o.flag(&["--force"]);
            let wait = o.flag(&["--wait"]);
            let sel = Selection::take(&mut o)?;
            let tracker = sel.tracker.clone();
            let mut sel = sel;
            sel.tracker = None;
            let sel = sel.with_terms(o.finish()?);
            if sel.terms.is_empty() && !sel.has_filter() {
                return Err(Usage("usage: seedwell debug announce <torrent> [--tracker X] [--force] [--wait]".into()));
            }
            Cmd::Announce { sel, tracker, force, wait }
        }
        "peers" => {
            let redact = o.flag(&["--redact"]);
            let follow = o.flag(&["--follow", "-f"]);
            let mut pos = o.finish()?;
            if pos.len() != 1 {
                return Err(Usage("usage: seedwell debug peers <torrent> [--redact] [--watch]".into()));
            }
            Cmd::Peers { torrent: pos.remove(0), redact, watch: watch || follow }
        }
        other => return Err(Usage(format!("unknown debug command `{other}`. {USAGE}"))),
    })
}

fn no_more(o: Opts) -> Result<(), Usage> {
    let pos = o.finish()?;
    match pos.first() {
        None => Ok(()),
        Some(x) => Err(Usage(format!("unexpected argument `{x}`"))),
    }
}

/// Decode an answer and render it.
fn decode<T: serde::de::DeserializeOwned>(raw: &Value, f: impl Fn(&T) -> String) -> R<String> {
    serde_json::from_value::<T>(raw.clone()).map(|t| f(&t)).map_err(|e| CliError::plain(1, format!("unexpected answer: {e}")))
}

/// Ask once, or every second with `--watch` (the screen is redrawn on a terminal; `--json` prints one line per tick).
async fn poll(g: &Global, watch: bool, c: &mut Client, req: impl Fn() -> Request, text: impl Fn(&Value) -> R<String>) -> R {
    loop {
        let raw = c.call(&req()).await?;
        let t = text(&raw)?;
        if !watch {
            show(g, &raw, || t.clone());
            return Ok(());
        }
        if g.json {
            println!("{}", json_text(&raw, false));
        } else {
            if std::io::stdout().is_terminal() {
                print!("\x1b[2J\x1b[H");
            }
            println!("{t}\n\n(refreshing every second; Ctrl-C to stop)");
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Run a `debug` command.
pub async fn run(cmd: Cmd, g: &Global) -> R {
    match cmd {
        Cmd::Health { watch } => {
            let mut c = connect(g).await?;
            poll(g, watch, &mut c, || Request::DebugHealth(Empty {}), |raw| decode::<DebugHealthDto>(raw, health)).await
        }
        Cmd::Perf { watch } => {
            let mut c = connect(g).await?;
            poll(g, watch, &mut c, || Request::DebugPerf(Empty {}), |raw| decode::<DebugPerfDto>(raw, perf)).await
        }
        Cmd::Trackers { torrent, tracker, watch } => {
            let mut c = connect(g).await?;
            let id = match torrent {
                Some(t) => Some(resolve_one(&mut c, &Selection::default().with_terms(vec![t]), "debug trackers").await?.id),
                None => None,
            };
            poll(g, watch, &mut c, || Request::DebugTrackers(DebugTrackersParams { id: id.clone(), tracker: tracker.clone() }), |raw| decode::<DebugTrackersDto>(raw, trackers)).await
        }
        Cmd::TrackersLog { log } => log_trackers(g, log).await,
        Cmd::Announce { sel, tracker, force, wait } => {
            let mut c = connect(g).await?;
            let rows = resolve(&mut c, &sel).await?;
            reannounce(g, &mut c, &rows, tracker.as_deref(), force, wait).await
        }
        Cmd::Peers { torrent, redact, watch } => {
            let mut c = connect(g).await?;
            let row = resolve_one(&mut c, &Selection::default().with_terms(vec![torrent]), "debug peers").await?;
            poll(g, watch, &mut c, || Request::DebugPeers(DebugPeersParams { id: row.id.clone(), redact }), |raw| decode::<DebugPeersDto>(raw, peers)).await
        }
    }
}

// ----------------------------------------------------------------------------- text

fn ago(now: u64, t: Option<u64>) -> String {
    match t {
        None => "never".into(),
        Some(t) if t <= now => format!("{} ago", human::duration(now - t)),
        Some(t) => format!("in {}", human::duration(t - now)),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `debug health`.
pub fn health(h: &DebugHealthDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{}", if h.ok && h.issues.is_empty() { "Seedwell looks healthy. Nothing wrong found.".to_owned() } else { h.summary.clone() });
    for i in &h.issues {
        let _ = writeln!(o, "\n[{}] {}: {}\n    dig deeper: {}", i.severity, i.area, i.message, i.next);
    }
    o.trim_end().to_owned()
}

/// `debug trackers`.
pub fn trackers(t: &DebugTrackersDto) -> String {
    let n = now();
    let mut o = String::new();
    let _ = writeln!(
        o,
        "Network: {}",
        if t.online { "online".to_owned() } else { format!("OFFLINE since {} (trackers are held, not failing)", ago(n, t.offline_since)) }
    );
    if t.transfers.is_empty() {
        let _ = writeln!(o, "No torrents with trackers match.");
    }
    for x in &t.transfers {
        let _ = writeln!(o, "\n{}  {}  [{}]", short(&x.id), x.name, x.state);
        if x.trackers.is_empty() {
            let _ = writeln!(o, "  no trackers (DHT and peer exchange only)");
        }
        for tr in &x.trackers {
            let _ = writeln!(o, "  {}  tier {}  {}", tr.url, tr.tier, tr.state.to_uppercase());
            let _ = writeln!(o, "      status     {}", tr.status);
            let _ = writeln!(
                o,
                "      last       announced {}, answered {}{}",
                ago(n, tr.last_announce_at),
                ago(n, tr.last_success_at),
                match (tr.seeders, tr.leechers) {
                    (Some(s), Some(l)) => format!("; {s} seeders, {l} leechers"),
                    _ => String::new(),
                }
            );
            let _ = writeln!(o, "      next       {} ({})", ago(n, tr.next_announce_at), tr.next_reason);
            let _ = writeln!(o, "      failures   {}", tr.failures);
            let gate = match tr.manual_ok_at {
                Some(t) if t > n => format!("Ask Again held back, may go out in {}", human::duration(t - n)),
                _ => "Ask Again may go out now".to_owned(),
            };
            let _ = writeln!(o, "      gate       {gate}{}", tr.tripped.as_deref().map(|r| format!("; circuit breaker: {r}")).unwrap_or_default());
        }
    }
    if t.truncated > 0 {
        let _ = writeln!(o, "\n... {} more torrents; name one to see it", t.truncated);
    }
    o.trim_end().to_owned()
}

/// `debug peers`.
pub fn peers(p: &DebugPeersDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{}  {}  [{}]", short(&p.id), p.name, p.state);
    let _ = writeln!(
        o,
        "Connections (all torrents): {} connected, {} handshaking, budget {}; incoming accepted {}, refused {}",
        p.connected, p.half_open, p.max_connections, p.incoming_accepted, p.incoming_refused
    );
    if p.peers.is_empty() {
        let _ = writeln!(o, "No peers connected to this torrent.");
    } else {
        let _ = writeln!(o, "{:<24} {:<16} {:<4} {:<3} {:<6} {:>10} {:>10} {:>4} {:>6} {:>7}", "ADDRESS", "CLIENT", "NET", "DIR", "FLAGS", "DOWN", "UP", "REQ", "HAS", "TIME");
        for x in &p.peers {
            let mut flags = String::new();
            for (on, ch) in [(x.choking_us, 'c'), (x.choked_by_us, 'C'), (x.interested, 'i'), (x.snubbed, 'S'), (x.upload_only, 'U'), (x.has_pex, 'x')] {
                flags.push(if on { ch } else { '.' });
            }
            let _ = writeln!(
                o,
                "{:<24} {:<16} {:<4} {:<3} {:<6} {:>10} {:>10} {:>4} {:>5.0}% {:>7}",
                x.addr,
                x.client.chars().take(16).collect::<String>(),
                x.transport,
                x.direction,
                flags,
                if x.down_rate > 0 { human::rate(x.down_rate) } else { "-".into() },
                if x.up_rate > 0 { human::rate(x.up_rate) } else { "-".into() },
                x.requests_in_flight,
                f64::from(x.progress) * 100.0,
                human::duration(x.connected_secs)
            );
        }
        let _ = writeln!(o, "flags: c they choke us, C we choke them, i they want our data, S snubbed, U upload-only, x peer exchange");
        if p.truncated > 0 {
            let _ = writeln!(o, "... {} more peers not shown", p.truncated);
        }
    }
    o.trim_end().to_owned()
}

fn lim(l: Option<u64>) -> String {
    l.map_or_else(|| "unlimited".to_owned(), human::rate)
}

/// `debug perf`.
pub fn perf(p: &DebugPerfDto) -> String {
    let mut o = String::new();
    let pr = &p.probe;
    let _ = writeln!(o, "RATES   down {} / {}{}   up {} / {}{}", human::rate(p.totals.down_rate), lim(p.limits.down_limit), if pr.down_limited { " (limiter on)" } else { "" }, human::rate(p.totals.up_rate), lim(p.limits.up_limit), if pr.up_limited { " (limiter on)" } else { "" });
    if p.limits.alt_active {
        let _ = writeln!(o, "        alternative speed limits are ON");
    }
    let _ = writeln!(o, "        favorite-tracker caps in force: {}", pr.tracker_caps);
    let _ = writeln!(o, "PEERS   {} connected over {} torrents; {} open of {} allowed, {} handshaking (cap {})", p.peers, p.torrent_count, pr.connections_open.saturating_sub(pr.half_open), pr.max_connections, pr.half_open, pr.max_half_open);
    let c = |k: &str| pr.counters.get(k).copied().unwrap_or(0);
    let _ = writeln!(o, "        incoming accepted {} refused {}; uTP in {} out {}; encrypted in {} out {} (fallbacks {})", c("incoming_accepted"), c("incoming_refused"), c("utp_accepted"), c("utp_dialed"), c("mse_accepted"), c("mse_dialed"), c("mse_fallbacks"));
    let _ = writeln!(o, "        hash failures {}, peers banned {}, peer exchange in {} out {}", c("hash_failures"), c("peers_banned"), c("pex_received"), c("pex_sent"));
    let _ = writeln!(o, "DISK    verification slots {} of {} busy (peak {}); {} verifying now", pr.verify_slots - pr.verify_free.min(pr.verify_slots), pr.verify_slots, c("verify_peak"), p.torrents.iter().filter(|t| t.state == "checking").count());
    let _ = writeln!(o, "NETWORK {}; listening on {}{}", if pr.online { "online".to_owned() } else { "OFFLINE".to_owned() }, pr.listen_port, pr.port_fallback.as_deref().map(|f| format!(" ({f})")).unwrap_or_default());
    let pm = if !pr.portmap_reported {
        "not reported yet".to_owned()
    } else if pr.portmap_unavailable {
        "no UPnP / NAT-PMP / PCP gateway answered".to_owned()
    } else {
        format!(
            "{} mapped, external TCP port {}{}",
            pr.portmap_method.as_deref().unwrap_or("?"),
            pr.external_port.map_or("?".into(), |p| p.to_string()),
            if pr.portmap_behind_cgnat { " (behind CGNAT: not reachable from outside)" } else { "" }
        )
    };
    let _ = writeln!(o, "        port mapping: {pm}");
    let _ = writeln!(o, "DHT     {}; lookups {}, announces {}", if pr.dht_running { "running" } else { "off" }, c("dht_lookups"), c("dht_announces"));
    let pc = &p.process;
    let _ = writeln!(
        o,
        "PROCESS pid {}  memory {}  cpu {}  threads {}  open files/sockets {}",
        pc.pid,
        pc.rss_bytes.map_or("?".into(), human::bytes),
        pc.cpu_percent.map_or("?".into(), |c| format!("{c:.1}%")),
        pc.threads.map_or("?".into(), |t| t.to_string()),
        pc.open_fds.map_or("?".into(), |t| t.to_string())
    );
    if !p.torrents.is_empty() {
        let _ = writeln!(o, "\nACTIVE TORRENTS");
        let _ = writeln!(o, "{:<8}  {:<12} {:>10} {:>10} {:>5} {:>9} {:>4}  NAME", "ID", "STATE", "DOWN", "UP", "PEERS", "LEFT", "Q");
        for t in &p.torrents {
            let _ = writeln!(
                o,
                "{:<8}  {:<12} {:>10} {:>10} {:>5} {:>9} {:>4}  {}{}{}",
                short(&t.id),
                t.state,
                if t.down_rate > 0 { human::rate(t.down_rate) } else { "-".into() },
                if t.up_rate > 0 { human::rate(t.up_rate) } else { "-".into() },
                t.peers,
                human::bytes(t.left_bytes),
                t.queue_pos,
                t.name.chars().take(48).collect::<String>(),
                if t.stalled { "  [STALLED]" } else { "" },
                t.waiting_note.as_deref().map(|n| format!("  ({n})")).unwrap_or_default()
            );
        }
    }
    o.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn parses_every_debug_command() {
        assert_eq!(parse(w("health")).unwrap(), Cmd::Health { watch: false });
        assert_eq!(parse(w("health --watch")).unwrap(), Cmd::Health { watch: true });
        assert!(matches!(parse(w("trackers sintel --tracker tl")).unwrap(), Cmd::Trackers { torrent: Some(_), tracker: Some(_), .. }));
        assert!(matches!(parse(w("trackers log --follow --errors --since 10m")).unwrap(), Cmd::TrackersLog { log } if log.follow && log.errors && log.since_secs == Some(600)));
        assert!(matches!(parse(w("announce sintel --tracker tl")).unwrap(), Cmd::Announce { tracker: Some(_), .. }));
        assert!(matches!(parse(w("peers sintel --redact -f")).unwrap(), Cmd::Peers { redact: true, watch: true, .. }));
        assert!(matches!(parse(w("perf")).unwrap(), Cmd::Perf { watch: false }));
        for bad in ["", "frob", "peers", "peers a b", "announce", "health extra", "trackers --enable"] {
            assert!(parse(w(bad)).is_err(), "`debug {bad}` should be a usage error");
        }
    }
}
