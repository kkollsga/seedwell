//! Public plain-data types: configuration, requests, progress, events.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::Error;

/// Settings shared by every download of one [`Downloader`](crate::Downloader).
#[derive(Clone, Debug)]
pub struct DownloaderConfig {
    /// Application version; the User-Agent is exactly `Seedwell/<version>`.
    pub version: String,
    /// Settings "Connections per host": a hard cap across **all** tasks.
    pub max_connections_per_host: usize,
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Idle timeout: no response header / body frame for this long.
    pub read_timeout: Duration,
    /// Redirects followed per request.
    pub max_redirects: u32,
    /// Consecutive failures (without progress) before giving up.
    pub max_retries: u32,
    /// First backoff step; doubles per failure, with jitter.
    pub backoff_base: Duration,
    /// Backoff ceiling.
    pub backoff_max: Duration,
    /// Longest `Retry-After` we are willing to sit out.
    pub max_retry_after: Duration,
    /// How often progress is made durable (fsync data, then rewrite the
    /// sidecar). A crash loses at most this much work.
    pub checkpoint_interval: Duration,
}

impl Default for DownloaderConfig {
    fn default() -> Self {
        DownloaderConfig {
            version: env!("CARGO_PKG_VERSION").to_owned(),
            max_connections_per_host: 4,
            connect_timeout: Duration::from_secs(15),
            read_timeout: Duration::from_secs(60),
            max_redirects: 10,
            max_retries: 6,
            backoff_base: Duration::from_secs(1),
            backoff_max: Duration::from_secs(60),
            max_retry_after: Duration::from_secs(600),
            checkpoint_interval: Duration::from_secs(2),
        }
    }
}

/// One direct download.
#[derive(Clone, Debug)]
pub struct DownloadRequest {
    /// Stable identity (the app's UUID). Staged state is found by this id, so
    /// a new process resumes the same download by passing the same id.
    pub id: String,
    /// Public `http`/`https` URL.
    pub url: String,
    /// Where the finished file goes.
    pub dest_dir: PathBuf,
    /// Where `<name>.part` and its sidecar live (`.incomplete`). Put it on the
    /// same volume as `dest_dir` so the final move is an atomic rename.
    pub incomplete_dir: PathBuf,
    /// Override the derived file name.
    pub file_name: Option<String>,
    /// Parallel ranges for this file (clamped to the per-host cap). Only used
    /// when the server supports ranges, the length is known and a validator
    /// exists; otherwise 1.
    pub connections: u32,
    /// Verify the finished file against this SHA-256 before moving it.
    pub sha256: Option<[u8; 32]>,
    /// Per-download limit in bytes/s (`None` = only the global gate applies).
    pub rate_limit: Option<u64>,
    /// Create the task paused (the engine's queue starts it later).
    pub start_paused: bool,
}

impl DownloadRequest {
    /// A request with a fresh random id and single-connection defaults.
    pub fn new(url: impl Into<String>, dest_dir: impl Into<PathBuf>, incomplete_dir: impl Into<PathBuf>) -> Self {
        DownloadRequest {
            id: crate::util::random_id(),
            url: url.into(),
            dest_dir: dest_dir.into(),
            incomplete_dir: incomplete_dir.into(),
            file_name: None,
            connections: 1,
            sha256: None,
            rate_limit: None,
            start_paused: false,
        }
    }
}

/// Lifecycle of a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Waiting for the first response.
    Connecting,
    /// Bytes are flowing.
    Downloading,
    /// Sitting out a backoff / `Retry-After` before trying again.
    Retrying,
    /// Paused by the caller (progress is on disk).
    Paused,
    /// Hashing the finished file.
    Verifying,
    /// File is in its destination. Terminal.
    Completed,
    /// Stopped by an error; `resume()` tries again. See `Progress::error`.
    Failed,
    /// Cancelled. Terminal.
    Cancelled,
}

/// Why the download started over instead of continuing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RestartReason {
    /// `ETag` / `Last-Modified` no longer matches: the file changed on the server.
    ValidatorChanged,
    /// The server answered a range request with a plain 200.
    RangeIgnored,
    /// The total size differs from what we staged.
    SizeChanged,
    /// The server gave no usable validator, so the continuation cannot be verified.
    NoValidator,
    /// The staged `.part` does not match its sidecar.
    PartMismatch,
}

/// Snapshot published through a `watch` channel (cheap to clone).
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// Current state.
    pub state: State,
    /// Total size, if known.
    pub total: Option<u64>,
    /// Bytes accepted so far.
    pub done: u64,
    /// Smoothed bytes per second (about the last 2 s).
    pub rate: u64,
    /// Open connections right now.
    pub connections: u32,
    /// Last error while `Failed` (or the one being retried).
    pub error: Option<Error>,
    /// Resolved file name once known.
    pub file_name: Option<String>,
    /// URL after redirects.
    pub final_url: Option<String>,
    /// `Content-Type` of the answer, so the app can divert `.torrent` payloads.
    pub content_type: Option<String>,
    /// Times the download restarted from zero.
    pub restarts: u32,
    /// Retry attempts so far in this run.
    pub retries: u32,
    /// Where the finished file landed (`Completed`).
    pub final_path: Option<PathBuf>,
}

impl Progress {
    pub(crate) fn new(state: State) -> Self {
        Progress {
            state,
            total: None,
            done: 0,
            rate: 0,
            connections: 0,
            error: None,
            file_name: None,
            final_url: None,
            content_type: None,
            restarts: 0,
            retries: 0,
            final_path: None,
        }
    }
}

/// Discrete things worth telling the user about (delivered on a bounded
/// channel; if nobody reads it, the oldest news is simply dropped).
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    /// State changed.
    StateChanged(State),
    /// The server answered: name, size and type are known.
    Resolved {
        /// File name (sanitized).
        file_name: String,
        /// Total bytes, if known.
        total: Option<u64>,
        /// `Content-Type`.
        content_type: Option<String>,
        /// URL after redirects.
        final_url: String,
    },
    /// Started over from zero, with the reason ("the file changed on the server").
    Restarted(RestartReason),
    /// A retry is scheduled.
    Retrying {
        /// Which consecutive failure this is.
        attempt: u32,
        /// How long we wait.
        delay: Duration,
        /// What went wrong.
        error: Error,
    },
    /// The file is in place.
    Completed(PathBuf),
    /// The task stopped with an error.
    Failed(Error),
}

/// What a cheap `Range: bytes=0-0` request learned about a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeInfo {
    /// URL after redirects.
    pub final_url: String,
    /// File name the download would get.
    pub file_name: String,
    /// `Content-Type`.
    pub content_type: Option<String>,
    /// Total size if known.
    pub total: Option<u64>,
    /// The server honours byte ranges.
    pub accept_ranges: bool,
    /// Strong `ETag`, if any.
    pub etag: Option<String>,
    /// `Last-Modified`, if any.
    pub last_modified: Option<String>,
}

/// A staged (resumable) download found on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedInfo {
    /// The id the download was created with.
    pub id: String,
    /// Original URL.
    pub url: String,
    /// Staged file name.
    pub name: String,
    /// Total size if known.
    pub total: Option<u64>,
    /// Bytes safely on disk.
    pub done: u64,
    /// The `.part` file.
    pub part_path: PathBuf,
}
