//! The wire protocol: JSON lines, one message per line, UTF-8, `\n` terminated.
//!
//! ```text
//! -> {"v":1,"id":1,"method":"hello","params":{"client":"claude","protocol":1}}
//! <- {"v":1,"id":1,"result":{"protocol":1,"server":"seedwell 0.0.1","permission":"control",...}}
//! -> {"v":1,"id":2,"method":"list","params":{"filter":"downloading"}}
//! <- {"v":1,"id":2,"result":{...}}
//! <- {"v":1,"event":"finished","seq":41,"data":{...}}          (after events_subscribe)
//! <- {"v":1,"id":3,"error":{"code":"permission_denied","message":"..."}}
//! ```
//!
//! Every method has one params struct below and one required [`Permission`]; the table is the
//! `methods!` invocation, which is also what the README documents. Unknown fields are rejected so
//! an agent's typo is an error instead of a silently ignored option.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use trs_state::config::Permission;

/// Protocol version spoken by this build. Bumped only for incompatible changes; new optional
/// fields and new methods do not bump it.
pub const PROTOCOL_VERSION: u32 = 1;

/// Most ids one call may name.
pub const MAX_IDS: usize = 1000;

/// A request id chosen by the client and echoed in the response.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Id {
    /// Numeric id.
    Num(u64),
    /// String id (at most 128 bytes).
    Str(String),
}

/// Machine-readable error class. Stable; new codes may be added (clients treat unknown as `other`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// The line is not valid JSON.
    ParseError,
    /// Valid JSON, but not a request.
    InvalidRequest,
    /// No such method.
    UnknownMethod,
    /// The params do not match the method.
    InvalidParams,
    /// The agent permission level is too low for this method.
    PermissionDenied,
    /// Agent access is switched off in Settings.
    AgentAccessOff,
    /// The first message of a connection must be `hello`.
    HelloRequired,
    /// Too many requests; see `data.retry_after_ms`.
    RateLimited,
    /// The server has its maximum number of clients.
    TooManyConnections,
    /// The request line exceeded the size cap.
    RequestTooLarge,
    /// No such transfer (or file, or profile).
    NotFound,
    /// An id prefix matches more than one transfer.
    Ambiguous,
    /// The transfer is already in the client.
    Duplicate,
    /// The arguments are well-formed but not acceptable (bad torrent, bad setting).
    Invalid,
    /// The confirmation token is unknown, used or belongs to another request.
    InvalidToken,
    /// The confirmation token expired; start the removal again.
    TokenExpired,
    /// This build or configuration cannot do that.
    Unsupported,
    /// The engine reported a failure.
    Engine,
    /// A bug or an unexpected failure inside the server.
    Internal,
    /// The app is shutting down.
    ShuttingDown,
    /// Waited too long.
    Timeout,
    /// The protocol version is not supported.
    UnsupportedVersion,
    /// A code from a newer server.
    #[serde(other)]
    Other,
}

impl ErrorCode {
    /// The stable snake_case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ParseError => "parse_error",
            Self::InvalidRequest => "invalid_request",
            Self::UnknownMethod => "unknown_method",
            Self::InvalidParams => "invalid_params",
            Self::PermissionDenied => "permission_denied",
            Self::AgentAccessOff => "agent_access_off",
            Self::HelloRequired => "hello_required",
            Self::RateLimited => "rate_limited",
            Self::TooManyConnections => "too_many_connections",
            Self::RequestTooLarge => "request_too_large",
            Self::NotFound => "not_found",
            Self::Ambiguous => "ambiguous",
            Self::Duplicate => "duplicate",
            Self::Invalid => "invalid",
            Self::InvalidToken => "invalid_token",
            Self::TokenExpired => "token_expired",
            Self::Unsupported => "unsupported",
            Self::Engine => "engine",
            Self::Internal => "internal",
            Self::ShuttingDown => "shutting_down",
            Self::Timeout => "timeout",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Other => "other",
        }
    }
}

/// An error answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApiError {
    /// Class of error.
    pub code: ErrorCode,
    /// Plain-language message for people and agents. Never contains passkeys or file contents.
    pub message: String,
    /// Extra structured detail (`retry_after_ms`, `required`, candidate ids, field errors).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ApiError {
    /// Error without extra data.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    /// Attach structured detail.
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    /// `invalid_params`.
    pub fn params(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidParams, message)
    }

    /// `invalid`.
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Invalid, message)
    }

    /// `not_found`.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }

    /// `internal`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::Internal, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for ApiError {}

/// A message the server sends: a response (`id` + `result`/`error`) or a pushed event.
///
/// One struct for both so a client decodes a line once and branches on which fields are set.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct ServerLine {
    /// Protocol version.
    #[serde(default)]
    pub v: u32,
    /// Id of the request this answers; `null` when the request could not be parsed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Id>,
    /// Success payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
    /// Event kind, for pushed events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// Event sequence number (monotonic per server run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// Event payload.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ServerLine {
    /// A success response.
    pub fn ok(id: Option<Id>, result: Value) -> Self {
        Self { v: PROTOCOL_VERSION, id, result: Some(result), ..Self::default() }
    }

    /// A failure response.
    pub fn err(id: Option<Id>, error: ApiError) -> Self {
        Self { v: PROTOCOL_VERSION, id, error: Some(error), ..Self::default() }
    }

    /// A pushed event.
    pub fn event(e: &crate::types::EventDto) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            event: Some(e.kind.clone()),
            seq: Some(e.seq),
            data: serde_json::to_value(e).ok(),
            ..Self::default()
        }
    }

    /// One line of JSON including the trailing newline.
    pub fn to_line(&self) -> Vec<u8> {
        let mut v = serde_json::to_vec(self).unwrap_or_else(|_| {
            br#"{"v":1,"error":{"code":"internal","message":"unserializable response"}}"#.to_vec()
        });
        v.push(b'\n');
        v
    }
}

// ----------------------------------------------------------------------------- params

/// No parameters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// `hello`: the first message of every connection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HelloParams {
    /// Who is calling ("claude", "seedwell-cli", ...). Shown in the activity log.
    pub client: String,
    /// Highest protocol version the client speaks.
    #[serde(default)]
    pub protocol: Option<u32>,
    /// The client's own version, for the log.
    #[serde(default)]
    pub version: Option<String>,
}

/// Which transfers `list` returns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ListFilter {
    /// Everything (default).
    #[default]
    All,
    /// Downloading now.
    Downloading,
    /// Waiting for a download slot.
    Queued,
    /// Sharing: seeding, including low power.
    Seeding,
    /// Seeding idle torrents (parked Cold/Warm).
    LowPower,
    /// Paused.
    Paused,
    /// Failed or missing files.
    Error,
    /// Finished at some point (torrents seeding afterwards included).
    Finished,
    /// Finished, newest first.
    Recent,
    /// Verifying files.
    Checking,
    /// Fetching magnet metadata.
    Metadata,
    /// Held until the user (or an agent) chooses what to download.
    AwaitingChoice,
    /// Held for the grace period before an auto-add starts.
    Pending,
}

/// Torrent or direct download.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KindFilter {
    /// BitTorrent.
    Torrent,
    /// Direct HTTP(S) download.
    Direct,
}

/// `list` parameters.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListParams {
    /// State filter.
    #[serde(default)]
    pub filter: ListFilter,
    /// Tracker profile id or host (substring, case-insensitive).
    #[serde(default)]
    pub tracker: Option<String>,
    /// Category name (movie, tv, music, audiobook, book, software, other).
    #[serde(default)]
    pub category: Option<String>,
    /// Case-insensitive substring of the name.
    #[serde(default)]
    pub text: Option<String>,
    /// Torrents or direct downloads only.
    #[serde(default)]
    pub kind: Option<KindFilter>,
    /// Page size (default 50, at most 500).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Rows to skip.
    #[serde(default)]
    pub offset: Option<u32>,
}

/// `info` parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InfoParams {
    /// Transfer id (40 hex characters) or a unique prefix of at least 6.
    pub id: String,
    /// Include files (default true).
    #[serde(default)]
    pub files: Option<bool>,
    /// Include trackers (default true).
    #[serde(default)]
    pub trackers: Option<bool>,
    /// Include connected peers (default true).
    #[serde(default)]
    pub peers: Option<bool>,
    /// Most files returned (default 2000).
    #[serde(default)]
    pub max_files: Option<u32>,
    /// Most peers returned (default 100).
    #[serde(default)]
    pub max_peers: Option<u32>,
}

/// `add` parameters: exactly one of `source`, `magnet`, `torrent_path`, `torrent_base64`, `url`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddParams {
    /// Auto-detected: a `magnet:` link, an `http(s)://` URL (direct download) or a file path.
    #[serde(default)]
    pub source: Option<String>,
    /// A magnet link.
    #[serde(default)]
    pub magnet: Option<String>,
    /// Absolute path (or `~/...`) of a `.torrent` file; the server reads it.
    #[serde(default)]
    pub torrent_path: Option<String>,
    /// The `.torrent` bytes, base64 (standard alphabet).
    #[serde(default)]
    pub torrent_base64: Option<String>,
    /// An http(s) URL to download directly (not BitTorrent).
    #[serde(default)]
    pub url: Option<String>,
    /// Folder to save into. Absolute or `~/...`. Default: the configured download folder.
    #[serde(default)]
    pub save_path: Option<String>,
    /// Add without starting.
    #[serde(default)]
    pub paused: bool,
    /// Category (movie, tv, music, audiobook, book, software, other); default: detect.
    #[serde(default)]
    pub category: Option<String>,
    /// Priority (high, normal, low); default: the matching rule's.
    #[serde(default)]
    pub priority: Option<PriorityParam>,
    /// Rule id to force (see `rules`).
    #[serde(default)]
    pub rule: Option<String>,
    /// Put at the top of the queue.
    #[serde(default)]
    pub queue_top: bool,
    /// Start at once instead of waiting out the grace period (`transfers.start_delay_secs`, 10 s
    /// by default), during which a new torrent is held and silent on the network.
    #[serde(default)]
    pub now: bool,
    /// Expected SHA-256 (hex) of a direct download.
    #[serde(default)]
    pub sha256: Option<String>,
    /// Peers `ip:port` to try right away (advanced; tests).
    #[serde(default)]
    pub peers: Vec<String>,
}

/// Priority names on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityParam {
    /// High.
    High,
    /// Normal.
    Normal,
    /// Low.
    Low,
}

impl PriorityParam {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Normal => "normal",
            Self::Low => "low",
        }
    }
}

/// File priority names on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilePriorityParam {
    /// Do not download.
    Skip,
    /// Low.
    Low,
    /// Normal.
    Normal,
    /// High.
    High,
}

impl FilePriorityParam {
    /// Wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Skip => "skip",
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
        }
    }
}

/// Which transfers a command applies to: `ids`, or `all` for pause/resume.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Targets {
    /// Ids (40 hex characters, or unique prefixes of at least 6).
    #[serde(default)]
    pub ids: Vec<String>,
    /// Every transfer (only where documented).
    #[serde(default)]
    pub all: bool,
}

/// Commands that name ids only.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdsParams {
    /// Ids (40 hex characters, or unique prefixes of at least 6).
    pub ids: Vec<String>,
}

/// `reannounce`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReannounceParams {
    /// Ids (40 hex characters, or unique prefixes of at least 6).
    pub ids: Vec<String>,
    /// Also override the tracker's minimum interval (once per tracker per 5 minutes).
    #[serde(default)]
    pub force: bool,
    /// Wait up to 30 s for the tracker's answer and report it.
    #[serde(default)]
    pub wait: bool,
}

/// `set_priority`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetPriorityParams {
    /// Transfers.
    pub ids: Vec<String>,
    /// New priority.
    pub priority: PriorityParam,
}

/// One entry of a partial file-priority update.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileSet {
    /// File index in `info` order.
    pub index: u32,
    /// Priority.
    pub priority: FilePriorityParam,
}

/// `set_file_priorities`: either the complete list in file order, or a partial `set`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetFilePrioritiesParams {
    /// Transfer.
    pub id: String,
    /// One priority per file, in `info` order.
    #[serde(default)]
    pub priorities: Option<Vec<FilePriorityParam>>,
    /// Change only these files.
    #[serde(default)]
    pub set: Option<Vec<FileSet>>,
}

/// `set_category`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetCategoryParams {
    /// Transfers.
    pub ids: Vec<String>,
    /// Category name, or null to detect again.
    #[serde(default)]
    pub category: Option<String>,
}

/// What to do on reaching a seeding goal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtGoalParam {
    /// Stop (pause), keep in the list.
    Stop,
    /// Remove from the client, keep files.
    Remove,
    /// Remove and delete files. Needs permission `full`.
    RemoveAndDelete,
}

/// `set_seed_goal`. With nothing set the transfer seeds forever.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetSeedGoalParams {
    /// Transfers.
    pub ids: Vec<String>,
    /// Stop at this upload/size ratio.
    #[serde(default)]
    pub ratio: Option<f32>,
    /// Stop after seeding this many seconds.
    #[serde(default)]
    pub seed_secs: Option<u32>,
    /// Never act before this many seconds of seeding (hit-and-run floor).
    #[serde(default)]
    pub min_seed_secs: Option<u32>,
    /// true: the first condition reached wins (default); false: all must be reached.
    #[serde(default)]
    pub any: Option<bool>,
    /// Action at the goal (default stop).
    #[serde(default)]
    pub at_goal: Option<AtGoalParam>,
    /// Stop as soon as the download finishes.
    #[serde(default)]
    pub stop_on_finish: bool,
    /// Seed forever (clears every goal).
    #[serde(default)]
    pub forever: bool,
    /// Go back to the goal the transfers' rule gives them (undoes an earlier `set_seed_goal`).
    /// Cannot be combined with any other option.
    #[serde(default)]
    pub rule_default: bool,
}

/// `set_limits`: absent fields are unchanged; `0` means unlimited. Bytes per second.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetLimitsParams {
    /// Global download limit.
    #[serde(default)]
    pub down_limit: Option<u64>,
    /// Global upload limit.
    #[serde(default)]
    pub up_limit: Option<u64>,
    /// Download limit while alternative speed is on.
    #[serde(default)]
    pub alt_down_limit: Option<u64>,
    /// Upload limit while alternative speed is on.
    #[serde(default)]
    pub alt_up_limit: Option<u64>,
    /// Switch alternative ("Limit Speed") limits on or off.
    #[serde(default)]
    pub alt_speed: Option<bool>,
}

/// `set_tracker_pex`: allow or forbid peer exchange (BEP 11) for the private torrents of a tracker.
/// Off by default; the master switch (Settings > Network, `network.pex`) overrides it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTrackerPexParams {
    /// A favorite's id, an automatic tracker's domain, or `other-private` (`trackers` lists them).
    pub id: String,
    /// Allow (`true`) or forbid (`false`).
    pub allow: bool,
}

/// `set_tracker_limits`: the shared speed caps of a favorite tracker; absent fields are
/// unchanged, `0` means unlimited. Bytes per second, for all of the tracker's torrents together.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetTrackerLimitsParams {
    /// The favorite's id (`trackers` lists them).
    pub id: String,
    /// Shared download cap.
    #[serde(default)]
    pub down_limit: Option<u64>,
    /// Shared upload cap.
    #[serde(default)]
    pub up_limit: Option<u64>,
}

/// `remove` (step 1).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveParams {
    /// Transfers to remove.
    pub ids: Vec<String>,
    /// Also delete the downloaded files (needs permission `full`).
    #[serde(default)]
    pub delete_files: bool,
}

/// `remove_confirm` (step 2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoveConfirmParams {
    /// The token returned by `remove`.
    pub token: String,
}

/// `tracker_stats`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerStatsParams {
    /// A profile id, `public`, `other-private` or `all`.
    #[serde(default)]
    pub tracker: Option<String>,
    /// Days of daily series (default 30, at most 400).
    #[serde(default)]
    pub days: Option<u32>,
}

/// `settings_get`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsGetParams {
    /// Dotted keys (`transfers.download_limit`); absent: every allow-listed setting.
    #[serde(default)]
    pub keys: Option<Vec<String>>,
}

/// `settings_set`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsSetParams {
    /// Dotted key to new value. All changes apply together or not at all.
    pub changes: Map<String, Value>,
}

/// `events_subscribe`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsSubscribeParams {
    /// Only these event kinds (absent: all).
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

/// `events`: poll the recent event buffer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsParams {
    /// Return events with `seq` greater than this (0: the oldest buffered).
    #[serde(default)]
    pub since: u64,
    /// Most events returned (default 100, at most 500).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Only these kinds.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

/// `wait_for`: block until transfers finish.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitForParams {
    /// Transfers to wait for.
    pub ids: Vec<String>,
    /// `all` (default) or `any` of them.
    #[serde(default)]
    pub mode: Option<WaitMode>,
    /// Seconds to wait (default 60, at most 300). The answer says whether it timed out.
    #[serde(default)]
    pub timeout_secs: Option<u32>,
}

/// How many of the ids must be done.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitMode {
    /// Every id.
    All,
    /// Any id.
    Any,
}

/// `choice`: the structure of a torrent that is held (or running).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChoiceParams {
    /// Transfer id.
    pub id: String,
}

/// `choose_files`: answer a held torrent's chooser (or change which files a running one wants).
/// Give exactly one of `accept_default`, `files`, `skip`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChooseFilesParams {
    /// Transfer id.
    pub id: String,
    /// Take the engine's preselection (what the library does not have, this Mac's build, ...).
    #[serde(default)]
    pub accept_default: bool,
    /// Download only these file indices (as in `info` / `choice`).
    #[serde(default)]
    pub files: Option<Vec<u32>>,
    /// Download everything except these file indices.
    #[serde(default)]
    pub skip: Option<Vec<u32>>,
}

/// `cancel_choice`: remove a held torrent; nothing was written, nothing is left.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelChoiceParams {
    /// Transfer id.
    pub id: String,
    /// Keep the `.torrent` file the torrent came from. By default it goes to the Trash (SPEC 7A.6),
    /// as Cancel does in the app; there is no file to trash for a magnet link or a web address.
    #[serde(default)]
    pub keep_file: bool,
}

/// `debug_trackers`: how the trackers of one or more torrents are doing.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugTrackersParams {
    /// One transfer (id or unique prefix); absent = the active ones (at most 100 torrents).
    #[serde(default)]
    pub id: Option<String>,
    /// Only trackers whose host contains this text (case-insensitive).
    #[serde(default)]
    pub tracker: Option<String>,
}

/// `sim_allow`: let torrents added by `seedwell sim` announce to the mock tracker on 127.0.0.1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SimAllowParams {
    /// Info-hashes (40 hex characters) of the simulated torrents; they need not be in the client yet.
    pub ids: Vec<String>,
    /// Allow (true) or stop allowing (false).
    pub allow: bool,
}

/// `debug_peers`: the peers of one torrent.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DebugPeersParams {
    /// Transfer id or unique prefix.
    pub id: String,
    /// Mask the last part of every address (`203.0.113.x`).
    #[serde(default)]
    pub redact: bool,
}

/// `move`: relocate transfers' files. Needs `full`, like a custom save folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveParams {
    /// Transfers.
    pub ids: Vec<String>,
    /// Absolute folder (or `~/...`) to move below, which also pins the transfers there. Absent:
    /// move to where the settings (category and rule locations) say they belong.
    #[serde(default)]
    pub path: Option<String>,
}

/// `stream_url`: serve a file of a transfer over loopback HTTP while it downloads (play while
/// downloading). Selects the file for download if it was skipped.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamUrlParams {
    /// Transfer id.
    pub id: String,
    /// File index (see `info`). Absent: the largest video or audio file.
    #[serde(default)]
    pub file: Option<u32>,
}

/// `stop_stream`: close a stream opened by `stream_url`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopStreamParams {
    /// The `token` `stream_url` returned.
    pub token: String,
}

/// `activity`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityParams {
    /// Most entries (default 50, newest first).
    #[serde(default)]
    pub limit: Option<u32>,
}

/// `awake.set`: keep the Mac awake while an agent works (SPEC 9A.7), or let it sleep again.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AwakeSetParams {
    /// Which lease (`agent:<name>`). Taking it again replaces the time.
    pub key: String,
    /// What is working, as people read it ("kglite").
    #[serde(default)]
    pub who: Option<String>,
    /// How long, in seconds (at most a day). Absent or 0: give the lease back.
    #[serde(default)]
    pub secs: Option<u64>,
}

/// `tracker_log`: the newest developer-mode tracker log entries (developer mode must be on to
/// have any). Passkeys and other secrets are masked in the file itself.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrackerLogParams {
    /// Most entries (default 50, at most 500; the newest).
    #[serde(default)]
    pub limit: Option<u32>,
    /// A torrent: info-hash prefix, or a word of its title.
    #[serde(default)]
    pub torrent: Option<String>,
    /// A tracker: part of its host.
    #[serde(default)]
    pub tracker: Option<String>,
    /// One event: started, regular, completed, stopped.
    #[serde(default)]
    pub event: Option<String>,
    /// Only failed exchanges.
    #[serde(default)]
    pub errors_only: Option<bool>,
    /// Only entries newer than this (Unix milliseconds): for following the log.
    #[serde(default)]
    pub since_ms: Option<u64>,
}

// ----------------------------------------------------------------------------- folder sync (SPEC 7G)

/// A folder: its id (`MFZWI3D-BONSGYC-…`), an unambiguous prefix of it (the first group is enough), or its
/// name on this Mac (any case).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncFolderParams {
    /// The folder.
    pub folder: String,
}

/// `sync.add`: share a folder of this Mac (this Mac becomes its owner).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncAddParams {
    /// Absolute path (or `~/...`) of an existing folder.
    pub path: String,
}

/// `sync.share`: make an invitation link to a folder.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncShareParams {
    /// The folder.
    pub folder: String,
    /// The role it admits at: `read_write` (default), `read_only`, `admin` or `owner` (the last two: owners only).
    #[serde(default)]
    pub role: Option<String>,
    /// How many devices it admits (default 1; 0 = unlimited; at most 1000).
    #[serde(default)]
    pub uses: Option<u32>,
    /// Days the link admits (default 7; 0 = no expiry).
    #[serde(default)]
    pub days: Option<u32>,
    /// One link for the folder and a channel: the channel (its id, a prefix of it or its name), or `new` for a new
    /// channel named after the folder. The channel's invitation has the same role, uses and expiry.
    #[serde(default)]
    pub chat: Option<String>,
}

/// `sync.set_share_until`: how long a folder is shared (owners only). After the end every device but the owners
/// is cut off (it keeps its files) and no link admits anyone.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncShareUntilParams {
    /// The folder.
    pub folder: String,
    /// Days from now (or from the current end with `extend`); 0 = no end.
    pub days: u32,
    /// Add the days to the current end instead of counting from now.
    #[serde(default)]
    pub extend: bool,
}

/// A channel (`chat.members`, `chat.leave`): its id, an unambiguous prefix of it, or its name on this Mac.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatChannelParams {
    /// The channel.
    pub channel: String,
}

/// `chat.new`: make a channel (this Mac is its owner).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatNewParams {
    /// Its name.
    pub name: String,
}

/// `chat.join`: join a channel by its `seedwell://chat/` link.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatJoinParams {
    /// The link.
    pub link: String,
}

/// `chat.share`: make an invitation link to a channel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatShareParams {
    /// The channel.
    pub channel: String,
    /// As for `sync.share`.
    #[serde(default)]
    pub role: Option<String>,
    /// As for `sync.share`.
    #[serde(default)]
    pub uses: Option<u32>,
    /// As for `sync.share`.
    #[serde(default)]
    pub days: Option<u32>,
}

/// `chat.set_role`: change a device's role in a channel (owners only).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatSetRoleParams {
    /// The channel.
    pub channel: String,
    /// As for `sync.set_role`.
    pub device: String,
    /// As for `sync.set_role`.
    pub role: String,
}

/// `chat.remove_device`: remove a device from a channel (owners only).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatDeviceParams {
    /// The channel.
    pub channel: String,
    /// As for `sync.remove_device`.
    pub device: String,
}

/// `chat.set_share_until`: how long a channel is shared (owners only).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatShareUntilParams {
    /// The channel.
    pub channel: String,
    /// As for `sync.set_share_until`.
    pub days: u32,
    /// As for `sync.set_share_until`.
    #[serde(default)]
    pub extend: bool,
}

/// `chat.send`: post to a channel. Needs control, and the channel's agent switch at Read and post.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatSendParams {
    /// The channel.
    pub channel: String,
    /// The message (text up to 16 KiB, JSON up to 64 KiB).
    pub body: String,
    /// The id of the message it answers.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// The body is JSON (a message for agents).
    #[serde(default)]
    pub json: bool,
    /// The topic it goes in (its id); absent: General.
    #[serde(default)]
    pub topic: Option<String>,
    /// The agent name it is posted under (1 to 64 characters, e.g. the project's); absent: the client's name.
    #[serde(default)]
    pub agent: Option<String>,
}

/// `chat.new_topic`: a new topic in a channel (anyone who may post; agents: the channel's switch at post).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatNewTopicParams {
    /// The channel.
    pub channel: String,
    /// Its title.
    pub title: String,
    /// The agent name it is posted under (1 to 64 characters, e.g. the project's); absent: the client's name.
    #[serde(default)]
    pub agent: Option<String>,
}

/// A device for private messages (`chat.dm_read`): its id, a prefix of it, or its name.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatDmParams {
    /// The device.
    pub device: String,
    /// Only messages before this one (its id).
    #[serde(default)]
    pub before: Option<String>,
    /// How many (default 50, at most 1000).
    #[serde(default)]
    pub last: Option<u32>,
}

/// `chat.dm_send`: a private message to a device that shares a channel with this Mac. Needs control, and the
/// Mac's switch for private messages at Read and post.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatDmSendParams {
    /// The device: its id, a prefix of it, or its name.
    pub device: String,
    /// The message.
    pub body: String,
    /// The id of the message it answers.
    #[serde(default)]
    pub reply_to: Option<String>,
    /// The body is JSON.
    #[serde(default)]
    pub json: bool,
    /// The agent name it is posted under (1 to 64 characters, e.g. the project's); absent: the client's name.
    #[serde(default)]
    pub agent: Option<String>,
}

/// `chat.dm_set_agents`: what agents on this Mac may do with private messages.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatDmAgentsParams {
    /// `off`, `read` or `post`.
    pub agents: String,
}

/// `chat.read`: a channel's messages. Needs read, and the channel's agent switch at Read or above.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatReadParams {
    /// The channel.
    pub channel: String,
    /// Only messages after this one (its id).
    #[serde(default)]
    pub since: Option<String>,
    /// Only the newest this many (default 50, at most 1000).
    #[serde(default)]
    pub last: Option<u32>,
    /// Only this topic's (its id, or `general`); absent: every message, topics' too.
    #[serde(default)]
    pub topic: Option<String>,
    /// Only what this agent (by name) has not been given yet, its own posts left out; its place then moves on
    /// (per agent and topic, kept by the channel). With `last` (default 200), not `since`.
    #[serde(default)]
    pub unread_for: Option<String>,
    /// With `unread_for`: say what is unread without moving the place.
    #[serde(default)]
    pub peek: bool,
}

/// `chat.add_address`: where devices of a channel can be reached (`host:port`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatAddressParams {
    /// The channel.
    pub channel: String,
    /// `host:port`.
    pub address: String,
}

/// `chat.rename`: a channel's name on this Mac (each device names its channels, as folders).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatRenameParams {
    /// The channel.
    pub channel: String,
    /// Its new name.
    pub name: String,
}

/// `chat.mute`: no notifications for a channel (or again).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatMuteParams {
    /// The channel.
    pub channel: String,
    /// Muted.
    pub muted: bool,
}

/// `chat.set_agents`: what agents on this Mac may do in a channel.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatAgentsParams {
    /// The channel.
    pub channel: String,
    /// `off`, `read` or `post` (read and post).
    pub agents: String,
}

/// `chat.set_wake`: the command run for each message from another device (while agents may read the channel).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatWakeParams {
    /// The channel.
    pub channel: String,
    /// The command (a shell line; the message comes as JSON on its standard input). Absent or empty: none.
    #[serde(default)]
    pub command: Option<String>,
}

/// A device of a folder: `sync.remove_device`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncDeviceParams {
    /// The folder.
    pub folder: String,
    /// The device: its id, an unambiguous prefix of it (the first group is enough), or its name.
    pub device: String,
}

/// `sync.set_role`: change a device's role (owners only).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSetRoleParams {
    /// The folder.
    pub folder: String,
    /// The device, as for [`SyncDeviceParams`].
    pub device: String,
    /// `owner`, `admin`, `read_write` or `read_only`.
    pub role: String,
}

/// `sync.revoke_invite`: an invitation admits nobody from now on.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRevokeInviteParams {
    /// The folder.
    pub folder: String,
    /// The invitation's id (from `sync.invites`).
    pub invite: String,
}

/// `sync.join`: sync a folder shared with you.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncJoinParams {
    /// A `seedwell://sync/…` invitation link. A secret: it is never logged.
    pub key_or_link: String,
    /// Where the folder goes (absolute or `~/...`; created when missing). Absent: the default location
    /// for joined folders (Settings › Folder Sync, else `~/Seedwell`) and the link's name.
    #[serde(default)]
    pub path: Option<String>,
}

/// `sync.rename`: rename a folder on this Mac (other devices keep their names).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRenameParams {
    /// The folder.
    pub folder: String,
    /// Its new name (one path component).
    pub name: String,
}

/// `sync.move`: move a folder on this Mac.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncMoveParams {
    /// The folder.
    pub folder: String,
    /// The new path of the folder itself (absolute or `~/...`; must not exist yet).
    pub path: String,
}

/// `sync.set_lan_only`: only sync a folder with devices on the local network (or everywhere again).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncLanOnlyParams {
    /// The folder.
    pub folder: String,
    /// True: only devices on the local network.
    pub lan_only: bool,
}

/// `sync.resolve`: settle one conflict copy. Nothing is deleted: the version not kept goes to the folder's archive.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncResolveParams {
    /// The folder.
    pub folder: String,
    /// The conflict copy, relative to the folder (as `sync.conflicts` lists it).
    pub copy: String,
    /// `original` (keep the file, archive the copy), `copy` (the copy replaces the file, which is archived) or
    /// `both` (the copy gets a plain name of its own).
    pub keep: String,
}

/// `sync.add_address`: where devices of this folder can be reached, typed in by hand.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncAddAddressParams {
    /// The folder.
    pub folder: String,
    /// `ip:port` or `host:port`.
    pub address: String,
}

/// `sync.remove` (step 1): stop syncing a folder on this Mac.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncRemoveParams {
    /// The folder.
    pub folder: String,
    /// Also move the folder to the Trash (needs permission `full`). Otherwise the files stay.
    #[serde(default)]
    pub delete_files: bool,
}

/// `sync.set_settings`: absent fields are unchanged; give at least one (`sync.settings` reads them).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncSettingsParams {
    /// The name other devices see (empty = the computer's name).
    #[serde(default)]
    pub device_name: Option<String>,
    /// The sync port (0 = choose one); applies at the next start.
    #[serde(default)]
    pub port: Option<u16>,
    /// Find devices on the local network.
    #[serde(default)]
    pub lan_discovery: Option<bool>,
    /// Find devices over the Internet (the Mainline DHT, daily rotating topics).
    #[serde(default)]
    pub dht_discovery: Option<bool>,
    /// Relay between this user's own devices.
    #[serde(default)]
    pub relay_own_devices: Option<bool>,
    /// Also relay between other members of shared folders.
    #[serde(default)]
    pub relay_everyone: Option<bool>,
    /// Internet upload limit, bytes per second (0 = none). The LAN is never limited.
    #[serde(default)]
    pub up_limit: Option<u64>,
    /// Internet download limit, bytes per second (0 = none).
    #[serde(default)]
    pub down_limit: Option<u64>,
    /// Where joined folders go by default (absolute or `~/...`; empty = `~/Seedwell`).
    #[serde(default)]
    pub default_location: Option<String>,
}

impl SyncSettingsParams {
    /// Nothing to change.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

// ----------------------------------------------------------------------------- the method table

macro_rules! methods {
    ($( $(#[$doc:meta])* $var:ident($ty:ty) = $name:literal, $perm:ident, $mutating:literal; )+) => {
        /// A decoded request.
        #[derive(Clone, Debug, PartialEq)]
        pub enum Request { $( $(#[$doc])* $var($ty), )+ }

        /// The method name without params.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum Method { $( $(#[$doc])* $var, )+ }

        impl Method {
            /// Every method, in documentation order.
            pub const ALL: &'static [Method] = &[ $( Method::$var, )+ ];

            /// Wire name.
            pub const fn name(self) -> &'static str {
                match self { $( Method::$var => $name, )+ }
            }

            /// Lowest permission level that may call it (`remove_confirm` needs `full` too when
            /// the removal deletes files; the server checks that against the stored plan).
            pub const fn required(self) -> Permission {
                match self { $( Method::$var => Permission::$perm, )+ }
            }

            /// Whether the call changes anything (these are written to the activity log).
            pub const fn mutating(self) -> bool {
                match self { $( Method::$var => $mutating, )+ }
            }

            /// Look a method up by wire name.
            pub fn from_name(name: &str) -> Option<Method> {
                match name { $( $name => Some(Method::$var), )+ _ => None }
            }
        }

        impl Request {
            /// Which method this is.
            pub fn method(&self) -> Method {
                match self { $( Request::$var(_) => Method::$var, )+ }
            }

            /// Decode `params` for `method`. `null` or absent params mean `{}`.
            pub fn decode(method: &str, params: Value) -> Result<Request, ApiError> {
                let params = if params.is_null() { Value::Object(Map::new()) } else { params };
                match method {
                    $( $name => serde_json::from_value::<$ty>(params)
                        .map(Request::$var)
                        .map_err(|e| ApiError::params(format!("{}: {}", $name, short_serde_error(&e)))), )+
                    other => Err(ApiError::new(
                        ErrorCode::UnknownMethod,
                        format!("unknown method `{}`", clip(other, 64)),
                    )),
                }
            }

            /// The params as JSON (for clients).
            pub fn params(&self) -> Value {
                match self { $( Request::$var(p) => serde_json::to_value(p).unwrap_or(Value::Null), )+ }
            }
        }
    };
}

methods! {
    /// Handshake: names the client, returns server version and permission.
    Hello(HelloParams) = "hello", ReadOnly, false;
    /// Liveness check.
    Ping(Empty) = "ping", ReadOnly, false;
    /// Totals: speeds, counts by state, limits.
    Status(Empty) = "status", ReadOnly, false;
    /// Filtered, paginated transfer list.
    List(ListParams) = "list", ReadOnly, false;
    /// One transfer with files, trackers and peers.
    Info(InfoParams) = "info", ReadOnly, false;
    /// Tracker profiles with live transfer counts.
    Trackers(Empty) = "trackers", ReadOnly, false;
    /// Historical statistics for a tracker profile.
    TrackerStats(TrackerStatsParams) = "tracker_stats", ReadOnly, false;
    /// The seeding rules.
    Rules(Empty) = "rules", ReadOnly, false;
    /// Allow-listed settings.
    SettingsGet(SettingsGetParams) = "settings_get", Full, false;
    /// Start receiving pushed events on this connection.
    EventsSubscribe(EventsSubscribeParams) = "events_subscribe", ReadOnly, false;
    /// Stop pushed events.
    EventsUnsubscribe(Empty) = "events_unsubscribe", ReadOnly, false;
    /// Poll buffered events since a cursor.
    Events(EventsParams) = "events", ReadOnly, false;
    /// Block until transfers finish.
    WaitFor(WaitForParams) = "wait_for", ReadOnly, false;
    /// Add a torrent, magnet or direct download.
    Add(AddParams) = "add", Control, true;
    /// Pause transfers.
    Pause(Targets) = "pause", Control, true;
    /// Resume transfers.
    Resume(Targets) = "resume", Control, true;
    /// Move transfers to the front of the queue.
    MoveToTop(IdsParams) = "move_to_top", Control, true;
    /// Queue and bandwidth priority.
    SetPriority(SetPriorityParams) = "set_priority", Control, true;
    /// Per-file priorities (skip files).
    SetFilePriorities(SetFilePrioritiesParams) = "set_file_priorities", Control, true;
    /// Category.
    SetCategory(SetCategoryParams) = "set_category", Control, true;
    /// Seeding goal.
    SetSeedGoal(SetSeedGoalParams) = "set_seed_goal", Control, true;
    /// Global limits and alternative speed.
    SetLimits(SetLimitsParams) = "set_limits", Control, true;
    /// Shared download / upload caps of a favorite tracker.
    SetTrackerLimits(SetTrackerLimitsParams) = "set_tracker_limits", Control, true;
    /// Peer exchange for a private tracker's torrents.
    SetTrackerPex(SetTrackerPexParams) = "set_tracker_pex", Full, true;
    /// Ask the trackers for peers now (the engine enforces the tracker's minimum interval).
    Reannounce(ReannounceParams) = "reannounce", Control, true;
    /// Verify files again.
    Recheck(IdsParams) = "recheck", Control, true;
    /// Step 1 of removal: returns a plan and a confirmation token.
    Remove(RemoveParams) = "remove", Control, true;
    /// Step 2 of removal: perform the planned removal.
    RemoveConfirm(RemoveConfirmParams) = "remove_confirm", Control, true;
    /// Change allow-listed settings.
    SettingsSet(SettingsSetParams) = "settings_set", Full, true;
    /// Developer mode: the newest tracker log entries (requests, answers, schedules, decisions).
    TrackerLog(TrackerLogParams) = "tracker_log", ReadOnly, false;
    /// The agent activity log.
    Activity(ActivityParams) = "activity", Full, false;
    /// Keep the Mac awake while an agent works, or let it sleep again (not in the activity log: it changes nothing Seedwell keeps).
    AwakeSet(AwakeSetParams) = "awake.set", Control, false;
    /// Who keeps the Mac awake, until when, and whether this Seedwell can.
    AwakeStatus(Empty) = "awake.status", ReadOnly, false;
    /// The structure and preselection of a torrent that is held (or running).
    Choice(ChoiceParams) = "choice", ReadOnly, false;
    /// Answer a held torrent's chooser: choose the files; it starts.
    ChooseFiles(ChooseFilesParams) = "choose_files", Control, true;
    /// Cancel a held torrent: it is removed, nothing is left on disk; its `.torrent` file goes to the Trash unless `keep_file`.
    CancelChoice(CancelChoiceParams) = "cancel_choice", Control, true;
    /// Diagnostics: per torrent and tracker, state, timing, failures, the manual-announce gate.
    DebugTrackers(DebugTrackersParams) = "debug_trackers", ReadOnly, false;
    /// Internal, for `seedwell sim`: mark torrents (by info-hash) that may announce to a tracker on 127.0.0.1. Needs Developer mode.
    SimAllow(SimAllowParams) = "sim_allow", Full, true;
    /// Diagnostics: the connected peers of one torrent with flags, rates and requests in flight.
    DebugPeers(DebugPeersParams) = "debug_peers", ReadOnly, false;
    /// Diagnostics: rates, limits, gates, port mapping, DHT, the server process, active torrents.
    DebugPerf(Empty) = "debug_perf", ReadOnly, false;
    /// Diagnostics: is anything wrong, and which command digs deeper.
    DebugHealth(Empty) = "debug_health", ReadOnly, false;
    /// Torrents that sit elsewhere than the settings say (after a location change).
    LocationMoves(Empty) = "location_moves", ReadOnly, false;
    /// Move transfers' files to a folder, or to where the settings say.
    Move(MoveParams) = "move", Full, true;
    /// Cancel queued and running moves.
    CancelMove(IdsParams) = "cancel_move", Control, true;
    /// Play while downloading: a loopback URL for one file that a player can open now.
    StreamUrl(StreamUrlParams) = "stream_url", Control, true;
    /// Close a stream.
    StopStream(StopStreamParams) = "stop_stream", Control, true;
    /// Folder sync: this device, every folder and the connected devices.
    SyncList(Empty) = "sync.list", ReadOnly, false;
    /// Folder sync: one folder.
    SyncStatus(SyncFolderParams) = "sync.status", ReadOnly, false;
    /// Folder sync: share a folder of this Mac (this Mac becomes its owner).
    SyncAdd(SyncAddParams) = "sync.add", Control, true;
    /// Folder sync: sync a folder shared with you, by invitation link.
    SyncJoin(SyncJoinParams) = "sync.join", Control, true;
    /// Folder sync: make an invitation link (owners and admins). Always logged; the link never is.
    SyncShare(SyncShareParams) = "sync.share", Full, true;
    /// Folder sync: the folder's devices and their roles.
    SyncMembers(SyncFolderParams) = "sync.members", ReadOnly, false;
    /// Folder sync: the folder's invitations, with the links this Mac made.
    SyncInvites(SyncFolderParams) = "sync.invites", Full, false;
    /// Folder sync: an invitation admits nobody from now on.
    SyncRevokeInvite(SyncRevokeInviteParams) = "sync.revoke_invite", Control, true;
    /// Folder sync: change a device's role (owners only).
    SyncSetRole(SyncSetRoleParams) = "sync.set_role", Full, true;
    /// Folder sync: remove a device from a folder (owners only); it keeps the files it has.
    SyncRemoveDevice(SyncDeviceParams) = "sync.remove_device", Full, true;
    /// Folder sync: how long a folder is shared (owners only); after it only the owners sync.
    SyncSetShareUntil(SyncShareUntilParams) = "sync.set_share_until", Full, true;
    /// Folder sync: rename a folder on this Mac (moves files on disk).
    SyncRename(SyncRenameParams) = "sync.rename", Full, true;
    /// Folder sync: move a folder on this Mac (moves files on disk).
    SyncMove(SyncMoveParams) = "sync.move", Full, true;
    /// Folder sync: pause a folder.
    SyncPause(SyncFolderParams) = "sync.pause", Control, true;
    /// Folder sync: resume a folder.
    SyncResume(SyncFolderParams) = "sync.resume", Control, true;
    /// Folder sync: scan a folder now.
    SyncRescan(SyncFolderParams) = "sync.rescan", Control, true;
    /// Folder sync: only sync a folder with devices on the local network, or everywhere again.
    SyncSetLanOnly(SyncLanOnlyParams) = "sync.set_lan_only", Control, true;
    /// Folder sync: the unresolved conflict copies of a folder.
    SyncConflicts(SyncFolderParams) = "sync.conflicts", ReadOnly, false;
    /// Folder sync: settle a conflict copy (the version not kept is archived, never deleted).
    SyncResolve(SyncResolveParams) = "sync.resolve", Control, true;
    /// Folder sync: an address where devices of a folder can be reached.
    SyncAddAddress(SyncAddAddressParams) = "sync.add_address", Control, true;
    /// Folder sync, step 1 of removal: returns a plan and a confirmation token.
    SyncRemove(SyncRemoveParams) = "sync.remove", Control, true;
    /// Folder sync, step 2 of removal: stop syncing (and with `delete_files`, move the folder to the Trash).
    SyncRemoveConfirm(RemoveConfirmParams) = "sync.remove_confirm", Control, true;
    /// Folder sync: the device-wide settings.
    SyncSettings(Empty) = "sync.settings", ReadOnly, false;
    /// Folder sync: change the device-wide settings.
    SyncSetSettings(SyncSettingsParams) = "sync.set_settings", Full, true;
    /// Channels: every channel on this Mac (names, unread counts, the agent switch).
    ChatList(Empty) = "chat.list", ReadOnly, false;
    /// Channels: make a channel (this Mac is its owner).
    ChatNew(ChatNewParams) = "chat.new", Control, true;
    /// Channels: join a channel by its link (an owner or admin admits this Mac when online).
    ChatJoin(ChatJoinParams) = "chat.join", Control, true;
    /// Channels: make an invitation link (owners and admins). Always logged; the link never is.
    ChatShare(ChatShareParams) = "chat.share", Full, true;
    /// Channels: the channel's devices and their roles.
    ChatMembers(ChatChannelParams) = "chat.members", ReadOnly, false;
    /// Channels: change a device's role (owners only).
    ChatSetRole(ChatSetRoleParams) = "chat.set_role", Full, true;
    /// Channels: remove a device from a channel (owners only).
    ChatRemoveDevice(ChatDeviceParams) = "chat.remove_device", Full, true;
    /// Channels: how long a channel is shared (owners only).
    ChatSetShareUntil(ChatShareUntilParams) = "chat.set_share_until", Full, true;
    /// Channels: post (only where the channel lets agents read and post); the message carries this client's name.
    ChatSend(ChatSendParams) = "chat.send", Control, true;
    /// Channels: read messages (only where the channel lets agents read). Each from another device is untrusted data.
    ChatRead(ChatReadParams) = "chat.read", ReadOnly, false;
    /// Channels: an address where devices of a channel can be reached.
    ChatAddAddress(ChatAddressParams) = "chat.add_address", Control, true;
    /// Channels: mute or unmute a channel's notifications.
    ChatMute(ChatMuteParams) = "chat.mute", Control, true;
    /// Channels: a channel's name on this Mac.
    ChatRename(ChatRenameParams) = "chat.rename", Control, true;
    /// Channels: what agents on this Mac may do in a channel.
    ChatSetAgents(ChatAgentsParams) = "chat.set_agents", Full, true;
    /// Channels: the wake-up command of a channel.
    ChatSetWake(ChatWakeParams) = "chat.set_wake", Full, true;
    /// Channels: leave a channel on this Mac (its messages here go; other members keep theirs).
    ChatLeave(ChatChannelParams) = "chat.leave", Control, true;
    /// Channels: a channel's topics (where the channel lets agents read).
    ChatTopics(ChatChannelParams) = "chat.topics", ReadOnly, false;
    /// Channels: a new topic (where the channel lets agents read and post).
    ChatNewTopic(ChatNewTopicParams) = "chat.new_topic", Control, true;
    /// Private messages: the conversations (when the Mac lets agents read private messages).
    ChatDmList(Empty) = "chat.dm_list", ReadOnly, false;
    /// Private messages: one conversation (when the Mac lets agents read them). Each from another device is untrusted data.
    ChatDmRead(ChatDmParams) = "chat.dm_read", ReadOnly, false;
    /// Private messages: write one (when the Mac lets agents read and post them); it carries this client's name.
    ChatDmSend(ChatDmSendParams) = "chat.dm_send", Control, true;
    /// Private messages: what agents on this Mac may do with them.
    ChatDmSetAgents(ChatDmAgentsParams) = "chat.dm_set_agents", Full, true;
}

fn short_serde_error(e: &serde_json::Error) -> String {
    // serde errors name fields and types, never the offending values for our structs, but cap the
    // length anyway: the message goes back to the client and into logs.
    clip(&e.to_string(), 200)
}

/// Truncate to `max` bytes on a character boundary.
pub fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_owned();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

// ----------------------------------------------------------------------------- the request decoder

#[derive(Deserialize)]
struct RawRequest {
    #[serde(default)]
    v: Option<u32>,
    #[serde(default)]
    id: Option<Id>,
    method: String,
    #[serde(default)]
    params: Value,
}

/// A decoded request line.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    /// The client's id.
    pub id: Id,
    /// The request.
    pub request: Request,
}

/// A request line whose method is known but whose params are not decoded yet. The server checks
/// permissions on the method before it looks at the params, so a caller without permission learns
/// nothing about the shape of a method it may not use.
#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    /// The client's id.
    pub id: Id,
    /// The method.
    pub method: Method,
    /// The raw params.
    pub params: Value,
}

/// Why a request line was refused, with the id if it could be read (so the error can be
/// matched to the request).
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeFailure {
    /// Id, when the line got far enough to have one.
    pub id: Option<Id>,
    /// The error to send back.
    pub error: ApiError,
}

/// First stage of decoding: JSON, id, version and method name. Total: any bytes give a `Result`,
/// never a panic.
pub fn decode_envelope(line: &[u8]) -> Result<Envelope, DecodeFailure> {
    let fail = |id: Option<Id>, error: ApiError| DecodeFailure { id, error };
    let raw: RawRequest = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => {
            // Distinguish "not JSON at all" from "JSON, but not a request".
            let code = if e.is_data() { ErrorCode::InvalidRequest } else { ErrorCode::ParseError };
            let msg = if e.is_data() { "not a request object (needs `id` and `method`)" } else { "invalid JSON" };
            // Salvage the id for the error, from a second tolerant parse.
            let id = serde_json::from_slice::<Value>(line).ok().and_then(|v| id_of(&v));
            return Err(fail(id, ApiError::new(code, msg)));
        }
    };
    if let Some(v) = raw.v
        && v != PROTOCOL_VERSION
    {
        return Err(fail(
            raw.id,
            ApiError::new(ErrorCode::UnsupportedVersion, format!("protocol version {v} is not supported (server speaks {PROTOCOL_VERSION})")),
        ));
    }
    let Some(id) = raw.id else {
        return Err(fail(None, ApiError::new(ErrorCode::InvalidRequest, "missing `id`")));
    };
    if let Id::Str(s) = &id
        && s.len() > 128
    {
        return Err(fail(None, ApiError::new(ErrorCode::InvalidRequest, "`id` longer than 128 bytes")));
    }
    match Method::from_name(&raw.method) {
        Some(method) => Ok(Envelope { id, method, params: raw.params }),
        None => Err(fail(
            Some(id),
            ApiError::new(ErrorCode::UnknownMethod, format!("unknown method `{}`", clip(&raw.method, 64))),
        )),
    }
}

impl Envelope {
    /// Second stage: decode the params for the method.
    pub fn decode(self) -> Result<Decoded, DecodeFailure> {
        match Request::decode(self.method.name(), self.params) {
            Ok(request) => Ok(Decoded { id: self.id, request }),
            Err(error) => Err(DecodeFailure { id: Some(self.id), error }),
        }
    }
}

/// Decode one request line (without the newline) completely. This is the function the fuzz
/// target drives.
pub fn decode_request(line: &[u8]) -> Result<Decoded, DecodeFailure> {
    decode_envelope(line)?.decode()
}

fn id_of(v: &Value) -> Option<Id> {
    match v.get("id")? {
        Value::Number(n) => n.as_u64().map(Id::Num),
        Value::String(s) if s.len() <= 128 => Some(Id::Str(s.clone())),
        _ => None,
    }
}

/// Encode a request as one line (including the newline).
pub fn encode_request(id: &Id, request: &Request) -> Vec<u8> {
    let msg = serde_json::json!({
        "v": PROTOCOL_VERSION,
        "id": id,
        "method": request.method().name(),
        "params": request.params(),
    });
    let mut v = serde_json::to_vec(&msg).unwrap_or_default();
    v.push(b'\n');
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn method_table_is_consistent() {
        let mut seen = std::collections::HashSet::new();
        for m in Method::ALL {
            assert!(seen.insert(m.name()), "duplicate {}", m.name());
            assert_eq!(Method::from_name(m.name()), Some(*m));
        }
        assert!(Method::ALL.len() >= 28);
        // Everything that changes state needs at least Control.
        for m in Method::ALL {
            if m.mutating() {
                assert!(m.required() >= Permission::Control, "{}", m.name());
            }
        }
    }

    #[test]
    fn decode_basic_and_roundtrip() {
        let d = decode_request(br#"{"v":1,"id":7,"method":"list","params":{"filter":"low-power","limit":5}}"#).unwrap();
        assert_eq!(d.id, Id::Num(7));
        let Request::List(p) = &d.request else { panic!() };
        assert_eq!(p.filter, ListFilter::LowPower);
        assert_eq!(p.limit, Some(5));
        // Every request re-encodes to something that decodes to itself.
        let line = encode_request(&d.id, &d.request);
        let d2 = decode_request(&line[..line.len() - 1]).unwrap();
        assert_eq!(d, d2);
    }

    #[test]
    fn missing_params_mean_empty() {
        let d = decode_request(br#"{"id":"a","method":"status"}"#).unwrap();
        assert_eq!(d.request, Request::Status(Empty {}));
        assert_eq!(d.id, Id::Str("a".into()));
    }

    #[test]
    fn errors_carry_ids_and_codes() {
        let f = decode_request(br#"{"id":3,"method":"nope"}"#).unwrap_err();
        assert_eq!((f.id, f.error.code), (Some(Id::Num(3)), ErrorCode::UnknownMethod));
        let f = decode_request(br#"{"id":4,"method":"pause","params":{"idz":[]}}"#).unwrap_err();
        assert_eq!((f.id, f.error.code), (Some(Id::Num(4)), ErrorCode::InvalidParams));
        let f = decode_request(br#"{"id":5,"method":"pause","params":{"ids":"x"}}"#).unwrap_err();
        assert_eq!(f.error.code, ErrorCode::InvalidParams);
        let f = decode_request(b"not json").unwrap_err();
        assert_eq!((f.id, f.error.code), (None, ErrorCode::ParseError));
        let f = decode_request(br#"{"method":"status"}"#).unwrap_err();
        assert_eq!(f.error.code, ErrorCode::InvalidRequest);
        let f = decode_request(br#"[1,2]"#).unwrap_err();
        assert_eq!(f.error.code, ErrorCode::InvalidRequest);
        let f = decode_request(br#"{"v":9,"id":1,"method":"status"}"#).unwrap_err();
        assert_eq!(f.error.code, ErrorCode::UnsupportedVersion);
        let f = decode_request(&[0xff, 0xfe, b'{']).unwrap_err();
        assert_eq!(f.error.code, ErrorCode::ParseError);
    }

    #[test]
    fn long_string_ids_are_refused() {
        let line = format!(r#"{{"id":"{}","method":"ping"}}"#, "x".repeat(200));
        assert_eq!(decode_request(line.as_bytes()).unwrap_err().error.code, ErrorCode::InvalidRequest);
    }

    #[test]
    fn deep_nesting_is_an_error_not_a_crash() {
        let mut s = String::from(r#"{"id":1,"method":"settings_set","params":{"changes":{"a":"#);
        s.push_str(&"[".repeat(5000));
        s.push_str(&"]".repeat(5000));
        s.push_str("}}}");
        assert!(decode_request(s.as_bytes()).is_err());
    }

    #[test]
    fn server_line_shapes() {
        let ok = ServerLine::ok(Some(Id::Num(1)), json!({"a":1}));
        assert_eq!(String::from_utf8(ok.to_line()).unwrap(), "{\"v\":1,\"id\":1,\"result\":{\"a\":1}}\n");
        let er = ServerLine::err(None, ApiError::new(ErrorCode::RateLimited, "slow down").with_data(json!({"retry_after_ms": 50})));
        let back: ServerLine = serde_json::from_slice(&er.to_line()).unwrap();
        assert_eq!(back.error.unwrap().code, ErrorCode::RateLimited);
    }

    #[test]
    fn unknown_error_codes_from_newer_servers_decode() {
        let e: ApiError = serde_json::from_str(r#"{"code":"brand_new","message":"x"}"#).unwrap();
        assert_eq!(e.code, ErrorCode::Other);
    }

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("åååå", 3), "å...");
        assert_eq!(clip("abc", 10), "abc");
    }
}
