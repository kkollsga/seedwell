//! The core's slow calls as `async` functions of the FFI (docs/CROSS-PLATFORM.md §1: the host's UI thread never waits
//! for the core).
//!
//! Each call here returns a future at once and does its work elsewhere ([`crate::calls`]): the user's commands to the
//! engine on the core's command lane, one after another in the order they were given; the user's changes to folder
//! sync's and chat's settings likewise on a lane of their own; reads, folder sync, chat, updates and Play on a thread
//! of their own. The host awaits them (Swift: `try await core.pause(ids:)` from a `Task`;
//! C#: `await core.Pause(ids)`). Under the FFI's name of each call there is a synchronous Rust function of the same
//! name (not exported) that does the work; Rust callers and tests use those.
//!
//! What stays synchronous is cheap by construction and proven so (`ui_wait_tests`): the snapshots, the view models,
//! the polls a host makes every second.

use std::sync::Arc;

use crate::chat_files::{ChatFileCard, ChatFileOpen};
use crate::chat_images::{ChatImageDraft, ChatImageInput, ChatImageView};
use crate::devices_text::{DevicesView, PairLinkView, RemoveDeviceView};
use crate::calls::{Polled, UI_WAIT};
use crate::held::CancelNote;
use crate::sync_share::{SyncShareCopied, SyncShareForm};
use crate::*;

/// The reads a host makes synchronously while it draws (a context menu's checkmarks, a Play entry, a tracker page's
/// warnings), served without waiting for a busy engine or a slow disk: a fresh read when it comes within
/// [`UI_WAIT`], else the last one.
#[derive(Default)]
pub(crate) struct Polls {
    info: Polls1<Result<TorrentInfo, CoreError>>,
    files: Polls1<Result<Vec<TorrentFile>, CoreError>>,
    volumes: Polls1<Vec<VolumeWarning>>,
}

struct Polls1<V>(Polled<String, V>);

impl<V: Clone + Send + 'static> Default for Polls1<V> {
    fn default() -> Self {
        Polls1(Polled::new(std::time::Duration::ZERO))
    }
}

impl Core {
    /// Get Info's details of a torrent, read within [`UI_WAIT`] or as last read (a context menu's checkmarks).
    pub(crate) fn info_now(&self, id: &str) -> Result<TorrentInfo, CoreError> {
        let (backend, key) = (self.backend.clone(), id.to_string());
        self.polls.info.0.get(id.to_string(), UI_WAIT, move || backend.info(&key)).unwrap_or_else(|| {
            Err(CoreError::Failed { message: "The torrent could not be read yet".into() })
        })
    }

    /// The files of a torrent, read within [`UI_WAIT`] or as last read; empty when there is nothing yet (Play entries).
    pub(crate) fn files_now(&self, id: &str) -> Vec<TorrentFile> {
        self.files_read(id).unwrap_or_default()
    }

    /// The files of a torrent, read within [`UI_WAIT`] or as last read; `None` when they have not been read yet.
    pub(crate) fn files_read(&self, id: &str) -> Option<Vec<TorrentFile>> {
        let (backend, key) = (self.backend.clone(), id.to_string());
        self.polls.files.0.get(id.to_string(), UI_WAIT, move || backend.files(&key)).and_then(Result::ok)
    }

    /// The different-disk warnings of a favorite's folders, looked at within [`UI_WAIT`] or as last looked at.
    pub(crate) fn tracker_volume_warnings_now(&self, profile: &ProfileRecord) -> Vec<VolumeWarning> {
        let key = format!("{:?} {:?} {:?} {:?}", profile.id, profile.incomplete_dir, profile.finished_dir, profile.categories);
        let (config, p) = (self.settings.config(), profile.clone());
        self.polls.volumes.0.get(key, UI_WAIT, move || settings::tracker_volume_warnings(&config, &p)).unwrap_or_default()
    }
}

impl Core {
    /// Runs `f` on the core's command lane, after every command given before it (a Pause and the Resume right after it
    /// reach the engine in that order).
    pub(crate) fn in_order<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Core) -> T + Send + 'static,
    ) -> impl std::future::Future<Output = T> + Send + 'static {
        let me = self.clone();
        self.commands.run(move || f(&me))
    }

    /// Runs `f` on the core's lane of folder sync and chat settings, after every such change given before it.
    pub(crate) fn in_sync_order<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Core) -> T + Send + 'static,
    ) -> impl std::future::Future<Output = T> + Send + 'static {
        let me = self.clone();
        self.sync_commands.run(move || f(&me))
    }

    /// Runs `f` on a thread of its own: a read or a long job that holds nothing else up.
    pub(crate) fn apart<T: Send + 'static>(
        self: &Arc<Self>,
        f: impl FnOnce(&Core) -> T + Send + 'static,
    ) -> impl std::future::Future<Output = T> + Send + 'static {
        let me = self.clone();
        crate::calls::apart(move || f(&me))
    }
}

/// `<in_order|in_sync_order|apart> fn <async name> = <sync name> "<FFI name>" (<args>) -> <result>;`: an exported `async` method of
/// [`Core`] named `<FFI name>` for the hosts, which runs the synchronous `<sync name>` on the lane or apart.
macro_rules! core_async {
    ($( $(#[$m:meta])* $how:ident fn $name:ident = $sync:ident $ffi:tt ( $($arg:ident : $ty:ty),* $(,)? ) $(-> $ret:ty)? ; )*) => {
        #[uniffi::export]
        impl Core {
            $(
                $(#[$m])*
                #[uniffi::method(name = $ffi)]
                pub async fn $name(self: Arc<Self>, $($arg: $ty),*) $(-> $ret)? {
                    self.$how(move |c| c.$sync($($arg),*)).await
                }
            )*
        }
    };
}

core_async! {
    // ------------------------------------------------------------ the user's commands to the engine, in order

    /// Adds a magnet link, `.torrent` file path or http(s) URL; the new transfer's id.
    in_order fn add_source_async = add_source "add_source" (source: String) -> Result<String, CoreError>;
    /// Pauses torrents (the user's pause).
    in_order fn pause_async = pause "pause" (ids: Vec<String>);
    in_order fn resume_async = resume "resume" (ids: Vec<String>);
    in_order fn move_to_top_async = move_to_top "move_to_top" (id: String);
    /// Removes torrents; with `delete_files` their files too.
    in_order fn remove_async = remove "remove" (ids: Vec<String>, delete_files: bool);
    in_order fn set_priority_async = set_priority "set_priority" (ids: Vec<String>, priority: Priority);
    in_order fn set_category_async = set_category "set_category" (ids: Vec<String>, category: Category);
    in_order fn set_seeding_goal_async = set_seeding_goal "set_seeding_goal" (ids: Vec<String>, goal: SeedingGoal);
    /// Forces a rule (by id) on torrents; `None` lets the rules decide again.
    in_order fn set_rule_async = set_rule "set_rule" (ids: Vec<String>, rule: Option<String>);
    /// Re-verifies the files of the torrents.
    in_order fn verify_async = verify "verify" (ids: Vec<String>);
    /// Stops sharing (pauses a seeding torrent).
    in_order fn stop_sharing_async = stop_sharing "stop_sharing" (ids: Vec<String>);
    in_order fn pause_all_async = pause_all "pause_all" ();
    in_order fn resume_all_async = resume_all "resume_all" ();
    /// Forgets the Recent list (the finished torrents keep seeding).
    in_order fn clear_recent_async = clear_recent "clear_recent" ();
    /// Global limits in bytes/s; `None` is unlimited. Kept in `config.toml`.
    in_order fn set_limits_async = set_limits "set_limits" (down: Option<u64>, up: Option<u64>);
    /// The user's Limit Speed toggle, kept in `config.toml`.
    in_order fn set_alt_speed_async = set_alt_speed "set_alt_speed" (on: bool);
    /// Validates and saves the settings; the engine applies the file within seconds.
    in_order fn settings_save_async = settings_save "settings_save" (settings: AppSettings) -> Result<(), CoreError>;
    /// Validates and saves the whole rule list; returns it with ids filled in.
    in_order fn rules_save_async = rules_save "rules_save" (doc: RulesDoc) -> Result<RulesDoc, CoreError>;
    /// Validates and saves the tracker profiles; returns them with ids filled in.
    in_order fn profiles_save_async = profiles_save "profiles_save" (profiles: Vec<ProfileRecord>) -> Result<Vec<ProfileRecord>, CoreError>;
    /// Moves torrents to where the settings say they belong.
    in_order fn move_torrents_async = move_torrents "move_torrents" (ids: Vec<String>) -> MoveResult;
    /// "Move to…": below `path`, pinned there.
    in_order fn move_to_async = move_to "move_to" (ids: Vec<String>, path: String) -> Result<MoveResult, CoreError>;
    in_order fn cancel_moves_async = cancel_moves "cancel_moves" (ids: Vec<String>);
    /// Answers a chooser: one flag per torrent file index (`true` = download).
    in_order fn confirm_choice_async = confirm_choice "confirm_choice" (id: String, files: Vec<bool>) -> Result<(), CoreError>;
    /// Start on a torrent that waits for a choice, with the default files.
    in_order fn confirm_choice_default_async = confirm_choice_default "confirm_choice_default" (id: String) -> Result<(), CoreError>;
    /// Stops the countdown of a torrent that waits to start.
    in_order fn pending_pause_async = pending_pause "pending_pause" (id: String) -> Result<(), CoreError>;
    in_order fn pending_resume_async = pending_resume "pending_resume" (id: String) -> Result<(), CoreError>;
    in_order fn pending_start_now_async = pending_start_now "pending_start_now" (id: String) -> Result<(), CoreError>;
    /// "Choose Files…": `true` when the chooser can open now; `false` for a magnet link fetching its metadata.
    in_order fn pending_choose_async = pending_choose "pending_choose" (id: String) -> Result<bool, CoreError>;
    /// Cancels a held torrent: it is removed and nothing was ever written.
    in_order fn cancel_choice_async = cancel_choice "cancel_choice" (id: String) -> Result<(), CoreError>;
    /// Cancel on a held torrent (removed; its `.torrent` file to the Trash unless `keep_file`): the note to show.
    in_order fn cancel_held_async = cancel_held "cancel_held" (id: String, keep_file: bool) -> Option<CancelNote>;
    /// Sets the priority of the listed files (indices from `files`).
    in_order fn set_file_priorities_async = set_file_priorities "set_file_priorities" (id: String, changes: Vec<FilePriorityChange>) -> Result<(), CoreError>;
    /// Deletes the tracker log.
    in_order fn tracker_log_clear_async = tracker_log_clear "tracker_log_clear" ();
    /// The torrent proxy on or off (the shield icon's click with a key installed, SPEC §7H): the new model. Refused
    /// without a key (the host opens the window then).
    in_order fn proxy_toggle_async = proxy_toggle "proxy_toggle" () -> Result<ProxyModel, CoreError>;
    /// Where the user is, for the torrent proxy's server choice (a code from the model's `countries`, "" none);
    /// chosen by hand only. The new model.
    in_order fn proxy_set_country_async = proxy_set_country "proxy_set_country" (code: String) -> Result<ProxyModel, CoreError>;
    /// Forgets the torrent proxy's key; torrents connect directly again.
    in_order fn proxy_remove_key_async = proxy_remove_key "proxy_remove_key" () -> Result<(), CoreError>;

    // ------------------------------------------------------------ reads and round trips, each on its own thread

    /// The user's Approve or Deny of an agent's request for access (a button, the menu, a notification's action): the
    /// change is made (a setting saved, a channel's switch, ...), and what happened is said.
    apart fn agent_request_decide_async = agent_request_decide "agent_request_decide" (id: String, approve: bool) -> AgentRequestAnswer;

    /// Tests NordVPN service credentials with NordVPN's documented SOCKS5 servers (a few seconds at most), keeps them
    /// in the secret store and turns the torrent proxy on (SPEC §7H). Wrong ones keep nothing and say so. The sentence
    /// to show.
    apart fn proxy_set_nordvpn_key_async = proxy_set_nordvpn_key "proxy_set_nordvpn_key" (user: String, pass: String) -> Result<String, CoreError>;
    /// "Find a Faster Server": tests every server of the torrent proxy now, switches to the best, and says the
    /// outcome (a few seconds at most).
    apart fn proxy_find_faster_async = proxy_find_faster "proxy_find_faster" () -> Result<String, CoreError>;
    /// A SOCKS5 proxy of the user's own (username and password both or neither): tested, kept, turned on.
    apart fn proxy_set_custom_async = proxy_set_custom "proxy_set_custom" (host: String, port: u16, user: Option<String>, pass: Option<String>) -> Result<String, CoreError>;

    /// "Ask Tracker Again": a manual re-announce.
    apart fn ask_tracker_again_async = ask_tracker_again "ask_tracker_again" (ids: Vec<String>) -> AskAgainOutcome;
    /// "Announce anyway": also past the tracker's minimum interval.
    apart fn announce_anyway_async = announce_anyway "announce_anyway" (ids: Vec<String>) -> AskAgainOutcome;
    /// Lifetime totals of one tracker (its header).
    apart fn tracker_stats_async = tracker_stats "tracker_stats" (tracker_id: String) -> TrackerStats;
    /// Daily up/down buckets for the last `days` days, oldest first.
    apart fn tracker_daily_async = tracker_daily "tracker_daily" (tracker_id: String, days: u32) -> Vec<DailyStat>;
    /// The real announce URL of one tracker line of Get Info, passkey included: for "Copy URL" only.
    apart fn tracker_url_async = tracker_url "tracker_url" (id: String, index: u32) -> Result<String, CoreError>;
    /// Incomplete/finished pairs on different disks, for the saved settings (looks at each folder's disk).
    apart fn volume_warnings_async = volume_warnings "volume_warnings" () -> Vec<VolumeWarning>;
    /// The different-disk warnings of a favorite's own folders.
    apart fn tracker_volume_warnings_async = tracker_volume_warnings "tracker_volume_warnings" (profile: ProfileRecord) -> Vec<VolumeWarning>;
    /// What Settings shows of this computer's agent tools, looked at again (`settings_view_host`).
    apart fn refresh_settings_host_facts_async = refresh_settings_host_facts "refresh_settings_host_facts" () -> crate::settings_view_host::SettingsHostFacts;
    /// The torrents a change of locations leaves elsewhere than the settings say (nothing moves).
    apart fn location_moves_async = location_moves "location_moves" () -> MoveProposalInfo;
    /// The chooser of a torrent that waits for a choice (or, with `reopen`, of a running one).
    apart fn choice_async = choice "choice" (id: String, reopen: bool) -> Result<ChoiceInfo, CoreError>;
    /// The `.torrent` file a held torrent came from, if it is still there and still this torrent's.
    apart fn held_source_file_async = held_source_file "held_source_file" (id: String) -> Option<String>;
    /// Whether `held_source_file` has a file to offer, without reading it.
    apart fn has_held_source_file_async = has_held_source_file "has_held_source_file" (id: String) -> bool;
    /// `.torrent` files that were in a watch folder before it joined the list.
    apart fn watch_backlog_async = watch_backlog "watch_backlog" (folder: String) -> Vec<String>;
    /// The files of a torrent.
    apart fn files_async = files "files" (id: String) -> Result<Vec<TorrentFile>, CoreError>;
    /// A local URL to play a downloading file in the user's own player.
    apart fn stream_url_async = stream_url "stream_url" (torrent_id: String, file_index: u32) -> Result<StreamInfo, CoreError>;
    /// A Play command: what the host starts, or the note why nothing starts.
    apart fn play_async = play "play" (row: TransferRow, file_index: Option<u32>, player_id: Option<String>) -> crate::player::PlayStart;
    /// The `.torrent` files already in a watch folder, offered once ever at start (reads the folders).
    apart fn backlog_prompt_async = backlog_prompt "backlog_prompt" () -> Option<BacklogPrompt>;
    /// A tracker's Info page: its stats and settings, read now.
    apart fn tracker_info_read_async = tracker_info_read "tracker_info_read" (tracker: TrackerSummary) -> TrackerInfoRead;

    // ------------------------------------------------------------ folder sync

    /// Starts sharing a folder on this computer (it becomes its owner).
    apart fn sync_add_folder_async = sync_add_folder "sync_add_folder" (path: String) -> Result<SyncFolder, CoreError>;
    /// Joins a shared folder with an invitation link, at `path` (empty: the default place).
    apart fn sync_join_async = sync_join "sync_join" (key_or_link: String, path: String) -> Result<SyncFolder, CoreError>;
    apart fn sync_create_invite_async = sync_create_invite "sync_create_invite" (id: String, role: SyncRole, max_uses: u32, valid_days: u32, channel_id: Option<String>) -> Result<SyncInvite, CoreError>;
    apart fn sync_revoke_invite_async = sync_revoke_invite "sync_revoke_invite" (id: String, invite_id: String) -> Result<(), CoreError>;
    /// Renames the folder on this computer (its directory).
    apart fn sync_rename_async = sync_rename "sync_rename" (id: String, name: String) -> Result<SyncFolder, CoreError>;
    /// Moves the folder on this computer to `path` (the full new path); resolves once the files moved.
    apart fn sync_move_async = sync_move "sync_move" (id: String, path: String) -> Result<SyncFolder, CoreError>;
    /// Stops syncing the folder here; with `trash_files` it then goes to the Trash. `Some`: the alert to show.
    apart fn sync_remove_async = sync_remove "sync_remove" (id: String, trash_files: bool) -> Result<Option<crate::sync_text::SyncAlert>, CoreError>;
    apart fn sync_reshare_async = sync_reshare "sync_reshare" (id: String) -> Result<SyncFolder, CoreError>;
    apart fn sync_set_role_async = sync_set_role "sync_set_role" (id: String, device_id: String, role: SyncRole) -> Result<(), CoreError>;
    apart fn sync_remove_device_async = sync_remove_device "sync_remove_device" (id: String, device_id: String) -> Result<(), CoreError>;
    apart fn sync_resolve_conflict_async = sync_resolve_conflict "sync_resolve_conflict" (id: String, copy: String, keep: SyncKeep) -> Result<(), CoreError>;
    /// Scans the whole folder now.
    apart fn sync_rescan_async = sync_rescan "sync_rescan" (id: String) -> Result<(), CoreError>;
    /// How many of a folder's files no other device is known to hold (reads its whole index).
    apart fn sync_only_here_async = sync_only_here "sync_only_here" (id: String) -> u64;
    /// The conflict copies in a folder (reads its whole index).
    apart fn sync_conflicts_async = sync_conflicts "sync_conflicts" (id: String) -> Vec<SyncConflict>;
    apart fn sync_set_share_until_async = sync_set_share_until "sync_set_share_until" (id: String, until_s: u64) -> Result<(), CoreError>;
    apart fn sync_set_channel_async = sync_set_channel "sync_set_channel" (id: String, channel_id: Option<String>) -> Result<(), CoreError>;
    apart fn sync_pause_async = sync_pause "sync_pause" (id: String, paused: bool) -> Result<(), CoreError>;
    apart fn sync_set_lan_only_async = sync_set_lan_only "sync_set_lan_only" (id: String, lan_only: bool) -> Result<(), CoreError>;
    /// The Add Folder window's Add (or Join).
    apart fn sync_add_start_async = sync_add_start "sync_add_start" (form: SyncAddForm) -> Result<SyncFolder, CoreError>;
    /// The Edit Folder window's Save (a rename, a move).
    apart fn sync_edit_save_async = sync_edit_save "sync_edit_save" (form: SyncEditForm) -> Result<SyncFolder, CoreError>;
    /// The Share window's Copy Link (adds a new folder first).
    apart fn sync_share_copy_link_async = sync_share_copy_link "sync_share_copy_link" (form: SyncShareForm) -> Result<SyncShareCopied, CoreError>;
    apart fn sync_set_on_demand_async = sync_set_on_demand "sync_set_on_demand" (id: String, on_demand: bool, path: Option<String>) -> Result<SyncFolder, CoreError>;
    apart fn sync_add_folder_on_demand_async = sync_add_folder_on_demand "sync_add_folder_on_demand" (path: String) -> Result<SyncFolder, CoreError>;
    apart fn sync_join_on_demand_async = sync_join_on_demand "sync_join_on_demand" (link: String, place: String) -> Result<SyncFolder, CoreError>;
    apart fn sync_set_follow_async = sync_set_follow "sync_set_follow" (id: String, on: bool) -> Result<(), CoreError>;
    apart fn sync_set_read_only_async = sync_set_read_only "sync_set_read_only" (id: String, on: bool) -> Result<(), CoreError>;
    /// The Share window's "On all my devices" (applied at once).
    apart fn sync_share_set_devices_async = sync_share_set_devices "sync_share_set_devices" (form: SyncShareForm, on: bool) -> Result<SyncShareForm, CoreError>;
    /// The Share window's "Read only" (applied at once; the form's next role).
    apart fn sync_share_set_read_only_async = sync_share_set_read_only "sync_share_set_read_only" (form: SyncShareForm, on: bool) -> Result<SyncShareForm, CoreError>;

    // ------------------------------------------------------------ devices

    apart fn devices_pair_link_async = devices_pair_link "devices_pair_link" () -> Result<PairLinkView, CoreError>;
    apart fn devices_join_async = devices_join "devices_join" (link: String) -> Result<DevicesView, CoreError>;
    apart fn devices_confirm_async = devices_confirm "devices_confirm" (device_id: String) -> Result<(), CoreError>;
    apart fn devices_refuse_async = devices_refuse "devices_refuse" (device_id: String) -> Result<(), CoreError>;
    apart fn devices_remove_view_async = devices_remove_view "devices_remove_view" (device_id: String) -> Result<RemoveDeviceView, CoreError>;
    apart fn devices_remove_async = devices_remove "devices_remove" (device_id: String, places: Vec<String>) -> Result<(), CoreError>;
    apart fn devices_leave_async = devices_leave "devices_leave" () -> Result<(), CoreError>;
    apart fn devices_start_over_async = devices_start_over "devices_start_over" () -> Result<DevicesView, CoreError>;

    // ------------------------------------------------------------ chat

    apart fn chat_new_async = chat_new "chat_new" (name: String, agents: bool) -> Result<SyncChannel, CoreError>;
    apart fn chat_join_async = chat_join "chat_join" (link: String) -> Result<SyncChannel, CoreError>;
    /// The New Channel window's Create (or Join).
    apart fn chat_new_start_async = chat_new_start "chat_new_start" (form: ChatNewForm) -> Result<SyncChannel, CoreError>;
    apart fn chat_send_async = chat_send "chat_send" (id: String, topic: Option<String>, body: String, reply_to: Option<String>) -> Result<SyncMessage, CoreError>;
    apart fn chat_send_direct_async = chat_send_direct "chat_send_direct" (device_id: String, body: String, reply_to: Option<String>) -> Result<SyncMessage, CoreError>;
    apart fn chat_new_topic_async = chat_new_topic "chat_new_topic" (channel_id: String, title: String) -> Result<SyncTopic, CoreError>;
    apart fn chat_leave_async = chat_leave "chat_leave" (id: String) -> Result<(), CoreError>;
    apart fn chat_delete_direct_async = chat_delete_direct "chat_delete_direct" (device_id: String) -> Result<(), CoreError>;
    /// A message with images and files: checked here, then prepared (read, hashed) and sent in the background
    /// (`chat_pending_sends` shows it meanwhile).
    apart fn chat_send_attachments_async = chat_send_attachments "chat_send_attachments" (place: String, topic: Option<String>, body: String, reply_to: Option<String>, inputs: Vec<ChatImageInput>) -> Result<(), CoreError>;
    apart fn chat_file_download_async = chat_file_download "chat_file_download" (place: String, hash: String, to: Option<String>) -> Result<ChatFileCard, CoreError>;
    apart fn chat_file_cancel_async = chat_file_cancel "chat_file_cancel" (place: String, hash: String) -> Result<ChatFileCard, CoreError>;
    apart fn chat_file_stop_sharing_async = chat_file_stop_sharing "chat_file_stop_sharing" (place: String, hash: String) -> Result<ChatFileCard, CoreError>;
    apart fn chat_file_open_async = chat_file_open "chat_file_open" (place: String, hash: String) -> Result<ChatFileOpen, CoreError>;
    /// An image picked or pasted for a message: read and checked.
    apart fn chat_image_check_async = chat_image_check "chat_image_check" (image: ChatImageInput) -> Result<ChatImageDraft, CoreError>;
    apart fn chat_image_preview_async = chat_image_preview "chat_image_preview" (channel_id: String, hash: String) -> Option<ChatImageView>;
    apart fn chat_image_view_async = chat_image_view "chat_image_view" (channel_id: String, hash: String, max_side: u32) -> Option<ChatImageView>;
    apart fn chat_image_fetch_async = chat_image_fetch "chat_image_fetch" (channel_id: String, hash: String) -> Result<(), CoreError>;
    apart fn chat_image_save_async = chat_image_save "chat_image_save" (channel_id: String, hash: String, path: String, as_png: bool) -> Result<(), CoreError>;
    apart fn chat_image_open_path_async = chat_image_open_path "chat_image_open_path" (channel_id: String, hash: String) -> Result<String, CoreError>;

    // ------------------------------------------------------------ folder sync's and chat's settings, in order

    /// No notifications for the channel (an @mention still notifies: the app's rule).
    in_sync_order fn chat_set_muted_async = chat_set_muted "chat_set_muted" (id: String, muted: bool);
    /// The channel's Agents: Yes/No, for every member device (owners and admins).
    in_sync_order fn chat_set_agents_async = chat_set_agents "chat_set_agents" (id: String, open: bool) -> Result<(), CoreError>;
    /// The command run for each message from another device while agents may read the channel; `None` or empty: none.
    in_sync_order fn chat_set_wake_command_async = chat_set_wake_command "chat_set_wake_command" (id: String, command: Option<String>) -> Result<(), CoreError>;
    /// The channel's name on this device.
    in_sync_order fn chat_rename_async = chat_rename "chat_rename" (id: String, name: String) -> Result<(), CoreError>;
    /// Saves the device-wide settings of folder sync.
    in_sync_order fn set_sync_settings_async = set_sync_settings "set_sync_settings" (settings: SyncSettings) -> Result<(), CoreError>;
    /// Moves folder sync to `port`: refused when another app has it; otherwise sync starts again on it in the
    /// background (`sync_settings().port` shows it a few seconds later).
    in_sync_order fn set_sync_port_async = set_sync_port "set_sync_port" (port: u16) -> Result<(), CoreError>;
    /// Sets up the rendezvous with a Cloudflare API token, in the background (`sync_rendezvous` shows how it goes).
    in_sync_order fn sync_rendezvous_set_up_async = sync_rendezvous_set_up "sync_rendezvous_set_up" (token: String) -> Result<(), CoreError>;
    /// Uses a rendezvous address the user already has, after testing it, in the background.
    in_sync_order fn sync_rendezvous_use_async = sync_rendezvous_use "sync_rendezvous_use" (url: String) -> Result<(), CoreError>;
    /// Tests the rendezvous in use, in the background.
    in_sync_order fn sync_rendezvous_test_async = sync_rendezvous_test "sync_rendezvous_test" () -> Result<(), CoreError>;
    /// Stops using the rendezvous, in the background.
    in_sync_order fn sync_rendezvous_turn_off_async = sync_rendezvous_turn_off "sync_rendezvous_turn_off" () -> Result<(), CoreError>;
}
