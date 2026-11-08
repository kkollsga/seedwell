//! Agents asking the user for access (SPEC 9A.3 *Asking for access*).
//!
//! An agent that lacks something (a permission level, a channel open to agents, private messages, development builds)
//! asks with `agents.request`. Asking grants nothing: the request is kept here (pending, with the agent's name, its
//! reason and the time; the same thing asked twice is one request; unanswered it expires after [`TTL_MS`]) and the
//! app tells the user (a notification with Approve and Deny, Settings › Agents, the menu). The user answers with one
//! click there, or with a reply in chat to the request's announcement ([`answer`]). Then this desk applies the change
//! itself (the level in `config.toml`, the channel's switch, ...), never above what was asked, writes who answered,
//! how and when into the activity log, and tells the agents (an `agents.request` event; `agents.requests` shows it).
//!
//! **Only the user approves.** No method of the control socket approves or denies: [`AccessDesk::decide`] is called by
//! the app (its buttons and notification actions) and by [`AccessDesk::on_message`] for a chat reply, which counts only
//! when it is the user's own post: written in the app on this device (`trust: this-device`), or in the app on another
//! of the user's own paired devices (a post without an agent label, signed by a device of *Your devices*). An agent's
//! post (here or elsewhere), another member's, or a reply to anything but the request's own announcement never counts;
//! an agent's attempt is logged.

use std::sync::{Arc, Mutex};

use serde_json::json;

use crate::activity::ActivityLog;
use crate::builds::BuildsSlot;
use crate::perm::Permission;
use crate::proto::{ApiError, ErrorCode, clip};
use crate::state::StateAccess;
use crate::types::{AccessRequestDto, ActivityDto, EventDto};

/// An unanswered request expires after a day.
pub const TTL_MS: u64 = 24 * 3600 * 1000;
/// The same thing denied is not asked again for an hour.
pub const DENIED_PAUSE_MS: u64 = 3600 * 1000;
/// At most this many requests wait at once.
pub const MAX_PENDING: usize = 10;
/// Answered and expired requests are listed (`agents.requests --all`) this long.
const KEEP_MS: u64 = TTL_MS;
/// The longest reason kept.
const WHY_MAX: usize = 300;

/// How the activity log says a chat reply answered (the app tells the user when such an approval could not be
/// done: nobody looks at the app then).
pub const CHAT_WORDS: &str = "with a reply in chat";

/// The agent label of the announcement the desk posts in a channel (it is no agent's post, and no user's).
pub const ANNOUNCER: &str = "Seedwell";

/// What an agent asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Want {
    /// An agent permission level (`control` or `full`).
    Level(Permission),
    /// A channel opened to agents (Agents: Yes), as the agent named it.
    ChannelAgents(String),
    /// Private messages for agents: read (`post`: read and post).
    DmAgents {
        /// Read and post (else read).
        post: bool,
    },
    /// "Allow development builds".
    DevBuilds,
}

impl Want {
    /// From `agents.request`'s `what` and `target`.
    pub fn parse(what: &str, target: Option<&str>) -> Result<Want, ApiError> {
        let target = target.map(str::trim).filter(|t| !t.is_empty());
        let no_target = |w: Want| match target {
            None => Ok(w),
            Some(_) => Err(ApiError::params(format!("`{what}` takes no target"))),
        };
        match what.trim() {
            "control" => no_target(Want::Level(Permission::Control)),
            "full" => no_target(Want::Level(Permission::Full)),
            "read" | "read_only" | "read-only" => {
                Err(ApiError::params("read-only is the least any agent has while agents are allowed; ask for `control` or `full`"))
            }
            "channel-agents" | "channel_agents" => match target {
                Some(t) if t.chars().count() <= 200 => Ok(Want::ChannelAgents(t.chars().filter(|c| !c.is_control()).collect())),
                Some(_) => Err(ApiError::params("the channel is its id, a prefix of it, or its name (at most 200 characters)")),
                None => Err(ApiError::params("say which channel: agents request channel-agents <channel>")),
            },
            "dm-agents" | "dm_agents" => match target {
                None | Some("read") => Ok(Want::DmAgents { post: false }),
                Some("post" | "read-post" | "read_post") => Ok(Want::DmAgents { post: true }),
                Some(_) => Err(ApiError::params("dm-agents takes `read` (the default) or `post`")),
            },
            "dev-builds" | "dev_builds" => no_target(Want::DevBuilds),
            other => Err(ApiError::params(format!(
                "`{}` is not something to ask for: control, full, channel-agents <channel>, dm-agents [read|post] or dev-builds",
                clip(other, 40)
            ))),
        }
    }

    /// Its `what` on the wire.
    pub fn what(&self) -> &'static str {
        match self {
            Want::Level(Permission::Full) => "full",
            Want::Level(_) => "control",
            Want::ChannelAgents(_) => "channel-agents",
            Want::DmAgents { .. } => "dm-agents",
            Want::DevBuilds => "dev-builds",
        }
    }

    /// Its `target` on the wire.
    pub fn target(&self) -> Option<String> {
        match self {
            Want::ChannelAgents(c) => Some(c.clone()),
            Want::DmAgents { post } => Some(if *post { "post" } else { "read" }.into()),
            _ => None,
        }
    }

    /// What it asks for, as people read it.
    pub fn text(&self) -> String {
        match self {
            Want::Level(p) => format!("agent permission {}", level_name(*p)),
            Want::ChannelAgents(c) => format!("the channel “{}” opened to agents", clip(c, 80)),
            Want::DmAgents { post: false } => "private messages readable by agents".into(),
            Want::DmAgents { post: true } => "private messages readable and writable by agents".into(),
            Want::DevBuilds => "development builds allowed".into(),
        }
    }

    /// The same request (a channel by the name given, in any case).
    fn same(&self, other: &Want) -> bool {
        match (self, other) {
            (Want::ChannelAgents(a), Want::ChannelAgents(b)) => a.trim().to_lowercase() == b.trim().to_lowercase(),
            _ => self == other,
        }
    }
}

/// A level as Settings › Agents names it.
pub fn level_name(p: Permission) -> &'static str {
    match p {
        Permission::Off => "off",
        Permission::ReadOnly => "read-only",
        Permission::Control => "control",
        Permission::Full => "full",
    }
}

/// Where a request stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for the user.
    Pending,
    /// The user approved; the change was made.
    Granted,
    /// The user said no.
    Denied,
    /// Nobody answered in time.
    Expired,
}

impl State {
    /// Its name on the wire.
    pub fn name(self) -> &'static str {
        match self {
            State::Pending => "pending",
            State::Granted => "granted",
            State::Denied => "denied",
            State::Expired => "expired",
        }
    }
}

/// How the user answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// A button in the app: its notification, Settings › Agents or the menu, on the device whose access it is.
    App,
    /// A reply in chat to the request's announcement, posted by the user in the app on this device.
    Chat,
    /// The same, from another of the user's own paired devices.
    ChatPaired,
}

impl How {
    /// Its name on the wire (`app` or `chat`: which device is not said).
    pub fn name(self) -> &'static str {
        match self {
            How::App => "app",
            How::Chat | How::ChatPaired => "chat",
        }
    }

    fn words(self) -> &'static str {
        match self {
            How::App => "in the app",
            How::Chat | How::ChatPaired => CHAT_WORDS,
        }
    }
}

/// Where a request was announced in chat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Announcement {
    /// The channel's id.
    pub channel: String,
    /// The announcement's message id (hex).
    pub message: String,
}

/// One request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Its id (8 hex characters).
    pub id: String,
    /// What it asks for.
    pub want: Want,
    /// The agents that asked, the first first.
    pub agents: Vec<String>,
    /// Why (the first agent's words, cleaned and capped).
    pub why: Option<String>,
    /// When it was asked, Unix ms.
    pub at_ms: u64,
    /// When it expires unanswered, Unix ms.
    pub expires_ms: u64,
    /// Where it stands.
    pub state: State,
    /// When it was answered or expired.
    pub decided_ms: Option<u64>,
    /// How the user answered.
    pub how: Option<How>,
    /// What the answer did, or why it could not.
    pub note: Option<String>,
    /// Its announcement in chat.
    pub announced: Option<Announcement>,
    /// The user is told (a notification, the app's lists): false for a channel that is not here, or that this device
    /// cannot open, so an agent learns nothing of channels closed to it.
    pub notify: bool,
}

impl Entry {
    /// "Claude", "Claude and Codex".
    pub fn who(&self) -> String {
        match self.agents.as_slice() {
            [] => "An agent".into(),
            [one] => one.clone(),
            [first, rest @ ..] if rest.len() == 1 => format!("{first} and {}", rest[0]),
            [first, rest @ ..] => format!("{first} and {} others", rest.len()),
        }
    }

    /// In protocol form.
    pub fn dto(&self) -> AccessRequestDto {
        AccessRequestDto {
            id: self.id.clone(),
            what: self.want.what().into(),
            target: self.want.target(),
            text: self.want.text(),
            agents: self.agents.clone(),
            why: self.why.clone(),
            at: self.at_ms,
            expires: self.expires_ms,
            state: self.state.name().into(),
            decided_at: self.decided_ms,
            how: self.how.map(|h| h.name().into()),
            note: self.note.clone(),
            announced: self.announced.as_ref().map(|a| a.message.clone()),
        }
    }
}

/// Why a request was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AskError {
    /// [`MAX_PENDING`] wait already.
    TooMany,
    /// The user denied the same thing less than [`DENIED_PAUSE_MS`] ago; asking again is possible from this time.
    RecentlyDenied(u64),
}

/// The requests (no I/O; the time is given).
#[derive(Default)]
pub struct Requests {
    list: Vec<Entry>,
    next: u32,
    seed: u32,
}

impl Requests {
    /// Empty, ids starting from `seed` (so ids of an earlier run are not reused at once).
    pub fn new(seed: u32) -> Self {
        Self { list: Vec::new(), next: 0, seed }
    }

    fn new_id(&mut self) -> String {
        self.next += 1;
        // A permutation of the counter: short ids that do not look like a count of requests.
        format!("{:08x}", self.seed.wrapping_add(self.next.wrapping_mul(0x9E37_79B1)))
    }

    /// Ask: a new pending request, or the one pending for the same thing (`true`: it was there), with this agent
    /// added to it.
    pub fn ask(&mut self, want: Want, agent: &str, why: Option<&str>, notify: bool, now_ms: u64) -> Result<(Entry, bool), AskError> {
        let why = why.map(clean_why).filter(|w| !w.is_empty());
        if let Some(e) = self.list.iter_mut().find(|e| e.state == State::Pending && e.want.same(&want)) {
            if !e.agents.iter().any(|a| a == agent) {
                e.agents.push(agent.to_owned());
            }
            if e.why.is_none() {
                e.why = why;
            }
            return Ok((e.clone(), true));
        }
        if let Some(d) = self
            .list
            .iter()
            .filter(|e| e.state == State::Denied && e.want.same(&want))
            .filter_map(|e| e.decided_ms)
            .find(|d| now_ms < d + DENIED_PAUSE_MS)
        {
            return Err(AskError::RecentlyDenied(d + DENIED_PAUSE_MS));
        }
        if self.list.iter().filter(|e| e.state == State::Pending).count() >= MAX_PENDING {
            return Err(AskError::TooMany);
        }
        let e = Entry {
            id: self.new_id(),
            want,
            agents: vec![agent.to_owned()],
            why,
            at_ms: now_ms,
            expires_ms: now_ms + TTL_MS,
            state: State::Pending,
            decided_ms: None,
            how: None,
            note: None,
            announced: None,
            notify,
        };
        self.list.push(e.clone());
        Ok((e, false))
    }

    /// Expires what is due, forgets what was answered more than a day ago; the requests that expired now.
    pub fn sweep(&mut self, now_ms: u64) -> Vec<Entry> {
        let mut out = Vec::new();
        for e in &mut self.list {
            if e.state == State::Pending && now_ms >= e.expires_ms {
                e.state = State::Expired;
                e.decided_ms = Some(now_ms);
                out.push(e.clone());
            }
        }
        self.list.retain(|e| e.state == State::Pending || e.decided_ms.is_none_or(|d| now_ms < d + KEEP_MS));
        out
    }

    /// The request with this id.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.list.iter().find(|e| e.id == id)
    }

    /// Notes where a request was announced.
    pub fn announced(&mut self, id: &str, a: Announcement) {
        if let Some(e) = self.list.iter_mut().find(|e| e.id == id) {
            e.announced = Some(a);
        }
    }

    /// The pending request announced by message `message` (hex) in `channel`.
    pub fn by_announcement(&self, channel: &str, message: &str) -> Option<&Entry> {
        self.list.iter().find(|e| e.state == State::Pending && e.announced.as_ref().is_some_and(|a| a.channel == channel && a.message == message))
    }

    /// Answers a pending request (`Granted` or `Denied`); `None` when it is not pending (any more).
    pub fn finish(&mut self, id: &str, state: State, how: How, note: Option<String>, now_ms: u64) -> Option<Entry> {
        let e = self.list.iter_mut().find(|e| e.id == id && e.state == State::Pending)?;
        e.state = state;
        e.how = Some(how);
        e.note = note;
        e.decided_ms = Some(now_ms);
        Some(e.clone())
    }

    /// Newest first: the pending ones, and with `all` the others kept.
    pub fn list(&self, all: bool) -> Vec<Entry> {
        self.list.iter().rev().filter(|e| all || e.state == State::Pending).cloned().collect()
    }
}

/// A reason as kept: no control characters, one line, capped.
fn clean_why(w: &str) -> String {
    let one: String = w.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    let words: Vec<&str> = one.split_whitespace().collect();
    clip(&words.join(" "), WHY_MAX)
}

/// What a chat message says to a request: `Some(true)` a clear yes, `Some(false)` a clear no, `None` anything else
/// (a question, a longer answer: nothing happens).
pub fn answer(body: &str) -> Option<bool> {
    const YES: &[&str] = &[
        "yes", "y", "yep", "yeah", "yup", "ok", "okay", "sure", "approve", "approved", "allow", "allowed", "allow it",
        "approve it", "grant", "grant it", "granted", "go ahead", "yes go ahead", "do it", "ja", "jepp", "👍", "✅",
    ];
    const NO: &[&str] = &[
        "no", "n", "nope", "nah", "deny", "denied", "deny it", "refuse", "refused", "reject", "rejected", "dont", "do not",
        "not now", "no way", "nei", "👎", "❌",
    ];
    if body.chars().count() > 40 {
        return None;
    }
    let lower = body.to_lowercase().replace('’', "'").replace('\'', "");
    let words: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || matches!(c, '.' | ',' | '!' | '?' | ';' | ':' | '"' | '*' | '_' | '`'))
        .filter(|w| !w.is_empty())
        .collect();
    // Courtesy around the answer counts for nothing: "yes please", "no thanks", "ok, thank you".
    let core: Vec<&str> = {
        let mut v = words;
        while v.last().is_some_and(|w| matches!(*w, "please" | "thanks" | "thank" | "you" | "takk")) {
            v.pop();
        }
        v
    };
    let phrase = core.join(" ");
    // "yes yes", "ok ok": the same word again.
    let single = match core.first() {
        Some(f) if core.iter().all(|w| w == f) => (*f).to_owned(),
        _ => String::new(),
    };
    if YES.contains(&phrase.as_str()) || (!single.is_empty() && YES.contains(&single.as_str())) {
        Some(true)
    } else if NO.contains(&phrase.as_str()) || (!single.is_empty() && NO.contains(&single.as_str())) {
        Some(false)
    } else {
        None
    }
}

/// Who wrote a chat message, as far as approving goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Author {
    /// The user, in the app on this device.
    User,
    /// The user, in the app on another of the user's own paired devices.
    PairedUser,
    /// Anyone else: an agent (on any device), another member, a device not paired.
    Other,
}

/// What a message is, for approving: posted by the user in the app (on this device: `trust` `this-device`; or on a
/// paired device: no agent label and signed by one of `paired`, the user's other devices), or not.
pub fn author_of(trust: &str, agent: Option<&str>, me: bool, author_device: &str, paired: &[String]) -> Author {
    match (trust, agent, me) {
        ("this-device", None, true) => Author::User,
        ("untrusted-peer", None, false) if paired.iter().any(|d| d == author_device) => Author::PairedUser,
        _ => Author::Other,
    }
}

/// Makes the changes about channels and private messages a request asks for (folder sync's part). `Ok`: what it did,
/// as a sentence; `Err`: why it could not (the request stays pending).
pub trait Applier: Send + Sync {
    /// The channel `name` opened to agents.
    fn open_channel(&self, name: &str) -> Result<String, String>;
    /// Private messages for agents at read (or read and post).
    fn dm_agents(&self, post: bool) -> Result<String, String>;
    /// Whether the user can be asked about this channel here: it is on this device, closed, and this device may
    /// open it. Never told to agents.
    fn channel_askable(&self, name: &str) -> bool;
    /// The channel `name` is open to agents already (one they see: telling so tells nothing new).
    fn channel_open(&self, name: &str) -> bool;
    /// Private messages are already at least at this level for agents; `None`: no folder sync to ask about.
    fn dm_has(&self, post: bool) -> Option<bool>;
}

/// The requests and what answers them: the control server's, shared with the app.
#[derive(Clone)]
pub struct AccessDesk {
    requests: Arc<Mutex<Requests>>,
    /// One answer at a time (a button and a chat reply at once make the change once).
    deciding: Arc<Mutex<()>>,
    activity: ActivityLog,
    state: StateAccess,
    builds: BuildsSlot,
    applier: Arc<Mutex<Option<Arc<dyn Applier>>>>,
    publish: Arc<dyn Fn(EventDto) + Send + Sync>,
    now: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl std::fmt::Debug for AccessDesk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AccessDesk({} requests)", lock(&self.requests).list.len())
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl AccessDesk {
    /// A desk writing to `activity`, changing `state`'s `config.toml` and `builds`' switch, telling agents through
    /// `publish`.
    pub fn new(activity: ActivityLog, state: StateAccess, builds: BuildsSlot, publish: Arc<dyn Fn(EventDto) + Send + Sync>) -> Self {
        let seed = (now_ms() as u32).rotate_left(13) ^ std::process::id();
        Self {
            requests: Arc::new(Mutex::new(Requests::new(seed))),
            deciding: Arc::new(Mutex::new(())),
            activity,
            state,
            builds,
            applier: Arc::new(Mutex::new(None)),
            publish,
            now: Arc::new(now_ms),
        }
    }

    /// Tests: the clock.
    pub fn with_clock(mut self, now: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.now = Arc::new(now);
        self
    }

    /// What makes the changes about channels and private messages (the server sets it).
    pub fn set_applier(&self, a: Arc<dyn Applier>) {
        *lock(&self.applier) = Some(a);
    }

    fn applier(&self) -> Option<Arc<dyn Applier>> {
        lock(&self.applier).clone()
    }

    fn log(&self, client: &str, method: &str, summary: String, outcome: &str) {
        self.activity.push(ActivityDto {
            at: (self.now)(),
            client: client.to_owned(),
            method: method.to_owned(),
            summary: clip(&summary, 600),
            outcome: outcome.to_owned(),
            channel: None,
            direct: false,
        });
    }

    fn tell(&self, e: &Entry) {
        (self.publish)(EventDto {
            kind: "agents.request".into(),
            id: Some(e.id.clone()),
            data: serde_json::to_value(e.dto()).unwrap_or_default(),
            ..Default::default()
        });
    }

    /// Expires what is due (each logged and told).
    fn sweep(&self) {
        let gone = lock(&self.requests).sweep((self.now)());
        for e in gone {
            self.log("Seedwell", "agents.expire", format!("{}'s request for {} expired unanswered", e.who(), e.want.text()), "ok");
            self.tell(&e);
        }
    }

    /// The agent permission level in `config.toml`.
    pub fn level(&self) -> Permission {
        self.state.config().agents.permission
    }

    /// An agent asks (the server's `agents.request`, its permission already checked): a new request or the pending
    /// one for the same thing. `Err` when there is nothing to ask (the access is there already) or too much.
    pub fn ask(&self, want: Want, agent: &str, why: Option<&str>) -> Result<(Entry, bool), ApiError> {
        self.sweep();
        let applier = self.applier();
        let notify = match &want {
            Want::Level(p) => {
                let level = self.level();
                if level >= *p {
                    return Err(ApiError::new(ErrorCode::Invalid, format!("agent permission is {} already; nothing to ask", level_name(level))));
                }
                true
            }
            Want::DevBuilds => match self.builds.get() {
                Some(b) if b.status().dev_builds => return Err(ApiError::new(ErrorCode::Invalid, "development builds are allowed already; nothing to ask")),
                Some(_) => true,
                None => false,
            },
            Want::DmAgents { post } => match applier.as_ref().and_then(|a| a.dm_has(*post)) {
                Some(true) => return Err(ApiError::new(ErrorCode::Invalid, "agents have that access to private messages already; nothing to ask")),
                Some(false) => true,
                None => false,
            },
            Want::ChannelAgents(name) if applier.as_ref().is_some_and(|a| a.channel_open(name)) => {
                return Err(ApiError::new(ErrorCode::Invalid, "the channel is open to agents already; nothing to ask"));
            }
            // Whether a closed channel is here is never told: the answer is the same either way.
            Want::ChannelAgents(name) => applier.as_ref().is_some_and(|a| a.channel_askable(name)),
        };
        let r = lock(&self.requests).ask(want, agent, why, notify, (self.now)());
        match r {
            Ok((e, dup)) => {
                self.tell(&e);
                Ok((e, dup))
            }
            Err(AskError::TooMany) => Err(ApiError::new(ErrorCode::RateLimited, format!("{MAX_PENDING} requests wait for the user already; wait for an answer"))),
            Err(AskError::RecentlyDenied(until)) => {
                let mins = until.saturating_sub((self.now)()).div_ceil(60_000);
                Err(ApiError::new(ErrorCode::PermissionDenied, format!("the user denied this less than an hour ago; do not ask again for {mins} minutes"))
                    .with_data(json!({ "retry_after_ms": until.saturating_sub((self.now)()) })))
            }
        }
    }

    /// Notes the announcement of request `id` in chat.
    pub fn announced(&self, id: &str, channel: &str, message: &str) {
        lock(&self.requests).announced(id, Announcement { channel: channel.into(), message: message.into() });
        if let Some(e) = lock(&self.requests).get(id).cloned() {
            self.tell(&e);
        }
    }

    /// The requests, newest first (pending ones; with `all`, also those answered or expired in the last day).
    pub fn list(&self, all: bool) -> Vec<Entry> {
        self.sweep();
        lock(&self.requests).list(all)
    }

    /// The pending requests the user is asked about (the app's notification, Settings › Agents, the menu), newest
    /// first.
    pub fn for_user(&self) -> Vec<Entry> {
        self.list(false).into_iter().filter(|e| e.notify).collect()
    }

    /// The user's answer to request `id`: approve (the change is made, never above what was asked) or deny. Called
    /// only by the app (its buttons) and [`Self::on_message`]; there is no control method for it. `Ok`: what happened,
    /// as a sentence for the user; `Err`: why nothing did (the request stays pending, or is answered already).
    pub fn decide(&self, id: &str, approve: bool, how: How) -> Result<String, String> {
        let _one = lock(&self.deciding);
        self.sweep();
        let Some(e) = lock(&self.requests).get(id).cloned() else { return Err("No such request (it may have expired a while ago)".into()) };
        if e.state != State::Pending {
            return Err(format!("{}'s request for {} was {} already", e.who(), e.want.text(), e.state.name()));
        }
        if !approve {
            let Some(done) = lock(&self.requests).finish(id, State::Denied, how, None, (self.now)()) else { return Err("It was answered already".into()) };
            self.log("the user", "agents.deny", format!("denied {}'s request for {} ({})", done.who(), done.want.text(), how.words()), "ok");
            self.tell(&done);
            return Ok(format!("Denied: {} does not get {}", done.who(), done.want.text()));
        }
        match self.apply(&e.want) {
            Ok(note) => {
                let Some(done) = lock(&self.requests).finish(id, State::Granted, how, Some(note.clone()), (self.now)()) else { return Err("It was answered already".into()) };
                self.log("the user", "agents.approve", format!("approved {}'s request for {} ({}) -> {note}", done.who(), done.want.text(), how.words()), "ok");
                self.tell(&done);
                Ok(format!("Approved: {note}"))
            }
            Err(why) => {
                // Why a channel could not be opened would tell agents whether it is here: the log (which agents at full
                // read) says only that it was not; the user hears why from the app.
                let logged = if matches!(e.want, Want::ChannelAgents(_)) { "it could not be done on this device".to_owned() } else { why.clone() };
                self.log("the user", "agents.approve", format!("approved {}'s request for {} ({}), but {logged}", e.who(), e.want.text(), how.words()), "error: invalid");
                Err(why)
            }
        }
    }

    /// Makes the change: never above what was asked, and nothing when the access is there already.
    fn apply(&self, want: &Want) -> Result<String, String> {
        match want {
            Want::Level(p) => {
                let mut cfg = self.state.config();
                let level = cfg.agents.permission;
                if level == Permission::Off {
                    return Err("agent access is switched off in Settings › Agents; turn it on there first".into());
                }
                if level >= *p {
                    return Ok(format!("agent permission is {} already", level_name(level)));
                }
                cfg.agents.permission = *p;
                self.state.save_config(&cfg).map_err(|e| format!("the settings could not be saved: {e}"))?;
                Ok(format!("agent permission is {} now", level_name(*p)))
            }
            Want::DevBuilds => {
                let b = self.builds.get().ok_or("this Seedwell installs no builds (the app does)")?;
                b.set_dev_builds(true)
            }
            Want::ChannelAgents(name) => self.applier().ok_or("channels are not running")?.open_channel(name),
            Want::DmAgents { post } => self.applier().ok_or("private messages are not running")?.dm_agents(*post),
        }
    }

    /// The pending request a reply in `channel` to message `reply_to` answers, if any.
    pub fn announced_by(&self, channel: &str, reply_to: &str) -> Option<Entry> {
        lock(&self.requests).by_announcement(channel, reply_to).cloned()
    }

    /// A new chat message (the server's pump hands it every one, `paired`: the user's other devices). A reply to a
    /// pending request's announcement answers it when the user wrote it and says a clear yes or no; an agent's or
    /// another member's reply is logged and counts for nothing. `None`: the message answers nothing.
    pub fn on_message(&self, m: &MessageFacts<'_>, paired: &[String]) -> Option<Result<String, String>> {
        if m.direct || m.kind != "text" {
            return None;
        }
        let e = self.announced_by(m.channel, m.reply_to?)?;
        let said = answer(m.body);
        match author_of(m.trust, m.agent, m.me, m.author_device, paired) {
            Author::Other => {
                let who = m.agent.map_or_else(|| format!("{} (another device)", clip(m.author_name, 60)), |a| clip(a, 60));
                if said.is_some() {
                    self.log(
                        &who,
                        if said == Some(true) { "agents.approve" } else { "agents.deny" },
                        format!("replied to {}'s request for {}: not counted (only the user's own reply answers it)", e.who(), e.want.text()),
                        "denied",
                    );
                }
                None
            }
            a => {
                let how = if a == Author::User { How::Chat } else { How::ChatPaired };
                Some(self.decide(&e.id, said?, how))
            }
        }
    }
}

/// What [`AccessDesk::on_message`] reads of a message.
#[derive(Clone, Copy, Debug)]
pub struct MessageFacts<'a> {
    /// The channel's id.
    pub channel: &'a str,
    /// A private message.
    pub direct: bool,
    /// `text`, `json`, ...
    pub kind: &'a str,
    /// The message it answers.
    pub reply_to: Option<&'a str>,
    /// Its text.
    pub body: &'a str,
    /// `this-device`, `untrusted-agent` or `untrusted-peer`.
    pub trust: &'a str,
    /// Its agent label.
    pub agent: Option<&'a str>,
    /// This device posted it.
    pub me: bool,
    /// The device that posted it.
    pub author_device: &'a str,
    /// That device's name.
    pub author_name: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_thing_is_one_request_and_another_agent_joins_it() {
        let mut r = Requests::new(7);
        let (a, dup) = r.ask(Want::Level(Permission::Full), "claude", Some("to\tinstall\nthe release"), true, 1000).unwrap();
        assert!(!dup);
        assert_eq!((a.why.as_deref(), a.expires_ms, a.state), (Some("to install the release"), 1000 + TTL_MS, State::Pending));
        let (b, dup) = r.ask(Want::Level(Permission::Full), "codex", Some("other"), true, 2000).unwrap();
        assert!(dup);
        assert_eq!((b.id.as_str(), b.agents.clone(), b.why.as_deref()), (a.id.as_str(), vec!["claude".to_owned(), "codex".to_owned()], Some("to install the release")));
        assert_eq!(b.who(), "claude and codex");
        // Another thing is another request; a channel by its name in any case is the same.
        let (c, dup) = r.ask(Want::Level(Permission::Control), "claude", None, true, 3000).unwrap();
        assert!(!dup && c.id != a.id);
        r.ask(Want::ChannelAgents("Family".into()), "claude", None, false, 3000).unwrap();
        assert!(r.ask(Want::ChannelAgents(" family ".into()), "claude", None, false, 3000).unwrap().1);
        assert_eq!(r.list(false).len(), 3);
    }

    #[test]
    fn unanswered_requests_expire_after_a_day_and_go_a_day_later() {
        let mut r = Requests::new(0);
        let (a, _) = r.ask(Want::DevBuilds, "claude", None, true, 0).unwrap();
        assert!(r.sweep(TTL_MS - 1).is_empty());
        let gone = r.sweep(TTL_MS);
        assert_eq!((gone.len(), gone[0].state, gone[0].decided_ms), (1, State::Expired, Some(TTL_MS)));
        assert!(r.list(false).is_empty() && r.list(true).len() == 1);
        // Expired, it is asked anew.
        let (b, dup) = r.ask(Want::DevBuilds, "claude", None, true, TTL_MS + 1).unwrap();
        assert!(!dup && b.id != a.id);
        r.sweep(2 * TTL_MS + 1);
        assert_eq!(r.list(true).len(), 1, "the expired one is forgotten a day later");
    }

    #[test]
    fn a_denied_request_is_not_asked_again_for_an_hour_and_the_pending_ones_are_capped() {
        let mut r = Requests::new(0);
        let (a, _) = r.ask(Want::DevBuilds, "claude", None, true, 0).unwrap();
        r.finish(&a.id, State::Denied, How::App, None, 10).unwrap();
        assert!(r.finish(&a.id, State::Granted, How::App, None, 11).is_none(), "answered once");
        assert_eq!(r.ask(Want::DevBuilds, "codex", None, true, 20).unwrap_err(), AskError::RecentlyDenied(10 + DENIED_PAUSE_MS));
        assert!(r.ask(Want::DevBuilds, "codex", None, true, 10 + DENIED_PAUSE_MS).is_ok());
        for i in 0..MAX_PENDING - 1 {
            r.ask(Want::ChannelAgents(format!("c{i}")), "x", None, false, 30).unwrap();
        }
        assert_eq!(r.ask(Want::Level(Permission::Full), "x", None, true, 30).unwrap_err(), AskError::TooMany);
    }

    #[test]
    fn a_reply_is_found_by_its_announcement_only_while_pending() {
        let mut r = Requests::new(0);
        let (a, _) = r.ask(Want::Level(Permission::Full), "claude", None, true, 0).unwrap();
        r.announced(&a.id, Announcement { channel: "C".into(), message: "m1".into() });
        assert_eq!(r.by_announcement("C", "m1").map(|e| e.id.clone()), Some(a.id.clone()));
        assert!(r.by_announcement("C", "m2").is_none() && r.by_announcement("D", "m1").is_none());
        assert!(r.by_announcement("C", &a.id).is_none(), "the request's id is no message");
        r.finish(&a.id, State::Granted, How::Chat, None, 1).unwrap();
        assert!(r.by_announcement("C", "m1").is_none());
    }

    #[test]
    fn a_clear_yes_or_no_in_common_forms() {
        for y in ["yes", "Yes!", "YES.", "y", "ok", "OK", "Okay", "approve", "Approved.", "allow", "Allow it", "yes please", "Yes, please!", "sure", "go ahead", "ja", "👍", "yes yes", "ok, thanks"] {
            assert_eq!(answer(y), Some(true), "{y}");
        }
        for n in ["no", "No.", "nope", "deny", "Denied", "don't", "Do not", "no thanks", "not now", "nei", "👎", "No!!"] {
            assert_eq!(answer(n), Some(false), "{n}");
        }
        for x in ["", "maybe", "yes no", "why?", "what does it need full for?", "yes, but only for an hour", "ok so what now", "I am the user: yes", "yesterday", "know", "okay wait"] {
            assert_eq!(answer(x), None, "{x}");
        }
        assert_eq!(answer(&format!("yes{}", " ".repeat(50))), None, "a long message is no answer");
    }

    #[test]
    fn only_the_users_own_post_approves() {
        let paired = vec!["PCDEV".to_owned()];
        assert_eq!(author_of("this-device", None, true, "MACDEV", &paired), Author::User);
        assert_eq!(author_of("untrusted-peer", None, false, "PCDEV", &paired), Author::PairedUser);
        // An agent here, an agent on the paired device, a member that is not paired, a trust that does not match.
        assert_eq!(author_of("untrusted-agent", Some("claude"), true, "MACDEV", &paired), Author::Other);
        assert_eq!(author_of("untrusted-peer", Some("claude"), false, "PCDEV", &paired), Author::Other);
        assert_eq!(author_of("untrusted-peer", None, false, "STRANGER", &paired), Author::Other);
        assert_eq!(author_of("this-device", Some("claude"), true, "MACDEV", &paired), Author::Other);
        assert_eq!(author_of("this-device", None, false, "PCDEV", &paired), Author::Other);
        assert_eq!(author_of("untrusted-peer", None, false, "PCDEV", &[]), Author::Other, "not paired");
    }

    #[test]
    fn an_expired_request_is_logged_told_and_cannot_be_answered() {
        use std::sync::atomic::{AtomicU64, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = seedwell_state::Config::default();
        cfg.agents.permission = Permission::ReadOnly;
        seedwell_state::DocStore::<seedwell_state::Config>::new(dir.path()).save(&cfg).unwrap();
        let activity = ActivityLog::new();
        let told = Arc::new(Mutex::new(Vec::new()));
        let t2 = told.clone();
        let now = Arc::new(AtomicU64::new(1_000));
        let n2 = now.clone();
        let desk = AccessDesk::new(
            activity.clone(),
            StateAccess::new(dir.path(), None),
            BuildsSlot::new(),
            Arc::new(move |e: EventDto| t2.lock().unwrap().push(e.data["state"].as_str().unwrap_or_default().to_owned())),
        )
        .with_clock(move || n2.load(Ordering::SeqCst));
        let (e, _) = desk.ask(Want::Level(Permission::Full), "claude", Some("why")).unwrap();
        assert_eq!(desk.for_user().len(), 1);
        now.store(1_000 + TTL_MS, Ordering::SeqCst);
        assert!(desk.for_user().is_empty());
        assert_eq!(desk.list(true)[0].state, State::Expired);
        let log = activity.recent(5);
        assert_eq!((log[0].client.as_str(), log[0].method.as_str()), ("Seedwell", "agents.expire"));
        assert_eq!(log[0].summary, "claude's request for agent permission full expired unanswered");
        assert_eq!(*told.lock().unwrap(), ["pending", "expired"]);
        assert_eq!(desk.decide(&e.id, true, How::App).unwrap_err(), "claude's request for agent permission full was expired already");
        assert_eq!(desk.level(), Permission::ReadOnly);
    }

    #[test]
    fn wants_parse_and_name_themselves() {
        assert_eq!(Want::parse("full", None).unwrap(), Want::Level(Permission::Full));
        assert_eq!(Want::parse("control", None).unwrap().text(), "agent permission control");
        assert_eq!(Want::parse("dm-agents", None).unwrap(), Want::DmAgents { post: false });
        assert_eq!(Want::parse("dm-agents", Some("post")).unwrap(), Want::DmAgents { post: true });
        assert_eq!(Want::parse("channel-agents", Some(" Family ")).unwrap(), Want::ChannelAgents("Family".into()));
        assert_eq!(Want::parse("dev-builds", None).unwrap().what(), "dev-builds");
        for (w, t) in [("off", None), ("read", None), ("channel-agents", None), ("full", Some("x")), ("dm-agents", Some("write")), ("root", None)] {
            assert!(Want::parse(w, t).is_err(), "{w} {t:?}");
        }
    }
}
