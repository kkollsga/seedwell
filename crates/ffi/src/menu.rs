//! The menu bar item (SPEC 8.1, 8.2) as view models every host draws: the Mac's NSStatusItem menu, the Windows tray
//! flyout. Rust decides (docs/CROSS-PLATFORM.md §1): which sections and rows there are, their order and caps, every
//! text, each row's mark, check and click, when the menu is rebuilt rather than updated (`signature`), and the icon's
//! look, accessibility label and refresh cadence. The host only draws and runs the commands.
//!
//! - [`Arrivals`]: which torrents just became downloads (the window switches to them, the menu lists them first).
//! - [`menu_model`]: the open menu.
//! - [`IconState`]: the icon (and the move bar's grace period, the one thing it remembers).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use trs_control::display as d;

use crate::row_text::{
    category_label, download_waiting, fraction, is_held, pending_remaining_ms, percent, progress_done, progress_size,
    shows_play, starting_line,
};
use crate::sync::{SyncSnapshot, SyncState};
use crate::types::{Snapshot, TransferKind, TransferRow, TransferState};

pub(crate) const APP_NAME: &str = "Seedwell";
/// Queued rows shown before "+ N more".
const QUEUED_SHOWN: usize = 3;
/// Recent rows shown.
const RECENT_SHOWN: usize = 3;
/// At most this many queued arrivals are lifted into Downloading.
const ARRIVED_QUEUED_SHOWN: usize = 3;
/// Longest name a Downloading row shows before the middle is cut (the name gives way, never the numbers under it).
const TITLE_LIMIT: usize = 44;
/// How long a file re-selection can explain a torrent turning into a download.
const RESELECT_WINDOW_MS: u64 = 60_000;
/// A move must still run after this long before the icon shows the move bar (a same-volume move is a rename of
/// about 30 ms; a bar for it would only flicker).
const MOVE_GRACE_MS: u64 = 400;
/// The open menu is refreshed this often.
const MENU_REFRESH_MS: u32 = 1_000;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// MARK: Arrivals

/// Which torrents just became downloads (SPEC 8.2, 8.3). Feed it every snapshot; it answers the ids that are new
/// since the last one.
///
/// A new download is a torrent that is not held (no countdown, no file choice), is going to download (downloading,
/// fetching metadata, or waiting in the queue) and was not seen as a download before. A torrent held at first sight
/// becomes an arrival when it is released (Start Now, OK, the countdown running out); one cancelled while held never
/// does. The first snapshot is the baseline: whatever was downloading when the app started is not new. Paused,
/// checking or failed rows are not arrivals until they actually download.
///
/// A torrent that was shared before (it seeded for a while) and downloads again is an arrival only when the user
/// re-selected files for it ([`Arrivals::user_reselected`], within a minute). One a recheck at launch found pieces
/// missing in comes back as a download quietly: nothing takes the window.
#[derive(uniffi::Object, Default)]
pub struct Arrivals {
    state: Mutex<ArrivalsState>,
}

#[derive(Default)]
struct ArrivalsState {
    /// Order of first sight, for the menu (newest first). Rows of the baseline share rank 0.
    rank: HashMap<String, u64>,
    /// Ids that already were arrivals (or were there at the start); they never switch again.
    started: HashSet<String>,
    /// Arrivals of this run, oldest first, while they are still downloads or queued.
    arrived: Vec<String>,
    primed: bool,
    sequence: u64,
    /// Torrents the user just changed the wanted files of, with the time (Unix ms).
    reselections: HashMap<String, u64>,
    /// Shared-before torrents that turned into downloads without the user asking (not arrivals, so far).
    quiet: HashSet<String>,
}

impl ArrivalsState {
    /// The rows that can arrive: downloads that are going to move data.
    fn is_eligible(r: &TransferRow) -> bool {
        !is_held(r) && matches!(r.state, TransferState::Downloading | TransferState::Metadata | TransferState::Queued)
    }

    /// A torrent that finished and seeded before: its next download is not a fresh add.
    fn was_shared(r: &TransferRow) -> bool {
        r.kind == TransferKind::Torrent && r.seed_secs > 0
    }

    fn user_reselected(&mut self, id: &str, now_ms: u64) -> bool {
        if self.quiet.remove(id) {
            self.arrived.push(id.to_owned());
            return true;
        }
        self.reselections.insert(id.to_owned(), now_ms);
        false
    }

    fn is_reselected(&mut self, id: &str, now_ms: u64) -> bool {
        self.reselections.retain(|_, at| now_ms.saturating_sub(*at) <= RESELECT_WINDOW_MS);
        self.reselections.remove(id).is_some()
    }

    fn observe(&mut self, s: &Snapshot, now_ms: u64) -> Vec<String> {
        let rows: Vec<&TransferRow> = s.downloading.iter().chain(&s.queued).collect();
        let present: HashSet<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        if !self.primed {
            self.primed = true;
            self.started = present.iter().map(|&id| id.to_owned()).collect();
            for r in &rows {
                self.rank.insert(r.id.clone(), 0);
            }
            return Vec::new();
        }
        // Gone (finished, removed): adding it again later is new again.
        self.started.retain(|id| present.contains(id.as_str()));
        self.rank.retain(|id, _| present.contains(id.as_str()));
        self.arrived.retain(|id| present.contains(id.as_str()));
        self.quiet.retain(|id| present.contains(id.as_str()));
        let mut fresh = Vec::new();
        for r in rows {
            if !self.rank.contains_key(&r.id) {
                self.sequence += 1;
                self.rank.insert(r.id.clone(), self.sequence);
            }
            if Self::is_eligible(r) && self.started.insert(r.id.clone()) {
                if Self::was_shared(r) && !self.is_reselected(&r.id, now_ms) {
                    self.quiet.insert(r.id.clone());
                    continue;
                }
                fresh.push(r.id.clone());
                self.arrived.push(r.id.clone());
            }
        }
        fresh
    }

    fn rank(&self, id: &str) -> u64 {
        self.rank.get(id).copied().unwrap_or(0)
    }
}

#[uniffi::export]
impl Arrivals {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The new downloads of this snapshot, oldest first. The first call is the baseline and returns none.
    pub fn observe(&self, snapshot: Snapshot, now_unix_ms: u64) -> Vec<String> {
        lock(&self.state).observe(&snapshot, now_unix_ms)
    }

    /// The user changed which files of `id` are wanted (ticked, Files…, the chooser). True when the torrent already
    /// turned into a download a moment ago, so it is an arrival after all.
    pub fn user_reselected(&self, id: String, now_unix_ms: u64) -> bool {
        lock(&self.state).user_reselected(&id, now_unix_ms)
    }

    /// Order of first sight (higher = newer; 0 for the baseline and unknown ids).
    pub fn rank(&self, id: String) -> u64 {
        lock(&self.state).rank(&id)
    }

    /// Arrivals of this run, oldest first, while they are still downloads or queued.
    pub fn arrived(&self) -> Vec<String> {
        lock(&self.state).arrived.clone()
    }
}

// MARK: The menu

/// The sections of the menu, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MenuSectionKind {
    Downloading,
    Queued,
    Recent,
}

/// The image at the left of a row.
#[derive(Clone, Copy, Debug, PartialEq, uniffi::Enum)]
pub enum MenuMark {
    /// A progress ring, 0...1 (the host draws at least a sliver).
    Ring { fraction: f64 },
    /// An orange dot: it waits for the user.
    Attention,
    /// A green check: finished.
    Done,
}

/// What a click on the row (outside its controls) does. The menu closes first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum MenuClick {
    /// Shows it in the file manager (Recent).
    Reveal,
    /// Opens the chooser of what to download (a torrent waiting for that choice).
    OpenChooser,
}

/// One torrent in the menu.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct MenuRow {
    pub id: String,
    /// Already cut in the middle where the menu cuts it.
    pub title: String,
    /// The grey line under the title; empty: none.
    pub subtitle: String,
    pub mark: Option<MenuMark>,
    /// The checkbox: ticked = running, empty = paused; `None` = the row has none.
    pub running: Option<bool>,
    /// The checkbox pauses and resumes the start countdown (`Core::pending_pause`/`pending_resume`), not the torrent.
    pub countdown: bool,
    /// Drawn in the secondary colour (a queued row).
    pub dimmed: bool,
    pub click: Option<MenuClick>,
    /// A chevron at the left opens the torrent's file list under the row.
    pub expands: bool,
    /// A Play icon at the right edge.
    pub playable: bool,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct MenuSection {
    pub kind: MenuSectionKind,
    /// The section header ("Downloading").
    pub title: String,
    /// A separator goes above the header.
    pub separator_before: bool,
    pub rows: Vec<MenuRow>,
    /// "+ 2 more" under the rows (a click shows the downloads in the window); `None`: all are listed.
    pub more: Option<String>,
}

/// "Sharing 5 torrents · 1 error" with the combined upload rate at the right edge. A click shows Sharing in the
/// window.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MenuSharingLine {
    pub title: String,
    /// "↑ 340 KB/s", or a bare "–" while nothing uploads.
    pub rate: String,
    /// The rate is drawn in the label colour (data flows), else in the secondary one.
    pub rate_active: bool,
    pub separator_before: bool,
}

/// The Folder Sync line; its submenu lists the folders (the sync view model's business).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct MenuSyncLine {
    /// "Folder Sync".
    pub title: String,
    /// "3 folders · Up to date", "Syncing 2 · 12 MB/s", "1 conflict", "1 folder missing".
    pub subtitle: String,
    /// Something in it needs the user (missing, can't sync, conflicts). The Mac draws it no differently (yet).
    pub needs_attention: bool,
    pub separator_before: bool,
}

/// The whole menu, top to bottom: the Offline line, the sections, Sharing, Folder Sync, then (always after a
/// separator) the Update line and the footer (Open on the left, Quit on the right).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct MenuModel {
    /// "Offline", a quiet line on top while the network is out and no Sharing line carries it.
    pub offline: Option<String>,
    pub sections: Vec<MenuSection>,
    pub sharing: Option<MenuSharingLine>,
    pub folder_sync: Option<MenuSyncLine>,
    /// "Update to Seedwell 0.2.0" while an update waits; the host reads it when it rebuilds the menu.
    pub update: Option<String>,
    /// "Open Seedwell…" (shortcut L).
    pub open_title: String,
    /// "Quit" (shortcut Q).
    pub quit_title: String,
    /// How often the open menu asks for a new model.
    pub refresh_ms: u32,
    /// When it differs from the last one the menu is rebuilt; otherwise the rows' subtitles, marks and checks, the
    /// Sharing line and the Folder Sync subtitle are updated in place (the highlight stays put).
    pub signature: String,
}

/// The menu for `snapshot`, with the torrents `arrivals` says just arrived listed first. `update_version`: the
/// version an update waits with (the host's updater knows it).
#[uniffi::export]
pub fn menu_model(
    snapshot: Snapshot,
    sync: SyncSnapshot,
    arrivals: Arc<Arrivals>,
    now_unix_ms: u64,
    update_version: Option<String>,
) -> MenuModel {
    let a = lock(&arrivals.state);
    build(&snapshot, &sync, &a, now_unix_ms, update_version)
}

/// The Downloading and Queued sections (SPEC 8.2). A torrent that just arrived is listed first in Downloading,
/// newest first, even while it waits for a free slot: the user must see what they just added without looking under
/// Queued. Held torrents (waiting for the user's choice, then counting down) stay ahead of it, in the engine's order.
fn layout<'a>(s: &'a Snapshot, a: &ArrivalsState) -> (Vec<&'a TransferRow>, Vec<&'a TransferRow>) {
    let mut lifted: Vec<&TransferRow> =
        s.queued.iter().filter(|r| r.state == TransferState::Queued && a.arrived.contains(&r.id)).collect();
    lifted.sort_by_key(|r| std::cmp::Reverse(a.rank(&r.id)));
    lifted.truncate(ARRIVED_QUEUED_SHOWN);
    let tier = |r: &TransferRow| if r.awaiting_choice { 0 } else if r.pending.is_some() { 1 } else { 2 };
    let mut all: Vec<(usize, &TransferRow)> = s.downloading.iter().chain(lifted.iter().copied()).enumerate().collect();
    // The engine's order inside a tier; newest arrival first among the others.
    all.sort_by_key(|&(i, r)| {
        let t = tier(r);
        (t, std::cmp::Reverse(if t == 2 { a.rank(&r.id) } else { 0 }), i)
    });
    let downloading = all.into_iter().map(|(_, r)| r).collect();
    let queued = s.queued.iter().filter(|r| !lifted.iter().any(|l| l.id == r.id)).collect();
    (downloading, queued)
}

/// The check of a row: running = downloading or queued, off = paused. A held row's countdown pauses too.
fn is_running(r: &TransferRow) -> bool {
    match &r.pending {
        Some(p) => !p.paused,
        None => r.state != TransferState::Paused,
    }
}

/// The torrent has a file list worth opening under its row.
fn expands(r: &TransferRow) -> bool {
    r.kind == TransferKind::Torrent && r.file_count > 1 && r.pending.is_none() && !r.awaiting_choice
}

/// The grey line of a downloading row: "42% · 3.1 MB/s · 4 min left". With no speed (or no estimate yet) the
/// engine's reason replaces the time ("42% · Waiting for peers"), never a stale one.
fn menu_subtitle(r: &TransferRow, now_ms: u64) -> String {
    let p = percent(r);
    match r.state {
        TransferState::Paused => format!("Paused · {p}%"),
        TransferState::Queued => "Up next · waiting for a free slot".into(),
        TransferState::Downloading => {
            if r.down_rate > 0
                && let Some(eta) = r.eta_secs
            {
                return format!("{p}% · {} · {}", d::rate(r.down_rate), d::remaining(Some(eta), true));
            }
            if r.down_rate == 0
                && let Some(w) = download_waiting(r, now_ms)
            {
                return format!("{p}% · {}{}", w.note, w.tail);
            }
            let rate = if r.down_rate > 0 { format!("{} · ", d::rate(r.down_rate)) } else { String::new() };
            format!("{p}% · {rate}{}", d::remaining(None, true))
        }
        _ => format!("{p}% · {}", d::remaining(r.eta_secs, true)),
    }
}

/// "Movie · 12 min ago".
fn recent_subtitle(r: &TransferRow, now_ms: u64) -> String {
    let mut parts = vec![category_label(r.category)];
    if let Some(at) = r.finished_at {
        parts.push(d::ago(at, now_ms / 1000, true));
    }
    parts.join(" · ")
}

fn ring(f: f64) -> Option<MenuMark> {
    Some(MenuMark::Ring { fraction: f })
}

/// A row of the Downloading section.
fn downloading_row(r: &TransferRow, now_ms: u64) -> MenuRow {
    let mut row = MenuRow {
        id: r.id.clone(),
        title: d::middle_truncated(&r.display_name, TITLE_LIMIT),
        subtitle: String::new(),
        mark: None,
        running: Some(is_running(r)),
        countdown: r.pending.is_some(),
        dimmed: false,
        click: None,
        expands: expands(r),
        playable: shows_play(r),
    };
    if r.awaiting_choice {
        // Waits for the user (SPEC §7A.6): no check, the whole name, and a click opens the chooser.
        row.title = r.display_name.clone();
        row.subtitle = "Choose what to download…".into();
        row.mark = Some(MenuMark::Attention);
        row.running = None;
        row.click = Some(MenuClick::OpenChooser);
    } else if let Some(p) = &r.pending {
        row.subtitle = starting_line(pending_remaining_ms(p, now_ms), p.paused);
        row.mark = ring(0.0);
    } else {
        row.subtitle = menu_subtitle(r, now_ms);
        row.mark = ring(if r.state == TransferState::Queued { 0.0 } else { fraction(r) });
    }
    row
}

/// A row of the Queued section: the whole name, dimmed, no grey line.
fn queued_row(r: &TransferRow) -> MenuRow {
    let mark = if r.pending.is_some() || r.state == TransferState::Queued {
        ring(0.0)
    } else if r.state == TransferState::Downloading || r.state == TransferState::Paused && progress_done(r) > 0 {
        ring(fraction(r))
    } else {
        None
    };
    MenuRow {
        id: r.id.clone(),
        title: r.display_name.clone(),
        subtitle: String::new(),
        mark,
        running: Some(is_running(r)),
        countdown: r.pending.is_some(),
        dimmed: true,
        click: None,
        expands: expands(r),
        playable: shows_play(r),
    }
}

/// A row of the Recent section: a check mark, and a click shows it in the file manager. Only a row with a file
/// list has the row's controls (the chevron, and Play).
fn recent_row(r: &TransferRow, now_ms: u64) -> MenuRow {
    let expands = expands(r);
    MenuRow {
        id: r.id.clone(),
        title: r.display_name.clone(),
        subtitle: recent_subtitle(r, now_ms),
        mark: Some(MenuMark::Done),
        running: None,
        countdown: false,
        dimmed: false,
        click: Some(MenuClick::Reveal),
        expands,
        playable: expands && shows_play(r),
    }
}

/// "Sharing 2 torrents", plus "· Offline" and "· 1 error" (only problems that need the user count), and the
/// combined upload rate at the right edge: "↑ 14 KB/s", or a bare "–" while nothing uploads.
fn sharing_line(s: &Snapshot, separator_before: bool) -> MenuSharingLine {
    let mut title = format!("Sharing {}", d::count(u64::from(s.sharing_total), "torrent"));
    if !s.online {
        title.push_str(" · Offline");
    }
    if s.error_count > 0 {
        title.push_str(&format!(" · {} error{}", s.error_count, if s.error_count == 1 { "" } else { "s" }));
    }
    MenuSharingLine {
        title,
        rate: d::live_rate(if s.online { s.up_rate } else { 0 }, "↑"),
        rate_active: s.online && s.up_rate > 0,
        separator_before,
    }
}

/// The Folder Sync line is in the menu only while there are synced folders (the user, 2026-10-26: folders are added
/// in the window).
fn shows_sync(s: &SyncSnapshot) -> bool {
    !s.folders.is_empty()
}

/// The menu's Folder Sync line: "3 folders · Up to date", "Syncing 2 · 12 MB/s", "1 conflict", "1 folder missing".
/// The thing that needs the user wins.
pub(crate) fn sync_summary_of(s: &SyncSnapshot) -> String {
    if !s.running && s.folders.is_empty() {
        return if s.problem.is_none() { "Starting…" } else { "Not running" }.into();
    }
    let f = &s.folders;
    if f.is_empty() {
        return "No folders".into();
    }
    let n = |pred: &dyn Fn(SyncState) -> bool| f.iter().filter(|x| pred(x.state)).count() as u64;
    let missing = n(&|st| st == SyncState::Missing);
    if missing > 0 {
        return format!("{} missing", d::count(missing, "folder"));
    }
    let errors = n(&|st| st == SyncState::Error);
    if errors > 0 {
        return format!("{} can't sync", d::count(errors, "folder"));
    }
    let conflicts: u64 = f.iter().map(|x| u64::from(x.conflicts)).sum();
    if conflicts > 0 {
        return d::count(conflicts, "conflict");
    }
    let syncing: Vec<_> = f.iter().filter(|x| x.state == SyncState::Syncing).collect();
    if !syncing.is_empty() {
        let rate: u64 = syncing.iter().map(|x| x.rate_in).sum();
        let tail = if rate > 0 { format!(" · {}", d::rate(rate)) } else { String::new() };
        return format!("Syncing {}{tail}", syncing.len());
    }
    let all = d::count(f.len() as u64, "folder");
    if f.iter().all(|x| x.state == SyncState::Paused) {
        return format!("{all} · Paused");
    }
    if f.iter().any(|x| matches!(x.state, SyncState::WaitingForDevices | SyncState::WaitingForAdmin)) {
        return format!("{all} · Waiting for devices");
    }
    if f.iter().any(|x| matches!(x.state, SyncState::Starting | SyncState::Scanning)) {
        return format!("{all} · Scanning…");
    }
    format!("{all} · Up to date")
}

/// Folder sync in one line (the menu's Folder Sync line, the window's sync page).
#[uniffi::export]
pub fn sync_summary(snapshot: SyncSnapshot) -> String {
    sync_summary_of(&snapshot)
}

/// A folder needs the user: missing, can't sync, or conflict copies.
fn sync_needs_attention(s: &SyncSnapshot) -> bool {
    s.folders.iter().any(|f| matches!(f.state, SyncState::Missing | SyncState::Error) || f.conflicts > 0)
}

/// What decides the menu's structure: the rows (and the state that picks how each is built), the shown queued rows
/// and their count, the shown recent rows, whether there is a Sharing line and its errors, the network, the rows
/// that wait for a choice, and whether there is a Folder Sync line (its folders are the submenu's own business).
fn signature(s: &Snapshot, down: &[&TransferRow], queued: &[&TransferRow], sync: &SyncSnapshot) -> String {
    let d: Vec<String> = down
        .iter()
        .map(|r| format!("{}{:?}{}{}", r.id, r.state, r.pending.is_some(), r.pending.is_some_and(|p| p.paused)))
        .collect();
    let mut q: Vec<String> = queued.iter().take(QUEUED_SHOWN).map(|r| r.id.clone()).collect();
    q.push(queued.len().to_string());
    let r: Vec<String> = s.recent.iter().take(RECENT_SHOWN).map(|r| r.id.clone()).collect();
    let a = [(s.sharing_total > 0).to_string(), s.error_count.to_string(), s.online.to_string()];
    let c: Vec<String> = s.downloading.iter().filter(|r| r.awaiting_choice).map(|r| r.id.clone()).collect();
    let parts = [d.join(","), q.join(","), r.join(","), a.join(","), c.join(",")];
    format!("{}|{}", parts.join("|"), if shows_sync(sync) { "sync" } else { "" })
}

fn build(s: &Snapshot, sync: &SyncSnapshot, a: &ArrivalsState, now_ms: u64, update_version: Option<String>) -> MenuModel {
    let (down, queued) = layout(s, a);
    // The Sharing line carries "Offline" when there is one; otherwise a single quiet line.
    let offline = (!s.online && s.sharing_total == 0).then(|| "Offline".to_string());
    let mut above = offline.is_some();
    let mut sections = Vec::new();
    if !down.is_empty() {
        sections.push(MenuSection {
            kind: MenuSectionKind::Downloading,
            title: "Downloading".into(),
            separator_before: false,
            rows: down.iter().map(|r| downloading_row(r, now_ms)).collect(),
            more: None,
        });
        above = true;
    }
    if !queued.is_empty() {
        let more = queued.len().saturating_sub(QUEUED_SHOWN);
        sections.push(MenuSection {
            kind: MenuSectionKind::Queued,
            title: "Queued".into(),
            separator_before: false,
            rows: queued.iter().take(QUEUED_SHOWN).map(|r| queued_row(r)).collect(),
            more: (more > 0).then(|| format!("+ {more} more")),
        });
        above = true;
    }
    if !s.recent.is_empty() {
        sections.push(MenuSection {
            kind: MenuSectionKind::Recent,
            title: "Recent".into(),
            separator_before: above,
            rows: s.recent.iter().take(RECENT_SHOWN).map(|r| recent_row(r, now_ms)).collect(),
            more: None,
        });
        above = true;
    }
    let sharing = (s.sharing_total > 0).then(|| sharing_line(s, above));
    above |= sharing.is_some();
    let folder_sync = shows_sync(sync).then(|| MenuSyncLine {
        title: "Folder Sync".into(),
        subtitle: sync_summary_of(sync),
        needs_attention: sync_needs_attention(sync),
        separator_before: sharing.is_none() && above,
    });
    MenuModel {
        offline,
        sections,
        sharing,
        folder_sync,
        update: update_version.map(|v| format!("Update to {APP_NAME} {v}")),
        open_title: format!("Open {APP_NAME}…"),
        quit_title: "Quit".into(),
        refresh_ms: MENU_REFRESH_MS,
        signature: signature(s, &down, &queued, sync),
    }
}

// MARK: The icon

/// How strongly an arrow is coloured for a transfer rate: 0 below 5 KB/s, then logarithmic up to 1 at 10 MB/s, in 8
/// steps (the icon is only redrawn when a step changes).
fn intensity(bytes_per_sec: u64) -> f64 {
    if bytes_per_sec < 5_000 {
        return 0.0;
    }
    let t = (bytes_per_sec as f64 / 5_000.0).log10() / (10_000_000.0_f64 / 5_000.0).log10();
    (t.clamp(0.0, 1.0) * 8.0).ceil() / 8.0
}

/// What the icon shows, quantised so that equal looks compare equal: the host redraws only when this changes.
#[derive(Clone, Copy, Debug, PartialEq, uniffi::Record)]
pub struct IconLook {
    /// How coloured the down arrow (green) and the up arrow (red) are, 0...1 in eighths.
    pub down: f64,
    pub up: f64,
    /// Total download progress, in steps of 2 % (a faint hairline); `None`: nothing downloads.
    pub progress: Option<f64>,
    /// Aggregate move progress (a green bar instead of the hairline) once a move has run long enough to show.
    pub moving: Option<f64>,
    /// A small orange dot: something waits for the user.
    pub attention: bool,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct IconModel {
    pub look: IconLook,
    /// "Seedwell, downloading 3.1 MB/s, uploading 340 KB/s, needs attention".
    pub accessibility_label: String,
    /// Ask again after this long (events ask sooner): twice a second while a move is watched, every second while the
    /// menu is open, every 5 s while anything moves or an arrow is still coloured, else every 30 s (errors, dead
    /// torrents and uploads start without an event).
    pub next_refresh_ms: u32,
    /// How late the timer may fire (lets the system coalesce wake-ups).
    pub refresh_leeway_ms: u32,
}

/// The menu bar icon. It remembers when the current move started (the move bar shows only for a move still
/// running after 0.4 s).
#[derive(uniffi::Object, Default)]
pub struct IconState {
    move_since_ms: Mutex<Option<u64>>,
}

/// The move bar: `percent` is the snapshot's aggregate move progress (`None` = nothing moves). Returns the fraction
/// to draw (0...1 in steps of 2 %), or `None` while nothing is shown.
fn move_fraction(since: &mut Option<u64>, percent: Option<u8>, now_ms: u64) -> Option<f64> {
    let Some(percent) = percent else {
        *since = None;
        return None;
    };
    let start = *since.get_or_insert(now_ms);
    if now_ms.saturating_sub(start) < MOVE_GRACE_MS {
        return None;
    }
    Some((f64::from(percent.min(100)) / 100.0 * 50.0).round() / 50.0)
}

#[uniffi::export]
impl IconState {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// The icon for `snapshot` at `now_unix_ms`; `menu_open` while the menu is shown.
    pub fn icon(&self, snapshot: Snapshot, now_unix_ms: u64, menu_open: bool) -> IconModel {
        let s = &snapshot;
        let active: Vec<&TransferRow> = s.downloading.iter().filter(|r| !is_held(r)).collect();
        let total: u64 = active.iter().map(|r| progress_size(r)).sum();
        let done: u64 = active.iter().map(|r| progress_done(r)).sum();
        let downloading = !active.is_empty();
        let mut since = lock(&self.move_since_ms);
        let look = IconLook {
            down: intensity(s.down_rate),
            up: intensity(s.up_rate),
            progress: downloading.then(|| (done as f64 / total.max(1) as f64 * 50.0).round() / 50.0),
            moving: move_fraction(&mut since, s.moving_percent, now_unix_ms),
            attention: s.attention,
        };
        let mut label = APP_NAME.to_string();
        if s.down_rate > 0 {
            label.push_str(&format!(", downloading {}", d::rate(s.down_rate)));
        } else if downloading {
            label.push_str(", downloading");
        }
        if s.up_rate > 0 {
            label.push_str(&format!(", uploading {}", d::rate(s.up_rate)));
        }
        if s.attention {
            label.push_str(", needs attention");
        }
        // A finished download's rate fades over a few seconds after its last block: the colours follow it.
        let moves = downloading || s.up_rate > 0 || s.down_rate > 0 || look.down > 0.0 || look.up > 0.0;
        let (next_refresh_ms, refresh_leeway_ms) = if since.is_some() {
            (500, 100)
        } else if menu_open {
            (MENU_REFRESH_MS, 100)
        } else if moves {
            (5_000, 2_000)
        } else {
            (30_000, 15_000)
        };
        IconModel { look, accessibility_label: label, next_refresh_ms, refresh_leeway_ms }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_text::tests::row;
    use crate::sync::{SyncAccess, SyncFolder, SyncRole};
    use crate::types::{Category, PendingStart, SharingCounts};

    fn held(id: &str) -> TransferRow {
        let mut r = row(id, TransferState::Downloading);
        r.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 7_000, paused: false });
        r
    }

    fn choice(id: &str) -> TransferRow {
        let mut r = row(id, TransferState::Downloading);
        r.awaiting_choice = true;
        r
    }

    fn snap(down: Vec<TransferRow>, queued: Vec<TransferRow>) -> Snapshot {
        Snapshot {
            down_rate: 0,
            up_rate: 0,
            downloading: down,
            queued,
            recent: Vec::new(),
            hidden_downloading: Vec::new(),
            hidden_queued: Vec::new(),
            hidden_recent: Vec::new(),
            sharing_busiest: Vec::new(),
            sharing: SharingCounts { popular: 0, active: 1, dormant: 0, error: 0, dead: 0, stopped: 0 },
            sharing_total: 1,
            trackers: Vec::new(),
            alt_speed: false,
            down_limit: None,
            up_limit: None,
            error_count: 0,
            moving_percent: None,
            attention: false,
            listen_port: 0,
            connections: 0,
            online: true,
            offline_since_ms: None,
        }
    }

    fn down(rows: Vec<TransferRow>) -> Snapshot {
        snap(rows, Vec::new())
    }

    fn empty() -> Snapshot {
        snap(Vec::new(), Vec::new())
    }

    fn layout_ids(s: &Snapshot, a: &ArrivalsState) -> (Vec<String>, Vec<String>) {
        let (d, q) = layout(s, a);
        (d.iter().map(|r| r.id.clone()).collect(), q.iter().map(|r| r.id.clone()).collect())
    }

    // MARK: Which torrents count as a new download

    #[test]
    fn the_first_snapshot_is_the_baseline() {
        let mut a = ArrivalsState::default();
        let s = snap(vec![row("old", TransferState::Downloading)], vec![row("q", TransferState::Queued)]);
        assert!(a.observe(&s, 0).is_empty());
        assert!(a.observe(&s, 0).is_empty());
    }

    #[test]
    fn a_torrent_added_without_a_hold_is_new_once() {
        let mut a = ArrivalsState::default();
        a.observe(&down(vec![row("old", TransferState::Downloading)]), 0);
        let s = down(vec![row("old", TransferState::Downloading), row("new", TransferState::Downloading)]);
        assert_eq!(a.observe(&s, 0), ["new"]);
        assert!(a.observe(&s, 0).is_empty(), "not on every refresh");
    }

    #[test]
    fn a_held_torrent_arrives_when_released() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        assert!(a.observe(&down(vec![held("n")]), 0).is_empty(), "countdown");
        assert!(a.observe(&down(vec![held("n")]), 0).is_empty());
        assert_eq!(a.observe(&down(vec![row("n", TransferState::Downloading)]), 0), ["n"], "Start Now / expiry");
    }

    #[test]
    fn a_choice_then_ok_arrives() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        assert!(a.observe(&down(vec![choice("c")]), 0).is_empty());
        assert_eq!(a.observe(&down(vec![row("c", TransferState::Downloading)]), 0), ["c"]);
    }

    #[test]
    fn a_cancelled_held_torrent_never_arrives() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        a.observe(&down(vec![held("n")]), 0);
        assert!(a.observe(&empty(), 0).is_empty());
    }

    #[test]
    fn a_queued_torrent_counts_but_paused_and_checking_do_not() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        let queued = || vec![row("q", TransferState::Queued), row("p", TransferState::Paused)];
        assert_eq!(a.observe(&snap(vec![row("chk", TransferState::Checking)], queued()), 0), ["q"]);
        assert_eq!(a.observe(&snap(vec![row("chk", TransferState::Downloading)], queued()), 0), ["chk"]);
    }

    #[test]
    fn a_torrent_that_left_and_comes_back_is_new_again() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        a.observe(&down(vec![row("x", TransferState::Downloading)]), 0);
        a.observe(&empty(), 0);
        assert_eq!(a.observe(&down(vec![row("x", TransferState::Downloading)]), 0), ["x"]);
    }

    #[test]
    fn several_at_once_come_oldest_first_in_list_order() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        let s = down(vec![row("a", TransferState::Downloading), row("b", TransferState::Downloading)]);
        assert_eq!(a.observe(&s, 0), ["a", "b"]);
    }

    /// "Harbor Watch — S01–S02": S01 finished and seeded 9 hours, S02 ticked: 42%, 3.1 MB/s, 4 minutes.
    fn again() -> TransferRow {
        let mut r = row("t", TransferState::Downloading);
        r.seed_secs = 9 * 3600;
        r.size_bytes = 10_000_000_000;
        r.done_bytes = 4_200_000_000;
        r.down_rate = 3_100_000;
        r.eta_secs = Some(240);
        r
    }

    #[test]
    fn a_reselected_torrent_is_an_arrival() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 1_000_000);
        assert!(!a.user_reselected("t", 1_000_000), "the user ticks S02 ...");
        assert_eq!(a.observe(&down(vec![again()]), 1_001_000), ["t"], "... and the row turns into a download");
        assert_eq!(a.arrived, ["t"]);
    }

    #[test]
    fn a_snapshot_that_was_faster_than_the_mark_still_arrives() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        assert!(a.observe(&down(vec![again()]), 0).is_empty(), "seen first, nobody asked yet");
        assert!(a.user_reselected("t", 0), "the mark arrives late");
        assert_eq!(a.arrived, ["t"]);
        assert!(a.observe(&down(vec![again()]), 0).is_empty(), "and only once");
    }

    #[test]
    fn a_recheck_at_launch_does_not_take_the_window() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        assert!(a.observe(&down(vec![again()]), 0).is_empty(), "pieces were missing: quietly a download");
        assert!(a.arrived.is_empty());
        // It is still listed under Downloading in the menu.
        assert_eq!(layout_ids(&down(vec![again()]), &a).0, ["t"]);
    }

    #[test]
    fn a_mark_older_than_a_minute_is_forgotten() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 1_000_000);
        a.user_reselected("t", 1_000_000);
        assert!(a.observe(&down(vec![again()]), 1_120_000).is_empty());
    }

    #[test]
    fn a_fresh_add_still_arrives_without_a_mark() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        assert_eq!(a.observe(&down(vec![row("new", TransferState::Downloading)]), 0), ["new"]);
    }

    #[test]
    fn the_object_answers_like_the_state() {
        let a = Arrivals::new();
        assert!(a.observe(empty(), 0).is_empty());
        assert_eq!(a.observe(down(vec![row("n", TransferState::Downloading)]), 0), ["n"]);
        assert_eq!(a.arrived(), ["n"]);
        assert_eq!(a.rank("n".into()), 1);
        assert_eq!(a.rank("unknown".into()), 0);
        assert!(!a.user_reselected("x".into(), 0));
    }

    // MARK: The menu's order

    #[test]
    fn the_newest_arrival_is_first_in_the_menu() {
        let mut a = ArrivalsState::default();
        let old = || vec![row("old1", TransferState::Downloading), row("old2", TransferState::Downloading)];
        a.observe(&down(old()), 0);
        let mut all = old();
        all.push(row("new", TransferState::Downloading));
        a.observe(&down(all.clone()), 0);
        assert_eq!(layout_ids(&down(all), &a).0, ["new", "old1", "old2"]);
    }

    #[test]
    fn held_torrents_stay_ahead_of_the_newest_arrival() {
        let mut a = ArrivalsState::default();
        a.observe(&down(vec![row("old", TransferState::Downloading)]), 0);
        let s = down(vec![row("old", TransferState::Downloading), choice("c"), held("n"), row("new", TransferState::Downloading)]);
        a.observe(&s, 0);
        assert_eq!(layout_ids(&s, &a).0, ["c", "n", "new", "old"]);
    }

    #[test]
    fn a_queued_arrival_is_listed_under_downloading_not_queued() {
        let mut a = ArrivalsState::default();
        a.observe(&snap(vec![row("d1", TransferState::Downloading)], vec![row("oldq", TransferState::Queued)]), 0);
        let s = snap(
            vec![row("d1", TransferState::Downloading)],
            vec![row("oldq", TransferState::Queued), row("newq", TransferState::Queued)],
        );
        a.observe(&s, 0);
        assert_eq!(layout_ids(&s, &a), (vec!["newq".to_string(), "d1".into()], vec!["oldq".to_string()]));
    }

    #[test]
    fn at_most_three_queued_arrivals_are_lifted_newest_first() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        for n in 1..=4 {
            let q: Vec<_> = (1..=n).map(|i| row(&format!("q{i}"), TransferState::Queued)).collect();
            a.observe(&snap(Vec::new(), q), 0);
        }
        let q: Vec<_> = (1..=4).map(|i| row(&format!("q{i}"), TransferState::Queued)).collect();
        let (d, rest) = layout_ids(&snap(Vec::new(), q), &a);
        assert_eq!(d, ["q4", "q3", "q2"]);
        assert_eq!(rest, ["q1"]);
    }

    #[test]
    fn without_arrivals_the_menu_keeps_the_engines_order() {
        let a = ArrivalsState::default();
        let s = snap(
            vec![row("a", TransferState::Downloading), row("b", TransferState::Downloading)],
            vec![row("q", TransferState::Queued)],
        );
        assert_eq!(layout_ids(&s, &a), (vec!["a".to_string(), "b".into()], vec!["q".to_string()]));
    }

    // MARK: Row texts

    fn dl(rate: u64, eta: Option<u64>, waiting: Option<&str>) -> TransferRow {
        let mut r = row("a", TransferState::Downloading);
        r.down_rate = rate;
        r.eta_secs = eta;
        r.waiting_note = waiting.map(Into::into);
        r.done_bytes = 420_000_000;
        r.size_bytes = 1_000_000_000;
        r
    }

    #[test]
    fn speed_comes_with_time_left() {
        assert_eq!(menu_subtitle(&dl(3_100_000, Some(240), None), 0), "42% · 3.1 MB/s · 4 min left");
        assert_eq!(menu_subtitle(&dl(3_100_000, None, None), 0), "42% · 3.1 MB/s · calculating…");
    }

    #[test]
    fn no_speed_shows_the_waiting_text_not_a_stale_time() {
        assert_eq!(menu_subtitle(&dl(0, Some(240), Some("Waiting for peers")), 0), "42% · Waiting for peers");
        assert_eq!(
            menu_subtitle(&dl(0, Some(240), Some("Tracker: Unregistered torrent")), 0),
            "42% · Tracker: Unregistered torrent"
        );
        assert_eq!(menu_subtitle(&dl(0, Some(240), None), 0), "42% · calculating…");
        let mut f = dl(0, None, Some("Bad gateway"));
        f.error_transient = true;
        f.retry_in_secs = Some(540);
        assert_eq!(menu_subtitle(&f, 0), "42% · Bad gateway · retrying in 9 min");
    }

    #[test]
    fn paused_queued_and_other_rows() {
        let mut p = dl(0, None, None);
        p.state = TransferState::Paused;
        assert_eq!(menu_subtitle(&p, 0), "Paused · 42%");
        assert_eq!(menu_subtitle(&row("q", TransferState::Queued), 0), "Up next · waiting for a free slot");
        let mut c = dl(0, Some(120), None);
        c.state = TransferState::Checking;
        assert_eq!(menu_subtitle(&c, 0), "42% · 2 min left");
    }

    #[test]
    fn a_download_again_counts_its_new_batch() {
        let mut r = row("a", TransferState::Downloading);
        r.size_bytes = 800_000_000;
        r.done_bytes = 650_000_000;
        r.session_size = 200_000_000;
        r.session_done = 50_000_000;
        r.down_rate = 1_000_000;
        r.eta_secs = Some(240);
        assert_eq!(menu_subtitle(&r, 0), "25% · 1.0 MB/s · 4 min left");
        assert_eq!(menu_subtitle(&again(), 0), "42% · 3.1 MB/s · 4 min left");
    }

    #[test]
    fn the_check_is_ticked_while_a_row_runs() {
        assert!(is_running(&row("a", TransferState::Downloading)));
        assert!(is_running(&row("a", TransferState::Queued)));
        assert!(!is_running(&row("a", TransferState::Paused)));
        let mut h = row("a", TransferState::Queued);
        h.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 5_000, paused: true });
        assert!(!is_running(&h), "a stopped countdown");
    }

    #[test]
    fn the_sharing_line_ends_with_the_combined_upload_rate() {
        let line = |up: u64, online: bool, errors: u32| {
            let mut s = empty();
            s.sharing_total = 5;
            s.up_rate = up;
            s.online = online;
            s.error_count = errors;
            sharing_line(&s, false)
        };
        let busy = line(340_000, true, 0);
        assert_eq!((busy.title.as_str(), busy.rate.as_str(), busy.rate_active), ("Sharing 5 torrents", "↑ 340 KB/s", true));
        let idle = line(0, true, 1);
        assert_eq!((idle.title.as_str(), idle.rate.as_str(), idle.rate_active), ("Sharing 5 torrents · 1 error", "–", false));
        let offline = line(9_000, false, 2);
        assert_eq!((offline.title.as_str(), offline.rate.as_str()), ("Sharing 5 torrents · Offline · 2 errors", "–"));
        assert!(!offline.rate_active);
    }

    // MARK: Folder Sync

    fn folder(name: &str, state: SyncState, rate: u64, conflicts: u32) -> SyncFolder {
        SyncFolder {
            id: format!("ID-{name}"),
            name: name.into(),
            path: format!("/sync/{name}"),
            access: SyncAccess::ReadWrite,
            role: SyncRole::Owner,
            share_until_s: None,
            state,
            files: 10,
            bytes: 1_000_000_000,
            need_files: 0,
            need_bytes: 0,
            rate_in: rate,
            rate_out: 0,
            connected: 0,
            members: 0,
            conflicts,
            lan_only: false,
            issues: Vec::new(),
            error: None,
            last_synced_s: None,
            channel_id: None,
            on_demand: false,
            unavailable: 0,
        }
    }

    fn sync_snap(folders: Vec<SyncFolder>, running: bool) -> SyncSnapshot {
        SyncSnapshot {
            running,
            problem: None,
            device_id: "D".into(),
            device_name: "Mac".into(),
            port: 22100,
            folders,
            devices: Vec::new(),
        }
    }

    fn up(name: &str) -> SyncFolder {
        folder(name, SyncState::UpToDate, 0, 0)
    }

    #[test]
    fn the_sync_line_says_what_needs_the_user_first() {
        let ok = vec![up("A"), up("B"), up("C")];
        assert_eq!(sync_summary_of(&sync_snap(ok.clone(), true)), "3 folders · Up to date");
        let syncing =
            vec![folder("A", SyncState::Syncing, 7_000_000, 0), folder("B", SyncState::Syncing, 5_000_000, 0), up("C")];
        assert_eq!(sync_summary_of(&sync_snap(syncing.clone(), true)), "Syncing 2 · 12 MB/s");
        let mut conflict = syncing.clone();
        conflict.push(folder("D", SyncState::UpToDate, 0, 1));
        assert_eq!(sync_summary_of(&sync_snap(conflict.clone(), true)), "1 conflict");
        let mut missing = conflict;
        missing.push(folder("E", SyncState::Missing, 0, 0));
        assert_eq!(sync_summary_of(&sync_snap(missing, true)), "1 folder missing");
        assert_eq!(sync_summary_of(&sync_snap(vec![folder("E", SyncState::Error, 0, 0)], true)), "1 folder can't sync");
        assert_eq!(sync_summary_of(&sync_snap(Vec::new(), true)), "No folders");
        assert_eq!(sync_summary_of(&sync_snap(Vec::new(), false)), "Starting…");
        let mut broken = sync_snap(Vec::new(), false);
        broken.problem = Some("port in use".into());
        assert_eq!(sync_summary_of(&broken), "Not running");
        assert_eq!(sync_summary_of(&sync_snap(vec![folder("A", SyncState::Paused, 0, 0)], true)), "1 folder · Paused");
        let waiting = vec![folder("A", SyncState::WaitingForAdmin, 0, 0), up("B")];
        assert_eq!(sync_summary_of(&sync_snap(waiting, true)), "2 folders · Waiting for devices");
        let scanning = vec![folder("A", SyncState::Scanning, 0, 0), folder("B", SyncState::Paused, 0, 0)];
        assert_eq!(sync_summary_of(&sync_snap(scanning, true)), "2 folders · Scanning…");
        assert_eq!(sync_summary(sync_snap(ok.clone(), true)), "3 folders · Up to date");
        assert!(sync_needs_attention(&sync_snap(vec![folder("E", SyncState::Missing, 0, 0)], true)));
        assert!(!sync_needs_attention(&sync_snap(ok, true)));
    }

    #[test]
    fn the_folder_sync_line_is_there_only_while_there_are_folders() {
        let a = ArrivalsState::default();
        let s = empty();
        let none = build(&s, &sync_snap(Vec::new(), false), &a, 0, None);
        assert!(none.folder_sync.is_none());
        assert!(build(&s, &sync_snap(Vec::new(), true), &a, 0, None).folder_sync.is_none());
        let one = build(&s, &sync_snap(vec![up("A")], true), &a, 0, None);
        assert_eq!(one.folder_sync.as_ref().map(|l| (l.title.as_str(), l.subtitle.as_str())), Some(("Folder Sync", "1 folder · Up to date")));
        assert_ne!(one.signature, none.signature, "the first folder brings the line in: the menu is rebuilt");
        let two = build(&s, &sync_snap(vec![up("A"), up("B")], true), &a, 0, None);
        assert_eq!(two.signature, one.signature, "folders are the submenu's, not the menu's structure");
    }

    // MARK: The whole menu

    fn recent(id: &str, finished_at: u64) -> TransferRow {
        let mut r = row(id, TransferState::Seeding);
        r.finished_at = Some(finished_at);
        r.category = Category::Tv;
        r
    }

    #[test]
    fn sections_caps_and_separators() {
        let a = ArrivalsState::default();
        let mut s = snap(
            vec![row("d", TransferState::Downloading)],
            (1..=5).map(|i| row(&format!("q{i}"), TransferState::Queued)).collect(),
        );
        s.recent = (1..=4).map(|i| recent(&format!("r{i}"), 1_000)).collect();
        let m = build(&s, &sync_snap(vec![up("A")], true), &a, 1_000_000 + 720_000, Some("0.2.0".into()));
        assert_eq!(m.offline, None);
        let titles: Vec<_> = m.sections.iter().map(|x| (x.title.as_str(), x.separator_before, x.rows.len())).collect();
        assert_eq!(titles, [("Downloading", false, 1), ("Queued", false, 3), ("Recent", true, 3)]);
        assert_eq!(m.sections[1].more.as_deref(), Some("+ 2 more"));
        assert_eq!(m.sections[0].more, None);
        let r = &m.sections[2].rows[0];
        assert_eq!((r.subtitle.as_str(), r.mark, r.click, r.running), ("TV · 12 min ago", Some(MenuMark::Done), Some(MenuClick::Reveal), None));
        let sharing = m.sharing.as_ref().unwrap();
        assert!(sharing.separator_before);
        assert!(!m.folder_sync.as_ref().unwrap().separator_before, "the Sharing line comes first, no separator between");
        assert_eq!(m.update.as_deref(), Some("Update to Seedwell 0.2.0"));
        assert_eq!((m.open_title.as_str(), m.quit_title.as_str(), m.refresh_ms), ("Open Seedwell…", "Quit", 1000));
    }

    #[test]
    fn offline_without_a_sharing_line_is_a_quiet_line_on_top() {
        let a = ArrivalsState::default();
        let mut s = empty();
        s.online = false;
        s.sharing_total = 0;
        s.recent = vec![recent("r", 1_000)];
        let m = build(&s, &sync_snap(vec![up("A")], true), &a, 2_000_000, None);
        assert_eq!(m.offline.as_deref(), Some("Offline"));
        assert!(m.sections[0].separator_before, "Recent under the Offline line");
        assert!(m.sharing.is_none());
        assert!(m.folder_sync.unwrap().separator_before);
        // With sharing, the Sharing line says it instead.
        s.sharing_total = 2;
        let m = build(&s, &sync_snap(Vec::new(), true), &a, 2_000_000, None);
        assert_eq!(m.offline, None);
        assert_eq!(m.sharing.unwrap().title, "Sharing 2 torrents · Offline");
        // An empty menu: nothing above, no separators.
        let mut bare = empty();
        bare.sharing_total = 0;
        let m = build(&bare, &sync_snap(vec![up("A")], true), &a, 0, None);
        assert!(m.sections.is_empty() && m.sharing.is_none() && m.update.is_none());
        assert!(!m.folder_sync.unwrap().separator_before);
        let m = build(&{ let mut b = empty(); b.sharing_total = 1; b }, &sync_snap(Vec::new(), true), &a, 0, None);
        assert!(!m.sharing.unwrap().separator_before);
    }

    #[test]
    fn each_row_kind_has_its_mark_check_and_click() {
        let mut a = ArrivalsState::default();
        a.observe(&empty(), 0);
        let long = "A".repeat(30) + &"B".repeat(30);
        let mut active = dl(3_100_000, Some(240), None);
        active.display_name = long.clone();
        active.file_count = 3;
        let mut counting = held("n");
        counting.pending = Some(PendingStart { starts_at_ms: 10_000, remaining_ms: 10_000, paused: false });
        let mut waiting = choice("c");
        waiting.display_name = long.clone();
        let mut paused = row("p", TransferState::Paused);
        paused.done_bytes = 50;
        let mut fresh = row("f", TransferState::Paused);
        fresh.display_name = long.clone();
        let s = snap(vec![active, counting, waiting], vec![row("q", TransferState::Queued), paused, fresh]);
        let m = build(&s, &sync_snap(Vec::new(), true), &a, 3_500, None);
        let d = &m.sections[0].rows;
        assert_eq!(d.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["c", "n", "a"]);
        // Waiting for a choice: the whole name, an orange dot, no check, the chooser.
        assert_eq!(d[0].title, long);
        assert_eq!((d[0].subtitle.as_str(), d[0].mark, d[0].running, d[0].click), ("Choose what to download…", Some(MenuMark::Attention), None, Some(MenuClick::OpenChooser)));
        assert!(!d[0].expands && !d[0].playable);
        // Counting down: an empty ring, the countdown, its check pauses the countdown.
        assert_eq!((d[1].subtitle.as_str(), d[1].mark, d[1].running, d[1].countdown), ("Starting in 7 s", Some(MenuMark::Ring { fraction: 0.0 }), Some(true), true));
        assert!(!d[1].playable, "held: nothing to play yet");
        // Downloading: the name cut in the middle, the ring at its progress, the file list, Play.
        assert_eq!(d[2].title.chars().count(), 44);
        assert!(d[2].title.starts_with("AAAA") && d[2].title.ends_with("BBBB") && d[2].title.contains('…'));
        assert_eq!((d[2].subtitle.as_str(), d[2].mark, d[2].running, d[2].countdown), ("42% · 3.1 MB/s · 4 min left", Some(MenuMark::Ring { fraction: 0.42 }), Some(true), false));
        assert!(d[2].expands && d[2].playable && !d[2].dimmed && d[2].click.is_none());
        // Queued: dimmed, the whole name, no grey line; a paused row with data keeps its ring, without data none.
        let q = &m.sections[1].rows;
        assert!(q.iter().all(|r| r.dimmed && r.subtitle.is_empty()));
        assert_eq!(q.iter().map(|r| r.mark).collect::<Vec<_>>(), [Some(MenuMark::Ring { fraction: 0.0 }), Some(MenuMark::Ring { fraction: 0.5 }), None]);
        assert_eq!(q.iter().map(|r| r.running).collect::<Vec<_>>(), [Some(true), Some(false), Some(false)]);
        assert_eq!(q[2].title, long);
    }

    #[test]
    fn a_recent_row_has_play_only_with_its_file_list() {
        let mut single = recent("s", 0);
        single.file_ext = "mkv".into();
        assert!(!recent_row(&single, 0).playable && !recent_row(&single, 0).expands);
        let mut pack = recent("p", 0);
        pack.file_ext = String::new();
        pack.has_video = true;
        pack.file_count = 8;
        assert!(recent_row(&pack, 0).playable && recent_row(&pack, 0).expands);
        assert_eq!(recent_row(&pack, 200_000).subtitle, "TV · 4 min ago", "minutes round up");
        pack.finished_at = None;
        assert_eq!(recent_row(&pack, 0).subtitle, "TV");
    }

    #[test]
    fn the_signature_changes_with_the_structure_not_with_numbers() {
        let a = ArrivalsState::default();
        let no_sync = sync_snap(Vec::new(), true);
        let base = snap(vec![dl(1_000_000, Some(60), None)], vec![row("q", TransferState::Queued)]);
        let sig = |s: &Snapshot| build(s, &no_sync, &a, 0, None).signature;
        let mut faster = base.clone();
        faster.downloading[0].down_rate = 9_000_000;
        faster.downloading[0].done_bytes += 10;
        faster.up_rate = 5;
        assert_eq!(sig(&faster), sig(&base));
        let mut paused = base.clone();
        paused.downloading[0].state = TransferState::Paused;
        assert_ne!(sig(&paused), sig(&base));
        let mut more = base.clone();
        more.queued.push(row("q2", TransferState::Queued));
        assert_ne!(sig(&more), sig(&base));
        let mut errors = base.clone();
        errors.error_count = 1;
        assert_ne!(sig(&errors), sig(&base));
        let mut offline = base.clone();
        offline.online = false;
        assert_ne!(sig(&offline), sig(&base));
        let mut countdown = base.clone();
        countdown.downloading.push(held("h"));
        let stopped = {
            let mut s = countdown.clone();
            s.downloading[1].pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 7_000, paused: true });
            s
        };
        assert_ne!(sig(&stopped), sig(&countdown));
    }

    // MARK: The icon

    #[test]
    fn the_arrows_colour_in_eight_logarithmic_steps() {
        assert_eq!(intensity(0), 0.0);
        assert_eq!(intensity(4_999), 0.0);
        assert_eq!(intensity(5_000), 0.0);
        assert_eq!(intensity(5_001), 0.125);
        assert_eq!(intensity(10_000_000), 1.0);
        assert_eq!(intensity(50_000_000), 1.0);
        assert!(intensity(100_000) > intensity(20_000));
    }

    #[test]
    fn a_rename_that_ends_quickly_never_shows_the_move_bar() {
        let mut since = None;
        let a = move_fraction(&mut since, Some(0), 1_000);
        let watching = since.is_some();
        let b = move_fraction(&mut since, Some(50), 1_100);
        let c = move_fraction(&mut since, None, 1_200);
        assert_eq!((a, b, c), (None, None, None));
        assert!(watching);
        assert!(since.is_none());
    }

    #[test]
    fn a_move_still_running_after_the_grace_shows_its_progress() {
        let mut since = None;
        move_fraction(&mut since, Some(0), 1_000);
        assert_eq!(move_fraction(&mut since, Some(10), 1_390), None);
        assert_eq!(move_fraction(&mut since, Some(40), 1_500), Some(0.4));
        assert_eq!(move_fraction(&mut since, Some(100), 4_000), Some(1.0));
    }

    #[test]
    fn move_progress_is_quantised_so_equal_looks_compare_equal() {
        let mut since = None;
        move_fraction(&mut since, Some(1), 1_000);
        let a = move_fraction(&mut since, Some(41), 2_000);
        let b = move_fraction(&mut since, Some(42), 2_500);
        assert!(a.is_some() && a == b);
    }

    #[test]
    fn the_bar_goes_when_the_moves_end_and_the_grace_starts_again_for_the_next() {
        let mut since = None;
        move_fraction(&mut since, Some(5), 1_000);
        assert!(move_fraction(&mut since, Some(5), 2_000).is_some());
        assert_eq!(move_fraction(&mut since, None, 3_000), None);
        assert_eq!(move_fraction(&mut since, Some(0), 6_000), None, "a new move: grace again");
        assert_eq!(move_fraction(&mut since, Some(0), 6_500), Some(0.0));
    }

    #[test]
    fn the_icon_label_progress_and_cadence() {
        let icon = IconState::new();
        let mut s = empty();
        let idle = icon.icon(s.clone(), 0, false);
        assert_eq!(idle.accessibility_label, "Seedwell");
        assert_eq!((idle.look.progress, idle.look.moving, idle.next_refresh_ms, idle.refresh_leeway_ms), (None, None, 30_000, 15_000));
        assert_eq!(icon.icon(s.clone(), 0, true).next_refresh_ms, 1_000, "the open menu refreshes every second");
        // Downloading (held rows do not count), with rates and attention.
        let mut a = row("a", TransferState::Downloading);
        a.done_bytes = 42;
        s.downloading = vec![a, held("h")];
        let m = icon.icon(s.clone(), 0, false);
        assert_eq!(m.accessibility_label, "Seedwell, downloading");
        assert_eq!((m.look.progress, m.next_refresh_ms), (Some(0.42), 5_000));
        s.down_rate = 3_100_000;
        s.up_rate = 340_000;
        s.attention = true;
        let m = icon.icon(s.clone(), 0, false);
        assert_eq!(m.accessibility_label, "Seedwell, downloading 3.1 MB/s, uploading 340 KB/s, needs attention");
        assert!(m.look.down > 0.0 && m.look.up > 0.0 && m.look.attention);
        // Only held rows: nothing downloads.
        let mut h = empty();
        h.downloading = vec![held("h")];
        assert_eq!(icon.icon(h, 0, false).look.progress, None);
        // A move is watched twice a second, from its start (in the grace period) on.
        let mut moving = empty();
        moving.moving_percent = Some(30);
        let first = icon.icon(moving.clone(), 10_000, false);
        assert_eq!((first.look.moving, first.next_refresh_ms, first.refresh_leeway_ms), (None, 500, 100));
        assert_eq!(icon.icon(moving, 10_500, true).look.moving, Some(0.3));
        assert_eq!(icon.icon(empty(), 11_000, false).next_refresh_ms, 30_000, "the move ended");
    }
}
