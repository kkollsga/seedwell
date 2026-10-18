//! `seedwell daemon`: the whole stack without a UI. It opens the state directory (database,
//! `config.toml`, `rules.toml`, `trackers.toml`, hot reload), starts the engine and serves the
//! control socket, so tests, benchmarks and Linux CI can drive a real engine through the same
//! protocol the app exposes.
//!
//! ```text
//! seedwell daemon --state-dir D --download-dir X [--listen-port P] [--bind 127.0.0.1]
//!     [--socket PATH] [--permission off|read_only|control|full|config] [--no-dht] [--no-trackers]
//!     [--portmap] [--ask] [--max-active N] [--shutdown-secs S] [--allow-lan-trackers]
//!     [--dev-mode] [--allow-loopback-trackers]
//! ```
//!
//! `--dev-mode` switches developer mode on for the run (whatever `config.toml` says): every tracker
//! exchange is written, secrets masked, to `<state-dir>/logs/trackers.log` (`seedwell log trackers`).
//!
//! **The flags win over `config.toml`** and are written into it before the engine starts, so a hot
//! reload of that file (which the daemon, like the app, applies) can never move the daemon back to
//! `~/Downloads` or a media library: the download and incomplete folders are the flags', watch
//! folders are cleared, and the
//! `.torrent` is never trashed. Everything else in `config.toml` (limits, queue, rules) is honoured.
//!
//! **Tracker destinations** follow the app: public addresses only, unless
//! `network.allow_lan_trackers` is on in `config.toml` (the Settings toggle) or `--allow-lan-trackers`
//! is given, which writes it there. That admits trackers on a private network (RFC 1918, CGNAT, ULA:
//! a LAN or VPN tracker); loopback, link-local and cloud-metadata addresses stay refused. Default: strict.
//!
//! It prints one line on stdout when it is ready, then runs until SIGINT/SIGTERM:
//! `seedwell daemon ready socket=<path> port=<tcp port> pid=<pid>`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::signal::unix::{SignalKind, signal};
use trs_control::{Permission, PermissionSource, StateAccess, serve_with as serve_backend_with};
use trs_engine::{Host, HostOptions};

use crate::args::{Opts, Usage};
use crate::commands::{CliError, Global};

fn bad(m: impl Into<String>) -> CliError {
    CliError::from(Usage(m.into()))
}

fn fail(m: impl Into<String>) -> CliError {
    CliError::plain(1, m)
}

/// Parse the options and run until signalled.
pub async fn run(mut o: Opts, g: &Global) -> Result<(), CliError> {
    let state_dir: PathBuf = o.value(&["--state-dir"])?.ok_or_else(|| bad("daemon needs --state-dir"))?.into();
    let download_dir: PathBuf = o.value(&["--download-dir"])?.ok_or_else(|| bad("daemon needs --download-dir"))?.into();
    let listen_port = o.number::<u16>(&["--listen-port"])?.unwrap_or(0);
    let bind: std::net::Ipv4Addr = match o.value(&["--bind"])? {
        None => std::net::Ipv4Addr::UNSPECIFIED,
        Some(b) => b.parse().map_err(|_| bad(format!("--bind expects an IPv4 address, got `{b}`")))?,
    };
    let no_dht = o.flag(&["--no-dht"]);
    let no_trackers = o.flag(&["--no-trackers"]);
    let portmap = o.flag(&["--portmap"]);
    // Smart add (SPEC 7A.6) holds multi-item and unsure torrents for a choice; a test daemon starts
    // everything at once unless asked to behave like the app.
    let ask = o.flag(&["--ask"]);
    // The grace period before a new torrent starts (SPEC 7A.7) is off in a test daemon (torrents start
    // at once, as scripts expect) unless `--grace SECS` asks for the app's behaviour.
    let grace = o.number::<u32>(&["--grace"])?.unwrap_or(0);
    let max_active = o.number::<usize>(&["--max-active"])?;
    let allow_lan_trackers = o.flag(&["--allow-lan-trackers"]);
    // Trackers on 127.0.0.1 (the mock tracker of `seedwell sim`, tests): off by default, like the app.
    let allow_loopback_trackers = o.flag(&["--allow-loopback-trackers"]);
    // Developer mode for this process: every tracker exchange goes to <state-dir>/logs/trackers.log.
    let dev_mode = o.flag(&["--dev-mode"]);
    let shutdown_secs = o.number::<u64>(&["--shutdown-secs"])?.unwrap_or(10);
    let permission = o.value(&["--permission"])?.unwrap_or_else(|| "control".into());
    let extra = o.finish()?;
    if let Some(x) = extra.first() {
        return Err(bad(format!("unexpected argument `{x}`")));
    }
    let perm = match permission.as_str() {
        "off" => PermissionSource::fixed(Permission::Off),
        "read_only" | "read-only" => PermissionSource::fixed(Permission::ReadOnly),
        "control" => PermissionSource::fixed(Permission::Control),
        "full" => PermissionSource::fixed(Permission::Full),
        "config" => PermissionSource::from_config_dir(&state_dir),
        x => return Err(bad(format!("--permission is off, read_only, control, full or config, not `{x}`"))),
    };
    std::fs::create_dir_all(&download_dir).map_err(|e| fail(format!("{}: {e}", download_dir.display())))?;
    let socket = if g.socket_given { g.socket.clone() } else { state_dir.join("control.sock") };

    let flags = Flags { listen_port, no_dht, portmap, ask, grace, max_active, allow_lan_trackers };
    let cfg = apply_flags_to_config(&state_dir, &download_dir, &flags)?;
    // The same setting the app honours, read from the file the flags were just written into.
    let allow_lan = cfg.network.allow_lan_trackers;
    let opts = HostOptions {
        tweak: Some(Box::new(move |ec| {
            // The settings the config file does not carry.
            ec.listen_addr = bind;
            ec.dht_bind = std::net::SocketAddrV4::new(bind, 0);
            ec.trackers_enabled = !no_trackers;
            ec.tracker_destinations.allow_lan_trackers = allow_lan;
            if allow_loopback_trackers {
                ec.tracker_destinations.allow_loopback = true;
            }
        })),
        persist_listen_port: false,
        developer_mode: dev_mode,
        ..HostOptions::default()
    };
    let host = Host::start(&state_dir, opts).await.map_err(|e| fail(format!("cannot start: {e}")))?;
    for w in &host.warnings {
        eprintln!("seedwell daemon: {w}");
    }
    let engine = host.handle();
    let store: Arc<dyn trs_control::state::StoreRef> = host.store().clone();
    let state = StateAccess::new(&state_dir, Some(store))
        .with_tz_offset(|| trs_engine::util::local_utc_offset(trs_engine::util::unix_secs() as i64));
    let options = trs_control::ServerOptions::default().with_config(&state.config());
    let server = serve_backend_with(engine.clone(), state, &socket, perm, options).await.map_err(|e| fail(format!("cannot serve {}: {e}", socket.display())))?;
    for w in server.warnings() {
        eprintln!("seedwell daemon: {w}");
    }
    println!("seedwell daemon ready socket={} port={} pid={}", socket.display(), engine.listen_port(), std::process::id());
    {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }

    let mut term = signal(SignalKind::terminate()).map_err(|e| fail(e.to_string()))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| fail(e.to_string()))?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    eprintln!("seedwell daemon: shutting down");
    server.shutdown().await;
    let r = host.shutdown(Duration::from_secs(shutdown_secs)).await;
    eprintln!("seedwell daemon: stopped (trackers confirmed {}, abandoned {})", r.stopped_confirmed, r.stopped_abandoned);
    Ok(())
}

/// The daemon's flags that end up in `config.toml`.
struct Flags {
    listen_port: u16,
    no_dht: bool,
    portmap: bool,
    ask: bool,
    /// `--grace`: seconds a new torrent is held before it starts (`transfers.start_delay_secs`).
    grace: u32,
    max_active: Option<usize>,
    /// `--allow-lan-trackers`: written as `network.allow_lan_trackers = true`. Absent, the file's
    /// own value stands (default off); a flag never turns it off.
    allow_lan_trackers: bool,
}

/// Write the flags into `<state_dir>/config.toml` (see the module docs for why) and return the
/// config that was saved.
fn apply_flags_to_config(
    state_dir: &std::path::Path,
    download_dir: &std::path::Path,
    f: &Flags,
) -> Result<trs_state::config::Config, CliError> {
    use trs_state::config::AfterAdd;
    let state = StateAccess::new(state_dir, None);
    let mut cfg = state.config();
    let abs = |p: &std::path::Path| -> Result<String, CliError> {
        std::path::absolute(p).map(|p| p.display().to_string()).map_err(|e| fail(format!("{}: {e}", p.display())))
    };
    cfg.folders.download_dir = abs(download_dir)?;
    cfg.folders.incomplete_dir = abs(&download_dir.join(".incomplete"))?;
    if !f.ask {
        cfg.transfers.ask_before_adding = trs_state::config::AskBeforeAdding::Never;
    }
    cfg.transfers.start_delay_secs = f.grace;
    cfg.folders.watch.clear();
    cfg.folders.torrent_after_add = AfterAdd::Leave;
    cfg.network.listen_port = f.listen_port;
    cfg.network.dht = !f.no_dht;
    cfg.network.upnp = f.portmap;
    cfg.network.natpmp = f.portmap;
    if let Some(n) = f.max_active {
        cfg.transfers.max_active_downloads = u32::try_from(n).unwrap_or(u32::MAX);
    }
    if f.allow_lan_trackers {
        cfg.network.allow_lan_trackers = true;
    }
    state.save_config(&cfg).map_err(|e| fail(format!("config.toml in {}: {e}", state_dir.display())))?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(allow_lan_trackers: bool) -> Flags {
        Flags { listen_port: 0, no_dht: true, portmap: false, ask: false, grace: 0, max_active: None, allow_lan_trackers }
    }

    fn apply(dir: &std::path::Path, f: &Flags) -> trs_state::config::Config {
        apply_flags_to_config(dir, &dir.join("dl"), f).unwrap()
    }

    #[test]
    fn lan_trackers_are_refused_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = apply(dir.path(), &flags(false));
        assert!(!cfg.network.allow_lan_trackers);
        assert!(!StateAccess::new(dir.path(), None).config().network.allow_lan_trackers);
    }

    #[test]
    fn the_flag_writes_the_setting() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = apply(dir.path(), &flags(true));
        assert!(cfg.network.allow_lan_trackers);
        let text = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        assert!(text.contains("allow_lan_trackers = true"), "{text}");
        // ... and it sticks for the next start without the flag.
        assert!(apply(dir.path(), &flags(false)).network.allow_lan_trackers);
    }

    #[test]
    fn the_setting_in_config_toml_is_honoured_without_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[network]\nallow_lan_trackers = true\n").unwrap();
        assert!(apply(dir.path(), &flags(false)).network.allow_lan_trackers);
    }
}
