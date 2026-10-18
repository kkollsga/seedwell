//! trs-portmap: NAT-PMP (RFC 6886), PCP (RFC 6887) and UPnP-IGD port mapping.
//!
//! * [`PortMapper`] is a sans-I/O state machine: `handle(input, now) -> Vec<Output>`.
//!   Time is injected as [`Millis`], so renewals, back-off and gateway reboots are
//!   tested on a virtual clock.
//! * [`driver::spawn`] wraps it in a small tokio driver (one UDP socket, short-lived
//!   TCP connections for SOAP, one timer).
//!
//! The protocol codecs ([`natpmp`], [`pcp`], [`ssdp`], [`upnp`], [`http`], [`xml`],
//! [`gateway`]) are pure functions over untrusted bytes and are fuzzed. See the README
//! for the politeness rules.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod core;
#[cfg(feature = "driver")]
pub mod driver;
pub mod gateway;
pub mod http;
pub mod natpmp;
pub mod pcp;
mod rng;
pub mod ssdp;
pub mod upnp;
pub mod xml;

use std::time::Duration;

pub use crate::core::{
    Config, Event, Failure, HttpFailure, HttpRequest, Input, MAX_WANTED, Mapping, NetInfo, Output,
    PortMapper, Wanted, is_public_ipv4,
};

/// Transport protocol of a mapping.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Protocol {
    /// TCP
    Tcp,
    /// UDP
    Udp,
}

impl Protocol {
    /// IANA protocol number (PCP).
    pub fn number(self) -> u8 {
        match self {
            Protocol::Tcp => 6,
            Protocol::Udp => 17,
        }
    }

    /// Name used by UPnP (`TCP` / `UDP`).
    pub fn name(self) -> &'static str {
        match self {
            Protocol::Tcp => "TCP",
            Protocol::Udp => "UDP",
        }
    }
}

/// Which mechanism created the mappings.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Method {
    /// PCP (RFC 6887)
    Pcp,
    /// NAT-PMP (RFC 6886)
    NatPmp,
    /// UPnP IGD
    Upnp,
}

/// Monotonic milliseconds since an arbitrary start. The caller owns the clock.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Millis(pub u64);

impl Millis {
    /// `self + d`, saturating.
    pub fn plus(self, d: Duration) -> Millis {
        Millis(
            self.0
                .saturating_add(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        )
    }

    /// `self + ms`, saturating.
    pub fn plus_ms(self, ms: u64) -> Millis {
        Millis(self.0.saturating_add(ms))
    }

    /// Time from `earlier` to `self` (zero if `earlier` is later).
    pub fn since(self, earlier: Millis) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}
