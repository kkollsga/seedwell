//! Agents' requests for access (SPEC 9A.3 *Asking for access*) as every host shows them: the rows of Settings › Agents
//! › Requests, the line of the menu and the main window, the words of the answer, and the notification's actions
//! (docs/CROSS-PLATFORM.md §1: Rust decides, the host draws).
//!
//! An agent that lacks access asks with `seedwell agents request`; the control server keeps the request
//! (`trs_control::access`). The host shows [`Core::agent_requests`] and, on Approve or Deny (a button, the menu, a
//! notification's action), calls `Core::agent_request_decide` (async), which makes the change and says what happened.
//! Nothing an agent runs reaches that call.

use crate::Core;
use trs_control::access::Entry;

/// How often a host shows the requests again while it shows them (they expire, others come).
const REFRESH_MS: u32 = 2_000;

/// One request waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AgentRequestRow {
    /// What Approve and Deny name.
    pub id: String,
    /// "Claude asks for agent permission full".
    pub title: String,
    /// "“to install the release” · 4 min ago": the agent's reason in its own words (quoted, never the app's), and when.
    pub detail: String,
}

/// The requests waiting for the user, for Settings › Agents, the menu and the main window.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AgentRequestsModel {
    /// Newest first.
    pub rows: Vec<AgentRequestRow>,
    /// The line of the menu and the main window while one waits ("Claude asks for agent permission full", "3 agents
    /// ask for access"); `None`: nothing waits (no line).
    pub line: Option<String>,
    /// "Approve" (the prominent one, last) and "Deny".
    pub approve_title: String,
    pub deny_title: String,
    /// Read the model again this often while it is shown.
    pub refresh_ms: u32,
    /// Changes when a request comes or goes: a menu that shows them is built again.
    pub signature: String,
}

/// What an answer did, for a short notice (or the row's place): `ok` false when nothing changed.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AgentRequestAnswer {
    pub ok: bool,
    pub title: String,
    pub body: String,
}

/// "just now", "4 min ago", "2 h ago".
pub(crate) fn ago(at_ms: u64, now_ms: u64) -> String {
    let s = now_ms.saturating_sub(at_ms) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

/// "Claude asks for agent permission full".
pub(crate) fn request_title(e: &Entry) -> String {
    let verb = if e.agents.len() == 1 { "asks" } else { "ask" };
    format!("{} {verb} for {}", e.who(), e.want.text())
}

/// The reason, quoted (an agent's words), when there is one.
pub(crate) fn quoted_why(e: &Entry) -> Option<String> {
    e.why.as_deref().map(|w| format!("“{w}”"))
}

/// The model of `requests` (newest first) at `now_ms`.
pub(crate) fn requests_model(requests: &[Entry], now_ms: u64) -> AgentRequestsModel {
    let rows: Vec<AgentRequestRow> = requests
        .iter()
        .map(|e| AgentRequestRow {
            id: e.id.clone(),
            title: request_title(e),
            detail: quoted_why(e).into_iter().chain(std::iter::once(ago(e.at_ms, now_ms))).collect::<Vec<_>>().join(" · "),
        })
        .collect();
    let line = match rows.as_slice() {
        [] => None,
        [one] => Some(one.title.clone()),
        many => Some(format!("{} requests from agents for access", many.len())),
    };
    let signature = requests.iter().map(|e| format!("{}:{}", e.id, e.agents.len())).collect::<Vec<_>>().join(",");
    AgentRequestsModel { rows, line, approve_title: "Approve".into(), deny_title: "Deny".into(), refresh_ms: REFRESH_MS, signature }
}

/// The notice after an answer (`Ok`: what it did; `Err`: why nothing did).
pub(crate) fn answer_of(approve: bool, r: Result<String, String>) -> AgentRequestAnswer {
    match r {
        Ok(text) => AgentRequestAnswer { ok: true, title: if approve { "Approved".into() } else { "Denied".into() }, body: cap(&text) },
        Err(why) => AgentRequestAnswer { ok: false, title: if approve { "Not approved".into() } else { "Not denied".into() }, body: cap(&why) },
    }
}

/// The first letter in capitals ("agent permission is full now" → "Agent permission is full now").
fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

#[uniffi::export]
impl Core {
    /// Agents' requests for access waiting for the user (Settings › Agents › Requests, the menu's and the main
    /// window's line), at `now_ms` (Unix ms). Cheap: the host may call it whenever it draws.
    pub fn agent_requests(&self, now_ms: u64) -> AgentRequestsModel {
        requests_model(&self.backend.agent_requests(), now_ms)
    }
}

impl Core {
    /// The user's Approve or Deny of request `id` (a button, the menu, a notification's action): the change is made,
    /// never above what was asked. The host awaits `agent_request_decide` and may show the answer.
    pub(crate) fn agent_request_decide(&self, id: String, approve: bool) -> AgentRequestAnswer {
        answer_of(approve, self.backend.agent_request_decide(&id, approve))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trs_control::access::{State, TTL_MS, Want};

    fn entry(id: &str, agents: &[&str], why: Option<&str>, at_ms: u64) -> Entry {
        Entry {
            id: id.into(),
            want: Want::Level(trs_control::Permission::Full),
            agents: agents.iter().map(|a| a.to_string()).collect(),
            why: why.map(str::to_owned),
            at_ms,
            expires_ms: at_ms + TTL_MS,
            state: State::Pending,
            decided_ms: None,
            how: None,
            note: None,
            announced: None,
            notify: true,
        }
    }

    #[test]
    fn rows_say_who_asks_what_why_and_when() {
        let now = 1_800_000_000_000;
        let m = requests_model(&[entry("a1", &["Claude"], Some("to install the release"), now - 4 * 60_000)], now);
        assert_eq!(
            m.rows,
            [AgentRequestRow { id: "a1".into(), title: "Claude asks for agent permission full".into(), detail: "“to install the release” · 4 min ago".into() }]
        );
        assert_eq!(m.line.as_deref(), Some("Claude asks for agent permission full"));
        assert_eq!((m.approve_title.as_str(), m.deny_title.as_str(), m.refresh_ms), ("Approve", "Deny", REFRESH_MS));
        let two = requests_model(&[entry("a1", &["Claude", "Codex"], None, now), entry("b2", &["Codex"], None, now - 2 * 3_600_000)], now);
        assert_eq!(two.rows[0].title, "Claude and Codex ask for agent permission full");
        assert_eq!((two.rows[0].detail.as_str(), two.rows[1].detail.as_str()), ("just now", "2 h ago"));
        assert_eq!(two.line.as_deref(), Some("2 requests from agents for access"));
        assert_eq!(two.signature, "a1:2,b2:1");
        assert_eq!((requests_model(&[], now).line, requests_model(&[], now).signature), (None, String::new()));
    }

    #[test]
    fn an_answer_says_what_it_did_or_why_not() {
        assert_eq!(
            answer_of(true, Ok("Approved: agent permission is full now".into())),
            AgentRequestAnswer { ok: true, title: "Approved".into(), body: "Approved: agent permission is full now".into() }
        );
        let no = answer_of(true, Err("agent access is switched off in Settings › Agents; turn it on there first".into()));
        assert_eq!((no.ok, no.title.as_str(), no.body.as_str()), (false, "Not approved", "Agent access is switched off in Settings › Agents; turn it on there first"));
        assert_eq!(answer_of(false, Ok("Denied: Claude does not get development builds allowed".into())).title, "Denied");
    }

    #[test]
    fn the_mock_core_answers_its_sample_once() {
        let dir = tempfile::tempdir().unwrap();
        let core = crate::Core::new(crate::CoreOptions {
            config_dir: dir.path().display().to_string(),
            mock: true,
            allow_loopback_trackers: false,
            forbidden_root: None,
        })
        .unwrap();
        let now = 1_900_000_000_000;
        let m = core.agent_requests(now);
        assert_eq!(m.rows.len(), 1);
        let id = m.rows[0].id.clone();
        let a = core.agent_request_decide(id.clone(), true);
        assert!(a.ok, "{a:?}");
        assert!(core.agent_requests(now).rows.is_empty());
        assert!(!core.agent_request_decide(id, false).ok, "answered once");
    }
}
