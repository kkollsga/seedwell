//! All of this crate's integration tests in one binary (one link, one launch): each former
//! `tests/<name>.rs` is a module here. Tests that measure the process (memory) keep their own binary.

#[allow(unused_imports)]
#[path = "../adversarial_support/mod.rs"]
mod adversarial_support;
#[allow(unused_imports)]
#[path = "../common/mod.rs"]
mod common;

mod adversarial_events;
mod adversarial_gate;
mod adversarial_parse;
mod adversarial_private;
mod adversarial_props;
mod adversarial_schedule;
mod adversarial_tiers;
mod adversarial_transport;
mod dual_stack;
#[cfg(feature = "transport")]
mod dual_stack_transport;
#[cfg(feature = "transport")]
mod engine_compat;
mod fixes;
mod gate;
mod hub_props;
mod hub_trace;
mod machine_props;
mod memory;
mod model_check;
mod multi_tier_independence;
#[cfg(feature = "transport")]
mod network;
mod outage;
mod quit_stops;
mod set_hub;
mod set_port;
mod simulation;
#[cfg(feature = "transport")]
mod transport;
