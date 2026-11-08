//! Every control of the Settings window has a CLI path (the user's decision, 2026-11-07: "The CLI should enable the
//! agent to make all changes"). The controls are found in the window itself, on every tab as the Mac and Windows draw
//! it, so a new setting without a CLI path fails here.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::{Map, Value};
use seedwell_control::proto::{ErrorCode, SyncSettingsParams};
use seedwell_control::settings as cli;
use seedwell_state::{Config, RuleSet, TrackerProfiles};

use super::tests::{live, state};
use super::*;
use crate::host::{host_features_all, host_windows_all};
use crate::library::tests::Clock;
use crate::settings::{apply_to_config, watch_to, AgentPermission, GoalKind, SettingsStore as Files};
use crate::settings_parse::SettingsFieldKind;
use crate::settings_view_apply::settings_apply;

/// Controls that change the Settings window's state but no setting of `config.toml`, `rules.toml` or `sync.toml`.
const NO_SETTING: [(SettingKey, &str); 3] = [
    (SettingKey::OnlineLookup, "not available yet: the switch is drawn off and disabled"),
    (SettingKey::SparkleCheck, "the Sparkle updater's own switch (a build with a Sparkle feed keeps it in Sparkle)"),
    (SettingKey::SparkleInstall, "the Sparkle updater's own switch"),
];

/// Controls whose setting is the user's alone (`seedwell_control::settings::DENIED`): an agent must not raise its own
/// access or silence the notice of its changes.
const USER_ONLY: [SettingKey; 3] = [SettingKey::AllowAgents, SettingKey::AgentPermission, SettingKey::AgentNotify];

/// Controls whose setting has a command of its own (`seedwell_control::settings::ROUTED`): `settings set` names it.
const OWN_COMMAND: [(SettingKey, &str); 1] = [(SettingKey::UpdatesDevBuilds, "seedwell update dev-builds")];

/// The states the window is drawn in: the defaults, and one that shows what they hide (a watch folder selected, every
/// content type open, a goal with a ratio and a time, the schedule on, agents allowed, developer mode on).
fn states() -> Vec<SettingsState> {
    let base = state();
    let mut full = state();
    full.settings.folders.watch.push(watch_to(&seedwell_state::config::WatchFolder::added("~/Torrents")));
    full.view.selected_watch = Some(0);
    full.view.expanded_types = CATEGORIES.to_vec();
    full.rules.default_rule.goal = GoalKind::RatioAndTime { ratio: 2.0, secs: 86_400 };
    full.settings.transfers.schedule_enabled = true;
    full.settings.agents.permission = AgentPermission::Control;
    full.settings.advanced.developer_mode = true;
    vec![base, full]
}

fn model(tab: SettingsTab, s: &SettingsState, live: SettingsLive, mac: bool) -> SettingsPageModel {
    let input = SettingsViewInput { tab, state: s.clone(), live };
    if mac {
        settings_view_model(input, host_windows_all(), host_features_all(), Arc::new(Clock))
    } else {
        settings_view_model(input, Vec::new(), Vec::new(), Arc::new(Clock))
    }
}

fn rows(b: &SettingsBlock) -> Vec<&SettingsRow> {
    fn walk<'a>(r: &'a SettingsRow, out: &mut Vec<&'a SettingsRow>) {
        out.push(r);
        if let SettingsRowStyle::Disclosure { children, .. } = &r.style {
            for c in children {
                walk(c, out);
            }
        }
    }
    let mut out = Vec::new();
    let groups: Vec<&SettingsGroup> = match b {
        SettingsBlock::Single { group } => vec![group],
        SettingsBlock::Pair { left, right } => vec![left, right],
        _ => Vec::new(),
    };
    for g in groups {
        for r in &g.rows {
            walk(r, &mut out);
        }
    }
    out
}

/// Where a part of the window that is not a row of controls is changed from the command line.
fn block_path(b: &SettingsBlock) -> &'static str {
    match b {
        SettingsBlock::Single { .. } | SettingsBlock::Pair { .. } => "its rows",
        SettingsBlock::TorrentProxy { .. } => "seedwell proxy",
        SettingsBlock::RulesEditor => "seedwell settings set --file rules|trackers",
        // AGENTS.md: the rendezvous is the user's own (their Cloudflare token); agents never set it up.
        SettingsBlock::Rendezvous => "the user's alone",
        // SPEC 9A.3: pairing the user's own devices is the app's alone.
        SettingsBlock::Devices => "the user's alone",
    }
}

/// Where what a button (or a list's menu, a row's click) does is done from the command line, or why it is not.
fn command_path(c: &SettingsCommand) -> &'static str {
    match c {
        SettingsCommand::Apply { change } => match change {
            SettingsChange::Set { .. } => "seedwell settings set",
            SettingsChange::AddWatchFolder { .. } | SettingsChange::RemoveWatchFolder { .. } => "seedwell settings set folders.watch=<json>",
            SettingsChange::SyncPortRefused { .. } => "seedwell sync settings --port",
            SettingsChange::SelectWatch { .. } | SettingsChange::ToggleType { .. } | SettingsChange::Copied { .. } => "the window's own state",
            SettingsChange::CliLinked { .. }
            | SettingsChange::CliLinkExists { .. }
            | SettingsChange::CliLinkFailed { .. }
            | SettingsChange::SkillInstalled { .. }
            | SettingsChange::SkillFailed { .. }
            | SettingsChange::SkillRemoved { .. }
            | SettingsChange::SkillRemoveFailed { .. } => "the window's own state",
        },
        SettingsCommand::Rules { .. } => "seedwell settings set --file rules|trackers",
        SettingsCommand::ShowTrackerTorrents { .. } => "seedwell list --tracker",
        SettingsCommand::ChooseFolder { .. } => "seedwell settings set <key>=<folder>",
        SettingsCommand::ChooseWatchFolder => "seedwell settings set folders.watch=<json>",
        SettingsCommand::CheckForUpdates | SettingsCommand::InstallUpdate => "seedwell update now",
        SettingsCommand::ShowUpdateLog => "a view (the log file)",
        SettingsCommand::InstallBuild => "seedwell update install <zip>",
        SettingsCommand::StopWatchingBuilds => "seedwell update watch --off",
        SettingsCommand::OpenUrl { .. } | SettingsCommand::CopyCliPath { .. } | SettingsCommand::ShowInFileManager { .. } => "a view",
        SettingsCommand::Ask { .. } => "its choices",
        // Linking the tool is how the tool gets on the PATH; an agent that runs it has it.
        SettingsCommand::LinkCli { .. } => "the tool itself",
        SettingsCommand::InstallSkill { .. } => "seedwell skill install",
        SettingsCommand::RemoveSkill { .. } => "seedwell skill uninstall",
        SettingsCommand::ViewTrackerLog | SettingsCommand::ShowTrackerLog => "seedwell log trackers",
        SettingsCommand::ClearTrackerLog => "seedwell log trackers --clear",
        // The system's own settings, which the user switches.
        SettingsCommand::ManageFinderExtension | SettingsCommand::ManageShareMenu | SettingsCommand::ShowFinderSetup => "the system's settings",
        // SPEC 9A.8: only the user answers an agent's request.
        SettingsCommand::DecideAgentRequest { .. } => "the user's alone",
    }
}

/// A value for `control` other than the one it shows (`None`: it has none to offer).
fn other_value(control: &SettingsControl) -> Option<(SettingKey, SettingsValue)> {
    let typed = |t: &str| SettingsValue::Typed { text: t.into() };
    let measure = |t: &str, units: &[String], unit: &str| SettingsValue::Measure { text: t.into(), unit: units.last().cloned().unwrap_or_else(|| unit.to_owned()) };
    let plus_one = |t: &str| (t.chars().filter(char::is_ascii_digit).collect::<String>().parse::<u64>().unwrap_or(0) + 1).to_string();
    Some(match control {
        // A control drawn disabled takes no change in this state.
        SettingsControl::Toggle { enabled: false, .. } | SettingsControl::Choice { enabled: false, .. } | SettingsControl::Field { enabled: false, .. } => return None,
        SettingsControl::Toggle { key, on, .. } => (key.clone(), SettingsValue::Flag { on: !on }),
        SettingsControl::Choice { key, options, selected, .. } => {
            let o = options.iter().find(|o| o.enabled && &o.id != selected)?;
            (key.clone(), SettingsValue::Choice { id: o.id.clone() })
        }
        SettingsControl::Checklist { key, checks, .. } | SettingsControl::Chips { key, checks, .. } => {
            let c = checks.iter().find(|c| !c.checked).or(checks.first())?;
            (key.clone(), SettingsValue::Choice { id: c.id.clone() })
        }
        SettingsControl::Folder { key, path, .. } => (key.clone(), typed(if path == "~/SeedwellCli" { "~/SeedwellCli2" } else { "~/SeedwellCli" })),
        SettingsControl::Field { key, kind, text, units, unit, .. } => (
            key.clone(),
            match kind {
                SettingsFieldKind::Rate => measure("7", units, unit),
                SettingsFieldKind::Size => measure("3", units, unit),
                SettingsFieldKind::Duration => measure("3", units, unit),
                SettingsFieldKind::Hours | SettingsFieldKind::Number => typed(&plus_one(text)),
                SettingsFieldKind::Time => typed(if text == "07:30" { "08:15" } else { "07:30" }),
                SettingsFieldKind::Port => typed(if text == "51515" { "51516" } else { "51515" }),
                SettingsFieldKind::SyncPort => typed(if text == "22111" { "22112" } else { "22111" }),
                SettingsFieldKind::Ratio => typed(if text == "2.5" { "3" } else { "2.5" }),
                SettingsFieldKind::Text => typed(if text == "Studio" { "Studio 2" } else { "Studio" }),
                SettingsFieldKind::Address => typed("tracker.example"),
            },
        ),
        SettingsControl::Button { .. } | SettingsControl::Link { .. } | SettingsControl::Label { .. } => return None,
    })
}

fn config_of(s: &crate::settings::AppSettings) -> Config {
    let mut c = Config::default();
    apply_to_config(s, &mut c);
    c
}

/// The keys of `config.toml` that differ, with the new value (`null`: no longer set).
fn diff(before: &Config, after: &Config) -> Map<String, Value> {
    let (b, a) = (cli::values(before).unwrap(), cli::values(after).unwrap());
    let keys: BTreeSet<&String> = b.keys().chain(a.keys()).collect();
    keys.into_iter().filter(|k| b.get(*k) != a.get(*k)).map(|k| (k.clone(), a.get(k).cloned().unwrap_or(Value::Null))).collect()
}

/// What `sync settings` would be given to make `before` into `after` (each field named, so a new one is a compile
/// error here until it has a CLI path).
fn sync_params(before: &crate::sync::SyncSettings, after: &crate::sync::SyncSettings) -> SyncSettingsParams {
    let crate::sync::SyncSettings {
        device_name,
        default_device_name: _,
        user_name,
        port,
        lan_discovery,
        dht_discovery,
        relay_own_devices,
        relay_everyone,
        up_limit,
        down_limit,
        default_location,
    } = after.clone();
    let b = before;
    SyncSettingsParams {
        device_name: (device_name != b.device_name).then_some(device_name),
        user_name: (user_name != b.user_name).then_some(user_name),
        port: (port != b.port).then_some(port),
        lan_discovery: (lan_discovery != b.lan_discovery).then_some(lan_discovery),
        dht_discovery: (dht_discovery != b.dht_discovery).then_some(dht_discovery),
        relay_own_devices: (relay_own_devices != b.relay_own_devices).then_some(relay_own_devices),
        relay_everyone: (relay_everyone != b.relay_everyone).then_some(relay_everyone),
        up_limit: (up_limit != b.up_limit).then_some(up_limit),
        down_limit: (down_limit != b.down_limit).then_some(down_limit),
        default_location: (default_location != b.default_location).then_some(default_location),
    }
}

/// What a control's change does through the command line: `Err` says why it cannot.
fn cli_change(s: &SettingsState, key: &SettingKey, value: SettingsValue) -> Result<&'static str, String> {
    let applied = settings_apply(s.clone(), SettingsChange::Set { key: key.clone(), value: value.clone() });
    if let Some((_, why)) = NO_SETTING.iter().find(|(k, _)| k == key) {
        return if applied.saves.is_empty() { Ok(why) } else { Err(format!("{key:?} saves {:?} but is listed as changing no setting", applied.saves)) };
    }
    if applied.refused || (applied.saves.is_empty() && applied.sync_port.is_none()) {
        return Err(format!("{key:?} with {value:?} changed nothing: give the test a value it takes"));
    }
    let mut path = "";
    if let Some(port) = applied.sync_port {
        let p = SyncSettingsParams { port: Some(port), ..Default::default() };
        assert!(!p.is_empty());
        path = "seedwell sync settings";
    }
    for store in &applied.saves {
        match store {
            SettingsStore::Settings => {
                let (before, after) = (config_of(&s.settings), config_of(&applied.state.settings));
                let changes = diff(&before, &after);
                if changes.is_empty() {
                    return Err(format!("{key:?} saves config.toml but changes no key of it"));
                }
                let invalid = !after.validate().is_empty();
                match cli::apply(&before, &changes) {
                    // The control's change does not validate either: Settings cannot save it (its banner says why).
                    Err(e) if invalid && e.code == ErrorCode::Invalid => path = "seedwell settings set (refused, as Settings' save is)",
                    Ok((got, _)) if got == after => path = "seedwell settings set",
                    Ok((got, _)) => return Err(format!("{key:?}: `settings set {changes:?}` ends in another state:\n{got:?}\nnot\n{after:?}")),
                    Err(e) if e.code == ErrorCode::InvalidParams && e.message.contains("not available to agents") => {
                        if !USER_ONLY.contains(key) {
                            return Err(format!("{key:?} writes a denied key ({}) but is not in USER_ONLY", e.message));
                        }
                        path = "the user's alone";
                    }
                    Err(e) if e.message.contains("has a command of its own") => match OWN_COMMAND.iter().find(|(k, _)| k == key) {
                        Some((_, cmd)) if e.message.contains(cmd) => path = cmd,
                        _ => return Err(format!("{key:?} is refused for a command ({}) that OWN_COMMAND does not name", e.message)),
                    },
                    Err(e) => return Err(format!("{key:?}: `settings set {changes:?}` was refused: {e:?}")),
                }
            }
            SettingsStore::Rules => {
                let dir = tempfile::tempdir().unwrap();
                let files = Files::new(dir.path());
                let (before, after) = (files.build_rules(&s.rules), files.build_rules(&applied.state.rules));
                if before == after {
                    return Err(format!("{key:?} saves rules.toml but changes nothing in it"));
                }
                let doc = cli::doc_get(&after, &TrackerProfiles::default(), "rules").unwrap();
                let back: RuleSet = cli::set_rules(&before, doc).map_err(|e| format!("{key:?}: `settings set --file rules` refused the rules: {e:?}"))?;
                if back != after {
                    return Err(format!("{key:?}: `settings set --file rules` ends in other rules"));
                }
                path = "seedwell settings set --file rules";
            }
            SettingsStore::Sync => {
                let (Some(b), Some(a)) = (&s.sync, &applied.state.sync) else { return Err(format!("{key:?} saves sync.toml without sync settings")) };
                if sync_params(b, a).is_empty() {
                    return Err(format!("{key:?} changes folder sync's settings in a way `sync settings` cannot say"));
                }
                path = "seedwell sync settings";
            }
        }
    }
    Ok(path)
}

/// Failure: a control of the Settings window (any tab, Mac or Windows) changes a setting an agent with permission full
/// cannot change from the command line (a new setting added without a CLI path), or the CLI's change of it ends in
/// another state than the control's (a watch folder added with other defaults, a switch that writes two keys), or a
/// button, a list's menu or a part of the window has no named CLI path.
#[test]
fn every_settings_control_has_a_cli_path_that_ends_in_the_same_state() {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut failures = Vec::new();
    let mut paths = Vec::new();
    let sparkle = SettingsUpdates::Sparkle { check: true, install: false, can_check: true, version: "0.1.20".into() };
    for s in states() {
        for tab in TABS {
            for (mac, live) in [(true, live()), (false, live()), (true, SettingsLive { updates: sparkle.clone(), ..live() })] {
                let m = model(tab, &s, live, mac);
                if m.tab != tab {
                    continue;
                }
                for b in &m.blocks {
                    paths.push(block_path(b));
                    for r in rows(b) {
                        let mut commands = Vec::new();
                        if let SettingsRowStyle::Item { select, menus, .. } = &r.style {
                            commands.extend(select.clone());
                            for e in menus.iter().flat_map(|m| &m.entries) {
                                if let SettingsMenuEntry::Item { command, .. } = e {
                                    commands.push(command.clone());
                                }
                            }
                        }
                        if let SettingsRowStyle::Disclosure { toggle, .. } = &r.style {
                            commands.push(toggle.clone());
                        }
                        for c in &r.controls {
                            match c {
                                SettingsControl::Button { action } => commands.push(action.command.clone()),
                                SettingsControl::Folder { choose, .. } => commands.push(choose.command.clone()),
                                _ => {}
                            }
                            let Some((key, value)) = other_value(c) else { continue };
                            match cli_change(&s, &key, value) {
                                Ok(p) => {
                                    seen.insert(format!("{key:?}"));
                                    paths.push(p);
                                }
                                Err(e) => failures.push(format!("{tab:?} {}: {e}", r.id)),
                            }
                        }
                        while let Some(c) = commands.pop() {
                            if let SettingsCommand::Ask { question } = &c {
                                commands.extend(question.choices.iter().map(|q| q.command.clone()));
                            }
                            paths.push(command_path(&c));
                        }
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // The window was really walked: a setting of each kind of store, and the ones the user keeps.
    for k in ["DownloadLimit", "ListenPort", "LaunchAtLogin", "UpdatesAutoInstall", "WatchMode { index: 0 }", "DefaultGoalRatio", "DeviceName", "SyncPort", "AgentPermission", "UpdatesDevBuilds", "TypeFinishedDir { category: Movie }", "SparkleCheck"] {
        assert!(seen.contains(k), "{k} was not found in the window: {seen:?}");
    }
    for p in ["seedwell proxy", "seedwell settings set --file rules|trackers", "seedwell log trackers --clear", "seedwell update now"] {
        assert!(paths.contains(&p), "{p}");
    }
}
