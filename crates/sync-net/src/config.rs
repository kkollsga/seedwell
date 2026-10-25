//! `sync.toml`: device-wide settings and the list of folders. Keys are never in it (see [`crate::secrets`]).
//!
//! The file is written by the service (atomically: a temp file renamed over it) and read at start.
//! Keys this version does not know are dropped when it rewrites the file.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Device-wide settings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// The name other devices see (empty = the computer's name, given by the app).
    pub device_name: String,
    /// TCP port for sync (0 = pick one at first start and keep it).
    pub port: u16,
    /// Find devices on the local network.
    pub lan_discovery: bool,
    /// Find devices over the Internet (Mainline DHT, daily rotating topics).
    pub dht_discovery: bool,
    /// Relay between this user's own devices when they cannot reach each other.
    pub relay_own_devices: bool,
    /// Also relay between other members of shared folders.
    pub relay_everyone: bool,
    /// Internet upload limit, KiB/s (0 = none). The LAN is never limited.
    pub up_kib: u32,
    /// Internet download limit, KiB/s (0 = none).
    pub down_kib: u32,
    /// Where joined folders go by default (empty = `~/Seedwell`, given by the app).
    pub default_location: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            device_name: String::new(),
            port: 0,
            lan_discovery: true,
            dht_discovery: true,
            relay_own_devices: true,
            relay_everyone: false,
            up_kib: 0,
            down_kib: 0,
            default_location: String::new(),
        }
    }
}

/// One folder on this device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderEntry {
    /// The folder id, as text.
    pub id: String,
    /// Where it is.
    pub path: PathBuf,
    /// Paused by the user.
    #[serde(default)]
    pub paused: bool,
    /// Keep replaced and deleted versions in `.seedwell/Archive`.
    #[serde(default = "yes")]
    pub archive: bool,
    /// Archive retention in days (0 = keep none).
    #[serde(default = "thirty")]
    pub archive_days: u32,
    /// Only sync over the local network.
    #[serde(default)]
    pub lan_only: bool,
    /// Local-only ignore patterns (in addition to the synced `.seedwellignore`).
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Addresses typed in by the user for devices of this folder (`host:port`).
    #[serde(default)]
    pub addresses: Vec<String>,
}

fn yes() -> bool {
    true
}

fn thirty() -> u32 {
    30
}

impl FolderEntry {
    /// A new entry with the defaults.
    pub fn new(id: String, path: PathBuf) -> Self {
        FolderEntry { id, path, paused: false, archive: true, archive_days: 30, lan_only: false, ignore: Vec::new(), addresses: Vec::new() }
    }
}

/// A device this one has synced with: remembered so it can be dialed again and named in the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// Its id, as text.
    pub id: String,
    /// Its name, from its last Hello.
    #[serde(default)]
    pub name: String,
    /// The folders it is a member of (ids, as text).
    #[serde(default)]
    pub folders: Vec<String>,
    /// Where it was last reached or said it can be reached (`ip:port`), newest first, at most 8.
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// The whole file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// Device-wide settings.
    pub settings: Settings,
    /// The folders.
    #[serde(rename = "folder")]
    pub folders: Vec<FolderEntry>,
    /// Devices met in those folders.
    #[serde(rename = "device")]
    pub devices: Vec<DeviceEntry>,
}

impl SyncConfig {
    /// Reads `path`; a missing file is the default configuration.
    pub fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e),
        }
    }

    /// Writes `path` atomically.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string(self).map_err(io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, format!("# Seedwell folder sync. Keys are in the Keychain, not here.\n{text}"))?;
        std::fs::rename(&tmp, path)
    }

    /// The entry of a folder.
    pub fn folder(&self, id: &str) -> Option<&FolderEntry> {
        self.folders.iter().find(|f| f.id == id)
    }

    /// The entry of a folder, to change.
    pub fn folder_mut(&mut self, id: &str) -> Option<&mut FolderEntry> {
        self.folders.iter_mut().find(|f| f.id == id)
    }

    /// The entry of a device, made when missing.
    pub fn device_mut(&mut self, id: &str) -> &mut DeviceEntry {
        if let Some(i) = self.devices.iter().position(|d| d.id == id) {
            return &mut self.devices[i];
        }
        self.devices.push(DeviceEntry { id: id.to_string(), name: String::new(), folders: Vec::new(), addresses: Vec::new() });
        let last = self.devices.len() - 1;
        &mut self.devices[last]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("sync.toml");
        assert_eq!(SyncConfig::load(&p).unwrap(), SyncConfig::default());
        let mut c = SyncConfig::default();
        c.settings.port = 22100;
        c.folders.push(FolderEntry::new("ABC".into(), "/tmp/x".into()));
        c.save(&p).unwrap();
        assert_eq!(SyncConfig::load(&p).unwrap(), c);
        std::fs::write(&p, "[[folder]]\nid = \"X\"\npath = \"/y\"\n").unwrap();
        let c = SyncConfig::load(&p).unwrap();
        assert!(c.folders[0].archive && c.settings.lan_discovery, "defaults fill in");
    }
}
