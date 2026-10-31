//! Folder sync and channels for the app (SPEC 7G): the records the Swift shell shows, the Keychain bridge for the
//! keys ([`SecretVault`]), and [`SyncBackend`], which the real backend (a `trs_sync_net::SyncHandle` next
//! to the engine) and the mock (a few fake folders and channels for design work) both implement. The membership
//! calls (`sync_members`, `sync_set_role`, …) take a folder's or a channel's id.
//!
//! The service starts in the background after the engine; until it is up (or when it failed) the
//! snapshot says so and every change is refused with a plain sentence. A sync failure never stops a
//! torrent.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use trs_control::sync::{SyncSlot, default_join_path, folder_path, start_sync_with};
use trs_engine::EngineHandle;
use trs_sync_net::chat::{AgentAccess, ChannelStatus, MessageInfo, MessageKind};
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
    /// The channel that goes with it on this Mac ([`Core::sync_set_channel`], or a link for both).
    pub channel_id: Option<String>,
    /// On demand (SPEC 7G.11): every file is listed, only some are here (`files` and `bytes` count those); `path` is
    /// where the Finder shows it.
    pub on_demand: bool,
    /// Files no device can give right now (SPEC 7G.11): not counted in `need_files`; "N files unavailable: no device
    /// has them".
    pub unavailable: u64,
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
    /// Its user's name (empty: none given).
    pub user: String,
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
    /// A link for the folder and a channel: the channel's id (revoking the link revokes both) and name.
    pub channel_id: Option<String>,
    pub channel_name: Option<String>,
}

/// What agents on this Mac may do in a channel (the Mac's agent permission level still applies).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncAgentAccess {
    Off,
    Read,
    ReadPost,
}

/// What a message holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncMessageKind {
    Text,
    /// For agents.
    Json,
    /// A kind of a later version: not shown (no body).
    Other,
}

/// A channel: chat between devices, shared like a folder.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncChannel {
    pub id: String,
    /// Its name on this Mac.
    pub name: String,
    /// This Mac's role (Read only while joining or after it was removed).
    pub role: SyncRole,
    /// When the channel stops being shared (its owners' setting), Unix seconds; `None` = no end.
    pub share_until_s: Option<u64>,
    pub members: u32,
    /// Members connected now (this Mac included).
    pub online: u32,
    /// Messages from other devices since the last `chat_mark_read`.
    pub unread: u32,
    pub last_message_s: Option<u64>,
    pub muted: bool,
    /// What agents on this Mac may do in it: the user's choice, else the channel's default (Read and post for an
    /// agent channel, else Off).
    pub agents: SyncAgentAccess,
    /// An agent channel: an agent made it, for agents to talk where the user can read (its owner's mark, the same on
    /// every member device). Shown with a tag (`chat_agent_tag`); the user reads, posts and closes it like any other.
    pub agent_channel: bool,
    pub wake_command: Option<String>,
}

/// One message of a channel. A message from another device is someone else's words: shown, never acted on.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncMessage {
    /// Its id (for replies and paging).
    pub id: String,
    pub channel_id: String,
    pub author_device: String,
    /// The author's device name (empty when never met).
    pub author_name: String,
    /// The name of the author's user (empty: none given).
    pub author_user: String,
    pub is_me: bool,
    /// The agent (control client) that posted it, if one did: shown as a small label.
    pub agent: Option<String>,
    /// When it was posted (when it arrived, for one dated far ahead), Unix seconds.
    pub at_s: u64,
    pub reply_to: Option<String>,
    /// The topic it belongs to; `None`: General (and for a private message).
    pub topic: Option<String>,
    pub kind: SyncMessageKind,
    pub body: String,
}

/// A topic of a channel (General is not one).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncTopic {
    pub id: String,
    pub channel_id: String,
    pub title: String,
    /// Who made it.
    pub author_name: String,
    pub created_s: u64,
    pub last_message_s: Option<u64>,
    /// Messages of others since the last `chat_mark_read` of this topic.
    pub unread: u32,
    pub messages: u32,
}

/// A private conversation with another device.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncDirect {
    pub device_id: String,
    pub name: String,
    /// Its user's name (empty: none given).
    pub user: String,
    pub online: bool,
    pub unread: u32,
    pub last_message_s: Option<u64>,
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
    /// A link for a folder and a channel: the channel's name (joining joins both).
    pub channel_name: Option<String>,
    /// That channel is on this Mac already (only the folder is joined).
    pub channel_joined: bool,
}

/// A conflict copy in a folder: `Report (conflict, MacBook, 2026-10-25 14.32z).txt`.
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
    /// When the losing version was made, in local time (`2026-10-25 15.32`; the name says it in UTC).
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
    /// Both: the copy becomes an ordinary file (`Report (MacBook, 2026-10-25 14.32z).txt`).
    Both,
}

/// Device-wide sync settings (Settings › Folder Sync).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncSettings {
    /// The name other devices see; empty = the computer's name.
    pub device_name: String,
    /// The computer's name (the placeholder of an empty device name).
    pub default_device_name: String,
    /// The name this Mac's user goes by in chats, shown with the device name; empty = none.
    pub user_name: String,
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

#[path = "sync_ondemand.rs"]
pub mod ondemand;

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
    fn create_invite(&self, id: &str, role: SyncRole, max_uses: u32, valid_days: u32, channel_id: Option<&str>) -> Result<SyncInvite, CoreError>;
    fn set_channel(&self, id: &str, channel_id: Option<&str>) -> Result<(), CoreError>;
    fn invites(&self, id: &str) -> Vec<SyncInvite>;
    fn revoke_invite(&self, id: &str, invite_id: &str) -> Result<(), CoreError>;
    fn rename(&self, id: &str, name: &str) -> Result<SyncFolder, CoreError>;
    fn move_to(&self, id: &str, path: &str) -> Result<SyncFolder, CoreError>;
    /// Stops syncing; returns where the folder is (`Core::sync_remove` then moves it to the Trash when asked to).
    fn remove(&self, id: &str) -> Result<String, CoreError>;
    /// Where a folder's files are on this computer, when not at its place (an on-demand folder's cache).
    fn files_path(&self, _id: &str) -> Option<String> {
        None
    }
    /// How many of a folder's files no other device is known to hold (reads its whole index).
    fn only_here(&self, _id: &str) -> u64 {
        0
    }
    /// [`Self::remove`] for a folder whose files go to the Trash next, from where they are (an on-demand folder's
    /// from beside its cache, under its name); then [`Self::finish_removal`].
    fn remove_for_trash(&self, id: &str) -> Result<String, CoreError> {
        self.remove(id)
    }
    /// After [`Self::remove_for_trash`]: the files went to the Trash, or not (then an on-demand folder's go to its
    /// place as an ordinary folder). Returns where they are.
    fn finish_removal(&self, files: &str, place: &str, trashed: bool) -> Result<String, CoreError> {
        Ok(if trashed { place } else { files }.to_string())
    }
    fn pause(&self, id: &str, paused: bool) -> Result<(), CoreError>;
    fn rescan(&self, id: &str) -> Result<(), CoreError>;
    fn conflicts(&self, id: &str) -> Vec<SyncConflict>;
    fn activity(&self, id: &str) -> SyncActivity;
    fn resolve(&self, id: &str, copy: &str, keep: SyncKeep) -> Result<(), CoreError>;
    fn set_lan_only(&self, id: &str, lan_only: bool) -> Result<(), CoreError>;
    fn settings(&self) -> SyncSettings;
    fn set_settings(&self, s: &SyncSettings) -> Result<(), CoreError>;
    /// Moves sync to `port` now (SPEC 7G.5): refused at once when the port is taken; otherwise the service
    /// starts again on it in the background (a few seconds).
    fn set_port(&self, port: u16) -> Result<(), CoreError>;
    /// Whether the router opened the sync port. Cheap: the settings page polls it.
    fn port_reachability(&self) -> Option<crate::types::ReachabilityInfo>;
    fn default_location(&self) -> String;
    /// Every channel, by name. Cheap: called once a second.
    fn chat_channels(&self) -> Vec<SyncChannel>;
    fn chat_new(&self, name: &str) -> Result<SyncChannel, CoreError>;
    fn chat_join(&self, link: &str) -> Result<SyncChannel, CoreError>;
    /// `limit` messages before `before` (or the newest), oldest first. Cheap: called on scroll.
    fn chat_messages(&self, id: &str, topic: Option<&str>, before: Option<&str>, limit: u32) -> Vec<SyncMessage>;
    fn chat_send(&self, id: &str, topic: Option<&str>, body: &str, reply_to: Option<&str>) -> Result<SyncMessage, CoreError>;
    fn chat_mark_read(&self, id: &str, topic: Option<&str>);
    /// Newest activity first. Cheap.
    fn chat_topics(&self, id: &str) -> Vec<SyncTopic>;
    /// On demand (SPEC 7G.11, [`ondemand`]): a folder full or on demand.
    fn set_on_demand(&self, _id: &str, _on: bool, _path: Option<&str>) -> Result<SyncFolder, CoreError> {
        Err(ondemand::not_on_demand().error)
    }
    fn join_on_demand(&self, _link: &str, _place: &str) -> Result<SyncFolder, CoreError> {
        Err(ondemand::not_on_demand().error)
    }
    fn od_item(&self, _id: &str, _item: &str) -> Result<Option<ondemand::SyncItem>, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_children(&self, _id: &str, _item: &str, _cursor: Option<&str>, _limit: u32) -> Result<ondemand::SyncItemPage, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_changes(&self, _id: &str, _anchor: u64, _limit: u32) -> Result<ondemand::SyncChangePage, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    /// The file manager's host of on-demand folders (the app's File Provider server), asked to let go of a folder
    /// before it is removed or switched to full, whoever asks.
    fn set_on_demand_host(&self, _host: Option<Arc<dyn trs_sync_net::OnDemandHost>>) {}
    /// The anchor for "now": changes from it report only what comes after (File Provider's current sync anchor).
    fn od_anchor(&self, _id: &str) -> Result<u64, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_fetch(&self, _id: &str, _item: &str, _wait_ms: u64) -> Result<ondemand::SyncFetch, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_cancel(&self, _id: &str, _item: &str) -> Result<(), ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_pin(&self, _id: &str, _item: &str, _on: bool) -> Result<ondemand::SyncItem, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_evict(&self, _id: &str, _item: &str) -> Result<ondemand::SyncEvicted, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_create(&self, _id: &str, _parent: &str, _name: &str, _dir: bool, _contents: Option<&str>) -> Result<ondemand::SyncItem, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    fn od_modify(&self, _id: &str, _item: &str, _parent: Option<&str>, _name: Option<&str>, _contents: Option<&str>, _base: Option<&str>) -> Result<ondemand::SyncItem, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    /// A delete of what the host saw: `seen` is the folder's version then (`od_newest_below`).
    fn od_delete(&self, _id: &str, _item: &str, _seen: Option<trs_sync_net::DeleteSeen>) -> Result<ondemand::SyncDeleted, ondemand::OdError> {
        Err(ondemand::not_on_demand())
    }
    /// Adds a folder of this computer on demand, whole or not at all (the mock: added, then switched).
    fn add_folder_on_demand(&self, path: &str) -> Result<SyncFolder, CoreError> {
        let f = self.add_folder(path)?;
        self.set_on_demand(&f.id, true, None)
    }
    /// Whether a folder is in the configuration, and on demand (`Some(None)`: it is not); `None` while folder sync
    /// is not running (nothing can be said).
    fn configured(&self, id: &str) -> Option<Option<bool>> {
        Some(self.snapshot().folders.iter().find(|f| f.id == id).map(|f| f.on_demand))
    }

    fn chat_new_topic(&self, id: &str, title: &str) -> Result<SyncTopic, CoreError>;
    /// Every private conversation, newest first. Cheap.
    fn chat_directs(&self) -> Vec<SyncDirect>;
    fn chat_direct_messages(&self, device_id: &str, before: Option<&str>, limit: u32) -> Vec<SyncMessage>;
    fn chat_send_direct(&self, device_id: &str, body: &str, reply_to: Option<&str>) -> Result<SyncMessage, CoreError>;
    fn chat_mark_direct_read(&self, device_id: &str);
    fn chat_delete_direct(&self, device_id: &str) -> Result<(), CoreError>;
    fn chat_direct_agents(&self) -> SyncAgentAccess;
    fn chat_set_direct_agents(&self, access: SyncAgentAccess);
    fn chat_set_muted(&self, id: &str, muted: bool);
    fn chat_set_agents(&self, id: &str, access: SyncAgentAccess);
    fn chat_set_wake_command(&self, id: &str, command: Option<&str>) -> Result<(), CoreError>;
    fn chat_leave(&self, id: &str) -> Result<(), CoreError>;
    fn chat_rename(&self, id: &str, name: &str) -> Result<(), CoreError>;
}

/// Why a private message cannot be written now (`SyncDirect::online` is false).
const DIRECT_OFFLINE: &str =
    "Private messages go only over a direct connection between the two devices, never through another device. You can write once both are online and can reach each other.";

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
    CoreError::Failed { message: concat!("This folder is no longer synced on this ", trs_platform::computer!(), ".").into() }
}

fn sync_err(e: SyncError) -> CoreError {
    match e {
        SyncError::NotFound => gone(),
        SyncError::Invalid(why) | SyncError::OnDemand(_, why) => failed(trs_control::redact::scrub_keys(&why)),
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
        channel_id: i.channel_id.clone(),
        channel_name: i.channel_name.clone(),
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
        channel_id: f.channel_id.clone(),
        on_demand: f.on_demand,
        unavailable: f.unavailable,
    }
}

/// A pasted invitation link, decoded (nothing is joined). Bare keys of the test builds are not links.
pub fn parse_link(text: &str) -> Option<(SyncLinkInfo, InviteLink)> {
    let (link, chat) = InviteLink::parse_bundle(text).ok()?;
    let i = &link.invite;
    let now = now_s();
    let info = SyncLinkInfo {
        role: role_of(i.role),
        name: link.name.clone(),
        folder_id: i.folder_id().to_string(),
        already_synced: false,
        // A link for both stops admitting when either does.
        expires_s: [Some(i), chat.as_ref().map(|c| &c.invite)].into_iter().flatten().filter(|i| i.expires_s != 0).map(|i| i.expires_s).min(),
        expired: i.expired(now) || chat.as_ref().is_some_and(|c| c.invite.expired(now)),
        channel_name: chat.as_ref().map(|c| c.name.clone().unwrap_or_else(|| "Channel".into())),
        channel_joined: false,
    };
    Some((info, link))
}

/// The channel's id of a link for a folder and a channel.
fn bundled_channel(text: &str) -> Option<String> {
    InviteLink::parse_bundle(text).ok()?.1.map(|c| c.invite.folder_id().to_string())
}

/// A conflict copy of a folder at `root` as the app shows it (`copy` and `original` in wire form).
fn agents_of(a: AgentAccess) -> SyncAgentAccess {
    match a {
        AgentAccess::Off => SyncAgentAccess::Off,
        AgentAccess::Read => SyncAgentAccess::Read,
        AgentAccess::ReadPost => SyncAgentAccess::ReadPost,
    }
}

fn agents_from(a: SyncAgentAccess) -> AgentAccess {
    match a {
        SyncAgentAccess::Off => AgentAccess::Off,
        SyncAgentAccess::Read => AgentAccess::Read,
        SyncAgentAccess::ReadPost => AgentAccess::ReadPost,
    }
}

fn channel_of(c: &ChannelStatus) -> SyncChannel {
    SyncChannel {
        id: c.id.clone(),
        name: c.name.clone(),
        role: c.role.map_or(SyncRole::ReadOnly, role_of),
        share_until_s: c.share_until,
        members: c.members,
        online: c.online,
        unread: c.unread,
        last_message_s: c.last_message_s,
        muted: c.muted,
        agents: agents_of(c.agents),
        agent_channel: c.agent_channel,
        wake_command: c.wake_command.clone(),
    }
}

fn message_of(m: &MessageInfo) -> SyncMessage {
    SyncMessage {
        id: m.id.clone(),
        channel_id: m.channel_id.clone(),
        author_device: m.author_device.clone(),
        author_name: m.author_name.clone(),
        author_user: m.author_user.clone(),
        is_me: m.me,
        agent: m.agent.clone(),
        at_s: m.at,
        reply_to: m.reply_to.clone(),
        topic: m.topic.clone(),
        kind: match m.kind.as_str() {
            "text" => SyncMessageKind::Text,
            "json" => SyncMessageKind::Json,
            _ => SyncMessageKind::Other,
        },
        body: m.body.clone(),
    }
}

fn topic_of(t: &trs_sync_net::chat::TopicStatus) -> SyncTopic {
    SyncTopic {
        id: t.id.clone(),
        channel_id: t.channel_id.clone(),
        title: t.title.clone(),
        author_name: t.author_name.clone(),
        created_s: t.created_s,
        last_message_s: t.last_message_s,
        unread: t.unread,
        messages: t.messages,
    }
}

fn direct_of(d: &trs_sync_net::direct::DirectStatus) -> SyncDirect {
    SyncDirect { device_id: d.device.to_string(), name: d.name.clone(), user: d.user.clone(), online: d.online, unread: d.unread, last_message_s: d.last_message_s }
}

/// A pasted channel link, decoded (nothing is joined).
pub fn parse_chat_link(text: &str) -> Option<SyncLinkInfo> {
    let link = InviteLink::parse_chat(text).ok()?;
    let i = &link.invite;
    Some(SyncLinkInfo {
        role: role_of(i.role),
        name: link.name.clone(),
        folder_id: i.folder_id().to_string(),
        already_synced: false,
        expires_s: (i.expires_s != 0).then_some(i.expires_s),
        expired: i.expired(now_s()),
        channel_name: None,
        channel_joined: false,
    })
}

fn conflict_of(root: &Path, copy: String, original: String) -> SyncConflict {
    let name = trs_sync::conflict::parse_conflict(&copy);
    SyncConflict {
        path: root.join(&copy).display().to_string(),
        copy,
        original,
        device: name.as_ref().map(|n| n.device.clone()).unwrap_or_default(),
        when: name.map(|n| match n.utc_s {
            Some(t) => trs_sync::clock::local_stamp(t, trs_engine::util::local_utc_offset(t)),
            None => n.stamp,
        })
        .unwrap_or_default(),
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
    engine: EngineHandle,
    /// Why the service is not running, once its start failed.
    problem: Arc<Mutex<Option<String>>>,
    default_name: String,
    /// The file manager's host of on-demand folders, kept across restarts of the service (`set_on_demand_host`).
    od_host: trs_sync_net::OnDemandHostCell,
}

fn is_test_run() -> bool {
    cfg!(test) || std::env::current_exe().ok().and_then(|p| p.parent().and_then(|d| d.file_name()).map(|n| n == "deps")).unwrap_or(false)
}

impl RealSync {
    /// Starts the service in the background: on all interfaces, IPv4 and IPv6 (`::`), LAN discovery on,
    /// the engine's DHT and port mapping lent to it. Tests bind to loopback and stay off the LAN.
    /// `forbidden_root`: an isolated (test) run's real home: Folder Sync starts LAN only, keeps joined folders in the
    /// run's state and refuses folders inside it (`trs_sync_net::service::Options::forbidden_root`).
    pub fn start(
        rt: &tokio::runtime::Handle,
        engine: &EngineHandle,
        dir: &Path,
        vault: Option<Arc<dyn SecretVault>>,
        forbidden_root: Option<PathBuf>,
    ) -> Arc<Self> {
        let slot = SyncSlot::new();
        let problem = Arc::new(Mutex::new(None));
        let default_name = trs_control::sync::default_device_name();
        let od_host: trs_sync_net::OnDemandHostCell = Arc::new(Mutex::new(None));
        let me = Arc::new(RealSync {
            rt: rt.clone(),
            slot: slot.clone(),
            engine: engine.clone(),
            problem: problem.clone(),
            default_name: default_name.clone(),
            od_host: od_host.clone(),
        });
        let secrets = secret_store(vault, dir);
        let (engine, dir) = (engine.clone(), dir.to_path_buf());
        // The same start again when the port changes (`SyncSlot::move_port`).
        let starter: trs_control::sync::Starter = Arc::new(move || {
            let (dir, secrets, engine, default_name, forbidden_root, od_host) =
                (dir.clone(), secrets.clone(), engine.clone(), default_name.clone(), forbidden_root.clone(), od_host.clone());
            Box::pin(async move {
                let client = format!("Seedwell {}", env!("CARGO_PKG_VERSION"));
                start_sync_with(&dir, client, secrets, Some(&engine), |o| {
                    o.default_device_name = default_name;
                    o.forbidden_root = forbidden_root;
                    o.on_demand_host = od_host;
                    if is_test_run() {
                        o.bind = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
                        o.lan_discovery = false;
                    } else {
                        o.bind = std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);
                    }
                })
                .await
            })
        });
        slot.set_starter(starter.clone());
        rt.spawn(async move {
            let started = starter().await;
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

    /// A disk was mounted or unmounted: a folder waiting for the disk with its original files is tried again (in the
    /// background).
    pub fn volumes_changed(&self) {
        if let Some(h) = self.slot.get() {
            self.rt.spawn(async move { h.volumes_changed().await });
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
    fn set_on_demand(&self, id: &str, on: bool, path: Option<&str>) -> Result<SyncFolder, CoreError> {
        self.od_set_mode(id, on, path)
    }
    fn join_on_demand(&self, link: &str, place: &str) -> Result<SyncFolder, CoreError> {
        self.od_join(link, place)
    }
    fn od_item(&self, id: &str, item: &str) -> Result<Option<ondemand::SyncItem>, ondemand::OdError> {
        RealSync::od_item(self, id, item)
    }
    fn od_children(&self, id: &str, item: &str, cursor: Option<&str>, limit: u32) -> Result<ondemand::SyncItemPage, ondemand::OdError> {
        RealSync::od_children(self, id, item, cursor, limit)
    }
    fn od_changes(&self, id: &str, anchor: u64, limit: u32) -> Result<ondemand::SyncChangePage, ondemand::OdError> {
        RealSync::od_changes(self, id, anchor, limit)
    }
    fn od_anchor(&self, id: &str) -> Result<u64, ondemand::OdError> {
        RealSync::od_anchor(self, id)
    }
    fn set_on_demand_host(&self, host: Option<Arc<dyn trs_sync_net::OnDemandHost>>) {
        *lock(&self.od_host) = host;
    }
    fn add_folder_on_demand(&self, path: &str) -> Result<SyncFolder, CoreError> {
        let h = self.handle()?;
        let p = folder_path(path, "the folder").map_err(api_err)?;
        let f = self.rt.block_on(h.add_folder_on_demand(p)).map_err(sync_err)?;
        Ok(folder_of(&f))
    }
    fn configured(&self, id: &str) -> Option<Option<bool>> {
        let h = self.slot.get()?;
        Some(folder_id(id).ok().and_then(|fid| h.configured(&fid)))
    }
    fn od_fetch(&self, id: &str, item: &str, wait_ms: u64) -> Result<ondemand::SyncFetch, ondemand::OdError> {
        RealSync::od_fetch(self, id, item, wait_ms)
    }
    fn od_cancel(&self, id: &str, item: &str) -> Result<(), ondemand::OdError> {
        RealSync::od_cancel(self, id, item)
    }
    fn od_pin(&self, id: &str, item: &str, on: bool) -> Result<ondemand::SyncItem, ondemand::OdError> {
        RealSync::od_pin(self, id, item, on)
    }
    fn od_evict(&self, id: &str, item: &str) -> Result<ondemand::SyncEvicted, ondemand::OdError> {
        RealSync::od_evict(self, id, item)
    }
    fn od_create(&self, id: &str, parent: &str, name: &str, dir: bool, contents: Option<&str>) -> Result<ondemand::SyncItem, ondemand::OdError> {
        RealSync::od_create(self, id, parent, name, dir, contents)
    }
    fn od_modify(&self, id: &str, item: &str, parent: Option<&str>, name: Option<&str>, contents: Option<&str>, base: Option<&str>) -> Result<ondemand::SyncItem, ondemand::OdError> {
        RealSync::od_modify(self, id, item, parent, name, contents, base)
    }
    fn od_delete(&self, id: &str, item: &str, seen: Option<trs_sync_net::DeleteSeen>) -> Result<ondemand::SyncDeleted, ondemand::OdError> {
        RealSync::od_delete(self, id, item, seen)
    }


    fn chat_channels(&self) -> Vec<SyncChannel> {
        self.slot.get().map(|h| h.chat_channels().iter().map(channel_of).collect()).unwrap_or_default()
    }

    fn chat_new(&self, name: &str) -> Result<SyncChannel, CoreError> {
        let h = self.handle()?;
        let c = self.rt.block_on(h.chat_new(name)).map_err(sync_err)?;
        Ok(channel_of(&c))
    }

    fn chat_join(&self, link: &str) -> Result<SyncChannel, CoreError> {
        let h = self.handle()?;
        let c = self.rt.block_on(h.chat_join(link)).map_err(sync_err)?;
        Ok(channel_of(&c))
    }

    fn chat_messages(&self, id: &str, topic: Option<&str>, before: Option<&str>, limit: u32) -> Vec<SyncMessage> {
        let (Some(h), Ok(id)) = (self.slot.get(), folder_id(id)) else { return Vec::new() };
        h.chat_messages(&id, topic, before, limit.clamp(1, 1000) as usize).map(|v| v.iter().map(message_of).collect()).unwrap_or_default()
    }

    fn chat_send(&self, id: &str, topic: Option<&str>, body: &str, reply_to: Option<&str>) -> Result<SyncMessage, CoreError> {
        let h = self.handle()?;
        let id = folder_id(id)?;
        // The app posts as the person at this Mac: no agent label.
        let m = self.rt.block_on(h.chat_send(&id, topic, MessageKind::Text, body, reply_to, None)).map_err(sync_err)?;
        Ok(message_of(&m))
    }

    fn chat_mark_read(&self, id: &str, topic: Option<&str>) {
        if let (Some(h), Ok(id)) = (self.slot.get(), folder_id(id)) {
            let _ = h.chat_mark_read(&id, topic);
        }
    }

    fn chat_topics(&self, id: &str) -> Vec<SyncTopic> {
        let (Some(h), Ok(id)) = (self.slot.get(), folder_id(id)) else { return Vec::new() };
        h.chat_topics(&id).map(|v| v.iter().map(topic_of).collect()).unwrap_or_default()
    }

    fn chat_new_topic(&self, id: &str, title: &str) -> Result<SyncTopic, CoreError> {
        let h = self.handle()?;
        let id = folder_id(id)?;
        let t = self.rt.block_on(h.chat_new_topic(&id, title, None)).map_err(sync_err)?;
        Ok(topic_of(&t))
    }

    fn chat_directs(&self) -> Vec<SyncDirect> {
        self.slot.get().map(|h| h.chat_directs().iter().map(direct_of).collect()).unwrap_or_default()
    }

    fn chat_direct_messages(&self, device_id: &str, before: Option<&str>, limit: u32) -> Vec<SyncMessage> {
        let (Some(h), Ok(dev)) = (self.slot.get(), device_id.parse::<trs_sync_proto::ids::DeviceId>()) else { return Vec::new() };
        h.chat_direct_messages(&dev, before, limit.clamp(1, 1000) as usize).map(|v| v.iter().map(message_of).collect()).unwrap_or_default()
    }

    fn chat_send_direct(&self, device_id: &str, body: &str, reply_to: Option<&str>) -> Result<SyncMessage, CoreError> {
        let h = self.handle()?;
        let dev = device_id.parse().map_err(|_| failed("Not a device"))?;
        let m = self.rt.block_on(h.chat_send_direct(&dev, MessageKind::Text, body, reply_to, None)).map_err(sync_err)?;
        Ok(message_of(&m))
    }

    fn chat_mark_direct_read(&self, device_id: &str) {
        if let (Some(h), Ok(dev)) = (self.slot.get(), device_id.parse()) {
            let _ = h.chat_mark_direct_read(&dev);
        }
    }

    fn chat_delete_direct(&self, device_id: &str) -> Result<(), CoreError> {
        let h = self.handle()?;
        let dev = device_id.parse().map_err(|_| failed("Not a device"))?;
        h.chat_delete_direct(&dev).map_err(sync_err)
    }

    fn chat_direct_agents(&self) -> SyncAgentAccess {
        self.slot.get().map_or(SyncAgentAccess::Off, |h| agents_of(h.chat_direct_agents()))
    }

    fn chat_set_direct_agents(&self, access: SyncAgentAccess) {
        if let Some(h) = self.slot.get() {
            let _ = h.chat_set_direct_agents(agents_from(access));
        }
    }

    fn chat_set_muted(&self, id: &str, muted: bool) {
        if let (Some(h), Ok(id)) = (self.slot.get(), folder_id(id)) {
            let _ = h.chat_set_muted(&id, muted);
        }
    }

    fn chat_set_agents(&self, id: &str, access: SyncAgentAccess) {
        if let (Some(h), Ok(id)) = (self.slot.get(), folder_id(id)) {
            let _ = h.chat_set_agents(&id, agents_from(access));
        }
    }

    fn chat_set_wake_command(&self, id: &str, command: Option<&str>) -> Result<(), CoreError> {
        let h = self.handle()?;
        h.chat_set_wake_command(&folder_id(id)?, command).map_err(sync_err)
    }

    fn chat_leave(&self, id: &str) -> Result<(), CoreError> {
        let h = self.handle()?;
        h.chat_leave(&folder_id(id)?).map_err(sync_err)
    }

    fn chat_rename(&self, id: &str, name: &str) -> Result<(), CoreError> {
        let h = self.handle()?;
        h.chat_rename(&folder_id(id)?, name).map_err(sync_err)
    }

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
        if let Err(e) = InviteLink::parse_bundle(key_or_link) {
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
                user: m.user,
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

    fn create_invite(&self, id: &str, role: SyncRole, max_uses: u32, valid_days: u32, channel_id: Option<&str>) -> Result<SyncInvite, CoreError> {
        let h = self.handle()?;
        let i = match channel_id {
            None => h.create_invite(&folder_id(id)?, role_from(role), max_uses, valid_days),
            Some(c) => h.create_bundle_invite(&folder_id(id)?, role_from(role), max_uses, valid_days, &folder_id(c)?),
        }
        .map_err(sync_err)?;
        Ok(invite_of(&i))
    }

    fn set_channel(&self, id: &str, channel_id: Option<&str>) -> Result<(), CoreError> {
        let h = self.handle()?;
        let channel = channel_id.map(folder_id).transpose()?;
        h.set_folder_channel(&folder_id(id)?, channel.as_ref()).map_err(sync_err)
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

    fn remove(&self, id: &str) -> Result<String, CoreError> {
        let h = self.handle()?;
        let fid = folder_id(id)?;
        let path = self.rt.block_on(h.remove_folder(&fid)).map_err(sync_err)?;
        Ok(path.display().to_string())
    }

    fn files_path(&self, id: &str) -> Option<String> {
        let (Ok(h), Ok(fid)) = (self.handle(), folder_id(id)) else { return None };
        h.folder_files(&fid).map(|p| p.display().to_string())
    }

    fn only_here(&self, id: &str) -> u64 {
        let (Ok(h), Ok(fid)) = (self.handle(), folder_id(id)) else { return 0 };
        h.only_here(&fid).unwrap_or(0)
    }

    fn remove_for_trash(&self, id: &str) -> Result<String, CoreError> {
        let h = self.handle()?;
        let fid = folder_id(id)?;
        let path = self.rt.block_on(h.remove_folder_for_trash(&fid)).map_err(sync_err)?;
        Ok(path.display().to_string())
    }

    fn finish_removal(&self, files: &str, place: &str, trashed: bool) -> Result<String, CoreError> {
        let h = self.handle()?;
        let at = self.rt.block_on(h.finish_removal(std::path::Path::new(files), std::path::Path::new(place), trashed)).map_err(sync_err)?;
        Ok(at.display().to_string())
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
            user_name: s.user_name,
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
        st.user_name = s.user_name.clone();
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

    fn set_port(&self, port: u16) -> Result<(), CoreError> {
        use trs_control::sync::PortChange;
        if port < 1024 {
            return Err(failed(PortChange::Invalid.to_string()));
        }
        let h = self.handle()?;
        if h.port() == port {
            return Ok(());
        }
        if !trs_control::sync::port_free(port) {
            return Err(failed(PortChange::InUse(port).to_string()));
        }
        let (slot, problem) = (self.slot.clone(), self.problem.clone());
        self.rt.spawn(async move {
            if let Err(e) = slot.move_port(port).await {
                eprintln!("seedwell: folder sync did not move to port {port}: {e}");
                *lock(&problem) = Some(readable(&e.to_string()));
            }
        });
        Ok(())
    }

    fn port_reachability(&self) -> Option<crate::types::ReachabilityInfo> {
        let h = self.slot.get()?;
        let r = trs_control::sync_reachability_dto(&self.engine.reachability(), h.port());
        Some(crate::types::ReachabilityInfo { state: r.state, summary: r.summary })
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
    fn chat_channels(&self) -> Vec<SyncChannel> {
        Vec::new()
    }
    fn chat_new(&self, _: &str) -> Result<SyncChannel, CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_join(&self, _: &str) -> Result<SyncChannel, CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_messages(&self, _: &str, _: Option<&str>, _: Option<&str>, _: u32) -> Vec<SyncMessage> {
        Vec::new()
    }
    fn chat_send(&self, _: &str, _: Option<&str>, _: &str, _: Option<&str>) -> Result<SyncMessage, CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_mark_read(&self, _: &str, _: Option<&str>) {}
    fn chat_topics(&self, _: &str) -> Vec<SyncTopic> {
        Vec::new()
    }
    fn chat_new_topic(&self, _: &str, _: &str) -> Result<SyncTopic, CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_directs(&self) -> Vec<SyncDirect> {
        Vec::new()
    }
    fn chat_direct_messages(&self, _: &str, _: Option<&str>, _: u32) -> Vec<SyncMessage> {
        Vec::new()
    }
    fn chat_send_direct(&self, _: &str, _: &str, _: Option<&str>) -> Result<SyncMessage, CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_mark_direct_read(&self, _: &str) {}
    fn chat_delete_direct(&self, _: &str) -> Result<(), CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_direct_agents(&self) -> SyncAgentAccess {
        SyncAgentAccess::Off
    }
    fn chat_set_direct_agents(&self, _: SyncAgentAccess) {}
    fn chat_set_muted(&self, _: &str, _: bool) {}
    fn chat_set_agents(&self, _: &str, _: SyncAgentAccess) {}
    fn chat_set_wake_command(&self, _: &str, _: Option<&str>) -> Result<(), CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_leave(&self, _: &str) -> Result<(), CoreError> {
        Err(failed("Channels are not available"))
    }
    fn chat_rename(&self, _: &str, _: &str) -> Result<(), CoreError> {
        Err(failed("Channels are not available"))
    }
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
    fn set_channel(&self, _: &str, _: Option<&str>) -> Result<(), CoreError> {
        Err(failed("Folder sync is not available"))
    }
    fn create_invite(&self, _: &str, _: SyncRole, _: u32, _: u32, _: Option<&str>) -> Result<SyncInvite, CoreError> {
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
    fn remove(&self, _: &str) -> Result<String, CoreError> {
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
    fn set_port(&self, _: u16) -> Result<(), CoreError> {
        Err(failed("Folder sync is not available"))
    }
    fn port_reachability(&self) -> Option<crate::types::ReachabilityInfo> {
        None
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
    /// A channel: its messages and settings (its name, role and sharing are in `f`).
    chat: Option<MockChat>,
}

/// Makes a mock folder record: (n, name, access, state, members, connected).
type MockMaker<'a> = dyn Fn(u32, &str, SyncAccess, SyncState, u32, u32) -> SyncFolder + 'a;

struct MockChat {
    messages: Vec<SyncMessage>,
    /// The ids of the messages read.
    read: std::collections::HashSet<String>,
    /// Its topics (their counts are worked out from the messages).
    topics: Vec<SyncTopic>,
    muted: bool,
    agents: SyncAgentAccess,
    /// Made by an agent.
    agent_channel: bool,
    wake: Option<String>,
}

/// The ids of the first `n` messages: read.
fn first_read(messages: &[SyncMessage], n: usize) -> std::collections::HashSet<String> {
    messages.iter().take(n).map(|m| m.id.clone()).collect()
}

struct MockDirect {
    device_id: String,
    name: String,
    online: bool,
    messages: Vec<SyncMessage>,
    read: usize,
}

struct MockState {
    folders: Vec<MockFolder>,
    /// Channels, kept like folders so the membership calls work on them too.
    channels: Vec<MockFolder>,
    directs: Vec<MockDirect>,
    direct_agents: SyncAgentAccess,
    settings: SyncSettings,
    next: u32,
}

/// A few fake folders in every state the UI draws, for design work and the snapshot renderer. Nothing on
/// disk is touched; the syncing folder moves along with the clock.
pub struct MockSync {
    state: Mutex<MockState>,
    started: Instant,
    /// On-demand folders' items ([`ondemand::MockOd`]), made when a folder is made on demand.
    od: Mutex<std::collections::BTreeMap<String, ondemand::MockOd>>,
}

fn mock_id(n: u32) -> String {
    let groups = ["MFZWI3D", "BONSGYC", "YLTMRWG", "QKZ4XNA", "P2RFOJE", "HTU7WQB"];
    format!("{}-{}-{}-{n:07}", groups[n as usize % 6], groups[(n as usize + 1) % 6], groups[(n as usize + 2) % 6])
}

/// A real invitation link to mock folder (or channel) `n` (nothing listens for it).
fn mock_invite(n: u32, k: u32, name: &str, role: SyncRole, max_uses: u32, valid_days: u32, chat: bool) -> SyncInvite {
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
    let l = InviteLink { invite, token_seed: seed, name: Some(name.to_owned()) };
    let link = if chat { l.format_chat() } else { l.format() };
    SyncInvite {
        id: trs_sync_proto::members::hex(&id),
        link,
        role,
        max_uses,
        uses: 0,
        expires_s: (expires_s != 0).then_some(expires_s),
        created_s: now,
        revoked: false,
        channel_id: None,
        channel_name: None,
    }
}

/// A mock device's user: "Ola" for "Ola's MacBook Air", none for "Studio iMac".
fn mock_user(device: &str) -> String {
    device.split_once("'s ").map(|(u, _)| u.to_string()).unwrap_or_default()
}

/// The mock devices of a folder: this Mac at `mine`, and `others` (name, role, online).
fn mock_members(mine: SyncRole, others: &[(&str, SyncRole, bool)], share_until_s: Option<u64>) -> Vec<SyncMember> {
    let ends = |role: SyncRole| share_until_s.filter(|_| role != SyncRole::Owner);
    let mut out = vec![SyncMember {
        device_id: "HTU7WQB-QKZ4XNA-MFZWI3D-YLTMRWG".into(),
        name: "Kristian's MacBook Pro".into(),
        user: "Kristian".into(),
        role: mine,
        online: true,
        is_me: true,
        expires_s: ends(mine),
    }];
    for (i, (name, role, online)) in others.iter().enumerate() {
        let groups = ["MFZWI3D-BONSGYC-YLTMRWG-QKZ4XNA", "P2RFOJE-HTU7WQB-MFZWI3D-BONSGYC", "QKZ4XNA-YLTMRWG-P2RFOJE-HTU7WQB"];
        out.push(SyncMember { device_id: groups[i % 3].into(), name: (*name).into(), user: mock_user(name), role: *role, online: *online, is_me: false, expires_s: ends(*role) });
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
            channel_id: None,
            on_demand: false,
            unavailable: 0,
        };
        let mut photos = mk(1, "Photos", SyncAccess::ReadWrite, SyncState::UpToDate, 3, 3);
        photos.role = SyncRole::Owner;
        // A folder with a chat: the Family channel goes with it.
        photos.channel_id = Some(mock_id(11));
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
                    let mut used = mock_invite(n, 1, &f.name, SyncRole::ReadWrite, 1, 7, false);
                    used.uses = 1;
                    vec![mock_invite(n, 2, &f.name, SyncRole::ReadOnly, 5, 30, false), used]
                } else {
                    Vec::new()
                };
                MockFolder { n, f, members, invites, chat: None }
            })
            .collect();
        let channels = Self::mock_channels(&mk);
        let now = now_s();
        let ola = "QKZ4XNA-YLTMRWG-P2RFOJE-HTU7WQB";
        let pm = |id: &str, from_me: bool, ago: u64, body: &str| SyncMessage {
            id: id.into(),
            channel_id: String::new(),
            author_device: if from_me { "HTU7WQB-QKZ4XNA-MFZWI3D-YLTMRWG".into() } else { ola.into() },
            author_name: if from_me { "Kristian's MacBook Pro".into() } else { "Ola's MacBook Air".into() },
            author_user: if from_me { "Kristian".into() } else { "Ola".into() },
            is_me: from_me,
            agent: None,
            at_s: now.saturating_sub(ago),
            reply_to: None,
            topic: None,
            kind: SyncMessageKind::Text,
            body: body.into(),
        };
        let directs = vec![MockDirect {
            device_id: ola.into(),
            name: "Ola's MacBook Air".into(),
            online: false,
            messages: vec![
                pm(&"e1".repeat(16), false, 5400, "Kan vi ta budsjettet på tomannshånd?"),
                pm(&"e2".repeat(16), true, 5000, "Ja, i morgen kl. 10?"),
                pm(&"e3".repeat(16), false, 1200, "Passer fint 👍"),
            ],
            read: 2,
        }];
        let state = MockState { folders, channels, directs, direct_agents: SyncAgentAccess::Off, settings: Self::default_settings(), next: 20 };
        Arc::new(MockSync { state: Mutex::new(state), started: Instant::now(), od: Mutex::new(std::collections::BTreeMap::new()) })
    }

    /// Three channels: one with an agent's answer and a reply to it, one where agents read and post, one muted.
    fn mock_channels(mk: &MockMaker) -> Vec<MockFolder> {
        let now = now_s();
        let me = "HTU7WQB-QKZ4XNA-MFZWI3D-YLTMRWG";
        let msg = |id: &str, ch: &str, who: &str, name: &str, agent: Option<&str>, ago: u64, reply: Option<&str>, kind: SyncMessageKind, body: &str| SyncMessage {
            id: id.into(),
            channel_id: ch.into(),
            author_device: who.into(),
            author_name: name.into(),
            author_user: mock_user(name),
            is_me: who == me,
            agent: agent.map(str::to_string),
            at_s: now.saturating_sub(ago),
            reply_to: reply.map(str::to_string),
            topic: None,
            kind,
            body: body.into(),
        };
        let in_topic = |mut m: SyncMessage, t: &str| {
            m.topic = Some(t.to_string());
            m
        };
        let (kari, studio) = ("MFZWI3D-BONSGYC-YLTMRWG-QKZ4XNA", "P2RFOJE-HTU7WQB-MFZWI3D-BONSGYC");
        let mut out = Vec::new();
        let mut family = mk(11, "Family", SyncAccess::ReadWrite, SyncState::UpToDate, 3, 2);
        family.role = SyncRole::Owner;
        let fid = family.id.clone();
        // General, and a topic with an agent's answer and a reply to it.
        let dinner = "d1".repeat(16);
        let messages = vec![
            msg("a1".repeat(16).as_str(), &fid, kari, "Kari's iPhone", None, 3 * 3600, None, SyncMessageKind::Text, "Middag på lørdag? Noen som kan bestille bord?"),
            msg("a2".repeat(16).as_str(), &fid, me, "Kristian's MacBook Pro", None, 3 * 3600 - 300, None, SyncMessageKind::Text, "Jeg spør agenten min. Se tråden «Lørdag»."),
            in_topic(msg(
                "a3".repeat(16).as_str(),
                &fid,
                me,
                "Kristian's MacBook Pro",
                Some("claude-code"),
                3 * 3600 - 240,
                Some(&"a1".repeat(16)),
                SyncMessageKind::Text,
                "Booked a table for 4 at Lofoten Fiskerestaurant, Saturday 18:00. The confirmation is in Shared/Dinner.pdf.",
            ), &dinner),
            in_topic(msg("a4".repeat(16).as_str(), &fid, kari, "Kari's iPhone", None, 2 * 3600, Some(&"a3".repeat(16)), SyncMessageKind::Text, "Perfekt, takk! 🙌"), &dinner),
            in_topic(msg("a5".repeat(16).as_str(), &fid, studio, "Studio iMac", None, 600, None, SyncMessageKind::Text, "Jeg kan kjøre."), &dinner),
            msg("a6".repeat(16).as_str(), &fid, studio, "Studio iMac", None, 300, None, SyncMessageKind::Text, "Noen som vet om butikken er åpen søndag?"),
        ];
        let topics = vec![SyncTopic {
            id: dinner.clone(),
            channel_id: fid.clone(),
            title: "Lørdag".into(),
            author_name: "Kristian's MacBook Pro".into(),
            created_s: now.saturating_sub(3 * 3600 - 280),
            last_message_s: None,
            unread: 0,
            messages: 0,
        }];
        let members = mock_members(SyncRole::Owner, &[("Kari's iPhone", SyncRole::ReadWrite, true), ("Studio iMac", SyncRole::Admin, false)], None);
        out.push(MockFolder {
            n: 11,
            f: family,
            members,
            invites: vec![mock_invite(11, 1, "Family", SyncRole::ReadWrite, 1, 7, true)],
            chat: Some(MockChat { read: first_read(&messages, 3), messages, topics, muted: false, agents: SyncAgentAccess::Off, agent_channel: false, wake: None }),
        });
        let mut build = mk(12, "Release notes", SyncAccess::ReadWrite, SyncState::UpToDate, 2, 2);
        build.role = SyncRole::Admin;
        let bid = build.id.clone();
        let messages = vec![
            msg("b1".repeat(16).as_str(), &bid, studio, "Studio iMac", Some("ci-bot"), 7200, None, SyncMessageKind::Json, r#"{"build":"0.1.6","status":"passed","tests":446}"#),
            msg(
                "b2".repeat(16).as_str(),
                &bid,
                me,
                "Kristian's MacBook Pro",
                Some("Seedwell"),
                7000,
                Some(&"b1".repeat(16)),
                SyncMessageKind::Text,
                "## Release notes drafted\n\nFrom the merged pull requests, in `docs/notes.md`:\n\n1. **Folder Sync**: links, roles, Finder badges\n2. **Chats**: topics and private messages\n\nTo check them:\n\n```sh\nseedwell chat read \"Release notes\" --project Seedwell\n```",
            ),
            msg("b3".repeat(16).as_str(), &bid, studio, "Studio iMac", Some("Website"), 6800, None, SyncMessageKind::Text, "Thanks. I'll link them from the download page.\n\n- [ ] screenshot\n- [x] changelog"),
        ];
        out.push(MockFolder {
            n: 12,
            f: build,
            members: mock_members(SyncRole::Admin, &[("Studio iMac", SyncRole::Owner, true)], None),
            invites: Vec::new(),
            chat: Some(MockChat {
                read: first_read(&messages, 2),
                messages,
                topics: Vec::new(),
                muted: false,
                agents: SyncAgentAccess::ReadPost,
                agent_channel: true,
                wake: Some("~/bin/wake-claude".into()),
            }),
        });
        let mut ola = mk(13, "Ola & Kristian", SyncAccess::ReadWrite, SyncState::UpToDate, 2, 0);
        ola.role = SyncRole::ReadWrite;
        ola.share_until_s = Some(now + 5 * 86_400);
        let oid = ola.id.clone();
        let messages = vec![msg("c1".repeat(16).as_str(), &oid, kari, "Ola's MacBook Air", None, 86_400, None, SyncMessageKind::Text, "Har du sett på tegningene?")];
        out.push(MockFolder {
            n: 13,
            f: ola,
            members: mock_members(SyncRole::ReadWrite, &[("Ola's MacBook Air", SyncRole::Owner, false)], Some(now + 5 * 86_400)),
            invites: Vec::new(),
            chat: Some(MockChat { read: first_read(&messages, 0), messages, topics: Vec::new(), muted: true, agents: SyncAgentAccess::Off, agent_channel: false, wake: None }),
        });
        out
    }

    fn default_settings() -> SyncSettings {
        SyncSettings {
            device_name: String::new(),
            default_device_name: "Kristian's MacBook Pro".into(),
            user_name: "Kristian".into(),
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
        let s = &mut *s;
        s.folders.iter_mut().chain(s.channels.iter_mut()).find(|m| m.f.id == id).map(f).ok_or_else(gone)
    }

    fn channel_of(m: &MockFolder) -> Option<SyncChannel> {
        let c = m.chat.as_ref()?;
        Some(SyncChannel {
            id: m.f.id.clone(),
            name: m.f.name.clone(),
            role: m.f.role,
            share_until_s: m.f.share_until_s,
            members: m.members.len() as u32,
            online: m.members.iter().filter(|d| d.online).count() as u32,
            unread: c.messages.iter().filter(|x| !x.is_me && !c.read.contains(&x.id)).count() as u32,
            last_message_s: c.messages.last().map(|x| x.at_s),
            muted: c.muted,
            agents: c.agents,
            agent_channel: c.agent_channel,
            wake_command: c.wake.clone(),
        })
    }

    fn with_chat<T>(&self, id: &str, f: impl FnOnce(&mut MockFolder) -> T) -> Result<T, CoreError> {
        let mut s = lock(&self.state);
        s.channels.iter_mut().find(|m| m.f.id == id).map(f).ok_or_else(|| failed(concat!("This channel is no longer on this ", trs_platform::computer!())))
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
            channel_id: None,
            on_demand: false,
            unavailable: 0,
        };
        let members = mock_members(if joining { SyncRole::ReadOnly } else { role }, &[], None);
        s.folders.push(MockFolder { n, f: f.clone(), members, invites: Vec::new(), chat: None });
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
    fn set_on_demand(&self, id: &str, on: bool, _path: Option<&str>) -> Result<SyncFolder, CoreError> {
        self.od_set_mode(id, on)
    }
    fn join_on_demand(&self, link: &str, place: &str) -> Result<SyncFolder, CoreError> {
        ondemand::mock_join(self, link, place)
    }
    fn od_item(&self, id: &str, item: &str) -> Result<Option<ondemand::SyncItem>, ondemand::OdError> {
        MockSync::od_item(self, id, item)
    }
    fn od_children(&self, id: &str, item: &str, _cursor: Option<&str>, _limit: u32) -> Result<ondemand::SyncItemPage, ondemand::OdError> {
        MockSync::od_children(self, id, item)
    }
    fn od_changes(&self, id: &str, anchor: u64, _limit: u32) -> Result<ondemand::SyncChangePage, ondemand::OdError> {
        MockSync::od_changes(self, id, anchor)
    }
    fn od_anchor(&self, id: &str) -> Result<u64, ondemand::OdError> {
        MockSync::od_changes(self, id, u64::MAX).map(|p| p.anchor)
    }
    fn od_fetch(&self, id: &str, item: &str, _wait_ms: u64) -> Result<ondemand::SyncFetch, ondemand::OdError> {
        MockSync::od_fetch(self, id, item)
    }
    fn od_cancel(&self, id: &str, item: &str) -> Result<(), ondemand::OdError> {
        MockSync::od_item(self, id, item).map(|_| ())
    }
    fn od_pin(&self, id: &str, item: &str, on: bool) -> Result<ondemand::SyncItem, ondemand::OdError> {
        MockSync::od_pin(self, id, item, on)
    }
    fn od_evict(&self, id: &str, item: &str) -> Result<ondemand::SyncEvicted, ondemand::OdError> {
        MockSync::od_evict(self, id, item)
    }
    fn od_create(&self, id: &str, parent: &str, name: &str, dir: bool, _contents: Option<&str>) -> Result<ondemand::SyncItem, ondemand::OdError> {
        MockSync::od_create(self, id, parent, name, dir)
    }
    fn od_modify(&self, id: &str, item: &str, parent: Option<&str>, name: Option<&str>, _contents: Option<&str>, _base: Option<&str>) -> Result<ondemand::SyncItem, ondemand::OdError> {
        self.od_rename(id, item, parent, name)
    }
    fn od_delete(&self, id: &str, item: &str, _seen: Option<trs_sync_net::DeleteSeen>) -> Result<ondemand::SyncDeleted, ondemand::OdError> {
        MockSync::od_delete(self, id, item)
    }


    fn chat_channels(&self) -> Vec<SyncChannel> {
        let mut out: Vec<SyncChannel> = lock(&self.state).channels.iter().filter_map(Self::channel_of).collect();
        out.sort_by_key(|c| c.name.to_lowercase());
        out
    }

    fn chat_new(&self, name: &str) -> Result<SyncChannel, CoreError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(failed("A channel needs a name"));
        }
        let mut s = lock(&self.state);
        let n = s.next;
        s.next += 1;
        let mut f = s.folders.first().map(|m| m.f.clone()).ok_or_else(gone)?;
        f.id = mock_id(n);
        f.name = name.into();
        f.role = SyncRole::Owner;
        f.share_until_s = None;
        let m = MockFolder {
            n,
            f,
            members: mock_members(SyncRole::Owner, &[], None),
            invites: Vec::new(),
            chat: Some(MockChat { messages: Vec::new(), read: Default::default(), topics: Vec::new(), muted: false, agents: SyncAgentAccess::Off, agent_channel: false, wake: None }),
        };
        let c = Self::channel_of(&m).ok_or_else(gone)?;
        s.channels.push(m);
        Ok(c)
    }

    fn chat_join(&self, link: &str) -> Result<SyncChannel, CoreError> {
        InviteLink::parse_chat(link).map_err(|e| failed(e.to_string()))?;
        let info = parse_chat_link(link).ok_or_else(|| failed("Not a channel link"))?;
        if lock(&self.state).channels.iter().any(|m| m.f.id == info.folder_id) {
            return Err(failed(concat!("This channel is already on this ", trs_platform::computer!())));
        }
        let mut c = self.chat_new(info.name.as_deref().unwrap_or("Channel"))?;
        self.with_chat(&c.id, |m| m.f.role = SyncRole::ReadOnly)?;
        c.role = SyncRole::ReadOnly;
        Ok(c)
    }

    fn chat_messages(&self, id: &str, topic: Option<&str>, before: Option<&str>, limit: u32) -> Vec<SyncMessage> {
        self.with_chat(id, |m| {
            let all: Vec<SyncMessage> = m.chat.as_ref().map(|c| c.messages.iter().filter(|x| x.topic.as_deref() == topic).cloned().collect()).unwrap_or_default();
            let end = before.and_then(|b| all.iter().position(|x| x.id == b)).unwrap_or(all.len());
            all[end.saturating_sub(limit as usize)..end].to_vec()
        })
        .unwrap_or_default()
    }

    fn chat_topics(&self, id: &str) -> Vec<SyncTopic> {
        self.with_chat(id, |m| {
            let Some(c) = m.chat.as_ref() else { return Vec::new() };
            let mut out: Vec<SyncTopic> = c
                .topics
                .iter()
                .map(|t| {
                    let msgs: Vec<&SyncMessage> = c.messages.iter().filter(|x| x.topic.as_deref() == Some(t.id.as_str())).collect();
                    SyncTopic {
                        last_message_s: msgs.last().map(|x| x.at_s),
                        unread: msgs.iter().filter(|x| !x.is_me && !c.read.contains(&x.id)).count() as u32,
                        messages: msgs.len() as u32,
                        ..t.clone()
                    }
                })
                .collect();
            out.sort_by_key(|t| std::cmp::Reverse(t.last_message_s.unwrap_or(t.created_s)));
            out
        })
        .unwrap_or_default()
    }

    fn chat_new_topic(&self, id: &str, title: &str) -> Result<SyncTopic, CoreError> {
        let title = title.trim();
        if title.is_empty() {
            return Err(failed("A topic needs a title"));
        }
        self.with_chat(id, |m| {
            if m.f.role == SyncRole::ReadOnly {
                return Err(failed("Read only members of a channel cannot post"));
            }
            let channel_id = m.f.id.clone();
            let c = m.chat.as_mut().ok_or_else(gone)?;
            let t = SyncTopic {
                id: format!("{:032x}", c.topics.len() as u128 + 1 + now_s() as u128 * 1000),
                channel_id,
                title: title.into(),
                author_name: "Kristian's MacBook Pro".into(),
                created_s: now_s(),
                last_message_s: None,
                unread: 0,
                messages: 0,
            };
            c.topics.push(t.clone());
            Ok(t)
        })?
    }

    fn chat_directs(&self) -> Vec<SyncDirect> {
        let mut out: Vec<SyncDirect> = lock(&self.state)
            .directs
            .iter()
            .map(|d| SyncDirect {
                device_id: d.device_id.clone(),
                name: d.name.clone(),
                user: mock_user(&d.name),
                online: d.online,
                unread: d.messages.iter().skip(d.read).filter(|m| !m.is_me).count() as u32,
                last_message_s: d.messages.last().map(|m| m.at_s),
            })
            .collect();
        out.sort_by_key(|d| std::cmp::Reverse(d.last_message_s));
        out
    }

    fn chat_direct_messages(&self, device_id: &str, before: Option<&str>, limit: u32) -> Vec<SyncMessage> {
        let s = lock(&self.state);
        let Some(d) = s.directs.iter().find(|d| d.device_id == device_id) else { return Vec::new() };
        let end = before.and_then(|b| d.messages.iter().position(|x| x.id == b)).unwrap_or(d.messages.len());
        d.messages[end.saturating_sub(limit as usize)..end].to_vec()
    }

    fn chat_send_direct(&self, device_id: &str, body: &str, reply_to: Option<&str>) -> Result<SyncMessage, CoreError> {
        if body.trim().is_empty() {
            return Err(failed("Nothing to send"));
        }
        let mut s = lock(&self.state);
        if s.directs.iter().any(|d| d.device_id == device_id && !d.online) {
            return Err(failed(readable(DIRECT_OFFLINE)));
        }
        // Any member of a mock channel can be written to.
        let name = s
            .channels
            .iter()
            .flat_map(|c| c.members.iter())
            .find(|m| m.device_id == device_id && !m.is_me)
            .map(|m| m.name.clone())
            .ok_or_else(|| failed(concat!("This ", trs_platform::computer!(), " shares no channel with that device")))?;
        let msg = SyncMessage {
            id: format!("{:032x}", now_s() as u128 * 1000 + 7),
            channel_id: String::new(),
            author_device: "HTU7WQB-QKZ4XNA-MFZWI3D-YLTMRWG".into(),
            author_name: "Kristian's MacBook Pro".into(),
            author_user: "Kristian".into(),
            is_me: true,
            agent: None,
            at_s: now_s(),
            reply_to: reply_to.map(str::to_string),
            topic: None,
            kind: SyncMessageKind::Text,
            body: body.into(),
        };
        if !s.directs.iter().any(|d| d.device_id == device_id) {
            s.directs.push(MockDirect { device_id: device_id.into(), name, online: true, messages: Vec::new(), read: 0 });
        }
        let d = s.directs.iter_mut().find(|d| d.device_id == device_id).ok_or_else(gone)?;
        d.messages.push(msg.clone());
        d.read = d.messages.len();
        Ok(msg)
    }

    fn chat_mark_direct_read(&self, device_id: &str) {
        if let Some(d) = lock(&self.state).directs.iter_mut().find(|d| d.device_id == device_id) {
            d.read = d.messages.len();
        }
    }

    fn chat_delete_direct(&self, device_id: &str) -> Result<(), CoreError> {
        let mut s = lock(&self.state);
        let before = s.directs.len();
        s.directs.retain(|d| d.device_id != device_id);
        if s.directs.len() == before { Err(failed(concat!("There is no conversation with that device on this ", trs_platform::computer!()))) } else { Ok(()) }
    }

    fn chat_direct_agents(&self) -> SyncAgentAccess {
        lock(&self.state).direct_agents
    }

    fn chat_set_direct_agents(&self, access: SyncAgentAccess) {
        lock(&self.state).direct_agents = access;
    }

    fn chat_send(&self, id: &str, topic: Option<&str>, body: &str, reply_to: Option<&str>) -> Result<SyncMessage, CoreError> {
        if body.trim().is_empty() {
            return Err(failed("Nothing to send"));
        }
        self.with_chat(id, |m| {
            if m.f.role == SyncRole::ReadOnly {
                return Err(failed("Read only members of a channel cannot post"));
            }
            let c = m.chat.as_mut().ok_or_else(gone)?;
            let msg = SyncMessage {
                id: format!("{:032x}", c.messages.len() as u128 + now_s() as u128 * 1000),
                channel_id: m.f.id.clone(),
                author_device: "HTU7WQB-QKZ4XNA-MFZWI3D-YLTMRWG".into(),
                author_name: "Kristian's MacBook Pro".into(),
                author_user: "Kristian".into(),
                is_me: true,
                agent: None,
                at_s: now_s(),
                reply_to: reply_to.map(str::to_string),
                topic: topic.map(str::to_string),
                kind: SyncMessageKind::Text,
                body: body.into(),
            };
            c.messages.push(msg.clone());
            c.read.insert(msg.id.clone());
            Ok(msg)
        })?
    }

    fn chat_mark_read(&self, id: &str, topic: Option<&str>) {
        let _ = self.with_chat(id, |m| {
            if let Some(c) = m.chat.as_mut() {
                let ids: Vec<String> = c.messages.iter().filter(|x| x.topic.as_deref() == topic).map(|x| x.id.clone()).collect();
                c.read.extend(ids);
            }
        });
    }

    fn chat_set_muted(&self, id: &str, muted: bool) {
        let _ = self.with_chat(id, |m| {
            if let Some(c) = m.chat.as_mut() {
                c.muted = muted;
            }
        });
    }

    fn chat_set_agents(&self, id: &str, access: SyncAgentAccess) {
        let _ = self.with_chat(id, |m| {
            if let Some(c) = m.chat.as_mut() {
                c.agents = access;
            }
        });
    }

    fn chat_rename(&self, id: &str, name: &str) -> Result<(), CoreError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(failed("a channel needs a name"));
        }
        self.with_chat(id, |m| m.f.name = name.chars().take(200).collect())
    }

    fn chat_set_wake_command(&self, id: &str, command: Option<&str>) -> Result<(), CoreError> {
        self.with_chat(id, |m| {
            if let Some(c) = m.chat.as_mut() {
                c.wake = command.map(str::trim).filter(|w| !w.is_empty()).map(str::to_string);
            }
        })
    }

    fn chat_leave(&self, id: &str) -> Result<(), CoreError> {
        let mut s = lock(&self.state);
        let before = s.channels.len();
        s.channels.retain(|m| m.f.id != id);
        if s.channels.len() == before { Err(failed(concat!("This channel is no longer on this ", trs_platform::computer!()))) } else { Ok(()) }
    }

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
        // A link for both: the channel too (a new mock channel, unless it is here), linked to the folder.
        let channel = match InviteLink::parse_bundle(key_or_link).ok().and_then(|(_, c)| c) {
            Some(c) => {
                let cid = c.invite.folder_id().to_string();
                if !lock(&self.state).channels.iter().any(|m| m.f.id == cid) {
                    // Under the link's id, so the link knows it is here.
                    let new = self.chat_join(&c.format_chat())?.id;
                    if let Some(m) = lock(&self.state).channels.iter_mut().find(|m| m.f.id == new) {
                        m.f.id = cid.clone();
                    }
                }
                Some(cid)
            }
            None => None,
        };
        let mut f = self.add(name, path, info.role, true);
        if let Some(c) = channel {
            self.set_channel(&f.id, Some(&c))?;
            f.channel_id = Some(c);
        }
        Ok(f)
    }

    fn set_channel(&self, id: &str, channel_id: Option<&str>) -> Result<(), CoreError> {
        if let Some(c) = channel_id
            && !lock(&self.state).channels.iter().any(|m| m.f.id == c)
        {
            return Err(failed(concat!("No such channel on this ", trs_platform::computer!())));
        }
        self.with(id, |m| m.f.channel_id = channel_id.map(str::to_string))
    }

    fn join_path(&self, key_or_link: &str) -> Option<String> {
        let (info, _) = parse_link(key_or_link)?;
        let name = info.name.unwrap_or_else(|| info.folder_id.split('-').next().unwrap_or("Folder").to_owned());
        let name = trs_platform::fs::clean_name(&name);
        Some(std::path::Path::new(&self.default_location()).join(name.trim().trim_start_matches('.')).display().to_string())
    }

    fn members(&self, id: &str) -> Vec<SyncMember> {
        self.with(id, |m| m.members.clone()).unwrap_or_default()
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
                return Err(failed(concat!("To stop syncing on this ", trs_platform::computer!(), ", remove the folder instead")));
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

    fn create_invite(&self, id: &str, role: SyncRole, max_uses: u32, valid_days: u32, channel_id: Option<&str>) -> Result<SyncInvite, CoreError> {
        let may = |m: &MockFolder, noun: &str| match (m.f.role, role) {
            (SyncRole::Owner, _) | (SyncRole::Admin, SyncRole::ReadWrite | SyncRole::ReadOnly) => Ok(()),
            (SyncRole::Admin, _) => Err(failed(format!("Only the {noun}'s owner can invite admins or owners"))),
            _ => Err(failed(format!("Only the {noun}'s owner or an admin can share it"))),
        };
        let mut s = lock(&self.state);
        let chat = match channel_id {
            None => None,
            Some(c) => {
                let ch = s.channels.iter_mut().find(|m| m.f.id == c).ok_or_else(|| failed(concat!("No such channel on this ", trs_platform::computer!())))?;
                may(ch, "channel").map_err(|e| failed(format!(concat!("This ", trs_platform::computer!(), " may not share the channel {} at that role ({e})"), ch.f.name, e = e)))?;
                let i = mock_invite(ch.n, ch.invites.len() as u32 + 3, &ch.f.name, role, max_uses, valid_days, true);
                ch.invites.insert(0, i.clone());
                Some((i, ch.f.id.clone(), ch.f.name.clone()))
            }
        };
        let st = &mut *s;
        let m = st.folders.iter_mut().chain(st.channels.iter_mut()).find(|m| m.f.id == id).ok_or_else(gone)?;
        may(m, if m.chat.is_some() { "channel" } else { "folder" })?;
        let mut i = mock_invite(m.n, m.invites.len() as u32 + 3, &m.f.name, role, max_uses, valid_days, m.chat.is_some());
        if let Some((ci, cid, cname)) = chat {
            let (fl, cl) = (InviteLink::parse(&i.link), InviteLink::parse_chat(&ci.link));
            if let (Ok(fl), Ok(cl)) = (fl, cl) {
                i.link = fl.format_bundle(&cl);
            }
            if m.f.channel_id.is_none() {
                m.f.channel_id = Some(cid.clone());
            }
            (i.channel_id, i.channel_name) = (Some(cid), Some(cname));
        }
        m.invites.insert(0, i.clone());
        Ok(i)
    }

    fn invites(&self, id: &str) -> Vec<SyncInvite> {
        self.with(id, |m| m.invites.clone()).unwrap_or_default()
    }

    fn revoke_invite(&self, id: &str, invite_id: &str) -> Result<(), CoreError> {
        let bundled = self.with(id, |m| {
            let i = m.invites.iter_mut().find(|i| i.id == invite_id).ok_or_else(|| failed("No such link"))?;
            i.revoked = true;
            let half = InviteLink::parse_bundle(&i.link).ok().and_then(|(_, c)| c).map(|c| trs_sync_proto::members::hex(&c.invite.id));
            Ok::<_, CoreError>(i.channel_id.clone().zip(half))
        })??;
        // A link for both: the channel's invitation goes too.
        if let Some((cid, iid)) = bundled
            && let Some(ch) = lock(&self.state).channels.iter_mut().find(|m| m.f.id == cid)
            && let Some(i) = ch.invites.iter_mut().find(|i| i.id == iid)
        {
            i.revoked = true;
        }
        Ok(())
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

    fn remove(&self, id: &str) -> Result<String, CoreError> {
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
            "Plans/Budget 2027 (conflict, Studio iMac, 2026-10-24 18.05z).numbers".into(),
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
        let conflicts = if m.f.conflicts > 0 { vec!["Plans/Budget 2027 (conflict, Studio iMac, 2026-10-24 18.05z).numbers".into()] } else { Vec::new() };
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

    fn set_port(&self, port: u16) -> Result<(), CoreError> {
        if port < 1024 {
            return Err(failed(trs_control::sync::PortChange::Invalid.to_string()));
        }
        lock(&self.state).settings.port = port;
        Ok(())
    }

    fn port_reachability(&self) -> Option<crate::types::ReachabilityInfo> {
        Some(crate::types::ReachabilityInfo { state: "mapped".into(), summary: concat!("Opened on the router with UPnP: your devices can reach this ", trs_platform::computer!()).into() })
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
        if let Some(cid) = bundled_channel(&text) {
            info.channel_joined = self.backend.sync().chat_channels().iter().any(|c| c.id == cid);
        }
        Some(info)
    }

    /// Why a pasted text is not a link this Mac can use, as a sentence for the add modal ("This link was made by a
    /// newer Seedwell: update Seedwell to use it."); `None` when [`Core::sync_parse_link`] reads it, or the text
    /// is empty.
    pub fn sync_link_problem(&self, text: String) -> Option<String> {
        if text.trim().is_empty() {
            return None;
        }
        InviteLink::parse_bundle(&text).err().map(|e| readable(&e.to_string()))
    }

    /// Every channel, by name. Cheap; called once a second.
    pub fn chat_channels(&self) -> Vec<SyncChannel> {
        self.backend.sync().chat_channels()
    }

    /// Makes a channel (this Mac is its owner). Share it with `sync_create_invite` on its id.
    pub fn chat_new(&self, name: String) -> Result<SyncChannel, CoreError> {
        self.backend.sync().chat_new(&name)
    }

    /// Joins a channel by its `seedwell://chat/` link. It waits until one of its owners or admins admits this Mac.
    pub fn chat_join(&self, link: String) -> Result<SyncChannel, CoreError> {
        self.backend.sync().chat_join(&link)
    }

    /// What a pasted channel link is (`folder_id` is the channel's id), or `None`.
    pub fn chat_parse_link(&self, text: String) -> Option<SyncLinkInfo> {
        let mut info = parse_chat_link(&text)?;
        info.already_synced = self.backend.sync().chat_channels().iter().any(|c| c.id == info.folder_id);
        Some(info)
    }

    /// A page of a channel's messages in `topic` (`None`: General), oldest first: the `limit` newest before
    /// `before` (a message id), or the newest. Cheap; called on scroll.
    pub fn chat_messages(&self, id: String, topic: Option<String>, before: Option<String>, limit: u32) -> Vec<SyncMessage> {
        self.backend.sync().chat_messages(&id, topic.as_deref(), before.as_deref(), limit)
    }

    /// Posts a text message in `topic` (`None`: General) as the person at this Mac (Owner, Admin and Read & write
    /// members post).
    pub fn chat_send(&self, id: String, topic: Option<String>, body: String, reply_to: Option<String>) -> Result<SyncMessage, CoreError> {
        self.backend.sync().chat_send(&id, topic.as_deref(), &body, reply_to.as_deref())
    }

    /// Every message of the channel's `topic` (`None`: General) is read.
    pub fn chat_mark_read(&self, id: String, topic: Option<String>) {
        self.backend.sync().chat_mark_read(&id, topic.as_deref())
    }

    /// The channel's topics, newest activity first (General is not listed). Cheap.
    pub fn chat_topics(&self, channel_id: String) -> Vec<SyncTopic> {
        self.backend.sync().chat_topics(&channel_id)
    }

    /// Makes a topic in the channel (anyone who may post).
    pub fn chat_new_topic(&self, channel_id: String, title: String) -> Result<SyncTopic, CoreError> {
        self.backend.sync().chat_new_topic(&channel_id, &title)
    }

    /// Every device this Mac has private messages with, newest first. Cheap.
    pub fn chat_directs(&self) -> Vec<SyncDirect> {
        self.backend.sync().chat_directs()
    }

    /// The private messages with a device, oldest first (`channel_id` is empty).
    pub fn chat_direct_messages(&self, device_id: String, before: Option<String>, limit: u32) -> Vec<SyncMessage> {
        self.backend.sync().chat_direct_messages(&device_id, before.as_deref(), limit)
    }

    /// Writes a private message (end-to-end encrypted) to a device this Mac has a conversation with or shares a
    /// channel with, while the two are connected directly (`SyncDirect::online`). Refused, with a plain reason,
    /// otherwise.
    pub fn chat_send_direct(&self, device_id: String, body: String, reply_to: Option<String>) -> Result<SyncMessage, CoreError> {
        self.backend.sync().chat_send_direct(&device_id, &body, reply_to.as_deref())
    }

    /// Every private message from the device is read.
    pub fn chat_mark_direct_read(&self, device_id: String) {
        self.backend.sync().chat_mark_direct_read(&device_id)
    }

    /// Deletes the private conversation with a device on this Mac: its messages and all this Mac knows of the
    /// device through it (the other device keeps its copy).
    pub fn chat_delete_direct(&self, device_id: String) -> Result<(), CoreError> {
        self.backend.sync().chat_delete_direct(&device_id)
    }

    /// The line a private conversation's page shows instead of the message field while the other device is not
    /// connected directly.
    pub fn chat_direct_offline_text(&self) -> String {
        DIRECT_OFFLINE.into()
    }

    /// What agents on this Mac may do with private messages.
    pub fn chat_direct_agents(&self) -> SyncAgentAccess {
        self.backend.sync().chat_direct_agents()
    }

    /// Sets what agents on this Mac may do with private messages.
    pub fn chat_set_direct_agents(&self, access: SyncAgentAccess) {
        self.backend.sync().chat_set_direct_agents(access)
    }

    /// No notifications for the channel (an @mention still notifies: the app's rule).
    pub fn chat_set_muted(&self, id: String, muted: bool) {
        self.backend.sync().chat_set_muted(&id, muted)
    }

    /// What agents on this Mac may do in the channel.
    pub fn chat_set_agents(&self, id: String, access: SyncAgentAccess) {
        self.backend.sync().chat_set_agents(&id, access)
    }

    /// The command run for each message from another device while agents may read the channel (the message as
    /// JSON on its standard input); `None` or empty: none.
    pub fn chat_set_wake_command(&self, id: String, command: Option<String>) -> Result<(), CoreError> {
        self.backend.sync().chat_set_wake_command(&id, command.as_deref())
    }

    /// The channel's name on this Mac (each device names its channels, as folders).
    pub fn chat_rename(&self, id: String, name: String) -> Result<(), CoreError> {
        self.backend.sync().chat_rename(&id, &name)
    }

    /// Leaves the channel on this Mac: its messages here go (other members keep theirs).
    pub fn chat_leave(&self, id: String) -> Result<(), CoreError> {
        self.backend.sync().chat_leave(&id)
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
    /// expiry. Owners invite at any role, admins at Read & write or Read only. With `channel_id` (for a folder), one
    /// link for the folder and that channel, with the same role, uses and expiry: refused, with the reason, when this
    /// Mac may not share the channel at that role. The folder goes with the channel here if it had none.
    pub fn sync_create_invite(&self, id: String, role: SyncRole, max_uses: u32, valid_days: u32, channel_id: Option<String>) -> Result<SyncInvite, CoreError> {
        self.backend.sync().create_invite(&id, role, max_uses, valid_days, channel_id.as_deref())
    }

    /// Sets the channel that goes with a folder on this Mac (`None`: none). A channel that is not here is refused.
    pub fn sync_set_channel(&self, id: String, channel_id: Option<String>) -> Result<(), CoreError> {
        self.backend.sync().set_channel(&id, channel_id.as_deref())
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

    /// Stops syncing the folder on this computer; other devices keep their copy. With `trash_files` the folder then
    /// goes to the Trash (the Recycle Bin on Windows), checked first: a folder that cannot go there (one the Recycle Bin
    /// would delete at once; with a stand-in Trash folder, one on another volume) stays synced, and the error says why
    /// and what to do.
    /// `Some`: the folder stopped syncing but the move failed afterwards; the alert to show. A folder no longer on disk
    /// has nothing to move. Blocks while the folder stops and moves: call it off the main thread.
    pub fn sync_remove(&self, id: String, trash_files: bool) -> Result<Option<crate::sync_text::SyncAlert>, CoreError> {
        crate::sync_removal::sync_remove(&*self.backend, &id, trash_files)
    }

    pub fn sync_pause(&self, id: String, paused: bool) -> Result<(), CoreError> {
        self.backend.sync().pause(&id, paused)
    }

    /// Scans the whole folder now.
    pub fn sync_rescan(&self, id: String) -> Result<(), CoreError> {
        self.backend.sync().rescan(&id)
    }

    /// How many of a folder's files no other device is known to hold: they go from the others' reach when this
    /// computer stops syncing it (reads its whole index: call when its removal question opens, not every second).
    pub fn sync_only_here(&self, id: String) -> u64 {
        self.backend.sync().only_here(&id)
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

    /// Moves folder sync to `port` (1024-65535) now: refused at once when another app has it; otherwise sync
    /// starts again on it in the background, and `sync_settings().port` shows it a few seconds later.
    pub fn set_sync_port(&self, port: u16) -> Result<(), CoreError> {
        self.backend.sync().set_port(port)
    }

    /// Whether the router opened the sync port (`None` while sync is not running).
    pub fn sync_reachability(&self) -> Option<crate::types::ReachabilityInfo> {
        self.backend.sync().port_reachability()
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
    fn a_conflict_named_in_utc_is_shown_in_local_time() {
        let at = 1_792_938_720; // 2026-10-25 14:32 UTC
        let c = conflict_of(Path::new("/d"), "a (conflict, iMac, 2026-10-25 14.32z).txt".into(), "a.txt".into());
        let local = trs_sync::clock::local_stamp(at, trs_engine::util::local_utc_offset(at));
        assert_eq!((c.device.as_str(), c.when.as_str()), ("iMac", local.as_str()));
        let old = conflict_of(Path::new("/d"), "a (conflict, iMac, 2026-10-25 15.32).txt".into(), "a.txt".into());
        assert_eq!(old.when, "2026-10-25 15.32", "named before 0.1.9: local time already");
    }

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
        let rw = core.sync_create_invite(f.id.clone(), SyncRole::ReadWrite, 1, 7, None).unwrap().link;
        let ro = core.sync_create_invite(f.id.clone(), SyncRole::ReadOnly, 0, 0, None).unwrap();
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
        assert!(std::path::Path::new(&place).ends_with("Seedwell/Notes"), "{place}");
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
        // Channels: one with an agent's answer and a reply to it.
        let chans = core.chat_channels();
        assert_eq!(chans.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Family", "Ola & Kristian", "Release notes"]);
        let fam = &chans[0];
        assert_eq!((fam.role, fam.unread, fam.agents), (SyncRole::Owner, 3, SyncAgentAccess::Off), "General's 1 and the topic's 2");
        let general = core.chat_messages(fam.id.clone(), None, None, 50);
        assert!(general.iter().all(|m| m.topic.is_none()) && general.len() == 3);
        let topics = core.chat_topics(fam.id.clone());
        assert_eq!((topics.len(), topics[0].title.as_str(), topics[0].messages, topics[0].unread), (1, "Lørdag", 3, 2));
        let msgs = core.chat_messages(fam.id.clone(), Some(topics[0].id.clone()), None, 50);
        assert!(msgs.iter().any(|m| m.agent.is_some() && m.reply_to.is_some()) && msgs.iter().any(|m| !m.is_me));
        assert_eq!(core.chat_messages(fam.id.clone(), Some(topics[0].id.clone()), Some(msgs[2].id.clone()), 1), vec![msgs[1].clone()]);
        let sent = core.chat_send(fam.id.clone(), Some(topics[0].id.clone()), "På vei!".into(), Some(msgs[2].id.clone())).unwrap();
        assert!(sent.is_me && sent.agent.is_none() && sent.topic == Some(topics[0].id.clone()));
        core.chat_mark_read(fam.id.clone(), Some(topics[0].id.clone()));
        assert_eq!(core.chat_topics(fam.id.clone())[0].unread, 0);
        assert_eq!(core.chat_channels()[0].unread, 1, "General's is left");
        core.chat_mark_read(fam.id.clone(), None);
        let t = core.chat_new_topic(fam.id.clone(), "Søndag".into()).unwrap();
        assert_eq!(core.chat_topics(fam.id.clone()).len(), 2);
        assert_eq!(t.title, "Søndag");
        // A private conversation, and a new one with a channel member.
        let dms = core.chat_directs();
        assert_eq!((dms.len(), dms[0].unread), (1, 1));
        let pm = core.chat_direct_messages(dms[0].device_id.clone(), None, 10);
        assert!(pm.len() == 3 && pm.iter().all(|m| m.channel_id.is_empty()));
        core.chat_mark_direct_read(dms[0].device_id.clone());
        assert_eq!(core.chat_directs()[0].unread, 0);
        let kari = core.sync_members(fam.id.clone()).into_iter().find(|m| !m.is_me).unwrap();
        core.chat_send_direct(kari.device_id.clone(), "Hei".into(), None).unwrap();
        assert_eq!(core.chat_directs().len(), 2);
        assert!(core.chat_send_direct("NOT-A-MEMBER".into(), "Hei".into(), None).is_err());
        assert_eq!(core.chat_direct_agents(), SyncAgentAccess::Off);
        core.chat_set_direct_agents(SyncAgentAccess::Read);
        assert_eq!(core.chat_direct_agents(), SyncAgentAccess::Read);
        core.chat_set_agents(fam.id.clone(), SyncAgentAccess::Read);
        core.chat_set_muted(fam.id.clone(), true);
        core.chat_rename(fam.id.clone(), " Family dinners ".into()).unwrap();
        assert!(core.chat_rename(fam.id.clone(), "  ".into()).is_err());
        assert!(core.chat_channels().iter().any(|c| c.id == fam.id && c.name == "Family dinners"));
        core.chat_rename(fam.id.clone(), "Family".into()).unwrap();
        core.chat_rename(fam.id.clone(), "Family".into()).unwrap();
        core.chat_set_wake_command(fam.id.clone(), Some("~/bin/wake".into())).unwrap();
        let fam2 = core.chat_channels().into_iter().find(|c| c.id == fam.id).unwrap();
        assert_eq!((fam2.unread, fam2.agents, fam2.muted, fam2.wake_command.as_deref()), (0, SyncAgentAccess::Read, true, Some("~/bin/wake")));
        assert_eq!(core.sync_members(fam.id.clone()).len(), 3, "membership calls take a channel's id");
        let chat_link = core.sync_create_invite(fam.id.clone(), SyncRole::ReadWrite, 1, 7, None).unwrap().link;
        assert!(chat_link.starts_with("seedwell://chat/"), "{chat_link}");
        assert!(core.sync_parse_link(chat_link.clone()).is_none() && core.chat_parse_link(chat_link.clone()).is_some());
        assert!(core.sync_link_problem(chat_link).is_some_and(|p| p.contains("channel")));
        // A folder with a chat, and one link for both.
        let photos = core.sync_snapshot().folders.into_iter().find(|f| f.name == "Photos").unwrap();
        assert_eq!(photos.channel_id, Some(fam.id.clone()));
        let n_ch = core.sync_invites(fam.id.clone()).len();
        let both = core.sync_create_invite(photos.id.clone(), SyncRole::ReadWrite, 1, 7, Some(fam.id.clone())).unwrap();
        assert_eq!((both.channel_id.as_deref(), both.channel_name.as_deref()), (Some(fam.id.as_str()), Some("Family")));
        assert_eq!(core.sync_invites(fam.id.clone()).len(), n_ch + 1, "the channel lists its half");
        let info = core.sync_parse_link(both.link.clone()).unwrap();
        assert_eq!(info.channel_name.as_deref(), Some("Family"));
        let e = core.chat_join(both.link.clone()).unwrap_err().to_string();
        assert!(e.contains("This is a folder link: add it under Folder Sync."), "{e}");
        let joined = core.sync_join(both.link.clone(), "/tmp/seedwell-mock/Photos 2".into()).unwrap();
        let linked = joined.channel_id.clone().unwrap();
        assert!(core.chat_channels().iter().any(|c| c.id == linked), "its channel is joined too");
        assert!(core.sync_parse_link(both.link.clone()).unwrap().channel_joined);
        core.sync_remove(joined.id, false).unwrap();
        core.sync_revoke_invite(photos.id.clone(), both.id.clone()).unwrap();
        assert!(core.sync_invites(fam.id.clone())[0].revoked, "revoking it revokes the channel's half");
        assert!(core.sync_set_channel(photos.id.clone(), Some("nope".into())).is_err());
        core.sync_set_channel(photos.id.clone(), None).unwrap();
        assert_eq!(core.sync_snapshot().folders.into_iter().find(|f| f.id == photos.id).unwrap().channel_id, None);
        assert!(core.sync_create_invite(photos.id.clone(), SyncRole::ReadWrite, 1, 7, Some("nope".into())).is_err());
        let new = core.chat_new("Plans".into()).unwrap();
        assert_eq!((new.role, new.members), (SyncRole::Owner, 1));
        core.chat_leave(new.id).unwrap();
        let reader = s_folder(&core, SyncRole::ReadOnly);
        assert!(core.sync_create_invite(reader, SyncRole::ReadOnly, 1, 7, None).is_err(), "a reader cannot share");
        assert!(matches!(core.sync_join("nope".into(), String::new()), Err(CoreError::Failed { .. })));
        let r = core.sync_rename(f.id.clone(), "Notebook".into()).unwrap();
        assert_eq!(r.path, "/tmp/seedwell-mock/Notebook");
        assert!(core.sync_rename(f.id.clone(), ".hidden".into()).is_err());
        let m = core.sync_move(f.id.clone(), "/tmp/elsewhere/Notebook".into()).unwrap();
        assert_eq!(m.path, "/tmp/elsewhere/Notebook");
        core.sync_pause(f.id.clone(), true).unwrap();
        assert_eq!(core.sync_snapshot().folders.iter().find(|x| x.id == f.id).unwrap().state, SyncState::Paused);
        core.sync_rescan(f.id.clone()).unwrap();
        assert_eq!(core.sync_remove(f.id.clone(), true).unwrap(), None, "not on disk: nothing to move");
        assert_eq!(core.sync_snapshot().folders.len(), n + 1, "the joined one stays");
        let e = core.sync_remove(f.id.clone(), false).unwrap_err();
        assert_eq!(e.to_string(), concat!("This folder is no longer synced on this ", trs_platform::computer!(), "."));
        let mut st = core.sync_settings();
        assert!(st.lan_discovery && st.relay_own_devices && !st.relay_everyone && st.port > 0);
        st.device_name = "Desk".into();
        st.up_limit = 2_000_000;
        core.set_sync_settings(st.clone()).unwrap();
        assert_eq!(core.sync_settings(), st);
        assert_eq!(core.sync_snapshot().device_name, "Desk");
        assert!(std::path::Path::new(&core.sync_default_location()).ends_with("Seedwell"));
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
        let inv = core.sync_create_invite(f.id.clone(), SyncRole::ReadWrite, 1, 7, None).unwrap();
        let link = inv.link.clone();
        assert!(!toml.contains(&link[16..]), "no link in sync.toml");
        let info = core.sync_parse_link(link.clone()).unwrap();
        assert!(info.already_synced && info.name.as_deref() == Some("Docs"));
        assert_eq!(core.sync_invites(f.id.clone())[0].id, inv.id);
        let members = core.sync_members(f.id.clone());
        assert_eq!(members.len(), 1);
        assert!(members[0].is_me && members[0].role == SyncRole::Owner && members[0].online);
        assert!(core.sync_set_role(f.id.clone(), members[0].device_id.clone(), SyncRole::ReadOnly).is_err(), "the only owner stays one");
        // A channel, through the same calls.
        let ch = core.chat_new("Plans".into()).unwrap();
        assert_eq!((ch.role, ch.agents, ch.members, ch.online), (SyncRole::Owner, SyncAgentAccess::Off, 1, 1));
        let m = core.chat_send(ch.id.clone(), None, "hello".into(), None).unwrap();
        assert_eq!((m.is_me, m.kind, m.agent.clone()), (true, SyncMessageKind::Text, None));
        assert_eq!(core.chat_messages(ch.id.clone(), None, None, 10), vec![m.clone()]);
        let t = core.chat_new_topic(ch.id.clone(), "Plans for May".into()).unwrap();
        let in_t = core.chat_send(ch.id.clone(), Some(t.id.clone()), "first".into(), None).unwrap();
        assert_eq!(core.chat_messages(ch.id.clone(), Some(t.id.clone()), None, 10), vec![in_t]);
        assert_eq!(core.chat_messages(ch.id.clone(), None, None, 10).len(), 1, "General keeps its own");
        assert_eq!(core.chat_topics(ch.id.clone())[0].messages, 1);
        assert!(core.chat_send_direct(core.sync_snapshot().device_id, "me".into(), None).is_err());
        assert!(core.chat_directs().is_empty());
        let chat_link = core.sync_create_invite(ch.id.clone(), SyncRole::ReadOnly, 1, 7, None).unwrap().link;
        let info = core.chat_parse_link(chat_link).unwrap();
        assert!(info.already_synced && info.folder_id == ch.id && info.role == SyncRole::ReadOnly);
        // One link for the folder and the channel.
        let both = core.sync_create_invite(f.id.clone(), SyncRole::ReadWrite, 2, 3, Some(ch.id.clone())).unwrap();
        assert_eq!((both.channel_id.clone(), both.channel_name.as_deref()), (Some(ch.id.clone()), Some("Plans")));
        assert_eq!(core.sync_snapshot().folders[0].channel_id, Some(ch.id.clone()), "the folder goes with it");
        let info = core.sync_parse_link(both.link.clone()).unwrap();
        assert!(info.already_synced && info.channel_joined && info.channel_name.as_deref() == Some("Plans"));
        let half = core.sync_invites(ch.id.clone()).into_iter().find(|i| i.max_uses == 2).unwrap();
        assert_eq!((half.role, half.expires_s), (SyncRole::ReadWrite, both.expires_s));
        let e = core.chat_join(both.link.clone()).unwrap_err().to_string();
        assert!(e.contains("This is a folder link: add it under Folder Sync."), "{e}");
        core.sync_revoke_invite(f.id.clone(), both.id.clone()).unwrap();
        assert!(core.sync_invites(ch.id.clone()).into_iter().find(|i| i.id == half.id).unwrap().revoked);
        assert!(core.sync_set_channel(f.id.clone(), Some(f.id.clone())).is_err(), "a folder is not a channel");
        core.sync_set_channel(f.id.clone(), None).unwrap();
        assert_eq!(core.sync_snapshot().folders[0].channel_id, None);
        assert_eq!(core.sync_members(ch.id.clone()).len(), 1);
        core.chat_set_agents(ch.id.clone(), SyncAgentAccess::ReadPost);
        assert!(core.chat_set_wake_command(ch.id.clone(), Some("cat\nrm".into())).is_err(), "one line");
        assert_eq!(core.chat_channels()[0].agents, SyncAgentAccess::ReadPost);
        core.chat_leave(ch.id).unwrap();
        assert!(core.chat_channels().is_empty());
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

        // Removed with its files: the folder goes to the Trash (the QA one: a test never uses the real Trash).
        let bin = work.path().join("bin");
        // SAFETY: nextest runs each test in a process of its own; the core reads it only during the removal.
        unsafe { std::env::set_var("SEEDWELL_QA_TRASH_DIR", &bin) };
        assert_eq!(core.sync_remove(f.id.clone(), true).unwrap(), None);
        assert!(bin.join("Papers/a.txt").is_file() && !work.path().join("Papers").exists(), "in the Trash");
        assert!(core.sync_snapshot().folders.is_empty());
        assert!(core.sync_create_invite(f.id, SyncRole::ReadOnly, 1, 7, None).is_err());
        assert!(core.shutdown(5).finished);
    }
}
