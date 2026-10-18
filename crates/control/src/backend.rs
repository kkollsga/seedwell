//! What the server needs from the engine. The real implementation maps `EngineHandle` (feature
//! `engine`); tests use an in-memory one. Everything protocol-shaped (ids, filters, permissions,
//! the two-step remove, redaction) happens above this trait, so an adapter stays a thin mapping.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::sync::broadcast;

use crate::proto::{ApiError, FilePriorityParam, PriorityParam, SetLimitsParams};
use crate::types::*;

/// Where an add comes from, after the server has read files and decoded base64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddSource {
    /// A `magnet:` URI.
    Magnet(String),
    /// The bytes of a `.torrent`.
    TorrentFile(Vec<u8>),
    /// An http(s) URL to download directly.
    Url(String),
}

/// A validated add.
#[derive(Clone, Debug)]
pub struct AddArgs {
    /// Source.
    pub source: AddSource,
    /// Absolute folder, `~` already expanded.
    pub save_path: Option<PathBuf>,
    /// Start paused.
    pub paused: bool,
    /// Category name (validated against the known set).
    pub category: Option<String>,
    /// Priority.
    pub priority: Option<PriorityParam>,
    /// Forced rule id.
    pub rule: Option<String>,
    /// Queue top.
    pub queue_top: bool,
    /// Expected SHA-256 of a direct download.
    pub sha256: Option<[u8; 32]>,
    /// Peers to try.
    pub peers: Vec<SocketAddr>,
    /// Skip the grace period before the torrent starts (`now`).
    pub start_now: bool,
}

/// `move_to` answer: the transfers that were queued and those that were refused, with the reason.
pub type MoveOutcome = (Vec<TransferId>, Vec<(TransferId, String)>);

/// The engine as the control server sees it. Futures must be `Send` (they run on the runtime).
pub trait Backend: Send + Sync + 'static {
    /// Every transfer plus totals; cheap.
    fn snapshot(&self) -> impl Future<Output = Result<SnapshotDto, ApiError>> + Send;
    /// Limits in effect and the alternative values.
    fn limits(&self) -> impl Future<Output = Result<LimitsDto, ApiError>> + Send;
    /// Files, trackers and peers of one transfer (untruncated; the server caps and redacts).
    fn detail(&self, id: TransferId) -> impl Future<Output = Result<DetailDto, ApiError>> + Send;
    /// Add a transfer.
    fn add(&self, args: AddArgs) -> impl Future<Output = Result<AddedDto, ApiError>> + Send;
    /// Pause.
    fn pause(&self, ids: Vec<TransferId>) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Resume.
    fn resume(&self, ids: Vec<TransferId>) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Pause everything.
    fn pause_all(&self) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Resume everything.
    fn resume_all(&self) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Front of the queue.
    fn move_to_top(&self, ids: Vec<TransferId>) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Priority.
    fn set_priority(&self, ids: Vec<TransferId>, p: PriorityParam) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Per-file priorities, one per file in order.
    fn set_file_priorities(
        &self,
        id: TransferId,
        p: Vec<FilePriorityParam>,
    ) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Category (`None` = detect again).
    fn set_category(&self, ids: Vec<TransferId>, c: Option<String>) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Put the seeding goal back to what the transfers' rules say.
    fn clear_seed_goal(&self, ids: Vec<TransferId>) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Seeding goal.
    fn set_seed_goal(&self, ids: Vec<TransferId>, g: SeedGoalDto) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Apply a limits patch (`0` = unlimited) and return the result.
    fn set_limits(&self, patch: SetLimitsParams) -> impl Future<Output = Result<LimitsDto, ApiError>> + Send;
    /// Ask trackers for peers now; the engine's tracker gate enforces the minimum interval and the
    /// answer says when the next announce may happen.
    fn reannounce(&self, ids: Vec<TransferId>) -> impl Future<Output = Result<Vec<ReannounceDto>, ApiError>> + Send;
    /// Verify files again.
    fn recheck(&self, ids: Vec<TransferId>) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// Remove transfers, optionally deleting their files.
    fn remove(&self, ids: Vec<TransferId>, delete_files: bool) -> impl Future<Output = Result<(), ApiError>> + Send;
    /// The chooser of a torrent (structure, preselection, reasons).
    fn choice(&self, _id: TransferId) -> impl Future<Output = Result<ChoiceDto, ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend has no chooser")) }
    }
    /// Answer a chooser: `None` = the engine's preselection, `Some(per-file priorities)` otherwise.
    fn choose_files(
        &self,
        _id: TransferId,
        _files: Option<Vec<FilePriorityParam>>,
    ) -> impl Future<Output = Result<(), ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend has no chooser")) }
    }
    /// Cancel a held torrent.
    fn cancel_choice(&self, _id: TransferId) -> impl Future<Output = Result<(), ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend has no chooser")) }
    }
    /// Cancel a held torrent for `seedwell cancel`: one in state `awaiting-choice` is dropped, one in
    /// `pending` is removed. Unless `keep_file`, the `.torrent` file it came from goes to the Trash.
    /// Returns the name of the trashed file.
    fn cancel_held(
        &self,
        id: TransferId,
        pending: bool,
        keep_file: bool,
    ) -> impl Future<Output = Result<Option<String>, ApiError>> + Send {
        let _ = (pending, keep_file);
        async move { self.cancel_choice(id).await.map(|()| None) }
    }
    /// Start a torrent that waits out its grace period now (`seedwell start` on a `pending` torrent).
    fn start_now(&self, _id: TransferId) -> impl Future<Output = Result<(), ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend has no grace period")) }
    }
    /// Allow or stop allowing simulated torrents (by info-hash) to use a loopback tracker.
    fn sim_allow(&self, _ids: Vec<TransferId>, _allow: bool) -> impl Future<Output = Result<(), ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend has no simulated torrents")) }
    }
    /// Gates, budgets, port mapping and counters, read on demand (`seedwell debug`).
    fn probe(&self) -> impl Future<Output = Result<ProbeDto, ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend has no diagnostics")) }
    }
    /// Torrents that sit elsewhere than the settings say.
    fn location_moves(&self) -> impl Future<Output = Result<MoveProposalDto, ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend cannot move files")) }
    }
    /// Move to `path` (pinning them there), or to where the settings say when `None`.
    fn move_to(
        &self,
        _ids: Vec<TransferId>,
        _path: Option<PathBuf>,
    ) -> impl Future<Output = Result<MoveOutcome, ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend cannot move files")) }
    }
    /// Cancel queued and running moves.
    fn cancel_moves(&self, _ids: Vec<TransferId>) -> impl Future<Output = Result<(), ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend cannot move files")) }
    }
    /// Serve `file` (default: the largest video or audio file) of a transfer over loopback HTTP.
    fn stream_url(
        &self,
        _id: TransferId,
        _file: Option<u32>,
    ) -> impl Future<Output = Result<StreamInfoDto, ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend cannot stream")) }
    }
    /// Close a stream; whether it existed.
    fn stop_stream(&self, _token: &str) -> impl Future<Output = Result<bool, ApiError>> + Send {
        async { Err(ApiError::new(crate::proto::ErrorCode::Unsupported, "this backend cannot stream")) }
    }
    /// Engine events, already mapped (the server assigns `seq` and `at`).
    fn subscribe(&self) -> broadcast::Receiver<EventDto>;
}
