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
}
