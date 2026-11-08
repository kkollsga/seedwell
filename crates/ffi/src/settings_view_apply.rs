//! What a change in the Settings window does (docs/CROSS-PLATFORM.md §1): [`settings_apply`] reads the value a control
//! sent (with the parsers of settings_parse.rs), changes the state and says which stores to save. A value it cannot
//! read changes nothing (`refused`); the field shows the value in force again.

use crate::menu::APP_NAME;
use crate::settings::{
    AgentPermission, AskMode, EncryptionSetting, OnBatteryMode, OnGoalAction, PreallocationMode, ScheduleDays, TorrentFileAction,
    WatchModeSetting,
};
use crate::settings::GoalKind;
use crate::settings_parse::{self as p, PortEntry, SettingsFieldKind};
use crate::settings_view::{
    category_from_id, SettingKey, SettingsApplied, SettingsChange, SettingsCommand, SettingsQuestion, SettingsQuestionChoice, SettingsState,
    SettingsStore, SettingsValue,
};

/// The longest device or user name.
const NAME_MAX: usize = 64;

/// What changed, as the apply functions below report it.
#[derive(Default)]
struct Outcome {
    saves: Vec<SettingsStore>,
    save_now: bool,
    sync_port: Option<u16>,
    refused: bool,
    ask: Option<SettingsQuestion>,
}

impl Outcome {
    fn save(store: SettingsStore) -> Self {
        Outcome { saves: vec![store], ..Outcome::default() }
    }
    fn refused() -> Self {
        Outcome { refused: true, ..Outcome::default() }
    }
}

pub(crate) fn on(v: &SettingsValue) -> Option<bool> {
    match v {
        SettingsValue::Flag { on } => Some(*on),
        _ => None,
    }
}

pub(crate) fn id(v: &SettingsValue) -> Option<&str> {
    match v {
        SettingsValue::Choice { id } => Some(id),
        _ => None,
    }
}

pub(crate) fn text(v: &SettingsValue) -> Option<&str> {
    match v {
        SettingsValue::Typed { text } | SettingsValue::Measure { text, .. } => Some(text),
        _ => None,
    }
}

pub(crate) fn measure(v: &SettingsValue) -> Option<(&str, &str)> {
    match v {
        SettingsValue::Measure { text, unit } => Some((text, unit)),
        SettingsValue::Typed { text } => Some((text, "")),
        _ => None,
    }
}

/// Sets `*field` to `value` when it is a change: the store to save, or nothing.
fn put<T: PartialEq>(field: &mut T, value: T, store: SettingsStore) -> Outcome {
    if *field == value {
        Outcome::default()
    } else {
        *field = value;
        Outcome::save(store)
    }
}

/// A switch.
fn put_on(field: &mut bool, v: &SettingsValue, store: SettingsStore) -> Outcome {
    on(v).map_or_else(Outcome::refused, |x| put(field, x, store))
}

/// A choice read with `read`.
fn put_choice<T: PartialEq>(field: &mut T, v: &SettingsValue, store: SettingsStore, read: impl Fn(&str) -> Option<T>) -> Outcome {
    id(v).and_then(read).map_or_else(Outcome::refused, |x| put(field, x, store))
}

/// A speed limit.
fn put_rate(field: &mut u64, v: &SettingsValue, store: SettingsStore) -> Outcome {
    measure(v).and_then(|(t, u)| read_field(SettingsFieldKind::Rate, t, u, *field)).map_or_else(Outcome::refused, |x| put(field, x, store))
}

/// A number typed into a field of `kind` (in `unit`; empty: the unit `current` is shown in), or `current` when the
/// text is just `current` written out (rounded: "1.235" for 1,234,567 B/s is no edit). `None`: refused.
pub(crate) fn read_field(kind: SettingsFieldKind, text: &str, unit: &str, current: u64) -> Option<u64> {
    let shown = settings_field_text(kind, current);
    let unit = if unit.is_empty() { shown.unit.as_str() } else { unit };
    if shown.unit == unit && text.trim() == shown.text {
        return Some(current);
    }
    // `current` written out in another unit: the unit was changed and the number converted with it
    // ([`settings_field_convert`]), "0.1 days" for 2 hours is no edit.
    if unit != shown.unit && written_in(kind, current, unit).is_some_and(|w| text.trim() == w) {
        return Some(current);
    }
    match kind {
        SettingsFieldKind::Rate => p::rate_parse(text, unit),
        SettingsFieldKind::Size => p::size_parse(text, unit),
        SettingsFieldKind::Hours => p::hours_parse(text).map(u64::from),
        SettingsFieldKind::Time => p::time_parse(text).map(u64::from),
        SettingsFieldKind::Number => p::whole_parse(text).map(u64::from),
        SettingsFieldKind::Duration => p::duration_parse(text, unit),
        SettingsFieldKind::Port | SettingsFieldKind::SyncPort => match p::port_parse(text) {
            PortEntry::Automatic => Some(0),
            PortEntry::Manual(port) => Some(port.into()),
            PortEntry::Invalid => None,
        },
        SettingsFieldKind::Ratio | SettingsFieldKind::Text | SettingsFieldKind::Address => None,
    }
}

/// A goal after one of its controls (`settings_goal_controls`: `DefaultGoal`, `DefaultGoalRatio`, `DefaultGoalTime`)
/// took `v`.
pub(crate) fn goal_edit(goal: &GoalKind, key: &SettingKey, v: &SettingsValue) -> Option<GoalKind> {
    match key {
        SettingKey::DefaultGoal | SettingKey::RuleGoal => id(v).and_then(|x| p::goal_with_id(goal, x)),
        SettingKey::DefaultGoalRatio | SettingKey::RuleGoalRatio => text(v).and_then(p::ratio_parse).map(|r| p::goal_with_ratio(goal, r)),
        SettingKey::DefaultGoalTime | SettingKey::RuleGoalTime => {
            measure(v).and_then(|(t, u)| read_field(SettingsFieldKind::Duration, t, u, p::goal_secs(goal))).map(|secs| p::goal_with_secs(goal, secs))
        }
        _ => None,
    }
}

/// How a field of `kind` shows a stored number.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SettingsFieldText {
    pub text: String,
    /// The unit it is shown in, and the units of its menu (empty: none).
    pub unit: String,
    pub units: Vec<String>,
}

/// `value` as a field of `kind` shows it: bytes per second (`Rate`), bytes (`Size`), hours, minutes since midnight
/// (`Time`), a port, a whole number, seconds (`Duration`). For the editors a host still draws itself (the rules and
/// trackers editors, a tracker's Info page); Settings' own fields come ready in the model.
#[uniffi::export]
pub fn settings_field_text(kind: SettingsFieldKind, value: u64) -> SettingsFieldText {
    let units = |u: &[&str]| u.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match kind {
        SettingsFieldKind::Rate => {
            let unit = p::rate_unit(value);
            SettingsFieldText { text: p::rate_number(value, unit), unit: unit.into(), units: units(&p::RATE_UNITS) }
        }
        SettingsFieldKind::Size => {
            let unit = p::size_unit(value);
            SettingsFieldText { text: p::size_number(value, unit), unit: unit.into(), units: units(&p::SIZE_UNITS) }
        }
        SettingsFieldKind::Duration => {
            let unit = p::duration_unit(value);
            SettingsFieldText { text: p::duration_number(value, unit), unit: unit.into(), units: units(&p::DURATION_UNITS) }
        }
        SettingsFieldKind::Time => SettingsFieldText { text: p::time_text(value as u32), unit: String::new(), units: Vec::new() },
        SettingsFieldKind::Port | SettingsFieldKind::SyncPort => {
            SettingsFieldText { text: p::port_text(u16::try_from(value).unwrap_or(0)), unit: String::new(), units: Vec::new() }
        }
        SettingsFieldKind::Number => SettingsFieldText { text: p::whole_text(value), unit: String::new(), units: Vec::new() },
        SettingsFieldKind::Hours | SettingsFieldKind::Ratio | SettingsFieldKind::Text | SettingsFieldKind::Address => {
            SettingsFieldText { text: value.to_string(), unit: String::new(), units: Vec::new() }
        }
    }
}

/// `value` as a field of `kind` writes it in `unit` (a field with a unit menu), `None` for a kind without units.
fn written_in(kind: SettingsFieldKind, value: u64, unit: &str) -> Option<String> {
    match kind {
        SettingsFieldKind::Rate => Some(p::rate_number(value, unit)),
        SettingsFieldKind::Size => Some(p::size_number(value, unit)),
        SettingsFieldKind::Duration => Some(p::duration_number(value, unit)),
        _ => None,
    }
}

/// The text of a field of `kind` when its unit menu goes from `from` to `to`: the same amount written in the new unit
/// (2 hours are "0.1" days, 1.5 MB/s is "1500" KB/s), so changing the unit never changes the value. Text that cannot
/// be read (or is empty) stays as it is. The host sends it with the new unit; [`settings_field_read`] takes it as the
/// value it was.
#[uniffi::export]
pub fn settings_field_convert(kind: SettingsFieldKind, text: String, from: String, to: String) -> String {
    let value = match kind {
        SettingsFieldKind::Rate => p::rate_parse(&text, &from),
        SettingsFieldKind::Size => p::size_parse(&text, &from),
        SettingsFieldKind::Duration => p::duration_parse(&text, &from),
        _ => None,
    };
    match value {
        Some(v) if !text.trim().is_empty() && from != to => written_in(kind, v, &to).unwrap_or(text),
        _ => text,
    }
}

/// What was typed into a field of `kind` in `unit`, as the number it stores (see [`settings_field_text`]); `current`
/// when the text is `current` as the field shows it; `None` when it cannot be read.
#[uniffi::export]
pub fn settings_field_read(kind: SettingsFieldKind, text: String, unit: String, current: u64) -> Option<u64> {
    read_field(kind, &text, &unit, current)
}

/// `goal` after one of its controls (from `settings_goal_controls`) sent `value`; `None` when it is refused.
#[uniffi::export]
pub fn settings_goal_apply(goal: GoalKind, key: SettingKey, value: SettingsValue) -> Option<GoalKind> {
    goal_edit(&goal, &key, &value)
}

/// A whole number.
fn put_whole(field: &mut u32, v: &SettingsValue, store: SettingsStore) -> Outcome {
    text(v).and_then(p::whole_parse).map_or_else(Outcome::refused, |x| put(field, x, store))
}

/// A path: empty is `None` (the default).
fn put_path(field: &mut Option<String>, v: &SettingsValue, store: SettingsStore) -> Outcome {
    let Some(t) = text(v) else { return Outcome::refused() };
    put(field, (!t.is_empty()).then(|| t.to_string()), store)
}

fn battery(id: &str) -> Option<OnBatteryMode> {
    Some(match id {
        "keep-going" => OnBatteryMode::KeepGoing,
        "limit" => OnBatteryMode::Limit,
        "pause" => OnBatteryMode::Pause,
        _ => return None,
    })
}

fn days(id: &str) -> Option<ScheduleDays> {
    Some(match id {
        "weekdays" => ScheduleDays::Weekdays,
        "weekends" => ScheduleDays::Weekends,
        "every-day" => ScheduleDays::EveryDay,
        _ => return None,
    })
}

fn ask(id: &str) -> Option<AskMode> {
    Some(match id {
        "never" => AskMode::Never,
        "multi-file" => AskMode::MultiFile,
        "always" => AskMode::Always,
        _ => return None,
    })
}

pub(crate) fn on_goal(id: &str) -> Option<OnGoalAction> {
    Some(match id {
        "stop-keep" => OnGoalAction::StopKeep,
        "remove-keep-files" => OnGoalAction::RemoveKeepFiles,
        "remove-delete-files" => OnGoalAction::RemoveDeleteFiles,
        _ => return None,
    })
}

fn action(id: &str) -> Option<TorrentFileAction> {
    Some(match id {
        "trash" => TorrentFileAction::Trash,
        "archive" => TorrentFileAction::Archive,
        "leave" => TorrentFileAction::Leave,
        _ => return None,
    })
}

fn encryption(id: &str) -> Option<EncryptionSetting> {
    Some(match id {
        "off" => EncryptionSetting::Off,
        "prefer" => EncryptionSetting::Prefer,
        "require" => EncryptionSetting::Require,
        _ => return None,
    })
}

fn permission(id: &str) -> Option<AgentPermission> {
    Some(match id {
        "read-only" => AgentPermission::ReadOnly,
        "control" => AgentPermission::Control,
        "full" => AgentPermission::Full,
        _ => return None,
    })
}

/// A device or user name: trimmed, at most 64 characters.
fn name(t: &str) -> String {
    t.trim().chars().take(NAME_MAX).collect()
}

fn set(s: &mut SettingsState, key: &SettingKey, v: &SettingsValue) -> Outcome {
    use SettingsStore::{Rules, Settings, Sync};
    let st = &mut s.settings;
    match key {
        // General
        SettingKey::LaunchAtLogin => put_on(&mut st.general.launch_at_login, v, Settings),
        SettingKey::NotifyFinished => put_on(&mut st.general.notify_finished, v, Settings),
        SettingKey::NotifyErrors => put_on(&mut st.general.notify_errors, v, Settings),
        SettingKey::NotifyGoal => put_on(&mut st.general.notify_goal, v, Settings),
        SettingKey::PlayWith => put_choice(&mut st.general.play_with, v, Settings, |x| Some(x.to_string())),
        SettingKey::PreventSleep => put_on(&mut st.general.prevent_sleep_while_downloading, v, Settings),
        SettingKey::KeepAwakeForAgents => put_on(&mut st.general.keep_awake_for_agents, v, Settings),
        SettingKey::OnBattery => put_choice(&mut st.general.on_battery, v, Settings, battery),
        SettingKey::UpdatesCheck => put_on(&mut st.updates.check, v, Settings),
        SettingKey::UpdatesAutoInstall => put_on(&mut st.updates.auto_install, v, Settings),
        SettingKey::UpdatesDevBuilds => put_on(&mut st.updates.dev_builds, v, Settings),
        SettingKey::SparkleCheck | SettingKey::SparkleInstall => Outcome::default(),
        // Transfers
        SettingKey::DownloadLimit => put_rate(&mut st.transfers.download_limit, v, Settings),
        SettingKey::UploadLimit => put_rate(&mut st.transfers.upload_limit, v, Settings),
        SettingKey::AltDownloadLimit => put_rate(&mut st.transfers.alt_download_limit, v, Settings),
        SettingKey::AltUploadLimit => put_rate(&mut st.transfers.alt_upload_limit, v, Settings),
        SettingKey::ScheduleEnabled => put_on(&mut st.transfers.schedule_enabled, v, Settings),
        SettingKey::ScheduleDays => put_choice(&mut st.transfers.schedule_days, v, Settings, days),
        SettingKey::ScheduleFrom => text(v).and_then(p::time_parse).map_or_else(Outcome::refused, |m| put(&mut st.transfers.schedule_from_minute, m, Settings)),
        SettingKey::ScheduleTo => text(v).and_then(p::time_parse).map_or_else(Outcome::refused, |m| put(&mut st.transfers.schedule_to_minute, m, Settings)),
        SettingKey::MaxActiveDownloads => put_whole(&mut st.transfers.max_active_downloads, v, Settings),
        SettingKey::MaxConnectionsPerHost => put_whole(&mut st.transfers.max_connections_per_host, v, Settings),
        SettingKey::StartDelay => put_whole(&mut st.transfers.start_delay_secs, v, Settings),
        SettingKey::AskBeforeAdding => put_choice(&mut st.transfers.ask_before_adding, v, Settings, ask),
        SettingKey::LargeTorrentGb => put_whole(&mut st.transfers.large_torrent_gb, v, Settings),
        SettingKey::DefaultGoal | SettingKey::DefaultGoalRatio | SettingKey::DefaultGoalTime => {
            let goal = s.rules.default_rule.goal;
            goal_edit(&goal, key, v).map_or_else(Outcome::refused, |g| put(&mut s.rules.default_rule.goal, g, Rules))
        }
        SettingKey::DefaultOnGoal => put_choice(&mut s.rules.default_rule.on_goal, v, Rules, on_goal),
        SettingKey::PrivateMinSeed => put_choice(&mut st.transfers.private_min_seed_secs, v, Settings, |x| x.parse().ok()),
        SettingKey::IdleMinUpload => {
            let field = &mut st.transfers.idle_min_upload_bytes;
            measure(v).and_then(|(t, u)| read_field(SettingsFieldKind::Size, t, u, *field)).map_or_else(Outcome::refused, |b| put(field, b, Settings))
        }
        SettingKey::IdleWindowHours => text(v).and_then(p::hours_parse).map_or_else(Outcome::refused, |h| put(&mut st.transfers.idle_window_hours, h, Settings)),
        // Folders
        SettingKey::IncompleteDir => text(v).map_or_else(Outcome::refused, |t| put(&mut st.folders.incomplete_dir, t.to_string(), Settings)),
        SettingKey::DownloadDir => text(v).map_or_else(Outcome::refused, |t| put(&mut st.folders.download_dir, t.to_string(), Settings)),
        SettingKey::ByStatus => put_on(&mut st.folders.by_status, v, Settings),
        SettingKey::SkipJunk => put_on(&mut st.folders.skip_junk, v, Settings),
        SettingKey::AskWhenUnsure => put_on(&mut st.folders.ask_when_unsure, v, Settings),
        // Not available yet: the switch is drawn off and disabled.
        SettingKey::OnlineLookup => Outcome::refused(),
        SettingKey::TorrentAfterAdd => put_choice(&mut st.folders.torrent_after_add, v, Settings, action),
        SettingKey::ArchiveDir => put_path(&mut st.folders.archive_dir, v, Settings),
        SettingKey::WatchMode { index } => match st.folders.watch.get_mut(*index as usize) {
            Some(w) => put_choice(&mut w.mode, v, Settings, |x| match x {
                "auto" => Some(WatchModeSetting::Auto),
                "ask" => Some(WatchModeSetting::Ask),
                _ => None,
            }),
            None => Outcome::refused(),
        },
        SettingKey::WatchEnabled { index } => match st.folders.watch.get_mut(*index as usize) {
            Some(w) => put_on(&mut w.enabled, v, Settings),
            None => Outcome::refused(),
        },
        SettingKey::WatchCategory { index } => match st.folders.watch.get_mut(*index as usize) {
            Some(w) => put_choice(&mut w.category, v, Settings, |x| if x == "detect" { Some(None) } else { category_from_id(x).map(Some) }),
            None => Outcome::refused(),
        },
        SettingKey::WatchThen { index } => match st.folders.watch.get_mut(*index as usize) {
            Some(w) => put_choice(&mut w.after_add, v, Settings, action),
            None => Outcome::refused(),
        },
        SettingKey::TypeIncompleteDir { category } => match st.folders.categories.iter_mut().find(|c| c.category == *category) {
            Some(c) => put_path(&mut c.incomplete_dir, v, Settings),
            None => Outcome::refused(),
        },
        SettingKey::TypeFinishedDir { category } => match st.folders.categories.iter_mut().find(|c| c.category == *category) {
            Some(c) => put_path(&mut c.finished_dir, v, Settings),
            None => Outcome::refused(),
        },
        // Network
        SettingKey::ListenPort => {
            let entry = text(v).map_or(PortEntry::Invalid, p::port_parse);
            s.view.port_invalid = entry == PortEntry::Invalid;
            match entry {
                PortEntry::Automatic => put(&mut st.network.listen_port, 0, Settings),
                PortEntry::Manual(port) => put(&mut st.network.listen_port, port, Settings),
                PortEntry::Invalid => Outcome::refused(),
            }
        }
        SettingKey::PortMapping => put_on(&mut st.network.port_mapping, v, Settings),
        SettingKey::AllowLanTrackers => put_on(&mut st.network.allow_lan_trackers, v, Settings),
        SettingKey::Dht => put_on(&mut st.network.dht, v, Settings),
        SettingKey::Pex => put_on(&mut st.network.pex, v, Settings),
        SettingKey::Encryption => put_choice(&mut st.network.encryption, v, Settings, encryption),
        SettingKey::Utp => put_on(&mut st.network.utp, v, Settings),
        SettingKey::MaxConnections => put_whole(&mut st.network.max_connections, v, Settings),
        SettingKey::MaxConnectionsPerTorrent => put_whole(&mut st.network.max_connections_per_torrent, v, Settings),
        // Folder Sync
        SettingKey::SyncPort => {
            let Some(sync) = &s.sync else { return Outcome::refused() };
            match text(v).map_or(PortEntry::Invalid, p::port_parse) {
                // Empty goes back to the port in use.
                PortEntry::Automatic => Outcome::refused(),
                PortEntry::Manual(port) if port == sync.port => Outcome::default(),
                PortEntry::Manual(port) => {
                    s.view.sync_port_error = None;
                    Outcome { sync_port: Some(port), ..Outcome::default() }
                }
                PortEntry::Invalid => {
                    s.view.sync_port_error = Some("Use a port from 1024 to 65535.".into());
                    Outcome::refused()
                }
            }
        }
        key => {
            let Some(sync) = s.sync.as_mut() else { return sync_or_agents(st, key, v) };
            match key {
                SettingKey::DeviceName => text(v).map_or_else(Outcome::refused, |t| put(&mut sync.device_name, name(t), Sync)),
                SettingKey::UserName => text(v).map_or_else(Outcome::refused, |t| put(&mut sync.user_name, name(t), Sync)),
                SettingKey::LanDiscovery => put_on(&mut sync.lan_discovery, v, Sync),
                SettingKey::DhtDiscovery => put_on(&mut sync.dht_discovery, v, Sync),
                SettingKey::RelayOwnDevices => put_on(&mut sync.relay_own_devices, v, Sync),
                SettingKey::RelayEveryone => put_on(&mut sync.relay_everyone, v, Sync),
                SettingKey::SyncDownLimit => put_rate(&mut sync.down_limit, v, Sync),
                SettingKey::SyncUpLimit => put_rate(&mut sync.up_limit, v, Sync),
                SettingKey::SyncLocation => text(v).map_or_else(Outcome::refused, |t| put(&mut sync.default_location, t.to_string(), Sync)),
                key => sync_or_agents(st, key, v),
            }
        }
    }
}

/// The keys of Agents and Advanced (and Folder Sync's, refused while sync has no settings).
fn sync_or_agents(st: &mut crate::settings::AppSettings, key: &SettingKey, v: &SettingsValue) -> Outcome {
    use SettingsStore::Settings;
    match key {
        SettingKey::AllowAgents => match on(v) {
            Some(true) if st.agents.permission == AgentPermission::Off => put(&mut st.agents.permission, AgentPermission::Control, Settings),
            Some(true) => Outcome::default(),
            Some(false) => put(&mut st.agents.permission, AgentPermission::Off, Settings),
            None => Outcome::refused(),
        },
        // The permission is picked while agents are allowed.
        SettingKey::AgentPermission if st.agents.permission == AgentPermission::Off => Outcome::refused(),
        SettingKey::AgentPermission => put_choice(&mut st.agents.permission, v, Settings, permission),
        SettingKey::AgentNotify => put_on(&mut st.agents.notify_changes, v, Settings),
        SettingKey::WriteCache => put_choice(&mut st.advanced.write_cache_mib, v, Settings, |x| x.parse().ok()),
        SettingKey::Preallocation => put_choice(&mut st.advanced.preallocation, v, Settings, |x| match x {
            "sparse" => Some(PreallocationMode::Sparse),
            "full" => Some(PreallocationMode::Full),
            _ => None,
        }),
        SettingKey::VerifyOnCompletion => put_on(&mut st.advanced.verify_on_completion, v, Settings),
        SettingKey::DeveloperMode => Outcome { save_now: true, ..put_on(&mut st.advanced.developer_mode, v, Settings) },
        _ => Outcome::refused(),
    }
}

/// The question when the skill files were changed since they were written.
fn replace_skill_question(dir: &str) -> SettingsQuestion {
    SettingsQuestion {
        title: "Replace the changed skill files?".into(),
        text: format!("Files in {dir} were changed since {APP_NAME} wrote them. Replacing them loses those changes."),
        choices: vec![SettingsQuestionChoice { title: "Replace".into(), command: SettingsCommand::InstallSkill { dir: dir.into(), force: true } }],
        cancel_title: "Cancel".into(),
    }
}

fn change(s: &mut SettingsState, c: &SettingsChange) -> Outcome {
    let view = &mut s.view;
    match c {
        SettingsChange::Set { key, value } => set(s, key, value),
        SettingsChange::SelectWatch { index } => {
            view.selected_watch = if view.selected_watch == Some(*index) { None } else { Some(*index) };
            Outcome::default()
        }
        SettingsChange::AddWatchFolder { path } => {
            let watch = &mut s.settings.folders.watch;
            if path.trim().is_empty() || watch.iter().any(|w| &w.path == path) {
                return Outcome::refused();
            }
            // The same new entry an agent's `settings set folders.watch` makes (`WatchFolder::added`).
            watch.push(crate::settings::watch_to(&seedwell_state::config::WatchFolder::added(path.clone())));
            s.view.selected_watch = Some(watch.len() as u32 - 1);
            Outcome::save(SettingsStore::Settings)
        }
        SettingsChange::RemoveWatchFolder { index } => {
            let watch = &mut s.settings.folders.watch;
            if (*index as usize) >= watch.len() {
                return Outcome::refused();
            }
            watch.remove(*index as usize);
            s.view.selected_watch = None;
            Outcome::save(SettingsStore::Settings)
        }
        SettingsChange::ToggleType { category } => {
            if let Some(n) = view.expanded_types.iter().position(|c| c == category) {
                view.expanded_types.remove(n);
            } else {
                view.expanded_types.push(*category);
            }
            Outcome::default()
        }
        SettingsChange::SyncPortRefused { message } => {
            view.sync_port_error = Some(message.clone());
            Outcome::default()
        }
        SettingsChange::Copied { on } => {
            view.copied = *on;
            Outcome::default()
        }
        SettingsChange::CliLinked { link, dir, on_path } => {
            view.cli_note = Some(if *on_path {
                format!("Linked {link}. Open a new terminal window and type seedwell help.")
            } else {
                format!("Linked {link}. Add {dir} to your PATH to type seedwell without its full path.")
            });
            Outcome::default()
        }
        SettingsChange::CliLinkExists { path } => {
            view.cli_note = Some(format!("{path} already exists and points somewhere else, so it was left alone."));
            Outcome::default()
        }
        SettingsChange::CliLinkFailed { dir, target } => {
            view.cli_note = Some(format!("Could not link into {dir}. In a terminal: sudo ln -s '{target}' {dir}/seedwell"));
            Outcome::default()
        }
        SettingsChange::SkillInstalled { dir } => {
            view.cli_note = Some(format!("Skill written to {dir}. Claude Code uses it in new sessions."));
            Outcome::default()
        }
        // The tool refuses to overwrite files that were changed, and says to add --force: ask first.
        SettingsChange::SkillFailed { dir, error } if error.contains("--force") => Outcome { ask: Some(replace_skill_question(dir)), ..Outcome::default() },
        SettingsChange::SkillFailed { error, .. } => {
            view.cli_note = Some(format!("Could not install the skill: {}", error.trim()));
            Outcome::default()
        }
        SettingsChange::SkillRemoved { dir } => {
            view.cli_note = Some(format!("Skill removed from {dir}."));
            Outcome::default()
        }
        SettingsChange::SkillRemoveFailed { error, .. } => {
            view.cli_note = Some(format!("Could not remove the skill: {}", error.trim()));
            Outcome::default()
        }
    }
}

/// `change` applied to `state`: the new state, which stores to save, and what else to do.
#[uniffi::export]
pub fn settings_apply(state: SettingsState, change: SettingsChange) -> SettingsApplied {
    let mut s = state;
    let o = self::change(&mut s, &change);
    SettingsApplied { state: s, saves: o.saves, save_now: o.save_now, sync_port: o.sync_port, refused: o.refused, ask: o.ask }
}
