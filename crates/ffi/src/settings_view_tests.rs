use std::sync::Arc;

use super::*;
use crate::host::{host_features_all, host_windows_all};
use crate::library::tests::Clock;
use crate::settings::{CategoryLocationSetting, GoalKind, SettingsStore as Files};
use crate::settings_parse::SettingsFieldKind;
use crate::settings_view_apply::{settings_apply, settings_field_convert, settings_field_read};
use crate::notify::SettingsBannerTone;
use crate::updates::UpdateButton;

/// What Windows passes (build 41) and its features.
const WINDOWS: [HostWindow; 8] = [
    HostWindow::GetInfo,
    HostWindow::MoveTo,
    HostWindow::NewChannel,
    HostWindow::ChatDialogs,
    HostWindow::AddFolder,
    HostWindow::FolderEditor,
    HostWindow::FolderShare,
    HostWindow::ChannelShare,
];

pub(crate) fn state() -> SettingsState {
    let dir = tempfile::tempdir().unwrap();
    let store = Files::new(dir.path());
    let mut settings = store.load_settings();
    settings.folders.download_dir = "~/Downloads".into();
    settings.folders.incomplete_dir = "~/Downloads/Incomplete".into();
    settings.folders.categories = vec![
        CategoryLocationSetting { category: Category::Movie, incomplete_dir: None, finished_dir: Some("~/Movies".into()) },
        CategoryLocationSetting { category: Category::Tv, incomplete_dir: None, finished_dir: None },
    ];
    SettingsState {
        settings,
        rules: store.load_rules(),
        sync: Some(SyncSettings {
            device_name: String::new(),
            default_device_name: "Kristian's MacBook".into(),
            user_name: "Kristian".into(),
            port: 22100,
            lan_discovery: true,
            dht_discovery: true,
            relay_own_devices: true,
            relay_everyone: false,
            up_limit: 0,
            down_limit: 2_500_000,
            default_location: String::new(),
        }),
        view: settings_view_state(),
    }
}

fn button(title: &str, enabled: bool) -> UpdateButton {
    UpdateButton { title: title.into(), help: format!("{title} help"), enabled }
}

pub(crate) fn live() -> SettingsLive {
    SettingsLive {
        problems: Vec::new(),
        notes: Vec::new(),
        restart_required: Vec::new(),
        volume_warnings: Vec::new(),
        active_port: 51413,
        port_fallback: None,
        reachability: Some(ReachabilityInfo { state: "open".into(), summary: "Open: 3 peers connected in".into() }),
        sync_reachability: None,
        sync_port_moving_to: None,
        rendezvous: true,
        diagnostics: DiagnosticsInfo { wakeups: 12, connections: 40, rechecks: 1, hash_failures: 0, peers_banned: 0, promotions: 0, demotions: 0 },
        activity: Vec::new(),
        requests: Vec::new(),
        updates: SettingsUpdates::Core {
            model: UpdatesModel {
                check_label: "Check automatically".into(),
                check_hint: "Once a day".into(),
                install_label: "Install automatically".into(),
                install_hint: "When nothing is downloading".into(),
                switches_enabled: true,
                status_label: "Check for updates".into(),
                status: "Seedwell 0.1.15 is up to date".into(),
                check_now: button("Check Now", true),
                install: None,
                notes_title: "Release Notes".into(),
                notes_url: None,
                show_log: None,
                available_version: None,
                dev_label: "Allow development builds".into(),
                dev_hint: "Builds signed with the build key".into(),
                install_build: None,
                watching: None,
                stop_watching: None,
                version: "0.1.15".into(),
                refresh_ms: 60_000,
            },
        },
        play_with: vec![
            PlayWithChoice { value: "auto".into(), title: "Automatic".into(), enabled: true, mp4_only: false },
            PlayWithChoice { value: "iina".into(), title: "IINA".into(), enabled: true, mp4_only: false },
            PlayWithChoice { value: "org.videolan.vlc".into(), title: "VLC".into(), enabled: true, mp4_only: false },
            PlayWithChoice { value: "com.apple.QuickTimePlayerX".into(), title: "QuickTime Player".into(), enabled: true, mp4_only: true },
        ],
        cli: SettingsCli { home: "/Users/me".into(), bundled: "/Applications/Seedwell.app/Contents/MacOS/seedwell".into() },
        host: crate::settings_view_host::SettingsHostFacts { version_line: "Seedwell 0.1.15".into(), ..Default::default() },
        finder_on: false,
        share_menu_on: true,
        state_dir: "/Users/me/Library/Application Support/Seedwell".into(),
    }
}

fn input(tab: SettingsTab, state: SettingsState, live: SettingsLive) -> SettingsViewInput {
    SettingsViewInput { tab, state, live }
}

fn mac(i: SettingsViewInput) -> SettingsPageModel {
    settings_view_model(i, host_windows_all(), host_features_all(), Arc::new(Clock))
}

fn windows(i: SettingsViewInput) -> SettingsPageModel {
    settings_view_model(i, WINDOWS.to_vec(), Vec::new(), Arc::new(Clock))
}

fn groups(m: &SettingsPageModel) -> Vec<&SettingsGroup> {
    m.blocks
        .iter()
        .flat_map(|b| match b {
            SettingsBlock::Single { group } => vec![group],
            SettingsBlock::Pair { left, right } => vec![left, right],
            _ => Vec::new(),
        })
        .collect()
}

fn titles(m: &SettingsPageModel) -> Vec<String> {
    groups(m).iter().map(|g| g.title.clone()).collect()
}

fn find<'a>(m: &'a SettingsPageModel, id: &str) -> &'a SettingsRow {
    groups(m).into_iter().flat_map(|g| g.rows.iter()).find(|r| r.id == id).unwrap_or_else(|| panic!("no row {id}"))
}

fn has_row(m: &SettingsPageModel, id: &str) -> bool {
    groups(m).into_iter().flat_map(|g| g.rows.iter()).any(|r| r.id == id)
}

fn buttons(r: &SettingsRow) -> Vec<String> {
    r.controls
        .iter()
        .filter_map(|c| match c {
            SettingsControl::Button { action: button } => Some(button.title.clone()),
            _ => None,
        })
        .collect()
}

fn texts(r: &SettingsRow) -> Vec<String> {
    r.controls
        .iter()
        .filter_map(|c| match c {
            SettingsControl::Label { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn field_text(r: &SettingsRow, n: usize) -> (String, String) {
    let fields: Vec<_> = r
        .controls
        .iter()
        .filter_map(|c| match c {
            SettingsControl::Field { text, unit, .. } => Some((text.clone(), unit.clone())),
            _ => None,
        })
        .collect();
    fields[n].clone()
}

fn set(key: SettingKey, value: SettingsValue) -> SettingsChange {
    SettingsChange::Set { key, value }
}

fn on(on: bool) -> SettingsValue {
    SettingsValue::Flag { on }
}

fn pick(id: &str) -> SettingsValue {
    SettingsValue::Choice { id: id.into() }
}

fn typed(text: &str) -> SettingsValue {
    SettingsValue::Typed { text: text.into() }
}

fn measure(text: &str, unit: &str) -> SettingsValue {
    SettingsValue::Measure { text: text.into(), unit: unit.into() }
}

// MARK: The tabs

#[test]
fn the_tabs_are_the_ones_the_host_can_draw() {
    let m = mac(input(SettingsTab::Rules, state(), live()));
    let all: Vec<_> = m.tabs.iter().map(|t| t.title.as_str()).collect();
    assert_eq!(all, ["General", "Transfers", "Folders", "Rules", "Network", "Folder Sync", "Devices", "Agents", "Advanced"]);
    assert_eq!(m.tabs[5].help, "Open the Folder Sync settings");
    assert_eq!(m.blocks, [SettingsBlock::RulesEditor]);
    let w = windows(input(SettingsTab::Rules, state(), live()));
    assert!(!w.tabs.iter().any(|t| t.tab == SettingsTab::Rules), "no rules editor on Windows yet");
    assert_eq!(w.tab, SettingsTab::General, "a tab the host lacks shows General");
    assert_eq!(windows(input(SettingsTab::Devices, state(), live())).blocks, [SettingsBlock::Devices]);
}

#[test]
fn the_banner_says_what_is_wrong_and_what_needs_a_restart() {
    let mut l = live();
    l.problems = vec!["config.toml: bad".into()];
    l.notes = vec!["config.toml: bad".into(), "rules.toml: odd".into()];
    l.restart_required = vec!["uTP".into(), "write cache".into()];
    let m = mac(input(SettingsTab::Network, state(), l));
    let b = m.banner.expect("a banner");
    assert_eq!(b.lines.iter().map(|l| (l.text.as_str(), l.tone)).collect::<Vec<_>>(), [
        ("config.toml: bad", SettingsBannerTone::Problem),
        ("rules.toml: odd", SettingsBannerTone::Problem),
        ("Restart Seedwell to apply: uTP, write cache", SettingsBannerTone::Restart),
    ]);
    assert!(b.action.is_none(), "no Open Settings in Settings");
    assert_eq!(m.footer, None, "the version line is General's");
    assert_eq!(m.refresh_ms, 2_000, "Network reads the port and reachability again");
    assert!(mac(input(SettingsTab::Advanced, state(), live())).banner.is_none());
}

// MARK: General

#[test]
fn general_has_updates_startup_notifications_playback_and_power() {
    let m = mac(input(SettingsTab::General, state(), live()));
    assert_eq!(titles(&m), ["Updates", "Startup", "Notifications", "Playback", "Power"]);
    assert_eq!(m.footer.as_deref(), Some("Seedwell 0.1.15"));
    assert_eq!(m.refresh_ms, 60_000, "the updates model's own pace");
    assert_eq!(find(&m, "updates.check").hint.as_deref(), Some("Once a day"));
    assert_eq!(buttons(find(&m, "updates.status")), ["Check Now"]);
    assert!(!has_row(&m, "updates.build"), "no build row while development builds are off");
    let p = find(&m, "playback.player");
    assert_eq!(p.hint.as_deref(), Some("Automatic tries IINA, VLC, then QuickTime Player (MP4 only)"), "the players found, in Automatic's order");
    let SettingsControl::Choice { options, selected, .. } = &p.controls[0] else { panic!() };
    assert_eq!(selected, "auto");
    assert_eq!(options.iter().map(|o| o.title.as_str()).collect::<Vec<_>>(), ["Automatic", "IINA", "VLC", "QuickTime Player"]);
    let SettingsControl::Choice { options, style, .. } = &find(&m, "power.battery").controls[0] else { panic!() };
    assert_eq!(*style, SettingsChoiceStyle::Segmented);
    assert_eq!(options.iter().map(|o| o.title.as_str()).collect::<Vec<_>>(), ["Keep going", "Limit Speed", "Pause downloads"]);

    let w = windows(input(SettingsTab::General, state(), live()));
    assert_eq!(titles(&w), ["Updates", "Startup", "Notifications", "Power"], "no players on Windows yet");
}

#[test]
fn the_players_are_the_cores_choices() {
    let mut s = state();
    s.settings.general.play_with = "org.videolan.vlc".into();
    let mut l = live();
    l.play_with.truncate(2);
    l.play_with.push(PlayWithChoice { value: "org.videolan.vlc".into(), title: "VLC (not installed)".into(), enabled: false, mp4_only: false });
    let m = mac(input(SettingsTab::General, s, l));
    let SettingsControl::Choice { options, selected, .. } = &find(&m, "playback.player").controls[0] else { panic!() };
    assert_eq!(selected, "org.videolan.vlc");
    assert_eq!(options.last().unwrap(), &SettingsOption { id: "org.videolan.vlc".into(), title: "VLC (not installed)".into(), enabled: false });
    assert_eq!(find(&m, "playback.player").hint.as_deref(), Some("Automatic plays with IINA"), "one that is gone is not named");
    let mut l = live();
    l.play_with.truncate(1);
    let m = mac(input(SettingsTab::General, state(), l));
    assert!(find(&m, "playback.player").hint.as_deref().is_some_and(|h| h.starts_with("No player found. Install ")));
}

#[test]
fn the_updates_rows_follow_the_updaters_model() {
    let mut l = live();
    let SettingsUpdates::Core { model } = &mut l.updates else { panic!() };
    model.install = Some(button("Update to 0.1.16", true));
    model.notes_url = Some("https://example.org/notes".into());
    model.show_log = Some(button("Show Log", true));
    model.install_build = Some(button("Install Build…", true));
    model.stop_watching = Some(button("Stop Watching", true));
    model.watching = Some("~/Seedwell/builds".into());
    let mut s = state();
    s.settings.updates.check = false;
    let m = mac(input(SettingsTab::General, s, l));
    let SettingsControl::Toggle { enabled, .. } = &find(&m, "updates.install").controls[0] else { panic!() };
    assert!(!enabled, "installing by itself needs checking on");
    let status = find(&m, "updates.status");
    assert_eq!(buttons(status), ["Check Now", "Update to 0.1.16", "Show Log"]);
    assert!(status.controls.contains(&SettingsControl::Link { title: "Release Notes".into(), url: "https://example.org/notes".into() }));
    let build = find(&m, "updates.build");
    assert_eq!(build.label, "Running 0.1.15");
    assert_eq!(build.hint.as_deref(), Some("Watching ~/Seedwell/builds"));
    assert_eq!(buttons(build), ["Install Build…", "Stop Watching"]);

    let mut l = live();
    l.updates = SettingsUpdates::Sparkle { check: false, install: true, can_check: true, version: "0.1.15".into() };
    let m = mac(input(SettingsTab::General, state(), l));
    assert_eq!(find(&m, "updates.status").hint.as_deref(), Some("Seedwell 0.1.15"));
    let mut l = live();
    l.updates = SettingsUpdates::Off;
    assert_eq!(titles(&mac(input(SettingsTab::General, state(), l)))[0], "Startup");
}

// MARK: Transfers

#[test]
fn transfers_shows_limits_in_their_units_and_the_default_rule() {
    let mut s = state();
    s.settings.transfers.alt_download_limit = 2_000_000;
    s.settings.transfers.alt_upload_limit = 500_000;
    s.settings.transfers.schedule_enabled = false;
    s.settings.transfers.private_min_seed_secs = 36 * 3600;
    s.rules.default_rule.goal = GoalKind::RatioOrTime { ratio: 1.0, secs: 7 * 86_400 };
    let m = mac(input(SettingsTab::Transfers, s, live()));
    assert_eq!(titles(&m), ["Speed", "Queue", "Seeding"]);
    assert_eq!(field_text(find(&m, "speed.down"), 0), (String::new(), "MB/s".into()), "empty is unlimited");
    let alt = find(&m, "speed.alt");
    assert_eq!((field_text(alt, 0), field_text(alt, 1)), (("2".into(), "MB/s".into()), ("500".into(), "KB/s".into())));
    #[cfg(target_os = "macos")]
    assert_eq!(alt.hint.as_deref(), Some("Toggle with ⌥⌘L or Transfers ▸ Limit Speed"));
    let schedule = find(&m, "speed.schedule");
    assert!(schedule.controls[1..].iter().all(|c| match c {
        SettingsControl::Choice { enabled, .. } | SettingsControl::Field { enabled, .. } => !enabled,
        _ => true,
    }), "the days and times wait for the switch");
    let goal = find(&m, "seeding.goal");
    assert_eq!(texts(goal), ["Ratio", "Time"]);
    assert_eq!((field_text(goal, 0), field_text(goal, 1)), (("1.0".into(), String::new()), ("7".into(), "days".into())));
    let SettingsControl::Choice { options, selected, .. } = &find(&m, "seeding.private").controls[1] else { panic!() };
    assert_eq!(selected, "129600");
    assert_eq!(options.last().unwrap().title, "36 hours", "a value from config.toml is listed as it is");
    assert_eq!(options[0].title, "Off");
    let idle = find(&m, "seeding.idle");
    assert_eq!(texts(idle), ["Stop seeding when less than", "uploaded in", "hours"]);
    assert_eq!(field_text(idle, 0), (String::new(), "GB".into()));
    assert_eq!(field_text(idle, 1).0, "150");
    assert_eq!(windows(input(SettingsTab::Transfers, state(), live())).blocks.len(), 3);
}

#[test]
fn a_goal_without_a_ratio_or_time_shows_only_its_choice() {
    let mut s = state();
    s.rules.default_rule.goal = GoalKind::Forever;
    let m = mac(input(SettingsTab::Transfers, s, live()));
    assert_eq!(find(&m, "seeding.goal").controls.len(), 1);
}

// MARK: Folders

#[test]
fn folders_lists_locations_watch_folders_types_and_the_torrent_file() {
    let mut s = state();
    s.settings.folders.watch = vec![WatchFolderSettingFixture::folder("~/Downloads", Some(Category::Tv))];
    s.view.selected_watch = Some(0);
    s.view.expanded_types = vec![Category::Movie];
    let mut l = live();
    l.volume_warnings = vec![VolumeWarning { category: None, incomplete: "/a".into(), finished: "/Volumes/b".into() }, VolumeWarning {
        category: Some(Category::Movie),
        incomplete: "/a".into(),
        finished: "/Volumes/m".into(),
    }];
    let m = mac(input(SettingsTab::Folders, s, l));
    assert_eq!(titles(&m), ["Locations", "Watch folders", "Organize", "Detection", "Folders by content type (optional)", "After adding a .torrent"]);
    assert!(matches!(m.blocks[2], SettingsBlock::Pair { .. }), "Organize and Detection side by side");
    assert_eq!(find(&m, "locations.warning").style, SettingsRowStyle::Warning);
    let watch = find(&m, "watch.0");
    let SettingsRowStyle::Item { icon, detail, selected, menus, .. } = &watch.style else { panic!() };
    assert_eq!(*icon, SettingsIcon::Content { category: Category::Tv });
    assert!(detail.starts_with("Always treat as TV · Ask first"));
    assert!(selected);
    assert_eq!(menus.iter().map(|m| m.title.as_str()).collect::<Vec<_>>(), ["Treat as", "Then"]);
    let g = groups(&m).into_iter().find(|g| g.id == "watch").unwrap();
    let lb = g.list_buttons.as_ref().unwrap();
    assert!(lb.remove.enabled);
    assert_eq!(lb.remove.command, SettingsCommand::Apply { change: SettingsChange::RemoveWatchFolder { index: 0 } });
    let SettingsRowStyle::Disclosure { expanded, status, status_accent, warning_help: warning, children, .. } = &find(&m, "type.movie").style else { panic!() };
    assert!(*expanded && *status_accent);
    assert_eq!(status, "custom folders");
    assert!(warning.is_some());
    assert_eq!(children.iter().map(|r| r.label.as_str()).collect::<Vec<_>>(), ["Incomplete", "Finished", DIFFERENT_DISKS]);
    assert_eq!(children[1].hint.as_deref(), Some("Default: ~/Downloads"));
    let SettingsRowStyle::Disclosure { expanded, status, .. } = &find(&m, "type.tv").style else { panic!() };
    assert!(!expanded);
    assert_eq!(status, "same as above");
    assert!(!has_row(&m, "after.archive"), "the archive folder only with Archive");
    let lookup = find(&m, "detection.lookup");
    assert!(lookup.quiet && matches!(lookup.controls[0], SettingsControl::Toggle { enabled: false, .. }));
}

#[test]
fn no_watch_folder_says_so_and_minus_waits_for_a_selection() {
    let mut s = state();
    s.settings.folders.watch.clear();
    s.settings.folders.torrent_after_add = TorrentFileAction::Archive;
    let m = windows(input(SettingsTab::Folders, s, live()));
    let g = groups(&m).into_iter().find(|g| g.id == "watch").unwrap();
    assert_eq!(g.rows[0].style, SettingsRowStyle::Empty);
    assert!(!g.list_buttons.as_ref().unwrap().remove.enabled);
    assert!(has_row(&m, "after.archive"));
}

struct WatchFolderSettingFixture;
impl WatchFolderSettingFixture {
    fn folder(path: &str, category: Option<Category>) -> crate::settings::WatchFolderSetting {
        crate::settings::WatchFolderSetting {
            path: path.into(),
            enabled: true,
            mode: WatchModeSetting::Ask,
            after_add: TorrentFileAction::Leave,
            archive_dir: None,
            category,
            rule: None,
            since: None,
        }
    }
}

// MARK: Network

#[test]
fn network_shows_the_port_in_use_or_why_it_is_another() {
    let mut s = state();
    s.settings.network.listen_port = 55252;
    let m = mac(input(SettingsTab::Network, s.clone(), live()));
    assert_eq!(titles(&m), ["Connection", "Peers"]);
    // The torrent proxy's box, between them, on every host (its words are the proxy models').
    assert_eq!(m.blocks[1], SettingsBlock::TorrentProxy { title: "Torrent proxy".into() });
    let port = find(&m, "connection.port");
    assert_eq!(field_text(port, 0).0, "55252");
    assert_eq!(texts(port), ["Active: 51413"]);
    assert_eq!(port.hint.as_deref(), Some("Leave empty for Automatic: a random port at every start."));
    assert_eq!(texts(find(&m, "connection.reach")), ["Open"]);
    let mut l = live();
    l.port_fallback = Some(PortFallbackInfo { wanted: 55252, using: 53781, message: "ignored".into() });
    l.reachability = None;
    s.view.port_invalid = true;
    let m = windows(input(SettingsTab::Network, s, l));
    let port = find(&m, "connection.port");
    assert_eq!(texts(port), ["Port 55252 is in use; using 53781"]);
    assert_eq!(port.hint.as_deref(), Some("Use a port from 1024 to 65535, or leave it empty."));
    assert!(!has_row(&m, "connection.reach"));
}

#[test]
fn reachability_is_one_word_per_state() {
    let word = |s: &str| match reachability(s, "x") {
        SettingsControl::Label { text, .. } => text,
        _ => unreachable!(),
    };
    assert_eq!(word("open"), "Open");
    assert_eq!(word("mapped"), "Opened on the router");
    assert_eq!(word("checking"), "Checking");
    assert_eq!(word("closed"), "Closed");
    assert_eq!(word("unknown"), "Not known");
    assert_eq!(word("something new"), "Not known");
}

// MARK: Folder Sync

#[test]
fn folder_sync_has_names_port_discovery_rendezvous_and_the_finder_on_a_mac() {
    let m = mac(input(SettingsTab::FolderSync, state(), live()));
    // What this system calls the computer ("This PC" in a build for Windows).
    assert_eq!(titles(&m), [concat!("This ", trs_platform::computer!()), "Finding devices", "Relaying", "Speed", "Folders", "Finder"]);
    assert!(m.blocks.contains(&SettingsBlock::Rendezvous));
    assert_eq!(m.refresh_ms, 1_000);
    let SettingsControl::Field { placeholder, .. } = &find(&m, "sync.device-name").controls[0] else { panic!() };
    assert_eq!(placeholder, "Kristian's MacBook", "empty is the computer's name");
    assert_eq!(field_text(find(&m, "sync.down"), 0), ("2.5".into(), "MB/s".into()));
    assert_eq!(buttons(find(&m, "sync.badges")), ["Turn On…"]);
    assert_eq!(texts(find(&m, "sync.share-menu")), ["On"]);
    assert_eq!(buttons(find(&m, "sync.share-menu")), ["Manage…"]);

    let mut l = live();
    l.rendezvous = false;
    l.sync_port_moving_to = Some(22200);
    let w = windows(input(SettingsTab::FolderSync, state(), l));
    assert_eq!(titles(&w).last().map(String::as_str), Some("Folders"), "no Finder rows on Windows");
    assert!(!w.blocks.contains(&SettingsBlock::Rendezvous));
    assert_eq!(find(&w, "sync.port").hint.as_deref(), Some("Starting folder sync on port 22200…"));
    let mut s = state();
    s.sync = None;
    assert!(mac(input(SettingsTab::FolderSync, s, live())).blocks.is_empty(), "nothing until sync has settings");
}

// MARK: Agents

#[test]
fn agents_requests_come_first_with_deny_and_approve() {
    let mut l = live();
    l.requests = vec![crate::agent_requests::AgentRequestRow {
        id: "5eed0a11".into(),
        title: "Claude asks for agent permission full".into(),
        detail: "“to install the release” · 4 min ago".into(),
    }];
    let m = mac(input(SettingsTab::Agents, state(), l));
    assert_eq!(titles(&m), ["Requests", "Agent access", "Command line", "Activity"]);
    assert_eq!(m.refresh_ms, 2_000);
    let r = find(&m, "agents.request.5eed0a11");
    assert_eq!((r.label.as_str(), r.hint.as_deref()), ("Claude asks for agent permission full", Some("“to install the release” · 4 min ago")));
    assert_eq!(buttons(r), ["Deny", "Approve"]);
    let commands: Vec<(SettingsCommand, SettingsButtonStyle)> = r
        .controls
        .iter()
        .filter_map(|c| match c {
            SettingsControl::Button { action } => Some((action.command.clone(), action.style)),
            _ => None,
        })
        .collect();
    assert_eq!(
        commands,
        [
            (SettingsCommand::DecideAgentRequest { id: "5eed0a11".into(), approve: false }, SettingsButtonStyle::Normal),
            (SettingsCommand::DecideAgentRequest { id: "5eed0a11".into(), approve: true }, SettingsButtonStyle::Prominent),
        ]
    );
    // None waiting: no group.
    assert_eq!(titles(&mac(input(SettingsTab::Agents, state(), live())))[0], "Agent access");
}

#[test]
fn agents_has_access_the_command_line_and_activity() {
    let mut l = live();
    l.activity = (0..10)
        .map(|n| AgentActivity { at_ms: 1_800_000_000_000 + n, client: "Claude".into(), method: "pause".into(), summary: format!("paused {n}"), outcome: if n == 0 { "denied".into() } else { "ok".into() } })
        .collect();
    let mut full = state();
    full.settings.agents.permission = AgentPermission::Full;
    let m = mac(input(SettingsTab::Agents, full, l));
    assert_eq!(titles(&m), ["Agent access", "Command line", "Activity"]);
    let SettingsControl::Choice { enabled, selected, .. } = &find(&m, "agents.permission").controls[0] else { panic!() };
    assert!(*enabled);
    assert_eq!(selected, "full");
    assert_eq!(buttons(find(&m, "agents.cli")), ["Copy CLI Path", "Install Command-Line Tool…"]);
    assert_eq!(buttons(find(&m, "agents.skill")), ["Install Skill for Claude Code"]);
    let activity = groups(&m).into_iter().find(|g| g.id == "agents.activity").unwrap();
    assert_eq!(activity.rows.len(), 9, "eight actions and Notify me");
    assert_eq!(activity.rows[0].style, SettingsRowStyle::Activity { time: "09:00".into(), client: "Claude".into(), outcome: Some("(denied)".into()) });
    assert!(activity.rows[8].quiet);

    let mut s = state();
    s.settings.agents.permission = AgentPermission::Off;
    s.view.copied = true;
    s.view.cli_note = Some("Linked it.".into());
    let mut l = live();
    l.host.cli = Some("/usr/local/bin/seedwell".into());
    l.host.skill_installed = true;
    let w = windows(input(SettingsTab::Agents, s, l));
    let SettingsControl::Choice { enabled, .. } = &find(&w, "agents.permission").controls[0] else { panic!() };
    assert!(!enabled, "the permission waits for Allow agents");
    let cli = find(&w, "agents.cli");
    assert_eq!(buttons(cli), ["Copied"]);
    assert_eq!(cli.hint.as_deref(), Some("Installed: /usr/local/bin/seedwell"));
    assert_eq!(buttons(find(&w, "agents.skill")), ["Remove Skill", "Update Skill"]);
    assert_eq!(find(&w, "agents.note").label, "Linked it.");
    assert_eq!(find(&w, "activity.empty").style, SettingsRowStyle::Empty);
}

#[test]
fn installing_the_tool_asks_where_first() {
    let m = mac(input(SettingsTab::Agents, state(), live()));
    let Some(SettingsControl::Button { action: button }) = find(&m, "agents.cli").controls.get(1) else { panic!() };
    let SettingsCommand::Ask { question } = &button.command else { panic!() };
    assert_eq!(question.title, "Install the command-line tool?");
    let dirs: Vec<_> = question
        .choices
        .iter()
        .map(|c| match &c.command {
            SettingsCommand::LinkCli { dir, target } => (c.title.as_str(), dir.as_str(), target.as_str()),
            _ => unreachable!(),
        })
        .collect();
    let bundled = "/Applications/Seedwell.app/Contents/MacOS/seedwell";
    assert_eq!(dirs, [("Link in ~/.local/bin", "/Users/me/.local/bin", bundled), ("Link in /usr/local/bin", "/usr/local/bin", bundled)]);
}

// MARK: Advanced

#[test]
fn advanced_has_disk_diagnostics_developer_and_files() {
    let mut s = state();
    s.settings.advanced.developer_mode = true;
    let m = mac(input(SettingsTab::Advanced, s.clone(), live()));
    assert_eq!(titles(&m), ["Disk", "Diagnostics", "Developer", "Files"]);
    assert_eq!(texts(find(&m, "diagnostics.engine")), ["40 connections · 12 wake-ups · 1 rechecks · 0 bad pieces"]);
    #[cfg(target_os = "macos")]
    assert_eq!(buttons(find(&m, "developer.log")), ["View Log…", "Show Log in Finder", "Clear Log"]);
    let w = windows(input(SettingsTab::Advanced, s, live()));
    assert_eq!(buttons(find(&w, "developer.log")).len(), 2, "no log viewer on Windows yet");
    assert!(!has_row(&mac(input(SettingsTab::Advanced, state(), live())), "developer.log"), "the log rows need developer mode");
    let SettingsControl::Button { action: button } = &find(&m, "files.state").controls[0] else { panic!() };
    assert_eq!(button.command, SettingsCommand::ShowInFileManager { path: "/Users/me/Library/Application Support/Seedwell".into() });
}

// MARK: Changes

#[test]
fn a_switch_or_a_choice_changes_its_setting_and_names_the_store() {
    let s = state();
    let a = settings_apply(s.clone(), set(SettingKey::NotifyErrors, on(!s.settings.general.notify_errors)));
    assert_eq!(a.saves, [SettingsStore::Settings]);
    assert_eq!(a.state.settings.general.notify_errors, !s.settings.general.notify_errors);
    let a = settings_apply(s.clone(), set(SettingKey::NotifyErrors, on(s.settings.general.notify_errors)));
    assert!(a.saves.is_empty(), "no change, nothing to save");
    assert_eq!(settings_apply(s.clone(), set(SettingKey::OnBattery, pick("pause"))).state.settings.general.on_battery, OnBatteryMode::Pause);
    assert!(settings_apply(s.clone(), set(SettingKey::OnBattery, pick("nap"))).refused);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::Encryption, pick("require"))).state.settings.network.encryption, EncryptionSetting::Require);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::ScheduleDays, pick("weekends"))).state.settings.transfers.schedule_days, ScheduleDays::Weekends);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::WriteCache, pick("128"))).state.settings.advanced.write_cache_mib, 128);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::PrivateMinSeed, pick("86400"))).state.settings.transfers.private_min_seed_secs, 86_400);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::PlayWith, pick("iina"))).state.settings.general.play_with, "iina");
    let dev = settings_apply(s.clone(), set(SettingKey::DeveloperMode, on(true)));
    assert!(dev.save_now, "the tracker log starts at once");
    let sparkle = settings_apply(s.clone(), set(SettingKey::SparkleCheck, on(false)));
    assert_eq!((sparkle.state, sparkle.saves.len()), (s.clone(), 0), "the Sparkle switches are the host's");
    assert!(settings_apply(s, set(SettingKey::OnlineLookup, on(true))).refused, "not available yet");
}

#[test]
fn a_rate_is_read_in_its_unit_and_the_shown_value_is_no_edit() {
    let mut s = state();
    s.settings.transfers.download_limit = 1_234_567;
    let a = settings_apply(s.clone(), set(SettingKey::DownloadLimit, measure("1.235", "MB/s")));
    assert!(a.saves.is_empty() && !a.refused, "the rounded text of the value in force changes nothing");
    let a = settings_apply(s.clone(), set(SettingKey::DownloadLimit, measure("1.235", "KB/s")));
    assert_eq!(a.state.settings.transfers.download_limit, 1_235, "a new unit is a new value");
    let a = settings_apply(s.clone(), set(SettingKey::UploadLimit, measure("", "MB/s")));
    assert!(a.saves.is_empty(), "already unlimited");
    let a = settings_apply(s.clone(), set(SettingKey::UploadLimit, measure("2,5", "MB/s")));
    assert_eq!(a.state.settings.transfers.upload_limit, 2_500_000);
    let a = settings_apply(s.clone(), set(SettingKey::UploadLimit, measure("fast", "MB/s")));
    assert!(a.refused && a.saves.is_empty());
    let a = settings_apply(s, set(SettingKey::SyncUpLimit, measure("500", "KB/s")));
    assert_eq!((a.state.sync.unwrap().up_limit, a.saves), (500_000, vec![SettingsStore::Sync]));
}

#[test]
fn changing_a_unit_converts_the_number_and_keeps_the_value() {
    use SettingsFieldKind::{Duration, Rate, Size};
    let convert = |k, t: &str, from: &str, to: &str| settings_field_convert(k, t.into(), from.into(), to.into());
    // 2 h shown as days is 0.1 days, not 2 days (the regression: the number stayed and the value became 2 days).
    assert_eq!(convert(Duration, "2", "hours", "days"), "0.1");
    assert_eq!(convert(Duration, "36", "hours", "days"), "1.5");
    assert_eq!(convert(Duration, "1.5", "days", "hours"), "36");
    assert_eq!(convert(Duration, "7", "days", "hours"), "168");
    assert_eq!(convert(Rate, "1.5", "MB/s", "KB/s"), "1500");
    assert_eq!(convert(Rate, "250", "KB/s", "MB/s"), "0.25");
    assert_eq!(convert(Size, "0,5", "GB", "MB"), "500");
    assert_eq!(convert(Size, "", "GB", "MB"), "", "empty (off) stays empty");
    assert_eq!(convert(Duration, "soon", "hours", "days"), "soon", "unreadable text stays for the field to refuse");
    assert_eq!(convert(Duration, "2", "hours", "hours"), "2");

    // The converted text sent with the new unit is the value in force: nothing changes, nothing is saved.
    let mut s = state();
    s.rules.default_rule.goal = GoalKind::Time { secs: 2 * 3600 };
    let a = settings_apply(s.clone(), set(SettingKey::DefaultGoalTime, measure("0.1", "days")));
    assert!(a.saves.is_empty() && !a.refused, "a unit change alone keeps 2 hours");
    assert_eq!(a.state.rules.default_rule.goal, GoalKind::Time { secs: 2 * 3600 });
    assert_eq!(settings_field_read(Duration, "0.1".into(), "days".into(), 2 * 3600), Some(2 * 3600));
    s.settings.transfers.download_limit = 1_500_000;
    let a = settings_apply(s.clone(), set(SettingKey::DownloadLimit, measure("1500", "KB/s")));
    assert!(a.saves.is_empty() && !a.refused);
    // A number typed in the new unit is a new value.
    let a = settings_apply(s, set(SettingKey::DefaultGoalTime, measure("3", "days")));
    assert_eq!(a.state.rules.default_rule.goal, GoalKind::Time { secs: 3 * 86_400 });
}

#[test]
fn numbers_times_sizes_and_hours_are_read_or_refused() {
    let s = state();
    assert_eq!(settings_apply(s.clone(), set(SettingKey::MaxConnections, typed("1,000"))).state.settings.network.max_connections, 1000);
    assert!(settings_apply(s.clone(), set(SettingKey::MaxConnections, typed("lots"))).refused);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::ScheduleFrom, typed("07:30"))).state.settings.transfers.schedule_from_minute, 450);
    assert!(settings_apply(s.clone(), set(SettingKey::ScheduleTo, typed("25:00"))).refused);
    let a = settings_apply(s.clone(), set(SettingKey::IdleMinUpload, measure("0,5", "GB")));
    assert_eq!(a.state.settings.transfers.idle_min_upload_bytes, 500_000_000);
    assert!(settings_apply(s.clone(), set(SettingKey::IdleMinUpload, measure("-1", "GB"))).refused);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::IdleWindowHours, typed("72"))).state.settings.transfers.idle_window_hours, 72);
    assert!(settings_apply(s, set(SettingKey::IdleWindowHours, typed("0"))).refused);
}

#[test]
fn the_default_goal_is_a_rules_change() {
    let mut s = state();
    s.rules.default_rule.goal = GoalKind::RatioOnly { ratio: 1.0 };
    let a = settings_apply(s.clone(), set(SettingKey::DefaultGoal, pick("either")));
    assert_eq!(a.saves, [SettingsStore::Rules]);
    assert_eq!(a.state.rules.default_rule.goal, GoalKind::RatioOrTime { ratio: 1.0, secs: 7 * 86_400 });
    let a = settings_apply(a.state, set(SettingKey::DefaultGoalRatio, typed("2,5")));
    assert_eq!(a.state.rules.default_rule.goal, GoalKind::RatioOrTime { ratio: 2.5, secs: 7 * 86_400 });
    let a = settings_apply(a.state, set(SettingKey::DefaultGoalTime, measure("36", "hours")));
    assert_eq!(a.state.rules.default_rule.goal, GoalKind::RatioOrTime { ratio: 2.5, secs: 36 * 3600 });
    let a = settings_apply(a.state, set(SettingKey::DefaultOnGoal, pick("remove-keep-files")));
    assert_eq!(a.state.rules.default_rule.on_goal, OnGoalAction::RemoveKeepFiles);
    assert!(settings_apply(s, set(SettingKey::DefaultGoalRatio, typed("much"))).refused);
}

#[test]
fn the_listening_port_is_automatic_manual_or_refused_with_a_hint() {
    let s = state();
    let a = settings_apply(s.clone(), set(SettingKey::ListenPort, typed("55252")));
    assert_eq!((a.state.settings.network.listen_port, a.state.view.port_invalid), (55252, false));
    let b = settings_apply(a.state.clone(), set(SettingKey::ListenPort, typed("80")));
    assert!(b.refused && b.state.view.port_invalid);
    assert_eq!(b.state.settings.network.listen_port, 55252, "the port in force stays");
    let c = settings_apply(b.state, set(SettingKey::ListenPort, typed("")));
    assert_eq!((c.state.settings.network.listen_port, c.state.view.port_invalid), (0, false), "empty is Automatic");
}

#[test]
fn the_sync_port_moves_now_and_a_refusal_is_its_hint() {
    let s = state();
    let a = settings_apply(s.clone(), set(SettingKey::SyncPort, typed("22200")));
    assert_eq!((a.sync_port, a.saves.len()), (Some(22200), 0), "the core moves sync; nothing to save");
    assert_eq!(settings_apply(s.clone(), set(SettingKey::SyncPort, typed("22100"))).sync_port, None, "the port in use");
    assert!(settings_apply(s.clone(), set(SettingKey::SyncPort, typed(""))).refused, "empty goes back to the port in use");
    let bad = settings_apply(s.clone(), set(SettingKey::SyncPort, typed("99999")));
    assert_eq!(bad.state.view.sync_port_error.as_deref(), Some("Use a port from 1024 to 65535."));
    let refused = settings_apply(bad.state, SettingsChange::SyncPortRefused { message: "Port 22200 is in use.".into() });
    let m = mac(input(SettingsTab::FolderSync, refused.state.clone(), live()));
    assert_eq!(find(&m, "sync.port").hint.as_deref(), Some("Port 22200 is in use."));
    let again = settings_apply(refused.state, set(SettingKey::SyncPort, typed("22300")));
    assert_eq!(again.state.view.sync_port_error, None);
}

#[test]
fn names_are_trimmed_and_short_and_sync_keys_wait_for_sync() {
    let s = state();
    let long = format!("  {}  ", "x".repeat(80));
    let a = settings_apply(s.clone(), set(SettingKey::DeviceName, typed(&long)));
    assert_eq!(a.state.sync.as_ref().unwrap().device_name, "x".repeat(64));
    assert_eq!(a.saves, [SettingsStore::Sync]);
    assert!(!settings_apply(s.clone(), set(SettingKey::LanDiscovery, on(false))).state.sync.unwrap().lan_discovery);
    assert_eq!(settings_apply(s.clone(), set(SettingKey::SyncLocation, typed("~/Shared"))).state.sync.unwrap().default_location, "~/Shared");
    let mut none = s;
    none.sync = None;
    assert!(settings_apply(none.clone(), set(SettingKey::UserName, typed("Anna"))).refused);
    assert!(settings_apply(none, set(SettingKey::SyncPort, typed("22200"))).refused);
}

#[test]
fn folders_and_watch_folders_change_in_place() {
    let s = state();
    let a = settings_apply(s.clone(), set(SettingKey::DownloadDir, typed("~/Media")));
    assert_eq!(a.state.settings.folders.download_dir, "~/Media");
    let a = settings_apply(s.clone(), set(SettingKey::TypeFinishedDir { category: Category::Movie }, typed("")));
    assert_eq!(a.state.settings.folders.categories[0].finished_dir, None, "empty is the default");
    let a = settings_apply(s.clone(), set(SettingKey::ArchiveDir, typed("~/Archive")));
    assert_eq!(a.state.settings.folders.archive_dir.as_deref(), Some("~/Archive"));

    let mut s = s;
    s.settings.folders.watch.clear();
    let a = settings_apply(s, SettingsChange::AddWatchFolder { path: "~/Downloads".into() });
    assert_eq!(a.state.settings.folders.watch.len(), 1);
    assert_eq!(a.state.view.selected_watch, Some(0), "the new folder is selected");
    let w = &a.state.settings.folders.watch[0];
    assert_eq!((w.enabled, w.mode, w.after_add), (true, WatchModeSetting::Ask, TorrentFileAction::Leave));
    assert!(settings_apply(a.state.clone(), SettingsChange::AddWatchFolder { path: "~/Downloads".into() }).refused, "once only");
    let b = settings_apply(a.state.clone(), set(SettingKey::WatchMode { index: 0 }, pick("auto")));
    assert_eq!(b.state.settings.folders.watch[0].mode, WatchModeSetting::Auto);
    let b = settings_apply(b.state, set(SettingKey::WatchCategory { index: 0 }, pick("tv")));
    assert_eq!(b.state.settings.folders.watch[0].category, Some(Category::Tv));
    let b = settings_apply(b.state, set(SettingKey::WatchCategory { index: 0 }, pick("detect")));
    assert_eq!(b.state.settings.folders.watch[0].category, None);
    let b = settings_apply(b.state, set(SettingKey::WatchThen { index: 0 }, pick("trash")));
    assert_eq!(b.state.settings.folders.watch[0].after_add, TorrentFileAction::Trash);
    let b = settings_apply(b.state, set(SettingKey::WatchEnabled { index: 0 }, on(false)));
    assert!(!b.state.settings.folders.watch[0].enabled);
    assert!(settings_apply(b.state.clone(), set(SettingKey::WatchEnabled { index: 3 }, on(false))).refused);
    let c = settings_apply(b.state.clone(), SettingsChange::SelectWatch { index: 0 });
    assert_eq!(c.state.view.selected_watch, None, "a second click unselects");
    let d = settings_apply(b.state, SettingsChange::RemoveWatchFolder { index: 0 });
    assert!(d.state.settings.folders.watch.is_empty() && d.state.view.selected_watch.is_none());
    assert_eq!(d.saves, [SettingsStore::Settings]);
    let e = settings_apply(d.state, SettingsChange::ToggleType { category: Category::Music });
    assert_eq!(e.state.view.expanded_types, [Category::Music]);
    assert!(settings_apply(e.state, SettingsChange::ToggleType { category: Category::Music }).state.view.expanded_types.is_empty());
}

#[test]
fn allowing_agents_gives_control_and_the_permission_waits_for_it() {
    let mut s = state();
    s.settings.agents.permission = AgentPermission::Off;
    assert!(settings_apply(s.clone(), set(SettingKey::AgentPermission, pick("full"))).refused);
    let a = settings_apply(s, set(SettingKey::AllowAgents, on(true)));
    assert_eq!(a.state.settings.agents.permission, AgentPermission::Control);
    let b = settings_apply(a.state, set(SettingKey::AgentPermission, pick("read-only")));
    assert_eq!(b.state.settings.agents.permission, AgentPermission::ReadOnly);
    assert!(settings_apply(b.state.clone(), set(SettingKey::AllowAgents, on(true))).saves.is_empty(), "already allowed");
    assert_eq!(settings_apply(b.state, set(SettingKey::AllowAgents, on(false))).state.settings.agents.permission, AgentPermission::Off);
}

#[test]
fn the_command_line_and_skill_results_become_the_note() {
    let s = state();
    let note = |c| settings_apply(s.clone(), c).state.view.cli_note.unwrap();
    assert_eq!(
        note(SettingsChange::CliLinked { link: "/Users/me/.local/bin/seedwell".into(), dir: "/Users/me/.local/bin".into(), on_path: true }),
        "Linked /Users/me/.local/bin/seedwell. Open a new terminal window and type seedwell help."
    );
    assert_eq!(
        note(SettingsChange::CliLinked { link: "/x/seedwell".into(), dir: "/x".into(), on_path: false }),
        "Linked /x/seedwell. Add /x to your PATH to type seedwell without its full path."
    );
    assert_eq!(note(SettingsChange::CliLinkExists { path: "/usr/local/bin/seedwell".into() }), "/usr/local/bin/seedwell already exists and points somewhere else, so it was left alone.");
    assert_eq!(
        note(SettingsChange::CliLinkFailed { dir: "/usr/local/bin".into(), target: "/A/seedwell".into() }),
        "Could not link into /usr/local/bin. In a terminal: sudo ln -s '/A/seedwell' /usr/local/bin/seedwell"
    );
    assert_eq!(note(SettingsChange::SkillInstalled { dir: "/d".into() }), "Skill written to /d. Claude Code uses it in new sessions.");
    assert_eq!(note(SettingsChange::SkillFailed { dir: "/d".into(), error: " disk full\n".into() }), "Could not install the skill: disk full");
    assert_eq!(note(SettingsChange::SkillRemoved { dir: "/d".into() }), "Skill removed from /d.");
    assert_eq!(note(SettingsChange::SkillRemoveFailed { dir: "/d".into(), error: "busy".into() }), "Could not remove the skill: busy");
    let changed = settings_apply(s.clone(), SettingsChange::SkillFailed { dir: "/d".into(), error: "files changed; add --force".into() });
    let q = changed.ask.expect("asks before replacing changed files");
    assert_eq!(q.title, "Replace the changed skill files?");
    assert_eq!(q.choices[0].command, SettingsCommand::InstallSkill { dir: "/d".into(), force: true });
    assert_eq!(changed.state.view.cli_note, None);
    let copied = settings_apply(s, SettingsChange::Copied { on: true });
    assert!(copied.state.view.copied);
    // Each system's form: its home as it writes it, the rest joined with its separator.
    let home = if cfg!(windows) { r"C:\Users\me\" } else { "/Users/me/" };
    let want = if cfg!(windows) { r"C:\Users\me\.claude\skills\seedwell" } else { "/Users/me/.claude/skills/seedwell" };
    assert_eq!(skill_dir(home), want);
}
