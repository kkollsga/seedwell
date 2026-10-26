//! The records and enums that cross the Swift boundary. Plain data; no behaviour.

/// How a transfer is fetched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TransferKind {
    Torrent,
    Direct,
}

/// Detected content type (SPEC §7A.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Category {
    Movie,
    Tv,
    Music,
    Audiobook,
    Book,
    Software,
    Other,
    Game,
}

/// Lifecycle state of a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum TransferState {
    /// Verifying files.
    Checking,
    /// A magnet is fetching its metadata.
    Metadata,
    Downloading,
    Queued,
    Paused,
    /// Seeding with a peer actively downloading from us.
    Seeding,
    /// Seeding but idle: announcing on schedule only (SPEC §5).
    LowPower,
    Error,
    /// Files are gone (unplugged volume, deleted); nothing is overwritten.
    MissingData,
    /// A direct download that finished.
    Completed,
}

/// Where a finished torrent sits in the Sharing view (SPEC §8.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, uniffi::Enum)]
pub enum SeedGroup {
    Popular,
    Active,
    Dormant,
    Error,
    Dead,
    /// Finished and stopped by the user or by a seeding goal. Not one of the five groups of the
    /// spec, but a stopped torrent has to be somewhere the user can resume it.
    Stopped,
}

/// Queue / bandwidth priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum Priority {
    High,
    Normal,
    Low,
}

/// One row in a list; the same record is used for every list in the UI.
#[derive(Clone, Debug, uniffi::Record)]
pub struct TransferRow {
    /// 40 hex characters (torrent id) or the direct download's id.
    pub id: String,
    /// The raw name (release name): Get Info, tooltips, search.
    pub name: String,
    /// What lists show: "Sintel (2010)", "Fallen Kingdom — S02". Computed once per torrent.
    pub display_name: String,
    pub kind: TransferKind,
    pub category: Category,
    pub size_bytes: u64,
    pub done_bytes: u64,
    /// Bytes per second.
    pub down_rate: u64,
    /// Bytes per second.
    pub up_rate: u64,
    pub eta_secs: Option<u64>,
    /// Tracker profile id, `public` or `other-private`.
    pub tracker_id: String,
    /// Tracker profile name, "Public", or the tracker host of an unprofiled private torrent.
    pub tracker_name: String,
    pub ratio: f64,
    pub ratio_goal: Option<f64>,
    pub seed_secs: u64,
    pub state: TransferState,
    /// Unix seconds.
    pub finished_at: Option<u64>,
    /// Absolute path of the file or folder.
    pub save_path: String,
    /// Lower-case extension without dot; empty for folders.
    pub file_ext: String,
    /// Sharing group of a finished torrent; `None` for downloads and direct downloads.
    pub seed_group: Option<SeedGroup>,
    /// Short reason for an error ("Tracker unreachable"), plain language.
    pub error: Option<String>,
    /// Seconds until the next retry of a failing tracker.
    pub retry_in_secs: Option<u64>,
    /// Percent done while the files are being moved to a new location.
    pub moving_percent: Option<u8>,
    /// Held until the user chooses what to download (SPEC §7A.6).
    pub awaiting_choice: bool,
    /// A private torrent (BEP 27).
    #[uniffi(default = false)]
    pub is_private: bool,
    /// Held for the grace period before an auto-add starts (SPEC §7A.7): silent on the network, the
    /// countdown belongs to the engine. `state` is `Queued` meanwhile.
    pub pending: Option<PendingStart>,
    pub peers: u32,
    /// Seeders + leechers the tracker reported at its last announce; `None` when it gave none.
    pub swarm: Option<u32>,
    /// Peer addresses known (connected or still to try): the fallback swarm size.
    pub known_peers: u32,
    /// The torrent holds a video file (Play button). Worked out once per torrent from its file list.
    pub has_video: bool,
    /// Lower-case extension of the largest video in a multi-file torrent (its icon); empty when none or not known yet.
    pub video_ext: String,
    /// Files in the torrent (padding left out); 0 while not known yet. More than 1 gets the row a disclosure triangle.
    pub file_count: u32,
    /// Unix seconds of the last upload or interested peer.
    pub last_activity: Option<u64>,
    /// Why a download is not moving ("Tracker: Unregistered torrent", "Waiting for peers"); the
    /// engine's words, shown instead of "Calculating time remaining…". `None` while data flows.
    pub waiting_note: Option<String>,
    /// `waiting_note` is a problem (error colour).
    pub waiting_is_error: bool,
    /// Every tracker halted on a permanent-looking reason: "Ask Tracker Again" is the way out.
    pub tracker_halted: bool,
    /// `error` is only a temporary tracker outage (not permanent, younger than a day): shown in the
    /// secondary colour, no attention dot, not counted in the menu's errors.
    #[uniffi(default = false)]
    pub error_transient: bool,
    /// An announce has been in flight since this unix ms; rows show "Announcing…" after half a second.
    #[uniffi(default = None)]
    pub announcing_since_ms: Option<u64>,
    /// Failed tries in a row while `error_transient` ("retrying in 9 min (attempt 3)").
    #[uniffi(default = 0)]
    pub retry_attempt: u32,
    /// Lifetime payload bytes uploaded (the Sharing row's "3 GB uploaded").
    #[uniffi(default = 0)]
    pub uploaded_bytes: u64,
    /// Files fully downloaded (padding left out); 0 while not known yet. Of `file_count`.
    #[uniffi(default = 0)]
    pub files_done: u32,
    /// Why a torrent stopped when the inactive condition (SPEC §7.5) did it: "0.3 GB uploaded in
    /// the last 150 h", the engine's words; the row reads "Stopped — …".
    #[uniffi(default = None)]
    pub idle_stopped: Option<String>,
    /// Size of the whole torrent, skipped files included (0 while the metadata is unknown).
    #[uniffi(default = 0)]
    pub total_bytes: u64,
    /// Verified bytes held of the whole torrent, skipped files included.
    #[uniffi(default = 0)]
    pub held_bytes: u64,
    /// Progress of the current download: wanted bytes verified beyond what the torrent held when
    /// it became a download again (a fresh download starts from 0). Progress is
    /// `session_done / session_size`; 0 for both = use `done_bytes / size_bytes`.
    #[uniffi(default = 0)]
    pub session_done: u64,
    /// Wanted bytes minus that starting point.
    #[uniffi(default = 0)]
    pub session_size: u64,
}

/// What "Ask Tracker Again" did.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct AskAgainOutcome {
    /// Trackers asked now.
    pub sent: u32,
    /// Trackers that must wait for their minimum interval.
    pub deferred: u32,
    /// Seconds until the earliest deferred ask is allowed.
    pub wait_secs: Option<u64>,
}

/// The countdown of a torrent that waits to start (SPEC §7A.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PendingStart {
    /// Unix milliseconds at which it starts (meaningless while `paused`).
    pub starts_at_ms: u64,
    /// Milliseconds left when the row was built.
    pub remaining_ms: u64,
    /// The countdown is stopped; the torrent stays held until resumed or started.
    pub paused: bool,
}

/// One tracker entry in the sidebar.
#[derive(Clone, Debug, uniffi::Record)]
pub struct TrackerSummary {
    /// A favorite's profile id, an automatic tracker's registrable domain (`example.org`),
    /// `public` or `other-private`.
    pub id: String,
    pub name: String,
    /// Torrents of this tracker, whatever their state.
    pub count: u32,
    /// A favorite: a stored profile that can have settings (SPEC 7E.1). Automatic trackers,
    /// Public and Other private are not.
    pub favorite: bool,
    /// A favorite in hidden mode (SPEC 7E.5): the sidebar does not list it; it stays here so its
    /// tab still works and Settings can open it.
    #[uniffi(default = false)]
    pub hidden: bool,
    /// With `hidden`: only the sharing side is hidden (downloads, and their notifications, show as usual).
    #[uniffi(default = false)]
    pub hide_only_seeding: bool,
    /// At least one of its torrents is private: only then does it have a peer exchange setting
    /// (SPEC §7B.4). Always true for Other private, never for Public.
    #[uniffi(default = false)]
    pub private: bool,
}

/// Lifetime totals of one tracker (SPEC §7E.2); read when its header is shown, not every second.
#[derive(Clone, Debug, uniffi::Record)]
pub struct TrackerStats {
    pub lifetime_down: u64,
    pub lifetime_up: u64,
    pub torrents_completed: u32,
    pub files_downloaded: u32,
    pub seed_secs: u64,
}

/// Which lines of the developer tracker log the viewer wants (all filters optional, empty = any).
#[derive(Clone, Debug, Default, uniffi::Record)]
pub struct TrackerLogQuery {
    /// Part of a torrent id, a title, or a tracker host.
    pub text: String,
    /// Part of a tracker host.
    pub tracker: String,
    /// `started`, `regular`, `completed`, `stopped`, or empty.
    pub event: String,
    pub errors_only: bool,
    /// At most this many (the newest); 0 = 300.
    pub limit: u32,
}

/// One line of the developer tracker log, ready to show. Secrets are already masked in the file.
#[derive(Clone, Debug, uniffi::Record)]
pub struct TrackerLogRow {
    pub ts_ms: u64,
    /// `announce`, `scrape`, `decision`, `gate`, `quit`.
    pub kind: String,
    pub torrent: String,
    /// First 8 hex digits of the torrent id.
    pub id: String,
    pub title: String,
    pub tracker: String,
    pub host: String,
    /// `HTTP`, `HTTPS`, `UDP` or empty.
    pub transport: String,
    pub event: String,
    pub error: bool,
    /// One line: what happened.
    pub summary: String,
    /// Request, response and schedule, several lines.
    pub detail: String,
}

/// Counts of the Sharing groups.
#[derive(Clone, Copy, Debug, Default, uniffi::Record)]
pub struct SharingCounts {
    pub popular: u32,
    pub active: u32,
    pub dormant: u32,
    pub error: u32,
    pub dead: u32,
    pub stopped: u32,
}

impl SharingCounts {
    pub fn total(&self) -> u32 {
        self.popular + self.active + self.dormant + self.error + self.dead + self.stopped
    }

    pub(crate) fn add(&mut self, g: SeedGroup) {
        match g {
            SeedGroup::Popular => self.popular += 1,
            SeedGroup::Active => self.active += 1,
            SeedGroup::Dormant => self.dormant += 1,
            SeedGroup::Error => self.error += 1,
            SeedGroup::Dead => self.dead += 1,
            SeedGroup::Stopped => self.stopped += 1,
        }
    }
}

/// Everything the main window and the menu need in one call.
#[derive(Clone, Debug, uniffi::Record)]
pub struct Snapshot {
    pub down_rate: u64,
    pub up_rate: u64,
    /// Downloading, checking, fetching metadata and awaiting a choice, in queue order.
    pub downloading: Vec<TransferRow>,
    /// Waiting or paused downloads.
    pub queued: Vec<TransferRow>,
    /// Finished downloads, newest first (until "Clear").
    pub recent: Vec<TransferRow>,
    /// What hidden mode (SPEC 7E.5) left out of `downloading`, `queued` and `recent`: the
    /// tracker's own tab, notifications and the sleep assertion still need them.
    #[uniffi(default = [])]
    pub hidden_downloading: Vec<TransferRow>,
    #[uniffi(default = [])]
    pub hidden_queued: Vec<TransferRow>,
    #[uniffi(default = [])]
    pub hidden_recent: Vec<TransferRow>,
    /// The busiest few seeding torrents (the menu's Sharing submenu).
    pub sharing_busiest: Vec<TransferRow>,
    pub sharing: SharingCounts,
    pub sharing_total: u32,
    pub trackers: Vec<TrackerSummary>,
    /// True while the alternative ("Limit Speed") limits are on.
    pub alt_speed: bool,
    /// The download limit in effect (global, or the Limit Speed one while it is on), bytes/s; `None` = unlimited.
    pub down_limit: Option<u64>,
    /// The upload limit in effect, bytes/s; `None` = unlimited.
    pub up_limit: Option<u64>,
    /// Torrents with a problem that needs the user (the menu's "· 2 errors"): Dead, and Error other than a
    /// temporary tracker outage.
    pub error_count: u32,
    /// Aggregate progress (0..=100) of the moves under way, weighted by size; `None` when no
    /// torrent is being moved (the menu bar icon's green bar).
    pub moving_percent: Option<u8>,
    /// Something needs the user: an error that needs action, a torrent awaiting a choice (a temporary
    /// tracker outage does not count).
    pub attention: bool,
    pub listen_port: u16,
    pub connections: u32,
    /// False while the engine has declared a network outage (SPEC 7C.4c): it sends almost
    /// nothing until the network is back. The window and the menu show a quiet "Offline".
    pub online: bool,
    /// Unix milliseconds at which the outage began, while there is one.
    pub offline_since_ms: Option<u64>,
}

/// One day of tracker traffic for the 30-day chart; `days_ago == 0` is today.
#[derive(Clone, Debug, uniffi::Record)]
pub struct DailyStat {
    pub days_ago: u32,
    pub down_bytes: u64,
    pub up_bytes: u64,
}

/// What to list from the Sharing view; paged so hundreds of dormant torrents stay cheap.
#[derive(Clone, Debug, uniffi::Record)]
pub struct RowQuery {
    pub group: Option<SeedGroup>,
    /// Profile id, `public` or `other-private`.
    pub tracker_id: Option<String>,
    /// Case-insensitive substring of the name; empty = all.
    pub text: String,
    /// Only this category; none = all.
    #[uniffi(default = None)]
    pub category: Option<Category>,
    pub offset: u32,
    pub limit: u32,
}

/// A tracker problem as Get Info explains it. Words from the engine; never an announce URL.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TrackerProblem {
    /// The tracker's host ("tracker-z.cc"), empty when not known.
    pub host: String,
    /// What is wrong, in a sentence a non-expert understands.
    pub message: String,
    /// Unix seconds when it began.
    pub since: Option<u64>,
    /// Unix milliseconds of the next try; `None` when Seedwell will not try by itself.
    pub next_try_ms: Option<u64>,
    /// Passing (not permanent, under a day): nothing to do.
    pub temporary: bool,
    /// A reassuring line when it applies ("Your files are fine. ...").
    pub reassurance: Option<String>,
}

/// Details shown in the Get Info popover.
#[derive(Clone, Debug, uniffi::Record)]
pub struct TorrentInfo {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub category: Category,
    pub size_bytes: u64,
    pub peers: u32,
    pub down_rate: u64,
    pub save_path: String,
    pub priority: Priority,
    /// Human-readable seeding goal, e.g. "Seed to ratio 1.0".
    pub seeding_goal: String,
    /// The inactive condition's live figure, from the engine: "Uploaded in the last 150 h: 0.3 GB
    /// (stops below 1 GB)". `None` when the condition is off for this torrent.
    #[uniffi(default = None)]
    pub idle_text: Option<String>,
    pub files_wanted: u32,
    pub files_total: u32,
    /// e.g. "sample skipped"; empty when nothing was skipped.
    pub skipped_note: String,
    pub rule_name: String,
    pub tracker_name: String,
    /// A disk problem (missing files, a failed write), in words.
    pub error: Option<String>,
    /// What the trackers said, when they fail or refused (`None` otherwise).
    #[uniffi(default = None)]
    pub tracker_problem: Option<TrackerProblem>,
    /// Every tracker halted on a permanent-looking reason ("Ask Tracker Again" applies).
    pub tracker_halted: bool,
    /// The goal is the user's own choice, not the rule's.
    pub goal_is_custom: bool,
    pub state: TransferState,
    /// Unix seconds.
    pub added_at: u64,
    pub finished_at: Option<u64>,
    pub seeded_secs: u64,
    pub ratio: f64,
    pub uploaded_total: u64,
    pub downloaded_total: u64,
    pub piece_count: u32,
    pub piece_length: u32,
    /// Lower-case hex info-hash (v1).
    pub info_hash: String,
    pub is_private: bool,
    /// Connected peers that are seeds.
    pub seeds_connected: u32,
    /// Tracker-reported swarm of the last announce.
    pub swarm_seeders: Option<u32>,
    pub swarm_leechers: Option<u32>,
    pub trackers: Vec<TrackerLine>,
}

/// One announce URL of a torrent, as Get Info lists it.
#[derive(Clone, Debug, uniffi::Record)]
pub struct TrackerLine {
    /// The full URL with every secret part replaced by `••••••`. Safe to show and to log.
    pub url: String,
    pub tier: u32,
    /// "announced", "failed: ..."
    pub status: String,
    /// Unix seconds.
    pub last_announce: Option<u64>,
    /// Unix seconds.
    pub next_announce: Option<u64>,
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    /// How the tracker is doing: `waiting`, `working`, `warning`, `backoff`, `halted`.
    #[uniffi(default = "waiting")]
    pub state: String,
    /// Failed tries in a row.
    #[uniffi(default = 0)]
    pub failures: u32,
    /// Unix seconds at which a failed announce is tried again.
    #[uniffi(default = None)]
    pub retry_at: Option<u64>,
    /// Unix seconds of the last announce the tracker answered.
    #[uniffi(default = None)]
    pub last_success: Option<u64>,
    /// Unix seconds before which an Ask Tracker Again waits (the tracker's minimum interval).
    #[uniffi(default = None)]
    pub manual_ok_at: Option<u64>,
}

/// Seeding goal choices offered in the UI.
#[derive(Clone, Copy, Debug, PartialEq, uniffi::Enum)]
pub enum SeedingGoal {
    RuleDefault,
    Ratio { ratio: f64 },
    Forever,
}

/// Errors surfaced to the UI.
#[derive(Debug, uniffi::Error)]
pub enum CoreError {
    InvalidSource { message: String },
    NotFound { id: String },
    /// Settings, rules or profiles failed validation; one `path: message` per problem.
    Invalid { problems: Vec<String> },
    Failed { message: String },
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSource { message } => write!(f, "{message}"),
            Self::NotFound { id } => write!(f, "no such transfer: {id}"),
            Self::Invalid { problems } => write!(f, "{}", problems.join("; ")),
            Self::Failed { message } => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CoreError {}

/// How a group of the chooser is labelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum ChoiceGroupKind {
    Season,
    Specials,
    Collection,
    Album,
    Platform,
    Extras,
    Other,
}

/// One selectable thing in the chooser: an episode, a film, an album, a title, a build.
#[derive(Clone, Debug, uniffi::Record)]
pub struct ChoiceItemInfo {
    /// `S03E01`, `Night of the Living Dead (1968)`, ...
    pub label: String,
    pub bytes: u64,
    /// Torrent file indices of the item.
    pub files: Vec<u32>,
    /// Ticked in the preselection.
    pub selected: bool,
    /// The library already has this exact file.
    pub in_library: bool,
    /// The library has the same item at another quality.
    pub other_quality: bool,
    /// `1080p WEB-DL`.
    pub quality: Option<String>,
    /// Software: the build is meant for this Mac.
    pub this_mac: bool,
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct ChoiceGroupInfo {
    pub label: String,
    pub kind: ChoiceGroupKind,
    pub items: Vec<ChoiceItemInfo>,
}

/// The chooser's content (SPEC §7A.6): structure, preselection and the reasons to ask.
#[derive(Clone, Debug, uniffi::Record)]
pub struct ChoiceInfo {
    pub id: String,
    pub name: String,
    pub display_name: String,
    pub tracker: String,
    /// The torrent is held right now (before anything is downloaded or announced).
    pub awaiting: bool,
    /// Plain-language reasons, e.g. "Seasons 1 and 2 are already in your library".
    pub reasons: Vec<String>,
    /// The classifier is unsure: highlight the category picker.
    pub unsure: bool,
    /// `single`, `tv`, `movies`, `music`, `books`, `audiobooks`, `software`, `flat` or empty.
    pub structure: String,
    pub groups: Vec<ChoiceGroupInfo>,
    pub file_count: u32,
    pub total_bytes: u64,
    pub selected_bytes: u64,
    pub free_bytes: Option<u64>,
    pub destination: Option<String>,
    pub category: Category,
    pub confidence: f32,
    pub library_checked: bool,
    /// The rule that will govern the torrent in this category ("Y – seed to 1.0", or "Default").
    pub rule_name: String,
    /// What that rule does when the download is done, from the engine: "seeds until ratio 1.0".
    pub then_text: String,
    /// The whole line for a terminal or an agent: "Rule Y · Saves to ~/Media/Movies · Then seeds until ratio 1.0".
    pub summary_text: String,
}

/// Counters for the Advanced tab (SPEC §8.4); all monotonic since launch except `connections`.
#[derive(Clone, Debug, uniffi::Record)]
pub struct DiagnosticsInfo {
    /// Wake-ups of the session, tracker and torrent actors: the idle-cost number.
    pub wakeups: u64,
    pub connections: u32,
    pub rechecks: u64,
    pub hash_failures: u64,
    pub peers_banned: u64,
    pub promotions: u64,
    pub demotions: u64,
}

/// One torrent that a change of locations would move (SPEC §7A.5).
#[derive(Clone, Debug, uniffi::Record)]
pub struct MoveItemInfo {
    pub id: String,
    pub name: String,
    pub bytes: u64,
    pub from: String,
    pub to: String,
    /// A verified copy to another disk instead of an instant rename.
    pub cross_volume: bool,
}

/// "Move the N existing torrents (X GB) to the new location?"
#[derive(Clone, Debug, uniffi::Record)]
pub struct MoveProposalInfo {
    pub items: Vec<MoveItemInfo>,
    pub total_bytes: u64,
    pub cross_volume: u32,
}

/// What `move_torrents` / `move_to` did.
#[derive(Clone, Debug, uniffi::Record)]
pub struct MoveResult {
    pub queued: u32,
    /// Plain-language reasons for the torrents that were not queued.
    pub refused: Vec<String>,
}

/// One agent action for the Agents tab and notifications ("Claude paused 3 transfers").
#[derive(Clone, Debug, uniffi::Record)]
pub struct AgentActivity {
    /// Unix milliseconds.
    pub at_ms: u64,
    pub client: String,
    pub method: String,
    pub summary: String,
    /// `ok`, `denied` or `error: <code>`.
    pub outcome: String,
}

/// Agents that asked to keep the Mac awake while they work (SPEC §9A.7).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct AgentsAwake {
    /// What is working ("kglite"), the one that lasts longest first, each once.
    pub who: Vec<String>,
    /// Unix milliseconds: when the last lease is over unless renewed.
    pub until_ms: u64,
}

/// What a watch folder found that the user may want to add (SPEC §7A.1 "Ask first").
#[derive(Clone, Debug, uniffi::Record)]
pub struct AddRequestInfo {
    pub path: String,
    pub name: String,
    pub total_bytes: u64,
    pub category: Category,
    pub tracker: String,
}

/// An automatic add, for the notification of SPEC §7A.1.
#[derive(Clone, Debug, uniffi::Record)]
pub struct AutoAdded {
    pub id: String,
    pub name: String,
    pub category: Category,
    pub tracker: String,
    pub destination: Option<String>,
}

/// Rare, push-style events. Called from a core thread; the Swift side must hop to the main actor.
#[uniffi::export(with_foreign)]
pub trait CoreListener: Send + Sync {
    fn on_transfer_finished(&self, row: TransferRow);
    fn on_error(&self, id: Option<String>, message: String);
    /// A seeding goal was reached (the torrent stopped or was removed per its rule).
    fn on_goal_reached(&self, row: TransferRow);
    /// A torrent is held until the user chooses what to download.
    fn on_needs_choice(&self, row: TransferRow);
    fn on_auto_added(&self, added: AutoAdded);
    /// The torrent was already added. `trackers_added` is how many announce URLs of the new
    /// source were merged into it (public torrents only; 0 = none).
    fn on_duplicate(&self, name: String, trackers_added: u32);
    fn on_add_request(&self, request: AddRequestInfo);
    /// An agent did something (also written to the activity log).
    fn on_agent_activity(&self, activity: AgentActivity);
    /// `config.toml`, `rules.toml` or `trackers.toml` changed on disk or in the app and was applied.
    fn on_settings_changed(&self, file: String, warnings: Vec<String>, restart_required: Vec<String>);
    /// A settings file is invalid; the previous values stay in force.
    fn on_settings_rejected(&self, file: String, error: String);
    /// A change of locations leaves torrents elsewhere than the settings now say: ask
    /// "Move the N existing torrents (X GB) to the new location?" (`location_moves`).
    fn on_locations_changed(&self, torrents: u32, bytes: u64);
    /// Counts that drive the menu bar icon changed (attention dot, downloading).
    fn on_attention_changed(&self);
    /// A torrent started waiting, or its countdown was paused, resumed, finished or cancelled
    /// (SPEC §7A.7): read `snapshot().downloading` rows with `pending` and update the notifier.
    fn on_pending_changed(&self);
}

/// Per-file download priority (Get Info > Files). Skip means "not wanted": the file is not downloaded
/// or seeded, and what is already on disk stays there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum FilePriority {
    Skip,
    Low,
    Normal,
    High,
}

/// One entry of `set_file_priorities`: the file's index in the torrent's list and its new priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct FilePriorityChange {
    pub index: u32,
    pub priority: FilePriority,
}

/// One file of a torrent, for Play (SPEC §7F.3): the file list of Get Info and the row menus.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct TorrentFile {
    /// Index in the torrent's file list (what `stream_url` takes). BEP 47 padding files are left out
    /// of the list but keep their place in the numbering.
    pub index: u32,
    /// Where the file sits in the tree the app shows, `/`-separated: the torrent's own folders without
    /// its root folder; for files placed into the library (SPEC 7A.3), the folders below the
    /// folder all placed files share.
    pub path: String,
    /// Where the file is (or will be) on disk, absolute.
    pub disk_path: String,
    /// The last path component.
    pub name: String,
    pub length: u64,
    /// Verified bytes of this file (approximate for files sharing pieces).
    pub done: u64,
    /// Payload bytes served from this file since the torrent went live (0 for a resting torrent); the
    /// app turns the change between two reads into a rate.
    #[uniffi(default = 0)]
    pub uploaded: u64,
    /// Selected for download (not skipped). Only wanted files can be streamed.
    pub wanted: bool,
    /// Skip, Low, Normal or High (`wanted` is `priority != Skip`).
    pub priority: FilePriority,
    /// The file name says it is a video (by content type of the extension).
    pub is_video: bool,
    /// MP4, M4V or MOV: QuickTime Player (AVFoundation) can play it.
    pub is_mp4_family: bool,
}

/// The configured listening port was taken by another app at start, and the engine uses another
/// one for now (the setting keeps the number the user chose).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PortFallbackInfo {
    pub wanted: u16,
    pub using: u16,
    /// Ready to show: "Port 55252 is in use by another app; using 61234 for now".
    pub message: String,
}

/// Can peers on the Internet reach the listening port (Settings › Network, beside Port mapping).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ReachabilityInfo {
    /// `open`, `mapped`, `checking`, `closed` or `unknown` (as `seedwell status --json`).
    pub state: String,
    /// Ready to show: "Open: 3 peers from the Internet connected in".
    pub summary: String,
}

/// A loopback stream of a downloading file (SPEC §7F.1): open `url` in a player.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct StreamInfo {
    /// `http://127.0.0.1:<port>/<token>/<file name>`. Empty when `complete`.
    pub url: String,
    /// The random per-stream token inside `url`; `stop_stream` takes it. Empty when `complete`.
    pub token: String,
    /// The file finished downloading meanwhile: open the file itself, there is no stream.
    pub complete: bool,
    /// The file had been skipped and is now selected for download.
    pub selected: bool,
    /// From the file extension, e.g. `video/x-matroska`.
    pub content_type: String,
    pub is_mp4_family: bool,
}
