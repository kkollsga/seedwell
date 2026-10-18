//! trs-engine: the BitTorrent session engine of Seedwell.
//!
//! One [`Engine`] runs a session actor, a tracker driver, an optional DHT node and one actor per
//! awake (Warm or Hot) torrent. Cold torrents are plain records. The app talks to it through the
//! cloneable [`EngineHandle`] and persists through the [`persist::Store`] trait.
//!
//! See `docs/ENGINE.md` for the design and this crate's `README.md` for what deviates from it.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod api;
pub mod budget;
pub mod choker;
pub mod config;
pub mod config_map;
pub mod counters;
pub mod persist;
mod probe;
pub mod picker;
pub mod policy;
pub mod queue;
pub mod ratelimit;
pub mod tier;
pub mod util;

mod chan;
mod choice;
mod edgefix;
mod host;
pub mod idle;
mod engine;
mod guard;
mod moveplan;
mod netstate;
mod mse;
mod net;
mod organize;
mod peer;
mod portmap;
mod session;
mod shared;
mod state_store;
mod status;
mod stream_source;
mod torrent;
mod tracker_driver;
mod tracker_log;
mod watch;

pub use engine::{BlockingHandle, Engine, EngineHandle, build_runtime};
pub use persist::{MemStore, Store};
pub use probe::EngineProbe;
pub use session::moves::{MOVE_STEP_BEFORE_COMMIT, MOVE_STEP_BEFORE_MARK};
pub use policy::PolicyData;
pub use host::{ConfigTweak, Host, HostOptions};
pub use state_store::StateStore;

pub use api::*;
pub use config::{EngineConfig, TierTimings};
pub use tier::Tier;
/// Same-volume check and the Settings warning for incomplete -> finished pairs on different
/// disks (SPEC 7A.4); see `trs_state::volume`.
pub use trs_state::volume::{CrossVolume, LocationScope, cross_volume_pairs, same_volume, volume_id};
