//! trs-tracker: BitTorrent tracker communication that is correct by
//! construction (SPEC 7B/7C). See README.md for the architecture, the invariant
//! to test mapping and how an engine drives it.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod audit;
pub mod backoff;
pub mod crash;
#[doc(hidden)]
pub mod fastmap;
pub mod gate;
#[cfg(feature = "transport")]
pub mod http;
pub mod hub;
pub mod machine;
pub mod outage;
pub mod policy;
pub mod response;
pub mod retry_after;
pub mod rng;
pub mod scrape;
pub mod set;
pub mod store;
pub mod time;
pub mod types;
#[cfg(feature = "transport")]
pub mod udp;
pub mod udp_proto;
pub mod url;

pub use machine::{
    AnnounceMachine, Health, Input, Intent, MachineConfig, Report, SendAnnounce, SendKind,
};
pub use response::{
    AnnounceError, AnnounceResponse, RetryIn, Timing, TransportError, clamp_timing,
    parse_announce_response,
};
pub use store::{MemoryStore, TimingKey, TimingRecord, TimingStore};
pub use time::Millis;
pub use types::{
    AnnounceParams, Crypto, Event, InfoHash20, Peer, PeerId20, RequestId, Stats, TorrentEvent, TorrentRole,
};
pub use url::{TrackerUrl, build_announce_url};
