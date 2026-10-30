//! Direct HTTP(S) downloads for Seedwell (SPEC 7D).
//!
//! Engine-agnostic: plain structs in, a [`RateGate`] injected so the global
//! download limit governs these bytes together with torrent traffic, progress
//! out through a `watch` channel plus a bounded event channel. Nothing here is
//! ever seeded or shown in Sharing.
//!
//! See the README for the resume/validation rules.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod core;
mod error;
mod gate;
pub mod name;
mod net;
mod ranges;
mod session;
mod sidecar;
mod small;
mod task;
mod types;
mod util;

pub use error::Error;
pub use gate::{BoxFuture, RateGate, TokenBucket, Unlimited};
pub use ranges::RangeSet;
pub use sidecar::Sidecar;
pub use small::{SmallResponse, get_small};
pub use task::{DownloadTask, Downloader, discard_staged, list_staged};
pub use types::{
    DownloadRequest, DownloaderConfig, Event, ProbeInfo, Progress, RestartReason, StagedInfo, State,
};

/// Per-connection write buffer in bytes.
pub const BUFFER_BYTES: usize = session::BUFFER_BYTES;
