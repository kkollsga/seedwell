//! trs-dht: Mainline DHT (BEP 5) with BEP 42 secure ids and BEP 43 read-only mode.
//!
//! * [`DhtCore`] is a sans-I/O state machine: `handle(input, now) -> Vec<Intent>`.
//! * [`spawn`] wraps it in a tokio UDP driver with a small async handle ([`DhtHandle`]).
//!
//! See the README for design, budgets and the private-torrent guarantee.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod codec;
mod core;
mod driver;
pub mod id;
pub mod limits;
pub mod lookup;
pub mod rng;
pub mod store;
pub mod table;
pub mod token;

pub use crate::core::{
    Config, DhtCore, Event, Input, Intent, LookupStats, MAX_SAVED_NODES, PublicInfoHash, Refused,
    StateError, Stats,
};
pub use crate::driver::{
    Alive, DEFAULT_BOOTSTRAP, DhtHandle, DriverConfig, Info, LookupUpdate, PeerStream, SharedIo, SharedSend, spawn,
    spawn_shared,
};
pub use crate::id::Id;
