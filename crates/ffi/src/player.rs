//! Play while downloading (SPEC §7F.3), what follows a Play command: which players this computer has
//! (`trs_platform::players`, remembered a few seconds), which one plays a file (the "Play videos with" setting, or
//! "Automatic"), and what the host does then: start the player ([`PlayLaunch`]) or show the calm note that says why
//! nothing starts. Also the setting's choices. The Play menus themselves are `row_commands::play_commands`.
//!
//! - [`Core::play`]: a Play command, the row's Play button or a file's Play.
//! - [`Core::play_offer`], [`Core::play_players`]: what the Play entries are made of.
//! - [`Core::play_with_choices`]: the setting's pop-up.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use trs_platform::players::{self as platform, KnownPlayer, Player, Target};
use trs_platform::COMPUTER;

use crate::play::is_mp4_family;
use crate::row_commands::{playable_videos, PlayOffer, PlayerOption};
use crate::row_text::{is_finished_direct_video, may_hold_video};
use crate::types::{CoreError, StreamInfo, TorrentFile, TransferRow};
use crate::Core;

/// How long the players found stay good: apps get installed and updated while Seedwell runs; reading a few file
/// headers is cheap but not free.
const MAX_AGE: Duration = Duration::from_secs(20);
const AUTO: &str = "auto";
const COULD_NOT_START: &str = "Couldn't start playing.";
const NO_VIDEO: &str = "There is no video to play in this download.";

// MARK: The players found

struct Directory {
    at: Option<Instant>,
    players: Vec<Player>,
    /// Tests: a fixed list instead of this computer's.
    fixed: Option<Vec<Player>>,
    /// A look is under way on a thread of its own ([`installed_cached`]).
    looking: bool,
}

static DIRECTORY: Mutex<Directory> = Mutex::new(Directory { at: None, players: Vec::new(), fixed: None, looking: false });

fn directory() -> MutexGuard<'static, Directory> {
    DIRECTORY.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The players on this computer, looked up again (on this thread) when older than `max_age`. Only off the host's UI
/// thread: a look reads app bundles and asks the system's app database. The UI's reads use [`installed_cached`].
fn installed(max_age: Duration) -> Vec<Player> {
    {
        let d = directory();
        if let Some(f) = &d.fixed {
            return f.clone();
        }
        if d.at.is_some_and(|t| t.elapsed() <= max_age) {
            return d.players.clone();
        }
    }
    // Looked up without the lock held: a UI read meanwhile gets the last list.
    let players = platform::installed();
    let mut d = directory();
    d.players = players;
    d.at = Some(Instant::now());
    d.players.clone()
}

/// The players as last looked up (empty before the first look), never waiting: a list older than `max_age` is looked
/// up again on a thread of its own, and the next read has it.
fn installed_cached(max_age: Duration) -> Vec<Player> {
    let mut d = directory();
    if let Some(f) = &d.fixed {
        return f.clone();
    }
    if d.at.is_none_or(|t| t.elapsed() > max_age) && !d.looking {
        d.looking = true;
        let spawned = std::thread::Builder::new().name("seedwell-players".into()).spawn(|| {
            let players = platform::installed();
            let mut d = directory();
            d.players = players;
            d.at = Some(Instant::now());
            d.looking = false;
        });
        if spawned.is_err() {
            d.looking = false;
        }
    }
    d.players.clone()
}

/// Looks the players up in the background now, so the first menu that offers Play has them (at the core's start).
pub(crate) fn look_up_players() {
    let _ = installed_cached(Duration::ZERO);
}

/// Tests: these players instead of this computer's (`None`: this computer's again). Nothing can be started with them.
#[uniffi::export]
pub fn players_override(players: Option<Vec<PlayerOption>>) {
    directory().fixed = players.map(|ps| {
        ps.into_iter()
            .map(|p| Player { id: p.id, name: p.name, path: PathBuf::new(), archs: None, runs: p.runs, mp4_only: p.mp4_only })
            .collect()
    });
}

fn option(p: &Player) -> PlayerOption {
    PlayerOption { id: p.id.clone(), name: p.name.clone(), runs: p.runs, mp4_only: p.mp4_only }
}

/// "VLC", or "VLC (needs update)" for one that cannot run here: the setting's pop-up.
fn menu_title(p: &Player) -> String {
    if p.runs { p.name.clone() } else { format!("{} (needs update)", p.name) }
}

// MARK: Which player

/// What Play does with the setting and the players found.
#[derive(Debug, PartialEq)]
enum Choice<'a> {
    Play(&'a Player),
    /// Everything installed that could play the file cannot run on this computer.
    NeedsUpdate(&'a Player),
    Nothing,
}

fn supports(p: &Player, mp4_family: bool) -> bool {
    !p.mp4_only || mp4_family
}

/// `preference` is the setting: `"auto"` or a player's identifier (any of its identifiers in `known`). A chosen player
/// that cannot run is not silently replaced; one that is gone or cannot play this kind of file is: by the first usable
/// one in the order of "Automatic".
fn resolve<'a>(known: &[KnownPlayer], preference: &str, installed: &'a [Player], mp4_family: bool) -> Choice<'a> {
    if preference != AUTO
        && let Some(spec) = platform::spec_in(known, preference)
        && let Some(chosen) = installed.iter().find(|p| p.id == spec.id && supports(p, mp4_family))
    {
        return if chosen.runs { Choice::Play(chosen) } else { Choice::NeedsUpdate(chosen) };
    }
    if let Some(first) = installed.iter().find(|p| p.runs && supports(p, mp4_family)) {
        return Choice::Play(first);
    }
    installed.iter().find(|p| supports(p, mp4_family)).map_or(Choice::Nothing, Choice::NeedsUpdate)
}

/// The note when nothing can start. `suggested`: the players to install; `mp4_only`: what the one that plays only the
/// MP4 family is called, if the system has one.
fn choice_note(choice: &Choice<'_>, mp4_family: bool, suggested: [&str; 2], mp4_only: Option<&str>) -> String {
    let [a, b] = suggested;
    match choice {
        Choice::Play(_) => String::new(),
        Choice::NeedsUpdate(p) => format!("{} can't run on this {COMPUTER}. Update it, or install {a}.", p.name),
        Choice::Nothing if mp4_family => format!("No video player found. Install {a} or {b}."),
        Choice::Nothing => match mp4_only {
            Some(qt) => format!("No video player found for this file. Install {a} or {b} ({qt} can't play it)."),
            None => format!("No video player found for this file. Install {a} or {b}."),
        },
    }
}

// MARK: What the host does

/// How the host starts a player.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PlayLaunch {
    /// Opens the file with the app (the Mac: LaunchServices' open with an app).
    OpenFile { app: String, path: String },
    /// Opens the URL with the app: the stream, or the player's own URL scheme carrying it.
    OpenUrl { app: String, url: String },
    /// Starts the app with these command-line arguments (Windows: every player, for streams and files).
    Run { app: String, arguments: Vec<String> },
}

/// What a Play command comes to.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PlayStart {
    /// Start `player` as `launch` says. When the system refuses, the note is [`play_launch_failed`].
    Launch { launch: PlayLaunch, player: String },
    /// Nothing starts: the calm note under the list says why (the host opens the main window to show it).
    Note { text: String },
}

/// The note when the system would not start the player: "VLC did not open: <the system's reason>".
#[uniffi::export]
pub fn play_launch_failed(player: String, error: String) -> String {
    format!("{player} did not open: {error}")
}

fn launch_of(player: &Player, target: Target<'_>) -> PlayLaunch {
    let s = |p: &Path| p.to_string_lossy().into_owned();
    match platform::launch(player, target) {
        platform::Launch::OpenFile { app, path } => PlayLaunch::OpenFile { app: s(&app), path: s(&path) },
        platform::Launch::OpenUrl { app, url } => PlayLaunch::OpenUrl { app: s(&app), url },
        platform::Launch::Run { app, arguments } => PlayLaunch::Run { app: s(&app), arguments },
    }
}

fn note(text: impl Into<String>) -> PlayStart {
    PlayStart::Note { text: text.into() }
}

fn not_there(name: &str) -> PlayStart {
    note(format!("“{name}” isn't at its saved location."))
}

/// The sentence the engine gave, else a plain fallback.
fn stream_note(e: &CoreError) -> String {
    match e {
        CoreError::Failed { message } | CoreError::InvalidSource { message } => message.clone(),
        _ => COULD_NOT_START.into(),
    }
}

fn finished(f: &TorrentFile) -> bool {
    f.length > 0 && f.done >= f.length
}

/// Where a finished file is: its own disk path, else the saved path, which is the file itself (a single-file torrent)
/// or the folder that holds it.
fn local_path(row: &TransferRow, file: &TorrentFile) -> Option<PathBuf> {
    if !file.disk_path.is_empty() {
        let p = PathBuf::from(&file.disk_path);
        if p.exists() {
            return Some(p);
        }
    }
    let base = Path::new(&row.save_path);
    if !std::fs::metadata(base).ok()?.is_dir() {
        return Some(base.to_path_buf());
    }
    let p = file.path.split('/').fold(base.to_path_buf(), |p, c| p.join(c));
    p.exists().then_some(p)
}

/// The largest playable video: what the row's Play button plays (episodes are played from the file list, where every
/// video has its own Play). The first of equals.
fn main_video(videos: &[TorrentFile]) -> Option<&TorrentFile> {
    videos.iter().reduce(|a, b| if b.length > a.length { b } else { a })
}

/// What a Play command needs of the core: the row's files, a stream, the setting, and the players.
struct Context<'a> {
    files: &'a dyn Fn(&str) -> Result<Vec<TorrentFile>, CoreError>,
    stream: &'a dyn Fn(&str, u32) -> Result<StreamInfo, CoreError>,
    preference: String,
    known: &'a [KnownPlayer],
    installed: Vec<Player>,
    suggested: [&'static str; 2],
    mp4_only: Option<&'static str>,
    launch: &'a dyn Fn(&Player, Target<'_>) -> PlayLaunch,
}

impl Context<'_> {
    /// The player for a file: the chosen one (Play With), else the setting's; or the note why there is none.
    fn player(&self, chosen: Option<&str>, mp4_family: bool) -> Result<Player, PlayStart> {
        if let Some(p) = chosen.and_then(|id| self.installed.iter().find(|p| p.id == id)) {
            return Ok(p.clone());
        }
        match resolve(self.known, &self.preference, &self.installed, mp4_family) {
            Choice::Play(p) => Ok(p.clone()),
            c => Err(note(choice_note(&c, mp4_family, self.suggested, self.mp4_only))),
        }
    }

    fn start(&self, player: &Player, target: Target<'_>) -> PlayStart {
        PlayStart::Launch { launch: (self.launch)(player, target), player: player.name.clone() }
    }

    /// `file_index` `None`: the row's own (a finished direct video's file, else its largest video).
    fn play(&self, row: &TransferRow, file_index: Option<u32>, chosen: Option<&str>) -> PlayStart {
        if file_index.is_none() && is_finished_direct_video(row) {
            let name = Path::new(&row.save_path).file_name().map_or_else(|| row.name.clone(), |n| n.to_string_lossy().into_owned());
            let player = match self.player(chosen, is_mp4_family(&name)) {
                Ok(p) => p,
                Err(n) => return n,
            };
            let path = Path::new(&row.save_path);
            return if path.exists() { self.start(&player, Target::File(path)) } else { not_there(&name) };
        }
        let files = if file_index.is_some() || may_hold_video(row) { (self.files)(&row.id).unwrap_or_default() } else { Vec::new() };
        let file = match file_index {
            Some(i) => files.into_iter().find(|f| f.index == i),
            None => main_video(&playable_videos(files)).cloned(),
        };
        let Some(file) = file else { return note(NO_VIDEO) };
        let player = match self.player(chosen, file.is_mp4_family) {
            Ok(p) => p,
            Err(n) => return n,
        };
        let open_local = || match local_path(row, &file) {
            Some(p) => self.start(&player, Target::File(&p)),
            None => not_there(&file.name),
        };
        if finished(&file) {
            return open_local();
        }
        match (self.stream)(&row.id, file.index) {
            // It finished while we were asking: open the file itself.
            Ok(info) if info.complete => open_local(),
            Ok(info) if info.url.is_empty() => note(COULD_NOT_START),
            Ok(info) => self.start(&player, Target::Url(&info.url)),
            Err(e) => note(stream_note(&e)),
        }
    }
}

/// One choice of the "Play videos with" pop-up.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PlayWithChoice {
    /// What the setting stores (`general.play_with`).
    pub value: String,
    pub title: String,
    /// A player that cannot run here, or one the setting names that is gone, is listed but not choosable.
    pub enabled: bool,
}

/// Automatic, then every player found; a player the setting names that this computer no longer has stays visible
/// instead of a blank pop-up.
fn choices(known: &[KnownPlayer], installed: &[Player], selection: &str) -> Vec<PlayWithChoice> {
    let mut out = vec![PlayWithChoice { value: AUTO.into(), title: "Automatic".into(), enabled: true }];
    out.extend(installed.iter().map(|p| PlayWithChoice { value: p.id.clone(), title: menu_title(p), enabled: p.runs }));
    if selection != AUTO && !installed.iter().any(|p| p.id == selection) {
        let name = platform::spec_in(known, selection).map_or(selection, |s| s.name);
        out.push(PlayWithChoice { value: selection.into(), title: format!("{name} (not installed)"), enabled: false });
    }
    out
}

impl Core {
    fn play_context<'a>(
        &self,
        files: &'a dyn Fn(&str) -> Result<Vec<TorrentFile>, CoreError>,
        stream: &'a dyn Fn(&str, u32) -> Result<StreamInfo, CoreError>,
    ) -> Context<'a> {
        Context {
            files,
            stream,
            preference: self.settings_load().general.play_with,
            known: platform::known(),
            installed: installed(MAX_AGE),
            suggested: platform::SUGGESTED,
            mp4_only: platform::MP4_ONLY_PLAYER,
            launch: &launch_of,
        }
    }
}

impl Core {
    /// A Play command: `RowCommand::PlayRow` and `RowCommand::Play` (`file_index` `None`: the row's own video: a finished
    /// direct download's file, else the torrent's largest selected video), and a file's Play in the file list.
    /// `player_id`: a chosen player (Play With); `None` lets the setting decide. A finished file opens directly; an
    /// unfinished one streams (a skipped one is selected). Blocks while a starting torrent makes the stream wait: call
    /// it off the main thread, then start the player or show the note.
    pub fn play(&self, row: TransferRow, file_index: Option<u32>, player_id: Option<String>) -> PlayStart {
        let backend = self.backend.clone();
        let files = |id: &str| backend.files(id);
        let stream = |id: &str, i: u32| backend.stream_url(id, i);
        self.play_context(&files, &stream).play(&row, file_index, player_id.as_deref())
    }
}

#[uniffi::export]
impl Core {
    /// The players on this computer, in the order of "Automatic" (Play With, `play_commands`). Never waits: the list as
    /// last looked up, looked up again in the background when it is older than a few seconds.
    pub fn play_players(&self) -> Vec<PlayerOption> {
        installed_cached(MAX_AGE).iter().map(option).collect()
    }

    /// What the Play entries of a row are made of (`row_commands`, `play_commands`, Get Info): its playable videos, and
    /// the players, looked up only when there is something to play.
    pub fn play_offer(&self, row: TransferRow) -> PlayOffer {
        // Drawn into menus on the host's UI thread: the files as read within a few milliseconds, or as last read.
        let videos = if may_hold_video(&row) { playable_videos(self.files_now(&row.id)) } else { Vec::new() };
        let wanted = is_finished_direct_video(&row) || !videos.is_empty();
        PlayOffer { videos, players: if wanted { self.play_players() } else { Vec::new() } }
    }

    /// The video the row's Play button plays: its largest selected video (`None` for a direct download or no video).
    pub fn play_main_video(&self, row: TransferRow) -> Option<TorrentFile> {
        main_video(&self.play_offer(row).videos).cloned()
    }

    /// Settings › General › Play videos with: its choices, `selection` being the value shown now.
    pub fn play_with_choices(&self, selection: String) -> Vec<PlayWithChoice> {
        // Drawn on the host's UI thread (Settings › General): the list as last looked up; each call has it looked up
        // again in the background, so the next one is current.
        choices(platform::known(), &installed_cached(Duration::ZERO), &selection)
    }

    /// QA (`play-players`): the machine, every player found with its executable's slices, and what the setting plays.
    pub fn play_players_report(&self) -> Vec<String> {
        let players = installed(Duration::ZERO);
        let mut out = vec![format!("host: {}", platform::host_line())];
        for p in &players {
            let archs = p.archs.as_ref().map(|a| a.iter().map(|x| x.name()).collect::<Vec<_>>().join("+")).filter(|s| !s.is_empty());
            out.push(format!(
                "player: {} at {} [{}] {}",
                menu_title(p),
                p.path.display(),
                archs.as_deref().unwrap_or("unknown"),
                if p.runs { "runs" } else { "CANNOT RUN" }
            ));
        }
        if players.is_empty() {
            out.push("player: none installed".into());
        }
        let pref = self.settings_load().general.play_with;
        for (label, mp4) in [("mkv", false), ("mp4", true)] {
            let text = match resolve(platform::known(), &pref, &players, mp4) {
                Choice::Play(p) => format!("plays with {}", p.name),
                Choice::NeedsUpdate(p) => format!("needs update: {}", p.name),
                Choice::Nothing => "no player".into(),
            };
            let with: Vec<&str> = players.iter().filter(|p| p.runs && supports(p, mp4)).map(|p| p.name.as_str()).collect();
            out.push(format!("setting {pref}: {label} {text}; Play With: {}", if with.is_empty() { "-".into() } else { with.join(", ") }));
        }
        out
    }

    /// QA (`play-app`): the known player at `app` (an app bundle or executable), started once on a local `file`.
    pub fn play_app(&self, app: String, file: String) -> PlayStart {
        let Some(player) = platform::player_at(Path::new(&app)) else { return note(format!("{app} is not a player Seedwell knows")) };
        if !player.runs {
            let c = Choice::NeedsUpdate(&player);
            return note(choice_note(&c, false, platform::SUGGESTED, platform::MP4_ONLY_PLAYER));
        }
        PlayStart::Launch { launch: launch_of(&player, Target::File(Path::new(&file))), player: player.name }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Category, FilePriority, TransferKind, TransferState};
    use platform::{Arch, MAC_PLAYERS, WINDOWS_PLAYERS};

    fn found(known: &[KnownPlayer], id: &str, archs: &[Arch], apple_silicon: bool, rosetta: bool) -> Player {
        let s = platform::spec_in(known, id).unwrap();
        Player {
            id: s.id.into(),
            name: s.name.into(),
            path: PathBuf::from(format!("/Applications/{}.app", s.name)),
            archs: Some(archs.to_vec()),
            runs: platform::can_run(Some(archs), apple_silicon, rosetta),
            mp4_only: s.mp4_only,
        }
    }

    fn arm(id: &str) -> Player {
        found(MAC_PLAYERS, id, &[Arch::Arm64], true, false)
    }

    const IINA: &str = "com.colliderli.iina";
    const VLC: &str = "org.videolan.vlc";
    const INFUSE: &str = "com.firecore.infuse";
    const QT: &str = "com.apple.QuickTimePlayerX";

    fn note_of(c: &Choice<'_>, mp4: bool) -> String {
        choice_note(c, mp4, ["IINA", "VLC"], Some("QuickTime"))
    }

    #[test]
    fn an_intel_only_player_without_rosetta_needs_an_update() {
        let installed = vec![found(MAC_PLAYERS, VLC, &[Arch::X86_64], true, false)];
        assert!(!installed[0].runs);
        let c = resolve(MAC_PLAYERS, AUTO, &installed, false);
        assert_eq!(c, Choice::NeedsUpdate(&installed[0]));
        assert_eq!(note_of(&c, false), format!("VLC can't run on this {COMPUTER}. Update it, or install IINA."));
        assert_eq!(resolve(MAC_PLAYERS, VLC, &installed, true), Choice::NeedsUpdate(&installed[0]), "a chosen one is not replaced silently");
        let rosetta = vec![found(MAC_PLAYERS, VLC, &[Arch::X86_64], true, true)];
        assert_eq!(resolve(MAC_PLAYERS, AUTO, &rosetta, false), Choice::Play(&rosetta[0]));
    }

    #[test]
    fn automatic_takes_iina_vlc_infuse_then_quicktime() {
        let all: Vec<Player> = [IINA, VLC, INFUSE, QT].iter().map(|id| arm(id)).collect();
        for n in 0..4 {
            let rest = &all[n..];
            assert_eq!(resolve(MAC_PLAYERS, AUTO, rest, true), Choice::Play(&rest[0]), "{}", rest[0].name);
        }
    }

    #[test]
    fn quicktime_plays_only_the_mp4_family() {
        let installed = vec![arm(QT)];
        assert_eq!(resolve(MAC_PLAYERS, AUTO, &installed, true), Choice::Play(&installed[0]));
        let c = resolve(MAC_PLAYERS, AUTO, &installed, false);
        assert_eq!(c, Choice::Nothing);
        assert_eq!(note_of(&c, false), "No video player found for this file. Install IINA or VLC (QuickTime can't play it).");
        assert_eq!(note_of(&Choice::Nothing, true), "No video player found. Install IINA or VLC.");
        assert_eq!(choice_note(&Choice::Nothing, false, ["VLC", "mpv"], None), "No video player found for this file. Install VLC or mpv.");
    }

    #[test]
    fn a_broken_first_choice_falls_through_to_one_that_runs() {
        let installed = vec![found(MAC_PLAYERS, VLC, &[Arch::X86_64], true, false), arm(INFUSE)];
        assert_eq!(resolve(MAC_PLAYERS, AUTO, &installed, false), Choice::Play(&installed[1]));
    }

    #[test]
    fn the_setting_picks_a_player_unless_it_is_gone_or_cannot_play_the_file() {
        let installed = vec![arm(IINA), arm(VLC), arm(QT)];
        assert_eq!(resolve(MAC_PLAYERS, VLC, &installed, false), Choice::Play(&installed[1]));
        assert_eq!(resolve(MAC_PLAYERS, QT, &installed, false), Choice::Play(&installed[0]), "QuickTime cannot play MKV");
        assert_eq!(resolve(MAC_PLAYERS, INFUSE, &installed, false), Choice::Play(&installed[0]), "not installed");
        assert_eq!(resolve(MAC_PLAYERS, "com.example.unknown", &installed, false), Choice::Play(&installed[0]));
        assert_eq!(resolve(MAC_PLAYERS, "com.firecore.infuse.mac", &[arm(INFUSE)], false), Choice::Play(&arm(INFUSE)), "any identifier");
        assert_eq!(resolve(MAC_PLAYERS, AUTO, &[], false), Choice::Nothing);
    }

    #[test]
    fn the_other_known_players_follow_as_a_last_resort() {
        let installed = vec![arm("io.mpv"), arm("com.movist.MovistPro"), arm(QT)];
        assert_eq!(resolve(MAC_PLAYERS, AUTO, &installed, false), Choice::Play(&installed[0]));
        let win = vec![found(WINDOWS_PLAYERS, "potplayer", &[], false, false), found(WINDOWS_PLAYERS, "vlc", &[], false, false)];
        assert_eq!(resolve(WINDOWS_PLAYERS, "potplayer", &win, false), Choice::Play(&win[0]));
    }

    #[test]
    fn the_setting_lists_automatic_the_players_and_one_that_is_gone() {
        let installed = vec![arm(IINA), found(MAC_PLAYERS, VLC, &[Arch::X86_64], true, false)];
        let c = choices(MAC_PLAYERS, &installed, AUTO);
        let shown: Vec<(&str, &str, bool)> = c.iter().map(|c| (c.value.as_str(), c.title.as_str(), c.enabled)).collect();
        assert_eq!(shown, [("auto", "Automatic", true), (IINA, "IINA", true), (VLC, "VLC (needs update)", false)]);
        let gone = choices(MAC_PLAYERS, &installed, INFUSE);
        assert_eq!(gone.last().map(|c| (c.title.as_str(), c.enabled)), Some(("Infuse (not installed)", false)));
        assert_eq!(choices(MAC_PLAYERS, &[], "nonsense").last().map(|c| c.title.as_str()), Some("nonsense (not installed)"));
    }

    // MARK: The whole command

    fn row(kind: TransferKind, state: TransferState, save_path: &str, ext: &str) -> TransferRow {
        let mut r = crate::row_text::tests::row("t1", state);
        r.kind = kind;
        r.save_path = save_path.into();
        r.file_ext = ext.into();
        r.category = Category::Movie;
        r.has_video = true;
        r
    }

    fn file(index: u32, name: &str, length: u64, done: u64, disk: &str) -> TorrentFile {
        TorrentFile {
            index,
            path: name.into(),
            disk_path: disk.into(),
            name: name.into(),
            length,
            done,
            uploaded: 0,
            wanted: true,
            priority: FilePriority::Normal,
            is_video: crate::play::is_video(name),
            is_mp4_family: is_mp4_family(name),
        }
    }

    fn run(
        row: &TransferRow,
        files: Vec<TorrentFile>,
        stream: Result<StreamInfo, CoreError>,
        installed: Vec<Player>,
        file_index: Option<u32>,
        chosen: Option<&str>,
    ) -> PlayStart {
        let f = move |_: &str| -> Result<Vec<TorrentFile>, CoreError> { Ok(files.clone()) };
        // Asked at most once (a CoreError is not Clone).
        let once = Mutex::new(Some(stream));
        let s = move |_: &str, _: u32| once.lock().unwrap().take().expect("the stream is asked for once");
        let ctx = Context {
            files: &f,
            stream: &s,
            preference: AUTO.into(),
            known: MAC_PLAYERS,
            installed,
            suggested: ["IINA", "VLC"],
            mp4_only: Some("QuickTime"),
            launch: &|p, t| match platform::launch_in(MAC_PLAYERS, p, t, true) {
                platform::Launch::OpenFile { app, path } => PlayLaunch::OpenFile { app: app.display().to_string(), path: path.display().to_string() },
                platform::Launch::OpenUrl { app, url } => PlayLaunch::OpenUrl { app: app.display().to_string(), url },
                platform::Launch::Run { app, arguments } => PlayLaunch::Run { app: app.display().to_string(), arguments },
            },
        };
        ctx.play(row, file_index, chosen)
    }

    fn stream(url: &str, complete: bool) -> Result<StreamInfo, CoreError> {
        Ok(StreamInfo { url: url.into(), token: String::new(), complete, selected: false, content_type: "video/x-matroska".into(), is_mp4_family: false })
    }

    #[test]
    fn the_play_button_streams_the_largest_video_in_the_setting_s_player() {
        let r = row(TransferKind::Torrent, TransferState::Downloading, "/nowhere/Pack", "");
        let files = vec![file(0, "E01.mkv", 100, 0, ""), file(1, "E02.mkv", 300, 0, ""), file(2, "E03.mkv", 300, 0, ""), file(3, "x.nfo", 900, 0, "")];
        let got = run(&r, files.clone(), stream("http://127.0.0.1:1/t/E02.mkv", false), vec![arm(VLC)], None, None);
        assert_eq!(
            got,
            PlayStart::Launch { launch: PlayLaunch::OpenUrl { app: "/Applications/VLC.app".into(), url: "http://127.0.0.1:1/t/E02.mkv".into() }, player: "VLC".into() }
        );
        assert_eq!(main_video(&playable_videos(files)).map(|f| f.index), Some(1), "the first of equals");
        // Play With a chosen player, on one episode; IINA through its scheme.
        let got = run(&r, vec![file(0, "E01.mkv", 100, 0, "")], stream("http://h/x", false), vec![arm(VLC), arm(IINA)], Some(0), Some(IINA));
        assert!(matches!(got, PlayStart::Launch { launch: PlayLaunch::OpenUrl { ref url, .. }, ref player } if url.starts_with("iina://weblink?url=http%3A%2F%2Fh%2Fx") && player == "IINA"));
    }

    #[test]
    fn notes_say_why_nothing_starts() {
        let r = row(TransferKind::Torrent, TransferState::Downloading, "/nowhere/Pack", "");
        assert_eq!(run(&r, vec![file(0, "a.nfo", 1, 0, "")], stream("", false), vec![arm(VLC)], None, None), note(NO_VIDEO));
        assert_eq!(run(&r, vec![file(0, "a.mkv", 1, 0, "")], stream("", false), vec![arm(QT)], None, None), note(note_of(&Choice::Nothing, false)));
        let failed = Err(CoreError::Failed { message: "The torrent is paused.".into() });
        assert_eq!(run(&r, vec![file(0, "a.mkv", 1, 0, "")], failed, vec![arm(VLC)], None, None), note("The torrent is paused."));
        let other = Err(CoreError::NotFound { id: "x".into() });
        assert_eq!(run(&r, vec![file(0, "a.mkv", 1, 0, "")], other, vec![arm(VLC)], None, None), note(COULD_NOT_START));
        // Finished, but not where it was saved.
        assert_eq!(run(&r, vec![file(0, "a.mkv", 5, 5, "")], stream("", false), vec![arm(VLC)], None, None), note("“a.mkv” isn't at its saved location."));
    }

    #[test]
    fn a_finished_file_opens_directly_also_one_that_finished_while_asking() {
        let dir = tempfile::tempdir().unwrap();
        let movie = dir.path().join("Movie.mp4");
        std::fs::write(&movie, b"x").unwrap();
        let r = row(TransferKind::Torrent, TransferState::Seeding, &dir.path().display().to_string(), "");
        let open = PlayStart::Launch { launch: PlayLaunch::OpenFile { app: "/Applications/QuickTime Player.app".into(), path: movie.display().to_string() }, player: "QuickTime Player".into() };
        // Found under the saved folder by its tree path.
        assert_eq!(run(&r, vec![file(0, "Movie.mp4", 5, 5, "")], stream("", false), vec![arm(QT)], None, None), open);
        // A stream that says the file completed meanwhile.
        assert_eq!(run(&r, vec![file(0, "Movie.mp4", 5, 1, "")], stream("", true), vec![arm(QT)], None, None), open);
        // Its own disk path first; a single-file torrent's saved path is the file.
        let single = row(TransferKind::Torrent, TransferState::Seeding, &movie.display().to_string(), "mp4");
        assert_eq!(run(&single, vec![file(0, "Other.mp4", 5, 5, "/gone/Other.mp4")], stream("", false), vec![arm(QT)], None, None), open);
    }

    #[test]
    fn a_finished_direct_video_opens_its_file() {
        let dir = tempfile::tempdir().unwrap();
        let clip = dir.path().join("clip.mkv");
        let r = row(TransferKind::Direct, TransferState::Completed, &clip.display().to_string(), "mkv");
        assert_eq!(run(&r, vec![], stream("", false), vec![arm(IINA)], None, None), note("“clip.mkv” isn't at its saved location."));
        std::fs::write(&clip, b"x").unwrap();
        assert_eq!(
            run(&r, vec![], stream("", false), vec![arm(IINA)], None, None),
            PlayStart::Launch { launch: PlayLaunch::OpenFile { app: "/Applications/IINA.app".into(), path: clip.display().to_string() }, player: "IINA".into() }
        );
        assert_eq!(run(&r, vec![], stream("", false), vec![arm(QT)], None, None), note(note_of(&Choice::Nothing, false)), "QuickTime and an MKV");
    }

    #[test]
    fn a_failed_launch_names_the_player() {
        assert_eq!(play_launch_failed("VLC".into(), "The application can't be opened.".into()), "VLC did not open: The application can't be opened.");
    }
}
