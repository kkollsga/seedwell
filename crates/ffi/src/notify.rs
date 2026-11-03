//! Notifications (SPEC §9, 7A.1, 7E.7, 7G.7) and the settings banner. Which core events become a system notification,
//! their words, bursts and grouping, what keeps one back (the Notifications settings, a hidden tracker, a muted channel,
//! a conversation already on screen, the starting notifier) and what a click opens (docs/CROSS-PLATFORM.md §1).
//!
//! The host makes one [`Notices`] and, for each core event it hears ([`crate::CoreListener`]), asks
//! [`Notices::event`]: post the [`Notice`] it returns (macOS: UNUserNotificationCenter; Windows: an app notification)
//! with [`Notice::click`] kept in it, or open the route it returns. It asks [`Notices::chat`] every
//! [`NoticesModel::chat_poll_ms`] for the banners of new chat messages. A click on a notification gives its `click`
//! back to [`notice_route`], which says what to open; the string survives a restart of the app.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::chat_page::{chat_notification_text, chat_place_title};
use crate::chat_text::general_unread;
use crate::library::LibraryPlace;
use crate::menu::APP_NAME;
use crate::row_text::category_label;
use crate::sync::SyncMessage;
use crate::types::{AgentActivity, AutoAdded, TrackerSummary, TransferRow};
use crate::Core;

/// More than this many finished downloads within [`BURST_MS`] become one "N downloads finished".
const BURST_MAX: usize = 3;
const BURST_MS: u64 = 10_000;
/// How often the host asks [`Notices::chat`].
const CHAT_POLL_MS: u64 = 2_000;
/// The burst notice replaces itself.
const BURST_KEY: &str = "finished-burst";

/// What a notification is about (for the host's logs and tests; every word is in the [`Notice`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum NoticeKind {
    /// "Download finished".
    Finished,
    /// "5 downloads finished": more than three within ten seconds.
    FinishedBurst,
    /// "Problem with …".
    Error,
    /// "Seeding goal reached".
    GoalReached,
    /// "Choose what to download".
    NeedsChoice,
    /// "Added …" by a watch folder or a feed (SPEC 7A.1).
    Added,
    /// "Already in Seedwell", or "Added 2 trackers to …".
    Duplicate,
    /// What an agent did (Settings › Agents › Notify).
    Agent,
    /// A new message in a conversation.
    ChatMessage,
    /// An agent asks for access (SPEC 9A.3): Approve and Deny as the notification's actions.
    AgentRequest,
    /// A file of an on-demand folder could not be opened ([`Notices::sync_ondemand_notice`]).
    SyncProblem,
    /// The user's "Free up space" kept some files, and why ([`Notices::sync_evict_notice`]).
    SyncKept,
}

/// What a click on a notification opens.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum NoticeRoute {
    /// The library window as it was.
    Window,
    /// The library window on this transfer: its list, the row selected and scrolled to (the Mac's `showMain(nil, id)`).
    Transfer { id: String },
    /// The chooser of what to download of a held torrent.
    Chooser { id: String },
    /// The starting notifier, on a torrent counting down to its start: the host asks `Starting::reveal`, then shows
    /// the notifier as `Starting::refresh` says.
    Starting { id: String },
    /// A place of the window (a conversation).
    Place { place: LibraryPlace },
    /// Settings › Agents, where agents' requests for access wait (with Approve and Deny).
    AgentRequests,
}

/// A button of a notification (macOS: a `UNNotificationAction` of the notice's category; Windows: a toast button).
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct NoticeAction {
    /// What the host gives back to [`notice_decision`] with the notice's `click`.
    pub id: String,
    pub title: String,
    /// Drawn as destructive (Deny).
    pub destructive: bool,
    /// The system asks to unlock the device first (Approve).
    pub authenticate: bool,
}

/// A kind of notification with buttons: the host registers each at launch ([`notice_categories`]).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NoticeCategory {
    pub id: String,
    pub actions: Vec<NoticeAction>,
}

/// What a notification's button decides: the user's answer to an agent's request for access, which the host passes
/// to `Core::agent_request_decide`.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NoticeDecision {
    pub request_id: String,
    pub approve: bool,
}

/// The category of a request's notice.
const REQUEST_CATEGORY: &str = "agent-request";
/// A request's notice key is this and the request's id.
const REQUEST_KEY_PREFIX: &str = "agent-request-";

/// How hard a notification asks to be seen. The host maps it to what its system offers; it never changes the user's
/// own choices (alert style, Focus, Do Not Disturb), which decide in the end.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum NoticeUrgency {
    /// An ordinary banner (macOS: `.active`; Windows: a default toast).
    Normal,
    /// It is about now: it may break through a Focus that allows time-sensitive notifications (macOS:
    /// `.timeSensitive`; Windows: a default toast).
    TimeSensitive,
    /// Someone waits for the answer: time-sensitive, and on screen until it is acted on where the system lets an app
    /// ask for that (macOS: `.timeSensitive`, as apps cannot make an alert persistent: the user's alert style for the
    /// app does; Windows: a toast with `scenario="reminder"`, which stays until a button or a dismissal). The host
    /// takes it away once it is answered anywhere ([`Notices::withdrawn`]).
    Persistent,
}

/// One system notification, ready to post.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct Notice {
    pub kind: NoticeKind,
    pub title: String,
    pub body: String,
    /// A later notice with the same key takes the place of the earlier one (the burst, a held torrent's choice, a
    /// conversation); `None`: a new notification each time. (Windows' tag holds 64 characters: hash a longer one.)
    pub key: Option<String>,
    /// What a click opens.
    pub route: NoticeRoute,
    /// The route as text: the host keeps it with the notification (macOS: `userInfo`; Windows: the launch argument) and
    /// gives it to [`notice_route`] after a click.
    pub click: String,
    /// Its buttons' category ([`notice_categories`]); `None`: no buttons.
    pub category: Option<String>,
    /// Its buttons, in order (Windows adds them to the toast; the Mac's come with the category).
    pub actions: Vec<NoticeAction>,
    /// How hard it asks to be seen.
    pub urgency: NoticeUrgency,
}

/// What the host does about an event.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum NoticeOutcome {
    /// Nothing: the setting is off, the tracker is hidden, the starting notifier shows it already, …
    Nothing,
    /// Post this notification.
    Post { notice: Notice },
    /// Open this, without a notification ("Choose Files…" on a magnet link asked for the chooser).
    Open { route: NoticeRoute },
}

/// A core event, as the host's listener heard it.
#[derive(Clone, Debug, uniffi::Enum)]
pub enum NoticeEvent {
    /// `on_transfer_finished`.
    Finished { row: TransferRow },
    /// `on_error`.
    Error { id: Option<String>, message: String },
    /// `on_goal_reached`.
    GoalReached { row: TransferRow },
    /// `on_needs_choice`, with what the host knows: the user asked for this torrent's chooser ("Choose Files…" on a
    /// magnet link), and the starting notifier is on screen after the host refreshed it for this event (it is the
    /// notice then, and holds the checklist, SPEC 8.6).
    NeedsChoice { row: TransferRow, chooser_requested: bool, starting_shown: bool },
    /// `on_auto_added`.
    AutoAdded { added: AutoAdded },
    /// `on_duplicate`.
    Duplicate { id: String, name: String, trackers_added: u32 },
    /// `on_agent_activity`.
    Agent { activity: AgentActivity },
}

/// What is on screen, for the chat banners: a conversation the user is looking at gets none.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NoticeView {
    /// The app is the active one (a quiet QA run, which never is, says true).
    pub in_front: bool,
    /// The place the library window shows; `None` while it is closed.
    pub window_place: Option<LibraryPlace>,
}

/// The host's part: how often to poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct NoticesModel {
    /// Ask [`Notices::chat`] this often, the window open or not.
    pub chat_poll_ms: u64,
}

/// The notifier: one per app run. It remembers the finished burst and the unread counts it has seen.
#[derive(uniffi::Object)]
pub struct Notices {
    core: Arc<Core>,
    state: Mutex<State>,
    start: Instant,
}

#[derive(Default)]
pub(crate) struct State {
    /// When the finished notices of the last [`BURST_MS`] were posted (ms since the notifier was made).
    burst: Vec<u64>,
    /// The agents' requests for access already posted (each is posted once).
    requests_posted: std::collections::HashSet<String>,
    /// When each on-demand problem was last told (ms since the notifier was made), by its key.
    od_told: HashMap<String, u64>,
    /// Each conversation's unread count at the last poll; `None` before the first, which only takes the baseline.
    seen: Option<HashMap<LibraryPlace, u32>>,
}

#[uniffi::export]
impl Notices {
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        Arc::new(Self { core, state: Mutex::new(State::default()), start: Instant::now() })
    }

    pub fn model(&self) -> NoticesModel {
        NoticesModel { chat_poll_ms: CHAT_POLL_MS }
    }

    /// What to do about a core event. Reads the settings and the library, so the host calls it from its own thread
    /// (inside the listener's callback it is moved off the core's runtime).
    pub fn event(&self, event: NoticeEvent) -> NoticeOutcome {
        let facts = off_runtime(|| Facts::read(&self.core, &event));
        let now = self.start.elapsed().as_millis() as u64;
        decide(&mut self.state.lock().unwrap_or_else(|e| e.into_inner()), event, &facts, now)
    }

    /// The notifications to take away now, of `delivered` (the keys of the app's notifications the system still holds,
    /// on screen or in its list): an agent's request answered anywhere (a button, Settings, the menu, a chat reply),
    /// expired or withdrawn, so a stale one is never answered. The host asks after each agent event and each answer,
    /// and once at launch (what an earlier run left).
    pub fn withdrawn(&self, delivered: Vec<String>) -> Vec<String> {
        if !delivered.iter().any(|k| k.starts_with(REQUEST_KEY_PREFIX)) {
            return Vec::new();
        }
        let pending = off_runtime(|| self.core.backend.agent_requests());
        stale_request_keys(&delivered, &pending)
    }

    /// Whether the user is told that an on-demand call failed (`kind`, from `CoreError::OnDemand`) for `path` (the
    /// item's path in folder `folder_id`), and how. The host asks whenever a call it made for the file manager failed
    /// (Windows: a Cloud Files callback answered with an error, whose own message Explorer shows): only what the user
    /// can act on is told (a version no device has, a file no connected device can give), once per file in
    /// [`OD_QUIET_MS`]; the rest (an item gone, an anchor expired, a change that came first) the host handles itself.
    pub fn sync_ondemand_notice(&self, folder_id: String, kind: crate::sync::ondemand::SyncErrorKind, path: String) -> Option<Notice> {
        let folder = off_runtime(|| self.core.backend.sync().snapshot().folders.into_iter().find(|f| f.id == folder_id).map(|f| f.name));
        let now = self.start.elapsed().as_millis() as u64;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ondemand_notice(&mut state.od_told, now, &folder_id, folder.as_deref(), kind, &path)
    }

    /// What the user is told after freeing space in an on-demand folder (`evicted`, from `sync_evict`) kept files: one
    /// notice for the folder, "N files stay on this computer" and why, in the core's words, which a second free of the
    /// folder replaces. For a host whose file manager gives a free no error to show (Windows: Explorer's "Free up
    /// space" only sets an attribute). `None` when everything was freed, and for the system's own clean-ups
    /// (`initiated_by_user` false: Storage Sense, disk pressure), which stay silent.
    pub fn sync_evict_notice(&self, folder_id: String, evicted: crate::sync::ondemand::SyncEvicted, initiated_by_user: bool) -> Option<Notice> {
        if !initiated_by_user || (evicted.kept_why.is_empty() && evicted.failed.is_empty()) {
            return None;
        }
        let folder = off_runtime(|| self.core.backend.sync().snapshot().folders.into_iter().find(|f| f.id == folder_id).map(|f| f.name));
        evict_notice(&folder_id, folder.as_deref(), &evicted, initiated_by_user)
    }

    /// The banners of new chat messages since the last call: for each conversation (a channel's General or topic, a
    /// private one) whose unread count went up, its newest message, when another device sent it, the channel is not
    /// muted and the conversation is not on screen. The first call only takes the counts.
    pub fn chat(&self, view: NoticeView) -> Vec<Notice> {
        let core: &Core = &self.core;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let seen = &mut state.seen;
        off_runtime(move || chat_step(seen, conversations(core), &view, |p| last_message(core, p)))
    }
}

/// How long the same on-demand problem of one file is not told again.
pub const OD_QUIET_MS: u64 = 10 * 60 * 1000;

/// [`Notices::sync_ondemand_notice`]'s rule: `told` holds when each key was last told; `folder` is the folder's name
/// (`None`: no longer synced here, nothing to tell).
pub(crate) fn ondemand_notice(
    told: &mut HashMap<String, u64>,
    now: u64,
    folder_id: &str,
    folder: Option<&str>,
    kind: crate::sync::ondemand::SyncErrorKind,
    path: &str,
) -> Option<Notice> {
    use crate::sync::ondemand::SyncErrorKind as K;
    let folder = folder?;
    let name = path.rsplit('/').find(|n| !n.is_empty()).unwrap_or(folder);
    let (title, body) = match kind {
        K::Unavailable => (
            format!("Can't open “{name}”"),
            format!("No device has this version of it right now. It opens once a device that has it connects (“{folder}” in Folder Sync)."),
        ),
        K::NotHereYet => (
            format!("“{name}” isn't here yet"),
            format!("No device that has it is connected. Open it again once one is (“{folder}” in Folder Sync shows the devices)."),
        ),
        K::NoItem | K::NotOnDemand | K::Gone | K::NotRunning | K::AnchorExpired | K::Changed => return None,
    };
    // One notification per file and problem (Windows' tag holds 64 characters: the path is hashed).
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        (folder_id, path, kind).hash(&mut h);
        format!("sync-od-{:016x}", h.finish())
    };
    if told.get(&key).is_some_and(|t| now.saturating_sub(*t) < OD_QUIET_MS) {
        return None;
    }
    told.retain(|_, t| now.saturating_sub(*t) < OD_QUIET_MS);
    told.insert(key.clone(), now);
    let route = NoticeRoute::Place { place: LibraryPlace::FolderSync };
    let click = click_of(&route);
    Some(Notice { kind: NoticeKind::SyncProblem, title, body, key: Some(key), route, click, category: None, actions: Vec::new(), urgency: NoticeUrgency::Normal })
}

/// Why a file a free kept stays ([`evict_notice`]).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kept {
    Pinned,
    OnlyCopy,
    Changed,
    InUse,
    Other(String),
}

/// [`Notices::sync_evict_notice`]'s words: `folder` is the folder's name (`None`: no longer synced here, nothing to
/// tell).
pub(crate) fn evict_notice(folder_id: &str, folder: Option<&str>, e: &crate::sync::ondemand::SyncEvicted, initiated_by_user: bool) -> Option<Notice> {
    use trs_sync::ondemand::{KEPT_CHANGED, KEPT_PINNED, NOT_HELD_ELSEWHERE};
    let folder = folder?;
    if !initiated_by_user {
        return None;
    }
    let mut why: std::collections::BTreeMap<Kept, u64> = std::collections::BTreeMap::new();
    for k in &e.kept_why {
        let kind = match k.error.as_str() {
            KEPT_PINNED => Kept::Pinned,
            NOT_HELD_ELSEWHERE => Kept::OnlyCopy,
            KEPT_CHANGED => Kept::Changed,
            other => Kept::Other(other.to_string()),
        };
        *why.entry(kind).or_default() += 1;
    }
    if !e.failed.is_empty() {
        *why.entry(Kept::InUse).or_default() += e.failed.len() as u64;
    }
    let n: u64 = why.values().sum();
    if n == 0 {
        return None;
    }
    let one = e.kept_why.iter().chain(&e.failed).next().filter(|_| n == 1);
    let title = match one {
        Some(f) => format!("“{}” stays on this {}", f.path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(&f.path), trs_platform::computer!()),
        None => format!("{n} files stay on this {}", trs_platform::computer!()),
    };
    let mut body = Vec::new();
    for (kind, c) in &why {
        let c = *c;
        // "It" for the one file the title names, "They" for all of them, else how many.
        let (s, is, it) = match (n, c == n) {
            (1, _) => ("It".to_string(), "is", "it"),
            (_, true) => ("They".to_string(), "are", "them"),
            _ if c == 1 => ("1".to_string(), "is", "it"),
            _ => (c.to_string(), "are", "them"),
        };
        body.push(match kind {
            Kept::Pinned => format!("{s} {is} set to always stay on this {}: stop keeping {it} here first.", trs_platform::computer!()),
            Kept::OnlyCopy => format!("{s} {is} kept until a device that has {it} too is connected, so nothing is lost."),
            Kept::Changed => format!("{s} changed on this {} and {is} synced first.", trs_platform::computer!()),
            Kept::InUse => format!("{s} couldn't be freed: {} open in another app.", if it == "it" { "it may be" } else { "they may be" }),
            Kept::Other(why) => format!("{s}: {why}."),
        });
    }
    body.push(format!("(“{folder}” in Folder Sync)"));
    // One per folder: a later free replaces it (Windows' tag holds 64 characters: the id is hashed).
    let key = {
        use std::hash::{Hash, Hasher};
        let mut h = std::hash::DefaultHasher::new();
        folder_id.hash(&mut h);
        format!("sync-kept-{:016x}", h.finish())
    };
    let route = NoticeRoute::Place { place: LibraryPlace::FolderSync };
    let click = click_of(&route);
    Some(Notice { kind: NoticeKind::SyncKept, title, body: body.join(" "), key: Some(key), route, click, category: None, actions: Vec::new(), urgency: NoticeUrgency::Normal })
}

/// The kinds of notification with buttons, for the host to register at launch (macOS: `UNNotificationCategory`).
#[uniffi::export]
pub fn notice_categories() -> Vec<NoticeCategory> {
    vec![NoticeCategory { id: REQUEST_CATEGORY.into(), actions: request_actions() }]
}

fn request_actions() -> Vec<NoticeAction> {
    vec![
        NoticeAction { id: "approve".into(), title: "Approve".into(), destructive: false, authenticate: true },
        NoticeAction { id: "deny".into(), title: "Deny".into(), destructive: true, authenticate: false },
    ]
}

/// What a notification's button decides: `action` (a [`NoticeAction::id`]) on the notice whose `click` it is. `None`:
/// a button of no request (the host opens the notice's route instead).
#[uniffi::export]
pub fn notice_decision(click: String, action: String) -> Option<NoticeDecision> {
    let id = click.strip_prefix("agent-request\n").filter(|id| !id.is_empty() && !id.contains('\n'))?;
    let approve = match action.as_str() {
        "approve" => true,
        "deny" => false,
        _ => return None,
    };
    Some(NoticeDecision { request_id: id.to_owned(), approve })
}

/// What a click on a notification opens: the route of its [`Notice::click`]. Text that is no route (a notification of
/// an older version) opens the window.
#[uniffi::export]
pub fn notice_route(click: String) -> NoticeRoute {
    let mut parts = click.split('\n');
    let kind = parts.next().unwrap_or_default();
    let mut next = || parts.next().filter(|s| !s.is_empty()).map(str::to_owned);
    match kind {
        "transfer" => next().map_or(NoticeRoute::Window, |id| NoticeRoute::Transfer { id }),
        "chooser" => next().map_or(NoticeRoute::Window, |id| NoticeRoute::Chooser { id }),
        "starting" => next().map_or(NoticeRoute::Window, |id| NoticeRoute::Starting { id }),
        "chat" => match next() {
            Some(channel_id) => NoticeRoute::Place { place: LibraryPlace::Chat { channel_id, topic_id: next() } },
            None => NoticeRoute::Window,
        },
        "direct" => next().map_or(NoticeRoute::Window, |device_id| NoticeRoute::Place { place: LibraryPlace::Direct { device_id } }),
        "agent-request" | "agent-requests" => NoticeRoute::AgentRequests,
        "folder-sync" => NoticeRoute::Place { place: LibraryPlace::FolderSync },
        _ => NoticeRoute::Window,
    }
}

/// The route as text ([`notice_route`] reads it back); ids hold no line breaks.
pub(crate) fn click_of(route: &NoticeRoute) -> String {
    match route {
        NoticeRoute::Window => "window".into(),
        NoticeRoute::Transfer { id } => format!("transfer\n{id}"),
        NoticeRoute::Chooser { id } => format!("chooser\n{id}"),
        NoticeRoute::Starting { id } => format!("starting\n{id}"),
        NoticeRoute::Place { place: LibraryPlace::Chat { channel_id, topic_id } } => {
            format!("chat\n{channel_id}\n{}", topic_id.as_deref().unwrap_or_default())
        }
        NoticeRoute::Place { place: LibraryPlace::Direct { device_id } } => format!("direct\n{device_id}"),
        NoticeRoute::Place { place: LibraryPlace::FolderSync } => "folder-sync".into(),
        NoticeRoute::Place { .. } => "window".into(),
        NoticeRoute::AgentRequests => "agent-requests".into(),
    }
}

/// Runs `f` on another thread when called on the core's runtime, where reading the library (a blocking call into that
/// runtime) would panic.
fn off_runtime<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| s.spawn(f).join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
    } else {
        f()
    }
}

fn notice(kind: NoticeKind, title: String, body: String, key: Option<String>, route: NoticeRoute) -> NoticeOutcome {
    let click = click_of(&route);
    NoticeOutcome::Post { notice: Notice { kind, title, body, key, route, click, category: None, actions: Vec::new(), urgency: NoticeUrgency::Normal } }
}

/// The notice key (the host's notification identifier) of request `id`'s notice.
fn request_key(id: &str) -> String {
    format!("{REQUEST_KEY_PREFIX}{id}")
}

/// The keys among `delivered` (the host's notifications still on screen or in its list) of request notices whose
/// request no longer waits (`pending`: the requests that do): answered anywhere, expired or withdrawn. Other keys are
/// never named.
pub(crate) fn stale_request_keys(delivered: &[String], pending: &[trs_control::access::Entry]) -> Vec<String> {
    delivered
        .iter()
        .filter(|k| k.strip_prefix(REQUEST_KEY_PREFIX).is_some_and(|id| !pending.iter().any(|e| e.id == id)))
        .cloned()
        .collect()
}

/// An agent's request for access, with Approve and Deny (Settings › Agents on a click). Its words are the core's; the
/// reason is the agent's own, quoted.
pub(crate) fn request_notice(e: &trs_control::access::Entry) -> NoticeOutcome {
    let mut body = crate::agent_requests::quoted_why(e).map(|w| format!("{w}\n")).unwrap_or_default();
    body.push_str("Approve gives exactly this; Deny refuses. Also in Settings › Agents.");
    NoticeOutcome::Post {
        notice: Notice {
            kind: NoticeKind::AgentRequest,
            title: crate::agent_requests::request_title(e),
            body,
            key: Some(request_key(&e.id)),
            route: NoticeRoute::AgentRequests,
            click: format!("agent-request\n{}", e.id),
            category: Some(REQUEST_CATEGORY.into()),
            actions: request_actions(),
            urgency: NoticeUrgency::Persistent,
        },
    }
}

// MARK: Transfers

/// The Notifications settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NoticeSettings {
    pub finished: bool,
    pub errors: bool,
    pub goal: bool,
    /// Settings › Agents › Notify me of changes.
    pub agents: bool,
}

/// What an event is decided on.
pub(crate) struct Facts {
    pub settings: NoticeSettings,
    pub trackers: Vec<TrackerSummary>,
    /// Downloading, checking and held ones, hidden ones too (`downloading` + `hidden_downloading`).
    pub downloading: Vec<TransferRow>,
    /// Waiting and paused downloads, hidden ones too.
    pub queued: Vec<TransferRow>,
    /// The tracker of the torrent an id-only event names (an error, an automatic add).
    pub tracker: Option<String>,
    /// For an agent's request for access: the requests the user is asked about, newest first.
    pub requests: Vec<trs_control::access::Entry>,
}

impl Facts {
    fn read(core: &Core, event: &NoticeEvent) -> Facts {
        let s = core.settings_load();
        let settings = NoticeSettings {
            finished: s.general.notify_finished,
            errors: s.general.notify_errors,
            goal: s.general.notify_goal,
            agents: s.agents.notify_changes,
        };
        let id = match event {
            NoticeEvent::Error { id, .. } => id.clone(),
            NoticeEvent::AutoAdded { added } => Some(added.id.clone()),
            _ => None,
        };
        // A duplicate looks for the torrent it repeats among the held ones.
        let needs_library = !matches!(event, NoticeEvent::Agent { .. });
        let (trackers, downloading, queued) = if needs_library {
            let snap = core.snapshot();
            let downloading = snap.downloading.into_iter().chain(snap.hidden_downloading).collect();
            let queued = snap.queued.into_iter().chain(snap.hidden_queued).collect();
            (snap.trackers, downloading, queued)
        } else {
            Default::default()
        };
        let requests = match event {
            NoticeEvent::Agent { activity } if activity.method == "agents.request" => core.backend.agent_requests(),
            _ => Vec::new(),
        };
        Facts { settings, trackers, downloading, queued, tracker: id.and_then(|id| core.tracker_of(id)), requests }
    }
}

/// Which side of a torrent a notice tells about (SPEC 7E.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Side {
    /// A download starting, needing a choice, finishing or failing.
    Download,
    /// A torrent that is seeding: its goal, or an error while sharing.
    Sharing,
}

/// A banner names a torrent, which defeats hiding it: for a tracker in full hidden mode nothing about its torrents is
/// announced; in "hide only what is sharing" mode downloads are announced as usual and anything about seeding is not.
pub(crate) fn silenced(tracker: Option<&str>, side: Side, trackers: &[TrackerSummary]) -> bool {
    let Some(t) = tracker.and_then(|id| trackers.iter().find(|t| t.id == id)) else { return false };
    t.hidden && (side == Side::Sharing || !t.hide_only_seeding)
}

pub(crate) fn decide(state: &mut State, event: NoticeEvent, f: &Facts, now_ms: u64) -> NoticeOutcome {
    use NoticeOutcome::{Nothing, Open};
    let hidden = |tracker: Option<&str>, side| silenced(tracker, side, &f.trackers);
    match event {
        NoticeEvent::Finished { row } => {
            if !f.settings.finished || hidden(Some(&row.tracker_id), Side::Download) {
                return Nothing;
            }
            state.burst.retain(|t| now_ms.saturating_sub(*t) < BURST_MS);
            state.burst.push(now_ms);
            let n = state.burst.len();
            if n <= BURST_MAX {
                notice(NoticeKind::Finished, "Download finished".into(), row.display_name, None, NoticeRoute::Transfer { id: row.id })
            } else {
                let body = format!("Open {APP_NAME} to see them.");
                notice(NoticeKind::FinishedBurst, format!("{n} downloads finished"), body, Some(BURST_KEY.into()), NoticeRoute::Window)
            }
        }
        NoticeEvent::Error { id, message } => {
            let in_downloads = |id: &str| f.downloading.iter().chain(&f.queued).any(|r| r.id == id);
            let side = if id.as_deref().is_some_and(in_downloads) { Side::Download } else { Side::Sharing };
            if !f.settings.errors || hidden(f.tracker.as_deref(), side) {
                return Nothing;
            }
            let name = id.as_deref().and_then(|id| f.downloading.iter().find(|r| r.id == id)).map_or("a transfer", |r| r.display_name.as_str());
            let route = id.map_or(NoticeRoute::Window, |id| NoticeRoute::Transfer { id });
            notice(NoticeKind::Error, format!("Problem with {name}"), message, None, route)
        }
        NoticeEvent::GoalReached { row } => {
            if !f.settings.goal || hidden(Some(&row.tracker_id), Side::Sharing) {
                return Nothing;
            }
            notice(NoticeKind::GoalReached, "Seeding goal reached".into(), row.display_name, None, NoticeRoute::Window)
        }
        NoticeEvent::NeedsChoice { row, chooser_requested, starting_shown } => {
            if starting_shown {
                Nothing
            } else if chooser_requested {
                Open { route: NoticeRoute::Chooser { id: row.id } }
            } else if hidden(Some(&row.tracker_id), Side::Download) {
                Nothing
            } else {
                let key = format!("choice-{}", row.id);
                notice(NoticeKind::NeedsChoice, "Choose what to download".into(), row.display_name, Some(key), NoticeRoute::Chooser { id: row.id })
            }
        }
        NoticeEvent::AutoAdded { added } => {
            // A torrent that waits to start has the starting notifier for its notice.
            let waits = f.downloading.iter().find(|r| r.id == added.id).is_some_and(|r| r.pending.is_some());
            if waits || hidden(f.tracker.as_deref(), Side::Download) {
                return Nothing;
            }
            let mut parts = vec![category_label(added.category), added.tracker];
            if let Some(d) = added.destination {
                parts.push(format!("→ {}", trs_platform::dirs::shown(std::path::Path::new(&d))));
            }
            let title = format!("Added {}", added.name);
            notice(NoticeKind::Added, title, parts.join(" · "), None, NoticeRoute::Transfer { id: added.id })
        }
        NoticeEvent::Duplicate { id, name, trackers_added } => {
            // Added again while the first copy is held (counting down, or waiting for a choice): the notice says so
            // and opens what holds it, so the second add never vanishes.
            let held = f.downloading.iter().chain(&f.queued).find(|r| r.id == id && (r.awaiting_choice || r.pending.is_some()));
            if let Some(r) = held {
                let (body, route) = if r.awaiting_choice {
                    (format!("{name} is waiting for you to choose what to download"), NoticeRoute::Chooser { id: id.clone() })
                } else {
                    (format!("{name} is about to start"), NoticeRoute::Starting { id: id.clone() })
                };
                return notice(NoticeKind::Duplicate, format!("Already in {APP_NAME}"), body, Some(format!("duplicate-{id}")), route);
            }
            let (title, body) = if trackers_added > 0 {
                let s = if trackers_added == 1 { "" } else { "s" };
                (format!("Added {trackers_added} tracker{s} to {name}"), format!("It was already in {APP_NAME}"))
            } else {
                (format!("Already in {APP_NAME}"), name)
            };
            notice(NoticeKind::Duplicate, title, body, None, NoticeRoute::Window)
        }
        NoticeEvent::Agent { activity } => {
            // An agent asks for access: its request, once, whatever the setting (the user must hear of it to answer).
            if activity.method == "agents.request" {
                let Some(e) = f.requests.iter().find(|e| !state.requests_posted.contains(&e.id)) else { return Nothing };
                state.requests_posted.insert(e.id.clone());
                return request_notice(e);
            }
            // An approval by a reply in chat that could not be done: the user is not looking at the app, so say so.
            if activity.method == "agents.approve" && activity.outcome.starts_with("error") && activity.summary.contains(trs_control::access::CHAT_WORDS) {
                return notice(NoticeKind::Agent, "Not approved".into(), activity.summary, None, NoticeRoute::AgentRequests);
            }
            // The user's own answers, and expiries, need no notice.
            if !f.settings.agents || activity.outcome != "ok" || activity.method.starts_with("agents.") {
                return Nothing;
            }
            notice(NoticeKind::Agent, activity.client, activity.summary, None, NoticeRoute::Window)
        }
    }
}

// MARK: Chat

/// One conversation as the poll sees it.
pub(crate) struct Conversation {
    pub place: LibraryPlace,
    pub unread: u32,
    /// "Team › Dinner", or the device of a private conversation.
    pub title: String,
    pub muted: bool,
}

fn conversations(core: &Core) -> Vec<Conversation> {
    let mut v = Vec::new();
    for c in core.chat_channels() {
        let topics = core.chat_topics(c.id.clone());
        v.push(Conversation {
            place: LibraryPlace::Chat { channel_id: c.id.clone(), topic_id: None },
            unread: general_unread(&c, &topics),
            title: chat_place_title(c.clone(), None),
            muted: c.muted,
        });
        for t in topics {
            v.push(Conversation {
                place: LibraryPlace::Chat { channel_id: c.id.clone(), topic_id: Some(t.id.clone()) },
                unread: t.unread,
                title: chat_place_title(c.clone(), Some(t)),
                muted: c.muted,
            });
        }
    }
    for d in core.chat_directs() {
        v.push(Conversation { place: LibraryPlace::Direct { device_id: d.device_id }, unread: d.unread, title: d.name, muted: false });
    }
    v
}

fn last_message(core: &Core, place: &LibraryPlace) -> Option<SyncMessage> {
    match place {
        LibraryPlace::Chat { channel_id, topic_id } => core.chat_messages(channel_id.clone(), topic_id.clone(), None, 1).pop(),
        LibraryPlace::Direct { device_id } => core.chat_direct_messages(device_id.clone(), None, 1).pop(),
        _ => None,
    }
}

/// The conversation is on screen: the app in front with the window on it.
fn showing(view: &NoticeView, place: &LibraryPlace) -> bool {
    view.in_front && view.window_place.as_ref() == Some(place)
}

/// The banners for the conversations whose unread count went up since `seen`, which becomes the counts of now. A muted
/// channel's counts are kept too, so unmuting it announces nothing old.
pub(crate) fn chat_step(
    seen: &mut Option<HashMap<LibraryPlace, u32>>,
    convs: Vec<Conversation>,
    view: &NoticeView,
    last: impl Fn(&LibraryPlace) -> Option<SyncMessage>,
) -> Vec<Notice> {
    let before = seen.take();
    let mut now = HashMap::with_capacity(convs.len());
    let mut out = Vec::new();
    for c in convs {
        now.insert(c.place.clone(), c.unread);
        let Some(before) = &before else { continue };
        if c.muted || c.unread <= before.get(&c.place).copied().unwrap_or(0) || showing(view, &c.place) {
            continue;
        }
        let Some(m) = last(&c.place).filter(|m| !m.is_me) else { continue };
        let text = chat_notification_text(c.title, m);
        let key = match &c.place {
            LibraryPlace::Chat { channel_id, topic_id } => format!("chat-{channel_id}/{}", topic_id.as_deref().unwrap_or_default()),
            LibraryPlace::Direct { device_id } => format!("direct-{device_id}"),
            _ => continue,
        };
        let route = NoticeRoute::Place { place: c.place };
        out.push(Notice {
            kind: NoticeKind::ChatMessage,
            title: text.title,
            body: text.body,
            key: Some(key),
            click: click_of(&route),
            route,
            category: None,
            actions: Vec::new(),
            urgency: NoticeUrgency::Normal,
        });
    }
    *seen = Some(now);
    out
}

// MARK: The settings banner

/// How a banner line looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsBannerTone {
    /// A setting that could not be saved or used (the Mac: red, a warning triangle).
    Problem,
    /// A change that applies after a restart (the Mac: orange, a circular arrow).
    Restart,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SettingsBannerLine {
    pub text: String,
    pub tone: SettingsBannerTone,
}

/// A button of the banner.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SettingsBannerAction {
    /// "Open Settings": the host opens Settings.
    pub title: String,
    pub help: String,
}

/// The banner under Settings (or, with `action`, on a page outside it) while settings have problems.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SettingsBanner {
    pub lines: Vec<SettingsBannerLine>,
    /// Only outside Settings, and only for problems: the way to them.
    pub action: Option<SettingsBannerAction>,
}

/// The banner of the settings' problems, or `None` when there are none. `problems`: what the last save or a rejected
/// edit of a settings file said (`CoreError::Invalid`, `on_settings_rejected`); `notes`: what loading the files found
/// (`Core::settings_problems`, read again after each `on_settings_changed`); `restart_required`: what
/// `on_settings_changed` said applies after a restart; `in_settings`: the banner is drawn in Settings itself.
#[uniffi::export]
pub fn settings_banner(problems: Vec<String>, notes: Vec<String>, restart_required: Vec<String>, in_settings: bool) -> Option<SettingsBanner> {
    let mut lines: Vec<SettingsBannerLine> = Vec::new();
    for text in problems.into_iter().chain(notes) {
        if !lines.iter().any(|l| l.text == text) {
            lines.push(SettingsBannerLine { text, tone: SettingsBannerTone::Problem });
        }
    }
    let problem = !lines.is_empty();
    if !restart_required.is_empty() {
        let text = format!("Restart {APP_NAME} to apply: {}", restart_required.join(", "));
        lines.push(SettingsBannerLine { text, tone: SettingsBannerTone::Restart });
    }
    if lines.is_empty() {
        return None;
    }
    let action = (problem && !in_settings).then(|| SettingsBannerAction {
        title: "Open Settings".into(),
        help: "Open Settings to fix the settings that could not be used".into(),
    });
    Some(SettingsBanner { lines, action })
}

#[cfg(test)]
#[path = "notify_tests.rs"]
mod tests;
