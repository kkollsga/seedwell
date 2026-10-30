//! The main ("library") window (SPEC 8.3) as one view model every host draws: the sidebar, the title bar's title and
//! summary, and the page shown (a list of torrents, a tracker's Info, Folder Sync, a conversation, Settings). Rust
//! decides (docs/CROSS-PLATFORM.md §1) what each shows, what selecting a place resets, where the window goes when its
//! place disappears, which groups are folded and how many rows are fetched, and how often the window asks again; and
//! (library_window.rs) the selection, the Remove… question, the keys, Get Info's place and the new-download takeover.
//! The host draws and runs the commands.
//!
//! - [`Library`]: one for the app's life; it holds the window's view state (place, search, category, folds, pages,
//!   selection) and reads the core.
//! - [`LocalClock`]: the host writes times and days in the user's own style.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use trs_platform::computer;

use crate::backend::Backend;
use crate::chat_text::{header, named, topic_line, UNNAMED_DEVICE};
use crate::library_rows::{
    filter_chip, list_signature, Lists, ListFilter, ListModel, RowContext, SharingRows, PAGE, SEED_GROUPS,
};
use crate::library_window::{add_sheet_of, AddLinkSheet, PendingRemoval, ScrollTarget};
use crate::removal::RemovalQuestion;
use crate::menu::sync_summary_of;
use crate::sidebar::{channel_menu, sidebar, tracker_menu, SidebarInputs, SidebarMenuEntry, SidebarModel};
use crate::sync::{SyncChannel, SyncDirect, SyncSnapshot, SyncTopic};
use crate::types::{Category, RowQuery, SeedGroup, Snapshot, TransferKind, TransferRow};
use crate::Core;

/// How often a shown window asks for a new model, and how late the timer may fire.
const REFRESH_MS: u32 = 1_000;
const REFRESH_LEEWAY_MS: u32 = 200;
const DAY_S: i64 = 86_400;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// MARK: Local time

/// The time of day and the day in the user's own style, which only the host knows (the Mac: the system's formats).
/// Rust decides which form a text uses; the host writes it. Called on the thread that asks for the model.
#[uniffi::export(with_foreign)]
pub trait LocalClock: Send + Sync {
    /// "22:29".
    fn time_of_day(&self, unix_s: u64) -> String;
    /// "Mon 22:29".
    fn weekday_time(&self, unix_s: u64) -> String;
    /// "1 Nov", or "1 Nov 2027" `with_year`.
    fn day(&self, unix_s: u64, with_year: bool) -> String;
    /// "Nov 1, 2026 at 22:29": a moment in full.
    fn date_time(&self, unix_s: u64) -> String;
    /// Seconds east of UTC at that moment in the user's time zone (which calendar day and year it is there).
    fn utc_offset_s(&self, unix_s: u64) -> i32;
}

/// The local calendar day of a moment, as days since 1970-01-01 there.
fn local_day_number(clock: &dyn LocalClock, unix_s: u64) -> i64 {
    (unix_s as i64 + i64::from(clock.utc_offset_s(unix_s))).div_euclid(DAY_S)
}

/// The local calendar year of a moment.
fn local_year(clock: &dyn LocalClock, unix_s: u64) -> i64 {
    let days = local_day_number(clock, unix_s);
    // Days since 1970-01-01 to the civil year (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    yoe + era * 400 + i64::from(mp >= 10)
}

/// "1 Nov", with the year only when it is not this year: the day the sync and chat lines name.
pub(crate) fn local_day(clock: &dyn LocalClock, unix_s: u64, now_s: u64) -> String {
    clock.day(unix_s, local_year(clock, unix_s) != local_year(clock, now_s))
}

/// "22:29" on the same day where the user is, else "Mon 22:29": when something began or will happen.
pub(crate) fn clock_at(clock: &dyn LocalClock, unix_s: u64, now_s: u64) -> String {
    if local_day_number(clock, unix_s) == local_day_number(clock, now_s) { clock.time_of_day(unix_s) } else { clock.weekday_time(unix_s) }
}

// MARK: The records

/// A place of the window: what the sidebar selects.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum LibraryPlace {
    Downloads,
    Sharing,
    FolderSync,
    Settings,
    /// A tracker's tab: every torrent of it, downloads and seeds.
    Tracker { tracker_id: String },
    /// A channel's General (`topic_id: None`) or one of its topics.
    Chat { channel_id: String, topic_id: Option<String> },
    /// A private conversation with a device.
    Direct { device_id: String },
}

/// What the window's page shows for the place.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum LibraryPage {
    /// Torrents in groups ([`LibraryViewModel::list`]).
    List,
    /// A tracker's Info page (the sidebar item stays selected).
    TrackerInfo,
    FolderSync,
    /// A channel's or a private conversation's messages.
    Conversation,
    Settings,
    /// The channel or topic of the place is gone: a quiet note ([`LibraryViewModel::missing`]).
    Missing,
}

/// The torrents of a tracker moving now, bytes/s (its tab's header).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct TrackerRates {
    pub down: u64,
    pub up: u64,
}

/// The whole window.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct LibraryViewModel {
    /// The place shown: what the sidebar selects (it may differ from the one asked for when that one disappeared).
    pub place: LibraryPlace,
    pub page: LibraryPage,
    pub sidebar: SidebarModel,
    /// The title bar: the place's name ("Downloads", a tracker's name, "Team › Dinner").
    pub title: String,
    /// A star before the title: a favorite tracker's tab.
    pub title_star: bool,
    /// The calm line under the title ("2 active · 1 up next · ↓ 2.1 MB/s · ↑ 340 KB/s").
    pub summary: String,
    /// The list of a list page.
    pub list: Option<ListModel>,
    /// The note of a missing page ("This topic is no longer in the channel").
    pub missing: Option<String>,
    /// On a tracker's tab (its list or its Info): all its torrents' rates now.
    pub tracker_rates: Option<TrackerRates>,
    /// The selected rows on screen, in list order (each row also says it is [`crate::ListRow::selected`]).
    pub selection: Vec<String>,
    /// The Remove… question waiting for its answer.
    pub removal: Option<PendingRemoval>,
    /// "Add a Download", while its sheet is open.
    pub add_sheet: Option<AddLinkSheet>,
    /// A row to bring into view (a new download); [`Library::scroll_done`] when it is.
    pub scroll_to: Option<ScrollTarget>,
    /// Ask again after this long; `None` while the window cannot be seen.
    pub refresh_ms: Option<u32>,
    /// How late that timer may fire (lets the system coalesce wake-ups).
    pub refresh_leeway_ms: u32,
    /// The place, the page, and the sidebar's and the list's structure: see [`ListModel::signature`].
    pub signature: String,
}

// MARK: State and inputs

/// What the user set in the window.
pub(crate) struct LibraryState {
    pub place: LibraryPlace,
    /// As typed; the lists compare it lower-cased.
    pub search: String,
    pub category: Option<Category>,
    /// The fold keys of the folded groups ("sharing.dormant", "tracker.downloads").
    pub folded: BTreeSet<String>,
    /// How many rows of a Sharing group are fetched ([`PAGE`] when not set).
    pub shown: HashMap<SeedGroup, u32>,
    pub visible: bool,
    /// The channels whose topics the sidebar shows.
    pub open_channels: HashSet<String>,
    /// A new download arrived while the user was busy: Downloads carries a dot until they go there.
    pub downloads_flag: bool,
    /// The tracker's Info page instead of its torrents.
    pub tracker_info: bool,
    /// Names of devices a private conversation was just started with (no message yet, so not listed by the core).
    pub direct_names: HashMap<String, String>,
    /// The selected rows' ids, and the row a ⇧-click range starts from (the last one clicked plainly or with ⌘).
    pub selection: BTreeSet<String>,
    pub anchor: Option<String>,
    /// The row whose Get Info is open.
    pub info: Option<String>,
    /// The Remove… question waiting for its answer.
    pub removal: Option<RemovalQuestion>,
    /// The "Add a Download" sheet is open, and what was typed in it.
    pub add_sheet: bool,
    pub add_text: String,
    /// A row to bring into view, and the number of the last such request.
    pub scroll: Option<ScrollTarget>,
    pub scroll_count: u32,
    /// The rows of the last model, in list order, as (group key, id).
    pub listed: Vec<(String, String)>,
    /// The window exists (it may be minimised or covered: see `visible`).
    pub open: bool,
    /// A new download that arrived while the window was closed (the newest).
    pub arrival: Option<String>,
}

impl LibraryState {
    pub(crate) fn new(folded: Vec<String>) -> Self {
        Self {
            place: LibraryPlace::Downloads,
            search: String::new(),
            category: None,
            folded: folded.into_iter().collect(),
            shown: HashMap::new(),
            visible: false,
            open_channels: HashSet::new(),
            downloads_flag: false,
            tracker_info: false,
            direct_names: HashMap::new(),
            selection: BTreeSet::new(),
            anchor: None,
            info: None,
            removal: None,
            add_sheet: false,
            add_text: String::new(),
            scroll: None,
            scroll_count: 0,
            listed: Vec::new(),
            open: false,
            arrival: None,
        }
    }

    /// The window closed: it opens again as new, with the folds it had and the download that arrived meanwhile.
    pub(crate) fn reset_window(&mut self) {
        let fresh = Self::new(Vec::new());
        let folded = std::mem::take(&mut self.folded);
        let (arrival, scroll_count) = (self.arrival.take(), self.scroll_count);
        *self = Self { folded, arrival, scroll_count, ..fresh };
    }

    /// Selecting a place: a tab opens on its first page of rows, unfiltered, on its torrents (not its Info), with nothing
    /// selected and Get Info closed. Another place also drops the Remove… question (it was about rows of this one).
    pub(crate) fn select(&mut self, place: LibraryPlace) {
        if place == LibraryPlace::Downloads {
            self.downloads_flag = false;
        }
        if place != self.place {
            self.removal = None;
        }
        self.selection.clear();
        self.anchor = None;
        self.info = None;
        self.place = place;
        self.tracker_info = false;
        self.shown.clear();
        self.category = None;
    }

    /// The first part of the place's fold keys: every tracker tab shares "tracker".
    fn page_key(&self) -> &'static str {
        match self.place {
            LibraryPlace::Downloads => "downloads",
            LibraryPlace::Sharing => "sharing",
            LibraryPlace::Tracker { .. } => "tracker",
            LibraryPlace::FolderSync => "folderSync",
            LibraryPlace::Settings => "settings",
            LibraryPlace::Chat { .. } => "chat",
            LibraryPlace::Direct { .. } => "direct",
        }
    }

    fn tracker_id(&self) -> Option<&str> {
        match &self.place {
            LibraryPlace::Tracker { tracker_id } => Some(tracker_id),
            _ => None,
        }
    }

    fn is_folded(&self, page: &str, group: &str) -> bool {
        self.folded.contains(&format!("{page}.{group}"))
    }
}

/// What one model is built from: the reads of one refresh.
#[derive(Clone)]
pub(crate) struct Inputs {
    pub snapshot: Snapshot,
    pub sync: SyncSnapshot,
    pub channels: Vec<SyncChannel>,
    /// Each channel's topics, newest activity first, in the order of `channels`.
    pub topics: Vec<Vec<SyncTopic>>,
    pub directs: Vec<SyncDirect>,
    /// The Sharing groups of the place (Sharing, a tracker's list), its folded groups not fetched.
    pub sharing: Option<SharingRows>,
    /// On a tracker's tab: every one of its seeds, whatever the list shows (its rates count them all).
    pub tracker_seeds: Vec<TransferRow>,
}

impl Inputs {
    /// A transfer the reads hold, by id.
    pub(crate) fn find(&self, id: &str) -> Option<&TransferRow> {
        let s = &self.snapshot;
        let sharing = self.sharing.iter().flat_map(|x| &x.rows).flat_map(|(_, v)| v);
        s.downloading.iter().chain(&s.queued).chain(&s.recent).chain(&s.hidden_downloading).chain(&s.hidden_queued).chain(&s.hidden_recent)
            .chain(sharing).chain(&self.tracker_seeds).find(|r| r.id == id)
    }

    fn channel(&self, id: &str) -> Option<(&SyncChannel, &[SyncTopic])> {
        let i = self.channels.iter().position(|c| c.id == id)?;
        Some((&self.channels[i], self.topics.get(i).map(Vec::as_slice).unwrap_or(&[])))
    }
}

// MARK: Building

/// "↓ 2.1 MB/s · ↑ 340 KB/s": both rates in the summary of Downloads and Sharing.
fn rates(s: &Snapshot) -> String {
    format!("↓ {} · ↑ {}", trs_control::display::rate(s.down_rate), trs_control::display::rate(s.up_rate))
}

/// "2 active · 1 up next · 1 to choose · 1 starting · 3 finished today · ↓ … · ↑ …". "Finished today" counts the
/// Recently Finished rows that finished on today's calendar day where the user is (the list keeps older ones until
/// they expire or are cleared).
fn downloads_summary(s: &Snapshot, now_s: u64, clock: &dyn LocalClock) -> String {
    let held = s.downloading.iter().filter(|r| r.awaiting_choice).count() as i64;
    let starting = s.downloading.iter().filter(|r| r.pending.is_some()).count() as i64;
    let mut parts = vec![format!("{} active", s.downloading.len() as i64 - held - starting), format!("{} up next", s.queued.len())];
    if held > 0 {
        parts.push(format!("{held} to choose"));
    }
    if starting > 0 {
        parts.push(format!("{starting} starting"));
    }
    let today = local_day_number(clock, now_s);
    let finished = s.recent.iter().filter(|r| r.finished_at.is_some_and(|at| local_day_number(clock, at) == today)).count();
    if finished > 0 {
        parts.push(format!("{finished} finished today"));
    }
    parts.push(rates(s));
    parts.join(" · ")
}

/// The private conversations of the sidebar: every one with messages, plus the one just started (no message yet).
fn direct_rows(i: &Inputs, st: &LibraryState) -> Vec<SyncDirect> {
    let mut v = i.directs.clone();
    if let LibraryPlace::Direct { device_id } = &st.place
        && !v.iter().any(|d| &d.device_id == device_id)
    {
        let name = st.direct_names.get(device_id).cloned().unwrap_or_else(|| UNNAMED_DEVICE.into());
        v.insert(0, SyncDirect { device_id: device_id.clone(), name, user: String::new(), online: false, unread: 0, last_message_s: None });
    }
    v
}

/// The name of the device of a private conversation.
fn direct_name(i: &Inputs, st: &LibraryState, device_id: &str) -> String {
    match i.directs.iter().find(|d| d.device_id == device_id) {
        Some(d) => named(&d.name, &d.user),
        None => st.direct_names.get(device_id).cloned().unwrap_or_else(|| UNNAMED_DEVICE.into()),
    }
}

/// The torrents of a tracker moving now: all its downloads and all its seeds (not only the rows its tab shows: a
/// search, a category, a fold or the paging leave the rates alone), each once.
fn tracker_rates(i: &Inputs, tracker: &str) -> TrackerRates {
    let s = &i.snapshot;
    let all = s.downloading.iter().chain(&s.hidden_downloading).chain(&s.queued).chain(&s.hidden_queued).chain(&i.tracker_seeds);
    let mut seen = HashSet::new();
    let mut rates = TrackerRates { down: 0, up: 0 };
    for r in all.filter(|r| r.tracker_id == tracker && r.kind == TransferKind::Torrent) {
        if seen.insert(r.id.as_str()) {
            rates.down += r.down_rate;
            rates.up += r.up_rate;
        }
    }
    rates
}

/// The window for `i`, as `st` asks for it, at `now_ms`.
pub(crate) fn build(i: &Inputs, st: &LibraryState, now_ms: u64, clock: &dyn LocalClock) -> LibraryViewModel {
    let s = &i.snapshot;
    let tracker = st.tracker_id().and_then(|id| s.trackers.iter().find(|t| t.id == id));
    let chat = match &st.place {
        LibraryPlace::Chat { channel_id, topic_id } => i.channel(channel_id).map(|(c, topics)| (c, topic_id.as_ref().and_then(|t| topics.iter().find(|x| &x.id == t)))),
        _ => None,
    };
    let direct = match &st.place {
        LibraryPlace::Direct { device_id } => i.directs.iter().find(|d| &d.device_id == device_id),
        _ => None,
    };
    let (title, summary) = match &st.place {
        LibraryPlace::Downloads => ("Downloads".to_owned(), downloads_summary(s, now_ms / 1000, clock)),
        LibraryPlace::Sharing => ("Sharing".to_owned(), format!("{} · {}", trs_control::display::count(u64::from(s.sharing_total), "torrent"), rates(s))),
        LibraryPlace::FolderSync => {
            let me = if i.sync.device_name.is_empty() { String::new() } else { format!(concat!(" · This ", computer!(), ": {}"), i.sync.device_name) };
            ("Folder Sync".to_owned(), sync_summary_of(&i.sync) + &me)
        }
        LibraryPlace::Settings => ("Settings".to_owned(), "Settings".to_owned()),
        LibraryPlace::Tracker { tracker_id } => match tracker {
            None => ("Tracker".to_owned(), tracker_id.clone()),
            Some(t) => {
                let line = if st.tracker_info { "Info" } else if t.favorite { "Favorite tracker" } else { "Tracker" };
                (t.name.clone(), line.to_owned())
            }
        },
        LibraryPlace::Chat { .. } => {
            let name = chat.map_or("Channel", |(c, _)| c.name.as_str());
            let title = match chat.and_then(|(_, t)| t) {
                Some(t) => format!("{name} › {}", t.title),
                None => name.to_owned(),
            };
            let summary = match chat {
                Some((_, Some(t))) => topic_line(t),
                Some((c, None)) => header(c, c.share_until_s.map(|u| local_day(clock, u, now_ms / 1000))),
                None => concat!("No longer on this ", computer!()).to_owned(),
            };
            (title, summary)
        }
        LibraryPlace::Direct { device_id } => (direct_name(i, st, device_id), crate::chat_text::chat_direct_line(direct.is_some_and(|d| d.online))),
    };
    let page = match &st.place {
        LibraryPlace::Downloads | LibraryPlace::Sharing => LibraryPage::List,
        LibraryPlace::Tracker { .. } if st.tracker_info && tracker.is_some() => LibraryPage::TrackerInfo,
        LibraryPlace::Tracker { .. } => LibraryPage::List,
        LibraryPlace::FolderSync => LibraryPage::FolderSync,
        LibraryPlace::Settings => LibraryPage::Settings,
        LibraryPlace::Direct { .. } => LibraryPage::Conversation,
        LibraryPlace::Chat { topic_id, .. } => match chat {
            Some((_, t)) if topic_id.is_none() || t.is_some() => LibraryPage::Conversation,
            _ => LibraryPage::Missing,
        },
    };
    let missing = (page == LibraryPage::Missing).then(|| {
        if chat.is_none() { concat!("This channel is no longer on this ", computer!()) } else { "This topic is no longer in the channel" }.to_owned()
    });
    let list = (page == LibraryPage::List).then(|| list(i, st, now_ms, tracker.map(|t| t.name.as_str())));
    let tracker_rates = match (&st.place, page) {
        (LibraryPlace::Tracker { tracker_id }, LibraryPage::List | LibraryPage::TrackerInfo) => Some(tracker_rates(i, tracker_id)),
        _ => None,
    };
    let listed: Vec<(String, String)> = list.iter().flat_map(|l| &l.groups).flat_map(|g| g.rows.iter().map(|r| (g.key.clone(), r.id.clone()))).collect();
    let removal = st.pending_removal(&listed);
    let selection = {
        let mut seen = HashSet::new();
        listed.iter().filter(|(_, id)| st.selection.contains(id) && seen.insert(id.as_str())).map(|(_, id)| id.clone()).collect()
    };
    let topics = |id: &str| i.channel(id).map(|(_, t)| t).unwrap_or(&[]);
    let open = |id: &str| st.open_channels.contains(id);
    let directs = direct_rows(i, st);
    let sidebar = sidebar(&SidebarInputs {
        snapshot: s,
        folders: i.sync.folders.len(),
        channels: &i.channels,
        topics: &topics,
        open: &open,
        directs: &directs,
        place: &st.place,
        downloads_flag: st.downloads_flag,
    });
    let under = removal.as_ref().map(|r| (r.question.plural, r.group_key.clone(), r.row_id.clone()));
    let signature = format!("{:?}|{page:?}|{}|{}|{under:?}", st.place, sidebar.signature, list.as_ref().map_or("", |l| l.signature.as_str()));
    LibraryViewModel {
        place: st.place.clone(),
        page,
        title_star: tracker.is_some_and(|t| t.favorite),
        sidebar,
        title,
        summary,
        list,
        missing,
        tracker_rates,
        selection,
        removal,
        add_sheet: add_sheet_of(st),
        scroll_to: st.scroll.clone(),
        refresh_ms: st.visible.then_some(REFRESH_MS),
        refresh_leeway_ms: REFRESH_LEEWAY_MS,
        signature,
    }
}

/// The builder of the lists of `page` ("downloads", "sharing", "tracker"), filtered as `st` says.
fn lists<'a>(st: &'a LibraryState, now_ms: u64, page: &'static str, tracker: Option<&'a str>) -> Lists<'a> {
    Lists {
        ctx: RowContext { now_ms, show_tracker: tracker.is_none(), filter: st.category },
        filter: ListFilter { tracker, category: st.category, text: st.search.to_lowercase() },
        folded: &st.folded,
        page,
    }
}

/// The list of a list page.
fn list(i: &Inputs, st: &LibraryState, now_ms: u64, tracker_name: Option<&str>) -> ListModel {
    let none = SharingRows::default();
    let sharing = i.sharing.as_ref().unwrap_or(&none);
    let filtered = !st.search.is_empty() || st.category.is_some();
    let (groups, empty) = match &st.place {
        LibraryPlace::Tracker { tracker_id } => {
            let groups = lists(st, now_ms, "tracker", Some(tracker_id.as_str())).tracker(&i.snapshot, sharing);
            let name = tracker_name.unwrap_or("this tracker");
            (groups, if filtered { format!("No matches on {name}") } else { format!("No torrents on {name}") })
        }
        LibraryPlace::Sharing => (lists(st, now_ms, "sharing", None).sharing(sharing), if filtered { "No matches" } else { "Nothing is being shared" }.to_owned()),
        _ => (lists(st, now_ms, "downloads", None).downloads(&i.snapshot), if filtered { "No matches" } else { "No downloads" }.to_owned()),
    };
    let mut groups = groups;
    for r in groups.iter_mut().flat_map(|g| &mut g.rows) {
        r.mark(st.selection.contains(&r.id), st.info.as_deref() == Some(r.id.as_str()));
    }
    let empty = groups.is_empty().then_some(empty);
    let filter_chip = st.category.map(filter_chip);
    let signature = list_signature(filter_chip.as_ref(), &groups, empty.as_deref());
    ListModel { filter_chip, groups, empty, signature }
}

// MARK: The object

/// One main window's view state and its reads of the core.
#[derive(uniffi::Object)]
pub struct Library {
    backend: Arc<dyn Backend>,
    clock: Arc<dyn LocalClock>,
    state: Mutex<LibraryState>,
    /// The reads of the last model, for [`Library::snapshot`] and the context menus.
    last: Mutex<Option<Arc<Inputs>>>,
}

impl Library {
    /// Reads what the model of `st` needs. A place that disappeared falls back first: a deleted tracker's tab to
    /// Sharing, a channel left (here or by its removal elsewhere) to Downloads. A topic shown has its channel open.
    fn gather(&self, st: &mut LibraryState) -> Inputs {
        let b = &self.backend;
        let snapshot = b.snapshot();
        let sync = b.sync();
        let channels = sync.chat_channels();
        if let LibraryPlace::Chat { channel_id, topic_id } = &st.place {
            if !channels.iter().any(|c| &c.id == channel_id) {
                st.select(LibraryPlace::Downloads);
            } else if topic_id.is_some() {
                let id = channel_id.clone();
                st.open_channels.insert(id);
            }
        }
        if let Some(id) = st.tracker_id()
            && !snapshot.trackers.iter().any(|t| t.id == id)
        {
            st.select(LibraryPlace::Sharing);
        }
        let topics = channels.iter().map(|c| sync.chat_topics(&c.id)).collect();
        let sharing = self.sharing_rows(st);
        let tracker_seeds = match st.tracker_id() {
            Some(id) => {
                let q = RowQuery { group: None, tracker_id: Some(id.to_owned()), text: String::new(), category: None, offset: 0, limit: u32::MAX };
                self.backend.rows(&q)
            }
            None => Vec::new(),
        };
        Inputs { snapshot, sync: sync.snapshot(), channels, topics, directs: sync.chat_directs(), sharing, tracker_seeds }
    }

    /// The Sharing groups of a list of seeds: counts, and a page of rows of each group on screen.
    fn sharing_rows(&self, st: &LibraryState) -> Option<SharingRows> {
        let page = match &st.place {
            LibraryPlace::Sharing => "sharing",
            LibraryPlace::Tracker { .. } if !st.tracker_info => "tracker",
            _ => return None,
        };
        let tracker = st.tracker_id();
        let block_folded = page == "tracker" && st.is_folded(page, "sharing");
        let counts = self.backend.sharing_counts(tracker, st.category, &st.search);
        let mut out = SharingRows { counts, rows: Vec::new() };
        for g in SEED_GROUPS {
            let key = format!("{g:?}").to_lowercase();
            if out.count(g) == 0 || block_folded || st.is_folded(page, &key) {
                continue;
            }
            let q = RowQuery {
                group: Some(g),
                tracker_id: tracker.map(str::to_owned),
                text: st.search.clone(),
                category: st.category,
                offset: 0,
                limit: st.shown.get(&g).copied().unwrap_or(PAGE),
            };
            out.rows.push((g, self.backend.rows(&q)));
        }
        Some(out)
    }

    pub(crate) fn with_state<T>(&self, f: impl FnOnce(&mut LibraryState) -> T) -> T {
        f(&mut lock(&self.state))
    }

    pub(crate) fn lock_state(&self) -> MutexGuard<'_, LibraryState> {
        lock(&self.state)
    }

    pub(crate) fn backend(&self) -> &Arc<dyn Backend> {
        &self.backend
    }

    /// The reads of the last model.
    pub(crate) fn last_inputs(&self) -> Option<Arc<Inputs>> {
        lock(&self.last).clone()
    }

    /// The window closed: its reads go with it.
    pub(crate) fn forget_inputs(&self) {
        *lock(&self.last) = None;
    }
}

#[uniffi::export]
impl Library {
    /// The window of `core`, on Downloads, closed until [`Library::set_open`]. `folded`: the fold keys the host
    /// remembered ([`Library::toggle_fold`]).
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>, folded: Vec<String>, clock: Arc<dyn LocalClock>) -> Arc<Self> {
        Arc::new(Self { backend: core.backend.clone(), clock, state: Mutex::new(LibraryState::new(folded)), last: Mutex::new(None) })
    }

    /// The window now: reads the core (cheap: the snapshot, sync's state, the channels and a page of each Sharing group
    /// on screen) and builds it. The host asks again after `refresh_ms`, and after every change it makes.
    pub fn model(&self, now_unix_ms: u64) -> LibraryViewModel {
        let mut st = lock(&self.state);
        let inputs = self.gather(&mut st);
        let m = build(&inputs, &st, now_unix_ms, self.clock.as_ref());
        st.settle(&m);
        *lock(&self.last) = Some(Arc::new(inputs));
        m
    }

    /// The snapshot the last model was built from (the host's other watchers share it).
    pub fn snapshot(&self) -> Snapshot {
        match lock(&self.last).as_ref() {
            Some(i) => i.snapshot.clone(),
            None => self.backend.snapshot(),
        }
    }

    /// Shows a place: on its torrents (not a tracker's Info), unfiltered, its Sharing groups on their first page; the
    /// Downloads flag goes when Downloads is shown.
    pub fn select_place(&self, place: LibraryPlace) {
        self.with_state(|st| st.select(place));
    }

    /// The search text of the lists (it stays when the place changes).
    pub fn set_search(&self, text: String) {
        self.with_state(|st| st.search = text);
    }

    /// A click on a row's category pill: the list shows only that category, or all again when it already did.
    pub fn toggle_category(&self, category: Category) {
        self.with_state(|st| st.category = if st.category == Some(category) { None } else { Some(category) });
    }

    /// The filter chip's ×: every category again.
    pub fn clear_category(&self) {
        self.with_state(|st| st.category = None);
    }

    /// Folds or unfolds the group `key` ([`crate::library_rows::ListGroup::key`]) of the place shown. Returns every
    /// fold key now folded, sorted, for the host to remember.
    pub fn toggle_fold(&self, key: String) -> Vec<String> {
        self.with_state(|st| {
            let k = format!("{}.{key}", st.page_key());
            if !st.folded.remove(&k) {
                st.folded.insert(k);
            }
            st.folded.iter().cloned().collect()
        })
    }

    /// "Show more" under a Sharing group: another page of its rows.
    pub fn show_more(&self, group: SeedGroup) {
        self.with_state(|st| *st.shown.entry(group).or_insert(PAGE) += PAGE);
    }

    /// The window can be seen (open, and not minimised or covered): only then does the model ask to be refreshed.
    pub fn set_visible(&self, visible: bool) {
        self.with_state(|st| st.visible = visible);
    }

    /// Opens or closes a channel in the sidebar (its topics under it).
    pub fn set_channel_open(&self, channel_id: String, open: bool) {
        self.with_state(|st| {
            if open { st.open_channels.insert(channel_id) } else { st.open_channels.remove(&channel_id) };
        });
    }

    /// A tracker's tab shows its Info page instead of its torrents (until another place is selected).
    pub fn set_tracker_info(&self, on: bool) {
        self.with_state(|st| st.tracker_info = on);
    }

    /// The name of a device a private conversation was just started with, until the core lists the conversation.
    pub fn note_direct_name(&self, device_id: String, name: String) {
        self.with_state(|st| st.direct_names.insert(device_id, name));
    }

    /// The context menu of a sidebar item (empty: none), from what the last model showed.
    pub fn sidebar_menu(&self, place: LibraryPlace) -> Vec<SidebarMenuEntry> {
        let Some(i) = lock(&self.last).clone() else { return Vec::new() };
        match &place {
            LibraryPlace::Tracker { tracker_id } => i.snapshot.trackers.iter().find(|t| &t.id == tracker_id).map(tracker_menu).unwrap_or_default(),
            LibraryPlace::Chat { channel_id, topic_id: None } => i.channel(channel_id).map(|(c, _)| channel_menu(c)).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
#[path = "library_tests.rs"]
pub(crate) mod tests;
