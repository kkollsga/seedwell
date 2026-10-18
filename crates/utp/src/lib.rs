//! trs-utp: uTP (BEP 29) with LEDBAT congestion control (RFC 6817).
//!
//! * [`Connection`] is a pure state machine: packets and time in, packets and bytes out.
//! * [`Endpoint`] multiplexes many connections over one UDP socket, hands non-uTP datagrams back
//!   (so the DHT and UDP trackers can share the port) and enforces abuse limits. Also sans-I/O.
//! * [`UtpSocket`] / [`UtpStream`] are the tokio driver: one task and one timer for every
//!   connection, and a stream that implements `AsyncRead + AsyncWrite`.
//!
//! See the README for design, memory budget and interop status.
// Unsafe code is confined to `sockopt` (one setsockopt wrapper); everything else is safe.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod bytes;
pub mod config;
pub mod conn;
mod driver;
pub mod endpoint;
pub mod ledbat;
pub mod packet;
pub mod rng;
pub mod rtt;
pub mod seq;
mod sockopt;
mod trace;

pub use crate::config::Config;
pub use crate::conn::{Connection, End, EV_READ, EV_WRITE, ReadOutcome, State, Stats, WriteOutcome};
pub use crate::driver::{ForeignDatagram, SocketStats, UtpSocket, UtpStream};
pub use crate::endpoint::{ConnHandle, ConnStatus, Disposition, Endpoint};
