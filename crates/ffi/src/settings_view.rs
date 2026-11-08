//! The Settings window as a view model every host draws with one generic renderer (docs/CROSS-PLATFORM.md §1): its
//! tabs, and for the tab shown its groups of rows, each row's label, hint and typed controls (switches, choices,
//! fields with units, folders, buttons, links, lines of text, list items), what is enabled, the problems banner and
//! the version line. Rust decides every word, which rows there are (and leaves out what the host cannot do:
//! [`HostWindow`], [`HostFeature`]) and what each change does ([`settings_apply`]); the host draws, keeps the
//! [`SettingsState`] it gets back, saves the stores it names and runs the [`SettingsCommand`]s.
//!
//! - [`settings_view_model`]: the window for a tab.
//! - [`settings_apply`] (settings_view_apply.rs): a change to the state, and what to save.
//! - [`settings_field_valid`] (settings_parse.rs): a field checked while it is typed.
//!
//! Some parts are drawn by views of their own, which the model only places ([`SettingsBlock`]): the Cloudflare
//! rendezvous (`RendezvousView`), Devices (`devices_view`) and the rules and trackers editors.

use seedwell_platform::file_manager;

use crate::host::{has, HostFeature, HostWindow};
use crate::library::LocalClock;
use crate::row_text::category_label;
use crate::settings::{
    AgentPermission, AppSettings, AskMode, EncryptionSetting, OnBatteryMode, OnGoalAction, PreallocationMode, RulesDoc, ScheduleDays,
    TorrentFileAction, VolumeWarning, WatchModeSetting,
};
use crate::settings_parse::{self as p, SettingsFieldKind};
use crate::settings_rules::RulesChange;
use crate::settings_text::{torrent_file_action_label, watch_folder_detail, watch_then_label};
use crate::sync::SyncSettings;
use crate::types::{AgentActivity, Category, DiagnosticsInfo, PortFallbackInfo, ReachabilityInfo};
use crate::notify::{settings_banner, SettingsBanner};
use crate::player::PlayWithChoice;
use crate::updates::UpdatesModel;
use crate::menu::APP_NAME;

/// Every content type, in the lists' order.
pub(crate) const CATEGORIES: [Category; 8] =
    [Category::Movie, Category::Tv, Category::Music, Category::Audiobook, Category::Book, Category::Software, Category::Game, Category::Other];

// MARK: The records

/// A tab of the Settings window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SettingsTab {
    General,
    Transfers,
    Folders,
    Rules,
    Network,
    FolderSync,
    Devices,
    Agents,
    Advanced,
}

/// A tab's button: its title and tooltip (the host picks its image by `tab`).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsTabItem {
    pub tab: SettingsTab,
    pub title: String,
    pub help: String,
}

/// A setting a control edits: what the host sends back with the new value ([`SettingsChange::Set`]).
#[derive(Clone, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SettingKey {
    // General
    LaunchAtLogin,
    NotifyFinished,
    NotifyErrors,
    NotifyGoal,
    PlayWith,
    PreventSleep,
    KeepAwakeForAgents,
    OnBattery,
    UpdatesCheck,
    UpdatesAutoInstall,
    UpdatesDevBuilds,
    /// The Sparkle updater's own switches (a Mac build with a Sparkle feed): the host sets them itself;
    /// `settings_apply` leaves the state as it is.
    SparkleCheck,
    SparkleInstall,
    // Transfers
    DownloadLimit,
    UploadLimit,
    AltDownloadLimit,
    AltUploadLimit,
    ScheduleEnabled,
    ScheduleDays,
    ScheduleFrom,
    ScheduleTo,
    MaxActiveDownloads,
    MaxConnectionsPerHost,
    StartDelay,
    AskBeforeAdding,
    LargeTorrentGb,
    /// The Default rule's goal, its ratio and its time (`rules.toml`).
    DefaultGoal,
    DefaultGoalRatio,
    DefaultGoalTime,
    DefaultOnGoal,
    PrivateMinSeed,
    IdleMinUpload,
    IdleWindowHours,
    // Folders
    IncompleteDir,
    DownloadDir,
    ByStatus,
    SkipJunk,
    AskWhenUnsure,
    OnlineLookup,
    TorrentAfterAdd,
    ArchiveDir,
    WatchMode { index: u32 },
    WatchEnabled { index: u32 },
    WatchCategory { index: u32 },
    WatchThen { index: u32 },
    TypeIncompleteDir { category: Category },
    TypeFinishedDir { category: Category },
    // Network
    ListenPort,
    PortMapping,
    AllowLanTrackers,
    Dht,
    Pex,
    Encryption,
    Utp,
    MaxConnections,
    MaxConnectionsPerTorrent,
    // Folder Sync (`sync.toml`)
    DeviceName,
    UserName,
    SyncPort,
    LanDiscovery,
    DhtDiscovery,
    RelayOwnDevices,
    RelayEveryone,
    SyncDownLimit,
    SyncUpLimit,
    SyncLocation,
    // Agents
    AllowAgents,
    AgentPermission,
    AgentNotify,
    // Advanced
    WriteCache,
    Preallocation,
    VerifyOnCompletion,
    DeveloperMode,
    // The rules editor (`rules_editor_apply`): the rule shown, or the Default rule.
    RuleName,
    /// A tracker ticked or unticked (`Choice` with its id).
    RuleTrackers,
    /// A content type ticked or unticked (`Choice` with its id; `any` clears them).
    RuleCategories,
    RulePrivate,
    RuleEnabled,
    /// Same as Default, or a goal of its own.
    RuleGoalMode,
    RuleGoal,
    RuleGoalRatio,
    RuleGoalTime,
    RuleMinSeed,
    RuleMinSeedTime,
    /// Same as Settings, Off, or a condition of its own.
    RuleIdleMode,
    RuleIdleSize,
    RuleIdleHours,
    RuleOnGoal,
    RulePriority,
    RuleLocation,
    /// Default, Unlimited, or a limit.
    RuleLimitMode,
    RuleLimit,
    // The trackers editor.
    TrackerName,
    TrackerRule,
    TrackerIncomplete,
    TrackerFinished,
    TrackerTypeIncomplete { category: Category },
    TrackerTypeFinished { category: Category },
    TrackerHidden,
    TrackerHideOnlySeeding,
    TrackerAddress { index: u32 },
}

/// A new value from a control.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsValue {
    /// A switch.
    Flag { on: bool },
    /// A choice: the [`SettingsOption::id`] picked.
    Choice { id: String },
    /// A field without a unit, as typed (a folder chosen with Choose…, too).
    Typed { text: String },
    /// A field with a unit: the text and the unit picked beside it (one of [`SettingsControl::Field`]'s `units`).
    Measure { text: String, unit: String },
}

/// How a choice is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsChoiceStyle {
    /// A pop-up menu.
    Menu,
    /// A segmented control (a few short choices).
    Segmented,
}

/// One choice.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsOption {
    pub id: String,
    pub title: String,
    pub enabled: bool,
}

/// How a button is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsButtonStyle {
    Normal,
    /// Drawn as a link (text in the accent colour).
    Link,
    /// The default button of its place (drawn prominent).
    Prominent,
    /// A plain plus and the title in the accent colour ("Add Address").
    Add,
    /// Only a minus in a circle; the title is its tooltip.
    Remove,
}

/// One entry of a checklist or a row of chips: ticked or not.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsCheck {
    pub id: String,
    pub title: String,
    pub checked: bool,
}

/// A part of a line of text, bold or not ("Applies to **44 torrents**.").
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsTextRun {
    pub text: String,
    pub bold: bool,
}

/// What a button does, or what the host does before a change (a folder picker).
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsCommand {
    /// A change of the Settings state itself: `settings_apply(state, change)`.
    Apply { change: SettingsChange },
    /// A change in the rules and trackers editors: `rules_editor_apply(input, change)`.
    Rules { change: RulesChange },
    /// Opens a tracker's tab in the window, with all of its torrents (a hidden tracker's "Show Torrents").
    ShowTrackerTorrents { tracker_id: String },
    /// A folder picker that starts at `start` (`~` expanded); the folder chosen is sent back as
    /// `Set { key, Text { text } }` (with `~` for the home folder).
    ChooseFolder { key: SettingKey, start: String },
    /// A folder picker for a new watch folder; the folder chosen is sent back as [`SettingsChange::AddWatchFolder`].
    ChooseWatchFolder,
    /// The updater (Settings › General › Updates): check now, install the version on offer, show the failed install's
    /// log, pick a build's zip to install (development builds), stop watching the builds folder.
    CheckForUpdates,
    InstallUpdate,
    ShowUpdateLog,
    InstallBuild,
    StopWatchingBuilds,
    /// Opens `url` in the browser.
    OpenUrl { url: String },
    /// Copies `text` to the clipboard, then sends [`SettingsChange::Copied`] `on`, and again `off` after
    /// [`SettingsPageModel::copied_ms`].
    CopyCliPath { text: String },
    /// Asks `question` first; the choice picked runs its command.
    Ask { question: SettingsQuestion },
    /// Links the command-line tool into `dir` (`dir/seedwell` pointing at `target`), then sends `CliLinked`,
    /// `CliLinkExists` or `CliLinkFailed`.
    LinkCli { dir: String, target: String },
    /// Runs the command-line tool's `skill install --dir <dir>` (with `--force` when `force`), then sends
    /// `SkillInstalled` or `SkillFailed`.
    InstallSkill { dir: String, force: bool },
    /// Runs `skill uninstall --dir <dir>`, then sends `SkillRemoved` or `SkillRemoveFailed`.
    RemoveSkill { dir: String },
    /// Shows `path` in the file manager.
    ShowInFileManager { path: String },
    /// The tracker log (Settings › Advanced › Developer): its viewer (`HostWindow::TrackerLog`), its file in the file
    /// manager (`Core::tracker_log_path`; before the first exchange its folder, else the state folder), Clear
    /// (`Core::tracker_log_clear`).
    ViewTrackerLog,
    ShowTrackerLog,
    ClearTrackerLog,
    /// The Mac's Finder extension and Share menu: their list in System Settings, and how to switch them on.
    ManageFinderExtension,
    ManageShareMenu,
    ShowFinderSetup,
    /// The user's answer to an agent's request for access (Settings › Agents › Requests): the host awaits
    /// `Core::agent_request_decide(id, approve)`, may show its answer, and reads the requests again.
    DecideAgentRequest { id: String, approve: bool },
}

/// A question the host asks before a command: the title, what it does, the choices (each one runs its command) and
/// Cancel.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsQuestion {
    pub title: String,
    pub text: String,
    pub choices: Vec<SettingsQuestionChoice>,
    pub cancel_title: String,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsQuestionChoice {
    pub title: String,
    pub command: SettingsCommand,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsButton {
    pub title: String,
    pub help: String,
    pub enabled: bool,
    pub style: SettingsButtonStyle,
    pub command: SettingsCommand,
    /// For tests and QA.
    pub identifier: Option<String>,
}

/// The colour of a line of text or of a symbol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsTone {
    Primary,
    Secondary,
    Accent,
    /// Orange.
    Warning,
    /// Red.
    Problem,
    /// Green.
    Good,
}

/// A symbol before a line of text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsSymbol {
    /// A check mark in a circle (open, on).
    Check,
    /// A port the router opened.
    Mapped,
    /// Still finding out.
    Checking,
    /// A cross in a circle (closed).
    Closed,
    /// A question mark in a circle (not known).
    Unknown,
    /// A warning triangle.
    Warning,
}

/// A control of a row, left to right.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsControl {
    /// A switch, or (`checkbox`) a checkbox.
    Toggle {
        key: SettingKey,
        on: bool,
        enabled: bool,
        help: String,
        checkbox: bool,
    },
    Choice {
        key: SettingKey,
        style: SettingsChoiceStyle,
        options: Vec<SettingsOption>,
        /// The option in force; none of them when it is not one of `options` (a segmented control with nothing
        /// selected).
        selected: String,
        enabled: bool,
        help: Option<String>,
    },
    /// A text field, with a unit menu when `units` is not empty. The host keeps what is typed and the unit picked
    /// while the field has focus, and sends `Measure` (or `Text`) on Return, when it loses focus, when the unit
    /// changes, and (`live`) after a pause in typing; while it is typed, text that `settings_field_valid` refuses is
    /// drawn in the error colour. A refused value puts the field's text back.
    Field {
        key: SettingKey,
        kind: SettingsFieldKind,
        text: String,
        placeholder: String,
        units: Vec<String>,
        unit: String,
        /// Points.
        width: u32,
        /// Right-aligned text (numbers with a unit).
        trailing: bool,
        live: bool,
        enabled: bool,
        help: Option<String>,
        identifier: Option<String>,
    },
    /// A folder: its path (edited in place, sent on every change), Choose… and, `clearable`, a button that empties it
    /// (back to the default, `placeholder`).
    Folder {
        key: SettingKey,
        path: String,
        placeholder: String,
        width: u32,
        clearable: bool,
        choose: SettingsButton,
        help: String,
    },
    Button {
        action: SettingsButton,
    },
    /// A pop-up button titled `title` whose menu ticks and unticks entries; a click sends `Choice` with the entry's id.
    Checklist {
        key: SettingKey,
        title: String,
        checks: Vec<SettingsCheck>,
    },
    /// A row of chips that are on or off; a click sends `Choice` with the chip's id. `help_on` and `help_off` are their
    /// tooltips, with `{}` for the chip's title.
    Chips {
        key: SettingKey,
        checks: Vec<SettingsCheck>,
        help_on: String,
        help_off: String,
    },
    Link {
        title: String,
        url: String,
    },
    /// A line of text (a symbol before it, when it has one).
    Label {
        text: String,
        tone: SettingsTone,
        symbol: Option<SettingsSymbol>,
        /// A smaller font: a status beside a control.
        small: bool,
        /// Digits of the same width (counters).
        monospaced_digits: bool,
        identifier: Option<String>,
    },
}

/// The image of a list item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsIcon {
    /// A content type's image.
    Content { category: Category },
    /// A watch folder that detects the content type.
    WatchFolder,
}

/// A line of a context menu.
// A view model crosses the FFI by value: boxing a variant would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsMenuEntry {
    Item { title: String, command: SettingsCommand, checked: bool },
    Separator,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsMenu {
    pub title: String,
    pub entries: Vec<SettingsMenuEntry>,
}

/// How a row is drawn.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsRowStyle {
    /// The label on the left, the controls beside it, the hint on the right.
    Standard,
    /// The label over its hint on the left, the controls on the right.
    Described,
    /// The label across the row, its one switch on the right.
    Wide,
    /// Only the label, a quiet paragraph.
    Note,
    /// The label as the quiet line a list shows while it is empty.
    Empty,
    /// The label after a warning triangle, orange.
    Warning,
    /// An item of a list (a watch folder): its image, the label in bold over `detail`, the controls on the right; a
    /// click sends `select`, a right-click shows `menus` (submenus).
    Item { icon: SettingsIcon, detail: String, selected: bool, select: Option<SettingsCommand>, menus: Vec<SettingsMenu> },
    /// An agent's action: the time, the client in bold, the label, and how it went in the error colour when it failed.
    Activity { time: String, client: String, outcome: Option<String> },
    /// A row that opens to more rows (a content type): a disclosure triangle (`toggle`), its image, the label, `status`
    /// on the right (accent: it sets something of its own) and a warning symbol with `warning_help` as its tooltip
    /// (not `warning`: C# nests the variants, and a field named like the `Warning` variant would clash).
    Disclosure {
        icon: SettingsIcon,
        expanded: bool,
        toggle: SettingsCommand,
        toggle_help: String,
        status: String,
        status_accent: bool,
        warning_help: Option<String>,
        children: Vec<SettingsRow>,
    },
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsRow {
    /// Stable within the tab (for tests and QA).
    pub id: String,
    pub label: String,
    pub hint: Option<String>,
    pub style: SettingsRowStyle,
    /// The whole row quiet (secondary colour).
    pub quiet: bool,
    pub controls: Vec<SettingsControl>,
}

/// Buttons under a list: + and − (drawn as symbols; `title` is their tooltip).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsListButtons {
    pub add: SettingsButton,
    pub remove: SettingsButton,
}

/// A titled box of rows.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsGroup {
    pub id: String,
    pub title: String,
    pub rows: Vec<SettingsRow>,
    pub list_buttons: Option<SettingsListButtons>,
}

/// A part of a tab, top to bottom.
// A view model crosses the FFI by value: boxing a variant would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsBlock {
    Single { group: SettingsGroup },
    /// Two groups side by side, of the same height.
    Pair { left: SettingsGroup, right: SettingsGroup },
    /// Folder Sync's Cloudflare rendezvous, drawn from `Core::sync_rendezvous` (`RendezvousView`).
    Rendezvous,
    /// Network's torrent proxy (SPEC §7H): a box titled `title` with the shield, drawn from `Core::proxy_model`
    /// (`ProxyModel`: the status line, the problem) and the key line of `Core::proxy_open_modal_model`; the key itself
    /// is never in either.
    TorrentProxy { title: String },
    /// The Devices tab, drawn from `Core::devices_view`.
    Devices,
    /// The rules and trackers editors.
    RulesEditor,
}

/// The Settings window, for the tab shown.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsPageModel {
    pub tabs: Vec<SettingsTabItem>,
    /// The tab shown: the one asked for, or General when the host lacks it.
    pub tab: SettingsTab,
    pub blocks: Vec<SettingsBlock>,
    /// A quiet, selectable line under the tab ("Seedwell 0.1.15 (build 1202)").
    pub footer: Option<String>,
    /// Problems with the settings, and what applies after a restart (`settings_banner`, in Settings).
    pub banner: Option<SettingsBanner>,
    /// While the tab is shown, read the live part of the input again this often (0: no need).
    pub refresh_ms: u32,
    /// How long "Copied" stays.
    pub copied_ms: u32,
}

// MARK: The state and the input

/// The window's own state, which lives as long as it is open: what is selected and open, and the notes a change
/// left. `settings_view_state()` makes a new one.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsViewState {
    /// The watch folder selected (the − button removes it).
    pub selected_watch: Option<u32>,
    /// The content types opened in Folders.
    pub expanded_types: Vec<Category>,
    /// The listening port typed last was refused.
    pub port_invalid: bool,
    /// Why folder sync's port was refused (by `settings_apply`, or the core: [`SettingsChange::SyncPortRefused`]).
    pub sync_port_error: Option<String>,
    /// What the last command-line tool or skill action did.
    pub cli_note: Option<String>,
    /// "Copied" shows on Copy CLI Path.
    pub copied: bool,
}

/// What Settings edits, and the window's state: what the host keeps between changes.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsState {
    /// `Core::settings_load`.
    pub settings: AppSettings,
    /// `Core::rules_load` (Transfers edits the Default rule).
    pub rules: RulesDoc,
    /// `Core::sync_settings` (`None` while folder sync has none).
    pub sync: Option<SyncSettings>,
    pub view: SettingsViewState,
}

/// What the updates group shows.
// A view model crosses the FFI by value: boxing a variant would buy nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsUpdates {
    /// No updater.
    Off,
    /// The core's updater (`Updates::model`).
    Core { model: UpdatesModel },
    /// The Sparkle updater of a Mac build with a Sparkle feed: its own switches and Check Now.
    Sparkle { check: bool, install: bool, can_check: bool, version: String },
}

/// The command-line tool and the skill, as the host finds them.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsCli {
    /// The home folder (where `~/.local/bin` and the skill folder are).
    pub home: String,
    /// The tool inside the app.
    pub bundled: String,
}

/// What the host reads for the tab shown, again every `refresh_ms`.
#[derive(Clone, Debug, uniffi::Record)]
pub struct SettingsLive {
    /// Why the last save or a rejected edit was refused (shown in the banner).
    pub problems: Vec<String>,
    /// `Core::settings_problems`: what loading the settings files found.
    pub notes: Vec<String>,
    /// Settings changed on disk that apply after a restart.
    pub restart_required: Vec<String>,
    /// `Core::volume_warnings`.
    pub volume_warnings: Vec<VolumeWarning>,
    /// `Core::listen_port`, `Core::port_fallback`, `Core::reachability`.
    pub active_port: u16,
    pub port_fallback: Option<PortFallbackInfo>,
    pub reachability: Option<ReachabilityInfo>,
    /// `Core::sync_reachability`, and the port folder sync is moving to after a change (until it is there).
    pub sync_reachability: Option<ReachabilityInfo>,
    pub sync_port_moving_to: Option<u16>,
    /// `Core::sync_rendezvous` has a view (the rendezvous section is drawn).
    pub rendezvous: bool,
    /// `Core::diagnostics`.
    pub diagnostics: DiagnosticsInfo,
    /// `Core::agent_activity`, newest first.
    pub activity: Vec<AgentActivity>,
    /// `Core::agent_requests(now).rows`: agents' requests for access waiting for the user.
    pub requests: Vec<crate::agent_requests::AgentRequestRow>,
    pub updates: SettingsUpdates,
    /// "Play videos with"'s choices (`Core::play_with_choices` of the setting; only with `HostWindow::Player`).
    pub play_with: Vec<PlayWithChoice>,
    pub cli: SettingsCli,
    /// `Core::settings_host_facts`: the command-line tool and the skill as the core found them, the version line.
    pub host: crate::settings_view_host::SettingsHostFacts,
    /// The Finder extension and the Share menu are switched on (the Mac).
    pub finder_on: bool,
    pub share_menu_on: bool,
    /// `Core::state_dir`.
    pub state_dir: String,
}

/// What the window is built from.
#[derive(Clone, Debug, uniffi::Record)]
pub struct SettingsViewInput {
    pub tab: SettingsTab,
    pub state: SettingsState,
    pub live: SettingsLive,
}

/// A new window's state: nothing selected, nothing open, no notes.
#[uniffi::export]
pub fn settings_view_state() -> SettingsViewState {
    SettingsViewState { selected_watch: None, expanded_types: Vec::new(), port_invalid: false, sync_port_error: None, cli_note: None, copied: false }
}

// MARK: The changes

/// A change of the Settings state ([`settings_apply`]).
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum SettingsChange {
    /// A control's new value.
    Set { key: SettingKey, value: SettingsValue },
    /// A watch folder clicked: selected, or no longer.
    SelectWatch { index: u32 },
    /// A new watch folder (from [`SettingsCommand::ChooseWatchFolder`]).
    AddWatchFolder { path: String },
    RemoveWatchFolder { index: u32 },
    /// A content type opened or closed.
    ToggleType { category: Category },
    /// The core refused folder sync's new port (`Core::set_sync_port`): why.
    SyncPortRefused { message: String },
    /// "Copied" on, and off again.
    Copied { on: bool },
    /// What [`SettingsCommand::LinkCli`] did: linked `link` in `dir` (`on_path`: `dir` is on the PATH), or
    /// `path` is there already and points elsewhere, or it failed.
    CliLinked { link: String, dir: String, on_path: bool },
    CliLinkExists { path: String },
    CliLinkFailed { dir: String, target: String },
    /// What [`SettingsCommand::InstallSkill`] and [`SettingsCommand::RemoveSkill`] did (`error`: the tool's error
    /// output).
    SkillInstalled { dir: String },
    SkillFailed { dir: String, error: String },
    SkillRemoved { dir: String },
    SkillRemoveFailed { dir: String, error: String },
}

/// Where a change is saved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SettingsStore {
    /// `Core::settings_save(state.settings)`, a moment after the last change.
    Settings,
    /// `Core::rules_save(state.rules)`, at once.
    Rules,
    /// `Core::set_sync_settings(state.sync)`, a moment after the last change.
    Sync,
}

/// What [`settings_apply`] made of a change.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct SettingsApplied {
    pub state: SettingsState,
    /// The stores to save (none when nothing changed).
    pub saves: Vec<SettingsStore>,
    /// Save `Settings` now rather than after a moment (Developer mode: its log starts at once).
    pub save_now: bool,
    /// Move folder sync to this port now: `Core::set_sync_port` (a refusal comes back as `SyncPortRefused`).
    pub sync_port: Option<u16>,
    /// The value was refused: the field shows the value in force again.
    pub refused: bool,
    /// Ask this first (the skill files were changed since they were written).
    pub ask: Option<SettingsQuestion>,
}

// MARK: Building it

pub(crate) fn tab_title(tab: SettingsTab) -> &'static str {
    match tab {
        SettingsTab::General => "General",
        SettingsTab::Transfers => "Transfers",
        SettingsTab::Folders => "Folders",
        SettingsTab::Rules => "Rules",
        SettingsTab::Network => "Network",
        SettingsTab::FolderSync => "Folder Sync",
        SettingsTab::Devices => "Devices",
        SettingsTab::Agents => "Agents",
        SettingsTab::Advanced => "Advanced",
    }
}

const TABS: [SettingsTab; 9] = [
    SettingsTab::General,
    SettingsTab::Transfers,
    SettingsTab::Folders,
    SettingsTab::Rules,
    SettingsTab::Network,
    SettingsTab::FolderSync,
    SettingsTab::Devices,
    SettingsTab::Agents,
    SettingsTab::Advanced,
];

/// The tabs `host` can draw: Rules needs the rules editor.
fn tabs(host: &[HostWindow]) -> Vec<SettingsTabItem> {
    TABS.iter()
        .filter(|t| **t != SettingsTab::Rules || has(host, Some(HostWindow::RulesEditor)))
        .map(|&tab| SettingsTabItem { tab, title: tab_title(tab).into(), help: format!("Open the {} settings", tab_title(tab)) })
        .collect()
}

pub(crate) fn row(id: &str, label: impl Into<String>, hint: Option<&str>, controls: Vec<SettingsControl>) -> SettingsRow {
    SettingsRow { id: id.into(), label: label.into(), hint: hint.map(Into::into), style: SettingsRowStyle::Standard, quiet: false, controls }
}

fn styled(id: &str, label: impl Into<String>, style: SettingsRowStyle, controls: Vec<SettingsControl>) -> SettingsRow {
    SettingsRow { id: id.into(), label: label.into(), hint: None, style, quiet: false, controls }
}

pub(crate) fn group(id: &str, title: &str, rows: Vec<SettingsRow>) -> SettingsGroup {
    SettingsGroup { id: id.into(), title: title.into(), rows, list_buttons: None }
}

fn block(g: SettingsGroup) -> SettingsBlock {
    SettingsBlock::Single { group: g }
}

// A control's parts, so one can be built from another with struct update syntax (an enum variant has none).

pub(crate) struct ToggleSpec {
    pub key: SettingKey,
    pub on: bool,
    pub enabled: bool,
    pub help: String,
    pub checkbox: bool,
}

impl From<ToggleSpec> for SettingsControl {
    fn from(t: ToggleSpec) -> Self {
        SettingsControl::Toggle { key: t.key, on: t.on, enabled: t.enabled, help: t.help, checkbox: t.checkbox }
    }
}

pub(crate) struct ChoiceSpec {
    pub key: SettingKey,
    pub style: SettingsChoiceStyle,
    pub options: Vec<SettingsOption>,
    pub selected: String,
    pub enabled: bool,
    pub help: Option<String>,
}

impl From<ChoiceSpec> for SettingsControl {
    fn from(c: ChoiceSpec) -> Self {
        SettingsControl::Choice { key: c.key, style: c.style, options: c.options, selected: c.selected, enabled: c.enabled, help: c.help }
    }
}

pub(crate) struct FieldSpec {
    pub key: SettingKey,
    pub kind: SettingsFieldKind,
    pub text: String,
    pub placeholder: String,
    pub units: Vec<String>,
    pub unit: String,
    pub width: u32,
    pub trailing: bool,
    pub live: bool,
    pub enabled: bool,
    pub help: Option<String>,
    pub identifier: Option<String>,
}

impl From<FieldSpec> for SettingsControl {
    fn from(f: FieldSpec) -> Self {
        SettingsControl::Field {
            key: f.key,
            kind: f.kind,
            text: f.text,
            placeholder: f.placeholder,
            units: f.units,
            unit: f.unit,
            width: f.width,
            trailing: f.trailing,
            live: f.live,
            enabled: f.enabled,
            help: f.help,
            identifier: f.identifier,
        }
    }
}

/// "On, click to turn off".
pub(crate) fn switch_spec(key: SettingKey, on: bool) -> ToggleSpec {
    ToggleSpec { key, on, enabled: true, help: if on { "On, click to turn off" } else { "Off, click to turn on" }.into(), checkbox: false }
}

pub(crate) fn switch(key: SettingKey, on: bool) -> SettingsControl {
    switch_spec(key, on).into()
}

fn switch_help(key: SettingKey, on: bool, help: &str) -> SettingsControl {
    ToggleSpec { help: help.into(), ..switch_spec(key, on) }.into()
}

fn option(id: &str, title: &str) -> SettingsOption {
    SettingsOption { id: id.into(), title: title.into(), enabled: true }
}

pub(crate) fn choice_spec(key: SettingKey, style: SettingsChoiceStyle, options: Vec<SettingsOption>, selected: &str) -> ChoiceSpec {
    ChoiceSpec { key, style, options, selected: selected.into(), enabled: true, help: None }
}

pub(crate) fn choice(key: SettingKey, style: SettingsChoiceStyle, options: Vec<SettingsOption>, selected: &str) -> SettingsControl {
    choice_spec(key, style, options, selected).into()
}

pub(crate) fn text(t: impl Into<String>) -> SettingsControl {
    SettingsControl::Label { text: t.into(), tone: SettingsTone::Primary, symbol: None, small: false, monospaced_digits: false, identifier: None }
}

pub(crate) fn quiet_text(t: impl Into<String>) -> SettingsControl {
    SettingsControl::Label { text: t.into(), tone: SettingsTone::Secondary, symbol: None, small: false, monospaced_digits: false, identifier: None }
}

pub(crate) fn button(title: impl Into<String>, help: impl Into<String>, command: SettingsCommand) -> SettingsButton {
    SettingsButton { title: title.into(), help: help.into(), enabled: true, style: SettingsButtonStyle::Normal, command, identifier: None }
}

fn button_control(b: SettingsButton) -> SettingsControl {
    SettingsControl::Button { action: b }
}

fn apply(change: SettingsChange) -> SettingsCommand {
    SettingsCommand::Apply { change }
}

fn set(key: SettingKey, value: SettingsValue) -> SettingsCommand {
    apply(SettingsChange::Set { key, value })
}

/// A plain field.
fn field(key: SettingKey, kind: SettingsFieldKind, text: String, width: u32) -> SettingsControl {
    field_spec(key, kind, text, width).into()
}

pub(crate) fn field_spec(key: SettingKey, kind: SettingsFieldKind, text: String, width: u32) -> FieldSpec {
    FieldSpec {
        key,
        kind,
        text,
        placeholder: String::new(),
        units: Vec::new(),
        unit: String::new(),
        width,
        trailing: false,
        live: false,
        enabled: true,
        help: None,
        identifier: None,
    }
}

/// A whole number.
fn number(key: SettingKey, n: u64) -> SettingsControl {
    field(key, SettingsFieldKind::Number, p::whole_text(n), 62)
}

/// A speed limit: the number, its unit, "Unlimited" when empty. Applies after a pause in typing.
pub(crate) fn rate(key: SettingKey, bytes: u64, width: u32) -> SettingsControl {
    let unit = p::rate_unit(bytes);
    SettingsControl::Field {
        key,
        kind: SettingsFieldKind::Rate,
        text: p::rate_number(bytes, unit),
        placeholder: "Unlimited".into(),
        units: p::RATE_UNITS.iter().map(|u| u.to_string()).collect(),
        unit: unit.into(),
        width,
        trailing: true,
        live: true,
        enabled: true,
        help: Some("Leave empty for no limit".into()),
        identifier: None,
    }
}

fn time(key: SettingKey, minute: u32, enabled: bool) -> SettingsControl {
    FieldSpec { enabled, ..field_spec(key, SettingsFieldKind::Time, p::time_text(minute), 58) }.into()
}

/// A folder with Choose….
pub(crate) fn folder(key: SettingKey, path: &str, placeholder: &str, width: u32, clearable: bool) -> SettingsControl {
    let start = if path.is_empty() { "~".to_string() } else { path.to_string() };
    SettingsControl::Folder {
        key: key.clone(),
        path: path.into(),
        placeholder: placeholder.into(),
        width,
        clearable,
        choose: button("Choose…", "Pick a folder", SettingsCommand::ChooseFolder { key, start }),
        help: if clearable { "A folder for this content type, or empty for the default" } else { "The folder's path" }.into(),
    }
}

// MARK: General

const LIMIT_SPEED_HINT: &str = concat!("Toggle with ", seedwell_platform::shortcut!(alt "L"), " or Transfers ▸ Limit Speed");

fn updates_group(u: &SettingsUpdates, s: &AppSettings) -> Option<SettingsGroup> {
    let link = |b: &crate::updates::UpdateButton, command| SettingsButton {
        style: SettingsButtonStyle::Link,
        ..SettingsButton { enabled: b.enabled, ..button(&b.title, &b.help, command) }
    };
    let plain = |b: &crate::updates::UpdateButton, command| SettingsButton { enabled: b.enabled, ..button(&b.title, &b.help, command) };
    match u {
        SettingsUpdates::Off => None,
        SettingsUpdates::Core { model: m } => {
            let mut rows = vec![
                row("updates.check", &m.check_label, Some(&m.check_hint), vec![ToggleSpec {
                    enabled: m.switches_enabled,
                    ..switch_spec(SettingKey::UpdatesCheck, s.updates.check)
                }.into()]),
                row("updates.install", &m.install_label, Some(&m.install_hint), vec![ToggleSpec {
                    enabled: m.switches_enabled && s.updates.check,
                    ..switch_spec(SettingKey::UpdatesAutoInstall, s.updates.auto_install)
                }.into()]),
            ];
            let mut status = vec![button_control(plain(&m.check_now, SettingsCommand::CheckForUpdates))];
            if let Some(b) = &m.install {
                status.push(button_control(plain(b, SettingsCommand::InstallUpdate)));
            }
            if let Some(url) = &m.notes_url {
                status.push(SettingsControl::Link { title: m.notes_title.clone(), url: url.clone() });
            }
            if let Some(b) = &m.show_log {
                status.push(button_control(link(b, SettingsCommand::ShowUpdateLog)));
            }
            rows.push(row("updates.status", &m.status_label, Some(&m.status), status));
            rows.push(row("updates.dev", &m.dev_label, Some(&m.dev_hint), vec![switch(SettingKey::UpdatesDevBuilds, s.updates.dev_builds)]));
            if let Some(b) = &m.install_build {
                let mut controls = vec![button_control(plain(b, SettingsCommand::InstallBuild))];
                if let Some(stop) = &m.stop_watching {
                    controls.push(button_control(plain(stop, SettingsCommand::StopWatchingBuilds)));
                }
                let watching = m.watching.as_ref().map(|w| format!("Watching {w}"));
                rows.push(row("updates.build", format!("Running {}", m.version), watching.as_deref(), controls));
            }
            Some(group("updates", "Updates", rows))
        }
        SettingsUpdates::Sparkle { check, install, can_check, version } => Some(group(
            "updates",
            "Updates",
            vec![
                row("updates.check", "Check automatically", Some("Once a day"), vec![switch(SettingKey::SparkleCheck, *check)]),
                row("updates.install", "Install automatically", Some("When nothing is downloading"), vec![ToggleSpec {
                    enabled: *check,
                    ..switch_spec(SettingKey::SparkleInstall, *install)
                }.into()]),
                row("updates.status", "Check for updates", Some(&format!("{APP_NAME} {version}")), vec![button_control(SettingsButton {
                    enabled: *can_check,
                    ..button("Check Now", format!("Look for a newer version of {APP_NAME} now"), SettingsCommand::CheckForUpdates)
                })]),
            ],
        )),
    }
}

fn players_choice(i: &SettingsViewInput) -> SettingsControl {
    let options = i.live.play_with.iter().map(|c| SettingsOption { id: c.value.clone(), title: c.title.clone(), enabled: c.enabled }).collect();
    choice(SettingKey::PlayWith, SettingsChoiceStyle::Menu, options, &i.state.settings.general.play_with)
}

fn general(i: &SettingsViewInput, host: &[HostWindow]) -> Vec<SettingsBlock> {
    let g = &i.state.settings.general;
    let mut out: Vec<SettingsBlock> = updates_group(&i.live.updates, &i.state.settings).into_iter().map(block).collect();
    out.push(block(group("startup", "Startup", vec![row("startup.login", "Launch at login", None, vec![switch(SettingKey::LaunchAtLogin, g.launch_at_login)])])));
    out.push(block(group(
        "notifications",
        "Notifications",
        vec![
            row("notify.finished", "Download finished", None, vec![switch(SettingKey::NotifyFinished, g.notify_finished)]),
            row("notify.errors", "Errors", None, vec![switch(SettingKey::NotifyErrors, g.notify_errors)]),
            row("notify.goal", "Seeding goal reached", None, vec![switch(SettingKey::NotifyGoal, g.notify_goal)]),
        ],
    )));
    if has(host, Some(HostWindow::Player)) {
        out.push(block(group(
            "playback",
            "Playback",
            vec![row("playback.player", "Play videos with", Some(&crate::player::automatic_hint(&i.live.play_with)), vec![players_choice(i)])],
        )));
    }
    let battery = match g.on_battery {
        OnBatteryMode::KeepGoing => "keep-going",
        OnBatteryMode::Limit => "limit",
        OnBatteryMode::Pause => "pause",
    };
    out.push(block(group(
        "power",
        "Power",
        vec![
            row("power.sleep", "Prevent sleep while downloading", None, vec![switch(SettingKey::PreventSleep, g.prevent_sleep_while_downloading)]),
            row("power.agents", "Keep awake while agents work", Some("The screen still locks. Agents ask with seedwell awake"), vec![switch(
                SettingKey::KeepAwakeForAgents,
                g.keep_awake_for_agents,
            )]),
            row("power.battery", "On battery", Some("Applies the Limit Speed settings, or pauses downloads"), vec![choice(
                SettingKey::OnBattery,
                SettingsChoiceStyle::Segmented,
                vec![option("keep-going", "Keep going"), option("limit", "Limit Speed"), option("pause", "Pause downloads")],
                battery,
            )]),
        ],
    )));
    out
}

// MARK: Transfers

pub(crate) fn goal_controls(goal_key: SettingKey, ratio_key: SettingKey, time_key: SettingKey, goal: &crate::settings::GoalKind) -> Vec<SettingsControl> {
    let mut out = vec![choice(goal_key, SettingsChoiceStyle::Menu, p::GOAL_CHOICES.iter().map(|(id, t)| option(id, t)).collect(), p::goal_id(goal))];
    if p::goal_has_ratio(goal) {
        out.push(quiet_text("Ratio"));
        out.push(field(ratio_key, SettingsFieldKind::Ratio, p::ratio_text(p::goal_ratio(goal)), 58));
    }
    if p::goal_has_time(goal) {
        out.push(quiet_text("Time"));
        out.push(duration(time_key, p::goal_secs(goal)));
    }
    out
}

/// A seeding goal's controls for an editor the host still draws itself (a rule's goal): the choice, then the ratio and
/// the time it has. Their keys are `DefaultGoal`, `DefaultGoalRatio` and `DefaultGoalTime`; `settings_goal_apply`
/// reads what they send.
#[uniffi::export]
pub fn settings_goal_controls(goal: crate::settings::GoalKind) -> Vec<SettingsControl> {
    goal_controls(SettingKey::DefaultGoal, SettingKey::DefaultGoalRatio, SettingKey::DefaultGoalTime, &goal)
}

/// The inactive-seed condition's controls for an editor the host still draws itself (a rule's), after `prefix`. The
/// keys are `IdleMinUpload` (a `Size` field) and `IdleWindowHours` (`Hours`); `settings_field_read` reads them.
#[uniffi::export]
pub fn settings_idle_controls(prefix: Option<String>, bytes: u64, hours: u32) -> Vec<SettingsControl> {
    idle_controls(prefix.as_deref(), SettingKey::IdleMinUpload, SettingKey::IdleWindowHours, bytes, hours)
}

/// A new rule as the rules editor starts it: on, any torrent, everything else the Default rule's.
#[uniffi::export]
pub fn settings_blank_rule() -> crate::settings::RuleRecord {
    crate::settings::RuleRecord {
        id: String::new(),
        name: String::new(),
        enabled: true,
        tracker_ids: Vec::new(),
        private_match: crate::settings::PrivateMatch::Any,
        categories: Vec::new(),
        goal: None,
        min_seed_secs: None,
        priority: None,
        on_goal: None,
        finished_location: None,
        up_limit: None,
        idle_below_bytes: None,
        idle_hours: p::IDLE_HOURS_DEFAULT,
    }
}

/// A speed limit's field (key `UploadLimit`) for a page the host still draws itself (a tracker's Info page).
#[uniffi::export]
pub fn settings_rate_control(bytes: u64, width: u32) -> SettingsControl {
    rate(SettingKey::UploadLimit, bytes, width)
}

/// A duration's field (hours or days, key `DefaultGoalTime`), for a rule's minimum seed time.
#[uniffi::export]
pub fn settings_duration_control(secs: u64) -> SettingsControl {
    duration(SettingKey::DefaultGoalTime, secs)
}

/// A number of hours or days.
pub(crate) fn duration(key: SettingKey, secs: u64) -> SettingsControl {
    let unit = p::duration_unit(secs);
    FieldSpec {
        units: p::DURATION_UNITS.iter().map(|u| u.to_string()).collect(),
        unit: unit.into(),
        ..field_spec(key, SettingsFieldKind::Duration, p::duration_number(secs, unit), 58)
    }.into()
}

pub(crate) fn on_goal_id(a: OnGoalAction) -> &'static str {
    match a {
        OnGoalAction::StopKeep => "stop-keep",
        OnGoalAction::RemoveKeepFiles => "remove-keep-files",
        OnGoalAction::RemoveDeleteFiles => "remove-delete-files",
    }
}

/// The inactive-seed condition: "[prefix] [1] [GB] uploaded in [150] hours".
pub(crate) fn idle_controls(prefix: Option<&str>, size_key: SettingKey, hours_key: SettingKey, bytes: u64, hours: u32) -> Vec<SettingsControl> {
    let unit = p::size_unit(bytes);
    let mut out: Vec<SettingsControl> = prefix.map(text).into_iter().collect();
    out.push(FieldSpec {
        placeholder: "Off".into(),
        units: p::SIZE_UNITS.iter().map(|u| u.to_string()).collect(),
        unit: unit.into(),
        trailing: true,
        help: Some("Leave empty to keep seeding however little is uploaded".into()),
        ..field_spec(size_key, SettingsFieldKind::Size, p::size_number(bytes, unit), 62)
    }.into());
    out.push(text("uploaded in"));
    out.push(FieldSpec {
        placeholder: p::IDLE_HOURS_DEFAULT.to_string(),
        trailing: true,
        help: Some("Hours spent seeding. Time the torrent is paused or the app is closed does not count.".into()),
        ..field_spec(hours_key, SettingsFieldKind::Hours, hours.to_string(), 54)
    }.into());
    out.push(text("hours"));
    out
}

fn transfers(i: &SettingsViewInput) -> Vec<SettingsBlock> {
    let t = &i.state.settings.transfers;
    let d = &i.state.rules.default_rule;
    let on = t.schedule_enabled;
    let days = match t.schedule_days {
        ScheduleDays::Weekends => "weekends",
        ScheduleDays::EveryDay => "every-day",
        _ => "weekdays",
    };
    let speed = group(
        "speed",
        "Speed",
        vec![
            row("speed.down", "Download limit", Some("Shared by torrents and direct downloads. Empty is no limit."), vec![rate(SettingKey::DownloadLimit, t.download_limit, 76)]),
            row("speed.up", "Upload limit", Some("A favorite tracker can have its own, in its Info page"), vec![rate(SettingKey::UploadLimit, t.upload_limit, 76)]),
            row("speed.alt", "Limit Speed (alt.)", Some(LIMIT_SPEED_HINT), vec![
                text("↓"),
                rate(SettingKey::AltDownloadLimit, t.alt_download_limit, 58),
                text("↑"),
                rate(SettingKey::AltUploadLimit, t.alt_upload_limit, 58),
            ]),
            row("speed.schedule", "Schedule", Some("Limit Speed turns on in this window"), vec![
                switch(SettingKey::ScheduleEnabled, on),
                ChoiceSpec {
                    enabled: on,
                    ..choice_spec(
                        SettingKey::ScheduleDays,
                        SettingsChoiceStyle::Segmented,
                        vec![option("weekdays", "Weekdays"), option("weekends", "Weekends"), option("every-day", "Every day")],
                        days,
                    )
                }.into(),
                time(SettingKey::ScheduleFrom, t.schedule_from_minute, on),
                quiet_text("–"),
                time(SettingKey::ScheduleTo, t.schedule_to_minute, on),
            ]),
        ],
    );
    let ask = match t.ask_before_adding {
        AskMode::Never => "never",
        AskMode::MultiFile => "multi-file",
        AskMode::Always => "always",
    };
    let queue = group(
        "queue",
        "Queue",
        vec![
            row("queue.active", "Active downloads", Some("Torrents and direct downloads together.\nThe rest wait in Up Next."), vec![number(
                SettingKey::MaxActiveDownloads,
                t.max_active_downloads.into(),
            )]),
            row("queue.per-host", "Connections per host", Some("Direct downloads. Polite to servers."), vec![number(
                SettingKey::MaxConnectionsPerHost,
                t.max_connections_per_host.into(),
            )]),
            row("queue.delay", "Wait before starting", Some("A new torrent is held, silent, while you can look. 0 starts at once."), vec![
                number(SettingKey::StartDelay, t.start_delay_secs.into()),
                text("s"),
            ]),
            row("queue.ask", "Ask before adding", None, vec![choice(
                SettingKey::AskBeforeAdding,
                SettingsChoiceStyle::Menu,
                vec![option("never", "Never"), option("multi-file", "Multi-file torrents only"), option("always", "Always")],
                ask,
            )]),
            row("queue.large", "Ask above", Some("Torrents larger than this show the chooser first. 0 = never."), vec![
                number(SettingKey::LargeTorrentGb, t.large_torrent_gb.into()),
                text("GB"),
            ]),
        ],
    );
    let floor = t.private_min_seed_secs;
    let mut floors: Vec<SettingsOption> = p::PRIVATE_FLOOR_DAYS.iter().map(|d| d * 86_400).map(|s| option(&s.to_string(), &p::private_floor_label(s))).collect();
    if !floors.iter().any(|o| o.id == floor.to_string()) {
        floors.push(option(&floor.to_string(), &p::private_floor_label(floor)));
    }
    let seeding = group(
        "seeding",
        "Seeding",
        vec![
            row(
                "seeding.goal",
                "Default goal",
                Some("The Default rule. Tracker rules override it."),
                goal_controls(SettingKey::DefaultGoal, SettingKey::DefaultGoalRatio, SettingKey::DefaultGoalTime, &d.goal),
            ),
            row("seeding.on-goal", "When a goal is reached", None, vec![choice(
                SettingKey::DefaultOnGoal,
                SettingsChoiceStyle::Menu,
                vec![option("stop-keep", "Stop, keep in list"), option("remove-keep-files", "Remove, keep files")],
                on_goal_id(d.on_goal),
            )]),
            row("seeding.private", "Private torrents", Some("Hit-and-run protection. A tracker rule’s own minimum wins."), vec![
                text("Seed at least"),
                ChoiceSpec {
                    help: Some(
                        "How long a private torrent keeps seeding before its goal can stop it. Off lets the goal decide alone. Stopping or removing by hand always works."
                            .into(),
                    ),
                    ..choice_spec(SettingKey::PrivateMinSeed, SettingsChoiceStyle::Menu, floors, &floor.to_string())
                }.into(),
            ]),
            row(
                "seeding.idle",
                "Inactive seeds",
                Some("Only hours spent seeding count. Empty = off."),
                idle_controls(Some("Stop seeding when less than"), SettingKey::IdleMinUpload, SettingKey::IdleWindowHours, t.idle_min_upload_bytes, t.idle_window_hours),
            ),
        ],
    );
    vec![block(speed), block(queue), block(seeding)]
}

// MARK: Folders

pub(crate) const DIFFERENT_DISKS: &str = "On different disks: finishing a download copies the files instead of an instant move.";
pub(crate) const DIFFERENT_DISKS_SHORT: &str = "On different disks: finishing a download copies the files";

/// The warning under a pair of locations on different disks (SPEC §7A.4), for editors the host still draws (a
/// tracker's folders): the line, or (`brief`) the tooltip of its symbol.
#[uniffi::export]
pub fn settings_different_disks(brief: bool) -> String {
    if brief { DIFFERENT_DISKS_SHORT } else { DIFFERENT_DISKS }.into()
}

pub(crate) fn warning_row(id: &str) -> SettingsRow {
    styled(id, DIFFERENT_DISKS, SettingsRowStyle::Warning, Vec::new())
}

pub(crate) fn category_id(c: Category) -> &'static str {
    match c {
        Category::Movie => "movie",
        Category::Tv => "tv",
        Category::Music => "music",
        Category::Audiobook => "audiobook",
        Category::Book => "book",
        Category::Software => "software",
        Category::Game => "game",
        Category::Other => "other",
    }
}

pub(crate) fn category_from_id(id: &str) -> Option<Category> {
    CATEGORIES.into_iter().find(|c| category_id(*c) == id)
}

fn watch_rows(i: &SettingsViewInput) -> Vec<SettingsRow> {
    let f = &i.state.settings.folders;
    if f.watch.is_empty() {
        return vec![styled("watch.empty", "No folder is watched. Add one to pick up new .torrent files by themselves.", SettingsRowStyle::Empty, Vec::new())];
    }
    f.watch
        .iter()
        .enumerate()
        .map(|(n, w)| {
            let index = n as u32;
            let mut treat = vec![
                SettingsMenuEntry::Item {
                    title: "Detect the content type".into(),
                    command: set(SettingKey::WatchCategory { index }, SettingsValue::Choice { id: "detect".into() }),
                    checked: w.category.is_none(),
                },
                SettingsMenuEntry::Separator,
            ];
            treat.extend(CATEGORIES.iter().map(|&c| SettingsMenuEntry::Item {
                title: category_label(c),
                command: set(SettingKey::WatchCategory { index }, SettingsValue::Choice { id: category_id(c).into() }),
                checked: w.category == Some(c),
            }));
            let then = [TorrentFileAction::Trash, TorrentFileAction::Leave]
                .iter()
                .map(|&a| SettingsMenuEntry::Item {
                    title: watch_then_label(a),
                    command: set(SettingKey::WatchThen { index }, SettingsValue::Choice { id: action_id(a).into() }),
                    checked: w.after_add == a,
                })
                .collect();
            let mode = match w.mode {
                WatchModeSetting::Auto => "auto",
                WatchModeSetting::Ask => "ask",
            };
            SettingsRow {
                id: format!("watch.{n}"),
                label: w.path.clone(),
                hint: None,
                style: SettingsRowStyle::Item {
                    icon: w.category.map_or(SettingsIcon::WatchFolder, |category| SettingsIcon::Content { category }),
                    detail: watch_folder_detail(w.clone()),
                    selected: i.state.view.selected_watch == Some(index),
                    select: Some(apply(SettingsChange::SelectWatch { index })),
                    menus: vec![SettingsMenu { title: "Treat as".into(), entries: treat }, SettingsMenu { title: "Then".into(), entries: then }],
                },
                quiet: false,
                controls: vec![
                    ChoiceSpec {
                        help: Some("Add new .torrent files from this folder by themselves, or ask first".into()),
                        ..choice_spec(SettingKey::WatchMode { index }, SettingsChoiceStyle::Menu, vec![option("auto", "Auto-add"), option("ask", "Ask first")], mode)
                    }.into(),
                    switch_help(SettingKey::WatchEnabled { index }, w.enabled, "Turn watching this folder on or off"),
                ],
            }
        })
        .collect()
}

pub(crate) fn action_id(a: TorrentFileAction) -> &'static str {
    match a {
        TorrentFileAction::Trash => "trash",
        TorrentFileAction::Archive => "archive",
        TorrentFileAction::Leave => "leave",
    }
}

fn type_rows(i: &SettingsViewInput) -> Vec<SettingsRow> {
    let f = &i.state.settings.folders;
    f.categories
        .iter()
        .map(|o| {
            let c = o.category;
            let warned = i.live.volume_warnings.iter().any(|w| w.category == Some(c));
            let expanded = i.state.view.expanded_types.contains(&c);
            let mut children = if expanded {
                vec![
                    row("incomplete", "Incomplete", Some(&format!("Default: {}", f.incomplete_dir)), vec![folder(
                        SettingKey::TypeIncompleteDir { category: c },
                        o.incomplete_dir.as_deref().unwrap_or(""),
                        "Default",
                        220,
                        true,
                    )]),
                    row("finished", "Finished", Some(&format!("Default: {}", f.download_dir)), vec![folder(
                        SettingKey::TypeFinishedDir { category: c },
                        o.finished_dir.as_deref().unwrap_or(""),
                        "Default",
                        220,
                        true,
                    )]),
                ]
            } else {
                Vec::new()
            };
            if expanded && warned {
                children.push(warning_row("warning"));
            }
            let own = o.incomplete_dir.is_some() || o.finished_dir.is_some();
            SettingsRow {
                id: format!("type.{}", category_id(c)),
                label: category_label(c),
                hint: None,
                style: SettingsRowStyle::Disclosure {
                    icon: SettingsIcon::Content { category: c },
                    expanded,
                    toggle: apply(SettingsChange::ToggleType { category: c }),
                    toggle_help: format!("Folders for {}", category_label(c)),
                    status: if own { "custom folders" } else { "same as above" }.into(),
                    status_accent: own,
                    warning_help: warned.then(|| DIFFERENT_DISKS_SHORT.into()),
                    children,
                },
                quiet: false,
                controls: Vec::new(),
            }
        })
        .collect()
}

fn folders(i: &SettingsViewInput) -> Vec<SettingsBlock> {
    let f = &i.state.settings.folders;
    let mut locations = vec![
        row("locations.incomplete", "Incomplete", Some("Where a download is written until it finishes. Same disk = instant move when done."), vec![folder(
            SettingKey::IncompleteDir,
            &f.incomplete_dir,
            "",
            230,
            false,
        )]),
        row("locations.finished", "Finished", Some("Where finished files live and are seeded from. One copy only."), vec![folder(
            SettingKey::DownloadDir,
            &f.download_dir,
            "",
            230,
            false,
        )]),
    ];
    if i.live.volume_warnings.iter().any(|w| w.category.is_none()) {
        locations.push(warning_row("locations.warning"));
    }
    let selected = i.state.view.selected_watch.filter(|s| (*s as usize) < f.watch.len());
    let watch = SettingsGroup {
        list_buttons: Some(SettingsListButtons {
            add: button("Add a folder to watch", "Add a folder to watch", SettingsCommand::ChooseWatchFolder),
            remove: SettingsButton {
                enabled: selected.is_some(),
                ..button("Stop watching the selected folder", "Stop watching the selected folder", apply(SettingsChange::RemoveWatchFolder { index: selected.unwrap_or(0) }))
            },
        }),
        ..group("watch", "Watch folders", watch_rows(i))
    };
    let wide = |id: &str, label: &str, key: SettingKey, on: bool| SettingsRow {
        controls: vec![switch_help(key, on, &format!("{}: {label}", if on { "On" } else { "Off" }))],
        ..styled(id, label, SettingsRowStyle::Wide, Vec::new())
    };
    let organize = group("organize", "Organize", vec![
        wide("organize.by-status", "Organize by seed status", SettingKey::ByStatus, f.by_status),
        styled(
            "organize.note",
            "Finished torrents sit in Active, Dormant, Stopped or Dead folders inside the Finished location, and move between them as their sharing state changes. Off keeps every torrent directly in the Finished location.",
            SettingsRowStyle::Note,
            Vec::new(),
        ),
    ]);
    let mut lookup = wide("detection.lookup", "Look up titles online (TMDB, MusicBrainz)", SettingKey::OnlineLookup, f.online_lookup);
    lookup.quiet = true;
    lookup.controls = vec![SettingsControl::Toggle { key: SettingKey::OnlineLookup, on: f.online_lookup, enabled: false, help: "Not available yet".into(), checkbox: false }];
    let detection = group("detection", "Detection", vec![
        wide("detection.junk", "Skip samples, .nfo and junk files", SettingKey::SkipJunk, f.skip_junk),
        wide("detection.unsure", "Ask when less than 80% confident", SettingKey::AskWhenUnsure, f.ask_when_unsure),
        lookup,
    ]);
    let types = group("types", "Folders by content type (optional)", type_rows(i));
    let actions = [TorrentFileAction::Trash, TorrentFileAction::Archive, TorrentFileAction::Leave];
    let mut after = vec![row("after.torrent", "The .torrent file", None, vec![ChoiceSpec {
        help: Some(format!("What happens to the .torrent file once {APP_NAME} has added it")),
        ..choice_spec(
            SettingKey::TorrentAfterAdd,
            SettingsChoiceStyle::Menu,
            actions.iter().map(|&a| option(action_id(a), &torrent_file_action_label(a))).collect(),
            action_id(f.torrent_after_add),
        )
    }.into()])];
    if f.torrent_after_add == TorrentFileAction::Archive {
        after.push(row("after.archive", "Archive folder", None, vec![folder(SettingKey::ArchiveDir, f.archive_dir.as_deref().unwrap_or(""), "", 230, false)]));
    }
    vec![
        block(group("locations", "Locations", locations)),
        block(watch),
        SettingsBlock::Pair { left: organize, right: detection },
        block(types),
        block(group("after", "After adding a .torrent", after)),
    ]
}

// MARK: Network

/// Whether peers on the Internet can reach a port, in one word, with its symbol and colour.
pub(crate) fn reachability(state: &str, identifier: &str) -> SettingsControl {
    let (word, symbol, tone) = match state {
        "open" => ("Open", SettingsSymbol::Check, SettingsTone::Good),
        "mapped" => ("Opened on the router", SettingsSymbol::Mapped, SettingsTone::Secondary),
        "checking" => ("Checking", SettingsSymbol::Checking, SettingsTone::Secondary),
        "closed" => ("Closed", SettingsSymbol::Closed, SettingsTone::Warning),
        _ => ("Not known", SettingsSymbol::Unknown, SettingsTone::Secondary),
    };
    SettingsControl::Label { text: word.into(), tone, symbol: Some(symbol), small: true, monospaced_digits: false, identifier: Some(identifier.into()) }
}

/// Beside the port field: the port in use, or why it is another one than the manual port.
fn active_port(i: &SettingsViewInput) -> Option<SettingsControl> {
    let manual = i.state.settings.network.listen_port;
    match &i.live.port_fallback {
        Some(f) if f.wanted == manual && manual != 0 => Some(SettingsControl::Label {
            text: format!("Port {} is in use; using {}", f.wanted, f.using),
            tone: SettingsTone::Warning,
            symbol: Some(SettingsSymbol::Warning),
            small: true,
            monospaced_digits: false,
            identifier: Some("port-fallback-warning".into()),
        }),
        _ if i.live.active_port != 0 => Some(SettingsControl::Label {
            text: format!("Active: {}", i.live.active_port),
            tone: SettingsTone::Secondary,
            symbol: None,
            small: true,
            monospaced_digits: false,
            identifier: Some("active-port".into()),
        }),
        _ => None,
    }
}

fn network(i: &SettingsViewInput) -> Vec<SettingsBlock> {
    let n = &i.state.settings.network;
    let port_hint = if i.state.view.port_invalid {
        "Use a port from 1024 to 65535, or leave it empty."
    } else {
        "Leave empty for Automatic: a random port at every start."
    };
    let mut port = vec![FieldSpec {
        placeholder: "Automatic".into(),
        identifier: Some("listen-port-field".into()),
        ..field_spec(SettingKey::ListenPort, SettingsFieldKind::Port, p::port_text(n.listen_port), 90)
    }.into()];
    port.extend(active_port(i));
    let mut connection = vec![
        row("connection.port", "Listening port", Some(port_hint), port),
        row("connection.mapping", "Port mapping", Some("PCP, NAT-PMP or UPnP on your router"), vec![switch(SettingKey::PortMapping, n.port_mapping)]),
    ];
    if let Some(r) = &i.live.reachability {
        connection.push(row("connection.reach", "From the Internet", Some(&r.summary), vec![reachability(&r.state, "reachability")]));
    }
    connection.push(row(
        "connection.lan-trackers",
        "Allow trackers on local network",
        Some("For a tracker on your LAN or VPN. Off by default: trackers must be on the Internet."),
        vec![switch(SettingKey::AllowLanTrackers, n.allow_lan_trackers)],
    ));
    let encryption = match n.encryption {
        EncryptionSetting::Off => "off",
        EncryptionSetting::Prefer => "prefer",
        EncryptionSetting::Require => "require",
    };
    let peers = group(
        "peers",
        "Peers",
        vec![
            row("peers.dht", "DHT", Some("Public torrents only; always off for private ones"), vec![switch(SettingKey::Dht, n.dht)]),
            row("peers.pex", "Peer exchange (PEX)", Some("Public torrents. Private trackers: off unless allowed on the tracker's page."), vec![switch(
                SettingKey::Pex,
                n.pex,
            )]),
            row("peers.encryption", "Encryption", Some("Peer connections. Require refuses peers that cannot encrypt."), vec![choice(
                SettingKey::Encryption,
                SettingsChoiceStyle::Menu,
                vec![option("off", "Off (plain only)"), option("prefer", "Prefer encrypted"), option("require", "Require encrypted")],
                encryption,
            )]),
            row("peers.utp", "uTP", Some("Peers over UDP. Turning it on applies after a restart."), vec![switch(SettingKey::Utp, n.utp)]),
            row("peers.connections", "Connections", Some("Across all torrents, and per torrent"), vec![
                number(SettingKey::MaxConnections, n.max_connections.into()),
                text("/"),
                number(SettingKey::MaxConnectionsPerTorrent, n.max_connections_per_torrent.into()),
            ]),
        ],
    );
    // The proxy between the two: it switches parts of both off while it is on (SPEC §7H).
    vec![block(group("connection", "Connection", connection)), SettingsBlock::TorrentProxy { title: "Torrent proxy".into() }, block(peers)]
}

// MARK: Folder Sync

fn folder_sync(i: &SettingsViewInput, features: &[HostFeature]) -> Vec<SettingsBlock> {
    let Some(s) = &i.state.sync else { return Vec::new() };
    let port_hint = if let Some(e) = &i.state.view.sync_port_error {
        e.clone()
    } else if let Some(port) = i.live.sync_port_moving_to {
        format!("Starting folder sync on port {port}…")
    } else {
        "Picked at the first start and kept. Separate from the torrent port.".into()
    };
    let name = |key: SettingKey, value: &str, placeholder: &str, identifier: &str| -> SettingsControl {
        FieldSpec { placeholder: placeholder.into(), identifier: Some(identifier.into()), ..field_spec(key, SettingsFieldKind::Text, value.into(), 230) }.into()
    };
    let mut this = vec![
        row("sync.device-name", "Device name", Some("What your other devices and the people you share with see"), vec![name(
            SettingKey::DeviceName,
            &s.device_name,
            &s.default_device_name,
            "sync-device-name",
        )]),
        row("sync.user-name", "Your name", Some("Shown with the device name in chats. Every member sees the change."), vec![name(
            SettingKey::UserName,
            &s.user_name,
            "None",
            "sync-user-name",
        )]),
        row("sync.port", "Sync port", Some(&port_hint), vec![FieldSpec {
            placeholder: "Port".into(),
            identifier: Some("sync-port".into()),
            enabled: s.port != 0,
            ..field_spec(SettingKey::SyncPort, SettingsFieldKind::SyncPort, p::port_text(s.port), 90)
        }.into()]),
    ];
    if let Some(r) = &i.live.sync_reachability {
        this.push(row("sync.reach", "From the Internet", Some(&r.summary), vec![reachability(&r.state, "reachability")]));
    }
    let mut out = vec![
        block(group("sync.this", concat!("This ", seedwell_platform::computer!()), this)),
        block(group(
            "sync.finding",
            "Finding devices",
            vec![
                row("sync.lan", "On the local network", Some("Turning it on applies at the next start"), vec![switch(SettingKey::LanDiscovery, s.lan_discovery)]),
                row("sync.dht", "Over the Internet", Some("Through the DHT, on a topic only members can work out"), vec![switch(SettingKey::DhtDiscovery, s.dht_discovery)]),
            ],
        )),
    ];
    if i.live.rendezvous {
        out.push(SettingsBlock::Rendezvous);
    }
    out.push(block(group(
        "sync.relaying",
        "Relaying",
        vec![
            row("sync.relay-own", "For my own devices", Some(concat!("When two of your devices cannot reach each other, this ", seedwell_platform::computer!(), " passes their encrypted data on")), vec![
                switch(SettingKey::RelayOwnDevices, s.relay_own_devices),
            ]),
            row("sync.relay-everyone", "For everyone in shared folders", Some("Also between other people’s devices in folders you share. Relays never see the files."), vec![
                switch(SettingKey::RelayEveryone, s.relay_everyone),
            ]),
        ],
    )));
    out.push(block(group(
        "sync.speed",
        "Speed",
        vec![
            row("sync.down", "Internet download limit", Some("Folder sync only, separate from torrents. Empty is no limit."), vec![rate(SettingKey::SyncDownLimit, s.down_limit, 76)]),
            row("sync.up", "Internet upload limit", Some("The local network is never limited"), vec![rate(SettingKey::SyncUpLimit, s.up_limit, 76)]),
        ],
    )));
    out.push(block(group(
        "sync.folders",
        "Folders",
        vec![row("sync.location", "Seedwell folder", Some("Where joined folders go. Seedwell in the sidebar shows your synced folders."), vec![folder(
            SettingKey::SyncLocation,
            &s.default_location,
            "~/Seedwell",
            230,
            true,
        )])],
    )));
    if features.contains(&HostFeature::FinderExtensions) {
        let on_or_turn_on = |on: bool, manage: SettingsCommand, manage_help: &str, turn_on_help: &str, identifier: &str| {
            if on {
                vec![
                    SettingsControl::Label { text: "On".into(), tone: SettingsTone::Secondary, symbol: Some(SettingsSymbol::Check), small: false, monospaced_digits: false, identifier: None },
                    button_control(button("Manage…", manage_help, manage)),
                ]
            } else {
                vec![button_control(SettingsButton { identifier: Some(identifier.into()), ..button("Turn On…", turn_on_help, SettingsCommand::ShowFinderSetup) })]
            }
        };
        out.push(block(group(
            "sync.finder",
            "Finder",
            vec![
                row(
                    "sync.badges",
                    "Sync status in Finder",
                    Some(if i.live.finder_on {
                        "Badges on synced folders and their files. Switched on and off in System Settings."
                    } else {
                        "Badges on synced folders and their files. macOS asks you to switch it on in System Settings."
                    }),
                    on_or_turn_on(
                        i.live.finder_on,
                        SettingsCommand::ManageFinderExtension,
                        "Open the list of extensions in System Settings",
                        "Show how to switch on Finder badges",
                        "finder-turn-on",
                    ),
                ),
                row(
                    "sync.share-menu",
                    "Seedwell in the Share menu",
                    Some("Right-click any folder, Share ▸ Seedwell: share it, or start syncing it"),
                    on_or_turn_on(
                        i.live.share_menu_on,
                        SettingsCommand::ManageShareMenu,
                        "Open the Share menu's list of extensions in System Settings",
                        "Show how to add Seedwell to the Share menu",
                        "share-menu-turn-on",
                    ),
                ),
            ],
        )));
    }
    out
}

// MARK: Agents

/// `~/.claude/skills/seedwell` under `home`, with the system's separator.
pub(crate) fn skill_dir(home: &str) -> String {
    std::path::Path::new(home).join(".claude").join("skills").join("seedwell").display().to_string()
}

/// The two folders a link is offered in: shared (may need an administrator) and the user's own.
pub(crate) fn cli_folders(home: &str) -> [String; 2] {
    ["/usr/local/bin".into(), format!("{}/.local/bin", home.trim_end_matches('/'))]
}

fn cli_question(cli: &SettingsCli) -> SettingsQuestion {
    let [shared, own] = cli_folders(&cli.home);
    let link = |dir: String| SettingsCommand::LinkCli { dir, target: cli.bundled.clone() };
    SettingsQuestion {
        title: "Install the command-line tool?".into(),
        text: format!(
            "This adds a link named “seedwell” that points into the {APP_NAME} app, so you can type seedwell in a terminal.\n\n/usr/local/bin is shared and may ask for an administrator. ~/.local/bin is yours alone; add it to your PATH if it is not there."
        ),
        choices: vec![
            SettingsQuestionChoice { title: "Link in ~/.local/bin".into(), command: link(own) },
            SettingsQuestionChoice { title: "Link in /usr/local/bin".into(), command: link(shared) },
        ],
        cancel_title: "Cancel".into(),
    }
}

fn agents(i: &SettingsViewInput, features: &[HostFeature], clock: &dyn LocalClock) -> Vec<SettingsBlock> {
    let a = &i.state.settings.agents;
    let permission = match a.permission {
        AgentPermission::Off => "off",
        AgentPermission::ReadOnly => "read-only",
        AgentPermission::Control => "control",
        AgentPermission::Full => "full",
    };
    let access = group(
        "agents.access",
        "Agent access",
        vec![
            row("agents.allow", "Allow agents", Some("Local only. Never on the network."), vec![switch_help(
                SettingKey::AllowAgents,
                a.permission != AgentPermission::Off,
                concat!("Let AI agents on this ", seedwell_platform::computer!(), " connect to Seedwell"),
            )]),
            row("agents.permission", "Permission", Some("Full also deletes files and changes settings."), vec![ChoiceSpec {
                enabled: a.permission != AgentPermission::Off,
                ..choice_spec(
                    SettingKey::AgentPermission,
                    SettingsChoiceStyle::Segmented,
                    vec![option("read-only", "Read-only"), option("control", "Control"), option("full", "Full")],
                    permission,
                )
            }.into()]),
            row("agents.never", "Never available", None, vec![quiet_text("Client identity · tracker timing · passkeys · auth headers")]),
        ],
    );
    let cli = &i.live.cli;
    let installed = &i.live.host.cli;
    let mut tool_buttons = vec![button_control(button(
        if i.state.view.copied { "Copied" } else { "Copy CLI Path" },
        "Copy the path of the seedwell command to your clipboard",
        SettingsCommand::CopyCliPath { text: installed.clone().unwrap_or_else(|| cli.bundled.clone()) },
    ))];
    if installed.is_none() && features.contains(&HostFeature::CommandLineLink) {
        tool_buttons.push(button_control(button(
            "Install Command-Line Tool…",
            "Link seedwell into /usr/local/bin or ~/.local/bin so you can type it in a terminal. Asks first.",
            SettingsCommand::Ask { question: cli_question(cli) },
        )));
    }
    let tool = SettingsRow {
        hint: Some(match installed {
            Some(path) => format!("Installed: {path}"),
            None => "Not installed. It is inside the app; you can also run it by its full path.".into(),
        }),
        ..styled("agents.cli", "seedwell command", SettingsRowStyle::Described, tool_buttons)
    };
    let dir = skill_dir(&cli.home);
    let skill_installed = i.live.host.skill_installed;
    let mut skill_buttons = Vec::new();
    if skill_installed {
        skill_buttons.push(button_control(button("Remove Skill", "Delete the skill files from ~/.claude/skills/seedwell", SettingsCommand::RemoveSkill {
            dir: dir.clone(),
        })));
    }
    skill_buttons.push(button_control(button(
        if skill_installed { "Update Skill" } else { "Install Skill for Claude Code" },
        "Write the Seedwell skill to ~/.claude/skills/seedwell so Claude Code knows the commands",
        SettingsCommand::InstallSkill { dir, force: false },
    )));
    let skill = SettingsRow {
        hint: Some(
            if skill_installed {
                "Installed in ~/.claude/skills/seedwell."
            } else {
                "Teaches Claude Code the seedwell commands. Writes two text files to ~/.claude/skills/seedwell, only when you press the button."
            }
            .into(),
        ),
        ..styled("agents.skill", "Skill for Claude Code", SettingsRowStyle::Described, skill_buttons)
    };
    let note = i.state.view.cli_note.clone().unwrap_or_else(|| {
        format!("The seedwell command lets you and AI agents control {APP_NAME} from a terminal (seedwell list, seedwell add, seedwell help agents). It follows the permission above and nothing is sent anywhere.")
    });
    let command_line = group("agents.command-line", "Command line", vec![tool, skill, styled("agents.note", note, SettingsRowStyle::Note, Vec::new())]);
    let mut activity: Vec<SettingsRow> = if i.live.activity.is_empty() {
        vec![styled("activity.empty", "Nothing yet. Every action an agent takes is listed here.", SettingsRowStyle::Empty, Vec::new())]
    } else {
        i.live
            .activity
            .iter()
            .take(8)
            .enumerate()
            .map(|(n, x)| {
                styled(
                    &format!("activity.{n}"),
                    &x.summary,
                    SettingsRowStyle::Activity {
                        time: clock_hhmm(clock, x.at_ms / 1000),
                        client: x.client.clone(),
                        outcome: (x.outcome != "ok").then(|| format!("({})", x.outcome)),
                    },
                    Vec::new(),
                )
            })
            .collect()
    };
    activity.push(SettingsRow {
        quiet: true,
        ..row("activity.notify", "Notify me", None, vec![
            switch_help(SettingKey::AgentNotify, a.notify_changes, "Show a notification whenever an agent changes something"),
            text("when an agent changes something"),
        ])
    });
    let mut blocks = Vec::new();
    if !i.live.requests.is_empty() {
        blocks.push(block(requests_group(&i.live.requests)));
    }
    blocks.extend([block(access), block(command_line), block(group("agents.activity", "Activity", activity))]);
    blocks
}

/// Settings › Agents › Requests: each agent's request for access with Deny and Approve (the prominent one, last).
fn requests_group(requests: &[crate::agent_requests::AgentRequestRow]) -> SettingsGroup {
    let mut rows: Vec<SettingsRow> = requests
        .iter()
        .map(|r| {
            let decide = |approve: bool| SettingsCommand::DecideAgentRequest { id: r.id.clone(), approve };
            let deny = SettingsButton { identifier: Some(format!("agents.request.{}.deny", r.id)), ..button("Deny", "Refuse it; nothing changes", decide(false)) };
            let approve = SettingsButton {
                style: SettingsButtonStyle::Prominent,
                identifier: Some(format!("agents.request.{}.approve", r.id)),
                ..button("Approve", "Give exactly this; nothing else changes", decide(true))
            };
            SettingsRow {
                hint: Some(r.detail.clone()),
                ..styled(&format!("agents.request.{}", r.id), &r.title, SettingsRowStyle::Described, vec![button_control(deny), button_control(approve)])
            }
        })
        .collect();
    rows.push(styled(
        "agents.requests.note",
        "An agent asked for access it lacks. Approve gives it exactly what it asked for; you can also answer with a reply of yes or no to its message in chat. Requests expire after a day.",
        SettingsRowStyle::Note,
        Vec::new(),
    ));
    group("agents.requests", "Requests", rows)
}

/// "08:05": the time of day where the user is, always 24-hour with two digits (the activity's column of times).
fn clock_hhmm(clock: &dyn LocalClock, unix_s: u64) -> String {
    let local = unix_s as i64 + i64::from(clock.utc_offset_s(unix_s));
    let minute = local.rem_euclid(86_400) / 60;
    format!("{:02}:{:02}", minute / 60, minute % 60)
}

// MARK: Advanced

fn advanced(i: &SettingsViewInput, host: &[HostWindow]) -> Vec<SettingsBlock> {
    let a = &i.state.settings.advanced;
    let caches = [16u32, 32, 64, 128, 256];
    let mut cache_options: Vec<SettingsOption> = caches.iter().map(|m| option(&m.to_string(), &format!("{m} MB"))).collect();
    if !caches.contains(&a.write_cache_mib) {
        cache_options.push(option(&a.write_cache_mib.to_string(), &format!("{} MB", a.write_cache_mib)));
    }
    let disk = group(
        "disk",
        "Disk",
        vec![
            row("disk.cache", "Write cache", Some("Applies after a restart"), vec![choice(
                SettingKey::WriteCache,
                SettingsChoiceStyle::Menu,
                cache_options,
                &a.write_cache_mib.to_string(),
            )]),
            row("disk.preallocation", "Preallocation", None, vec![choice(
                SettingKey::Preallocation,
                SettingsChoiceStyle::Menu,
                vec![option("sparse", "Sparse files"), option("full", "Reserve full size")],
                match a.preallocation {
                    PreallocationMode::Sparse => "sparse",
                    PreallocationMode::Full => "full",
                },
            )]),
            row("disk.verify", "Verify on completion", None, vec![switch(SettingKey::VerifyOnCompletion, a.verify_on_completion)]),
        ],
    );
    let d = &i.live.diagnostics;
    let diagnostics = group(
        "diagnostics",
        "Diagnostics",
        vec![row("diagnostics.engine", "Engine", Some("Since launch. Wake-ups are what idle costs."), vec![SettingsControl::Label {
            text: format!("{} connections · {} wake-ups · {} rechecks · {} bad pieces", d.connections, d.wakeups, d.rechecks, d.hash_failures),
            tone: SettingsTone::Secondary,
            symbol: None,
            small: false,
            monospaced_digits: true,
            identifier: None,
        }])],
    );
    let mut developer = vec![SettingsRow {
        hint: Some("Logs every tracker request and answer to a file. Passkeys are never written.".into()),
        ..styled("developer.mode", "Developer mode", SettingsRowStyle::Described, vec![switch(SettingKey::DeveloperMode, a.developer_mode)])
    }];
    if a.developer_mode {
        let mut log = Vec::new();
        if has(host, Some(HostWindow::TrackerLog)) {
            log.push(button_control(button("View Log…", "Open the tracker log viewer with live updates and filters", SettingsCommand::ViewTrackerLog)));
        }
        log.push(button_control(button(concat!("Show Log in ", file_manager!()), concat!("Reveal the tracker log files in ", file_manager!()), SettingsCommand::ShowTrackerLog)));
        log.push(button_control(button("Clear Log", "Delete all recorded tracker log entries", SettingsCommand::ClearTrackerLog)));
        developer.push(row("developer.log", "Tracker log", None, log));
    }
    let files = group(
        "files",
        "Files",
        vec![row("files.state", "State and settings", Some("config.toml, rules.toml, trackers.toml and state.redb"), vec![button_control(button(
            concat!("Show in ", file_manager!()),
            concat!("Reveal the settings and state folder in ", file_manager!()),
            SettingsCommand::ShowInFileManager { path: i.live.state_dir.clone() },
        ))])],
    );
    vec![block(disk), block(diagnostics), block(group("developer", "Developer", developer)), block(files)]
}

// MARK: The window

/// The Settings window showing `input.tab`. `host`: the windows the host has (`host_windows_all()` on the Mac): the
/// Rules tab needs `RulesEditor`, Play videos with `Player`, View Log… `TrackerLog`. `features`: what else it has
/// (`host_features_all()` on the Mac): the Finder rows need `FinderExtensions`, Install Command-Line Tool…
/// `CommandLineLink`. `clock`: the times of the agents' actions.
#[uniffi::export]
pub fn settings_view_model(input: SettingsViewInput, host: Vec<HostWindow>, features: Vec<HostFeature>, clock: std::sync::Arc<dyn LocalClock>) -> SettingsPageModel {
    let tabs = tabs(&host);
    let tab = if tabs.iter().any(|t| t.tab == input.tab) { input.tab } else { SettingsTab::General };
    let blocks = match tab {
        SettingsTab::General => general(&input, &host),
        SettingsTab::Transfers => transfers(&input),
        SettingsTab::Folders => folders(&input),
        SettingsTab::Rules => vec![SettingsBlock::RulesEditor],
        SettingsTab::Network => network(&input),
        SettingsTab::FolderSync => folder_sync(&input, &features),
        SettingsTab::Devices => vec![SettingsBlock::Devices],
        SettingsTab::Agents => agents(&input, &features, clock.as_ref()),
        SettingsTab::Advanced => advanced(&input, &host),
    };
    let refresh_ms = match tab {
        SettingsTab::Network => 2_000,
        // Requests come, expire and are answered elsewhere (a chat reply).
        SettingsTab::Agents => 2_000,
        SettingsTab::FolderSync => 1_000,
        SettingsTab::General => match &input.live.updates {
            SettingsUpdates::Core { model } => model.refresh_ms.max(1_000),
            _ => 0,
        },
        _ => 0,
    };
    SettingsPageModel {
        tabs,
        tab,
        blocks,
        footer: (tab == SettingsTab::General && !input.live.host.version_line.is_empty()).then(|| input.live.host.version_line.clone()),
        banner: settings_banner(input.live.problems.clone(), input.live.notes.clone(), input.live.restart_required.clone(), true),
        refresh_ms,
        copied_ms: 1_500,
    }
}

#[cfg(test)]
#[path = "settings_view_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "settings_cli_tests.rs"]
mod cli_tests;
