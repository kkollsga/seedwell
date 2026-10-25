//! trs-sync-net: the folder-sync driver (SPEC §7G).
//!
//! * [`service`]: [`service::SyncHandle`], the service and its API (folders, keys, settings, snapshot).
//! * [`peer`] and [`session`]: one connection: the magic, TLS ([`tls`]), Hello, the folder proofs, then
//!   index updates and chunk requests ([`framed`] carries the messages).
//! * [`runtime`]: one folder's loop (watcher → scan → announce; peers' changes → pull through [`source`]).
//! * [`lan`]: finding devices on the local network.
//! * [`relay`], [`punch`], [`relay_server`]: reaching devices behind NATs: member relays, hole punching
//!   through a common peer, and the stand-alone `seedwell relay` server ([`relay_server::run_relay`]).
//! * [`config`], [`secrets`]: `sync.toml`, and where keys are kept.
//! * [`status`]: what the UI sees.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod config;
pub mod describe;
pub mod framed;
pub mod lan;
pub mod limit;
pub mod peer;
pub mod punch;
pub mod relay;
pub mod relay_server;
pub mod runtime;
pub mod secrets;
pub mod service;
pub mod session;
pub mod shared;
pub mod source;
pub mod status;
pub mod tls;

pub use relay_server::{RelayOptions, run_relay};
pub use service::{Activity, Keep, Options, PublicDiscovery, Shares, SyncError, SyncHandle};
