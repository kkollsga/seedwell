//! What the UI and the agent interface see: one snapshot of every folder and device.

use trs_sync_proto::keys::Access;
use trs_sync_proto::members::Role;

/// What a folder is doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderState {
    /// Starting: the first scan has not finished.
    Starting,
    /// Everything this device knows of is here.
    UpToDate,
    /// Scanning the disk.
    Scanning,
    /// Fetching or applying changes.
    Syncing,
    /// Needs data that no connected device has.
    WaitingForDevices,
    /// Joining: waits for one of the folder's owners or admins to be online and admit this device.
    WaitingForAdmin,
    /// Paused by the user.
    Paused,
    /// The folder is not where it should be (`.seedwell` missing: an unmounted disk, a deleted folder).
    Missing,
    /// Something keeps failing (the reason is in `error`).
    Error,
}

/// One folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderStatus {
    /// The folder id, as text.
    pub id: String,
    /// Its name on this device.
    pub name: String,
    /// Where it is.
    pub path: String,
    /// What this device may do (from its role; Read while it is not admitted yet).
    pub access: Access,
    /// This device's role (`None` while joining, or after it was removed).
    pub role: Option<Role>,
    /// When the folder stops being shared (its owners' setting; Unix seconds); `None` = no end.
    pub share_until: Option<u64>,
    /// What it is doing.
    pub state: FolderState,
    /// Files and directories in the folder (as last scanned).
    pub files: u64,
    /// Their size.
    pub bytes: u64,
    /// Paths still needed, and their size.
    pub need_files: u64,
    /// Bytes still needed.
    pub need_bytes: u64,
    /// Bytes per second fetched (moving average).
    pub rate_in: u64,
    /// Bytes per second served.
    pub rate_out: u64,
    /// Devices connected for this folder.
    pub connected: u32,
    /// Devices known in this folder.
    pub members: u32,
    /// Conflict copies not yet resolved.
    pub conflicts: u32,
    /// Synced only with devices on the local network.
    pub lan_only: bool,
    /// Paths that cannot be synced (with the reason).
    pub issues: Vec<(String, String)>,
    /// The last error, if the state is `Error`.
    pub error: Option<String>,
    /// When the folder last had nothing left to do, Unix seconds.
    pub last_synced_s: Option<u64>,
    /// The channel that goes with it on this device (its id), while that channel is here.
    pub channel_id: Option<String>,
    /// On demand (SPEC §7G.11): `files` and `bytes` count what is on this device, `path` is where the host shows it.
    pub on_demand: bool,
    /// A full folder here whose files are on demand on the devices that do not hold them (SPEC §7G.11, *Modes*):
    /// this device keeps its files at `path`.
    pub on_demand_others: bool,
    /// Files no device can give right now (SPEC §7G.11): not counted as needed; "N files unavailable: no device has
    /// them". Removed only by the user deleting such an item (on a device where the folder is on demand).
    pub unavailable: u64,
    /// Joining: what it does now to find an owner or admin, as a sentence ("Looking for Anna's Mac mini on the
    /// local network…"); `None` once admitted.
    pub progress: Option<String>,
    /// Joining: the last thing that went wrong ("Anna's Mac mini at 192.168.68.57:55799: no answer").
    pub problem: Option<String>,
    /// As an owner or admin: the attempts to join with a link that this device refused, newest last.
    pub join_attempts: Vec<JoinAttemptStatus>,
    /// Admitted, with members of which none is connected: whom it waits for, since when, and why it cannot reach
    /// one ("Waiting for Kristian's Mac mini (last seen 3 days ago: nothing answers at 192.168.68.57:55799)",
    /// "Waiting for 2 devices: Mac mini (3 days ago), Makro (1 hour ago)"); or connected to members none of which has
    /// sent its list of the folder yet ("Waiting for Mac mini to list Movies"). `None` otherwise.
    pub waiting: Option<String>,
    /// As an owner: the members not connected and not seen for a day or more (or never here), which this device may
    /// remove (SPEC §7G.5). Nothing is removed unless the user says so.
    pub gone: Vec<GoneDevice>,
    /// As a read-write or read-only member of a full folder whose every owner and admin is gone (nobody can admit
    /// devices or change roles any more): it may be shared again from this device ([`crate::SyncHandle::reshare`]).
    pub reshare: bool,
    /// While it scans (`state` is `Scanning`): the phase and how far it is, for the app's words and ring (SPEC §7G.2).
    pub scan: Option<trs_sync::scan::ScanProgress>,
}

/// A member of a folder or channel that has not been connected for a day or more (or never here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoneDevice {
    /// Its id, as text.
    pub id: String,
    /// Its name ("device MFZWI3D" when it never gave one).
    pub name: String,
    /// When it was last connected, Unix seconds; `None`: never here.
    pub last_seen_s: Option<u64>,
}

/// An attempt to join with a link that this device refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinAttemptStatus {
    /// When, Unix seconds.
    pub at_s: u64,
    /// The device that tried, as text.
    pub device_id: String,
    /// Its name (empty when it did not say).
    pub name: String,
    /// Its client (`Seedwell 0.1.9`).
    pub client: String,
    /// Why it was refused.
    pub reason: String,
}

impl From<&crate::shared::JoinAttempt> for JoinAttemptStatus {
    fn from(a: &crate::shared::JoinAttempt) -> Self {
        JoinAttemptStatus { at_s: a.at_s, device_id: a.device.to_string(), name: a.name.clone(), client: a.client.clone(), reason: a.reason.clone() }
    }
}

/// A device of a folder or channel here that is not connected now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineDevice {
    /// Its id, as text.
    pub id: String,
    /// Its name.
    pub name: String,
    /// When a session with it last ended (or began), Unix seconds; `None`: never connected here.
    pub last_seen_s: Option<u64>,
    /// Why the last attempt to connect to it failed.
    pub problem: Option<String>,
    /// The folders and channels it is in (ids, as text).
    pub folders: Vec<String>,
}

/// A connected device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceStatus {
    /// Its id, as text.
    pub id: String,
    /// Its name.
    pub name: String,
    /// Its client (`Seedwell 0.1.5`).
    pub client: String,
    /// Where the connection goes (`192.168.1.20:41200`).
    pub address: String,
    /// `LAN`, `Internet` or `Relay`.
    pub via: String,
    /// The folders shared with it (ids, as text).
    pub folders: Vec<String>,
    /// Bytes received from it in this session.
    pub rx_bytes: u64,
    /// Bytes sent to it.
    pub tx_bytes: u64,
}

/// Everything at once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// This device's id, as text.
    pub device_id: String,
    /// This device's name.
    pub device_name: String,
    /// The port sync listens on (0 = not listening).
    pub port: u16,
    /// The folders.
    pub folders: Vec<FolderStatus>,
    /// The connected devices.
    pub devices: Vec<DeviceStatus>,
    /// The devices of the folders and channels here that are not connected.
    pub offline: Vec<OfflineDevice>,
}
