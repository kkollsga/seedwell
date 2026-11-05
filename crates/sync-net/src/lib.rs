//! trs-sync-net: the folder-sync driver (SPEC §7G).
//!
//! * [`service`]: [`service::SyncHandle`], the service and its API (folders, keys, settings, snapshot).
//! * [`peer`] and [`session`]: one connection: the magic, TLS ([`tls`]), Hello, the folder proofs, then
//!   membership, index updates and chunk requests ([`framed`] carries the messages).
//! * [`roles`]: a folder's or channel's membership on this device: kept, proved, applied when it changes.
//! * [`chat`]: channels: their messages, kept and passed on, and the wake-up command for agents.
//! * [`room`]: where attachments belong (a channel, a private conversation); [`attachments`]: sending them and what
//!   they are for callers.
//! * [`blobs`]: the images chat messages name: sealed at rest, fetched from members by hash.
//! * [`files`]: the files chat messages name: served in place by their sender, downloaded on request piece by piece.
//! * [`devices`]: the user's own devices: pairing, "on all my devices", and admitting each other.
//! * [`runtime`]: one folder's loop (watcher → scan → announce; peers' changes → pull through [`source`]).
//! * [`lan`]: finding devices on the local network; `joining`: what a joining folder or channel says it does.
//! * [`relay`], [`punch`], [`relay_server`]: reaching devices behind NATs: member relays, hole punching
//!   through a common peer, and the stand-alone `seedwell relay` server ([`relay_server::run_relay`]).
//! * [`netchange`]: network changes (the system's signal, or a check of its own) find the devices again at once.
//! * [`rendezvous`]: the user's own address book on Cloudflare: this device's sealed record, and lookups.
//! * [`config`], [`secrets`]: `sync.toml`, and where keys are kept; `at_rest`: messages encrypted on disk.
//! * [`placed`]: on-demand content kept in the host's placeholders (Windows' Cloud Files), stored once.
//! * [`status`]: what the UI sees.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod absence;
mod at_rest;
pub mod attachments;
pub mod blobs;
pub mod files;
pub mod room;
pub mod chat;
pub mod config;
pub mod devices;
pub mod describe;
pub mod direct;
pub mod framed;
mod joining;
mod journal;
pub mod lan;
pub mod limit;
pub mod netchange;
pub mod peer;
pub mod placed;
pub mod punch;
pub mod relay;
pub mod relay_server;
pub mod rendezvous;
pub mod roles;
pub mod runtime;
pub mod secrets;
pub mod service;
pub mod session;
pub mod shared;
pub mod source;
pub mod status;
pub mod tls;
#[cfg(test)]
mod dial_tests;

pub use relay_server::{RelayOptions, run_relay};
pub use rendezvous::RendezvousStatus;
pub use service::{Activity, DeleteSeen, FULL_HERE, InviteInfo, Keep, LocalChange, LocalTorrents, MemberInfo, OnDemandError, OnDemandHost, OnDemandHostCell, Options, PublicDiscovery, SyncError, SyncHandle};
