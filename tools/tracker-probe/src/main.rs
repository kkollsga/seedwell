//! tracker-probe: observe what real BitTorrent clients send to trackers.
//!
//! This tool only OBSERVES mainstream clients (qBittorrent, Transmission) (and runs a local mock tracker for
//! regression controls). Seedwell never impersonates another client.

#![forbid(unsafe_code)]

mod analyze;
mod bencode;
mod diff;
mod log;
mod mock;
mod mktest;
mod proxy;
mod record;
mod rewrite;
mod server;

use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "tracker-probe", version, about = "Logging tracker proxy, mock tracker and capture analyzer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Logging reverse proxy in front of real HTTP(S) trackers.
    Proxy(proxy::Args),
    /// Rewrite a .torrent's tracker URLs to go through the proxy (info-hash preserved).
    Rewrite(rewrite::Args),
    /// Human-readable timeline and detector verdicts for a capture.
    Analyze(analyze::AnalyzeArgs),
    /// Detector verdicts only; exit code 1 on any violation.
    Check(analyze::CheckArgs),
    /// Semantic comparison of two captures.
    Diff(diff::Args),
    /// Local mock tracker (optionally with broken IPv6) for positive controls.
    Mock(mock::Args),
    /// Generate synthetic test torrents that announce to a given URL.
    Mktest(mktest::Args),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.cmd {
        Cmd::Proxy(a) => proxy::run(a).await.map(|()| false),
        Cmd::Mock(a) => mock::run(a).await.map(|()| false),
        Cmd::Rewrite(a) => rewrite::run(a).map(|()| false),
        Cmd::Analyze(a) => analyze::run_analyze(&a),
        Cmd::Check(a) => analyze::run_check(&a),
        Cmd::Diff(a) => diff::run(&a).map(|()| false),
        Cmd::Mktest(a) => mktest::run(&a).map(|()| false),
    };
    match result {
        Ok(false) => ExitCode::SUCCESS,
        Ok(true) => ExitCode::from(1),
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}
