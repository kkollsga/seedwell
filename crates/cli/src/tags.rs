//! Tags on messages (SPEC §7G.9): an agent, or the user in the app, marks a message `seen`, `actioned`, `backlog`,
//! `needs-triage` or a word of its own. Each tag has an owner, the one who set it, so with several agents in a channel
//! everyone sees who has seen what and who put what in their backlog.
//!
//! A tag is a way of using a channel, like mail: a small text message in the message's thread whose header says
//! `type: tag`, `re` (the message) and `tags` (the owner's whole set, comma-separated; none: cleared). An owner's
//! newest note for a message is its set. The owner is the agent name (a project, on any computer), or the user of
//! the device that posted it without one. Devices without tags show the notes as small messages.

use serde_json::{Value, json};
use trs_control::proto::*;
use trs_control::types::*;

use crate::commands::{CliError, R, ask};
use crate::mail::{Header, format_mail, parse_header};

/// The tags the guide and the app offer (others are kept as given).
pub const KNOWN: [&str; 4] = ["seen", "actioned", "backlog", "needs-triage"];

/// At most this many tags per owner and message, each at most this long.
const MAX_TAGS: usize = 8;
const MAX_TAG: usize = 32;

/// A tag as it is kept: lowercase, words joined by `-` ("Needs triage" → `needs-triage`).
pub fn normalize(tag: &str) -> Result<String, String> {
    let t = tag.trim().trim_start_matches('#').to_lowercase().split(|c: char| c.is_whitespace() || c == '_').filter(|w| !w.is_empty()).collect::<Vec<_>>().join("-");
    if t.is_empty() || t.chars().count() > MAX_TAG || !t.chars().all(|c| c.is_alphanumeric() || c == '-') || t.starts_with('-') {
        return Err(format!("`{tag}` is not a tag: one or a few words, letters, digits and -, at most {MAX_TAG} characters ({} …)", KNOWN.join(", ")));
    }
    Ok(t)
}

/// A tag note (`type: tag`), and a seen receipt of mail before tags (`type: seen`, read as the tag `seen`).
pub fn is_tag(m: &ChatMessageDto) -> bool {
    m.kind == "text" && parse_header(&m.body).is_some_and(|(h, _)| matches!(h.kind.as_deref(), Some("tag" | "seen")))
}

/// Who owns the tags of a note: its agent's name, else the user of the device that posted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    /// An agent, by its name (a project: the same on every computer).
    Agent(String),
    /// The user of a device (a note posted without an agent name).
    User {
        /// The device, by id.
        device: String,
    },
}

impl Owner {
    /// The owner of a note.
    pub fn of(m: &ChatMessageDto) -> Owner {
        match &m.agent {
            Some(a) => Owner::Agent(a.clone()),
            None => Owner::User { device: m.author_device.clone() },
        }
    }

    fn same(&self, other: &Owner) -> bool {
        match (self, other) {
            (Owner::Agent(a), Owner::Agent(b)) => a.eq_ignore_ascii_case(b),
            (Owner::User { device: a }, Owner::User { device: b }) => a == b,
            _ => false,
        }
    }
}

/// One owner's tags on a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owned {
    /// Who.
    pub owner: Owner,
    /// How it reads: "kglite" (an agent), "the user on Studio Mac".
    pub name: String,
    /// The computer of its newest note.
    pub computer: String,
    /// Its tags, in the order it set them.
    pub tags: Vec<String>,
    /// When it set them (its newest note), Unix seconds.
    pub at: u64,
    /// When it first tagged the message `seen`.
    pub seen_at: Option<u64>,
}

fn computer(m: &ChatMessageDto) -> String {
    if m.author_name.is_empty() { crate::chat::first_group(&m.author_device).to_string() } else { m.author_name.clone() }
}

/// The tags each owner has on the message `id`, oldest owner first; owners whose set is empty are left out.
pub fn of(id: &str, msgs: &[ChatMessageDto]) -> Vec<Owned> {
    let mut out: Vec<Owned> = Vec::new();
    for m in msgs.iter().filter(|m| is_tag(m)) {
        let Some((h, _)) = parse_header(&m.body) else { continue };
        if !h.re.as_deref().is_some_and(|r| r.len() >= 8 && id.starts_with(r)) {
            continue;
        }
        let tags = if h.kind.as_deref() == Some("seen") { vec!["seen".to_string()] } else { list(h.tags.as_deref()) };
        let owner = Owner::of(m);
        let i = match out.iter().position(|o| o.owner.same(&owner)) {
            Some(i) => i,
            None => {
                let name = match &owner {
                    Owner::Agent(a) => a.clone(),
                    Owner::User { .. } => format!("the user on {}", computer(m)),
                };
                out.push(Owned { owner, name, computer: computer(m), tags: Vec::new(), at: 0, seen_at: None });
                out.len() - 1
            }
        };
        let o = &mut out[i];
        if tags.iter().any(|t| t == "seen") {
            o.seen_at = Some(o.seen_at.map_or(m.at, |s| s.min(m.at)));
        }
        // A receipt adds `seen`; a tag note is the owner's whole set (the newest counts).
        if m.at >= o.at {
            if h.kind.as_deref() == Some("seen") {
                if !o.tags.iter().any(|t| t == "seen") {
                    o.tags.push("seen".into());
                }
            } else {
                o.tags = tags;
            }
            o.at = m.at;
            o.computer = computer(m);
        }
    }
    out.retain(|o| !o.tags.is_empty());
    out
}

/// The tags `owner` (an agent name) has on `id`.
pub fn of_agent(id: &str, agent: &str, msgs: &[ChatMessageDto]) -> Vec<String> {
    let me = Owner::Agent(agent.to_string());
    of(id, msgs).into_iter().find(|o| o.owner.same(&me)).map(|o| o.tags).unwrap_or_default()
}

/// A note's tag list ("seen, backlog") as tags.
fn list(s: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in s.unwrap_or("").split(',').filter_map(|t| normalize(t).ok()) {
        if !out.contains(&t) && out.len() < MAX_TAGS {
            out.push(t);
        }
    }
    out
}

/// A new set from the current one: `add` added, `remove` taken out, or (`clear`) none.
pub fn change(current: &[String], add: &[String], remove: &[String], clear: bool) -> Result<Vec<String>, String> {
    let mut set: Vec<String> = if clear { Vec::new() } else { current.to_vec() };
    for t in remove {
        let t = normalize(t)?;
        set.retain(|x| *x != t);
    }
    for t in add {
        let t = normalize(t)?;
        if !set.contains(&t) {
            set.push(t);
        }
    }
    if set.len() > MAX_TAGS {
        return Err(format!("at most {MAX_TAGS} tags on a message"));
    }
    Ok(set)
}

/// The text of a tag note: `tags` (the whole set) on the message `re`.
pub fn note(re: &str, to: Option<&str>, tags: &[String]) -> String {
    let h = Header { to: to.map(str::to_owned), kind: Some("tag".into()), re: Some(re.chars().take(8).collect()), tags: Some(tags.join(", ")), ..Default::default() };
    format_mail(&h, "")
}

/// "kglite: seen, backlog · the user on Studio Mac: needs-triage".
pub fn line(owned: &[Owned]) -> String {
    owned.iter().map(|o| format!("{}: {}", o.name, o.tags.join(", "))).collect::<Vec<_>>().join(" · ")
}

/// `tag`: changes `me`'s tags on the message `id` in `channel` (nothing to change: shows them). The text to print, and
/// the JSON.
pub async fn run(c: &mut trs_control::client::Client, channel: &str, me: &str, id: &str, add: &[String], remove: &[String], clear: bool) -> R<(String, Value)> {
    let (_, l): (_, ChatMessagesDto) = ask(c, Request::ChatRead(ChatReadParams { channel: channel.to_string(), last: Some(1000), ..Default::default() })).await?;
    let q = id.trim().trim_start_matches('#').to_lowercase();
    if q.len() < 8 {
        return Err(CliError::plain(2, "give the message's id: its first 8 characters at least"));
    }
    let hits: Vec<&ChatMessageDto> = l.messages.iter().filter(|m| m.id.starts_with(&q)).collect();
    let m = match hits.as_slice() {
        [m] => *m,
        [] => return Err(CliError::plain(3, format!("no message {q} in {} (it may be older than the last 1000 messages)", l.name))),
        _ => return Err(CliError::plain(3, format!("more than one message starts with {q}; give more of the id"))),
    };
    if is_tag(m) {
        return Err(CliError::plain(2, format!("{q} is a tag note itself; tag the message it is about")));
    }
    let short: String = m.id.chars().take(8).collect();
    let mut owned = of(&m.id, &l.messages);
    let changing = !add.is_empty() || !remove.is_empty() || clear;
    let mut said = String::new();
    if changing {
        let mine = of_agent(&m.id, me, &l.messages);
        let set = change(&mine, add, remove, clear).map_err(|e| CliError::plain(2, e))?;
        if set != mine {
            let req = ChatSendParams { channel: channel.to_string(), body: note(&m.id, m.agent.as_deref(), &set), reply_to: None, json: false, topic: m.topic.clone(), agent: Some(me.to_string()) };
            let (_, sent): (_, ChatMessageDto) = ask(c, Request::ChatSend(req)).await?;
            let me_owner = Owner::Agent(me.to_string());
            owned.retain(|o| !o.owner.same(&me_owner));
            if !set.is_empty() {
                let seen_at = set.iter().any(|t| t == "seen").then_some(sent.at);
                owned.push(Owned { owner: me_owner, name: me.to_string(), computer: computer(&sent), tags: set.clone(), at: sent.at, seen_at });
            }
        }
        said = if set.is_empty() { format!("As {me}: no tags of yours on {short}.") } else { format!("As {me}: {short} tagged {}.", set.join(", ")) };
    }
    let all = if owned.is_empty() { format!("No tags on {short}.") } else { format!("Tags on {short}: {}", line(&owned)) };
    let text = if said.is_empty() { all } else { format!("{said}\n{all}") };
    let j = json!({
        "id": short,
        "yours": owned.iter().find(|o| o.owner.same(&Owner::Agent(me.to_string()))).map(|o| o.tags.clone()).unwrap_or_default(),
        "tags": owned.iter().map(|o| json!({ "owner": o.name, "agent": matches!(o.owner, Owner::Agent(_)), "computer": o.computer, "tags": o.tags, "at": o.at })).collect::<Vec<_>>(),
    });
    Ok((text, j))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, agent: Option<&str>, device: &str, at: u64, body: &str) -> ChatMessageDto {
        ChatMessageDto {
            id: id.into(),
            author_device: device.into(),
            author_name: format!("{device} Mac"),
            agent: agent.map(str::to_owned),
            at,
            kind: "text".into(),
            body: body.into(),
            ..Default::default()
        }
    }

    #[test]
    fn tags_are_words_kept_lowercase() {
        assert_eq!(normalize(" Needs triage ").unwrap(), "needs-triage");
        assert_eq!(normalize("#Backlog").unwrap(), "backlog");
        assert!(normalize("").is_err() && normalize("a,b").is_err() && normalize(&"x".repeat(33)).is_err());
    }

    #[test]
    fn each_owner_has_its_newest_set_and_a_receipt_counts_as_seen() {
        let t = |tags: &[&str]| note("0392ba7c", None, &tags.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let msgs = vec![
            msg("0392ba7cffff", Some("mcp-servers"), "A", 1, "---\nmail: 1\ntype: bug\n---\nthe bug"),
            msg("aa", Some("kglite"), "B", 2, "---\nmail: 1\ntype: seen\nre: 0392ba7c\n---\n"),
            msg("bb", Some("kglite"), "B", 3, &t(&["seen", "backlog"])),
            msg("cc", None, "A", 4, &t(&["needs-triage"])),
            msg("dd", Some("KGLite"), "C", 5, &t(&["seen", "actioned"])),
            msg("ee", Some("codingest"), "B", 6, &t(&["seen"])),
            msg("ff", Some("codingest"), "B", 7, &t(&[])),
        ];
        let o = of("0392ba7cffff", &msgs);
        assert_eq!(line(&o), "kglite: seen, actioned · the user on A Mac: needs-triage", "codingest cleared its own");
        assert_eq!((o[0].seen_at, o[0].computer.as_str()), (Some(2), "C Mac"), "first seen by its receipt; the newest note's computer");
        assert_eq!(of_agent("0392ba7cffff", "kglite", &msgs), ["seen", "actioned"]);
        assert!(of("0000000000", &msgs).is_empty());
        assert_eq!(change(&["seen".into()], &["Backlog".into()], &[], false).unwrap(), ["seen", "backlog"]);
        assert_eq!(change(&["seen".into(), "backlog".into()], &[], &["backlog".into()], false).unwrap(), ["seen"]);
        assert!(change(&["seen".into()], &[], &[], true).unwrap().is_empty());
    }
}
