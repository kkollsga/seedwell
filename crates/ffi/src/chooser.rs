//! The chooser of what to download (SPEC 7A.6) as a model every host draws (docs/CROSS-PLATFORM.md §1): the chooser
//! window, the notifier's checklist of a torrent that waits for a choice and the list under its row in the window
//! show one [`Chooser`] per torrent, so a tick in one shows in the others. Rust decides what the items are (seasons,
//! albums, the Extras group), what starts ticked (everything but the Extras for a torrent that waits), the quick picks,
//! every word and count, and what Start sends; the host draws the [`ChooserPage`] and calls the methods its clicks and
//! keys stand for.
//!
//! - [`Choices`]: the one chooser per torrent, loaded from the core, with Start, Cancel and the category change.
//! - [`Chooser`]: one torrent's ticks, folds and quick pick; [`Chooser::view`] its view model.
//! - [`held_actions`]: Cancel and Start of everything that is held or new, with their keys.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use seedwell_control::display;
use seedwell_platform::computer;

use crate::calls::{Polled, UI_WAIT};
use crate::files::FileCheck;
use crate::held::{cancel_help, choice_default_files, CancelNote};
use crate::row_text::category_label;
use crate::types::{Category, ChoiceGroupInfo, ChoiceGroupKind, ChoiceInfo, ChoiceItemInfo, CoreError};
use crate::Core;

// MARK: Cancel and Start

/// A key that presses a button of [`HeldActions`] while its block owns the keys.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum HeldKey {
    /// Return (Enter): Start.
    Return,
    /// Escape: Cancel.
    Escape,
    /// Shift-Escape: Cancel, keeping the `.torrent` file.
    ShiftEscape,
}

/// A button of [`HeldActions`].
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HeldAction {
    pub title: String,
    pub help: String,
    pub enabled: bool,
    /// The keys that press it; empty unless its block owns the keys.
    pub keys: Vec<HeldKey>,
}

/// Cancel's Shift variant: it keeps the `.torrent` file. The caption shows under Cancel while Shift is held.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HeldKeepFile {
    /// "Keep .torrent file".
    pub caption: String,
    /// The keys that cancel and keep the file (⇧Esc); empty unless the block owns the keys.
    pub keys: Vec<HeldKey>,
}

/// The two actions of everything that is held or new (SPEC 7A.6): **Cancel** and **Start**, the same words everywhere.
/// Return is Start, Escape is Cancel and Shift-Escape is Cancel keeping the `.torrent` file, for the one block that owns
/// the keys. Each host keeps its own button order (the Mac: Cancel, then Start, the default, at the right).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct HeldActions {
    /// Cancel: removes the torrent (`Core::cancel_held`, its `.torrent` file to the Trash).
    pub cancel: HeldAction,
    /// Present only when there is a `.torrent` file to keep: Shift held, Cancel keeps it (`cancel_held(id, true)`).
    pub keep_file: Option<HeldKeepFile>,
    /// Start: `None` where there is nothing to start yet (a magnet link fetching its files).
    pub start: Option<HeldAction>,
}

const CANCEL: &str = "Cancel";
const START: &str = "Start";
const KEEP_FILE: &str = "Keep .torrent file";

/// Cancel and Start of a held torrent: `has_source_file` gives Cancel its Shift variant, `active` the keys (the block
/// that owns Return and Escape), `can_start` enables Start and `start_help` is its tooltip.
#[uniffi::export]
pub fn held_actions(has_source_file: bool, active: bool, can_start: bool, start_help: String) -> HeldActions {
    held(has_source_file, active, Some((can_start, start_help)))
}

pub(crate) fn held(has_source_file: bool, active: bool, start: Option<(bool, String)>) -> HeldActions {
    let keys = |k: HeldKey| if active { vec![k] } else { Vec::new() };
    HeldActions {
        cancel: HeldAction { title: CANCEL.into(), help: cancel_help(has_source_file), enabled: true, keys: keys(HeldKey::Escape) },
        keep_file: has_source_file.then(|| HeldKeepFile { caption: KEEP_FILE.into(), keys: keys(HeldKey::ShiftEscape) }),
        start: start.map(|(enabled, help)| HeldAction { title: START.into(), help, enabled, keys: keys(HeldKey::Return) }),
    }
}

// MARK: The view model

/// A quick pick of the chooser: what to tick in one click.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ChooserPick {
    /// "All": every item but the Extras.
    All,
    /// "What I don't have": every item the library does not have.
    Missing,
    /// "Latest season": the last one.
    Latest,
}

fn pick_title(p: ChooserPick) -> &'static str {
    match p {
        ChooserPick::All => "All",
        ChooserPick::Missing => "What I don't have",
        ChooserPick::Latest => "Latest season",
    }
}

/// A quick pick's button.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChooserPickButton {
    pub pick: ChooserPick,
    pub title: String,
    pub help: String,
    /// The ticks are this pick's (drawn as the chosen segment).
    pub selected: bool,
}

/// A category in a menu of categories, the current one checked.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct CategoryChoice {
    pub category: Category,
    pub title: String,
    pub checked: bool,
}

/// Every category, in the order the menus list them, `current` checked.
#[uniffi::export]
pub fn category_choices(current: Category) -> Vec<CategoryChoice> {
    use Category::*;
    [Movie, Tv, Music, Audiobook, Book, Software, Game, Other]
        .into_iter()
        .map(|c| CategoryChoice { category: c, title: category_label(c), checked: c == current })
        .collect()
}

/// A line of the rule card: a quiet label and its value ("Rule " **Y – seed to 1.0**).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChooserLine {
    pub label: String,
    pub value: String,
    /// The value is drawn in a heavier weight (the rule's name).
    pub emphasized: bool,
}

/// One row of the list: a group (level 0) or one of its items (level 1), or an item without a group.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChooserRow {
    /// Stable for the chooser's life: what `toggle` and `toggle_folded` take.
    pub id: String,
    pub level: u32,
    pub title: String,
    /// "10 episodes", "1080p WEB-DL · for this Mac"; empty for none.
    pub detail: String,
    /// "4.3 GB".
    pub size: String,
    /// "In library" or "Other quality", drawn as a green badge; `None` for none.
    pub badge: Option<String>,
    pub check: FileCheck,
    /// The check's tooltip: "Skip this" or "Download this".
    pub check_help: String,
    /// What a screen reader says of the check: "checked", "partly checked", "unchecked".
    pub check_value: String,
    /// Drawn in the secondary colour: in the library and not ticked, or the Extras.
    pub dimmed: bool,
    /// A group with items: it has a fold chevron.
    pub foldable: bool,
    /// Its items are shown under it.
    pub open: bool,
    /// The chevron's tooltip ("Hide the contents" / "Show the contents") and its screen reader name ("Collapse" /
    /// "Expand"); empty when not `foldable`.
    pub fold_help: String,
    pub fold_label: String,
    /// The keyboard cursor of the checklist is on it (Space ticks it, ↑ ↓ move it).
    pub cursor: bool,
}

/// The "Select all / none" line of the checklist: one box for every item.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ChooserAll {
    pub check: FileCheck,
    /// "Select none" when everything is ticked, else "Select all".
    pub title: String,
    pub help: String,
    /// The box's own tooltip and what a screen reader says of it, as a row's.
    pub check_help: String,
    pub check_value: String,
}

/// The chooser of one torrent as every host draws it: the header, the category card with its Change menu, the rule
/// card, the notice, the quick picks, the rows, the totals and the buttons. The notifier's checklist and the window's
/// list under a held row draw `rows`, `all`, `selected_line` and `space_note`; the chooser window draws all of it.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ChooserPage {
    pub torrent_id: String,
    /// "Choose what to download" (a held torrent) or "Choose files" (a running one's Files…).
    pub origin: String,
    /// The release name.
    pub name: String,
    /// "212 GB · 58 files · tracker-y.net".
    pub summary: String,
    pub category: Category,
    /// "TV · Fallen Kingdom".
    pub category_title: String,
    /// "98% confident".
    pub category_detail: String,
    /// The classifier is unsure: the card is drawn in the warning colour, the notice with a question mark.
    pub unsure: bool,
    /// The card's menu button: "Change", its tooltip and the categories.
    pub change_title: String,
    pub change_help: String,
    pub categories: Vec<CategoryChoice>,
    /// The rule card's lines (Rule, Saves to, Then); empty: no card.
    pub rule_lines: Vec<ChooserLine>,
    /// Why it asks ("Seasons 1 and 2 are already in …"); `None` for nothing.
    pub notice: Option<String>,
    /// The quick picks; empty when there is only one (no segmented control then).
    pub picks: Vec<ChooserPickButton>,
    pub rows: Vec<ChooserRow>,
    pub all: ChooserAll,
    /// "1 season + 2 episodes · 86.0 GB · 412 GB free".
    pub totals_line: String,
    /// "126 GB of 212 GB" (the checklist's line).
    pub selected_line: String,
    /// More is ticked than there is room for: the totals are drawn in the warning colour with `space_warning` ("Not
    /// enough space") beside them, the checklist with `space_note` ("Not enough space at the destination") under it.
    pub not_enough_space: bool,
    pub space_warning: String,
    pub space_note: String,
    /// The torrent waits for this choice (Cancel and Start) rather than a running one's Files… (Cancel and Download).
    pub awaiting: bool,
    /// The chooser window's buttons. Awaiting: Cancel and Start ([`held_actions`], the window owns the keys). Else
    /// Cancel closes without a change and the primary button ("Download") sends the ticks.
    pub buttons: HeldActions,
    /// Something is ticked: Start (Download) can send it.
    pub can_confirm: bool,
    /// How many items are ticked, and which groups are open (QA's state line).
    pub checked_count: u32,
    pub open_groups: Vec<String>,
}

// MARK: The model

#[derive(Clone, Debug)]
struct Item {
    id: String,
    title: String,
    detail: String,
    size: u64,
    in_library: bool,
    is_extras: bool,
    /// Files that belong to no item (kept whenever something is chosen): never counted as a title.
    is_other: bool,
    badge: &'static str,
    children: Vec<Item>,
    /// File indices of the torrent this item stands for (all of its items' for a group).
    files: Vec<u32>,
}

impl Item {
    fn leaves(&self) -> Vec<&Item> {
        if self.children.is_empty() { vec![self] } else { self.children.iter().collect() }
    }
}

#[derive(Clone, Debug)]
struct State {
    torrent_id: String,
    origin: String,
    name: String,
    summary: String,
    category: Category,
    category_title: String,
    category_detail: String,
    notice: Option<String>,
    items: Vec<Item>,
    destination: String,
    rule_name: String,
    then_line: String,
    free_space: u64,
    unit: (&'static str, &'static str),
    child_unit: (&'static str, &'static str),
    picks: Vec<ChooserPick>,
    checked: HashSet<String>,
    open: Vec<String>,
    pick: Option<ChooserPick>,
    /// The user ticked or unticked something (or took a pick): a category change keeps their ticks then.
    edited: bool,
    file_count: u32,
    awaiting: bool,
    unsure: bool,
    cursor: Option<String>,
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `path` written with `~` for the home folder `home` (a Unix habit; Windows keeps the full path).
pub(crate) fn shown_in(path: &str, home: Option<&str>) -> String {
    if cfg!(unix)
        && let Some(home) = home.map(|h| h.trim_end_matches('/')).filter(|h| !h.is_empty())
    {
        if path == home {
            return "~".into();
        }
        if let Some(rest) = path.strip_prefix(home).filter(|r| r.starts_with('/')) {
            return format!("~{rest}");
        }
    }
    path.into()
}

pub(crate) fn home() -> Option<String> {
    seedwell_platform::dirs::home().map(|h| h.display().to_string())
}

impl State {
    /// The chooser of what the engine found (`info`); `origin` is its header line.
    fn new(info: &ChoiceInfo, origin: &str, home: Option<&str>) -> State {
        // A torrent classified Other has no albums or seasons: its items are listed plainly.
        let generic = info.category == Category::Other;
        let leaf = |gi: usize, ii: usize, i: &ChoiceItemInfo| {
            let mut detail = i.quality.clone().unwrap_or_default();
            if i.this_mac {
                let here = concat!("for this ", computer!());
                detail = if detail.is_empty() { here.into() } else { format!("{detail} · {here}") };
            }
            Item {
                id: format!("g{gi}i{ii}"),
                title: i.label.clone(),
                detail,
                size: i.bytes,
                in_library: i.in_library || i.other_quality,
                is_extras: false,
                is_other: false,
                badge: if i.other_quality && !i.in_library { "Other quality" } else { "In library" },
                children: Vec::new(),
                files: i.files.clone(),
            }
        };
        let mut items = Vec::new();
        let mut preselected = HashSet::new();
        for (gi, g) in info.groups.iter().enumerate() {
            let leaves: Vec<Item> = g.items.iter().enumerate().map(|(ii, i)| leaf(gi, ii, i)).collect();
            for (l, src) in leaves.iter().zip(&g.items) {
                if src.selected {
                    preselected.insert(l.id.clone());
                }
            }
            let flat = (generic && g.kind != ChoiceGroupKind::Extras && g.kind != ChoiceGroupKind::Other)
                || matches!(g.kind, ChoiceGroupKind::Collection | ChoiceGroupKind::Other);
            if flat {
                items.extend(leaves.into_iter().map(|mut l| {
                    l.is_other = g.kind == ChoiceGroupKind::Other;
                    l
                }));
                continue;
            }
            let n = leaves.len();
            let detail = match g.kind {
                ChoiceGroupKind::Extras => "samples, featurettes, .nfo".into(),
                ChoiceGroupKind::Season | ChoiceGroupKind::Specials => plural(n, "episode", "episodes"),
                _ => plural(n, "item", "items"),
            };
            items.push(Item {
                id: format!("g{gi}"),
                title: g.label.clone(),
                detail,
                size: leaves.iter().map(|l| l.size).sum(),
                in_library: !leaves.is_empty() && leaves.iter().all(|l| l.in_library),
                is_extras: g.kind == ChoiceGroupKind::Extras,
                is_other: false,
                badge: "In library",
                files: leaves.iter().flat_map(|l| l.files.iter().copied()).collect(),
                children: leaves,
            });
        }
        let unit = if generic {
            ("Item", "Items")
        } else {
            match info.structure.as_str() {
                "tv" => ("Season", "Seasons"),
                "movies" => ("Movie", "Movies"),
                "music" => ("Album", "Albums"),
                "books" | "audiobooks" => ("Title", "Titles"),
                "software" => ("Build", "Builds"),
                _ => ("Item", "Items"),
            }
        };
        let child_unit = match info.structure.as_str() {
            "tv" => ("Episode", "Episodes"),
            "music" => ("Track", "Tracks"),
            _ => ("File", "Files"),
        };
        let title = if info.display_name.is_empty() { &info.name } else { &info.display_name };
        let mut picks = vec![ChooserPick::All];
        if items.iter().any(|i| i.in_library) || info.library_checked {
            picks.push(ChooserPick::Missing);
        }
        if info.structure == "tv" {
            picks.push(ChooserPick::Latest);
        }
        let mut summary = vec![display::bytes(info.total_bytes), plural(info.file_count as usize, "file", "files")];
        if !info.tracker.is_empty() {
            summary.push(info.tracker.clone());
        }
        let destination = match info.destination.as_deref() {
            None | Some("") => "the download folder".into(),
            Some(d) => shown_in(d, home),
        };
        let mut s = State {
            torrent_id: info.id.clone(),
            origin: origin.into(),
            name: info.name.clone(),
            summary: summary.join(" · "),
            category: info.category,
            category_title: format!("{} · {title}", category_label(info.category)),
            category_detail: format!("{}% confident", (info.confidence * 100.0).round() as i64),
            notice: (!info.reasons.is_empty()).then(|| info.reasons.join(" ")),
            items,
            destination,
            rule_name: info.rule_name.clone(),
            then_line: info.then_text.clone(),
            free_space: info.free_bytes.unwrap_or(u64::MAX / 2),
            unit,
            child_unit,
            picks,
            checked: preselected,
            open: Vec::new(),
            pick: None,
            edited: false,
            file_count: info.file_count,
            awaiting: info.awaiting,
            unsure: info.unsure,
            cursor: None,
        };
        let regular_on = |s: &State, other: bool| s.items.iter().filter(|i| !i.is_extras && (other || !i.is_other)).all(|i| s.check(i) == FileCheck::On);
        if s.picks.contains(&ChooserPick::Missing) && !regular_on(&s, true) {
            s.pick = Some(ChooserPick::Missing);
        } else if regular_on(&s, false) {
            s.pick = Some(ChooserPick::All);
        }
        if let Some(first) = s.items.iter().find(|i| !i.children.is_empty() && s.check(i) != FileCheck::Off) {
            s.open = vec![first.id.clone()];
        }
        s
    }

    fn all_items(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().flat_map(|i| std::iter::once(i).chain(i.children.iter()))
    }

    fn find(&self, id: &str) -> Option<&Item> {
        self.all_items().find(|i| i.id == id)
    }

    fn leaves(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().flat_map(Item::leaves)
    }

    fn check(&self, item: &Item) -> FileCheck {
        let leaves = item.leaves();
        let n = leaves.iter().filter(|l| self.checked.contains(&l.id)).count();
        if n == 0 {
            FileCheck::Off
        } else if n == leaves.len() {
            FileCheck::On
        } else {
            FileCheck::Mixed
        }
    }

    fn all_check(&self) -> FileCheck {
        let total = self.leaves().count();
        let n = self.leaves().filter(|l| self.checked.contains(&l.id)).count();
        if n == 0 {
            FileCheck::Off
        } else if n == total {
            FileCheck::On
        } else {
            FileCheck::Mixed
        }
    }

    fn toggle(&mut self, id: &str) {
        let Some(item) = self.find(id) else { return };
        let on = self.check(item) == FileCheck::On;
        let ids: Vec<String> = item.leaves().iter().map(|l| l.id.clone()).collect();
        for id in ids {
            if on {
                self.checked.remove(&id);
            } else {
                self.checked.insert(id);
            }
        }
        self.pick = None;
        self.edited = true;
    }

    fn toggle_folded(&mut self, id: &str) {
        if !self.items.iter().any(|i| i.id == id && !i.children.is_empty()) {
            return;
        }
        if let Some(at) = self.open.iter().position(|o| o == id) {
            self.open.remove(at);
        } else {
            self.open.push(id.into());
        }
    }

    fn apply(&mut self, p: ChooserPick) {
        self.pick = Some(p);
        self.edited = true;
        let regular: Vec<&Item> = self.items.iter().filter(|i| !i.is_extras).collect();
        let take: Vec<&Item> = match p {
            ChooserPick::All => regular,
            ChooserPick::Missing => regular.into_iter().filter(|i| !i.in_library).collect(),
            ChooserPick::Latest => regular.last().copied().into_iter().collect(),
        };
        self.checked = take.iter().flat_map(|i| i.leaves()).map(|l| l.id.clone()).collect();
    }

    fn set_all(&mut self, on: bool) {
        self.checked = if on { self.leaves().map(|l| l.id.clone()).collect() } else { HashSet::new() };
        self.pick = None;
        self.edited = true;
    }

    /// Ticks the leaves whose files `files` all wants (one flag per torrent file).
    fn tick_files(&mut self, files: &[bool]) {
        self.checked = self
            .leaves()
            .filter(|l| !l.files.is_empty() && l.files.iter().all(|&f| files.get(f as usize).copied().unwrap_or(false)))
            .map(|l| l.id.clone())
            .collect();
        self.pick = None;
    }

    fn file_flags(&self) -> Vec<bool> {
        let mut flags = vec![false; self.file_count as usize];
        for l in self.leaves().filter(|l| self.checked.contains(&l.id)) {
            for &f in &l.files {
                if let Some(flag) = flags.get_mut(f as usize) {
                    *flag = true;
                }
            }
        }
        flags
    }

    fn selected_bytes(&self) -> u64 {
        self.leaves().filter(|l| self.checked.contains(&l.id)).map(|l| l.size).sum()
    }

    fn total_bytes(&self) -> u64 {
        self.items.iter().map(|i| i.size).sum()
    }

    /// "1 season", "3 episodes" or "1 season + 2 episodes": whole items count as the unit, the ticked items of a
    /// partly ticked group as theirs. Empty when nothing is ticked.
    fn selection_phrase(&self) -> String {
        let main: Vec<&Item> = self.items.iter().filter(|i| !i.is_extras && !i.is_other && self.check(i) != FileCheck::Off).collect();
        let whole = main.iter().filter(|i| self.check(i) == FileCheck::On || i.children.is_empty()).count();
        let parts: usize = main
            .iter()
            .filter(|i| self.check(i) == FileCheck::Mixed)
            .map(|i| i.leaves().iter().filter(|l| self.checked.contains(&l.id)).count())
            .sum();
        let mut out = Vec::new();
        if whole > 0 {
            out.push(plural(whole, self.unit.0, self.unit.1));
        }
        if parts > 0 {
            out.push(plural(parts, self.child_unit.0, self.child_unit.1));
        }
        out.join(" + ")
    }

    fn visible(&self) -> Vec<(&Item, u32)> {
        self.items
            .iter()
            .flat_map(|top| {
                let open = self.open.contains(&top.id);
                std::iter::once((top, 0)).chain(top.children.iter().filter(move |_| open).map(|c| (c, 1)))
            })
            .collect()
    }

    fn move_cursor(&mut self, delta: i32) {
        let ids: Vec<String> = self.visible().iter().map(|(i, _)| i.id.clone()).collect();
        if ids.is_empty() {
            return;
        }
        let at = self.cursor.as_ref().and_then(|c| ids.iter().position(|i| i == c)).map_or(if delta > 0 { -1 } else { ids.len() as i64 }, |p| p as i64);
        let to = (at + i64::from(delta)).clamp(0, ids.len() as i64 - 1);
        self.cursor = Some(ids[to as usize].clone());
    }

    fn toggle_cursor(&mut self) {
        let visible = self.visible();
        let Some(id) = self
            .cursor
            .as_ref()
            .and_then(|c| visible.iter().find(|(i, _)| &i.id == c))
            .or(visible.first())
            .map(|(i, _)| i.id.clone())
        else {
            return;
        };
        self.cursor = Some(id.clone());
        self.toggle(&id);
    }

    fn view(&self, has_source_file: bool) -> ChooserPage {
        let selected = self.selected_bytes();
        let can_confirm = selected > 0;
        let not_enough_space = selected > self.free_space;
        let rows = self
            .visible()
            .into_iter()
            .map(|(i, level)| {
                let check = self.check(i);
                let foldable = level == 0 && !i.children.is_empty();
                let open = foldable && self.open.contains(&i.id);
                ChooserRow {
                    id: i.id.clone(),
                    level,
                    title: i.title.clone(),
                    detail: i.detail.clone(),
                    size: display::bytes(i.size),
                    badge: i.in_library.then(|| i.badge.into()),
                    check,
                    check_help: check_help(check).into(),
                    check_value: check_value(check).into(),
                    dimmed: (i.in_library && check == FileCheck::Off) || i.is_extras,
                    foldable,
                    open,
                    fold_help: if !foldable { "" } else if open { "Hide the contents" } else { "Show the contents" }.into(),
                    fold_label: if !foldable { "" } else if open { "Collapse" } else { "Expand" }.into(),
                    cursor: self.cursor.as_deref() == Some(i.id.as_str()),
                }
            })
            .collect();
        let all = self.all_check();
        let phrase = self.selection_phrase();
        let mut rule_lines = Vec::new();
        if !self.rule_name.is_empty() || !self.destination.is_empty() {
            let line = |label: &str, value: &str, emphasized| ChooserLine { label: label.into(), value: value.into(), emphasized };
            if !self.rule_name.is_empty() {
                rule_lines.push(line("Rule ", &self.rule_name, true));
            }
            rule_lines.push(line("Saves to ", &self.destination, false));
            if !self.then_line.is_empty() {
                rule_lines.push(line("Then ", &self.then_line, false));
            }
        }
        let buttons = if self.awaiting {
            held(has_source_file, true, Some((can_confirm, "Start downloading the ticked files".into())))
        } else {
            HeldActions {
                cancel: HeldAction { title: CANCEL.into(), help: "Close without changing anything".into(), enabled: true, keys: vec![HeldKey::Escape] },
                keep_file: None,
                start: Some(HeldAction { title: "Download".into(), help: "Download the files ticked now".into(), enabled: can_confirm, keys: vec![HeldKey::Return] }),
            }
        };
        ChooserPage {
            torrent_id: self.torrent_id.clone(),
            origin: self.origin.clone(),
            name: self.name.clone(),
            summary: self.summary.clone(),
            category: self.category,
            category_title: self.category_title.clone(),
            category_detail: self.category_detail.clone(),
            unsure: self.unsure,
            change_title: "Change".into(),
            change_help: "Change what kind of content this is".into(),
            categories: category_choices(self.category),
            rule_lines,
            notice: self.notice.clone(),
            picks: if self.picks.len() > 1 {
                self.picks
                    .iter()
                    .map(|&p| ChooserPickButton {
                        pick: p,
                        title: pick_title(p).into(),
                        help: format!("Use the {} preset for what to download", pick_title(p)),
                        selected: self.pick == Some(p),
                    })
                    .collect()
            } else {
                Vec::new()
            },
            rows,
            all: ChooserAll {
                check: all,
                title: if all == FileCheck::On { "Select none" } else { "Select all" }.into(),
                help: if all == FileCheck::On { "Untick every file" } else { "Tick every file" }.into(),
                check_help: check_help(all).into(),
                check_value: check_value(all).into(),
            },
            totals_line: format!(
                "{} · {} · {} free",
                if phrase.is_empty() { "Nothing selected".into() } else { phrase.to_lowercase() },
                display::bytes(selected),
                display::bytes(self.free_space)
            ),
            selected_line: format!("{} of {}", display::bytes(selected), display::bytes(self.total_bytes())),
            not_enough_space,
            space_warning: "Not enough space".into(),
            space_note: "Not enough space at the destination".into(),
            awaiting: self.awaiting,
            buttons,
            can_confirm,
            checked_count: self.checked.len() as u32,
            open_groups: self.open.clone(),
        }
    }
}

fn check_help(c: FileCheck) -> &'static str {
    if c == FileCheck::On { "Skip this" } else { "Download this" }
}

fn check_value(c: FileCheck) -> &'static str {
    match c {
        FileCheck::On => "checked",
        FileCheck::Mixed => "partly checked",
        FileCheck::Off => "unchecked",
    }
}

/// One torrent's chooser: its items, ticks, folds and quick pick. Shared by every place that shows it; every method
/// takes its own lock.
#[derive(uniffi::Object)]
pub struct Chooser {
    state: Mutex<State>,
}

impl Chooser {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn with(info: &ChoiceInfo, origin: &str, home: Option<&str>) -> Arc<Chooser> {
        Arc::new(Chooser { state: Mutex::new(State::new(info, origin, home)) })
    }
}

#[uniffi::export]
impl Chooser {
    /// The chooser of `info` as the engine found it (its own preselection), headed `origin`. [`Choices`] makes the
    /// ones the app shows; this is for tests and snapshots.
    #[uniffi::constructor]
    pub fn new(info: ChoiceInfo, origin: String) -> Arc<Self> {
        Chooser::with(&info, &origin, home().as_deref())
    }

    pub fn view(&self, has_source_file: bool) -> ChooserPage {
        self.lock().view(has_source_file)
    }

    /// A click on a row's box or title: a group ticks or unticks all of its items.
    pub fn toggle(&self, id: String) {
        let mut s = self.lock();
        s.cursor = Some(id.clone());
        s.toggle(&id);
    }

    /// A group's chevron: shows or hides its items. The ticks never change.
    pub fn toggle_folded(&self, id: String) {
        let mut s = self.lock();
        s.cursor = Some(id.clone());
        s.toggle_folded(&id);
    }

    /// Every group open (QA).
    pub fn open_all(&self) {
        let mut s = self.lock();
        s.open = s.items.iter().filter(|i| !i.children.is_empty()).map(|i| i.id.clone()).collect();
    }

    pub fn apply_pick(&self, pick: ChooserPick) {
        self.lock().apply(pick);
    }

    /// "Select all / none": everything ticked turns into nothing ticked, anything else into everything.
    pub fn toggle_all(&self) {
        let mut s = self.lock();
        let on = s.all_check() != FileCheck::On;
        s.set_all(on);
    }

    pub fn select_all(&self, on: bool) {
        self.lock().set_all(on);
    }

    /// ↑ (-1) and ↓ (1) in the checklist.
    pub fn move_cursor(&self, delta: i32) {
        self.lock().move_cursor(delta);
    }

    /// Space in the checklist: ticks or unticks the row under the cursor (the first one when there is none).
    pub fn toggle_cursor(&self) {
        self.lock().toggle_cursor();
    }

    /// The row whose title holds `text` (case aside), an exact title first (QA's stand-ins for clicks).
    pub fn find_row(&self, text: String) -> Option<String> {
        let s = self.lock();
        let t = text.to_lowercase();
        s.all_items().find(|i| i.title.to_lowercase() == t).or_else(|| s.all_items().find(|i| i.title.to_lowercase().contains(&t))).map(|i| i.id.clone())
    }

    /// What Start sends: one flag per torrent file, the files of every ticked item.
    pub fn file_flags(&self) -> Vec<bool> {
        self.lock().file_flags()
    }

    /// Bytes of the ticked items.
    pub fn selected_bytes(&self) -> u64 {
        self.lock().selected_bytes()
    }

    pub fn total_bytes(&self) -> u64 {
        self.lock().total_bytes()
    }

    pub fn awaiting(&self) -> bool {
        self.lock().awaiting
    }

    pub fn category(&self) -> Category {
        self.lock().category
    }

    /// Where the files go, as the header and the notifier show it ("~/Media/TV/Show"); `None` when the plan names
    /// no folder of its own.
    pub fn destination(&self) -> Option<String> {
        let s = self.lock();
        (!s.destination.starts_with("the ")).then(|| s.destination.clone())
    }
}

// MARK: The choosers of the app

/// What [`Choices`] needs of the core (a stand-in in tests).
pub(crate) trait ChoiceSource: Send + Sync {
    fn load(&self, id: &str, reopen: bool) -> Result<ChoiceInfo, CoreError>;
    fn confirm(&self, id: &str, files: &[bool]) -> Result<(), CoreError>;
    fn cancel(&self, id: &str, keep_file: bool) -> Option<CancelNote>;
    fn has_source(&self, id: &str) -> bool;
    fn set_category(&self, id: &str, category: Category);
}

impl ChoiceSource for Core {
    fn load(&self, id: &str, reopen: bool) -> Result<ChoiceInfo, CoreError> {
        self.choice(id.into(), reopen)
    }
    fn confirm(&self, id: &str, files: &[bool]) -> Result<(), CoreError> {
        self.confirm_choice(id.into(), files.to_vec())
    }
    fn cancel(&self, id: &str, keep_file: bool) -> Option<CancelNote> {
        self.cancel_held(id.into(), keep_file)
    }
    fn has_source(&self, id: &str) -> bool {
        self.has_held_source_file(id.into())
    }
    fn set_category(&self, id: &str, category: Category) {
        Core::set_category(self, vec![id.into()], category);
    }
}

/// How long a look at a held torrent's `.torrent` file stays good (the notifier asks every second).
const SOURCE_FRESH: Duration = Duration::from_secs(2);

/// The one [`Chooser`] per torrent that the notifier, the chooser window and the list under a held row share (SPEC
/// 7A.6): ticks made in one show in the others, and Start or Cancel in any of them resolves the torrent for all.
#[derive(uniffi::Object)]
pub struct Choices {
    source: Arc<dyn ChoiceSource>,
    home: Option<String>,
    models: Mutex<HashMap<String, Arc<Chooser>>>,
    /// Choosers being read from the engine (by torrent and `reopen`): a host draws them on its UI thread, which never
    /// waits for a busy engine.
    loads: Polled<(String, bool), Result<ChoiceInfo, CoreError>>,
    /// Whether a held torrent's `.torrent` file is still there (a look at a disk that may be slow or gone).
    sources: Polled<String, bool>,
}

impl Choices {
    pub(crate) fn with(source: Arc<dyn ChoiceSource>, home: Option<String>) -> Arc<Choices> {
        Arc::new(Choices {
            source,
            home,
            models: Mutex::new(HashMap::new()),
            loads: Polled::new(Duration::ZERO),
            sources: Polled::new(SOURCE_FRESH),
        })
    }

    fn models(&self) -> MutexGuard<'_, HashMap<String, Arc<Chooser>>> {
        self.models.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn origin(awaiting: bool) -> &'static str {
        if awaiting { "Choose what to download" } else { "Choose files" }
    }

    /// Keeps the chooser `info` describes as the shared one of `id` (one made meanwhile wins). A held torrent starts
    /// with everything but the Extras ticked (`choice_default_files`).
    fn keep(&self, id: String, info: &ChoiceInfo) -> Arc<Chooser> {
        let c = Chooser::with(info, Self::origin(info.awaiting), self.home.as_deref());
        if info.awaiting {
            c.lock().tick_files(&choice_default_files(info));
        }
        self.models().entry(id).or_insert(c).clone()
    }

    /// The shared chooser of `id`, loaded from the core on this thread when there is none yet.
    pub(crate) fn load_now(&self, id: &str, reopen: bool) -> Result<Arc<Chooser>, CoreError> {
        if let Some(c) = self.loaded(id.to_string()) {
            return Ok(c);
        }
        let info = self.source.load(id, reopen)?;
        Ok(self.keep(id.to_string(), &info))
    }

    /// Start: the ticks go to the engine and the torrent starts through the normal path (no countdown). The chooser is
    /// forgotten: the host closes what shows it.
    pub fn confirm(&self, id: String) -> Result<(), CoreError> {
        // Start before any list showed it (a row's Start, the menu's): the default ticks, as the list would show them.
        let c = match self.loaded(id.clone()) {
            Some(c) => c,
            None => self.load_now(&id, false)?,
        };
        self.source.confirm(&id, &c.file_flags())?;
        self.models().remove(&id);
        Ok(())
    }

    /// Cancel: the held torrent is removed and its `.torrent` file goes to the Trash unless `keep_file` (Shift).
    /// The note to show quietly when the file could not be moved.
    pub fn cancel(&self, id: String, keep_file: bool) -> Option<CancelNote> {
        let note = self.source.cancel(&id, keep_file);
        self.models().remove(&id);
        note
    }

    /// The category pill's menu and the chooser's Change: the engine plans the torrent again and the shared chooser
    /// is rebuilt in place (new place, rule, structure, picks). A torrent that waits keeps the user's ticks where the
    /// same files are still there; one nobody touched gets the default again. Choosing the current category does
    /// nothing.
    pub fn change_category(&self, id: String, category: Category) {
        let Some(c) = self.loaded(id.clone()) else { return self.source.set_category(&id, category) };
        let (before, edited, awaiting, origin) = {
            let s = c.lock();
            if s.category == category {
                return;
            }
            (s.file_flags(), s.edited, s.awaiting, s.origin.clone())
        };
        self.source.set_category(&id, category);
        let Ok(info) = self.source.load(&id, !awaiting) else { return };
        let mut fresh = State::new(&info, &origin, self.home.as_deref());
        if info.awaiting {
            if edited {
                fresh.tick_files(&before);
                fresh.edited = true;
            } else {
                fresh.tick_files(&choice_default_files(&info));
            }
        }
        *c.lock() = fresh;
    }
}

#[uniffi::export]
impl Choices {
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>) -> Arc<Self> {
        Choices::with(core, home())
    }

    /// Choosers of [`chooser_sample`] for every torrent, reaching no engine (snapshots and design work).
    #[uniffi::constructor]
    pub fn sample() -> Arc<Self> {
        Choices::with(Arc::new(SampleSource), home())
    }

    /// The shared chooser of `id`, loaded from the core the first time. `reopen` is for a running torrent (Get Info ▸
    /// Files); a held one is asked as it is. A held torrent starts with everything but the Extras ticked
    /// (`choice_default_files`): the list shows exactly what Start does. `None` while the core cannot say (yet): the
    /// engine is asked on a thread of its own and waited for a few milliseconds at most; a later call has it.
    pub fn chooser(&self, id: String, reopen: bool) -> Option<Arc<Chooser>> {
        if let Some(c) = self.models().get(&id) {
            return Some(c.clone());
        }
        let key = (id.clone(), reopen);
        let read = match self.loads.kept(&key) {
            Some(r) => Some(r),
            None => {
                let (src, i) = (self.source.clone(), id.clone());
                self.loads.get(key.clone(), UI_WAIT, move || src.load(&i, reopen))
            }
        };
        // A read is used once: the next chooser of this torrent is read again.
        self.loads.forget(&key);
        let info = read?.ok()?;
        Some(self.keep(id, &info))
    }

    /// The shared chooser of `id`, loaded from the core when it is not yet: the FFI's `load`, which waits for the engine
    /// off the caller's thread (a chooser window about to open). `None` when the core cannot say.
    #[uniffi::method(name = "load")]
    pub async fn load_async(self: Arc<Self>, id: String, reopen: bool) -> Option<Arc<Chooser>> {
        crate::calls::apart(move || self.load_now(&id, reopen).ok()).await
    }

    /// Whether Cancel would move a `.torrent` file to the Trash, looked at now off the caller's thread (the FFI's
    /// `has_source_file`; `has_source` answers at once with what was last seen).
    #[uniffi::method(name = "has_source_file")]
    pub async fn has_source_file_async(self: Arc<Self>, id: String) -> bool {
        crate::calls::apart(move || {
            let (src, i) = (self.source.clone(), id.clone());
            self.sources.wait_for(id, move || src.has_source(&i)).unwrap_or(false)
        })
        .await
    }

    /// The chooser of `id` when one is loaded (never loads).
    pub fn loaded(&self, id: String) -> Option<Arc<Chooser>> {
        self.models().get(&id).cloned()
    }

    /// Torrents with a chooser loaded, in order.
    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.models().keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Cancel would move a `.torrent` file to the Trash (so Shift has something to keep). Looked at off the caller's
    /// thread, waited for a few milliseconds at most; `false` until the first look is done.
    pub fn has_source(&self, id: String) -> bool {
        let (src, i) = (self.source.clone(), id.clone());
        self.sources.get(id, UI_WAIT, move || src.has_source(&i)).unwrap_or(false)
    }

    /// Start (the FFI's `confirm`, off the caller's thread).
    #[uniffi::method(name = "confirm")]
    pub async fn confirm_async(self: Arc<Self>, id: String) -> Result<(), CoreError> {
        crate::calls::apart(move || self.confirm(id)).await
    }

    /// Cancel (the FFI's `cancel`, off the caller's thread).
    #[uniffi::method(name = "cancel")]
    pub async fn cancel_async(self: Arc<Self>, id: String, keep_file: bool) -> Option<CancelNote> {
        crate::calls::apart(move || self.cancel(id, keep_file)).await
    }

    /// A category change (the FFI's `change_category`, off the caller's thread).
    #[uniffi::method(name = "change_category")]
    pub async fn change_category_async(self: Arc<Self>, id: String, category: Category) {
        crate::calls::apart(move || self.change_category(id, category)).await
    }

    /// Forgets the chooser of `id` (a running torrent's Files… window closed).
    pub fn forget(&self, id: String) {
        self.models().remove(&id);
    }

    /// Forgets the choosers of torrents that are not in `ids` (those no longer waiting).
    pub fn prune(&self, ids: Vec<String>) {
        let keep: HashSet<String> = ids.into_iter().collect();
        self.models().retain(|id, _| keep.contains(id));
    }
}

// MARK: The sample

/// A season pack for snapshots and design work: seasons 1 and 2 in the library, 3 to 5 to choose, and Extras.
#[uniffi::export]
pub fn chooser_sample() -> ChoiceInfo {
    let season = |n: u32, eps: u32, gb: f64, library: bool| ChoiceGroupInfo {
        label: format!("Season {n}"),
        kind: ChoiceGroupKind::Season,
        items: (1..=eps)
            .map(|e| ChoiceItemInfo {
                label: format!("S{n:02}E{e:02}"),
                bytes: (gb * 1e9 / f64::from(eps)) as u64,
                files: vec![n * 100 + e],
                selected: !library,
                in_library: library,
                other_quality: false,
                quality: (e == 1).then(|| "1080p WEB-DL".into()),
                this_mac: false,
            })
            .collect(),
    };
    let extra = |label: &str, bytes: u64, file: u32| ChoiceItemInfo {
        label: label.into(),
        bytes,
        files: vec![file],
        selected: false,
        in_library: false,
        other_quality: false,
        quality: None,
        this_mac: false,
    };
    ChoiceInfo {
        id: "sample".into(),
        name: "Fallen.Kingdom.Complete.Series.S01-S05.1080p.WEB-DL.x264".into(),
        display_name: "Fallen Kingdom".into(),
        tracker: "tracker-y.net".into(),
        awaiting: true,
        reasons: vec!["Seasons 1 and 2 are already in ~/Media/TV/Fallen Kingdom, so they are not selected.".into()],
        unsure: false,
        structure: "tv".into(),
        groups: vec![
            season(1, 10, 41.2, true),
            season(2, 10, 42.0, true),
            season(3, 10, 43.1, false),
            season(4, 10, 42.7, false),
            season(5, 8, 40.5, false),
            ChoiceGroupInfo {
                label: "Extras".into(),
                kind: ChoiceGroupKind::Extras,
                items: vec![extra("Featurettes", 2_000_000_000, 900), extra("Sample", 100_000_000, 901)],
            },
        ],
        file_count: 902,
        total_bytes: 211_600_000_000,
        selected_bytes: 126_300_000_000,
        free_bytes: Some(412_000_000_000),
        destination: Some("~/Media/TV/Fallen Kingdom".into()),
        category: Category::Tv,
        confidence: 0.98,
        library_checked: true,
        rule_name: "Y – seed to 1.0".into(),
        then_text: "seeds until ratio 1.0".into(),
        summary_text: String::new(),
    }
}

#[cfg(test)]
#[path = "chooser_tests.rs"]
pub(crate) mod tests;

/// The sample's choosers (snapshots, design work): every torrent is [`chooser_sample`], `sample` has a `.torrent` file
/// to keep, and nothing reaches an engine.
struct SampleSource;

impl ChoiceSource for SampleSource {
    fn load(&self, id: &str, _reopen: bool) -> Result<ChoiceInfo, CoreError> {
        Ok(ChoiceInfo { id: id.into(), ..chooser_sample() })
    }
    fn confirm(&self, _id: &str, _files: &[bool]) -> Result<(), CoreError> {
        Ok(())
    }
    fn cancel(&self, _id: &str, _keep_file: bool) -> Option<CancelNote> {
        None
    }
    fn has_source(&self, id: &str) -> bool {
        id == "sample"
    }
    fn set_category(&self, _id: &str, _category: Category) {}
}
