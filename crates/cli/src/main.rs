//! `seedwell`: see `seedwell_cli`.
#![forbid(unsafe_code)]

/// Big blocks are mapped and unmapped directly (`seedwell_storage::MappedAlloc`): the daemon's footprint does not
/// keep the buffers of its start-up burst.
#[cfg(feature = "daemon")]
#[global_allocator]
static ALLOC: seedwell_storage::MappedAlloc = seedwell_storage::MappedAlloc;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let daemon = argv.iter().any(|a| a == "daemon" || a == "relay");
    // The daemon (and the sync relay) want the engine's multi-thread runtime; every other command is one round trip.
    let code = if daemon && cfg!(feature = "daemon") {
        match seedwell_cli::runtime() {
            Ok(rt) => {
                let c = rt.block_on(seedwell_cli::run(argv));
                rt.shutdown_timeout(std::time::Duration::from_secs(2));
                c
            }
            Err(e) => {
                eprintln!("seedwell: cannot start the runtime: {e}");
                1
            }
        }
    } else {
        match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(seedwell_cli::run(argv)),
            Err(e) => {
                eprintln!("seedwell: cannot start the runtime: {e}");
                1
            }
        }
    };
    std::process::exit(code);
}
