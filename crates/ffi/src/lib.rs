//! trs-ffi: the UniFFI surface between the Rust core and the Swift shell.
//!
//! The UI *pulls* a compact [`Snapshot`] (about once a second, only while something is visible)
//! and sends commands. The core pushes only rare events through [`CoreListener`]. Everything the
//! UI needs about transfers goes through the internal [`Backend`] trait: the real engine
//! (`trs_engine::Host`, in-process) in the app, a simulated library behind `--mock`. Settings,
//! rules and tracker profiles are read and written straight through `trs-state`, in both modes.

mod backend;
mod favorites;
mod mock;
mod play;
mod real;
mod rows;
mod settings;
mod types;

use std::path::PathBuf;
use std::sync::Arc;

pub use backend::{AttrRow, Backend, ShutdownInfo};
pub use settings::*;
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
    /// Isolated runs: refuse to start when the state folder or any folder in `config.toml`
    /// (download, incomplete, watch, archive, categories) lies inside this path (the real home).
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
}

#[uniffi::export]
impl Core {
    /// Opens (or creates) the state in `options.config_dir` and starts the engine.
    #[uniffi::constructor]
    pub fn new(options: CoreOptions) -> Result<Arc<Self>, CoreError> {
        let dir = PathBuf::from(&options.config_dir);
        let backend: Arc<dyn Backend> = if options.mock {
            let _ = std::fs::create_dir_all(&dir);
            mock::MockBackend::start(options.config_dir.clone())
        } else {
            real::RealBackend::start(real::StartOptions {
                socket: dir.join("control.sock"),
                dir: dir.clone(),
                allow_loopback_trackers: options.allow_loopback_trackers,
                forbidden_root: options.forbidden_root.clone().map(PathBuf::from),
            })?
        };
        Ok(Arc::new(Self { backend, settings: SettingsStore::new(&dir), dir }))
    }

    /// Cheap; the shell calls it about once a second while something is visible.
    pub fn snapshot(&self) -> Snapshot {
        self.backend.snapshot()
    }

    /// Registers the event listener, replacing any previous one.
    pub fn set_listener(&self, listener: Arc<dyn CoreListener>) {
        self.backend.set_listener(listener);
    }

    /// Finished torrents (the Sharing view), filtered and paged.
    pub fn sharing_rows(&self, query: RowQuery) -> Vec<TransferRow> {
        self.backend.rows(&query)
    }

    /// Sizes of the Sharing groups, optionally within one tracker and matching a search text.
    pub fn sharing_counts(&self, tracker_id: Option<String>, text: String) -> SharingCounts {
        self.backend.sharing_counts(tracker_id.as_deref(), &text)
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

    /// "Ask Tracker Again": a manual re-announce, allowed once per tracker minimum interval.
    pub fn ask_tracker_again(&self, ids: Vec<String>) -> AskAgainOutcome {
        self.backend.ask_tracker_again(&ids)
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

    pub fn set_alt_speed(&self, on: bool) {
        self.backend.set_alt_speed(on);
    }

    // ---------------------------------------------------------------- settings

    /// Every setting the Settings window edits.
    pub fn settings_load(&self) -> AppSettings {
        self.settings.load_settings()
    }

    /// Validates and saves; the engine applies the file within seconds.
    pub fn settings_save(&self, settings: AppSettings) -> Result<(), CoreError> {
        self.settings.save_settings(&settings)?;
        self.backend.settings_saved();
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

    /// The `.torrent` file a torrent that waits for a decision came from, if it still lies where Seedwell
    /// left it: read now, and only returned when it is a regular `.torrent` file (not a link) with this
    /// torrent's info-hash. Call it right before moving the file to the Trash. `None` for magnet links,
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
            offset: 0,
            limit: 1000,
        };
        let dormant = core.sharing_rows(q(Some(SeedGroup::Dormant), None));
        assert!(dormant.len() > 150);
        let z = core.sharing_rows(q(None, Some("tracker-z.cc")));
        assert!(!z.is_empty() && z.iter().all(|r| r.tracker_id == "tracker-z.cc"));
        let counts = core.sharing_counts(Some("tracker-z.cc".into()), String::new());
        assert_eq!(counts.total() as usize, z.len());
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
        let row = s.downloading.iter().find(|r| !r.file_ext.is_empty()).expect("a single-file download");
        let files = core.files(row.id.clone()).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].index, 0);
        assert_eq!(files[0].is_video, matches!(row.file_ext.as_str(), "mkv" | "mp4" | "avi" | "mov" | "m4v" | "webm" | "wmv" | "ts"));
        if files[0].is_video {
            let s = core.stream_url(row.id.clone(), 0).unwrap();
            assert!(s.url.starts_with("http://127.0.0.1:"));
        }
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
            goal: Some(GoalKind::Ratio { ratio: 3.0 }),
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
        assert!(st.network.listen_port > 0, "the first launch writes the port back");
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
        let a = core.add_source(pack("Night.of.the.Living.Dead.1968.1080p.BluRay.x264-GRP.mkv")).unwrap();
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
        core.cancel_choice(a).unwrap();
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
