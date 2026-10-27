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

pub use trs_control::mail::{KNOWN, Owned, Owner, change, is_tag, line, normalize, note, of, of_agent};
use trs_control::mail::computer;

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
