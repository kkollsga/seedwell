//! The main window's state machines (SPEC 8.3), decided here for every host: which rows are selected and what a click,
//! ⌘-click, ⇧-click, ⌘A or an arrow key does to them; the inline Remove… question and when it goes; what each key
//! does; which place Get Info for a row shows; whether a new download takes the window or only flags Downloads, also
//! when it arrives while the window is closed; and the "Add a Download" sheet. The host maps its key presses and clicks to these calls,
//! runs the [`LibraryKeyAction`]s that touch what it still owns (the file lists under the rows), and draws the model.
//!
//! The state lives in [`LibraryState`] (library.rs), for the app's whole life: closing the window resets what the
//! window shows ([`Library::set_open`]) but keeps a new download that arrived meanwhile.

use std::collections::BTreeSet;

use crate::library::{Inputs, Library, LibraryPlace, LibraryState, LibraryViewModel};
use crate::removal::{removal_question, RemovalQuestion};
use crate::types::{Snapshot, TransferRow};

// MARK: The records

/// The Remove… question while it waits for its answer: under its row for one transfer, in the title bar for several.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PendingRemoval {
    pub question: RemovalQuestion,
    /// The group and the row it is drawn under (the last row of its transfers in the list); `None`: in the title bar.
    pub group_key: Option<String>,
    pub row_id: Option<String>,
}

/// "Add a Download" (File › Add Magnet Link or URL…): a sheet with one field.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AddLinkSheet {
    pub title: String,
    /// The field's placeholder.
    pub placeholder: String,
    pub choose_file_title: String,
    pub choose_file_help: String,
    pub cancel_title: String,
    pub cancel_help: String,
    /// The default button; enabled only while [`add_link_source`] accepts the text.
    pub confirm_title: String,
    pub confirm_help: String,
    /// What was typed in the field ([`Library::set_add_sheet_text`]): it stays while the sheet is open, whatever
    /// happens behind it (a new download, a click in the notifier), and goes when the sheet closes.
    pub text: String,
}

/// Bring this row into view (a new download, an arrow key). Each request has its own number; the host says when it is
/// done.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct ScrollTarget {
    pub row_id: String,
    pub request: u32,
    /// Centre the row once the list has settled (a new download: the page may have just changed). `false`: scroll at
    /// once and only as far as it takes to show the row (the selection moved with ↑ or ↓).
    pub center: bool,
}

/// A key the window handles. The host maps its key codes: Delete is Backspace or Forward Delete, Return is Return or
/// Enter, Up and Down the arrows, I and A the letters by what they type (⌘I is Get Info, ⌘A Select All; on Windows
/// Ctrl is [`KeyModifiers::command`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum LibraryKey {
    Delete,
    Return,
    Escape,
    Space,
    Left,
    Right,
    Up,
    Down,
    I,
    A,
}

/// The modifier keys held with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, uniffi::Record)]
pub struct KeyModifiers {
    pub shift: bool,
    pub command: bool,
    pub option: bool,
    pub control: bool,
}

/// Where the key was pressed: anywhere in the window (not in a text field: the host keeps those), or in the list while
/// it has the focus. The host asks for the window first and for the list only when the window did not handle it.
/// Edit › Select All asks as ⌘A in the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum KeyScope {
    Window,
    List,
}

/// What the host knows that the window's state does not (yet): its own inline edits and the file lists under the rows.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct WindowContext {
    /// An inline edit or a question of the host's own is open (a favorite tracker's rename, its removal question):
    /// a new download must not take the window from under it.
    pub editing: bool,
    /// The torrents whose file list is open under their row.
    pub open_lists: Vec<String>,
    /// The torrent whose file rows are selected (in its open list), and how many are.
    pub file_torrent: Option<String>,
    pub file_rows: u32,
}

/// What the host does for a key: the parts that touch the file lists it still owns. Everything else (the selection,
/// Get Info, the Remove… question) the library did already; the host asks for the model again.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum LibraryKeyAction {
    /// Space on selected file rows: check them, or skip them when they are all checked.
    ToggleFileRows,
    /// → on one selected file row: open it (a folder).
    OpenFileFolder,
    /// ← on one selected file row: close it (an open folder), else select the folder it is in; at the top the selection
    /// goes to its torrent's row ([`Library::set_selection`]).
    CloseFileFolder,
    /// → on a selected torrent with several files: its list opens.
    OpenFiles { torrent_id: String },
    /// ← on a selected torrent whose list is open: it closes.
    CloseFiles { torrent_id: String },
    /// Escape in the list, or ⌘A: the file rows are deselected (⌘A selected every torrent row instead).
    ClearFileSelection,
}

/// The answer to a key.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct KeyOutcome {
    /// The key did something: the host swallows it. Otherwise it passes it on (to the list, the system).
    pub handled: bool,
    pub action: Option<LibraryKeyAction>,
}

impl KeyOutcome {
    const IGNORED: Self = Self { handled: false, action: None };
    const HANDLED: Self = Self { handled: true, action: None };

    fn run(action: LibraryKeyAction) -> Self {
        Self { handled: true, action: Some(action) }
    }
}

/// What a new download did to the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum NewDownloadOutcome {
    /// The window went to Downloads (it was elsewhere, or on a tracker's Info) or stayed there, selected the row and asks
    /// to scroll to it; Get Info closed. The host deselects the file rows.
    Shown,
    /// The user is busy (Settings, an edit, the Remove… question, the "Add a Download" sheet): only Downloads in the
    /// sidebar carries a dot (not while Downloads is shown: the row is listed there already), and the sheet stays open
    /// with what was typed in it.
    Flagged,
    /// Nothing changed: the user ticked its files in its own open list and it stays in the tab shown, or the window
    /// opened with a request of its own, or the download is gone.
    Unchanged,
    /// The window is closed: its next plain opening goes to it ([`Library::take_arrival`]).
    Remembered,
}

// MARK: The texts

/// The "Add a Download" sheet, with what was typed in it.
fn add_link_sheet(text: &str) -> AddLinkSheet {
    AddLinkSheet {
        title: "Add a Download".into(),
        placeholder: "Magnet link or URL".into(),
        choose_file_title: "Choose .torrent File…".into(),
        choose_file_help: "Pick a .torrent file from disk instead".into(),
        cancel_title: "Cancel".into(),
        cancel_help: "Close without adding".into(),
        confirm_title: "Add".into(),
        confirm_help: "Add this magnet link or URL as a download".into(),
        text: text.to_owned(),
    }
}

/// A space or a tab, not a line break (what the sheet trims around the link).
fn is_blank(c: char) -> bool {
    matches!(c, '\t' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}')
}

/// What the add sheet adds for `text`: the link without the spaces around it; `None` (the Add button is disabled) when
/// nothing is left.
#[uniffi::export]
pub fn add_link_source(text: String) -> Option<String> {
    let t = text.trim_matches(is_blank);
    (!t.is_empty()).then(|| t.to_owned())
}

// MARK: The rules

/// The ids listed from position `a` to `b` (either way round).
fn span(order: &[(String, String)], a: usize, b: usize) -> BTreeSet<String> {
    order[a.min(b)..=a.max(b)].iter().map(|(_, i)| i.clone()).collect()
}

/// ⇧-click: the ids from the anchor's row to the clicked occurrence, in list order. `order` lists the rows on screen as
/// (group, id), so a torrent listed twice resolves to the occurrence clicked, and the anchor's occurrence nearest the
/// click is used (a duplicate in another group must not stretch the range). Without the anchor on screen, the clicked
/// row alone.
pub(crate) fn range(order: &[(String, String)], anchor: Option<&str>, group: &str, id: &str) -> BTreeSet<String> {
    let alone = || BTreeSet::from([id.to_owned()]);
    let Some(b) = order.iter().position(|(g, i)| g == group && i == id) else { return alone() };
    let Some(a) = order.iter().enumerate().filter(|(_, (_, i))| Some(i.as_str()) == anchor).map(|(n, _)| n).min_by_key(|&n| n.abs_diff(b)) else {
        return alone();
    };
    span(order, a, b)
}

/// Every download and queued torrent, the hidden ones too (a tracker's tab shows them).
fn all_downloads(s: &Snapshot) -> impl Iterator<Item = &TransferRow> {
    s.downloading.iter().chain(&s.hidden_downloading).chain(&s.queued).chain(&s.hidden_queued)
}

impl LibraryState {
    /// A click on a row: plain selects it alone, ⌘ adds or removes it, ⇧ selects the range from the anchor (the last
    /// row clicked plainly or with ⌘) when that is on screen.
    pub(crate) fn click(&mut self, group: &str, id: &str, command: bool, shift: bool) {
        if command {
            if !self.selection.remove(id) {
                self.selection.insert(id.to_owned());
            }
            self.anchor = Some(id.to_owned());
        } else if shift && self.anchor.as_ref().is_some_and(|a| self.listed.iter().any(|(_, i)| i == a)) {
            self.selection = range(&self.listed, self.anchor.as_deref(), group, id);
        } else {
            self.select_only(id);
        }
    }

    /// The row alone is selected, and the anchor of a ⇧-click range.
    fn select_only(&mut self, id: &str) {
        self.selection = BTreeSet::from([id.to_owned()]);
        self.anchor = Some(id.to_owned());
    }

    /// ⌘A: every row on screen, as listed (a folded group's rows are not); the first is the anchor of a range.
    fn select_all(&mut self) -> bool {
        let Some((_, first)) = self.listed.first() else { return false };
        self.anchor = Some(first.clone());
        self.selection = self.listed.iter().map(|(_, id)| id.clone()).collect();
        true
    }

    /// ↑ or ↓: the selection moves to the row before the first selected one or after the last, as listed (across
    /// groups); with nothing selected ↓ picks the first row and ↑ the last. With ⇧ the range from the anchor grows or
    /// shrinks at its moving end, as a ⇧-click there would (an anchor no longer on screen starts at the selection's
    /// other end). At either end of the list it goes no further. Returns the row moved to, to bring into view.
    fn step(&mut self, down: bool, shift: bool) -> Option<String> {
        let n = self.listed.len();
        let last = n.checked_sub(1)?;
        let selected: Vec<usize> = (0..n).filter(|&k| self.selection.contains(&self.listed[k].1)).collect();
        let next = |k: usize| if down { (k + 1).min(last) } else { k.saturating_sub(1) };
        let to = match (selected.first(), selected.last()) {
            (Some(&lo), Some(&hi)) if shift => {
                // The anchor's occurrence nearest the selection (a torrent listed twice must not stretch the range).
                let distance = |k: usize| if k < lo { lo - k } else { k.saturating_sub(hi) };
                let anchor = (0..n).filter(|&k| Some(self.listed[k].1.as_str()) == self.anchor.as_deref()).min_by_key(|&k| distance(k));
                let a = anchor.unwrap_or(if down { lo } else { hi });
                let end = if hi > a { hi } else if lo < a { lo } else { a };
                let to = next(end);
                self.anchor = Some(self.listed[a].1.clone());
                self.selection = span(&self.listed, a, to);
                return Some(self.listed[to].1.clone());
            }
            (Some(&lo), Some(&hi)) => next(if down { hi } else { lo }),
            _ if down => 0,
            _ => last,
        };
        let id = self.listed[to].1.clone();
        self.select_only(&id);
        Some(id)
    }

    /// Asks the host to bring `id` into view.
    fn scroll_to(&mut self, id: &str, center: bool) {
        self.scroll_count += 1;
        self.scroll = Some(ScrollTarget { row_id: id.to_owned(), request: self.scroll_count, center });
    }

    /// The place Get Info for `id` shows. On a tracker's tab, every torrent of that tracker stays there (whatever its
    /// group, folded or not). Elsewhere only a torrent that still downloads or waits is Downloads' own; one that just
    /// finished is listed in both Downloads ("Recently Finished") and Sharing, so the user stays where they are.
    pub(crate) fn info_place(&self, i: &Inputs, id: &str) -> LibraryPlace {
        let s = &i.snapshot;
        if let LibraryPlace::Tracker { tracker_id } = &self.place {
            let recent = s.recent.iter().chain(&s.hidden_recent);
            if all_downloads(s).chain(recent).chain(&i.tracker_seeds).any(|r| r.id == id && &r.tracker_id == tracker_id) {
                return self.place.clone();
            }
        }
        if s.downloading.iter().chain(&s.queued).any(|r| r.id == id) {
            return LibraryPlace::Downloads;
        }
        if s.recent.iter().any(|r| r.id == id) && self.place != LibraryPlace::Sharing {
            return LibraryPlace::Downloads;
        }
        LibraryPlace::Sharing
    }

    /// A download that arrives because its files were ticked in the window's own open list stays where it is listed
    /// already: Downloads, or its own tracker's tab (not its Info page).
    fn stays_in_place(&self, s: &Snapshot, id: &str, context: &WindowContext) -> bool {
        if self.tracker_info || !context.open_lists.iter().any(|x| x == id) {
            return false;
        }
        match &self.place {
            LibraryPlace::Downloads => true,
            LibraryPlace::Tracker { tracker_id } => all_downloads(s).any(|r| r.id == id && &r.tracker_id == tracker_id),
            _ => false,
        }
    }

    /// A torrent became a download (SPEC 8.3): the window goes to Downloads, selects its row and scrolls to it, with Get
    /// Info closed and the search cleared. Not while the user is in Settings or in the middle of something (an edit of
    /// the host's, the Remove… question, the add sheet): then only Downloads is flagged, unless it is shown (the dot
    /// would stay on the place the user is in: only going there clears it). A closed window remembers it (the newest)
    /// for its next opening.
    pub(crate) fn new_download(&mut self, s: &Snapshot, id: &str, context: &WindowContext) -> NewDownloadOutcome {
        if !self.open {
            self.arrival = Some(id.to_owned());
            return NewDownloadOutcome::Remembered;
        }
        if self.place == LibraryPlace::Settings || context.editing || self.removal.is_some() || self.add_sheet {
            if self.place != LibraryPlace::Downloads {
                self.downloads_flag = true;
            }
            return NewDownloadOutcome::Flagged;
        }
        if self.stays_in_place(s, id, context) {
            return NewDownloadOutcome::Unchanged;
        }
        self.search.clear();
        if self.place != LibraryPlace::Downloads || self.tracker_info {
            self.select(LibraryPlace::Downloads);
        } else {
            self.info = None;
        }
        self.select_only(id);
        self.scroll_to(id, true);
        NewDownloadOutcome::Shown
    }

    /// The user asked for this torrent (a click on its title in the notifier; the host brings the window forward): as a
    /// new download, except that Settings does not hold the window, since the user is the one leaving it. An open add
    /// sheet keeps the window and its text (the place behind it does not change either), as an edit or the Remove…
    /// question does: Downloads is flagged.
    pub(crate) fn show_torrent(&mut self, s: &Snapshot, id: &str, context: &WindowContext) -> NewDownloadOutcome {
        if self.open && !self.add_sheet && self.place == LibraryPlace::Settings {
            self.select(LibraryPlace::Downloads);
        }
        self.new_download(s, id, context)
    }

    /// The window opened: the download that arrived while it was closed is forgotten, and shown when the window was
    /// opened plainly (not on a place, a row or Settings) and it still downloads or waits.
    pub(crate) fn take_arrival(&mut self, s: &Snapshot, plain: bool, context: &WindowContext) -> NewDownloadOutcome {
        match self.arrival.take() {
            Some(id) if plain && s.downloading.iter().chain(&s.queued).any(|r| r.id == id) => self.new_download(s, &id, context),
            _ => NewDownloadOutcome::Unchanged,
        }
    }

    /// The question as the list can show it: only while one of its transfers is on screen (never remove what the user
    /// cannot see), under the last row of it for one transfer.
    pub(crate) fn pending_removal(&self, listed: &[(String, String)]) -> Option<PendingRemoval> {
        let q = self.removal.as_ref()?;
        let last = listed.iter().rev().find(|(_, id)| q.ids.contains(id))?;
        let (group_key, row_id) = if q.plural { (None, None) } else { (Some(last.0.clone()), Some(last.1.clone())) };
        Some(PendingRemoval { question: q.clone(), group_key, row_id })
    }

    /// After a model: the selection, the anchor and Get Info keep only rows on screen (a folded group, a search, a
    /// removal or another place took the others: Delete never acts on what cannot be seen), and the Remove… question
    /// goes when none of its rows is on screen any more (removed, folded away, on another page).
    pub(crate) fn settle(&mut self, m: &LibraryViewModel) {
        self.listed = m.list.iter().flat_map(|l| &l.groups).flat_map(|g| g.rows.iter().map(|r| (g.key.clone(), r.id.clone()))).collect();
        let on_screen: BTreeSet<&str> = self.listed.iter().map(|(_, id)| id.as_str()).collect();
        self.selection.retain(|id| on_screen.contains(id.as_str()));
        if self.anchor.as_deref().is_some_and(|a| !on_screen.contains(a)) {
            self.anchor = None;
        }
        if self.info.as_deref().is_some_and(|a| !on_screen.contains(a)) {
            self.info = None;
        }
        if m.removal.is_none() {
            self.removal = None;
        }
    }

    /// What `key` does now. `rows` are the selected transfers on screen, in list order (what Delete asks about).
    /// Returns the outcome, and the removal to carry out (Return on the question).
    pub(crate) fn key(
        &mut self,
        key: LibraryKey,
        mods: KeyModifiers,
        scope: KeyScope,
        context: &WindowContext,
        rows: impl FnOnce(&Self) -> Vec<TransferRow>,
    ) -> (KeyOutcome, Option<RemovalQuestion>) {
        if self.place == LibraryPlace::Settings {
            return (KeyOutcome::IGNORED, None);
        }
        let only_shift = !(mods.command || mods.option || mods.control);
        let only_command = mods == KeyModifiers { command: true, ..KeyModifiers::default() };
        let files_selected = context.file_torrent.is_some() && context.file_rows > 0;
        let one = |st: &Self| (st.selection.len() == 1).then(|| st.selection.iter().next().cloned()).flatten();
        let outcome = match (scope, key) {
            // Delete asks to remove the selected transfers, ⇧Delete to remove them and trash their files; not while the
            // question is open or file rows are selected.
            (KeyScope::Window, LibraryKey::Delete) => {
                if self.removal.is_some() || !only_shift || context.file_rows > 0 {
                    return (KeyOutcome::IGNORED, None);
                }
                let rows = rows(self);
                if rows.is_empty() {
                    return (KeyOutcome::IGNORED, None);
                }
                self.removal = Some(removal_question(rows, mods.shift));
                KeyOutcome::HANDLED
            }
            // Return answers the question, only while one of its rows is on screen; otherwise the question goes.
            (KeyScope::Window, LibraryKey::Return) => {
                let Some(q) = self.removal.take() else { return (KeyOutcome::IGNORED, None) };
                if mods.shift || !only_shift {
                    // With a modifier it is not the answer; the question stays.
                    self.removal = Some(q);
                    return (KeyOutcome::IGNORED, None);
                }
                if !self.listed.iter().any(|(_, id)| q.ids.contains(id)) {
                    return (KeyOutcome::IGNORED, None);
                }
                self.selection.clear();
                return (KeyOutcome::HANDLED, Some(q));
            }
            (KeyScope::Window, LibraryKey::Escape) => match self.removal.take() {
                Some(_) => KeyOutcome::HANDLED,
                None => KeyOutcome::IGNORED,
            },
            // ⌘I opens Get Info of the one selected torrent, as Space does (the context menu names it).
            (KeyScope::Window, LibraryKey::I) => match one(self) {
                Some(id) if only_command => {
                    self.info = Some(id);
                    KeyOutcome::HANDLED
                }
                _ => KeyOutcome::IGNORED,
            },
            // ⌘A (Edit › Select All) selects every row on screen; selected file rows are deselected, as a click on a
            // row does.
            (KeyScope::Window, LibraryKey::A) => {
                if !only_command || !self.select_all() {
                    KeyOutcome::IGNORED
                } else if context.file_torrent.is_some() || context.file_rows > 0 {
                    KeyOutcome::run(LibraryKeyAction::ClearFileSelection)
                } else {
                    KeyOutcome::HANDLED
                }
            }
            (KeyScope::Window, _) => KeyOutcome::IGNORED,
            // Space checks or skips the selected file rows, else opens Get Info of the one selected torrent.
            (KeyScope::List, LibraryKey::Space) => {
                if files_selected {
                    KeyOutcome::run(LibraryKeyAction::ToggleFileRows)
                } else if let Some(id) = one(self) {
                    self.info = Some(id);
                    KeyOutcome::HANDLED
                } else {
                    KeyOutcome::IGNORED
                }
            }
            // → opens the selected folder of a file list, else the file list of the one selected torrent.
            (KeyScope::List, LibraryKey::Right) => {
                if context.file_torrent.is_some() && context.file_rows == 1 {
                    KeyOutcome::run(LibraryKeyAction::OpenFileFolder)
                } else {
                    match one(self).and_then(|id| rows(self).into_iter().find(|r| r.id == id)) {
                        Some(r) if r.file_count > 1 && !context.open_lists.contains(&r.id) => KeyOutcome::run(LibraryKeyAction::OpenFiles { torrent_id: r.id }),
                        _ => KeyOutcome::IGNORED,
                    }
                }
            }
            // ← closes the selected folder of a file list (or climbs out of it), else the one selected torrent's list.
            (KeyScope::List, LibraryKey::Left) => {
                if context.file_torrent.is_some() {
                    if context.file_rows == 1 { KeyOutcome::run(LibraryKeyAction::CloseFileFolder) } else { KeyOutcome::IGNORED }
                } else {
                    match one(self) {
                        Some(id) if context.open_lists.contains(&id) => KeyOutcome::run(LibraryKeyAction::CloseFiles { torrent_id: id }),
                        _ => KeyOutcome::IGNORED,
                    }
                }
            }
            // ↑ and ↓ move the selection (⇧: extend it) and bring the row into view; not while file rows are selected.
            // An open Get Info follows the selection when it moved: to the one row selected, closed for several (⌘I
            // opens it only for one).
            (KeyScope::List, LibraryKey::Up | LibraryKey::Down) => {
                if !only_shift || files_selected {
                    return (KeyOutcome::IGNORED, None);
                }
                let before = self.selection.clone();
                match self.step(key == LibraryKey::Down, mods.shift) {
                    Some(id) => {
                        if self.info.is_some() && self.selection != before {
                            self.info = one(self);
                        }
                        self.scroll_to(&id, false);
                        KeyOutcome::HANDLED
                    }
                    None => KeyOutcome::IGNORED,
                }
            }
            // Escape in the list deselects everything, the file rows too.
            (KeyScope::List, LibraryKey::Escape) => {
                self.selection.clear();
                KeyOutcome::run(LibraryKeyAction::ClearFileSelection)
            }
            (KeyScope::List, _) => KeyOutcome::IGNORED,
        };
        (outcome, None)
    }
}

// MARK: The object

impl Library {
    /// The transfers of `ids` as the last model listed them, in list order and each once.
    fn listed_rows(&self, st: &LibraryState, ids: &BTreeSet<String>) -> Vec<TransferRow> {
        let Some(i) = self.last_inputs() else { return Vec::new() };
        let mut seen = BTreeSet::new();
        st.listed.iter().filter(|(_, id)| ids.contains(id) && seen.insert(id.as_str())).filter_map(|(_, id)| i.find(id).cloned()).collect()
    }

    /// The snapshot the window last showed (a fresh one before its first model).
    fn last_snapshot(&self) -> Snapshot {
        self.last_inputs().map_or_else(|| self.backend().snapshot(), |i| i.snapshot.clone())
    }
}

#[uniffi::export]
impl Library {
    /// A click on a row of the list (`group_key`: the group it is listed in): plain, with ⌘ or with ⇧.
    pub fn click_row(&self, group_key: String, row_id: String, command: bool, shift: bool) {
        self.with_state(|st| st.click(&group_key, &row_id, command, shift));
    }

    /// Selects exactly these rows (the anchor stays): `[]` when a file row is clicked, the torrent's row when ← climbs
    /// out of its file list.
    pub fn set_selection(&self, row_ids: Vec<String>) {
        self.with_state(|st| st.selection = row_ids.into_iter().collect());
    }

    /// The transfers a context menu on `row_id` acts on: the whole selection when the row is in it, else the row alone.
    pub fn menu_rows(&self, row_id: String) -> Vec<TransferRow> {
        let st = self.lock_state();
        let ids = if st.selection.contains(&row_id) && st.selection.len() > 1 { st.selection.clone() } else { BTreeSet::from([row_id]) };
        self.listed_rows(&st, &ids)
    }

    /// Get Info of a row opens (its popover), or closes with `None`.
    pub fn set_info(&self, row_id: Option<String>) {
        self.with_state(|st| st.info = row_id);
    }

    /// The popover of `row_id` was dismissed (another may have opened meanwhile: that one stays).
    pub fn info_closed(&self, row_id: String) {
        self.with_state(|st| {
            if st.info.as_deref() == Some(row_id.as_str()) {
                st.info = None;
            }
        });
    }

    /// Selects the row in the place its Get Info belongs to (usually the one shown): [`LibraryState::info_place`].
    pub fn reveal_row(&self, row_id: String) {
        let inputs = self.last_inputs();
        self.with_state(|st| {
            let target = match &inputs {
                Some(i) => st.info_place(i, &row_id),
                None => st.place.clone(),
            };
            if target != st.place {
                st.select(target);
            }
            st.select_only(&row_id);
        });
    }

    /// Remove… (`delete_files`: and Delete Files…) on these transfers: the question waits under the row, or in the title
    /// bar for several. A new question replaces the one open.
    pub fn ask_removal(&self, rows: Vec<TransferRow>, delete_files: bool) {
        if rows.is_empty() {
            return;
        }
        self.with_state(|st| st.removal = Some(removal_question(rows, delete_files)));
    }

    /// Cancel on the question.
    pub fn cancel_removal(&self) {
        self.with_state(|st| st.removal = None);
    }

    /// The question's Remove (or Delete) button: the transfers go, with their files when it asked so.
    pub fn confirm_removal(&self) {
        let Some(q) = self.with_state(|st| st.removal.take()) else { return };
        self.backend().remove(&q.ids, q.delete_files);
    }

    /// What `key` does: see [`KeyScope`] and [`LibraryKeyAction`]. The host asks for the model again afterwards.
    pub fn key(&self, key: LibraryKey, modifiers: KeyModifiers, scope: KeyScope, context: WindowContext) -> KeyOutcome {
        let (outcome, remove) = {
            let mut st = self.lock_state();
            st.key(key, modifiers, scope, &context, |st| self.listed_rows(st, &st.selection))
        };
        if let Some(q) = remove {
            self.backend().remove(&q.ids, q.delete_files);
        }
        outcome
    }

    /// A torrent became a download (the app's watcher): see [`NewDownloadOutcome`].
    pub fn new_download(&self, row_id: String, context: WindowContext) -> NewDownloadOutcome {
        let s = self.last_snapshot();
        self.with_state(|st| st.new_download(&s, &row_id, &context))
    }

    /// A click on the torrent's title in the notifier: see [`LibraryState::show_torrent`].
    pub fn show_torrent(&self, row_id: String, context: WindowContext) -> NewDownloadOutcome {
        let s = self.last_snapshot();
        self.with_state(|st| st.show_torrent(&s, &row_id, &context))
    }

    /// The window just opened (after the request it was opened with was carried out): the download that arrived while
    /// it was closed is shown when it was opened `plain`ly and still downloads; forgotten either way.
    pub fn take_arrival(&self, plain: bool, context: WindowContext) -> NewDownloadOutcome {
        if !self.with_state(|st| st.arrival.is_some()) {
            return NewDownloadOutcome::Unchanged;
        }
        let s = self.backend().snapshot();
        self.with_state(|st| st.take_arrival(&s, plain, &context))
    }

    /// The list brought [`LibraryViewModel::scroll_to`] into view.
    pub fn scroll_done(&self, request: u32) {
        self.with_state(|st| {
            if st.scroll.as_ref().is_some_and(|s| s.request == request) {
                st.scroll = None;
            }
        });
    }

    /// The window was opened (`true`) or closed. A closed window forgets what it showed: it opens again on Downloads,
    /// unfiltered, with nothing selected or asked (the folds and a download that arrived meanwhile are kept).
    pub fn set_open(&self, open: bool) {
        self.with_state(|st| {
            if !open {
                st.reset_window();
            }
            st.open = open;
        });
        if !open {
            self.forget_inputs();
        }
    }

    /// File › Add Magnet Link or URL…: the sheet opens ([`LibraryViewModel::add_sheet`]), empty (one open already
    /// keeps its text).
    pub fn show_add_sheet(&self) {
        self.with_state(|st| {
            if !st.add_sheet {
                st.add_text.clear();
            }
            st.add_sheet = true;
        });
    }

    /// The sheet closes (Cancel, Add, Choose .torrent File…); what was typed in it goes.
    pub fn close_add_sheet(&self) {
        self.with_state(|st| {
            st.add_sheet = false;
            st.add_text.clear();
        });
    }

    /// The text in the sheet's field changed (each keystroke, a paste); ignored while the sheet is closed.
    pub fn set_add_sheet_text(&self, text: String) {
        self.with_state(|st| {
            if st.add_sheet {
                st.add_text = text;
            }
        });
    }
}

/// The sheet's texts, while it is open.
pub(crate) fn add_sheet_of(st: &LibraryState) -> Option<AddLinkSheet> {
    st.add_sheet.then(|| add_link_sheet(&st.add_text))
}

#[cfg(test)]
#[path = "library_window_tests.rs"]
mod tests;
