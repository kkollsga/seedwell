//! The starting notifier (SPEC 7A.7, 8.6) as a model every host draws (docs/CROSS-PLATFORM.md §1): one block per
//! torrent that waits to start, each with its own countdown, or for the user's choice of files (with the shared
//! [`Chooser`](crate::chooser::Chooser) checklist), or a magnet link fetching its file list. Rust decides which
//! torrents are shown and when the notifier shows, stays or closes, every word, the countdown's line and ring, which
//! block owns Return and Escape, and what each button does; the host draws the [`StartingPage`], ticks it every
//! [`StartingPage::tick_ms`] while it is up, and calls the [`Starting`] method a click or key stands for.
//!
//! One countdown per torrent rather than a shared clock: the engine owns each deadline (torrents added together
//! share one to the second, so their rows tick in step), and one added three seconds later is not hurried.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use trs_control::display;

use crate::chooser::{category_choices, held, home, shown_in, CategoryChoice, Choices, HeldActions};
use crate::held::CancelNote;
use crate::menu::APP_NAME;
use crate::row_text::{category_label, pending_remaining_ms, starting_line};
use crate::types::{Category, CoreError, TransferRow};

/// How often the host asks again while the notifier is up (the countdowns move by the second).
const TICK_MS: u32 = 1_000;

/// A countdown as the host animates it between two looks: [`starting_tick`] gives its line and ring at any moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct StartingCountdown {
    /// When it starts, Unix ms (a running countdown).
    pub starts_at_ms: u64,
    /// The wait as first seen: the ring's full circle.
    pub total_ms: u64,
    /// Stopped: how long was left; it stands still.
    pub stopped_ms: Option<u64>,
}

/// A countdown at one moment.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct StartingTick {
    /// "Starting in 7 s", "Starting…", "Paused".
    pub line: String,
    /// How much of the ring is still full, 0...1.
    pub fraction: f64,
    /// Stopped: the ring holds still and dims, a pause glyph in it.
    pub paused: bool,
}

fn remaining(c: &StartingCountdown, now_ms: u64) -> u64 {
    c.stopped_ms.unwrap_or_else(|| c.starts_at_ms.saturating_sub(now_ms))
}

/// The countdown `c` at `now_ms` (Unix ms): its line and how full its ring is.
#[uniffi::export]
pub fn starting_tick(countdown: StartingCountdown, now_ms: u64) -> StartingTick {
    let left = remaining(&countdown, now_ms);
    let fraction = if countdown.total_ms == 0 { 0.0 } else { (left as f64 / countdown.total_ms as f64).clamp(0.0, 1.0) };
    StartingTick { line: starting_line(left, countdown.stopped_ms.is_some()), fraction, paused: countdown.stopped_ms.is_some() }
}

/// What a block is waiting for.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum StartingBlockState {
    /// Its countdown: the ring and the line ([`starting_tick`]), then Cancel and Start (start now).
    Countdown { countdown: StartingCountdown },
    /// The user's choice of files: compact, Cancel and Start (everything but the Extras); open (`expanded`), the
    /// chooser's checklist (`Choices::chooser`) and its Cancel and Start (the ticks).
    Choice { expanded: bool },
    /// A magnet link fetching its file list: a spinner, `line` and Cancel.
    Fetching { line: String },
}

/// The fold chevron in front of a block's title.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct StartingChevron {
    pub open: bool,
    pub help: String,
    /// What a screen reader calls it: "Collapse" or "Expand".
    pub label: String,
}

/// The category pill in a block's corner: its menu re-plans the torrent (`Starting::change_category`).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct StartingCategory {
    pub category: Category,
    /// "Movie".
    pub title: String,
    /// "Category: Movie — click to change".
    pub help: String,
    pub choices: Vec<CategoryChoice>,
}

/// One torrent of the notifier.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct StartingBlock {
    pub id: String,
    pub title: String,
    /// "Movie · 1.0 GB · ~/Media/Movies": what it is, its size once known and where it goes.
    pub detail: String,
    /// The title and its line open the torrent in the main window (`Starting::open`); this is their tooltip.
    pub open_help: String,
    /// `None` until the files are known (a magnet link).
    pub category: Option<StartingCategory>,
    /// Countdown: Choose Files (`Starting::choose_files`). Choice: opens or closes the checklist
    /// (`Starting::toggle_expanded`). `None` while fetching.
    pub chevron: Option<StartingChevron>,
    pub state: StartingBlockState,
    /// The countdown's line now, "Choose what to download" or "Fetching file list…".
    pub line: String,
    /// Cancel (`Starting::cancel`) and Start (`start_now` for a countdown, `confirm` for a choice), with their keys
    /// when this block owns them.
    pub actions: HeldActions,
    /// This block owns Return and Escape: the open checklist, else the only block there is.
    pub active: bool,
}

/// What the host does with the notifier after a look.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum StartingShow {
    /// Shows it (or keeps it up) and ticks. `stays`: someone has to choose, so a click elsewhere must not close it.
    Show { stays: bool },
    /// Torrents still wait but the user put the notifier away: it stays closed and stops ticking.
    Quiet,
    /// Nothing waits: it closes.
    Close,
}

/// The notifier as the host draws it.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct StartingPage {
    pub blocks: Vec<StartingBlock>,
    pub show: StartingShow,
    /// Several blocks share the notifier: each is drawn more compactly.
    pub compact: bool,
    /// The small × in the corner (while someone has to choose): puts it away and leaves the torrents waiting
    /// (`Starting::dismiss_all`); its tooltip. `None`: no ×.
    pub close_help: Option<String>,
    /// How often to look again while it is up.
    pub tick_ms: u32,
}

fn chevron(open: bool, help: &str) -> StartingChevron {
    StartingChevron { open, help: help.into(), label: if open { "Collapse" } else { "Expand" }.into() }
}

/// What a click on "Choose Files" of a countdown got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum StartingChoose {
    /// The block opens its checklist here.
    Open,
    /// A magnet link: its file list is fetched first; the block opens when it arrives.
    AfterMetadata,
    Failed,
}

/// What a Cancel in the notifier did.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct StartingCancel {
    /// The torrent was in the notifier and is removed now (the host closes what showed it).
    pub cancelled: bool,
    /// Its `.torrent` file could not go to the Trash: a quiet notification says so.
    pub note: Option<CancelNote>,
}

/// What [`Starting`] asks of the engine besides the choosers.
pub(crate) trait PendingSource: Send + Sync {
    /// "Choose Files…": ends the countdown, keeps it held. `Ok(false)`: a magnet link, fetched first.
    fn choose(&self, id: &str) -> Result<bool, CoreError>;
    fn pause(&self, id: &str, pause: bool);
    fn start_now(&self, id: &str);
}

impl PendingSource for crate::Core {
    fn choose(&self, id: &str) -> Result<bool, CoreError> {
        self.pending_choose(id.into())
    }
    fn pause(&self, id: &str, pause: bool) {
        let _ = if pause { self.pending_pause(id.into()) } else { self.pending_resume(id.into()) };
    }
    fn start_now(&self, id: &str) {
        let _ = self.pending_start_now(id.into());
    }
}

#[derive(Clone, Debug)]
struct Item {
    id: String,
    title: String,
    kind: String,
    folder: Option<String>,
    size: u64,
    category: Category,
    total_ms: u64,
    starts_at_ms: u64,
    stopped_ms: Option<u64>,
    awaiting: bool,
    fetching: bool,
    has_source: bool,
}

impl Item {
    fn paused(&self) -> bool {
        self.stopped_ms.is_some()
    }
    /// Waiting for the user rather than for the clock.
    fn choosing(&self) -> bool {
        self.awaiting || self.fetching
    }
    fn countdown(&self) -> StartingCountdown {
        StartingCountdown { starts_at_ms: self.starts_at_ms, total_ms: self.total_ms, stopped_ms: self.stopped_ms }
    }
    fn detail(&self) -> String {
        let size = (self.size > 0).then(|| display::bytes(self.size));
        [Some(self.kind.clone()), size, self.folder.clone()].into_iter().flatten().collect::<Vec<_>>().join(" · ")
    }
}

#[derive(Default)]
struct State {
    items: Vec<Item>,
    /// Torrents the user closed the notifier on; it opens again only for one not in this set.
    dismissed: HashSet<String>,
    /// The one block whose checklist is open (SPEC 8.6); every other block is compact.
    expanded: Option<String>,
    /// Magnet links whose file list the user asked for and is still coming: their block opens when it arrives.
    requested: HashSet<String>,
}

impl State {
    fn active_choice(&self) -> Option<&str> {
        self.items.iter().find(|i| i.awaiting && Some(&i.id) == self.expanded.as_ref()).map(|i| i.id.as_str())
    }
    /// The block that owns Return (Start) and Escape (Cancel): the open checklist, else the only block there is.
    fn active_block(&self) -> Option<&str> {
        self.active_choice().or(match &self.items[..] {
            [only] => Some(only.id.as_str()),
            _ => None,
        })
    }
    fn wants_to_show(&self) -> bool {
        self.items.iter().any(|i| !self.dismissed.contains(&i.id))
    }
    /// Someone has to decide: the notifier stays until Start, Cancel or its ×.
    fn has_choice(&self) -> bool {
        self.items.iter().any(|i| i.choosing() && !self.dismissed.contains(&i.id))
    }
    fn collapse(&mut self, id: &str) {
        if self.expanded.as_deref() == Some(id) {
            self.expanded = None;
        }
    }
}

/// The folder the files go to: where the row's path lives, `~`-abbreviated. A magnet link's path is the download
/// folder itself.
pub(crate) fn folder_of(save_path: &str, home: Option<&str>) -> Option<String> {
    if save_path.is_empty() {
        return None;
    }
    let parent = Path::new(save_path).parent().map(|p| p.display().to_string()).filter(|p| !p.is_empty());
    Some(shown_in(parent.as_deref().unwrap_or(save_path), home))
}

/// The notifier's state: its torrents, which the user put away, the open checklist and the magnet links whose list
/// was asked for. One per app; the host asks it to look ([`Starting::refresh`]) on every pending event and every
/// `tick_ms` while it is up.
#[derive(uniffi::Object)]
pub struct Starting {
    choices: Arc<Choices>,
    pending: Arc<dyn PendingSource>,
    home: Option<String>,
    state: Mutex<State>,
}

impl Starting {
    pub(crate) fn with(choices: Arc<Choices>, pending: Arc<dyn PendingSource>, home: Option<String>) -> Arc<Starting> {
        Arc::new(Starting { choices, pending, home, state: Mutex::new(State::default()) })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Replaces the items with what the core reports (`rows`: the downloading rows).
    fn sync(&self, rows: &[TransferRow], now_ms: u64) {
        let mut s = self.lock();
        let known: HashMap<String, Item> = s.items.drain(..).map(|i| (i.id.clone(), i)).collect();
        let magnet = |r: &TransferRow| r.size_bytes == 0;
        let mut items = Vec::new();
        for r in rows.iter().filter(|r| r.awaiting_choice || r.pending.is_some() || (s.requested.contains(&r.id) && magnet(r))) {
            let old = known.get(&r.id);
            let kind = if magnet(r) { "Magnet link".into() } else { category_label(r.category) };
            let base = Item {
                id: r.id.clone(),
                title: r.display_name.clone(),
                kind,
                folder: folder_of(&r.save_path, self.home.as_deref()),
                size: r.size_bytes,
                category: r.category,
                total_ms: 1_000,
                starts_at_ms: now_ms,
                stopped_ms: None,
                awaiting: false,
                fetching: false,
                has_source: self.choices.has_source(r.id.clone()),
            };
            let item = match r.pending.filter(|_| !r.awaiting_choice) {
                // Waiting for the user: no countdown; a magnet link without metadata is still fetching. The chooser's
                // own place, when the plan names one (it follows a category change at once).
                None => Item {
                    folder: self.choices.loaded(r.id.clone()).and_then(|c| c.destination()).or(base.folder.clone()),
                    total_ms: old.map_or(1_000, |o| o.total_ms),
                    awaiting: !magnet(r),
                    fetching: magnet(r),
                    ..base
                },
                Some(p) => {
                    let left = pending_remaining_ms(&p, now_ms);
                    Item {
                        total_ms: old.map_or(0, |o| o.total_ms).max(left).max(1_000),
                        starts_at_ms: now_ms + left,
                        stopped_ms: p.paused.then_some(left),
                        ..base
                    }
                }
            };
            items.push(item);
        }
        let ids: HashSet<&String> = items.iter().map(|i| &i.id).collect();
        s.dismissed.retain(|id| ids.contains(id));
        // An open block goes with its torrent. One between countdown and choice (the engine's answer to Choose Files
        // comes a moment after the click) is in `rows` but not in the items: it stays open.
        if let Some(e) = s.expanded.clone()
            && !rows.iter().any(|r| r.id == e)
        {
            s.expanded = None;
        }
        s.items = items;
    }

    fn page(&self, s: &State, now_ms: u64) -> StartingPage {
        let active = s.active_block().map(str::to_owned);
        let blocks = s
            .items
            .iter()
            .map(|i| {
                let is_active = active.as_deref() == Some(i.id.as_str());
                let chooser = if i.awaiting { self.choices.loaded(i.id.clone()) } else { None };
                let (state, chevron, actions, line) = if let Some(c) = chooser {
                    let expanded = s.expanded.as_deref() == Some(i.id.as_str());
                    let help = if expanded { "Start downloading the ticked files" } else { "Start downloading every file. Open the list to choose" };
                    let can_start = c.selected_bytes() > 0;
                    (
                        StartingBlockState::Choice { expanded },
                        Some(chevron(expanded, if expanded { "Back to the short view" } else { "Show the files" })),
                        held(i.has_source, is_active, Some((can_start, help.into()))),
                        "Choose what to download".to_owned(),
                    )
                } else if i.choosing() {
                    let line = "Fetching file list…".to_owned();
                    (StartingBlockState::Fetching { line: line.clone() }, None, held(false, is_active, None), line)
                } else {
                    (
                        StartingBlockState::Countdown { countdown: i.countdown() },
                        Some(chevron(false, "Choose files: ends the countdown and shows the files")),
                        held(i.has_source, is_active, Some((true, "Skip the wait and start downloading now".into()))),
                        starting_tick(i.countdown(), now_ms).line,
                    )
                };
                StartingBlock {
                    id: i.id.clone(),
                    title: i.title.clone(),
                    detail: i.detail(),
                    open_help: format!("Open in {APP_NAME}"),
                    // A category is only known once the files are.
                    category: (i.size > 0).then(|| StartingCategory {
                        category: i.category,
                        title: category_label(i.category),
                        help: format!("Category: {} — click to change", category_label(i.category)),
                        choices: category_choices(i.category),
                    }),
                    chevron,
                    state,
                    line,
                    actions,
                    active: is_active,
                }
            })
            .collect();
        let show = if s.wants_to_show() {
            StartingShow::Show { stays: s.has_choice() }
        } else if !s.items.is_empty() {
            StartingShow::Quiet
        } else {
            StartingShow::Close
        };
        StartingPage {
            blocks,
            show,
            compact: s.items.len() > 1,
            close_help: s.has_choice().then(|| "Close this and leave the torrents waiting. They stay in the menu and the main window".into()),
            tick_ms: TICK_MS,
        }
    }
}

impl Starting {
    /// A countdown's chevron, "Choose Files": the countdown ends, the torrent stays held and this block's checklist
    /// opens here; every other block goes compact. A magnet link opens once its file list has arrived.
    pub fn choose_files(&self, id: String) -> StartingChoose {
        match self.pending.choose(&id) {
            Ok(true) => {
                self.lock().expanded = Some(id);
                StartingChoose::Open
            }
            Ok(false) => {
                let mut s = self.lock();
                s.requested.insert(id.clone());
                s.expanded = Some(id);
                StartingChoose::AfterMetadata
            }
            Err(_) => StartingChoose::Failed,
        }
    }

    /// Stops or runs a countdown again.
    pub fn toggle_pause(&self, id: String) {
        let paused = self.lock().items.iter().find(|i| i.id == id).is_some_and(Item::paused);
        self.pending.pause(&id, !paused);
    }

    /// Start on a countdown: it starts now.
    pub fn start_now(&self, id: String) {
        self.pending.start_now(&id);
    }

    /// Start on a choice: the ticks go to the engine (an error for the host's alert, "Couldn't start").
    pub fn confirm(&self, id: String) -> Result<(), CoreError> {
        self.lock().collapse(&id);
        self.choices.confirm(id)
    }

    /// Cancel (`keep_file`: ⇧, keeping the `.torrent` file): the torrent is removed; the note to show quietly when
    /// the file could not be moved. Escape and ⇧Escape can both arrive: a torrent is cancelled once.
    pub fn cancel(&self, id: String, keep_file: bool) -> StartingCancel {
        {
            let mut s = self.lock();
            if !s.items.iter().any(|i| i.id == id) {
                return StartingCancel { cancelled: false, note: None };
            }
            // Fetching a magnet link's list: not waiting for a choice yet, nothing more to wait for.
            s.requested.remove(&id);
            s.collapse(&id);
        }
        StartingCancel { cancelled: true, note: self.choices.cancel(id, keep_file) }
    }

    /// The title and its line: the host opens the main window on this torrent. A countdown ends first (as the chevron
    /// does) and the torrent waits for the choice, which the window shows.
    pub fn open(&self, id: String) {
        let counting = self.lock().items.iter().any(|i| i.id == id && !i.choosing());
        if counting && matches!(self.pending.choose(&id), Ok(false)) {
            self.lock().requested.insert(id);
        }
    }

    /// The category pill's menu: the torrent is planned again as `category`.
    pub fn change_category(&self, id: String, category: Category) {
        self.choices.change_category(id, category);
    }
}

#[uniffi::export]
impl Starting {
    /// The notifier of the app, its checklists the `choices`' (shared with the chooser window and the window's list).
    #[uniffi::constructor]
    pub fn new(core: Arc<crate::Core>, choices: Arc<Choices>) -> Arc<Self> {
        Starting::with(choices, core, home())
    }

    /// Looks at the core's downloading rows (the displayed ones: a hidden tracker's torrents are not named) at
    /// `now_ms` and says what to draw and whether to show. `launch`: the app just started, so torrents already
    /// waiting for a choice are not announced again. The checklists of torrents that wait are loaded here, the
    /// others' forgotten.
    pub fn refresh(&self, rows: Vec<TransferRow>, now_ms: u64, launch: bool) -> StartingPage {
        self.sync(&rows, now_ms);
        let waiting: Vec<String> = {
            let mut s = self.lock();
            if launch {
                let choosing: Vec<String> = s.items.iter().filter(|i| i.choosing()).map(|i| i.id.clone()).collect();
                s.dismissed.extend(choosing);
            }
            s.items.iter().filter(|i| i.awaiting).map(|i| i.id.clone()).collect()
        };
        self.choices.prune(waiting.clone());
        for id in waiting {
            self.choices.chooser(id, false);
        }
        let s = self.lock();
        self.page(&s, now_ms)
    }

    // The notifier's buttons reach the engine: each returns at once and runs off the caller's thread (the FFI's names
    // are those of the synchronous functions they run). The host draws again (`refresh`) when one is done.

    /// "Choose Files" ([`Starting::choose_files`]).
    #[uniffi::method(name = "choose_files")]
    pub async fn choose_files_async(self: Arc<Self>, id: String) -> StartingChoose {
        crate::calls::apart(move || self.choose_files(id)).await
    }

    /// Stops or runs a countdown again ([`Starting::toggle_pause`]).
    #[uniffi::method(name = "toggle_pause")]
    pub async fn toggle_pause_async(self: Arc<Self>, id: String) {
        crate::calls::apart(move || self.toggle_pause(id)).await
    }

    /// Start on a countdown ([`Starting::start_now`]).
    #[uniffi::method(name = "start_now")]
    pub async fn start_now_async(self: Arc<Self>, id: String) {
        crate::calls::apart(move || self.start_now(id)).await
    }

    /// Start on a choice ([`Starting::confirm`]).
    #[uniffi::method(name = "confirm")]
    pub async fn confirm_async(self: Arc<Self>, id: String) -> Result<(), CoreError> {
        crate::calls::apart(move || self.confirm(id)).await
    }

    /// Cancel ([`Starting::cancel`]).
    #[uniffi::method(name = "cancel")]
    pub async fn cancel_async(self: Arc<Self>, id: String, keep_file: bool) -> StartingCancel {
        crate::calls::apart(move || self.cancel(id, keep_file)).await
    }

    /// The title and its line ([`Starting::open`]): the host opens the main window on the torrent once it is done.
    #[uniffi::method(name = "open")]
    pub async fn open_async(self: Arc<Self>, id: String) {
        crate::calls::apart(move || self.open(id)).await
    }

    /// The category pill's menu ([`Starting::change_category`]).
    #[uniffi::method(name = "change_category")]
    pub async fn change_category_async(self: Arc<Self>, id: String, category: Category) {
        crate::calls::apart(move || self.change_category(id, category)).await
    }

    /// The page as it stands, without a look at the core.
    pub fn page_now(&self, now_ms: u64) -> StartingPage {
        let s = self.lock();
        self.page(&s, now_ms)
    }

    /// The file list of a magnet link the user asked to choose files for has arrived: its block opens. `false` when
    /// nobody asked (the host then says "Choose what to download" its own way).
    pub fn consume_request(&self, id: String) -> bool {
        let mut s = self.lock();
        let asked = s.requested.remove(&id);
        if asked {
            s.expanded = Some(id);
        }
        asked
    }

    /// Asks for `id`'s file list before its chooser can open (a magnet link, from outside the notifier): its block
    /// shows while the list is coming and opens when it arrives.
    pub fn request(&self, id: String) {
        self.lock().requested.insert(id);
    }

    /// Opens this block's checklist and compacts every other one.
    pub fn expand(&self, id: String) {
        self.lock().expanded = Some(id);
    }

    pub fn collapse(&self, id: String) {
        self.lock().collapse(&id);
    }

    /// A choice's chevron: opens or closes its checklist.
    pub fn toggle_expanded(&self, id: String) {
        let mut s = self.lock();
        s.expanded = if s.expanded.as_deref() == Some(id.as_str()) { None } else { Some(id) };
    }

    /// The user put the notifier away (its ×, a click elsewhere, the menu bar menu opening on its spot): it stays
    /// closed for these torrents and opens again only for a new one.
    pub fn dismiss_all(&self) {
        let mut s = self.lock();
        let ids: Vec<String> = s.items.iter().map(|i| i.id.clone()).collect();
        s.dismissed.extend(ids);
    }
}

#[cfg(test)]
#[path = "starting_tests.rs"]
mod tests;
