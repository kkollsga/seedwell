//! The lists of the main window (SPEC 8.3) as view models every host draws: Downloads, Sharing and a tracker's tab.
//! Rust decides (docs/CROSS-PLATFORM.md §1) which groups there are, in what order, their headers, counts, hints, fold
//! state and "Show more", and for every row its texts and colours, its rate slot, its controls with their help, its
//! category pill and whether it opens a file list. The host draws them and runs the controls.

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

use trs_control::display as d;
use trs_platform::file_manager;

use crate::menu::APP_NAME;
use crate::row_commands::RowCommand;
use crate::row_text::{
    can_ask_again, category_label, category_plural, download_waiting, fraction, is_announcing, is_held, pending_remaining_ms,
    percent, progress_done, progress_size, retry_note, seed_group_name, shows_play, starting_line, TextRun, TextTone,
};
use crate::rows::matches_text;
use crate::types::{Category, SeedGroup, SharingCounts, Snapshot, TransferKind, TransferRow, TransferState};

/// Rows of a Sharing group fetched at first; "Show more" adds as many.
pub(crate) const PAGE: u32 = 150;

/// The Sharing groups, in the order they are listed.
pub(crate) const SEED_GROUPS: [SeedGroup; 6] =
    [SeedGroup::Popular, SeedGroup::Active, SeedGroup::Dormant, SeedGroup::Error, SeedGroup::Dead, SeedGroup::Stopped];

const REVEAL: &str = concat!("Show in ", file_manager!());

/// How a row is drawn: the group it is listed in decides.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowStyle {
    Download,
    Queued,
    Recent,
    Popular,
    Active,
    Dormant,
    Error,
    Dead,
    Stopped,
}

impl RowStyle {
    pub(crate) fn of(g: SeedGroup) -> Self {
        match g {
            SeedGroup::Popular => Self::Popular,
            SeedGroup::Active => Self::Active,
            SeedGroup::Dormant => Self::Dormant,
            SeedGroup::Error => Self::Error,
            SeedGroup::Dead => Self::Dead,
            SeedGroup::Stopped => Self::Stopped,
        }
    }

    /// A seed that shares (its upload rate and totals are shown).
    fn shares(self) -> bool {
        matches!(self, Self::Popular | Self::Active | Self::Dormant | Self::Stopped)
    }
}

// MARK: The records

/// What a control at the right of a row is. The host picks the image by it and runs it (a core call, a popover).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum RowControlKind {
    /// "Ask Again": a small button with words (the tracker said no or cannot be reached).
    AskAgain,
    /// Get Info: an info glyph that opens the row's Get Info.
    GetInfo,
    /// Play while downloading: a plain play triangle next to the pause ring.
    PlayWhileDownloading,
    /// Play a finished or sharing video: a plain play triangle.
    Play,
    /// Progress and pause in one ring ([`RowControl::ring`]).
    PauseRing,
    /// Show in the file manager: a magnifier.
    Reveal,
    /// Resting in low power: a moon (no action).
    LowPower,
    /// An error mark (no action).
    ErrorBadge,
    /// A dead torrent's mark (no action).
    DeadBadge,
    /// "Remove…": a small button with words; asks under the row first.
    Remove,
    /// Resume sharing a stopped seed: a play circle.
    ResumeSharing,
    /// Cancel and Start of a torrent that waits for the user's choice of files.
    ChoiceActions,
    /// Cancel and Start of a torrent counting down to its start.
    CountdownActions,
}

/// The ring of [`RowControlKind::PauseRing`].
#[derive(Clone, Copy, Debug, PartialEq, uniffi::Record)]
pub struct RingLook {
    /// 0...1 (the host draws at least a sliver).
    pub fraction: f64,
    /// The glyph inside resumes (a play triangle) instead of pausing.
    pub paused: bool,
    /// The row stopped on an error: the glyph retries (a circular arrow), the action resumes.
    pub retry: bool,
}

/// A control at the right of a row, in the order they are drawn, left to right.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RowControl {
    pub kind: RowControlKind,
    /// The words of a button that has them ("Ask Again", "Remove…"); `None`: an image.
    pub title: Option<String>,
    /// The tooltip.
    pub help: String,
    /// What a screen reader says when it differs from the tooltip.
    pub accessibility: Option<String>,
    /// Shown only while the pointer is over the row or the row is selected.
    pub hover_only: bool,
    /// Keeps its place while hidden, so the text beside it never shifts.
    pub keeps_space: bool,
    pub ring: Option<RingLook>,
    /// What it does, for the hosts that run it as a row command (`None`: an image without an action).
    pub command: Option<RowCommand>,
}

/// The live rate at the right of a row: "↑ 1.2 MB/s" in the label colour while data flows, "–" in the secondary
/// colour when idle.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct RateSlot {
    pub text: String,
    pub active: bool,
    pub help: String,
}

/// The row's category as a small capsule after its name; a click filters the list to it (or clears the filter).
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct RowPill {
    pub category: Category,
    pub label: String,
    /// The list is filtered to this category.
    pub active: bool,
    pub help: String,
    pub accessibility: String,
}

/// "TV ×" above a filtered list: the category shown, and the way back (a click clears the filter).
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct FilterChip {
    pub category: Category,
    pub label: String,
    pub help: String,
    pub accessibility: String,
}

/// What the disclosure triangle at the left of a row opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum RowDisclosure {
    /// The torrent's files (it has several).
    Files,
    /// The choice of files of a torrent that waits for it: a triangle only while the chooser has more than one file
    /// (the host's chooser knows).
    Choice,
}

/// One torrent in a list.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ListRow {
    pub id: String,
    /// The shown name ("Sintel (2010)"); the host cuts it in the middle when it does not fit.
    pub title: String,
    /// The raw name as the title's tooltip, when it differs.
    pub title_help: Option<String>,
    /// The extension whose file icon the row shows (empty: a folder).
    pub icon_ext: String,
    /// A direct download: the icon has a small link badge.
    pub linked: bool,
    /// Drawn quieter: the title in the secondary colour and regular weight, the icon faded (queued, resting, stopped).
    pub dimmed: bool,
    /// An orange dot before the title: it waits for the user's choice.
    pub attention_dot: bool,
    pub pill: RowPill,
    /// The grey line under the title, in runs of colour.
    pub status: Vec<TextRun>,
    /// A third line ("3 of 24 files · 1.8 GB of 14 GB downloaded · 3 GB uploaded").
    pub detail: Option<String>,
    /// A thin progress bar under the lines, 0...1 (the host draws at least a sliver).
    pub progress: Option<f64>,
    pub rate: Option<RateSlot>,
    pub controls: Vec<RowControl>,
    pub disclosure: Option<RowDisclosure>,
    /// The torrent itself, for the host's own services (its context menu, Get Info, the file list).
    pub transfer: TransferRow,
    /// In the selection (drawn highlighted).
    pub selected: bool,
    /// Its Get Info is open (the popover at the row).
    pub info_open: bool,
    /// Changes whenever anything above does: the host may skip redrawing a row whose revision it has drawn.
    pub revision: u64,
}

impl ListRow {
    /// Marks the row selected and its Get Info open, in its revision too.
    pub(crate) fn mark(&mut self, selected: bool, info_open: bool) {
        if !selected && !info_open {
            return;
        }
        (self.selected, self.info_open) = (selected, info_open);
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.revision, selected, info_open).hash(&mut h);
        self.revision = h.finish();
    }
}

/// What a group header's action does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum ListCommand {
    /// "Clear" on Recently Finished: forgets the list (`Core::clear_recent`; the torrents keep seeding).
    ClearRecent,
}

/// A small button at the right of a group header.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct GroupAction {
    pub title: String,
    pub help: String,
    pub command: ListCommand,
}

/// "Show 80 more…" under a group whose rows are not all fetched: `Library::show_more(group)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct GroupMore {
    pub title: String,
    pub help: String,
    pub group: SeedGroup,
}

/// A group of rows, or a block of a tracker's tab that holds groups ([`ListGroup::block`]).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ListGroup {
    /// Unique in the list ("next", "dormant", "downloads"): what [`crate::library::Library::toggle_fold`] takes.
    pub key: String,
    /// The header; `None`: no header (the Downloads tab's active downloads), and the group does not fold.
    pub title: Option<String>,
    /// The count at the right of the header.
    pub count: Option<String>,
    /// The longer explanation of the group, as the header's tooltip.
    pub hint: Option<String>,
    /// The header's colour (`Problem`: red); a block's header is drawn stronger, in the label colour.
    pub tone: TextTone,
    /// A top-level block of a tracker's tab ("Downloads", "Sharing"): a stronger header, no rows of its own.
    pub block: bool,
    /// The block this group sits in.
    pub parent: Option<String>,
    /// Folded: the header only (a click on the title unfolds it); no rows are shipped.
    pub folded: bool,
    /// The tooltip of the title, which folds the group: "Hide Up Next" / "Show Up Next".
    pub fold_help: Option<String>,
    pub action: Option<GroupAction>,
    pub more: Option<GroupMore>,
    pub rows: Vec<ListRow>,
}

/// A list page: Downloads, Sharing or a tracker's torrents.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ListModel {
    /// Shown above the groups while the list is filtered to a category.
    pub filter_chip: Option<FilterChip>,
    /// In order; the groups of a folded block are not there.
    pub groups: Vec<ListGroup>,
    /// The quiet line of a list with nothing in it ("No downloads", "No matches on tracker-z.cc").
    pub empty: Option<String>,
    /// The structure: which groups, folded or not, which rows in what order and which controls each has. While it
    /// stays the same the host may update the rows in place by id; when it changes, it rebuilds the list.
    pub signature: String,
}

// MARK: One row

/// What a row's texts depend on besides the row.
pub(crate) struct RowContext {
    pub now_ms: u64,
    /// False inside a tracker's own tab, where every row would repeat its name.
    pub show_tracker: bool,
    /// The category the list is filtered to.
    pub filter: Option<Category>,
}

fn run(text: impl Into<String>, tone: TextTone) -> TextRun {
    TextRun { text: text.into(), tone }
}

/// Runs one after the other; neighbours of one tone are joined and empty ones dropped.
fn joined(runs: Vec<TextRun>) -> Vec<TextRun> {
    let mut out: Vec<TextRun> = Vec::new();
    for r in runs.into_iter().filter(|r| !r.text.is_empty()) {
        match out.last_mut() {
            Some(last) if last.tone == r.tone => last.text.push_str(&r.text),
            _ => out.push(r),
        }
    }
    out
}

/// The extension whose file icon the row shows: the file's own; for a multi-file torrent whose main content is video,
/// the largest video's (the icon a single-file video row gets); else a folder.
fn icon_ext(r: &TransferRow) -> String {
    if r.file_ext.is_empty() && r.has_video && !r.video_ext.is_empty() { r.video_ext.clone() } else { r.file_ext.clone() }
}

/// "8 / 57 peers" while a download runs, after its line. The rate is not here: it has its own slot.
fn live_suffix(r: &TransferRow, style: RowStyle) -> String {
    if style == RowStyle::Download && r.state == TransferState::Downloading { d::peers(r.peers, r.known_peers) } else { String::new() }
}

/// The rate slot: always there for a download (↓) and a sharing row (↑); `None` for held, queued, finished and
/// failing rows.
fn rate_slot(r: &TransferRow, style: RowStyle) -> Option<RateSlot> {
    let (rate, arrow, help) = match style {
        RowStyle::Download if is_held(r) => return None,
        RowStyle::Download => {
            let on = r.state == TransferState::Downloading && r.down_rate > 0;
            (if on { r.down_rate } else { 0 }, "↓", "Download speed")
        }
        s if s.shares() => {
            let on = r.moving_percent.is_none() && r.up_rate > 0;
            (if on { r.up_rate } else { 0 }, "↑", "Upload speed")
        }
        _ => return None,
    };
    Some(RateSlot { text: d::live_rate(rate, arrow), active: rate > 0, help: help.into() })
}

/// " of 2.0": the ratio goal after a ratio.
fn goal_suffix(r: &TransferRow) -> String {
    r.ratio_goal.map(|g| format!(" of {}", d::goal(g))).unwrap_or_default()
}

/// A Sharing row's line: "1 / 4 peers · Ratio 0.27 of 1.0" ("Moving… 42%" while its files move).
fn sharing_line(r: &TransferRow) -> String {
    if let Some(p) = r.moving_percent {
        return format!("Moving… {p}%");
    }
    format!("{} · Ratio {}{}", d::peers(r.peers, r.known_peers), d::ratio(r.ratio), goal_suffix(r))
}

/// A Sharing row's third line: "3 of 24 files · 1.8 GB of 14 GB downloaded · 3 GB uploaded". The files are the ones
/// fully downloaded, of all files (a single file leaves them out); the sizes are of the whole torrent, skipped files
/// included.
fn sharing_totals_line(r: &TransferRow) -> String {
    let mut parts = Vec::new();
    if r.file_count > 1 {
        parts.push(format!("{} of {} files", r.files_done, r.file_count));
    }
    let total = r.total_bytes.max(r.size_bytes);
    let held = r.held_bytes.max(r.done_bytes).min(total);
    let down = if held >= total { d::bytes(total) } else { format!("{} of {}", d::bytes(held), d::bytes(total)) };
    parts.push(format!("{down} downloaded"));
    parts.push(format!("{} uploaded", d::bytes(r.uploaded_bytes)));
    parts.join(" · ")
}

/// The grey line before the live suffix and "Announcing…".
fn base_line(r: &TransferRow, style: RowStyle, show_tracker: bool, now_ms: u64) -> String {
    let size = d::bytes(r.size_bytes);
    match style {
        RowStyle::Download => {
            if r.awaiting_choice {
                return "Choose what to download…".into();
            }
            if let Some(p) = &r.pending {
                let tail = if p.paused { " — won't start until you resume" } else { "" };
                return starting_line(pending_remaining_ms(p, now_ms), p.paused) + tail;
            }
            if let Some(p) = r.moving_percent {
                return format!("Moving… {p}%");
            }
            if let Some(e) = &r.error {
                return if r.state == TransferState::Error { format!("{e} · Resume to try again") } else { e.clone() };
            }
            if let Some(w) = download_waiting(r, now_ms) {
                return format!("{}{}{}", w.lead, w.note, w.tail);
            }
            let progress = d::progress(progress_done(r), progress_size(r));
            match r.state {
                TransferState::Metadata => "Fetching metadata…".into(),
                TransferState::Checking => format!("Checking files… {}%", percent(r)),
                TransferState::Paused => format!("Paused — {progress}"),
                _ => format!("{progress} — {}", d::remaining(r.eta_secs, false)),
            }
        }
        RowStyle::Queued => {
            if let Some(p) = r.moving_percent {
                return format!("Moving… {p}%");
            }
            let lead = if r.kind == TransferKind::Direct { format!("{} — ", r.tracker_name) } else { String::new() };
            format!("{lead}{} — {size}", if r.state == TransferState::Paused { "Paused" } else { "Waiting" })
        }
        RowStyle::Recent => {
            let when = r.finished_at.map(|at| d::ago(at, now_ms / 1000, false)).unwrap_or_default();
            format!("{size} — {when} · {}", category_plural(r.category))
        }
        RowStyle::Popular | RowStyle::Active | RowStyle::Dormant => sharing_line(r),
        RowStyle::Error => {
            let mut parts: Vec<String> = Vec::new();
            if show_tracker {
                parts.push(r.tracker_name.clone());
            }
            parts.push(r.error.clone().unwrap_or_else(|| "Problem".into()));
            parts.extend(retry_note(r, now_ms));
            parts.join(" · ")
        }
        RowStyle::Dead => {
            let mut parts: Vec<String> = Vec::new();
            if show_tracker {
                parts.push(r.tracker_name.clone());
            }
            parts.push(r.error.clone().unwrap_or_else(|| "Gone".into()));
            parts.join(" · ")
        }
        RowStyle::Stopped => {
            let idle = r.idle_stopped.as_ref().map(|s| format!(" — {s}")).unwrap_or_default();
            format!("Ratio {}{} · Stopped{idle}", d::ratio(r.ratio), goal_suffix(r))
        }
    }
}

/// The grey line of a row as one text: the line, the live suffix, and one rule for every torrent: a quiet
/// "Announcing…" while an announce is in flight (after a failure the retry note says "retrying now…" instead).
pub(crate) fn status_line(r: &TransferRow, style: RowStyle, show_tracker: bool, now_ms: u64) -> String {
    let line = base_line(r, style, show_tracker, now_ms);
    let live = live_suffix(r, style);
    let mut out = if live.is_empty() { line } else { format!("{line} · {live}") };
    let quiet = matches!(style, RowStyle::Recent | RowStyle::Queued | RowStyle::Error | RowStyle::Dead);
    if is_announcing(r, now_ms) && r.retry_in_secs.is_none() && !quiet {
        out.push_str(" · Announcing…");
    }
    out
}

/// The row waits for the user: its line is orange (a choice, a download that stopped on an error).
fn needs_attention(r: &TransferRow, style: RowStyle) -> bool {
    r.awaiting_choice || (style == RowStyle::Download && r.error.is_some())
}

/// The grey line in colours. Problems that need the user (a dead tracker, a disk error) colour the reason red; a
/// temporary tracker outage stays calm: "tracker-z.cc · Bad gateway · retrying in 9 min". A download that cannot go
/// on says why in red: "0.0 of 1.9 GB — Tracker: Unregistered torrent".
pub(crate) fn status_runs(r: &TransferRow, style: RowStyle, show_tracker: bool, now_ms: u64) -> Vec<TextRun> {
    let base = if needs_attention(r, style) { TextTone::Attention } else { TextTone::Secondary };
    let runs = match style {
        RowStyle::Error | RowStyle::Dead => {
            let calm = style == RowStyle::Error && r.error_transient;
            let fallback = if style == RowStyle::Dead { "Gone" } else { "Problem" };
            let mut v = Vec::new();
            if show_tracker {
                v.push(run(format!("{} · ", r.tracker_name), base));
            }
            v.push(run(r.error.clone().unwrap_or_else(|| fallback.into()), if calm { TextTone::Secondary } else { TextTone::Problem }));
            if style == RowStyle::Error
                && let Some(note) = retry_note(r, now_ms)
            {
                v.push(run(format!(" · {note}"), base));
            }
            v
        }
        RowStyle::Download if r.waiting_is_error => match download_waiting(r, now_ms) {
            Some(w) => vec![run(w.lead, base), run(w.note, TextTone::Problem), run(w.tail, base)],
            None => vec![run(status_line(r, style, show_tracker, now_ms), base)],
        },
        _ => vec![run(status_line(r, style, show_tracker, now_ms), base)],
    };
    joined(runs)
}

fn control(kind: RowControlKind, help: impl Into<String>, command: Option<RowCommand>) -> RowControl {
    RowControl {
        kind,
        title: None,
        help: help.into(),
        accessibility: None,
        hover_only: false,
        keeps_space: false,
        ring: None,
        command,
    }
}

fn info(keeps_space: bool) -> RowControl {
    RowControl { hover_only: true, keeps_space, ..control(RowControlKind::GetInfo, "Get Info", Some(RowCommand::GetInfo)) }
}

fn play() -> RowControl {
    RowControl { accessibility: Some("Play".into()), ..control(RowControlKind::Play, "Play", Some(RowCommand::PlayRow)) }
}

/// The way out of a halted tracker: ask once more now (the tracker's minimum interval still applies).
fn ask_again(r: &TransferRow) -> RowControl {
    RowControl {
        title: Some("Ask Again".into()),
        ..control(
            RowControlKind::AskAgain,
            format!("Ask the tracker again now. {APP_NAME} never does this by itself after a tracker said no, and asks at most once a minute."),
            Some(RowCommand::AskTrackerAgain { ids: vec![r.id.clone()] }),
        )
    }
}

/// The controls at the right of a row, left to right.
fn controls(r: &TransferRow, style: RowStyle) -> Vec<RowControl> {
    let mut v = Vec::new();
    match style {
        RowStyle::Download => {
            if r.awaiting_choice {
                v.push(control(RowControlKind::ChoiceActions, "Start downloading the ticked files. Nothing downloads before this", None));
            } else if r.pending.is_some() {
                v.push(control(RowControlKind::CountdownActions, "Skip the wait and start downloading now", None));
            } else {
                if can_ask_again(r) {
                    v.push(ask_again(r));
                }
                v.push(info(false));
                let stopped = matches!(r.state, TransferState::Paused | TransferState::Error);
                // Only next to a pause glyph: beside a paused row's resume triangle it would read as a second resume
                // button (Play stays in the context menu).
                if shows_play(r) && !stopped {
                    v.push(RowControl {
                        accessibility: Some("Play".into()),
                        ..control(RowControlKind::PlayWhileDownloading, "Play while downloading", Some(RowCommand::PlayRow))
                    });
                }
                let retry = r.state == TransferState::Error;
                let help = if retry { "Try again" } else if stopped { "Resume" } else { "Pause" };
                v.push(RowControl {
                    ring: Some(RingLook { fraction: fraction(r), paused: stopped, retry }),
                    ..control(RowControlKind::PauseRing, help, Some(if stopped { RowCommand::Resume } else { RowCommand::Pause }))
                });
            }
        }
        RowStyle::Queued => v.push(info(false)),
        RowStyle::Recent => {
            // Play sits right before the magnifier, so the two read as a pair.
            if shows_play(r) {
                v.push(play());
            }
            v.push(control(RowControlKind::Reveal, REVEAL, Some(RowCommand::Reveal)));
        }
        RowStyle::Popular | RowStyle::Active => {
            if shows_play(r) {
                v.push(play());
            }
            v.push(info(true));
        }
        RowStyle::Dormant => {
            // The moon in a fixed spot next to the info slot, so it lines up on every resting row.
            if shows_play(r) {
                v.push(play());
            }
            v.push(control(RowControlKind::LowPower, "Low power: resting, wakes instantly", None));
            v.push(info(true));
        }
        RowStyle::Error => v.push(control(RowControlKind::ErrorBadge, r.error.clone().unwrap_or_else(|| "Problem".into()), None)),
        RowStyle::Dead => {
            v.push(control(RowControlKind::DeadBadge, r.error.clone().unwrap_or_else(|| "Gone".into()), None));
            if can_ask_again(r) {
                v.push(ask_again(r));
            }
            v.push(RowControl {
                title: Some("Remove…".into()),
                ..control(
                    RowControlKind::Remove,
                    format!("Remove this torrent from {APP_NAME}; asks under its row first, and keeps the files"),
                    Some(RowCommand::Remove),
                )
            });
        }
        RowStyle::Stopped => {
            if shows_play(r) {
                v.push(play());
            }
            v.push(control(RowControlKind::ResumeSharing, "Resume sharing", Some(RowCommand::Resume)));
        }
    }
    v
}

pub(crate) fn category_pill(category: Category, filter: Option<Category>) -> RowPill {
    let label = category_label(category);
    let active = filter == Some(category);
    RowPill {
        category,
        help: if active { format!("Showing only {label}. Click to show all") } else { format!("Show only {label}") },
        accessibility: format!("Category {label}"),
        label,
        active,
    }
}

pub(crate) fn filter_chip(category: Category) -> FilterChip {
    let label = category_label(category);
    FilterChip {
        category,
        help: format!("Showing only {label}. Click to show all"),
        accessibility: format!("Category filter {label}, clear"),
        label,
    }
}

/// The triangle: a torrent with several files, or one that waits for a choice (selection mode, SPEC 8.3).
fn disclosure(r: &TransferRow) -> Option<RowDisclosure> {
    if r.awaiting_choice {
        Some(RowDisclosure::Choice)
    } else if r.file_count > 1 && r.kind == TransferKind::Torrent && !is_held(r) {
        Some(RowDisclosure::Files)
    } else {
        None
    }
}

/// A row as `style` draws it.
pub(crate) fn list_row(r: &TransferRow, style: RowStyle, ctx: &RowContext) -> ListRow {
    let mut row = ListRow {
        id: r.id.clone(),
        title: r.display_name.clone(),
        title_help: (r.name != r.display_name).then(|| r.name.clone()),
        icon_ext: icon_ext(r),
        linked: r.kind == TransferKind::Direct,
        dimmed: matches!(style, RowStyle::Queued | RowStyle::Dormant | RowStyle::Stopped),
        attention_dot: r.awaiting_choice,
        pill: category_pill(r.category, ctx.filter),
        status: status_runs(r, style, ctx.show_tracker, ctx.now_ms),
        detail: style.shares().then(|| sharing_totals_line(r)),
        progress: (style == RowStyle::Download && !is_held(r)).then(|| fraction(r)),
        rate: rate_slot(r, style),
        controls: controls(r, style),
        disclosure: disclosure(r),
        transfer: r.clone(),
        selected: false,
        info_open: false,
        revision: 0,
    };
    row.revision = revision(&row);
    row
}

/// A hash of everything the row shows and carries.
fn revision(row: &ListRow) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&row.id, &row.title, &row.title_help, &row.icon_ext, row.linked, row.dimmed, row.attention_dot).hash(&mut h);
    (&row.pill, &row.status, &row.detail, row.progress.map(f64::to_bits), &row.rate, row.disclosure).hash(&mut h);
    for c in &row.controls {
        (c.kind, &c.title, &c.help, &c.accessibility, c.hover_only, c.keeps_space).hash(&mut h);
        c.ring.map(|g| (g.fraction.to_bits(), g.paused, g.retry)).hash(&mut h);
        format!("{:?}", c.command).hash(&mut h);
    }
    hash_transfer(&row.transfer, &mut h);
    h.finish()
}

fn hash_transfer(t: &TransferRow, h: &mut impl Hasher) {
    (&t.id, &t.name, &t.display_name, t.kind as u8, t.category as u8, t.size_bytes, t.done_bytes, t.down_rate, t.up_rate).hash(h);
    (t.eta_secs, &t.tracker_id, &t.tracker_name, t.ratio.to_bits(), t.ratio_goal.map(f64::to_bits), t.seed_secs).hash(h);
    (t.state as u8, t.finished_at, &t.save_path, &t.file_ext, t.seed_group.map(|g| g as u8), &t.error, t.retry_in_secs).hash(h);
    (t.moving_percent, t.awaiting_choice, t.is_private, t.pending.map(|p| (p.starts_at_ms, p.remaining_ms, p.paused))).hash(h);
    (t.peers, t.swarm, t.known_peers, t.has_video, &t.video_ext, t.file_count, t.last_activity, &t.waiting_note).hash(h);
    (t.waiting_is_error, t.tracker_halted, t.error_transient, t.announcing_since_ms, t.retry_attempt, t.uploaded_bytes).hash(h);
    (t.files_done, &t.idle_stopped, t.total_bytes, t.held_bytes, t.session_done, t.session_size).hash(h);
}

// MARK: The groups

/// What the lists are filtered by.
pub(crate) struct ListFilter<'a> {
    /// The tracker of the tab; `None`: every tracker.
    pub tracker: Option<&'a str>,
    pub category: Option<Category>,
    /// Lower-cased.
    pub text: String,
}

impl ListFilter<'_> {
    fn matches(&self, r: &TransferRow) -> bool {
        self.tracker.is_none_or(|t| r.tracker_id == t) && self.category.is_none_or(|c| r.category == c) && matches_text(r, &self.text)
    }
}

/// The Sharing groups as the backend counted and paged them for the shown place.
#[derive(Clone, Debug, Default)]
pub(crate) struct SharingRows {
    pub counts: SharingCounts,
    /// The fetched rows of each group that is not folded (a folded group fetches none).
    pub rows: Vec<(SeedGroup, Vec<TransferRow>)>,
}

impl SharingRows {
    pub(crate) fn count(&self, g: SeedGroup) -> u32 {
        let c = &self.counts;
        match g {
            SeedGroup::Popular => c.popular,
            SeedGroup::Active => c.active,
            SeedGroup::Dormant => c.dormant,
            SeedGroup::Error => c.error,
            SeedGroup::Dead => c.dead,
            SeedGroup::Stopped => c.stopped,
        }
    }

    pub(crate) fn fetched(&self, g: SeedGroup) -> &[TransferRow] {
        self.rows.iter().find(|(x, _)| *x == g).map(|(_, v)| v.as_slice()).unwrap_or(&[])
    }
}

/// The longer explanation of a Sharing group, as its header's tooltip.
fn hint(g: SeedGroup) -> Option<String> {
    match g {
        SeedGroup::Dormant => Some("Resting in low power: still announcing on schedule, wakes instantly when a peer asks.".into()),
        SeedGroup::Error => Some("Retrying automatically.".into()),
        SeedGroup::Dead => Some("Nothing is removed automatically.".into()),
        _ => None,
    }
}

/// The key a group's fold is remembered by: "downloads.next", "sharing.dormant"; every tracker tab shares
/// "tracker.<group>".
pub(crate) fn fold_key(page: &str, group: &str) -> String {
    format!("{page}.{group}")
}

/// Builds the groups of one list.
pub(crate) struct Lists<'a> {
    pub ctx: RowContext,
    pub filter: ListFilter<'a>,
    /// The fold keys the user folded.
    pub folded: &'a BTreeSet<String>,
    /// "downloads", "sharing" or "tracker": the first part of the fold keys.
    pub page: &'static str,
}

impl Lists<'_> {
    fn is_folded(&self, key: &str) -> bool {
        self.folded.contains(&fold_key(self.page, key))
    }

    /// A group whose rows are listed in full (the downloads); folded, it ships none.
    fn group(&self, key: &str, title: Option<&str>, style: RowStyle, rows: &[&TransferRow]) -> ListGroup {
        let folded = title.is_some() && self.is_folded(key);
        ListGroup {
            key: key.into(),
            title: title.map(Into::into),
            count: None,
            hint: None,
            tone: if style == RowStyle::Dead { TextTone::Problem } else { TextTone::Secondary },
            block: false,
            parent: None,
            folded,
            fold_help: title.map(|t| if folded { format!("Show {t}") } else { format!("Hide {t}") }),
            action: None,
            more: None,
            rows: if folded { Vec::new() } else { rows.iter().map(|r| list_row(r, style, &self.ctx)).collect() },
        }
    }

    /// A Sharing group: `n` torrents, the fetched ones listed and "Show more" for the rest.
    fn seed_group(&self, g: SeedGroup, sharing: &SharingRows, n: u32) -> ListGroup {
        let key = format!("{g:?}").to_lowercase();
        let fetched: Vec<&TransferRow> = sharing.fetched(g).iter().collect();
        let mut group = self.group(&key, Some(seed_group_name(g)), RowStyle::of(g), &fetched);
        group.count = Some(n.to_string());
        group.hint = hint(g);
        let more = n.saturating_sub(fetched.len() as u32);
        if !group.folded && more > 0 {
            group.more = Some(GroupMore { title: format!("Show {more} more…"), help: format!("Show the other {more} in this list"), group: g });
        }
        group
    }

    /// The Downloads tab: the active downloads (no header), Up Next, and Recently Finished with "Clear"; each only
    /// when it has rows.
    /// The rows of `lists` the filter lets through, in order.
    fn pick<'r>(&self, lists: &[&'r [TransferRow]]) -> Vec<&'r TransferRow> {
        lists.iter().flat_map(|l| l.iter()).filter(|r| self.filter.matches(r)).collect()
    }

    pub(crate) fn downloads(&self, s: &Snapshot) -> Vec<ListGroup> {
        let (active, next, recent) = (self.pick(&[&s.downloading]), self.pick(&[&s.queued]), self.pick(&[&s.recent]));
        let mut groups = Vec::new();
        if !active.is_empty() {
            groups.push(self.group("active", None, RowStyle::Download, &active));
        }
        if !next.is_empty() {
            groups.push(self.group("next", Some("Up Next"), RowStyle::Queued, &next));
        }
        if !recent.is_empty() {
            let mut g = self.group("recent", Some("Recently Finished"), RowStyle::Recent, &recent);
            g.action = Some(GroupAction { title: "Clear".into(), help: "Clear for this whole group".into(), command: ListCommand::ClearRecent });
            groups.push(g);
        }
        groups
    }

    /// The Sharing tab: the five groups of SPEC 8.3 (plus Stopped for paused seeds), each only when it has torrents.
    pub(crate) fn sharing(&self, sharing: &SharingRows) -> Vec<ListGroup> {
        SEED_GROUPS
            .iter()
            .filter_map(|&g| {
                let n = sharing.count(g);
                (n > 0).then(|| self.seed_group(g, sharing, n))
            })
            .collect()
    }

    /// A tracker's tab (SPEC 8.3): a Downloads block (Downloading, Queued, and Recently Finished for downloads that are
    /// not torrents) and a Sharing block (the Sharing groups), each subgroup only when it has rows and each block only
    /// when a subgroup does. A torrent that finished downloading leaves the Downloads block at once and is listed in
    /// its Sharing group, with its upload rate like any other seed. A folded block lists none of its groups.
    pub(crate) fn tracker(&self, s: &Snapshot, sharing: &SharingRows) -> Vec<ListGroup> {
        let downloading = self.pick(&[&s.downloading, &s.hidden_downloading]);
        let queued = self.pick(&[&s.queued, &s.hidden_queued]);
        // A finished torrent is a seed: it is in the Sharing groups, not here. Only what Sharing cannot list (a finished
        // direct download) stays under Recently Finished.
        let finished: Vec<&TransferRow> = self.pick(&[&s.recent, &s.hidden_recent]).into_iter().filter(|r| r.kind != TransferKind::Torrent).collect();
        let mut downloads = Vec::new();
        for (key, title, style, rows) in [
            ("downloading", "Downloading", RowStyle::Download, &downloading),
            ("queued", "Queued", RowStyle::Queued, &queued),
            ("recent", "Recently Finished", RowStyle::Recent, &finished),
        ] {
            if !rows.is_empty() {
                let mut g = self.group(key, Some(title), style, rows);
                g.count = Some(rows.len().to_string());
                g.parent = Some("downloads".into());
                downloads.push(g);
            }
        }
        let mut seeds = Vec::new();
        for g in SEED_GROUPS {
            let n = (sharing.fetched(g).len() as u32).max(sharing.count(g));
            if n > 0 {
                let mut group = self.seed_group(g, sharing, n);
                group.parent = Some("sharing".into());
                seeds.push(group);
            }
        }
        let mut out = self.block("downloads", "Downloads", downloads);
        out.extend(self.block("sharing", "Sharing", seeds));
        out
    }

    fn block(&self, key: &str, title: &str, kids: Vec<ListGroup>) -> Vec<ListGroup> {
        if kids.is_empty() {
            return Vec::new();
        }
        let n: u64 = kids.iter().map(|k| k.count.as_deref().and_then(|c| c.parse::<u64>().ok()).unwrap_or(k.rows.len() as u64)).sum();
        let mut header = self.group(key, Some(title), RowStyle::Download, &[]);
        header.count = Some(n.to_string());
        header.block = true;
        let folded = header.folded;
        let mut out = vec![header];
        if !folded {
            out.extend(kids);
        }
        out
    }
}

/// The structure of a list (see [`ListModel::signature`]).
pub(crate) fn list_signature(chip: Option<&FilterChip>, groups: &[ListGroup], empty: Option<&str>) -> String {
    let mut s = format!("{:?}|{}|", chip.map(|c| c.category), empty.unwrap_or(""));
    for g in groups {
        s.push_str(&format!(
            "[{}:{}{}{}{}{}{}:",
            g.key,
            g.title.is_some() as u8,
            g.folded as u8,
            g.block as u8,
            g.parent.as_deref().unwrap_or(""),
            g.more.is_some() as u8,
            g.action.is_some() as u8
        ));
        for r in &g.rows {
            s.push_str(&r.id);
            s.push('(');
            for c in &r.controls {
                s.push_str(&format!("{:?},", c.kind));
            }
            s.push_str(&format!("{}{}{}{:?})", r.detail.is_some() as u8, r.progress.is_some() as u8, r.rate.is_some() as u8, r.disclosure));
        }
        s.push(']');
    }
    s
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::row_text::tests::row;
    use crate::types::PendingStart;

    /// A finished "Sintel" seeding in Active, 1 GB, ratio 0.94 of 2.0 (the Swift row tests' row).
    pub(crate) fn seed() -> TransferRow {
        let mut r = row("a", TransferState::Seeding);
        r.name = "Sintel.2010.1080p".into();
        r.display_name = "Sintel (2010)".into();
        r.size_bytes = 1_000_000_000;
        r.done_bytes = 1_000_000_000;
        r.total_bytes = 1_000_000_000;
        r.ratio = 0.94;
        r.ratio_goal = Some(2.0);
        r.seed_group = Some(SeedGroup::Active);
        r.finished_at = Some(1);
        r
    }

    /// A download of `seed`'s torrent.
    pub(crate) fn download(state: TransferState) -> TransferRow {
        let mut r = seed();
        r.state = state;
        r.seed_group = None;
        r
    }

    fn line(r: &TransferRow, style: RowStyle) -> String {
        status_line(r, style, true, 0)
    }

    fn text(runs: &[TextRun]) -> String {
        runs.iter().map(|r| r.text.as_str()).collect()
    }

    fn ctx() -> RowContext {
        RowContext { now_ms: 0, show_tracker: true, filter: None }
    }

    fn kinds(r: &ListRow) -> Vec<RowControlKind> {
        r.controls.iter().map(|c| c.kind).collect()
    }

    #[test]
    fn sharing_rows_show_peers_and_ratio_only() {
        // No rate (it has its own slot), tracker, seeding time or swarm.
        let mut r = seed();
        (r.ratio, r.ratio_goal, r.peers, r.known_peers, r.up_rate) = (0.27, None, 1, 250, 1_200_000);
        assert_eq!(line(&r, RowStyle::Active), "1 / 250 peers · Ratio 0.27");
        (r.ratio_goal, r.known_peers) = (Some(1.0), 0);
        assert_eq!(line(&r, RowStyle::Active), "1 / 1 peers · Ratio 0.27 of 1.0");
        let mut d = seed();
        (d.seed_secs, d.ratio, d.ratio_goal) = (3 * 86_400, 1.04, None);
        assert_eq!(line(&d, RowStyle::Dormant), "0 / 0 peers · Ratio 1.04");
        let mut p = seed();
        (p.peers, p.up_rate) = (4, 45_000);
        assert_eq!(line(&p, RowStyle::Popular), "4 / 4 peers · Ratio 0.94 of 2.0");
        // Moving wins.
        let mut m = seed();
        m.moving_percent = Some(42);
        assert_eq!(line(&m, RowStyle::Active), "Moving… 42%");
    }

    #[test]
    fn the_live_rate_has_its_slot_at_the_right_of_the_name() {
        let slot = |r: &TransferRow, s| rate_slot(r, s).map(|x| (x.text, x.active));
        let up = |n| TransferRow { up_rate: n, ..seed() };
        assert_eq!(slot(&up(45_000), RowStyle::Popular), Some(("↑ 45 KB/s".into(), true)));
        assert_eq!(slot(&up(1_200_000), RowStyle::Active), Some(("↑ 1.2 MB/s".into(), true)));
        assert_eq!(slot(&up(0), RowStyle::Active), Some(("–".into(), false)), "idle: still there");
        assert_eq!(slot(&up(1_000), RowStyle::Recent), None);
        assert_eq!(rate_slot(&up(1_000), RowStyle::Stopped).unwrap().help, "Upload speed");
        let down = |state, n| TransferRow { down_rate: n, ..download(state) };
        assert_eq!(slot(&down(TransferState::Downloading, 0), RowStyle::Download), Some(("–".into(), false)));
        assert_eq!(slot(&down(TransferState::Downloading, 2_100_000), RowStyle::Download), Some(("↓ 2.1 MB/s".into(), true)));
        assert_eq!(slot(&down(TransferState::Paused, 2_100_000), RowStyle::Download), Some(("–".into(), false)));
        assert_eq!(slot(&down(TransferState::Queued, 1_000_000), RowStyle::Download), Some(("–".into(), false)), "queued: nothing flows");
        assert_eq!(rate_slot(&down(TransferState::Downloading, 1), RowStyle::Download).unwrap().help, "Download speed");
        let mut moving = up(1_000);
        moving.moving_percent = Some(3);
        assert_eq!(slot(&moving, RowStyle::Active), Some(("–".into(), false)));
        let mut held = down(TransferState::Downloading, 5);
        held.awaiting_choice = true;
        assert_eq!(slot(&held, RowStyle::Download), None);
        held.awaiting_choice = false;
        held.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 1, paused: false });
        assert_eq!(slot(&held, RowStyle::Download), None);
    }

    #[test]
    fn a_download_that_cannot_fit_says_so_and_how_to_retry() {
        let msg = "Not enough space: 4.2 GB still to download, 1.1 GB free";
        let mut r = download(TransferState::Error);
        r.error = Some(msg.into());
        assert_eq!(line(&r, RowStyle::Download), format!("{msg} · Resume to try again"));
        let row = list_row(&r, RowStyle::Download, &ctx());
        assert_eq!(row.status, [TextRun { text: format!("{msg} · Resume to try again"), tone: TextTone::Attention }], "the line is orange");
        assert!(!line(&download(TransferState::Downloading), RowStyle::Download).contains("Resume"));
    }

    #[test]
    fn the_sharing_totals_count_the_whole_torrent() {
        let mut r = seed();
        (r.size_bytes, r.done_bytes, r.total_bytes, r.held_bytes) = (9_800_000, 9_800_000, 17_000_000, 9_800_000);
        (r.file_count, r.files_done, r.uploaded_bytes) = (13, 8, 0);
        assert_eq!(sharing_totals_line(&r), "8 of 13 files · 9.8 MB of 17 MB downloaded · 0 bytes uploaded");
        (r.held_bytes, r.files_done, r.uploaded_bytes) = (17_000_000, 13, 3_000_000_000);
        assert_eq!(sharing_totals_line(&r), "13 of 13 files · 17 MB downloaded · 3.0 GB uploaded");
        // One file leaves the file count out.
        let mut single = seed();
        (single.size_bytes, single.done_bytes, single.total_bytes, single.held_bytes) = (592_000_000, 592_000_000, 592_000_000, 592_000_000);
        single.uploaded_bytes = 3_000_000_000;
        assert_eq!(sharing_totals_line(&single), "592 MB downloaded · 3.0 GB uploaded");
        (single.held_bytes, single.done_bytes) = (100_000_000, 100_000_000);
        assert_eq!(sharing_totals_line(&single), "100 MB of 592 MB downloaded · 3.0 GB uploaded");
        let mut part = seed();
        (part.size_bytes, part.done_bytes, part.total_bytes, part.held_bytes, part.file_count, part.files_done) =
            (592_000_000, 592_000_000, 1_400_000_000, 700_000_000, 24, 4);
        assert!(sharing_totals_line(&part).contains("700 MB of 1.4 GB downloaded"));
        // Only seeds that share have the third line.
        assert!(list_row(&part, RowStyle::Active, &ctx()).detail.is_some());
        assert!(list_row(&part, RowStyle::Error, &ctx()).detail.is_none());
    }

    #[test]
    fn a_stopped_row_drops_the_tracker_and_keeps_the_goal() {
        let mut r = seed();
        (r.state, r.seed_group, r.ratio, r.ratio_goal) = (TransferState::Paused, Some(SeedGroup::Stopped), 0.4, Some(1.0));
        assert_eq!(line(&r, RowStyle::Stopped), "Ratio 0.40 of 1.0 · Stopped");
        r.idle_stopped = Some("0.3 GB uploaded in the last 150 h".into());
        assert_eq!(line(&r, RowStyle::Stopped), "Ratio 0.40 of 1.0 · Stopped — 0.3 GB uploaded in the last 150 h");
    }

    #[test]
    fn downloads_end_with_the_peers_and_leave_the_rate_to_its_slot() {
        let mut r = download(TransferState::Downloading);
        (r.peers, r.known_peers, r.down_rate, r.done_bytes, r.size_bytes, r.eta_secs) = (12, 340, 2_100_000, 73_000_000, 85_000_000, Some(120));
        assert_eq!(line(&r, RowStyle::Download), "73 of 85 MB — 2 minutes remaining · 12 / 340 peers");
        let dl = |peers, known| TransferRow { peers, known_peers: known, ..download(TransferState::Downloading) };
        assert_eq!(live_suffix(&dl(4, 90), RowStyle::Download), "4 / 90 peers");
        assert_eq!(live_suffix(&dl(4, 0), RowStyle::Download), "4 / 4 peers");
        assert_eq!(live_suffix(&dl(0, 0), RowStyle::Download), "0 / 0 peers", "a running download always shows both numbers");
        assert_eq!(live_suffix(&download(TransferState::Paused), RowStyle::Download), "");
        assert_eq!(live_suffix(&TransferRow { peers: 4, ..seed() }, RowStyle::Active), "", "sharing rows have their own line");
        let mut flowing = download(TransferState::Downloading);
        (flowing.peers, flowing.down_rate, flowing.done_bytes, flowing.size_bytes, flowing.eta_secs) = (3, 1_000_000, 500_000_000, 1_000_000_000, Some(500));
        assert!(line(&flowing, RowStyle::Download).contains("remaining"));
    }

    #[test]
    fn a_download_without_peers_says_why() {
        let mut r = download(TransferState::Downloading);
        (r.done_bytes, r.size_bytes) = (0, 1_900_000_000);
        r.waiting_note = Some("Waiting for peers".into());
        assert_eq!(line(&r, RowStyle::Download), "0.0 of 1.9 GB — Waiting for peers · 0 / 0 peers");
        assert!(!line(&r, RowStyle::Download).contains("Calculating"));
    }

    #[test]
    fn a_halted_tracker_is_named_in_red_and_offers_asking_again() {
        let mut r = download(TransferState::Downloading);
        (r.done_bytes, r.size_bytes) = (0, 1_900_000_000);
        (r.waiting_note, r.waiting_is_error, r.tracker_halted) = (Some("Tracker: Unregistered torrent".into()), true, true);
        let row = list_row(&r, RowStyle::Download, &ctx());
        // The reason alone takes the error colour, and the line ends there (no peers).
        assert_eq!(
            row.status,
            [
                TextRun { text: "0.0 of 1.9 GB — ".into(), tone: TextTone::Secondary },
                TextRun { text: "Tracker: Unregistered torrent".into(), tone: TextTone::Problem }
            ]
        );
        assert_eq!(kinds(&row), [RowControlKind::AskAgain, RowControlKind::GetInfo, RowControlKind::PlayWhileDownloading, RowControlKind::PauseRing]);
        let ask = &row.controls[0];
        assert_eq!(ask.title.as_deref(), Some("Ask Again"));
        assert_eq!(ask.help, "Ask the tracker again now. Seedwell never does this by itself after a tracker said no, and asks at most once a minute.");
        assert_eq!(ask.command, Some(RowCommand::AskTrackerAgain { ids: vec!["a".into()] }));
        // A failing (not halted) tracker is calm and says when it retries.
        let mut f = download(TransferState::Downloading);
        (f.done_bytes, f.waiting_note, f.retry_in_secs, f.error_transient, f.retry_attempt) = (0, Some("Bad gateway".into()), Some(7_200), true, 3);
        assert!(line(&f, RowStyle::Download).contains("Bad gateway · retrying in 2 h"));
        assert_eq!(list_row(&f, RowStyle::Download, &ctx()).status.len(), 1, "one calm run");
        assert_eq!(kinds(&list_row(&f, RowStyle::Download, &ctx()))[0], RowControlKind::AskAgain);
        assert_ne!(kinds(&list_row(&download(TransferState::Downloading), RowStyle::Download, &ctx()))[0], RowControlKind::AskAgain);
    }

    #[test]
    fn the_retry_line_is_gone_once_the_engine_reports_no_problem() {
        let mut r = seed();
        (r.seed_group, r.error, r.retry_in_secs, r.error_transient, r.file_ext) = (Some(SeedGroup::Error), Some("Bad gateway".into()), Some(540), true, String::new());
        let row = list_row(&r, RowStyle::Error, &ctx());
        assert_eq!(text(&row.status), "Public · Bad gateway · retrying in 9 min");
        assert_eq!(row.status.len(), 1, "a temporary outage stays in the secondary colour");
        assert_eq!(kinds(&row), [RowControlKind::ErrorBadge]);
        assert_eq!(row.controls[0].help, "Bad gateway");
        // A lasting error is red, without the tracker inside its tab.
        r.error_transient = false;
        let tab = RowContext { show_tracker: false, ..ctx() };
        assert_eq!(list_row(&r, RowStyle::Error, &tab).status[0], TextRun { text: "Bad gateway".into(), tone: TextTone::Problem });
        // The announce worked: no error, no retry, a normal group.
        (r.error, r.retry_in_secs, r.error_transient, r.seed_group) = (None, None, false, Some(SeedGroup::Active));
        assert!(!line(&r, RowStyle::Active).contains("retrying") && !line(&r, RowStyle::Active).contains("Bad gateway"));
        assert_eq!(text(&list_row(&TransferRow { error: None, ..r.clone() }, RowStyle::Error, &ctx()).status), "Public · Problem");
    }

    #[test]
    fn a_dead_torrent_says_gone_in_red_and_offers_removing_it() {
        let mut r = seed();
        r.seed_group = Some(SeedGroup::Dead);
        let row = list_row(&r, RowStyle::Dead, &ctx());
        assert_eq!(row.status, [TextRun { text: "Public · ".into(), tone: TextTone::Secondary }, TextRun { text: "Gone".into(), tone: TextTone::Problem }]);
        assert_eq!(kinds(&row), [RowControlKind::DeadBadge, RowControlKind::AskAgain, RowControlKind::Remove]);
        let remove = &row.controls[2];
        assert_eq!((remove.title.as_deref(), remove.help.as_str()), (Some("Remove…"), "Remove this torrent from Seedwell; asks under its row first, and keeps the files"));
        assert_eq!(remove.command, Some(RowCommand::Remove));
        assert_eq!(row.controls[0].help, "Gone");
    }

    #[test]
    fn a_held_download_waits_for_the_user() {
        let mut c = download(TransferState::Queued);
        c.awaiting_choice = true;
        let row = list_row(&c, RowStyle::Download, &ctx());
        assert_eq!(row.status, [TextRun { text: "Choose what to download…".into(), tone: TextTone::Attention }]);
        assert!(row.attention_dot && row.progress.is_none() && row.rate.is_none());
        assert_eq!(kinds(&row), [RowControlKind::ChoiceActions]);
        assert_eq!(row.controls[0].help, "Start downloading the ticked files. Nothing downloads before this");
        assert_eq!(row.disclosure, Some(RowDisclosure::Choice));
        let mut p = download(TransferState::Queued);
        p.pending = Some(PendingStart { starts_at_ms: 7_000, remaining_ms: 7_000, paused: false });
        let row = list_row(&p, RowStyle::Download, &ctx());
        assert_eq!(text(&row.status), "Starting in 7 s");
        assert_eq!(kinds(&row), [RowControlKind::CountdownActions]);
        assert_eq!(row.controls[0].help, "Skip the wait and start downloading now");
        assert!(!row.attention_dot && row.disclosure.is_none());
        p.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 4_000, paused: true });
        assert_eq!(line(&p, RowStyle::Download), "Paused — won't start until you resume");
    }

    #[test]
    fn a_download_says_what_it_is_doing() {
        let mut r = download(TransferState::Metadata);
        r.done_bytes = 0;
        assert_eq!(line(&r, RowStyle::Download), "Fetching metadata…");
        r.state = TransferState::Checking;
        r.done_bytes = 420_000_000;
        assert_eq!(line(&r, RowStyle::Download), "Checking files… 42%");
        r.state = TransferState::Paused;
        assert_eq!(line(&r, RowStyle::Download), "Paused — 0.4 of 1.0 GB");
        r.state = TransferState::Downloading;
        assert_eq!(line(&r, RowStyle::Download), "0.4 of 1.0 GB — Calculating time remaining… · 0 / 0 peers");
        r.moving_percent = Some(7);
        assert_eq!(line(&r, RowStyle::Download), "Moving… 7% · 0 / 0 peers");
        // An announce in flight for over half a second is said quietly.
        let mut a = download(TransferState::Downloading);
        a.announcing_since_ms = Some(1_000);
        assert!(status_line(&a, RowStyle::Download, true, 1_600).ends_with(" · Announcing…"));
        assert!(!status_line(&a, RowStyle::Download, true, 1_400).contains("Announcing"));
        a.retry_in_secs = Some(0);
        assert!(!status_line(&a, RowStyle::Download, true, 1_600).contains("Announcing"), "the retry note says it instead");
        let mut q = download(TransferState::Queued);
        q.announcing_since_ms = Some(0);
        assert!(!status_line(&q, RowStyle::Queued, true, 9_000).contains("Announcing"));
    }

    #[test]
    fn the_progress_of_a_download_again_counts_the_new_batch() {
        let mut r = download(TransferState::Downloading);
        (r.size_bytes, r.done_bytes, r.session_size, r.session_done, r.down_rate, r.eta_secs) = (800_000_000, 600_000_000, 200_000_000, 0, 1_000_000, Some(240));
        let row = list_row(&r, RowStyle::Download, &RowContext { show_tracker: false, ..ctx() });
        assert_eq!(row.progress, Some(0.0));
        assert!(text(&row.status).starts_with("0.0 of 200 MB"));
        r.session_done = 50_000_000;
        assert_eq!(list_row(&r, RowStyle::Download, &ctx()).progress, Some(0.25));
    }

    #[test]
    fn queued_and_recent_rows() {
        let mut q = download(TransferState::Queued);
        let row = list_row(&q, RowStyle::Queued, &ctx());
        assert_eq!(text(&row.status), "Waiting — 1.0 GB");
        assert!(row.dimmed && row.rate.is_none() && row.progress.is_none());
        assert_eq!(kinds(&row), [RowControlKind::GetInfo]);
        assert!(row.controls[0].hover_only && !row.controls[0].keeps_space);
        q.state = TransferState::Paused;
        q.kind = TransferKind::Direct;
        q.tracker_name = "example.org".into();
        let row = list_row(&q, RowStyle::Queued, &ctx());
        assert_eq!(text(&row.status), "example.org — Paused — 1.0 GB");
        assert!(row.linked);
        let mut done = seed();
        done.finished_at = Some(1_000);
        let row = list_row(&done, RowStyle::Recent, &RowContext { now_ms: 1_000_000 + 12 * 60_000, ..ctx() });
        assert_eq!(text(&row.status), "1.0 GB — 12 minutes ago · Movies");
        assert_eq!(kinds(&row), [RowControlKind::Play, RowControlKind::Reveal]);
        assert_eq!(row.controls[0].help, "Play");
        assert_eq!(row.controls[1].command, Some(RowCommand::Reveal));
        assert!(!row.dimmed);
        done.file_ext = "iso".into();
        assert_eq!(kinds(&list_row(&done, RowStyle::Recent, &ctx())), [RowControlKind::Reveal]);
    }

    #[test]
    fn every_style_has_its_controls() {
        let c = ctx();
        let k = |r: &TransferRow, s| kinds(&list_row(r, s, &c));
        let video = seed();
        let mut iso = seed();
        iso.file_ext = "iso".into();
        assert_eq!(k(&video, RowStyle::Popular), [RowControlKind::Play, RowControlKind::GetInfo]);
        assert_eq!(k(&iso, RowStyle::Active), [RowControlKind::GetInfo]);
        assert_eq!(k(&video, RowStyle::Dormant), [RowControlKind::Play, RowControlKind::LowPower, RowControlKind::GetInfo]);
        assert_eq!(k(&iso, RowStyle::Dormant), [RowControlKind::LowPower, RowControlKind::GetInfo]);
        assert_eq!(k(&video, RowStyle::Stopped), [RowControlKind::Play, RowControlKind::ResumeSharing]);
        let row = list_row(&video, RowStyle::Dormant, &c);
        assert_eq!(row.controls[1].help, "Low power: resting, wakes instantly");
        assert!(row.controls[2].hover_only && row.controls[2].keeps_space, "the info slot keeps its place");
        assert!(row.dimmed);
        let stopped = list_row(&video, RowStyle::Stopped, &c);
        assert_eq!((stopped.controls[1].help.as_str(), stopped.controls[1].command.clone()), ("Resume sharing", Some(RowCommand::Resume)));
        // The pause ring: progress, and what a click does.
        let mut dl = download(TransferState::Downloading);
        dl.done_bytes = 250_000_000;
        let row = list_row(&dl, RowStyle::Download, &c);
        let ring = row.controls.last().unwrap();
        assert_eq!((ring.help.as_str(), ring.command.clone()), ("Pause", Some(RowCommand::Pause)));
        assert_eq!(ring.ring, Some(RingLook { fraction: 0.25, paused: false, retry: false }));
        assert_eq!(row.controls[1].help, "Play while downloading");
        dl.state = TransferState::Paused;
        let row = list_row(&dl, RowStyle::Download, &c);
        assert_eq!(kinds(&row), [RowControlKind::GetInfo, RowControlKind::PauseRing], "no Play beside a resume triangle");
        assert_eq!(row.controls[1].help, "Resume");
        dl.state = TransferState::Error;
        let ring = list_row(&dl, RowStyle::Download, &c).controls.pop().unwrap();
        assert_eq!((ring.help.as_str(), ring.command, ring.ring.map(|g| (g.paused, g.retry))), ("Try again", Some(RowCommand::Resume), Some((true, true))));
    }

    #[test]
    fn a_row_shows_its_name_icon_and_category() {
        let mut r = seed();
        let row = list_row(&r, RowStyle::Active, &ctx());
        assert_eq!((row.title.as_str(), row.title_help.as_deref()), ("Sintel (2010)", Some("Sintel.2010.1080p")));
        assert_eq!(row.icon_ext, "mkv");
        assert_eq!(row.pill, RowPill { category: Category::Movie, label: "Movie".into(), active: false, help: "Show only Movie".into(), accessibility: "Category Movie".into() });
        let filtered = list_row(&r, RowStyle::Active, &RowContext { filter: Some(Category::Movie), ..ctx() });
        assert!(filtered.pill.active);
        assert_eq!(filtered.pill.help, "Showing only Movie. Click to show all");
        r.name = r.display_name.clone();
        assert_eq!(list_row(&r, RowStyle::Active, &ctx()).title_help, None);
        // A folder of videos shows its largest video's icon; a folder without, a folder.
        (r.file_ext, r.has_video, r.video_ext, r.file_count) = (String::new(), true, "mp4".into(), 3);
        let folder = list_row(&r, RowStyle::Active, &ctx());
        assert_eq!((folder.icon_ext.as_str(), folder.disclosure), ("mp4", Some(RowDisclosure::Files)));
        r.has_video = false;
        assert_eq!(list_row(&r, RowStyle::Active, &ctx()).icon_ext, "");
        let chip = filter_chip(Category::Tv);
        assert_eq!((chip.label.as_str(), chip.help.as_str(), chip.accessibility.as_str()), ("TV", "Showing only TV. Click to show all", "Category filter TV, clear"));
    }

    #[test]
    fn the_revision_follows_what_the_row_shows() {
        let r = seed();
        let a = list_row(&r, RowStyle::Active, &ctx()).revision;
        assert_eq!(a, list_row(&r, RowStyle::Active, &ctx()).revision);
        assert_ne!(a, list_row(&TransferRow { up_rate: 5_000, ..r.clone() }, RowStyle::Active, &ctx()).revision);
        assert_ne!(a, list_row(&TransferRow { save_path: "/y".into(), ..r.clone() }, RowStyle::Active, &ctx()).revision, "what only the host's menus use counts too");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_finished_row_shows_it_in_finder() {
        let row = list_row(&seed(), RowStyle::Recent, &ctx());
        assert_eq!(row.controls.last().unwrap().help, "Show in Finder");
    }
}
