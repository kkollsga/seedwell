//! Engine configuration. One plain struct; the app builds it from `config.toml` and can replace it
//! at runtime with `EngineHandle::update_config`.

use std::net::{Ipv4Addr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use trs_classify::Category;
pub use trs_state::config::Schedule as AltSchedule;

/// Timings that drive the Cold / Warm / Hot power tiers (SPEC 5.1, ENGINE.md 3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierTimings {
    /// Hot torrents with no interest and no download for this long drop to Warm.
    pub hot_to_warm: Duration,
    /// Warm torrents with no interest for this long drop to Cold. With the Hot step the total is
    /// the SPEC's "N minutes (default 10) without peer interest".
    pub warm_to_cold: Duration,
    /// A Warm torrent that uploaded without pause for this long becomes Hot.
    pub upload_for_hot: Duration,
    /// A Warm torrent with at least this many interested peers becomes Hot.
    pub interested_for_hot: usize,
}

impl Default for TierTimings {
    fn default() -> Self {
        Self {
            hot_to_warm: Duration::from_secs(5 * 60),
            warm_to_cold: Duration::from_secs(5 * 60),
            upload_for_hot: Duration::from_secs(60),
            interested_for_hot: 2,
        }
    }
}

/// A category's overrides of the two locations (SPEC 7A.4); `None` = the app default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CategoryLocation {
    /// Staging root for this category's unfinished downloads (`<dir>/<info-hash>/`).
    pub incomplete: Option<PathBuf>,
    /// Finished location.
    pub finished: Option<PathBuf>,
}

/// When the engine holds a torrent for a choice before downloading anything (SPEC 7A.6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AskMode {
    /// Never hold: everything starts at once (the engine's default, as before smart add).
    #[default]
    Never,
    /// Hold when there is a real choice: several items, items already in the library, platform
    /// variants, a very large torrent, an unsure category.
    Smart,
    /// Hold every torrent that has files to choose from.
    Always,
}

/// Smart add settings (SPEC 7A.6, Settings > Transfers).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SmartAddConfig {
    /// When to hold.
    pub mode: AskMode,
    /// A torrent larger than this many bytes is held ("Very large"); 0 = no size trigger. The
    /// free space at the destination is a trigger of its own and always applies.
    pub large_bytes: u64,
    /// Test hook: pretend this many bytes are free at every destination.
    pub free_space_override: Option<u64>,
}

impl Default for SmartAddConfig {
    fn default() -> Self {
        Self { mode: AskMode::Never, large_bytes: 50_000_000_000, free_space_override: None }
    }
}

/// Content detection settings (SPEC 7A.2). `EngineConfig::detect == None` switches detection off:
/// no categories, no junk skipping, rules see every torrent as `Other`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetectionConfig {
    /// Leave samples, `.nfo` and other junk unchecked (priority 0).
    pub skip_junk: bool,
    /// Ask (in watch folders) when the confidence is below the threshold.
    pub ask_when_unsure: bool,
    /// Confidence below which the category is `Other` and the user is asked.
    pub confidence_threshold: f32,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self { skip_junk: true, ask_when_unsure: true, confidence_threshold: 0.8 }
    }
}

/// Peer-wire encryption (MSE): how connections are opened and which ones are accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum EncryptionPolicy {
    /// Plain handshakes only; an encrypted handshake is refused.
    Off,
    /// Offer encryption on outgoing connections (falling back to plain), accept both. After the
    /// handshake the stream stays plain when the peer allows it.
    #[default]
    Prefer,
    /// Encrypted (RC4) connections only, both ways.
    Require,
}

/// What to do with a `.torrent` after it was added.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AfterAdd {
    /// Move to the Trash (`~/.Trash`, with a non-colliding name).
    Trash,
    /// Move to this folder.
    Archive(PathBuf),
    /// Leave it where it is.
    Leave,
    /// Delete it.
    Delete,
}

/// Whether a watch folder adds on its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchMode {
    /// Add without asking (unless the category is uncertain).
    Auto,
    /// Emit an [`crate::EngineEvent::AddRequest`] first.
    Ask,
}

/// One watched folder (SPEC 7A.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchConfig {
    /// Folder (absolute, `~` already expanded).
    pub path: PathBuf,
    /// Auto-add or ask.
    pub mode: WatchMode,
    /// What happens to the `.torrent`.
    pub after_add: AfterAdd,
    /// Forced category.
    pub category: Option<Category>,
    /// Forced rule id.
    pub rule: Option<String>,
    /// Only `.torrent` files that arrived after this moment are picked up (SPEC 7A.1); older ones
    /// are the folder's backlog ([`crate::EngineHandle::watch_backlog`]). `None` means "the
    /// moment this folder joined the list in this process", so a caller that sets nothing never
    /// mass-adds old files; `trs_state::Config` keeps the persisted value (`since`).
    pub since: Option<std::time::SystemTime>,
}

/// Port mapping settings (Network tab).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct PortmapConfig {
    /// Map the listen port at all.
    pub enabled: bool,
    /// UPnP IGD.
    pub upnp: bool,
    /// NAT-PMP and PCP.
    pub natpmp: bool,
    /// Tests: talk to a gateway at this address (NAT-PMP/PCP port included) instead of finding
    /// the default gateway.
    pub test_gateway: Option<SocketAddrV4>,
}

/// Direct download settings (SPEC 7D).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectConfig {
    /// Connections per host, across all direct downloads (SPEC: at most 4).
    pub max_connections_per_host: usize,
    /// Parallel ranges per file.
    pub connections_per_download: u32,
}

impl Default for DirectConfig {
    fn default() -> Self {
        Self { max_connections_per_host: 4, connections_per_download: 4 }
    }
}

/// Crash simulation for moves (tests only): called with a step number before every file-system
/// change of a relocation; returning `false` stops the move right there and leaves everything
/// as a crash would.
#[derive(Clone)]
pub struct MoveFaultFn(pub trs_storage::FaultHook);

impl std::fmt::Debug for MoveFaultFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MoveFaultFn")
    }
}

/// Test hook: called while an edge file is being renamed, after the rename and before the new
/// layout is installed, with storage's path gate held (see `switch_layout_renaming`). A test makes
/// it slow to give a concurrent write every chance to open the old path.
#[derive(Clone)]
pub struct EdgeHookFn(pub Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for EdgeHookFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EdgeHookFn")
    }
}

/// Source of "now" for the alt-speed schedule: `(unix seconds, UTC offset in seconds)`.
/// `None` in the config uses the system clock and the local zone.
#[derive(Clone)]
pub struct ClockFn(pub Arc<dyn Fn() -> (i64, i32) + Send + Sync>);

impl std::fmt::Debug for ClockFn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClockFn")
    }
}

/// Everything configurable. `Default` gives the SPEC defaults.
#[derive(Clone, Debug)]
pub struct EngineConfig {
    /// Address the TCP listener binds to.
    pub listen_addr: Ipv4Addr,
    /// TCP listen port; 0 picks a free port (read it back with `EngineHandle::listen_port`). Changing it
    /// at runtime rebinds live (`0` = a fresh random port).
    pub listen_port: u16,
    /// Default save directory for torrents added without one.
    pub default_save_dir: PathBuf,
    /// Write incomplete torrents below `<save>/.incomplete` and move them into place when done.
    pub use_staging: bool,
    /// Global cap on peer connections (SPEC 5.2: 400).
    pub max_connections: usize,
    /// Cap on connections of one Hot torrent.
    pub max_connections_per_torrent: usize,
    /// Cap on connections of one Warm torrent (ENGINE.md 3: "small peer set (<= 8)").
    pub warm_connections_per_torrent: usize,
    /// Cap on connections that are still connecting or handshaking.
    pub max_half_open: usize,
    /// Downloads running at once (torrents that still need data). Higher-priority torrents are exempt.
    pub max_active_downloads: usize,
    /// Global download limit in bytes/s.
    pub down_limit: Option<u64>,
    /// Global upload limit in bytes/s.
    pub up_limit: Option<u64>,
    /// Download limit while alt-speed is on.
    pub alt_down_limit: Option<u64>,
    /// Upload limit while alt-speed is on.
    pub alt_up_limit: Option<u64>,
    /// Alt-speed ("Limit Speed") switched on by hand (the toggle).
    pub alt_speed: bool,
    /// Automatic alt-speed window; the effective state is `alt_speed || (window open)`.
    pub alt_schedule: Option<AltSchedule>,
    /// Runtime state kept by the session: the schedule's window is open right now.
    pub alt_scheduled_on: bool,
    /// Upload slots shared by all torrents when the upload rate is unlimited.
    pub upload_slots_unlimited: usize,
    /// Run the DHT for public torrents.
    pub dht_enabled: bool,
    /// UDP address of the DHT node.
    pub dht_bind: SocketAddrV4,
    /// DHT bootstrap routers (`host:port`); empty uses the library defaults.
    pub dht_bootstrap: Vec<String>,
    /// Accept loopback/private DHT nodes (tests only).
    pub dht_allow_local: bool,
    /// The master switch for `ut_pex` (SPEC 7B.4): public torrents use it when on; private torrents
    /// additionally need their tracker to allow it (`trackers.toml`). Off = no PEX at all.
    pub pex_enabled: bool,
    /// Peer-wire encryption (`network.encryption`).
    pub encryption: EncryptionPolicy,
    /// uTP (BEP 29) on the UDP port shared with the DHT (`network.utp`). Incoming uTP is accepted
    /// and outgoing connections use it for peers known to support it; TCP stays the default.
    pub utp: bool,
    /// Dial TCP at all. Off means every outgoing connection is uTP (tests, uTP-only swarms).
    pub outgoing_tcp: bool,
    /// How long the uTP leg of a dial may take when TCP is the other option.
    pub utp_probe_timeout: Duration,
    /// Talk to trackers at all (tests of pure peer-to-peer behaviour turn it off).
    pub trackers_enabled: bool,
    /// Peers requested per announce.
    pub numwant: u32,
    /// Power-tier timings.
    pub tiers: TierTimings,
    /// Disk I/O threads.
    pub io_threads: usize,
    /// Hash threads; 0 means "performance cores - 1".
    pub hash_threads: usize,
    /// Write cache budget in bytes (SPEC 5.2: 32 MiB).
    pub write_cache_bytes: usize,
    /// Shared file-descriptor cache capacity.
    pub fd_cache: usize,
    /// Hash failures attributed to one peer before it is banned.
    pub max_hash_failures_per_peer: u32,
    /// Largest `ut_metadata` we accept (wire hard cap is 16 MiB).
    pub max_metadata_size: u32,
    /// Version string for the User-Agent and the extended handshake `v`.
    pub client_version: String,
    /// TCP connect timeout for outgoing peers.
    pub connect_timeout: Duration,
    /// Handshake must complete within this long.
    pub handshake_timeout: Duration,
    /// Tests: a torrent's startup (reading its `.torrent`, checking its files) takes at least this long,
    /// like a disk that has to spin up.
    pub test_startup_delay: Duration,
    /// Persistence writes are batched for this long (ENGINE.md 7: 2 s).
    pub persist_debounce: Duration,
    /// Resume data of a downloading torrent is written at most this often.
    pub resume_interval: Duration,
    /// How often a downloading torrent re-evaluates its peers' choke state.
    pub rechoke_interval: Duration,
    /// Run a full recheck for torrents added with existing data (otherwise trust nothing and check
    /// only what is on disk). Always true except in benchmarks that pre-seed resume data.
    pub verify_on_add: bool,
    /// Staging folder for unfinished downloads (`<dir>/<info-hash>/...`). `None` with
    /// `use_staging` stages below `<save>/.incomplete`.
    pub incomplete_dir: Option<PathBuf>,
    /// Reserve the full size of every file up front instead of sparse files.
    pub preallocate: bool,
    /// Content detection; `None` = off.
    pub detect: Option<DetectionConfig>,
    /// Organize finished torrents into `Active/`, `Dormant/`, `Stopped/`, `Dead/` below their
    /// finished location (SPEC 7A.3).
    pub by_status: bool,
    /// How often the status folders are re-evaluated (one hour; tests shorten it).
    pub status_interval: Duration,
    /// Watched folders.
    pub watch: Vec<WatchConfig>,
    /// Per-category location overrides, indexed by `Category::index()` (SPEC 7A.4).
    pub category_locations: [CategoryLocation; 8],
    /// Smart add: when a torrent is held for a choice (SPEC 7A.6).
    pub smart_add: SmartAddConfig,
    /// Grace period in seconds before a torrent that would start without asking actually starts
    /// (SPEC 7A.7): it is held, silent on the network, and the app counts down. 0 = start at
    /// once (the engine default; the settings file defaults to 10).
    pub start_delay_secs: u32,
    /// Seconds in one "hour" of the inactive stop condition's window (SPEC §7.5). 3600; tests set 1.
    pub idle_hour_secs: u32,
    /// Byte/s limit on cross-volume copies of moves (0 = unlimited, the default: the copy is
    /// unbuffered and one per destination volume, so seeding stays responsive).
    pub move_rate_limit: u64,
    /// Developer mode (`advanced.developer_mode`): log every tracker exchange, secrets masked.
    pub developer_mode: bool,
    /// Developer mode forced on for this process (`seedwell daemon --dev-mode`): kept by every
    /// settings reload.
    pub developer_forced: bool,
    /// Folder of `trackers.log` (`<state dir>/logs`); `None` = the log writes nothing.
    pub devlog_dir: Option<PathBuf>,
    /// Free space that must stay on the destination volume after a cross-volume copy (SPEC 7A.5:
    /// 1 GB, on top of the torrent's own size).
    pub move_reserve_bytes: u64,
    /// Test hook: simulate a crash in the middle of a move.
    pub move_fault: Option<MoveFaultFn>,
    /// Test hook: runs in the middle of the rename of an edge file (see [`EdgeHookFn`]).
    pub edge_hook: Option<EdgeHookFn>,
    /// Test hook: copy even when a move stays on one volume (exercises the cross-volume path
    /// on a single disk). Never set outside tests.
    pub force_copy_moves: bool,
    /// `.torrent` handling for adds that did not come from a watch folder, when the caller
    /// asks the engine to finish it ([`crate::EngineHandle::add_torrent_path`]).
    pub torrent_after_add: AfterAdd,
    /// Port mapping.
    pub portmap: PortmapConfig,
    /// Direct downloads.
    pub direct: DirectConfig,
    /// Where "Trash" is; `None` = `~/.Trash`.
    pub trash_dir: Option<PathBuf>,
    /// Test hook for the schedule's notion of "now".
    pub clock: Option<ClockFn>,
    /// Where trackers may live. The default refuses loopback, link-local, cloud metadata and
    /// private networks (`trs-tracker`'s production policy); tests opt in to loopback, and a
    /// "my tracker is on my LAN/VPN" setting would set `allow_lan_trackers`.
    pub tracker_destinations: trs_tracker::policy::DestinationPolicy,
    /// How many torrents may hash their files at the same time (add with existing data, Recheck,
    /// reconnect after an unplug, launch): 2. A constant of the engine, not a user setting; the
    /// rest wait without an actor ("Waiting to verify"). Read once at start.
    pub verify_slots: usize,
    /// One "minute" of the retry schedules of faults that clear by themselves (missing data 1, 5,
    /// 15, 60 min; disk full / read-only 10, 20, 40, 60 min). 60 s; tests shorten it.
    pub retry_minute: Duration,
    /// A running download below this payload rate (bytes/s) for `stall_after` stops counting
    /// toward `max_active_downloads` (2 KiB/s, qBittorrent's "slow torrent threshold").
    pub stall_rate: u64,
    /// How long a download must stay below `stall_rate` (or above it again) before the queue
    /// changes its mind (60 s, hysteresis both ways).
    pub stall_after: Duration,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            listen_addr: Ipv4Addr::UNSPECIFIED,
            listen_port: 0,
            default_save_dir: PathBuf::from("."),
            use_staging: false,
            max_connections: 400,
            max_connections_per_torrent: 80,
            warm_connections_per_torrent: 8,
            max_half_open: 32,
            max_active_downloads: 3,
            down_limit: None,
            up_limit: None,
            alt_down_limit: None,
            alt_up_limit: None,
            alt_speed: false,
            alt_schedule: None,
            alt_scheduled_on: false,
            upload_slots_unlimited: 40,
            dht_enabled: true,
            dht_bind: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
            dht_bootstrap: Vec::new(),
            dht_allow_local: false,
            pex_enabled: true,
            encryption: EncryptionPolicy::Prefer,
            utp: true,
            outgoing_tcp: true,
            utp_probe_timeout: Duration::from_secs(5),
            trackers_enabled: true,
            numwant: 50,
            tiers: TierTimings::default(),
            io_threads: 4,
            hash_threads: 0,
            write_cache_bytes: 32 << 20,
            fd_cache: 512,
            max_hash_failures_per_peer: 3,
            max_metadata_size: 16 << 20,
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
            connect_timeout: Duration::from_secs(10),
            handshake_timeout: Duration::from_secs(10),
            test_startup_delay: Duration::ZERO,
            persist_debounce: Duration::from_secs(2),
            resume_interval: Duration::from_secs(30),
            rechoke_interval: Duration::from_secs(10),
            verify_on_add: true,
            incomplete_dir: None,
            preallocate: false,
            detect: None,
            by_status: false,
            status_interval: Duration::from_secs(3600),
            watch: Vec::new(),
            category_locations: Default::default(),
            smart_add: SmartAddConfig::default(),
            start_delay_secs: 0,
            idle_hour_secs: 3600,
            move_rate_limit: 0,
            developer_mode: false,
            developer_forced: false,
            devlog_dir: None,
            move_reserve_bytes: 1 << 30,
            move_fault: None,
            edge_hook: None,
            force_copy_moves: false,
            torrent_after_add: AfterAdd::Leave,
            portmap: PortmapConfig::default(),
            direct: DirectConfig::default(),
            trash_dir: None,
            clock: None,
            tracker_destinations: trs_tracker::policy::DestinationPolicy::STRICT,
            verify_slots: 2,
            retry_minute: Duration::from_secs(60),
            stall_rate: 2048,
            stall_after: Duration::from_secs(60),
        }
    }
}

impl EngineConfig {
    /// Alternative limits in force: the manual toggle or the schedule's window.
    pub fn alt_active(&self) -> bool {
        self.alt_speed || self.alt_scheduled_on
    }

    /// Effective global download limit, alt-speed applied.
    pub fn effective_down(&self) -> Option<u64> {
        if self.alt_active() { self.alt_down_limit } else { self.down_limit }
    }

    /// Effective global upload limit, alt-speed applied.
    pub fn effective_up(&self) -> Option<u64> {
        if self.alt_active() { self.alt_up_limit } else { self.up_limit }
    }

    /// Total upload slots across all torrents, sized from the upload limit (ENGINE.md 5).
    pub fn total_upload_slots(&self) -> usize {
        match self.effective_up() {
            // About one slot per 30 KiB/s: enough parallelism to fill the pipe, few enough that
            // each peer still gets a useful rate.
            Some(limit) => ((limit / (30 * 1024)) as usize).clamp(4, 200),
            None => self.upload_slots_unlimited.max(4),
        }
    }

    /// Peer cap for a torrent in a tier.
    pub fn peer_cap(&self, hot: bool) -> usize {
        if hot { self.max_connections_per_torrent } else { self.warm_connections_per_torrent }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alt_speed_selects_limits() {
        let mut c = EngineConfig {
            down_limit: Some(10),
            up_limit: Some(20),
            alt_down_limit: Some(1),
            alt_up_limit: Some(2),
            ..EngineConfig::default()
        };
        assert_eq!((c.effective_down(), c.effective_up()), (Some(10), Some(20)));
        c.alt_speed = true;
        assert_eq!((c.effective_down(), c.effective_up()), (Some(1), Some(2)));
    }

    #[test]
    fn upload_slots_scale_with_limit() {
        let mut c = EngineConfig::default();
        assert_eq!(c.total_upload_slots(), 40);
        c.up_limit = Some(10 * 1024);
        assert_eq!(c.total_upload_slots(), 4);
        c.up_limit = Some(300 * 1024);
        assert_eq!(c.total_upload_slots(), 10);
        c.up_limit = Some(1 << 40);
        assert_eq!(c.total_upload_slots(), 200);
    }
}
