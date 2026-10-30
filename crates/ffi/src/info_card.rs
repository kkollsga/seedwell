//! Get Info (SPEC 8.3) as one view model every host draws: the header, the sections (General, Peers, Trackers, Rule
//! and goal) with their labels and values, which rows are shown when, every value's wording (sizes, dates, ratios,
//! peers, the trackers' live status, the save path, the private-tracker notes), the buttons and menus with their help
//! texts, and how often an open card asks again. Rust decides (docs/CROSS-PLATFORM.md §1); the host draws it in its own
//! popover and runs the commands (a core call, the file manager, the pasteboard, a dialog).
//!
//! - [`GetInfoCard`]: one per open card; it reads the torrent when it opens and when [`GetInfoCard::reload`] asks (after a
//!   command, and every 2 s while the card is open).
//! - [`InfoCardModel`]: what the host draws, built for a moment ([`GetInfoCard::model`]) from the row as the list has it
//!   now and the torrent as last read.

use std::sync::{Arc, Mutex};

use trs_control::display;
use trs_platform::file_manager;

use crate::backend::Backend;
use crate::library::{clock_at, LocalClock};
use crate::row_commands::{play_commands, seeding_goal_commands, CommandEntry, PlayOffer};
use crate::row_text::{can_ask_again, category_label};
use crate::types::{TorrentInfo, TrackerLine, TrackerProblem, TransferKind, TransferRow, TransferState};
use crate::Core;

/// How often an open card asks for a new model (the trackers' live status ticks), and how late the timer may fire.
const REFRESH_MS: u32 = 1_000;
const REFRESH_LEEWAY_MS: u32 = 200;
/// How long a read of the torrent is shown before an open card reads it again (off the main thread): its peers, rates,
/// ratio and trackers move.
const READ_EVERY_MS: u64 = 2_000;

/// "Show in Finder", "Show in File Explorer".
const REVEAL: &str = concat!("Show in ", file_manager!());

// MARK: The records

/// The colour of a piece of a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum InfoTone {
    /// The value's own colour.
    Primary,
    /// The calm secondary colour: a note beside the value.
    Secondary,
    /// A problem that needs the user (red).
    Problem,
}

/// A piece of a value in one colour; a value is its runs one after the other.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct InfoRun {
    pub text: String,
    pub tone: InfoTone,
}

/// How a value is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum InfoValueStyle {
    /// Wraps as long as it is.
    Text,
    /// At most three lines, cut in the middle (a release name, a path).
    Long,
    /// A code in a fixed-width font (the info-hash).
    Code,
}

/// What a button of the card does. The host runs it for the card's row and then calls [`GetInfoCard::reload`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum InfoCommand {
    /// Shows the torrent's files in the file manager.
    Reveal,
    /// Asks whether to remove the torrent, under its row (the files stay).
    Remove,
    /// Opens the chooser of what to download.
    ChooseFiles,
    /// A manual re-announce (the tracker's minimum interval still applies).
    AskTrackerAgain,
    /// A re-announce past the tracker's minimum interval (one per tracker per 5 minutes).
    AnnounceAnyway,
    /// The real announce URL of the tracker ([`InfoTracker::index`]) to the pasteboard, and nowhere else.
    CopyTrackerUrl,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct InfoButton {
    /// The button's title; for one drawn as its symbol, its accessibility label.
    pub title: String,
    pub help: String,
    pub command: InfoCommand,
    /// Drawn as a symbol only (the host picks it by the command: Location's Show in Finder).
    pub symbol_only: bool,
    /// Removes something: drawn in the warning colour.
    pub destructive: bool,
    /// The card closes before the command runs (Remove…: the question waits under the row).
    pub closes_card: bool,
}

/// A pop-up menu as a value: its title is what is in force now.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct InfoMenu {
    pub title: String,
    pub help: String,
    pub entries: Vec<CommandEntry>,
}

/// A Play control: a button for an item ("Play"), a pop-up for a submenu ("Play With ▸", a pack's episodes).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct InfoPlayControl {
    pub entry: CommandEntry,
    pub help: String,
}

/// One line of a section: a label and a value (text, a menu, or controls).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct InfoCardLine {
    /// Empty for a note under the line before it.
    pub label: String,
    pub value: Vec<InfoRun>,
    pub style: InfoValueStyle,
    /// The value's hover text.
    pub help: Option<String>,
    /// The value can be selected and copied (not a row of controls).
    pub selectable: bool,
    /// A button after the value (Location: Show in Finder; Files: Choose…).
    pub button: Option<InfoButton>,
    /// The value is a pop-up menu (Then: the seeding goal).
    pub menu: Option<InfoMenu>,
    /// The value is these controls (Play).
    pub play_controls: Vec<InfoPlayControl>,
}

/// One tracker of the torrent: its URL (secret parts masked), its live status and what it reported.
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Record)]
pub struct InfoTracker {
    /// Its place in the torrent's tracker list (for [`InfoCommand::CopyTrackerUrl`]).
    pub index: u32,
    /// One line, cut in the middle; the whole of it is its hover text.
    pub url: String,
    /// "Copy URL": shown while the pointer is over the tracker, and in its context menu.
    pub copy: InfoButton,
    /// What the tracker side is doing now ("Tracker OK · next announce 22:29", "Retrying tracker in 9 min (attempt 3)").
    pub live: String,
    /// While the tracker's own minimum interval holds a user's ask: "Tracker allows the next announce at 22:29".
    pub allowed: Option<String>,
    /// Beside `allowed`: "Announce anyway".
    pub announce_anyway: Option<InfoButton>,
    /// "Announced · last 5 minutes ago · next in 25 min · 120 seeders · 30 leechers".
    pub detail: String,
}

/// A titled group of lines.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct InfoCardSection {
    pub title: String,
    pub rows: Vec<InfoCardLine>,
    /// Trackers: "Ask Tracker Again" above the list, when the trackers failed or halted.
    pub ask_again: Option<InfoButton>,
    /// Beside it: "Announcing stopped until you ask." when every tracker halted.
    pub ask_again_note: Option<String>,
    pub trackers: Vec<InfoTracker>,
}

/// The whole card.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct InfoCardModel {
    pub row_id: String,
    /// The header's file icon: the extension, and a link badge for a direct download.
    pub icon_ext: String,
    pub linked: bool,
    pub title: String,
    /// "Movie · 1.2 GB · 2.1 MB/s from 8 peers" ("from 1 peer").
    pub subtitle: String,
    /// General, Peers, Trackers (when it has any), Rule and goal; none when the torrent could not be read.
    pub sections: Vec<InfoCardSection>,
    /// The bottom line, right-aligned: Show in Finder, Remove….
    pub buttons: Vec<InfoButton>,
    /// Ask again after this long while the card is open, and how late that timer may fire.
    pub refresh_ms: u32,
    pub refresh_leeway_ms: u32,
    /// Read the torrent again ([`GetInfoCard::reload`], off the main thread) after this long; 0: now.
    pub reload_due_ms: u32,
    /// The sections, their lines and the trackers: the same signature, the host may update in place.
    pub signature: String,
}

// MARK: Texts

/// What the State line says.
fn state_label(s: TransferState) -> &'static str {
    match s {
        TransferState::Checking => "Checking files",
        TransferState::Metadata => "Fetching metadata",
        TransferState::Downloading => "Downloading",
        TransferState::Queued => "Waiting",
        TransferState::Paused => "Paused",
        TransferState::Seeding => "Seeding",
        TransferState::LowPower => "Seeding (resting)",
        TransferState::Error => "Error",
        TransferState::MissingData => "Files missing",
        TransferState::Completed => "Finished",
    }
}

/// "256 KB", "4 MB": a piece's size, in binary units with the Mac's adaptive precision (no decimals for KB, at most
/// one for MB, two beyond; no trailing zeros).
pub(crate) fn piece_size(bytes: u32) -> String {
    let n = f64::from(bytes);
    let trim = |v: f64, decimals: usize, unit: &str| {
        let s = format!("{v:.decimals$}");
        let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_owned() } else { s };
        format!("{s} {unit}")
    };
    match bytes {
        0 => "Zero KB".into(),
        1 => "1 byte".into(),
        b if b < 1 << 10 => format!("{b} bytes"),
        b if b < 1 << 20 => trim(n / 1024.0, 0, "KB"),
        b if b < 1 << 30 => trim(n / f64::from(1u32 << 20), 1, "MB"),
        _ => trim(n / f64::from(1u32 << 30), 2, "GB"),
    }
}

/// The host of a (masked) announce URL ("tracker.example"), as written; empty when it has none.
pub(crate) fn url_host(url: &str) -> &str {
    let Some((scheme, rest)) = url.split_once("://") else { return "" };
    if scheme.is_empty() || !scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)) {
        return "";
    }
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    match host.strip_prefix('[') {
        Some(v6) => v6.split_once(']').map_or("", |(h, _)| h),
        None => host.split_once(':').map_or(host, |(h, _)| h),
    }
}

/// What the tracker side is doing now, worked out from absolute times (so a ticking card needs no new read): the words
/// match the plain-language problems.
pub(crate) fn tracker_live(t: &TrackerLine, now_s: u64, clock: &dyn LocalClock) -> String {
    match t.state.as_str() {
        "backoff" => match t.retry_at {
            None => "Tracker not responding".into(),
            Some(retry) if retry <= now_s => "Retrying tracker now…".into(),
            Some(retry) => format!("Retrying tracker in {} (attempt {})", display::until(retry - now_s), t.failures + 1),
        },
        "halted" => "Announcing stopped: the tracker refused this torrent. It is asked again when you ask.".into(),
        "working" | "warning" => match t.next_announce {
            Some(next) => format!("Tracker OK · next announce {}", clock_at(clock, next, now_s)),
            None => "Tracker OK".into(),
        },
        _ => match url_host(&t.url) {
            "" => "Announcing to the tracker…".into(),
            host => format!("Announcing to {host}…"),
        },
    }
}

/// While a user's ask waits for the tracker's own minimum interval: when it allows the next one.
pub(crate) fn tracker_allowed_at(t: &TrackerLine, now_s: u64) -> Option<u64> {
    t.manual_ok_at.filter(|&ok| ok > now_s)
}

/// "? seeders · 1 leecher": what a tracker reported of its swarm.
fn swarm_counts(seeders: Option<u32>, leechers: Option<u32>) -> String {
    let n = |v: Option<u32>, noun: &str| v.map_or_else(|| format!("? {noun}s"), |v| display::count(u64::from(v), noun));
    format!("{} · {}", n(seeders, "seeder"), n(leechers, "leecher"))
}

/// The status, when the tracker was asked last and will be next, and what it reported.
fn tracker_detail(t: &TrackerLine, now_s: u64) -> String {
    let mut chars = t.status.chars();
    let status = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    let mut parts = vec![status];
    if let Some(at) = t.last_announce {
        parts.push(format!("last {}", display::ago(at, now_s, false)));
    }
    if let Some(at) = t.next_announce {
        parts.push(format!("next {}", display::in_future(at, now_s)));
    }
    if t.seeders.is_some() || t.leechers.is_some() {
        parts.push(swarm_counts(t.seeders, t.leechers));
    }
    parts.join(" · ")
}

// MARK: Building

fn run(text: impl Into<String>, tone: InfoTone) -> InfoRun {
    InfoRun { text: text.into(), tone }
}

fn line(label: &str, value: impl Into<String>) -> InfoCardLine {
    toned(label, value, InfoTone::Primary)
}

fn toned(label: &str, value: impl Into<String>, tone: InfoTone) -> InfoCardLine {
    runs(label, vec![run(value, tone)])
}

fn runs(label: &str, value: Vec<InfoRun>) -> InfoCardLine {
    InfoCardLine {
        label: label.into(),
        value: value.into_iter().filter(|r| !r.text.is_empty()).collect(),
        style: InfoValueStyle::Text,
        help: None,
        selectable: true,
        button: None,
        menu: None,
        play_controls: Vec::new(),
    }
}

fn styled(mut r: InfoCardLine, style: InfoValueStyle) -> InfoCardLine {
    r.style = style;
    r
}

fn button(title: &str, help: &str, command: InfoCommand) -> InfoButton {
    InfoButton { title: title.into(), help: help.into(), command, symbol_only: false, destructive: false, closes_card: false }
}

/// What the trackers said, in plain words: what is wrong, which tracker, since when, when Seedwell tries again, and
/// (for a passing outage on a finished torrent) that the files are fine. A passing outage is plain text; only a refusal
/// that needs the user is red.
fn tracker_problem(p: &TrackerProblem, now_s: u64, clock: &dyn LocalClock, out: &mut Vec<InfoCardLine>) {
    out.push(toned("Problem", &p.message, if p.temporary { InfoTone::Primary } else { InfoTone::Problem }));
    if !p.host.is_empty() {
        out.push(line("Tracker", &p.host));
    }
    if let Some(since) = p.since {
        out.push(line("Since", clock_at(clock, since, now_s)));
    }
    if let Some(next) = p.next_try_ms {
        out.push(line("Next try", clock_at(clock, next / 1000, now_s)));
    } else if !p.temporary {
        out.push(toned("Next try", "Not until you ask the tracker again", InfoTone::Secondary));
    }
    if let Some(note) = &p.reassurance {
        out.push(toned("", note, InfoTone::Secondary));
    }
}

fn general(c: &CardInputs, i: &TorrentInfo) -> InfoCardSection {
    let (row, now_s, clock) = (c.row, c.now_s, c.clock);
    let mut rows = vec![line("Title", &row.display_name)];
    if row.name != row.display_name {
        rows.push(styled(line("Release", &row.name), InfoValueStyle::Long));
    }
    if let Some(disk) = &i.error {
        rows.push(toned("Problem", disk, InfoTone::Problem));
    }
    if let Some(p) = &i.tracker_problem {
        tracker_problem(p, now_s, clock, &mut rows);
    }
    rows.push(line("Category", category_label(i.category)));
    rows.push(line("Size", display::bytes(i.size_bytes)));
    rows.push(line("State", state_label(i.state)));
    rows.push(line("Added", clock.date_time(i.added_at)));
    if let Some(f) = i.finished_at {
        rows.push(line("Finished", clock.date_time(f)));
    }
    if i.seeded_secs > 0 {
        rows.push(line("Seeding time", display::duration(i.seeded_secs)));
    }
    if let Some(t) = row.last_activity
        && row.seed_group.is_some()
    {
        rows.push(line("Last active", display::ago(t, now_s, false)));
    }
    let goal = row.ratio_goal.map(|g| format!(" of {}", display::goal(g))).unwrap_or_default();
    rows.push(line("Ratio", display::ratio(i.ratio) + &goal));
    rows.push(line("Uploaded", display::bytes(i.uploaded_total)));
    rows.push(line("Downloaded", display::bytes(i.downloaded_total)));
    let mut location = styled(line("Location", trs_platform::dirs::shown(std::path::Path::new(&i.save_path))), InfoValueStyle::Long);
    location.button = Some(InfoButton { symbol_only: true, ..button(REVEAL, REVEAL, InfoCommand::Reveal) });
    rows.push(location);
    let skipped = if i.skipped_note.is_empty() { String::new() } else { format!(" · {}", i.skipped_note) };
    let mut files = runs("Files", vec![run(format!("{} of {}", i.files_wanted, i.files_total), InfoTone::Primary), run(skipped, InfoTone::Secondary)]);
    if c.chooser_available && i.files_total > 1 {
        files.button = Some(button("Choose…", "Choose which files to download", InfoCommand::ChooseFiles));
    }
    rows.push(files);
    if i.piece_count > 0 {
        rows.push(line("Pieces", format!("{} × {}", i.piece_count, piece_size(i.piece_length))));
    }
    rows.push(styled(line("Info-hash", &i.info_hash), InfoValueStyle::Code));
    rows.push(line("Type", if i.is_private { "Private" } else { "Public" }));
    if !c.play.is_empty() {
        let mut play = runs("Play", Vec::new());
        play.selectable = false;
        play.play_controls = c.play.iter().filter_map(play_control).collect();
        rows.push(play);
    }
    InfoCardSection { title: "General".into(), rows, ask_again: None, ask_again_note: None, trackers: Vec::new() }
}

/// A Play entry as a control: "Play" a button, a submenu a pop-up; separators are not drawn.
fn play_control(e: &CommandEntry) -> Option<InfoPlayControl> {
    let help = match e {
        CommandEntry::Item { title, .. } => format!("Play with {title}"),
        CommandEntry::Submenu { .. } => "Choose what to play".into(),
        CommandEntry::Note { .. } | CommandEntry::Separator => return None,
    };
    Some(InfoPlayControl { entry: e.clone(), help })
}

fn peers(c: &CardInputs, i: &TorrentInfo) -> InfoCardSection {
    let leechers = i.peers.saturating_sub(i.seeds_connected);
    let split = if i.peers > 0 {
        format!(" · {}, {}", display::count(u64::from(i.seeds_connected), "seed"), display::count(u64::from(leechers), "leecher"))
    } else {
        String::new()
    };
    let mut connected = runs("Connected", vec![run(display::peers(i.peers, c.row.known_peers), InfoTone::Primary), run(split, InfoTone::Secondary)]);
    connected.help = Some(display::PEERS_HELP.into());
    let swarm = if i.swarm_seeders.is_some() || i.swarm_leechers.is_some() {
        line("Swarm (tracker)", swarm_counts(i.swarm_seeders, i.swarm_leechers))
    } else {
        toned("Swarm (tracker)", "Not reported by the tracker", InfoTone::Secondary)
    };
    let mut rows = vec![connected, swarm];
    if i.state == TransferState::Downloading {
        rows.push(line("Speed", format!("{} down", display::rate(i.down_rate))));
    }
    InfoCardSection { title: "Peers".into(), rows, ask_again: None, ask_again_note: None, trackers: Vec::new() }
}

fn trackers(c: &CardInputs, i: &TorrentInfo) -> Option<InfoCardSection> {
    if i.trackers.is_empty() {
        return None;
    }
    let ask = (can_ask_again(c.row) || i.tracker_halted).then(|| {
        button(
            "Ask Tracker Again",
            "Ask the tracker again now. Seedwell never does this by itself after a tracker said no, and asks at most once a minute.",
            InfoCommand::AskTrackerAgain,
        )
    });
    let note = (ask.is_some() && i.tracker_halted).then(|| "Announcing stopped until you ask.".to_owned());
    let trackers = i
        .trackers
        .iter()
        .enumerate()
        .map(|(n, t)| {
            let allowed = tracker_allowed_at(t, c.now_s);
            InfoTracker {
                index: n as u32,
                url: t.url.clone(),
                copy: button("Copy URL", "Copy URL", InfoCommand::CopyTrackerUrl),
                live: tracker_live(t, c.now_s, c.clock),
                allowed: allowed.map(|ok| format!("Tracker allows the next announce at {}", clock_at(c.clock, ok, c.now_s))),
                announce_anyway: allowed.map(|_| {
                    button(
                        "Announce anyway",
                        "Ask now, ignoring the wait the tracker set. Allowed once per tracker per 5 minutes.",
                        InfoCommand::AnnounceAnyway,
                    )
                }),
                detail: tracker_detail(t, c.now_s),
            }
        })
        .collect();
    Some(InfoCardSection { title: "Trackers".into(), rows: Vec::new(), ask_again: ask, ask_again_note: note, trackers })
}

fn goal(c: &CardInputs, i: &TorrentInfo) -> InfoCardSection {
    let mut then = runs("Then", Vec::new());
    then.selectable = false;
    then.menu = Some(InfoMenu {
        title: i.seeding_goal.clone(),
        help: "Choose what happens when this torrent is done downloading".into(),
        // The row menu's "Seeding Goal ▸", entry for entry.
        entries: seeding_goal_commands(c.row.clone(), Some(i.clone())),
    });
    let mut rows = vec![line("Rule", &i.rule_name), then];
    if let Some(idle) = &i.idle_text {
        rows.push(line("Inactive", idle));
    }
    InfoCardSection { title: "Rule and goal".into(), rows, ask_again: None, ask_again_note: None, trackers: Vec::new() }
}

/// What one card is built from.
pub(crate) struct CardInputs<'a> {
    /// The row as the list has it now.
    pub row: &'a TransferRow,
    /// The torrent as last read; `None` when it could not be.
    pub info: Option<&'a TorrentInfo>,
    /// The Play entries of the row (empty: nothing to play).
    pub play: &'a [CommandEntry],
    /// The engine can hold a torrent for a choice and reopen it ("Choose…").
    pub chooser_available: bool,
    pub now_s: u64,
    pub clock: &'a dyn LocalClock,
}

/// The card for `c`.
pub(crate) fn build(c: &CardInputs) -> InfoCardModel {
    let row = c.row;
    let mut subtitle = vec![category_label(row.category), display::bytes(row.size_bytes)];
    match c.info {
        Some(i) if row.state == TransferState::Downloading => {
            subtitle.push(format!("{} from {}", display::rate(i.down_rate), display::count(u64::from(i.peers), "peer")));
        }
        Some(i) if row.seed_group.is_some() => subtitle.push(i.tracker_name.clone()),
        _ => {}
    }
    let sections: Vec<InfoCardSection> = match c.info {
        Some(i) => [Some(general(c, i)), Some(peers(c, i)), trackers(c, i), Some(goal(c, i))].into_iter().flatten().collect(),
        None => Vec::new(),
    };
    let buttons = vec![
        button(REVEAL, concat!("Show the downloaded files in ", file_manager!()), InfoCommand::Reveal),
        InfoButton {
            destructive: true,
            closes_card: true,
            ..button("Remove…", "Remove this torrent from Seedwell; asks under its row first, and keeps the files", InfoCommand::Remove)
        },
    ];
    let signature = sections
        .iter()
        .map(|s| {
            let rows: Vec<&str> = s.rows.iter().map(|r| r.label.as_str()).collect();
            format!("{}[{}]{}{}", s.title, rows.join(","), s.trackers.len(), if s.ask_again.is_some() { "+ask" } else { "" })
        })
        .collect::<Vec<_>>()
        .join("|");
    InfoCardModel {
        row_id: row.id.clone(),
        icon_ext: row.file_ext.clone(),
        linked: row.kind == TransferKind::Direct,
        title: row.display_name.clone(),
        subtitle: subtitle.join(" · "),
        sections,
        buttons,
        refresh_ms: REFRESH_MS,
        refresh_leeway_ms: REFRESH_LEEWAY_MS,
        reload_due_ms: READ_EVERY_MS as u32,
        signature,
    }
}

// MARK: The object

/// The torrent as last read, and when it was shown first (unix ms; `None` until a model shows it).
struct Read {
    info: Option<TorrentInfo>,
    shown_ms: Option<u64>,
}

/// One open Get Info card: the torrent as last read, and the Play entries of its row when it opened.
#[derive(uniffi::Object)]
pub struct GetInfoCard {
    backend: Arc<dyn Backend>,
    clock: Arc<dyn LocalClock>,
    row_id: String,
    chooser_available: bool,
    play: Vec<CommandEntry>,
    read: Mutex<Read>,
}

impl GetInfoCard {
    pub(crate) fn with_backend(
        backend: Arc<dyn Backend>,
        row: &TransferRow,
        clock: Arc<dyn LocalClock>,
        chooser_available: bool,
        play: Option<PlayOffer>,
    ) -> Arc<Self> {
        let play = play.map(|offer| play_commands(row.clone(), offer)).unwrap_or_default();
        let read = Mutex::new(Read { info: backend.info(&row.id).ok(), shown_ms: None });
        Arc::new(Self { backend, clock, row_id: row.id.clone(), chooser_available, play, read })
    }

    /// The torrent as last read, and how long until it is read again (a read counts from the first model that shows it).
    fn info(&self, now_ms: u64) -> (Option<TorrentInfo>, u32) {
        let mut r = self.read.lock().unwrap_or_else(|e| e.into_inner());
        let shown = *r.shown_ms.get_or_insert(now_ms);
        let due = (shown + READ_EVERY_MS).saturating_sub(now_ms).min(READ_EVERY_MS);
        (r.info.clone(), due as u32)
    }
}

#[uniffi::export]
impl GetInfoCard {
    /// The card of `row` as it opens: reads the torrent. `chooser_available`: the engine can hold a torrent for a
    /// choice and reopen it; `play`: what the row's Play entries are made of (`None` or no videos: no Play line).
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>, row: TransferRow, clock: Arc<dyn LocalClock>, chooser_available: bool, play: Option<PlayOffer>) -> Arc<Self> {
        Self::with_backend(core.backend.clone(), &row, clock, chooser_available, play)
    }

    /// The card now, for `row` as the list has it now (it must be the card's row). Cheap: nothing is read; the host asks
    /// again after `refresh_ms` while the card is open, and after every change it makes ([`GetInfoCard::reload`] first),
    /// and reads the torrent again off the main thread once `reload_due_ms` has passed.
    pub fn model(&self, row: TransferRow, now_unix_ms: u64) -> InfoCardModel {
        let (info, due) = self.info(now_unix_ms);
        let mut m = build(&CardInputs {
            row: &row,
            info: info.as_ref(),
            play: &self.play,
            chooser_available: self.chooser_available,
            now_s: now_unix_ms / 1000,
            clock: self.clock.as_ref(),
        });
        m.reload_due_ms = due;
        m
    }

    /// Reads the torrent again: after a command of the card (a goal set, a tracker asked), and while it is open when
    /// [`InfoCardModel::reload_due_ms`] says so (a slow read: off the main thread).
    pub fn reload(&self) {
        let info = self.backend.info(&self.row_id).ok();
        *self.read.lock().unwrap_or_else(|e| e.into_inner()) = Read { info, shown_ms: None };
    }
}

#[cfg(test)]
#[path = "info_card_tests.rs"]
mod tests;
