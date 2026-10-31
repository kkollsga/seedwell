//! trs-ffi: the UniFFI surface between the Rust core and the Swift shell.
//!
//! The UI *pulls* a compact [`Snapshot`] (about once a second, only while something is visible)
//! and sends commands. The core pushes only rare events through [`CoreListener`]. Everything the
//! UI needs about transfers goes through the internal [`Backend`] trait: the real engine
//! (`trs_engine::Host`, in-process) in the app, a simulated library behind `--mock`. Settings,
//! rules and tracker profiles are read and written straight through `trs-state`, in both modes.

mod app_place;
mod backend;
mod battery;
mod chat_text;
mod display;
mod favorites;
mod files;
mod held;
mod info_card;
mod library;
mod library_rows;
mod library_window;
mod mail;
mod menu;
mod mock;
mod play;
mod power;
mod real;
mod removal;
mod row_commands;
mod row_text;
mod rows;
mod settings;
mod settings_text;
mod sidebar;
mod sources;
mod sync;
mod sync_host;
mod sync_menu;
mod sync_removal;
mod sync_text;
mod types;
mod updates;

use std::path::PathBuf;
use std::sync::Arc;

pub use backend::{AttrRow, Backend, ShutdownInfo};
pub use chat_text::*;
pub use display::RateDirection;
pub use held::{CancelNote, CancelQuestion};
pub use info_card::*;
pub use library::*;
pub use library_rows::*;
pub use library_window::*;
pub use menu::*;
pub use removal::*;
pub use updates::{EnvVar, InstallCommand, UpdateButton, Updates, UpdatesModel};
pub use row_commands::*;
pub use row_text::*;
pub use settings::*;
pub use settings_text::*;
pub use sidebar::*;
pub use sync::*;
pub use sync_host::{SyncHostBatch, SyncHostDomain, SyncHostFolderState, SyncHostListener, SyncHostNotice, SyncHostRemoval, SyncHostText};
pub use sync_menu::*;
pub use sync_text::*;
pub use types::*;

use trs_state::rules::TorrentAttrs;

uniffi::setup_scaffolding!();

/// Blocks of 128 KiB and more are mapped and unmapped directly, so the buffers of a start-up burst
/// (hash scratch, piece buffers, grown tables) do not stay resident in the system allocator next to
/// the small long-lived records: about 3.5 MB less footprint for a seeder (`trs_storage::MappedAlloc`).
#[global_allocator]
static ALLOC: trs_storage::MappedAlloc = trs_storage::MappedAlloc;

/// How to start the core.
#[derive(Clone, Debug, uniffi::Record)]
pub struct CoreOptions {
    /// The state directory (`config.toml`, `state.redb`, `control.sock`, ...).
    pub config_dir: String,
    /// The simulated library instead of the engine (design work, snapshot renderer).
    pub mock: bool,
    /// QA only: allow trackers on loopback (a local mock tracker). Never set by the app itself.
    pub allow_loopback_trackers: bool,
    /// Isolated runs (QA): refuse to start when the state folder or any folder in `config.toml`
    /// (download, incomplete, watch, archive, categories) lies inside this path (the real home: `~` on the Mac, the
    /// user profile on Windows), a temporary folder excepted. Set, it also makes the run's Trash `Trash` in its state
    /// folder, never the user's.
    pub forbidden_root: Option<String>,
}

/// What a shutdown achieved.
#[derive(Clone, Copy, Debug, uniffi::Record)]
pub struct ShutdownSummary {
    pub confirmed: u32,
    pub abandoned: u32,
    /// False when the wait ran out before the engine was done.
    pub finished: bool,
}

/// The one object the shell talks to.
#[derive(uniffi::Object)]
pub struct Core {
    backend: Arc<dyn Backend>,
    settings: SettingsStore,
    dir: PathBuf,
    /// An isolated (test) run (`forbidden_root` set): it reaches no real tracker and no real update server.
    isolated: bool,
    /// "On battery" (SPEC §8.4), fed by `power_changed` and by changes of `config.toml`.
    battery: Arc<battery::Battery>,
    /// Where the control server finds the updater (`update.*`), once the host has made one.
    builds: trs_control::BuildsSlot,
    /// The File Provider extension's server, while the app runs it (`sync_host_start`).
    #[cfg(unix)]
    host: std::sync::Mutex<Option<sync_host::Server>>,
}

impl Core {
    /// Opens the state and starts the engine (or the mock), and folder sync with its keys in `vault`
    /// (`None`: a 0600 file in the state folder).
    fn open(options: CoreOptions, vault: Option<Arc<dyn SecretVault>>) -> Result<Arc<Self>, CoreError> {
        let dir = PathBuf::from(&options.config_dir);
        let builds = trs_control::BuildsSlot::new();
        let backend: Arc<dyn Backend> = if options.mock {
            let _ = std::fs::create_dir_all(&dir);
            mock::MockBackend::start(options.config_dir.clone())
        } else {
            real::RealBackend::start(real::StartOptions {
                socket: dir.join("control.sock"),
                dir: dir.clone(),
                allow_loopback_trackers: options.allow_loopback_trackers,
                forbidden_root: options.forbidden_root.clone().map(PathBuf::from),
                secret_vault: vault,
                builds: builds.clone(),
            })?
        };
        let battery = Arc::new(battery::Battery::new(backend.clone(), dir.clone()));
        let isolated = options.forbidden_root.is_some();
        Ok(Arc::new(Self {
            backend,
            settings: SettingsStore::new(&dir),
            dir,
            isolated,
            battery,
            builds,
            #[cfg(unix)]
            host: std::sync::Mutex::new(None),
        }))
    }

}

#[uniffi::export]
impl Core {
    /// Opens (or creates) the state in `options.config_dir` and starts the engine.
    #[uniffi::constructor]
    pub fn new(options: CoreOptions) -> Result<Arc<Self>, CoreError> {
        Self::open(options, None)
    }

    /// Cheap; the shell calls it about once a second while something is visible.
    pub fn snapshot(&self) -> Snapshot {
        self.backend.snapshot()
    }

    /// Registers the event listener, replacing any previous one.
    pub fn set_listener(&self, listener: Arc<dyn CoreListener>) {
        self.backend.set_listener(Arc::new(battery::Listener { inner: listener, battery: self.battery.clone() }));
    }

    /// The power source: `true` on battery. The host reports it at start and whenever it changes (macOS:
    /// IOPS; Windows: `GetSystemPowerStatus` and `WM_POWERBROADCAST`); repeating the same report does
    /// nothing. The "On battery" setting decides what follows: nothing, the app's own Limit Speed, or
    /// pausing the downloads until AC power is back (SPEC §8.4).
    pub fn power_changed(&self, on_battery: bool) {
        self.battery.power_changed(on_battery);
    }

    /// Finished torrents (the Sharing view), filtered and paged.
    pub fn sharing_rows(&self, query: RowQuery) -> Vec<TransferRow> {
        self.backend.rows(&query)
    }

    /// The sidebar tracker id of a transfer, hidden or not (notifications of hidden trackers are held back).
    pub fn tracker_of(&self, id: String) -> Option<String> {
        self.backend.tracker_of(&id)
    }

    /// Sizes of the Sharing groups, optionally within one tracker and matching a search text.
    pub fn sharing_counts(&self, tracker_id: Option<String>, category: Option<Category>, text: String) -> SharingCounts {
        self.backend.sharing_counts(tracker_id.as_deref(), category, &text)
    }

    /// Lifetime totals of one tracker; call when its header is shown, not every second.
    pub fn tracker_stats(&self, tracker_id: String) -> TrackerStats {
        self.backend.tracker_stats(&tracker_id)
    }

    /// Daily up/down buckets for the last `days` days, oldest first.
    pub fn tracker_daily(&self, tracker_id: String, days: u32) -> Vec<DailyStat> {
        self.backend.tracker_daily(&tracker_id, days)
    }

    /// Details for the Get Info popover.
    pub fn info(&self, id: String) -> Result<TorrentInfo, CoreError> {
        self.backend.info(&id)
    }

    /// The real announce URL of one tracker line of Get Info, passkey included: for "Copy URL" only.
    pub fn tracker_url(&self, id: String, index: u32) -> Result<String, CoreError> {
        self.backend.tracker_url(&id, index)
    }

    /// A magnet link without any passkey (SPEC §7B.4).
    pub fn magnet_link(&self, id: String) -> Result<String, CoreError> {
        self.backend.magnet_link(&id)
    }

    /// Adds a magnet link, `.torrent` file path or http(s) URL.
    pub fn add_source(&self, source: String) -> Result<String, CoreError> {
        // A path to a file that is not there (or cannot be read) is said as such, before the backend
        // would call it "not a .torrent file".
        if let Some(e) = sources::file_problem(&source) {
            return Err(e);
        }
        self.backend.add_source(&source)
    }

    pub fn pause(&self, ids: Vec<String>) {
        self.backend.pause(&ids);
    }

    pub fn resume(&self, ids: Vec<String>) {
        self.backend.resume(&ids);
    }

    pub fn move_to_top(&self, id: String) {
        self.backend.move_to_top(&id);
    }

    pub fn remove(&self, ids: Vec<String>, delete_files: bool) {
        self.backend.remove(&ids, delete_files);
    }

    pub fn set_priority(&self, ids: Vec<String>, priority: Priority) {
        self.backend.set_priority(&ids, priority);
    }

    pub fn set_category(&self, ids: Vec<String>, category: Category) {
        self.backend.set_category(&ids, category);
    }

    pub fn set_seeding_goal(&self, ids: Vec<String>, goal: SeedingGoal) {
        self.backend.set_seeding_goal(&ids, goal);
    }

    /// Forces a rule (by id) on torrents; `None` lets the rules decide again.
    pub fn set_rule(&self, ids: Vec<String>, rule: Option<String>) {
        self.backend.set_rule(&ids, rule);
    }

    /// Re-verifies the files of the torrents.
    pub fn verify(&self, ids: Vec<String>) {
        self.backend.verify(&ids);
    }

    /// "Ask Tracker Again": a manual re-announce. Seedwell's own backoff never delays it; the tracker's
    /// minimum interval does.
    pub fn ask_tracker_again(&self, ids: Vec<String>) -> AskAgainOutcome {
        self.backend.ask_tracker_again(&ids, false)
    }

    /// "Announce anyway": also past the tracker's minimum interval (one per tracker per 5 minutes).
    pub fn announce_anyway(&self, ids: Vec<String>) -> AskAgainOutcome {
        self.backend.ask_tracker_again(&ids, true)
    }

    /// Stops sharing (pauses a seeding torrent).
    pub fn stop_sharing(&self, ids: Vec<String>) {
        self.backend.pause(&ids);
    }

    pub fn pause_all(&self) {
        self.backend.pause_all();
    }

    pub fn resume_all(&self) {
        self.backend.resume_all();
    }

    /// Forgets the Recent list (the finished torrents keep seeding).
    pub fn clear_recent(&self) {
        self.backend.clear_recent();
    }

    /// Global limits in bytes/s; `None` is unlimited.
    pub fn set_limits(&self, down: Option<u64>, up: Option<u64>) {
        self.backend.set_limits(down, up);
    }

    /// The user's Limit Speed toggle (⌥⌘L). Kept in `config.toml` like the CLI's, so a later change of
    /// any setting (the engine applies the whole file again) and the next start keep it.
    pub fn set_alt_speed(&self, on: bool) {
        self.backend.set_alt_speed(on);
        let _ = self.settings.save_alt_speed(on);
    }

    // ---------------------------------------------------------------- settings

    /// Every setting the Settings window edits.
    pub fn settings_load(&self) -> AppSettings {
        self.settings.load_settings()
    }

    /// Problems in the settings files, one sentence each, for the host to show (Settings, a
    /// notice at start). A bad value in `config.toml` costs only itself: the line names the key,
    /// the bad value and what is used instead and why ("config.toml: folders.torrent_after_add:
    /// \"keep\" is not a valid value (…); \"leave\", which leaves your files alone, is used until
    /// it is fixed"; or the last good value from the backup, or the default).
    /// Empty when the files are fine.
    pub fn settings_problems(&self) -> Vec<String> {
        self.settings.load_notes()
    }

    /// Validates and saves; the engine applies the file within seconds.
    pub fn settings_save(&self, settings: AppSettings) -> Result<(), CoreError> {
        self.settings.save_settings(&settings)?;
        self.backend.settings_saved();
        self.battery.settings_changed();
        Ok(())
    }

    /// Ordered rules and the Default rule.
    pub fn rules_load(&self) -> RulesDoc {
        self.settings.load_rules()
    }

    /// Validates and saves the whole rule list; returns it with ids filled in.
    pub fn rules_save(&self, doc: RulesDoc) -> Result<RulesDoc, CoreError> {
        let saved = self.settings.save_rules(&doc)?;
        self.backend.settings_saved();
        Ok(saved)
    }

    /// Dry run of saving `doc`: how many torrents the rule `rule_id` governs (`"*"` = the Default
    /// rule, `""` = the rule that was appended last and has no id yet) ("affects 44
    /// torrents") and how many would change their goal, priority or limit.
    pub fn rule_preview(&self, doc: RulesDoc, rule_id: String) -> RulePreview {
        let before = self.settings.load_rules_set();
        let after = self.settings.build_rules(&doc);
        let profiles = self.settings.load_profiles_set();
        let attrs = self.backend.torrent_attrs();
        // A rule that is not saved yet has no id: it is the one the builder made last.
        let default_rule = rule_id == "*";
        let id = if rule_id.is_empty() {
            after.rules.last().map(|r| r.id.clone()).unwrap_or_default()
        } else {
            rule_id
        };
        let (mut affected, mut changed) = (0u32, 0u32);
        let mut note = String::new();
        for a in &attrs {
            let linked = a.profile.as_deref().and_then(|p| profiles.get(p)).and_then(|p| p.rule.as_deref());
            let t = TorrentAttrs {
                tracker: a.profile.as_deref(),
                linked_rule: linked,
                private: a.private,
                category: settings::category_from(a.category),
            };
            let governed = if default_rule {
                after.find(&t).is_none()
            } else {
                after.find(&t).is_some_and(|(r, _)| r.id == id)
            };
            if governed {
                affected += 1;
            }
            let (b, n) = (before.resolve(&t), after.resolve(&t));
            if b.goal != n.goal || b.priority != n.priority || b.up_limit != n.up_limit || b.min_seed_time != n.min_seed_time || b.idle != n.idle {
                changed += 1;
                if note.is_empty() && b.goal != n.goal {
                    note = goal_change_note(&n.goal);
                }
            }
        }
        RulePreview { affected, changed, change_note: note }
    }

    /// Tracker profiles (SPEC §7E.1).
    pub fn profiles_load(&self) -> Vec<ProfileRecord> {
        self.settings.load_profiles()
    }

    /// Validates and saves the whole list; returns it with ids filled in.
    pub fn profiles_save(&self, profiles: Vec<ProfileRecord>) -> Result<Vec<ProfileRecord>, CoreError> {
        let saved = self.settings.save_profiles(&profiles)?;
        self.backend.settings_saved();
        Ok(saved)
    }

    /// A profile named after an announce URL's host, ready for "Create tracker profile…".
    pub fn profile_suggest(&self, announce_url: String) -> Result<ProfileRecord, CoreError> {
        self.settings.suggest_profile(&announce_url)
    }

    /// Incomplete/finished pairs on different disks (the orange warning of SPEC §7A.4), for the
    /// saved settings. Cheap: a few `stat` calls.
    pub fn volume_warnings(&self) -> Vec<VolumeWarning> {
        settings::volume_warnings(&self.settings.config())
    }

    /// The different-disk warnings of a favorite's own folders, from the editor's draft.
    pub fn tracker_volume_warnings(&self, profile: ProfileRecord) -> Vec<VolumeWarning> {
        settings::tracker_volume_warnings(&self.settings.config(), &profile)
    }

    /// `~/` expanded against the home folder in use (`--home-root` aware).
    pub fn expand_path(&self, path: String) -> String {
        settings::expand_home(&path).display().to_string()
    }

    /// Counters for the Advanced tab.
    pub fn diagnostics(&self) -> DiagnosticsInfo {
        self.backend.diagnostics()
    }

    /// The torrents a change of locations leaves elsewhere than the settings say (nothing moves).
    pub fn location_moves(&self) -> MoveProposalInfo {
        self.backend.location_moves()
    }

    /// Moves torrents to where the settings say they belong; they move one after the other,
    /// seeding goes on, rows show "Moving… 42%".
    pub fn move_torrents(&self, ids: Vec<String>) -> MoveResult {
        self.backend.move_torrents(&ids)
    }

    /// "Move to…": below `path`, pinned there.
    pub fn move_to(&self, ids: Vec<String>, path: String) -> Result<MoveResult, CoreError> {
        self.backend.move_to(&ids, &settings::expand_home(&path))
    }

    pub fn cancel_moves(&self, ids: Vec<String>) {
        self.backend.cancel_moves(&ids);
    }

    /// The chooser of a torrent that waits for a choice (or, with `reopen`, of a running one:
    /// Get Info ▸ Files).
    pub fn choice(&self, id: String, reopen: bool) -> Result<ChoiceInfo, CoreError> {
        self.backend.choice(&id, reopen)
    }

    /// Answers a chooser: one flag per torrent file index (`true` = download). A held torrent
    /// starts; a running one changes which files it wants.
    pub fn confirm_choice(&self, id: String, files: Vec<bool>) -> Result<(), CoreError> {
        self.backend.confirm_choice(&id, &files)
    }

    /// Start on a torrent that waits for a choice, with nothing ticked by the user (a row's Start, the menu's): it
    /// downloads what [`held::choice_default_files`] says, everything but the Extras.
    pub fn confirm_choice_default(&self, id: String) -> Result<(), CoreError> {
        let info = self.backend.choice(&id, false)?;
        self.backend.confirm_choice(&id, &held::choice_default_files(&info))
    }

    /// Stops the countdown of a torrent that waits to start (SPEC §7A.7). It stays held and silent.
    pub fn pending_pause(&self, id: String) -> Result<(), CoreError> {
        self.backend.pending_act(&id, backend::PendingAct::Pause).map(|_| ())
    }

    /// Runs a stopped countdown again (at least 3 s are left).
    pub fn pending_resume(&self, id: String) -> Result<(), CoreError> {
        self.backend.pending_act(&id, backend::PendingAct::Resume).map(|_| ())
    }

    /// Starts a waiting torrent now.
    pub fn pending_start_now(&self, id: String) -> Result<(), CoreError> {
        self.backend.pending_act(&id, backend::PendingAct::StartNow).map(|_| ())
    }

    /// "Choose Files…": cancels the countdown and keeps the torrent held. `true`: the chooser can
    /// open now. `false`: a magnet link, whose metadata is fetched first; the chooser follows as a
    /// needs-choice event.
    pub fn pending_choose(&self, id: String) -> Result<bool, CoreError> {
        self.backend.pending_act(&id, backend::PendingAct::Choose)
    }

    /// Cancels a held torrent: it is removed and nothing was ever written.
    pub fn cancel_choice(&self, id: String) -> Result<(), CoreError> {
        self.backend.cancel_choice(&id)
    }

    /// Cancel on a held torrent (SPEC 7A.6), the whole of it: the torrent is removed (one in its countdown, which the
    /// engine does not cancel, is removed without its files) and, unless `keep_file` (⇧-Cancel), the `.torrent` file it
    /// came from goes to the Trash. A file that cannot be moved never stops the removal: the note says so, for a quiet
    /// notification. `None`: nothing to say. Blocks briefly (the removal and a file move).
    pub fn cancel_held(&self, id: String, keep_file: bool) -> Option<CancelNote> {
        held::cancel_held(&*self.backend, &id, keep_file)
    }

    /// The `.torrent` file a torrent that waits for a decision came from, if it still lies where Seedwell
    /// left it: read now, and only returned when it is a regular `.torrent` file (not a link) with this
    /// torrent's info-hash (`cancel_held` reads it itself, before the cancel). `None` for magnet links,
    /// web addresses, and files that are gone or changed.
    pub fn held_source_file(&self, id: String) -> Option<String> {
        self.backend.held_source(&id)
    }

    /// Whether `held_source_file` has a file to offer, without reading it (cheap, for a button's label).
    pub fn has_held_source_file(&self, id: String) -> bool {
        self.backend.has_held_source(&id)
    }

    /// `.torrent` files that were in a watch folder before it joined the list; the app may offer
    /// to add them once. Empty for an unknown folder.
    pub fn watch_backlog(&self, folder: String) -> Vec<String> {
        self.backend.watch_backlog(&settings::expand_home(&folder))
    }

    // ---------------------------------------------------------------- play while downloading (SPEC §7F)

    /// The files of a torrent, for the Play entries (video files, whether they are selected, how far
    /// they are). Padding files are left out.
    pub fn files(&self, id: String) -> Result<Vec<TorrentFile>, CoreError> {
        self.backend.files(&id)
    }

    /// Sets the priority of the listed files (indices from `files`). Skip stops downloading and
    /// seeding a file; nothing is deleted from disk. One call changes any number of files.
    pub fn set_file_priorities(&self, id: String, changes: Vec<FilePriorityChange>) -> Result<(), CoreError> {
        self.backend.set_file_priorities(&id, &changes)
    }

    /// A local URL to play a downloading file in the user's own player. Finished files do not need
    /// it (the app opens the file itself; `complete` says it finished meanwhile). A
    /// skipped file is selected for download first. Errors carry a plain sentence for the user.
    pub fn stream_url(&self, torrent_id: String, file_index: u32) -> Result<StreamInfo, CoreError> {
        self.backend.stream_url(&torrent_id, file_index)
    }

    /// The OS path monitor reports whether a usable network path exists (SPEC 7C.4c). The app
    /// debounces it (3 s). While unsatisfied the engine sends nothing; when it is satisfied
    /// again the engine probes once and lets the announces that came due go out, spread.
    pub fn network_path_changed(&self, satisfied: bool) {
        self.backend.network_path_changed(satisfied);
    }

    /// The Mac is about to sleep (nothing expensive happens).
    pub fn system_will_sleep(&self) {
        self.backend.system_will_sleep();
    }

    /// The Mac woke: the engine waits for the network, then probes once (SPEC 7C.4c).
    pub fn system_did_wake(&self) {
        self.backend.system_did_wake();
    }

    /// A volume was mounted or unmounted. The engine looks at the folders torrents save to
    /// (once each) and marks torrents on a drive that is gone "Drive not connected", or resumes
    /// those on a drive that came back, verifying only what it must, two torrents at a time.
    pub fn volumes_changed(&self) {
        self.backend.volumes_changed();
    }

    /// The port the engine listens on now: the manual one, or the random one picked at this start. It changes
    /// when the setting is applied (live, no restart).
    pub fn listen_port(&self) -> u16 {
        self.backend.listen_port()
    }

    /// Whether peers on the Internet can reach the listening port: a peer that connected in, or else
    /// what the router said to the port mapping. Cheap; the Network settings poll it.
    pub fn reachability(&self) -> Option<ReachabilityInfo> {
        self.backend.reachability()
    }

    /// Present when the configured listening port was in use by another app at start and the engine
    /// listens on another one for now.
    pub fn port_fallback(&self) -> Option<PortFallbackInfo> {
        self.backend.port_fallback()
    }

    /// Closes the stream with this token.
    pub fn stop_stream(&self, token: String) {
        self.backend.stop_stream(&token);
    }

    // ---------------------------------------------------------------- developer mode

    /// The newest lines of the tracker log that pass `query`, oldest first (a live tail: the shell
    /// calls it about once a second while the viewer is open).
    pub fn tracker_log(&self, query: TrackerLogQuery) -> Vec<TrackerLogRow> {
        let q = trs_devlog::read::Query {
            torrent: None,
            tracker: (!query.tracker.is_empty()).then(|| query.tracker.clone()),
            event: (!query.event.is_empty()).then(|| query.event.clone()),
            errors_only: query.errors_only,
            text: (!query.text.is_empty()).then(|| query.text.clone()),
            limit: if query.limit == 0 { 300 } else { query.limit as usize },
        };
        trs_devlog::read::tail(&self.dir.join("logs"), &q)
            .iter()
            .filter_map(|l| trs_devlog::view::Row::parse(l))
            .map(|r| TrackerLogRow {
                ts_ms: r.ts_ms,
                kind: r.kind,
                torrent: r.torrent,
                id: r.id,
                title: r.title,
                tracker: r.tracker,
                host: r.host,
                transport: r.transport,
                event: r.event,
                error: r.error,
                summary: r.summary,
                detail: r.detail,
            })
            .collect()
    }

    /// Path of `trackers.log` (Show Log in Finder).
    pub fn tracker_log_path(&self) -> String {
        self.dir.join("logs").join("trackers.log").display().to_string()
    }

    /// Deletes the tracker log. The setting is untouched.
    pub fn tracker_log_clear(&self) {
        self.backend.tracker_log_clear(&self.dir.join("logs"));
    }

    /// Lines dropped because the writer could not keep up (0 normally).
    pub fn tracker_log_dropped(&self) -> u64 {
        self.backend.tracker_log_dropped()
    }

    // ---------------------------------------------------------------- agents

    /// The newest agent actions, newest first.
    pub fn agent_activity(&self, limit: u32) -> Vec<AgentActivity> {
        self.backend.agent_activity(limit)
    }

    /// Agents that asked to keep the Mac awake while they work (`None`: none). Cheap; the app holds a
    /// power assertion while this is `Some` and "Keep awake while agents work" is on.
    pub fn agents_awake(&self) -> Option<AgentsAwake> {
        self.backend.agents_awake()
    }

    /// Path of the control socket, or `None` when agents cannot connect.
    pub fn control_socket(&self) -> Option<String> {
        self.backend.control_socket()
    }

    /// The state directory in use.
    pub fn state_dir(&self) -> String {
        self.dir.display().to_string()
    }

    // ---------------------------------------------------------------- shutdown

    /// Stops the engine: sends the `stopped` announces and flushes state. Waits at most
    /// `deadline_secs` (SPEC §12 #16: 120 s for Quit, 3 s on logout). Blocks; call it off the
    /// main thread.
    pub fn shutdown(&self, deadline_secs: u32) -> ShutdownSummary {
        let r = self.backend.shutdown(deadline_secs);
        ShutdownSummary { confirmed: r.confirmed, abandoned: r.abandoned, finished: r.finished }
    }

    /// Cuts a shutdown that is already running short (logout or power off).
    pub fn shorten_shutdown(&self, secs: u32) {
        self.backend.shorten_shutdown(secs);
    }
}

fn goal_change_note(g: &trs_state::rules::SeedingGoal) -> String {
    use trs_state::rules::SeedingGoal as G;
    match g {
        G::StopWhenFinished => "they stop sharing as soon as the download finishes".into(),
        G::Ratio { ratio } => format!("they stay seeding until ratio {ratio:.1}"),
        G::Time { time } => format!("they stay seeding for {} days", time.0 / 86_400),
        G::RatioOrTime { ratio, time } => format!("they seed to ratio {ratio:.1} or {} days", time.0 / 86_400),
        G::RatioAndTime { ratio, time } => format!("they seed to ratio {ratio:.1} and {} days", time.0 / 86_400),
        G::Forever => "they seed forever".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_core() -> (Arc<Core>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(CoreOptions {
            config_dir: dir.path().display().to_string(),
            mock: true,
            allow_loopback_trackers: false,
            forbidden_root: None,
        })
        .unwrap();
        (core, dir)
    }

    #[test]
    fn mock_snapshot_matches_concepts() {
        let (core, _d) = mock_core();
        let s = core.snapshot();
        assert_eq!(s.downloading.len(), 2);
        assert!(s.sharing_total >= 212);
        assert_eq!(s.sharing.total(), s.sharing_total);
        assert!(s.sharing.error + s.sharing.dead >= 3);
        assert!(s.trackers.iter().map(|t| t.count).sum::<u32>() >= 212);
        // No favorites: the trackers come from the torrents themselves, Public is last.
        assert!(s.trackers.iter().all(|t| !t.favorite));
        assert_eq!(s.trackers.last().map(|t| t.id.as_str()), Some("public"));
        assert!(s.trackers.iter().any(|t| t.id == "tracker-z.cc" && t.count > 0));
    }

    #[test]
    fn sharing_rows_page_and_filter() {
        let (core, _d) = mock_core();
        let q = |group, tracker: Option<&str>| RowQuery {
            group,
            tracker_id: tracker.map(str::to_owned),
            text: String::new(),
            category: None,
            offset: 0,
            limit: 1000,
        };
        let dormant = core.sharing_rows(q(Some(SeedGroup::Dormant), None));
        assert!(dormant.len() > 150);
        let z = core.sharing_rows(q(None, Some("tracker-z.cc")));
        assert!(!z.is_empty() && z.iter().all(|r| r.tracker_id == "tracker-z.cc"));
        let counts = core.sharing_counts(Some("tracker-z.cc".into()), None, String::new());
        assert_eq!(counts.total() as usize, z.len());
        // A category narrows rows and counts alike.
        let cat = z[0].category;
        let only = core.sharing_rows(RowQuery { category: Some(cat), ..q(None, Some("tracker-z.cc")) });
        assert!(!only.is_empty() && only.len() <= z.len() && only.iter().all(|r| r.category == cat));
        assert_eq!(core.sharing_counts(Some("tracker-z.cc".into()), Some(cat), String::new()).total() as usize, only.len());
    }

    #[test]
    fn a_bad_settings_value_is_named_and_the_rest_is_kept() {
        let (core, d) = mock_core();
        assert_eq!(core.settings_problems(), Vec::<String>::new());
        std::fs::write(d.path().join("config.toml"), "[agents]\npermission = \"full\"\n[folders]\ntorrent_after_add = \"keep\"\n").unwrap();
        let p = core.settings_problems();
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("config.toml: folders.torrent_after_add: \"keep\" is not a valid value ("), "{}", p[0]);
        assert!(p[0].ends_with("\"leave\", which leaves your files alone, is used until it is fixed"), "{}", p[0]);
        let s = core.settings_load();
        assert_eq!(s.agents.permission, AgentPermission::Full, "the rest of the file is in force");
        assert_eq!(s.folders.torrent_after_add, TorrentFileAction::Leave, "not the default (Trash): no backup has a value");
    }

    #[test]
    fn pause_resume_and_remove() {
        let (core, _d) = mock_core();
        let id = core.snapshot().downloading[0].id.clone();
        core.pause(vec![id.clone()]);
        assert!(core.snapshot().downloading.iter().all(|r| r.id != id));
        core.resume(vec![id.clone()]);
        core.remove(vec![id.clone()], false);
        assert!(core.info(id).is_err());
    }

    #[test]
    fn mock_files_and_stream_url() {
        let (core, _d) = mock_core();
        let s = core.snapshot();
        // Every single-file row lists one file named with its extension, so a video is a video (Play offers it).
        let singles: Vec<&TransferRow> =
            s.downloading.iter().chain(&s.sharing_busiest).filter(|r| !r.file_ext.is_empty()).collect();
        assert!(singles.iter().any(|r| row_text::is_video_ext(&r.file_ext)), "a single-file video");
        for r in &singles {
            let files = core.files(r.id.clone()).unwrap();
            assert_eq!(files.len(), 1);
            assert_eq!(files[0].index, 0);
            assert!(files[0].name.ends_with(&format!(".{}", r.file_ext)), "{}", files[0].name);
            assert_eq!(files[0].is_video, row_text::is_video_ext(&r.file_ext), "{}", files[0].name);
            if files[0].is_video {
                let s = core.stream_url(r.id.clone(), 0).unwrap();
                assert!(s.url.starts_with("http://127.0.0.1:"));
            }
        }
        let row = singles[0];
        assert!(matches!(core.stream_url(row.id.clone(), 99), Err(CoreError::Failed { .. })));
        assert!(matches!(core.files("nope".into()), Err(CoreError::NotFound { .. })));
        core.set_file_priorities(row.id.clone(), vec![FilePriorityChange { index: 0, priority: FilePriority::Low }]).unwrap();
        assert_eq!(core.files(row.id.clone()).unwrap()[0].priority, FilePriority::Low);
        assert!(core.set_file_priorities("nope".into(), vec![]).is_err());
        core.stop_stream("no-such-token".into());
    }

    #[test]
    fn add_source_validates() {
        let (core, _d) = mock_core();
        assert!(core.add_source("garbage".into()).is_err());
        assert!(
            core.add_source(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Test".into()
            )
            .is_ok()
        );
    }

    #[test]
    fn adding_a_missing_torrent_file_says_it_is_missing() {
        let (core, d) = mock_core();
        let gone = d.path().join("debian.torrent").display().to_string();
        let e = core.add_source(gone.clone()).unwrap_err();
        assert!(matches!(&e, CoreError::InvalidSource { message } if *message == format!("File not found: {gone}")), "{e:?}");
        assert!(matches!(core.add_source("garbage".into()), Err(CoreError::InvalidSource { .. })));
    }

    #[test]
    fn a_magnet_link_has_no_source_file() {
        let (core, _d) = mock_core();
        let id = core
            .add_source("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Test".into())
            .unwrap();
        assert!(!core.has_held_source_file(id.clone()));
        assert_eq!(core.held_source_file(id), None);
        assert_eq!(core.held_source_file("not-an-id".into()), None);
    }

    #[test]
    fn rule_preview_counts_torrents() {
        let (core, _d) = mock_core();
        let mut doc = core.rules_load();
        doc.rules.push(RuleRecord {
            id: String::new(),
            name: "Everything high".into(),
            enabled: true,
            tracker_ids: vec!["public".into()],
            private_match: PrivateMatch::Any,
            categories: vec![],
            goal: Some(GoalKind::RatioOnly { ratio: 3.0 }),
            min_seed_secs: None,
            priority: Some(Priority::High),
            on_goal: None,
            finished_location: None,
            up_limit: None,
            idle_below_bytes: None,
            idle_hours: 150,
        });
        let p = core.rule_preview(doc, String::new());
        assert!(p.affected > 10, "{p:?}");
        assert!(p.changed > 0);
    }

    fn set_on_battery(core: &Core, mode: OnBatteryMode) {
        let mut s = core.settings_load();
        s.general.on_battery = mode;
        core.settings_save(s).unwrap();
    }

    fn state_of(core: &Core, id: &str) -> Option<TransferState> {
        let s = core.snapshot();
        s.downloading.iter().chain(&s.queued).find(|r| r.id == id).map(|r| r.state)
    }

    #[test]
    fn on_battery_the_downloads_pause_and_on_ac_what_the_app_paused_resumes() {
        let (core, _d) = mock_core();
        set_on_battery(&core, OnBatteryMode::Pause);
        let running: Vec<String> = core.snapshot().downloading.iter().filter(|r| r.state == TransferState::Downloading).map(|r| r.id.clone()).collect();
        assert!(running.len() >= 2, "{running:?}");
        core.power_changed(false);
        assert!(running.iter().all(|id| state_of(&core, id) == Some(TransferState::Downloading)), "a start on AC power changes nothing");
        core.power_changed(true);
        assert!(running.iter().all(|id| state_of(&core, id) == Some(TransferState::Paused)));
        // The user resumes one on battery (it keeps running) and pauses another (it stays paused on AC).
        core.resume(vec![running[0].clone()]);
        core.pause(vec![running[1].clone()]);
        core.power_changed(true);
        assert_ne!(state_of(&core, &running[0]), Some(TransferState::Paused), "a repeated report does not pause again");
        core.power_changed(false);
        assert_eq!(state_of(&core, &running[1]), Some(TransferState::Paused), "the user's pause is the user's");
        assert!(running[2..].iter().all(|id| state_of(&core, id) != Some(TransferState::Paused)), "the rest resumed");
    }

    #[test]
    fn on_battery_limit_speed_is_the_apps_own_and_the_setting_can_change_on_battery() {
        let (core, d) = mock_core();
        core.power_changed(true);
        assert!(core.snapshot().alt_speed, "Limit (the default) switches Limit Speed on at once");
        assert!(!SettingsStore::new(d.path()).config().transfers.alt.enabled, "the app's limit is not the user's toggle");
        // The setting changes on battery: the limit goes and the downloads pause.
        set_on_battery(&core, OnBatteryMode::Pause);
        let s = core.snapshot();
        assert!(!s.alt_speed);
        assert!(s.downloading.iter().all(|r| r.state != TransferState::Downloading));
        set_on_battery(&core, OnBatteryMode::KeepGoing);
        assert!(core.snapshot().downloading.iter().any(|r| r.state == TransferState::Downloading), "they run again");
        // The user's toggle ends the app's limit; back on AC it is left as the user set it.
        set_on_battery(&core, OnBatteryMode::Limit);
        assert!(core.snapshot().alt_speed);
        core.set_alt_speed(false);
        assert!(!core.snapshot().alt_speed);
        core.set_alt_speed(true);
        core.power_changed(false);
        assert!(core.snapshot().alt_speed, "the user's Limit Speed stays on AC");
    }

    /// A real core in temporary folders (HOME too: the engine's guard treats it as the real home), with
    /// no watch folder and no port mapping.
    fn real_core() -> (Arc<Core>, PathBuf, [tempfile::TempDir; 2]) {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nextest runs every test in its own process.
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = work.path().join("state");
        let store = SettingsStore::new(&dir);
        let mut st = store.load_settings();
        st.folders.download_dir = work.path().join("done").display().to_string();
        st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
        st.folders.watch.clear();
        st.network.port_mapping = false;
        st.transfers.start_delay_secs = 0;
        store.save_settings(&st).unwrap();
        let options = CoreOptions { config_dir: dir.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None };
        (Core::new(options).unwrap(), dir, [home, work])
    }

    #[test]
    fn an_isolated_run_trashes_into_its_own_state_never_the_users_trash() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nextest runs every test in its own process.
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = work.path().join("state");
        let store = SettingsStore::new(&dir);
        let mut st = store.load_settings();
        st.folders.download_dir = work.path().join("done").display().to_string();
        st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
        st.folders.watch.clear();
        st.folders.torrent_after_add = TorrentFileAction::Trash;
        st.network.port_mapping = false;
        store.save_settings(&st).unwrap();
        let options = CoreOptions {
            config_dir: dir.display().to_string(),
            mock: false,
            allow_loopback_trackers: false,
            forbidden_root: Some(home.path().display().to_string()),
        };
        let core = Core::new(options).unwrap();
        assert_eq!(core.backend.trash_dir(), Some(dir.join("Trash")));
        // An added .torrent file that its setting sends to the Trash lands there.
        let mut t = b"d4:infod6:lengthi40000e4:name5:a.bin12:piece lengthi65536e6:pieces20:".to_vec();
        t.extend_from_slice(&[7u8; 20]);
        t.extend_from_slice(b"ee");
        let tf = work.path().join("one.torrent");
        std::fs::write(&tf, &t).unwrap();
        core.add_source(tf.display().to_string()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while tf.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(!tf.exists(), "the .torrent file was finished");
        let trashed: Vec<_> = std::fs::read_dir(dir.join("Trash")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(trashed.len(), 1, "{trashed:?}");
        core.shutdown(5);
    }

    #[test]
    fn limit_speed_from_the_app_outlives_a_settings_change_and_a_restart() {
        let (core, dir, _tmp) = real_core();
        core.set_alt_speed(true);
        assert!(core.snapshot().alt_speed);
        assert!(SettingsStore::new(&dir).config().transfers.alt.enabled, "kept in config.toml, as the CLI keeps it");
        // Any other change of the settings: the engine applies the file again a moment later.
        let mut s = core.settings_load();
        (s.transfers.download_limit, s.transfers.alt_download_limit) = (7_000_000, 3_000_000);
        core.settings_save(s).unwrap();
        let applied = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(100));
            matches!(core.snapshot().down_limit, Some(3_000_000 | 7_000_000))
        });
        assert!(applied, "the saved file was applied");
        let s = core.snapshot();
        assert!(s.alt_speed && s.down_limit == Some(3_000_000), "Limit Speed is still on: {:?}", s.down_limit);
        assert!(core.shutdown(5).finished);
        drop(core);
        let options = CoreOptions { config_dir: dir.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None };
        let core = Core::new(options).unwrap();
        assert!(core.snapshot().alt_speed, "and after a restart");
        core.set_alt_speed(false);
        assert!(!SettingsStore::new(&dir).config().transfers.alt.enabled);
        assert!(core.shutdown(5).finished);
    }

    /// A download nobody serves (it stays downloading), named so that the classifier is sure of it and it
    /// starts without asking.
    fn add_movie(core: &Core, dir: &std::path::Path) -> String {
        let name = "Sintel.2010.1080p.BluRay.x264-GRP.mkv";
        let mut t = format!("d4:infod6:lengthi40000e4:name{}:{name}12:piece lengthi65536e6:pieces20:", name.len()).into_bytes();
        t.extend_from_slice(&[5u8; 20]);
        t.extend_from_slice(b"ee");
        let tf = dir.join("movie.torrent");
        std::fs::write(&tf, &t).unwrap();
        core.add_source(tf.display().to_string()).unwrap()
    }

    /// Waits up to 5 s for the state of `id` to satisfy `want`.
    fn wait_state(core: &Core, id: &str, want: impl Fn(Option<TransferState>) -> bool) -> bool {
        (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            want(state_of(core, id))
        })
    }

    fn running(s: Option<TransferState>) -> bool {
        matches!(s, Some(TransferState::Downloading | TransferState::Queued | TransferState::Checking))
    }

    struct Quiet;
    impl CoreListener for Quiet {
        fn on_transfer_finished(&self, _: TransferRow) {}
        fn on_error(&self, _: Option<String>, _: String) {}
        fn on_goal_reached(&self, _: TransferRow) {}
        fn on_needs_choice(&self, _: TransferRow) {}
        fn on_auto_added(&self, _: AutoAdded) {}
        fn on_duplicate(&self, _: String, _: u32) {}
        fn on_add_request(&self, _: AddRequestInfo) {}
        fn on_agent_activity(&self, _: AgentActivity) {}
        fn on_settings_changed(&self, _: String, _: Vec<String>, _: Vec<String>) {}
        fn on_settings_rejected(&self, _: String, _: String) {}
        fn on_locations_changed(&self, _: u32, _: u64) {}
        fn on_attention_changed(&self) {}
        fn on_pending_changed(&self) {}
    }

    /// The setting changed in `config.toml` by something else than Settings (the CLI, an edit) while on
    /// battery: the engine's reload tells the rule (from inside the engine's runtime).
    #[test]
    fn a_setting_changed_in_the_file_on_battery_applies() {
        let (core, dir, tmp) = real_core();
        core.set_listener(Arc::new(Quiet));
        let id = add_movie(&core, tmp[1].path());
        assert!(wait_state(&core, &id, running));
        core.power_changed(true);
        assert!(core.snapshot().alt_speed, "Limit, the default");
        let store = SettingsStore::new(&dir);
        let mut s = store.load_settings();
        s.general.on_battery = OnBatteryMode::Pause;
        store.save_settings(&s).unwrap();
        // No snapshot is read meanwhile, so the rule has to read a fresh one when the reload comes.
        core.resume(Vec::new());
        std::thread::sleep(std::time::Duration::from_millis(3500));
        assert_eq!(state_of(&core, &id), Some(TransferState::Paused));
        assert!(!core.snapshot().alt_speed);
        assert!(core.shutdown(5).finished);
    }

    /// The app's pause on battery is stored as running: after a quit on battery the download comes back
    /// running, and the host's report at start pauses it again.
    #[test]
    fn a_quit_on_battery_brings_the_downloads_back_and_the_report_at_start_pauses_them_again() {
        let (core, dir, tmp) = real_core();
        set_on_battery(&core, OnBatteryMode::Pause);
        let id = add_movie(&core, tmp[1].path());
        let wait = |core: &Core, want: &dyn Fn(Option<TransferState>) -> bool| wait_state(core, &id, want);
        assert!(wait(&core, &running), "{:?}", state_of(&core, &id));
        core.power_changed(true);
        assert!(wait(&core, &|s| s == Some(TransferState::Paused)), "paused on battery: {:?}", state_of(&core, &id));
        assert!(core.shutdown(5).finished);
        drop(core);

        let options = CoreOptions { config_dir: dir.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None };
        let core = Core::new(options).unwrap();
        assert!(running(state_of(&core, &id)), "back running after the quit: {:?}", state_of(&core, &id));
        core.power_changed(true);
        assert!(wait(&core, &|s| s == Some(TransferState::Paused)), "paused again at start: {:?}", state_of(&core, &id));
        core.power_changed(false);
        assert!(wait(&core, &running), "and running on AC: {:?}", state_of(&core, &id));
        assert!(core.shutdown(5).finished);
    }

    #[test]
    fn real_core_starts_snapshots_and_stops() {
        // The engine's guard treats $HOME as the real home under test: point HOME at one temp
        // folder and every folder of the config at another.
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nothing else here reads the environment.
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = work.path().join("state");
        let store = SettingsStore::new(&dir);
        let mut st = store.load_settings();
        st.folders.download_dir = work.path().join("done").display().to_string();
        st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
        st.folders.watch.clear();
        st.folders.torrent_after_add = TorrentFileAction::Leave;
        st.network.port_mapping = false;
        assert_eq!(st.transfers.start_delay_secs, 10, "the grace period defaults to ten seconds");
        assert_eq!(st.transfers.private_min_seed_secs, 10 * 86_400, "private torrents are held 10 days by default");
        st.transfers.private_min_seed_secs = 3 * 86_400;
        st.transfers.start_delay_secs = 4;
        store.save_settings(&st).unwrap();
        // Saved and read back.
        assert_eq!(store.load_settings().transfers.start_delay_secs, 4);
        assert_eq!(store.load_settings().transfers.private_min_seed_secs, 3 * 86_400);
        let core = Core::new(CoreOptions {
            config_dir: dir.display().to_string(),
            mock: false,
            allow_loopback_trackers: false,
            forbidden_root: None,
        })
        .unwrap();
        let s = core.snapshot();
        assert_eq!(s.sharing_total, 0);
        assert!(s.trackers.iter().any(|t| t.id == "public"));
        assert!(core.control_socket().is_some());
        let st = core.settings_load();
        assert_eq!(st.network.listen_port, 0, "an automatic port is not written back");
        assert!(s.listen_port > 0, "the engine listens on a random port");
        assert!(core.add_source("garbage".into()).is_err());

        // File priorities through the FFI: a three-file torrent (hashes are not checked until data arrives).
        let mut t = b"d4:infod5:filesld6:lengthi40000e4:pathl5:a.binee".to_vec();
        t.extend_from_slice(b"d6:lengthi40000e4:pathl3:sub5:b.mkvee");
        t.extend_from_slice(b"d4:attr1:p6:lengthi40000e4:pathl4:.pad5:40000ee");
        t.extend_from_slice(b"e4:name4:pack12:piece lengthi65536e6:pieces40:");
        t.extend_from_slice(&[7u8; 40]);
        t.extend_from_slice(b"ee");
        let tf = work.path().join("pack.torrent");
        std::fs::write(&tf, &t).unwrap();
        let id = core.add_source(tf.display().to_string()).unwrap();
        core.pause(vec![id.clone()]);
        let files = core.files(id.clone()).unwrap();
        assert_eq!(files.iter().map(|f| f.index).collect::<Vec<_>>(), [0, 1], "padding is not listed");
        assert!(files.iter().all(|f| f.priority == FilePriority::Normal && f.wanted));
        assert!(files[1].is_video);
        core.set_file_priorities(
            id.clone(),
            vec![
                FilePriorityChange { index: 0, priority: FilePriority::Skip },
                FilePriorityChange { index: 1, priority: FilePriority::High },
                FilePriorityChange { index: 2, priority: FilePriority::Skip },
            ],
        )
        .unwrap();
        let files = core.files(id.clone()).unwrap();
        assert_eq!((files[0].priority, files[0].wanted), (FilePriority::Skip, false));
        assert_eq!((files[1].priority, files[1].wanted), (FilePriority::High, true));
        assert!(core.set_file_priorities(id.clone(), vec![FilePriorityChange { index: 9, priority: FilePriority::Low }]).is_err());
        assert!(matches!(core.set_file_priorities("nope".into(), vec![]), Err(CoreError::NotFound { .. })));

        // The grace period (SPEC 7A.7) through the FFI: a new torrent is pending with a deadline.
        let pack = |name: &str| {
            let mut t = format!("d4:infod6:lengthi40000e4:name{}:{name}12:piece lengthi65536e6:pieces20:", name.len()).into_bytes();
            t.extend_from_slice(&[9u8; 20]);
            t.extend_from_slice(b"ee");
            let f = work.path().join(format!("{name}.torrent"));
            std::fs::write(&f, &t).unwrap();
            f.display().to_string()
        };
        let pending_row = |id: &str| core.snapshot().downloading.into_iter().find(|r| r.id == id);
        let night = pack("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv");
        let a = core.add_source(night.clone()).unwrap();
        let p = pending_row(&a).and_then(|r| r.pending).expect("a pending row");
        assert!(!p.paused && p.remaining_ms > 2500 && p.remaining_ms <= 4000, "{p:?}");
        assert_eq!(pending_row(&a).unwrap().state, TransferState::Queued);
        assert!(!pending_row(&a).unwrap().awaiting_choice);
        core.pending_pause(a.clone()).unwrap();
        assert!(pending_row(&a).unwrap().pending.unwrap().paused);
        core.pending_resume(a.clone()).unwrap();
        assert!(!pending_row(&a).unwrap().pending.unwrap().paused);
        // Choose Files…: the countdown ends, the torrent stays held for the chooser.
        assert!(core.pending_choose(a.clone()).unwrap());
        let r = pending_row(&a).unwrap();
        assert!(r.pending.is_none() && r.awaiting_choice, "{r:?}");
        assert!(matches!(core.pending_pause(a.clone()), Err(CoreError::InvalidSource { .. })), "no longer pending");
        // Cancel: removed, and its .torrent file in the Trash (the QA one: a test never uses the real Trash).
        let bin = work.path().join("bin");
        // SAFETY: as for HOME above; the core reads it only during the cancel.
        unsafe { std::env::set_var("SEEDWELL_QA_TRASH_DIR", &bin) };
        assert!(core.has_held_source_file(a.clone()));
        assert_eq!(core.cancel_held(a.clone(), false), None);
        let night = std::path::Path::new(&night);
        assert!(!night.exists() && bin.join(night.file_name().unwrap()).is_file(), "the file is in the Trash");
        assert!(core.info(a).is_err(), "and the torrent is gone");
        // Cancel in the countdown, keeping the file: the engine does not cancel it, so it is removed.
        let c = core.add_source(pack("Big.Buck.Bunny.2008.1080p.BluRay.x264-GRP.mkv")).unwrap();
        assert!(pending_row(&c).unwrap().pending.is_some());
        assert_eq!(core.cancel_held(c.clone(), true), None);
        let gone = (0..100).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            core.info(c.clone()).is_err()
        });
        assert!(gone, "removed");
        assert!(work.path().join("Big.Buck.Bunny.2008.1080p.BluRay.x264-GRP.mkv.torrent").is_file(), "the file is kept");
        // Start Now.
        let b = core.add_source(pack("Sintel.2010.1080p.BluRay.x264-GRP.mkv")).unwrap();
        assert!(pending_row(&b).unwrap().pending.is_some());
        core.pending_start_now(b.clone()).unwrap();
        assert!(core.snapshot().downloading.iter().chain(&core.snapshot().queued).all(|r| r.id != b || r.pending.is_none()));
        assert!(core.pending_start_now("nope".into()).is_err());
        let r = core.shutdown(5);
        assert!(r.finished);
    }
}
