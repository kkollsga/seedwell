//! `seedwell`: see `trs_cli`.
#![forbid(unsafe_code)]

/// Big blocks are mapped and unmapped directly (`trs_storage::MappedAlloc`): the daemon's footprint does not
/// keep the buffers of its start-up burst.
#[cfg(feature = "daemon")]
#[global_allocator]
static ALLOC: trs_storage::MappedAlloc = trs_storage::MappedAlloc;

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let daemon = argv.iter().any(|a| a == "daemon");
    // The daemon wants the engine's multi-thread runtime; every other command is one round trip.
    let code = if daemon && cfg!(feature = "daemon") {
        match trs_cli::runtime() {
            Ok(rt) => {
                let c = rt.block_on(trs_cli::run(argv));
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
            Ok(rt) => rt.block_on(trs_cli::run(argv)),
            Err(e) => {
                eprintln!("seedwell: cannot start the runtime: {e}");
                1
            }
        }
    };
    std::process::exit(code);
}
