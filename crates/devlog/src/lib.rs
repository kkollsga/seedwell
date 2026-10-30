//! Developer-mode log of tracker communication (SPEC 8.7).
//!
//! * [`Entry`] is plain data filled by the engine. [`DevLog::log`] is the **logging boundary**: it
//!   masks every secret (`trs_state::url::mask_secrets_for_log`, `mask_text`), formats one JSON line
//!   and hands the finished string to a bounded channel. Nothing unmasked ever reaches the writer.
//! * Off, the cost is one relaxed atomic load ([`DevLog::enabled`]); callers build entries only
//!   when it is true. No thread exists until the first time the log is switched on.
//! * The writer runs on its own thread. The channel is bounded and **never blocks the sender**: a
//!   full channel drops the line and counts it ([`DevLog::dropped`]).
//! * `trackers.log` rotates at 10 MB and keeps 3 files (`trackers.log`, `.1`, `.2`).
//! * [`read`] tails the files for the CLI and the app viewer; [`view::Row`] renders a
//!   line for people.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod entry;
mod writer;
pub mod read;
pub mod view;

pub use entry::{Entry, Kind, Next, Request, Response, iso_time};
pub use writer::{DevLog, FileSink, LineSink, Limits};

/// File name of the tracker log inside the log folder.
pub const FILE_NAME: &str = "trackers.log";

#[cfg(test)]
mod tests;
