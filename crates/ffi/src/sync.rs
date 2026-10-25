//! Folder sync for the app (SPEC 7G): the records the Swift shell shows, the Keychain bridge for the
//! keys ([`SecretVault`]), and [`SyncBackend`], which the real backend (a `trs_sync_net::SyncHandle` next
//! to the engine) and the mock (a few fake folders for design work) both implement.
//!
//! The service starts in the background after the engine; until it is up (or when it failed) the
//! snapshot says so and every change is refused with a plain sentence. A sync failure never stops a
//! torrent.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use trs_control::sync::{SyncSlot, default_join_path, folder_path, start_sync_with};
use trs_engine::EngineHandle;
use trs_sync_net::secrets::{FileSecrets, SecretStore};
use trs_sync_net::status::{FolderState, FolderStatus};
use trs_sync_net::{Keep, SyncError, SyncHandle};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::keys::Access;
use trs_sync_proto::members::{InviteLink, Role};

use crate::Core;
use crate::types::CoreError;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ------------------------------------------------------------------------------------------ records

/// What this Mac may do in a folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncAccess {
    /// The Read & write key: changes go both ways.
    ReadWrite,
    /// The Read only key: changes only arrive.
    ReadOnly,
    /// The Encrypted key (v1.x): stores data it cannot read.
    Encrypted,
}

/// A device's role in a folder (SPEC 7G.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncRole {
    /// Changes roles, removes devices, invites at any role.
    Owner,
    /// Invites at Read & write or Read only; cannot change roles.
    Admin,
    /// Changes go both ways.
    ReadWrite,
    /// Changes only arrive.
    ReadOnly,
}

/// What a folder is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncState {
    Starting,
    UpToDate,
    Scanning,
    Syncing,
    WaitingForDevices,
    /// Joining: waits for one of the folder's owners or admins to be online and admit this Mac.
    WaitingForAdmin,
    Paused,
    /// `.seedwell` is gone: an unmounted disk or a deleted folder. Nothing is deleted anywhere.
    Missing,
    Error,
}

/// A path that cannot be synced, and why.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncIssue {
    pub path: String,
    pub reason: String,
}

/// One synced folder.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncFolder {
    pub id: String,
    /// Its name on this Mac (the directory's name).
    pub name: String,
    pub path: String,
    /// From the role (Read only while joining).
    pub access: SyncAccess,
    /// This Mac's role (Read only while joining, or after it was removed: the state says which).
    pub role: SyncRole,
    /// When the folder stops being shared (its owners' setting), Unix seconds; `None` = no end. After it only the
    /// owners sync: every other device is cut off (it keeps its files) and no link admits anyone.
    pub share_until_s: Option<u64>,
    pub state: SyncState,
    /// Files and directories (as last scanned; 0 when not counted yet).
    pub files: u64,
    pub bytes: u64,
    /// Paths and bytes still to fetch.
    pub need_files: u64,
    pub need_bytes: u64,
    /// Bytes per second fetched and served.
    pub rate_in: u64,
    pub rate_out: u64,
    /// Devices connected for this folder, and every device known in it.
    pub connected: u32,
    pub members: u32,
    /// Conflict copies not yet resolved.
    pub conflicts: u32,
    /// Synced only with devices on the local network.
    pub lan_only: bool,
    pub issues: Vec<SyncIssue>,
    /// The reason, when the state is `Error`.
    pub error: Option<String>,
    /// When it last had nothing left to do, Unix seconds.
    pub last_synced_s: Option<u64>,
}

/// A connected device.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncDevice {
    pub id: String,
    pub name: String,
    /// `Seedwell 0.1.5`.
    pub client: String,
    pub address: String,
    /// `LAN`, `Internet` or `Relay`.
    pub via: String,
    /// The folders shared with it (ids).
    pub folders: Vec<String>,
    /// Bytes received from it and sent to it in this session.
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// Everything the menu, the window and Settings show about folder sync.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncSnapshot {
    /// The service runs (false while it starts, or when it could not).
    pub running: bool,
    /// Why it does not run, in a plain sentence (`None` while it is still starting).
    pub problem: Option<String>,
    pub device_id: String,
    pub device_name: String,
    /// The port sync listens on (0 = not listening).
    pub port: u16,
    pub folders: Vec<SyncFolder>,
    pub devices: Vec<SyncDevice>,
}

/// A device of a folder, with its role.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncMember {
    pub device_id: String,
    /// Its name (empty when never met).
    pub name: String,
    pub role: SyncRole,
    /// A session with it is open now for this folder.
    pub online: bool,
    pub is_me: bool,
    /// When its access ends: the folder's share end, for a device that is not an owner (Unix seconds); `None` =
    /// no end.
    pub expires_s: Option<u64>,
}

/// An invitation to a folder. The link is a secret: shown and copied, never logged.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncInvite {
    /// Its id (for revoking).
    pub id: String,
    /// The `seedwell://sync/…` link (empty for one another device made).
    pub link: String,
    pub role: SyncRole,
    /// Devices it admits; 0 = unlimited. `uses == max_uses`: used up.
    pub max_uses: u32,
    pub uses: u32,
    /// When it stops admitting, Unix seconds; `None` = never.
    pub expires_s: Option<u64>,
    pub created_s: u64,
    pub revoked: bool,
}

/// What a pasted link is, for the add modal ("Read & write · Photos").
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncLinkInfo {
    /// The role it admits at.
    pub role: SyncRole,
    /// The link's name hint, if it has one.
    pub name: Option<String>,
    pub folder_id: String,
    /// The folder is already synced on this Mac.
    pub already_synced: bool,
    /// When it stops admitting, Unix seconds; `None` = never.
    pub expires_s: Option<u64>,
    /// It has expired already.
    pub expired: bool,
}

/// A conflict copy in a folder: `Report (conflict, MacBook, 2026-10-25 14.32).txt`.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncConflict {
    /// The copy, absolute.
    pub path: String,
    /// The copy, relative to the folder (what [`Core::sync_resolve_conflict`] takes).
    pub copy: String,
    /// The file it is a copy of, relative to the folder (`Docs/Report.txt`).
    pub original: String,
    /// The device whose version it is.
    pub device: String,
    /// `2026-10-25 14.32`, as in the name.
    pub when: String,
}

/// Which paths of a folder are busy, for Finder's badges (relative to the folder, `/`-separated). Cheap to
/// ask every second.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct SyncActivity {
    /// Paths still to fetch (or delete), the first in path order.
    pub pending: Vec<String>,
    /// More paths are pending than listed.
    pub pending_more: bool,
    /// Conflict copies not yet resolved.
    pub conflicts: Vec<String>,
    /// The folder has ignore patterns of its own: a path that is not pending is not necessarily synced.
    pub own_ignores: bool,
}

/// Which version of a conflict to keep. The other one goes to the folder's archive on this Mac.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncKeep {
    /// The file at the original path.
    Original,
    /// The conflict copy: it replaces the original.
    Copy,
    /// Both: the copy becomes an ordinary file (`Report (MacBook, 2026-10-25 14.32).txt`).
    Both,
}

/// Device-wide sync settings (Settings › Folder Sync).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncSettings {
    /// The name other devices see; empty = the computer's name.
    pub device_name: String,
    /// The computer's name (the placeholder of an empty device name).
    pub default_device_name: String,
    /// The sync port in use (read only in the app).
    pub port: u16,
    /// Applies at the next start.
    pub lan_discovery: bool,
    pub dht_discovery: bool,
    pub relay_own_devices: bool,
    pub relay_everyone: bool,
    /// Internet limits in bytes per second, 0 = none. The LAN is never limited.
    pub up_limit: u64,
    pub down_limit: u64,
    /// Where joined folders go; empty = `~/Seedwell`.
    pub default_location: String,
}

// ------------------------------------------------------------------------------------------ keychain

/// Why the vault could not do something.
#[derive(Debug, uniffi::Error)]
pub enum VaultError {
    Failed { message: String },
}

impl std::error::Error for VaultError {}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VaultError::Failed { message } => f.write_str(message),
        }
    }
}

impl From<uniffi::UnexpectedUniFFICallbackError> for VaultError {
    fn from(e: uniffi::UnexpectedUniFFICallbackError) -> Self {
        VaultError::Failed { message: e.reason }
    }
}

/// Where folder keys and the device key live: the Keychain, implemented in Swift (generic passwords,
/// service `io.github.kkollsga.Seedwell.sync`, account = `name`). Called from core threads.
#[uniffi::export(with_foreign)]
pub trait SecretVault: Send + Sync {
    /// The secret called `name`, if stored.
    fn get(&self, name: String) -> Result<Option<Vec<u8>>, VaultError>;
    /// Stores or replaces a secret.
    fn put(&self, name: String, value: Vec<u8>) -> Result<(), VaultError>;
    /// Forgets a secret (no error when it was not there).
    fn delete(&self, name: String) -> Result<(), VaultError>;
}

/// [`SecretVault`] as the sync service's [`SecretStore`].
pub struct VaultStore(pub Arc<dyn SecretVault>);

fn vault_io(e: VaultError) -> std::io::Error {
    std::io::Error::other(format!("Keychain: {e}"))
}

impl SecretStore for VaultStore {
    fn get(&self, name: &str) -> std::io::Result<Option<Vec<u8>>> {
        self.0.get(name.to_owned()).map_err(vault_io)
    }
    fn put(&self, name: &str, value: &[u8]) -> std::io::Result<()> {
        self.0.put(name.to_owned(), value.to_vec()).map_err(vault_io)
    }
    fn delete(&self, name: &str) -> std::io::Result<()> {
        self.0.delete(name.to_owned()).map_err(vault_io)
    }
}

/// The store sync uses: the vault the app passed, else a 0600 file in the state folder (tests, `--mock`,
/// isolated QA runs, which must never touch the user's Keychain).
pub fn secret_store(vault: Option<Arc<dyn SecretVault>>, state_dir: &Path) -> Arc<dyn SecretStore> {
    match vault {
        Some(v) => Arc::new(VaultStore(v)),
        None => Arc::new(FileSecrets::new(state_dir.join("sync").join("secrets"))),
    }
}

// ------------------------------------------------------------------------------------------ the contract

/// What the UI can ask of folder sync. Calls may block (moving a folder copies across disks): the app
/// calls the changing ones off the main thread.
pub trait SyncBackend: Send + Sync {
    fn snapshot(&self) -> SyncSnapshot;
    fn add_folder(&self, path: &str) -> Result<SyncFolder, CoreError>;
    fn join(&self, key_or_link: &str, path: &str) -> Result<SyncFolder, CoreError>;
    /// Where a joined folder goes by default (`~/Seedwell/<name>`, numbered when taken).
    fn join_path(&self, key_or_link: &str) -> Option<String>;
    fn members(&self, id: &str) -> Vec<SyncMember>;
    fn set_role(&self, id: &str, device_id: &str, role: SyncRole) -> Result<(), CoreError>;
    fn remove_device(&self, id: &str, device_id: &str) -> Result<(), CoreError>;
    fn set_share_until(&self, id: &str, until_s: u64) -> Result<(), CoreError>;
    fn create_invite(&self, id: &str, role: SyncRole, max_uses: u32, valid_days: u32) -> Result<SyncInvite, CoreError>;
    fn invites(&self, id: &str) -> Vec<SyncInvite>;
    fn revoke_invite(&self, id: &str, invite_id: &str) -> Result<(), CoreError>;
    fn rename(&self, id: &str, name: &str) -> Result<SyncFolder, CoreError>;
    fn move_to(&self, id: &str, path: &str) -> Result<SyncFolder, CoreError>;
    /// Stops syncing; returns where the folder is (the app moves it to the Trash when asked to).
    fn remove(&self, id: &str, trash_files: bool) -> Result<String, CoreError>;
    fn pause(&self, id: &str, paused: bool) -> Result<(), CoreError>;
    fn rescan(&self, id: &str) -> Result<(), CoreError>;
    fn conflicts(&self, id: &str) -> Vec<SyncConflict>;
    fn activity(&self, id: &str) -> SyncActivity;
    fn resolve(&self, id: &str, copy: &str, keep: SyncKeep) -> Result<(), CoreError>;
    fn set_lan_only(&self, id: &str, lan_only: bool) -> Result<(), CoreError>;
    fn settings(&self) -> SyncSettings;
    fn set_settings(&self, s: &SyncSettings) -> Result<(), CoreError>;
    fn default_location(&self) -> String;
}

/// A sentence for people: the service's lowercase reasons get a capital and a full stop.
pub fn readable(msg: &str) -> String {
    let msg = msg.trim();
    let mut out: String = match msg.chars().next() {
        Some(c) if c.is_lowercase() => c.to_uppercase().chain(msg.chars().skip(1)).collect(),
        _ => msg.to_owned(),
    };
    if !out.ends_with(['.', '!', '?']) {
        out.push('.');
    }
    out
}

fn failed(msg: impl AsRef<str>) -> CoreError {
    CoreError::Failed { message: readable(msg.as_ref()) }
}

fn gone() -> CoreError {
    CoreError::Failed { message: "This folder is no longer synced on this Mac.".into() }
}

fn sync_err(e: SyncError) -> CoreError {
    match e {
        SyncError::NotFound => gone(),
        SyncError::Invalid(why) => failed(trs_control::redact::scrub_keys(&why)),
        SyncError::Io(e) => failed(trs_control::redact::scrub_keys(&e.to_string())),
    }
}

fn api_err(e: trs_control::ApiError) -> CoreError {
    failed(e.message)
}

pub fn access_of(a: Access) -> SyncAccess {
    match a {
        Access::Write => SyncAccess::ReadWrite,
        Access::Read => SyncAccess::ReadOnly,
        Access::Storage => SyncAccess::Encrypted,
    }
}

pub fn role_of(r: Role) -> SyncRole {
    match r {
        Role::Owner => SyncRole::Owner,
        Role::Admin => SyncRole::Admin,
        Role::ReadWrite => SyncRole::ReadWrite,
        Role::ReadOnly => SyncRole::ReadOnly,
    }
}

fn role_from(r: SyncRole) -> Role {
    match r {
        SyncRole::Owner => Role::Owner,
        SyncRole::Admin => Role::Admin,
        SyncRole::ReadWrite => Role::ReadWrite,
        SyncRole::ReadOnly => Role::ReadOnly,
    }
}

fn invite_of(i: &trs_sync_net::InviteInfo) -> SyncInvite {
    SyncInvite {
        id: i.id.clone(),
        link: i.link.clone(),
        role: role_of(i.role),
        max_uses: i.max_uses,
        uses: i.uses,
        expires_s: i.expires_s,
        created_s: i.created_s,
        revoked: i.revoked,
    }
}

fn state_of(s: &FolderState) -> SyncState {
    match s {
        FolderState::Starting => SyncState::Starting,
        FolderState::UpToDate => SyncState::UpToDate,
        FolderState::Scanning => SyncState::Scanning,
        FolderState::Syncing => SyncState::Syncing,
        FolderState::WaitingForDevices => SyncState::WaitingForDevices,
        FolderState::WaitingForAdmin => SyncState::WaitingForAdmin,
        FolderState::Paused => SyncState::Paused,
        FolderState::Missing => SyncState::Missing,
        FolderState::Error => SyncState::Error,
    }
}

fn folder_of(f: &FolderStatus) -> SyncFolder {
    SyncFolder {
        id: f.id.clone(),
        name: f.name.clone(),
        path: f.path.clone(),
        access: access_of(f.access),
        role: f.role.map_or(SyncRole::ReadOnly, role_of),
        share_until_s: f.share_until,
        state: state_of(&f.state),
        files: f.files,
        bytes: f.bytes,
        need_files: f.need_files,
        need_bytes: f.need_bytes,
        rate_in: f.rate_in,
        rate_out: f.rate_out,
        connected: f.connected,
        members: f.members,
        conflicts: f.conflicts,
        lan_only: f.lan_only,
        issues: f.issues.iter().take(200).map(|(p, r)| SyncIssue { path: p.clone(), reason: r.clone() }).collect(),
        error: f.error.as_deref().map(|e| readable(&trs_control::redact::scrub_keys(e))),
        last_synced_s: f.last_synced_s,
    }
}

/// A pasted invitation link, decoded (nothing is joined). Bare keys of the test builds are not links.
pub fn parse_link(text: &str) -> Option<(SyncLinkInfo, InviteLink)> {
    let link = InviteLink::parse(text).ok()?;
    let i = &link.invite;
    let info = SyncLinkInfo {
        role: role_of(i.role),
        name: link.name.clone(),
        folder_id: i.folder_id().to_string(),
        already_synced: false,
        expires_s: (i.expires_s != 0).then_some(i.expires_s),
        expired: i.expired(now_s()),
    };
    Some((info, link))
}

/// A conflict copy of a folder at `root` as the app shows it (`copy` and `original` in wire form).
fn conflict_of(root: &Path, copy: String, original: String) -> SyncConflict {
    let name = trs_sync::conflict::parse_conflict(&copy);
    SyncConflict {
        path: root.join(&copy).display().to_string(),
        copy,
        original,
        device: name.as_ref().map(|n| n.device.clone()).unwrap_or_default(),
        when: name.map(|n| n.stamp).unwrap_or_default(),
    }
}

fn default_location_of(setting: &str) -> String {
    let base = if setting.trim().is_empty() { "~/Seedwell" } else { setting };
    crate::settings::expand_home(base).display().to_string()
}

// ------------------------------------------------------------------------------------------ real

/// The sync service next to the engine.
pub struct RealSync {
    rt: tokio::runtime::Handle,
    slot: SyncSlot,
    /// Why the service is not running, once its start failed.
    problem: Arc<Mutex<Option<String>>>,
    default_name: String,
}

fn is_test_run() -> bool {
    cfg!(test) || std::env::current_exe().ok().and_then(|p| p.parent().and_then(|d| d.file_name()).map(|n| n == "deps")).unwrap_or(false)
}

impl RealSync {
    /// Starts the service in the background: on all interfaces, IPv4 and IPv6 (`::`), LAN discovery on,
    /// the engine's DHT and port mapping lent to it. Tests bind to loopback and stay off the LAN.
    pub fn start(rt: &tokio::runtime::Handle, engine: &EngineHandle, dir: &Path, vault: Option<Arc<dyn SecretVault>>) -> Arc<Self> {
        let slot = SyncSlot::new();
        let problem = Arc::new(Mutex::new(None));
        let default_name = trs_control::sync::default_device_name();
        let me = Arc::new(RealSync { rt: rt.clone(), slot: slot.clone(), problem: problem.clone(), default_name: default_name.clone() });
        let secrets = secret_store(vault, dir);
        let (engine, dir) = (engine.clone(), dir.to_path_buf());
        rt.spawn(async move {
            let client = format!("Seedwell {}", env!("CARGO_PKG_VERSION"));
            let started = start_sync_with(&dir, client, secrets, Some(&engine), |o| {
                o.default_device_name = default_name;
                o.utc_offset = Some(Arc::new(trs_engine::util::local_utc_offset));
                if is_test_run() {
                    o.bind = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
                    o.lan_discovery = false;
                } else {
                    o.bind = std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);
                }
            })
            .await;
            match started {
                Ok(h) => slot.set(Some(h)),
                Err(e) => {
                    eprintln!("seedwell: folder sync did not start: {e}");
                    *lock(&problem) = Some(readable(&format!("Folder sync could not start: {e}")));
                }
            }
        });
        me
    }

    /// Where the control server finds the service.
    pub fn slot(&self) -> SyncSlot {
        self.slot.clone()
    }

    /// Stops the service (the app quits).
    pub async fn stop(&self) {
        if let Some(h) = self.slot.get() {
            self.slot.set(None);
            h.shutdown().await;
        }
    }

    fn handle(&self) -> Result<SyncHandle, CoreError> {
        self.slot.get().ok_or_else(|| {
            failed(lock(&self.problem).clone().unwrap_or_else(|| "Folder sync is still starting. Try again in a moment.".into()))
        })
    }

    fn status(&self, h: &SyncHandle, id: &str) -> Result<FolderStatus, CoreError> {
        h.snapshot().folders.into_iter().find(|f| f.id == id).ok_or_else(gone)
    }
}

impl SyncBackend for RealSync {
    fn snapshot(&self) -> SyncSnapshot {
        let Some(h) = self.slot.get() else {
            return SyncSnapshot {
                running: false,
                problem: lock(&self.problem).clone(),
                device_id: String::new(),
                device_name: String::new(),
                port: 0,
                folders: Vec::new(),
                devices: Vec::new(),
            };
        };
        let s = h.snapshot();
        SyncSnapshot {
            running: true,
            problem: None,
            device_id: s.device_id,
            device_name: s.device_name,
            port: s.port,
            folders: s.folders.iter().map(folder_of).collect(),
            devices: s
                .devices
                .into_iter()
                .map(|d| SyncDevice {
                    id: d.id,
                    name: d.name,
                    client: d.client,
                    address: d.address,
                    via: d.via,
                    folders: d.folders,
                    rx_bytes: d.rx_bytes,
                    tx_bytes: d.tx_bytes,
                })
                .collect(),
        }
    }

    fn add_folder(&self, path: &str) -> Result<SyncFolder, CoreError> {
        let h = self.handle()?;
        let p = folder_path(path, "the folder").map_err(api_err)?;
        let f = self.rt.block_on(h.add_folder(p)).map_err(sync_err)?;
        Ok(folder_of(&f))
    }

    fn join(&self, key_or_link: &str, path: &str) -> Result<SyncFolder, CoreError> {
        let h = self.handle()?;
        if let Err(e) = InviteLink::parse(key_or_link) {
            return Err(failed(e.to_string()));
        }
        let p = if path.trim().is_empty() {
            default_join_path(&h.settings().default_location, key_or_link).map_err(api_err)?
        } else {
            folder_path(path, "the location").map_err(api_err)?
        };
        let f = self.rt.block_on(h.join_folder(key_or_link, p)).map_err(sync_err)?;
        Ok(folder_of(&f))
    }

    fn join_path(&self, key_or_link: &str) -> Option<String> {
        let loc = self.slot.get().map(|h| h.settings().default_location).unwrap_or_default();
        let base = default_location_of(&loc);
        default_join_path(&base, key_or_link).ok().map(|p| p.display().to_string())
    }

    fn members(&self, id: &str) -> Vec<SyncMember> {
        let (Ok(h), Ok(fid)) = (self.handle(), folder_id(id)) else { return Vec::new() };
        h.members(&fid)
            .unwrap_or_default()
            .into_iter()
            .map(|m| SyncMember {
                device_id: m.device.to_string(),
                name: m.name,
                role: role_of(m.role),
                online: m.online,
                is_me: m.is_me,
                expires_s: m.expires_s,
            })
            .collect()
    }

    fn set_role(&self, id: &str, device_id: &str, role: SyncRole) -> Result<(), CoreError> {
        let h = self.handle()?;
        let device = device_id.parse().map_err(|_| failed("Not a device of this folder"))?;
        h.set_role(&folder_id(id)?, &device, role_from(role)).map_err(sync_err)
    }

    fn remove_device(&self, id: &str, device_id: &str) -> Result<(), CoreError> {
        let h = self.handle()?;
        let device = device_id.parse().map_err(|_| failed("Not a device of this folder"))?;
        h.remove_device(&folder_id(id)?, &device).map_err(sync_err)
    }

    fn set_share_until(&self, id: &str, until_s: u64) -> Result<(), CoreError> {
        let h = self.handle()?;
        h.set_share_until(&folder_id(id)?, until_s).map_err(sync_err)
    }

    fn create_invite(&self, id: &str, role: SyncRole, max_uses: u32, valid_days: u32) -> Result<SyncInvite, CoreError> {
        let h = self.handle()?;
        let i = h.create_invite(&folder_id(id)?, role_from(role), max_uses, valid_days).map_err(sync_err)?;
        Ok(invite_of(&i))
    }

    fn invites(&self, id: &str) -> Vec<SyncInvite> {
        let (Ok(h), Ok(fid)) = (self.handle(), folder_id(id)) else { return Vec::new() };
        h.invites(&fid).unwrap_or_default().iter().map(invite_of).collect()
    }

    fn revoke_invite(&self, id: &str, invite_id: &str) -> Result<(), CoreError> {
        let h = self.handle()?;
        h.revoke_invite(&folder_id(id)?, invite_id).map_err(sync_err)
    }

    fn rename(&self, id: &str, name: &str) -> Result<SyncFolder, CoreError> {
        let h = self.handle()?;
        let fid = folder_id(id)?;
        let f = self.rt.block_on(h.rename_folder(&fid, name)).map_err(sync_err)?;
        Ok(folder_of(&f))
    }

    fn move_to(&self, id: &str, path: &str) -> Result<SyncFolder, CoreError> {
        let h = self.handle()?;
        let fid = folder_id(id)?;
        let p = folder_path(path, "the new place").map_err(api_err)?;
        let f = self.rt.block_on(h.move_folder(&fid, p)).map_err(sync_err)?;
        Ok(folder_of(&f))
    }

    fn remove(&self, id: &str, _trash_files: bool) -> Result<String, CoreError> {
        // Moving the files to the Trash is the app's (it knows the user's Trash): only stop here.
        let h = self.handle()?;
        let fid = folder_id(id)?;
        let path = self.rt.block_on(h.remove_folder(&fid)).map_err(sync_err)?;
        Ok(path.display().to_string())
    }

    fn pause(&self, id: &str, paused: bool) -> Result<(), CoreError> {
        let h = self.handle()?;
        let fid = folder_id(id)?;
        self.rt.block_on(h.set_paused(&fid, paused)).map_err(sync_err)
    }

    fn rescan(&self, id: &str) -> Result<(), CoreError> {
        let h = self.handle()?;
        h.rescan(&folder_id(id)?).map_err(sync_err)
    }

    fn activity(&self, id: &str) -> SyncActivity {
        let (Ok(h), Ok(fid)) = (self.handle(), folder_id(id)) else { return SyncActivity::default() };
        let Ok(a) = h.activity(&fid) else { return SyncActivity::default() };
        SyncActivity { pending: a.pending, pending_more: a.pending_more, conflicts: a.conflicts, own_ignores: a.own_ignores }
    }

    fn conflicts(&self, id: &str) -> Vec<SyncConflict> {
        let (Ok(h), Ok(fid)) = (self.handle(), folder_id(id)) else { return Vec::new() };
        let Ok(f) = self.status(&h, id) else { return Vec::new() };
        let list = self.rt.block_on(h.conflicts(&fid)).unwrap_or_default();
        let mut out: Vec<SyncConflict> = list.into_iter().map(|(c, o)| conflict_of(Path::new(&f.path), c, o)).collect();
        out.sort_by(|a, b| b.when.cmp(&a.when).then_with(|| a.copy.cmp(&b.copy)));
        out
    }

    fn resolve(&self, id: &str, copy: &str, keep: SyncKeep) -> Result<(), CoreError> {
        let h = self.handle()?;
        let fid = folder_id(id)?;
        let keep = match keep {
            SyncKeep::Original => Keep::Original,
            SyncKeep::Copy => Keep::Copy,
            SyncKeep::Both => Keep::Both,
        };
        self.rt.block_on(h.resolve_conflict(&fid, copy, keep)).map_err(sync_err)
    }

    fn set_lan_only(&self, id: &str, lan_only: bool) -> Result<(), CoreError> {
        let h = self.handle()?;
        h.set_lan_only(&folder_id(id)?, lan_only).map_err(sync_err)
    }

    fn settings(&self) -> SyncSettings {
        let st = self.slot.get().map(|h| (h.settings(), h.port()));
        let (s, port) = st.unwrap_or_default();
        SyncSettings {
            device_name: s.device_name,
            default_device_name: self.default_name.clone(),
            port: if port != 0 { port } else { s.port },
            lan_discovery: s.lan_discovery,
            dht_discovery: s.dht_discovery,
            relay_own_devices: s.relay_own_devices,
            relay_everyone: s.relay_everyone,
            up_limit: u64::from(s.up_kib) * 1024,
            down_limit: u64::from(s.down_kib) * 1024,
            default_location: s.default_location,
        }
    }

    fn set_settings(&self, s: &SyncSettings) -> Result<(), CoreError> {
        let h = self.handle()?;
        let mut st = h.settings();
        st.device_name = s.device_name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
        st.lan_discovery = s.lan_discovery;
        st.dht_discovery = s.dht_discovery;
        st.relay_own_devices = s.relay_own_devices;
        st.relay_everyone = s.relay_everyone;
        st.up_kib = kib(s.up_limit);
        st.down_kib = kib(s.down_limit);
        st.default_location = if s.default_location.trim().is_empty() {
            String::new()
        } else {
            folder_path(&s.default_location, "the default location").map_err(api_err)?.display().to_string()
        };
        h.set_settings(st).map_err(sync_err)
    }

    fn default_location(&self) -> String {
        default_location_of(&self.slot.get().map(|h| h.settings().default_location).unwrap_or_default())
    }
}

fn folder_id(id: &str) -> Result<FolderId, CoreError> {
    id.parse().map_err(|_| gone())
}

fn kib(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(1024)).unwrap_or(u32::MAX)
}

// ------------------------------------------------------------------------------------------ not running

/// A backend without sync (none of the current ones; the trait's default).
pub struct NoSync;

impl SyncBackend for NoSync {
    fn snapshot(&self) -> SyncSnapshot {
        SyncSnapshot {
            running: false,
            problem: Some("Folder sync is not available.".into()),
            device_id: String::new(),
            device_name: String::new(),
            port: 0,
            folders: Vec::new(),
            devices: Vec::new(),
        }
    }
    fn add_folder(&self, _: &str) -> Result<SyncFolder, CoreError> {
        Err(failed("Folder sync is not available"))
    }
    fn join(&self, _: &str, _: &str) -> Result<SyncFolder, CoreError> {
        Err(failed("Folder sync is not available"))
    }
    fn join_path(&self, _: &str) -> Option<String> {
        None
    }
    fn members(&self, _: &str) -> Vec<SyncMember> {
        Vec::new()
    }
    fn set_role(&self, _: &str, _: &str, _: SyncRole) -> Result<(), CoreError> {
        Err(gone())
    }
    fn remove_device(&self, _: &str, _: &str) -> Result<(), CoreError> {
        Err(gone())
    }
    fn set_share_until(&self, _: &str, _: u64) -> Result<(), CoreError> {
        Err(gone())
    }
    fn create_invite(&self, _: &str, _: SyncRole, _: u32, _: u32) -> Result<SyncInvite, CoreError> {
        Err(gone())
    }
    fn invites(&self, _: &str) -> Vec<SyncInvite> {
        Vec::new()
    }
    fn revoke_invite(&self, _: &str, _: &str) -> Result<(), CoreError> {
        Err(gone())
    }
    fn rename(&self, _: &str, _: &str) -> Result<SyncFolder, CoreError> {
        Err(gone())
    }
    fn move_to(&self, _: &str, _: &str) -> Result<SyncFolder, CoreError> {
        Err(gone())
    }
    fn remove(&self, _: &str, _: bool) -> Result<String, CoreError> {
        Err(gone())
    }
    fn pause(&self, _: &str, _: bool) -> Result<(), CoreError> {
        Err(gone())
    }
    fn rescan(&self, _: &str) -> Result<(), CoreError> {
        Err(gone())
    }
    fn conflicts(&self, _: &str) -> Vec<SyncConflict> {
        Vec::new()
    }
    fn activity(&self, _: &str) -> SyncActivity {
        SyncActivity::default()
    }
    fn resolve(&self, _: &str, _: &str, _: SyncKeep) -> Result<(), CoreError> {
        Err(gone())
    }
    fn set_lan_only(&self, _: &str, _: bool) -> Result<(), CoreError> {
        Err(gone())
    }
    fn settings(&self) -> SyncSettings {
        MockSync::default_settings()
    }
    fn set_settings(&self, _: &SyncSettings) -> Result<(), CoreError> {
        Err(failed("Folder sync is not available"))
    }
    fn default_location(&self) -> String {
        default_location_of("")
    }
}

// ------------------------------------------------------------------------------------------ mock

struct MockFolder {
    n: u32,
    f: SyncFolder,
    members: Vec<SyncMember>,
    invites: Vec<SyncInvite>,
}

struct MockState {
    folders: Vec<MockFolder>,
    settings: SyncSettings,
    next: u32,
}

/// A few fake folders in every state the UI draws, for design work and the snapshot renderer. Nothing on
/// disk is touched; the syncing folder moves along with the clock.
pub struct MockSync {
    state: Mutex<MockState>,
    started: Instant,
}

fn mock_id(n: u32) -> String {
    let groups = ["MFZWI3D", "BONSGYC", "YLTMRWG", "QKZ4XNA", "P2RFOJE", "HTU7WQB"];
    format!("{}-{}-{}-{n:07}", groups[n as usize % 6], groups[(n as usize + 1) % 6], groups[(n as usize + 2) % 6])
}

/// A real invitation link to mock folder `n` (nothing listens for it).
fn mock_invite(n: u32, k: u32, name: &str, role: SyncRole, max_uses: u32, valid_days: u32) -> SyncInvite {
    let secret = trs_sync_proto::keys::FolderSecret::from_bytes([n as u8 + 1; 32]);
    let now = now_s();
    let expires_s = if valid_days == 0 { 0 } else { now + u64::from(valid_days) * 86_400 };
    let id = [k as u8 + 1; 16];
    let seed = [(n * 16 + k) as u8; 32];
    let invite = trs_sync_proto::members::new_invite(
        *secret.public_key(),
        id,
        seed,
        role_from(role),
        max_uses,
        expires_s,
        now,
        trs_sync_proto::members::Signer::Folder,
        &|m| secret.sign(m),
    );
    let link = InviteLink { invite, token_seed: seed, name: Some(name.to_owned()) }.format();
    SyncInvite {
        id: trs_sync_proto::members::hex(&id),
        link,
        role,
        max_uses,
        uses: 0,
        expires_s: (expires_s != 0).then_some(expires_s),
        created_s: now,
        revoked: false,
    }
}

/// The mock devices of a folder: this Mac at `mine`, and `others` (name, role, online).
fn mock_members(mine: SyncRole, others: &[(&str, SyncRole, bool)], share_until_s: Option<u64>) -> Vec<SyncMember> {
    let ends = |role: SyncRole| share_until_s.filter(|_| role != SyncRole::Owner);
    let mut out = vec![SyncMember {
        device_id: "HTU7WQB-QKZ4XNA-MFZWI3D-YLTMRWG".into(),
        name: "Kristian's MacBook Pro".into(),
        role: mine,
        online: true,
        is_me: true,
        expires_s: ends(mine),
    }];
    for (i, (name, role, online)) in others.iter().enumerate() {
        let groups = ["MFZWI3D-BONSGYC-YLTMRWG-QKZ4XNA", "P2RFOJE-HTU7WQB-MFZWI3D-BONSGYC", "QKZ4XNA-YLTMRWG-P2RFOJE-HTU7WQB"];
        out.push(SyncMember { device_id: groups[i % 3].into(), name: (*name).into(), role: *role, online: *online, is_me: false, expires_s: ends(*role) });
    }
    out
}

impl MockSync {
    pub fn new() -> Arc<Self> {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/me".into());
        let base = format!("{home}/Seedwell");
        let mk = |n: u32, name: &str, access: SyncAccess, state: SyncState, members: u32, connected: u32| SyncFolder {
            id: mock_id(n),
            name: name.into(),
            path: format!("{base}/{name}"),
            access,
            role: if access == SyncAccess::ReadOnly { SyncRole::ReadOnly } else { SyncRole::ReadWrite },
            share_until_s: None,
            state,
            files: 1_200 + u64::from(n) * 731,
            bytes: (8 + u64::from(n) * 5) * 1_000_000_000,
            need_files: 0,
            need_bytes: 0,
            rate_in: 0,
            rate_out: 0,
            connected,
            members,
            conflicts: 0,
            lan_only: false,
            issues: Vec::new(),
            error: None,
            last_synced_s: Some(now_s().saturating_sub(120)),
        };
        let mut photos = mk(1, "Photos", SyncAccess::ReadWrite, SyncState::UpToDate, 3, 3);
        photos.role = SyncRole::Owner;
        photos.path = format!("{home}/Pictures/Photos");
        let mut docs = mk(2, "Documents", SyncAccess::ReadWrite, SyncState::Syncing, 2, 2);
        docs.need_files = 312;
        docs.role = SyncRole::Owner;
        // Shared for a while (the owner's setting).
        docs.share_until_s = Some(now_s() + 12 * 86_400);
        let docs_end = docs.share_until_s;
        let mut projects = mk(3, "Projects", SyncAccess::ReadWrite, SyncState::UpToDate, 2, 1);
        projects.conflicts = 1;
        projects.role = SyncRole::Admin;
        projects.issues = vec![SyncIssue { path: "Build/Icon\r".into(), reason: "The name cannot be used on every device".into() }];
        let camera = mk(4, "Camera Uploads from the Family iPhone and iPad", SyncAccess::ReadOnly, SyncState::WaitingForDevices, 1, 0);
        let mut archive = mk(5, "Archive", SyncAccess::ReadWrite, SyncState::Missing, 2, 0);
        archive.path = "/Volumes/Backup/Archive".into();
        archive.role = SyncRole::ReadWrite;
        let members = [
            mock_members(SyncRole::Owner, &[("Studio iMac", SyncRole::Owner, true), ("Kristian's iPhone", SyncRole::ReadWrite, true), ("Office Mac mini", SyncRole::ReadOnly, true)], None),
            mock_members(SyncRole::Owner, &[("Studio iMac", SyncRole::Admin, true), ("Kristian's iPhone", SyncRole::ReadWrite, true)], docs_end),
            mock_members(SyncRole::Admin, &[("Studio iMac", SyncRole::Owner, true), ("Ola's MacBook Air", SyncRole::ReadWrite, false)], None),
            mock_members(SyncRole::ReadOnly, &[("Family iPad", SyncRole::Owner, false)], None),
            mock_members(SyncRole::ReadWrite, &[("Studio iMac", SyncRole::Owner, false), ("Office Mac mini", SyncRole::ReadWrite, false)], None),
        ];
        let folders = [photos, docs, projects, camera, archive]
            .into_iter()
            .zip(members)
            .enumerate()
            .map(|(i, (f, members))| {
                let n = i as u32 + 1;
                let invites = if matches!(f.role, SyncRole::Owner | SyncRole::Admin) {
                    let mut used = mock_invite(n, 1, &f.name, SyncRole::ReadWrite, 1, 7);
                    used.uses = 1;
                    vec![mock_invite(n, 2, &f.name, SyncRole::ReadOnly, 5, 30), used]
                } else {
                    Vec::new()
                };
                MockFolder { n, f, members, invites }
            })
            .collect();
        Arc::new(MockSync { state: Mutex::new(MockState { folders, settings: Self::default_settings(), next: 6 }), started: Instant::now() })
    }

    fn default_settings() -> SyncSettings {
        SyncSettings {
            device_name: String::new(),
            default_device_name: "Kristian's MacBook Pro".into(),
            port: 22_100,
            lan_discovery: true,
            dht_discovery: true,
            relay_own_devices: true,
            relay_everyone: false,
            up_limit: 0,
            down_limit: 0,
            default_location: String::new(),
        }
    }

    fn with<T>(&self, id: &str, f: impl FnOnce(&mut MockFolder) -> T) -> Result<T, CoreError> {
        let mut s = lock(&self.state);
        s.folders.iter_mut().find(|m| m.f.id == id).map(f).ok_or_else(gone)
    }

    fn add(&self, name: String, path: String, role: SyncRole, joining: bool) -> SyncFolder {
        let mut s = lock(&self.state);
        let n = s.next;
        s.next += 1;
        let access = if role == SyncRole::ReadOnly || joining { SyncAccess::ReadOnly } else { SyncAccess::ReadWrite };
        let f = SyncFolder {
            id: mock_id(n),
            name: name.clone(),
            path,
            access,
            role: if joining { SyncRole::ReadOnly } else { role },
            share_until_s: None,
            state: if joining { SyncState::WaitingForAdmin } else { SyncState::Starting },
            files: 0,
            bytes: 0,
            need_files: 0,
            need_bytes: 0,
            rate_in: 0,
            rate_out: 0,
            connected: 0,
            members: 0,
            conflicts: 0,
            lan_only: false,
            issues: Vec::new(),
            error: None,
            last_synced_s: None,
        };
        let members = mock_members(if joining { SyncRole::ReadOnly } else { role }, &[], None);
        s.folders.push(MockFolder { n, f: f.clone(), members, invites: Vec::new() });
        f
    }
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn parent_of(path: &str) -> String {
    Path::new(path).parent().map(|p| p.display().to_string()).unwrap_or_default()
}

impl SyncBackend for MockSync {
    fn snapshot(&self) -> SyncSnapshot {
        let t = self.started.elapsed().as_secs();
        let s = lock(&self.state);
        let folders: Vec<SyncFolder> = s
            .folders
            .iter()
            .map(|m| {
                let mut f = m.f.clone();
                if f.state == SyncState::Syncing {
                    // 42 % at the start, a percent every two seconds, then up to date.
                    let done = (42 + t / 2).min(100);
                    f.need_bytes = f.bytes * (100 - done) / 100;
                    f.need_files = f.need_files * (100 - done) / 58;
                    f.rate_in = if done < 100 { 12_000_000 + (t % 5) * 400_000 } else { 0 };
                    if done >= 100 {
                        f.state = SyncState::UpToDate;
                    }
                }
                if f.state == SyncState::UpToDate && f.name == "Photos" {
                    f.rate_out = 0;
                }
                f
            })
            .collect();
        let devices = vec![
            SyncDevice {
                id: "MFZWI3D-BONSGYC-YLTMRWG-QKZ4XNA".into(),
                name: "Studio iMac".into(),
                client: "Seedwell 0.1.5".into(),
                address: "192.168.68.20:22100".into(),
                via: "LAN".into(),
                folders: folders.iter().filter(|f| f.connected > 0).map(|f| f.id.clone()).collect(),
                rx_bytes: 8_400_000_000,
                tx_bytes: 1_250_000_000,
            },
            SyncDevice {
                id: "P2RFOJE-HTU7WQB-MFZWI3D-BONSGYC".into(),
                name: "Kristian's iPhone".into(),
                client: "Seedwell 0.1.5".into(),
                address: "84.212.33.10:41200".into(),
                via: "Internet".into(),
                folders: folders.iter().filter(|f| f.connected > 1).map(|f| f.id.clone()).collect(),
                rx_bytes: 120_000_000,
                tx_bytes: 3_900_000_000,
            },
            SyncDevice {
                id: "QKZ4XNA-YLTMRWG-P2RFOJE-HTU7WQB".into(),
                name: "Office Mac mini".into(),
                client: "Seedwell 0.1.5".into(),
                address: "relay".into(),
                via: "Relay".into(),
                folders: folders.iter().filter(|f| f.connected > 2).map(|f| f.id.clone()).collect(),
                rx_bytes: 0,
                tx_bytes: 52_000_000,
            },
        ];
        let name = if s.settings.device_name.is_empty() { s.settings.default_device_name.clone() } else { s.settings.device_name.clone() };
        SyncSnapshot { running: true, problem: None, device_id: "HTU7WQB-QKZ4XNA-MFZWI3D-YLTMRWG".into(), device_name: name, port: s.settings.port, folders, devices }
    }

    fn add_folder(&self, path: &str) -> Result<SyncFolder, CoreError> {
        let p = crate::settings::expand_home(path.trim());
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).filter(|n| !n.is_empty()).ok_or_else(|| failed("Choose a folder"))?;
        let path = p.display().to_string();
        if lock(&self.state).folders.iter().any(|m| path.starts_with(&m.f.path) || m.f.path.starts_with(&path)) {
            return Err(failed(format!("{path} overlaps a synced folder")));
        }
        Ok(self.add(name, path, SyncRole::Owner, false))
    }

    fn join(&self, key_or_link: &str, path: &str) -> Result<SyncFolder, CoreError> {
        let (info, _) = parse_link(key_or_link).ok_or_else(|| failed("Not a folder link"))?;
        if info.expired {
            return Err(failed("This link has expired"));
        }
        let path = if path.trim().is_empty() { self.join_path(key_or_link).unwrap_or_default() } else { path.trim().to_owned() };
        let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "Folder".into());
        Ok(self.add(name, path, info.role, true))
    }

    fn join_path(&self, key_or_link: &str) -> Option<String> {
        let (info, _) = parse_link(key_or_link)?;
        let name = info.name.unwrap_or_else(|| info.folder_id.split('-').next().unwrap_or("Folder").to_owned());
        let name: String = name.chars().filter(|c| !c.is_control() && *c != '/' && *c != ':').collect();
        Some(format!("{}/{}", self.default_location(), name.trim().trim_start_matches('.')))
    }

    fn members(&self, id: &str) -> Vec<SyncMember> {
        lock(&self.state).folders.iter().find(|m| m.f.id == id).map(|m| m.members.clone()).unwrap_or_default()
    }

    fn set_role(&self, id: &str, device_id: &str, role: SyncRole) -> Result<(), CoreError> {
        self.with(id, |m| {
            if m.f.role != SyncRole::Owner {
                return Err(failed("Only the folder's owner can change roles"));
            }
            let d = m.members.iter_mut().find(|d| d.device_id == device_id).ok_or_else(|| failed("Not a device of this folder"))?;
            d.role = role;
            d.expires_s = m.f.share_until_s.filter(|_| role != SyncRole::Owner);
            Ok(())
        })?
    }

    fn remove_device(&self, id: &str, device_id: &str) -> Result<(), CoreError> {
        self.with(id, |m| {
            if m.f.role != SyncRole::Owner {
                return Err(failed("Only the folder's owner can change roles"));
            }
            if m.members.iter().any(|d| d.device_id == device_id && d.is_me) {
                return Err(failed("To stop syncing on this Mac, remove the folder instead"));
            }
            m.members.retain(|d| d.device_id != device_id);
            Ok(())
        })?
    }

    fn set_share_until(&self, id: &str, until_s: u64) -> Result<(), CoreError> {
        self.with(id, |m| {
            if m.f.role != SyncRole::Owner {
                return Err(failed("Only the folder's owner can change how long it is shared."));
            }
            if until_s != 0 && until_s <= now_s() {
                return Err(failed("Pick an end in the future."));
            }
            m.f.share_until_s = (until_s != 0).then_some(until_s);
            for d in &mut m.members {
                d.expires_s = m.f.share_until_s.filter(|_| d.role != SyncRole::Owner);
            }
            Ok(())
        })?
    }

    fn create_invite(&self, id: &str, role: SyncRole, max_uses: u32, valid_days: u32) -> Result<SyncInvite, CoreError> {
        self.with(id, |m| {
            match (m.f.role, role) {
                (SyncRole::Owner, _) | (SyncRole::Admin, SyncRole::ReadWrite | SyncRole::ReadOnly) => {}
                (SyncRole::Admin, _) => return Err(failed("Only the folder's owner can invite admins or owners")),
                _ => return Err(failed("Only the folder's owner or an admin can share it")),
            }
            let k = m.invites.len() as u32 + 3;
            let i = mock_invite(m.n, k, &m.f.name, role, max_uses, valid_days);
            m.invites.insert(0, i.clone());
            Ok(i)
        })?
    }

    fn invites(&self, id: &str) -> Vec<SyncInvite> {
        lock(&self.state).folders.iter().find(|m| m.f.id == id).map(|m| m.invites.clone()).unwrap_or_default()
    }

    fn revoke_invite(&self, id: &str, invite_id: &str) -> Result<(), CoreError> {
        self.with(id, |m| {
            let i = m.invites.iter_mut().find(|i| i.id == invite_id).ok_or_else(|| failed("No such link"))?;
            i.revoked = true;
            Ok(())
        })?
    }

    fn rename(&self, id: &str, name: &str) -> Result<SyncFolder, CoreError> {
        let name = name.trim();
        if name.is_empty() || name.contains('/') || name.starts_with('.') {
            return Err(failed("Not a usable folder name"));
        }
        self.with(id, |m| {
            m.f.path = format!("{}/{name}", parent_of(&m.f.path));
            m.f.name = name.to_owned();
            m.f.clone()
        })
    }

    fn move_to(&self, id: &str, path: &str) -> Result<SyncFolder, CoreError> {
        let path = path.trim().to_owned();
        self.with(id, |m| {
            m.f.name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| m.f.name.clone());
            m.f.path = path;
            m.f.clone()
        })
    }

    fn remove(&self, id: &str, _trash_files: bool) -> Result<String, CoreError> {
        let mut s = lock(&self.state);
        let i = s.folders.iter().position(|m| m.f.id == id).ok_or_else(gone)?;
        Ok(s.folders.remove(i).f.path)
    }

    fn pause(&self, id: &str, paused: bool) -> Result<(), CoreError> {
        self.with(id, |m| m.f.state = if paused { SyncState::Paused } else { SyncState::UpToDate })
    }

    fn rescan(&self, id: &str) -> Result<(), CoreError> {
        self.with(id, |_| ())
    }

    fn conflicts(&self, id: &str) -> Vec<SyncConflict> {
        let s = lock(&self.state);
        let Some(m) = s.folders.iter().find(|m| m.f.id == id && m.f.conflicts > 0) else { return Vec::new() };
        vec![conflict_of(
            Path::new(&m.f.path),
            "Plans/Budget 2027 (conflict, Studio iMac, 2026-10-24 18.05).numbers".into(),
            "Plans/Budget 2027.numbers".into(),
        )]
    }

    fn resolve(&self, id: &str, _: &str, _: SyncKeep) -> Result<(), CoreError> {
        self.with(id, |m| m.f.conflicts = m.f.conflicts.saturating_sub(1))
    }

    fn activity(&self, id: &str) -> SyncActivity {
        let s = lock(&self.state);
        let Some(m) = s.folders.iter().find(|m| m.f.id == id) else { return SyncActivity::default() };
        let pending = if m.f.state == SyncState::Syncing { vec!["Scans/2026/Receipts.pdf".into()] } else { Vec::new() };
        let conflicts = if m.f.conflicts > 0 { vec!["Plans/Budget 2027 (conflict, Studio iMac, 2026-10-24 18.05).numbers".into()] } else { Vec::new() };
        SyncActivity { pending, pending_more: false, conflicts, own_ignores: false }
    }

    fn set_lan_only(&self, id: &str, lan_only: bool) -> Result<(), CoreError> {
        self.with(id, |m| m.f.lan_only = lan_only)
    }

    fn settings(&self) -> SyncSettings {
        lock(&self.state).settings.clone()
    }

    fn set_settings(&self, s: &SyncSettings) -> Result<(), CoreError> {
        let mut st = lock(&self.state);
        let port = st.settings.port;
        st.settings = SyncSettings { port, default_device_name: st.settings.default_device_name.clone(), ..s.clone() };
        Ok(())
    }

    fn default_location(&self) -> String {
        default_location_of(&lock(&self.state).settings.default_location)
    }
}

// ------------------------------------------------------------------------------------------ Core

#[uniffi::export]
impl Core {
    /// Opens the core with the Keychain for folder sync's keys (the app's own run). `Core::new` keeps
    /// them in a 0600 file in the state folder instead (tests, `--mock`, isolated QA runs).
    #[uniffi::constructor]
    pub fn with_secret_vault(options: crate::CoreOptions, vault: Arc<dyn SecretVault>) -> Result<Arc<Self>, CoreError> {
        Core::open(options, Some(vault))
    }

    /// Every folder and connected device. Cheap; the menu and the window call it once a second while open.
    pub fn sync_snapshot(&self) -> SyncSnapshot {
        self.backend.sync().snapshot()
    }

    /// Starts sharing a folder on this Mac (this Mac becomes its owner).
    pub fn sync_add_folder(&self, path: String) -> Result<SyncFolder, CoreError> {
        self.backend.sync().add_folder(&path)
    }

    /// Joins a shared folder with an invitation link, at `path` (empty: the default place). Created when
    /// missing. The folder waits until one of its owners or admins is online and admits this Mac.
    pub fn sync_join(&self, key_or_link: String, path: String) -> Result<SyncFolder, CoreError> {
        self.backend.sync().join(&key_or_link, &path)
    }

    /// What a pasted text is: an invitation link (its role, name hint and expiry), or `None` (also for bare
    /// keys of the test builds).
    pub fn sync_parse_link(&self, text: String) -> Option<SyncLinkInfo> {
        let (mut info, _) = parse_link(&text)?;
        info.already_synced = self.backend.sync().snapshot().folders.iter().any(|f| f.id == info.folder_id);
        Some(info)
    }

    /// Why a pasted text is not a link this Mac can use, as a sentence for the add modal ("This link was made by a
    /// newer Seedwell: update Seedwell to use it."); `None` when [`Core::sync_parse_link`] reads it, or the text
    /// is empty.
    pub fn sync_link_problem(&self, text: String) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        InviteLink::parse(&text).err().map(|e| readable(&e.to_string()))
    }

    /// Where a joined folder goes by default: `<default location>/<name>`, numbered when taken.
    pub fn sync_join_path(&self, key_or_link: String) -> Option<String> {
        self.backend.sync().join_path(&key_or_link)
    }

    /// The folder's devices and their roles (this Mac first). Never waits for the folder's core.
    pub fn sync_members(&self, id: String) -> Vec<SyncMember> {
        self.backend.sync().members(&id)
    }

    /// Changes a device's role (owners only; a folder keeps at least one owner).
    pub fn sync_set_role(&self, id: String, device_id: String, role: SyncRole) -> Result<(), CoreError> {
        self.backend.sync().set_role(&id, &device_id, role)
    }

    /// Removes a device from the folder (owners only). It is cut off at once and keeps the files it has.
    pub fn sync_remove_device(&self, id: String, device_id: String) -> Result<(), CoreError> {
        self.backend.sync().remove_device(&id, &device_id)
    }

    /// Sets how long the folder is shared (owners only): `until_s` Unix seconds, 0 = no end. After it every
    /// device but the owners is cut off (it keeps its files) and no link admits anyone; a later end, or none,
    /// lets them back in. An end in the past is refused (to cut a device off now, remove it).
    pub fn sync_set_share_until(&self, id: String, until_s: u64) -> Result<(), CoreError> {
        self.backend.sync().set_share_until(&id, until_s)
    }

    /// Makes an invitation link: `max_uses` 0 = unlimited, `valid_days` (how long the link admits) 0 = no
    /// expiry. Owners invite at any role, admins at Read & write or Read only.
    pub fn sync_create_invite(&self, id: String, role: SyncRole, max_uses: u32, valid_days: u32) -> Result<SyncInvite, CoreError> {
        self.backend.sync().create_invite(&id, role, max_uses, valid_days)
    }

    /// The folder's invitations, newest first (links for those this Mac made). Never waits for the core.
    pub fn sync_invites(&self, id: String) -> Vec<SyncInvite> {
        self.backend.sync().invites(&id)
    }

    /// Revokes an invitation: it admits nobody from now on.
    pub fn sync_revoke_invite(&self, id: String, invite_id: String) -> Result<(), CoreError> {
        self.backend.sync().revoke_invite(&id, &invite_id)
    }

    /// Renames the folder on this Mac (its directory); other devices keep their names.
    pub fn sync_rename(&self, id: String, name: String) -> Result<SyncFolder, CoreError> {
        self.backend.sync().rename(&id, &name)
    }

    /// Moves the folder on this Mac to `path` (the full new path). Blocks while the files move.
    pub fn sync_move(&self, id: String, path: String) -> Result<SyncFolder, CoreError> {
        self.backend.sync().move_to(&id, &path)
    }

    /// Stops syncing the folder on this Mac and returns its path. Other devices keep their copy. With
    /// `trash_files` the app moves the folder to the Trash itself (`FileManager.trashItem`).
    pub fn sync_remove(&self, id: String, trash_files: bool) -> Result<String, CoreError> {
        self.backend.sync().remove(&id, trash_files)
    }

    pub fn sync_pause(&self, id: String, paused: bool) -> Result<(), CoreError> {
        self.backend.sync().pause(&id, paused)
    }

    /// Scans the whole folder now.
    pub fn sync_rescan(&self, id: String) -> Result<(), CoreError> {
        self.backend.sync().rescan(&id)
    }

    /// The conflict copies in a folder (reads its whole index: call when its page opens, not every second).
    pub fn sync_conflicts(&self, id: String) -> Vec<SyncConflict> {
        self.backend.sync().conflicts(&id)
    }

    /// Which paths of a folder are busy, for Finder's badges ([`SyncActivity`]).
    pub fn sync_activity(&self, id: String) -> SyncActivity {
        self.backend.sync().activity(&id)
    }

    /// Resolves a conflict (`copy` from [`SyncConflict::copy`]). Nothing is deleted: the version not
    /// kept goes to the folder's archive.
    pub fn sync_resolve_conflict(&self, id: String, copy: String, keep: SyncKeep) -> Result<(), CoreError> {
        self.backend.sync().resolve(&id, &copy, keep)
    }

    /// Only sync a folder with devices on the local network (or with every device again).
    pub fn sync_set_lan_only(&self, id: String, lan_only: bool) -> Result<(), CoreError> {
        self.backend.sync().set_lan_only(&id, lan_only)
    }

    pub fn sync_settings(&self) -> SyncSettings {
        self.backend.sync().settings()
    }

    /// Saves the device-wide settings. The device name applies at once; LAN discovery at the next start.
    pub fn set_sync_settings(&self, settings: SyncSettings) -> Result<(), CoreError> {
        self.backend.sync().set_settings(&settings)
    }

    /// Where joined folders go: the setting, else `~/Seedwell`.
    pub fn sync_default_location(&self) -> String {
        self.backend.sync().default_location()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_read_as_sentences() {
        assert_eq!(readable("no such folder"), "No such folder.");
        assert_eq!(readable("/x overlaps the synced folder /y"), "/x overlaps the synced folder /y.");
        assert_eq!(readable("Done."), "Done.");
    }

    struct MemVault(Mutex<std::collections::HashMap<String, Vec<u8>>>);
    impl SecretVault for MemVault {
        fn get(&self, name: String) -> Result<Option<Vec<u8>>, VaultError> {
            Ok(lock(&self.0).get(&name).cloned())
        }
        fn put(&self, name: String, value: Vec<u8>) -> Result<(), VaultError> {
            if name.is_empty() {
                return Err(VaultError::Failed { message: "no name".into() });
            }
            lock(&self.0).insert(name, value);
            Ok(())
        }
        fn delete(&self, name: String) -> Result<(), VaultError> {
            lock(&self.0).remove(&name);
            Ok(())
        }
    }

    #[test]
    fn the_vault_serves_as_the_secret_store() {
        let v: Arc<dyn SecretVault> = Arc::new(MemVault(Mutex::new(Default::default())));
        let dir = tempfile::tempdir().unwrap();
        let s = secret_store(Some(v), dir.path());
        s.put("device", b"k").unwrap();
        assert_eq!(s.get("device").unwrap(), Some(b"k".to_vec()));
        s.delete("device").unwrap();
        assert_eq!(s.get("device").unwrap(), None);
        let e = s.put("", b"x").unwrap_err();
        assert!(e.to_string().contains("Keychain: no name"), "{e}");
        assert!(!dir.path().join("sync").exists(), "nothing written to disk");
        // Without a vault: the 0600 file (in `sync/`, which the service makes).
        std::fs::create_dir_all(dir.path().join("sync")).unwrap();
        let f = secret_store(None, dir.path());
        f.put("device", b"k").unwrap();
        assert!(dir.path().join("sync/secrets").exists());
    }

    fn mock_core() -> (Arc<Core>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(crate::CoreOptions {
            config_dir: dir.path().display().to_string(),
            mock: true,
            allow_loopback_trackers: false,
            forbidden_root: None,
        })
        .unwrap();
        (core, dir)
    }

    #[test]
    fn the_mock_serves_folders_in_every_state() {
        let (core, _d) = mock_core();
        let s = core.sync_snapshot();
        assert!(s.running && s.problem.is_none());
        let states: Vec<SyncState> = s.folders.iter().map(|f| f.state).collect();
        for want in [SyncState::UpToDate, SyncState::Syncing, SyncState::WaitingForDevices, SyncState::Missing] {
            assert!(states.contains(&want), "{want:?} in {states:?}");
        }
        assert!(s.folders.iter().any(|f| f.conflicts > 0 && f.access == SyncAccess::ReadWrite));
        assert!(s.folders.iter().any(|f| f.access == SyncAccess::ReadOnly));
        let syncing = s.folders.iter().find(|f| f.state == SyncState::Syncing).unwrap();
        assert!(syncing.rate_in > 0 && syncing.need_bytes > 0 && syncing.need_bytes < syncing.bytes);
        assert!(s.devices.iter().any(|d| d.via == "LAN") && s.devices.iter().any(|d| d.via == "Relay"));
        let projects = s.folders.iter().find(|f| f.conflicts > 0).unwrap();
        let c = core.sync_conflicts(projects.id.clone());
        assert_eq!(c.len(), 1);
        assert!(c[0].path.contains("(conflict, "));
        assert!(core.sync_conflicts(s.folders[0].id.clone()).is_empty());
    }

    /// A mock folder where this Mac has `role`.
    fn s_folder(core: &Core, role: SyncRole) -> String {
        core.sync_snapshot().folders.into_iter().find(|f| f.role == role).map(|f| f.id).expect("a mock folder with that role")
    }

    #[test]
    fn the_mock_adds_joins_edits_and_removes() {
        let (core, _d) = mock_core();
        let n = core.sync_snapshot().folders.len();
        let f = core.sync_add_folder("/tmp/seedwell-mock/Notes".into()).unwrap();
        assert_eq!((f.name.as_str(), f.access), ("Notes", SyncAccess::ReadWrite));
        assert!(core.sync_add_folder("/tmp/seedwell-mock/Notes/Inner".into()).is_err(), "overlap");
        assert_eq!(f.role, SyncRole::Owner);
        let rw = core.sync_create_invite(f.id.clone(), SyncRole::ReadWrite, 1, 7).unwrap().link;
        let ro = core.sync_create_invite(f.id.clone(), SyncRole::ReadOnly, 0, 0).unwrap();
        assert!(rw.starts_with("seedwell://sync/") && rw.ends_with("?name=Notes"), "{rw}");
        assert_eq!((ro.max_uses, ro.expires_s), (0, None));
        assert_eq!(core.sync_invites(f.id.clone()).len(), 2);
        let info = core.sync_parse_link(rw.clone()).unwrap();
        assert_eq!((info.role, info.name.as_deref(), info.expired), (SyncRole::ReadWrite, Some("Notes"), false));
        assert!(info.expires_s.is_some());
        assert_eq!(core.sync_parse_link(ro.link.clone()).unwrap().role, SyncRole::ReadOnly);
        assert!(core.sync_parse_link("hello".into()).is_none());
        assert!(core.sync_parse_link("SWW-AAAA".into()).is_none(), "bare keys are no longer links");
        assert_eq!(core.sync_link_problem(rw.clone()), None);
        assert_eq!(core.sync_link_problem("  ".into()), None);
        assert_eq!(core.sync_link_problem("hello".into()).as_deref(), Some("Not a Seedwell folder link."));
        assert_eq!(core.sync_link_problem("SWW-AAAA".into()).as_deref(), Some("Folder keys are no longer shared: ask for a link to the folder."));
        assert_eq!(core.sync_link_problem(format!("{}X", &rw[..40])).as_deref(), Some("The folder link is damaged or incomplete."));
        let place = core.sync_join_path(rw.clone()).unwrap();
        assert!(place.ends_with("/Seedwell/Notes"), "{place}");
        let j = core.sync_join(ro.link.clone(), "/tmp/seedwell-mock/Joined".into()).unwrap();
        assert_eq!((j.access, j.state), (SyncAccess::ReadOnly, SyncState::WaitingForAdmin));
        // Roles: the mock's own device is the owner of what it adds.
        let members = core.sync_members(f.id.clone());
        assert!(members[0].is_me && members[0].role == SyncRole::Owner);
        // How long it is shared: the owner's setting, shown on the folder and as non-owners' access end.
        let end = now_s() + 7 * 86_400;
        assert!(core.sync_set_share_until(f.id.clone(), now_s() - 1).is_err(), "an end in the past");
        core.sync_set_share_until(f.id.clone(), end).unwrap();
        assert_eq!(core.sync_snapshot().folders.iter().find(|x| x.id == f.id).unwrap().share_until_s, Some(end));
        assert_eq!(core.sync_members(f.id.clone())[0].expires_s, None, "an owner's access does not end");
        let admin = s_folder(&core, SyncRole::Admin);
        let e = core.sync_set_share_until(admin, end).unwrap_err();
        assert!(e.to_string().contains("Only the folder's owner"), "{e}");
        core.sync_set_share_until(f.id.clone(), 0).unwrap();
        assert_eq!(core.sync_snapshot().folders.iter().find(|x| x.id == f.id).unwrap().share_until_s, None);
        core.sync_revoke_invite(f.id.clone(), ro.id.clone()).unwrap();
        assert!(core.sync_invites(f.id.clone()).iter().any(|i| i.id == ro.id && i.revoked));
        let reader = s_folder(&core, SyncRole::ReadOnly);
        assert!(core.sync_create_invite(reader, SyncRole::ReadOnly, 1, 7).is_err(), "a reader cannot share");
        assert!(matches!(core.sync_join("nope".into(), String::new()), Err(CoreError::Failed { .. })));
        let r = core.sync_rename(f.id.clone(), "Notebook".into()).unwrap();
        assert_eq!(r.path, "/tmp/seedwell-mock/Notebook");
        assert!(core.sync_rename(f.id.clone(), ".hidden".into()).is_err());
        let m = core.sync_move(f.id.clone(), "/tmp/elsewhere/Notebook".into()).unwrap();
        assert_eq!(m.path, "/tmp/elsewhere/Notebook");
        core.sync_pause(f.id.clone(), true).unwrap();
        assert_eq!(core.sync_snapshot().folders.iter().find(|x| x.id == f.id).unwrap().state, SyncState::Paused);
        core.sync_rescan(f.id.clone()).unwrap();
        assert_eq!(core.sync_remove(f.id.clone(), true).unwrap(), "/tmp/elsewhere/Notebook");
        assert_eq!(core.sync_snapshot().folders.len(), n + 1, "the joined one stays");
        let e = core.sync_remove(f.id.clone(), false).unwrap_err();
        assert_eq!(e.to_string(), "This folder is no longer synced on this Mac.");
        let mut st = core.sync_settings();
        assert!(st.lan_discovery && st.relay_own_devices && !st.relay_everyone && st.port > 0);
        st.device_name = "Desk".into();
        st.up_limit = 2_000_000;
        core.set_sync_settings(st.clone()).unwrap();
        assert_eq!(core.sync_settings(), st);
        assert_eq!(core.sync_snapshot().device_name, "Desk");
        assert!(core.sync_default_location().ends_with("/Seedwell"));
    }

    #[test]
    fn the_real_core_runs_folder_sync() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nothing else here reads the environment.
        unsafe { std::env::set_var("HOME", home.path()) };
        let dir = work.path().join("state");
        let store = crate::SettingsStore::new(&dir);
        let mut st = store.load_settings();
        st.folders.download_dir = work.path().join("done").display().to_string();
        st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
        st.folders.watch.clear();
        st.folders.torrent_after_add = crate::TorrentFileAction::Leave;
        st.network.port_mapping = false;
        st.network.dht = false;
        store.save_settings(&st).unwrap();
        let core = Core::new(crate::CoreOptions { config_dir: dir.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
        let t0 = Instant::now();
        while !core.sync_snapshot().running {
            assert!(t0.elapsed().as_secs() < 20, "sync did not start: {:?}", core.sync_snapshot().problem);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let s = core.sync_snapshot();
        assert!(s.port > 0 && !s.device_id.is_empty() && s.folders.is_empty());
        // Keys are in the state folder's 0600 file without a vault, never in sync.toml.
        assert!(dir.join("sync/secrets").exists());

        let docs = work.path().join("Docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), b"hello").unwrap();
        let f = core.sync_add_folder(docs.display().to_string()).unwrap();
        assert_eq!((f.name.as_str(), f.access), ("Docs", SyncAccess::ReadWrite));
        assert!(docs.join(".seedwell").is_dir());
        let toml = std::fs::read_to_string(dir.join("sync.toml")).unwrap();
        assert_eq!(f.role, SyncRole::Owner);
        let inv = core.sync_create_invite(f.id.clone(), SyncRole::ReadWrite, 1, 7).unwrap();
        let link = inv.link.clone();
        assert!(!toml.contains(&link[16..]), "no link in sync.toml");
        let info = core.sync_parse_link(link.clone()).unwrap();
        assert!(info.already_synced && info.name.as_deref() == Some("Docs"));
        assert_eq!(core.sync_invites(f.id.clone())[0].id, inv.id);
        let members = core.sync_members(f.id.clone());
        assert_eq!(members.len(), 1);
        assert!(members[0].is_me && members[0].role == SyncRole::Owner && members[0].online);
        assert!(core.sync_set_role(f.id.clone(), members[0].device_id.clone(), SyncRole::ReadOnly).is_err(), "the only owner stays one");
        let e = core.sync_set_share_until(f.id.clone(), 1).unwrap_err();
        assert!(e.to_string().contains("Pick an end in the future."), "{e}");
        let end = now_s() + 3 * 86_400;
        core.sync_set_share_until(f.id.clone(), end).unwrap();
        assert_eq!(core.sync_snapshot().folders[0].share_until_s, Some(end));
        core.sync_set_share_until(f.id.clone(), 0).unwrap();
        assert_eq!(core.sync_snapshot().folders[0].share_until_s, None);
        let e = core.sync_join(link, work.path().join("Again").display().to_string()).unwrap_err();
        assert!(e.to_string().contains("already synced"), "{e}");
        assert!(core.sync_add_folder(home.path().join("x").display().to_string()).is_err(), "never the real home under test");

        let r = core.sync_rename(f.id.clone(), "Papers".into()).unwrap();
        assert_eq!(r.path, work.path().join("Papers").display().to_string());
        assert!(work.path().join("Papers/a.txt").exists() && !docs.exists());
        core.sync_pause(f.id.clone(), true).unwrap();
        assert_eq!(core.sync_snapshot().folders[0].state, SyncState::Paused);
        core.sync_pause(f.id.clone(), false).unwrap();
        core.sync_rescan(f.id.clone()).unwrap();
        assert!(core.sync_conflicts(f.id.clone()).is_empty());

        let mut st = core.sync_settings();
        assert!(!st.default_device_name.is_empty());
        st.device_name = "Test Mac".into();
        st.down_limit = 1024 * 1024;
        core.set_sync_settings(st).unwrap();
        let back = core.sync_settings();
        assert_eq!((back.device_name.as_str(), back.down_limit), ("Test Mac", 1024 * 1024));
        assert_eq!(core.sync_snapshot().device_name, "Test Mac");
        assert_eq!(core.sync_default_location(), home.path().join("Seedwell").display().to_string());

        let path = core.sync_remove(f.id.clone(), false).unwrap();
        assert_eq!(path, work.path().join("Papers").display().to_string());
        assert!(work.path().join("Papers/a.txt").exists(), "the files stay");
        assert!(core.sync_snapshot().folders.is_empty());
        assert!(core.sync_create_invite(f.id, SyncRole::ReadOnly, 1, 7).is_err());
        assert!(core.shutdown(5).finished);
    }
}
