//! The commands of a torrent row (SPEC §8.5) as a list every host draws: the row's right-click menu in the window
//! and in the menu bar menu, the window's context menu over a selection, the Windows tray flyout. Rust decides
//! (docs/CROSS-PLATFORM.md §1) which commands there are and in what order, their titles, checks, enabled state, the
//! destructive one, the shortcut and the submenus; the host draws the list and runs each [`RowCommand`] (a core call,
//! or its own service: the file manager, the pasteboard, a window or a dialog).
//!
//! - [`row_commands`]: the whole menu of one row or a selection.
//! - [`play_commands`]: its Play part on its own (Get Info's Play controls, a video's menu in the file list).
//! - [`seeding_goal_commands`]: its Seeding Goal part on its own (Get Info's "Then" menu).
//! - [`capable_players`], [`playable_videos`], [`play_episode_label`]: what those Play entries are made of.

use trs_platform::file_manager;

use crate::play::is_mp4_family;
use crate::row_text::{can_ask_again, category_label, is_finished_direct_video, is_held};
use crate::settings::RuleRecord;
use crate::types::{
    Category, Priority, SeedingGoal, TorrentFile, TorrentInfo, TransferKind, TransferRow, TransferState,
};

/// What a command of the list does. Each applies to the rows the list was made for, unless it names its own.
///
/// (No variant shares a name with a field of another: in C# each variant is a nested record and each field a
/// property of it.)
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum RowCommand {
    /// A manual re-announce of these rows (the ones whose tracker said no or cannot be reached).
    AskTrackerAgain { ids: Vec<String> },
    /// "Start" on a torrent that waits for a choice: the choice as it stands (everything unless changed).
    ConfirmChoice,
    /// "Start" on a torrent counting down to its start: it starts now.
    StartNow,
    /// "Cancel" on a held torrent: removed (its `.torrent` file to the Trash unless the host keeps it).
    CancelHeld,
    /// Opens the chooser of what to download ("Choose Files…" of a torrent waiting for a choice, "Files…").
    OpenChooser,
    /// "Choose Files…" of a countdown: ends the countdown, keeps it held and opens the chooser (`pending_choose`).
    ChooseFiles,
    Resume,
    Pause,
    StopSharing,
    MoveToTop,
    /// Shows the rows in the file manager.
    Reveal,
    /// Opens the row's Get Info.
    GetInfo,
    /// Asks for a folder and moves the rows below it.
    MoveTo,
    SetSeedingGoal { goal: SeedingGoal },
    /// Asks for a ratio ("Seed until ratio…") and sets it as the goal.
    CustomGoal,
    SetPriority { priority: Priority },
    SetCategory { category: Category },
    /// Forces a rule by id; `None` lets the rules decide again ("Automatic, by tracker").
    SetRule { rule_id: Option<String> },
    /// Opens the rules in Settings.
    EditRules,
    Verify,
    CopyMagnet,
    /// Asks whether to remove the rows (their files stay).
    Remove,
    /// Asks whether to remove the rows and delete their files.
    RemoveAndDeleteFiles,
    /// Plays a file of the row: `file_index` in the torrent's file list, `None` for a finished direct download's own
    /// file. `player_id` is a chosen player (Play With), `None` lets the setting decide.
    Play { file_index: Option<u32>, player_id: Option<String> },
    /// The Play button of a row in the window: the row's main video, in the player the setting picks (the host finds
    /// the file: the largest video, or a finished direct download's own file).
    PlayRow,
}

/// One line of the list.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum CommandEntry {
    Item {
        command: RowCommand,
        title: String,
        /// Drawn with a check mark.
        checked: bool,
        enabled: bool,
        /// Removes or deletes something: drawn in the warning colour.
        destructive: bool,
        /// A letter pressed with the system's command key (⌘ on the Mac, Ctrl on Windows), shown beside the title.
        shortcut: Option<String>,
    },
    /// A line that only informs ("Detected: Movie", "No Player Installed"): drawn disabled, does nothing.
    Note { title: String },
    Submenu { title: String, entries: Vec<CommandEntry> },
    Separator,
}

/// A video player the host found on this computer.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PlayerOption {
    /// What `RowCommand::Play` names it by (the Mac: its bundle identifier).
    pub id: String,
    pub name: String,
    /// It starts on this computer (the Mac: a slice for this CPU).
    pub runs: bool,
    /// It plays only MP4, M4V and MOV (QuickTime Player).
    pub mp4_only: bool,
}

/// What the Play entries of one row are made of.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct PlayOffer {
    /// The row's videos that can be played ([`playable_videos`] of its files), in the list's order. Empty for a
    /// direct download, and for a torrent without videos.
    pub videos: Vec<TorrentFile>,
    /// The players found on this computer, in the order of "Automatic".
    pub players: Vec<PlayerOption>,
}

/// "Show in Finder", "Show in File Explorer".
const REVEAL: &str = concat!("Show in ", file_manager!());

/// The most episodes a Play submenu lists; the rest are in Get Info.
const EPISODES_SHOWN: usize = 40;

fn item(title: impl Into<String>, command: RowCommand) -> CommandEntry {
    checked(title, false, command)
}

fn checked(title: impl Into<String>, on: bool, command: RowCommand) -> CommandEntry {
    CommandEntry::Item { command, title: title.into(), checked: on, enabled: true, destructive: false, shortcut: None }
}

fn note(title: impl Into<String>) -> CommandEntry {
    CommandEntry::Note { title: title.into() }
}

fn submenu(title: impl Into<String>, entries: Vec<CommandEntry>) -> CommandEntry {
    CommandEntry::Submenu { title: title.into(), entries }
}

const PRIORITIES: [Priority; 3] = [Priority::High, Priority::Normal, Priority::Low];

/// The order of the Category submenu.
const CATEGORIES: [Category; 8] = [
    Category::Movie,
    Category::Tv,
    Category::Music,
    Category::Audiobook,
    Category::Book,
    Category::Software,
    Category::Game,
    Category::Other,
];

fn priority_title(p: Priority) -> &'static str {
    match p {
        Priority::High => "★ High",
        Priority::Normal => "Normal",
        Priority::Low => "Low",
    }
}

/// The commands of `rows` (one row, or a selection: the first row decides what kind it is). `info` is the first
/// row's Get Info (a single row only: its priority, goal and rule are checked), `rules` the rule list (the enabled
/// ones are offered), `play` what the Play entries are made of (a single row only). `in_menu_bar`: the menu bar's
/// version ("Move to Top of Queue", "Files…", no "Move to…"); `chooser_available`: the engine can hold a torrent
/// for a choice and reopen it.
#[uniffi::export]
pub fn row_commands(
    rows: Vec<TransferRow>,
    info: Option<TorrentInfo>,
    rules: Vec<RuleRecord>,
    play: Option<PlayOffer>,
    in_menu_bar: bool,
    chooser_available: bool,
) -> Vec<CommandEntry> {
    let Some(first) = rows.first() else { return vec![] };
    let single = rows.len() == 1;
    let sharing = first.seed_group.is_some();
    let info = if single { info } else { None };
    let mut out = Vec::new();

    // Play first (SPEC §7F.3): only for a single torrent that holds a video selected for download.
    if single && let Some(offer) = &play {
        let entries = play_entries(first, offer);
        if !entries.is_empty() {
            out.extend(entries);
            out.push(CommandEntry::Separator);
        }
    }

    // Actions
    let ask: Vec<String> = rows.iter().filter(|r| can_ask_again(r)).map(|r| r.id.clone()).collect();
    if !ask.is_empty() {
        out.push(item("Ask Tracker Again", RowCommand::AskTrackerAgain { ids: ask }));
    }
    let paused = first.state == TransferState::Paused;
    if first.awaiting_choice {
        if single {
            out.push(item("Start", RowCommand::ConfirmChoice));
            out.push(item("Cancel", RowCommand::CancelHeld));
        }
        out.push(item("Choose Files…", RowCommand::OpenChooser));
    } else if first.pending.is_some() && single {
        out.push(item("Start", RowCommand::StartNow));
        out.push(item("Cancel", RowCommand::CancelHeld));
        out.push(item("Choose Files…", RowCommand::ChooseFiles));
    } else if sharing {
        out.push(if paused {
            item("Resume Sharing", RowCommand::Resume)
        } else {
            item("Stop Sharing", RowCommand::StopSharing)
        });
    } else {
        out.push(if paused { item("Resume", RowCommand::Resume) } else { item("Pause", RowCommand::Pause) });
        if single {
            out.push(item(if in_menu_bar { "Move to Top of Queue" } else { "Move to Top" }, RowCommand::MoveToTop));
        }
    }
    out.push(CommandEntry::Separator);

    // Reveal / info
    out.push(item(REVEAL, RowCommand::Reveal));
    if single {
        out.push(CommandEntry::Item {
            command: RowCommand::GetInfo,
            title: "Get Info".into(),
            checked: false,
            enabled: true,
            destructive: false,
            shortcut: Some("I".into()),
        });
    }
    if !in_menu_bar && !is_held(first) {
        out.push(item("Move to…", RowCommand::MoveTo));
    }
    out.push(CommandEntry::Separator);

    // Settings submenus
    if sharing {
        out.push(goal_menu(first, info.as_ref(), single));
    }
    let priority = info.as_ref().map(|i| i.priority);
    out.push(submenu(
        "Priority",
        PRIORITIES
            .iter()
            .map(|&p| checked(priority_title(p), priority == Some(p), RowCommand::SetPriority { priority: p }))
            .collect(),
    ));
    out.push(category_menu(first, single));
    out.push(rule_menu(&rules, info.as_ref().map(|i| i.rule_name.as_str())));
    if in_menu_bar && chooser_available && !sharing && single && first.pending.is_none() {
        out.push(item("Files…", RowCommand::OpenChooser));
    }
    out.push(CommandEntry::Separator);

    // Utilities, then destructive last
    if sharing {
        out.push(item("Verify Files", RowCommand::Verify));
    }
    if single && first.kind == TransferKind::Torrent {
        out.push(item("Copy Magnet Link", RowCommand::CopyMagnet));
    }
    out.push(CommandEntry::Separator);
    out.push(item("Remove…", RowCommand::Remove));
    out.push(CommandEntry::Item {
        command: RowCommand::RemoveAndDeleteFiles,
        title: "Remove and Delete Files…".into(),
        checked: false,
        enabled: true,
        destructive: true,
        shortcut: None,
    });
    tidy(out)
}

/// No separator at either end or right after another (a direct download has no "Copy Magnet Link", a rule list may
/// be empty), in the submenus too.
fn tidy(entries: Vec<CommandEntry>) -> Vec<CommandEntry> {
    let mut out: Vec<CommandEntry> = Vec::with_capacity(entries.len());
    for e in entries {
        match e {
            CommandEntry::Separator if matches!(out.last(), None | Some(CommandEntry::Separator)) => {}
            CommandEntry::Submenu { title, entries } => {
                out.push(CommandEntry::Submenu { title, entries: tidy(entries) });
            }
            e => out.push(e),
        }
    }
    if matches!(out.last(), Some(CommandEntry::Separator)) {
        out.pop();
    }
    out
}

/// "Seeding Goal ▸" of a sharing row: the rule's default or the user's own goal, checked for a single row.
fn goal_menu(first: &TransferRow, info: Option<&TorrentInfo>, single: bool) -> CommandEntry {
    submenu("Seeding Goal", goal_entries(first, info, single))
}

/// The seeding goals one can choose, the one in force checked for a single row: the rule's default, or the user's own
/// goal (Ratio 1.0, 3.0, Seed Forever; a custom ratio checks none of them).
fn goal_entries(first: &TransferRow, info: Option<&TorrentInfo>, single: bool) -> Vec<CommandEntry> {
    let custom = single && info.is_some_and(|i| i.goal_is_custom);
    let goal = first.ratio_goal;
    let set = |goal| RowCommand::SetSeedingGoal { goal };
    vec![
        checked("Rule default", single && !custom, set(SeedingGoal::RuleDefault)),
        CommandEntry::Separator,
        checked("Ratio 1.0", custom && goal == Some(1.0), set(SeedingGoal::Ratio { ratio: 1.0 })),
        checked("Ratio 3.0", custom && goal == Some(3.0), set(SeedingGoal::Ratio { ratio: 3.0 })),
        checked("Seed Forever", custom && goal.is_none(), set(SeedingGoal::Forever)),
        CommandEntry::Separator,
        item("Custom…", RowCommand::CustomGoal),
    ]
}

/// The entries of one row's "Seeding Goal ▸" on their own: Get Info's "Then" menu is the row menu's, entry for entry
/// (`info`: the row's Get Info, which says whether the goal is the user's own).
#[uniffi::export]
pub fn seeding_goal_commands(row: TransferRow, info: Option<TorrentInfo>) -> Vec<CommandEntry> {
    goal_entries(&row, info.as_ref(), true)
}

/// "Category: Movie ▸" (a single row) or "Category ▸" (a selection).
fn category_menu(first: &TransferRow, single: bool) -> CommandEntry {
    let current = category_label(first.category);
    let mut entries: Vec<CommandEntry> = CATEGORIES
        .iter()
        .map(|&c| checked(category_label(c), single && c == first.category, RowCommand::SetCategory { category: c }))
        .collect();
    if single {
        entries.push(CommandEntry::Separator);
        entries.push(note(format!("Detected: {current}")));
    }
    submenu(if single { format!("Category: {current}") } else { "Category".into() }, entries)
}

/// "Rule: Default ▸": automatic, or one of the enabled rules forced; `current` is the rule that governs the row now.
fn rule_menu(rules: &[RuleRecord], current: Option<&str>) -> CommandEntry {
    let mut entries = vec![
        checked("Automatic, by tracker", current == Some("Default"), RowCommand::SetRule { rule_id: None }),
        CommandEntry::Separator,
    ];
    for r in rules.iter().filter(|r| r.enabled) {
        entries.push(checked(r.name.clone(), current == Some(r.name.as_str()), RowCommand::SetRule {
            rule_id: Some(r.id.clone()),
        }));
    }
    entries.push(CommandEntry::Separator);
    entries.push(item("Edit Rules…", RowCommand::EditRules));
    submenu(format!("Rule: {}", current.unwrap_or("Default")), entries)
}

// MARK: Play

/// "Play" and "Play With ▸" of one row: empty when it has nothing to play. A finished direct download plays its own
/// file; a torrent plays its videos (one: a plain "Play"; several: a submenu of episodes).
#[uniffi::export]
pub fn play_commands(row: TransferRow, offer: PlayOffer) -> Vec<CommandEntry> {
    play_entries(&row, &offer)
}

fn play_entries(row: &TransferRow, offer: &PlayOffer) -> Vec<CommandEntry> {
    let play = |file_index: Option<u32>, player_id: Option<String>| RowCommand::Play { file_index, player_id };
    let nobody = || submenu("Play With", vec![note("No Player Installed")]);
    if is_finished_direct_video(row) {
        let file_name = row.save_path.rsplit('/').next().unwrap_or_default();
        let capable = capable(&offer.players, is_mp4_family(file_name));
        let with = if capable.is_empty() {
            nobody()
        } else {
            submenu("Play With", capable.iter().map(|p| item(p.name.clone(), play(None, Some(p.id.clone())))).collect())
        };
        return vec![item("Play", play(None, None)), with];
    }
    let videos = &offer.videos;
    let Some(v0) = videos.first() else { return vec![] };
    let shown = &videos[..videos.len().min(EPISODES_SHOWN)];
    let episodes = |player: Option<&PlayerOption>| -> Vec<CommandEntry> {
        let mut out: Vec<CommandEntry> = shown
            .iter()
            .map(|f| item(episode_label(&f.name), play(Some(f.index), player.map(|p| p.id.clone()))))
            .collect();
        if videos.len() > shown.len() {
            out.push(note(format!("+ {} more in Get Info", videos.len() - shown.len())));
        }
        out
    };
    // Play With lists the players that can run here and play the first video's container.
    let capable = capable(&offer.players, v0.is_mp4_family);
    let with = if capable.is_empty() {
        nobody()
    } else if videos.len() == 1 {
        submenu(
            "Play With",
            capable.iter().map(|p| item(p.name.clone(), play(Some(v0.index), Some(p.id.clone())))).collect(),
        )
    } else {
        submenu("Play With", capable.iter().map(|p| submenu(p.name.clone(), episodes(Some(p)))).collect())
    };
    let first =
        if videos.len() == 1 { item("Play", play(Some(v0.index), None)) } else { submenu("Play", episodes(None)) };
    vec![first, with]
}

fn capable(players: &[PlayerOption], mp4_family: bool) -> Vec<PlayerOption> {
    players.iter().filter(|p| p.runs && (!p.mp4_only || mp4_family)).cloned().collect()
}

/// The players that can run here and play a file of this container (`mp4_family`: MP4, M4V, MOV), in the order
/// they were given (the order of "Automatic").
#[uniffi::export]
pub fn capable_players(players: Vec<PlayerOption>, mp4_family: bool) -> Vec<PlayerOption> {
    capable(&players, mp4_family)
}

/// The files of a torrent that Play offers: videos selected for download.
#[uniffi::export]
pub fn playable_videos(files: Vec<TorrentFile>) -> Vec<TorrentFile> {
    files.into_iter().filter(|f| f.is_video && f.wanted).collect()
}

/// What an episode is called in the Play menus: "S01E02" when the file name has it, else the name without its
/// extension.
#[uniffi::export]
pub fn play_episode_label(name: String) -> String {
    episode_label(&name)
}

fn episode_label(name: &str) -> String {
    let base = match name.rfind('.') {
        Some(dot) if dot > 0 => &name[..dot],
        _ => name,
    };
    episode_code(base).map_or_else(|| base.to_string(), |code| code.to_ascii_uppercase())
}

/// The first `S01E02` (also `s1e2`, `S01E02E03`, `S01E02-E03`) in `s`: 1 or 2 season digits, 1 to 3 episode digits.
fn episode_code(s: &str) -> Option<&str> {
    let b = s.as_bytes();
    let digits = |from: usize, max: usize| b[from..].iter().take(max).take_while(|c| c.is_ascii_digit()).count();
    let is = |i: usize, c: u8| b.get(i).is_some_and(|x| x.eq_ignore_ascii_case(&c));
    for start in 0..b.len() {
        if !is(start, b's') {
            continue;
        }
        let season = digits(start + 1, 2);
        if season == 0 || !is(start + 1 + season, b'e') {
            continue;
        }
        let at = start + 2 + season;
        let episode = digits(at, 3);
        if episode == 0 {
            continue;
        }
        let mut end = at + episode;
        // A second episode: "E03" or "-E03".
        let dash = usize::from(b.get(end) == Some(&b'-'));
        if is(end + dash, b'e') {
            let more = digits(end + dash + 1, 3);
            if more > 0 {
                end += dash + 1 + more;
            }
        }
        return Some(&s[start..end]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::row_text::tests::row;
    use crate::settings::PrivateMatch;
    use crate::types::{FilePriority, PendingStart, SeedGroup};

    fn titles(entries: &[CommandEntry]) -> String {
        entries
            .iter()
            .map(|e| match e {
                // The file manager's name differs per system.
                CommandEntry::Item { title, .. } if title == REVEAL => "Reveal".into(),
                CommandEntry::Item { title, checked: true, .. } => format!("{title} ✓"),
                CommandEntry::Item { title, enabled: false, .. } | CommandEntry::Note { title } => {
                    format!("({title})")
                }
                CommandEntry::Item { title, .. } => title.clone(),
                CommandEntry::Submenu { title, .. } => format!("{title} >"),
                CommandEntry::Separator => "-".into(),
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn sub<'a>(entries: &'a [CommandEntry], prefix: &str) -> &'a [CommandEntry] {
        entries
            .iter()
            .find_map(|e| match e {
                CommandEntry::Submenu { title, entries } if title.starts_with(prefix) => Some(entries.as_slice()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no submenu {prefix}"))
    }

    fn command<'a>(entries: &'a [CommandEntry], name: &str) -> &'a RowCommand {
        entries
            .iter()
            .find_map(|e| match e {
                CommandEntry::Item { title, command, .. } if title == name => Some(command),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no item {name}"))
    }

    fn info(priority: Priority, rule: &str, custom: bool) -> TorrentInfo {
        TorrentInfo {
            id: "a".into(),
            name: "a".into(),
            display_name: "a".into(),
            category: Category::Movie,
            size_bytes: 100,
            peers: 0,
            down_rate: 0,
            save_path: "/x".into(),
            priority,
            seeding_goal: String::new(),
            idle_text: None,
            files_wanted: 1,
            files_total: 1,
            skipped_note: String::new(),
            rule_name: rule.into(),
            tracker_name: "Public".into(),
            error: None,
            tracker_problem: None,
            tracker_halted: false,
            goal_is_custom: custom,
            state: TransferState::Seeding,
            added_at: 0,
            finished_at: None,
            seeded_secs: 0,
            ratio: 0.0,
            uploaded_total: 0,
            downloaded_total: 0,
            piece_count: 1,
            piece_length: 16_384,
            info_hash: "a".into(),
            is_private: false,
            seeds_connected: 0,
            swarm_seeders: None,
            swarm_leechers: None,
            trackers: vec![],
        }
    }

    fn rule(id: &str, name: &str, enabled: bool) -> RuleRecord {
        RuleRecord {
            id: id.into(),
            name: name.into(),
            enabled,
            tracker_ids: vec![],
            private_match: PrivateMatch::Any,
            categories: vec![],
            goal: None,
            min_seed_secs: None,
            priority: None,
            on_goal: None,
            finished_location: None,
            up_limit: None,
            idle_below_bytes: None,
            idle_hours: 150,
        }
    }

    fn sharing(id: &str, state: TransferState) -> TransferRow {
        let mut r = row(id, state);
        r.seed_group = Some(SeedGroup::Active);
        r
    }

    fn video(index: u32, name: &str) -> TorrentFile {
        TorrentFile {
            index,
            path: name.into(),
            disk_path: String::new(),
            name: name.into(),
            length: 1,
            done: 0,
            uploaded: 0,
            wanted: true,
            priority: FilePriority::Normal,
            is_video: true,
            is_mp4_family: name.ends_with(".mp4"),
        }
    }

    fn player(id: &str, runs: bool, mp4_only: bool) -> PlayerOption {
        PlayerOption { id: id.into(), name: id.to_uppercase(), runs, mp4_only }
    }

    fn one(r: TransferRow, in_menu_bar: bool) -> Vec<CommandEntry> {
        row_commands(vec![r], None, vec![], None, in_menu_bar, true)
    }

    #[test]
    fn a_download_in_the_window_pauses_moves_to_top_and_moves_files() {
        let e = one(row("a", TransferState::Downloading), false);
        assert_eq!(
            titles(&e),
            "Pause|Move to Top|-|Reveal|Get Info|Move to…|-|Priority >|Category: Movie >|Rule: Default >|-|\
             Copy Magnet Link|-|Remove…|Remove and Delete Files…"
        );
        assert_eq!(*command(&e, "Pause"), RowCommand::Pause);
        let CommandEntry::Item { shortcut, .. } = &e[4] else { panic!() };
        assert_eq!(shortcut.as_deref(), Some("I"));
        let CommandEntry::Item { destructive, .. } = e.last().unwrap() else { panic!() };
        assert!(destructive);
        assert!(e.iter().filter(|x| matches!(x, CommandEntry::Item { destructive: true, .. })).count() == 1);
    }

    #[test]
    fn the_menu_bar_says_move_to_top_of_queue_offers_files_and_no_move_to() {
        let e = one(row("a", TransferState::Paused), true);
        assert_eq!(
            titles(&e),
            "Resume|Move to Top of Queue|-|Reveal|Get Info|-|Priority >|Category: Movie >|Rule: Default >|\
             Files…|-|Copy Magnet Link|-|Remove…|Remove and Delete Files…"
        );
        assert_eq!(*command(&e, "Files…"), RowCommand::OpenChooser);
        // Without the chooser there is no "Files…".
        let e = row_commands(vec![row("a", TransferState::Paused)], None, vec![], None, true, false);
        assert!(!titles(&e).contains("|Files…|"));
    }

    #[test]
    fn a_sharing_row_stops_or_resumes_sharing_verifies_and_sets_its_goal() {
        let e = one(sharing("a", TransferState::Seeding), false);
        assert_eq!(
            titles(&e),
            "Stop Sharing|-|Reveal|Get Info|Move to…|-|Seeding Goal >|Priority >|Category: Movie >|\
             Rule: Default >|-|Verify Files|Copy Magnet Link|-|Remove…|Remove and Delete Files…"
        );
        assert_eq!(*command(&e, "Stop Sharing"), RowCommand::StopSharing);
        let e = one(sharing("a", TransferState::Paused), true);
        assert_eq!(*command(&e, "Resume Sharing"), RowCommand::Resume);
        // The menu bar has no "Files…" for a sharing row.
        assert!(!titles(&e).contains("|Files…|"));
    }

    #[test]
    fn the_seeding_goal_checks_the_rule_default_or_the_users_own_goal() {
        let mut r = sharing("a", TransferState::Seeding);
        r.ratio_goal = Some(3.0);
        let goal = |r: &TransferRow, custom: bool| {
            let info = Some(info(Priority::Normal, "Default", custom));
            let e = row_commands(vec![r.clone()], info, vec![], None, false, true);
            titles(sub(&e, "Seeding Goal"))
        };
        assert_eq!(goal(&r, false), "Rule default ✓|-|Ratio 1.0|Ratio 3.0|Seed Forever|-|Custom…");
        assert_eq!(goal(&r, true), "Rule default|-|Ratio 1.0|Ratio 3.0 ✓|Seed Forever|-|Custom…");
        r.ratio_goal = None;
        assert_eq!(goal(&r, true), "Rule default|-|Ratio 1.0|Ratio 3.0|Seed Forever ✓|-|Custom…");
        r.ratio_goal = Some(2.0);
        assert_eq!(goal(&r, true), "Rule default|-|Ratio 1.0|Ratio 3.0|Seed Forever|-|Custom…");
        // Without Get Info the goal counts as the rule's.
        let e = one(r.clone(), false);
        assert_eq!(titles(sub(&e, "Seeding Goal")), "Rule default ✓|-|Ratio 1.0|Ratio 3.0|Seed Forever|-|Custom…");
        assert_eq!(
            *command(sub(&e, "Seeding Goal"), "Ratio 3.0"),
            RowCommand::SetSeedingGoal { goal: SeedingGoal::Ratio { ratio: 3.0 } }
        );
        assert_eq!(*command(sub(&e, "Seeding Goal"), "Custom…"), RowCommand::CustomGoal);
    }

    #[test]
    fn get_infos_goal_menu_is_the_row_menus_entry_for_entry() {
        let mut r = sharing("a", TransferState::Seeding);
        for (goal, custom) in [(Some(1.0), true), (Some(3.0), true), (None, true), (Some(2.0), true), (Some(1.0), false)] {
            r.ratio_goal = goal;
            let i = info(Priority::Normal, "Default", custom);
            let menu = row_commands(vec![r.clone()], Some(i.clone()), vec![], None, false, true);
            assert_eq!(seeding_goal_commands(r.clone(), Some(i)), sub(&menu, "Seeding Goal"), "{goal:?} {custom}");
        }
        // A download's Get Info has the same menu (its row menu has none until it shares).
        let d = TransferRow { seed_group: None, ..sharing("d", TransferState::Downloading) };
        assert_eq!(
            titles(&seeding_goal_commands(d, Some(info(Priority::Normal, "Default", false)))),
            "Rule default ✓|-|Ratio 1.0|Ratio 3.0|Seed Forever|-|Custom…"
        );
    }

    #[test]
    fn several_rows_check_nothing_and_leave_out_the_single_row_commands() {
        let rows = vec![sharing("a", TransferState::Seeding), sharing("b", TransferState::Seeding)];
        let e = row_commands(rows, Some(info(Priority::High, "Default", true)), vec![], None, false, true);
        assert_eq!(
            titles(&e),
            "Stop Sharing|-|Reveal|Move to…|-|Seeding Goal >|Priority >|Category >|Rule: Default >|-|\
             Verify Files|-|Remove…|Remove and Delete Files…"
        );
        assert_eq!(titles(sub(&e, "Seeding Goal")), "Rule default|-|Ratio 1.0|Ratio 3.0|Seed Forever|-|Custom…");
        assert_eq!(titles(sub(&e, "Priority")), "★ High|Normal|Low");
        assert_eq!(titles(sub(&e, "Category")), "Movie|TV|Music|Audiobook|Book|App|Game|Other");
        let downloads = vec![row("a", TransferState::Downloading), row("b", TransferState::Paused)];
        let e = row_commands(downloads, None, vec![], None, false, true);
        assert!(titles(&e).starts_with("Pause|-|Reveal|Move to…|"));
    }

    #[test]
    fn priority_category_and_rule_are_checked_from_get_info() {
        let rules = vec![rule("r1", "Keep long", true), rule("r2", "Off", false), rule("r3", "Movies", true)];
        let mut r = row("a", TransferState::Downloading);
        r.category = Category::Tv;
        let e = row_commands(vec![r], Some(info(Priority::Low, "Movies", false)), rules.clone(), None, false, true);
        assert_eq!(titles(sub(&e, "Priority")), "★ High|Normal|Low ✓");
        assert_eq!(*command(sub(&e, "Priority"), "Low"), RowCommand::SetPriority { priority: Priority::Low });
        assert_eq!(titles(sub(&e, "Category")), "Movie|TV ✓|Music|Audiobook|Book|App|Game|Other|-|(Detected: TV)");
        assert!(e.iter().any(|x| matches!(x, CommandEntry::Submenu { title, .. } if title == "Category: TV")));
        assert_eq!(*command(sub(&e, "Category"), "App"), RowCommand::SetCategory { category: Category::Software });
        assert!(e.iter().any(|x| matches!(x, CommandEntry::Submenu { title, .. } if title == "Rule: Movies")));
        assert_eq!(titles(sub(&e, "Rule")), "Automatic, by tracker|-|Keep long|Movies ✓|-|Edit Rules…");
        assert_eq!(*command(sub(&e, "Rule"), "Keep long"), RowCommand::SetRule { rule_id: Some("r1".into()) });
        assert_eq!(*command(sub(&e, "Rule"), "Automatic, by tracker"), RowCommand::SetRule { rule_id: None });
        assert_eq!(*command(sub(&e, "Rule"), "Edit Rules…"), RowCommand::EditRules);
        let queued = vec![row("a", TransferState::Queued)];
        let e = row_commands(queued, Some(info(Priority::Normal, "Default", false)), rules, None, false, true);
        assert_eq!(titles(sub(&e, "Rule")), "Automatic, by tracker ✓|-|Keep long|Movies|-|Edit Rules…");
    }

    #[test]
    fn a_torrent_waiting_for_a_choice_starts_cancels_or_opens_the_chooser() {
        let mut r = row("a", TransferState::Queued);
        r.awaiting_choice = true;
        let e = one(r.clone(), false);
        assert_eq!(
            titles(&e),
            "Start|Cancel|Choose Files…|-|Reveal|Get Info|-|Priority >|Category: Movie >|Rule: Default >|-|\
             Copy Magnet Link|-|Remove…|Remove and Delete Files…"
        );
        assert_eq!(*command(&e, "Start"), RowCommand::ConfirmChoice);
        assert_eq!(*command(&e, "Cancel"), RowCommand::CancelHeld);
        assert_eq!(*command(&e, "Choose Files…"), RowCommand::OpenChooser);
        // Several: only the chooser of the first.
        let e = row_commands(vec![r.clone(), r.clone()], None, vec![], None, false, true);
        assert!(titles(&e).starts_with("Choose Files…|-|Reveal|-|"));
        // The menu bar also offers "Files…".
        assert!(titles(&one(r, true)).contains("|Files…|"));
    }

    #[test]
    fn a_countdown_starts_now_cancels_or_chooses_files_and_has_no_files_entry() {
        let mut r = row("a", TransferState::Queued);
        r.pending = Some(PendingStart { starts_at_ms: 0, remaining_ms: 7_000, paused: false });
        let e = one(r.clone(), true);
        assert_eq!(
            titles(&e),
            "Start|Cancel|Choose Files…|-|Reveal|Get Info|-|Priority >|Category: Movie >|Rule: Default >|-|\
             Copy Magnet Link|-|Remove…|Remove and Delete Files…"
        );
        assert_eq!(*command(&e, "Start"), RowCommand::StartNow);
        assert_eq!(*command(&e, "Choose Files…"), RowCommand::ChooseFiles);
        // Several countdowns are paused or resumed like downloads.
        let e = row_commands(vec![r.clone(), r], None, vec![], None, false, true);
        assert!(titles(&e).starts_with("Pause|-|Reveal|-|"));
    }

    #[test]
    fn ask_tracker_again_names_the_rows_whose_tracker_failed() {
        let mut halted = row("a", TransferState::Downloading);
        halted.tracker_halted = true;
        let fine = row("b", TransferState::Downloading);
        let mut dead = sharing("c", TransferState::Seeding);
        dead.seed_group = Some(SeedGroup::Dead);
        let e = row_commands(vec![fine.clone(), halted, dead], None, vec![], None, false, true);
        assert_eq!(titles(&e).split('|').next(), Some("Ask Tracker Again"));
        let ids = vec!["a".to_string(), "c".to_string()];
        assert_eq!(*command(&e, "Ask Tracker Again"), RowCommand::AskTrackerAgain { ids });
        assert!(!titles(&one(fine, false)).contains("Ask Tracker Again"));
    }

    #[test]
    fn a_direct_download_has_no_magnet_link_and_no_double_separator() {
        let mut r = row("a", TransferState::Downloading);
        r.kind = TransferKind::Direct;
        let e = one(r, false);
        assert!(!titles(&e).contains("Copy Magnet Link"));
        assert!(titles(&e).ends_with("|Rule: Default >|-|Remove…|Remove and Delete Files…"), "{}", titles(&e));
    }

    #[test]
    fn without_rules_the_rule_menu_has_one_separator() {
        let e = one(row("a", TransferState::Downloading), false);
        assert_eq!(titles(sub(&e, "Rule")), "Automatic, by tracker|-|Edit Rules…");
    }

    #[test]
    fn tidy_drops_separators_at_the_ends_and_in_a_row() {
        let sep = || CommandEntry::Separator;
        let inner = submenu("S", vec![sep(), note("n"), sep()]);
        let list = vec![sep(), item("A", RowCommand::Pause), sep(), sep(), inner, sep()];
        assert_eq!(titles(&tidy(list.clone())), "A|-|S >");
        assert_eq!(titles(sub(&tidy(list), "S")), "(n)");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_mac_shows_in_finder() {
        assert_eq!(REVEAL, "Show in Finder");
        assert!(one(row("a", TransferState::Downloading), false).iter().any(
            |e| matches!(e, CommandEntry::Item { title, command: RowCommand::Reveal, .. } if title == "Show in Finder")
        ));
    }

    #[test]
    fn no_rows_no_commands() {
        assert!(row_commands(vec![], None, vec![], None, false, true).is_empty());
    }

    #[test]
    fn play_comes_first_for_a_single_row_with_videos() {
        let offer = PlayOffer { videos: vec![video(3, "Sintel.2010.mkv")], players: vec![player("vlc", true, false)] };
        let a = || vec![row("a", TransferState::Downloading)];
        let e = row_commands(a(), None, vec![], Some(offer.clone()), false, true);
        assert!(titles(&e).starts_with("Play|Play With >|-|Pause|"));
        assert_eq!(*command(&e, "Play"), RowCommand::Play { file_index: Some(3), player_id: None });
        let vlc = RowCommand::Play { file_index: Some(3), player_id: Some("vlc".into()) };
        assert_eq!(*command(sub(&e, "Play With"), "VLC"), vlc);
        // Not for a selection, and nothing without videos.
        let two = vec![row("a", TransferState::Downloading), row("b", TransferState::Downloading)];
        assert!(titles(&row_commands(two, None, vec![], Some(offer), false, true)).starts_with("Pause|"));
        let none = PlayOffer { videos: vec![], players: vec![player("vlc", true, false)] };
        assert!(titles(&row_commands(a(), None, vec![], Some(none), false, true)).starts_with("Pause|"));
    }

    #[test]
    fn a_pack_lists_its_episodes_under_play_and_each_player() {
        let videos = vec![video(0, "Show.S01E01.mkv"), video(1, "Show.S01E02.mkv")];
        let players = vec![player("iina", true, false), player("qt", true, true), player("old", false, false)];
        let e = play_commands(row("a", TransferState::Downloading), PlayOffer { videos, players });
        assert_eq!(titles(&e), "Play >|Play With >");
        assert_eq!(titles(sub(&e, "Play")), "S01E01|S01E02");
        // QuickTime only plays MP4; a player that cannot run is left out.
        assert_eq!(titles(sub(&e, "Play With")), "IINA >");
        let iina = sub(sub(&e, "Play With"), "IINA");
        assert_eq!(*command(iina, "S01E02"), RowCommand::Play { file_index: Some(1), player_id: Some("iina".into()) });
    }

    #[test]
    fn a_long_pack_shows_forty_episodes_and_says_how_many_more() {
        let videos: Vec<TorrentFile> = (0..45).map(|i| video(i, &format!("Show.S01E{:02}.mkv", i + 1))).collect();
        let e = play_commands(row("a", TransferState::Downloading), PlayOffer { videos, players: vec![] });
        let play = sub(&e, "Play");
        assert_eq!(play.len(), 41);
        assert_eq!(play.last(), Some(&CommandEntry::Note { title: "+ 5 more in Get Info".into() }));
        assert_eq!(titles(sub(&e, "Play With")), "(No Player Installed)");
    }

    #[test]
    fn a_finished_direct_video_plays_its_own_file() {
        let mut r = row("a", TransferState::Completed);
        r.kind = TransferKind::Direct;
        r.save_path = "/d/clip.mp4".into();
        r.file_ext = "mp4".into();
        let players = vec![player("qt", true, true), player("vlc", true, false)];
        let e = play_commands(r.clone(), PlayOffer { videos: vec![], players: players.clone() });
        assert_eq!(titles(&e), "Play|Play With >");
        assert_eq!(*command(&e, "Play"), RowCommand::Play { file_index: None, player_id: None });
        assert_eq!(titles(sub(&e, "Play With")), "QT|VLC");
        // An MKV is not for QuickTime.
        r.save_path = "/d/clip.mkv".into();
        r.file_ext = "mkv".into();
        let e = play_commands(r.clone(), PlayOffer { videos: vec![], players });
        assert_eq!(titles(sub(&e, "Play With")), "VLC");
        assert_eq!(titles(&play_commands(r, PlayOffer { videos: vec![], players: vec![] })), "Play|Play With >");
        // Unfinished: nothing to play yet.
        let mut d = row("d", TransferState::Downloading);
        d.kind = TransferKind::Direct;
        assert!(play_commands(d, PlayOffer { videos: vec![], players: vec![] }).is_empty());
    }

    #[test]
    fn players_are_capable_when_they_run_and_play_the_container() {
        let players = vec![player("iina", true, false), player("qt", true, true), player("old", false, false)];
        let ids = |mp4| capable_players(players.clone(), mp4).into_iter().map(|p| p.id).collect::<Vec<_>>();
        assert_eq!(ids(true), ["iina", "qt"]);
        assert_eq!(ids(false), ["iina"]);
    }

    #[test]
    fn playable_videos_are_the_wanted_video_files() {
        let mut skipped = video(1, "b.mkv");
        skipped.wanted = false;
        let mut text = video(2, "c.nfo");
        text.is_video = false;
        let got = playable_videos(vec![video(0, "a.mkv"), skipped, text]);
        assert_eq!(got.iter().map(|f| f.index).collect::<Vec<_>>(), [0]);
    }

    #[test]
    fn an_episode_is_named_by_its_code_else_by_its_name() {
        assert_eq!(episode_label("Example.Show.S01E02.1080p.mkv"), "S01E02");
        assert_eq!(episode_label("example.show.s03e10.mkv"), "S03E10");
        assert_eq!(episode_label("Sintel.2010.1080p.mkv"), "Sintel.2010.1080p");
        assert_eq!(episode_label("Show.S01E02E03.mkv"), "S01E02E03");
        assert_eq!(episode_label("Show.S01E02-E03.mkv"), "S01E02-E03");
        assert_eq!(episode_label("Show.s1e2.mkv"), "S1E2");
        // Three season digits are not a code; a later one is found.
        assert_eq!(episode_label("Show.S123E01.S02E05.mkv"), "S02E05");
        assert_eq!(episode_label("Show.S01E.mkv"), "Show.S01E");
        assert_eq!(episode_label("noext"), "noext");
        assert_eq!(episode_label(".hidden"), ".hidden");
    }
}
