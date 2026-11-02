//! `seedwell agents`: ask the user for access this agent lacks, and see what became of it (SPEC 9A.3 *Asking for
//! access*). Asking grants nothing: the user answers in Seedwell (a notification with Approve and Deny, Settings ›
//! Agents, the menu) or with a reply of yes or no to the request's announcement in chat. No command answers.
#![allow(missing_docs)] // variant fields are named after the options they hold

use trs_control::proto::{AgentsRequestParams, AgentsRequestsParams, Request};
use trs_control::types::{AccessAskedDto, AccessRequestDto, AccessRequestsDto};

use crate::args::{Opts, Usage};
use crate::commands::{Global, R, ask, connect, show};

/// An `agents` command.
#[derive(Clone, Debug, PartialEq)]
pub enum Cmd {
    /// Ask for `what` (with its target), why, announced in a channel (its topic, the message it answers).
    Request { what: String, target: Option<String>, why: Option<String>, channel: Option<String>, topic: Option<String>, reply: Option<String> },
    /// The requests (`all`: also those answered or expired in the last day).
    Requests { all: bool },
}

/// `agents` with the agent's name.
#[derive(Clone, Debug, PartialEq)]
pub struct Agents {
    pub cmd: Cmd,
    pub agent: Option<String>,
}

const USAGE: &str = "usage: seedwell agents request control|full|channel-agents <channel>|dm-agents [read|post]|dev-builds [--why TEXT] [--channel C [--topic T] [--reply ID]] [--as NAME] | agents requests [--all]";

/// Words after `agents`.
pub fn parse(args: Vec<String>) -> Result<Agents, Usage> {
    let mut o = Opts::new(args);
    let agent = o.value(&["--as"])?;
    let why = o.value(&["--why"])?;
    let channel = o.value(&["--channel"])?;
    let topic = o.value(&["--topic"])?;
    let reply = o.value(&["--reply", "--reply-to"])?;
    let all = o.flag(&["--all"]);
    let pos = o.finish()?;
    let words: Vec<&str> = pos.iter().map(String::as_str).collect();
    let asks = why.is_some() || channel.is_some() || topic.is_some() || reply.is_some();
    if channel.is_none() && (topic.is_some() || reply.is_some()) {
        return Err(Usage("--topic and --reply go with --channel".into()));
    }
    let cmd = match (words.as_slice(), all) {
        (["requests"] | [], _) if !asks => Cmd::Requests { all },
        (["request", what @ ("control" | "full" | "dev-builds")], false) => {
            Cmd::Request { what: (*what).into(), target: None, why, channel, topic, reply }
        }
        (["request", "channel-agents", target], false) => {
            Cmd::Request { what: "channel-agents".into(), target: Some((*target).into()), why, channel, topic, reply }
        }
        (["request", "dm-agents"], false) => Cmd::Request { what: "dm-agents".into(), target: None, why, channel, topic, reply },
        (["request", "dm-agents", level @ ("read" | "post")], false) => {
            Cmd::Request { what: "dm-agents".into(), target: Some((*level).into()), why, channel, topic, reply }
        }
        (["request", "channel-agents"], false) => return Err(Usage("say which channel: seedwell agents request channel-agents <channel>".into())),
        (["approve" | "deny" | "grant", ..], _) => {
            return Err(Usage("only the user answers a request, in Seedwell or with a reply in chat; no command does".into()));
        }
        _ => return Err(Usage(USAGE.into())),
    };
    Ok(Agents { cmd, agent })
}

/// Run an `agents` command.
pub async fn run(a: Agents, g: &Global) -> R {
    let mut c = connect(g).await?;
    match a.cmd {
        Cmd::Request { what, target, why, channel, topic, reply } => {
            let agent = crate::chat::label(g, &a.agent, &None);
            let req = Request::AgentsRequest(AgentsRequestParams { what, target, why, agent: Some(agent), channel, topic, reply_to: reply });
            let (raw, d): (_, AccessAskedDto) = ask(&mut c, req).await?;
            show(g, &raw, || asked_words(&d));
        }
        Cmd::Requests { all } => {
            let (raw, d): (_, AccessRequestsDto) = ask(&mut c, Request::AgentsRequests(AgentsRequestsParams { all })).await?;
            show(g, &raw, || list_words(&d));
        }
    }
    Ok(())
}

/// What `agents request` says.
pub fn asked_words(d: &AccessAskedDto) -> String {
    format!("{}.\nRequest {}: {}.", d.note.trim_end_matches('.'), d.request.id, state_words(&d.request))
}

/// Where a request stands, in a few words.
fn state_words(r: &AccessRequestDto) -> String {
    let by = r.how.as_deref().map(|h| if h == "chat" { " (a reply in chat)" } else { " (in the app)" }).unwrap_or_default();
    let note = r.note.as_deref().map(|n| format!(": {n}")).unwrap_or_default();
    match r.state.as_str() {
        "pending" => format!("pending, expires {}", crate::chat::time(r.expires / 1000)),
        "granted" => format!("granted by the user{by}{note}"),
        "denied" => format!("denied by the user{by}"),
        "expired" => "expired unanswered".into(),
        other => other.into(),
    }
}

/// What `agents requests` says.
pub fn list_words(d: &AccessRequestsDto) -> String {
    let mut lines = vec![format!("Agent permission: {}.", d.permission.replace('_', "-"))];
    if d.requests.is_empty() {
        lines.push("No requests. When something is refused for lack of access, the error names the `seedwell agents request` that asks for it.".into());
    }
    for r in &d.requests {
        let why = r.why.as_deref().map(|w| format!(" (\"{w}\")")).unwrap_or_default();
        lines.push(format!("{}  {} for {}{why}: {}.", r.id, r.agents.join(", "), r.text, state_words(r)));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Agents, Usage> {
        parse(s.split_whitespace().map(str::to_owned).collect())
    }

    #[test]
    fn the_words_parse() {
        assert_eq!(p("").unwrap().cmd, Cmd::Requests { all: false });
        assert_eq!(p("requests --all").unwrap().cmd, Cmd::Requests { all: true });
        let a = p("request full --why install --as kglite --channel Bridge --topic kglite --reply 91b10241").unwrap();
        assert_eq!(a.agent.as_deref(), Some("kglite"));
        assert_eq!(
            a.cmd,
            Cmd::Request {
                what: "full".into(),
                target: None,
                why: Some("install".into()),
                channel: Some("Bridge".into()),
                topic: Some("kglite".into()),
                reply: Some("91b10241".into())
            }
        );
        assert!(matches!(p("request channel-agents Family").unwrap().cmd, Cmd::Request { ref what, target: Some(ref t), .. } if what == "channel-agents" && t == "Family"));
        assert!(matches!(p("request dm-agents").unwrap().cmd, Cmd::Request { target: None, .. }));
        assert!(matches!(p("request dm-agents post").unwrap().cmd, Cmd::Request { target: Some(ref t), .. } if t == "post"));
        assert!(matches!(p("request dev-builds").unwrap().cmd, Cmd::Request { ref what, .. } if what == "dev-builds"));
        for bad in [
            "request",
            "request read",
            "request off",
            "request channel-agents",
            "request full extra",
            "request dm-agents write",
            "request full --all",
            "requests --why x",
            "request full --topic t",
            "approve 5eed0a11",
            "deny 5eed0a11",
            "grant full",
        ] {
            assert!(p(bad).is_err(), "{bad}");
        }
        assert!(p("approve 5eed0a11").unwrap_err().0.contains("only the user"));
    }

    #[test]
    fn it_says_what_was_asked_and_where_each_request_stands() {
        let r = AccessRequestDto {
            id: "5eed0a11".into(),
            what: "full".into(),
            text: "agent permission full".into(),
            agents: vec!["claude".into()],
            why: Some("install the release".into()),
            state: "granted".into(),
            how: Some("chat".into()),
            note: Some("agent permission is full now".into()),
            ..Default::default()
        };
        let d = AccessRequestsDto { permission: "read_only".into(), requests: vec![r.clone(), AccessRequestDto { id: "0b0b0b0b".into(), state: "denied".into(), how: Some("app".into()), note: None, why: None, ..r.clone() }] };
        assert_eq!(
            list_words(&d),
            "Agent permission: read-only.\n5eed0a11  claude for agent permission full (\"install the release\"): granted by the user (a reply in chat): agent permission is full now.\n0b0b0b0b  claude for agent permission full: denied by the user (in the app)."
        );
        let asked = AccessAskedDto { request: AccessRequestDto { state: "expired".into(), ..r }, duplicate: false, note: "Asked the user for agent permission full; nothing changes until the user answers.".into() };
        assert_eq!(asked_words(&asked), "Asked the user for agent permission full; nothing changes until the user answers.\nRequest 5eed0a11: expired unanswered.");
        assert!(list_words(&AccessRequestsDto { permission: "control".into(), requests: vec![] }).contains("No requests."));
    }
}
