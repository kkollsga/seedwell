//! The contract the engine (real or mock) implements. `Core` forwards every call here unchanged.

use std::sync::Arc;

use crate::types::{
    AgentActivity, Category, ChoiceInfo, CoreError, DiagnosticsInfo, MoveProposalInfo, MoveResult, CoreListener, DailyStat, Priority, RowQuery, SeedingGoal,
    SharingCounts, Snapshot, StreamInfo, TorrentFile, TorrentInfo, TrackerStats, TransferRow,
};

/// The facts a rule looks at, per torrent (rule preview).
#[derive(Clone, Debug)]
pub struct AttrRow {
    pub id: String,
    pub profile: Option<String>,
    pub private: bool,
    pub category: Category,
}

/// What the notifier can do with a torrent that waits to start (SPEC 7A.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingAct {
    Pause,
    Resume,
    StartNow,
    /// Cancel the countdown; the torrent stays held for the chooser.
    Choose,
}

/// What a shutdown achieved.
#[derive(Clone, Copy, Debug, Default)]
pub struct ShutdownInfo {
    pub confirmed: u32,
    pub abandoned: u32,
    pub finished: bool,
}

/// Everything the UI can ask of the core. Implementations must be cheap to call from any thread.
pub trait Backend: Send + Sync {
    fn snapshot(&self) -> Snapshot;
    fn set_listener(&self, listener: Arc<dyn CoreListener>);
    /// Finished torrents (the Sharing view), filtered and paged.
    fn rows(&self, q: &RowQuery) -> Vec<TransferRow>;
    fn sharing_counts(&self, tracker_id: Option<&str>, category: Option<Category>, text: &str) -> SharingCounts;
    /// The sidebar tracker id of a transfer (hidden or not), for deciding about a notification.
    fn tracker_of(&self, _id: &str) -> Option<String> {
        None
    }
    /// The primary announce hosts of the torrents of an automatic tracker (for "Save as Favorite").
    fn tracker_hosts(&self, group_id: &str) -> Vec<String>;
    fn tracker_stats(&self, tracker_id: &str) -> TrackerStats;
    fn tracker_daily(&self, tracker_id: &str, days: u32) -> Vec<DailyStat>;
    fn info(&self, id: &str) -> Result<TorrentInfo, CoreError>;
    /// The real announce URL (passkey included) of tracker `index` in `info(id).trackers`, for the
    /// pasteboard. Never logged.
    fn tracker_url(&self, id: &str, index: u32) -> Result<String, CoreError>;
    fn magnet_link(&self, id: &str) -> Result<String, CoreError>;
    fn add_source(&self, source: &str) -> Result<String, CoreError>;
    fn pause(&self, ids: &[String]);
    fn resume(&self, ids: &[String]);
    /// A pause the app makes by itself (on battery, SPEC §8.4): for this run only (stored as running),
    /// and only running downloads are touched. The user's pause or resume of one makes the pause theirs.
    fn pause_by_app(&self, ids: &[String]);
    /// Ends the app's own pause of those it still holds; one the user paused or resumed since is left alone.
    fn resume_by_app(&self, ids: &[String]);
    fn move_to_top(&self, id: &str);
    fn remove(&self, ids: &[String], delete_files: bool);
    fn set_priority(&self, ids: &[String], priority: Priority);
    fn set_category(&self, ids: &[String], category: Category);
    fn set_seeding_goal(&self, ids: &[String], goal: SeedingGoal);
    fn set_rule(&self, ids: &[String], rule: Option<String>);
    fn verify(&self, ids: &[String]);
    /// Asks the trackers again now (manual re-announce). Seedwell's own failure backoff never holds it
    /// back; each tracker's minimum interval does unless `force` ("Announce anyway", one per tracker
    /// per 5 minutes), so this is never a way to hammer one.
    fn ask_tracker_again(&self, ids: &[String], force: bool) -> crate::types::AskAgainOutcome;
    fn pause_all(&self);
    fn resume_all(&self);
    fn clear_recent(&self);
    fn set_limits(&self, down: Option<u64>, up: Option<u64>);
    /// The user's Limit Speed toggle; it also ends the app's own.
    fn set_alt_speed(&self, on: bool);
    /// Limit Speed switched by the app itself (on battery): this run only, never in `config.toml`.
    fn set_alt_speed_by_app(&self, on: bool);
    fn torrent_attrs(&self) -> Vec<AttrRow>;
    fn agent_activity(&self, limit: u32) -> Vec<AgentActivity>;
    /// Agents' live leases on keeping the Mac awake (`None`: no agent asked). Cheap.
    fn agents_awake(&self) -> Option<crate::types::AgentsAwake>;
    /// Path of the control socket agents connect to (`None` for the mock).
    fn control_socket(&self) -> Option<String>;
    /// Starts (once) and waits for the shutdown of the engine, at most `deadline_secs`.
    fn shutdown(&self, deadline_secs: u32) -> ShutdownInfo;
    /// Lowers the wait of a shutdown in progress (logout / power off: 3 s, SPEC §12 #16).
    fn shorten_shutdown(&self, secs: u32);
    fn diagnostics(&self) -> DiagnosticsInfo;
    fn location_moves(&self) -> MoveProposalInfo;
    fn move_torrents(&self, ids: &[String]) -> MoveResult;
    fn move_to(&self, ids: &[String], path: &std::path::Path) -> Result<MoveResult, CoreError>;
    fn cancel_moves(&self, ids: &[String]);
    /// The chooser of a torrent (held, or running when `reopen`).
    fn choice(&self, id: &str, reopen: bool) -> Result<ChoiceInfo, CoreError>;
    /// Answers a chooser: one flag per torrent file index (`true` = download).
    fn confirm_choice(&self, id: &str, files: &[bool]) -> Result<(), CoreError>;
    /// Pauses, resumes or starts the countdown of a pending torrent (SPEC 7A.7). For `Choose` the
    /// answer says whether the chooser can open now (a magnet link has to fetch its metadata first).
    fn pending_act(&self, id: &str, _act: PendingAct) -> Result<bool, CoreError> {
        Err(CoreError::NotFound { id: id.into() })
    }
    /// Cancels a held torrent: it is removed and leaves nothing on disk.
    fn cancel_choice(&self, id: &str) -> Result<(), CoreError>;
    /// The `.torrent` file a held torrent came from (SPEC 7A.6), for Cancel to move to the Trash: read
    /// now, and only returned when it is a regular `.torrent` file with the torrent's info-hash.
    fn held_source(&self, _id: &str) -> Option<String> {
        None
    }
    /// Whether a held torrent has such a file (cheap, nothing is read; for showing the Shift variant).
    fn has_held_source(&self, _id: &str) -> bool {
        false
    }
    /// A folder that stands in for the Trash when Cancel moves that file or a removed synced folder goes there (`None`:
    /// the system's Trash): the mock's own, so that design work never fills the real one.
    fn trash_dir(&self) -> Option<std::path::PathBuf> {
        None
    }
    /// `.torrent` files that were already in a watch folder when it joined the list (SPEC 7A.1).
    fn watch_backlog(&self, folder: &std::path::Path) -> Vec<String>;
    /// The files of a torrent (padding files left out), with their indices.
    fn files(&self, id: &str) -> Result<Vec<TorrentFile>, CoreError>;
    /// Changes the priority of some files (padding files cannot be changed). A change while downloading
    /// starts or stops fetching at once; a finished torrent with a newly wanted file downloads again.
    fn set_file_priorities(&self, id: &str, changes: &[crate::types::FilePriorityChange]) -> Result<(), CoreError>;
    /// A loopback URL for playing a file while it downloads (SPEC §7F). Selects the file if it was
    /// skipped; `complete` means the file finished and there is no URL.
    fn stream_url(&self, id: &str, file_index: u32) -> Result<StreamInfo, CoreError>;
    /// Closes a stream by its token (unknown tokens are ignored).
    fn stop_stream(&self, _token: &str) {}
    /// The OS reports whether a usable network path exists (`NWPathMonitor`; SPEC 7C.4c).
    fn network_path_changed(&self, _satisfied: bool) {}
    /// The Mac is about to sleep.
    fn system_will_sleep(&self) {}
    /// The Mac woke.
    fn system_did_wake(&self) {}
    /// A volume was mounted or unmounted.
    fn volumes_changed(&self) {}
    /// Set when the configured listening port was taken at start (SPEC 7F settings note, Network tab).
    fn port_fallback(&self) -> Option<crate::types::PortFallbackInfo> {
        None
    }
    /// The port the engine listens on now (0 when unknown).
    fn listen_port(&self) -> u16 {
        0
    }
    /// Whether peers on the Internet can reach the listening port (`None` when unknown).
    fn reachability(&self) -> Option<crate::types::ReachabilityInfo> {
        None
    }
    /// Settings, rules or profiles were saved by the app: let the engine pick them up now.
    fn settings_saved(&self) {}
    /// Deletes the developer tracker log (the engine's writer starts a fresh file).
    fn tracker_log_clear(&self, dir: &std::path::Path) {
        for n in ["trackers.log", "trackers.log.1", "trackers.log.2"] {
            let _ = std::fs::remove_file(dir.join(n));
        }
    }
    /// Log lines the writer dropped since the app started (it was too slow).
    fn tracker_log_dropped(&self) -> u64 {
        0
    }
    /// Folder sync (SPEC 7G): the service next to the engine, or the mock's fake folders.
    fn sync(&self) -> Arc<dyn crate::sync::SyncBackend> {
        Arc::new(crate::sync::NoSync)
    }
}
