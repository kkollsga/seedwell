//! Mail and tags in a channel's text (SPEC §7G.8, §7G.9), shared by `seedwell mail`/`seedwell tag` and the app.
//!
//! A mail is a text message that starts with a header between `---` lines (`mail: 1`, `to`, `type`, `re`, `state`,
//! `summary`, …), then Markdown. A tag is a way of using a channel, like mail: a small text message in the message's
//! thread whose header says `type: tag`, `re` (the message) and `tags` (the owner's whole set, comma-separated;
//! none: cleared). An owner's newest note for a message is its set. The owner is the agent name (a project, on any
//! computer), or the user of the device that posted it without one.

use crate::types::ChatMessageDto;

/// A mail's header. Every field may be missing; `type` is `kind` here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// The project it is for.
    pub to: Option<String>,
    /// `type`: `bug`, `request`, `question`, `heads-up`, `reply`, `status`, `seen`, `tag`, `hook`…, lowercase.
    pub kind: Option<String>,
    /// The message it answers or is about (its id's first 8 characters or more), lowercase.
    pub re: Option<String>,
    /// A status: `done`, or `open` (progress on an item that stays open), lowercase.
    pub state: Option<String>,
    /// One line about it.
    pub summary: Option<String>,
    /// The sender's own next step when the answer comes (`send --then`): shown to it with the answer.
    pub then: Option<String>,
    /// A tag note's whole set (`type: tag`, §7G.9), comma-separated.
    pub tags: Option<String>,
    /// Files passed through a synced folder (§7G.10): `<folder id>:<the pass's folder in it>`.
    pub pass: Option<String>,
    /// The sender's git commit when the mail carries files (`d153c95`, `d153c95+changes` for a working tree that
    /// differs from it), so the recipient can tell which version it got.
    pub commit: Option<String>,
}

/// A text's mail header and the text after it; `None` when it does not start with one (`---` … `mail:` … `---`).
pub fn parse_header(body: &str) -> Option<(Header, String)> {
    let text = body.replace("\r\n", "\n");
    let rest = text.strip_prefix("---\n")?;
    let end = rest.lines().position(|l| l.trim() == "---")?;
    let lines: Vec<&str> = rest.lines().collect();
    let mut h = Header::default();
    let mut is_mail = false;
    for l in &lines[..end] {
        let Some((k, v)) = l.split_once(':') else { continue };
        let v = v.trim().trim_matches('"').trim();
        let v = (!v.is_empty()).then(|| v.to_string());
        match k.trim().to_ascii_lowercase().as_str() {
            "mail" => is_mail = true,
            "to" => h.to = v,
            "type" => h.kind = v.map(|t| t.to_lowercase()),
            "re" => h.re = v.map(|r| r.to_lowercase()),
            "state" => h.state = v.map(|s| s.to_lowercase()),
            "summary" => h.summary = v,
            "then" => h.then = v,
            "tags" => h.tags = v,
            "pass" => h.pass = v,
            "commit" => h.commit = v,
            _ => {}
        }
    }
    if !is_mail {
        return None;
    }
    let after = lines[end + 1..].join("\n");
    Some((h, after.trim_start_matches('\n').to_string()))
}

/// The text of a mail: its header, then `body`.
pub fn format_mail(h: &Header, body: &str) -> String {
    let one = |v: &str| v.replace(['\n', '\r'], " ").trim().to_string();
    let mut o = String::from("---\nmail: 1\n");
    for (k, v) in [("to", &h.to), ("type", &h.kind), ("re", &h.re), ("state", &h.state), ("summary", &h.summary), ("then", &h.then), ("tags", &h.tags), ("pass", &h.pass), ("commit", &h.commit)] {
        if let Some(v) = v.as_deref().map(one).filter(|v| !v.is_empty()) {
            o.push_str(&format!("{k}: {v}\n"));
        }
    }
    o.push_str("---\n");
    o.push_str(body.trim_start_matches('\n'));
    o
}

// ----------------------------------------------------------------------------- tags

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

/// A posted message as the tag rules need it: the CLI's [`ChatMessageDto`] and the app's message record both are one.
pub trait Posted {
    /// Its id.
    fn id(&self) -> &str;
    /// A text message (not JSON, a topic or a kind of a later version).
    fn is_text(&self) -> bool;
    /// Its text.
    fn body(&self) -> &str;
    /// The agent that posted it.
    fn agent(&self) -> Option<&str>;
    /// The device that posted it.
    fn device(&self) -> &str;
    /// That device's name (empty when never met).
    fn device_name(&self) -> &str;
    /// When it was posted, Unix seconds.
    fn at(&self) -> u64;
}

impl Posted for ChatMessageDto {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_text(&self) -> bool {
        self.kind == "text"
    }
    fn body(&self) -> &str {
        &self.body
    }
    fn agent(&self) -> Option<&str> {
        self.agent.as_deref()
    }
    fn device(&self) -> &str {
        &self.author_device
    }
    fn device_name(&self) -> &str {
        &self.author_name
    }
    fn at(&self) -> u64 {
        self.at
    }
}

/// The header of a tag note (`type: tag`) or of a seen receipt of mail before tags (`type: seen`, read as the tag
/// `seen`).
fn tag_header(m: &impl Posted) -> Option<Header> {
    if !m.is_text() {
        return None;
    }
    parse_header(m.body()).map(|(h, _)| h).filter(|h| matches!(h.kind.as_deref(), Some("tag" | "seen")))
}

/// A tag note (`type: tag`), and a seen receipt of mail before tags (`type: seen`, read as the tag `seen`).
pub fn is_tag(m: &impl Posted) -> bool {
    tag_header(m).is_some()
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
    pub fn of(m: &impl Posted) -> Owner {
        match m.agent() {
            Some(a) => Owner::Agent(a.to_string()),
            None => Owner::User { device: m.device().to_string() },
        }
    }

    /// The same owner (agent names compare without case).
    pub fn same(&self, other: &Owner) -> bool {
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

/// The computer a message came from: its device name, else its device id's first group.
pub fn computer(m: &impl Posted) -> String {
    if m.device_name().is_empty() { first_group(m.device()).to_string() } else { m.device_name().to_string() }
}

/// The owners' tags from `notes` (tag notes about one message), oldest owner first; owners whose set is empty are
/// left out. An owner's newest note is its set; a receipt adds `seen`.
fn owners<'a, M: Posted + 'a>(notes: impl Iterator<Item = (&'a M, Header)>) -> Vec<Owned> {
    let mut out: Vec<Owned> = Vec::new();
    for (m, h) in notes {
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
            o.seen_at = Some(o.seen_at.map_or(m.at(), |s| s.min(m.at())));
        }
        // A receipt adds `seen`; a tag note is the owner's whole set (the newest counts).
        if m.at() >= o.at {
            if h.kind.as_deref() == Some("seen") {
                if !o.tags.iter().any(|t| t == "seen") {
                    o.tags.push("seen".into());
                }
            } else {
                o.tags = tags;
            }
            o.at = m.at();
            o.computer = computer(m);
        }
    }
    out.retain(|o| !o.tags.is_empty());
    out
}

/// The tags each owner has on the message `id`, oldest owner first; owners whose set is empty are left out.
pub fn of<M: Posted>(id: &str, msgs: &[M]) -> Vec<Owned> {
    owners(msgs.iter().filter_map(|m| tag_header(m).map(|h| (m, h))).filter(|(_, h)| h.re.as_deref().is_some_and(|r| r.len() >= 8 && id.starts_with(r))))
}

/// The tags on every message `msgs` hold notes about, by the message id's first 8 characters (as `re` names it),
/// each as [`of`] gives them; messages whose owners all cleared their tags are left out. One pass for a whole
/// conversation (the app's cards).
pub fn by_message<M: Posted>(msgs: &[M]) -> std::collections::BTreeMap<String, Vec<Owned>> {
    let mut groups: std::collections::BTreeMap<String, Vec<(&M, Header)>> = std::collections::BTreeMap::new();
    for m in msgs {
        if let Some(h) = tag_header(m)
            && let Some(re) = h.re.as_deref().filter(|r| r.len() >= 8)
        {
            groups.entry(re[..8].to_string()).or_default().push((m, h));
        }
    }
    groups.into_iter().map(|(k, notes)| (k, owners(notes.into_iter()))).filter(|(_, o)| !o.is_empty()).collect()
}

/// The tags `owner` (an agent name) has on `id`.
pub fn of_agent<M: Posted>(id: &str, agent: &str, msgs: &[M]) -> Vec<String> {
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

/// The first group of a device id (`MFZWI3D` of `MFZWI3D-BONSGYC-…`).
fn first_group(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

/// "Needs triage" for `needs-triage`: a tag as a label.
pub fn label(tag: &str) -> String {
    let words = tag.replace('-', " ");
    let mut c = words.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessageDto;

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
        let all = by_message(&msgs);
        assert_eq!(all.keys().collect::<Vec<_>>(), ["0392ba7c"]);
        assert_eq!(all["0392ba7c"], o, "the same owners as for the one message");
        assert_eq!(change(&["seen".into()], &["Backlog".into()], &[], false).unwrap(), ["seen", "backlog"]);
        assert_eq!(change(&["seen".into(), "backlog".into()], &[], &["backlog".into()], false).unwrap(), ["seen"]);
        assert!(change(&["seen".into()], &[], &[], true).unwrap().is_empty());
    }
}
