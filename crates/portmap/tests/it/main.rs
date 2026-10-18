//! All of this crate's integration tests in one binary (one link, one launch): each former
//! `tests/<name>.rs` is a module here. Tests that measure the process (memory) keep their own binary.

#[allow(unused_imports)]
#[path = "../common/mod.rs"]
mod common;

mod core_virtual;
#[cfg(feature = "driver")]
mod natpmp_loopback;
#[cfg(feature = "driver")]
mod pcp_loopback;
#[cfg(feature = "driver")]
mod real_router;
#[cfg(feature = "driver")]
mod upnp_loopback;
