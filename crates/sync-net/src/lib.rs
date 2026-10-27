//! trs-sync-net: the folder-sync driver (SPEC §7G).
//!
//! * [`service`]: [`service::SyncHandle`], the service and its API (folders, keys, settings, snapshot).
//! * [`peer`] and [`session`]: one connection: the magic, TLS ([`tls`]), Hello, the folder proofs, then
//!   membership, index updates and chunk requests ([`framed`] carries the messages).
//! * [`roles`]: a folder's or channel's membership on this device: kept, proved, applied when it changes.
//! * [`chat`]: channels: their messages, kept and passed on, and the wake-up command for agents.
//! * [`runtime`]: one folder's loop (watcher → scan → announce; peers' changes → pull through [`source`]).
//! * [`lan`]: finding devices on the local network.
//! * [`relay`], [`punch`], [`relay_server`]: reaching devices behind NATs: member relays, hole punching
//!   through a common peer, and the stand-alone `seedwell relay` server ([`relay_server::run_relay`]).
//! * [`config`], [`secrets`]: `sync.toml`, and where keys are kept; `at_rest`: messages encrypted on disk.
//! * [`status`]: what the UI sees.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod at_rest;
pub mod chat;
pub mod config;
pub mod describe;
pub mod direct;
pub mod framed;
pub mod lan;
pub mod limit;
pub mod peer;
pub mod punch;
pub mod relay;
pub mod relay_server;
pub mod roles;
pub mod runtime;
pub mod secrets;
pub mod service;
pub mod session;
pub mod shared;
pub mod source;
pub mod status;
pub mod tls;

pub use relay_server::{RelayOptions, run_relay};
pub use service::{Activity, InviteInfo, Keep, MemberInfo, Options, PublicDiscovery, SyncError, SyncHandle};
