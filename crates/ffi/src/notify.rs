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
    /// A place of the window (a conversation).
    Place { place: LibraryPlace },
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
    Duplicate { name: String, trackers_added: u32 },
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
        "chat" => match next() {
            Some(channel_id) => NoticeRoute::Place { place: LibraryPlace::Chat { channel_id, topic_id: next() } },
            None => NoticeRoute::Window,
        },
        "direct" => next().map_or(NoticeRoute::Window, |device_id| NoticeRoute::Place { place: LibraryPlace::Direct { device_id } }),
        _ => NoticeRoute::Window,
    }
}

/// The route as text ([`notice_route`] reads it back); ids hold no line breaks.
pub(crate) fn click_of(route: &NoticeRoute) -> String {
    match route {
        NoticeRoute::Window => "window".into(),
        NoticeRoute::Transfer { id } => format!("transfer\n{id}"),
        NoticeRoute::Chooser { id } => format!("chooser\n{id}"),
        NoticeRoute::Place { place: LibraryPlace::Chat { channel_id, topic_id } } => {
            format!("chat\n{channel_id}\n{}", topic_id.as_deref().unwrap_or_default())
        }
        NoticeRoute::Place { place: LibraryPlace::Direct { device_id } } => format!("direct\n{device_id}"),
        NoticeRoute::Place { .. } => "window".into(),
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
    NoticeOutcome::Post { notice: Notice { kind, title, body, key, route, click } }
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
        let needs_library = !matches!(event, NoticeEvent::Duplicate { .. } | NoticeEvent::Agent { .. });
        let (trackers, downloading, queued) = if needs_library {
            let snap = core.snapshot();
            let downloading = snap.downloading.into_iter().chain(snap.hidden_downloading).collect();
            let queued = snap.queued.into_iter().chain(snap.hidden_queued).collect();
            (snap.trackers, downloading, queued)
        } else {
            Default::default()
        };
        Facts { settings, trackers, downloading, queued, tracker: id.and_then(|id| core.tracker_of(id)) }
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
        NoticeEvent::Duplicate { name, trackers_added } => {
            let (title, body) = if trackers_added > 0 {
                let s = if trackers_added == 1 { "" } else { "s" };
                (format!("Added {trackers_added} tracker{s} to {name}"), format!("It was already in {APP_NAME}"))
            } else {
                (format!("Already in {APP_NAME}"), name)
            };
            notice(NoticeKind::Duplicate, title, body, None, NoticeRoute::Window)
        }
        NoticeEvent::Agent { activity } => {
            if !f.settings.agents || activity.outcome != "ok" {
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
        out.push(Notice { kind: NoticeKind::ChatMessage, title: text.title, body: text.body, key: Some(key), click: click_of(&route), route });
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
