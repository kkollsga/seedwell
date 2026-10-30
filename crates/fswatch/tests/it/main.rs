//! All of this crate's integration tests in one binary. Every test uses its own temporary folder.

mod common;
#[cfg(target_os = "macos")]
mod fsevents;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
mod native;
#[cfg(windows)]
mod rdcw;
mod poll;
