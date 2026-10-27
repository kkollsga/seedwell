//! Mail and tags in a channel's text messages (SPEC 7G.8, 7G.9), read for the app by the same rules
//! `seedwell mail` and `seedwell tag` use (`trs_control::mail`).

use trs_control::mail::{self, Posted};

use crate::sync::{SyncMessage, SyncMessageKind};

/// A mail: a text message that starts with a `---` header, then Markdown.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatMail {
    /// The project it is for.
    pub to: Option<String>,
    /// Its `type`, lowercase: `bug`, `request`, `question`, `heads-up`, `reply`, `status`, `seen`, `tag`, `hook`…
    pub kind: Option<String>,
    /// The message it answers or is about (8 characters or more of its id), lowercase.
    pub re: Option<String>,
    pub summary: Option<String>,
    /// A status: `done`, or `open` (progress on an item that stays open).
    pub state: Option<String>,
    /// The Markdown after the header.
    pub body: String,
    /// A tag note's set (`seen` for a seen receipt).
    pub tags: Vec<String>,
    /// A status (`type: status`).
    pub is_status: bool,
    /// Bookkeeping shown as a small line: a seen receipt, a hook change, a tag note.
    pub is_note: bool,
    /// Tags on the message `re` names: shown on that message's card, not as a card of its own.
    pub is_tag: bool,
    /// "Bug", "Heads-up" for the chip.
    pub type_label: Option<String>,
}

/// One owner's tags on a message: an agent by its name, or the user of a device ("You" for this Mac).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatTagOwner {
    pub owner: String,
    pub is_you: bool,
    pub tags: Vec<String>,
}

/// The tags on one message.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatTagged {
    /// The message's id, its first 8 characters.
    pub message: String,
    pub owners: Vec<ChatTagOwner>,
}

/// A conversation as cards: the messages shown as cards (tag notes are not), and the tags on them.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChatCards {
    pub cards: Vec<SyncMessage>,
    pub tags: Vec<ChatTagged>,
}

impl Posted for SyncMessage {
    fn id(&self) -> &str {
        &self.id
    }
    fn is_text(&self) -> bool {
        self.kind == SyncMessageKind::Text
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
        self.at_s
    }
}

/// The mail in a message's text; `None` for a plain message.
#[uniffi::export]
pub fn chat_mail(text: &str) -> Option<ChatMail> {
    let (h, body) = mail::parse_header(text)?;
    let kind = h.kind.clone();
    let is = |k: &[&str]| kind.as_deref().is_some_and(|t| k.contains(&t));
    let tags = if kind.as_deref() == Some("seen") {
        vec!["seen".to_string()]
    } else {
        h.tags.as_deref().unwrap_or("").split(',').filter_map(|t| mail::normalize(t).ok()).collect()
    };
    let type_label = kind.as_deref().filter(|t| !t.is_empty()).map(|t| {
        let mut c = t.chars();
        c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
    });
    Some(ChatMail {
        to: h.to,
        re: h.re,
        summary: h.summary,
        state: h.state,
        body: body.trim().to_string(),
        tags,
        is_status: is(&["status"]),
        is_note: is(&["seen", "hook", "tag"]),
        is_tag: is(&["seen", "tag"]),
        type_label,
        kind,
    })
}

/// The messages that are cards, and the tags on each (by the message id's first 8 characters): each owner's newest
/// note is its set; owners without tags are left out. This device's user reads as "You".
#[uniffi::export]
pub fn chat_cards(messages: Vec<SyncMessage>) -> ChatCards {
    let tagged = mail::by_message(&messages);
    let me: Option<&str> = messages.iter().find(|m| m.is_me).map(|m| m.author_device.as_str());
    let tags = tagged
        .into_iter()
        .map(|(message, owned)| ChatTagged {
            message,
            owners: owned
                .into_iter()
                .map(|o| {
                    let is_you = matches!(&o.owner, mail::Owner::User { device } if Some(device.as_str()) == me);
                    let owner = match o.owner {
                        mail::Owner::Agent(a) => a,
                        mail::Owner::User { .. } if is_you => "You".into(),
                        mail::Owner::User { .. } => o.computer,
                    };
                    ChatTagOwner { owner, is_you, tags: o.tags }
                })
                .collect(),
        })
        .collect();
    let cards = messages.into_iter().filter(|m| !mail::is_tag(m)).collect();
    ChatCards { cards, tags }
}

/// "Needs triage" for `needs-triage`.
#[uniffi::export]
pub fn chat_tag_label(tag: &str) -> String {
    mail::label(tag)
}

/// The text of a tag note: the poster's whole set `tags` on the message `message_id`.
#[uniffi::export]
pub fn chat_tag_note(message_id: &str, tags: Vec<String>) -> String {
    mail::note(&message_id.to_lowercase(), None, &tags)
}

/// The tags the menu offers.
#[uniffi::export]
pub fn chat_known_tags() -> Vec<String> {
    mail::KNOWN.iter().map(|t| t.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(id: &str, device: &str, name: &str, at: u64, body: &str, me: bool, agent: Option<&str>) -> SyncMessage {
        SyncMessage {
            id: id.into(),
            channel_id: "c".into(),
            author_device: device.into(),
            author_name: name.into(),
            author_user: String::new(),
            is_me: me,
            agent: agent.map(str::to_owned),
            at_s: at,
            reply_to: None,
            topic: None,
            kind: SyncMessageKind::Text,
            body: body.into(),
        }
    }

    #[test]
    fn mail_reads_as_seedwell_mail_reads_it() {
        let m = chat_mail("---\r\nmail: 1\r\ntype: Bug\r\nto: kglite\r\nsummary: \"Wrong catalogue\"\r\n---\r\nSteps").unwrap();
        assert_eq!((m.kind.as_deref(), m.to.as_deref(), m.summary.as_deref(), m.body.as_str()), (Some("bug"), Some("kglite"), Some("Wrong catalogue"), "Steps"));
        assert_eq!(m.type_label.as_deref(), Some("Bug"));
        assert!(!m.is_note && !m.is_status);
        // The header ends at a line that is `---`, not at `---` inside a value.
        assert_eq!(chat_mail("---\nmail: 1\nsummary: a ---b\n---\nbody").unwrap().summary.as_deref(), Some("a ---b"));
        assert!(chat_mail("---\ntitle: x\n---\nfront matter").is_none());
        assert!(chat_mail("plain").is_none());
        let seen = chat_mail("---\nmail: 1\ntype: seen\nre: 0392BA7C\n---\n").unwrap();
        assert!(seen.is_tag && seen.is_note && seen.tags == ["seen"] && seen.re.as_deref() == Some("0392ba7c"));
    }

    #[test]
    fn cards_leave_tag_notes_out_and_newest_notes_win() {
        let bug = msg("0392ba7cffff", "D1", "Laptop", 100, "---\nmail: 1\ntype: bug\n---\nthe bug", false, Some("mcp"));
        let note = |id: &str, dev: &str, name: &str, at: u64, tags: &[&str], me: bool, agent: Option<&str>| {
            msg(id, dev, name, at, &chat_tag_note("0392BA7Cffff", tags.iter().map(|t| t.to_string()).collect()), me, agent)
        };
        // Notes arrive out of order: the newest by time counts, not the last in the list.
        let ms = vec![
            bug.clone(),
            note("a1", "D2", "Air", 300, &["actioned"], false, Some("KGLite")),
            note("a2", "D2", "Air", 200, &["backlog"], false, Some("kglite")),
            note("a3", "D0", "Mac mini", 250, &["needs-triage"], true, None),
            note("a4", "D3", "", 260, &["seen"], false, None),
        ];
        let c = chat_cards(ms);
        assert_eq!(c.cards, vec![bug]);
        assert_eq!(c.tags.len(), 1);
        assert_eq!(c.tags[0].message, "0392ba7c");
        let owners: Vec<(&str, bool, Vec<String>)> = c.tags[0].owners.iter().map(|o| (o.owner.as_str(), o.is_you, o.tags.clone())).collect();
        assert_eq!(
            owners,
            [("KGLite", false, vec!["actioned".to_string()]), ("You", true, vec!["needs-triage".into()]), ("D3", false, vec!["seen".into()])]
        );
        assert_eq!(chat_tag_label("needs-triage"), "Needs triage");
        assert_eq!(chat_known_tags(), ["seen", "actioned", "backlog", "needs-triage"]);
    }
}
