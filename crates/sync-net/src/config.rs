//! `sync.toml`: device-wide settings and the lists of folders and channels. Keys are never in it (see
//! [`crate::secrets`]).
//!
//! The file is written by the service (atomically: a temp file renamed over it) and read at start.
//! Keys this version does not know are dropped when it rewrites the file. `format` says which layout it has
//! ([`CONFIG_FORMAT`]; a file without it is format 1): a later version raises it only when an older one would
//! misread the file, and an older one then refuses it instead of dropping what it does not know.

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
    /// The folder's public key, hex (empty in folders made by the test builds before roles: refused).
    #[serde(default)]
    pub folder_pk: String,
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
    /// The channel that goes with it (its id; empty: none): a setting of this device, which a link for both
    /// sets on the devices that join with it.
    #[serde(default)]
    pub channel: String,
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
        FolderEntry {
            id,
            folder_pk: String::new(),
            path,
            paused: false,
            archive: true,
            archive_days: 30,
            lan_only: false,
            ignore: Vec::new(),
            addresses: Vec::new(),
            channel: String::new(),
        }
    }
}

/// Private messages on this device (`[direct]`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct DirectEntry {
    /// What agents on this Mac may do with private messages: `off` (the default), `read` or `post`.
    pub agents: String,
    /// The sequence number of this device's last private message.
    pub seq: u64,
    /// The last message read with each device: `<device id>:<time>:<seq>:<message id>`.
    pub read: Vec<String>,
}

/// One channel on this device (`[[channel]]`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelEntry {
    /// The channel id, as text.
    pub id: String,
    /// The channel's public key, hex.
    pub channel_pk: String,
    /// Its name on this device.
    pub name: String,
    /// No notifications for it (an @mention still notifies: the app's rule).
    pub muted: bool,
    /// What agents on this Mac may do in it: `off` (the default), `read` or `post`.
    pub agents: String,
    /// The command run for each message from another device while agents may read it (empty: none).
    pub wake: String,
    /// The last message read here: its time and id (hex).
    pub read_at: u64,
    /// See `read_at`: the message's sequence number.
    pub read_seq: u64,
    /// See `read_at`.
    pub read_id: String,
    /// The last message read in each topic: `<topic id>:<time>:<seq>:<message id>`.
    pub topic_read: Vec<String>,
    /// The last message each agent of this Mac was given: `<time>:<seq>:<message id>:<agent>\u{1f}<place>`.
    pub agent_read: Vec<String>,
    /// The sequence number of this device's last message in it.
    pub seq: u64,
    /// Addresses typed in by the user for devices of this channel (`host:port`).
    pub addresses: Vec<String>,
}

/// A device this one has synced with: remembered so it can be dialed again and named in the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// Its id, as text.
    pub id: String,
    /// Its name, from its last Hello.
    #[serde(default)]
    pub name: String,
    /// The folders and channels it is a member of (ids, as text).
    #[serde(default)]
    pub folders: Vec<String>,
    /// Where it was last reached or said it can be reached (`ip:port`), newest first, at most 8.
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// The layout of `sync.toml` this version writes and reads.
pub const CONFIG_FORMAT: u32 = 1;

/// The whole file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncConfig {
    /// The file's layout ([`CONFIG_FORMAT`]).
    pub format: u32,
    /// Device-wide settings.
    pub settings: Settings,
    /// Private messages: this Mac's settings.
    pub direct: DirectEntry,
    /// The folders.
    #[serde(rename = "folder")]
    pub folders: Vec<FolderEntry>,
    /// The channels.
    #[serde(rename = "channel")]
    pub channels: Vec<ChannelEntry>,
    /// Devices met in those folders.
    #[serde(rename = "device")]
    pub devices: Vec<DeviceEntry>,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig { format: CONFIG_FORMAT, settings: Settings::default(), direct: DirectEntry::default(), folders: Vec::new(), channels: Vec::new(), devices: Vec::new() }
    }
}

impl SyncConfig {
    /// Reads `path`; a missing file is the default configuration. A file of a later format is refused.
    pub fn load(path: &Path) -> io::Result<Self> {
        let bad = |why: String| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {why}", path.display()));
        let c: SyncConfig = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| bad(e.to_string()))?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        if c.format > CONFIG_FORMAT {
            return Err(bad(format!("written by a newer Seedwell (format {}): update Seedwell", c.format)));
        }
        Ok(c)
    }

    /// Writes `path` atomically, in this version's format.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let text = toml::to_string(&SyncConfig { format: CONFIG_FORMAT, ..self.clone() }).map_err(io::Error::other)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, format!("# Seedwell folder sync. Secrets are kept elsewhere (the Keychain or a 0600 file), not here.\n{text}"))?;
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

    /// The entry of a channel, to change.
    pub fn channel_mut(&mut self, id: &str) -> Option<&mut ChannelEntry> {
        self.channels.iter_mut().find(|c| c.id == id)
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
        assert_eq!(c.format, CONFIG_FORMAT, "a file without a format is format 1");
        assert!(std::fs::read_to_string(&p).is_ok_and(|t| !t.contains("format")));
        c.save(&p).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("format = 1\n"));
        std::fs::write(&p, "format = 2\n[settings]\nport = 1\n").unwrap();
        let e = SyncConfig::load(&p).unwrap_err().to_string();
        assert!(e.contains("newer Seedwell"), "{e}");
    }
}
