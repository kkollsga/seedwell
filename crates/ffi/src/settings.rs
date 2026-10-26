//! Settings, rules and tracker profiles over FFI: typed records per Settings tab, backed by the
//! TOML documents of `trs-state` (SPEC §8.4, §7, §7E).
//!
//! Every save is "load the document, overwrite the fields this record knows, validate, write".
//! Unknown keys survive, and a field this module does not know about is never reset. The engine
//! (a `Host` with hot reload) applies the written file within its 2 s poll; nothing here talks to it.

use std::path::{Path, PathBuf};

use trs_state::config::{
    AfterAdd, AskBeforeAdding, Config, DaySet, Encryption, OnBattery, Permission, Preallocation, TimeOfDay, WatchFolder, WatchMode,
};
use trs_state::duration::Secs;
use trs_state::rules::{self, DefaultRule, Match, OnGoal, Rule, RuleSet};
use trs_state::trackers::{Profile, TrackerProfiles, UrlPattern};
use trs_state::{DocError, DocStore, FieldError};

use crate::types::{Category, CoreError, Priority};

// ---------------------------------------------------------------- enums

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TorrentFileAction {
    Trash,
    Archive,
    Leave,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum WatchModeSetting {
    Auto,
    Ask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum OnBatteryMode {
    KeepGoing,
    Limit,
    Pause,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum AskMode {
    Never,
    MultiFile,
    Always,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum AgentPermission {
    Off,
    ReadOnly,
    Control,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PreallocationMode {
    Sparse,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ScheduleDays {
    Weekdays,
    Weekends,
    EveryDay,
    /// Bit 0 = Monday.
    Custom { mask: u8 },
}

// ---------------------------------------------------------------- records

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct WatchFolderSetting {
    pub path: String,
    pub enabled: bool,
    pub mode: WatchModeSetting,
    pub after_add: TorrentFileAction,
    pub archive_dir: Option<String>,
    /// Always treat torrents from this folder as this category.
    pub category: Option<Category>,
    pub rule: Option<String>,
    /// Baseline stamped by the engine: older `.torrent` files are the "backlog". Pass it through.
    pub since: Option<u64>,
}

/// Per-category overrides of the two locations (SPEC §7A.4); `None` = the app default.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct CategoryLocationSetting {
    pub category: Category,
    pub incomplete_dir: Option<String>,
    pub finished_dir: Option<String>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct GeneralSettings {
    pub launch_at_login: bool,
    pub notify_finished: bool,
    pub notify_errors: bool,
    pub notify_goal: bool,
    pub menu_bar_show_speed: bool,
    pub menu_bar_split_by_tracker: bool,
    pub prevent_sleep_while_downloading: bool,
    /// Keep the Mac awake while an agent says it is working (SPEC §9A.7).
    pub keep_awake_for_agents: bool,
    pub on_battery: OnBatteryMode,
    /// "Play videos with": `"auto"` or a player's bundle identifier (SPEC §7F.3).
    pub play_with: String,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct UpdatePreferences {
    /// Look for a newer release once a day (release builds only).
    pub check: bool,
    /// Install it by itself when nothing is downloading (release builds only).
    pub auto_install: bool,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct TransferSettings {
    /// Bytes per second; 0 = unlimited.
    pub download_limit: u64,
    pub upload_limit: u64,
    pub alt_download_limit: u64,
    pub alt_upload_limit: u64,
    pub schedule_enabled: bool,
    pub schedule_days: ScheduleDays,
    /// Minutes after local midnight.
    pub schedule_from_minute: u32,
    pub schedule_to_minute: u32,
    pub max_active_downloads: u32,
    pub max_connections_per_host: u32,
    pub ask_before_adding: AskMode,
    /// Torrents larger than this many GB ask before downloading (SPEC §7A.6); 0 = never.
    pub large_torrent_gb: u32,
    /// Seconds a torrent that would start without asking is held, silent, before it starts
    /// (SPEC §7A.7); 0 = start at once. At most 600.
    pub start_delay_secs: u32,
    /// Hit-and-run floor for private torrents, seconds: their goal cannot count as reached before
    /// they have seeded this long unless their rule has its own floor (SPEC §7). 0 = off.
    pub private_min_seed_secs: u64,
    /// Stop seeding a finished torrent that uploaded less than this many bytes in the last
    /// `idle_window_hours` hours of seeding (SPEC §7.5). 0 = off.
    #[uniffi(default = 0)]
    pub idle_min_upload_bytes: u64,
    /// The window of `idle_min_upload_bytes`, hours of seeding.
    #[uniffi(default = 150)]
    pub idle_window_hours: u32,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct FolderSettings {
    /// The app default **finished location**.
    pub download_dir: String,
    /// The app default **incomplete location**.
    pub incomplete_dir: String,
    /// Per-category overrides, always seven entries in category order.
    pub categories: Vec<CategoryLocationSetting>,
    /// Organize finished torrents into `Active/`, `Dormant/`, `Stopped/` and `Dead/` folders
    /// below their finished location (SPEC §7A.3).
    pub by_status: bool,
    pub torrent_after_add: TorrentFileAction,
    pub archive_dir: Option<String>,
    pub watch: Vec<WatchFolderSetting>,
    pub skip_junk: bool,
    pub ask_when_unsure: bool,
    pub confidence_threshold: f64,
    pub online_lookup: bool,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct NetworkSettings {
    pub listen_port: u16,
    /// Trackers on a private network (LAN, VPN) are refused unless this is on. Needs a restart.
    pub allow_lan_trackers: bool,
    /// UPnP and NAT-PMP / PCP together: one switch in the UI.
    pub port_mapping: bool,
    pub dht: bool,
    pub pex: bool,
    pub max_connections: u32,
    pub max_connections_per_torrent: u32,
    /// Peer-wire encryption policy. Takes effect for new connections at once.
    pub encryption: EncryptionSetting,
    /// uTP on the UDP port. Turning it off is live; turning it on applies after a restart.
    pub utp: bool,
}

/// Peer-wire encryption policy (`network.encryption`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum EncryptionSetting {
    Off,
    Prefer,
    Require,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct AgentSettings {
    pub permission: AgentPermission,
    pub notify_changes: bool,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct AdvancedSettings {
    pub write_cache_mib: u32,
    pub preallocation: PreallocationMode,
    pub verify_on_completion: bool,
    pub background_scrub: bool,
    /// Log every tracker exchange, secrets masked (SPEC 8.7).
    pub developer_mode: bool,
}

/// Every setting the UI edits, one record so a tab can save atomically.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct AppSettings {
    pub general: GeneralSettings,
    pub transfers: TransferSettings,
    pub folders: FolderSettings,
    pub network: NetworkSettings,
    pub agents: AgentSettings,
    pub updates: UpdatePreferences,
    pub advanced: AdvancedSettings,
}

// ---------------------------------------------------------------- conversions: enums

fn category_to(c: trs_classify::Category) -> Category {
    use trs_classify::Category as C;
    match c {
        C::Movie => Category::Movie,
        C::Tv => Category::Tv,
        C::Music => Category::Music,
        C::Audiobook => Category::Audiobook,
        C::Book => Category::Book,
        C::Software => Category::Software,
        C::Other => Category::Other,
        C::Game => Category::Game,
    }
}

pub(crate) fn category_from(c: Category) -> trs_classify::Category {
    use trs_classify::Category as C;
    match c {
        Category::Movie => C::Movie,
        Category::Tv => C::Tv,
        Category::Music => C::Music,
        Category::Audiobook => C::Audiobook,
        Category::Book => C::Book,
        Category::Software => C::Software,
        Category::Other => C::Other,
        Category::Game => C::Game,
    }
}

pub(crate) fn category_to_ffi(c: trs_classify::Category) -> Category {
    category_to(c)
}

fn prio_to(p: rules::Priority) -> Priority {
    match p {
        rules::Priority::High => Priority::High,
        rules::Priority::Normal => Priority::Normal,
        rules::Priority::Low => Priority::Low,
    }
}

fn prio_from(p: Priority) -> rules::Priority {
    match p {
        Priority::High => rules::Priority::High,
        Priority::Normal => rules::Priority::Normal,
        Priority::Low => rules::Priority::Low,
    }
}

fn after_to(a: AfterAdd) -> TorrentFileAction {
    match a {
        AfterAdd::Trash => TorrentFileAction::Trash,
        AfterAdd::Archive => TorrentFileAction::Archive,
        AfterAdd::Leave => TorrentFileAction::Leave,
    }
}

fn after_from(a: TorrentFileAction) -> AfterAdd {
    match a {
        TorrentFileAction::Trash => AfterAdd::Trash,
        TorrentFileAction::Archive => AfterAdd::Archive,
        TorrentFileAction::Leave => AfterAdd::Leave,
    }
}

fn days_to(d: DaySet) -> ScheduleDays {
    match d {
        DaySet::WEEKDAYS => ScheduleDays::Weekdays,
        DaySet::WEEKENDS => ScheduleDays::Weekends,
        DaySet::EVERY_DAY => ScheduleDays::EveryDay,
        DaySet(mask) => ScheduleDays::Custom { mask },
    }
}

fn days_from(d: ScheduleDays) -> DaySet {
    match d {
        ScheduleDays::Weekdays => DaySet::WEEKDAYS,
        ScheduleDays::Weekends => DaySet::WEEKENDS,
        ScheduleDays::EveryDay => DaySet::EVERY_DAY,
        ScheduleDays::Custom { mask } => DaySet(mask & 0b111_1111),
    }
}

// ---------------------------------------------------------------- Config <-> AppSettings

const CATEGORIES: [trs_classify::Category; 8] = [
    trs_classify::Category::Movie,
    trs_classify::Category::Tv,
    trs_classify::Category::Music,
    trs_classify::Category::Audiobook,
    trs_classify::Category::Book,
    trs_classify::Category::Software,
    trs_classify::Category::Other,
    trs_classify::Category::Game,
];

pub(crate) fn from_config(c: &Config) -> AppSettings {
    AppSettings {
        general: GeneralSettings {
            launch_at_login: c.general.launch_at_login,
            notify_finished: c.general.notify_finished,
            notify_errors: c.general.notify_errors,
            notify_goal: c.general.notify_goal,
            menu_bar_show_speed: c.general.menu_bar_show_speed,
            menu_bar_split_by_tracker: c.general.menu_bar_split_by_tracker,
            prevent_sleep_while_downloading: c.general.prevent_sleep_while_downloading,
            keep_awake_for_agents: c.general.keep_awake_for_agents,
            on_battery: match c.general.on_battery {
                OnBattery::KeepGoing => OnBatteryMode::KeepGoing,
                OnBattery::Limit => OnBatteryMode::Limit,
                OnBattery::Pause => OnBatteryMode::Pause,
            },
            play_with: c.general.play_with.clone(),
        },
        transfers: TransferSettings {
            download_limit: c.transfers.download_limit,
            upload_limit: c.transfers.upload_limit,
            alt_download_limit: c.transfers.alt.download_limit,
            alt_upload_limit: c.transfers.alt.upload_limit,
            schedule_enabled: c.transfers.alt.schedule.enabled,
            schedule_days: days_to(c.transfers.alt.schedule.days),
            schedule_from_minute: u32::from(c.transfers.alt.schedule.from.0),
            schedule_to_minute: u32::from(c.transfers.alt.schedule.to.0),
            max_active_downloads: c.transfers.max_active_downloads,
            max_connections_per_host: c.transfers.max_connections_per_host,
            large_torrent_gb: c.transfers.large_torrent_gb,
            start_delay_secs: c.transfers.start_delay_secs,
            private_min_seed_secs: c.transfers.private_min_seed_secs,
            idle_min_upload_bytes: c.transfers.idle_min_upload_bytes,
            idle_window_hours: c.transfers.idle_window_hours,
            ask_before_adding: match c.transfers.ask_before_adding {
                AskBeforeAdding::Never => AskMode::Never,
                AskBeforeAdding::MultiFile => AskMode::MultiFile,
                AskBeforeAdding::Always => AskMode::Always,
            },
        },
        folders: FolderSettings {
            download_dir: c.folders.download_dir.clone(),
            incomplete_dir: c.folders.incomplete_dir.clone(),
            categories: CATEGORIES
                .iter()
                .map(|&cat| {
                    let o = c.folders.categories.get(cat);
                    CategoryLocationSetting {
                        category: category_to(cat),
                        incomplete_dir: o.incomplete_dir.clone(),
                        finished_dir: o.finished_dir.clone(),
                    }
                })
                .collect(),
            by_status: c.folders.by_status,
            torrent_after_add: after_to(c.folders.torrent_after_add),
            archive_dir: c.folders.archive_dir.clone(),
            watch: c
                .folders
                .watch
                .iter()
                .map(|w| WatchFolderSetting {
                    path: w.path.clone(),
                    enabled: w.enabled,
                    mode: match w.mode {
                        WatchMode::Auto => WatchModeSetting::Auto,
                        WatchMode::Ask => WatchModeSetting::Ask,
                    },
                    after_add: after_to(w.after_add),
                    archive_dir: w.archive_dir.clone(),
                    category: w.category.map(category_to),
                    rule: w.rule.clone(),
                    since: w.since,
                })
                .collect(),
            skip_junk: c.detection.skip_junk,
            ask_when_unsure: c.detection.ask_when_unsure,
            confidence_threshold: c.detection.confidence_threshold,
            online_lookup: c.detection.online_lookup,
        },
        network: NetworkSettings {
            listen_port: c.network.listen_port,
            allow_lan_trackers: c.network.allow_lan_trackers,
            port_mapping: c.network.upnp || c.network.natpmp,
            dht: c.network.dht,
            pex: c.network.pex,
            max_connections: c.network.max_connections,
            max_connections_per_torrent: c.network.max_connections_per_torrent,
            encryption: match c.network.encryption {
                Encryption::Off => EncryptionSetting::Off,
                Encryption::Prefer => EncryptionSetting::Prefer,
                Encryption::Require => EncryptionSetting::Require,
            },
            utp: c.network.utp,
        },
        agents: AgentSettings {
            permission: match c.agents.permission {
                Permission::Off => AgentPermission::Off,
                Permission::ReadOnly => AgentPermission::ReadOnly,
                Permission::Control => AgentPermission::Control,
                Permission::Full => AgentPermission::Full,
            },
            notify_changes: c.agents.notify_changes,
        },
        updates: UpdatePreferences { check: c.updates.check, auto_install: c.updates.auto_install },
        advanced: AdvancedSettings {
            write_cache_mib: c.advanced.write_cache_mib,
            preallocation: match c.advanced.preallocation {
                Preallocation::Sparse => PreallocationMode::Sparse,
                Preallocation::Full => PreallocationMode::Full,
            },
            verify_on_completion: c.advanced.verify_on_completion,
            background_scrub: c.advanced.background_scrub,
            developer_mode: c.advanced.developer_mode,
        },
    }
}

/// Writes the fields of `s` into `c`; everything else in `c` stays as it is.
pub(crate) fn apply_to_config(s: &AppSettings, c: &mut Config) {
    let g = &s.general;
    c.general.launch_at_login = g.launch_at_login;
    c.general.notify_finished = g.notify_finished;
    c.general.notify_errors = g.notify_errors;
    c.general.notify_goal = g.notify_goal;
    c.general.menu_bar_show_speed = g.menu_bar_show_speed;
    c.general.menu_bar_split_by_tracker = g.menu_bar_split_by_tracker;
    c.general.prevent_sleep_while_downloading = g.prevent_sleep_while_downloading;
    c.general.keep_awake_for_agents = g.keep_awake_for_agents;
    c.general.play_with = g.play_with.clone();
    c.general.on_battery = match g.on_battery {
        OnBatteryMode::KeepGoing => OnBattery::KeepGoing,
        OnBatteryMode::Limit => OnBattery::Limit,
        OnBatteryMode::Pause => OnBattery::Pause,
    };

    let t = &s.transfers;
    c.transfers.download_limit = t.download_limit;
    c.transfers.upload_limit = t.upload_limit;
    c.transfers.alt.download_limit = t.alt_download_limit;
    c.transfers.alt.upload_limit = t.alt_upload_limit;
    c.transfers.alt.schedule.enabled = t.schedule_enabled;
    c.transfers.alt.schedule.days = days_from(t.schedule_days);
    c.transfers.alt.schedule.from = TimeOfDay(t.schedule_from_minute.min(1439) as u16);
    c.transfers.alt.schedule.to = TimeOfDay(t.schedule_to_minute.min(1439) as u16);
    c.transfers.max_active_downloads = t.max_active_downloads;
    c.transfers.max_connections_per_host = t.max_connections_per_host;
    c.transfers.large_torrent_gb = t.large_torrent_gb;
    c.transfers.start_delay_secs = t.start_delay_secs.min(trs_state::config::MAX_START_DELAY_SECS);
    c.transfers.private_min_seed_secs = t.private_min_seed_secs.min(trs_state::duration::MAX_SECS);
    c.transfers.idle_min_upload_bytes = t.idle_min_upload_bytes.min(rules::MAX_IDLE_BYTES);
    c.transfers.idle_window_hours = t.idle_window_hours.clamp(1, rules::MAX_IDLE_HOURS);
    c.transfers.ask_before_adding = match t.ask_before_adding {
        AskMode::Never => AskBeforeAdding::Never,
        AskMode::MultiFile => AskBeforeAdding::MultiFile,
        AskMode::Always => AskBeforeAdding::Always,
    };

    let f = &s.folders;
    c.folders.download_dir = f.download_dir.clone();
    c.folders.incomplete_dir = f.incomplete_dir.clone();
    for o in &f.categories {
        let slot = c.folders.categories.get_mut(category_from(o.category));
        slot.incomplete_dir = o.incomplete_dir.clone().filter(|d| !d.trim().is_empty());
        slot.finished_dir = o.finished_dir.clone().filter(|d| !d.trim().is_empty());
    }
    c.folders.by_status = f.by_status;
    c.folders.torrent_after_add = after_from(f.torrent_after_add);
    c.folders.archive_dir = f.archive_dir.clone();
    c.folders.watch = f
        .watch
        .iter()
        .map(|w| WatchFolder {
            path: w.path.clone(),
            enabled: w.enabled,
            mode: match w.mode {
                WatchModeSetting::Auto => WatchMode::Auto,
                WatchModeSetting::Ask => WatchMode::Ask,
            },
            after_add: after_from(w.after_add),
            archive_dir: w.archive_dir.clone(),
            category: w.category.map(category_from),
            rule: w.rule.clone(),
            since: w.since,
        })
        .collect();
    c.detection.skip_junk = f.skip_junk;
    c.detection.ask_when_unsure = f.ask_when_unsure;
    c.detection.confidence_threshold = f.confidence_threshold;
    c.detection.online_lookup = f.online_lookup;

    let n = &s.network;
    c.network.listen_port = n.listen_port;
    c.network.allow_lan_trackers = n.allow_lan_trackers;
    c.network.upnp = n.port_mapping;
    c.network.natpmp = n.port_mapping;
    c.network.dht = n.dht;
    c.network.pex = n.pex;
    c.network.max_connections = n.max_connections;
    c.network.max_connections_per_torrent = n.max_connections_per_torrent;
    c.network.encryption = match n.encryption {
        EncryptionSetting::Off => Encryption::Off,
        EncryptionSetting::Prefer => Encryption::Prefer,
        EncryptionSetting::Require => Encryption::Require,
    };
    c.network.utp = n.utp;
    // `lsd` (BEP 14) and `ipv6` have no engine implementation yet: the keys stay in config.toml
    // untouched and the app shows no control for them (SPEC 5.2).

    c.agents.permission = match s.agents.permission {
        AgentPermission::Off => Permission::Off,
        AgentPermission::ReadOnly => Permission::ReadOnly,
        AgentPermission::Control => Permission::Control,
        AgentPermission::Full => Permission::Full,
    };
    c.agents.notify_changes = s.agents.notify_changes;
    c.updates.check = s.updates.check;
    c.updates.auto_install = s.updates.auto_install;

    c.advanced.write_cache_mib = s.advanced.write_cache_mib;
    c.advanced.preallocation = match s.advanced.preallocation {
        PreallocationMode::Sparse => Preallocation::Sparse,
        PreallocationMode::Full => Preallocation::Full,
    };
    c.advanced.verify_on_completion = s.advanced.verify_on_completion;
    c.advanced.background_scrub = s.advanced.background_scrub;
    c.advanced.developer_mode = s.advanced.developer_mode;
}

// ---------------------------------------------------------------- rules

#[derive(Clone, Copy, Debug, PartialEq, uniffi::Enum)]
pub enum GoalKind {
    StopWhenFinished,
    Ratio { ratio: f64 },
    Time { secs: u64 },
    RatioOrTime { ratio: f64, secs: u64 },
    RatioAndTime { ratio: f64, secs: u64 },
    Forever,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum OnGoalAction {
    StopKeep,
    RemoveKeepFiles,
    RemoveDeleteFiles,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum PrivateMatch {
    Any,
    PrivateOnly,
    PublicOnly,
}

/// One rule of the ordered list (SPEC §7). An unset action falls back to the Default rule.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RuleRecord {
    /// Empty for a new rule: the id is made from the name on save.
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// Tracker profile ids; `public` = torrents without a profile. Empty = any tracker.
    pub tracker_ids: Vec<String>,
    pub private_match: PrivateMatch,
    /// Empty = any content type.
    pub categories: Vec<Category>,
    pub goal: Option<GoalKind>,
    pub min_seed_secs: Option<u64>,
    pub priority: Option<Priority>,
    pub on_goal: Option<OnGoalAction>,
    /// Finished-location override, a plain folder (SPEC §7A.4: rule > category > app default).
    pub finished_location: Option<String>,
    /// Bytes per second; `None` = the Default rule's; 0 = unlimited.
    pub up_limit: Option<u64>,
    /// The inactive stop condition (SPEC §7.5): `None` = the global setting, `Some(0)` = off for
    /// this rule, otherwise stop below this many bytes uploaded in `idle_hours`.
    #[uniffi(default = None)]
    pub idle_below_bytes: Option<u64>,
    /// Window of `idle_below_bytes`, hours of seeding.
    #[uniffi(default = 150)]
    pub idle_hours: u32,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct DefaultRuleRecord {
    pub goal: GoalKind,
    pub min_seed_secs: Option<u64>,
    pub priority: Priority,
    pub on_goal: OnGoalAction,
    pub finished_location: Option<String>,
    pub up_limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulesDoc {
    /// Checked top to bottom; the first match wins.
    pub rules: Vec<RuleRecord>,
    /// Always last, cannot be deleted or moved.
    pub default_rule: DefaultRuleRecord,
}

/// "Applies to 44 torrents. Saving changes 3."
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulePreview {
    pub affected: u32,
    /// Torrents whose goal, priority or limit would change if this were saved.
    pub changed: u32,
    /// Plain-language description of the most common change, empty when none.
    pub change_note: String,
}

fn goal_to(g: rules::SeedingGoal) -> GoalKind {
    use rules::SeedingGoal as G;
    match g {
        G::StopWhenFinished => GoalKind::StopWhenFinished,
        G::Ratio { ratio } => GoalKind::Ratio { ratio },
        G::Time { time } => GoalKind::Time { secs: time.0 },
        G::RatioOrTime { ratio, time } => GoalKind::RatioOrTime { ratio, secs: time.0 },
        G::RatioAndTime { ratio, time } => GoalKind::RatioAndTime { ratio, secs: time.0 },
        G::Forever => GoalKind::Forever,
    }
}

fn goal_from(g: GoalKind) -> rules::SeedingGoal {
    use rules::SeedingGoal as G;
    match g {
        GoalKind::StopWhenFinished => G::StopWhenFinished,
        GoalKind::Ratio { ratio } => G::Ratio { ratio },
        GoalKind::Time { secs } => G::Time { time: Secs(secs) },
        GoalKind::RatioOrTime { ratio, secs } => G::RatioOrTime { ratio, time: Secs(secs) },
        GoalKind::RatioAndTime { ratio, secs } => G::RatioAndTime { ratio, time: Secs(secs) },
        GoalKind::Forever => G::Forever,
    }
}

fn on_goal_to(a: OnGoal) -> OnGoalAction {
    match a {
        OnGoal::StopKeep => OnGoalAction::StopKeep,
        OnGoal::RemoveKeepFiles => OnGoalAction::RemoveKeepFiles,
        OnGoal::RemoveDeleteFiles => OnGoalAction::RemoveDeleteFiles,
    }
}

fn on_goal_from(a: OnGoalAction) -> OnGoal {
    match a {
        OnGoalAction::StopKeep => OnGoal::StopKeep,
        OnGoalAction::RemoveKeepFiles => OnGoal::RemoveKeepFiles,
        OnGoalAction::RemoveDeleteFiles => OnGoal::RemoveDeleteFiles,
    }
}

fn rule_to(r: &Rule) -> RuleRecord {
    RuleRecord {
        id: r.id.clone(),
        name: r.name.clone(),
        enabled: r.enabled,
        tracker_ids: r.when.tracker.clone(),
        private_match: match r.when.private {
            None => PrivateMatch::Any,
            Some(true) => PrivateMatch::PrivateOnly,
            Some(false) => PrivateMatch::PublicOnly,
        },
        categories: r.when.category.iter().copied().map(category_to).collect(),
        goal: r.goal.map(goal_to),
        min_seed_secs: r.min_seed_time.map(|s| s.0),
        priority: r.priority.map(prio_to),
        on_goal: r.on_goal.map(on_goal_to),
        finished_location: r.finished_dir.clone(),
        up_limit: r.up_limit,
        idle_below_bytes: r.stop_when_idle.map(|i| i.stop().map_or(0, |s| s.below)),
        idle_hours: r.stop_when_idle.and_then(|i| i.stop()).map_or(rules::DEFAULT_IDLE_HOURS, |s| s.hours),
    }
}

pub(crate) fn rule_from(r: &RuleRecord) -> Rule {
    Rule {
        id: r.id.clone(),
        name: r.name.clone(),
        enabled: r.enabled,
        when: Match {
            tracker: r.tracker_ids.clone(),
            private: match r.private_match {
                PrivateMatch::Any => None,
                PrivateMatch::PrivateOnly => Some(true),
                PrivateMatch::PublicOnly => Some(false),
            },
            category: r.categories.iter().copied().map(category_from).collect(),
        },
        goal: r.goal.map(goal_from),
        min_seed_time: r.min_seed_secs.filter(|s| *s > 0).map(Secs),
        priority: r.priority.map(prio_from),
        on_goal: r.on_goal.map(on_goal_from),
        finished_dir: r.finished_location.clone().filter(|t| !t.trim().is_empty()),
        up_limit: r.up_limit,
        stop_when_idle: r.idle_below_bytes.map(|b| {
            if b == 0 {
                rules::IdleSetting::Off
            } else {
                rules::IdleSetting::On(rules::IdleStop { below: b, hours: r.idle_hours })
            }
        }),
    }
}

fn default_to(d: &DefaultRule) -> DefaultRuleRecord {
    DefaultRuleRecord {
        goal: goal_to(d.goal),
        min_seed_secs: d.min_seed_time.map(|s| s.0),
        priority: prio_to(d.priority),
        on_goal: on_goal_to(d.on_goal),
        finished_location: d.finished_dir.clone(),
        up_limit: d.up_limit,
    }
}

fn apply_default(d: &DefaultRuleRecord, out: &mut DefaultRule) {
    out.goal = goal_from(d.goal);
    out.min_seed_time = d.min_seed_secs.filter(|s| *s > 0).map(Secs);
    out.priority = prio_from(d.priority);
    out.on_goal = on_goal_from(d.on_goal);
    out.finished_dir = d.finished_location.clone().filter(|t| !t.trim().is_empty());
    out.up_limit = d.up_limit;
}

pub(crate) fn rules_to_doc(set: &RuleSet) -> RulesDoc {
    RulesDoc { rules: set.rules.iter().map(rule_to).collect(), default_rule: default_to(&set.default) }
}

// ---------------------------------------------------------------- tracker profiles

/// A tracker profile (SPEC §7E.1): a name and the announce URLs that belong to it.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct ProfileRecord {
    /// Empty for a new profile: the id is made from the name on save.
    pub id: String,
    pub name: String,
    /// Announce URLs or host patterns. Passkeys are dropped.
    pub urls: Vec<String>,
    /// Rule applied to this tracker's torrents.
    pub rule: Option<String>,
    pub color: Option<String>,
    pub icon: Option<String>,
    /// One shared download cap for all of this tracker's torrents, bytes/s; `None` = unlimited.
    pub down_limit: Option<u64>,
    /// One shared upload cap for all of this tracker's torrents, bytes/s; `None` = unlimited.
    pub up_limit: Option<u64>,
    /// This tracker's own incomplete folder (SPEC §7A.4); `None` = same as Settings.
    #[uniffi(default = None)]
    pub incomplete_dir: Option<String>,
    /// This tracker's own finished folder; `None` = same as Settings.
    #[uniffi(default = None)]
    pub finished_dir: Option<String>,
    /// Per-category folders of this tracker; a category not listed (or with both `None`) follows
    /// the tracker-wide folders.
    #[uniffi(default = [])]
    pub categories: Vec<CategoryLocationSetting>,
    /// Hidden mode (SPEC §7E.5): left out of the Downloads and Sharing tabs, the menu and the
    /// sidebar; opened from Settings.
    #[uniffi(default = false)]
    pub hidden: bool,
    /// With `hidden`: only the seeding torrents are hidden.
    #[uniffi(default = false)]
    pub hide_only_seeding: bool,
    /// Peer exchange (BEP 11) is allowed for this tracker's private torrents (SPEC §7B.4). Off by default.
    #[uniffi(default = false)]
    pub allow_pex: bool,
}

pub(crate) fn profile_to(p: &Profile) -> ProfileRecord {
    ProfileRecord {
        id: p.id.clone(),
        name: p.name.clone(),
        urls: p.urls.iter().map(|u| u.as_str().to_string()).collect(),
        rule: p.rule.clone(),
        color: p.color.clone(),
        icon: p.icon.clone(),
        down_limit: p.down_cap(),
        up_limit: p.up_cap(),
        incomplete_dir: p.location.incomplete_dir.clone(),
        finished_dir: p.location.finished_dir.clone(),
        categories: CATEGORIES
            .iter()
            .map(|&cat| {
                let o = p.categories.get(cat);
                CategoryLocationSetting {
                    category: category_to(cat),
                    incomplete_dir: o.incomplete_dir.clone(),
                    finished_dir: o.finished_dir.clone(),
                }
            })
            .filter(|o| o.incomplete_dir.is_some() || o.finished_dir.is_some())
            .collect(),
        hidden: p.hidden,
        hide_only_seeding: p.hide_only_seeding,
        allow_pex: p.allow_pex,
    }
}

// ---------------------------------------------------------------- the store

fn slug(name: &str, fallback: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    let s = s.trim_matches('-');
    let mut s = if s.is_empty() { fallback.to_string() } else { s.to_string() };
    s.truncate(48);
    s
}

fn unique_id(base: String, taken: &dyn Fn(&str) -> bool) -> String {
    if !taken(&base) && !["public", "other-private", "all"].contains(&base.as_str()) {
        return base;
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken(c))
        .unwrap_or(base)
}

fn problems(errs: &[FieldError]) -> CoreError {
    CoreError::Invalid { problems: errs.iter().map(ToString::to_string).collect() }
}

fn doc_err(e: &DocError) -> CoreError {
    match e {
        DocError::Invalid { errors, .. } => problems(errors),
        other => CoreError::Failed { message: other.to_string() },
    }
}

/// The three documents of one state directory.
pub struct SettingsStore {
    cfg: DocStore<Config>,
    rules: DocStore<RuleSet>,
    profiles: DocStore<TrackerProfiles>,
}

impl SettingsStore {
    pub fn new(dir: &Path) -> Self {
        Self {
            cfg: DocStore::<Config>::new(dir),
            rules: DocStore::<RuleSet>::new(dir),
            profiles: DocStore::<TrackerProfiles>::new(dir),
        }
    }

    pub fn config(&self) -> Config {
        self.cfg.load().value
    }

    pub fn load_settings(&self) -> AppSettings {
        from_config(&self.config())
    }

    pub fn save_settings(&self, s: &AppSettings) -> Result<(), CoreError> {
        let mut c = self.config();
        apply_to_config(s, &mut c);
        let errs = c.validate();
        if !errs.is_empty() {
            return Err(problems(&errs));
        }
        self.cfg.save(&c).map_err(|e| doc_err(&e))
    }

    pub fn load_rules_set(&self) -> RuleSet {
        self.rules.load().value
    }

    pub fn load_rules(&self) -> RulesDoc {
        rules_to_doc(&self.load_rules_set())
    }

    /// Builds the rule set that saving `doc` would produce (ids made, validation not run).
    pub fn build_rules(&self, doc: &RulesDoc) -> RuleSet {
        let mut set = self.load_rules_set();
        let mut taken: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for r in &doc.rules {
            let mut rule = rule_from(r);
            if rule.id.is_empty() {
                let base = slug(&rule.name, "rule");
                rule.id = unique_id(base, &|c| taken.iter().any(|t| t == c));
            }
            taken.push(rule.id.clone());
            out.push(rule);
        }
        set.rules = out;
        apply_default(&doc.default_rule, &mut set.default);
        set
    }

    pub fn save_rules(&self, doc: &RulesDoc) -> Result<RulesDoc, CoreError> {
        let set = self.build_rules(doc);
        let errs = set.validate();
        if !errs.is_empty() {
            return Err(problems(&errs));
        }
        self.rules.save(&set).map_err(|e| doc_err(&e))?;
        Ok(rules_to_doc(&set))
    }

    pub fn load_profiles_set(&self) -> TrackerProfiles {
        self.profiles.load().value
    }

    pub fn load_profiles(&self) -> Vec<ProfileRecord> {
        self.load_profiles_set().profiles.iter().map(profile_to).collect()
    }

    pub fn save_profiles(&self, list: &[ProfileRecord]) -> Result<Vec<ProfileRecord>, CoreError> {
        let mut set = self.load_profiles_set();
        let mut taken: Vec<String> = Vec::new();
        let mut out = Vec::new();
        for p in list {
            let mut urls = Vec::new();
            for u in &p.urls {
                let u = u.trim();
                if u.is_empty() {
                    continue;
                }
                let pat = UrlPattern::parse(u).map_err(|e| CoreError::Invalid {
                    problems: vec![format!("{}: {e}", p.name)],
                })?;
                // The same address twice is one address.
                if !urls.contains(&pat) {
                    urls.push(pat);
                }
            }
            let id = if p.id.is_empty() {
                unique_id(slug(&p.name, "tracker"), &|c| taken.iter().any(|t| t == c))
            } else {
                p.id.clone()
            };
            taken.push(id.clone());
            let (location, categories) = record_locations(p);
            out.push(Profile {
                id,
                name: p.name.trim().to_string(),
                urls,
                rule: p.rule.clone().filter(|r| !r.is_empty()),
                color: p.color.clone(),
                icon: p.icon.clone(),
                down_limit: p.down_limit.filter(|v| *v != 0),
                up_limit: p.up_limit.filter(|v| *v != 0),
                location,
                categories,
                hidden: p.hidden,
                hide_only_seeding: p.hidden && p.hide_only_seeding,
                allow_pex: p.allow_pex,
            });
        }
        set.profiles = out;
        // A favorite owns the setting of the domains it covers.
        set.prune_pex();
        let errs = set.validate();
        if !errs.is_empty() {
            return Err(problems(&errs));
        }
        self.profiles.save(&set).map_err(|e| doc_err(&e))?;
        Ok(set.profiles.iter().map(profile_to).collect())
    }

    /// Whether peer exchange is allowed for the private torrents of a tracker, by the id the app
    /// knows it by: a favorite's id, an automatic tracker's domain, or `other-private`.
    pub fn tracker_pex(&self, tracker_id: &str) -> bool {
        let set = self.load_profiles_set();
        if set.get(tracker_id).is_some() {
            return set.allows_pex(Some(tracker_id), None);
        }
        match tracker_id {
            "other-private" => set.allows_pex(None, None),
            id => set.allows_pex(None, Some(id)),
        }
    }

    /// Allow or forbid peer exchange for a tracker's private torrents.
    pub fn set_tracker_pex(&self, tracker_id: &str, allow: bool) -> Result<(), CoreError> {
        let mut set = self.load_profiles_set();
        set.set_pex(tracker_id, allow).map_err(|e| problems(&e))?;
        self.profiles.save(&set).map_err(|e| doc_err(&e))?;
        Ok(())
    }

    /// A ready-to-save profile named after an announce URL's host ("Create tracker profile…").
    pub fn suggest_profile(&self, announce_url: &str) -> Result<ProfileRecord, CoreError> {
        let p = Profile::suggest_from_announce(announce_url)
            .map_err(|e| CoreError::InvalidSource { message: e.to_string() })?;
        let set = self.load_profiles_set();
        let id = unique_id(p.id.clone(), &|c| set.get(c).is_some());
        Ok(ProfileRecord { id: String::new(), ..profile_to(&Profile { id, ..p }) })
    }
}

/// Expands `~` against `$HOME` (which `--home-root` redirects).
pub fn expand_home(p: &str) -> PathBuf {
    if (p == "~" || p.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME") {
            let mut b = PathBuf::from(home);
            if let Some(rest) = p.strip_prefix("~/") {
                b.push(rest);
            }
            return b;
        }
    PathBuf::from(p)
}

/// An incomplete and a finished location on different disks (SPEC §7A.4 warning).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct VolumeWarning {
    /// `None` = the app defaults, else the category whose pair crosses.
    pub category: Option<Category>,
    pub incomplete: String,
    pub finished: String,
}

pub(crate) fn volume_warnings(c: &Config) -> Vec<VolumeWarning> {
    trs_state::volume::cross_volume_pairs(c)
        .into_iter()
        .map(|p| VolumeWarning {
            category: match p.scope {
                trs_state::volume::LocationScope::AppDefault => None,
                trs_state::volume::LocationScope::Category(c) => Some(category_to(c)),
            },
            incomplete: p.incomplete,
            finished: p.finished,
        })
        .collect()
}

/// The different-disk pairs among a tracker's own folders (the editor's draft, SPEC §7A.4); the
/// warning's `category` is `None` for the tracker-wide pair.
pub(crate) fn tracker_volume_warnings(c: &Config, rec: &ProfileRecord) -> Vec<VolumeWarning> {
    let (own, cats) = record_locations(rec);
    trs_state::volume::tracker_cross_volume_pairs(c, &own, &cats)
        .into_iter()
        .map(|p| VolumeWarning {
            category: match p.scope {
                trs_state::volume::LocationScope::AppDefault => None,
                trs_state::volume::LocationScope::Category(c) => Some(category_to(c)),
            },
            incomplete: p.incomplete,
            finished: p.finished,
        })
        .collect()
}

/// The folders a [`ProfileRecord`] sets, blanks read as "same as Settings".
fn record_locations(p: &ProfileRecord) -> (trs_state::config::LocationOverride, trs_state::config::CategoryLocations) {
    let blank = |d: &Option<String>| d.clone().filter(|d| !d.trim().is_empty());
    let mut categories = trs_state::config::CategoryLocations::default();
    for o in &p.categories {
        let slot = categories.get_mut(category_from(o.category));
        slot.incomplete_dir = blank(&o.incomplete_dir);
        slot.finished_dir = blank(&o.finished_dir);
    }
    (trs_state::config::LocationOverride { incomplete_dir: blank(&p.incomplete_dir), finished_dir: blank(&p.finished_dir) }, categories)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip_keeps_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let mut s = store.load_settings();
        s.transfers.upload_limit = 5_000_000;
        s.general.notify_goal = true;
        store.save_settings(&s).unwrap();
        let again = store.load_settings();
        assert_eq!(again.transfers.upload_limit, 5_000_000);
        assert!(again.general.notify_goal);
        assert!(again.updates.check && !again.updates.auto_install);
        assert_eq!(s, again);
    }

    #[test]
    fn allow_lan_trackers_is_the_real_config_field() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let mut s = store.load_settings();
        assert!(!s.network.allow_lan_trackers, "off by default");
        s.network.allow_lan_trackers = true;
        store.save_settings(&s).unwrap();
        assert!(store.config().network.allow_lan_trackers);
        assert!(store.load_settings().network.allow_lan_trackers);
        // A config.toml that older builds wrote (the key was an unknown key) reads the same.
        std::fs::write(dir.path().join("config.toml"), "[network]\nallow_lan_trackers = true\n").unwrap();
        assert!(SettingsStore::new(dir.path()).config().network.allow_lan_trackers);
        // Turning it off writes it off.
        let mut s = store.load_settings();
        s.network.allow_lan_trackers = false;
        store.save_settings(&s).unwrap();
        assert!(!store.config().network.allow_lan_trackers);
    }

    #[test]
    fn encryption_and_utp_round_trip_through_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let mut s = store.load_settings();
        assert_eq!(s.network.encryption, EncryptionSetting::Prefer);
        assert!(s.network.utp);
        for (e, want) in [
            (EncryptionSetting::Off, Encryption::Off),
            (EncryptionSetting::Require, Encryption::Require),
            (EncryptionSetting::Prefer, Encryption::Prefer),
        ] {
            s.network.encryption = e;
            s.network.utp = e != EncryptionSetting::Require;
            store.save_settings(&s).unwrap();
            assert_eq!(store.config().network.encryption, want);
            assert_eq!(store.config().network.utp, e != EncryptionSetting::Require);
            assert_eq!(store.load_settings().network.encryption, e);
        }
        // The keys the app does not show are left alone.
        std::fs::write(dir.path().join("config.toml"), "[network]\nlsd = false\nipv6 = false\nencryption = \"require\"\n").unwrap();
        let store = SettingsStore::new(dir.path());
        let mut s = store.load_settings();
        assert_eq!(s.network.encryption, EncryptionSetting::Require);
        s.network.dht = !s.network.dht;
        store.save_settings(&s).unwrap();
        let c = store.config();
        assert!(!c.network.lsd && !c.network.ipv6);
        assert_eq!(c.network.encryption, Encryption::Require);
    }

    #[test]
    fn play_with_is_saved_in_config_toml() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let mut s = store.load_settings();
        assert_eq!(s.general.play_with, "auto");
        s.general.play_with = "org.videolan.vlc".into();
        store.save_settings(&s).unwrap();
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("play_with = \"org.videolan.vlc\""), "{text}");
        assert_eq!(store.load_settings().general.play_with, "org.videolan.vlc");
        s.general.play_with = "two words".into();
        assert!(matches!(store.save_settings(&s), Err(CoreError::Invalid { .. })));
    }

    #[test]
    fn invalid_settings_report_paths() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let mut s = store.load_settings();
        s.folders.download_dir = "relative/path".into();
        let Err(CoreError::Invalid { problems }) = store.save_settings(&s) else { panic!("expected invalid") };
        assert!(problems[0].contains("download_dir"), "{problems:?}");
    }

    #[test]
    fn rules_get_ids_and_validate() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let mut doc = store.load_rules();
        doc.rules.push(RuleRecord {
            id: String::new(),
            name: "Z: priority, H&R safe".into(),
            enabled: true,
            tracker_ids: vec![],
            private_match: PrivateMatch::PrivateOnly,
            categories: vec![Category::Tv],
            goal: Some(GoalKind::Ratio { ratio: 2.0 }),
            min_seed_secs: Some(72 * 3600),
            priority: Some(Priority::High),
            on_goal: None,
            finished_location: None,
            up_limit: None,
            idle_below_bytes: Some(2_000_000_000),
            idle_hours: 100,
        });
        let saved = store.save_rules(&doc).unwrap();
        assert_eq!(saved.rules[0].id, "z-priority-h-r-safe");
        assert_eq!(store.load_rules(), saved);
        doc.rules[0].goal = Some(GoalKind::Ratio { ratio: -1.0 });
        assert!(matches!(store.save_rules(&doc), Err(CoreError::Invalid { .. })));
    }

    #[test]
    fn profiles_strip_passkeys_and_get_ids() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let saved = store
            .save_profiles(&[ProfileRecord {
                id: String::new(),
                name: "Tracker Z".into(),
                urls: vec!["https://tracker-z.test/a/0123456789abcdef0123456789abcdef/announce".into()],
                rule: None,
                color: None,
                icon: None,
                down_limit: None,
                up_limit: None,
                incomplete_dir: None,
                finished_dir: None,
                categories: vec![],
                hidden: false,
                hide_only_seeding: false,
                allow_pex: false,
            }])
            .unwrap();
        assert_eq!(saved[0].id, "tracker-z");
        assert!(!saved[0].urls[0].contains("0123456789abcdef"));
    }

    #[test]
    fn peer_exchange_per_tracker_is_stored_for_favorites_and_automatic_trackers() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        assert!(!store.tracker_pex("example.org") && !store.tracker_pex("other-private"), "off by default");
        store.set_tracker_pex("example.org", true).unwrap();
        store.set_tracker_pex("other-private", true).unwrap();
        assert!(store.set_tracker_pex("public", true).is_err());
        let again = SettingsStore::new(dir.path());
        assert!(again.tracker_pex("example.org") && again.tracker_pex("other-private") && !again.tracker_pex("debian.org"));
        // Saving the automatic tracker as a favorite takes the setting along; saving records keeps it.
        let mut set = again.load_profiles_set();
        let id = set.add_favorite("", &["tracker.example.org".to_string()]).unwrap();
        let recs: Vec<ProfileRecord> = set.profiles.iter().map(profile_to).collect();
        assert!(recs.iter().find(|r| r.id == id).unwrap().allow_pex);
        let saved = again.save_profiles(&recs).unwrap();
        assert!(saved[0].allow_pex);
        assert!(again.tracker_pex(&id) && again.tracker_pex("other-private"));
        let text = std::fs::read_to_string(dir.path().join("trackers.toml")).unwrap();
        assert!(text.contains("pex_trackers = [\"@other-private\"]"), "the domain entry gave way to the favorite: {text}");
        // Off again, through the same call.
        again.set_tracker_pex(&id, false).unwrap();
        assert!(!again.tracker_pex(&id));
    }

    #[test]
    fn a_favorites_folders_and_hidden_mode_survive_a_save_and_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let rec = ProfileRecord {
            id: "tz".into(),
            name: "TZ".into(),
            urls: vec!["tracker-z.test".into()],
            rule: None,
            color: None,
            icon: None,
            down_limit: None,
            up_limit: None,
            incomplete_dir: Some(" ".into()),
            finished_dir: Some("/Volumes/Media/TZ".into()),
            categories: vec![
                CategoryLocationSetting { category: Category::Movie, incomplete_dir: None, finished_dir: Some("/Volumes/Media/TZ/Movies".into()) },
                CategoryLocationSetting { category: Category::Tv, incomplete_dir: None, finished_dir: None },
            ],
            hidden: true,
            hide_only_seeding: true,
            allow_pex: false,
        };
        let saved = store.save_profiles(&[rec]).unwrap();
        assert_eq!(saved[0].incomplete_dir, None, "a blank folder is \"same as Settings\"");
        assert_eq!(saved[0].finished_dir.as_deref(), Some("/Volumes/Media/TZ"));
        assert_eq!(saved[0].categories.len(), 1, "an empty category entry is dropped");
        assert!(saved[0].hidden && saved[0].hide_only_seeding);
        assert_eq!(store.load_profiles(), saved);
        let relative = ProfileRecord { finished_dir: Some("relative".into()), ..saved[0].clone() };
        assert!(matches!(store.save_profiles(&[relative]), Err(CoreError::Invalid { .. })));
        // "Only seeding" means nothing without hidden mode.
        let off = ProfileRecord { hidden: false, ..saved[0].clone() };
        assert!(!store.save_profiles(&[off]).unwrap()[0].hide_only_seeding);
    }

    #[test]
    fn a_favorites_speed_caps_survive_a_save_and_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let rec = |down, up| ProfileRecord {
            id: "tz".into(),
            name: "TZ".into(),
            urls: vec!["tracker-z.test".into()],
            rule: None,
            color: None,
            icon: None,
            down_limit: down,
            up_limit: up,
            incomplete_dir: None,
            finished_dir: None,
            categories: vec![],
            hidden: false,
            hide_only_seeding: false,
            allow_pex: false,
        };
        let saved = store.save_profiles(&[rec(Some(5_000_000), Some(0))]).unwrap();
        // 0 is "unlimited", stored as no key at all.
        assert_eq!((saved[0].down_limit, saved[0].up_limit), (Some(5_000_000), None));
        let text = std::fs::read_to_string(dir.path().join("trackers.toml")).unwrap();
        assert!(text.contains("down_limit = 5000000") && !text.contains("up_limit"), "{text}");
        let again = SettingsStore::new(dir.path()).load_profiles();
        assert_eq!((again[0].down_limit, again[0].up_limit), (Some(5_000_000), None));
        assert!(store.save_profiles(&[rec(Some(u64::MAX), None)]).is_err());
    }

    #[test]
    fn one_disk_gives_no_warning() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::new(dir.path());
        let mut s = store.load_settings();
        s.folders.download_dir = dir.path().join("done").display().to_string();
        s.folders.incomplete_dir = dir.path().join("inc").display().to_string();
        store.save_settings(&s).unwrap();
        assert_eq!(volume_warnings(&store.config()), vec![]);
    }
}
