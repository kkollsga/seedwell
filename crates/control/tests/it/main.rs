//! All of this crate's integration tests in one binary (one link, one launch): each former
//! `tests/<name>.rs` is a module here. Tests that measure the process (memory) keep their own binary.

#[allow(unused_imports)]
#[path = "../common/mod.rs"]
mod common;

mod access;
#[cfg(feature = "sync")]
mod access_chat;
mod decoder;
#[cfg(feature = "sync")]
mod proxy_sync;
mod server;
#[cfg(feature = "sync")]
mod sync;
