//! Seedwell's streaming server (SPEC 7F): play a downloading file in the user's own player.
//!
//! A loopback HTTP/1.1 server answers `GET`/`HEAD` with `Range` for `http://127.0.0.1:<port>/<token>/<name>`.
//! Bytes come from a [`PieceSource`] (implemented by the engine) and only ever from verified
//! pieces. Every response is a read cursor; the cursors of a stream are merged into one urgency
//! picture ([`urgency`]) that is pushed to the source. The head, tail and container index
//! (`moov`, `Cues`) are fetched first ([`container`]).
//!
//! The crate does not depend on the engine. See `README.md` and `ENGINE_INTEGRATION.md`.

mod conn;
pub mod container;
pub mod http;
mod index;
pub mod mime;
mod server;
pub mod source;
mod stream;
pub mod urgency;

pub use mime::Container;
pub use server::{Config, EndReason, Event, EventSink, ServerStats, StreamHandle, StreamServer, peer_allowed};
pub use source::{FileRange, Geometry, OwnerId, PieceSource, Priority, StreamId, UrgentRange};
pub use urgency::PlanConfig;
