//! Public types of the control API: commands' arguments, query results and events.
//!
//! These are plain data. `trs-ffi` and the `seedwell` CLI map them onto their own
//! records; nothing here depends on UniFFI or serde.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::tier::Tier;

pub use trs_classify::Category;

/// Identifies a torrent: the 20-byte swarm id (v1 info-hash, or the truncated v2 hash).
pub type TorrentId = [u8; 20];

/// Lifecycle state shown to the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TorrentState {
    /// Verifying existing data.
    Checking,
    /// Magnet link: fetching the `info` dictionary from peers.
    FetchingMetadata,
    /// Waiting for a download slot.
    Queued,
    /// Downloading.
    Downloading,
    /// Seeding and someone is downloading from us.
    Seeding,
    /// Seeding, idle (Cold or Warm without uploads).
    LowPower,
    /// Stopped by the user or by a seeding goal.
    Paused,
    /// Files are gone (unplugged volume, deleted); nothing is overwritten.
    MissingData,
    /// Failed; see the row's `error`.
    Error,
    /// A direct download that finished (torrents go to Seeding instead).
    Completed,
    /// Held until the user chooses what to download (SPEC 7A.6): a `.torrent` is not announced and
    /// opens no connections, a magnet has its metadata but requests no pieces.
    AwaitingChoice,
    /// Added without asking, and waiting out the grace period before it starts (SPEC 7A.7): held
    /// and silent like `AwaitingChoice`. [`TorrentRow::pending`] has the deadline.
    Pending,
}

impl TorrentState {
    /// Stable lower-case name for logs and the control socket.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::FetchingMetadata => "metadata",
            Self::Queued => "queued",
            Self::Downloading => "downloading",
            Self::Seeding => "seeding",
            Self::LowPower => "low-power",
            Self::Paused => "paused",
            Self::MissingData => "missing-data",
            Self::Error => "error",
            Self::Completed => "completed",
            Self::AwaitingChoice => "awaiting-choice",
            Self::Pending => "pending",
        }
    }
}

/// What a manual re-announce did for one tracker (SPEC 7B.3: the min-interval gate still applies).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReannounceResult {
    /// Torrent.
    pub id: TorrentId,
    /// Tracker URL with the passkey removed.
    pub url_redacted: String,
    /// The announce goes out now; `false` = deferred.
    pub sent: bool,
    /// When a deferred announce is allowed (unix milliseconds).
    pub not_before_ms: Option<u64>,
    /// `Some(n)` when the wait is Seedwell's own backoff after `n` failed tries (not the tracker's
    /// minimum interval).
    pub waiting_after_failures: Option<u32>,
    /// A forced announce was asked but is rate limited (one per tracker per 5 min): unix ms it is allowed.
    pub force_limited_until_ms: Option<u64>,
    /// The tracker's own minimum interval holds the announce until this unix ms (a normal ask waits;
    /// "Announce anyway" overrides it).
    pub min_interval_until_ms: Option<u64>,
}

/// Group of a seeding torrent in the Sharing view (SPEC 8.3), computed for every snapshot row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SeedGroup {
    /// In demand now: at least 3 peers downloading from it, or more than 250 KB/s averaged over 10 minutes.
    Popular,
    /// Uploaded something or had an interested peer in the last 24 hours.
    Active,
    /// Neither for 24 hours; resting in low power, still announcing.
    Dormant,
    /// A problem that began less than 24 hours ago (all trackers failing, files missing, disk error).
    Error,
    /// The error has lasted over 24 hours, or the tracker says the torrent is unregistered.
    Dead,
}

/// What a row of the snapshot is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RowKind {
    /// A BitTorrent download or seed.
    Torrent,
    /// A direct HTTP(S) download (SPEC 7D): never seeded, never part of Sharing.
    Direct,
}

/// Queue and bandwidth priority (SPEC 7.3). Weights 4 : 2 : 1 when limits are saturated.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Priority {
    /// Jumps the queue, never paused by queue limits.
    High,
    /// Default.
    Normal,
    /// Last in line.
    Low,
}

impl Priority {
    /// Share of a saturated limit relative to the others.
    pub fn weight(self) -> u32 {
        match self {
            Self::High => 4,
            Self::Normal => 2,
            Self::Low => 1,
        }
    }

    pub(crate) fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::High,
            2 => Self::Low,
            _ => Self::Normal,
        }
    }

    pub(crate) fn to_u8(self) -> u8 {
        match self {
            Self::High => 0,
            Self::Normal => 1,
            Self::Low => 2,
        }
    }
}

/// Per-file download priority. `Skip` (0) means the file is not downloaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FilePriority {
    /// Do not download.
    Skip,
    /// Low.
    Low,
    /// Normal (default).
    Normal,
    /// High.
    High,
}

impl FilePriority {
    /// Numeric form used in records and the picker (0 = skip).
    pub fn to_u8(self) -> u8 {
        match self {
            Self::Skip => 0,
            Self::Low => 1,
            Self::Normal => 2,
            Self::High => 3,
        }
    }

    /// Inverse of [`FilePriority::to_u8`]; unknown values mean Normal.
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::Skip,
            1 => Self::Low,
            3 => Self::High,
            _ => Self::Normal,
        }
    }
}

/// What to do when the seeding goal is reached (SPEC 7.2 `at_goal`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtGoal {
    /// Stop (pause) the torrent and keep it in the list.
    Stop,
    /// Remove it from the client, keeping the files.
    Remove,
    /// Remove it from the client and delete its files.
    RemoveAndDelete,
}

/// Seeding goal (SPEC 7.2). `None` fields are "no limit"; with nothing set the torrent seeds forever.
// size: 24 bytes
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeedGoal {
    /// Stop at this upload/size ratio.
    pub ratio: Option<f32>,
    /// Stop after seeding this many seconds.
    pub seed_secs: Option<u32>,
    /// Hard floor that overrides the goal (hit-and-run protection).
    pub min_seed_secs: u32,
    /// `true`: the first reached condition wins; `false`: all must be reached.
    pub any: bool,
    /// What to do on reaching the goal.
    pub at_goal: AtGoal,
    /// "Stop when the download finishes": never seed (apart from `min_seed_secs`).
    pub stop_on_finish: bool,
    /// The inactive condition (SPEC §7.5): stop when less than this many MB (decimal) were
    /// uploaded during the last `idle_hours` hours of seeding. 0 = off. Always the rule's (or the
    /// global setting's): a goal chosen by hand does not change it.
    pub idle_mb: u32,
    /// The window of `idle_mb`, hours of seeding.
    pub idle_hours: u16,
}

impl SeedGoal {
    /// Seed forever.
    pub const FOREVER: SeedGoal = SeedGoal {
        ratio: None,
        seed_secs: None,
        min_seed_secs: 0,
        any: true,
        at_goal: AtGoal::Stop,
        stop_on_finish: false,
        idle_mb: 0,
        idle_hours: 0,
    };

    /// Stop at `ratio`.
    pub fn ratio(ratio: f32) -> Self {
        Self {
            ratio: Some(ratio),
            ..Self::FOREVER
        }
    }

    /// Stop the moment the download finishes (rule `stop_when_finished`).
    pub fn stop_when_finished() -> Self {
        Self {
            stop_on_finish: true,
            ..Self::FOREVER
        }
    }

    /// Whether the goal can ever be reached (a forever-seeder needs no goal checks).
    pub fn is_set(&self) -> bool {
        self.stop_on_finish || self.ratio.is_some() || self.seed_secs.is_some() || self.idle_mb != 0
    }

    /// The inactive condition as (bytes, hours), when on.
    pub fn idle(&self) -> Option<(u64, u32)> {
        (self.idle_mb != 0 && self.idle_hours != 0).then(|| (u64::from(self.idle_mb) * 1_000_000, u32::from(self.idle_hours)))
    }

    /// Set the inactive condition from bytes (rounded up to whole MB) and hours.
    pub fn with_idle(mut self, bytes: u64, hours: u32) -> Self {
        self.idle_mb = u32::try_from(bytes.div_ceil(1_000_000)).unwrap_or(u32::MAX);
        self.idle_hours = u16::try_from(hours).unwrap_or(u16::MAX);
        if self.idle_mb == 0 {
            self.idle_hours = 0;
        }
        self
    }

    /// "less than 1 GB in 150 h" (the words after a goal, or alone).
    pub fn idle_clause(&self) -> Option<String> {
        self.idle().map(|(b, h)| format!("less than {} in {h} h", idle_size(b)))
    }

    /// The goal in words, the one text every front end shows ("Seed to ratio 1.0", "Seed forever",
    /// "Stop when finished", "Seed for 3 days", "Ratio 1.0 or 3 days").
    pub fn describe(&self) -> String {
        self.describe_for(false)
    }

    /// [`describe`](Self::describe), with the hit-and-run floor when there is one: "Ratio 1.0 or
    /// 7 days, at least 10 days (private)". `private` adds the tag, since the floor of a private
    /// torrent mostly comes from the global setting (SPEC §7).
    pub fn describe_for(&self, private: bool) -> String {
        format!("{}{}", self.describe_goal(), self.floor_suffix(private))
    }

    fn floor_suffix(&self, private: bool) -> String {
        if self.min_seed_secs == 0 || !self.is_set() {
            return String::new();
        }
        format!(", at least {}{}", goal_duration(self.min_seed_secs), if private { " (private)" } else { "" })
    }

    fn describe_goal(&self) -> String {
        if self.stop_on_finish {
            return "Stop when finished".into();
        }
        let Some(idle) = self.idle_clause() else { return self.describe_base() };
        if self.ratio.is_none() && self.seed_secs.is_none() {
            return format!("Seed until {idle}");
        }
        format!("{}, or {idle}", self.describe_base())
    }

    fn describe_base(&self) -> String {
        match (self.ratio, self.seed_secs) {
            (Some(r), Some(s)) => {
                format!("Ratio {r:.1} {} {}", if self.any { "or" } else { "and" }, goal_duration(s))
            }
            (Some(r), None) => format!("Seed to ratio {r:.1}"),
            (None, Some(s)) => format!("Seed for {}", goal_duration(s)),
            (None, None) => "Seed forever".into(),
        }
    }

    /// The goal as the end of a sentence about a rule: "seeds until ratio 1.0", "seeds forever",
    /// "stops sharing when finished", "seeds for 3 days", "seeds until ratio 1.0 or 3 days".
    pub fn then_phrase(&self) -> String {
        self.then_phrase_for(false)
    }

    /// [`then_phrase`](Self::then_phrase) with the floor: "seeds until ratio 1.0 or 7 days, at
    /// least 10 days (private)".
    pub fn then_phrase_for(&self, private: bool) -> String {
        format!("{}{}", self.then_goal(), self.floor_suffix(private))
    }

    fn then_goal(&self) -> String {
        if self.stop_on_finish {
            return "stops sharing when finished".into();
        }
        let Some(idle) = self.idle_clause() else { return self.then_base() };
        if self.ratio.is_none() && self.seed_secs.is_none() {
            return format!("seeds until {idle}");
        }
        format!("{}, or {idle}", self.then_base())
    }

    fn then_base(&self) -> String {
        match (self.ratio, self.seed_secs) {
            (Some(r), Some(s)) => {
                format!("seeds until ratio {r:.1} {} {}", if self.any { "or" } else { "and" }, goal_duration(s))
            }
            (Some(r), None) => format!("seeds until ratio {r:.1}"),
            (None, Some(s)) => format!("seeds for {}", goal_duration(s)),
            (None, None) => "seeds forever".into(),
        }
    }

    /// Whether the goal is met after seeding `seeded_secs` at `ratio`.
    pub fn reached(&self, ratio: f64, seeded_secs: u64) -> bool {
        if !self.is_set() {
            return false;
        }
        if seeded_secs < u64::from(self.min_seed_secs) {
            return false;
        }
        if self.stop_on_finish {
            return true;
        }
        let r = self.ratio.map(|g| ratio >= f64::from(g));
        let t = self.seed_secs.map(|g| seeded_secs >= u64::from(g));
        match (r, t) {
            (Some(a), Some(b)) => {
                if self.any {
                    a || b
                } else {
                    a && b
                }
            }
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => false,
        }
    }
}

/// The text with its first letter in lower case ("Seed until less than 1 MB" -> "seed until less
/// than 1 MB"): a goal inside a sentence, without turning units into "mb".
pub fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map_or_else(String::new, |f| f.to_lowercase().chain(c).collect())
}

/// A byte count for the inactive condition: "1 GB", "0.3 GB", "500 MB", "2 TB" (decimal, like the
/// rest of the app).
pub fn idle_size(bytes: u64) -> String {
    let trim = |v: f64| {
        let t = format!("{v:.1}");
        t.strip_suffix(".0").map_or(t.clone(), str::to_owned)
    };
    if bytes >= 1_000_000_000_000 {
        format!("{} TB", trim(bytes as f64 / 1e12))
    } else if bytes >= 100_000_000 {
        format!("{} GB", trim(bytes as f64 / 1e9))
    } else if bytes >= 1_000_000 {
        format!("{} MB", (bytes as f64 / 1e6).round() as u64)
    } else {
        format!("{} KB", bytes / 1000)
    }
}

fn goal_duration(secs: u32) -> String {
    if secs >= 86_400 {
        let d = secs / 86_400;
        format!("{d} day{}", if d == 1 { "" } else { "s" })
    } else {
        let h = (secs / 3600).max(1);
        format!("{h} hour{}", if h == 1 { "" } else { "s" })
    }
}

/// "Rule Y – seed to 1.0 · Saves to ~/Media/Movies · Then seeds until ratio 1.0": what a torrent in a
/// category gets, as three parts (a UI styles them, a terminal prints [`RuleSummary::text`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RuleSummary {
    /// Name of the rule in effect ("Default" when no rule matched).
    pub rule: String,
    /// Where the files will be saved; empty when not known.
    pub saves_to: String,
    /// "seeds until ratio 1.0".
    pub then: String,
}

impl RuleSummary {
    /// The summary as one line: `Rule <name> · Saves to <folder> · Then <goal>`; empty parts are left out.
    pub fn text(&self) -> String {
        let mut parts = Vec::new();
        if !self.rule.is_empty() {
            parts.push(format!("Rule {}", self.rule));
        }
        if !self.saves_to.is_empty() {
            parts.push(format!("Saves to {}", self.saves_to));
        }
        if !self.then.is_empty() {
            parts.push(format!("Then {}", self.then));
        }
        parts.join(" \u{b7} ")
    }
}

impl Default for SeedGoal {
    fn default() -> Self {
        Self::FOREVER
    }
}

/// Where a new torrent comes from.
#[derive(Clone, Debug)]
pub enum AddSource {
    /// The bytes of a `.torrent` file.
    TorrentFile(Vec<u8>),
    /// A `magnet:` URI.
    Magnet(String),
}

/// Options for [`crate::EngineHandle::add`].
///
/// Fields typed `Option` are *overrides*: `None` means "follow the rules and settings" (the
/// matching rule's priority and goal, the detected category, the configured locations), and a
/// value is remembered as the user's choice so later rule edits do not silently undo it.
#[derive(Clone, Debug, Default)]
pub struct AddOptions {
    /// Save directory: the finished location of this torrent (its files are written below it in
    /// the torrent's own layout) and pins it there. `None` uses the configured finished location
    /// (rule, category, app default).
    pub save_path: Option<PathBuf>,
    /// Add paused (nothing starts until `resume`).
    pub paused: bool,
    /// Per-file priorities in file order; empty means "decide" (junk files are skipped when
    /// the setting is on, everything else Normal).
    pub file_priorities: Vec<FilePriority>,
    /// Category chosen by the user (or forced by a watch folder). `None` detects it.
    pub category: Option<Category>,
    /// Queue and bandwidth priority; `None` follows the rule.
    pub priority: Option<Priority>,
    /// Peers to try right away (also used by tests).
    pub peers: Vec<SocketAddr>,
    /// Put the torrent at the top of the queue.
    pub queue_top: bool,
    /// Seeding goal; `None` follows the rule.
    pub seed_goal: Option<SeedGoal>,
    /// Force this rule id instead of matching (`rules.toml` id).
    pub rule: Option<String>,
    /// Smart add (SPEC 7A.6). `None`: hold the torrent for a choice when the settings say it
    /// needs one (unless `file_priorities` already carry the caller's choice). `Some(false)`:
    /// start at once whatever the settings say (the caller has chosen, or wants everything).
    /// `Some(true)`: hold it for a choice in any case.
    pub hold: Option<bool>,
    /// Skip the grace period (SPEC 7A.7, `transfers.start_delay_secs`): start at once when
    /// nothing needs a choice. `false` follows the setting.
    pub start_now: bool,
}

/// The grace period of a pending torrent (SPEC 7A.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingStatus {
    /// Unix milliseconds at which the torrent starts. While `paused` this moves with the clock
    /// (it is `now + remaining_ms`) and means nothing.
    pub deadline_ms: u64,
    /// Milliseconds left at the time the row was built.
    pub remaining_ms: u64,
    /// The countdown is stopped ([`crate::EngineHandle::pending_pause`]); the torrent stays held.
    pub paused: bool,
}

/// What happened to a pending torrent ([`EngineEvent::Pending`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingChange {
    /// Added and held; the countdown runs.
    Added,
    /// The countdown was stopped.
    Paused,
    /// The countdown runs again (from the full delay).
    Resumed,
    /// The grace period is over (or "start now"): the torrent starts.
    Started,
    /// The user wants to choose files: the countdown is cancelled, the torrent stays held.
    Chosen,
}

/// Options for [`crate::EngineHandle::add_download`] (SPEC 7D).
#[derive(Clone, Debug, Default)]
pub struct DownloadOptions {
    /// Destination folder; the configured download folder when `None`.
    pub dest_dir: Option<PathBuf>,
    /// File name instead of the one the server suggests.
    pub file_name: Option<String>,
    /// Parallel ranges (clamped to the per-host cap); the configured default when `None`.
    pub connections: Option<u32>,
    /// Verify the finished file against this SHA-256 before it is moved into place.
    pub sha256: Option<[u8; 32]>,
    /// Per-download limit, bytes/s (the global limit applies on top).
    pub rate_limit: Option<u64>,
    /// Add paused.
    pub paused: bool,
    /// Category of the download: a finished file goes to that category's finished folder, when it has one.
    pub category: Option<Category>,
}

/// Result of a successful add.
#[derive(Clone, Debug)]
pub struct Added {
    /// Torrent id.
    pub id: TorrentId,
    /// Display name (the magnet's `dn`, or the id in hex until metadata arrives).
    pub name: String,
    /// The torrent is held in state [`TorrentState::AwaitingChoice`]: nothing is announced or
    /// downloaded until [`crate::EngineHandle::confirm_choice`] (or removed with
    /// [`crate::EngineHandle::cancel_choice`]). Only `.torrent` adds know this at once; a magnet
    /// is held later, when its metadata arrives (watch for [`EngineEvent::ChoiceNeeded`]).
    pub needs_choice: bool,
    /// The structure, preselection and reasons when `needs_choice` is true.
    pub choice: Option<Box<ChoicePreview>>,
    /// The torrent is held for the grace period (state [`TorrentState::Pending`]); it starts at
    /// the deadline unless the countdown is paused or the user asks to choose files.
    pub pending: Option<PendingStatus>,
}

/// Why an add or another command failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// The `.torrent` or magnet could not be parsed.
    Invalid(String),
    /// A torrent with this id exists already.
    Duplicate(TorrentId),
    /// The torrent was already added; its new announce URLs were merged in (public torrents only).
    TrackersMerged {
        /// The existing torrent.
        id: TorrentId,
        /// Its name.
        name: String,
        /// How many trackers were added.
        added: usize,
    },
    /// No such torrent.
    NotFound,
    /// The torrent has no metadata yet.
    NoMetadata,
    /// The persistence layer failed.
    Store(String),
    /// The engine is shutting down.
    ShuttingDown,
    /// Operating-system error (sockets, directories).
    Io(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid torrent: {m}"),
            Self::Duplicate(_) => f.write_str("torrent already added"),
            Self::TrackersMerged { name, added, .. } => write!(f, "Added {added} tracker{} to {name}", if *added == 1 { "" } else { "s" }),
            Self::NotFound => f.write_str("no such torrent"),
            Self::NoMetadata => f.write_str("metadata not available yet"),
            Self::Store(m) => write!(f, "state store: {m}"),
            Self::ShuttingDown => f.write_str("engine is shutting down"),
            Self::Io(m) => write!(f, "i/o error: {m}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// One row of [`Snapshot`]; what lists in the UI show.
#[derive(Clone, Debug)]
pub struct TorrentRow {
    /// Torrent id.
    pub id: TorrentId,
    /// Display name.
    pub name: String,
    /// State.
    pub state: TorrentState,
    /// Whole torrent size in bytes (0 until metadata).
    pub total_bytes: u64,
    /// Bytes of the files the user wants.
    pub wanted_bytes: u64,
    /// Verified bytes.
    pub done_bytes: u64,
    /// Progress of the current download session: verified bytes of the wanted files beyond what
    /// the torrent already held when it became a download again (0 base for a fresh download).
    pub session_done: u64,
    /// Size of the current download session: wanted bytes minus that base. Download progress is
    /// `session_done / session_size`.
    pub session_size: u64,
    /// Download rate, bytes/s.
    pub down_rate: u64,
    /// Upload rate, bytes/s.
    pub up_rate: u64,
    /// Connected peers.
    pub peers: u32,
    /// Connected peers that are seeds.
    pub seeds: u32,
    /// Seeders the tracker reported at its last announce (`complete`), if it said.
    pub swarm_seeds: Option<u32>,
    /// Leechers the tracker reported at its last announce (`incomplete`), if it said.
    pub swarm_leechers: Option<u32>,
    /// Peer addresses known for this torrent (connected or still to try): tracker and incoming
    /// connections (private torrents; plus PEX when the tracker allows it); plus DHT and PEX for
    /// public ones.
    pub known_peers: u32,
    /// Estimated seconds to completion.
    pub eta_secs: Option<u64>,
    /// Lifetime uploaded / torrent size.
    pub ratio: f64,
    /// Host of the first tracker ("" when none). Passkeys are never included.
    pub tracker_host: String,
    /// Power tier.
    pub tier: Tier,
    /// BEP 27 private flag.
    pub private: bool,
    /// Unix seconds when the download finished.
    pub finished_at: Option<u64>,
    /// Unix seconds when the torrent was added.
    pub added_at: u64,
    /// Error text for `Error` / `MissingData`.
    pub error: Option<String>,
    /// Absolute path of the file or folder.
    pub save_path: PathBuf,
    /// Priority.
    pub priority: Priority,
    /// Queue position (0 = front).
    pub queue_pos: u32,
    /// Lifetime payload bytes uploaded.
    pub uploaded_total: u64,
    /// Lifetime payload bytes downloaded.
    pub downloaded_total: u64,
    /// Detected or chosen category.
    pub category: Option<Category>,
    /// Seeding goal in effect (rule-derived unless the user overrode it).
    pub seed_goal: SeedGoal,
    /// Matched tracker profile id (`trackers.toml`); `None` = Public or Other private.
    pub tracker_profile: Option<String>,
    /// Rule id that was forced for this torrent (`None` = the rules decide).
    pub rule: Option<String>,
    /// The rule that governs it right now (forced, linked from the tracker profile, or matched);
    /// `None` = the Default rule.
    pub rule_in_effect: Option<String>,
    /// Torrent or direct download.
    pub kind: RowKind,
    /// Source URL of a direct download (passkey-free by construction: direct links only).
    pub url: Option<String>,
    /// Per-torrent upload limit from the rule, bytes/s.
    pub up_limit: Option<u64>,
    /// The files have paths of their own (placed by an earlier version, SPEC 7A.3), not the torrent's layout.
    pub organized: bool,
    /// Sharing group; `None` unless the torrent is a finished, not stopped torrent.
    pub seed_group: Option<SeedGroup>,
    /// Unix seconds of the last upload or interested peer (finished torrents).
    pub last_activity: Option<u64>,
    /// Unix seconds when the current error began.
    pub error_since: Option<u64>,
    /// For a tracker error: unix milliseconds of the next announce attempt.
    pub next_retry_ms: Option<u64>,
    /// The files are being moved to another location (SPEC 7A.5): progress 0..=1. Seeding goes on
    /// meanwhile; the row shows "Moving... 42%".
    pub moving: Option<f32>,
    /// Why the last automatic move (status folders) was not done, e.g. a locked file; the torrent
    /// keeps seeding where it is. Cleared when a move works or none is due any more.
    pub move_note: Option<String>,
    /// The cleaned display title ("Sintel (2010)"), computed once and persisted with the torrent
    /// (name, file list and category together; for a single-file torrent the file's name when it
    /// says more). Empty until the torrent's files are known; use [`TorrentRow::display_title`].
    pub title: String,
    /// Seconds this torrent has been seeding in total (all stretches, including the current one).
    pub seeded_secs: u64,
    /// `seed_goal` is the user's own choice for this torrent; `false` = it follows the rule
    /// ([`EngineHandle::clear_seed_goal`] puts it back).
    pub seed_goal_user: bool,
    /// The grace period while the state is [`TorrentState::Pending`].
    pub pending: Option<PendingStatus>,
    /// Every tracker of the torrent fails or has halted, in words ("Tracker: Unregistered
    /// torrent", "Tracker unreachable"). Set in every state, finished or not.
    pub tracker_error: Option<String>,
    /// `tracker_error` is permanent-looking (unregistered, banned, bad passkey): the trackers
    /// stopped announcing until the user asks again ("Ask Tracker Again", `reannounce`).
    pub tracker_halted: bool,
    /// What `tracker_error` means, in a plain sentence for Get Info ("The tracker's server is down
    /// or overloaded (it answered 502 Bad Gateway)."). Never contains the announce URL.
    pub tracker_detail: Option<String>,
    /// An announce is in flight since this unix ms (any announce but `stopped`); the row shows
    /// "Announcing…" once it has lasted half a second.
    pub announcing_since_ms: Option<u64>,
    /// Consecutive failed tries of the failing trackers ("attempt 3"); 0 when none fail.
    pub tracker_failures: u32,
    /// The disk problem (missing files, a failed write), when there is one; `error` is this or the
    /// short tracker reason.
    pub disk_error: Option<String>,
    /// The only problem is a temporary tracker outage: not permanent and younger than a day. It is
    /// shown calmly (no attention dot, not counted as an error); anything else needs the user.
    pub error_transient: bool,
    /// A running download that moves (almost) nothing and so does not count toward
    /// `max_active_downloads` (SPEC 7.3: below 2 KiB/s for 60 s).
    pub stalled: bool,
    /// Why the torrent is held although it is not paused, in words, from the engine: "Disk full —
    /// retrying in 10 min", "Waiting to verify". Shown by the row instead of the progress details.
    pub hold_note: Option<String>,
    /// The inactive stop condition (SPEC §7.5) of a finished torrent, with the live figure.
    pub idle: Option<IdleStatus>,
}

/// The inactive stop condition of one torrent: what went out in the last window of seeding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdleStatus {
    /// Bytes uploaded in the last `hours` hours of seeding.
    pub uploaded: u64,
    /// The threshold, bytes.
    pub below: u64,
    /// The window, hours.
    pub hours: u32,
    /// The torrent was stopped by this condition.
    pub stopped: bool,
}

impl IdleStatus {
    /// Why it stopped, after "Stopped — ": "0.3 GB uploaded in the last 150 h".
    pub fn reason(&self) -> String {
        format!("{} uploaded in the last {} h", idle_size(self.uploaded), self.hours)
    }

    /// The live figure for Get Info: "Uploaded in the last 150 h: 0.3 GB (stops below 1 GB)".
    pub fn info(&self) -> String {
        format!("Uploaded in the last {} h: {} (stops below {})", self.hours, idle_size(self.uploaded), idle_size(self.below))
    }
}

/// The words of [`TorrentRow::stalled`].
pub const STALLED_TEXT: &str = "Stalled — not counted in the queue";

/// The rule behind [`TorrentRow::waiting_note`].
pub fn waiting_note_of(state: TorrentState, peers: u32, down_rate: u64, tracker_error: Option<&str>, halted: bool) -> Option<RowNote> {
    if !matches!(state, TorrentState::Downloading | TorrentState::FetchingMetadata) || peers > 0 || down_rate > 0 {
        return None;
    }
    Some(match tracker_error {
        Some(t) => RowNote { text: t.to_owned(), error: halted },
        None => RowNote { text: "Waiting for peers".into(), error: false },
    })
}

#[cfg(test)]
mod waiting_tests {
    use super::*;

    #[test]
    fn nobody_connected_says_why() {
        let w = |s, p, d, e: Option<&str>| waiting_note_of(s, p, d, e, e.is_some_and(|t| t.starts_with("Rejected")));
        let dl = TorrentState::Downloading;
        assert_eq!(w(dl, 0, 0, None), Some(RowNote { text: "Waiting for peers".into(), error: false }));
        assert_eq!(w(dl, 0, 0, Some("Rejected: “Unregistered torrent”")), Some(RowNote { text: "Rejected: “Unregistered torrent”".into(), error: true }));
        assert_eq!(w(dl, 0, 0, Some("Bad gateway")), Some(RowNote { text: "Bad gateway".into(), error: false }), "a temporary outage is not an error");
        assert_eq!(w(dl, 2, 0, Some("Tracker unreachable")), None, "anything connected: the row shows its progress");
        assert_eq!(w(dl, 0, 1, None), None);
        assert_eq!(w(TorrentState::Seeding, 0, 0, Some("Tracker unreachable")), None, "only downloads");
        assert_eq!(w(TorrentState::Paused, 0, 0, None), None);
    }
}

/// The one grey line a row shows instead of its progress details (SPEC 8.3): why nothing moves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowNote {
    /// The words ("Tracker: Unregistered torrent", "Waiting for peers").
    pub text: String,
    /// Shown in the error colour: the tracker refused for good. A passing outage stays grey.
    pub error: bool,
}

impl TorrentRow {
    /// Why a download is not moving: every tracker failed and there is nobody to talk to, or
    /// there is simply nobody yet. `None` while anything is connected or flowing (the row then
    /// shows its progress and time remaining) and for torrents that are not downloading. One
    /// text for the app and the CLI.
    pub fn waiting_note(&self) -> Option<RowNote> {
        if self.kind != RowKind::Torrent {
            return None;
        }
        if let Some(t) = &self.hold_note {
            return Some(RowNote { text: t.clone(), error: false });
        }
        let base = waiting_note_of(self.state, self.peers, self.down_rate, self.tracker_error.as_deref(), self.tracker_halted);
        // A stalled download says so, unless a tracker problem explains more.
        if self.stalled && self.state == TorrentState::Downloading && !base.as_ref().is_some_and(|n| n.error) {
            return Some(RowNote { text: STALLED_TEXT.into(), error: false });
        }
        base
    }

    /// The seeding goal as Get Info and `info` show it: the goal in words, and "Rule default (...)" when it
    /// is the rule's own.
    pub fn goal_text(&self) -> String {
        if self.seed_goal_user {
            self.seed_goal.describe_for(self.private)
        } else {
            format!("Rule default ({})", lower_first(&self.seed_goal.describe_for(self.private)))
        }
    }

    /// Why a torrent stopped by the inactive condition stopped ("0.3 GB uploaded in the last
    /// 150 h"); `None` for anything else. One text for the app and the CLI.
    pub fn idle_reason(&self) -> Option<String> {
        self.idle.filter(|i| i.stopped).map(|i| i.reason())
    }

    /// The live figure of the inactive condition for Get Info and `info`.
    pub fn idle_info(&self) -> Option<String> {
        self.idle.map(|i| i.info())
    }

    /// The title lists show: the persisted clean title, else the cleaned raw name.
    pub fn display_title(&self) -> String {
        if self.title.is_empty() { trs_classify::clean_name(&self.name) } else { self.title.clone() }
    }
}

/// Counts of torrents per tier and state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// All torrents.
    pub total: u32,
    /// Downloading.
    pub downloading: u32,
    /// Queued.
    pub queued: u32,
    /// Seeding (active or low power).
    pub seeding: u32,
    /// Paused.
    pub paused: u32,
    /// Error or missing data.
    pub errors: u32,
    /// Cold tier.
    pub cold: u32,
    /// Warm tier.
    pub warm: u32,
    /// Hot tier.
    pub hot: u32,
}

/// Aggregates over all torrents, without building a row per torrent: what a headless client or
/// a status bar needs. Costs one pass over the table and no allocation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Totals {
    /// Torrents (direct downloads are not counted).
    pub torrents: u32,
    /// Verified bytes over all torrents.
    pub done_bytes: u64,
    /// Total bytes over all torrents.
    pub total_bytes: u64,
    /// Current download rate, bytes/s.
    pub down_rate: u64,
    /// Current upload rate, bytes/s.
    pub up_rate: u64,
    /// Connected peers.
    pub peers: u64,
    /// Verifying.
    pub checking: u32,
    /// Seeding or low power.
    pub seeding: u32,
    /// Error or missing data.
    pub errors: u32,
    /// Everything else (downloading, queued, paused, fetching metadata).
    pub other: u32,
    /// Connections that completed their handshake (what a status line shows).
    pub connections: u32,
    /// Connections being dialled or handshaking; they hold a slot of the budget but are not connected.
    pub half_open: u32,
}

/// Cheap global view; the UI pulls it about once a second while visible.
#[derive(Clone, Debug)]
pub struct Snapshot {
    /// One row per torrent, in queue order.
    pub rows: Vec<TorrentRow>,
    /// Global download rate, bytes/s.
    pub down_rate: u64,
    /// Global upload rate, bytes/s.
    pub up_rate: u64,
    /// Counts.
    pub counts: Counts,
    /// Alt-speed on (manual toggle or schedule).
    pub alt_speed: bool,
    /// Alt-speed is on because the schedule says so (not the manual toggle).
    pub alt_speed_scheduled: bool,
    /// Effective download limit.
    pub down_limit: Option<u64>,
    /// Effective upload limit.
    pub up_limit: Option<u64>,
    /// Peer connections that completed their handshake.
    pub connections: u32,
    /// Connections still being dialled or handshaking (not counted in `connections`).
    pub connecting: u32,
    /// TCP port we listen on.
    pub listen_port: u16,
    /// Set when the configured listen port was taken and another one is used instead.
    pub port_fallback: Option<PortFallback>,
    /// False while the engine has declared an outage (SPEC 7C.4c): the network is gone and it
    /// sends almost nothing until it is back. A wake check is not an outage.
    pub online: bool,
    /// Unix milliseconds at which the outage began, while there is one.
    pub offline_since_ms: Option<u64>,
}

/// The configured listen port could not be bound; the engine listens on another one.
///
/// The configuration keeps the port the user chose, so the next start tries it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortFallback {
    /// The configured port.
    pub wanted: u16,
    /// The port in use now (TCP, and the same number for uTP and the DHT).
    pub using: u16,
    /// Ready to show: "Port 55252 is in use by another app; using 61234 for now".
    pub message: String,
}

/// Container family of a streamed file (what decides which players can open it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamContainer {
    /// MP4 / M4V / MOV / M4A: QuickTime Player (AVFoundation) plays these.
    Mp4Family,
    /// MKV / WebM.
    Matroska,
    /// AVI.
    Avi,
    /// MPEG transport stream.
    MpegTs,
    /// Other video containers (wmv, flv, mpg, ogv...).
    OtherVideo,
    /// Audio only.
    Audio,
    /// Unknown extension.
    Unknown,
}

impl StreamContainer {
    /// Stable lower-case name for the control socket and agents.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mp4Family => "mp4_family",
            Self::Matroska => "matroska",
            Self::Avi => "avi",
            Self::MpegTs => "mpeg_ts",
            Self::OtherVideo => "other_video",
            Self::Audio => "audio",
            Self::Unknown => "unknown",
        }
    }
}

impl StreamContainer {
    /// The container family a file name suggests (by extension).
    pub fn of_name(name: &str) -> Self {
        trs_stream::mime::container_of(name).into()
    }
}

impl From<trs_stream::Container> for StreamContainer {
    fn from(c: trs_stream::Container) -> Self {
        use trs_stream::Container as C;
        match c {
            C::Mp4Family => Self::Mp4Family,
            C::Matroska => Self::Matroska,
            C::Avi => Self::Avi,
            C::MpegTs => Self::MpegTs,
            C::OtherVideo => Self::OtherVideo,
            C::Audio => Self::Audio,
            C::Unknown => Self::Unknown,
        }
    }
}

/// A file served over loopback HTTP while it downloads (SPEC 7F.1), from
/// [`crate::EngineHandle::stream_url`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    /// `http://127.0.0.1:<port>/<token>/<name>`: hand this to the player. `None` when the file
    /// is complete (`complete`): open the file itself.
    pub url: Option<String>,
    /// The random 128-bit token inside `url`; [`crate::EngineHandle::stop_stream`] takes it.
    pub token: Option<String>,
    /// Index of the file in the torrent.
    pub file_index: u32,
    /// Last path component.
    pub file_name: String,
    /// Size in bytes.
    pub file_len: u64,
    /// From the extension, e.g. `video/x-matroska`.
    pub content_type: String,
    /// Container family.
    pub container: StreamContainer,
    /// QuickTime Player (AVFoundation) can play it.
    pub is_mp4_family: bool,
    /// Every piece of the file is verified already: no server is involved.
    pub complete: bool,
    /// The file was unselected and this call selected it for download.
    pub selected: bool,
}

/// One file of a torrent.
#[derive(Clone, Debug)]
pub struct FileInfo {
    /// Path relative to the torrent root (components joined by `/`), or the absolute destination of a
    /// file that placement or organizing put elsewhere (live and resting torrents alike).
    pub path: String,
    /// Length in bytes.
    pub length: u64,
    /// Verified bytes of this file (approximate for files sharing pieces).
    pub done: u64,
    /// Priority.
    pub priority: FilePriority,
    /// BEP 47 padding file.
    pub padding: bool,
    /// Payload bytes served from this file since the torrent went live (0 for a resting torrent).
    pub uploaded: u64,
}

/// An announce URL that may hold a passkey: `Debug` hides it, only [`SecretUrl::reveal`] returns it.
#[derive(Clone, PartialEq, Eq)]
pub struct SecretUrl(String);

impl SecretUrl {
    /// Wrap a raw URL.
    pub fn new(raw: &str) -> Self {
        Self(raw.to_owned())
    }
    /// The real URL. Never log this.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for SecretUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SecretUrl(..)")
    }
}

/// Last known state of one tracker.
#[derive(Clone, Debug)]
pub struct TrackerInfo {
    /// URL with passkeys removed.
    pub url: String,
    /// The whole URL for the screen, every secret part replaced by `••••••`.
    pub shown_url: String,
    /// The announce URL as it is in the torrent, passkey included. Debug never prints it.
    pub full_url: SecretUrl,
    /// Tier index.
    pub tier: u32,
    /// Human-readable status ("announced", "failed: ...", "halted: ...").
    pub status: String,
    /// Unix ms of the last announce we sent.
    pub last_announce_ms: Option<u64>,
    /// Unix ms of the next regular announce.
    pub next_announce_ms: Option<u64>,
    /// Seeders reported by the tracker.
    pub seeds: Option<u64>,
    /// Leechers reported by the tracker.
    pub leechers: Option<u64>,
    /// Consecutive failures.
    pub failures: u32,
    /// Diagnostics (`seedwell debug trackers`): how the tracker is doing, as one word.
    pub state: TrackerState,
    /// Unix ms of the last announce that the tracker answered.
    pub last_success_ms: Option<u64>,
    /// Unix ms at which a failed announce is tried again (backoff).
    pub retry_at_ms: Option<u64>,
    /// Unix ms before which a manual "Ask Tracker Again" is held back (the tracker's minimum interval).
    pub manual_ok_ms: Option<u64>,
    /// Why the circuit breaker holds the tracker back, if it does.
    pub tripped: Option<&'static str>,
    /// What is wrong, in a plain sentence (see `describe_failure`); `None` while it works. No URL.
    pub problem: Option<String>,
}

/// How one tracker of one torrent is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TrackerState {
    /// Nothing heard yet.
    #[default]
    Waiting,
    /// The last announce worked.
    Working,
    /// Works, with a warning from the tracker.
    Warn,
    /// The last announce failed; it is tried again later.
    Backoff,
    /// The tracker said no for good (unregistered, banned, ...); nothing is sent until the user asks.
    Halted,
}

impl TrackerState {
    /// The stable word used by the CLI and JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Working => "working",
            Self::Warn => "warning",
            Self::Backoff => "backoff",
            Self::Halted => "halted",
        }
    }
}

/// One connected peer.
#[derive(Clone, Debug)]
pub struct PeerInfo {
    /// Remote address.
    pub addr: SocketAddr,
    /// Client string from the extended handshake.
    pub client: String,
    /// We connected to them.
    pub outgoing: bool,
    /// Download rate from this peer.
    pub down_rate: u64,
    /// Upload rate to this peer.
    pub up_rate: u64,
    /// Fraction of pieces they have, 0..=1.
    pub progress: f32,
    /// They are choking us.
    pub choking_us: bool,
    /// We are choking them.
    pub choked_by_us: bool,
    /// They are interested in us.
    pub interested: bool,
    /// The connection runs over uTP (otherwise TCP).
    pub utp: bool,
    /// Diagnostics (`seedwell debug peers`): block requests sent and not yet answered.
    pub requests_in_flight: u32,
    /// Seconds since the connection was established.
    pub connected_secs: u64,
    /// We stopped asking this peer for data because it did not deliver.
    pub snubbed: bool,
    /// The peer only uploads to nobody (it is a seed and wants nothing).
    pub upload_only: bool,
    /// The peer announced it supports peer exchange.
    pub has_pex: bool,
}

/// Everything about one torrent (the Get Info popover and `get_transfer`).
#[derive(Clone, Debug)]
pub struct TorrentDetail {
    /// The list row.
    pub row: TorrentRow,
    /// Files.
    pub files: Vec<FileInfo>,
    /// Trackers.
    pub trackers: Vec<TrackerInfo>,
    /// Peers (empty for Cold torrents).
    pub peers: Vec<PeerInfo>,
    /// Piece count.
    pub piece_count: u32,
    /// Piece length.
    pub piece_length: u32,
    /// Torrent comment.
    pub comment: Option<String>,
}

/// Events pushed to subscribers (UI notifications, agents, tests).
#[derive(Clone, Debug)]
pub enum EngineEvent {
    /// A torrent was added.
    Added {
        /// Id.
        id: TorrentId,
        /// Name.
        name: String,
    },
    /// Magnet metadata arrived and was verified.
    MetadataReceived {
        /// Id.
        id: TorrentId,
        /// Name from the metadata.
        name: String,
    },
    /// The wanted files finished downloading.
    Finished {
        /// Id.
        id: TorrentId,
    },
    /// State changed.
    StateChanged {
        /// Id.
        id: TorrentId,
        /// New state.
        state: TorrentState,
    },
    /// A torrent hit an error.
    Error {
        /// Id.
        id: TorrentId,
        /// Message.
        message: String,
    },
    /// The seeding goal was reached.
    SeedingGoalReached {
        /// Id.
        id: TorrentId,
    },
    /// What the trackers say about the torrent changed (a failure began, its retry count or time
    /// moved, or an announce worked and the problem is gone): rows refresh at once.
    TrackerHealthChanged {
        /// Id.
        id: TorrentId,
    },
    /// A torrent was removed.
    Removed {
        /// Id.
        id: TorrentId,
    },
    /// The grace period of a torrent changed (SPEC 7A.7).
    Pending {
        /// Id.
        id: TorrentId,
        /// What happened.
        change: PendingChange,
    },
    /// Files that were unselected got selected for download because somebody asked to stream
    /// them (SPEC 7F.2: "selecting a file for streaming selects it for download").
    FilesSelected {
        /// Id.
        id: TorrentId,
        /// Indices in the torrent's file list.
        files: Vec<u32>,
    },
    /// A peer was banned for sending corrupt data.
    PeerBanned {
        /// Id.
        id: TorrentId,
        /// Banned address.
        addr: SocketAddr,
    },
    /// A torrent was added without asking (watch folder in `auto` mode); the notification text
    /// of SPEC 7A.1 is built from these fields.
    AutoAdded {
        /// Id.
        id: TorrentId,
        /// Name.
        name: String,
        /// The clean display title ("Sintel (2010)").
        title: String,
        /// Category in effect.
        category: Category,
        /// Matched tracker profile.
        tracker_profile: Option<String>,
        /// The finished location the torrent's own layout goes into.
        destination: Option<PathBuf>,
        /// Where it came from.
        origin: AddOrigin,
    },
    /// A watch folder found a torrent that needs a decision (mode `ask`, or the category is
    /// uncertain): show the add panel with this preview, then call
    /// [`crate::EngineHandle::add_torrent_path`].
    AddRequest(Box<AddRequest>),
    /// The torrent is already in the client; nothing was changed.
    Duplicate {
        /// Id of the existing torrent.
        id: TorrentId,
        /// Its name.
        name: String,
        /// New announce URLs merged into it by this add (0 = none; public torrents only).
        trackers_added: usize,
    },
    /// A file in a watch folder could not be used.
    WatchError {
        /// The file.
        path: PathBuf,
        /// Why.
        message: String,
    },
    /// A settings file was reloaded and applied.
    ConfigReloaded {
        /// `config.toml`, `rules.toml` or `trackers.toml`.
        file: String,
        /// Non-fatal warnings (unknown keys, dangling references).
        warnings: Vec<String>,
        /// Settings that only take effect after a restart.
        restart_required: Vec<String>,
    },
    /// A settings file is invalid; the previous values stay in force.
    ConfigRejected {
        /// File name.
        file: String,
        /// Field-path errors.
        error: String,
    },
    /// Port mapping status changed.
    PortMap(PortMapStatus),
    /// The alternative speed limits switched on or off (manual toggle or schedule).
    AltSpeed {
        /// Alt limits are in force now.
        on: bool,
        /// They are on because the schedule's window is open.
        scheduled: bool,
    },
    /// A direct download restarted from zero (the file changed on the server).
    DirectRestarted {
        /// Id.
        id: TorrentId,
        /// Plain-language reason.
        reason: String,
    },
    /// A torrent is held until the user chooses what to download (SPEC 7A.6): show the orange
    /// dot, "Choose what to download...", a notification that opens the chooser
    /// ([`crate::EngineHandle::choice`]). Emitted for every hold (watch folder, agent, magnet).
    ChoiceNeeded {
        /// Id.
        id: TorrentId,
        /// Name.
        name: String,
        /// Why the engine did not simply start it.
        reasons: Vec<ChoiceReason>,
        /// Where the add came from.
        origin: AddOrigin,
    },
    /// The torrent's choice was confirmed or cancelled; the dot can go.
    ChoiceResolved {
        /// Id.
        id: TorrentId,
        /// `true`: confirmed (the torrent starts); `false`: cancelled (it is removed).
        confirmed: bool,
    },
    /// A change of the app-level or category locations leaves torrents elsewhere than the settings
    /// now say (SPEC 7A.5): ask "Move N torrents (X GB)?" and call
    /// [`crate::EngineHandle::location_moves`] / [`crate::EngineHandle::move_torrents`].
    LocationsChanged {
        /// Torrents that would move.
        torrents: u32,
        /// Their size in bytes.
        bytes: u64,
    },
    /// A move was queued or started.
    MoveStarted {
        /// Id.
        id: TorrentId,
        /// Where the files are now.
        from: PathBuf,
        /// Where they are going.
        to: PathBuf,
        /// Bytes to relocate.
        bytes: u64,
        /// The move crosses volumes (copy with verification) instead of a rename.
        cross_volume: bool,
    },
    /// Progress of a cross-volume copy, about once a second.
    MoveProgress {
        /// Id.
        id: TorrentId,
        /// Bytes copied (and verified) so far.
        done: u64,
        /// Bytes in all.
        total: u64,
    },
    /// The files are at their new place; the old copy is gone.
    MoveFinished {
        /// Id.
        id: TorrentId,
        /// New location of the content (folder or file).
        to: PathBuf,
    },
    /// A move failed or was refused; the torrent keeps (or resumes) seeding from where it was.
    MoveFailed {
        /// Id.
        id: TorrentId,
        /// Plain-language reason ("Not enough free space on ...").
        reason: String,
    },
}

/// Where an add came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddOrigin {
    /// A command (UI, CLI, agent).
    User,
    /// A watch folder; the file path.
    Watch(PathBuf),
}

/// Why a watch folder asks instead of adding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AskReason {
    /// The folder's mode is `ask`.
    FolderMode,
    /// The category confidence is below the threshold.
    LowConfidence,
}

/// A torrent file waiting for the user's decision.
#[derive(Clone, Debug)]
pub struct AddRequest {
    /// The `.torrent` in the watch folder.
    pub path: PathBuf,
    /// Why the engine did not add it.
    pub reason: AskReason,
    /// What adding it would do.
    pub preview: AddPreview,
    /// Category forced by the folder, if any.
    pub forced_category: Option<Category>,
    /// Rule forced by the folder, if any.
    pub forced_rule: Option<String>,
}

/// Statistics bucket a torrent counts toward (SPEC 7E).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProfileBucket {
    /// A tracker profile id.
    Profile(String),
    /// No profile, public torrent.
    Public,
    /// No profile, private torrent.
    OtherPrivate,
}

/// One file in an [`AddPreview`].
#[derive(Clone, Debug)]
pub struct PreviewFile {
    /// Path inside the torrent.
    pub path: String,
    /// Length.
    pub length: u64,
    /// Junk classification ("sample", "nfo", ...); such files start unchecked when skipping junk.
    pub junk: Option<&'static str>,
}

/// What an add would do (the add panel, watch-folder "ask").
#[derive(Clone, Debug)]
pub struct AddPreview {
    /// Torrent id.
    pub id: TorrentId,
    /// Name.
    pub name: String,
    /// Total size.
    pub total_bytes: u64,
    /// Number of files.
    pub file_count: u32,
    /// BEP 27 private flag.
    pub private: bool,
    /// Host of the first tracker.
    pub tracker_host: String,
    /// Statistics bucket.
    pub bucket: ProfileBucket,
    /// Category the engine would use (`Other` while unsure).
    pub category: Category,
    /// Best guess of the classifier even when unsure.
    pub detected: Category,
    /// Classifier confidence 0..=1.
    pub confidence: f32,
    /// Confidence is below the threshold: ask the user.
    pub uncertain: bool,
    /// Rule that would apply.
    pub rule: Option<String>,
    /// Priority from the rule.
    pub priority: Priority,
    /// Seeding goal from the rule.
    pub seed_goal: SeedGoal,
    /// Upload limit from the rule.
    pub up_limit: Option<u64>,
    /// Library folder the files would go to (None = torrent layout in the download folder).
    pub destination: Option<PathBuf>,
    /// Files (capped at 2000).
    pub files: Vec<PreviewFile>,
    /// The torrent is already in the client.
    pub duplicate: bool,
    /// Structure, preselection and the reasons to ask (SPEC 7A.6). For a magnet it is empty
    /// (`groups` empty, `needs_choice` false): the files are not known before the metadata.
    pub choice: ChoicePreview,
}

/// Why a torrent needs a choice before anything is downloaded (SPEC 7A.6 table).
#[derive(Clone, Debug, PartialEq)]
pub enum ChoiceReason {
    /// Season pack, several seasons, collection, discography, bundle: more than one item.
    SeveralItems {
        /// Items in the torrent.
        items: u32,
    },
    /// Some items are already in the library (same size at the destination, or the same
    /// episode / title at another quality).
    InLibrary {
        /// Items found.
        items: u32,
    },
    /// Software for several systems; this Mac's build is preselected.
    PlatformVariants,
    /// Larger than the "very large" limit of Settings > Transfers.
    VeryLarge {
        /// Torrent size.
        bytes: u64,
        /// The limit.
        limit: u64,
    },
    /// More than the free space at the destination (or at the incomplete location).
    NotEnoughSpace {
        /// Bytes the torrent needs.
        needed: u64,
        /// Free bytes.
        free: u64,
    },
    /// The classifier is less confident than the threshold.
    Unsure {
        /// Confidence 0..=1.
        confidence: f32,
    },
    /// The watch folder is set to "ask first".
    FolderAsk,
    /// Settings > Transfers asks for every torrent.
    Always,
}

/// What the library holds of an item (SPEC 7A.6 "You have some").
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LibraryState {
    /// Not in the library (or not checked).
    Missing,
    /// A file of the same size is at the item's destination.
    InLibrary {
        /// The existing file.
        path: PathBuf,
    },
    /// The same episode / title exists at another quality or size.
    OtherQuality {
        /// The existing file.
        path: PathBuf,
    },
}

impl LibraryState {
    /// Anything of this item is in the library.
    pub fn present(&self) -> bool {
        !matches!(self, LibraryState::Missing)
    }
}

/// How a group of items is labelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChoiceGroupKind {
    /// A TV season (the number).
    Season(u16),
    /// TV specials (season 0).
    Specials,
    /// A movie collection or one folder of it.
    Collection,
    /// An artist's albums.
    Album,
    /// Software builds of one operating system.
    Platform,
    /// Junk and extras: samples, `.nfo`, featurettes. Unticked by default.
    Extras,
    /// Anything else, including files that belong to no item (kept whenever something is chosen).
    Other,
}

/// One selectable thing in the chooser: an episode, a film, an album, a title, a build.
#[derive(Clone, Debug, PartialEq)]
pub struct ChoiceItem {
    /// `S03E01`, `Night of the Living Dead (1968)`, `Album (2007)`, `App-1.2-arm64.dmg`.
    pub label: String,
    /// Stable key (see `trs_classify::Item::identity`).
    pub identity: String,
    /// Bytes of its files.
    pub bytes: u64,
    /// Torrent file indices of the item (every file: video + subtitles, tracks + cover, ...).
    pub files: Vec<u32>,
    /// Ticked in the preselection.
    pub selected: bool,
    /// Library presence.
    pub library: LibraryState,
    /// Resolution / format and source (`1080p WEB-DL`).
    pub quality: Option<String>,
    /// Software: the build is meant for this Mac.
    pub this_mac: bool,
    /// Software: operating system (`macos`, `windows`, `linux`, ...) and CPU (`arm64`, `x86_64`, ...).
    pub platform: Option<(&'static str, &'static str)>,
}

/// A group of items (a season, an artist, a platform) with its own header.
#[derive(Clone, Debug, PartialEq)]
pub struct ChoiceGroup {
    /// `Season 3`, `Specials`, `Windows`, `Extras`.
    pub label: String,
    /// Kind.
    pub kind: ChoiceGroupKind,
    /// Items.
    pub items: Vec<ChoiceItem>,
}

/// The chooser's content (SPEC 7A.6, concept 10): structure, preselection, reasons.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ChoicePreview {
    /// The torrent is private (BEP 27): its goal text names the hit-and-run floor as private.
    pub private: bool,
    /// The engine holds (or would hold) this torrent until the user chooses.
    pub needs_choice: bool,
    /// The torrent is held right now (state `AwaitingChoice`).
    pub awaiting: bool,
    /// Why; empty when no choice is needed.
    pub reasons: Vec<ChoiceReason>,
    /// `single`, `tv`, `movies`, `music`, `books`, `audiobooks`, `software`, `flat`, or empty
    /// when the files are not known.
    pub structure: &'static str,
    /// Groups in display order; the last ones are "Other files" (kept) and "Extras" (unticked).
    pub groups: Vec<ChoiceGroup>,
    /// Preselection per torrent file index (`true` = download). BEP 47 padding files are `true`
    /// (they are never chosen or skipped).
    pub files: Vec<bool>,
    /// Bytes of the preselected files.
    pub selected_bytes: u64,
    /// Files in the preselection.
    pub selected_files: u32,
    /// Whole torrent size.
    pub total_bytes: u64,
    /// Free bytes at the destination (and the incomplete location: the smaller of the two).
    pub free_bytes: Option<u64>,
    /// Where the files will be saved (library folder, or the download folder).
    pub destination: Option<PathBuf>,
    /// Category in effect, so the picker can show it (and highlight it when `Unsure`).
    pub category: Option<Category>,
    /// Classifier confidence 0..=1.
    pub confidence: f32,
    /// Items in the library were found while checking (the library was looked at).
    pub library_checked: bool,
    /// Name of the rule the torrent gets in the category shown ("Default" when none matches).
    pub rule_name: String,
    /// The seeding goal that rule gives (or the user's own, for a torrent that exists).
    pub goal: SeedGoal,
}

impl ChoicePreview {
    /// "Rule …", "Saves to …", "Then …" for the category shown; the one source of this text.
    pub fn summary(&self) -> RuleSummary {
        RuleSummary {
            rule: self.rule_name.clone(),
            saves_to: self.destination.as_ref().map(|d| d.display().to_string()).unwrap_or_default(),
            then: self.goal.then_phrase_for(self.private),
        }
    }
}

/// How a chooser is answered.
#[derive(Clone, Debug)]
pub enum ChoiceSelection {
    /// Accept the engine's preselection ("Download 3 Seasons" without touching a tick).
    Default,
    /// One priority per torrent file index (`Skip` = do not download).
    Files(Vec<FilePriority>),
}

/// What a move does (SPEC 7A.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveKind {
    /// The files are final (a seeding torrent): they are relocated, seeding goes on meanwhile.
    Finished,
    /// A download in progress: the partial files in the incomplete location are relocated while
    /// that torrent is paused.
    Incomplete,
    /// A download in progress whose incomplete location stays: only the place the files go to
    /// when the download finishes changes. Nothing is copied.
    Retarget,
}

/// One torrent a location change would move (or a move that was queued).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveItem {
    /// Torrent.
    pub id: TorrentId,
    /// Name.
    pub name: String,
    /// Bytes that would be relocated (0 for [`MoveKind::Retarget`]).
    pub bytes: u64,
    /// Where the content is now (folder or file).
    pub from: PathBuf,
    /// Where it would go.
    pub to: PathBuf,
    /// The move crosses volumes: a verified copy instead of a rename.
    pub cross_volume: bool,
    /// What it does.
    pub kind: MoveKind,
}

/// The torrents a change of locations leaves elsewhere than the settings now say: the content of
/// "Move the N existing torrents (X GB) to the new location?" (SPEC 7A.5).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MoveProposal {
    /// One entry per torrent that would move.
    pub items: Vec<MoveItem>,
    /// Their bytes in all.
    pub total_bytes: u64,
    /// How many of them cross volumes (copies; the rest are renames).
    pub cross_volume: u32,
}

/// Answer of [`crate::EngineHandle::move_torrents`] and [`crate::EngineHandle::move_to`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MoveReport {
    /// Torrents put in the move queue (they move one after the other, at most one copy per
    /// destination volume at a time). Progress is `TorrentRow::moving` and the `Move*` events.
    pub queued: Vec<TorrentId>,
    /// Torrents that were not queued, with the reason ("unknown", "waiting for a choice", ...).
    pub refused: Vec<(TorrentId, String)>,
}

/// Port mapping status for the Network settings pane.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PortMapStatus {
    /// `pcp`, `nat-pmp` or `upnp` once a gateway answered.
    pub method: Option<&'static str>,
    /// External address of the gateway, if known.
    pub external_ip: Option<std::net::Ipv4Addr>,
    /// The external address is not reachable from the Internet (private or CGNAT).
    pub behind_cgnat: bool,
    /// TCP port as seen from outside.
    pub external_tcp_port: Option<u16>,
    /// UDP port as seen from outside.
    pub external_udp_port: Option<u16>,
    /// The extra port (folder sync's, `EngineHandle::set_extra_mapped_port`), TCP, as seen from outside.
    pub extra_external_tcp_port: Option<u16>,
    /// The extra port, UDP (sync's uTP), as seen from outside.
    pub extra_external_udp_port: Option<u16>,
    /// No method worked.
    pub unavailable: bool,
}

/// Can peers on the Internet reach the listening port (Settings › Network, `seedwell status`)?
/// Two signs, neither needing an outside service: what the router said to the port mapping, and
/// whether a peer from a public IPv4 address has connected in over TCP (what a tracker's
/// "connectable" check tries) since the port was opened.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Reachability {
    /// The TCP port we listen on.
    pub port: u16,
    /// Port mapping is switched on.
    pub mapping_enabled: bool,
    /// The router's last answer; `None` while it is being asked, or with mapping off.
    pub portmap: Option<PortMapStatus>,
    /// Incoming TCP connections from public IPv4 addresses on this port.
    pub incoming: u64,
    /// When the last of them came in (Unix seconds).
    pub last_incoming_at: Option<u64>,
}

/// What [`crate::EngineHandle::shutdown`] achieved.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShutdownReport {
    /// Torrents whose tracker session was closed with a confirmed `stopped`.
    pub stopped_confirmed: u32,
    /// Torrents whose `stopped` was still unanswered at the deadline (best effort, then dropped).
    pub stopped_abandoned: u32,
    /// Time spent waiting.
    pub waited: Duration,
}

/// Diagnostic counters (Advanced tab, tests). Monotonic, except `boot_waiting`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    /// DHT lookups started by the engine.
    pub dht_lookups: u64,
    /// DHT announces started by the engine.
    pub dht_announces: u64,
    /// `ut_pex` messages sent.
    pub pex_sent: u64,
    /// `ut_pex` messages received and used.
    pub pex_received: u64,
    /// Hash failures seen.
    pub hash_failures: u64,
    /// Peers banned.
    pub peers_banned: u64,
    /// Tier promotions.
    pub promotions: u64,
    /// Tier demotions.
    pub demotions: u64,
    /// Wake-ups of the session, tracker and torrent actors (idle wake-up accounting).
    pub wakeups: u64,
    /// Incoming connections accepted.
    pub incoming_accepted: u64,
    /// Incoming connections refused (budget, unknown torrent, bad handshake).
    pub incoming_refused: u64,
    /// Of `incoming_refused`: no room (connection cap, too many handshakes pending).
    pub refused_busy: u64,
    /// Of `incoming_refused`: the handshake timed out or failed.
    pub refused_handshake: u64,
    /// Of `incoming_refused`: the torrent is unknown or not running.
    pub refused_torrent: u64,
    /// Of `incoming_refused`: the torrent refused the peer (banned, checking, full).
    pub refused_peer: u64,
    /// Full rechecks run (a clean restart runs none).
    pub rechecks: u64,
    /// Incoming uTP connections that completed the BitTorrent handshake.
    pub utp_accepted: u64,
    /// Outgoing uTP connections that completed the BitTorrent handshake.
    pub utp_dialed: u64,
    /// Incoming connections that opened with the encrypted handshake (MSE) and were accepted.
    pub mse_accepted: u64,
    /// Outgoing connections that opened with the encrypted handshake.
    pub mse_dialed: u64,
    /// Outgoing encrypted attempts that failed and were redialled as plain connections.
    pub mse_fallbacks: u64,
    /// Torrent actors started since the engine began (start-up brings them up a few at a time).
    pub actors_spawned: u64,
    /// Torrents still waiting for their turn to be brought up after start-up.
    pub boot_waiting: u64,
    /// The most torrents that hashed their files at the same time (never above `verify_slots`).
    pub verify_peak: u64,
    /// Queue plans run (a bulk pause or resume runs one).
    pub replans: u64,
}
