//! `seedwell daemon`: the whole stack without a UI. It opens the state directory (database,
//! `config.toml`, `rules.toml`, `trackers.toml`, hot reload), starts the engine and serves the
//! control socket, so tests, benchmarks and Linux CI can drive a real engine through the same
//! protocol the app exposes.
//!
//! ```text
//! seedwell daemon --state-dir D --download-dir X [--listen-port P] [--bind 127.0.0.1]
//!     [--socket PATH] [--permission off|read_only|control|full|config] [--no-dht] [--no-trackers]
//!     [--portmap] [--ask] [--max-active N] [--shutdown-secs S] [--allow-lan-trackers]
//!     [--dev-mode] [--allow-loopback-trackers] [--no-sync]
//! ```
//!
//! **Folder sync** (SPEC 7G) runs next to the engine unless `--no-sync`: `sync.toml` and `sync/` in the
//! state directory, keys in `<state-dir>/sync/secrets` (0600), devices found over the engine's DHT, the
//! sync port mapped by its port mapper, bound to `--bind` (LAN discovery is off on a loopback bind). It
//! stops before the engine.
//!
//! `--dev-mode` switches developer mode on for the run (whatever `config.toml` says): every tracker
//! exchange is written, secrets masked, to `<state-dir>/logs/trackers.log` (`seedwell log trackers`).
//!
//! **The flags win over `config.toml`** and are written into it before the engine starts, so a hot
//! reload of that file (which the daemon, like the app, applies) can never move the daemon back to
//! `~/Downloads` or a media library: the download and incomplete folders are the flags', watch
//! folders are cleared, and the
//! `.torrent` is never trashed. Everything else in `config.toml` (limits, queue, rules) is honoured.
//! A `config.toml` with values that cannot be used (`LoadSource::Repaired`), or one that is broken,
//! is never rewritten: writing it would turn each salvaged value into its replacement for good and
//! lose what the user wrote. The flags then apply in memory only, at start and at every hot reload
//! (`HostOptions::adjust_config`), the warnings are logged once at start, and the file stays as it is.
//!
//! **Tracker destinations** follow the app: public addresses only, unless
//! `network.allow_lan_trackers` is on in `config.toml` (the Settings toggle) or `--allow-lan-trackers`
//! is given, which writes it there. That admits trackers on a private network (RFC 1918, CGNAT, ULA:
//! a LAN or VPN tracker); loopback, link-local and cloud-metadata addresses stay refused. Default: strict.
//!
//! It prints one line on stdout when it is ready, then runs until SIGINT/SIGTERM:
//! `seedwell daemon ready socket=<path> port=<tcp port> pid=<pid> sync_port=<port, 0 without sync>`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use trs_control::sync::{SyncSlot, start_sync_with};
use trs_control::{EngineBackend, Permission, PermissionSource, StateAccess, serve_backend};
use trs_engine::{Host, HostOptions};
use trs_sync_net::secrets::FileSecrets;

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
    let no_sync = o.flag(&["--no-sync"]);
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
    let (cfg, adjust, written) = apply_flags_to_config(&state_dir, &download_dir, &flags)?;
    if !written {
        eprintln!("seedwell daemon: config.toml has values that cannot be used: the options apply to this run only and the file is left as it is");
    }
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
        developer_mode: dev_mode,
        adjust_config: Some(adjust),
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
    let slot = SyncSlot::new();
    let sync = if no_sync { None } else { start_folder_sync(&state_dir, &engine, bind, &slot).await };
    slot.set(sync.clone());
    let backend = Arc::new(EngineBackend::new(engine.clone()).with_sync(slot.clone()));
    let server = serve_backend(backend, state, &socket, perm, options).await.map_err(|e| fail(format!("cannot serve {}: {e}", socket.display())))?;
    for w in server.warnings() {
        eprintln!("seedwell daemon: {w}");
    }
    println!(
        "seedwell daemon ready socket={} port={} pid={} sync_port={}",
        socket.display(),
        engine.listen_port(),
        std::process::id(),
        sync.as_ref().map_or(0, |h| h.port())
    );
    {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
    }

    trs_platform::signal::Shutdown::new().map_err(|e| fail(e.to_string()))?.recv().await;
    eprintln!("seedwell daemon: shutting down");
    server.shutdown().await;
    // The slot's service: a port change may have started another one.
    if let Some(h) = slot.get() {
        slot.set(None);
        h.shutdown().await;
    }
    let r = host.shutdown(Duration::from_secs(shutdown_secs)).await;
    eprintln!("seedwell daemon: stopped (trackers confirmed {}, abandoned {})", r.stopped_confirmed, r.stopped_abandoned);
    Ok(())
}

/// The folder sync service next to the engine. A failure is reported and the daemon runs on without
/// it: a sync problem never stops torrents (SPEC 7G).
async fn start_folder_sync(state_dir: &std::path::Path, engine: &trs_engine::EngineHandle, bind: std::net::Ipv4Addr, slot: &SyncSlot) -> Option<trs_sync_net::SyncHandle> {
    let secrets: Arc<dyn trs_sync_net::secrets::SecretStore> = Arc::new(FileSecrets::new(state_dir.join("sync").join("secrets")));
    let (state_dir, engine) = (state_dir.to_path_buf(), engine.clone());
    // The same start again when the port changes (`SyncSlot::move_port`).
    let starter: trs_control::sync::Starter = Arc::new(move || {
        let (state_dir, engine, secrets) = (state_dir.clone(), engine.clone(), secrets.clone());
        Box::pin(async move {
            let client = format!("Seedwell {}", env!("CARGO_PKG_VERSION"));
            start_sync_with(&state_dir, client, secrets, Some(&engine), |o| {
                o.bind = std::net::IpAddr::V4(bind);
                // Announcing test daemons on the real LAN would only add noise.
                o.lan_discovery = !bind.is_loopback();
            })
            .await
        })
    });
    slot.set_starter(starter.clone());
    let started = starter().await;
    match started {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("seedwell daemon: folder sync did not start: {e}");
            None
        }
    }
}

/// The daemon's flags that end up in `config.toml`.
#[derive(Clone)]
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

/// The flags, put over a loaded `config.toml`.
fn apply_flags(cfg: &mut trs_state::config::Config, download_dir: &str, incomplete_dir: &str, f: &Flags) {
    use trs_state::config::AfterAdd;
    cfg.folders.download_dir = download_dir.to_owned();
    cfg.folders.incomplete_dir = incomplete_dir.to_owned();
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
}

/// Put the flags over `<state_dir>/config.toml` and write them into it (see the module docs for
/// why), unless the file has values that cannot be used or is broken: then nothing is written.
/// Returns the config with the flags, the same change for the host to make at start and at every
/// hot reload, and whether the file was written.
fn apply_flags_to_config(
    state_dir: &std::path::Path,
    download_dir: &std::path::Path,
    f: &Flags,
) -> Result<(trs_state::config::Config, trs_engine::ConfigAdjust, bool), CliError> {
    use trs_state::{DocStore, LoadSource};
    let abs = |p: &std::path::Path| -> Result<String, CliError> {
        std::path::absolute(p).map(|p| p.display().to_string()).map_err(|e| fail(format!("{}: {e}", p.display())))
    };
    let (dl, incomplete) = (abs(download_dir)?, abs(&download_dir.join(".incomplete"))?);
    let docs = DocStore::<trs_state::config::Config>::new(state_dir);
    let loaded = docs.load();
    let mut cfg = loaded.value;
    apply_flags(&mut cfg, &dl, &incomplete, f);
    // A clean file takes the flags (a hot reload of it keeps them); one with bad values is left
    // for the user to fix, so that its salvaged values never become its replacements for good.
    let clean = matches!(loaded.source, LoadSource::File | LoadSource::Created) && loaded.error.is_none();
    if clean {
        docs.save(&cfg).map_err(|e| fail(format!("config.toml in {}: {e}", state_dir.display())))?;
    }
    let f = f.clone();
    let adjust: trs_engine::ConfigAdjust = std::sync::Arc::new(move |c| apply_flags(c, &dl, &incomplete, &f));
    Ok((cfg, adjust, clean))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(allow_lan_trackers: bool) -> Flags {
        Flags { listen_port: 0, no_dht: true, portmap: false, ask: false, grace: 0, max_active: None, allow_lan_trackers }
    }

    fn apply(dir: &std::path::Path, f: &Flags) -> trs_state::config::Config {
        apply_flags_to_config(dir, &dir.join("dl"), f).unwrap().0
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

    /// A file with a bad value (or a broken one) is not written: the flags come back in memory,
    /// and the host gets the same change for every reload. A clean file is written as before.
    #[test]
    fn a_file_with_bad_values_is_left_as_it_is_and_the_flags_apply_in_memory() {
        use trs_state::config::{AfterAdd, Config};
        for text in ["[folders]\ntorrent_after_add = \"keep\"\n[network]\nupnp = true\n", "[network\nupnp = true\n"] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("config.toml"), text).unwrap();
            let f = Flags { max_active: Some(5), ..flags(false) };
            let (cfg, adjust, written) = apply_flags_to_config(dir.path(), &dir.path().join("dl"), &f).unwrap();
            assert!(!written);
            assert_eq!(std::fs::read_to_string(dir.path().join("config.toml")).unwrap(), text, "the user's file is left as it is");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "nothing is written next to it");
            assert!(cfg.folders.download_dir.ends_with("dl") && cfg.folders.watch.is_empty() && !cfg.network.upnp);
            assert_eq!((cfg.folders.torrent_after_add, cfg.transfers.max_active_downloads), (AfterAdd::Leave, 5));
            let mut reloaded = Config::default();
            adjust(&mut reloaded);
            assert_eq!((reloaded.folders.download_dir.as_str(), reloaded.transfers.max_active_downloads), (cfg.folders.download_dir.as_str(), 5));
            assert!(reloaded.folders.watch.is_empty());
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[network]\nupnp = true\n").unwrap();
        let (_, _, written) = apply_flags_to_config(dir.path(), &dir.path().join("dl"), &flags(false)).unwrap();
        assert!(written, "a clean file takes the flags");
        assert!(std::fs::read_to_string(dir.path().join("config.toml")).unwrap().contains("upnp = false"));
    }

    #[test]
    fn the_setting_in_config_toml_is_honoured_without_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[network]\nallow_lan_trackers = true\n").unwrap();
        assert!(apply(dir.path(), &flags(false)).network.allow_lan_trackers);
    }
}
