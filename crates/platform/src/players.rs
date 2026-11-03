//! Video players on this computer (SPEC §7F.3): which known players are installed, whether each can run here, and how
//! one is handed a stream URL or a file. Which one Play uses (Automatic's order, the setting) and every word about it
//! are trs-ffi's; this module only finds the apps and says how each is started.
//!
//! - [`known`]: this system's players, in the order of "Automatic".
//! - [`installed`]: those found (the Mac: LaunchServices, then the Applications folders; Windows: App Paths, the
//!   registered applications and the handlers of video files, then `PATH`).
//! - [`launch`]: how one is started on a file or a URL; the host runs it.

use std::path::{Path, PathBuf};

mod macho;
mod pe;
mod registry;

#[cfg(target_os = "macos")]
mod mac;
#[cfg(windows)]
mod windows;

pub use macho::architectures;
pub use pe::pe_architecture;
pub use registry::{command_executable, expand_env, file_name_lower};

/// The CPU families that matter for "can this app run here".
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Arch {
    Arm64,
    X86_64,
    /// 32-bit Intel (i386).
    X86,
    Other,
}

impl Arch {
    /// "arm64", "x86_64", "x86", "other".
    pub fn name(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::X86_64 => "x86_64",
            Self::X86 => "x86",
            Self::Other => "other",
        }
    }
}

/// How a player is handed a stream URL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchStyle {
    /// A URL scheme of the app; `{url}` is replaced by the percent-encoded stream URL.
    Scheme(&'static str),
    /// The URL is opened with the app (the Mac: Apple events, like `open -a App URL`).
    OpenUrl,
    /// The URL is the app's command-line argument.
    Argument,
}

/// A player Seedwell knows how to start.
#[derive(Debug, PartialEq, Eq)]
pub struct KnownPlayer {
    /// What the setting stores (the Mac: the primary bundle identifier; Windows: a short name).
    pub id: &'static str,
    pub name: &'static str,
    /// Every identifier the app has been seen with, `id` first (the Mac: bundle identifiers).
    pub ids: &'static [&'static str],
    /// The files to look for: app bundles in the Applications folders (the Mac), executables (Windows).
    pub files: &'static [&'static str],
    pub launch: LaunchStyle,
    /// Only plays MP4, M4V and MOV (QuickTime Player).
    pub mp4_only: bool,
}

/// The Mac's players. IINA, VLC, Infuse, then QuickTime (MP4 family only) are "Automatic"'s order; the rest follow as
/// a last resort when none of the four is usable.
pub const MAC_PLAYERS: &[KnownPlayer] = &[
    KnownPlayer {
        id: "com.colliderli.iina",
        name: "IINA",
        ids: &["com.colliderli.iina"],
        files: &["IINA.app"],
        launch: LaunchStyle::Scheme("iina://weblink?url={url}"),
        mp4_only: false,
    },
    KnownPlayer { id: "org.videolan.vlc", name: "VLC", ids: &["org.videolan.vlc"], files: &["VLC.app"], launch: LaunchStyle::OpenUrl, mp4_only: false },
    KnownPlayer {
        id: "com.firecore.infuse",
        name: "Infuse",
        ids: &["com.firecore.infuse", "com.firecore.infuse.mac"],
        files: &["Infuse.app"],
        launch: LaunchStyle::Scheme("infuse://x-callback-url/play?url={url}"),
        mp4_only: false,
    },
    KnownPlayer {
        id: "com.apple.QuickTimePlayerX",
        name: "QuickTime Player",
        ids: &["com.apple.QuickTimePlayerX"],
        files: &["QuickTime Player.app"],
        launch: LaunchStyle::OpenUrl,
        mp4_only: true,
    },
    KnownPlayer { id: "io.mpv", name: "mpv", ids: &["io.mpv", "org.mpv.mpv"], files: &["mpv.app"], launch: LaunchStyle::Argument, mp4_only: false },
    KnownPlayer {
        id: "com.movist.MovistPro",
        name: "Movist Pro",
        ids: &["com.movist.MovistPro", "com.cocoable.MovistPro"],
        files: &["Movist Pro.app"],
        launch: LaunchStyle::OpenUrl,
        mp4_only: false,
    },
    KnownPlayer {
        id: "com.electronic.ElmediaPlayer",
        name: "Elmedia Player",
        ids: &["com.electronic.ElmediaPlayer", "com.eltima.elmedia", "com.eltima.ElmediaPlayer"],
        files: &["Elmedia Player.app", "Elmedia Video Player.app"],
        launch: LaunchStyle::OpenUrl,
        mp4_only: false,
    },
];

/// Windows' players, in the order of "Automatic". Each takes the URL or the file as its argument.
pub const WINDOWS_PLAYERS: &[KnownPlayer] = &[
    KnownPlayer { id: "vlc", name: "VLC", ids: &["vlc"], files: &["vlc.exe"], launch: LaunchStyle::Argument, mp4_only: false },
    KnownPlayer { id: "mpv", name: "mpv", ids: &["mpv"], files: &["mpv.exe"], launch: LaunchStyle::Argument, mp4_only: false },
    KnownPlayer {
        id: "mpc-hc",
        name: "MPC-HC",
        ids: &["mpc-hc"],
        files: &["mpc-hc64.exe", "mpc-hc.exe"],
        launch: LaunchStyle::Argument,
        mp4_only: false,
    },
    KnownPlayer {
        id: "potplayer",
        name: "PotPlayer",
        ids: &["potplayer"],
        files: &["PotPlayerMini64.exe", "PotPlayerMini.exe"],
        launch: LaunchStyle::Argument,
        mp4_only: false,
    },
];

/// This system's players, in the order of "Automatic" (none yet on Linux).
pub fn known() -> &'static [KnownPlayer] {
    if cfg!(target_os = "macos") {
        MAC_PLAYERS
    } else if cfg!(windows) {
        WINDOWS_PLAYERS
    } else {
        &[]
    }
}

/// The two players the notes suggest installing ("Install IINA or VLC"), the first also when an installed one cannot
/// run ("Update it, or install IINA").
#[cfg(target_os = "macos")]
pub const SUGGESTED: [&str; 2] = ["IINA", "VLC"];
/// The two players the notes suggest installing ("Install IINA or VLC"), the first also when an installed one cannot
/// run ("Update it, or install IINA").
#[cfg(not(target_os = "macos"))]
pub const SUGGESTED: [&str; 2] = ["VLC", "mpv"];

/// What the note calls the player that plays only the MP4 family, when a file of another kind finds nothing else
/// ("QuickTime can't play it"); `None` where there is none.
#[cfg(target_os = "macos")]
pub const MP4_ONLY_PLAYER: Option<&str> = Some("QuickTime");
/// What the note calls the player that plays only the MP4 family, when a file of another kind finds nothing else
/// ("QuickTime can't play it"); `None` where there is none.
#[cfg(not(target_os = "macos"))]
pub const MP4_ONLY_PLAYER: Option<&str> = None;

/// The known player an identifier names (any of its identifiers), in `list`.
pub fn spec_in<'a>(list: &'a [KnownPlayer], id: &str) -> Option<&'a KnownPlayer> {
    list.iter().find(|p| p.id == id || p.ids.contains(&id))
}

/// The known player of this system an identifier names.
pub fn spec(id: &str) -> Option<&'static KnownPlayer> {
    spec_in(known(), id)
}

/// A known player found on this computer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Player {
    /// Its [`KnownPlayer::id`].
    pub id: String,
    pub name: String,
    /// The app bundle (the Mac) or the executable (Windows).
    pub path: PathBuf,
    /// The CPU families of its executable; `None` when they could not be read (or are not checked here).
    pub archs: Option<Vec<Arch>>,
    /// It starts on this computer (the Mac: an arm64 slice, or x86_64 with Rosetta installed; Windows:
    /// [`can_run_on_windows`]).
    pub runs: bool,
    pub mp4_only: bool,
}

impl Player {
    #[cfg_attr(not(any(target_os = "macos", windows)), allow(dead_code))]
    pub(crate) fn found(spec: &KnownPlayer, path: PathBuf, archs: Option<Vec<Arch>>, runs: bool) -> Self {
        Self { id: spec.id.into(), name: spec.name.into(), path, archs, runs, mp4_only: spec.mp4_only }
    }
}

// `installed()`: the known players installed here, in [`known`]'s order. Reads a few file headers (the Mac) or registry
// keys (Windows): call it off the main thread now and then, not for every menu.
#[cfg(target_os = "macos")]
pub use mac::installed;
#[cfg(windows)]
pub use windows::installed;
#[cfg(not(any(target_os = "macos", windows)))]
pub fn installed() -> Vec<Player> {
    Vec::new()
}

// `player_at(app)`: the known player at `app` (an app bundle on the Mac, an executable on Windows), whether or not the
// system lists it: what QA's `play-app` starts. `None` when it is not one Seedwell knows.
#[cfg(not(any(target_os = "macos", windows)))]
pub fn player_at(_app: &Path) -> Option<Player> {
    None
}
#[cfg(target_os = "macos")]
pub use mac::player_at;
#[cfg(windows)]
pub use windows::player_at;

// `host_line`: what the machine can run, in a line for QA ("Apple silicon, Rosetta installed"; "arm64 Windows, x64
// emulation").
#[cfg(not(any(target_os = "macos", windows)))]
pub fn host_line() -> String {
    std::env::consts::ARCH.to_string()
}
#[cfg(target_os = "macos")]
pub use mac::host_line;
#[cfg(windows)]
pub use windows::host_line;

/// What a player is started on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target<'a> {
    /// A file on disk (a finished download).
    File(&'a Path),
    /// A stream (loopback http).
    Url(&'a str),
}

/// How the host starts a player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Opens the file with the app.
    OpenFile { app: PathBuf, path: PathBuf },
    /// Opens the URL with the app: the stream itself, or the player's own scheme carrying it.
    OpenUrl { app: PathBuf, url: String },
    /// Starts the app with these arguments.
    Run { app: PathBuf, arguments: Vec<String> },
}

/// Files are opened with the app (the Mac: LaunchServices) rather than handed over as an argument.
const OPEN_FILES_WITH_APP: bool = cfg!(target_os = "macos");

/// How `player` (of `list`) is started on `target`. A file is simply opened with the app (where the system can, else
/// it is the argument); only streams go through URL schemes and arguments.
pub fn launch_in(list: &[KnownPlayer], player: &Player, target: Target<'_>, open_files_with_app: bool) -> Launch {
    let app = player.path.clone();
    let style = spec_in(list, &player.id).map_or(LaunchStyle::Argument, |s| s.launch);
    match target {
        Target::File(path) if open_files_with_app => Launch::OpenFile { app, path: path.to_path_buf() },
        Target::File(path) => Launch::Run { app, arguments: vec![path.to_string_lossy().into_owned()] },
        Target::Url(url) => match style {
            LaunchStyle::Scheme(template) => Launch::OpenUrl { app, url: template.replace("{url}", &encode(url)) },
            LaunchStyle::OpenUrl => Launch::OpenUrl { app, url: url.to_string() },
            LaunchStyle::Argument => Launch::Run { app, arguments: vec![url.to_string()] },
        },
    }
}

/// How `player` is started on `target` on this system.
pub fn launch(player: &Player, target: Target<'_>) -> Launch {
    launch_in(known(), player, target, OPEN_FILES_WITH_APP)
}

/// `s` as a URL query value: everything but RFC 3986's unreserved characters percent-encoded (UTF-8).
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Whether an app whose executable has these slices starts on a machine (`apple_silicon`, `rosetta`). Unknown (an
/// unreadable or script-based executable) is not held against the app.
pub fn can_run(archs: Option<&[Arch]>, apple_silicon: bool, rosetta: bool) -> bool {
    let Some(archs) = archs.filter(|a| !a.is_empty()) else { return true };
    if apple_silicon {
        archs.contains(&Arch::Arm64) || (archs.contains(&Arch::X86_64) && rosetta)
    } else {
        archs.contains(&Arch::X86_64)
    }
}

/// Whether a Windows app built for `archs` starts on a PC whose own CPU is `native` (the machine's, not the process's:
/// Seedwell may itself run emulated). x86 (32-bit) apps run everywhere, through WOW64; x64 apps run on an x64 PC and,
/// with `x64_emulation` (Windows 11 on Arm; Windows 10 on Arm has none), on an Arm one; arm64 apps run only on Arm.
/// Unknown (an unreadable executable) is not held against the app.
pub fn can_run_on_windows(archs: Option<&[Arch]>, native: Arch, x64_emulation: bool) -> bool {
    let Some(archs) = archs.filter(|a| !a.is_empty()) else { return true };
    archs.iter().any(|&a| match (native, a) {
        (_, Arch::Other) | (Arch::Other, _) => false,
        (n, a) if n == a => true,
        (_, Arch::X86) => true,
        (Arch::Arm64, Arch::X86_64) => x64_emulation,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn player(list: &[KnownPlayer], id: &str) -> Player {
        let s = spec_in(list, id).unwrap();
        Player::found(s, PathBuf::from(format!("/Applications/{}.app", s.name)), Some(vec![Arch::Arm64]), true)
    }

    const STREAM: &str = "http://127.0.0.1:8080/stream/abc/0/Movie%20One.mkv";

    #[test]
    fn a_scheme_carries_the_stream_percent_encoded() {
        let iina = player(MAC_PLAYERS, "com.colliderli.iina");
        let Launch::OpenUrl { url, app } = launch_in(MAC_PLAYERS, &iina, Target::Url(STREAM), true) else { panic!() };
        assert_eq!(app, PathBuf::from("/Applications/IINA.app"));
        assert_eq!(url, "iina://weblink?url=http%3A%2F%2F127.0.0.1%3A8080%2Fstream%2Fabc%2F0%2FMovie%2520One.mkv");
        let infuse = player(MAC_PLAYERS, "com.firecore.infuse.mac");
        let Launch::OpenUrl { url, .. } = launch_in(MAC_PLAYERS, &infuse, Target::Url(STREAM), true) else { panic!() };
        assert!(url.starts_with("infuse://x-callback-url/play?url=http%3A%2F%2F127.0.0.1"));
    }

    #[test]
    fn players_without_a_scheme_open_the_url_or_take_it_as_their_argument() {
        for id in ["org.videolan.vlc", "com.apple.QuickTimePlayerX", "com.movist.MovistPro", "com.electronic.ElmediaPlayer"] {
            let p = player(MAC_PLAYERS, id);
            assert_eq!(launch_in(MAC_PLAYERS, &p, Target::Url(STREAM), true), Launch::OpenUrl { app: p.path.clone(), url: STREAM.into() }, "{id}");
        }
        let mpv = player(MAC_PLAYERS, "io.mpv");
        assert_eq!(launch_in(MAC_PLAYERS, &mpv, Target::Url(STREAM), true), Launch::Run { app: mpv.path.clone(), arguments: vec![STREAM.into()] });
        for id in ["vlc", "mpv", "mpc-hc", "potplayer"] {
            let p = player(WINDOWS_PLAYERS, id);
            assert_eq!(launch_in(WINDOWS_PLAYERS, &p, Target::Url(STREAM), false), Launch::Run { app: p.path.clone(), arguments: vec![STREAM.into()] });
        }
    }

    #[test]
    fn a_file_is_opened_with_the_app_where_the_system_can_else_it_is_the_argument() {
        let file = Path::new("/Downloads/Movie One.mkv");
        for id in ["com.colliderli.iina", "com.firecore.infuse", "org.videolan.vlc", "io.mpv"] {
            let p = player(MAC_PLAYERS, id);
            assert_eq!(launch_in(MAC_PLAYERS, &p, Target::File(file), true), Launch::OpenFile { app: p.path.clone(), path: file.into() }, "{id}");
        }
        let vlc = player(WINDOWS_PLAYERS, "vlc");
        assert_eq!(launch_in(WINDOWS_PLAYERS, &vlc, Target::File(file), false), Launch::Run { app: vlc.path.clone(), arguments: vec![file.display().to_string()] });
    }

    #[test]
    fn a_player_is_found_by_any_of_its_identifiers() {
        assert_eq!(spec_in(MAC_PLAYERS, "com.firecore.infuse.mac").map(|s| s.id), Some("com.firecore.infuse"));
        assert_eq!(spec_in(MAC_PLAYERS, "org.mpv.mpv").map(|s| s.name), Some("mpv"));
        assert!(spec_in(MAC_PLAYERS, "nonsense").is_none());
        assert_eq!(spec_in(WINDOWS_PLAYERS, "mpc-hc").map(|s| s.files), Some(&["mpc-hc64.exe", "mpc-hc.exe"][..]));
        let ids: Vec<&str> = MAC_PLAYERS.iter().map(|p| p.id).collect();
        assert_eq!(&ids[..4], ["com.colliderli.iina", "org.videolan.vlc", "com.firecore.infuse", "com.apple.QuickTimePlayerX"], "Automatic's order");
    }

    #[test]
    fn an_app_runs_with_a_slice_for_this_cpu_or_rosetta() {
        let (arm, intel, both) = (vec![Arch::Arm64], vec![Arch::X86_64], vec![Arch::X86_64, Arch::Arm64]);
        assert!(can_run(Some(&arm), true, false) && !can_run(Some(&arm), false, false));
        assert!(!can_run(Some(&intel), true, false), "Intel-only without Rosetta");
        assert!(can_run(Some(&intel), true, true) && can_run(Some(&intel), false, false));
        assert!(can_run(Some(&both), true, false) && can_run(Some(&both), false, false));
        assert!(!can_run(Some(&[Arch::Other]), true, true));
        assert!(can_run(None, true, false) && can_run(Some(&[]), false, false), "unknown is not held against an app");
    }

    #[test]
    fn a_windows_app_runs_natively_through_wow64_or_x64_emulation() {
        use Arch::*;
        let run = |a: Arch, native: Arch, emu: bool| can_run_on_windows(Some(&[a]), native, emu);
        // An x64 PC: x64 and x86, never arm64.
        assert!(run(X86_64, X86_64, false) && run(X86, X86_64, false));
        assert!(!run(Arm64, X86_64, false) && !run(Arm64, X86_64, true));
        // An Arm PC: arm64 and x86 always, x64 only with Windows 11's emulation.
        assert!(run(Arm64, Arm64, false) && run(X86, Arm64, false));
        assert!(run(X86_64, Arm64, true) && !run(X86_64, Arm64, false), "Windows 10 on Arm has no x64 emulation");
        // A 32-bit PC runs only x86.
        assert!(run(X86, X86, false) && !run(X86_64, X86, true) && !run(Arm64, X86, true));
        assert!(!run(Other, X86_64, true) && !run(Other, Arm64, true) && !run(X86, Other, true));
        assert!(can_run_on_windows(None, Arm64, false) && can_run_on_windows(Some(&[]), X86_64, false), "unknown is not held against an app");
    }

    #[test]
    fn query_values_keep_only_unreserved_characters() {
        assert_eq!(encode("a-b._~Z9"), "a-b._~Z9");
        assert_eq!(encode("a b/c?d=é"), "a%20b%2Fc%3Fd%3D%C3%A9");
    }
}
