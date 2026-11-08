//! Folder sync (SPEC 7G) next to the engine (feature `sync`).
//!
//! * [`start_sync`]: the sync service for a Host, with the engine's DHT for discovery ([`DhtDiscovery`])
//!   and its port mapper for the sync port (TCP and UDP). The engine lends sync these two things only.
//! * [`SyncSlot`]: where the control server finds the running service (set once it has started, cleared
//!   before it stops); the `sync.*` methods answer "folder sync is not running" while it is empty. It also
//!   moves the service to another port ([`SyncSlot::move_port`]).
//! * The mapping from the service's status to the protocol's DTOs, folder selection, the Trash.

use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use seedwell_platform::fs::{OtherVolume, TrashError, TrashPlace, Trashed};

use serde_json::json;
use seedwell_engine::EngineHandle;
use seedwell_sync_net::secrets::SecretStore;
use seedwell_sync_net::service::BoxFuture;
use seedwell_sync_net::status::{FolderState, FolderStatus, Snapshot};
use seedwell_sync_net::{Options, PublicDiscovery, SyncError, SyncHandle};
use seedwell_sync_proto::ids::FolderId;
use seedwell_sync_proto::keys::Access;
use seedwell_sync_proto::members::{InviteLink, Role};

use crate::ops;
use crate::proto::{ApiError, ErrorCode, clip};
use crate::redact::scrub_keys;
use crate::types::*;

/// Longest a DHT announce or lookup for sync is followed.
pub const LOOKUP_SECS: u64 = 30;
/// Most addresses one lookup returns.
pub const MAX_PEERS: usize = 200;

/// Where the control server finds the running sync service. Cheap to clone; the app or the daemon sets
/// it once the service has started and clears it before stopping it.
#[derive(Clone, Default)]
pub struct SyncSlot {
    handle: Arc<RwLock<Option<SyncHandle>>>,
    /// Starts the service again as it was started (set by whoever started it): a port change restarts it.
    starter: Arc<RwLock<Option<Starter>>>,
    /// One port change at a time.
    moving: Arc<tokio::sync::Mutex<()>>,
}

/// Starts the sync service with the options it was first started with (the port comes from `sync.toml`).
pub type Starter = Arc<dyn Fn() -> BoxFuture<'static, io::Result<SyncHandle>> + Send + Sync>;

/// Why the sync port did not change.
#[derive(Debug, PartialEq, Eq)]
pub enum PortChange {
    /// Below 1024.
    Invalid,
    /// Another app has the port (TCP or UDP).
    InUse(u16),
    /// No service runs, or it was not started through this slot.
    NotRunning,
    /// The service stopped and did not start again.
    Failed(String),
}

impl std::fmt::Display for PortChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortChange::Invalid => write!(f, "Use a port from 1024 to 65535."),
            PortChange::InUse(p) => write!(f, "Port {p} is in use by another app."),
            PortChange::NotRunning => write!(f, "Folder sync is not running."),
            PortChange::Failed(e) => write!(f, "Folder sync could not start again: {e}"),
        }
    }
}

/// Whether `port` is free for sync on this Mac: TCP and its UDP twin, on every address (IPv4 and IPv6).
pub fn port_free(port: u16) -> bool {
    use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, UdpSocket};
    let taken = |r: io::Result<()>| r.is_err_and(|e| e.kind() == io::ErrorKind::AddrInUse);
    let ips: [std::net::IpAddr; 2] = [Ipv4Addr::UNSPECIFIED.into(), Ipv6Addr::UNSPECIFIED.into()];
    !ips.iter().any(|&ip| {
        let a = SocketAddr::new(ip, port);
        taken(TcpListener::bind(a).map(drop)) || taken(UdpSocket::bind(a).map(drop))
    })
}

impl SyncSlot {
    /// An empty slot (sync not running).
    pub fn new() -> Self {
        Self::default()
    }

    /// A slot holding `h`.
    pub fn with(h: SyncHandle) -> Self {
        let s = Self::new();
        s.set(Some(h));
        s
    }

    /// Put the service in (or take it out with `None`).
    pub fn set(&self, h: Option<SyncHandle>) {
        *self.handle.write().unwrap_or_else(|e| e.into_inner()) = h;
    }

    /// The service, when it runs.
    pub fn get(&self) -> Option<SyncHandle> {
        self.handle.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How to start the service again (for [`SyncSlot::move_port`]).
    pub fn set_starter(&self, s: Starter) {
        *self.starter.write().unwrap_or_else(|e| e.into_inner()) = Some(s);
    }

    /// Moves folder sync to `port` (0: a free one) now: the port is saved, the service stops and starts again
    /// on it (folders, channels and sessions come back in a few seconds; devices find the new port as they
    /// find any address). A port another app holds is refused and nothing changes. Returns the port in use.
    pub async fn move_port(&self, port: u16) -> Result<u16, PortChange> {
        if port != 0 && port < 1024 {
            return Err(PortChange::Invalid);
        }
        let _one = self.moving.lock().await;
        let h = self.get().ok_or(PortChange::NotRunning)?;
        if port != 0 && h.port() == port {
            return Ok(port);
        }
        let start = self.starter.read().unwrap_or_else(|e| e.into_inner()).clone().ok_or(PortChange::NotRunning)?;
        if port != 0 && !port_free(port) {
            return Err(PortChange::InUse(port));
        }
        let mut st = h.settings();
        st.port = port;
        h.set_settings(st).map_err(|e| PortChange::Failed(e.to_string()))?;
        self.set(None);
        h.shutdown().await;
        drop(h);
        let h = start().await.map_err(|e| PortChange::Failed(e.to_string()))?;
        let now = h.port();
        self.set(Some(h));
        Ok(now)
    }
}

/// [`PublicDiscovery`] over the engine's Mainline DHT. The DHT node is asked for at every lookup (it moves
/// to a new socket when the listen port changes, and may be switched off); while it is off, lookups find
/// nothing. Topics are 20-byte public hashes, never a torrent's info-hash.
pub struct DhtDiscovery {
    engine: EngineHandle,
}

impl DhtDiscovery {
    /// Over `engine`'s DHT.
    pub fn new(engine: EngineHandle) -> Self {
        Self { engine }
    }
}

impl PublicDiscovery for DhtDiscovery {
    fn announce(&self, topic: [u8; 20], port: u16) -> BoxFuture<'_, Vec<SocketAddr>> {
        Box::pin(async move {
            // Peers on the Internet must be given the port the router opened, when it opened another one.
            let port = self.engine.reachability().portmap.and_then(|s| s.extra_external_tcp_port).filter(|&p| p != 0).unwrap_or(port);
            lookup(self.engine.public_dht().await, topic, Some(port)).await
        })
    }

    fn lookup(&self, topic: [u8; 20]) -> BoxFuture<'_, Vec<SocketAddr>> {
        Box::pin(async move { lookup(self.engine.public_dht().await, topic, None).await })
    }

    fn public_address(&self) -> Option<SocketAddr> {
        // What the router said it opened for sync's port, at its public address (not one behind a carrier's NAT).
        let pm = self.engine.reachability().portmap?;
        let ip = pm.external_ip.filter(|_| !pm.behind_cgnat)?;
        let port = pm.extra_external_tcp_port.filter(|&p| p != 0)?;
        Some(SocketAddr::new(ip.into(), port))
    }
}

/// Announce `port` under `topic` (or only look it up) and collect the peers found, for at most
/// [`LOOKUP_SECS`] and [`MAX_PEERS`] addresses.
pub async fn lookup(dht: Option<seedwell_dht::DhtHandle>, topic: [u8; 20], announce: Option<u16>) -> Vec<SocketAddr> {
    collect(dht, topic, announce, Duration::from_secs(LOOKUP_SECS), MAX_PEERS).await
}

async fn collect(dht: Option<seedwell_dht::DhtHandle>, topic: [u8; 20], announce: Option<u16>, within: Duration, cap: usize) -> Vec<SocketAddr> {
    let Some(dht) = dht else { return Vec::new() };
    // A sync topic is public by construction (a daily hash only members can work out).
    let Some(h) = seedwell_dht::PublicInfoHash::new(topic, false) else { return Vec::new() };
    let deadline = tokio::time::Instant::now() + within;
    let started = match announce {
        Some(port) => tokio::time::timeout_at(deadline, dht.announce(h, port)).await,
        None => tokio::time::timeout_at(deadline, dht.get_peers(h)).await,
    };
    let Ok(Ok(Ok(mut stream))) = started else { return Vec::new() };
    let mut out: Vec<SocketAddr> = Vec::new();
    while let Ok(Some(u)) = tokio::time::timeout_at(deadline, stream.next()).await {
        if let seedwell_dht::LookupUpdate::Peers(v) = u {
            for p in v {
                let a = SocketAddr::V4(p);
                if out.len() < cap && !out.contains(&a) {
                    out.push(a);
                }
            }
            // A lookup may stop at the cap; an announce runs on to its end (the stream closes after it).
            if announce.is_none() && out.len() >= cap {
                break;
            }
        }
    }
    out
}

/// The engine's torrents, for folder sync ([`seedwell_sync_net::LocalTorrents`]): a folder's files are never moved from under
/// a torrent, and a torrent missing its files looks again after a folder's files moved (they may be back).
struct EngineTorrents(EngineHandle);

/// Longest the engine is asked which torrents use a folder, and their paths are resolved on the disk.
const TORRENTS_WAIT: Duration = Duration::from_secs(10);

impl seedwell_sync_net::LocalTorrents for EngineTorrents {
    fn using(&self, path: &Path) -> BoxFuture<'static, Result<Vec<String>, String>> {
        let (e, path) = (self.0.clone(), path.to_path_buf());
        Box::pin(async move {
            let late = || "the torrents did not answer in time".to_string();
            let end = tokio::time::Instant::now() + TORRENTS_WAIT;
            let places = tokio::time::timeout_at(end, e.torrent_places()).await.map_err(|_| late())?.map_err(|e| e.to_string())?;
            // Resolving reads the disk (links, the case names have there): off the engine's actor and the caller's task.
            tokio::time::timeout_at(end, tokio::task::spawn_blocking(move || torrents_using(&places, &path)))
                .await
                .map_err(|_| late())?
                .map_err(|e| e.to_string())
        })
    }

    fn moved(&self) {
        // Every folder torrents save to is looked at once, off the engine's actor; one found again recovers.
        self.0.volumes_changed();
    }
}

/// The names of the torrents with files at or below `path`, or whose folder holds it: where they are now, their staging
/// folders, and where a move puts them ([`seedwell_engine::TorrentPlaces`]); a torrent on its way to a place not known yet
/// counts too. Both sides are compared as the disk names them ([`seedwell_platform::dirs::real_path`]: a folder added
/// through a link, `~/Movies` → `/Volumes/Media/Movies`, is the folder a torrent saves to), and without regard to case
/// on a volume that finds names in any case ([`seedwell_platform::fs::folds_case`]). Reads the disk: off any actor.
fn torrents_using(torrents: &[seedwell_engine::TorrentPlaces], path: &Path) -> Vec<String> {
    let real = |p: &Path| seedwell_platform::dirs::real_path(p).unwrap_or_else(|| p.to_path_buf());
    let folder = real(path);
    let fold = seedwell_platform::fs::folds_case(&folder);
    let names = |p: &Path| -> Vec<String> {
        p.components()
            .map(|c| {
                let n = c.as_os_str().to_string_lossy();
                if fold { n.to_lowercase() } else { n.into_owned() }
            })
            .collect()
    };
    let within = |a: &[String], b: &[String]| a.len() >= b.len() && a[..b.len()] == *b;
    let f = names(&folder);
    torrents
        .iter()
        .filter(|t| {
            t.moving_somewhere
                || t.paths.iter().filter(|p| !p.as_os_str().is_empty()).any(|p| {
                    let at = names(&real(p));
                    within(&at, &f) || within(&f, &at)
                })
        })
        .map(|t| t.name.clone())
        .collect()
}

/// The computer's name (`scutil --get ComputerName` on macOS, the host name elsewhere), for the device
/// name when none is set.
pub fn default_device_name() -> String {
    let run = |cmd: &str, args: &[&str]| {
        std::process::Command::new(cmd)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let name = if cfg!(target_os = "macos") { run("scutil", &["--get", "ComputerName"]) } else { None };
    let name = name.or_else(|| run("hostname", &[]));
    name.map(|n| n.chars().filter(|c| !c.is_control()).take(64).collect())
        .unwrap_or_else(|| if cfg!(target_os = "macos") { "Mac".into() } else { "Seedwell".into() })
}

/// Start the sync service of a Host: `<state_dir>/sync.toml` and `<state_dir>/sync/`, keys in `secrets`,
/// devices found over `engine`'s DHT (when given), and the sync port mapped by its port mapper.
/// `client` is `Seedwell <version>`, as everywhere else.
pub async fn start_sync(state_dir: &Path, client: String, secrets: Arc<dyn SecretStore>, engine: Option<&EngineHandle>) -> io::Result<SyncHandle> {
    start_sync_with(state_dir, client, secrets, engine, |_| {}).await
}

/// [`start_sync`] with the options adjusted last (tests bind to loopback and switch LAN discovery off).
pub async fn start_sync_with(
    state_dir: &Path,
    client: String,
    secrets: Arc<dyn SecretStore>,
    engine: Option<&EngineHandle>,
    tweak: impl FnOnce(&mut Options),
) -> io::Result<SyncHandle> {
    let mut opts = Options::new(state_dir.to_path_buf(), client, secrets);
    opts.default_device_name = default_device_name();
    if let Some(e) = engine {
        opts.discovery = Some(Arc::new(DhtDiscovery::new(e.clone())));
        opts.torrents = Some(Arc::new(EngineTorrents(e.clone())));
    }
    tweak(&mut opts);
    let h = SyncHandle::start(opts).await?;
    if let Some(e) = engine {
        e.set_extra_mapped_port(Some(h.port()));
    }
    Ok(h)
}

// ----------------------------------------------------------------------------- protocol mapping

/// The answer while no service runs.
pub fn not_running() -> ApiError {
    ApiError::new(ErrorCode::Unsupported, "folder sync is not running")
}

/// A refusal of the service in protocol form (keys masked: a pasted key may be quoted back).
pub fn err(e: SyncError) -> ApiError {
    match e {
        SyncError::NotFound => ApiError::not_found("no such synced folder (see `seedwell sync list`)"),
        SyncError::Invalid(why) | SyncError::OnDemand(_, why) => ApiError::invalid(clip(&scrub_keys(&why), 300)),
        SyncError::Io(e) => ApiError::new(ErrorCode::Engine, clip(&scrub_keys(&e.to_string()), 300)),
    }
}

/// The state name on the wire.
pub fn state_name(s: &FolderState) -> &'static str {
    match s {
        FolderState::Starting => "starting",
        FolderState::UpToDate => "up-to-date",
        FolderState::Scanning => "scanning",
        FolderState::Syncing => "syncing",
        FolderState::WaitingForDevices => "waiting-for-devices",
        FolderState::WaitingForAdmin => "waiting-for-admin",
        FolderState::Paused => "paused",
        FolderState::Missing => "missing",
        FolderState::Error => "error",
    }
}

/// The access name on the wire.
pub fn access_name(a: Access) -> &'static str {
    match a {
        Access::Write => "read_write",
        Access::Read => "read_only",
        Access::Storage => "encrypted",
    }
}

/// The role name on the wire.
pub fn role_name(r: Role) -> &'static str {
    match r {
        Role::Owner => "owner",
        Role::Admin => "admin",
        Role::ReadWrite => "read_write",
        Role::ReadOnly => "read_only",
    }
}

/// A role from its wire name (also `rw`, `ro`, `read-write`, `read-only`).
pub fn parse_role(s: &str) -> Result<Role, ApiError> {
    match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
        "owner" => Ok(Role::Owner),
        "admin" => Ok(Role::Admin),
        "read_write" | "rw" => Ok(Role::ReadWrite),
        "read_only" | "ro" => Ok(Role::ReadOnly),
        _ => Err(ApiError::params("`role` is owner, admin, read_write or read_only")),
    }
}

/// One invitation in protocol form.
pub fn invite_dto(i: &seedwell_sync_net::InviteInfo) -> SyncInviteDto {
    SyncInviteDto {
        id: i.id.clone(),
        link: i.link.clone(),
        role: role_name(i.role).into(),
        max_uses: i.max_uses,
        uses: i.uses,
        expires_at: i.expires_s,
        created_at: i.created_s,
        revoked: i.revoked,
        channel_id: i.channel_id.clone(),
        channel_name: i.channel_name.as_deref().map(|n| clip(&ops::clean_name(n), 100)),
    }
}

/// One member in protocol form.
pub fn member_dto(m: &seedwell_sync_net::MemberInfo) -> SyncMemberDto {
    SyncMemberDto {
        device_id: m.device.to_string(),
        name: clip(&ops::clean_name(&m.name), 100),
        user: clip(&ops::clean_name(&m.user), 100),
        role: role_name(m.role).into(),
        online: m.online,
        me: m.is_me,
        expires_at: m.expires_s,
        removal: m.removal.as_ref().map(|a| match a {
            seedwell_sync_net::removals::Asked::Waiting => "waiting".into(),
            seedwell_sync_net::removals::Asked::Refused(_) => "refused".into(),
        }),
        removal_reason: match &m.removal {
            Some(seedwell_sync_net::removals::Asked::Refused(why)) => Some(clip(why, 300)),
            _ => None,
        },
    }
}

/// The member `q` names: its device id, an unambiguous prefix (4+ characters), its name or its user's (any case).
pub fn resolve_member<'a>(members: &'a [seedwell_sync_net::MemberInfo], q: &str) -> Result<&'a seedwell_sync_net::MemberInfo, ApiError> {
    let q = q.trim();
    if q.is_empty() || q.len() > 200 {
        return Err(ApiError::params("`device` is a device id, a prefix of it, or the device's name"));
    }
    let nq = norm_id(q);
    let lower = q.to_lowercase();
    let hits: Vec<&seedwell_sync_net::MemberInfo> = members
        .iter()
        .filter(|m| {
            let id = norm_id(&m.device.to_string());
            id == nq || (nq.len() >= MIN_FOLDER_PREFIX && id.starts_with(&nq)) || m.name.to_lowercase() == lower || (!m.user.is_empty() && m.user.to_lowercase() == lower)
        })
        .collect();
    match hits.as_slice() {
        [] => Err(ApiError::not_found(format!("no device `{}` in this folder (see `seedwell sync members`)", clip(q, 60)))),
        [one] => Ok(one),
        many => Err(ApiError::new(ErrorCode::Ambiguous, format!("`{}` matches {} devices; use more of the id", clip(q, 60), many.len()))),
    }
}

/// One folder in protocol form.
pub fn folder_dto(f: &FolderStatus) -> SyncFolderDto {
    SyncFolderDto {
        id: f.id.clone(),
        name: clip(&ops::clean_name(&f.name), 200),
        path: f.path.clone(),
        access: access_name(f.access).into(),
        role: f.role.map(|r| role_name(r).into()),
        share_until: f.share_until,
        state: state_name(&f.state).into(),
        files: f.files,
        bytes: f.bytes,
        need_files: f.need_files,
        need_bytes: f.need_bytes,
        rate_in: f.rate_in,
        eta_secs: f.eta_secs,
        rate_out: f.rate_out,
        connected: f.connected,
        members: f.members,
        conflicts: f.conflicts,
        lan_only: f.lan_only,
        issues: f.issues.iter().take(200).map(|(p, r)| SyncIssueDto { path: clip(p, 400), reason: clip(r, 200) }).collect(),
        error: f.error.as_deref().map(|e| clip(&scrub_keys(e), 300)),
        last_synced_at: f.last_synced_s,
        channel_id: f.channel_id.clone(),
        on_demand: f.on_demand,
        on_demand_others: f.on_demand_others,
        fetching: None,
        unavailable: f.unavailable,
        progress: f.progress.as_deref().map(|t| clip(t, 300)),
        problem: f.problem.as_deref().map(|t| clip(&scrub_keys(t), 300)),
        join_attempts: f.join_attempts.iter().map(attempt_dto).collect(),
        waiting: f.waiting.as_deref().map(|t| clip(t, 400)),
        gone: f.gone.iter().map(gone_dto).collect(),
        can_reshare: f.reshare,
    }
}

/// A gone member in protocol form.
pub fn gone_dto(d: &seedwell_sync_net::status::GoneDevice) -> GoneDeviceDto {
    GoneDeviceDto { device_id: d.id.clone(), name: clip(&ops::clean_name(&d.name), 100), last_seen_at: d.last_seen_s }
}

/// A refused join attempt in protocol form.
pub fn attempt_dto(a: &seedwell_sync_net::status::JoinAttemptStatus) -> JoinAttemptDto {
    JoinAttemptDto {
        at: a.at_s,
        device_id: a.device_id.clone(),
        name: clip(&ops::clean_name(&a.name), 100),
        client: clip(&ops::clean_name(&a.client), 60),
        reason: clip(&scrub_keys(&a.reason), 300),
    }
}

/// One conflict copy in protocol form; the device and time come from the copy's name.
pub fn conflict_dto(copy: &str, original: &str) -> SyncConflictDto {
    let (device, when) = seedwell_sync::conflict::parse_conflict(copy).map(|c| (c.device, c.stamp)).unwrap_or_default();
    SyncConflictDto { copy: clip(copy, 400), original: clip(original, 400), device: clip(&ops::clean_name(&device), 100), when: clip(&when, 40) }
}

/// `sync.list` in protocol form.
pub fn list_dto(s: &Snapshot, external_port: Option<u16>) -> SyncListDto {
    SyncListDto {
        device_id: s.device_id.clone(),
        device_name: clip(&ops::clean_name(&s.device_name), 100),
        port: s.port,
        external_port,
        folders: s.folders.iter().map(folder_dto).collect(),
        devices: s
            .devices
            .iter()
            .map(|d| SyncDeviceDto {
                id: d.id.clone(),
                name: clip(&ops::clean_name(&d.name), 100),
                client: clip(&ops::clean_name(&d.client), 60),
                address: d.address.clone(),
                via: d.via.clone(),
                folders: d.folders.clone(),
                rx_bytes: d.rx_bytes,
                tx_bytes: d.tx_bytes,
            })
            .collect(),
        offline_devices: s
            .offline
            .iter()
            .map(|d| SyncOfflineDeviceDto {
                id: d.id.clone(),
                name: clip(&ops::clean_name(&d.name), 100),
                last_seen_at: d.last_seen_s,
                last_problem: d.problem.as_deref().map(|p| clip(p, 300)),
                folders: d.folders.clone(),
            })
            .collect(),
    }
}

/// The id text without dashes and spaces, in capitals, with `0`, `1`, `8` read as `O`, `I`, `B` (as ids
/// are parsed).
fn norm_id(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| match c.to_ascii_uppercase() {
            '0' => 'O',
            '1' => 'I',
            '8' => 'B',
            c => c,
        })
        .collect()
}

/// Shortest id prefix a folder can be named by.
pub const MIN_FOLDER_PREFIX: usize = 4;

/// The folder `q` names: its id, an unambiguous prefix of it (4+ characters; the first group is enough), or
/// its name on this Mac (any case). Several matches are an error that lists them; nothing is guessed.
pub fn resolve_folder<'a>(folders: &'a [FolderStatus], q: &str) -> Result<&'a FolderStatus, ApiError> {
    let q = q.trim();
    if q.is_empty() || q.len() > 200 {
        return Err(ApiError::params("`folder` is a folder id, a prefix of it, or the folder's name"));
    }
    let nq = norm_id(q);
    if let Some(f) = folders.iter().find(|f| !nq.is_empty() && norm_id(&f.id) == nq) {
        return Ok(f);
    }
    let lower = q.to_lowercase();
    let mut hits: Vec<&FolderStatus> = folders.iter().filter(|f| f.name.to_lowercase() == lower).collect();
    if nq.len() >= MIN_FOLDER_PREFIX && nq.len() == q.chars().filter(|c| *c != '-').count() {
        for f in folders.iter().filter(|f| norm_id(&f.id).starts_with(&nq)) {
            if !hits.iter().any(|h| h.id == f.id) {
                hits.push(f);
            }
        }
    }
    match hits.as_slice() {
        [] => Err(ApiError::not_found(format!("no synced folder `{}` (see `seedwell sync list`)", clip(q, 60)))),
        [one] => Ok(one),
        many => {
            let cands: Vec<_> = many.iter().take(8).map(|f| json!({"id": f.id, "name": f.name})).collect();
            Err(ApiError::new(ErrorCode::Ambiguous, format!("`{}` matches {} folders; use more of the id", clip(q, 60), many.len()))
                .with_data(json!({ "candidates": cands })))
        }
    }
}

/// A folder's id as the service takes it.
pub fn folder_id(f: &FolderStatus) -> Result<FolderId, ApiError> {
    f.id.parse().map_err(|_| ApiError::internal("the service returned a malformed folder id"))
}

// ----------------------------------------------------------------------------- paths and the Trash

fn is_test_binary() -> bool {
    std::env::current_exe().ok().and_then(|p| p.parent().and_then(|d| d.file_name()).map(|n| n == "deps")).unwrap_or(false)
}

/// Whether the real-home guard is on: under `cargo test`, or with `SEEDWELL_FORBID_REAL_HOME`.
fn guard_active() -> bool {
    (cfg!(test) || is_test_binary() || std::env::var_os("SEEDWELL_FORBID_REAL_HOME").is_some()) && std::env::var_os("SEEDWELL_ALLOW_REAL_HOME").is_none()
}

/// The engine's real-home guard (`seedwell_engine::guard`), for sync's paths: under `cargo test`, or with
/// `SEEDWELL_FORBID_REAL_HOME`, nothing inside the real home folder is synced, moved or trashed.
fn guard(what: &str, p: &Path) -> Result<(), ApiError> {
    if !guard_active() {
        return Ok(());
    }
    let Some(home) = seedwell_platform::dirs::home().filter(|h| h.is_absolute() && h.parent().is_some()) else { return Ok(()) };
    // The temporary folder is inside the home folder on Windows.
    if p.starts_with(&home) && !seedwell_platform::dirs::in_temp_folder(p) {
        return Err(ApiError::invalid(format!(
            "refusing to use {what} {} inside the real home directory while running under test (set HOME to a temporary folder)",
            p.display()
        )));
    }
    Ok(())
}

/// A folder path from a caller: absolute or `~/...`, never a system, Library or hidden folder, and never
/// the real home under test.
pub fn folder_path(raw: &str, what: &str) -> Result<PathBuf, ApiError> {
    let p = ops::clean_path(raw, what)?;
    if ops::is_sensitive_dir(&p) {
        return Err(ApiError::invalid(format!("`{what}` points at a system, Library or hidden folder (or the home folder itself); choose a normal folder")));
    }
    guard("the folder", &p)?;
    Ok(p)
}

/// One path component from a folder name or a link's name hint; `None` when nothing usable is left.
fn component(name: &str) -> Option<String> {
    let n: String = seedwell_platform::fs::clean_name(name).chars().take(100).collect();
    let n = n.trim().trim_start_matches('.').trim().to_owned();
    (!n.is_empty()).then_some(n)
}

/// `dir/name`, or `dir/name 2`, `dir/name 3`, … when it is taken.
fn unique_in(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if std::fs::symlink_metadata(&first).is_err() {
        return first;
    }
    for i in 2..10_000 {
        let p = dir.join(format!("{name} {i}"));
        if std::fs::symlink_metadata(&p).is_err() {
            return p;
        }
    }
    dir.join(format!("{name} {}", std::process::id()))
}

/// Where a folder joined without a path goes: the default location for joined folders (empty = `~/Seedwell`),
/// under the link's name hint (else the first group of the folder id), with a number when that is taken.
pub fn default_join_path(default_location: &str, key_or_link: &str) -> Result<PathBuf, ApiError> {
    let link = InviteLink::parse(key_or_link).map_err(|e| ApiError::invalid(e.to_string()))?;
    let base = if default_location.trim().is_empty() { "~/Seedwell" } else { default_location };
    let base = folder_path(base, "the default location")?;
    let id = link.invite.folder_id().to_string();
    let name = link.name.as_deref().and_then(component).unwrap_or_else(|| id.split('-').next().unwrap_or("Folder").to_owned());
    Ok(unique_in(&base, &name))
}

/// Where a removed folder would go in the Trash (`trash_dir`, else the system's), checked before anything is
/// removed: a Trash folder (the setting, Linux's home trash) must be on the folder's volume, so the move is a rename,
/// and a folder on another disk is refused (it would have to be copied); the Mac's own Trash keeps a folder in the
/// Trash of its own volume; the Recycle Bin refuses a folder it would delete outright instead of keeping.
pub fn trash_target(folder: &Path, trash_dir: Option<&Path>) -> Result<Trashed, ApiError> {
    match trash_dir.map(|t| TrashPlace::Folder(t.to_path_buf())).or_else(seedwell_platform::fs::system_trash) {
        Some(TrashPlace::Folder(t)) => guard(concat!("the ", seedwell_platform::trash!()), &t)?,
        Some(TrashPlace::RecycleBin | TrashPlace::VolumeTrash) if guard_active() => {
            return Err(ApiError::invalid(concat!("refusing to use the ", seedwell_platform::trash!(), " while running under test (set a Trash folder)")));
        }
        Some(TrashPlace::RecycleBin | TrashPlace::VolumeTrash) => {}
        None => return Err(trash_error(folder, TrashError::NoTrash)),
    }
    guard("the folder", folder)?;
    seedwell_platform::fs::trash_check(folder, trash_dir, OtherVolume::Refuse).map_err(|e| trash_error(folder, e))
}

/// Move `folder` to the Trash, after [`trash_target`] said it can go.
pub fn move_to_trash(folder: &Path, trash_dir: Option<&Path>) -> Result<Trashed, TrashError> {
    seedwell_platform::fs::move_to_trash(folder, trash_dir, OtherVolume::Refuse)
}

/// What stopping a folder on this computer leaves the other devices: `only_here` is how many of its files no other
/// device is known to hold ([`seedwell_sync_net::SyncHandle::only_here`]).
pub fn others_after_removal(only_here: u64) -> String {
    match only_here {
        0 => "Other devices keep their copy.".into(),
        1 => concat!("1 file exists only on this ", seedwell_platform::computer!(), "; other devices lose access to it.").into(),
        n => format!(concat!("{} files exist only on this ", seedwell_platform::computer!(), "; other devices lose access to them."), n),
    }
}

/// " (where)" for a folder moved to a Trash folder; nothing for the Recycle Bin, which has no path.
pub fn trash_shown(t: &Trashed) -> String {
    t.path().map(|p| format!(" ({})", p.display())).unwrap_or_default()
}

/// Why `folder` cannot go to the Trash, and what to do instead.
fn trash_error(folder: &Path, e: TrashError) -> ApiError {
    use seedwell_platform::{file_manager, trash};
    ApiError::invalid(match e {
        TrashError::NoTrash => concat!("HOME is not set, so there is no ", trash!(), " to move the folder to; remove it without deleting files").into(),
        TrashError::OtherVolume { trash } => format!(
            concat!("{} is on another volume than the ", trash!(), " ({}); remove it without deleting files, then delete the folder in ", file_manager!()),
            folder.display(),
            trash.display()
        ),
        TrashError::WouldDelete { reason } => format!(
            concat!("the ", trash!(), " would delete {} at once instead of keeping it ({}); remove it without deleting files, then delete the folder in ", file_manager!()),
            folder.display(),
            reason
        ),
        TrashError::Io(e) => format!(concat!("{} cannot go to the ", trash!(), " ({}); remove it without deleting files"), folder.display(), e),
    })
}

// ----------------------------------------------------------------------------- channels

/// One channel in protocol form.
pub fn channel_dto(c: &seedwell_sync_net::chat::ChannelStatus) -> ChatChannelDto {
    ChatChannelDto {
        id: c.id.clone(),
        name: clip(&ops::clean_name(&c.name), 200),
        role: c.role.map(|r| role_name(r).into()),
        joining: c.joining,
        error: c.error.as_deref().map(|e| clip(&scrub_keys(e), 300)),
        share_until: c.share_until,
        members: c.members,
        online: c.online,
        unread: c.unread,
        last_message_at: c.last_message_s,
        muted: c.muted,
        agents: if c.agents { "yes" } else { "no" }.into(),
        wake_command: c.wake_command.clone(),
        issues: c.issues.iter().take(50).map(|i| clip(i, 300)).collect(),
        progress: c.progress.as_deref().map(|t| clip(t, 300)),
        problem: c.problem.as_deref().map(|t| clip(&scrub_keys(t), 300)),
        join_attempts: c.join_attempts.iter().map(attempt_dto).collect(),
        waiting: c.waiting.as_deref().map(|t| clip(t, 400)),
        gone: c.gone.iter().map(gone_dto).collect(),
    }
}

/// One message in protocol form: the same fields as the service's (and a wake-up command's) JSON.
pub fn message_dto(m: &seedwell_sync_net::chat::MessageInfo) -> ChatMessageDto {
    ChatMessageDto {
        id: m.id.clone(),
        channel_id: m.channel_id.clone(),
        channel_name: clip(&ops::clean_name(&m.channel_name), 200),
        author_device: m.author_device.clone(),
        author_name: clip(&ops::clean_name(&m.author_name), 100),
        author_user: clip(&ops::clean_name(&m.author_user), 100),
        me: m.me,
        agent: m.agent.clone(),
        at: m.at,
        future: m.future,
        reply_to: m.reply_to.clone(),
        topic: m.topic.clone(),
        topic_title: m.topic_title.as_deref().map(|t| clip(&ops::clean_name(t), 300)),
        direct: m.direct,
        peer_device: m.peer_device.clone(),
        kind: m.kind.clone(),
        body: m.body.clone(),
        trust: m.trust.clone(),
        attachments: m.attachments.iter().map(attachment_dto).collect(),
    }
}

/// An image of a message in protocol form (its name cleaned as names are).
pub fn attachment_dto(a: &seedwell_sync_net::chat::AttachmentInfo) -> ChatAttachmentDto {
    ChatAttachmentDto {
        hash: a.hash.clone(),
        kind: a.kind.clone(),
        name: clip(&ops::clean_name(&a.name), 200),
        size: a.size,
        width: a.width,
        height: a.height,
        state: a.state.clone(),
        problem: a.problem.as_deref().map(|p| clip(p, 300)),
        done: a.done,
        rate: a.rate,
        path: a.path.clone(),
    }
}

/// The channel `q` names: its id, an unambiguous prefix of it (4+ characters), or its name (any case).
pub fn resolve_channel<'a>(channels: &'a [seedwell_sync_net::chat::ChannelStatus], q: &str) -> Result<&'a seedwell_sync_net::chat::ChannelStatus, ApiError> {
    let q = q.trim();
    if q.is_empty() || q.len() > 200 {
        return Err(ApiError::params("`channel` is a channel id, a prefix of it, or the channel's name"));
    }
    let nq = norm_id(q);
    if let Some(c) = channels.iter().find(|c| !nq.is_empty() && norm_id(&c.id) == nq) {
        return Ok(c);
    }
    let lower = q.to_lowercase();
    let mut hits: Vec<&seedwell_sync_net::chat::ChannelStatus> = channels.iter().filter(|c| c.name.to_lowercase() == lower).collect();
    if nq.len() >= MIN_FOLDER_PREFIX && nq.len() == q.chars().filter(|c| *c != '-').count() {
        for c in channels.iter().filter(|c| norm_id(&c.id).starts_with(&nq)) {
            if !hits.iter().any(|h| h.id == c.id) {
                hits.push(c);
            }
        }
    }
    match hits.as_slice() {
        [] => Err(ApiError::not_found(format!("no channel `{}` (see `seedwell chat list`)", clip(q, 60)))),
        [one] => Ok(one),
        many => {
            let cands: Vec<_> = many.iter().take(8).map(|c| json!({"id": c.id, "name": c.name})).collect();
            Err(ApiError::new(ErrorCode::Ambiguous, format!("`{}` matches {} channels; use more of the id", clip(q, 60), many.len()))
                .with_data(json!({ "candidates": cands })))
        }
    }
}

/// One topic in protocol form.
pub fn topic_dto(t: &seedwell_sync_net::chat::TopicStatus) -> ChatTopicDto {
    ChatTopicDto {
        id: t.id.clone(),
        channel_id: t.channel_id.clone(),
        title: clip(&ops::clean_name(&t.title), 300),
        author_name: clip(&ops::clean_name(&t.author_name), 100),
        created_at: t.created_s,
        last_message_at: t.last_message_s,
        unread: t.unread,
        messages: t.messages,
    }
}

/// One private conversation in protocol form.
pub fn direct_dto(d: &seedwell_sync_net::direct::DirectStatus) -> ChatDirectDto {
    ChatDirectDto {
        device_id: d.device.to_string(),
        name: clip(&ops::clean_name(&d.name), 100),
        user: clip(&ops::clean_name(&d.user), 100),
        online: d.online,
        unread: d.unread,
        last_message_at: d.last_message_s,
    }
}

/// A channel service error for the protocol (not found means the channel).
pub fn chat_err(e: SyncError) -> ApiError {
    match e {
        SyncError::NotFound => ApiError::not_found("no such channel (see `seedwell chat list`)"),
        e => err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(id: &str, name: &str) -> FolderStatus {
        FolderStatus {
            id: id.into(),
            name: name.into(),
            path: format!("/tmp/{name}"),
            access: Access::Write,
            role: Some(Role::Owner),
            share_until: None,
            state: FolderState::UpToDate,
            files: 0,
            bytes: 0,
            need_files: 0,
            need_bytes: 0,
            rate_in: 0,
            eta_secs: None,
            rate_out: 0,
            connected: 0,
            members: 0,
            conflicts: 0,
            lan_only: false,
            issues: Vec::new(),
            error: None,
            last_synced_s: None,
            channel_id: None,
            on_demand: false,
            on_demand_others: false,
            unavailable: 0,
            progress: None,
            problem: None,
            join_attempts: Vec::new(),
            waiting: None,
            gone: Vec::new(),
            reshare: false,
            scan: None,
        }
    }

    fn places(t: &[(&str, &Path)]) -> Vec<seedwell_engine::TorrentPlaces> {
        t.iter().map(|(n, p)| seedwell_engine::TorrentPlaces { name: (*n).into(), paths: vec![p.to_path_buf()], moving_somewhere: false }).collect()
    }

    /// A torrent counts as using a folder when its files are in it (below it, or the folder itself is the torrent's) or
    /// its folder holds the synced folder; a torrent next to it (a name that only starts the same) does not.
    #[test]
    fn a_torrent_uses_a_folder_it_is_in_or_that_is_in_it_and_not_one_beside_it() {
        let t = places(&[
            ("in", Path::new("/d/Movies/V for Vendetta (2005)")),
            ("holds", Path::new("/d")),
            ("beside", Path::new("/d/Movies 2/x.mkv")),
            ("no metadata", Path::new("")),
        ]);
        assert_eq!(torrents_using(&t, Path::new("/d/Movies")), ["in", "holds"]);
        assert_eq!(torrents_using(&t, Path::new("/d/Movies/V for Vendetta (2005)")), ["in", "holds"]);
        assert!(torrents_using(&t, Path::new("/e")).is_empty());
    }

    /// Review of 0.1.18's on-demand fix, #1: the folder added as a link (`~/Movies` → `/Volumes/Media/Movies`) and a
    /// torrent saving to the real folder were two different texts, so freeing the folder's copies moved the torrent's
    /// file away. Fails when the guard compares paths as given: through a link, in another case on a disk that finds
    /// names in any case, or for a file the torrent has not written yet.
    #[cfg(unix)]
    #[test]
    fn a_torrent_in_a_folder_added_through_a_link_or_in_another_case_uses_it() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("Media").join("Movies");
        std::fs::create_dir_all(media.join("V for Vendetta (2005)")).unwrap();
        let link = dir.path().join("Movies");
        std::os::unix::fs::symlink(&media, &link).unwrap();
        // The torrent as the engine knows it (the real folder), and one that has not made its folder yet.
        let t = places(&[("seeding", &media.join("V for Vendetta (2005)")), ("new", &media.join("Not There Yet"))]);
        assert_eq!(torrents_using(&t, &link), ["seeding", "new"], "a folder added through a link");
        // In another case (the folder the user typed differently): one folder on a disk that folds case.
        let other_case = dir.path().join("media").join("movies");
        if seedwell_platform::fs::folds_case(&media) {
            assert_eq!(torrents_using(&t, &other_case), ["seeding", "new"], "another case on a disk that folds case");
            let t = places(&[("typed", &dir.path().join("MEDIA/MOVIES/not there yet"))]);
            assert_eq!(torrents_using(&t, &link), ["typed"], "a torrent's path in another case");
        }
        // A torrent on its way to a place not known yet may be going there.
        let moving = [seedwell_engine::TorrentPlaces { name: "moving".into(), paths: vec![dir.path().join("elsewhere")], moving_somewhere: true }];
        assert_eq!(torrents_using(&moving, &link), ["moving"]);
    }

    #[test]
    fn a_folder_says_whom_it_waits_for_which_devices_are_gone_and_whether_it_may_be_shared_again() {
        let mut f = folder("MFZWI3D-BONSGYC-YLTMRWG", "Docs");
        let d = folder_dto(&f);
        let json = serde_json::to_value(&d).unwrap();
        assert!(json.get("waiting").is_none() && json.get("gone").is_none() && json.get("can_reshare").is_none(), "nothing to say: nothing given");
        f.waiting = Some("Waiting for Mac mini (last seen 3 days ago: nothing answers at 192.168.68.57:55799)".into());
        f.gone = vec![seedwell_sync_net::status::GoneDevice { id: "BONSGYC-X".into(), name: "Mac mini".into(), last_seen_s: Some(7) }];
        f.reshare = true;
        let json = serde_json::to_value(folder_dto(&f)).unwrap();
        assert_eq!(json["waiting"], "Waiting for Mac mini (last seen 3 days ago: nothing answers at 192.168.68.57:55799)");
        assert_eq!(json["gone"], serde_json::json!([{"device_id": "BONSGYC-X", "name": "Mac mini", "last_seen_at": 7}]));
        assert_eq!(json["can_reshare"], true);
    }

    #[test]
    fn folders_are_named_by_id_prefix_or_name() {
        let fs = vec![folder("MFZWI3D-BONSGYC-YLTMRWG", "Docs"), folder("MFZWQQQ-AAAAAAA-BBBBBBB", "Photos"), folder("ZZZZZZZ-AAAAAAA-CCCCCCC", "mfzw")];
        assert_eq!(resolve_folder(&fs, "MFZWI3D-BONSGYC-YLTMRWG").unwrap().name, "Docs");
        assert_eq!(resolve_folder(&fs, "mfzwi3d").unwrap().name, "Docs", "the first group, any case");
        assert_eq!(resolve_folder(&fs, "photos").unwrap().name, "Photos");
        let e = resolve_folder(&fs, "MFZW").unwrap_err();
        assert_eq!(e.code, ErrorCode::Ambiguous, "two prefixes and a name");
        assert!(e.data.unwrap().to_string().contains("candidates"));
        assert_eq!(resolve_folder(&fs, "nothing").unwrap_err().code, ErrorCode::NotFound);
        assert_eq!(resolve_folder(&fs, "MFZ").unwrap_err().code, ErrorCode::NotFound, "too short for a prefix");
        assert_eq!(resolve_folder(&fs, " ").unwrap_err().code, ErrorCode::InvalidParams);
    }

    #[test]
    fn the_trash_must_be_on_the_folders_volume_and_names_do_not_collide() {
        let dir = tempfile::tempdir().unwrap();
        let trash = dir.path().join("Trash");
        let f = dir.path().join("Docs");
        std::fs::create_dir_all(&f).unwrap();
        let t = trash_target(&f, Some(&trash)).unwrap();
        assert_eq!(t, Trashed::At(trash.join("Docs")));
        std::fs::create_dir_all(trash.join("Docs")).unwrap();
        let t = trash_target(&f, Some(&trash)).unwrap();
        assert_eq!(t, Trashed::At(trash.join("Docs 2")));
        assert_eq!(trash_shown(&t), format!(" ({})", trash.join("Docs 2").display()));
        assert_eq!(move_to_trash(&f, Some(&trash)).unwrap(), t);
        assert!(!f.exists() && trash.join("Docs 2").is_dir());
        // A folder on an unmounted (or other) volume is refused before anything happens.
        let e = trash_target(Path::new("/Volumes/no-such-volume-for-seedwell/Docs"), Some(&trash)).unwrap_err();
        assert!(e.message.contains("another volume"), "{}", e.message);
    }

    /// An invitation link to the folder of `key` (an owner's).
    fn test_link(key: &seedwell_sync_proto::keys::FolderKey, name: Option<&str>) -> String {
        let s = key.secret().unwrap();
        let invite = seedwell_sync_proto::members::new_invite(
            *s.public_key(),
            [1; 16],
            [2; 32],
            Role::ReadWrite,
            1,
            0,
            1,
            seedwell_sync_proto::members::Signer::Folder,
            &|m| s.sign(m),
        );
        InviteLink { invite, token_seed: [2; 32], name: name.map(str::to_owned), agents: false, reach: None }.format()
    }

    #[test]
    fn joined_folders_get_a_default_place() {
        let dir = tempfile::tempdir().unwrap();
        let key = seedwell_sync_proto::keys::FolderKey::Owner(seedwell_sync_proto::keys::FolderSecret::from_bytes([7; 32]));
        let link = test_link(&key, Some("../My Docs"));
        let base = dir.path().display().to_string();
        assert_eq!(default_join_path(&base, &link).unwrap(), dir.path().join("My Docs"), "the name hint, made safe");
        let bare = default_join_path(&base, &test_link(&key, None)).unwrap();
        assert_eq!(bare, dir.path().join(key.folder_id().to_string().split('-').next().unwrap()));
        assert!(default_join_path(&base, "SWW-AAAA").is_err(), "bare keys are no longer links");
        assert_eq!(default_join_path(&base, "nonsense").unwrap_err().code, ErrorCode::Invalid);
    }
}

