//! All of this crate's integration tests in one binary (one link, one launch): each former
//! `tests/<name>.rs` is a module here. Tests that measure the process (memory) keep their own binary.

#[allow(unused_imports)]
#[path = "../common/mod.rs"]
mod common;

mod basic;
mod boot;
mod devlog;
mod direct;
mod encryption;
mod gaps;
mod idle_stop;
mod lifecycle;
mod listen_port;
mod locations;
mod merge_trackers;
mod moves;
mod network;
mod organize;
mod outage;
mod pending_start;
mod policy;
mod portmap;
mod private_wire;
mod quit;
mod recover;
mod redownload;
mod redundant;
mod row_facts;
mod seeding;
mod settings;
mod sharing;
mod smart_add;
mod state_store;
mod status_folders;
mod streaming;
mod streaming_players;
mod swarm;
mod tracker_audit;
mod tracker_limits;
mod utp;
mod watch;
