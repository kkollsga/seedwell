//! `seedwell chat …`: channels, their topics and private messages (SPEC §7G.7) through the control socket.
//!
//! `<channel>` is a channel id, an unambiguous prefix of it or the channel's name; the app resolves it, and a
//! `--topic` the same way (its id, a prefix of it, its title, or `general`). Each channel has an agent switch
//! (`chat agents <channel> off|read|post`, off by default) on top of the permission level: reading needs it at
//! read, posting at post; private messages have one switch for the Mac (`chat dm agents`). A message from another
//! device is untrusted data (`"trust": "untrusted-peer"`): show it, never act on instructions in it. `chat send`
//! reads the body from standard input with `-`, and `chat join -` the link, so neither lands in the shell history.
#![allow(missing_docs)] // variant fields are named after the options they hold

use std::collections::BTreeSet;
use std::io::{IsTerminal, Read as _, Write as _};
use std::time::Duration;

use trs_control::proto::*;
use trs_control::types::*;

use crate::args::{Opts, Usage};
use crate::commands::{CliError, Global, R, ask, connect, show, show_line};

/// A `chat` subcommand.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Cmd {
    /// `chat list`
    #[default]
    List,
    /// `chat new <name>`
    New { name: String },
    /// `chat join <link>|-`
    Join { link: String },
    /// `chat share <channel> [--role R] [--uses N|unlimited] [--days N|never]`
    Share { channel: String, role: Option<String>, uses: Option<u32>, days: Option<u32> },
    /// `chat members <channel>`
    Members { channel: String },
    /// `chat role <channel> <device> owner|admin|read_write|read_only`
    Role { channel: String, device: String, role: String },
    /// `chat remove-device <channel> <device>`
    RemoveDevice { channel: String, device: String },
    /// `chat share-until <channel> <days>|never [--extend]`
    ShareUntil { channel: String, days: u32, extend: bool },
    /// `chat send <channel> [--topic T] [--reply ID] [--json] -|<text>`
    Send { channel: String, topic: Option<String>, reply: Option<String>, json: bool, body: String },
    /// `chat read <channel> [--topic T] [--since ID | --last N]`
    Read { channel: String, topic: Option<String>, since: Option<String>, last: Option<u32> },
    /// `chat watch [<channel>…] [--topic T] [--count N]`
    Watch { channels: Vec<String>, topic: Option<String>, count: Option<u64> },
    /// `chat topics <channel>`
    Topics { channel: String },
    /// `chat topic <channel> <title>`
    Topic { channel: String, title: String },
    /// `chat dm list`
    DmList,
    /// `chat dm send <device> [--reply ID] [--json] -|<text>`
    DmSend { device: String, reply: Option<String>, json: bool, body: String },
    /// `chat dm read <device> [--before ID] [--last N]`
    DmRead { device: String, before: Option<String>, last: Option<u32> },
    /// `chat dm watch [<device>…] [--count N]`
    DmWatch { devices: Vec<String>, count: Option<u64> },
    /// `chat dm agents off|read|post`
    DmAgents { access: String },
    /// `chat address <channel> <host:port>`
    Address { channel: String, address: String },
    /// `chat mute <channel> [on|off]`
    Mute { channel: String, on: bool },
    /// `chat agents <channel> off|read|post`
    Agents { channel: String, access: String },
    /// `chat wake <channel> <command>|off` (a command with options goes in quotes)
    Wake { channel: String, command: Option<String> },
    /// `chat leave <channel>`
    Leave { channel: String },
}

/// The one-line usage.
pub const USAGE: &str = "usage: seedwell chat list | new <name> | join <link>|- | share <channel> [--role read_write|read_only|admin|owner] [--uses N|unlimited] [--days N|never] | members <channel> | role <channel> <device> owner|admin|read_write|read_only | remove-device <channel> <device> | share-until <channel> <days>|never [--extend] | topics <channel> | topic <channel> <title> | send <channel> [--topic T] [--reply ID] [--as-json] -|<text> | read <channel> [--topic T] [--since ID | --last N] | watch [<channel>...] [--topic T] [--count N] | dm list | dm send <device> [--reply ID] [--as-json] -|<text> | dm read <device> [--before ID] [--last N] | dm watch [<device>...] [--count N] | dm agents off|read|post | address <channel> <host:port> | mute <channel> [on|off] | agents <channel> off|read|post | wake <channel> <command>|off | leave <channel>";

const DM_USAGE: &str = "usage: seedwell chat dm list | send <device> [--reply ID] [--as-json] -|<text> | read <device> [--before ID] [--last N] | watch [<device>...] [--count N] | agents off|read|post";

fn access(a: String, what: &str) -> Result<String, Usage> {
    if !matches!(a.as_str(), "off" | "read" | "post" | "read_post") {
        return Err(Usage(format!("{what} is off, read or post, not `{a}`")));
    }
    Ok(a)
}

/// Words after `chat dm`.
fn parse_dm(mut it: impl Iterator<Item = String>) -> Result<Cmd, Usage> {
    let sub = it.next().ok_or_else(|| Usage(DM_USAGE.into()))?;
    let mut o = Opts::new(it.collect());
    Ok(match sub.as_str() {
        "list" | "ls" => {
            args(o, 0, "dm list")?;
            Cmd::DmList
        }
        "send" | "post" => {
            let reply = o.value(&["--reply", "--reply-to"])?;
            let json = o.flag(&["--json-body", "--as-json"]);
            let mut p = o.finish()?;
            if p.len() < 2 {
                return Err(Usage("usage: seedwell chat dm send <device> [--reply ID] [--as-json] -|<text>   (- reads the message from standard input)".into()));
            }
            let device = p.remove(0);
            Cmd::DmSend { device, reply, json, body: p.join(" ") }
        }
        "read" => {
            let before = o.value(&["--before"])?;
            let last = o.number::<u32>(&["--last", "-n"])?;
            Cmd::DmRead { device: args(o, 1, "dm read <device> [--before ID] [--last N]")?.remove(0), before, last }
        }
        "watch" | "follow" => {
            let count = o.number::<u64>(&["--count"])?;
            Cmd::DmWatch { devices: o.finish()?, count }
        }
        "agents" => Cmd::DmAgents { access: access(args(o, 1, "dm agents off|read|post")?.remove(0), "agents")? },
        other => return Err(Usage(format!("unknown chat dm command `{other}`; {DM_USAGE}"))),
    })
}

fn args(o: Opts, n: usize, usage: &str) -> Result<Vec<String>, Usage> {
    let pos = o.finish()?;
    if pos.len() != n {
        return Err(Usage(format!("usage: seedwell chat {usage}")));
    }
    Ok(pos)
}

fn days(v: Option<String>) -> Result<Option<u32>, Usage> {
    match v.as_deref() {
        None => Ok(None),
        Some("never" | "unlimited") => Ok(Some(0)),
        Some(n) => Ok(Some(n.parse::<u32>().ok().filter(|n| *n >= 1).ok_or_else(|| Usage(format!("--days is a number of days or never, not `{n}`")))?)),
    }
}

/// Words after `chat`.
pub fn parse(args_in: Vec<String>) -> Result<Cmd, Usage> {
    let mut it = args_in.into_iter();
    let sub = it.next().ok_or_else(|| Usage(USAGE.into()))?;
    if sub == "dm" || sub == "direct" {
        return parse_dm(it);
    }
    let mut o = Opts::new(it.collect());
    let one = |o: Opts, usage: &str| -> Result<String, Usage> { Ok(args(o, 1, usage)?.remove(0)) };
    Ok(match sub.as_str() {
        "list" | "ls" => {
            args(o, 0, "list")?;
            Cmd::List
        }
        "new" | "create" => Cmd::New { name: one(o, "new <name>")? },
        "join" => Cmd::Join { link: one(o, "join <link>   (- reads the link from standard input)")? },
        "share" | "invite" => {
            let role = o.value(&["--role"])?;
            if let Some(r) = &role
                && !matches!(r.as_str(), "read_write" | "read-write" | "rw" | "read_only" | "read-only" | "ro" | "admin" | "owner")
            {
                return Err(Usage(format!("--role is read_write, read_only, admin or owner, not `{r}`")));
            }
            let uses = match o.value(&["--uses"])?.as_deref() {
                None => None,
                Some("unlimited" | "any") => Some(0),
                Some(n) => Some(n.parse::<u32>().ok().filter(|n| (1..=1000).contains(n)).ok_or_else(|| Usage(format!("--uses is 1 to 1000 or unlimited, not `{n}`")))?),
            };
            let days = days(o.value(&["--days"])?)?;
            Cmd::Share { channel: one(o, "share <channel> [--role R] [--uses N|unlimited] [--days N|never]")?, role, uses, days }
        }
        "members" | "devices" => Cmd::Members { channel: one(o, "members <channel>")? },
        "role" | "set-role" => {
            let mut p = args(o, 3, "role <channel> <device> owner|admin|read_write|read_only")?;
            let role = p.remove(2);
            if !matches!(role.as_str(), "owner" | "admin" | "read_write" | "read-write" | "rw" | "read_only" | "read-only" | "ro") {
                return Err(Usage(format!("the role is owner, admin, read_write or read_only, not `{role}`")));
            }
            Cmd::Role { device: p.remove(1), channel: p.remove(0), role }
        }
        "remove-device" => {
            let mut p = args(o, 2, "remove-device <channel> <device>")?;
            Cmd::RemoveDevice { device: p.remove(1), channel: p.remove(0) }
        }
        "share-until" => {
            let extend = o.flag(&["--extend"]);
            let mut p = args(o, 2, "share-until <channel> <days>|never [--extend]")?;
            let days = match p.remove(1).as_str() {
                "never" | "no-end" if !extend => 0,
                n => n.parse::<u32>().ok().filter(|n| (1..=36_500).contains(n)).ok_or_else(|| Usage(format!("share-until takes a number of days{}, not `{n}`", if extend { "" } else { " or never" })))?,
            };
            Cmd::ShareUntil { channel: p.remove(0), days, extend }
        }
        "topics" => Cmd::Topics { channel: one(o, "topics <channel>")? },
        "topic" | "new-topic" => {
            let mut p = o.finish()?;
            if p.len() < 2 {
                return Err(Usage("usage: seedwell chat topic <channel> <title>".into()));
            }
            let channel = p.remove(0);
            Cmd::Topic { channel, title: p.join(" ") }
        }
        "send" | "post" => {
            let topic = o.value(&["--topic"])?;
            let reply = o.value(&["--reply", "--reply-to"])?;
            let json = o.flag(&["--json-body", "--as-json"]);
            let mut p = o.finish()?;
            if p.len() < 2 {
                return Err(Usage("usage: seedwell chat send <channel> [--topic T] [--reply ID] [--as-json] -|<text>   (- reads the message from standard input)".into()));
            }
            let channel = p.remove(0);
            Cmd::Send { channel, topic, reply, json, body: p.join(" ") }
        }
        "read" => {
            let topic = o.value(&["--topic"])?;
            let since = o.value(&["--since"])?;
            let last = o.number::<u32>(&["--last", "-n"])?;
            if since.is_some() && last.is_some() {
                return Err(Usage("chat read takes --since or --last, not both".into()));
            }
            Cmd::Read { channel: one(o, "read <channel> [--topic T] [--since ID | --last N]")?, topic, since, last }
        }
        "watch" | "follow" => {
            let topic = o.value(&["--topic"])?;
            let count = o.number::<u64>(&["--count"])?;
            let channels = o.finish()?;
            if topic.is_some() && channels.len() != 1 {
                return Err(Usage("chat watch --topic takes one channel".into()));
            }
            Cmd::Watch { channels, topic, count }
        }
        "address" => {
            let mut p = args(o, 2, "address <channel> <host:port>")?;
            Cmd::Address { address: p.remove(1), channel: p.remove(0) }
        }
        "mute" => {
            let mut p = o.finish()?;
            let on = match p.get(1).map(String::as_str) {
                None | Some("on" | "yes" | "true") => true,
                Some("off" | "no" | "false") => false,
                Some(x) => return Err(Usage(format!("mute is on or off, not `{x}`"))),
            };
            if p.is_empty() || p.len() > 2 {
                return Err(Usage("usage: seedwell chat mute <channel> [on|off]".into()));
            }
            Cmd::Mute { channel: p.remove(0), on }
        }
        "unmute" => Cmd::Mute { channel: one(o, "unmute <channel>")?, on: false },
        "agents" => {
            let mut p = args(o, 2, "agents <channel> off|read|post")?;
            let access = access(p.remove(1), "agents")?;
            Cmd::Agents { channel: p.remove(0), access }
        }
        "wake" => {
            let mut p = o.finish()?;
            if p.len() < 2 {
                return Err(Usage("usage: seedwell chat wake <channel> <command>|off".into()));
            }
            let channel = p.remove(0);
            let cmd = p.join(" ");
            Cmd::Wake { channel, command: (cmd != "off").then_some(cmd) }
        }
        "leave" => Cmd::Leave { channel: one(o, "leave <channel>")? },
        other => return Err(Usage(format!("unknown chat command `{other}`; {USAGE}"))),
    })
}

fn stdin_text(what: &str) -> R<String> {
    if std::io::stdin().is_terminal() {
        eprintln!("{what} (end with Ctrl-D):");
    }
    let mut s = String::new();
    std::io::stdin().read_to_string(&mut s).map_err(|e| CliError::plain(1, format!("cannot read standard input: {e}")))?;
    let s = s.trim_end_matches(['\n', '\r']).to_owned();
    if s.trim().is_empty() {
        return Err(CliError::plain(2, "nothing on standard input"));
    }
    Ok(s)
}

fn ch(channel: String) -> ChatChannelParams {
    ChatChannelParams { channel }
}

/// Run a `chat` command.
pub async fn run(cmd: Cmd, g: &Global) -> R {
    let mut c = connect(g).await?;
    match cmd {
        Cmd::List => {
            let (raw, l): (_, ChatListDto) = ask(&mut c, Request::ChatList(Empty {})).await?;
            show(g, &raw, || list(&l));
        }
        Cmd::New { name } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatNew(ChatNewParams { name })).await?;
            show(g, &raw, || format!("Made the channel {} (this Mac is its owner).\nInvite another device: seedwell chat share {}", d.name, first_group(&d.id)));
        }
        Cmd::Join { link } => {
            let link = if link == "-" { stdin_text("Channel link")?.trim().to_owned() } else { link };
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatJoin(ChatJoinParams { link })).await?;
            show(g, &raw, || match d.role.as_deref() {
                Some(r) => format!("Joined {} ({}).", d.name, role(r)),
                None => format!("Joining {}: it starts once one of the channel's owners or admins is online.", d.name),
            });
        }
        Cmd::Share { channel, role: r, uses, days } => {
            let (raw, i): (_, SyncInviteDto) = ask(&mut c, Request::ChatShare(ChatShareParams { channel, role: r, uses, days })).await?;
            show(g, &raw, || crate::sync::invite(&i));
        }
        Cmd::Members { channel } => {
            let (raw, m): (_, SyncMembersDto) = ask(&mut c, Request::ChatMembers(ch(channel))).await?;
            show(g, &raw, || crate::sync::members(&m));
        }
        Cmd::Role { channel, device, role: r } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatSetRole(ChatSetRoleParams { channel, device: device.clone(), role: r.clone() })).await?;
            show(g, &raw, || format!("{device} is now {} in {}.", role(&r.replace('-', "_")), d.name));
        }
        Cmd::RemoveDevice { channel, device } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatRemoveDevice(ChatDeviceParams { channel, device: device.clone() })).await?;
            show(g, &raw, || format!("{device} is out of {}; it keeps the messages it has.", d.name));
        }
        Cmd::ShareUntil { channel, days, extend } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatSetShareUntil(ChatShareUntilParams { channel, days, extend })).await?;
            show(g, &raw, || match d.share_until {
                Some(t) => format!("{} is shared until {}; after that only its owners post and read new messages.", d.name, trs_control::human::date(t)),
                None => format!("{} is shared with no end.", d.name),
            });
        }
        Cmd::Send { channel, topic, reply, json, body } => {
            let body = if body == "-" { stdin_text("Message")? } else { body };
            let (raw, m): (_, ChatMessageDto) = ask(&mut c, Request::ChatSend(ChatSendParams { channel, body, reply_to: reply, json, topic })).await?;
            show(g, &raw, || match &m.topic_title {
                Some(t) => format!("Posted to {} › {t} ({}).", m.channel_name, m.id),
                None => format!("Posted to {} ({}).", m.channel_name, m.id),
            });
        }
        Cmd::Read { channel, topic, since, last } => {
            let (raw, l): (_, ChatMessagesDto) = ask(&mut c, Request::ChatRead(ChatReadParams { channel, since, last, topic })).await?;
            show(g, &raw, || messages(&l));
        }
        Cmd::Topics { channel } => {
            let (raw, l): (_, ChatTopicsDto) = ask(&mut c, Request::ChatTopics(ch(channel))).await?;
            show(g, &raw, || topics(&l));
        }
        Cmd::Topic { channel, title } => {
            let (raw, t): (_, ChatTopicDto) = ask(&mut c, Request::ChatNewTopic(ChatNewTopicParams { channel, title })).await?;
            show(g, &raw, || format!("Made the topic {} ({}).", t.title, t.id));
        }
        Cmd::DmList => {
            let (raw, l): (_, ChatDirectsDto) = ask(&mut c, Request::ChatDmList(Empty {})).await?;
            show(g, &raw, || directs(&l));
        }
        Cmd::DmSend { device, reply, json, body } => {
            let body = if body == "-" { stdin_text("Message")? } else { body };
            let (raw, m): (_, ChatMessageDto) = ask(&mut c, Request::ChatDmSend(ChatDmSendParams { device, body, reply_to: reply, json })).await?;
            show(g, &raw, || format!("Written privately to {} ({}).", m.peer_device.as_deref().map_or("?", first_group), m.id));
        }
        Cmd::DmRead { device, before, last } => {
            let (raw, l): (_, ChatMessagesDto) = ask(&mut c, Request::ChatDmRead(ChatDmParams { device, before, last })).await?;
            show(g, &raw, || if l.messages.is_empty() { format!("No private messages with {}.", if l.name.is_empty() { first_group(&l.id) } else { &l.name }) } else { messages(&l) });
        }
        Cmd::DmAgents { access } => {
            let (raw, _): (_, serde_json::Value) = ask(&mut c, Request::ChatDmSetAgents(ChatDmAgentsParams { agents: access.clone() })).await?;
            show(g, &raw, || format!("Agents on this Mac may {} with private messages.", agents(raw["agents"].as_str().unwrap_or(&access))));
        }
        Cmd::DmWatch { devices, count } => {
            // Names resolve to device ids first, so the stream can be filtered.
            let wanted: BTreeSet<String> = if devices.is_empty() {
                BTreeSet::new()
            } else {
                let (_, l): (_, ChatDirectsDto) = ask(&mut c, Request::ChatDmList(Empty {})).await?;
                let mut ids = BTreeSet::new();
                for q in &devices {
                    let lower = q.to_lowercase();
                    let hit = l.conversations.iter().find(|d| d.device_id == *q || d.name.to_lowercase() == lower || (q.len() >= 4 && d.device_id.starts_with(q.as_str())));
                    // A device not written with yet: taken as given (its id).
                    ids.insert(hit.map_or_else(|| q.clone(), |d| d.device_id.clone()));
                }
                ids
            };
            watch(&mut c, g, count, "watching private messages", |m| m.direct && (wanted.is_empty() || m.peer_device.as_ref().is_some_and(|d| wanted.contains(d)))).await?;
        }
        Cmd::Watch { channels, topic, count } => {
            // Names resolve to ids first, so the stream can be filtered.
            let wanted: BTreeSet<String> = if channels.is_empty() {
                BTreeSet::new()
            } else {
                let (_, l): (_, ChatListDto) = ask(&mut c, Request::ChatList(Empty {})).await?;
                let mut ids = BTreeSet::new();
                for q in &channels {
                    let lower = q.to_lowercase();
                    let hit = l.channels.iter().find(|ch| ch.id == *q || ch.name.to_lowercase() == lower || (q.len() >= 4 && ch.id.starts_with(q.as_str())));
                    let ch = hit.ok_or_else(|| CliError::plain(3, format!("no channel `{q}` (see `seedwell chat list`)")))?;
                    ids.insert(ch.id.clone());
                }
                ids
            };
            // A topic resolves to its id (`general`: the messages in no topic).
            let topic: Option<Option<String>> = match topic {
                None => None,
                Some(t) if t.eq_ignore_ascii_case("general") => Some(None),
                Some(t) => {
                    let (_, l): (_, ChatTopicsDto) = ask(&mut c, Request::ChatTopics(ch(channels[0].clone()))).await?;
                    let lower = t.to_lowercase();
                    let hits: Vec<&ChatTopicDto> = l.topics.iter().filter(|x| x.id == t || x.title.to_lowercase() == lower || (t.len() >= 4 && x.id.starts_with(&lower))).collect();
                    match hits.as_slice() {
                        [one] => Some(Some(one.id.clone())),
                        [] => return Err(CliError::plain(3, format!("no topic `{t}` in {} (see `seedwell chat topics`)", l.name))),
                        _ => return Err(CliError::plain(2, format!("`{t}` matches more than one topic; give its id"))),
                    }
                }
            };
            let what = if channels.is_empty() { "watching channel and private messages" } else { "watching channel messages" };
            watch(&mut c, g, count, what, |m| {
                let in_channel = if wanted.is_empty() { true } else { !m.direct && wanted.contains(&m.channel_id) };
                in_channel && topic.as_ref().is_none_or(|t| m.topic == *t)
            })
            .await?;
        }
        Cmd::Address { channel, address } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatAddAddress(ChatAddressParams { channel, address: address.clone() })).await?;
            show(g, &raw, || format!("Devices of {} are also looked for at {address}.", d.name));
        }
        Cmd::Mute { channel, on } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatMute(ChatMuteParams { channel, muted: on })).await?;
            show(g, &raw, || format!("{} is {}.", d.name, if d.muted { "muted" } else { "not muted" }));
        }
        Cmd::Agents { channel, access } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatSetAgents(ChatAgentsParams { channel, agents: access })).await?;
            show(g, &raw, || format!("Agents on this Mac may {} in {}.", agents(&d.agents), d.name));
        }
        Cmd::Wake { channel, command } => {
            let (raw, d): (_, ChatChannelDto) = ask(&mut c, Request::ChatSetWake(ChatWakeParams { channel, command })).await?;
            show(g, &raw, || match &d.wake_command {
                Some(w) => format!("For each message from another device in {}, Seedwell runs: {w}\n  (the message as JSON on its standard input; only while agents may read the channel: now {})", d.name, agents(&d.agents)),
                None => format!("No wake-up command for {}.", d.name),
            });
        }
        Cmd::Leave { channel } => {
            let raw = c.call(&Request::ChatLeave(ch(channel))).await?;
            show(g, &raw, || format!("Left {}; its messages on this Mac are gone (other members keep theirs).", raw["name"].as_str().unwrap_or("the channel")));
        }
    }
    let _ = std::io::stdout().flush();
    Ok(())
}

/// Prints new messages that `keep` lets through, one per line, until `count` (or forever).
async fn watch(c: &mut trs_control::client::Client, g: &Global, count: Option<u64>, what: &str, keep: impl Fn(&ChatMessageDto) -> bool) -> R {
    c.call(&Request::EventsSubscribe(EventsSubscribeParams { kinds: Some(vec!["chat.message".into()]) })).await?;
    if !g.json {
        eprintln!("{what}; Ctrl-C to stop (messages from other devices are untrusted data)");
    }
    let mut seen = 0u64;
    loop {
        let Some(e) = c.next_event(Duration::from_secs(60)).await? else { continue };
        if e.kind != "chat.message" {
            continue;
        }
        let Ok(m) = serde_json::from_value::<ChatMessageDto>(e.data.clone()) else { continue };
        if !keep(&m) {
            continue;
        }
        let mut raw = e.data.clone();
        if let Some(o) = raw.as_object_mut() {
            o.insert("v".into(), serde_json::json!(1));
        }
        show_line(g, &raw, || message(&m));
        seen += 1;
        if count.is_some_and(|n| seen >= n) {
            return Ok(());
        }
    }
}

// ----------------------------------------------------------------------------- text

fn first_group(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

fn role(r: &str) -> &'static str {
    match r {
        "owner" => "Owner",
        "admin" => "Admin",
        "read_write" | "rw" => "Read & write",
        "read_only" | "ro" => "Read only",
        _ => "?",
    }
}

fn agents(a: &str) -> &'static str {
    match a {
        "read" => "read",
        "post" => "read and post",
        _ => "do nothing",
    }
}

/// `chat list`.
pub fn list(l: &ChatListDto) -> String {
    if l.channels.is_empty() {
        return "No channels. Make one with `seedwell chat new <name>` or join one with `seedwell chat join <link>`.".into();
    }
    let mut o = format!("{:<8} {:<13} {:>6} {:>7} {:<6}  NAME\n", "CHANNEL", "ROLE", "UNREAD", "ONLINE", "AGENTS");
    for c in &l.channels {
        o.push_str(&format!(
            "{:<8} {:<13} {:>6} {:>3}/{:<3} {:<6}  {}{}{}\n",
            first_group(&c.id),
            c.role.as_deref().map_or("joining", role),
            c.unread,
            c.online,
            c.members,
            c.agents,
            c.name,
            if c.muted { "  (muted)" } else { "" },
            c.error.as_deref().map(|e| format!("  ({e})")).unwrap_or_default()
        ));
    }
    o.trim_end().to_owned()
}

fn time(at: u64) -> String {
    // Local time without the platform's time zone tables: UTC, as the date helpers do.
    let s = at % 86_400;
    format!("{} {:02}:{:02}", trs_control::human::date(at), s / 3600, s % 3600 / 60)
}

/// One message, for people.
pub fn message(m: &ChatMessageDto) -> String {
    let who = if m.me { "this Mac".to_string() } else if m.author_name.is_empty() { first_group(&m.author_device).to_string() } else { m.author_name.clone() };
    let agent = m.agent.as_deref().map(|a| format!(" via {a}")).unwrap_or_default();
    let reply = m.reply_to.as_deref().map(|r| format!(" (re {})", &r[..r.len().min(8)])).unwrap_or_default();
    let place = match (&m.topic_title, m.direct) {
        (_, true) => format!("(private, {}) ", m.peer_device.as_deref().map_or("?", first_group)),
        (Some(t), _) => format!("› {t} "),
        _ => String::new(),
    };
    let body = match m.kind.as_str() {
        "other" => "(a kind of message this version cannot show)".to_string(),
        "topic" => format!("made the topic {}", m.body),
        _ => m.body.clone(),
    };
    format!("[{}] {place}{who}{agent}{reply}: {body}   #{}", time(m.at), &m.id[..m.id.len().min(8)])
}

/// `chat topics`.
pub fn topics(l: &ChatTopicsDto) -> String {
    if l.topics.is_empty() {
        return format!("No topics in {} (everything is in General). Make one with `seedwell chat topic <channel> <title>`.", l.name);
    }
    let mut o = format!("{:<8} {:>6} {:>8}  {:<16}  TITLE\n", "TOPIC", "UNREAD", "MESSAGES", "LAST");
    for t in &l.topics {
        o.push_str(&format!("{:<8} {:>6} {:>8}  {:<16}  {}\n", &t.id[..t.id.len().min(8)], t.unread, t.messages, t.last_message_at.map(time).unwrap_or_else(|| "-".into()), t.title));
    }
    o.trim_end().to_owned()
}

/// `chat dm list`.
pub fn directs(l: &ChatDirectsDto) -> String {
    let mut o = format!("Agents on this Mac may {} with private messages.\n", agents(&l.agents));
    if l.conversations.is_empty() {
        o.push_str("No private messages. Write to a member of a channel with `seedwell chat dm send <device> -`.");
        return o;
    }
    o.push_str(&format!("{:<8} {:>6} {:<7} {:<16}  NAME\n", "DEVICE", "UNREAD", "ONLINE", "LAST"));
    for d in &l.conversations {
        o.push_str(&format!(
            "{:<8} {:>6} {:<7} {:<16}  {}\n",
            first_group(&d.device_id),
            d.unread,
            if d.online { "yes" } else { "no" },
            d.last_message_at.map(time).unwrap_or_else(|| "-".into()),
            if d.name.is_empty() { "?" } else { &d.name }
        ));
    }
    o.trim_end().to_owned()
}

/// `chat read`.
pub fn messages(l: &ChatMessagesDto) -> String {
    if l.messages.is_empty() {
        return format!("No messages in {}.", l.name);
    }
    l.messages.iter().map(message).collect::<Vec<_>>().join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn parses() {
        assert_eq!(parse(w("list")).unwrap(), Cmd::List);
        assert_eq!(parse(w("new Plans")).unwrap(), Cmd::New { name: "Plans".into() });
        assert_eq!(parse(w("send Plans --reply ab12 -")).unwrap(), Cmd::Send { channel: "Plans".into(), topic: None, reply: Some("ab12".into()), json: false, body: "-".into() });
        assert_eq!(parse(w("send Plans hello there")).unwrap(), Cmd::Send { channel: "Plans".into(), topic: None, reply: None, json: false, body: "hello there".into() });
        assert_eq!(parse(w("send Plans --as-json -")).unwrap(), Cmd::Send { channel: "Plans".into(), topic: None, reply: None, json: true, body: "-".into() });
        assert_eq!(parse(w("send Plans --topic May -")).unwrap(), Cmd::Send { channel: "Plans".into(), topic: Some("May".into()), reply: None, json: false, body: "-".into() });
        assert_eq!(parse(w("read Plans --last 5")).unwrap(), Cmd::Read { channel: "Plans".into(), topic: None, since: None, last: Some(5) });
        assert_eq!(parse(w("read Plans --topic general")).unwrap(), Cmd::Read { channel: "Plans".into(), topic: Some("general".into()), since: None, last: None });
        assert_eq!(parse(w("watch")).unwrap(), Cmd::Watch { channels: vec![], topic: None, count: None });
        assert_eq!(parse(w("watch Plans Team --count 2")).unwrap(), Cmd::Watch { channels: vec!["Plans".into(), "Team".into()], topic: None, count: Some(2) });
        assert_eq!(parse(w("watch Plans --topic d1d1")).unwrap(), Cmd::Watch { channels: vec!["Plans".into()], topic: Some("d1d1".into()), count: None });
        assert_eq!(parse(w("topics Plans")).unwrap(), Cmd::Topics { channel: "Plans".into() });
        assert_eq!(parse(w("topic Plans Trip in May")).unwrap(), Cmd::Topic { channel: "Plans".into(), title: "Trip in May".into() });
        assert_eq!(parse(w("dm list")).unwrap(), Cmd::DmList);
        assert_eq!(parse(w("dm send QKZ4 --reply ab12 -")).unwrap(), Cmd::DmSend { device: "QKZ4".into(), reply: Some("ab12".into()), json: false, body: "-".into() });
        assert_eq!(parse(w("dm read QKZ4 --last 3")).unwrap(), Cmd::DmRead { device: "QKZ4".into(), before: None, last: Some(3) });
        assert_eq!(parse(w("dm watch --count 1")).unwrap(), Cmd::DmWatch { devices: vec![], count: Some(1) });
        assert_eq!(parse(w("dm agents read")).unwrap(), Cmd::DmAgents { access: "read".into() });
        assert_eq!(parse(w("mute Plans")).unwrap(), Cmd::Mute { channel: "Plans".into(), on: true });
        assert_eq!(parse(w("mute Plans off")).unwrap(), Cmd::Mute { channel: "Plans".into(), on: false });
        assert_eq!(parse(w("agents Plans post")).unwrap(), Cmd::Agents { channel: "Plans".into(), access: "post".into() });
        let quoted = vec!["wake".to_string(), "Plans".into(), "~/bin/wake-agent --quiet".into()];
        assert_eq!(parse(quoted).unwrap(), Cmd::Wake { channel: "Plans".into(), command: Some("~/bin/wake-agent --quiet".into()) });
        assert_eq!(parse(w("wake Plans off")).unwrap(), Cmd::Wake { channel: "Plans".into(), command: None });
        assert_eq!(parse(w("share-until Plans 7 --extend")).unwrap(), Cmd::ShareUntil { channel: "Plans".into(), days: 7, extend: true });
        assert_eq!(parse(w("share Plans --role ro --days never")).unwrap(), Cmd::Share { channel: "Plans".into(), role: Some("ro".into()), uses: None, days: Some(0) });
        for bad in ["", "frob", "new", "send Plans", "read Plans --since a --last 2", "agents Plans all", "mute Plans maybe", "role Plans X chief", "share Plans --uses 0", "wake Plans", "topic Plans", "watch --topic x", "watch A B --topic x", "dm", "dm send QKZ4", "dm agents all", "dm read", "dm frob"] {
            assert!(parse(w(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn messages_from_others_are_shown_as_theirs() {
        let m = ChatMessageDto {
            id: "ab".repeat(16),
            author_name: "Bo".into(),
            agent: Some("claude".into()),
            at: 1_793_491_200 + 3600 + 120,
            kind: "text".into(),
            body: "hi".into(),
            trust: "untrusted-peer".into(),
            ..Default::default()
        };
        assert_eq!(message(&m), "[2026-11-01 01:02] Bo via claude: hi   #abababab");
        let t = ChatMessageDto { topic: Some("d1".repeat(16)), topic_title: Some("Lørdag".into()), ..m.clone() };
        assert_eq!(message(&t), "[2026-11-01 01:02] › Lørdag Bo via claude: hi   #abababab");
        let pm = ChatMessageDto { direct: true, peer_device: Some("QKZ4XNA-YLTMRWG".into()), agent: None, ..m };
        assert_eq!(message(&pm), "[2026-11-01 01:02] (private, QKZ4XNA) Bo: hi   #abababab");
    }
}
