//! Parsing: argument words into a [`Cmd`]. No I/O happens here, so every command, every help
//! example and the skill's examples are tested by parsing them.
#![allow(missing_docs)] // variant fields are named after the options they hold


use trs_control::proto::{AtGoalParam, FilePriorityParam, KindFilter, ListFilter, PriorityParam, SetLimitsParams};

use crate::args::{Opts, Usage, parse_duration, parse_index_list, parse_rate};
use crate::select::{Selection, parse_state};
use crate::{chat, debug, mail, sim, skill, sync};

/// What `start` does with the files.
#[derive(Clone, Debug, PartialEq)]
pub enum StartMode {
    /// Every file ticked, as Start does in the app.
    All,
    /// The engine's preselection (what the library does not have, this Mac's build, ...).
    Suggested,
    /// Only these files.
    Files(Vec<u32>),
    /// All but these.
    Skip(Vec<u32>),
}

/// `list` arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ListArgs {
    /// State filter.
    pub filter: ListFilter,
    /// Tracker host or favorite.
    pub tracker: Option<String>,
    /// Category.
    pub category: Option<String>,
    /// Name substring.
    pub text: Option<String>,
    /// Torrent or direct.
    pub kind: Option<KindFilter>,
    /// Page size.
    pub limit: Option<u32>,
    /// Rows to skip.
    pub offset: Option<u32>,
}

/// `add` arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AddArgs {
    /// Sources.
    pub sources: Vec<String>,
    /// `--save-path`.
    pub save_path: Option<String>,
    /// `--paused`.
    pub paused: bool,
    /// `--category`.
    pub category: Option<String>,
    /// `--priority`.
    pub priority: Option<PriorityParam>,
    /// `--rule`.
    pub rule: Option<String>,
    /// `--top`.
    pub queue_top: bool,
    /// `--now`.
    pub now: bool,
    /// `--sha256`.
    pub sha256: Option<String>,
    /// `--peer`.
    pub peers: Vec<String>,
}

/// `goal` arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GoalArgs {
    /// `--ratio`.
    pub ratio: Option<f32>,
    /// `--time` in seconds.
    pub time: Option<u64>,
    /// `--min-time` in seconds.
    pub min_time: Option<u64>,
    /// `--all-conditions`.
    pub all_conditions: bool,
    /// `--stop-on-finish`.
    pub stop_on_finish: bool,
    /// `--forever`.
    pub forever: bool,
    /// `--rule-default`.
    pub rule_default: bool,
    /// `--at-goal`.
    pub at_goal: Option<AtGoalParam>,
}

/// `log trackers` arguments (also used by `debug trackers log`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LogArgs {
    /// `--follow`.
    pub follow: bool,
    /// `--errors`.
    pub errors: bool,
    /// `--torrent`.
    pub torrent: Option<String>,
    /// `--tracker`.
    pub tracker: Option<String>,
    /// `--event`.
    pub event: Option<String>,
    /// `--limit`.
    pub limit: Option<u32>,
    /// `--since` in seconds.
    pub since_secs: Option<u64>,
}

/// What `seedwell dev` does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevAction {
    /// Switch Developer mode on.
    On,
    /// Switch it off.
    Off,
    /// Show whether it is on, and the log.
    Status,
}

/// One command.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// `status`
    Status,
    /// `list`
    List(ListArgs),
    /// `info`
    Info { sel: Selection, files: bool, peers: bool, trackers: bool },
    /// `add`
    Add(AddArgs),
    /// `start`
    Start { sel: Selection, mode: StartMode },
    /// `cancel`
    Cancel { sel: Selection, keep_file: bool },
    /// `choice`
    Choice { sel: Selection },
    /// `pause`
    Pause { sel: Selection },
    /// `resume`
    Resume { sel: Selection },
    /// `top`
    Top { sel: Selection },
    /// `priority`
    Priority { sel: Selection, level: PriorityParam },
    /// `category`
    Category { sel: Selection, category: Option<String> },
    /// `files`
    Files { sel: Selection, set: Vec<(Vec<u32>, FilePriorityParam)>, only: Option<Vec<u32>> },
    /// `goal`
    Goal { sel: Selection, goal: GoalArgs },
    /// `reannounce`
    Reannounce { sel: Selection, force: bool, wait: bool },
    /// `recheck`
    Recheck { sel: Selection },
    /// `remove`
    Remove { sel: Selection, delete_files: bool, yes: bool },
    /// `remove-confirm`
    RemoveConfirm { token: String },
    /// `moves`
    Moves,
    /// `move`
    Move { sel: Selection, path: Option<String> },
    /// `cancel-move`
    CancelMove { sel: Selection },
    /// `stream`
    Stream { sel: Selection, file: Option<u32> },
    /// `stop-stream`
    StopStream { token: String },
    /// `limits`
    Limits(SetLimitsParams),
    /// `trackers`
    Trackers,
    /// `tracker <name> limits`
    TrackerLimits { name: String, down: Option<u64>, up: Option<u64> },
    /// `tracker <name> pex [on|off]`: show or set peer exchange for a private tracker.
    TrackerPex { name: String, set: Option<bool> },
    /// `stats`
    Stats { tracker: Option<String>, days: Option<u32> },
    /// `rules`
    Rules,
    /// `settings get`
    SettingsGet { keys: Vec<String> },
    /// `settings set`
    SettingsSet { pairs: Vec<(String, String)> },
    /// `wait`
    Wait { sel: Selection, any: bool, timeout: u32 },
    /// `watch`
    Watch { kinds: Option<Vec<String>>, count: Option<u64>, for_secs: Option<u64> },
    /// `activity`
    Activity { limit: Option<u32> },
    /// `log trackers`
    LogTrackers(LogArgs),
    /// `dev on|off|status`
    Dev(DevAction),
    /// `debug …`
    Debug(debug::Cmd),
    /// `sim …`
    Sim(sim::Cmd),
    /// `skill …`
    Skill(skill::Cmd),
    /// `sync …` (folder sync)
    Sync(sync::Cmd),
    /// `chat …` (channels)
    Chat(chat::Cmd),
    /// `mail …` (agents of different projects)
    Mail(mail::Mail),
    /// `relay [--port N] [--bind ADDR] [--key-file PATH]`
    Relay(sync::RelayArgs),
    /// `help [x]`
    Help(Option<String>),
    /// `daemon …` (parsed by the daemon itself)
    Daemon(Vec<String>),
}

fn no_more(pos: &[String]) -> Result<(), Usage> {
    match pos.first() {
        None => Ok(()),
        Some(x) => Err(Usage(format!("unexpected argument `{x}`"))),
    }
}

fn exactly_one(mut pos: Vec<String>, usage: &str) -> Result<String, Usage> {
    if pos.len() != 1 {
        return Err(Usage(format!("usage: seedwell {usage}")));
    }
    Ok(pos.remove(0))
}

/// `high|normal|low`.
pub fn parse_priority(p: &str) -> Result<PriorityParam, Usage> {
    match p.to_ascii_lowercase().as_str() {
        "high" => Ok(PriorityParam::High),
        "normal" => Ok(PriorityParam::Normal),
        "low" => Ok(PriorityParam::Low),
        x => Err(Usage(format!("priority is high, normal or low, not `{x}`"))),
    }
}

fn selection(mut o: Opts, what: &str) -> Result<(Selection, Opts), Usage> {
    let sel = Selection::take(&mut o)?;
    let _ = what;
    Ok((sel, o))
}

fn sel_cmd(o: Opts, what: &str) -> Result<Selection, Usage> {
    let (sel, o) = selection(o, what)?;
    let sel = sel.with_terms(o.finish()?);
    sel.validate(what)?;
    Ok(sel)
}

/// Words after `seedwell` (global options already taken out) into a command.
pub fn parse(args: Vec<String>) -> Result<Cmd, Usage> {
    let mut it = args.into_iter();
    let name = it.next().ok_or_else(|| Usage("no command; try `seedwell help`".into()))?;
    let rest: Vec<String> = it.collect();
    let mut o = Opts::new(rest.clone());
    Ok(match name.as_str() {
        "status" => {
            no_more(&o.finish()?)?;
            Cmd::Status
        }
        "list" | "ls" => {
            let tracker = o.value(&["--tracker"])?;
            let category = o.value(&["--category"])?;
            let text = o.value(&["--text", "--search"])?;
            let kind = o.value(&["--kind"])?;
            let limit = o.number::<u32>(&["--limit"])?;
            let offset = o.number::<u32>(&["--offset"])?;
            let state = o.value(&["--state", "--filter"])?;
            let mut pos = o.finish()?;
            let state = state.or_else(|| (!pos.is_empty()).then(|| pos.remove(0)));
            no_more(&pos)?;
            let filter = match state {
                None => ListFilter::All,
                Some(f) => parse_state(&f)?,
            };
            let kind = match kind.as_deref() {
                None => None,
                Some("torrent") => Some(KindFilter::Torrent),
                Some("direct") => Some(KindFilter::Direct),
                Some(k) => return Err(Usage(format!("--kind is torrent or direct, not `{k}`"))),
            };
            Cmd::List(ListArgs { filter, tracker, category, text, kind, limit, offset })
        }
        "info" | "show" => {
            let no_files = o.flag(&["--no-files"]);
            let no_peers = o.flag(&["--no-peers"]);
            let no_trackers = o.flag(&["--no-trackers"]);
            Cmd::Info { sel: sel_cmd(o, "info")?, files: !no_files, peers: !no_peers, trackers: !no_trackers }
        }
        "add" => {
            let save_path = o.value(&["--save-path", "--dir", "-d"])?;
            let paused = o.flag(&["--paused"]);
            let category = o.value(&["--category"])?;
            let rule = o.value(&["--rule"])?;
            let queue_top = o.flag(&["--top"]);
            let now = o.flag(&["--now"]);
            let sha256 = o.value(&["--sha256"])?;
            let peers = o.values(&["--peer"])?;
            let priority = o.value(&["--priority"])?.map(|p| parse_priority(&p)).transpose()?;
            let sources = o.finish()?;
            if sources.is_empty() {
                return Err(Usage("add needs a magnet link, a web address or a .torrent file".into()));
            }
            Cmd::Add(AddArgs { sources, save_path, paused, category, priority, rule, queue_top, now, sha256, peers })
        }
        "start" => {
            let suggested = o.flag(&["--suggested", "--default"]);
            let files = o.value(&["--files"])?.map(|l| parse_index_list(&l)).transpose()?;
            let skip = o.value(&["--skip"])?.map(|l| parse_index_list(&l)).transpose()?;
            if u8::from(suggested) + u8::from(files.is_some()) + u8::from(skip.is_some()) > 1 {
                return Err(Usage("give one of --suggested, --files (only these) or --skip (all but these)".into()));
            }
            let mode = match (files, skip) {
                (Some(f), _) => StartMode::Files(f),
                (_, Some(s)) => StartMode::Skip(s),
                _ if suggested => StartMode::Suggested,
                _ => StartMode::All,
            };
            Cmd::Start { sel: sel_cmd(o, "start")?, mode }
        }
        "cancel" => {
            let keep_file = o.flag(&["--keep-file"]);
            Cmd::Cancel { sel: sel_cmd(o, "cancel")?, keep_file }
        }
        "choice" => Cmd::Choice { sel: sel_cmd(o, "choice")? },
        "pause" => Cmd::Pause { sel: sel_cmd(o, "pause")? },
        "resume" => Cmd::Resume { sel: sel_cmd(o, "resume")? },
        "top" => Cmd::Top { sel: sel_cmd(o, "top")? },
        "priority" => {
            let (sel, o) = selection(o, "priority")?;
            let mut pos = o.finish()?;
            let level = pos.pop().ok_or_else(|| Usage("usage: seedwell priority <torrent>... high|normal|low".into()))?;
            let level = parse_priority(&level)?;
            let sel = sel.with_terms(pos);
            sel.validate("priority")?;
            Cmd::Priority { sel, level }
        }
        "category" => {
            let (sel, o) = selection(o, "category")?;
            let mut pos = o.finish()?;
            let c = pos.pop().ok_or_else(|| Usage("usage: seedwell category <torrent>... <movie|tv|music|audiobook|book|software|other|none>".into()))?;
            let sel = sel.with_terms(pos);
            sel.validate("category")?;
            Cmd::Category { sel, category: (c != "none").then_some(c) }
        }
        "files" => {
            let mut set = Vec::new();
            for (names, p) in [
                (&["--skip"][..], FilePriorityParam::Skip),
                (&["--want", "--normal"][..], FilePriorityParam::Normal),
                (&["--low"][..], FilePriorityParam::Low),
                (&["--high"][..], FilePriorityParam::High),
            ] {
                if let Some(l) = o.value(names)? {
                    set.push((parse_index_list(&l)?, p));
                }
            }
            let only = o.value(&["--only"])?.map(|l| parse_index_list(&l)).transpose()?;
            Cmd::Files { sel: sel_cmd(o, "files")?, set, only }
        }
        "goal" | "seed-goal" => {
            let g = GoalArgs {
                ratio: o.number::<f32>(&["--ratio"])?,
                time: o.value(&["--time"])?.map(|t| parse_duration(&t)).transpose()?,
                min_time: o.value(&["--min-time"])?.map(|t| parse_duration(&t)).transpose()?,
                all_conditions: o.flag(&["--all-conditions"]),
                stop_on_finish: o.flag(&["--stop-on-finish"]),
                forever: o.flag(&["--forever"]),
                rule_default: o.flag(&["--rule-default", "--clear"]),
                at_goal: match o.value(&["--at-goal"])?.as_deref() {
                    None => None,
                    Some("stop") => Some(AtGoalParam::Stop),
                    Some("remove") => Some(AtGoalParam::Remove),
                    Some("remove-and-delete") | Some("remove_and_delete") => Some(AtGoalParam::RemoveAndDelete),
                    Some(x) => return Err(Usage(format!("--at-goal is stop, remove or remove-and-delete, not `{x}`"))),
                },
            };
            Cmd::Goal { sel: sel_cmd(o, "goal")?, goal: g }
        }
        "reannounce" => {
            let force = o.flag(&["--force"]);
            let wait = o.flag(&["--wait"]);
            Cmd::Reannounce { sel: sel_cmd(o, "reannounce")?, force, wait }
        }
        "recheck" | "verify" => Cmd::Recheck { sel: sel_cmd(o, "recheck")? },
        "remove" | "rm" => {
            let delete_files = o.flag(&["--delete-files", "--delete"]);
            let yes = o.flag(&["-y", "--yes"]);
            Cmd::Remove { sel: sel_cmd(o, "remove")?, delete_files, yes }
        }
        "remove-confirm" => Cmd::RemoveConfirm { token: exactly_one(o.finish()?, "remove-confirm <token>")? },
        "moves" => {
            no_more(&o.finish()?)?;
            Cmd::Moves
        }
        "move" => {
            let to_settings = o.flag(&["--to-settings"]);
            let (sel, o) = selection(o, "move")?;
            let mut pos = o.finish()?;
            let path = if to_settings {
                None
            } else {
                Some(pos.pop().ok_or_else(|| Usage("usage: seedwell move <torrent>... <path>   (or: seedwell move <torrent>... --to-settings)".into()))?)
            };
            let sel = sel.with_terms(pos);
            sel.validate("move")?;
            Cmd::Move { sel, path }
        }
        "cancel-move" => Cmd::CancelMove { sel: sel_cmd(o, "cancel-move")? },
        "stream" => {
            let file = o.number::<u32>(&["--file", "-f"])?;
            Cmd::Stream { sel: sel_cmd(o, "stream")?, file }
        }
        "stop-stream" => Cmd::StopStream { token: exactly_one(o.finish()?, "stop-stream <token>")? },
        "limits" | "limit" => {
            let p = SetLimitsParams {
                down_limit: o.value(&["--down"])?.map(|v| parse_rate(&v)).transpose()?,
                up_limit: o.value(&["--up"])?.map(|v| parse_rate(&v)).transpose()?,
                alt_down_limit: o.value(&["--alt-down"])?.map(|v| parse_rate(&v)).transpose()?,
                alt_up_limit: o.value(&["--alt-up"])?.map(|v| parse_rate(&v)).transpose()?,
                alt_speed: match o.value(&["--alt"])?.as_deref() {
                    None => None,
                    Some("on") | Some("true") => Some(true),
                    Some("off") | Some("false") => Some(false),
                    Some(x) => return Err(Usage(format!("--alt is on or off, not `{x}`"))),
                },
            };
            no_more(&o.finish()?)?;
            Cmd::Limits(p)
        }
        "trackers" => {
            no_more(&o.finish()?)?;
            Cmd::Trackers
        }
        "tracker" | "tracker-limits" => {
            let down = o.value(&["--down"])?.map(|v| parse_rate(&v)).transpose()?;
            let up = o.value(&["--up"])?.map(|v| parse_rate(&v)).transpose()?;
            let mut pos = o.finish()?;
            if name == "tracker" && pos.len() >= 2 && pos[1] == "pex" {
                // `tracker <name> pex [on|off]`
                let usage = "usage: seedwell tracker <name> pex [on|off]";
                if down.is_some() || up.is_some() || pos.len() > 3 {
                    return Err(Usage(usage.into()));
                }
                let set = match pos.get(2).map(String::as_str) {
                    None => None,
                    Some("on" | "yes" | "true" | "allow") => Some(true),
                    Some("off" | "no" | "false" | "forbid") => Some(false),
                    Some(_) => return Err(Usage(usage.into())),
                };
                return Ok(Cmd::TrackerPex { name: pos.swap_remove(0), set });
            }
            if name == "tracker" {
                // `tracker <name> limits`
                if pos.len() != 2 || pos[1] != "limits" {
                    return Err(Usage("usage: seedwell tracker <name> limits [--down R] [--up R] | seedwell tracker <name> pex [on|off]".into()));
                }
                pos.pop();
            }
            Cmd::TrackerLimits { name: exactly_one(pos, "tracker <name> limits [--down R] [--up R]")?, down, up }
        }
        "stats" => {
            let days = o.number::<u32>(&["--days"])?;
            let mut pos = o.finish()?;
            let tracker = (!pos.is_empty()).then(|| pos.remove(0));
            no_more(&pos)?;
            Cmd::Stats { tracker, days }
        }
        "rules" => {
            no_more(&o.finish()?)?;
            Cmd::Rules
        }
        "settings" => {
            let pos = o.finish()?;
            let mut it = pos.into_iter();
            match it.next().as_deref() {
                None | Some("get") => Cmd::SettingsGet { keys: it.collect() },
                Some("set") => {
                    let mut pairs = Vec::new();
                    for kv in it {
                        let Some((k, v)) = kv.split_once('=') else {
                            return Err(Usage(format!("`{kv}` should look like key=value")));
                        };
                        pairs.push((k.to_owned(), v.to_owned()));
                    }
                    if pairs.is_empty() {
                        return Err(Usage("usage: seedwell settings set key=value...".into()));
                    }
                    Cmd::SettingsSet { pairs }
                }
                Some(x) => return Err(Usage(format!("settings: unknown subcommand `{x}` (get, set)"))),
            }
        }
        "wait" => {
            let any = o.flag(&["--any"]);
            let timeout = o.number::<u32>(&["--timeout"])?.unwrap_or(600);
            Cmd::Wait { sel: sel_cmd(o, "wait")?, any, timeout }
        }
        "watch" => {
            let kinds = o.value(&["--kinds"])?.map(|k| k.split(',').map(str::to_owned).collect::<Vec<_>>());
            let count = o.number::<u64>(&["--count"])?;
            let for_secs = o.number::<u64>(&["--for"])?;
            no_more(&o.finish()?)?;
            Cmd::Watch { kinds, count, for_secs }
        }
        "activity" => {
            let limit = o.number::<u32>(&["--limit"])?;
            no_more(&o.finish()?)?;
            Cmd::Activity { limit }
        }
        "log" => {
            let a = parse_log_args(&mut o)?;
            let pos = o.finish()?;
            if pos != ["trackers"] {
                return Err(Usage("usage: seedwell log trackers [--follow] [--torrent T] [--tracker HOST] [--event E] [--errors] [--limit N]".into()));
            }
            Cmd::LogTrackers(a)
        }
        "dev" => {
            let pos = o.finish()?;
            Cmd::Dev(match pos.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
                ["on"] => DevAction::On,
                ["off"] => DevAction::Off,
                [] | ["status"] => DevAction::Status,
                _ => return Err(Usage("usage: seedwell dev on|off|status".into())),
            })
        }
        "debug" => Cmd::Debug(debug::parse(rest)?),
        "sim" => Cmd::Sim(sim::parse(rest)?),
        "skill" => Cmd::Skill(skill::parse(rest)?),
        "sync" => Cmd::Sync(sync::parse(rest)?),
        "chat" => Cmd::Chat(chat::parse(rest)?),
        "mail" => Cmd::Mail(mail::parse(rest)?),
        "relay" => Cmd::Relay(sync::parse_relay(rest)?),
        "help" => {
            // Options an agent passes everywhere (`--as kglite`, `--channel C`) change nothing here.
            let _ = o.value(&["--as"])?;
            let _ = o.value(&["--channel"])?;
            let pos = o.finish()?;
            Cmd::Help(pos.into_iter().next())
        }
        "daemon" => Cmd::Daemon(rest),
        other => return Err(Usage(format!("unknown command `{other}`; try `seedwell help`"))),
    })
}

/// The options shared by `log trackers` and `debug trackers log`.
pub fn parse_log_args(o: &mut Opts) -> Result<LogArgs, Usage> {
    Ok(LogArgs {
        follow: o.flag(&["--follow", "-f"]),
        errors: o.flag(&["--errors"]),
        torrent: o.value(&["--torrent"])?,
        tracker: o.value(&["--tracker"])?,
        event: o.value(&["--event"])?,
        limit: o.number::<u32>(&["--limit", "-n"])?,
        since_secs: o.value(&["--since"])?.map(|s| parse_duration(&s)).transpose()?,
    })
}

/// A tiny shell-like splitter (quotes only), for the examples in the help text and the skill.
pub fn shell_words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Cmd, Usage> {
        let words = shell_words(s);
        assert_eq!(words[0], "seedwell");
        let mut w: Vec<String> = words[1..].to_vec();
        w.retain(|a| a != "--json");
        parse(w)
    }

    #[test]
    fn every_command_parses() {
        assert_eq!(p("seedwell status").unwrap(), Cmd::Status);
        assert!(matches!(p("seedwell list downloading --limit 5").unwrap(), Cmd::List(a) if a.filter == ListFilter::Downloading && a.limit == Some(5)));
        assert!(matches!(p("seedwell info sintel --no-peers").unwrap(), Cmd::Info { peers: false, files: true, .. }));
        assert!(matches!(p("seedwell add a.torrent b.torrent --paused --category tv").unwrap(), Cmd::Add(a) if a.sources.len() == 2 && a.paused));
        assert!(matches!(p("seedwell start sintel --files 0-2").unwrap(), Cmd::Start { mode: StartMode::Files(f), .. } if f == vec![0, 1, 2]));
        assert!(matches!(p("seedwell cancel sintel --keep-file").unwrap(), Cmd::Cancel { keep_file: true, .. }));
        assert!(matches!(p("seedwell choice sintel").unwrap(), Cmd::Choice { .. }));
        assert!(matches!(p("seedwell pause --all").unwrap(), Cmd::Pause { sel } if sel.is_plain_all()));
        assert!(matches!(p("seedwell resume sintel").unwrap(), Cmd::Resume { .. }));
        assert!(matches!(p("seedwell top sintel").unwrap(), Cmd::Top { .. }));
        assert!(matches!(p("seedwell priority sintel high").unwrap(), Cmd::Priority { level: PriorityParam::High, .. }));
        assert!(matches!(p("seedwell priority --state downloading low").unwrap(), Cmd::Priority { sel, .. } if sel.state.is_some()));
        assert!(matches!(p("seedwell category sintel none").unwrap(), Cmd::Category { category: None, .. }));
        assert!(matches!(p("seedwell files sintel --skip 3,5-7 --high 0").unwrap(), Cmd::Files { set, .. } if set.len() == 2));
        assert!(matches!(p("seedwell goal sintel --ratio 2 --time 7d --at-goal stop").unwrap(), Cmd::Goal { goal, .. } if goal.time == Some(604_800)));
        assert!(matches!(p("seedwell reannounce sintel").unwrap(), Cmd::Reannounce { .. }));
        assert!(matches!(p("seedwell recheck sintel").unwrap(), Cmd::Recheck { .. }));
        assert!(matches!(p("seedwell remove sintel --delete-files -y").unwrap(), Cmd::Remove { delete_files: true, yes: true, .. }));
        assert!(matches!(p("seedwell remove-confirm abc").unwrap(), Cmd::RemoveConfirm { .. }));
        assert_eq!(p("seedwell moves").unwrap(), Cmd::Moves);
        assert!(matches!(p("seedwell move sintel /tmp/x").unwrap(), Cmd::Move { path: Some(_), .. }));
        assert!(matches!(p("seedwell move --all --to-settings").unwrap(), Cmd::Move { path: None, .. }));
        assert!(matches!(p("seedwell cancel-move sintel").unwrap(), Cmd::CancelMove { .. }));
        assert!(matches!(p("seedwell stream sintel --file 2").unwrap(), Cmd::Stream { file: Some(2), .. }));
        assert!(matches!(p("seedwell stop-stream tok").unwrap(), Cmd::StopStream { .. }));
        assert!(matches!(p("seedwell limits --down 5MB/s --up unlimited").unwrap(), Cmd::Limits(l) if l.down_limit == Some(5_000_000) && l.up_limit == Some(0)));
        assert_eq!(p("seedwell trackers").unwrap(), Cmd::Trackers);
        assert!(matches!(p("seedwell tracker tl limits --down 5MB/s --up 500KB/s").unwrap(), Cmd::TrackerLimits { down: Some(5_000_000), up: Some(500_000), .. }));
        assert!(matches!(p("seedwell tracker tl pex").unwrap(), Cmd::TrackerPex { set: None, .. }));
        assert!(matches!(p("seedwell tracker tl pex on").unwrap(), Cmd::TrackerPex { set: Some(true), .. }));
        assert!(matches!(p("seedwell tracker tl pex off").unwrap(), Cmd::TrackerPex { set: Some(false), .. }));
        assert!(p("seedwell tracker tl pex maybe").is_err());
        assert!(matches!(p("seedwell stats tl --days 14").unwrap(), Cmd::Stats { days: Some(14), .. }));
        assert_eq!(p("seedwell rules").unwrap(), Cmd::Rules);
        assert!(matches!(p("seedwell settings get a.b").unwrap(), Cmd::SettingsGet { keys } if keys == vec!["a.b"]));
        assert!(matches!(p("seedwell settings set a.b=1 c=d").unwrap(), Cmd::SettingsSet { pairs } if pairs.len() == 2));
        assert!(matches!(p("seedwell wait sintel --any --timeout 5").unwrap(), Cmd::Wait { any: true, timeout: 5, .. }));
        assert!(matches!(p("seedwell watch --kinds a,b --count 2").unwrap(), Cmd::Watch { count: Some(2), .. }));
        assert!(matches!(p("seedwell activity --limit 3").unwrap(), Cmd::Activity { limit: Some(3) }));
        assert!(matches!(p("seedwell log trackers --errors --limit 20").unwrap(), Cmd::LogTrackers(a) if a.errors));
        assert_eq!(p("seedwell dev on").unwrap(), Cmd::Dev(DevAction::On));
        assert_eq!(p("seedwell dev off").unwrap(), Cmd::Dev(DevAction::Off));
        assert_eq!(p("seedwell dev").unwrap(), Cmd::Dev(DevAction::Status));
        assert!(p("seedwell dev maybe").is_err());
        assert!(matches!(p("seedwell help agents").unwrap(), Cmd::Help(Some(_))));
        assert!(matches!(p("seedwell daemon --state-dir x").unwrap(), Cmd::Daemon(_)));
        assert_eq!(p("seedwell sync list").unwrap(), Cmd::Sync(sync::Cmd::List));
        assert_eq!(p("seedwell chat list --json").unwrap(), Cmd::Chat(chat::Cmd::List));
        assert!(matches!(p("seedwell chat send Plans -").unwrap(), Cmd::Chat(chat::Cmd::Send { .. })));
        assert!(matches!(p("seedwell sync status MFZWI3D").unwrap(), Cmd::Sync(sync::Cmd::Status { .. })));
        assert!(matches!(p("seedwell sync add ~/Shared").unwrap(), Cmd::Sync(sync::Cmd::Add { .. })));
        assert!(matches!(p("seedwell sync join seedwell://sync/ABCDEF --to ~/Seedwell/Docs").unwrap(), Cmd::Sync(sync::Cmd::Join { to: Some(_), .. })));
        assert!(matches!(p("seedwell sync share Docs").unwrap(), Cmd::Sync(sync::Cmd::Share { .. })));
        assert!(matches!(p("seedwell sync rename Docs Papers").unwrap(), Cmd::Sync(sync::Cmd::Rename { .. })));
        assert!(matches!(p("seedwell sync move Docs /tmp/Docs").unwrap(), Cmd::Sync(sync::Cmd::Move { .. })));
        assert!(matches!(p("seedwell sync pause Docs").unwrap(), Cmd::Sync(sync::Cmd::Pause { .. })));
        assert!(matches!(p("seedwell sync resume Docs").unwrap(), Cmd::Sync(sync::Cmd::Resume { .. })));
        assert!(matches!(p("seedwell sync rescan Docs").unwrap(), Cmd::Sync(sync::Cmd::Rescan { .. })));
        assert!(matches!(p("seedwell sync lan-only Docs on").unwrap(), Cmd::Sync(sync::Cmd::LanOnly { on: true, .. })));
        assert!(matches!(p("seedwell sync conflicts Docs").unwrap(), Cmd::Sync(sync::Cmd::Conflicts { .. })));
        assert!(matches!(p("seedwell sync resolve Docs x.txt --keep copy").unwrap(), Cmd::Sync(sync::Cmd::Resolve { .. })));
        assert!(matches!(p("seedwell sync address Docs nas.local:22000").unwrap(), Cmd::Sync(sync::Cmd::Address { .. })));
        assert!(matches!(p("seedwell sync remove Docs --delete-files").unwrap(), Cmd::Sync(sync::Cmd::Remove { delete_files: true, yes: false, .. })));
        assert!(matches!(p("seedwell sync remove-confirm tok").unwrap(), Cmd::Sync(sync::Cmd::RemoveConfirm { .. })));
        assert_eq!(p("seedwell sync settings").unwrap(), Cmd::Sync(sync::Cmd::Settings));
        assert!(matches!(p("seedwell sync settings --device-name Studio").unwrap(), Cmd::Sync(sync::Cmd::SetSettings(_))));
        assert!(matches!(p("seedwell relay --port 22067").unwrap(), Cmd::Relay(a) if a.port == Some(22067)));
        assert_eq!(p("seedwell relay").unwrap(), Cmd::Relay(sync::RelayArgs::default()));
    }

    #[test]
    fn usage_errors() {
        for bad in [
            "seedwell",
            "seedwell frobnicate",
            "seedwell pause",
            "seedwell pause sintel --all",
            "seedwell priority sintel urgent",
            "seedwell start sintel --files 1 --skip 2",
            "seedwell add",
            "seedwell limits --alt maybe",
            "seedwell tracker tl",
            "seedwell list nonsense",
            "seedwell status extra",
            "seedwell settings set noequals",
            "seedwell move sintel",
            "seedwell sync",
            "seedwell sync status",
            "seedwell relay --port nope",
        ] {
            let words = shell_words(bad);
            assert!(parse(words[1..].to_vec()).is_err(), "{bad} should be a usage error");
        }
    }

    #[test]
    fn every_help_example_parses() {
        for c in crate::help::COMMANDS {
            for e in c.examples {
                if let Err(u) = p(e) {
                    panic!("`{e}` ({}) does not parse: {u}", c.name);
                }
            }
        }
    }

    #[test]
    fn every_command_in_the_table_is_known_to_the_parser() {
        for c in crate::help::COMMANDS {
            let words = vec![c.name.to_owned()];
            if let Err(Usage(m)) = parse(words) {
                assert!(!m.contains("unknown command"), "{} is in the help table but not parsed", c.name);
            }
        }
    }
}
