//! `seedwell sync …`: folder sync (SPEC 7G) through the control socket, and `seedwell relay`.
//!
//! `<folder>` is a folder id, an unambiguous prefix of it (the first group is enough) or the folder's name;
//! the app resolves it (`sync.*` methods), so an ambiguous one is exit 3 with the candidates. Keys are
//! secrets: `sync share` prints them (permission full), nothing else does, and `sync join -` reads the key
//! from standard input so it stays out of the shell history.
#![allow(missing_docs)] // variant fields are named after the options they hold

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write as _};

use trs_control::human;
use trs_control::proto::*;
use trs_control::types::*;

use crate::args::{Opts, Usage, parse_rate};
use crate::commands::{CliError, Global, R, ask, connect, json_text, show};

/// A `sync` subcommand.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Cmd {
    /// `sync list`
    #[default]
    List,
    /// `sync status <folder>`
    Status { folder: String },
    /// `sync add <path>`
    Add { path: String },
    /// `sync join <key-or-link> [--to <path>]` (`-` reads the key from standard input)
    Join { key: String, to: Option<String> },
    /// `sync share <folder>`
    Share { folder: String },
    /// `sync rename <folder> <name>`
    Rename { folder: String, name: String },
    /// `sync move <folder> <path>`
    Move { folder: String, path: String },
    /// `sync pause <folder>`
    Pause { folder: String },
    /// `sync resume <folder>`
    Resume { folder: String },
    /// `sync rescan <folder>`
    Rescan { folder: String },
    /// `sync lan-only <folder> on|off`
    LanOnly { folder: String, on: bool },
    /// `sync conflicts <folder>`
    Conflicts { folder: String },
    /// `sync resolve <folder> <copy> --keep original|copy|both`
    Resolve { folder: String, copy: String, keep: String },
    /// `sync address <folder> <host:port>`
    Address { folder: String, address: String },
    /// `sync remove <folder> [--delete-files] [-y]`
    Remove { folder: String, delete_files: bool, yes: bool },
    /// `sync remove-confirm <token>`
    RemoveConfirm { token: String },
    /// `sync settings`: read them.
    Settings,
    /// `sync settings --device-name N ...`: change them.
    SetSettings(SyncSettingsParams),
}

/// The one-line usage.
pub const USAGE: &str = "usage: seedwell sync list | status <folder> | add <path> | join <key-or-link> [--to <path>] | share <folder> | rename <folder> <name> | move <folder> <path> | pause <folder> | resume <folder> | rescan <folder> | lan-only <folder> on|off | conflicts <folder> | resolve <folder> <copy> --keep original|copy|both | address <folder> <host:port> | remove <folder> [--delete-files] [-y] | remove-confirm <token> | settings [--device-name N] [--port N] [--lan on|off] [--dht on|off] [--relay-own on|off] [--relay-everyone on|off] [--up R] [--down R] [--default-location PATH]";

fn on_off(v: Option<String>, name: &str) -> Result<Option<bool>, Usage> {
    match v.as_deref() {
        None => Ok(None),
        Some("on" | "yes" | "true") => Ok(Some(true)),
        Some("off" | "no" | "false") => Ok(Some(false)),
        Some(x) => Err(Usage(format!("{name} is on or off, not `{x}`"))),
    }
}

fn args(o: Opts, n: usize, usage: &str) -> Result<Vec<String>, Usage> {
    let pos = o.finish()?;
    if pos.len() != n {
        return Err(Usage(format!("usage: seedwell sync {usage}")));
    }
    Ok(pos)
}

/// Words after `sync`.
pub fn parse(args_in: Vec<String>) -> Result<Cmd, Usage> {
    let mut it = args_in.into_iter();
    let sub = it.next().ok_or_else(|| Usage(USAGE.into()))?;
    let mut o = Opts::new(it.collect());
    let one = |o: Opts, usage: &str| -> Result<String, Usage> { Ok(args(o, 1, usage)?.remove(0)) };
    Ok(match sub.as_str() {
        "list" | "ls" => {
            args(o, 0, "list")?;
            Cmd::List
        }
        "status" | "info" => Cmd::Status { folder: one(o, "status <folder>")? },
        "add" => Cmd::Add { path: one(o, "add <path>")? },
        "join" => {
            let to = o.value(&["--to"])?;
            Cmd::Join { key: one(o, "join <key-or-link> [--to <path>]   (- reads the key from standard input)")?, to }
        }
        "share" | "keys" => Cmd::Share { folder: one(o, "share <folder>")? },
        "rename" => {
            let mut p = args(o, 2, "rename <folder> <name>")?;
            Cmd::Rename { name: p.remove(1), folder: p.remove(0) }
        }
        "move" => {
            let mut p = args(o, 2, "move <folder> <path>")?;
            Cmd::Move { path: p.remove(1), folder: p.remove(0) }
        }
        "pause" => Cmd::Pause { folder: one(o, "pause <folder>")? },
        "resume" => Cmd::Resume { folder: one(o, "resume <folder>")? },
        "rescan" | "scan" => Cmd::Rescan { folder: one(o, "rescan <folder>")? },
        "lan-only" | "lan_only" => {
            let mut p = args(o, 2, "lan-only <folder> on|off")?;
            let on = match p.remove(1).as_str() {
                "on" | "yes" | "true" => true,
                "off" | "no" | "false" => false,
                x => return Err(Usage(format!("lan-only is on or off, not `{x}`"))),
            };
            Cmd::LanOnly { folder: p.remove(0), on }
        }
        "conflicts" => Cmd::Conflicts { folder: one(o, "conflicts <folder>")? },
        "resolve" => {
            let keep = o.value(&["--keep"])?.ok_or_else(|| Usage("usage: seedwell sync resolve <folder> <copy> --keep original|copy|both".into()))?;
            if !matches!(keep.as_str(), "original" | "copy" | "both") {
                return Err(Usage(format!("--keep is original, copy or both, not `{keep}`")));
            }
            let mut p = args(o, 2, "resolve <folder> <copy> --keep original|copy|both")?;
            Cmd::Resolve { copy: p.remove(1), folder: p.remove(0), keep }
        }
        "address" | "add-address" => {
            let mut p = args(o, 2, "address <folder> <host:port>")?;
            Cmd::Address { address: p.remove(1), folder: p.remove(0) }
        }
        "remove" | "rm" => {
            let delete_files = o.flag(&["--delete-files", "--delete"]);
            let yes = o.flag(&["-y", "--yes"]);
            Cmd::Remove { folder: one(o, "remove <folder> [--delete-files] [-y]")?, delete_files, yes }
        }
        "remove-confirm" => Cmd::RemoveConfirm { token: one(o, "remove-confirm <token>")? },
        "settings" => {
            let p = SyncSettingsParams {
                device_name: o.value(&["--device-name", "--name"])?,
                port: o.number::<u16>(&["--port"])?,
                lan_discovery: on_off(o.value(&["--lan"])?, "--lan")?,
                dht_discovery: on_off(o.value(&["--dht", "--internet"])?, "--dht")?,
                relay_own_devices: on_off(o.value(&["--relay-own"])?, "--relay-own")?,
                relay_everyone: on_off(o.value(&["--relay-everyone"])?, "--relay-everyone")?,
                up_limit: o.value(&["--up"])?.map(|v| parse_rate(&v)).transpose()?,
                down_limit: o.value(&["--down"])?.map(|v| parse_rate(&v)).transpose()?,
                default_location: o.value(&["--default-location"])?,
            };
            args(o, 0, "settings [--device-name N] [--port N] [--lan on|off] [--dht on|off] [--relay-own on|off] [--relay-everyone on|off] [--up R] [--down R] [--default-location PATH]")?;
            if p.is_empty() { Cmd::Settings } else { Cmd::SetSettings(p) }
        }
        other => return Err(Usage(format!("unknown sync command `{other}`. {USAGE}"))),
    })
}

/// A path as the app needs it: `~/…` stays (the app expands it), anything else is made absolute against the
/// current folder (the app does not know where the CLI runs).
fn abs(p: &str) -> R<String> {
    if p == "~" || p.starts_with("~/") {
        return Ok(p.to_owned());
    }
    std::path::absolute(p).map(|a| a.display().to_string()).map_err(|e| CliError::plain(2, format!("{p}: {e}")))
}

/// The key from standard input, for `sync join -`.
fn key_from_stdin() -> R<String> {
    if std::io::stdin().is_terminal() {
        eprint!("Folder key or link: ");
        let _ = std::io::stderr().flush();
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| CliError::plain(1, format!("cannot read the key: {e}")))?;
    let k = line.trim().to_owned();
    if k.is_empty() {
        return Err(CliError::plain(2, "no key given on standard input"));
    }
    Ok(k)
}

fn folder(folder: String) -> SyncFolderParams {
    SyncFolderParams { folder }
}

/// Run a `sync` command.
pub async fn run(cmd: Cmd, g: &Global) -> R {
    let mut c = connect(g).await?;
    match cmd {
        Cmd::List => {
            let (raw, l): (_, SyncListDto) = ask(&mut c, Request::SyncList(Empty {})).await?;
            show(g, &raw, || list(&l));
        }
        Cmd::Status { folder: f } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncStatus(folder(f))).await?;
            show(g, &raw, || status(&d));
        }
        Cmd::Add { path } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncAdd(SyncAddParams { path: abs(&path)? })).await?;
            show(g, &raw, || format!("Sharing {} from {}.\nGive another device its key: seedwell sync share {}", d.name, d.path, first_group(&d.id)));
        }
        Cmd::Join { key, to } => {
            let key = if key == "-" { key_from_stdin()? } else { key };
            let path = to.as_deref().map(abs).transpose()?;
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncJoin(SyncJoinParams { key_or_link: key, path })).await?;
            show(g, &raw, || format!("Joined {} at {} ({}).", d.name, d.path, access(&d.access)));
        }
        Cmd::Share { folder: f } => {
            let (raw, s): (_, SyncSharesDto) = ask(&mut c, Request::SyncShare(folder(f))).await?;
            show(g, &raw, || shares(&s));
        }
        Cmd::Rename { folder: f, name } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncRename(SyncRenameParams { folder: f, name })).await?;
            show(g, &raw, || format!("Renamed: {} is now {}.", first_group(&d.id), d.path));
        }
        Cmd::Move { folder: f, path } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncMove(SyncMoveParams { folder: f, path: abs(&path)? })).await?;
            show(g, &raw, || format!("Moved {} to {}.", d.name, d.path));
        }
        Cmd::Pause { folder: f } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncPause(folder(f))).await?;
            show(g, &raw, || format!("Paused {}.", d.name));
        }
        Cmd::Resume { folder: f } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncResume(folder(f))).await?;
            show(g, &raw, || format!("Resumed {}.", d.name));
        }
        Cmd::Rescan { folder: f } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncRescan(folder(f))).await?;
            show(g, &raw, || format!("Scanning {} now.", d.name));
        }
        Cmd::LanOnly { folder: f, on } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncSetLanOnly(SyncLanOnlyParams { folder: f, lan_only: on })).await?;
            show(g, &raw, || if on { format!("{} now syncs only with devices on the local network.", d.name) } else { format!("{} syncs with devices everywhere again.", d.name) });
        }
        Cmd::Conflicts { folder: f } => {
            let (raw, l): (_, SyncConflictsDto) = ask(&mut c, Request::SyncConflicts(folder(f))).await?;
            show(g, &raw, || conflicts(&l));
        }
        Cmd::Resolve { folder: f, copy, keep } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncResolve(SyncResolveParams { folder: f, copy, keep: keep.clone() })).await?;
            show(g, &raw, || format!("Resolved in {} (kept {keep}); the other version is in the folder's archive.", d.name));
        }
        Cmd::Address { folder: f, address } => {
            let (raw, d): (_, SyncFolderDto) = ask(&mut c, Request::SyncAddAddress(SyncAddAddressParams { folder: f, address: address.clone() })).await?;
            show(g, &raw, || format!("{} will try {address}.", d.name));
        }
        Cmd::Remove { folder: f, delete_files, yes } => remove(g, c, f, delete_files, yes).await?,
        Cmd::RemoveConfirm { token } => {
            let (raw, r): (_, SyncRemovedDto) = ask(&mut c, Request::SyncRemoveConfirm(RemoveConfirmParams { token })).await?;
            show(g, &raw, || removed(&r));
        }
        Cmd::Settings => {
            let (raw, s): (_, SyncSettingsDto) = ask(&mut c, Request::SyncSettings(Empty {})).await?;
            show(g, &raw, || settings(&s));
        }
        Cmd::SetSettings(p) => {
            let mut p = p;
            if let Some(loc) = p.default_location.take() {
                p.default_location = Some(if loc.trim().is_empty() { loc } else { abs(&loc)? });
            }
            let (raw, s): (_, SyncSettingsDto) = ask(&mut c, Request::SyncSetSettings(p)).await?;
            show(g, &raw, || settings(&s));
        }
    }
    Ok(())
}

async fn remove(g: &Global, mut c: trs_control::Client, f: String, delete_files: bool, yes: bool) -> R {
    let (raw, plan): (_, SyncRemovePlanDto) = ask(&mut c, Request::SyncRemove(SyncRemoveParams { folder: f, delete_files })).await?;
    if !yes {
        if g.json {
            println!("{}", json_text(&raw, true));
            eprintln!("seedwell: nothing removed yet; confirm within {} s with `seedwell sync remove-confirm <token>` or repeat with --yes", plan.expires_in_secs);
            return Ok(());
        }
        println!("{}", plan.summary);
        if !std::io::stdin().is_terminal() {
            return Err(CliError::plain(1, "not removing: no terminal to ask on; repeat with --yes to confirm"));
        }
        print!("Proceed? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled; nothing was removed.");
            return Ok(());
        }
    }
    let (raw, r): (_, SyncRemovedDto) = ask(&mut c, Request::SyncRemoveConfirm(RemoveConfirmParams { token: plan.token })).await?;
    show(g, &raw, || removed(&r));
    Ok(())
}

/// `seedwell relay` arguments.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RelayArgs {
    /// `--port` (default [`RELAY_PORT`]; 0 = any).
    pub port: Option<u16>,
    /// `--bind` (default all interfaces).
    pub bind: Option<String>,
    /// `--key-file`: where the relay's key is kept (its id is part of its address, so it must persist).
    pub key_file: Option<String>,
}

/// The relay's port unless `--port` says otherwise.
pub const RELAY_PORT: u16 = 22067;

/// `seedwell relay [--port N] [--bind ADDR] [--key-file PATH]`: a self-hosted relay for folder sync (SPEC 7G.3).
pub fn parse_relay(args_in: Vec<String>) -> Result<RelayArgs, Usage> {
    let mut o = Opts::new(args_in);
    let a = RelayArgs { port: o.number::<u16>(&["--port", "-p"])?, bind: o.value(&["--bind"])?, key_file: o.value(&["--key-file"])? };
    let pos = o.finish()?;
    if let Some(x) = pos.first() {
        return Err(Usage(format!("unexpected argument `{x}`; usage: seedwell relay [--port N] [--bind ADDR] [--key-file PATH]")));
    }
    Ok(a)
}

/// Where the relay keeps its key by default: next to Seedwell's state on macOS, `~/.local/state/seedwell`
/// elsewhere.
#[cfg(feature = "daemon")]
fn default_relay_key_file() -> R<std::path::PathBuf> {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).ok_or_else(|| CliError::plain(1, "HOME is not set; give --key-file"))?;
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Seedwell/relay.key")
    } else {
        home.join(".local/state/seedwell/relay.key")
    })
}

/// The relay's key: read from `path`, or made on first start and written there (0600).
#[cfg(feature = "daemon")]
fn relay_key(path: &std::path::Path) -> R<trs_sync_proto::device::DeviceKey> {
    use std::io::Read as _;
    use trs_sync_net::secrets::{FileSecrets, SecretStore};
    use trs_sync_proto::device::DeviceKey;
    let fail = |e: &dyn std::fmt::Display| CliError::plain(1, format!("relay key {}: {e}", path.display()));
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| fail(&e))?;
    }
    let store = FileSecrets::new(path.to_path_buf());
    if let Some(pkcs8) = store.get("relay").map_err(|e| fail(&e))? {
        return DeviceKey::from_pkcs8(&pkcs8).map_err(|e| fail(&format!("{e:?}")));
    }
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut seed)).map_err(|e| fail(&e))?;
    let k = DeviceKey::from_seed(&seed);
    store.put("relay", &k.to_pkcs8()).map_err(|e| fail(&e))?;
    Ok(k)
}

/// Run the relay until SIGINT or SIGTERM. It prints one line when it listens:
/// `seedwell relay ready address=relay://<ip:port>#<relay id> pid=<pid>`.
#[cfg(feature = "daemon")]
pub async fn relay(a: RelayArgs) -> R {
    use tokio::signal::unix::{SignalKind, signal};
    use trs_sync_net::RelayOptions;
    let key_file = match &a.key_file {
        Some(k) => std::path::PathBuf::from(k),
        None => default_relay_key_file()?,
    };
    let key = relay_key(&key_file)?;
    let bind: std::net::IpAddr = match a.bind.as_deref() {
        None => std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
        Some(b) => b.parse().map_err(|_| CliError::from(Usage(format!("--bind expects an IP address, got `{b}`"))))?,
    };
    let port = a.port.unwrap_or(RELAY_PORT);
    let listener = tokio::net::TcpListener::bind((bind, port)).await.map_err(|e| CliError::plain(1, format!("cannot listen on {bind}:{port}: {e}")))?;
    let local = listener.local_addr().map_err(|e| CliError::plain(1, e.to_string()))?;
    let opts = RelayOptions::new(key);
    println!("seedwell relay ready address={} pid={}", opts.url(&local.to_string()), std::process::id());
    let _ = std::io::stdout().flush();
    let mut term = signal(SignalKind::terminate()).map_err(|e| CliError::plain(1, e.to_string()))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| CliError::plain(1, e.to_string()))?;
    tokio::select! {
        _ = trs_sync_net::run_relay(listener, opts) => {}
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    eprintln!("seedwell relay: stopped");
    Ok(())
}

/// Built without the daemon (the thin client): no relay.
#[cfg(not(feature = "daemon"))]
pub async fn relay(_a: RelayArgs) -> R {
    Err(CliError::plain(1, "the sync relay is not available in this build (built without the `daemon` feature)"))
}

// ----------------------------------------------------------------------------- text

fn first_group(id: &str) -> &str {
    id.split('-').next().unwrap_or(id)
}

fn access(a: &str) -> &'static str {
    match a {
        "read_write" => "Read & write",
        "read_only" => "Read only",
        _ => "Encrypted",
    }
}

fn ago(t: Option<u64>) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    match t {
        None => "never".into(),
        Some(t) if t <= now => format!("{} ago", human::duration(now - t)),
        Some(_) => "just now".into(),
    }
}

/// `sync list`.
pub fn list(l: &SyncListDto) -> String {
    let mut o = String::new();
    let _ = writeln!(
        o,
        "This Mac: {}  (device {})  port {}{}",
        l.device_name,
        first_group(&l.device_id),
        l.port,
        l.external_port.map(|p| format!(", router port {p}")).unwrap_or_default()
    );
    if l.folders.is_empty() {
        let _ = write!(o, "No synced folders. Share one with `seedwell sync add <path>` or join one with `seedwell sync join <key>`.");
        return o;
    }
    let _ = writeln!(o, "\n{:<8} {:<20} {:<13} {:>10} {:>7}  NAME / PATH", "FOLDER", "STATE", "ACCESS", "TO FETCH", "DEVICES");
    for f in &l.folders {
        let _ = writeln!(
            o,
            "{:<8} {:<20} {:<13} {:>10} {:>3}/{:<3}  {}  {}",
            first_group(&f.id),
            f.state,
            access(&f.access),
            if f.need_bytes > 0 || f.need_files > 0 { human::bytes(f.need_bytes) } else { "-".into() },
            f.connected,
            f.members,
            f.name,
            f.path
        );
    }
    if !l.devices.is_empty() {
        let _ = writeln!(o, "\nCONNECTED DEVICES");
        for d in &l.devices {
            let _ = writeln!(o, "  {:<7}  {}  ({}, {} via {}, {} folders)", first_group(&d.id), d.name, d.client, d.address, d.via, d.folders.len());
        }
    }
    o.trim_end().to_owned()
}

/// `sync status`.
pub fn status(f: &SyncFolderDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "{}  {}", f.name, f.path);
    let _ = writeln!(o, "  id         {}", f.id);
    let _ = writeln!(o, "  state      {}{}", f.state, f.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default());
    let _ = writeln!(o, "  access     {}", access(&f.access));
    let _ = writeln!(o, "  devices    {} connected of {} known{}", f.connected, f.members, if f.lan_only { " (local network only)" } else { "" });
    if f.need_files > 0 || f.need_bytes > 0 {
        let _ = writeln!(o, "  to fetch   {} files, {}", f.need_files, human::bytes(f.need_bytes));
    }
    let _ = writeln!(o, "  in sync    {}", ago(f.last_synced_at));
    if f.conflicts > 0 {
        let _ = writeln!(o, "  conflicts  {}", f.conflicts);
    }
    if !f.issues.is_empty() {
        let _ = writeln!(o, "  not synced ({}):", f.issues.len());
        for i in f.issues.iter().take(20) {
            let _ = writeln!(o, "    {}: {}", i.path, i.reason);
        }
    }
    o.trim_end().to_owned()
}

/// `sync conflicts`.
pub fn conflicts(l: &SyncConflictsDto) -> String {
    if l.conflicts.is_empty() {
        return format!("No conflicts in {}.", l.name);
    }
    let mut o = String::new();
    let _ = writeln!(o, "{} conflict{} in {}:", l.conflicts.len(), if l.conflicts.len() == 1 { "" } else { "s" }, l.name);
    for c in &l.conflicts {
        let by = match (c.device.is_empty(), c.when.is_empty()) {
            (false, false) => format!("  ({}, {})", c.device, c.when),
            (false, true) => format!("  ({})", c.device),
            (true, false) => format!("  ({})", c.when),
            _ => String::new(),
        };
        let _ = writeln!(o, "  {}  <- {}{by}", c.copy, c.original);
    }
    let _ = write!(o, "Settle one with: seedwell sync resolve {} <copy> --keep original|copy|both", first_group(&l.id));
    o
}

/// `sync share`.
pub fn shares(s: &SyncSharesDto) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "Keys of {} ({}). Anyone with a key has the folder: send it only to people you trust.", s.name, first_group(&s.id));
    if let (Some(k), Some(l)) = (&s.read_write, &s.read_write_link) {
        let _ = writeln!(o, "\nRead & write\n  {k}\n  {l}");
    }
    if let (Some(k), Some(l)) = (&s.read_only, &s.read_only_link) {
        let _ = writeln!(o, "\nRead only\n  {k}\n  {l}");
    }
    o.trim_end().to_owned()
}

/// `sync settings`.
pub fn settings(s: &SyncSettingsDto) -> String {
    let lim = |l: Option<u64>| l.map_or_else(|| "unlimited".to_owned(), human::rate);
    let yn = |b: bool| if b { "on" } else { "off" };
    let mut o = String::new();
    if !s.changed.is_empty() {
        let _ = writeln!(o, "changed: {}{}", s.changed.join(", "), if s.note.is_empty() { String::new() } else { format!(" ({})", s.note) });
    }
    let _ = writeln!(o, "device name       {}", if s.device_name.is_empty() { "(the computer's name)" } else { &s.device_name });
    let _ = writeln!(o, "port              {}", if s.port == 0 { "automatic".to_owned() } else { s.port.to_string() });
    let _ = writeln!(o, "find on the LAN   {}", yn(s.lan_discovery));
    let _ = writeln!(o, "find on Internet  {}", yn(s.dht_discovery));
    let _ = writeln!(o, "relay own devices {}   relay everyone {}", yn(s.relay_own_devices), yn(s.relay_everyone));
    let _ = writeln!(o, "Internet limits   down {}, up {} (the LAN is never limited)", lim(s.down_limit), lim(s.up_limit));
    let _ = write!(o, "joined folders go {}", if s.default_location.is_empty() { "~/Seedwell" } else { &s.default_location });
    o
}

fn removed(r: &SyncRemovedDto) -> String {
    match &r.trashed_to {
        Some(t) => format!("{} no longer syncs on this Mac; the folder is in the Trash ({t}).", r.name),
        None => format!("{} no longer syncs on this Mac; its files stay in {}.", r.name, r.path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(s: &str) -> Vec<String> {
        crate::cmd::shell_words(s)
    }

    #[test]
    fn parses_every_sync_command() {
        assert_eq!(parse(w("list")).unwrap(), Cmd::List);
        assert_eq!(parse(w("status MFZWI3D")).unwrap(), Cmd::Status { folder: "MFZWI3D".into() });
        assert_eq!(parse(w("add ~/Documents/Shared")).unwrap(), Cmd::Add { path: "~/Documents/Shared".into() });
        assert_eq!(parse(w("join SWR-ABC --to /tmp/x")).unwrap(), Cmd::Join { key: "SWR-ABC".into(), to: Some("/tmp/x".into()) });
        assert_eq!(parse(w("join -")).unwrap(), Cmd::Join { key: "-".into(), to: None });
        assert_eq!(parse(w("share Docs")).unwrap(), Cmd::Share { folder: "Docs".into() });
        assert_eq!(parse(w("rename Docs 'Work Docs'")).unwrap(), Cmd::Rename { folder: "Docs".into(), name: "Work Docs".into() });
        assert_eq!(parse(w("move Docs /tmp/y")).unwrap(), Cmd::Move { folder: "Docs".into(), path: "/tmp/y".into() });
        assert_eq!(parse(w("pause Docs")).unwrap(), Cmd::Pause { folder: "Docs".into() });
        assert_eq!(parse(w("resume Docs")).unwrap(), Cmd::Resume { folder: "Docs".into() });
        assert_eq!(parse(w("rescan Docs")).unwrap(), Cmd::Rescan { folder: "Docs".into() });
        assert_eq!(parse(w("lan-only Docs on")).unwrap(), Cmd::LanOnly { folder: "Docs".into(), on: true });
        assert_eq!(parse(w("lan-only Docs off")).unwrap(), Cmd::LanOnly { folder: "Docs".into(), on: false });
        assert_eq!(parse(w("conflicts Docs")).unwrap(), Cmd::Conflicts { folder: "Docs".into() });
        assert_eq!(
            parse(w("resolve Docs 'a (Mac, 2026-10-25 14.32).txt' --keep both")).unwrap(),
            Cmd::Resolve { folder: "Docs".into(), copy: "a (Mac, 2026-10-25 14.32).txt".into(), keep: "both".into() }
        );
        assert_eq!(parse(w("address Docs 192.168.1.20:22000")).unwrap(), Cmd::Address { folder: "Docs".into(), address: "192.168.1.20:22000".into() });
        assert_eq!(parse(w("remove Docs --delete-files -y")).unwrap(), Cmd::Remove { folder: "Docs".into(), delete_files: true, yes: true });
        assert_eq!(parse(w("remove-confirm tok")).unwrap(), Cmd::RemoveConfirm { token: "tok".into() });
        assert_eq!(parse(w("settings")).unwrap(), Cmd::Settings);
        let Cmd::SetSettings(p) = parse(w("settings --device-name 'Studio Mac' --lan off --up 1MB/s --down unlimited")).unwrap() else { panic!() };
        assert_eq!((p.device_name.as_deref(), p.lan_discovery, p.up_limit, p.down_limit), (Some("Studio Mac"), Some(false), Some(1_000_000), Some(0)));
        for bad in ["", "frob", "status", "status a b", "rename Docs", "move Docs", "address Docs", "remove", "settings --lan maybe", "settings extra", "list x", "join", "lan-only Docs", "lan-only Docs maybe", "conflicts", "resolve Docs x", "resolve Docs x --keep neither", "resolve Docs --keep copy"] {
            assert!(parse(w(bad)).is_err(), "`sync {bad}` should be a usage error");
        }
    }

    #[test]
    fn relay_parses() {
        assert_eq!(parse_relay(w("")).unwrap(), RelayArgs::default());
        assert_eq!(parse_relay(w("--port 22067")).unwrap().port, Some(22067));
        assert_eq!(parse_relay(w("--bind 127.0.0.1 --key-file /tmp/k")).unwrap(), RelayArgs { port: None, bind: Some("127.0.0.1".into()), key_file: Some("/tmp/k".into()) });
        assert!(parse_relay(w("--port x")).is_err());
        assert!(parse_relay(w("extra")).is_err());
    }

    #[test]
    fn texts_never_hide_what_matters() {
        let s = SyncSharesDto { id: "MFZWI3D-X".into(), name: "Docs".into(), read_only: Some("SWR-K".into()), read_only_link: Some("seedwell://sync/SWR-K".into()), ..Default::default() };
        let t = shares(&s);
        assert!(t.contains("SWR-K") && !t.contains("Read & write"), "{t}");
        let l = SyncListDto { device_name: "Mac".into(), device_id: "AAAAAAA-B".into(), port: 22000, ..Default::default() };
        assert!(list(&l).contains("No synced folders"));
    }
}
