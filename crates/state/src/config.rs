//! `config.toml`: every setting of the Settings window (SPEC §8.4), typed, with SPEC defaults.
//!
//! Layout: one table per Settings tab plus `[detection]`. Rates are bytes per second and `0`
//! means unlimited. Paths are strings that may start with `~`. Rule and tracker details live in
//! `rules.toml` / `trackers.toml`; this file only refers to them by id.

use std::fmt;

use serde::de::{self, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use trs_classify::Category;

use crate::doc::{Document, Parsed, parse_toml, render_table, salvage_table};
use crate::duration::Secs;
use crate::error::{DocError, FieldError};
use crate::ids::valid_id;
use crate::serde_util::category_opt;

/// Highest `version` this build reads.
pub const CONFIG_VERSION: u32 = 6;
/// Default for [`Transfers::private_min_seed_secs`]: 10 days (SPEC §7, §7B.5b hit-and-run floor).
pub const DEFAULT_PRIVATE_MIN_SEED_SECS: u64 = 10 * 86_400;
/// Default for [`Transfers::large_torrent_gb`] (SPEC §7A.6).
pub const DEFAULT_LARGE_TORRENT_GB: u32 = 50;
/// Default for [`Transfers::start_delay_secs`] (SPEC §7A.7).
pub const DEFAULT_START_DELAY_SECS: u32 = 10;
/// Largest [`Transfers::start_delay_secs`].
pub const MAX_START_DELAY_SECS: u32 = 600;
/// Largest accepted rate limit in bytes/s (1 TiB/s: far beyond any link, and well inside TOML's i64).
pub const MAX_RATE: u64 = 1 << 40;
const HEADER: &str = "# Seedwell settings. Edits are picked up within a few seconds.\n\
# Comments are not preserved when the app rewrites this file; unknown keys are.\n\n";

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $( $(#[$vm:meta])* $var:ident = $s:literal ),+ $(,)? } default $def:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub enum $name { $( $(#[$vm])* #[serde(rename = $s)] $var ),+ }
        impl Default for $name { fn default() -> Self { $name::$def } }
        impl $name {
            /// The name used in TOML.
            pub const fn as_str(self) -> &'static str { match self { $( $name::$var => $s ),+ } }
        }
    };
}

string_enum! {
    /// What to do on battery power.
    OnBattery { /** Do not change behaviour. */ KeepGoing = "keep_going", /** Apply the alternative speed limits. */ Limit = "limit", /** Pause downloads. */ Pause = "pause" } default Limit
}
string_enum! {
    /// When to show the add panel for a new torrent.
    AskBeforeAdding { /** Never. */ Never = "never", /** Only for multi-file torrents. */ MultiFile = "multi_file", /** Always. */ Always = "always" } default MultiFile
}
string_enum! {
    /// What to do with the `.torrent` file after adding it.
    AfterAdd { /** Move to Trash. */ Trash = "trash", /** Move to the archive folder. */ Archive = "archive", /** Leave it. */ Leave = "leave" } default Trash
}
string_enum! {
    /// Watch-folder behaviour for a new `.torrent`.
    WatchMode { /** Add without asking. */ Auto = "auto", /** Show the add panel. */ Ask = "ask" } default Auto
}
string_enum! {
    /// Peer-wire encryption policy.
    Encryption { /** Plain only. */ Off = "off", /** Prefer encrypted, accept plain. */ Prefer = "prefer", /** Encrypted only. */ Require = "require" } default Prefer
}
string_enum! {
    /// Agent access level (SPEC §9A.3); ordered from least to most access.
    Permission { /** No access. */ Off = "off", /** Status and lists. */ ReadOnly = "read_only", /** Add, pause, limits. */ Control = "control", /** Also remove files, save rules, change settings. */ Full = "full" } default Off
}
string_enum! {
    /// File preallocation strategy.
    Preallocation { /** Sparse files. */ Sparse = "sparse", /** Reserve the full size up front. */ Full = "full" } default Sparse
}
string_enum! {
    /// Log verbosity.
    LogLevel { /** Errors only. */ Error = "error", /** Warnings and errors. */ Warn = "warn", /** Normal. */ Info = "info", /** Verbose. */ Debug = "debug", /** Very verbose. */ Trace = "trace" } default Info
}

impl Permission {
    /// True when this level grants at least `needed`.
    pub fn allows(self, needed: Permission) -> bool {
        self >= needed
    }
}

// ------------------------------------------------------------------ schedule

/// Minutes after local midnight, written `"HH:MM"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TimeOfDay(pub u16);

impl TimeOfDay {
    /// Parse `HH:MM` (00:00 to 23:59).
    pub fn parse(s: &str) -> Result<Self, String> {
        let (h, m) = s.trim().split_once(':').ok_or("expected HH:MM")?;
        if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
            return Err("expected HH:MM".into());
        }
        let (h, m): (u16, u16) = (h.parse().map_err(|_| "bad hour")?, m.parse().map_err(|_| "bad minute")?);
        if h > 23 || m > 59 {
            return Err("hour must be 00-23 and minute 00-59".into());
        }
        Ok(TimeOfDay(h * 60 + m))
    }
}

impl fmt::Display for TimeOfDay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:02}:{:02}", self.0 / 60, self.0 % 60)
    }
}

impl Serialize for TimeOfDay {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for TimeOfDay {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        TimeOfDay::parse(&s).map_err(de::Error::custom)
    }
}

const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// A set of weekdays (bit 0 = Monday). Written as `"weekdays"`, `"weekends"`, `"every_day"`
/// or a list like `["mon", "wed"]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DaySet(pub u8);

impl DaySet {
    /// Monday to Friday.
    pub const WEEKDAYS: DaySet = DaySet(0b001_1111);
    /// Saturday and Sunday.
    pub const WEEKENDS: DaySet = DaySet(0b110_0000);
    /// All seven days.
    pub const EVERY_DAY: DaySet = DaySet(0b111_1111);

    /// True when `weekday` (0 = Monday .. 6 = Sunday) is in the set.
    pub fn contains(self, weekday: u8) -> bool {
        weekday < 7 && self.0 & (1 << weekday) != 0
    }
}

impl Serialize for DaySet {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            DaySet::WEEKDAYS => s.serialize_str("weekdays"),
            DaySet::WEEKENDS => s.serialize_str("weekends"),
            DaySet::EVERY_DAY => s.serialize_str("every_day"),
            DaySet(bits) => {
                let names: Vec<&str> = (0..7).filter(|i| bits & (1 << i) != 0).map(|i| DAY_NAMES[i]).collect();
                names.serialize(s)
            }
        }
    }
}

impl<'de> Deserialize<'de> for DaySet {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        fn day(s: &str) -> Option<u8> {
            DAY_NAMES.iter().position(|n| n.eq_ignore_ascii_case(s.trim())).map(|i| i as u8)
        }
        impl<'de> Visitor<'de> for V {
            type Value = DaySet;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("\"weekdays\", \"weekends\", \"every_day\" or a list of mon..sun")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<DaySet, E> {
                match v.trim().to_ascii_lowercase().as_str() {
                    "weekdays" => Ok(DaySet::WEEKDAYS),
                    "weekends" => Ok(DaySet::WEEKENDS),
                    "every_day" | "everyday" | "daily" => Ok(DaySet::EVERY_DAY),
                    other => day(other)
                        .map(|d| DaySet(1 << d))
                        .ok_or_else(|| E::custom(format!("unknown day set `{v}`"))),
                }
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<DaySet, A::Error> {
                let mut bits = 0u8;
                let mut n = 0;
                while let Some(s) = seq.next_element::<String>()? {
                    n += 1;
                    if n > 64 {
                        return Err(de::Error::custom("too many days"));
                    }
                    bits |= 1 << day(&s).ok_or_else(|| de::Error::custom(format!("unknown day `{s}` (use mon..sun)")))?;
                }
                Ok(DaySet(bits))
            }
        }
        d.deserialize_any(V)
    }
}

/// When the alternative speed limits switch on by themselves.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Schedule {
    /// Master switch for the schedule.
    pub enabled: bool,
    /// Days the window starts on.
    pub days: DaySet,
    /// Window start (local time).
    pub from: TimeOfDay,
    /// Window end (local time). Earlier than `from` means the window ends the next morning.
    pub to: TimeOfDay,
}

impl Default for Schedule {
    fn default() -> Self {
        Schedule { enabled: false, days: DaySet::WEEKDAYS, from: TimeOfDay(8 * 60), to: TimeOfDay(17 * 60) }
    }
}

impl Schedule {
    /// Is the window open at local `weekday` (0 = Monday) and `minute` of the day (0..1440)?
    ///
    /// The schedule must be `enabled`. A window that crosses midnight belongs to the day it starts on.
    pub fn active_at(&self, weekday: u8, minute: u16) -> bool {
        if !self.enabled {
            return false;
        }
        let (from, to) = (self.from.0, self.to.0);
        if from < to {
            self.days.contains(weekday) && minute >= from && minute < to
        } else if from > to {
            (self.days.contains(weekday) && minute >= from)
                || (self.days.contains((weekday + 6) % 7) && minute < to)
        } else {
            false
        }
    }
}

// ------------------------------------------------------------------ sections

/// General tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Start at login.
    pub launch_at_login: bool,
    /// Notify when a download finishes.
    pub notify_finished: bool,
    /// Notify on errors.
    pub notify_errors: bool,
    /// Notify when a seeding goal is reached.
    pub notify_goal: bool,
    /// Unused since 2026-10-13: the menu bar icon shows no text, only coloured arrows (kept so older
    /// config files load unchanged).
    pub menu_bar_show_speed: bool,
    /// Split the sharing line by tracker.
    pub menu_bar_split_by_tracker: bool,
    /// Keep the Mac awake while downloading.
    pub prevent_sleep_while_downloading: bool,
    /// Keep the Mac awake while an agent says it is working (SPEC 9A.7); the screen still locks.
    pub keep_awake_for_agents: bool,
    /// Behaviour on battery.
    pub on_battery: OnBattery,
    /// Which app "Play" uses for videos (SPEC 7F.3): `"auto"` (IINA, VLC, Infuse, then QuickTime)
    /// or the bundle identifier of a player, e.g. `"org.videolan.vlc"`. App-only preference; the
    /// engine never reads it.
    pub play_with: String,
}

/// The default of [`General::play_with`].
pub const PLAY_WITH_AUTO: &str = "auto";

impl Default for General {
    fn default() -> Self {
        General {
            launch_at_login: false,
            notify_finished: true,
            notify_errors: true,
            notify_goal: false,
            menu_bar_show_speed: false,
            menu_bar_split_by_tracker: false,
            prevent_sleep_while_downloading: true,
            keep_awake_for_agents: true,
            on_battery: OnBattery::Limit,
            play_with: PLAY_WITH_AUTO.to_owned(),
        }
    }
}

/// Updates (Settings > General). Both only act in release builds of the app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Updates {
    /// Look for a newer release once a day.
    pub check: bool,
    /// Install a found update by itself when nothing is downloading.
    pub auto_install: bool,
}

impl Default for Updates {
    fn default() -> Self {
        Updates { check: true, auto_install: false }
    }
}

/// Alternative ("Limit Speed") limits and their schedule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AltSpeed {
    /// Manual toggle (⌥⌘L): alternative limits in force regardless of the schedule.
    pub enabled: bool,
    /// Alternative download limit, bytes/s, 0 = unlimited.
    pub download_limit: u64,
    /// Alternative upload limit, bytes/s, 0 = unlimited.
    pub upload_limit: u64,
    /// Automatic window.
    pub schedule: Schedule,
}

impl Default for AltSpeed {
    fn default() -> Self {
        AltSpeed { enabled: false, download_limit: 2_000_000, upload_limit: 500_000, schedule: Schedule::default() }
    }
}

/// Transfers tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Transfers {
    /// Global download limit, bytes/s, 0 = unlimited.
    pub download_limit: u64,
    /// Global upload limit, bytes/s, 0 = unlimited.
    pub upload_limit: u64,
    /// Alternative limits.
    pub alt: AltSpeed,
    /// Active downloads (torrents and direct downloads together); the rest queue.
    pub max_active_downloads: u32,
    /// Connections per host for direct downloads.
    pub max_connections_per_host: u32,
    /// When to ask before adding.
    pub ask_before_adding: AskBeforeAdding,
    /// A torrent larger than this many GB (10^9 bytes) is held for a choice before anything is
    /// announced or downloaded (SPEC §7A.6 "Very large"). `0` switches the size trigger off.
    pub large_torrent_gb: u32,
    /// Grace period, seconds, before an auto-added torrent starts (SPEC §7A.7): it is held, silent
    /// on the network, and a notifier counts down. `0` starts at once.
    pub start_delay_secs: u32,
    /// Hit-and-run floor for private torrents (`info.private = 1`), seconds: their seeding goal
    /// cannot count as reached before they have seeded this long, unless the rule that applies has
    /// its own `min_seed_time`. `0` = off. Public torrents are unaffected (SPEC §7, §7B).
    pub private_min_seed_secs: u64,
    /// Stop seeding a finished torrent that uploaded less than this many bytes during the last
    /// `idle_window_hours` hours of seeding (SPEC §7.5). `0` = off. A rule's `stop_when_idle` wins.
    pub idle_min_upload_bytes: u64,
    /// The window of `idle_min_upload_bytes`, hours of seeding.
    pub idle_window_hours: u32,
}

impl Default for Transfers {
    fn default() -> Self {
        Transfers {
            download_limit: 0,
            upload_limit: 0,
            alt: AltSpeed::default(),
            max_active_downloads: 3,
            max_connections_per_host: 4,
            ask_before_adding: AskBeforeAdding::MultiFile,
            large_torrent_gb: DEFAULT_LARGE_TORRENT_GB,
            start_delay_secs: DEFAULT_START_DELAY_SECS,
            private_min_seed_secs: DEFAULT_PRIVATE_MIN_SEED_SECS,
            idle_min_upload_bytes: 0,
            idle_window_hours: crate::rules::DEFAULT_IDLE_HOURS,
        }
    }
}

/// One watched folder (SPEC §7A.1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchFolder {
    /// Folder to watch.
    pub path: String,
    /// Switch off without deleting.
    pub enabled: bool,
    /// Add automatically or ask first.
    pub mode: WatchMode,
    /// What to do with the `.torrent` afterwards.
    pub after_add: AfterAdd,
    /// Archive folder (required when `after_add = "archive"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_dir: Option<String>,
    /// Always treat torrents from this folder as this category.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "category_opt")]
    pub category: Option<Category>,
    /// Always apply this rule id (from `rules.toml`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Unix seconds when this folder was added to the watch list. Only `.torrent` files that
    /// appeared after it are picked up; older ones are the "backlog" the app may offer once.
    /// The engine host stamps it (first launch, new entry, re-enabled entry); `None` is stamped
    /// at the next load. Never hand-edited in practice.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

impl Default for WatchFolder {
    fn default() -> Self {
        WatchFolder {
            path: "~/Downloads".into(),
            enabled: true,
            mode: WatchMode::Auto,
            after_add: AfterAdd::Trash,
            archive_dir: None,
            category: None,
            rule: None,
            since: None,
        }
    }
}

/// Per-category location overrides (SPEC §7A.4). Both optional; unset means "the app default".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocationOverride {
    /// Where this category's downloads are written while incomplete (staging root; each torrent
    /// gets `<dir>/<info-hash>/`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incomplete_dir: Option<String>,
    /// Where finished files of this category live: a plain folder (`/Volumes/Media/Movies`; the
    /// torrent's own layout goes below it, SPEC §7A.3). A rule's `finished_dir` still wins over it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_dir: Option<String>,
}

impl LocationOverride {
    /// Neither location is overridden.
    pub fn is_empty(&self) -> bool {
        self.incomplete_dir.is_none() && self.finished_dir.is_none()
    }
}

/// The per-category overrides, `[folders.categories.<name>]`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CategoryLocations {
    /// Movies.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub movie: LocationOverride,
    /// TV.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub tv: LocationOverride,
    /// Music.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub music: LocationOverride,
    /// Audiobooks.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub audiobook: LocationOverride,
    /// Books.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub book: LocationOverride,
    /// Software.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub software: LocationOverride,
    /// Everything else.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub other: LocationOverride,
    /// Games.
    #[serde(skip_serializing_if = "LocationOverride::is_empty")]
    pub game: LocationOverride,
}

impl CategoryLocations {
    /// No category overrides anything.
    pub fn is_empty(&self) -> bool {
        Category::ALL.iter().all(|c| self.get(*c).is_empty())
    }
    /// The overrides of `c`.
    pub fn get(&self, c: Category) -> &LocationOverride {
        match c {
            Category::Movie => &self.movie,
            Category::Tv => &self.tv,
            Category::Music => &self.music,
            Category::Audiobook => &self.audiobook,
            Category::Book => &self.book,
            Category::Software => &self.software,
            Category::Other => &self.other,
            Category::Game => &self.game,
        }
    }
    /// Mutable access to the overrides of `c`.
    pub fn get_mut(&mut self, c: Category) -> &mut LocationOverride {
        match c {
            Category::Movie => &mut self.movie,
            Category::Tv => &mut self.tv,
            Category::Music => &mut self.music,
            Category::Audiobook => &mut self.audiobook,
            Category::Book => &mut self.book,
            Category::Software => &mut self.software,
            Category::Other => &mut self.other,
            Category::Game => &mut self.game,
        }
    }
}

/// Folders tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Folders {
    /// The app-level default **finished location** (SPEC §7A.4): where finished torrents live,
    /// each with its own layout below it. (Named `download_dir` since version 1.)
    pub download_dir: String,
    /// The app-level default **incomplete location**: staging root for unfinished downloads
    /// (same volume as the finished location recommended; a different volume makes finishing
    /// a copy, see `trs_state::volume`).
    pub incomplete_dir: String,
    /// Organize finished torrents by sharing state (SPEC §7A.3): `Active/`, `Dormant/`, `Stopped/`
    /// and `Dead/` below their finished location. Off keeps the layout flat.
    pub by_status: bool,
    /// `.torrent` handling for drops, double-clicks and magnet-less adds.
    pub torrent_after_add: AfterAdd,
    /// Archive folder for `torrent_after_add = "archive"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_dir: Option<String>,
    /// Watched folders, in order.
    pub watch: Vec<WatchFolder>,
    /// Per-category overrides of the incomplete and finished locations (SPEC §7A.4).
    #[serde(skip_serializing_if = "CategoryLocations::is_empty")]
    pub categories: CategoryLocations,
}

impl Default for Folders {
    fn default() -> Self {
        Folders {
            download_dir: "~/Downloads".into(),
            incomplete_dir: "~/Downloads/.incomplete".into(),
            by_status: false,
            torrent_after_add: AfterAdd::Trash,
            archive_dir: None,
            watch: vec![WatchFolder::default()],
            categories: CategoryLocations::default(),
        }
    }
}

/// Content detection toggles (Folders tab, "Detection").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Detection {
    /// Leave samples, `.nfo` and other junk unchecked.
    pub skip_junk: bool,
    /// Ask when less confident than `confidence_threshold`.
    pub ask_when_unsure: bool,
    /// Confidence below which the category is `other` / asks (SPEC: 0.8).
    pub confidence_threshold: f64,
    /// Online title lookup (opt-in, not in v1).
    pub online_lookup: bool,
}

impl Default for Detection {
    fn default() -> Self {
        Detection { skip_junk: true, ask_when_unsure: true, confidence_threshold: 0.8, online_lookup: false }
    }
}

/// Network tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Network {
    /// TCP/uTP listen port; 0 (or absent) = a random port at every start, never written back. Only a
    /// manual choice stores a number (a number stored by an older version counts as a manual choice).
    pub listen_port: u16,
    /// DHT for public torrents.
    pub dht: bool,
    /// Peer exchange for public torrents.
    pub pex: bool,
    /// Local service discovery for public torrents.
    pub lsd: bool,
    /// UPnP port mapping.
    pub upnp: bool,
    /// NAT-PMP port mapping.
    pub natpmp: bool,
    /// uTP transport.
    pub utp: bool,
    /// IPv6.
    pub ipv6: bool,
    /// Encryption policy.
    pub encryption: Encryption,
    /// Global connection cap.
    pub max_connections: u32,
    /// Per-torrent connection cap.
    pub max_connections_per_torrent: u32,
    /// Trackers on a private network (RFC 1918, CGNAT, ULA: a LAN or VPN tracker) may be
    /// contacted. Off by default: a tracker URL comes from a stranger's `.torrent`, and the
    /// strict policy keeps it from aiming requests at the user's router or local services
    /// (SPEC 7C, `trs-tracker` destination policy). Loopback, link-local and cloud metadata
    /// addresses stay refused whatever this says. Takes effect when the engine starts.
    pub allow_lan_trackers: bool,
}

impl Default for Network {
    fn default() -> Self {
        Network {
            listen_port: 0,
            dht: true,
            pex: true,
            lsd: true,
            upnp: true,
            natpmp: true,
            utp: true,
            ipv6: true,
            encryption: Encryption::Prefer,
            max_connections: 200,
            max_connections_per_torrent: 50,
            allow_lan_trackers: false,
        }
    }
}

/// Agents tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agents {
    /// Access level. `off` until the user enables it; the Settings toggle sets `control` (SPEC §12 #14).
    pub permission: Permission,
    /// Notify when an agent changes something.
    pub notify_changes: bool,
    /// Calls per second allowed per client.
    pub rate_limit_per_sec: u32,
}

impl Default for Agents {
    fn default() -> Self {
        Agents { permission: Permission::Off, notify_changes: true, rate_limit_per_sec: 20 }
    }
}

/// Power-tier timings (docs/ENGINE.md §3).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tiers {
    /// Hot torrent with no interest drops to Warm after this long.
    pub hot_to_warm: Secs,
    /// Warm torrent with no interest drops to Cold after this long (total idle = both).
    pub warm_to_cold: Secs,
    /// Uploading this long promotes Warm to Hot.
    pub promote_upload_after: Secs,
    /// This many interested peers promote Warm to Hot.
    pub promote_interested_peers: u32,
}

impl Default for Tiers {
    fn default() -> Self {
        Tiers {
            hot_to_warm: Secs::minutes(5),
            warm_to_cold: Secs::minutes(5),
            promote_upload_after: Secs::from_secs(60),
            promote_interested_peers: 2,
        }
    }
}

/// Logging.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Logging {
    /// Verbosity.
    pub level: LogLevel,
    /// Rotate a log file at this size.
    pub max_file_mib: u32,
    /// Rotated files kept.
    pub keep_files: u32,
    /// Log folder (default: `~/Library/Logs/Seedwell`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

impl Default for Logging {
    fn default() -> Self {
        Logging { level: LogLevel::Info, max_file_mib: 8, keep_files: 5, dir: None }
    }
}

/// Advanced tab.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    /// Tier timings.
    pub tiers: Tiers,
    /// Write cache, MiB.
    pub write_cache_mib: u32,
    /// Preallocation.
    pub preallocation: Preallocation,
    /// Verify files when a download completes.
    pub verify_on_completion: bool,
    /// Slowly re-verify cold torrents in the background.
    pub background_scrub: bool,
    /// Only scrub on AC power.
    pub scrub_on_ac_only: bool,
    /// File-descriptor cache size.
    pub fd_limit: u32,
    /// Peers remembered per torrent for fast restarts (0 = off).
    pub peers_cache_per_torrent: u32,
    /// Page cache of `state.redb`, MiB.
    pub db_cache_mib: u32,
    /// Speed limit of the copy when a torrent moves to another volume, MiB/s (0 = unlimited, the
    /// default: the copy reads and writes without caching, so seeding stays responsive; SPEC §7A.5).
    pub move_limit_mib: u32,
    /// Developer mode: every tracker exchange (request, response, schedule, decisions) is written,
    /// with secrets masked, to `logs/trackers.log` in the state folder (SPEC 8.7). Off by default.
    pub developer_mode: bool,
    /// Logging.
    pub logging: Logging,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced {
            tiers: Tiers::default(),
            write_cache_mib: 32,
            preallocation: Preallocation::Sparse,
            verify_on_completion: true,
            background_scrub: true,
            scrub_on_ac_only: true,
            fd_limit: 256,
            peers_cache_per_torrent: 64,
            db_cache_mib: 4,
            move_limit_mib: 0,
            developer_mode: false,
            logging: Logging::default(),
        }
    }
}

/// The whole of `config.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// File format version.
    pub version: u32,
    /// General tab.
    pub general: General,
    /// Transfers tab.
    pub transfers: Transfers,
    /// Folders tab.
    pub folders: Folders,
    /// Detection toggles.
    pub detection: Detection,
    /// Network tab.
    pub network: Network,
    /// Agents tab.
    pub agents: Agents,
    /// Updates group of the General tab.
    pub updates: Updates,
    /// Advanced tab.
    pub advanced: Advanced,
    /// Keys this build does not know, preserved verbatim when saving.
    #[serde(skip)]
    pub unknown: toml::Table,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            general: General::default(),
            transfers: Transfers::default(),
            folders: Folders::default(),
            detection: Detection::default(),
            network: Network::default(),
            agents: Agents::default(),
            updates: Updates::default(),
            advanced: Advanced::default(),
            unknown: toml::Table::new(),
        }
    }
}

// ------------------------------------------------------------------ validation

pub(crate) fn check_path(errs: &mut Vec<FieldError>, path: &str, v: &str) {
    if v.is_empty() {
        errs.push(FieldError::new(path, "must not be empty"));
    } else if v.len() > 4096 {
        errs.push(FieldError::new(path, "longer than 4096 bytes"));
    } else if v.chars().any(char::is_control) {
        errs.push(FieldError::new(path, "contains a control character"));
    } else if !(std::path::Path::new(v).is_absolute() || v == "~" || v.starts_with("~/")) {
        errs.push(FieldError::new(path, "must be absolute or start with `~/`"));
    }
}

pub(crate) fn check_range(errs: &mut Vec<FieldError>, path: &str, v: u64, lo: u64, hi: u64) {
    if v < lo || v > hi {
        errs.push(FieldError::new(path, format!("must be between {lo} and {hi} (got {v})")));
    }
}

/// What a location string of an older version (it could be a destination template, SPEC §7A.3 of
/// 2026-10-13) means now: the fixed folder in front of the first `{`, cut back to a whole folder.
/// A plain folder is returned unchanged (without a trailing `/`). Empty when nothing is left.
pub fn fixed_prefix(s: &str) -> String {
    let Some(at) = s.find('{') else { return s.trim_end_matches('/').to_owned() };
    match s[..at].rfind('/') {
        Some(i) if i > 0 => s[..i].to_owned(),
        _ => String::new(),
    }
}

impl Config {
    /// Collect every problem (not just the first).
    pub fn validate(&self) -> Vec<FieldError> {
        let mut e = Vec::new();
        if self.version == 0 || self.version > CONFIG_VERSION {
            e.push(FieldError::new("version", format!("must be 1..={CONFIG_VERSION}")));
        }
        let pw = &self.general.play_with;
        if pw.is_empty() || pw.len() > 255 || pw.chars().any(|c| c.is_control() || c.is_whitespace()) {
            e.push(FieldError::new(
                "general.play_with",
                "must be \"auto\" or the bundle identifier of a player (no spaces)",
            ));
        }
        let t = &self.transfers;
        for (path, v) in [
            ("transfers.download_limit", t.download_limit),
            ("transfers.upload_limit", t.upload_limit),
            ("transfers.alt.download_limit", t.alt.download_limit),
            ("transfers.alt.upload_limit", t.alt.upload_limit),
        ] {
            check_range(&mut e, path, v, 0, MAX_RATE);
        }
        check_range(&mut e, "transfers.max_active_downloads", t.max_active_downloads.into(), 1, 100);
        check_range(&mut e, "transfers.max_connections_per_host", t.max_connections_per_host.into(), 1, 16);
        check_range(&mut e, "transfers.large_torrent_gb", t.large_torrent_gb.into(), 0, 1_000_000);
        check_range(&mut e, "transfers.start_delay_secs", t.start_delay_secs.into(), 0, MAX_START_DELAY_SECS.into());
        check_range(&mut e, "transfers.private_min_seed_secs", t.private_min_seed_secs, 0, crate::duration::MAX_SECS);
        check_range(&mut e, "transfers.idle_min_upload_bytes", t.idle_min_upload_bytes, 0, crate::rules::MAX_IDLE_BYTES);
        check_range(&mut e, "transfers.idle_window_hours", t.idle_window_hours.into(), 1, crate::rules::MAX_IDLE_HOURS.into());
        let s = &t.alt.schedule;
        if s.days.0 == 0 || s.days.0 > 0b111_1111 {
            e.push(FieldError::new("transfers.alt.schedule.days", "must name at least one day"));
        }
        if s.from == s.to {
            e.push(FieldError::new("transfers.alt.schedule.to", "must differ from `from` (use 00:00-23:59 for all day)"));
        }

        let f = &self.folders;
        check_path(&mut e, "folders.download_dir", &f.download_dir);
        check_path(&mut e, "folders.incomplete_dir", &f.incomplete_dir);
        match (&f.archive_dir, f.torrent_after_add) {
            // Blamed on the choice that cannot work: a file with bad values then keeps every other
            // value and this one leaves the .torrent alone (a missing key cannot be replaced).
            (None, AfterAdd::Archive) => {
                e.push(FieldError::new("folders.torrent_after_add", "\"archive\" needs folders.archive_dir, the folder to move the .torrent files to"));
            }
            (Some(d), _) => check_path(&mut e, "folders.archive_dir", d),
            _ => {}
        }
        for c in Category::ALL {
            let o = f.categories.get(c);
            let p = format!("folders.categories.{}", c.as_str());
            if let Some(d) = &o.incomplete_dir {
                check_path(&mut e, &format!("{p}.incomplete_dir"), d);
            }
            if let Some(d) = &o.finished_dir {
                check_path(&mut e, &format!("{p}.finished_dir"), d);
            }
        }
        if f.watch.len() > 64 {
            e.push(FieldError::new("folders.watch", "more than 64 watch folders"));
        }
        for (i, w) in f.watch.iter().enumerate() {
            let p = format!("folders.watch[{i}]");
            check_path(&mut e, &format!("{p}.path"), &w.path);
            if f.watch[..i].iter().any(|o| o.path == w.path) {
                e.push(FieldError::new(format!("{p}.path"), format!("duplicate watch folder {}", w.path)));
            }
            match (&w.archive_dir, w.after_add) {
                (None, AfterAdd::Archive) => {
                    e.push(FieldError::new(format!("{p}.archive_dir"), "required when after_add = \"archive\""));
                }
                (Some(d), _) => check_path(&mut e, &format!("{p}.archive_dir"), d),
                _ => {}
            }
            if let Some(r) = &w.rule
                && !valid_id(r)
            {
                e.push(FieldError::new(format!("{p}.rule"), "not a valid rule id (a-z, 0-9, `-`, `_`; up to 64)"));
            }
        }

        let d = &self.detection;
        if !(d.confidence_threshold.is_finite() && (0.5..=1.0).contains(&d.confidence_threshold)) {
            e.push(FieldError::new("detection.confidence_threshold", "must be between 0.5 and 1.0"));
        }

        let n = &self.network;
        if n.listen_port != 0 && n.listen_port < 1024 {
            e.push(FieldError::new("network.listen_port", "must be 0 (random) or 1024-65535"));
        }
        check_range(&mut e, "network.max_connections", n.max_connections.into(), 1, 10_000);
        check_range(
            &mut e,
            "network.max_connections_per_torrent",
            n.max_connections_per_torrent.into(),
            1,
            u64::from(n.max_connections.max(1)),
        );
        check_range(&mut e, "agents.rate_limit_per_sec", self.agents.rate_limit_per_sec.into(), 1, 1000);

        let a = &self.advanced;
        let tiers = &a.tiers;
        check_range(&mut e, "advanced.tiers.hot_to_warm", tiers.hot_to_warm.0, 10, 86_400);
        check_range(&mut e, "advanced.tiers.warm_to_cold", tiers.warm_to_cold.0, 10, 86_400);
        check_range(&mut e, "advanced.tiers.promote_upload_after", tiers.promote_upload_after.0, 1, 3600);
        check_range(&mut e, "advanced.tiers.promote_interested_peers", tiers.promote_interested_peers.into(), 1, 1000);
        check_range(&mut e, "advanced.write_cache_mib", a.write_cache_mib.into(), 1, 4096);
        check_range(&mut e, "advanced.fd_limit", a.fd_limit.into(), 16, 8192);
        check_range(&mut e, "advanced.peers_cache_per_torrent", a.peers_cache_per_torrent.into(), 0, 256);
        check_range(&mut e, "advanced.db_cache_mib", a.db_cache_mib.into(), 1, 256);
        check_range(&mut e, "advanced.move_limit_mib", a.move_limit_mib.into(), 0, 100_000);
        check_range(&mut e, "advanced.logging.max_file_mib", a.logging.max_file_mib.into(), 1, 1024);
        check_range(&mut e, "advanced.logging.keep_files", a.logging.keep_files.into(), 1, 100);
        if let Some(d) = &a.logging.dir {
            check_path(&mut e, "advanced.logging.dir", d);
        }
        e
    }

    /// Stamp the watch folders that have no baseline yet (SPEC §7A.1: only `.torrent` files that
    /// appear after a folder joined the list are picked up). `previous` is the configuration that
    /// was in force before this one (`None` at start-up). An entry gets `since = now` when it has
    /// none, when its path was not in `previous` (a new or changed entry), or when it was
    /// disabled in `previous` and is enabled now. Returns true if anything changed (the caller
    /// then saves the file). Entries that keep their path and stay enabled keep their baseline.
    pub fn stamp_watch_baselines(&mut self, previous: Option<&Config>, now: u64) -> bool {
        let mut changed = false;
        for w in &mut self.folders.watch {
            let before = previous.and_then(|p| p.folders.watch.iter().find(|o| o.path == w.path));
            let fresh = match (previous, before) {
                (Some(_), None) => true,
                (Some(_), Some(o)) => !o.enabled && w.enabled,
                (None, _) => false,
            };
            if (w.since.is_none() || fresh) && w.since != Some(now) {
                w.since = Some(now);
                changed = true;
            }
        }
        changed
    }

    /// Effective global limits `(download, upload)` in bytes/s (`None` = unlimited) given whether
    /// the alternative limits are in force right now.
    pub fn effective_limits(&self, alt_active: bool) -> (Option<u64>, Option<u64>) {
        let nz = |v: u64| (v != 0).then_some(v);
        if alt_active {
            (nz(self.transfers.alt.download_limit), nz(self.transfers.alt.upload_limit))
        } else {
            (nz(self.transfers.download_limit), nz(self.transfers.upload_limit))
        }
    }
}

fn category_finished_dirs(t: &toml::Table) -> impl Iterator<Item = &str> {
    t.get("folders")
        .and_then(toml::Value::as_table)
        .and_then(|f| f.get("categories"))
        .and_then(toml::Value::as_table)
        .into_iter()
        .flat_map(|c| c.values())
        .filter_map(|v| v.as_table()?.get("finished_dir")?.as_str())
}

/// Does the file carry keys that were retired?
fn needs_migration(t: &toml::Table) -> bool {
    let folders = t.get("folders").and_then(toml::Value::as_table);
    folders.is_some_and(|f| f.contains_key("seeding") || f.contains_key("placement") || f.contains_key("templates"))
        || category_finished_dirs(t).any(|d| d.contains('{'))
}

/// Files written before the flat layout (SPEC §7A.3, 2026-10-14) have keys that no longer exist:
///
/// * `folders.templates` and `folders.placement` (destination templates, "place & rename",
///   "hardlink") are dropped. Torrents already placed keep their recorded paths; nothing moves.
/// * A template-valued `folders.categories.<name>.finished_dir` becomes its fixed folder (the part
///   before the first `{`, see [`fixed_prefix`]); when nothing is left the override is dropped, so
///   the category uses the app default.
/// * `[folders.seeding]` (`mode`, `dir`) is older still, from before the location model of §7A.4: a
///   separate "seeding folder" used when organize was off. With placement `off` or `hardlink`, a
///   fixed `dir` becomes the finished location (`folders.download_dir`, which is where those
///   torrents were saved anyway), otherwise it was never used and is dropped.
///
/// Returns the rewritten text, or `None` when the file has none of these (then it is parsed
/// untouched, with its error positions intact).
fn migrate_retired_keys(text: &str) -> Option<String> {
    let mut t: toml::Table = text.parse().ok()?;
    if !needs_migration(&t) {
        return None;
    }
    let folders = t.get_mut("folders")?.as_table_mut()?;
    let placement = folders.remove("placement");
    let placement = placement.as_ref().and_then(toml::Value::as_str).unwrap_or("place_rename");
    folders.remove("templates");
    let legacy = folders.remove("seeding");
    let fixed_dir = legacy.as_ref().and_then(toml::Value::as_table).and_then(|s| {
        (s.get("mode").and_then(toml::Value::as_str) == Some("fixed"))
            .then(|| s.get("dir").and_then(toml::Value::as_str).map(str::to_owned))
            .flatten()
    });
    if let Some(dir) = fixed_dir
        && matches!(placement, "off" | "hardlink")
    {
        folders.insert("download_dir".into(), toml::Value::String(dir));
    }
    if let Some(cats) = folders.get_mut("categories").and_then(toml::Value::as_table_mut) {
        for cat in cats.iter_mut().filter_map(|(_, v)| v.as_table_mut()) {
            let Some(d) = cat.get("finished_dir").and_then(toml::Value::as_str).filter(|d| d.contains('{')) else {
                continue;
            };
            let plain = fixed_prefix(d);
            if plain.is_empty() {
                cat.remove("finished_dir");
            } else {
                cat.insert("finished_dir".into(), toml::Value::String(plain));
            }
        }
    }
    toml::to_string(&t).ok()
}

/// The settings that move, trash or delete the user's files, with the value of each that leaves
/// them alone: what a bad value of one becomes when the backup has no usable value for it (SPEC §10).
///
/// * `folders.torrent_after_add` moves the added `.torrent` to the Trash or the archive folder:
///   `"leave"` leaves it where it is.
/// * `folders.by_status` files finished torrents into `Active/`, `Dormant/`, `Stopped/` and `Dead/`
///   as their state changes (SPEC 7A.3): `false` moves nothing.
///
/// The others are not single values: a bad `[[folders.watch]]` entry (its `after_add` trashes or
/// archives what it picks up) is ignored as a whole, so that folder is not watched and nothing in
/// it is touched; a bad `folders.archive_dir` takes the backup's folder or none, and then
/// `torrent_after_add = "archive"` without a folder is refused on `torrent_after_add` itself.
pub fn leaves_files_alone(key: &str) -> Option<toml::Value> {
    match key {
        "folders.torrent_after_add" => Some(toml::Value::String(AfterAdd::Leave.as_str().to_owned())),
        "folders.by_status" => Some(toml::Value::Boolean(false)),
        _ => None,
    }
}

impl Document for Config {
    const FILE: &'static str = "config.toml";

    fn defaults() -> Self {
        Config::default()
    }

    fn parse_str(text: &str) -> Result<Parsed<Self>, DocError> {
        let migrated = migrate_retired_keys(text);
        let text = migrated.as_deref().unwrap_or(text);
        let (mut cfg, unknown, warnings) = parse_toml::<Config>(Self::FILE, text, CONFIG_VERSION)?;
        cfg.unknown = unknown;
        // Older files are read as they are (every new key has a default) and become the current
        // version the next time they are written; `file_is_outdated` makes the store do that
        // right after loading, with the usual backup of the old file.
        // v5: the write cache default went from 64 to 32 MiB (same throughput, half the peak). Older
        // files carry the old default written out; a 64 there is that default, not a choice, so it
        // follows the new one. Any other value was set on purpose and stays.
        if cfg.version < 5 && cfg.advanced.write_cache_mib == 64 {
            cfg.advanced.write_cache_mib = 32;
        }
        // v6: a move is no longer throttled by default (0 = unlimited; it was 100 MiB/s). An old
        // file carries the old default written out; exactly 100 is that default, not a choice.
        if cfg.version < 6 && cfg.advanced.move_limit_mib == 100 {
            cfg.advanced.move_limit_mib = 0;
        }
        if cfg.version < CONFIG_VERSION {
            cfg.version = CONFIG_VERSION;
        }
        let errors = cfg.validate();
        if !errors.is_empty() {
            return Err(DocError::Invalid { file: Self::FILE, errors });
        }
        Ok(Parsed { value: cfg, warnings })
    }

    fn salvage(text: &str, last_good: Option<&str>) -> Option<Parsed<Self>> {
        let table = |text: &str| -> Option<toml::Table> { migrate_retired_keys(text).as_deref().unwrap_or(text).parse().ok() };
        let input = table(text)?;
        // A file of a newer format (or no number at all) is never half-read.
        if let Some(v) = input.get("version") {
            v.as_integer().filter(|n| (1..=i64::from(CONFIG_VERSION)).contains(n))?;
        }
        let defaults = toml::Table::try_from(Config::default()).ok()?;
        let backup = last_good.filter(|t| Self::parse_str(t).is_ok()).and_then(table);
        let (good, mut warnings) = salvage_table::<Config>(&input, &defaults, backup.as_ref(), leaves_files_alone, Config::validate)?;
        // The kept values go through the normal path (migrations, unknown keys).
        let parsed = Self::parse_str(&toml::to_string(&good).ok()?).ok()?;
        warnings.extend(parsed.warnings);
        Some(Parsed { value: parsed.value, warnings })
    }

    fn file_is_outdated(text: &str) -> bool {
        let table = text.parse::<toml::Table>().ok();
        table
            .as_ref()
            .and_then(|t| t.get("version").and_then(toml::Value::as_integer))
            .is_some_and(|v| v < i64::from(CONFIG_VERSION))
            || table.as_ref().is_some_and(needs_migration)
    }

    fn render(&self) -> Result<String, DocError> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(DocError::Invalid { file: Self::FILE, errors });
        }
        let table = toml::Table::try_from(self).map_err(|e| DocError::Parse {
            file: Self::FILE,
            line: None,
            column: None,
            message: e.to_string(),
        })?;
        render_table(Self::FILE, table, &self.unknown, HEADER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Unix-style absolute test path made absolute on this system: unchanged on Unix, a drive letter in front on Windows.
    fn abs(p: &str) -> String {
        if cfg!(windows) { format!("C:{p}") } else { p.to_string() }
    }

    /// A TOML test document whose quoted Unix-style absolute paths (`"/x"`) are made absolute on this system.
    fn win(doc: &str) -> String {
        if cfg!(windows) { doc.replace("\"/", "\"C:/") } else { doc.to_string() }
    }

    #[test]
    fn defaults_validate_and_roundtrip() {
        let c = Config::default();
        assert!(c.validate().is_empty(), "{:?}", c.validate());
        let text = c.render().unwrap();
        let back = Config::parse_str(&text).unwrap();
        assert_eq!(back.value, c);
        assert!(back.warnings.is_empty());
    }

    #[test]
    fn developer_mode_is_off_by_default_and_round_trips() {
        assert!(!Config::default().advanced.developer_mode);
        let absent = Config::parse_str("[advanced]\nmove_limit_mib = 5\n").unwrap();
        assert!(!absent.value.advanced.developer_mode, "absent means off");
        let mut c = Config::default();
        c.advanced.developer_mode = true;
        assert!(c.validate().is_empty());
        let out = c.render().unwrap();
        assert!(out.contains("developer_mode = true"), "{out}");
        let again = Config::parse_str(&out).unwrap();
        assert!(again.value.advanced.developer_mode);
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    }

    #[test]
    fn updates_default_and_round_trip() {
        let d = Config::default().updates;
        assert!(d.check && !d.auto_install);
        let old = Config::parse_str("version = 6\n[general]\nlaunch_at_login = true\n").unwrap();
        assert!(old.value.updates.check && !old.value.updates.auto_install, "absent section means defaults");
        assert!(old.warnings.is_empty(), "{:?}", old.warnings);
        let c = Config { updates: Updates { check: false, auto_install: true }, ..Config::default() };
        let out = c.render().unwrap();
        assert!(out.contains("[updates]") && out.contains("auto_install = true"), "{out}");
        let again = Config::parse_str(&out).unwrap();
        assert_eq!(again.value.updates, Updates { check: false, auto_install: true });
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    }

    #[test]
    fn play_with_defaults_to_auto_and_round_trips() {
        assert_eq!(Config::default().general.play_with, "auto");
        let p = Config::parse_str("[general]\nlaunch_at_login = true\n").unwrap();
        assert_eq!(p.value.general.play_with, "auto", "absent means auto");
        let mut c = Config::default();
        c.general.play_with = "com.colliderli.iina".into();
        assert!(c.validate().is_empty());
        let out = c.render().unwrap();
        assert!(out.contains("play_with = \"com.colliderli.iina\""), "{out}");
        let again = Config::parse_str(&out).unwrap();
        assert_eq!(again.value.general.play_with, "com.colliderli.iina");
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    }

    #[test]
    fn play_with_rejects_empty_and_spaced_values() {
        for bad in ["", "my player", "a\tb"] {
            let mut c = Config::default();
            c.general.play_with = bad.into();
            let errs = c.validate();
            assert!(errs.iter().any(|e| e.path == "general.play_with"), "{bad:?}: {errs:?}");
        }
    }

    #[test]
    fn empty_file_is_all_defaults() {
        assert_eq!(Config::parse_str("").unwrap().value, Config::default());
    }

    #[test]
    fn the_listen_port_is_automatic_unless_a_number_is_stored() {
        // Absent and 0 both mean "random at every start"; the default is that.
        assert_eq!(Config::default().network.listen_port, 0);
        assert_eq!(Config::parse_str("[network]\ndht = true\n").unwrap().value.network.listen_port, 0);
        assert_eq!(Config::parse_str("[network]\nlisten_port = 0\n").unwrap().value.network.listen_port, 0);
        // A port stored by an older version (auto pick or manual, indistinguishable) is a manual port.
        let old = Config::parse_str("version = 1\n[network]\nlisten_port = 55252\n").unwrap().value;
        assert_eq!(old.network.listen_port, 55252);
        assert!(old.validate().is_empty());
        // Range as before: 1024..=65535, or 0.
        assert!(Config::parse_str("[network]\nlisten_port = 1023\n").is_err());
    }

    #[test]
    fn partial_file_fills_defaults() {
        let p = Config::parse_str("[network]\nlisten_port = 51413\n").unwrap();
        assert_eq!(p.value.network.listen_port, 51413);
        assert!(p.value.network.dht);
        assert_eq!(p.value.transfers.max_active_downloads, 3);
    }

    #[test]
    fn unknown_keys_warn_and_survive_a_save() {
        let text = "mystery = 1\n[network]\nlisten_port = 51413\nfuture_flag = true\n[future]\nx = \"y\"\n";
        let p = Config::parse_str(text).unwrap();
        let mut paths: Vec<_> = p.warnings.iter().map(|w| w.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, ["future", "mystery", "network.future_flag"]);
        let out = p.value.render().unwrap();
        let again = Config::parse_str(&out).unwrap();
        assert_eq!(again.warnings.len(), 3);
        assert!(out.contains("future_flag = true") && out.contains("mystery = 1"));
    }

    #[test]
    fn allow_lan_trackers_defaults_off_and_round_trips() {
        assert!(!Config::default().network.allow_lan_trackers);
        let p = Config::parse_str("[network]\nlisten_port = 51413\n").unwrap();
        assert!(!p.value.network.allow_lan_trackers, "absent means off");
        let mut c = Config::default();
        c.network.allow_lan_trackers = true;
        let out = c.render().unwrap();
        assert!(out.contains("allow_lan_trackers = true"), "{out}");
        let again = Config::parse_str(&out).unwrap();
        assert!(again.value.network.allow_lan_trackers);
        assert!(again.warnings.is_empty(), "{:?}", again.warnings);
    }

    #[test]
    fn allow_lan_trackers_from_a_file_written_before_it_was_a_field() {
        // Until it became a real field the key lived among the unknown keys; the same
        // line now simply reads as the setting, without a warning.
        let p = Config::parse_str("[network]\nallow_lan_trackers = true\n").unwrap();
        assert!(p.value.network.allow_lan_trackers);
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
    }

    #[test]
    fn allow_lan_trackers_must_be_a_boolean() {
        for bad in ["\"yes\"", "1", "[true]"] {
            let err = Config::parse_str(&format!("[network]\nallow_lan_trackers = {bad}\n")).unwrap_err();
            match err {
                DocError::Parse { line, file, .. } => {
                    assert_eq!(file, "config.toml");
                    assert_eq!(line, Some(2), "{bad}");
                }
                other => panic!("{bad}: {other:?}"),
            }
        }
    }

    #[test]
    fn type_errors_carry_line_and_column() {
        let err = Config::parse_str("[network]\nlisten_port = \"abc\"\n").unwrap_err();
        match err {
            DocError::Parse { line, file, .. } => {
                assert_eq!(file, "config.toml");
                assert_eq!(line, Some(2));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn validation_reports_every_problem_with_paths() {
        let text = "[network]\nlisten_port = 80\nmax_connections = 0\n[transfers]\nmax_active_downloads = 0\n\
                    [folders]\ndownload_dir = \"relative/dir\"\n[folders.categories.movie]\nfinished_dir = \"relative/movies\"\n";
        let Err(DocError::Invalid { errors, .. }) = Config::parse_str(text) else { panic!() };
        let paths: Vec<_> = errors.iter().map(|e| e.path.as_str()).collect();
        for want in [
            "network.listen_port",
            "network.max_connections",
            "transfers.max_active_downloads",
            "folders.download_dir",
            "folders.categories.movie.finished_dir",
        ] {
            assert!(paths.contains(&want), "{want} missing in {paths:?}");
        }
    }

    #[test]
    fn a_folder_is_an_absolute_path_of_this_system_or_starts_with_a_tilde() {
        let check = |v: &str| {
            let mut e = Vec::new();
            check_path(&mut e, "folders.download_dir", v);
            e.is_empty()
        };
        assert!(check("~") && check("~/Downloads"));
        assert!(!check("relative/dir") && !check("~user/x"));
        let abs = std::env::temp_dir().join("Downloads");
        assert!(check(abs.to_str().unwrap()), "{}", abs.display());
        // A path from the root of no drive is not absolute on Windows.
        assert_eq!(check("/data/seed"), cfg!(unix));
    }

    #[test]
    fn organize_by_status_is_off_by_default_and_round_trips() {
        assert!(!Config::default().folders.by_status);
        let mut c = Config::default();
        c.folders.by_status = true;
        let out = c.render().unwrap();
        assert!(out.contains("by_status = true"), "{out}");
        assert!(Config::parse_str(&out).unwrap().value.folders.by_status);
        assert!(!Config::parse_str("[folders]\ndownload_dir = \"~/x\"\n").unwrap().value.folders.by_status);
    }

    #[test]
    fn templates_placement_and_template_locations_of_older_files_are_dropped() {
        let old = &win("version = 2\n[folders]\ndownload_dir = \"~/Downloads\"\nplacement = \"hardlink\"\n\
            [folders.templates]\nmovie = \"~/Media/Movies/{title} ({year})/{title}.{ext}\"\n\
            [folders.categories.movie]\nfinished_dir = \"/Volumes/M/Movies/{title} ({year})/\"\n\
            [folders.categories.tv]\nfinished_dir = \"{show}/Season {season}\"\nincomplete_dir = \"/Volumes/M/inc\"\n\
            [folders.categories.music]\nfinished_dir = \"/Volumes/M/Music\"\n");
        assert!(Config::file_is_outdated(old));
        let p = Config::parse_str(old).unwrap();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(p.value.version, CONFIG_VERSION);
        let cats = &p.value.folders.categories;
        assert_eq!(cats.movie.finished_dir.as_deref(), Some(abs("/Volumes/M/Movies").as_str()), "the fixed prefix");
        assert_eq!(cats.tv.finished_dir, None, "nothing fixed in front: the app default");
        assert_eq!(cats.tv.incomplete_dir.as_deref(), Some(abs("/Volumes/M/inc").as_str()));
        assert_eq!(cats.music.finished_dir.as_deref(), Some(abs("/Volumes/M/Music").as_str()), "a plain folder is kept");
        assert_eq!(p.value.folders.download_dir, "~/Downloads");
        let out = p.value.render().unwrap();
        assert!(!out.contains("template") && !out.contains("placement") && !out.contains('{'), "{out}");
        assert!(!Config::file_is_outdated(&out));
        assert_eq!(Config::parse_str(&out).unwrap().value, p.value);
    }

    #[test]
    fn fixed_prefix_cuts_at_the_first_variable() {
        assert_eq!(fixed_prefix("~/Media/TV/{show}/Season {season:02}/x.{ext}"), "~/Media/TV");
        assert_eq!(fixed_prefix("/Volumes/M/Movies/"), "/Volumes/M/Movies");
        assert_eq!(fixed_prefix("{title}"), "");
        assert_eq!(fixed_prefix("/{title}"), "");
        assert_eq!(fixed_prefix("/a/b{x}"), "/a");
    }

    #[test]
    fn retired_seeding_folder_migrates_to_the_finished_location() {
        // Organize off: the fixed seeding folder was where the files lived, i.e. the finished location.
        let old = &win("version = 2\n[folders]\ndownload_dir = \"~/Downloads\"\nplacement = \"off\"\n[folders.seeding]\nmode = \"fixed\"\ndir = \"/data/seed\"\n");
        assert!(Config::file_is_outdated(old), "the store rewrites the file once");
        let p = Config::parse_str(old).unwrap();
        assert_eq!(p.value.folders.download_dir, abs("/data/seed"));
        assert!(p.value.unknown.get("folders").is_none_or(|f| f.get("seeding").is_none()), "not kept as an unknown key");
        let out = p.value.render().unwrap();
        assert!(!out.contains("seeding"), "{out}");
        assert!(!Config::file_is_outdated(&out));
        assert_eq!(Config::parse_str(&out).unwrap().value, p.value);

        // Organize on: the fixed folder was never used, so it is dropped and nothing moves.
        let organized = win("[folders]\ndownload_dir = \"~/Downloads\"\n[folders.seeding]\nmode = \"fixed\"\ndir = \"/data/seed\"\n");
        assert_eq!(Config::parse_str(&organized).unwrap().value.folders.download_dir, "~/Downloads");

        // "organized" mode, a fixed mode without a folder (formerly an error), a plain old file: all load.
        for t in [
            "[folders.seeding]\nmode = \"organized\"\n",
            "[folders]\nplacement = \"off\"\n[folders.seeding]\nmode = \"fixed\"\n",
            "version = 1\n",
        ] {
            let c = Config::parse_str(t).unwrap_or_else(|e| panic!("{t:?}: {e}")).value;
            assert_eq!(c.folders.download_dir, "~/Downloads", "{t:?}");
        }
        // A file without the key is parsed as it is.
        assert!(!Config::file_is_outdated(&Config::default().render().unwrap()));
    }

    #[test]
    fn newer_version_is_refused_not_misparsed() {
        let err = Config::parse_str("version = 99\n[weird]\nx=1\n").unwrap_err();
        assert!(matches!(err, DocError::Unsupported { found: 99, supported: CONFIG_VERSION, .. }));
    }

    #[test]
    fn old_default_move_limit_becomes_unlimited() {
        let old = |mib: u32| format!("version = 5\n[advanced]\nmove_limit_mib = {mib}\n");
        let p = Config::parse_str(&old(100)).unwrap();
        assert_eq!((p.value.advanced.move_limit_mib, p.value.version), (0, CONFIG_VERSION));
        assert_eq!(Config::parse_str(&old(50)).unwrap().value.advanced.move_limit_mib, 50);
        let cur = format!("version = {CONFIG_VERSION}\n[advanced]\nmove_limit_mib = 100\n");
        assert_eq!(Config::parse_str(&cur).unwrap().value.advanced.move_limit_mib, 100);
        assert_eq!(Config::default().advanced.move_limit_mib, 0);
        assert!(Config::file_is_outdated(&old(100)));
    }

    #[test]
    fn old_default_write_cache_follows_the_new_default() {
        let old = |mib: u32| format!("version = 4\n[advanced]\nwrite_cache_mib = {mib}\n");
        let p = Config::parse_str(&old(64)).unwrap();
        assert_eq!((p.value.advanced.write_cache_mib, p.value.version), (32, CONFIG_VERSION));
        // A value chosen on purpose stays, and a current file's 64 is a choice too.
        assert_eq!(Config::parse_str(&old(128)).unwrap().value.advanced.write_cache_mib, 128);
        let cur = format!("version = {CONFIG_VERSION}\n[advanced]\nwrite_cache_mib = 64\n");
        assert_eq!(Config::parse_str(&cur).unwrap().value.advanced.write_cache_mib, 64);
        assert!(Config::file_is_outdated(&old(64)));
    }

    #[test]
    fn schedule_windows() {
        let mut s = Schedule { enabled: true, ..Schedule::default() };
        assert!(s.active_at(0, 9 * 60) && !s.active_at(0, 17 * 60) && !s.active_at(5, 9 * 60));
        s.from = TimeOfDay(22 * 60);
        s.to = TimeOfDay(6 * 60);
        assert!(s.active_at(4, 23 * 60)); // Friday night
        assert!(s.active_at(5, 5 * 60)); // Saturday early morning belongs to Friday's window
        assert!(!s.active_at(5, 23 * 60)); // Saturday night: weekdays only
        assert!(!s.active_at(0, 5 * 60)); // Monday early morning follows Sunday
        s.enabled = false;
        assert!(!s.active_at(0, 23 * 60));
    }

    #[test]
    fn day_sets_parse_both_forms() {
        let p = Config::parse_str("[transfers.alt.schedule]\ndays = [\"sat\", \"MON\"]\n").unwrap();
        assert_eq!(p.value.transfers.alt.schedule.days, DaySet(0b010_0001));
        let text = p.value.render().unwrap();
        assert!(text.contains("days = [\"mon\", \"sat\"]"), "{text}");
        assert!(Config::parse_str("[transfers.alt.schedule]\ndays = []\n").is_err());
        assert!(Config::parse_str("[transfers.alt.schedule]\ndays = [\"funday\"]\n").is_err());
    }

    #[test]
    fn time_of_day_strictness() {
        assert_eq!(TimeOfDay::parse("08:05"), Ok(TimeOfDay(485)));
        for bad in ["8:05", "24:00", "12:60", "12-00", "ab:cd", "", "12:5"] {
            assert!(TimeOfDay::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn permission_ordering() {
        assert!(Permission::Full.allows(Permission::Control));
        assert!(!Permission::ReadOnly.allows(Permission::Control));
        assert!(!Permission::Off.allows(Permission::ReadOnly));
    }

    #[test]
    fn effective_limits_zero_is_unlimited() {
        let mut c = Config::default();
        c.transfers.upload_limit = 5_000_000;
        assert_eq!(c.effective_limits(false), (None, Some(5_000_000)));
        assert_eq!(c.effective_limits(true), (Some(2_000_000), Some(500_000)));
    }
}
