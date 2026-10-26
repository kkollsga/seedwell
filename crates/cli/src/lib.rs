//! The `seedwell` command: the CLI for the control socket (for people, scripts and AI agents),
//! the Agent Skill that ships inside it, and `seedwell daemon` (headless engine, for tests,
//! benchmarks and Linux CI).
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod args;
pub mod awake;
pub mod chat;
pub mod mail;
pub mod pass;
pub mod cmd;
pub mod commands;
#[cfg(feature = "daemon")]
pub mod daemon;
pub mod debug;
pub mod help;
pub mod render;
pub mod select;
pub mod sim;
pub mod skill;
pub mod tags;
pub mod sync;

use std::path::PathBuf;

use args::{Opts, Usage};
use commands::{CliError, Global};

/// Print `seedwell help [topic]`.
pub fn print_help(topic: Option<&str>) -> Result<(), CliError> {
    match topic {
        None => print!("{}", help::overview()),
        Some("agents") => print!("{}", help::agents_guide()),
        Some("chat") => print!("{}", help::CHAT_GUIDE),
        Some("mail") => print!("{}", help::MAIL_GUIDE),
        Some(name) => match help::find(name) {
            Some(c) => print!("{}", help::command_help(c)),
            None => return Err(Usage(format!("no help for `{name}`; try `seedwell help`")).into()),
        },
    }
    Ok(())
}

/// Run the CLI with `argv` (without the program name) on a tokio runtime; returns the exit code.
pub async fn run(argv: Vec<String>) -> i32 {
    let mut o = Opts::new(argv);
    let json = o.flag(&["--json"]);
    let help = o.flag(&["-h", "--help"]);
    let version = o.flag(&["-V", "--version"]);
    let launch = o.flag(&["--launch"]);
    let yes_i_know = o.flag(&["--yes-i-know"]);
    let globals = o.value(&["--socket"]).and_then(|s| Ok((s, o.value(&["--client"])?)));
    let (socket, client) = match globals {
        Ok(v) => v,
        Err(u) => {
            eprintln!("seedwell: {u}");
            return 2;
        }
    };
    if version {
        println!("seedwell {}", env!("CARGO_PKG_VERSION"));
        return 0;
    }
    let rest = o.into_rest();
    let Some(first) = rest.first().cloned() else {
        print!("{}", help::overview());
        return if help { 0 } else { 2 };
    };
    // `seedwell mail <sub> --help`: that one command, not the whole page.
    if help && first == "mail"
        && let Some(text) = rest.get(1).and_then(|sub| mail::sub_help(sub)) {
            print!("{text}");
            return 0;
        }
    if help && first != "help" {
        return match help::find(&first) {
            Some(c) => {
                print!("{}", help::command_help(c));
                0
            }
            None => {
                print!("{}", help::overview());
                0
            }
        };
    }
    let g = Global {
        json,
        socket_given: socket.is_some(),
        socket: socket.map_or_else(trs_control::default_socket_path, PathBuf::from),
        client_given: client.is_some(),
        client: client.unwrap_or_else(|| "seedwell-cli".into()),
        launch,
        yes_i_know,
    };
    if first == "daemon" {
        return daemon_entry(rest[1..].to_vec(), &g).await;
    }
    let cmd = match cmd::parse(rest) {
        Ok(c) => c,
        Err(u) => {
            let mut e = CliError::from(u);
            if !json
                && let Some(c) = help::find(&first) {
                    // A long usage (chat, mail) would bury the one thing that went wrong: point to the help instead.
                    let usage = c.usage.lines().next().unwrap_or("");
                    if usage.chars().count() <= 160 {
                        e.message.push_str(&format!("\nusage: seedwell {usage}   (seedwell {first} --help for examples)"));
                    } else if !e.message.starts_with("usage:") {
                        e.message.push_str(&format!("\n(seedwell {first} --help shows every option)"));
                    }
                }
            return finish(&g, &e);
        }
    };
    match commands::run(cmd, &g).await {
        Ok(()) => 0,
        Err(e) => finish(&g, &e),
    }
}

fn finish(g: &Global, e: &CliError) -> i32 {
    commands::print_error(g, e);
    e.code
}

#[cfg(feature = "daemon")]
async fn daemon_entry(rest: Vec<String>, g: &Global) -> i32 {
    match daemon::run(Opts::new(rest), g).await {
        Ok(()) => 0,
        Err(e) => finish(g, &e),
    }
}

#[cfg(not(feature = "daemon"))]
async fn daemon_entry(_rest: Vec<String>, _g: &Global) -> i32 {
    eprintln!("seedwell: this build has no daemon (built without the `daemon` feature)");
    2
}

/// The multi-thread runtime the daemon uses (the engine's own sizing).
#[cfg(feature = "daemon")]
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    trs_engine::build_runtime()
}

/// Without the daemon there is nothing multi-threaded to run.
#[cfg(not(feature = "daemon"))]
pub fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread().enable_all().build()
}
