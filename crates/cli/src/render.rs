//! Human-readable renderings of protocol results: the CLI's default output. The texts that the app
//! also shows (titles, the seeding goal in words, waiting notes) come from the engine, not from here.

use std::fmt::Write as _;

use trs_control::human;
use trs_control::types::*;

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn limit(l: Option<u64>) -> String {
    l.map_or_else(|| "unlimited".to_owned(), human::rate)
}

fn trunc(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}~")
}

/// `seedwell status`.
pub fn status(s: &StatusDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{}   agent permission: {}", s.server, s.permission);
    let _ = writeln!(
        o,
        "down {}   up {}   {} connections",
        human::rate(s.totals.down_rate),
        human::rate(s.totals.up_rate),
        s.totals.connections
    );
    let _ = writeln!(
        o,
        "limits: down {}, up {}{}",
        limit(s.limits.down_limit),
        limit(s.limits.up_limit),
        if s.limits.alt_active { "  (alternative speed ON)" } else { "" }
    );
    if let Some(r) = &s.reachability {
        let _ = writeln!(o, "port {}: {}", r.port, r.summary);
    }
    if s.total == 0 {
        o.push_str("no transfers");
    } else {
        let _ = write!(o, "{} transfers: {}", s.total, s.summary.rsplit("; ").next().unwrap_or(""));
    }
    o
}

/// A table of transfers.
pub fn list(l: &ListDto) -> String {
    if l.transfers.is_empty() {
        return if l.total == 0 { "No matching transfers.".into() } else { format!("No transfers on this page ({} match).", l.total) };
    }
    let mut o = String::new();
    let _ = writeln!(o, "{:<8}  {:<16}  {:>6}  {:>10}  {:>10}  {:>8}  NAME", "ID", "STATE", "DONE", "DOWN", "UP", "ETA");
    for t in &l.transfers {
        let state = match (&t.pending, t.kind.as_str()) {
            (Some(p), _) if p.paused => "pending (paused)".to_owned(),
            (Some(p), _) => format!("pending {}s", p.starts_in_secs),
            (None, "direct") => format!("{} (dl)", t.state),
            _ => t.state.clone(),
        };
        let _ = writeln!(
            o,
            "{:<8}  {:<16}  {:>6}  {:>10}  {:>10}  {:>8}  {}",
            t.short_id(),
            trunc(&state, 16),
            human::percent(t.progress).replace(".0%", "%"),
            if t.down_rate > 0 { human::rate(t.down_rate) } else { "-".into() },
            if t.up_rate > 0 { human::rate(t.up_rate) } else { "-".into() },
            t.eta_secs.map_or("-".into(), human::duration),
            trunc(shown_name(t), 70)
        );
    }
    let shown = l.transfers.len() as u32;
    if l.total > shown + l.offset || l.offset > 0 {
        let _ = write!(o, "showing {}-{} of {} (use --offset/--limit to page)", l.offset + 1, l.offset + shown, l.total);
    } else {
        o.pop(); // trailing newline
    }
    o
}

/// The name to show: the engine's clean title when it has one, the raw name otherwise.
fn shown_name(t: &TransferDto) -> &str {
    if t.title.is_empty() { &t.name } else { &t.title }
}

/// One-line description of a transfer, used in summaries.
pub fn line(t: &TransferDto) -> String {
    format!("{} {} [{}] {} {}", t.short_id(), shown_name(t), t.state, human::percent(t.progress), if t.down_rate > 0 { human::rate(t.down_rate) } else { String::new() })
        .trim_end()
        .to_owned()
}

/// `seedwell info`.
pub fn info(d: &DetailDto) -> String {
    let t = &d.transfer;
    let mut o = String::new();
    let _ = writeln!(o, "{}", shown_name(t));
    if !t.title.is_empty() && t.title != t.name {
        let _ = writeln!(o, "  name      {}", t.name);
    }
    let _ = writeln!(o, "  id        {}", t.id);
    let _ = writeln!(o, "  kind      {}{}", t.kind, if t.private { "  (private)" } else { "" });
    let _ = writeln!(o, "  state     {}   {} of {}", t.state, human::bytes(t.done_bytes), human::bytes(t.wanted_bytes));
    if let Some(e) = &t.error {
        let _ = writeln!(o, "  error     {e}");
    }
    if let Some(n) = &t.waiting_note {
        let _ = writeln!(o, "  waiting   {n}");
    }
    let _ = writeln!(
        o,
        "  speed     down {}, up {}   {} ({} seeds){}",
        human::rate(t.down_rate),
        human::rate(t.up_rate),
        peers_text(t.peers, t.peers_known),
        t.seeds,
        t.eta_secs.map_or(String::new(), |e| format!("   eta {}", human::duration(e)))
    );
    let _ = writeln!(
        o,
        "  ratio     {:.2}   uploaded {}   downloaded {}",
        t.ratio,
        human::bytes(t.uploaded_total),
        human::bytes(t.downloaded_total)
    );
    let _ = writeln!(
        o,
        "  setup     priority {}, category {}, tracker {}{}, tier {}",
        t.priority,
        t.category.as_deref().unwrap_or("-"),
        if t.tracker_host.is_empty() { "-" } else { &t.tracker_host },
        t.tracker_profile.as_deref().map_or(String::new(), |p| format!(" (profile {p})")),
        t.tier
    );
    let goal = if t.seed_goal.text.is_empty() { "seed forever".to_owned() } else { lower_first(&t.seed_goal.text) };
    let goal = format!(
        "{goal}{}{}",
        if t.seed_goal_user { "  (set for this torrent; `seedwell goal --rule-default` returns to the rule)" } else { "  (from the rule)" },
        if t.seeded_secs > 0 { format!("   seeded {}", human::duration(t.seeded_secs)) } else { String::new() }
    );
    let _ = writeln!(o, "  goal      {goal}");
    if let Some(i) = &t.idle {
        let _ = writeln!(o, "  idle      {}{}", i, t.idle_stopped.as_deref().map_or(String::new(), |s| format!("  - stopped: {s}")));
    }
    // Where the files are now: placed or organized files can live outside the save path, so the
    // folder all (non-padding) files share wins when the file list is known.
    let _ = writeln!(o, "  path      {}", common_dir(d.files.iter().filter(|f| !f.padding).map(|f| f.path.as_str())).unwrap_or(&t.save_path));
    if let Some(u) = &t.url {
        let _ = writeln!(o, "  url       {u}");
    }
    let _ = writeln!(
        o,
        "  added     {}{}",
        human::ago(now(), t.added_at),
        t.finished_at.map_or(String::new(), |f| format!(", finished {}", human::ago(now(), f)))
    );
    if let Some(c) = &d.comment {
        let _ = writeln!(o, "  comment   {}", trunc(c, 120));
    }
    if !d.files.is_empty() {
        let _ = writeln!(o, "Files ({}{}):", d.files.len(), if d.files_truncated > 0 { format!(" of {}", d.files.len() as u32 + d.files_truncated) } else { String::new() });
        for f in d.files.iter().filter(|f| !f.padding) {
            let pct = if f.length == 0 { 100.0 } else { f.done as f64 * 100.0 / f.length as f64 };
            let _ = writeln!(o, "  [{:>3}] {:<6} {:>5.1}% {:>9}  {}", f.index, f.priority, pct, human::bytes(f.length), f.path);
        }
    }
    if !d.trackers.is_empty() {
        let _ = writeln!(o, "Trackers ({}):", d.trackers.len());
        for tr in &d.trackers {
            let _ = writeln!(
                o,
                "  tier {} {}  {}{}",
                tr.tier,
                tr.url,
                tr.status,
                match (tr.seeds, tr.leechers) {
                    (Some(s), Some(l)) => format!("  ({s} seeders, {l} leechers)"),
                    _ => String::new(),
                }
            );
        }
    }
    if !d.peers.is_empty() {
        let _ = writeln!(o, "Peers ({}{}):", d.peers.len(), if d.peers_truncated > 0 { format!(" of {}", d.peers.len() as u32 + d.peers_truncated) } else { String::new() });
        for p in &d.peers {
            let _ = writeln!(
                o,
                "  {:<22} {:<18} down {:>10} up {:>10} {:>5.1}%{}",
                p.addr,
                trunc(&p.client, 18),
                human::rate(p.down_rate),
                human::rate(p.up_rate),
                p.progress * 100.0,
                if p.outgoing { "" } else { "  (incoming)" }
            );
        }
    }
    o.trim_end().to_owned()
}

/// Result of a bulk command.
pub fn action(a: &ActionDto) -> String {
    match a.transfers.as_slice() {
        [] => format!("{}: nothing", a.action),
        [one] => format!("{}: {} ({})", a.action, one.name, short(&one.id)),
        many => {
            let names: Vec<_> = many.iter().take(5).map(|t| trunc(&t.name, 40)).collect();
            format!("{}: {} transfers ({}{})", a.action, a.count, names.join(", "), if many.len() > 5 { ", ..." } else { "" })
        }
    }
}

/// `seedwell add`.
pub fn added(a: &AddedDto) -> String {
    let mut s = format!("added {} {} ({})", a.kind, a.name, short(&a.id));
    if let Some(p) = &a.pending {
        s.push_str(&format!(
            "\nHeld for {} s, then it starts by itself (nothing is announced or downloaded meanwhile). `seedwell start {id}` starts it now, `seedwell cancel {id}` dismisses it.",
            p.starts_in_secs,
            id = short(&a.id)
        ));
    }
    if a.needs_choice {
        s.push_str(&format!(
            "\nHeld until you choose what to download (nothing is announced or downloaded yet). `seedwell choice {id}` shows what is in it; `seedwell start {id}` (or `--files 0-9,12` / `--skip 3,5`) starts it; `seedwell cancel {id}` dismisses it.",
            id = short(&a.id)
        ));
        if let Some(c) = &a.choice {
            s.push('\n');
            s.push_str(&choice(c));
        }
    }
    s
}

/// `seedwell cancel`: what was dismissed and what happened to the `.torrent` file.
pub fn cancelled(v: &serde_json::Value) -> String {
    let name = v["transfers"][0]["name"].as_str().unwrap_or("torrent");
    match v["torrent_file"].as_str() {
        Some("trashed") => format!(concat!("cancelled: {}; its .torrent file ({}) is in the ", trs_platform::trash!()), name, v["trashed_file"].as_str().unwrap_or("?")),
        Some("kept") => format!("cancelled: {name}; the .torrent file was kept"),
        _ => format!("cancelled: {name}"),
    }
}

/// `seedwell files` without options.
pub fn files(d: &DetailDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{}", shown_name(&d.transfer));
    for f in d.files.iter().filter(|f| !f.padding) {
        let pct = if f.length == 0 { 100.0 } else { f.done as f64 * 100.0 / f.length as f64 };
        let _ = writeln!(o, "  [{:>3}] {:<6} {:>5.1}% {:>9}  {}", f.index, f.priority, pct, human::bytes(f.length), f.path);
    }
    o.trim_end().to_owned()
}

/// `8 / 57 peers`: connected over the peer addresses known; always both numbers.
pub fn peers_text(connected: u32, known: u32) -> String {
    format!("{connected} / {} peers", known.max(connected))
}

/// `seedwell reannounce` / `debug announce`.
pub fn reannounce(v: &serde_json::Value) -> String {
    let mut s = String::new();
    for t in v["transfers"].as_array().into_iter().flatten() {
        s.push_str(&format!("{}: {}\n", t["name"].as_str().unwrap_or("?"), t["note"].as_str().unwrap_or("")));
        for tr in t["trackers"].as_array().into_iter().flatten() {
            s.push_str(&format!(
                "  {}  {}{}{}\n",
                tr["url"].as_str().unwrap_or(""),
                tr["status"].as_str().unwrap_or(""),
                tr["not_before"].as_u64().map_or(String::new(), |t| format!("  (next allowed {})", clock(t))),
                tr["answer"].as_str().map_or(String::new(), |a| format!("\n    answer: {a}"))
            ));
        }
    }
    s.trim_end().to_owned()
}

/// "in 4m 10s" or "now" for a Unix time in seconds.
pub fn clock(unix: u64) -> String {
    let n = now();
    if unix > n { format!("in {}", human::duration(unix - n)) } else { "now".into() }
}

/// `seedwell choice`: the structure of a held (or running) torrent.
pub fn choice(c: &ChoiceDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{}", c.summary);
    if !c.rule_summary.is_empty() {
        let _ = writeln!(o, "{}", c.rule_summary);
    }
    for r in &c.reasons {
        let _ = writeln!(o, "  ask because: {}", r.text);
    }
    for g in &c.groups {
        let _ = writeln!(o, "{} ({})", g.label, g.items.len());
        for i in g.items.iter().take(60) {
            let mark = if i.selected { "[x]" } else { "[ ]" };
            let lib = match i.library.as_str() {
                "in_library" => "  in library",
                "other_quality" => "  in library (other quality)",
                _ => "",
            };
            let q = i.quality.as_deref().map(|q| format!("  {q}")).unwrap_or_default();
            let mac = if i.this_mac { concat!("  <- this ", trs_platform::computer!()) } else { "" };
            let first = i.files.first().map(|f| format!("#{f}")).unwrap_or_default();
            let _ = writeln!(o, "  {mark} {:<40} {:>9}  files {first}{}{q}{lib}{mac}", trunc(&i.label, 40), human::bytes(i.bytes), if i.files.len() > 1 { format!("+{}", i.files.len() - 1) } else { String::new() });
        }
        if g.items.len() > 60 {
            let _ = writeln!(o, "  ... {} more", g.items.len() - 60);
        }
    }
    o.trim_end().to_owned()
}

/// `seedwell moves`: what a location change would move.
pub fn proposal(m: &MoveProposalDto) -> String {
    if m.items.is_empty() {
        return "Everything is where the settings say.".into();
    }
    let mut o = format!("{} torrent(s), {}, {} across volumes\n", m.items.len(), human::bytes(m.total_bytes), m.cross_volume);
    for i in m.items.iter().take(40) {
        let _ = writeln!(o, "{}  {:<40} {:>9}  {} -> {}  [{}{}]", short(&i.id), trunc(&i.name, 40), human::bytes(i.bytes), i.from, i.to, i.kind, if i.cross_volume { ", copy" } else { "" });
    }
    o.trim_end().to_owned()
}

/// `seedwell move`: what was queued.
pub fn move_report(m: &MoveReportDto) -> String {
    let mut o = format!("queued {} move(s)", m.queued.len());
    for r in &m.refused {
        let _ = write!(o, "\nnot moved {}: {}", short(&r.id), r.reason);
    }
    o
}

/// `seedwell limits`.
pub fn limits(l: &LimitsDto) -> String {
    format!(
        "download limit {}, upload limit {}\nalternative speed {} (down {}, up {}){}",
        limit(l.down_limit),
        limit(l.up_limit),
        if l.alt_active { "ON" } else { "off" },
        limit(l.alt_down_limit),
        limit(l.alt_up_limit),
        if l.alt_scheduled { ", switched on by the schedule" } else { "" }
    )
}

/// `seedwell tracker-limits`.
pub fn tracker_limits(v: &serde_json::Value) -> String {
    format!(
        "{}: download cap {}, upload cap {} (all of its torrents together)",
        v["id"].as_str().unwrap_or("?"),
        limit(v["down_limit"].as_u64()),
        limit(v["up_limit"].as_u64())
    )
}

/// `seedwell trackers`.
pub fn trackers(t: &TrackersDto) -> String {
    let mut o = String::new();
    if t.profiles.is_empty() && t.groups.is_empty() {
        let _ = writeln!(o, "No trackers yet.");
    }
    for p in t.profiles.iter().chain(&t.groups) {
        let _ = writeln!(
            o,
            "{:<16} {}{}  {} transfers ({} sharing), up {}, down {}{}\n{:<16} {}",
            p.id,
            p.name,
            if p.favorite { "  [favorite]" } else { "" },
            p.transfers,
            p.sharing,
            human::rate(p.up_rate),
            human::rate(p.down_rate),
            caps_and_pex(p, t.pex_enabled),
            "",
            p.urls.join(", ")
        );
    }
    let _ = write!(
        o,
        "public (no tracker): {}   other private: {}{}",
        t.public_transfers,
        t.other_private_transfers,
        pex_tag(t.other_private_transfers > 0, t.other_private_allow_pex, t.pex_enabled)
    );
    if !t.pex_enabled {
        let _ = write!(o, "\nPeer exchange is off in Settings > Network: no torrent uses it, whatever the trackers allow.");
    }
    o
}

fn caps_and_pex(p: &ProfileDto, master: bool) -> String {
    let caps = match (p.down_limit, p.up_limit) {
        (None, None) => String::new(),
        (d, u) => format!("  [cap down {}, up {}]", limit(d), limit(u)),
    };
    format!("{caps}{}", pex_tag(p.private, p.allow_pex, master))
}

/// The peer exchange marker of a private tracker (nothing for a public one).
fn pex_tag(private: bool, allowed: bool, master: bool) -> String {
    match (private, allowed, master) {
        (false, ..) => String::new(),
        (true, false, _) => "  [peer exchange off]".into(),
        (true, true, true) => "  [peer exchange allowed]".into(),
        (true, true, false) => "  [peer exchange allowed, but off in Settings > Network]".into(),
    }
}

/// `seedwell tracker <name> pex`.
pub fn tracker_pex(v: &serde_json::Value) -> String {
    let id = v["id"].as_str().unwrap_or("?");
    let allowed = v["allow_pex"].as_bool().unwrap_or(false);
    let master = v["pex_enabled"].as_bool().unwrap_or(true);
    let mut o = format!("{id}: peer exchange {}", if allowed { "allowed" } else { "off" });
    if allowed && !master {
        o.push_str(" (but off in Settings > Network, so not used)");
    }
    if !allowed {
        o.push_str("\nMost private trackers forbid peer exchange; turn it on only if this tracker's rules allow it.");
    }
    o
}

fn bucket(b: &BucketDto) -> String {
    format!(
        "up {:>9}  down {:>9}  ratio {}  completed {}",
        human::bytes(b.uploaded),
        human::bytes(b.downloaded),
        b.ratio.map_or("-".into(), |r| format!("{r:.2}")),
        b.torrents_completed
    )
}

/// `seedwell stats`.
pub fn stats(s: &TrackerStatsDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "Statistics for {}", s.tracker);
    let _ = writeln!(o, "  today       {}", bucket(&s.today));
    let _ = writeln!(o, "  this week   {}", bucket(&s.this_week));
    let _ = writeln!(o, "  this month  {}", bucket(&s.this_month));
    let _ = writeln!(o, "  all time    {}", bucket(&s.all_time));
    let active: Vec<&DayDto> = s.series.iter().filter(|d| d.bucket.uploaded > 0 || d.bucket.downloaded > 0).collect();
    if !active.is_empty() {
        let _ = writeln!(o, "Daily ({} days, active days shown):", s.series.len());
        for d in active.iter().rev().take(14).rev() {
            let _ = writeln!(o, "  {}  up {:>9}  down {:>9}", d.date, human::bytes(d.bucket.uploaded), human::bytes(d.bucket.downloaded));
        }
    }
    o.trim_end().to_owned()
}

/// A remove plan.
pub fn plan(p: &RemovePlanDto) -> String {
    let mut o = String::new();
    for i in &p.items {
        let _ = writeln!(o, "  {} {} ({} on disk) at {}", short(&i.id), i.name, human::bytes(i.bytes_on_disk), i.path);
    }
    let _ = write!(o, "{}", p.summary);
    o
}

/// After a removal.
pub fn removed(r: &RemovedDto) -> String {
    let mut s = format!("removed {} transfer(s){}", r.removed.len(), if r.files_deleted { " and deleted their files" } else { " (files kept)" });
    if !r.missing.is_empty() {
        let _ = write!(s, "; {} were already gone", r.missing.len());
    }
    s
}

/// Events: one line each.
pub fn event(e: &EventDto) -> String {
    let t = e.at / 1000;
    let (h, m, s) = ((t / 3600) % 24, (t / 60) % 60, t % 60);
    let what = match (&e.name, &e.id) {
        (Some(n), _) => n.clone(),
        (None, Some(id)) => short(id).to_owned(),
        _ => String::new(),
    };
    let extra = match &e.data {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Object(o) => o
            .iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::String(s) => Some(format!("{k}={s}")),
                serde_json::Value::Number(n) => Some(format!("{k}={n}")),
                serde_json::Value::Bool(b) => Some(format!("{k}={b}")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    };
    format!("{h:02}:{m:02}:{s:02}Z  #{:<5} {:<20} {} {}", e.seq, e.kind, what, extra).trim_end().to_owned()
}

/// `tracker_log`: one line per entry, newest last.
pub fn tracker_log(v: &serde_json::Value) -> String {
    let entries = v["entries"].as_array().map_or(&[][..], Vec::as_slice);
    if entries.is_empty() {
        return if v["enabled"] == false {
            "Developer mode is off, so no tracker exchanges are logged. The user can turn it on in Settings > Advanced.".into()
        } else {
            "No tracker log entries match.".into()
        };
    }
    entries
        .iter()
        .map(|e| {
            let r = trs_devlog::view::Row::from_value(e);
            let t = r.ts_ms / 1000;
            format!(
                "{:02}:{:02}:{:02}Z  {}  {}  {}{}",
                (t / 3600) % 24,
                (t / 60) % 60,
                t % 60,
                r.id,
                trunc(&r.host, 28),
                r.summary,
                if r.error { "  [FAILED]" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Activity log entries.
pub fn activity(v: &[ActivityDto]) -> String {
    if v.is_empty() {
        return "No agent activity recorded.".into();
    }
    v.iter()
        .map(|a| {
            let t = a.at / 1000;
            format!("{:02}:{:02}:{:02}Z  {:<14} {:<8} {}", (t / 3600) % 24, (t / 60) % 60, t % 60, trunc(&a.client, 14), a.outcome, a.summary)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Wait results.
pub fn wait(w: &WaitDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{}", if w.done { "done" } else if w.timed_out { "timed out, still in progress" } else { "waiting" });
    for t in &w.transfers {
        let _ = writeln!(o, "  {}", line(t));
    }
    o.trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str, state: &str) -> TransferDto {
        let mut t = TransferDto { id: "ab".repeat(20), kind: "torrent".into(), name: name.into(), state: state.into(), progress: 0.425, down_rate: 1_200_000, ..Default::default() };
        t.fill_human();
        t
    }

    #[test]
    fn list_table_has_header_and_rows() {
        let l = ListDto { total: 1, offset: 0, transfers: vec![t("Ubuntu", "downloading")] };
        let s = list(&l);
        assert!(s.starts_with("ID "), "{s}");
        assert!(s.contains("abababab") && s.contains("downloading") && s.contains("42.5%") && s.contains("1.2 MB/s") && s.contains("Ubuntu"), "{s}");
        assert_eq!(list(&ListDto::default()), "No matching transfers.");
    }

    #[test]
    fn long_names_are_cut() {
        assert_eq!(trunc("abcdef", 4), "abc~");
        assert_eq!(trunc("åäö", 5), "åäö");
    }

    #[test]
    fn actions_and_events() {
        let a = ActionDto { action: "paused".into(), count: 1, transfers: vec![IdName { id: "ab".repeat(20), name: "X".into() }] };
        assert_eq!(action(&a), "paused: X (abababab)");
        let e = EventDto { seq: 3, at: 3_723_000, kind: "finished".into(), name: Some("X".into()), ..Default::default() };
        assert_eq!(event(&e), "01:02:03Z  #3     finished             X");
    }

    #[test]
    fn info_renders_sections() {
        let d = DetailDto {
            transfer: t("Ubuntu", "seeding"),
            files: vec![FileDto { index: 0, path: "a.iso".into(), length: 100, done: 100, priority: "normal".into(), padding: false }],
            trackers: vec![TrackerDto { url: "https://t.org/a/***/announce".into(), tier: 0, status: "announced".into(), ..Default::default() }],
            ..Default::default()
        };
        let s = info(&d);
        assert!(s.contains("Files (1):") && s.contains("Trackers (1):") && s.contains("a.iso"), "{s}");
    }
}

/// The deepest directory that contains every path (`None` for no paths or nothing in common).
fn common_dir<'a>(mut paths: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let first = paths.next()?;
    let mut end = first.rfind('/')?;
    for p in paths {
        while !(p.len() > end && p.as_bytes()[end] == b'/' && p[..end] == first[..end]) {
            end = first[..end].rfind('/')?;
        }
    }
    (end > 0).then(|| &first[..end])
}

#[cfg(test)]
mod common_dir_tests {
    use super::common_dir;

    #[test]
    fn finds_the_shared_folder() {
        assert_eq!(common_dir(["/a/b/c.iso"].into_iter()), Some("/a/b"));
        assert_eq!(common_dir(["/a/b/x/1", "/a/b/y/2"].into_iter()), Some("/a/b"));
        assert_eq!(common_dir(["/a/bc/1", "/a/b/2"].into_iter()), Some("/a"));
        assert_eq!(common_dir(["/x/1", "/y/2"].into_iter()), None);
        assert_eq!(common_dir(std::iter::empty()), None);
    }
}

/// "Seed until less than 1 MB" -> "seed until less than 1 MB": only the first letter, so units survive.
fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_lowercase().chain(c).collect())
}

#[cfg(test)]
mod peers_tests {
    use super::peers_text;

    #[test]
    fn connected_over_swarm() {
        assert_eq!(peers_text(8, 57), "8 / 57 peers");
        assert_eq!(peers_text(0, 0), "0 / 0 peers");
        assert_eq!(peers_text(5, 3), "5 / 5 peers");
    }
}
