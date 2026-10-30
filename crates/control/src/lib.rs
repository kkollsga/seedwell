//! The Seedwell agent interface (SPEC 9A): a versioned JSON-lines control protocol, the local
//! server behind `control.sock`, and a small async client used by the `seedwell` CLI.
//!
//! * [`proto`]: wire format, every method with its params and required permission.
//! * [`types`]: result types. [`ops`], [`settings`], [`redact`]: pure logic.
//! * [`server`]: [`serve_backend`] over any [`Backend`]; with feature `engine`, [`serve`] and
//!   [`start_for_app`] wire it to `trs_engine::EngineHandle`.
//! * `sync` (feature `sync`): the folder sync service next to the engine (`start_sync`, the DHT
//!   discovery it borrows, `SyncSlot`) and what the `sync.*` methods need.
//! * [`client`]: connect, handshake, request, events.
//!
//! Security model: Unix domain socket only (never TCP), mode 0600 in a 0700 directory, same-uid
//! peers only, permission level re-read on every call, rate limited, size capped, every mutating
//! call logged, passkeys and URL queries redacted on the way out. See `README.md`.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod activity;
pub mod awake;
pub mod b64;
pub mod backend;
pub mod builds;
pub mod client;
pub mod confirm;
pub mod debug;
pub mod display;
pub mod human;
pub mod limits;
pub mod mail;
pub mod ops;
pub mod perm;
pub mod proto;
pub mod redact;
pub mod server;
pub mod settings;
pub mod socket;
pub mod state;
pub mod transport;
pub mod types;

#[cfg(feature = "engine")]
mod engine_backend;
#[cfg(feature = "sync")]
pub mod sync;

pub use activity::ActivityLog;
pub use awake::Awake;
pub use backend::{AddArgs, AddSource, Backend};
pub use builds::{Builds, BuildsSlot};
pub use client::{Client, ClientError};
pub use perm::{Permission, PermissionHandle, PermissionSource};
pub use proto::{ApiError, ErrorCode, Method, Request, PROTOCOL_VERSION};
pub use server::{ServerHandle, ServerOptions, serve_backend};
pub use socket::default_socket_path;
pub use state::StateAccess;

#[cfg(feature = "engine")]
pub use engine_backend::{AppServerConfig, EngineBackend, reachability_dto, serve, serve_with, start_for_app, sync_reachability_dto};
#[cfg(feature = "sync")]
pub use engine_backend::start_for_app_with_sync;
