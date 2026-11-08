//! The file lists of a host (SPEC 8.2, 8.3) as models every host draws (docs/CROSS-PLATFORM.md §1): the list under a
//! torrent's row in the window and the menu bar's inline checklist. Rust keeps which torrents have their list open,
//! each list's tree ([`crate::files`]), open folders, filter and the selection of file rows, the edits on their way
//! to the engine and when to read the files again; it gives the host every word, the rows and the commands of a
//! row's menu. The host reads the files off its main thread when told (`Core::files`, then [`FileLists::loaded`] or
//! [`FileLists::read`]), sends the edits after `flush_ms` of quiet ([`FileLists::take_edits`], `Core::
//! set_file_priorities`, [`FileLists::sent`]), and draws the [`FileListView`] or [`FileMenuView`].
//!
//! Edits show at once and go to the engine as one call after a quiet moment, so a run of clicks is one change. While
//! edits are on their way the list is not read again (the engine's answer would undo them on screen), and a list
//! closed meanwhile keeps its edits until they are sent.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use seedwell_platform::file_manager;

use crate::files::{priority_label, FileCheck, FileRefresh, FileRow, Tree};
use crate::host::HostWindow;
use crate::menu::APP_NAME;
use crate::row_commands::{play_commands, CommandEntry, PlayOffer, PlayerOption, RowCommand};
use crate::types::{FilePriority, FilePriorityChange, TorrentFile, TransferRow};

/// How often an open list is read again (progress, priorities someone else changed).
const REFRESH_MS: u32 = 1_000;
/// A read sooner than this after the last one is skipped (the host's timer is not exact).
const MIN_GAP_MS: u64 = 900;
/// The quiet moment after an edit before the edits go to the engine.
const FLUSH_MS: u32 = 250;
/// The window's list scrolls inside after this many rows (about 280 pt), unless Show All is on.
const WINDOW_ROWS: u32 = 13;
/// A long list gets a filter field.
const FILTER_FROM: u32 = 20;
/// The menu opens every folder of a torrent with at most this many files and folders.
const MENU_OPEN_LIMIT: u32 = 14;

/// Which list a [`FileLists`] keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FileListPlace {
    /// The lists under the rows of the window: any number open, a selection of file rows, a header.
    Window,
    /// The menu bar's inline checklist: one open at a time, no selection, the menu's rows and footer.
    Menu,
}

/// What the host reads next for a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FileListRead {
    Nothing,
    /// The whole list again (`loaded`): the engine refused the edits, show what it has.
    Reload,
    /// The numbers (`read`): the edits arrived.
    Read,
}

/// The edits of a list to send now (`Core::set_file_priorities`, then [`FileLists::sent`]).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileEdits {
    pub changes: Vec<FilePriorityChange>,
    /// Some files were ticked: a finished torrent may become a download again (SPEC 8.3), so the host notes the
    /// torrent as chosen again by the user (it is not announced as new).
    pub ticked: bool,
}

/// The selected file rows (in one list at a time).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileSelection {
    pub torrent_id: String,
    pub nodes: Vec<u32>,
}

/// ← on a selected file row.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileLeft {
    pub handled: bool,
    /// The selection moved up to the torrent's own row: the host selects that row.
    pub select_torrent: Option<String>,
}

/// A button of the list's header.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileListButton {
    pub title: String,
    pub help: String,
    pub enabled: bool,
}

/// The header's filter field (a long list's).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileFilterField {
    /// "Filter files".
    pub placeholder: String,
    pub text: String,
    /// The clear button (shown while there is text): its tooltip and screen reader name.
    pub clear_help: String,
    pub clear_label: String,
}

/// The line above the rows: "12 of 13 files · 14.2 GB of 15.1 GB selected", the filter, Show All, Select All and
/// Select None.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileListHeader {
    pub summary: String,
    pub summary_help: String,
    pub filter: Option<FileFilterField>,
    /// "Show All" / "Show Less" (`FileLists::view`'s `show_all` is the host's remembered choice); `None` while the
    /// list fits.
    pub show_all: Option<FileListButton>,
    /// `FileLists::select_all(torrent, true)`.
    pub select_all: FileListButton,
    /// `FileLists::select_all(torrent, false)`.
    pub select_none: FileListButton,
    pub file_count: u32,
    pub wanted_count: u32,
}

/// A priority in a row's priority pop-up.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FilePriorityChoice {
    pub priority: FilePriority,
    pub title: String,
    pub checked: bool,
}

/// A video's Play button: `FileLists::file`, then the host plays it.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FilePlayButton {
    /// "Play S01E02.mkv", its tooltip and screen reader name.
    pub help: String,
}

/// A row of the window's list.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct FileListRow {
    pub file: FileRow,
    pub selected: bool,
    /// A folder's chevron (`FileLists::toggle_folder`): its tooltip and screen reader name; empty for a file.
    pub fold_help: String,
    pub fold_label: String,
    /// The check (`FileLists::toggle_check`): its tooltip and what a screen reader says of it.
    pub check_help: String,
    pub check_value: String,
    /// The size capsule as a screen reader says it: "1.2 GB, 42 percent downloaded".
    pub size_label: String,
    /// The priority pop-up (`FileLists::set_priority`): High, Normal, Low, the row's own checked.
    pub priorities: Vec<FilePriorityChoice>,
    pub play: Option<FilePlayButton>,
}

/// The list under a torrent's row.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct FileListView {
    pub torrent_id: String,
    /// "Loading files…" or "Couldn't read the files.", shown instead of the list; `None` once it is here.
    pub status: Option<String>,
    pub header: Option<FileListHeader>,
    /// "No files." or "No files match." when there are no rows.
    pub empty: Option<String>,
    pub rows: Vec<FileListRow>,
    /// The list scrolls inside after this many rows; `None`: as tall as its rows (Show All).
    pub max_rows: Option<u32>,
    /// The priority pop-up's tooltip ("Priority") and the complete check mark's screen reader name ("Complete").
    pub priority_help: String,
    pub complete_label: String,
    /// How often the host asks [`FileLists::due`]; how long it waits after an edit before [`FileLists::take_edits`].
    pub refresh_ms: u32,
    pub flush_ms: u32,
}

/// The menu bar's checklist of one torrent.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct FileMenuView {
    pub torrent_id: String,
    /// At most `file_menu_cap()`; a click toggles the check (`FileLists::toggle_check`), the chevron the folder.
    pub rows: Vec<FileRow>,
    /// Some row is a video: every row keeps the Play column free.
    pub reserves_play: bool,
    /// "Show All in Seedwell… (12 more)": opens the window's list. `None` when every row fits.
    pub more: Option<String>,
    /// "8 of 13 files · 16 MB of 17 MB".
    pub footer: String,
    /// Rows the panel makes room for (it scrolls past about seven).
    pub height_rows: u32,
    pub flush_ms: u32,
}

/// What an entry of a file row's menu does. It acts on the rows the menu was opened on (the selection when the
/// row is in it, else the row).
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum FileCommand {
    /// Shows the file or folder in the file manager: `path` when it is on disk, else (`None`) the torrent's own place,
    /// as the row's Show in Finder.
    Reveal { path: Option<String> },
    /// `FileLists::set_priority` with the row's node.
    SetFilePriority { priority: FilePriority },
    /// Plays the file (`FileLists::file`); `player_id` a chosen player (Play With), `None` the setting's.
    PlayFile { node: u32, player_id: Option<String> },
}

/// One line of a file row's menu.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum FileMenuEntry {
    Item { title: String, command: FileCommand },
    /// Informs only ("No Player Installed"): drawn disabled.
    Note { title: String },
    Submenu { title: String, entries: Vec<FileMenuEntry> },
    Separator,
}

/// The disclosure triangle of a torrent row: its tooltip and screen reader name.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FileListToggle {
    pub help: String,
    pub label: String,
}

/// The triangle that opens (`expanded` false) or closes a torrent's file list.
#[uniffi::export]
pub fn file_list_toggle(expanded: bool) -> FileListToggle {
    if expanded {
        FileListToggle { help: "Hide the files".into(), label: "Hide files".into() }
    } else {
        FileListToggle { help: "Show the files".into(), label: "Show files".into() }
    }
}

/// "Show All" / "Show Less" of a list that grows to its full height instead of scrolling inside (`on`: grown).
#[uniffi::export]
pub fn file_list_show_all(on: bool) -> FileListButton {
    if on {
        FileListButton { title: "Show Less".into(), help: "Scroll long file lists inside their row again".into(), enabled: true }
    } else {
        FileListButton {
            title: "Show All".into(),
            help: "Grow long file lists to their full height instead of scrolling inside".into(),
            enabled: true,
        }
    }
}

#[derive(Default)]
struct List {
    tree: Option<Tree>,
    failed: bool,
    open: HashSet<usize>,
    filter: String,
    /// Edits made here and not sent yet, by file index.
    pending: BTreeMap<u32, FilePriority>,
    /// Sends the engine has not answered.
    inflight: u32,
    reading: bool,
    last_read_ms: Option<u64>,
}

impl List {
    fn busy(&self) -> bool {
        !self.pending.is_empty() || self.inflight > 0
    }

    fn rows(&self) -> Vec<FileRow> {
        self.tree.as_ref().map(|t| t.rows(&self.open, &self.filter)).unwrap_or_default()
    }

    fn edit(&mut self, changes: Vec<FilePriorityChange>) -> bool {
        let Some(t) = self.tree.as_mut() else { return false };
        if changes.is_empty() {
            return false;
        }
        t.apply(&changes);
        for c in changes {
            self.pending.insert(c.index, c.priority);
        }
        true
    }
}

struct Sel {
    torrent: String,
    nodes: BTreeSet<u32>,
    anchor: Option<u32>,
}

#[derive(Default)]
struct Lists {
    /// Torrents with their list open, in the order they were opened.
    expanded: Vec<String>,
    lists: HashMap<String, List>,
    sel: Option<Sel>,
}

impl Lists {
    fn selected(&self, torrent: &str, node: u32) -> bool {
        self.sel.as_ref().is_some_and(|s| s.torrent == torrent && s.nodes.contains(&node))
    }

    /// The rows an action on `node` covers: the whole selection when `node` is in it, else just `node`.
    fn targets(&self, torrent: &str, node: u32) -> Vec<usize> {
        match &self.sel {
            Some(s) if s.torrent == torrent && s.nodes.contains(&node) => s.nodes.iter().map(|&n| n as usize).collect(),
            _ => vec![node as usize],
        }
    }

    fn list(&mut self, torrent: &str) -> Option<&mut List> {
        self.lists.get_mut(torrent)
    }

    fn clear_selection_of(&mut self, torrent: &str) {
        if self.sel.as_ref().is_some_and(|s| s.torrent == torrent) {
            self.sel = None;
        }
    }

    /// Drops a closed list, unless edits are still on their way (it goes once they are sent).
    fn drop_closed(&mut self, torrent: &str) {
        if !self.expanded.iter().any(|e| e == torrent) && !self.lists.get(torrent).is_some_and(List::busy) {
            self.lists.remove(torrent);
        }
    }
}

fn check_texts(c: FileCheck) -> (&'static str, &'static str) {
    match c {
        FileCheck::On => ("Skip this; the file stays on disk if already downloaded", "Downloading"),
        FileCheck::Mixed => ("Download this", "Some files"),
        FileCheck::Off => ("Download this", "Skipped"),
    }
}

fn size_label(f: &FileRow) -> String {
    if f.fill >= 1.0 {
        format!("{}, downloaded", f.size)
    } else if f.fill > 0.0 {
        format!("{}, {} percent downloaded", f.size, (f.fill * 100.0).floor() as u32)
    } else {
        f.size.clone()
    }
}

const REVEAL: &str = concat!("Show in ", file_manager!());

fn file_entries(entries: Vec<CommandEntry>, node: u32) -> Vec<FileMenuEntry> {
    entries
        .into_iter()
        .filter_map(|e| match e {
            CommandEntry::Item { command: RowCommand::Play { player_id, .. }, title, .. } => {
                Some(FileMenuEntry::Item { title, command: FileCommand::PlayFile { node, player_id } })
            }
            CommandEntry::Item { .. } => None,
            CommandEntry::Note { title } => Some(FileMenuEntry::Note { title }),
            CommandEntry::Submenu { title, entries } => Some(FileMenuEntry::Submenu { title, entries: file_entries(entries, node) }),
            CommandEntry::Separator => Some(FileMenuEntry::Separator),
        })
        .collect()
}

/// The open file lists of one place. Every method takes the lists' own lock, and none waits for the engine.
#[derive(uniffi::Object)]
pub struct FileLists {
    place: FileListPlace,
    state: Mutex<Lists>,
    /// Whether the host plays videos (`HostWindow::Player`): a file row's Play button and the menu's room for it come
    /// only then. All hosts can, until [`FileLists::set_host_windows`] says otherwise.
    plays: std::sync::atomic::AtomicBool,
}

impl FileLists {
    fn lock(&self) -> MutexGuard<'_, Lists> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn edit(&self, torrent: &str, make: impl FnOnce(&Lists, &Tree) -> Vec<FilePriorityChange>) -> bool {
        let mut s = self.lock();
        let changes = match s.lists.get(torrent).and_then(|l| l.tree.as_ref()) {
            Some(t) => make(&s, t),
            None => return false,
        };
        s.list(torrent).is_some_and(|l| l.edit(changes))
    }
}

#[uniffi::export]
impl FileLists {
    #[uniffi::constructor]
    pub fn new(place: FileListPlace) -> Arc<Self> {
        Arc::new(FileLists { place, state: Mutex::new(Lists::default()), plays: std::sync::atomic::AtomicBool::new(true) })
    }

    /// The windows this host has (the Library's `set_host_windows`, once): without `HostWindow::Player` a video's row
    /// has no Play button and the menu bar's checklist keeps no room for one.
    pub fn set_host_windows(&self, host: Vec<HostWindow>) {
        self.plays.store(host.contains(&HostWindow::Player), std::sync::atomic::Ordering::Relaxed);
    }

    // MARK: Open and closed

    /// Opens `torrent`'s list. `true`: the host reads its files now (`Core::files` off the main thread, then
    /// `loaded`). The menu keeps one list open: opening one closes the other.
    pub fn expand(&self, torrent: String) -> bool {
        let mut s = self.lock();
        if self.place == FileListPlace::Menu {
            let others: Vec<String> = s.expanded.iter().filter(|e| **e != torrent).cloned().collect();
            s.expanded.retain(|e| *e == torrent);
            for o in others {
                s.drop_closed(&o);
            }
        }
        if !s.expanded.contains(&torrent) {
            s.expanded.push(torrent.clone());
        }
        let fresh = !s.lists.contains_key(&torrent);
        let list = s.lists.entry(torrent).or_default();
        // A list kept for its edits has its tree: only its numbers are read again.
        fresh || list.tree.is_none()
    }

    /// Closes `torrent`'s list; its file rows give up the selection.
    pub fn collapse(&self, torrent: String) {
        let mut s = self.lock();
        s.expanded.retain(|e| *e != torrent);
        s.clear_selection_of(&torrent);
        s.drop_closed(&torrent);
    }

    /// The disclosure triangle: `true` when the host reads the files now.
    pub fn toggle(&self, torrent: String) -> bool {
        if self.is_expanded(torrent.clone()) {
            self.collapse(torrent);
            false
        } else {
            self.expand(torrent)
        }
    }

    /// Another tab or page: every list closes.
    pub fn collapse_all(&self) {
        let mut s = self.lock();
        let all = std::mem::take(&mut s.expanded);
        s.sel = None;
        for t in all {
            s.drop_closed(&t);
        }
    }

    pub fn is_expanded(&self, torrent: String) -> bool {
        self.lock().expanded.contains(&torrent)
    }

    /// The torrents whose list is open, in the order they were opened.
    pub fn expanded(&self) -> Vec<String> {
        self.lock().expanded.clone()
    }

    /// The window closed: the lists are forgotten (their edits still go out); which are open is remembered.
    pub fn suspend(&self) {
        let mut s = self.lock();
        s.sel = None;
        s.lists.retain(|_, l| l.busy());
    }

    /// The window opened again: the lists to read now.
    pub fn resume(&self) -> Vec<String> {
        let mut s = self.lock();
        let mut read = Vec::new();
        for t in s.expanded.clone() {
            let l = s.lists.entry(t.clone()).or_default();
            if l.tree.is_none() {
                read.push(t);
            }
        }
        read
    }

    // MARK: Reading the files

    /// The files of `torrent` as the engine lists them (`None`: they could not be read). Builds the tree outside the
    /// lock (thousands of files take a moment to sort): call it off the main thread. A list closed meanwhile ignores
    /// it.
    pub fn loaded(&self, torrent: String, files: Option<Vec<TorrentFile>>, now_ms: u64) {
        let built = files.map(|f| Tree::new(&f, now_ms));
        let menu = self.place == FileListPlace::Menu;
        let mut s = self.lock();
        let Some(l) = s.list(&torrent) else { return };
        l.reading = false;
        let Some(t) = built else {
            l.failed = true;
            return;
        };
        l.failed = false;
        // Folders open as they start the first time, and again when the list changed shape.
        if l.tree.as_ref().is_none_or(|old| old.nodes.len() != t.nodes.len()) {
            l.open = if menu { t.menu_initially_open(MENU_OPEN_LIMIT) } else { t.initially_expanded() }.into_iter().collect();
        }
        l.tree = Some(t);
        l.last_read_ms = Some(now_ms);
    }

    /// The lists to read again now (`Core::files`, then `read`): open, loaded, no edits on their way, and not read
    /// in the last second. They count as being read until `read`.
    pub fn due(&self, now_ms: u64) -> Vec<String> {
        let mut s = self.lock();
        let mut out = Vec::new();
        for (t, l) in &mut s.lists {
            let fresh = l.last_read_ms.is_some_and(|r| now_ms.saturating_sub(r) < MIN_GAP_MS);
            if l.tree.is_some() && !l.reading && !l.busy() && !fresh {
                l.reading = true;
                l.last_read_ms = Some(now_ms);
                out.push(t.clone());
            }
        }
        out.sort();
        out
    }

    /// New numbers for a list being read (`None`: the read failed). `true` when a row changed: draw it again. Ignored
    /// while edits are on their way.
    pub fn read(&self, torrent: String, files: Option<Vec<TorrentFile>>, now_ms: u64) -> bool {
        let mut s = self.lock();
        let Some(l) = s.list(&torrent) else { return false };
        l.reading = false;
        let (Some(files), Some(t)) = (files, l.tree.as_mut()) else { return false };
        if !l.pending.is_empty() || l.inflight > 0 {
            return false;
        }
        match t.refresh(&files, false, now_ms) {
            FileRefresh::Same => false,
            FileRefresh::Changed => true,
            FileRefresh::Rebuild => {
                // Files came or went: build it again.
                *t = Tree::new(&files, now_ms);
                true
            }
        }
    }

    // MARK: Folders and the filter

    pub fn set_filter(&self, torrent: String, text: String) {
        if let Some(l) = self.lock().list(&torrent) {
            l.filter = text;
        }
    }

    pub fn toggle_folder(&self, torrent: String, node: u32) {
        let mut s = self.lock();
        if let Some(l) = s.list(&torrent) {
            let n = node as usize;
            if !l.open.remove(&n) {
                l.open.insert(n);
            }
        }
    }

    pub fn set_folder(&self, torrent: String, node: u32, open: bool) {
        let mut s = self.lock();
        if let Some(l) = s.list(&torrent).filter(|l| l.tree.is_some()) {
            if open {
                l.open.insert(node as usize);
            } else {
                l.open.remove(&(node as usize));
            }
        }
    }

    /// Every folder open (QA).
    pub fn open_all_folders(&self, torrent: String) {
        let mut s = self.lock();
        if let Some(l) = s.list(&torrent)
            && let Some(t) = &l.tree
        {
            l.open = t.folder_ids().into_iter().collect();
        }
    }

    // MARK: Selection (the window)

    /// A click on a file or folder row (⌘: add or remove it, ⇧: the range from the last click). The host deselects
    /// the torrent rows.
    pub fn click(&self, torrent: String, node: u32, command: bool, shift: bool) {
        let mut s = self.lock();
        let Some(l) = s.lists.get(&torrent) else { return };
        let rows: Vec<u32> = l.rows().iter().map(|r| r.id).collect();
        if s.sel.as_ref().is_none_or(|x| x.torrent != torrent) {
            s.sel = Some(Sel { torrent, nodes: BTreeSet::new(), anchor: None });
        }
        let Some(sel) = s.sel.as_mut() else { return };
        let range = sel.anchor.and_then(|a| Some((rows.iter().position(|&r| r == a)?, rows.iter().position(|&r| r == node)?)));
        match range {
            Some((a, b)) if shift => sel.nodes = rows[a.min(b)..=a.max(b)].iter().copied().collect(),
            _ if command => {
                if !sel.nodes.remove(&node) {
                    sel.nodes.insert(node);
                }
                sel.anchor = Some(node);
            }
            _ => {
                sel.nodes = BTreeSet::from([node]);
                sel.anchor = Some(node);
            }
        }
    }

    pub fn clear_selection(&self) {
        self.lock().sel = None;
    }

    pub fn selection(&self) -> Option<FileSelection> {
        self.lock().sel.as_ref().map(|s| FileSelection { torrent_id: s.torrent.clone(), nodes: s.nodes.iter().copied().collect() })
    }

    /// Space: checks what is unchecked among the selected rows, else skips them. The torrent edited, if any (the host
    /// sends its edits after `flush_ms`).
    pub fn space(&self) -> Option<String> {
        let (torrent, nodes) = {
            let s = self.lock();
            let sel = s.sel.as_ref().filter(|x| !x.nodes.is_empty())?;
            (sel.torrent.clone(), sel.nodes.iter().map(|&n| n as usize).collect::<Vec<_>>())
        };
        self.edit(&torrent, |_, t| t.toggle_changes(&nodes)).then_some(torrent)
    }

    /// → on the selected row: opens a folder.
    pub fn right(&self) -> bool {
        let mut s = self.lock();
        let Some((torrent, node)) = s.sel.as_ref().filter(|x| x.nodes.len() == 1).map(|x| (x.torrent.clone(), *x.nodes.first().unwrap_or(&0)))
        else {
            return false;
        };
        match s.list(&torrent).filter(|l| l.tree.is_some()) {
            Some(l) => {
                l.open.insert(node as usize);
                true
            }
            None => false,
        }
    }

    /// ← on the selected row: closes an open folder, else selects the folder it is in, or gives the selection to the
    /// torrent's own row.
    pub fn left(&self) -> FileLeft {
        let none = FileLeft { handled: false, select_torrent: None };
        let mut s = self.lock();
        let Some((torrent, node)) = s.sel.as_ref().filter(|x| x.nodes.len() == 1).map(|x| (x.torrent.clone(), *x.nodes.first().unwrap_or(&0)))
        else {
            return none;
        };
        let Some(l) = s.list(&torrent) else { return none };
        let Some(row) = l.rows().into_iter().find(|r| r.id == node) else { return none };
        if row.is_folder && l.open.contains(&(node as usize)) {
            l.open.remove(&(node as usize));
            return FileLeft { handled: true, select_torrent: None };
        }
        if row.parent > 0 {
            s.sel = Some(Sel { torrent, nodes: BTreeSet::from([row.parent]), anchor: Some(row.parent) });
            return FileLeft { handled: true, select_torrent: None };
        }
        s.sel = None;
        FileLeft { handled: true, select_torrent: Some(torrent) }
    }

    // MARK: Edits

    /// The check of a row (of the selection, when the row is in it): what is unchecked gets checked, else skipped.
    /// `true` when something changed (the host draws again and sends after `flush_ms`).
    pub fn toggle_check(&self, torrent: String, node: u32) -> bool {
        self.edit(&torrent, |s, t| t.toggle_changes(&s.targets(&torrent, node)))
    }

    /// The priority pop-up and the menu's Priority: the wanted files below change (all of them when none is).
    pub fn set_priority(&self, torrent: String, node: u32, priority: FilePriority) -> bool {
        self.edit(&torrent, |s, t| t.priority_changes(&s.targets(&torrent, node), priority))
    }

    /// Select All (`on`) / Select None: what the filter shows, everything without one.
    pub fn select_all(&self, torrent: String, on: bool) -> bool {
        self.edit(&torrent, |s, t| {
            let l = &s.lists[&torrent];
            // With a filter, the files it shows: not the folders above them, which hold files it hides.
            let nodes: Vec<usize> = if l.filter.trim().is_empty() {
                t.top().to_vec()
            } else {
                l.rows().iter().filter(|r| !r.is_folder).map(|r| r.id as usize).collect()
            };
            t.want_changes(&nodes, on)
        })
    }

    /// The edits to send now, if any: they count as on their way until `sent`.
    pub fn take_edits(&self, torrent: String) -> Option<FileEdits> {
        let mut s = self.lock();
        let l = s.list(&torrent).filter(|l| !l.pending.is_empty())?;
        let changes: Vec<FilePriorityChange> = std::mem::take(&mut l.pending).into_iter().map(|(index, priority)| FilePriorityChange { index, priority }).collect();
        l.inflight += 1;
        let ticked = changes.iter().any(|c| c.priority != FilePriority::Skip);
        Some(FileEdits { changes, ticked })
    }

    /// The engine answered a send (`ok`: it took the edits). What to read next: everything again when it refused,
    /// the numbers once nothing more is on its way.
    pub fn sent(&self, torrent: String, ok: bool) -> FileListRead {
        let mut s = self.lock();
        let Some(l) = s.list(&torrent) else { return FileListRead::Nothing };
        l.inflight = l.inflight.saturating_sub(1);
        let next = if !ok {
            l.pending.clear();
            l.reading = true;
            FileListRead::Reload
        } else if !l.busy() {
            l.reading = true;
            FileListRead::Read
        } else {
            FileListRead::Nothing
        };
        if !s.expanded.contains(&torrent) {
            if let Some(l) = s.list(&torrent) {
                l.reading = false;
            }
            s.drop_closed(&torrent);
            return FileListRead::Nothing;
        }
        next
    }

    /// Edits of the list not answered yet.
    pub fn has_pending(&self, torrent: String) -> bool {
        self.lock().lists.get(&torrent).is_some_and(List::busy)
    }

    // MARK: What the host draws

    /// The file a row is, as the engine listed it (Play); `None` for a folder.
    pub fn file(&self, torrent: String, node: u32) -> Option<TorrentFile> {
        self.lock().lists.get(&torrent)?.tree.as_ref()?.torrent_file(node as usize)
    }

    /// The first row, every folder open, whose name holds `text` (case aside): QA's stand-in for a click.
    pub fn find(&self, torrent: String, text: String) -> Option<u32> {
        let s = self.lock();
        let t = s.lists.get(&torrent)?.tree.as_ref()?;
        let open: HashSet<usize> = t.folder_ids().into_iter().collect();
        let needle = text.to_lowercase();
        t.rows(&open, "").into_iter().find(|r| r.name.to_lowercase().contains(&needle)).map(|r| r.id)
    }

    /// The list under `torrent`'s row; `show_all` is the host's remembered Show All. `None` when it is not open.
    pub fn view(&self, torrent: String, show_all: bool) -> Option<FileListView> {
        let plays = self.plays.load(std::sync::atomic::Ordering::Relaxed);
        let s = self.lock();
        let l = s.lists.get(&torrent).filter(|_| s.expanded.contains(&torrent))?;
        let mut v = FileListView {
            torrent_id: torrent.clone(),
            status: None,
            header: None,
            empty: None,
            rows: Vec::new(),
            max_rows: Some(WINDOW_ROWS),
            priority_help: "Priority".into(),
            complete_label: "Complete".into(),
            refresh_ms: REFRESH_MS,
            flush_ms: FLUSH_MS,
        };
        let Some(t) = &l.tree else {
            v.status = Some(if l.failed { "Couldn't read the files." } else { "Loading files…" }.into());
            return Some(v);
        };
        let rows = l.rows();
        let totals = t.totals();
        let filtering = !l.filter.is_empty();
        let overflows = rows.len() as u32 > WINDOW_ROWS;
        v.header = Some(FileListHeader {
            summary: totals.summary.clone(),
            summary_help: "Skipped files stay on disk".into(),
            filter: (totals.file_count > FILTER_FROM).then(|| FileFilterField {
                placeholder: "Filter files".into(),
                text: l.filter.clone(),
                clear_help: "Clear the file filter".into(),
                clear_label: "Clear filter".into(),
            }),
            show_all: (overflows || show_all).then(|| file_list_show_all(show_all)),
            select_all: FileListButton {
                title: "Select All".into(),
                help: "Download every file".into(),
                enabled: !(totals.wanted_count == totals.file_count && !filtering),
            },
            select_none: FileListButton {
                title: "Select None".into(),
                help: "Skip every file; skipped files stay on disk".into(),
                enabled: !(totals.wanted_count == 0 && !filtering),
            },
            file_count: totals.file_count,
            wanted_count: totals.wanted_count,
        });
        if rows.is_empty() {
            v.empty = Some(if filtering { "No files match." } else { "No files." }.into());
        }
        if show_all && !rows.is_empty() {
            v.max_rows = None;
        }
        v.rows = rows
            .into_iter()
            .map(|f| {
                let (check_help, check_value) = check_texts(f.check);
                let (fold_help, fold_label) = match (f.is_folder, f.is_open) {
                    (false, _) => ("", ""),
                    (true, true) => ("Collapse this folder", "Collapse"),
                    (true, false) => ("Expand this folder", "Expand"),
                };
                FileListRow {
                    selected: s.selected(&torrent, f.id),
                    fold_help: fold_help.into(),
                    fold_label: fold_label.into(),
                    check_help: check_help.into(),
                    check_value: check_value.into(),
                    size_label: size_label(&f),
                    priorities: [FilePriority::High, FilePriority::Normal, FilePriority::Low]
                        .into_iter()
                        .map(|p| FilePriorityChoice { priority: p, title: priority_label(Some(p)), checked: f.priority == Some(p) })
                        .collect(),
                    play: (plays && f.is_video && !f.is_folder).then(|| FilePlayButton { help: format!("Play {}", f.name) }),
                    file: f,
                }
            })
            .collect();
        Some(v)
    }

    /// The menu of a row of `torrent`'s list (right-click): Show in Finder, Play and Play With for a video (`players`:
    /// what the host found; only with `HostWindow::Player`), Priority. Empty when the list is not open.
    pub fn menu(&self, torrent: String, node: u32, row: TransferRow, players: Vec<PlayerOption>, host: Vec<HostWindow>) -> Vec<FileMenuEntry> {
        let s = self.lock();
        let Some(t) = s.lists.get(&torrent).and_then(|l| l.tree.as_ref()) else { return Vec::new() };
        let Some(f) = t.row_of(node as usize) else { return Vec::new() };
        let path = t.disk_location(node as usize).filter(|p| std::path::Path::new(p).exists());
        let mut out = vec![FileMenuEntry::Item { title: REVEAL.into(), command: FileCommand::Reveal { path } }];
        if f.is_video
            && host.contains(&HostWindow::Player)
            && let Some(file) = t.torrent_file(node as usize)
        {
            out.extend(file_entries(play_commands(row, PlayOffer { videos: vec![file], players }), node));
        }
        out.push(FileMenuEntry::Separator);
        let p = |priority| FileMenuEntry::Item { title: priority_label(Some(priority)), command: FileCommand::SetFilePriority { priority } };
        let skip = FileMenuEntry::Item { title: "Skip".into(), command: FileCommand::SetFilePriority { priority: FilePriority::Skip } };
        out.push(FileMenuEntry::Submenu {
            title: "Priority".into(),
            entries: vec![p(FilePriority::High), p(FilePriority::Normal), p(FilePriority::Low), FileMenuEntry::Separator, skip],
        });
        out
    }

    /// The menu bar's checklist of `torrent` (a `Menu` place's), `None` until it is loaded.
    pub fn menu_view(&self, torrent: String) -> Option<FileMenuView> {
        let s = self.lock();
        let l = s.lists.get(&torrent)?;
        let t = l.tree.as_ref()?;
        let cap = crate::files::MENU_CAP;
        let m = t.menu_rows(&l.open, cap);
        Some(FileMenuView {
            torrent_id: torrent,
            reserves_play: self.plays.load(std::sync::atomic::Ordering::Relaxed) && m.rows.iter().any(|r| r.is_video),
            more: (m.hidden > 0).then(|| format!("Show All in {APP_NAME}… ({} more)", m.hidden)),
            rows: m.rows,
            footer: t.totals().menu_footer,
            height_rows: (t.nodes.len() as u32 - 1).min(cap + 1),
            flush_ms: FLUSH_MS,
        })
    }
}

#[cfg(test)]
#[path = "file_list_tests.rs"]
mod tests;
