//! Integration tests of trs-sync-net (one binary; temp folders and loopback only).

mod bundle;
mod chat;
mod devices;
mod direct;
mod files;
mod gone;
mod ignored;
mod images;
mod isolation;
mod lazy;
mod lan;
mod limits;
mod ondemand;
mod reach;
mod read_only;
mod relay;
mod removals;
mod rendezvous;
mod roles;
mod torrents;
mod two_devices;
mod utp;
mod versions;

/// A loopback address that refuses connections, and nothing else can listen at
/// ([`trs_platform::net::closed_tcp_port`]).
pub(crate) fn closed_addr() -> std::net::SocketAddr {
    let lo = std::net::Ipv4Addr::LOCALHOST;
    std::net::SocketAddr::from((lo, trs_platform::net::closed_tcp_port(&[lo.into()])))
}
