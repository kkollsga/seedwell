//! All of this crate's integration tests in one binary (one link, one launch): each former
//! `tests/<name>.rs` is a module here. Tests that measure the process (memory) keep their own binary.

#[allow(unused_imports)]
#[path = "../sim/mod.rs"]
mod sim;

mod bencode_interop;
mod core_behaviour;
mod driver_local;
mod driver_shared;
mod real_world;
mod simulation;
