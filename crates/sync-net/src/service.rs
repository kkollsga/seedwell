//! The sync service: folders, the listener, dialing, discovery, and the API the app, the CLI and agents
//! use ([`SyncHandle`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc, watch};
use tokio::task::JoinHandle;
use trs_sync::archive::Retention;
use trs_sync::clock::SystemClock;
use trs_sync::folder::{FolderConfig, FolderCore};
use trs_sync::fs::{Fs, StdFs};
use trs_sync::ignore::Ignore;
use trs_sync::store_redb::{RedbReader, RedbStore};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::keys::{Access, FolderKey, FolderSecret};
use trs_sync_proto::members::{Body, InviteLink, Membership, Role, Signer, Statement, hex, new_invite};
use trs_sync_proto::model::STATE_DIR;
use trs_sync_proto::reach::LinkReach;
use trs_sync_proto::msg::{AddressRecord, Addresses, Message};

use crate::config::{FolderEntry, Settings, SyncConfig};
use crate::secrets::SecretStore;
use crate::session::{self, Accepted, Via};
use crate::shared::{Ctx, FolderShared, HubMsg, Live, Names, Registry, Space, SpaceKind, lock};
use crate::status::{DeviceStatus, FolderState, FolderStatus, JoinAttemptStatus, OfflineDevice, Snapshot};
use crate::tls::Tls;

/// A boxed future (the DHT adapter's answers).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Public peer discovery, lent by the torrent engine (its Mainline DHT). Topics are public and carry
/// nothing but a hash, so this never touches private torrents.
pub trait PublicDiscovery: Send + Sync {
    /// Announces `port` under `topic` and returns the peers found there.
    fn announce(&self, topic: [u8; 20], port: u16) -> BoxFuture<'_, Vec<SocketAddr>>;
    /// Looks up `topic` without announcing.
    fn lookup(&self, topic: [u8; 20]) -> BoxFuture<'_, Vec<SocketAddr>>;
    /// This device's public address and the port the router opened for sync, when the port mapping knows them
    /// (not behind a carrier-grade NAT): put in the links this device makes.
    fn public_address(&self) -> Option<SocketAddr> {
        None
    }
}

/// How to start the service.
pub struct Options {
    /// Seedwell's state directory; sync keeps `sync.toml` and `sync/` in it.
    pub state_dir: PathBuf,
    /// `Seedwell <version>`.
    pub client: String,
    /// The device name when none is set (the computer's name).
    pub default_device_name: String,
    /// Where keys are kept.
    pub secrets: Arc<dyn SecretStore>,
    /// The DHT, when the engine runs one.
    pub discovery: Option<Arc<dyn PublicDiscovery>>,
    /// Listen on this address (tests use loopback; the app all interfaces).
    pub bind: IpAddr,
    /// Run LAN discovery (off in tests that run several devices on one machine).
    pub lan_discovery: bool,
    /// How often a folder held back by an unfinished move (its original's disk not there) is tried again, besides
    /// [`SyncHandle::volumes_changed`].
    pub held_retry: Duration,
    /// LAN discovery's UDP port ([`crate::lan::PORT`]; tests pick their own).
    pub lan_port: u16,
    /// Where every LAN announcement also goes, by unicast (tests: the other services on loopback). With
    /// `bind` on one address these are the only destinations; broadcast and multicast need all interfaces.
    pub lan_targets: Vec<SocketAddr>,
    /// Run uTP on the sync port's UDP twin (dialed with TCP for Internet addresses; hole punching).
    pub utp: bool,
    /// Dial over TCP (tests turn it off to force uTP).
    pub dial_tcp: bool,
    /// **Tests:** at most this many direct dials, from the start (`None`: no limit). See
    /// [`SyncHandle::test_dials_left`].
    pub dials: Option<u32>,
    /// The protocol versions to speak, lowest and highest ([`trs_sync_proto::msg::MIN_PROTOCOL`],
    /// [`trs_sync_proto::msg::PROTOCOL`]); tests play older or newer devices with others.
    pub protocol: (u32, u32),
    /// The feature bits to offer ([`trs_sync_proto::msg::FEATURES`]).
    pub features: u64,
    /// An isolated (test) run: the real home (the user profile on Windows). A folder inside it is refused (a
    /// temporary folder excepted); a first start finds no device over the Internet and relays nothing (LAN only),
    /// and joined folders go to `Sync` in the state folder unless another place is set.
    pub forbidden_root: Option<PathBuf>,
    /// A headless daemon's switch for private messages (`seedwell daemon --chat-agents`), which has no app for the
    /// user to set it in: private messages are at this level for agents for the run, in memory only (never written
    /// to `sync.toml`). Channels need none: each says itself whether it is open to agents. The app leaves it `None`.
    pub chat_agents: Option<crate::chat::AgentAccess>,
    /// The host that shows on-demand folders in the file manager, once there is one (the app sets it when it serves
    /// the Mac's File Provider; a headless daemon has none). Shared, so it outlives a restart of the service.
    pub on_demand_host: OnDemandHostCell,
    /// This device keeps folders on demand ([`trs_platform::ON_DEMAND_FOLDERS`]: a file manager host shows them). Off,
    /// a folder that is on demand on the device that put it on all the user's devices is joined here in full, and its
    /// page says why.
    pub on_demand: bool,
    /// Seedwell's own torrents, when the engine runs beside the service (the app, the daemon): a folder's files are
    /// never moved from under one ([`LocalTorrents`]).
    pub torrents: Option<Arc<dyn LocalTorrents>>,
}

/// Seedwell's own torrents, which may seed from inside a synced folder (SPEC §7G.11, *Modes*). Lent by whoever runs the
/// engine beside the service (`trs_control::sync::start_sync_with`). Freeing a folder's local copies (its files moved
/// into this device's cache) is refused while a torrent uses files in it, and after any move of a folder's files a
/// torrent that missed its files looks again (they may be back).
pub trait LocalTorrents: Send + Sync {
    /// The names of the torrents whose files are at or below `path` (absolute), or that hold it; an error with the
    /// reason when that cannot be told now (the engine did not answer in time). Answers within seconds.
    fn using(&self, path: &Path) -> BoxFuture<'static, Result<Vec<String>, String>>;
    /// A folder's files moved on this device (a switch, a move, a removal): torrents missing their files look again.
    /// Returns at once.
    fn moved(&self);
}

/// The host that shows on-demand folders in the file manager (the Mac's File Provider domain, SPEC 7G.11). Before a
/// folder is removed or switched to full, by whoever asks (the app, the control socket, an agent), and after the
/// checks that need no host, the host lets go of it, so its place is an ordinary folder again.
pub trait OnDemandHost: Send + Sync {
    /// Lets go of folder `folder` (called `name`, at `place`): its pending changes handed over first, what the system
    /// kept put where the user finds it. Blocks until done. An error refuses the removal or switch: nothing changes.
    fn release(&self, folder: &str, name: &str, place: &Path) -> Result<(), String>;
    /// The removal or switch is over, done or not (the host shows the folder again if it is still on demand).
    fn settled(&self, folder: &str);
}

/// Where the host is, if there is one ([`Options::on_demand_host`]).
pub type OnDemandHostCell = Arc<Mutex<Option<Arc<dyn OnDemandHost>>>>;

/// Tells the host a removal or switch is over, however it ended.
pub(crate) struct Settle {
    host: Arc<dyn OnDemandHost>,
    folder: String,
}

impl Drop for Settle {
    fn drop(&mut self) {
        self.host.settled(&self.folder);
    }
}

impl Options {
    /// The defaults for a state directory.
    pub fn new(state_dir: PathBuf, client: String, secrets: Arc<dyn SecretStore>) -> Self {
        Options {
            state_dir,
            client,
            default_device_name: "Mac".into(),
            secrets,
            discovery: None,
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            lan_discovery: true,
            held_retry: Duration::from_secs(60),
            lan_port: crate::lan::PORT,
            lan_targets: Vec::new(),
            utp: true,
            dial_tcp: true,
            dials: None,
            protocol: (trs_sync_proto::msg::MIN_PROTOCOL, trs_sync_proto::msg::PROTOCOL),
            features: trs_sync_proto::msg::FEATURES,
            forbidden_root: None,
            chat_agents: None,
            on_demand_host: Arc::new(Mutex::new(None)),
            on_demand: trs_platform::ON_DEMAND_FOLDERS,
            torrents: None,
        }
    }
}

/// Why a request was refused.
#[derive(Debug)]
pub enum SyncError {
    /// No such folder.
    NotFound,
    /// An on-demand call refused for a reason a host acts on (SPEC 7G.11), with the words to show.
    OnDemand(OnDemandError, String),
    /// The request is not acceptable (a bad key, a path in use, …), with the reason.
    Invalid(String),
    /// Reading or writing failed.
    Io(io::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::NotFound => f.write_str("no such folder"),
            SyncError::OnDemand(_, why) => f.write_str(why),
            SyncError::Invalid(why) => f.write_str(why),
            SyncError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncError {}

/// What the host saw of an item it deletes (SPEC 7G.11): the delete is refused when that moved on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeleteSeen {
    /// A folder: the newest sequence below it then (another device's version below since refuses).
    Folder(u64),
    /// A file: its content version then.
    File(String),
}

/// Why an on-demand call failed, for a host that acts on it (a File Provider error, a Cloud Files status).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnDemandError {
    /// The folder is not on demand.
    NotOnDemand,
    /// No such item (any more).
    NoItem,
    /// The anchor is from before the folder's index was made anew: list everything again.
    AnchorExpired,
    /// No device has this version right now.
    Unavailable,
    /// A fetch waited its time and the content is not here yet.
    NotHereYet,
    /// Something there changed since the host saw it (a folder deleted with a version that is not its newest).
    Changed,
    /// A name the folder does not sync (SPEC §7G.2): nothing at it was recorded; a rename onto it took the item out of
    /// the folder (deleted on every device; the content stays the host's).
    Ignored,
    /// A rename onto a name the folder does not sync, of content not on this device: nothing changed.
    IgnoredNotHere,
}

pub(crate) fn on_demand(kind: OnDemandError, why: impl Into<String>) -> SyncError {
    SyncError::OnDemand(kind, why.into())
}

impl From<io::Error> for SyncError {
    fn from(e: io::Error) -> Self {
        SyncError::Io(e)
    }
}

#[path = "service_ondemand.rs"]
mod ondemand;
#[path = "service_placed.rs"]
mod placed_api;
pub use placed_api::LocalChange;
pub(crate) use ondemand::check_kept;
pub use ondemand::{FETCH_GRACE, FETCH_STALL, FetchProgress, FetchWait, Fetched};

pub(crate) fn invalid(why: impl Into<String>) -> SyncError {
    SyncError::Invalid(why.into())
}

/// Which version of a conflict to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    /// The file at the original path (the conflict copy goes to the archive).
    Original,
    /// The conflict copy (it replaces the original, which goes to the archive).
    Copy,
    /// Both: the copy becomes an ordinary file under a plain name.
    Both,
}

/// Which paths of a folder are busy, for Finder's badges. Cheap: read from what the folder keeps anyway.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    /// Paths still to fetch (or delete), the first in path order, wire form.
    pub pending: Vec<String>,
    /// More paths are pending than listed.
    pub pending_more: bool,
    /// Conflict copies not yet resolved, wire form.
    pub conflicts: Vec<String>,
    /// The folder has ignore patterns of its own (`.seedwellignore`, or local ones in the config): a path
    /// that is not pending is not necessarily synced.
    pub own_ignores: bool,
}

/// A device in a folder, for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberInfo {
    /// Its id.
    pub device: DeviceId,
    /// Its name: its own signed profile in the membership, else from its Hello or as remembered; empty when unknown.
    pub name: String,
    /// Its user's name, from the same profile (empty: none given).
    pub user: String,
    /// Its role.
    pub role: Role,
    /// A session with it is open now for this folder.
    pub online: bool,
    /// This device.
    pub is_me: bool,
    /// When its access ends: the folder's share end, for a device that is not an owner (Unix seconds); `None` =
    /// no end.
    pub expires_s: Option<u64>,
}

/// An invitation of a folder, for the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InviteInfo {
    /// Its id, hex.
    pub id: String,
    /// The link (empty for an invitation another device made: only its maker can show it).
    pub link: String,
    /// The role it admits at.
    pub role: Role,
    /// Admissions it allows; 0 = unlimited.
    pub max_uses: u32,
    /// Devices admitted with it.
    pub uses: u32,
    /// When it stops admitting, Unix seconds.
    pub expires_s: Option<u64>,
    /// When it was made.
    pub created_s: u64,
    /// Revoked.
    pub revoked: bool,
    /// For a folder's link that also invites to a channel: that channel's id.
    pub channel_id: Option<String>,
    /// And its name.
    pub channel_name: Option<String>,
}

pub(crate) struct Runner {
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

/// Where a device was reached or said it can be reached.
#[derive(Default)]
pub(crate) struct AddressBook {
    pub(crate) by_device: HashMap<DeviceId, BTreeSet<SocketAddr>>,
    /// Addresses found for a folder whose device we do not know yet (the DHT gives no ids).
    pub(crate) hints: HashMap<FolderId, BTreeSet<SocketAddr>>,
    /// Last dial attempt per address, for backoff.
    pub(crate) tried: HashMap<SocketAddr, (Instant, u32)>,
    /// Addresses whose last dial connected and whose session the device there turned down in its setup (nothing in
    /// common, a refused link, the device stopping): what made it refuse may change at any moment, so their wait is
    /// at most [`REFUSED_WAIT_MAX`], and it goes when a folder opens or closes here or a device is admitted.
    pub(crate) refused: BTreeSet<SocketAddr>,
    /// Addresses whose dial connected and whose session is being set up: not dialed again meanwhile, and not failing
    /// (no relay or hole punch for a device that answered).
    pub(crate) in_setup: BTreeSet<SocketAddr>,
    /// The newest signed address record of each device (passed on to other members).
    pub(crate) records: HashMap<DeviceId, AddressRecord>,
}

pub(crate) struct Inner {
    pub(crate) ctx: Arc<Ctx>,
    pub(crate) dir: PathBuf,
    pub(crate) cfg_path: PathBuf,
    pub(crate) cfg: Mutex<SyncConfig>,
    /// Taken by a save of `cfg` for its whole write ([`SyncHandle::save_config`]), never by a read.
    pub(crate) cfg_saving: Mutex<()>,
    /// A save of `cfg` is queued off the caller's thread ([`SyncHandle::save_config_soon`]) and has not started.
    pub(crate) cfg_save_queued: AtomicBool,
    /// **Tests:** how long a queued save waits once taken ([`SyncHandle::test_queued_save_wait`]).
    pub(crate) queued_save_wait_ms: AtomicU64,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) port: u16,
    /// The address the sync port is bound to ([`Options::bind`]).
    pub(crate) bind: IpAddr,
    /// LAN discovery runs (the option and the setting, at the start).
    pub(crate) lan_runs: bool,
    pub(crate) discovery: Option<Arc<dyn PublicDiscovery>>,
    pub(crate) runners: Mutex<HashMap<FolderId, Runner>>,
    pub(crate) book: Mutex<AddressBook>,
    pub(crate) tasks: Mutex<Vec<JoinHandle<()>>>,
    pub(crate) stopping: AtomicBool,
    pub(crate) wake_dialer: Notify,
    pub(crate) lan: crate::lan::Lan,
    /// uTP on the sync port's UDP twin.
    pub(crate) utp: Option<trs_utp::UtpSocket>,
    /// Dial over TCP (off in tests that force uTP).
    pub(crate) dial_tcp: bool,
    /// Relays and hole punching.
    pub(crate) net: crate::relay::Net,
    /// How long stopping a folder waits for it to let go ([`STOP_WAIT`]; shorter in tests).
    pub(crate) stop_wait: Mutex<Duration>,
    /// **Tests:** how long a moved folder stays closed before it opens again (a resumed one, before its sessions
    /// start anew).
    pub(crate) reopen_wait: Mutex<Duration>,
    /// **Tests:** how many more direct dials this device makes (`None`: no limit).
    pub(crate) dials_left: Mutex<Option<u32>>,
    /// **Tests:** direct dials made so far.
    pub(crate) dials_made: AtomicU64,
    /// How long a member that is not connected stays away before it counts as gone ([`crate::absence::GONE_AFTER_S`];
    /// shorter in tests).
    pub(crate) gone_after_s: AtomicU64,
    /// Folders held back by a move cut off that cannot be finished yet (the original's disk not there), with why: shown
    /// as missing, tried again when volumes change and on [`Options::held_retry`].
    pub(crate) held: Mutex<BTreeMap<FolderId, Held>>,
    /// One retry of the held-back folders at a time (a mount and the timer at once).
    held_retrying: tokio::sync::Mutex<()>,
    /// The runtime the service runs on (its synchronous methods are called from other threads too).
    pub(crate) rt: tokio::runtime::Handle,
    /// An isolated (test) run's forbidden root ([`Options::forbidden_root`]).
    pub(crate) forbidden_root: Option<PathBuf>,
    /// The file manager's host of on-demand folders ([`Options::on_demand_host`]).
    pub(crate) on_demand_host: OnDemandHostCell,
    /// This device keeps folders on demand ([`Options::on_demand`]).
    pub(crate) on_demand: bool,
    /// Seedwell's own torrents ([`Options::torrents`]).
    pub(crate) torrents: Option<Arc<dyn LocalTorrents>>,
    /// Network changes ([`crate::netchange`]).
    pub(crate) netwatch: crate::netchange::NetWatch,
    /// The rendezvous ([`crate::rendezvous`]).
    pub(crate) rendezvous: crate::rendezvous::State,
}

/// The running service. Cheap to clone.
#[derive(Clone)]
pub struct SyncHandle {
    pub(crate) inner: Arc<Inner>,
}

const DEVICE_KEY: &str = "device";

/// The page of a folder joined in full although it is on demand on the device that put it on all the user's devices.
pub const FULL_HERE: &str = concat!(
    "On demand on your other device, but this ",
    trs_platform::computer!(),
    " can't keep folders on demand yet: every file is downloaded here."
);

/// How long stopping a folder waits for its runtime and its index to let go.
const STOP_WAIT: Duration = Duration::from_secs(60);

/// A folder public key from its hex form in `sync.toml`.
pub(crate) fn public_key_from_hex(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    u64::from_be_bytes(b)
}

/// A user name as typed: control characters dropped, trimmed, at most 64 characters; empty = none.
fn clean_user_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).collect();
    name.trim().chars().take(trs_sync_proto::msg::MAX_DEVICE_NAME).collect::<String>().trim_end().to_string()
}

pub(crate) fn random_id() -> [u8; 16] {
    let mut b = [0u8; 16];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    b
}

impl SyncHandle {
    /// Starts the service: loads the device key (made on first start), the folders, binds the port.
    pub async fn start(opts: Options) -> io::Result<SyncHandle> {
        let dir = opts.state_dir.join("sync");
        std::fs::create_dir_all(&dir)?;
        let cfg_path = opts.state_dir.join("sync.toml");
        let first = !cfg_path.exists();
        let mut cfg = SyncConfig::load(&cfg_path)?;
        if opts.forbidden_root.is_some() {
            let s = &mut cfg.settings;
            if first {
                s.dht_discovery = false;
                s.relay_own_devices = false;
                s.relay_everyone = false;
            }
            // Never the user's ~/Seedwell: the run's own state. (A place set inside the forbidden root is refused
            // where it is set, and by the app's start.)
            if s.default_location.trim().is_empty() {
                s.default_location = opts.state_dir.join("Sync").display().to_string();
            }
            cfg.save(&cfg_path)?;
        }

        let key = match opts.secrets.get(DEVICE_KEY)? {
            Some(pkcs8) => DeviceKey::from_pkcs8(&pkcs8).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("device key: {e:?}")))?,
            None => {
                let k = DeviceKey::generate_system().map_err(io::Error::other)?;
                opts.secrets.put(DEVICE_KEY, &k.to_pkcs8())?;
                k
            }
        };
        let key = Arc::new(key);
        let tls = Tls::new(&key).map_err(io::Error::other)?;

        let (bind, listener, utp, foreign_rx) = bind_port(opts.bind, cfg.settings.port, opts.utp).await?;
        let port = listener.local_addr()?.port();
        if cfg.settings.port != port {
            cfg.settings.port = port;
            cfg.save(&cfg_path)?;
        }
        let name = if cfg.settings.device_name.is_empty() { opts.default_device_name.clone() } else { cfg.settings.device_name.clone() };

        let (hub_tx, hub_rx) = mpsc::unbounded_channel();
        let direct = crate::direct::Direct::open(&dir, &key, &mut cfg, opts.secrets.clone())?;
        if cfg.upgraded {
            cfg.upgraded = false;
            cfg.save(&cfg_path)?;
        }
        let limit_lan = crate::limit::qa_limit_lan(std::env::var_os(crate::limit::QA_LIMIT_LAN).as_deref(), &dir);
        if limit_lan {
            eprintln!("seedwell sync: QA: the speed limits apply to LAN sessions too ({})", crate::limit::QA_LIMIT_LAN);
        } else if std::env::var_os(crate::limit::QA_LIMIT_LAN).is_some() {
            eprintln!("seedwell sync: {} ignored: only an isolated run (a state folder in a temporary folder) limits the LAN", crate::limit::QA_LIMIT_LAN);
        }
        match crate::shared::qa_slow_saves(std::env::var_os(crate::shared::QA_SLOW_SAVES).as_deref(), &dir) {
            Ok(Some(wait)) => {
                crate::shared::test_slow_saves(wait);
                eprintln!("seedwell sync: QA: every save takes {} ms longer ({})", wait.as_millis(), crate::shared::QA_SLOW_SAVES);
            }
            Ok(None) => {}
            Err(why) => eprintln!("seedwell sync: {} ignored: {why}", crate::shared::QA_SLOW_SAVES),
        }
        let ctx = Arc::new(Ctx {
            me: key.device_id(),
            key,
            client: opts.client.clone(),
            device_name: Mutex::new(name),
            user_name: Mutex::new(clean_user_name(&cfg.settings.user_name)),
            tls,
            registry: Registry::default(),
            hub: hub_tx,
            limits: crate::limit::Limits::default(),
            limit_lan: AtomicBool::new(limit_lan),
            secrets: opts.secrets.clone(),
            protocol: opts.protocol,
            features: opts.features,
            chat_events: tokio::sync::broadcast::channel(1024).0,
            direct,
            hold_wait: Mutex::new(Duration::from_secs(10)),
            scan_step_wait: Mutex::new(Duration::ZERO),
            fetch_wait: Mutex::new(Duration::ZERO),
            stream_wait: Mutex::new(Duration::ZERO),
            serve_wait: Mutex::new(Duration::ZERO),
            scan_hold: Mutex::new(Duration::ZERO),
            silent_ondemand: AtomicBool::new(false),
            folder_gen: std::sync::atomic::AtomicU64::new(0),
            file_hooks: crate::files::Hooks::default(),
            sessions: std::sync::atomic::AtomicU64::new(0),
            ours: Mutex::new(cfg.folders.iter().filter_map(|f| f.id.parse().ok()).collect()),
            setup_wait: Mutex::new(Duration::ZERO),
            setups_waiting: std::sync::atomic::AtomicUsize::new(0),
            yes_to_holds: AtomicBool::new(false),
            pairing: Mutex::new(Vec::new()),
            devices_wake: Notify::new(),
            devices_lock: tokio::sync::Mutex::new(()),
            hold_key_changes: AtomicBool::new(false),
            hold_switch: AtomicBool::new(false),
            hold_read_only_again: AtomicBool::new(false),
            switches_waiting: std::sync::atomic::AtomicUsize::new(0),
            settling: Mutex::new(std::time::Instant::now() + FETCH_GRACE),
            fetch_grace: Mutex::new(FETCH_GRACE),
            fetch_stall: Mutex::new(FETCH_STALL),
        });
        ctx.limits.set_kib(cfg.settings.up_kib, cfg.settings.down_kib);
        if let Some(a) = opts.chat_agents {
            lock(&ctx.direct.settings).agents = a;
        }
        let lan_runs = opts.lan_discovery && cfg.settings.lan_discovery;
        let lan = crate::lan::Lan::new(ctx.clone(), bind, opts.lan_port, opts.lan_targets);
        let inner = Arc::new(Inner {
            ctx,
            dir,
            cfg_path,
            cfg: Mutex::new(cfg),
            cfg_saving: Mutex::new(()),
            cfg_save_queued: AtomicBool::new(false),
            queued_save_wait_ms: AtomicU64::new(0),
            secrets: opts.secrets,
            port,
            bind,
            lan_runs,
            discovery: opts.discovery,
            runners: Mutex::new(HashMap::new()),
            book: Mutex::new(AddressBook::default()),
            tasks: Mutex::new(Vec::new()),
            stopping: AtomicBool::new(false),
            wake_dialer: Notify::new(),
            lan,
            utp,
            dial_tcp: opts.dial_tcp,
            net: crate::relay::Net::default(),
            stop_wait: Mutex::new(STOP_WAIT),
            reopen_wait: Mutex::new(Duration::ZERO),
            dials_left: Mutex::new(opts.dials),
            dials_made: AtomicU64::new(0),
            gone_after_s: AtomicU64::new(crate::absence::GONE_AFTER_S),
            held: Mutex::new(BTreeMap::new()),
            held_retrying: tokio::sync::Mutex::new(()),
            rt: tokio::runtime::Handle::current(),
            forbidden_root: opts.forbidden_root,
            on_demand_host: opts.on_demand_host,
            on_demand: opts.on_demand,
            torrents: opts.torrents,
            netwatch: crate::netchange::NetWatch::default(),
            rendezvous: crate::rendezvous::State::default(),
        });
        let h = SyncHandle { inner };

        // Folders made by the test builds before 0.1.6 cannot be opened (no folder key in the entry, a membership
        // file or a stored link of the earlier format): they are forgotten here, their files stay, and the user
        // adds or joins them again.
        let old: Vec<FolderEntry> = lock(&h.inner.cfg).folders.iter().filter(|e| h.made_by_test_build(e)).cloned().collect();
        if !old.is_empty() {
            let gone: BTreeSet<String> = old.iter().map(|e| e.id.clone()).collect();
            lock(&h.inner.cfg).folders.retain(|e| !gone.contains(&e.id));
            h.save_config()?;
            for e in &old {
                if let Ok(id) = e.id.parse::<FolderId>() {
                    for name in [crate::roles::secret_name(&id), crate::roles::invite_name(&id), crate::roles::links_name(&id)] {
                        let _ = h.inner.secrets.delete(&name);
                    }
                    let _ = std::fs::remove_file(h.index_path(&id));
                    let _ = std::fs::remove_file(h.members_path(&id));
                }
                eprintln!("seedwell sync: folder {} was made by a test build; it is no longer synced (its files stay)", e.path.display());
            }
        }
        // Channels the same way (none can come from those builds, but a stored link or membership file may).
        let old: Vec<crate::config::ChannelEntry> = lock(&h.inner.cfg).channels.iter().filter(|e| h.channel_from_test_build(e)).cloned().collect();
        for e in &old {
            if let Ok(id) = e.id.parse::<FolderId>() {
                let _ = h.chat_leave(&id);
            }
            eprintln!(concat!("seedwell sync: channel {} was made by a test build; it is no longer on this ", trs_platform::computer!()), e.name);
        }
        let (stopped, switched) = h.recover_journals().await;
        *lock(&h.inner.held) = stopped.clone();
        let entries = lock(&h.inner.cfg).folders.clone();
        for e in entries {
            // A move cut off that cannot be finished yet (the original's disk not there): never opened at either end.
            if e.id.parse::<FolderId>().is_ok_and(|id| stopped.contains_key(&id)) {
                continue;
            }
            if let Err(err) = h.open_folder(&e).await {
                // A folder that cannot open (its key is gone, its disk is missing) stays in the configuration;
                // removing it again (sync.remove) clears it.
                eprintln!("seedwell sync: folder {}: {err}", e.id);
            }
        }
        let channels = lock(&h.inner.cfg).channels.clone();
        for e in channels {
            if let Err(err) = h.open_channel(&e).await {
                eprintln!("seedwell sync: channel {}: {err}", e.id);
            }
        }
        // The user's own devices (after the folders and channels: what follows is then marked).
        let group = lock(&h.inner.cfg).my_devices.clone();
        if let Some(e) = group
            && let Err(err) = h.open_devices(&e).await
        {
            eprintln!("seedwell sync: your devices: {err}");
        }
        for id in &switched {
            h.follow_mode_changed(id).await;
        }

        let mut tasks = vec![
            tokio::spawn(accept_loop(h.clone(), listener)),
            tokio::spawn(hub_loop(h.clone(), hub_rx)),
            tokio::spawn(dial_loop(h.clone())),
            tokio::spawn(access_clock(h.clone())),
            tokio::spawn(held_loop(h.clone(), opts.held_retry)),
            tokio::spawn(crate::netchange::run(h.clone())),
            tokio::spawn(crate::rendezvous::run(h.clone())),
            tokio::spawn(crate::devices::run(h.clone())),
        ];
        if h.inner.discovery.is_some() {
            tasks.push(tokio::spawn(dht_loop(h.clone())));
        }
        if h.inner.lan_runs {
            tasks.push(tokio::spawn(crate::lan::run(h.clone())));
        }
        if let Some(u) = h.inner.utp.clone() {
            tasks.push(tokio::spawn(utp_accept_loop(h.clone(), u)));
        }
        if let Some(rx) = foreign_rx {
            tasks.push(tokio::spawn(foreign_loop(h.clone(), rx)));
        }
        *lock(&h.inner.tasks) = tasks;
        // Downloads that were running when the service stopped resume (paused until a device that has them connects).
        h.wake_rooms();
        for url in h.relay_urls() {
            crate::relay_server::ensure_registered(&h, &url);
        }
        h.publish_profiles();
        // The first round of dials now (opening a folder or channel wakes it too; a Mac with only private
        // conversations has none).
        h.inner.wake_dialer.notify_one();
        Ok(h)
    }

    /// Signs this device's current names (its own and its user's) into every folder and channel it is a member of,
    /// where they differ from the ones published there, and sends them round.
    fn publish_profiles(&self) {
        for f in self.inner.ctx.registry.spaces() {
            if crate::roles::publish_profile(&self.inner.ctx, &f) {
                self.spawn_changed(&f);
            }
        }
    }

    /// This device's id.
    pub fn device_id(&self) -> DeviceId {
        self.inner.ctx.me
    }

    /// The port sync listens on.
    pub fn port(&self) -> u16 {
        self.inner.port
    }

    /// Stops everything: folder runtimes, sessions, the listener; returns once the folders' indexes and the channels'
    /// stores are closed (at most 10 s after the rest), so a service can start again on the same state in this
    /// process. The handle shows no folders or channels afterwards.
    pub async fn shutdown(&self) {
        self.inner.stopping.store(true, Ordering::SeqCst);
        // A read mark's save still queued is made now, one under way is waited for, and none comes after.
        self.save_queued_config(true);
        self.halt_rooms();
        let runners: Vec<Runner> = lock(&self.inner.runners).drain().map(|(_, r)| r).collect();
        for r in &runners {
            let _ = r.stop.send(true);
        }
        for r in runners {
            let _ = tokio::time::timeout(Duration::from_secs(5), r.task).await;
        }
        self.inner.ctx.registry.stop_peers();
        for t in lock(&self.inner.tasks).drain(..) {
            t.abort();
        }
        // Let go of every folder and channel and wait (10 s at most) until nothing else holds one: a folder's index
        // is open while its core lives, and a service started again in this process must be able to open it.
        // A folder counts as closed when its `closed` flag is up, not when its last `Arc` is gone: the count drops
        // to zero before the fields (the index) are dropped, on whichever thread let go last.
        let reg = &self.inner.ctx.registry;
        let folders: Vec<Arc<AtomicBool>> = reg.folders().iter().map(|f| f.closed.watch()).collect();
        let channels: Vec<std::sync::Weak<crate::chat::ChannelShared>> = reg.channels().iter().map(Arc::downgrade).collect();
        for f in reg.folders() {
            reg.remove_folder(&f.id);
        }
        for c in reg.channels() {
            reg.remove_channel(&c.id);
        }
        let end = Instant::now() + Duration::from_secs(10);
        while folders.iter().any(|f| !f.load(Ordering::SeqCst)) || channels.iter().any(|c| c.strong_count() > 0) {
            if Instant::now() >= end {
                eprintln!("seedwell sync: a folder or channel was still in use 10 s after the service stopped");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub(crate) fn save_config(&self) -> io::Result<()> {
        // The configuration is locked only while its text is made, never for the write: the app's calls read it, and
        // a disk may take long to save (a virus scanner reads each new file first). The saves take turns, and each
        // writes the configuration as it is at its turn, so the file never goes back to an older one.
        let turn = lock(&self.inner.cfg_saving);
        self.write_config(&turn)
    }

    /// Writes the configuration as it is now; `_turn` is the save's turn (`cfg_saving`), held by the caller.
    fn write_config(&self, _turn: &std::sync::MutexGuard<'_, ()>) -> io::Result<()> {
        let text = lock(&self.inner.cfg).text()?;
        SyncConfig::write_text(&self.inner.cfg_path, &text)
    }

    /// Saves the configuration on the service's threads, for a change made by a call the app makes on its UI thread
    /// (a read mark): that call never waits for the disk. Changes made while a save is queued go with it; the service
    /// saves one still queued when it stops ([`Self::shutdown`]).
    pub(crate) fn save_config_soon(&self) {
        if self.inner.cfg_save_queued.swap(true, Ordering::SeqCst) {
            return;
        }
        let h = self.clone();
        drop(self.inner.rt.spawn_blocking(move || h.save_queued_config(false)));
    }

    /// The save [`Self::save_config_soon`] queued, if it has not been made; `at_stop`: the service stops
    /// ([`Self::shutdown`]), else the queued task. The turn is taken first, so a stop waits for a save that took the
    /// queue, and once the service stopped the task writes nothing (the stop saved it): no save of this service lands
    /// after its shutdown returned (over a new service's on the same state).
    fn save_queued_config(&self, at_stop: bool) {
        let turn = lock(&self.inner.cfg_saving);
        if !at_stop && self.inner.stopping.load(Ordering::SeqCst) {
            return;
        }
        // Down under the turn: a change made after this goes with the next save, one made before it with this one.
        if !self.inner.cfg_save_queued.swap(false, Ordering::SeqCst) {
            return;
        }
        let wait = self.inner.queued_save_wait_ms.load(Ordering::SeqCst);
        if wait > 0 && !at_stop {
            std::thread::sleep(Duration::from_millis(wait));
        }
        if let Err(e) = self.write_config(&turn) {
            eprintln!("seedwell sync: the configuration could not be saved: {e}");
        }
    }

    /// **Tests only:** a queued save of the configuration ([`Self::save_config_soon`]), once taken, waits this long
    /// before it writes (as a slow disk holds it).
    #[doc(hidden)]
    pub fn test_queued_save_wait(&self, wait: Duration) {
        self.inner.queued_save_wait_ms.store(wait.as_millis() as u64, Ordering::SeqCst);
    }

    /// **Tests only:** a save of the configuration is queued and not taken yet.
    #[doc(hidden)]
    pub fn test_save_queued(&self) -> bool {
        self.inner.cfg_save_queued.load(Ordering::SeqCst)
    }

    fn index_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.redb"))
    }

    /// Where an on-demand folder keeps the content held on this device.
    pub(crate) fn cache_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.cache"))
    }

    /// Where a folder kept in place notes what is in its placeholders ([`crate::placed`]).
    pub(crate) fn placed_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.placed"))
    }

    /// Where an on-demand folder keeps its host's last change-feed anchor.
    pub(crate) fn anchor_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.anchor"))
    }

    pub(crate) fn members_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.members"))
    }

    /// A folder made by a test build before 0.1.6: no folder key in its entry, a membership file of the earlier
    /// format, or a stored link (the one it joins with, or one it made) this version cannot read.
    fn made_by_test_build(&self, e: &FolderEntry) -> bool {
        let Ok(id) = e.id.parse::<FolderId>() else { return e.folder_pk.is_empty() };
        let unreadable = |name: String| {
            self.inner.secrets.get(&name).ok().flatten().is_some_and(|b| {
                String::from_utf8_lossy(&b).lines().filter(|l| !l.trim().is_empty()).any(|l| InviteLink::parse(l).is_err())
            })
        };
        e.folder_pk.is_empty()
            || crate::roles::from_test_build(&self.members_path(&id))
            || unreadable(crate::roles::invite_name(&id))
            || unreadable(crate::roles::links_name(&id))
    }

    /// A channel of a test build (as for folders: a membership file of the earlier format, a stored link this
    /// version cannot read).
    fn channel_from_test_build(&self, e: &crate::config::ChannelEntry) -> bool {
        let Ok(id) = e.id.parse::<FolderId>() else { return true };
        let unreadable = |name: String| {
            self.inner.secrets.get(&name).ok().flatten().is_some_and(|b| {
                String::from_utf8_lossy(&b).lines().filter(|l| !l.trim().is_empty()).any(|l| InviteLink::parse_chat(l).is_err())
            })
        };
        crate::roles::from_test_build(&self.members_path(&id)) || unreadable(crate::roles::invite_name(&id)) || unreadable(crate::roles::links_name(&id))
    }

    /// Opens a configured folder and starts its runtime.
    async fn open_folder(&self, e: &FolderEntry) -> Result<(), SyncError> {
        let id: FolderId = e.id.parse().map_err(|_| invalid("bad folder id"))?;
        let pk = public_key_from_hex(&e.folder_pk)
            .filter(|pk| FolderId::of_public_key(pk) == id)
            .ok_or_else(|| invalid("this folder was made by a test build: remove it and add it again"))?;
        let secret = match self.inner.secrets.get(&crate::roles::secret_name(&id))? {
            None => None,
            Some(b) => {
                let s: [u8; 32] = b.try_into().map_err(|_| invalid("this folder was made by a test build: remove it and add it again"))?;
                let s = FolderSecret::from_bytes(s);
                if *s.public_key() != pk {
                    return Err(invalid("the folder's owner key does not fit the folder"));
                }
                Some(s)
            }
        };
        let invite = match self.inner.secrets.get(&crate::roles::invite_name(&id))? {
            Some(b) => String::from_utf8(b).ok().and_then(|t| InviteLink::parse(&t).ok()),
            None => None,
        };
        let links: BTreeMap<[u8; 16], String> = match self.inner.secrets.get(&crate::roles::links_name(&id))? {
            Some(b) => String::from_utf8_lossy(&b)
                .lines()
                .filter_map(|l| InviteLink::parse(l).ok().map(|k| (k.invite.id, l.to_string())))
                .collect(),
            None => BTreeMap::new(),
        };
        let members_path = self.members_path(&id);
        let membership = crate::roles::load(&members_path, pk)?;
        let role = membership.role(self.inner.ctx.key.public_key());
        // Joined with write access and not admitted yet: its changes meanwhile become its own once it is.
        let awaiting = role.is_none() && invite.as_ref().is_some_and(|i| i.invite.role.access() == Access::Write);
        let writers = membership.writers();
        let members = crate::roles::member_ids(&membership);
        // An on-demand folder's files are this device's content cache, which is ours: its marker is made here.
        let root = if e.on_demand { self.cache_path(&id) } else { e.path.clone() };
        if e.on_demand {
            std::fs::create_dir_all(root.join(STATE_DIR))?;
        }
        // Kept in place: the host's placeholders hold what is here, and the core reads them through the notes.
        let placed = if e.on_demand && e.in_place {
            Some(Arc::new(crate::placed::Placed::open(e.path.clone(), root.clone(), self.placed_path(&id))?))
        } else {
            None
        };
        let core_placed = placed.clone();
        let index = self.index_path(&id);
        let ctx = self.inner.ctx.clone();
        let entry = e.clone();
        let core = tokio::task::spawn_blocking(move || -> Result<(FolderCore, RedbReader, trs_sync::ignore::Current, trs_sync::ondemand::Replacing), SyncError> {
            let fs = StdFs::new(root.clone());
            let (case_insensitive, exec_bits) = match fs.lstat(STATE_DIR) {
                Ok(Some(_)) => (
                    trs_sync::path::probe_case_insensitive(&fs).unwrap_or(true),
                    trs_sync::path::probe_exec_bits(&fs).unwrap_or(cfg!(unix)),
                ),
                _ => (cfg!(target_os = "macos"), cfg!(unix)),
            };
            let store = RedbStore::open(&index, random_u64())?;
            let reader = store.reader();
            let access = role.map_or(Access::Read, Role::access);
            let cfg = FolderConfig {
                me: ctx.me,
                folder_pk: pk,
                access,
                case_insensitive,
                byte_exact_names: !cfg!(target_os = "macos"),
                exec_bits,
                archive: entry.archive,
                on_demand: entry.on_demand,
            };
            let ignore = load_ignore(&fs, &entry.ignore);
            let clock: Arc<dyn trs_sync::clock::Clock> = Arc::new(SystemClock);
            let core_fs: Arc<dyn Fs> = match core_placed {
                Some(p) => Arc::new(crate::placed::PlaceFs::new(p)),
                None => Arc::new(fs),
            };
            let mut core = FolderCore::new(cfg, Box::new(store), core_fs, Some(ctx.key.clone()), clock, Arc::new(random_id), ignore)
                .map_err(|e| invalid(e.to_string()))?;
            // A `.seedwellignore` that arrives or changes later applies at once (SPEC §7G.2).
            core.follow_ignore_file(&entry.ignore)?;
            let ignore = core.ignore();
            let replacing = core.replacing();
            core.set_membership(&writers, access).map_err(|e| invalid(e.to_string()))?;
            core.set_awaiting_write(awaiting);
            // Who is no member any more is no source or holder (as `roles::changed` keeps it).
            if role.is_some() {
                core.set_members(&members)?;
            }
            core.set_pins(entry.pins.iter().cloned());
            core.set_list_first(entry.on_demand_others);
            // Peers' chunks come with the core let go (the folder's streamer runs the fetches).
            core.set_fetch_apart(true);
            if entry.mode_switched {
                // Switched: to full, what is still online only is fetched now; to on demand, what is here is
                // announced again as an on-demand device's.
                core.set_on_demand(entry.on_demand)?;
            }
            Ok((core, reader, ignore, replacing))
        })
        .await
        .map_err(io::Error::other)??;
        let (core, reader, ignore, replacing) = core;
        if e.mode_switched {
            if let Some(c) = lock(&self.inner.cfg).folder_mut(&e.id) {
                c.mode_switched = false;
            }
            self.save_config()?;
        }

        let (seq_tx, _) = watch::channel(core.index_state().map(|(_, s)| s).unwrap_or(0));
        let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let space = Space::new(id, SpaceKind::Folder, FolderKey::Member(pk), secret, membership, members_path, invite, links, name);
        space.paused.store(e.paused, Ordering::Relaxed);
        space.lan_only.store(e.lan_only, Ordering::Relaxed);
        crate::devices::mark_following(&self.inner.ctx, &space);
        let mut items = trs_sync::ondemand::Items::default();
        items.set_not_members(core.not_members());
        let jobs = core.streams();
        let shared = Arc::new(FolderShared {
            space: Arc::new(space),
            root: if e.on_demand { self.cache_path(&id) } else { e.path.clone() },
            place: Mutex::new(e.path.clone()),
            on_demand: e.on_demand,
            list_first: AtomicBool::new(e.on_demand_others),
            pins: Mutex::new(e.pins.iter().cloned().collect()),
            items: Mutex::new(items),
            freeing: Mutex::new(BTreeMap::new()),
            ignore,
            replacing,
            placed,
            core: Mutex::new(core),
            index: reader,
            local_seq: seq_tx,
            remote_changed: Notify::new(),
            live: Mutex::new(Live::default()),
            rescan: Notify::new(),
            stopped: watch::channel(false).0,
            retention: Retention { max_age_s: i64::from(e.archive_days) * 86_400, ..Retention::default() },
            fetched: crate::limit::Meter::default(),
            served: crate::limit::Meter::default(),
            fetches: Mutex::new(Default::default()),
            announce_pause: Mutex::new(Duration::ZERO),
            write_wait: Mutex::new(Duration::ZERO),
            streams: Mutex::new(HashMap::new()),
            streamer: Mutex::new(Default::default()),
            fetcher: Mutex::new(Default::default()),
            jobs,
            closed: crate::shared::Closed::default(),
        });
        // Devices met in this folder before are members to dial, at the addresses they had.
        {
            let mut members = shared.members.write().unwrap_or_else(|e| e.into_inner());
            let mut book = lock(&self.inner.book);
            for d in &e.devices {
                let Ok(dev) = d.id.parse::<DeviceId>() else { continue };
                members.insert(dev);
                for a in &d.addresses {
                    if let Ok(sa) = a.parse::<SocketAddr>() {
                        book.by_device.entry(dev).or_default().insert(sa);
                    }
                }
            }
            for a in &e.addresses {
                if let Ok(sa) = a.parse::<SocketAddr>() {
                    book.hints.entry(id).or_default().insert(sa);
                }
            }
        }
        self.learn_from_link(&shared);
        self.inner.ctx.registry.add_folder(shared.clone());
        self.folders_moved();
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(crate::runtime::run(self.inner.ctx.clone(), shared, rx));
        lock(&self.inner.runners).insert(id, Runner { stop, task });
        self.inner.wake_dialer.notify_one();
        // Announced on the LAN now, not at the next round.
        self.inner.lan.wake.notify_one();
        // Peers already connected learn about the new folder.
        self.offer_folder(id).await;
        Ok(())
    }

    /// Offers a newly opened folder to every connected device: each is already a member of some folder
    /// with us, and a device that does not hold this folder's key learns only its id. Those that hold it
    /// answer with their own Join, and the folder starts syncing over the existing session.
    async fn offer_folder(&self, folder: FolderId) {
        // Devices not connected now (one that dialled while it was closed found nothing to share) are dialled.
        self.inner.wake_dialer.notify_one();
        for p in self.inner.ctx.registry.peers() {
            // Stale on that session (it closed here while linked): the session starts anew, and the new one's Join
            // says what we hold, so the peer sends again everything since (nothing of it was taken meanwhile).
            if p.is_stale(&folder) && !self.plays_0_1_11() {
                let reason = "a folder was opened again here: starting the session anew".to_string();
                let _ = p.control.send(Message::Close(trs_sync_proto::msg::Close { reason })).await;
                self.inner.wake_dialer.notify_one();
                continue;
            }
            // Linked, or in our Join on this session already (offered, its answer pending): not again.
            if p.access(&folder).is_some() || (!self.plays_0_1_11() && lock(&p.joined).contains(&folder)) {
                continue;
            }
            let join = crate::peer::own_join(&self.inner.ctx, &[folder], &p.exporter, &p.device_pk, p.lan).await;
            if !join.folders.is_empty() {
                lock(&p.joined).insert(folder);
                lock(&p.offered).insert(folder);
                let _ = p.control.send(Message::Join(join)).await;
            }
        }
    }

    /// **Tests:** [`Ctx::plays_0_1_11`](crate::shared::Ctx::plays_0_1_11).
    fn plays_0_1_11(&self) -> bool {
        self.inner.ctx.plays_0_1_11()
    }

    /// A folder opened or closed here: sessions being set up check their folders again, and devices that turned our
    /// sessions down are dialed again at once (what we offer them changed).
    fn folders_moved(&self) {
        self.inner.ctx.folder_gen.fetch_add(1, Ordering::SeqCst);
        self.forget_refusals();
    }

    /// The addresses whose sessions were turned down in their setup lose their backoff, and the dial loop runs.
    pub(crate) fn forget_refusals(&self) {
        {
            let mut book = lock(&self.inner.book);
            for a in std::mem::take(&mut book.refused) {
                book.tried.remove(&a);
            }
        }
        self.inner.wake_dialer.notify_one();
    }

    /// The folder closes here (paused, stopped): every session that has it linked marks it stale at this moment
    /// ([`crate::shared::PeerHandle::close_folder`]).
    fn close_on_sessions(&self, folder: &FolderId) {
        for p in self.inner.ctx.registry.peers() {
            // (As 0.1.11: nothing marked; what peers send meanwhile is dropped, and not asked for again.)
            if self.plays_0_1_11() {
                continue;
            }
            p.close_folder(folder);
        }
    }

    /// Starts sharing a folder on this Mac: makes its secret, and this device its owner.
    pub async fn add_folder(&self, path: PathBuf) -> Result<FolderStatus, SyncError> {
        self.add_folder_with(path, false).await
    }

    /// [`Self::add_folder`], on demand for the devices without its files (`on_demand_others`, as
    /// [`Self::set_files_on_demand`] marks it) from the start: its files stay where they are, and its first scan lists
    /// them and reads none (SPEC §7G.11, *Listed first, hashed when first fetched*), so the other devices list it within
    /// seconds whatever its size. What the app's Add dialog does with *Files on demand* on.
    pub async fn add_folder_with(&self, path: PathBuf, on_demand_others: bool) -> Result<FolderStatus, SyncError> {
        let path = absolute(&path)?;
        let meta = std::fs::metadata(&path).map_err(|_| invalid(format!("{} does not exist", path.display())))?;
        if !meta.is_dir() {
            return Err(invalid(format!("{} is not a folder", path.display())));
        }
        refuse_cloud_root(&path)?;
        self.refuse_forbidden(&path)?;
        self.refuse_overlap(&path)?;
        let secret = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
        let id = FolderId::of_public_key(secret.public_key());
        let me = *self.inner.ctx.key.public_key();
        let grant = Statement::by_owner(&secret, Body::Grant { device: me, role: Role::Owner, serial: 1, at_s: crate::roles::now_s(), invite: None });
        let mut m = Membership::new(*secret.public_key());
        m.add([grant]);
        crate::roles::save(&self.members_path(&id), &m)?;
        self.inner.secrets.put(&crate::roles::secret_name(&id), secret.as_bytes())?;
        self.install_with(*secret.public_key(), path, None, false, on_demand_others).await
    }

    /// Adds a folder of this device with its local copies freed at once (SPEC §7G.11; the user's explicit choice: the
    /// app's *Files on demand* adds a full folder that offers it, [`Self::set_files_on_demand`]). Refused while a
    /// torrent uses files in it. Its files go into this device's cache before the folder
    /// ever opens, so no scan holds it busy, and the add ends on demand or adds nothing. The configuration and the
    /// journal of the move come first: a move cut off is finished (or undone) at the next start, as a switch's is; a
    /// move that cannot be finished now keeps the folder, held back, and says so; any other failure forgets it again,
    /// its files where they were.
    pub async fn add_folder_on_demand(&self, path: PathBuf) -> Result<FolderStatus, SyncError> {
        let path = absolute(&path)?;
        let meta = std::fs::metadata(&path).map_err(|_| invalid(format!("{} does not exist", path.display())))?;
        if !meta.is_dir() {
            return Err(invalid(format!("{} is not a folder", path.display())));
        }
        refuse_cloud_root(&path)?;
        self.refuse_forbidden(&path)?;
        self.refuse_overlap(&path)?;
        self.refuse_in_use(&path).await?;
        let secret = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
        let id = FolderId::of_public_key(secret.public_key());
        let me = *self.inner.ctx.key.public_key();
        let grant = Statement::by_owner(&secret, Body::Grant { device: me, role: Role::Owner, serial: 1, at_s: crate::roles::now_s(), invite: None });
        let mut m = Membership::new(*secret.public_key());
        m.add([grant]);
        crate::roles::save(&self.members_path(&id), &m)?;
        self.inner.secrets.put(&crate::roles::secret_name(&id), secret.as_bytes())?;
        let mut entry = FolderEntry::new(id.to_string(), path.clone());
        entry.folder_pk = hex(secret.public_key());
        {
            let mut cfg = lock(&self.inner.cfg);
            cfg.folders.retain(|f| f.id != entry.id);
            cfg.folders.push(entry.clone());
        }
        lock(&self.inner.ctx.ours).insert(id);
        let undo = |me: &Self, why: String| {
            let _ = me.forget_folder(&id);
            prune_state(&path);
            invalid(why)
        };
        if let Err(e) = self.save_config().and_then(|()| std::fs::create_dir_all(path.join(STATE_DIR))) {
            return Err(undo(self, format!("the folder could not be added ({e}); nothing changed")));
        }
        let cache = self.cache_path(&id);
        let j = Journal::Switch { on: true, from: path.clone(), to: cache.clone(), place: path.clone(), copied: false, renamed: false, made_to: !cache.exists(), done: false };
        if let Err(e) = write_journal_at(&self.journal_path(&id), &j) {
            return Err(undo(self, format!("the move could not be noted ({e}); nothing changed")));
        }
        match self.noted_move(&id, j).await {
            Err(e) if unfinished(&e) => return Err(self.hold_unfinished(&id, &e)),
            Err(e) => {
                self.drop_journal(&id);
                return Err(undo(self, format!("its files could not be moved into this device's storage ({e}); nothing was added, they are in {}", path.display())));
            }
            // From another volume, an original that could not be deleted all the way is left set aside: said where,
            // as a switch says it.
            Ok(Some(left)) => eprintln!("seedwell sync: the folder's files are in {}; a copy left behind could not be deleted: {}", cache.display(), left.display()),
            Ok(None) => {}
        }
        entry.on_demand = true;
        entry.mode_switched = true;
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            *e = entry.clone();
        }
        // Saved, the journal goes; not saved, it stays and the next start finishes the switch.
        if self.save_config().is_ok() {
            self.drop_journal(&id);
        }
        self.open_folder(&entry).await?;
        self.folder_status(&id).ok_or(SyncError::NotFound)
    }

    /// Joins a shared folder with an invitation link, at `path` (created when missing). The folder waits until
    /// one of its owners or admins is online and admits this device.
    ///
    /// A link for a folder and its channel joins both (the channel only when it is not here yet) and links them on
    /// this device. Both invitations are checked first: an expired one refuses the whole join; one that is used up
    /// or revoked is refused by whoever admits, for each.
    pub async fn join_folder(&self, key_or_link: &str, path: PathBuf) -> Result<FolderStatus, SyncError> {
        self.join_with(key_or_link, path, false).await
    }

    async fn join_with(&self, key_or_link: &str, path: PathBuf, on_demand: bool) -> Result<FolderStatus, SyncError> {
        let (link, chat) = InviteLink::parse_bundle(key_or_link).map_err(|e| invalid(e.to_string()))?;
        let now = crate::roles::now_s();
        if link.invite.expired(now) || chat.as_ref().is_some_and(|c| c.invite.expired(now)) {
            return Err(invalid("this link has expired"));
        }
        let path = absolute(&path)?;
        // An on-demand folder's place is where the host shows it, inside the system's cloud folders on a Mac.
        if !on_demand {
            refuse_cloud_root(&path)?;
        }
        self.refuse_forbidden(&path)?;
        self.refuse_overlap(&path)?;
        let id = link.invite.folder_id();
        // Running or not (held back by an unfinished move, failed to open): one folder is never joined twice.
        if self.inner.ctx.registry.folder(&id).is_some() || lock(&self.inner.cfg).folder(&id.to_string()).is_some() {
            return Err(invalid(concat!("this folder is already synced on this ", trs_platform::computer!())));
        }
        // Files of an earlier time on demand are never read in as this device's: the user decides what they are.
        let cache = self.cache_path(&id);
        if on_demand && cache.exists() {
            return Err(invalid(format!("files this folder had on this {} earlier are still in {}: move them away first", trs_platform::computer!(), cache.display())));
        }
        let channel = chat.as_ref().map(|c| c.invite.folder_id());
        if let Some(c) = chat
            && self.inner.ctx.registry.channel(&c.invite.folder_id()).is_none()
        {
            self.chat_join_link(c).await?;
        }
        if !on_demand {
            std::fs::create_dir_all(&path)?;
        }
        self.inner.secrets.put(&crate::roles::invite_name(&id), link.format().as_bytes())?;
        self.install(link.invite.folder_pk, path, channel, on_demand).await
    }

    /// Joins a folder that one of the user's own devices put on all of them ([`crate::devices`]), at `path` (made
    /// here, never one that is there; on demand, it is the folder's place, where the host shows it, and is not made):
    /// no invitation, it proves it is one of that device's own.
    pub(crate) async fn join_following_at(&self, folder_pk: [u8; 32], path: PathBuf, on_demand: bool) -> Result<(), SyncError> {
        let path = absolute(&path)?;
        if !on_demand {
            refuse_cloud_root(&path)?;
        }
        self.refuse_forbidden(&path)?;
        self.refuse_overlap(&path)?;
        let id = FolderId::of_public_key(&folder_pk);
        if self.inner.ctx.registry.folder(&id).is_some() || lock(&self.inner.cfg).folder(&id.to_string()).is_some() {
            return Ok(());
        }
        if on_demand {
            // Files of an earlier time on demand are never read in as this device's: the user decides what they are.
            let cache = self.cache_path(&id);
            if cache.exists() {
                return Err(invalid(format!("files this folder had on this {} earlier are still in {}: move them away first", trs_platform::computer!(), cache.display())));
            }
        } else {
            std::fs::create_dir(&path)?;
        }
        self.install(folder_pk, path, None, on_demand).await.map(|_| ())
    }

    /// Folder `id` was joined in full although it is on demand on the device that put it on all the user's devices
    /// ([`FolderEntry::full_here`]): its page says why.
    pub(crate) fn note_full_here(&self, id: &FolderId) {
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.full_here = true;
        }
        let _ = self.save_config();
    }

    async fn install(&self, folder_pk: [u8; 32], path: PathBuf, channel: Option<FolderId>, on_demand: bool) -> Result<FolderStatus, SyncError> {
        self.install_with(folder_pk, path, channel, on_demand, false).await
    }

    async fn install_with(&self, folder_pk: [u8; 32], path: PathBuf, channel: Option<FolderId>, on_demand: bool, others: bool) -> Result<FolderStatus, SyncError> {
        let id = FolderId::of_public_key(&folder_pk);
        if on_demand {
            std::fs::create_dir_all(self.cache_path(&id).join(STATE_DIR))?;
        } else {
            std::fs::create_dir_all(path.join(STATE_DIR))?;
        }
        let mut entry = FolderEntry::new(id.to_string(), path);
        entry.folder_pk = hex(&folder_pk);
        entry.on_demand = on_demand;
        entry.on_demand_others = others && !on_demand;
        entry.channel = channel.map(|c| c.to_string()).unwrap_or_default();
        {
            let mut cfg = lock(&self.inner.cfg);
            cfg.folders.retain(|f| f.id != entry.id);
            cfg.folders.push(entry.clone());
        }
        lock(&self.inner.ctx.ours).insert(id);
        self.save_config()?;
        self.open_folder(&entry).await?;
        self.folder_status(&id).ok_or(SyncError::NotFound)
    }

    /// Asks the file manager's host, if there is one, to let go of on-demand folder `id` ([`OnDemandHost::release`]):
    /// its refusal is the caller's. The returned guard tells the host when the caller is done.
    pub(crate) async fn release_host(&self, id: &FolderId, entry: &FolderEntry) -> Result<Option<Settle>, SyncError> {
        let Some(host) = lock(&self.inner.on_demand_host).clone() else { return Ok(None) };
        let (folder, place) = (id.to_string(), entry.path.clone());
        let name = place.file_name().map_or_else(|| folder.clone(), |n| n.to_string_lossy().into_owned());
        let h = host.clone();
        let f = folder.clone();
        tokio::task::spawn_blocking(move || h.release(&f, &name, &place))
            .await
            .map_err(|e| invalid(e.to_string()))?
            .map_err(|why| invalid(format!("its folder in the file manager could not be let go of first: {why}; nothing changed")))?;
        Ok(Some(Settle { host, folder }))
    }

    /// Whether folder `id` is in this device's configuration, and on demand: `None` when it is not (removed, or
    /// never here). From the configuration only, so a folder that could not open, or is stopped for a move, counts.
    pub fn configured(&self, id: &FolderId) -> Option<bool> {
        lock(&self.inner.cfg).folder(&id.to_string()).map(|e| e.on_demand)
    }

    /// Whether folder `id` is on demand for the devices that join it: on demand here, or a full folder here that
    /// offers it ([`FolderEntry::on_demand_others`]).
    pub fn offers_on_demand(&self, id: &FolderId) -> bool {
        lock(&self.inner.cfg).folder(&id.to_string()).is_some_and(|e| e.on_demand || e.on_demand_others)
    }

    /// *Files on demand* as the user turns it on and off (SPEC §7G.11, *Modes*). On a full folder, whose files are on
    /// this device, it changes nothing on disk: nothing is read, moved, renamed or changed; the folder stays full at
    /// its path, and the devices that do not hold the files (the user's other devices that join it) get placeholders.
    /// Off on such a folder only takes that back. Off on a folder that is on demand here (joined on demand, or its
    /// local copies freed) switches it to full ([`Self::set_on_demand`]: the files the cache holds move to `place`, or
    /// the folder's place, the rest is fetched). Freeing this device's copies is never a side effect of this: that is
    /// [`Self::free_local_copies`]. While the folder is on all the user's devices, the statement says it anew whenever
    /// it differs from the switch (another device of the user may have changed it since).
    pub async fn set_files_on_demand(&self, id: &FolderId, on: bool, place: Option<PathBuf>) -> Result<FolderStatus, SyncError> {
        self.refuse_held(id)?;
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if entry.on_demand && !on {
            // Full again, and so for the devices that join it (the statement says it there).
            return self.set_on_demand(id, false, place).await;
        }
        if !entry.on_demand && entry.on_demand_others != on {
            if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
                e.on_demand_others = on;
            }
            if let Err(e) = self.save_config() {
                if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
                    e.on_demand_others = entry.on_demand_others;
                }
                return Err(SyncError::Io(e));
            }
            // Its scans list and do not read from the next step on (SPEC §7G.11, *Listed first*).
            if let Some(f) = self.inner.ctx.registry.folder(id) {
                f.list_first.store(on, Ordering::SeqCst);
            }
        }
        self.follow_mode_changed(id).await;
        self.folder_status(id).ok_or(SyncError::NotFound)
    }

    /// How many files of an on-demand folder are online only here (their content on other devices): what a switch to
    /// full fetches; the rest is moved from the cache. 0 for a full folder. Walks the index's read-only handle (never
    /// waits for the folder's core): ask once, before a switch.
    pub fn online_only(&self, id: &FolderId) -> Result<u64, SyncError> {
        let Some(f) = self.inner.ctx.registry.folder(id).filter(|f| f.on_demand) else { return Ok(0) };
        let view = f.index.view();
        let store: &dyn trs_sync::index::IndexStore = &view;
        let (mut n, mut after) = (0u64, None::<String>);
        loop {
            let page = store.local_paths("", after.as_deref(), 1000)?;
            let Some(last) = page.last().cloned() else { return Ok(n) };
            for p in &page {
                if let Some(r) = store.local(p)?
                    && r.dataless
                    && !r.file.info.deleted
                    && r.file.info.kind == trs_sync_proto::model::FileKind::File
                {
                    n += 1;
                }
            }
            after = Some(last);
        }
    }

    /// Frees this device's local copies of a full folder: its files move into this device's cache (a rename on one
    /// volume) and it is on demand here from then on ([`Self::set_on_demand`]). The user's explicit choice only, never
    /// a side effect of *Files on demand*. Refused while a torrent of this device uses files in the folder (they would
    /// go from under it), naming them.
    pub async fn free_local_copies(&self, id: &FolderId, place: Option<PathBuf>) -> Result<FolderStatus, SyncError> {
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if !entry.on_demand {
            self.refuse_in_use(&entry.path).await?;
        }
        self.set_on_demand(id, true, place).await
    }

    /// Refuses a move of the files at `path` while a torrent of this device uses files there ([`LocalTorrents`]).
    pub(crate) async fn refuse_in_use(&self, path: &Path) -> Result<(), SyncError> {
        let Some(t) = self.inner.torrents.clone() else { return Ok(()) };
        let names = t.using(path).await.map_err(|why| invalid(format!("the torrents using files in {} could not be told ({why}); nothing changed", path.display())))?;
        if names.is_empty() {
            return Ok(());
        }
        Err(invalid(in_use_text(&names)))
    }

    /// The file manager's host of on-demand folders, or none ([`Options::on_demand_host`]).
    pub fn set_on_demand_host(&self, host: Option<Arc<dyn OnDemandHost>>) {
        *lock(&self.inner.on_demand_host) = host;
    }

    /// Stops syncing a folder on this device. The files stay; its index and `.seedwell` go. (Moving the
    /// files to the Trash is the app's job, which knows the user's Trash.) Returns where the files are.
    ///
    /// A folder on all the user's devices that this device owns goes from Seedwell on all of them, their files kept
    /// ([`Self::removes_everywhere`]); any other followed folder goes from this device only, and does not come back.
    pub async fn remove_folder(&self, id: &FolderId) -> Result<PathBuf, SyncError> {
        self.remove(id, false, true).await
    }

    /// [`Self::remove_folder`] (or [`Self::remove_folder_for_trash`], `for_trash`) on this device only, also for a
    /// folder on all the user's devices that this device owns: the agent interface's removal (agents never change what
    /// is on the user's other devices). A followed folder does not come back here.
    pub async fn remove_folder_here(&self, id: &FolderId, for_trash: bool) -> Result<PathBuf, SyncError> {
        self.remove(id, for_trash, false).await
    }

    /// How many files of a folder no other device is known to hold (they go from the others' reach if this device stops
    /// syncing it; SPEC §7G.11). Reads the folder's whole index (through its read-only handle): ask when a removal is
    /// about to be confirmed, not every second.
    pub fn only_here(&self, id: &FolderId) -> Result<u64, SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        let view = f.index.view();
        // Devices with entries here that are no members any more hold nothing for the others.
        let members = crate::roles::member_ids(&f.membership.read().unwrap_or_else(|e| e.into_inner()));
        let gone = trs_sync::index::IndexStore::peer_ids(&view)?.into_iter().filter(|d| !members.contains(d)).collect();
        Ok(trs_sync::ondemand::files_only_here(&view, &gone)?)
    }

    /// Where a folder's files are on this device: an on-demand folder's in its cache, a full folder's at its place
    /// (running or not).
    pub fn folder_files(&self, id: &FolderId) -> Option<PathBuf> {
        let e = lock(&self.inner.cfg).folder(&id.to_string()).cloned()?;
        Some(if e.on_demand { self.cache_path(id) } else { e.path })
    }

    /// Stops syncing a folder whose files go to the Trash next, from where they are: an on-demand folder's cache is not
    /// moved to the folder's place first (another volume, maybe, with a Trash of its own) but renamed under the
    /// folder's name beside it (`<id>.removed/<name>`, the same volume, so a Trash check of the cache holds for it).
    /// Returns the files to move to the Trash; then [`Self::finish_removal`] says how that went.
    pub async fn remove_folder_for_trash(&self, id: &FolderId) -> Result<PathBuf, SyncError> {
        self.remove(id, true, true).await
    }

    /// After [`Self::remove_folder_for_trash`]: `files` went to the Trash, or not (it refused): then an on-demand
    /// folder's files go to its `place` (or next to it) as an ordinary folder, never deleted. Returns where they are.
    pub async fn finish_removal(&self, files: &Path, place: &Path, trashed: bool) -> Result<PathBuf, SyncError> {
        let holder = files.parent().filter(|h| h.parent() == Some(self.inner.dir.as_path()) && h.extension().is_some_and(|x| x == "removed"));
        let Some(holder) = holder.map(Path::to_path_buf) else { return Ok(files.to_path_buf()) };
        let id = holder.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<FolderId>().ok());
        if trashed {
            let _ = std::fs::remove_dir(&holder);
            id.inspect(|id| self.drop_journal(id));
            return Ok(place.to_path_buf());
        }
        let to = free_place(place);
        let moved = match id {
            Some(id) => self.journaled_move(&id, Journal::Remove { from: files.to_path_buf(), to: to.clone(), place: place.to_path_buf(), copied: false, renamed: false, made_to: !to.exists(), done: false }).await?,
            None => move_tree(files, &to),
        };
        moved.map_err(|e| invalid(format!("the folder's files could not be moved to {} ({e}); they are in {}", to.display(), files.display())))?;
        let _ = std::fs::remove_dir(&holder);
        id.inspect(|id| self.drop_journal(id));
        Ok(to)
    }

    async fn remove(&self, id: &FolderId, for_trash: bool, everywhere: bool) -> Result<PathBuf, SyncError> {
        // On all the user's devices and owned here: removed, it goes from all of them (their files kept). Otherwise,
        // taken off this one, it does not come back by itself.
        let everywhere = if everywhere { self.everywhere_follow(id) } else { None };
        if everywhere.is_none() {
            self.skip_if_followed(id);
        }
        let removed = self.remove_here(id, for_trash).await;
        if removed.is_ok()
            && let Some((pk, name, serial)) = everywhere
        {
            self.removed_everywhere(pk, name, serial).await;
        }
        removed
    }

    async fn remove_here(&self, id: &FolderId, for_trash: bool) -> Result<PathBuf, SyncError> {
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned();
        // The file manager's host lets go of an on-demand folder first (told when it is over).
        let _settle = match entry.as_ref().filter(|e| e.on_demand) {
            Some(e) => self.release_host(id, e).await?,
            None => None,
        };
        if lock(&self.inner.held).contains_key(id) {
            return self.remove_held(id).await;
        }
        // Kept in place: what the placeholders held comes back into the cache, so it goes where the cache goes.
        self.take_back_placed(id).await?;
        // (An on-demand folder's root is its cache; its place is where the host shows it.)
        let cache = self.cache_path(id);
        let place = entry.as_ref().filter(|e| e.on_demand && cache.exists()).map(|e| e.path.clone());
        // An on-demand folder's files held here (changes no other device has, the former folder after a switch to on
        // demand, ignored files, the archive) are never deleted: they become an ordinary folder at the folder's place
        // (or next to it, when something is there), where the user finds them; for the Trash, they go from beside the
        // cache. The journal first, before anything is touched: a crash from here on finishes the removal at the next
        // start (the files at the place), and nothing is forgotten before the files are where they go.
        let planned = match &place {
            Some(place) => {
                let to = if for_trash {
                    let holder = self.inner.dir.join(format!("{id}.removed"));
                    holder.join(place.file_name().map_or_else(|| std::ffi::OsString::from(id.to_string()), |n| n.to_os_string()))
                } else {
                    free_place_name(place)
                };
                let j = Journal::Remove { from: cache.clone(), to: to.clone(), place: place.clone(), copied: false, renamed: false, made_to: !to.exists(), done: false };
                write_journal_at(&self.journal_path(id), &j).map_err(|e| invalid(format!("the removal could not be noted ({e}); nothing changed")))?;
                Some((to, j))
            }
            None => None,
        };
        let path = match self.stop_folder(id).await {
            Ok((p, closed)) => {
                if let (Some(_), Some(e)) = (&planned, &entry)
                    && !closed.load(Ordering::SeqCst)
                {
                    // Its core still runs somewhere: the cache is not moved under it. Nothing changes.
                    self.drop_journal(id);
                    self.reopen_when_closed(closed, e.clone());
                    return Err(invalid(format!(
                        "the folder is still busy (a scan or a transfer); it stays, its files in {}: try again in a moment",
                        cache.display()
                    )));
                }
                p
            }
            // In the configuration but not running (it could not open): only its entry and state go.
            Err(SyncError::NotFound) => entry.as_ref().map(|e| e.path.clone()).ok_or(SyncError::NotFound)?,
            Err(e) => {
                self.drop_journal(id);
                return Err(e);
            }
        };
        let Some((to, j)) = planned else {
            self.forget_folder(id)?;
            prune_state(&path);
            return Ok(path);
        };
        // An empty folder there (the host's) is taken over.
        let r = match take_over_empty(&to) {
            Ok(()) => self.noted_move(id, j).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &r
            && unfinished(e)
        {
            return Err(self.hold_unfinished(id, e));
        }
        let leftover = match r {
            Ok(leftover) => leftover,
            Err(e) => {
                // Nothing changed: the folder syncs on, its files in the cache.
                self.drop_journal(id);
                if let Some(e) = &entry {
                    let _ = self.open_folder(e).await;
                }
                return Err(invalid(format!("its files could not be moved to {} ({e}); the folder stays, its files in {}", to.display(), cache.display())));
            }
        };
        prune_state(&to);
        self.forget_folder(id)?;
        if !for_trash {
            self.drop_journal(id);
        }
        if let Some(l) = leftover {
            eprintln!("seedwell sync: the folder's files are in {}; a copy left behind could not be deleted: {}", to.display(), l.display());
        }
        Ok(to)
    }

    /// Moves `j`'s files (`from` → `to`) with the journal written first and noted once a copy is whole
    /// ([`move_tree_noted`]): the outer error is the journal's, the inner the move's (nothing moved).
    async fn journaled_move(&self, id: &FolderId, j: Journal) -> io::Result<io::Result<Option<PathBuf>>> {
        write_journal_at(&self.journal_path(id), &j)?;
        Ok(self.noted_move(id, j).await)
    }

    /// An [`Unfinished`] move of the folder: its journal stays, the folder is held (shown in error, with why) until a
    /// start finishes the move. Returns the error to report.
    pub(crate) fn hold_unfinished(&self, id: &FolderId, e: &io::Error) -> SyncError {
        let why = format!("A move could not be finished ({e}); it is finished when Seedwell starts again");
        let held = Held { why: why.clone(), waiting: false, role: self.held_role(id) };
        lock(&self.inner.held).insert(*id, held);
        invalid(why)
    }

    /// Moves `j`'s files, its journal written already: noted whole once a copy is ([`move_tree_noted`]), and over once
    /// it is (a start after that only saves what follows the move, whatever is at `from` by then).
    pub(crate) async fn noted_move(&self, id: &FolderId, j: Journal) -> io::Result<Option<PathBuf>> {
        let path = self.journal_path(id);
        let (from, to) = j.paths();
        let moved = tokio::task::spawn_blocking(move || {
            let manifest = path.with_extension("manifest");
            let leftover = move_tree_kept(&from, &to, |by_rename| write_journal_at(&path, &j.copied(by_rename)), false, Some(&manifest))?;
            if let Err(e) = write_journal_at(&path, &j.finished()) {
                eprintln!("seedwell sync: {} moved to {}, but that could not be noted ({e})", from.display(), to.display());
            }
            Ok(leftover)
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)));
        // A torrent whose files were here (or are now, back from an earlier move) looks again.
        if moved.is_ok() {
            self.files_moved();
        }
        moved
    }

    /// A folder's files moved on this device: the torrents missing theirs look again ([`LocalTorrents::moved`]).
    fn files_moved(&self) {
        if let Some(t) = &self.inner.torrents {
            t.moved();
        }
    }

    /// Removes a folder held back by an unfinished move: it is forgotten, its retry ends and its journal goes, and
    /// nothing on disk is touched. Returns where its original files are; the log also names the partial copy. (Under
    /// the retry's guard: a retry never finishes a move of a folder removed meanwhile.)
    async fn remove_held(&self, id: &FolderId) -> Result<PathBuf, SyncError> {
        let _one = self.inner.held_retrying.lock().await;
        let j: Option<Journal> = std::fs::read(self.journal_path(id)).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let place = lock(&self.inner.cfg).folder(&id.to_string()).map(|e| e.path.clone()).unwrap_or_default();
        lock(&self.inner.held).remove(id);
        self.forget_folder(id)?;
        self.drop_journal(id);
        let (from, to) = j.map(|j| j.paths()).unwrap_or_else(|| (place.clone(), place.clone()));
        eprintln!(
            "seedwell sync: folder {id} no longer syncs; nothing was deleted: its original files are in {} (on a disk not mounted now), a partial copy is in {}",
            from.display(),
            to.display()
        );
        Ok(from)
    }

    /// Forgets a stopped folder: its entry, keys, index and membership, and the devices met only there.
    fn forget_folder(&self, id: &FolderId) -> Result<(), SyncError> {
        lock(&self.inner.ctx.ours).remove(id);
        // Joined again later in the same session, it is offered again there.
        for p in self.inner.ctx.registry.peers() {
            lock(&p.joined).remove(id);
            lock(&p.offered).remove(id);
            lock(&p.answered).remove(id);
        }
        lock(&self.inner.cfg).folders.retain(|f| f.id != id.to_string());
        self.save_config()?;
        for name in [crate::roles::secret_name(id), crate::roles::invite_name(id), crate::roles::links_name(id)] {
            self.inner.secrets.delete(&name)?;
        }
        let _ = std::fs::remove_file(self.index_path(id));
        let _ = std::fs::remove_file(self.members_path(id));
        // What the host kept of it goes with it.
        crate::placed::Placed::remove_log(&self.placed_path(id));
        let _ = std::fs::remove_file(self.anchor_path(id));
        self.forget_strangers();
        Ok(())
    }

    pub(crate) fn journal_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.journal"))
    }

    pub(crate) fn drop_journal(&self, id: &FolderId) {
        let _ = std::fs::remove_file(self.journal_path(id));
        let _ = std::fs::remove_file(self.manifest_path(id));
    }

    /// Where a move to another volume keeps the manifest of its source (beside its journal).
    pub(crate) fn manifest_path(&self, id: &FolderId) -> PathBuf {
        self.journal_path(id).with_extension("manifest")
    }

    /// At start, before any folder opens: a removal or switch that was cut off (a crash, the power) is finished, or
    /// undone when its files never moved. Never deletes a file.
    /// Returns the folders whose change could not be finished: they stay stopped (with the reason) until a start that
    /// can; and those whose switch between full and on demand was finished (the *On all my devices* statement says
    /// their new mode once the user's devices are open).
    async fn recover_journals(&self) -> (BTreeMap<FolderId, Held>, Vec<FolderId>) {
        let (mut stopped, mut switched) = (BTreeMap::new(), Vec::new());
        let Ok(dir) = std::fs::read_dir(&self.inner.dir) else { return (stopped, switched) };
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".journal").and_then(|i| i.parse::<FolderId>().ok()) else { continue };
            let j: Option<Journal> = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
            match self.recover(&id, j).await {
                Err(err) => {
                    eprintln!("seedwell sync: folder {id}: an interrupted change could not be finished ({err}); it stays stopped, and it is tried again when a disk is mounted");
                    stopped.insert(id, self.held_of(&id, &err));
                    continue;
                }
                Ok(true) => switched.push(id),
                Ok(false) => {}
            }
            self.drop_journal(&id);
        }
        (stopped, switched)
    }

    /// Finishes or undoes the change `j` of folder `id`; `true` when that switched the folder between full and on
    /// demand.
    async fn recover(&self, id: &FolderId, j: Option<Journal>) -> Result<bool, SyncError> {
        match j {
            None => Ok(false),
            Some(Journal::Remove { from, to, place, copied, renamed, made_to, done }) => {
                // The move is finished (or a partial copy undone), then the files go to the place if they are not
                // there (a removal on its way to the Trash), then the folder is forgotten, as the removal would have.
                let at = match finish_or_done(&from, &to, copied, renamed, made_to, done, self.manifest_path(id)).await? {
                    Some(leftover) => {
                        if let Some(l) = leftover {
                            eprintln!("seedwell sync: folder {id}: a copy left behind could not be deleted: {}", l.display());
                        }
                        to
                    }
                    None => from,
                };
                if at.exists() && at.starts_with(&self.inner.dir) {
                    let dest = free_place(&place);
                    let j = Journal::Remove { from: at.clone(), to: dest.clone(), place, copied: false, renamed: false, made_to: !dest.exists(), done: false };
                    self.journaled_move(id, j).await?.map_err(|e| invalid(format!("its files stay in {} ({e})", at.display())))?;
                    prune_state(&dest);
                }
                let _ = std::fs::remove_dir(self.inner.dir.join(format!("{id}.removed")));
                self.forget_folder(id).map(|()| false)
            }
            Some(Journal::Move { from, to, copied, renamed, made_to, done }) => {
                // Finished when its copy was whole (or it was a rename), undone otherwise; over, only saved.
                if let Some(leftover) = finish_or_done(&from, &to, copied, renamed, made_to, done, self.manifest_path(id)).await? {
                    if let Some(l) = leftover {
                        eprintln!("seedwell sync: folder {id}: a copy left behind could not be deleted: {}", l.display());
                    }
                    if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()).filter(|e| e.path == from) {
                        e.path = to;
                    }
                    self.save_config()?;
                    self.files_moved();
                }
                Ok(false)
            }
            Some(Journal::Switch { on, from, to, place, copied, renamed, made_to, done: over }) => {
                let done = lock(&self.inner.cfg).folder(&id.to_string()).is_none_or(|e| e.on_demand == on);
                if done {
                    return Ok(false);
                }
                // Finished when its copy was whole (or it was a rename), undone otherwise: never a half-deleted place.
                // Over, only saved.
                if let Some(leftover) = finish_or_done(&from, &to, copied, renamed, made_to, over, self.manifest_path(id)).await? {
                    if let Some(l) = leftover {
                        eprintln!("seedwell sync: folder {id}: a copy left behind could not be deleted: {}", l.display());
                    }
                    if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
                        e.on_demand = on;
                        e.on_demand_others &= on;
                        e.pins = Vec::new();
                        e.mode_switched = true;
                        e.path = place;
                    }
                    self.save_config()?;
                    // Back in place (a switch to full): a torrent missing them finds them now, not at its next retry
                    // (the daemon has no disk observer to tell it).
                    self.files_moved();
                    return Ok(true);
                }
                Ok(false)
            }
        }
    }


    /// Stops a folder's runtime and takes it out of the registry. Its root, and a flag that is up once its index is
    /// closed: it waits (a minute at most) until nothing holds the folder any more, which on a loaded Mac takes a while.
    async fn stop_folder(&self, id: &FolderId) -> Result<(PathBuf, Arc<AtomicBool>), SyncError> {
        // Closed first (out of the registry), then stale on every session that has it linked (opened again, those
        // sessions start anew); a link made meanwhile finds it gone ([`crate::peer::start_link`]).
        let f = self.inner.ctx.registry.remove_folder(id).ok_or(SyncError::NotFound)?;
        self.folders_moved();
        self.close_on_sessions(id);
        f.stopped.send_replace(true);
        let runner = lock(&self.inner.runners).remove(id);
        let end = Instant::now() + *lock(&self.inner.stop_wait);
        if let Some(r) = runner {
            let _ = r.stop.send(true);
            let _ = tokio::time::timeout_at(end.into(), r.task).await;
        }
        for p in self.inner.ctx.registry.peers() {
            p.folders.write().unwrap_or_else(|e| e.into_inner()).remove(id);
        }
        let (root, closed) = (f.root.clone(), f.closed.watch());
        drop(f);
        while !closed.load(Ordering::SeqCst) && Instant::now() < end {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok((root, closed))
    }

    /// Moves a folder on this Mac to `new_path` (a rename when only the last name changes). Sync stops for
    /// the move and starts again at the new place; other devices are not affected.
    pub async fn move_folder(&self, id: &FolderId, new_path: PathBuf) -> Result<FolderStatus, SyncError> {
        self.refuse_held(id)?;
        let new_path = absolute(&new_path)?;
        if new_path.exists() {
            return Err(invalid(format!("{} already exists", new_path.display())));
        }
        refuse_cloud_root(&new_path)?;
        self.refuse_forbidden(&new_path)?;
        self.refuse_overlap_other(&new_path, Some(id))?;
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if entry.on_demand {
            return Err(invalid(
                "an on-demand folder's place belongs to the system's file provider (its files are in this device's cache): it cannot be moved here; rename it, or switch it to full first",
            ));
        }
        if new_path.starts_with(&entry.path) {
            return Err(invalid("a folder cannot move into itself"));
        }
        // A torrent seeding from it would lose its files.
        self.refuse_in_use(&entry.path).await?;
        // The journal first: a move to another volume cut off is finished (its copy whole) or undone at the next start.
        let j = Journal::Move { from: entry.path.clone(), to: new_path.clone(), copied: false, renamed: false, made_to: !new_path.exists(), done: false };
        write_journal_at(&self.journal_path(id), &j).map_err(|e| invalid(format!("the move could not be noted ({e}); nothing changed")))?;
        let closed = match self.stop_folder(id).await {
            Ok((_, closed)) => closed,
            Err(e) => {
                self.drop_journal(id);
                return Err(e);
            }
        };
        if !closed.load(Ordering::SeqCst) {
            // Its core still runs somewhere (a scan on a loaded Mac): moving the files under it would read as
            // deletions. They stay, and the folder starts again where it is once its index is free.
            self.drop_journal(id);
            self.reopen_when_closed(closed, entry);
            return Err(invalid("the folder is still busy (a scan or a transfer); it stays where it is, try again in a moment"));
        }
        // Stopping took a while (up to a minute): a torrent added meanwhile, or moved into the folder, is asked about
        // again right before the move. Refused, nothing moved: the folder syncs on where it is.
        if let Err(e) = self.refuse_in_use(&entry.path).await {
            self.drop_journal(id);
            self.open_folder(&entry).await?;
            return Err(e);
        }
        let moved = self.noted_move(id, j).await;
        if let Err(e) = &moved
            && unfinished(e)
        {
            return Err(self.hold_unfinished(id, e));
        }
        let mut entry2 = entry.clone();
        let mut saved = Ok(());
        if moved.is_ok() {
            entry2.path = new_path;
            if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
                e.path = entry2.path.clone();
            }
            saved = self.save_config();
        }
        // Saved (or nothing moved), the journal goes; not saved, the next start finishes the move.
        if moved.is_err() || saved.is_ok() {
            self.drop_journal(id);
        }
        // Back to syncing, at the new place or (if the move failed, the original untouched) the old one.
        let wait = *lock(&self.inner.reopen_wait);
        tokio::time::sleep(wait).await;
        self.open_folder(&entry2).await?;
        saved?;
        if let Some(l) = moved? {
            eprintln!("seedwell sync: {} moved to {}; a copy left behind could not be deleted: {}", entry.path.display(), entry2.path.display(), l.display());
        }
        self.folder_status(id).ok_or(SyncError::NotFound)
    }

    /// Opens `entry` again once the index of its stopped folder is closed (ten minutes at most): a move or switch that
    /// found it still busy changed nothing, and the folder goes on as it was.
    pub(crate) fn reopen_when_closed(&self, closed: Arc<AtomicBool>, entry: FolderEntry) {
        let me = self.clone();
        tokio::spawn(async move {
            let end = Instant::now() + Duration::from_secs(600);
            while !closed.load(Ordering::SeqCst) && Instant::now() < end {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            if let Err(e) = me.open_folder(&entry).await {
                eprintln!("seedwell sync: {} could not start again after a refused move: {e}", entry.path.display());
            }
        });
    }

    /// Renames a folder on this Mac (its directory). Other devices keep their own names.
    pub async fn rename_folder(&self, id: &FolderId, name: &str) -> Result<FolderStatus, SyncError> {
        self.refuse_held(id)?;
        let name = name.trim();
        if name.is_empty() || name.contains('/') || name == "." || name == ".." || name.starts_with('.') {
            return Err(invalid("not a usable folder name"));
        }
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        let parent = entry.path.parent().ok_or_else(|| invalid("the folder has no parent"))?;
        let to = parent.join(name);
        if entry.on_demand && entry.in_place {
            // Its content is in the host's placeholders at its place: a new name would be a new place without them.
            return Err(invalid("a folder whose files on demand are kept in place is renamed after it is switched to full"));
        }
        if entry.on_demand {
            // Its files are in this device's cache, and its place is the host's: only the name it shows changes.
            if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
                e.path = to.clone();
            }
            self.save_config()?;
            if let Some(f) = self.inner.ctx.registry.folder(id) {
                *lock(&f.place) = to;
                *lock(&f.name) = name.to_string();
            }
            return self.folder_status(id).ok_or(SyncError::NotFound);
        }
        self.move_folder(id, to).await
    }

    /// Pauses or resumes a folder.
    pub async fn set_paused(&self, id: &FolderId, paused: bool) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        // Closed first, then stale on every session that has it linked: a link made meanwhile sees the flag
        // ([`crate::peer::start_link`]), so none is missed, and nothing of it is taken from a stale session.
        f.paused.store(paused, Ordering::SeqCst);
        self.folders_moved();
        if paused {
            self.close_on_sessions(id);
        }
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.paused = paused;
        }
        self.save_config()?;
        if paused {
            lock(&f.live).state = FolderState::Paused;
        } else {
            lock(&f.live).state = FolderState::Starting;
            f.rescan.notify_one();
            // In the background (the call never waits on the network): sessions where it went stale start anew;
            // others link it now (a session that began while it was paused has not). (0.1.11 did nothing here.)
            let (me, id) = (self.clone(), *id);
            if self.plays_0_1_11() {
                return Ok(());
            }
            tokio::spawn(async move {
                let wait = *lock(&me.inner.reopen_wait);
                tokio::time::sleep(wait).await;
                me.offer_folder(id).await;
            });
        }
        Ok(())
    }

    /// The conflict copies in a folder not yet resolved: (copy, the path it is a copy of), in the
    /// folder's wire form. Reads the whole index: call when the list is shown, not for every update.
    pub async fn conflicts(&self, id: &FolderId) -> Result<Vec<(String, String)>, SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        Ok(crate::runtime::refresh_conflicts(&f).await?)
    }

    /// What a folder is busy with, path by path ([`Activity`]). Never waits for the folder's core.
    pub fn activity(&self, id: &FolderId) -> Result<Activity, SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        let local = lock(&self.inner.cfg).folders.iter().any(|e| e.id == id.to_string() && !e.ignore.is_empty());
        // (The file is looked for by the folder's runtime: the folder's disk may be slow or gone, and this is asked
        // every few seconds from the app's main thread.)
        let live = lock(&f.live);
        let own_ignores = local || live.ignore_file;
        Ok(Activity {
            pending: live.pending.0.clone(),
            pending_more: live.pending.1,
            conflicts: live.conflicts.keys().cloned().collect(),
            own_ignores,
        })
    }

    /// Resolves a conflict: `copy` is the conflict copy (in the folder, wire form). The version that is
    /// not kept goes to this Mac's archive, never away; the change then syncs like any edit.
    pub async fn resolve_conflict(&self, id: &FolderId, copy: &str, keep: Keep) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        trs_sync_proto::model::validate_path(copy).map_err(|_| invalid(format!("{copy:?} is not a path in the folder")))?;
        let parsed = trs_sync::conflict::parse_conflict(copy);
        let known = lock(&f.live).conflicts.get(copy).cloned();
        let Some(original) = known.or_else(|| parsed.map(|c| c.original)) else {
            return Err(invalid(format!("{copy} is not a conflict copy")));
        };
        let root = f.root.clone();
        let (copy_path, original) = (copy.to_string(), original.to_string());
        if f.on_demand {
            self.resolve_on_demand(&f, copy_path, original, keep).await?;
            lock(&f.live).conflicts.remove(copy);
            return Ok(());
        }
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let copy = copy_path;
            let fs = StdFs::new(root);
            if fs.lstat(STATE_DIR)?.is_none() {
                return Err(io::Error::other("the folder is missing"));
            }
            let to_archive = |p: &str| to_archive(&fs, p);
            match keep {
                Keep::Original => to_archive(&copy),
                Keep::Copy => {
                    to_archive(&original)?;
                    fs.rename(&copy, &original)
                }
                Keep::Both => {
                    let name = plain_name(&copy, |p| Ok(fs.lstat(p)?.is_some()))?;
                    fs.rename(&copy, &name)
                }
            }
        })
        .await
        .map_err(io::Error::other)??;
        lock(&f.live).conflicts.remove(copy);
        f.rescan.notify_one();
        Ok(())
    }

    /// [`Self::resolve_conflict`] in an on-demand folder: through the core's host changes (the cache is no folder a
    /// scan reads changes from), with both versions here; what goes is kept in the cache's archive, as a full folder
    /// keeps it.
    async fn resolve_on_demand(&self, f: &Arc<FolderShared>, copy: String, original: String, keep: Keep) -> Result<(), SyncError> {
        let refused = self
            .with_core(f, move |c| {
                use trs_sync::ondemand::ItemState;
                if !c.signs() {
                    return Err(trs_sync::folder::HostError::ReadOnly);
                }
                for p in [&copy, &original] {
                    if !c.item_at(p)?.is_some_and(|i| matches!(i.state, ItemState::Downloaded | ItemState::ModifiedHere)) {
                        return Ok(Some("Download both versions first"));
                    }
                }
                // (Kept in place: through the notes, so content in a placeholder comes back into the cache first.)
                let fs = c.fs();
                match keep {
                    Keep::Original => {
                        to_archive(&*fs, &copy)?;
                        c.host_delete(&copy, &[])?;
                    }
                    // Checked as a whole first: a refusal changes nothing.
                    Keep::Copy => {
                        c.host_replace(&copy, &original)?;
                    }
                    Keep::Both => {
                        // Free in the index too: an online-only file there has no file in the cache.
                        let name = plain_name(&copy, |p| Ok(fs.lstat(p)?.is_some() || c.item_at(p)?.is_some()))?;
                        c.host_rename(&copy, &name)?;
                    }
                }
                Ok(None)
            })
            .await?;
        match refused {
            Some(why) => Err(invalid(why)),
            None => Ok(()),
        }
    }

    /// Scans the whole folder now.
    pub fn rescan(&self, id: &FolderId) -> Result<(), SyncError> {
        self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?.rescan.notify_one();
        Ok(())
    }

    /// Every device that joined a folder or channel, also those that left, in the order they joined, each with when
    /// and its user's name from its signed profile (empty: none): the same on every member device (a channel's
    /// colours follow it). Never waits for the folder's core.
    pub fn joined(&self, id: &FolderId) -> Result<Vec<(DeviceId, u64, String)>, SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        Ok(m.joined()
            .into_iter()
            .map(|(pk, at)| (DeviceId::of_public_key(&pk), at, m.profile(&pk).map(|(_, user)| user.to_owned()).unwrap_or_default()))
            .collect())
    }

    /// The devices of a folder (or channel) and their roles. Never waits for the folder's core.
    pub fn members(&self, id: &FolderId) -> Result<Vec<MemberInfo>, SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let me = self.inner.ctx.me;
        let online: BTreeMap<DeviceId, String> = self
            .inner
            .ctx
            .registry
            .peers()
            .into_iter()
            .filter(|p| p.access(id).is_some())
            .map(|p| (p.device, p.name.clone()))
            .collect();
        let remembered: BTreeMap<String, String> = lock(&self.inner.cfg).all_devices().map(|d| (d.id.clone(), d.name.clone())).collect();
        let my_name = lock(&self.inner.ctx.device_name).clone();
        let my_user = lock(&self.inner.ctx.user_name).clone();
        let profiles: BTreeMap<DeviceId, Names> = f.profile_names().into_iter().collect();
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<MemberInfo> = m
            .members()
            .map(|(pk, mem)| {
                let device = DeviceId::of_public_key(pk);
                let (name, user) = if device == me {
                    (my_name.clone(), my_user.clone())
                } else {
                    let name = profiles
                        .get(&device)
                        .map(|n| &n.device)
                        .or_else(|| online.get(&device))
                        .cloned()
                        .or_else(|| remembered.get(&device.to_string()).cloned())
                        .unwrap_or_default();
                    (name, profiles.get(&device).map(|n| n.user.clone()).unwrap_or_default())
                };
                MemberInfo {
                    device,
                    name,
                    user,
                    role: mem.role,
                    online: device == me || online.contains_key(&device),
                    is_me: device == me,
                    expires_s: if mem.role == Role::Owner { None } else { m.share_until() },
                }
            })
            .collect();
        out.sort_by(|a, b| b.is_me.cmp(&a.is_me).then(b.role.cmp(&a.role)).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    /// Makes an invitation to the folder: `role`, `max_uses` (0 = unlimited) and `valid_days` (how long the link
    /// admits; 0 = no expiry). Owners invite at any role, admins at Read & write or Read only. The link is a
    /// secret: the caller treats it as one (the agent interface returns it only with Full permission).
    pub fn create_invite(&self, id: &FolderId, role: Role, max_uses: u32, valid_days: u32) -> Result<InviteInfo, SyncError> {
        self.make_invite(id, role, max_uses, valid_days, crate::roles::now_s(), None)
    }

    /// Makes one link for a folder and a channel: an invitation to each, with the same role, uses and expiry. This
    /// device must be allowed to share both at that role (both are checked before either is made). The folder is
    /// linked to the channel here when it has no channel yet.
    pub fn create_bundle_invite(&self, id: &FolderId, role: Role, max_uses: u32, valid_days: u32, channel: &FolderId) -> Result<InviteInfo, SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        let c = self.inner.ctx.registry.channel(channel).ok_or_else(|| invalid(concat!("no such channel on this ", trs_platform::computer!())))?;
        self.may_invite(&f, role, max_uses)?;
        let name = lock(&c.name).clone();
        self.may_invite(&c, role, max_uses).map_err(|e| invalid(format!(concat!("this ", trs_platform::computer!(), " may not share the channel {name} at that role ({e})"), name = name, e = e)))?;
        // One clock reading for both halves: read twice, they could expire a second apart.
        let now = crate::roles::now_s();
        let ci = self.make_invite(channel, role, max_uses, valid_days, now, None)?;
        let cl = InviteLink::parse_chat(&ci.link).map_err(|e| invalid(e.to_string()))?;
        let fi = match self.make_invite(id, role, max_uses, valid_days, now, Some(&cl)) {
            Ok(i) => i,
            Err(e) => {
                let _ = self.revoke_invite(channel, &ci.id);
                return Err(e);
            }
        };
        let linked = lock(&self.inner.cfg).folder(&id.to_string()).is_some_and(|e| !e.channel.is_empty());
        if !linked {
            self.set_folder_channel(id, Some(channel))?;
        }
        Ok(fi)
    }

    /// Links a folder on this device to a channel here (`None`: none); a setting of this device only.
    pub fn set_folder_channel(&self, id: &FolderId, channel: Option<&FolderId>) -> Result<(), SyncError> {
        if let Some(c) = channel
            && self.inner.ctx.registry.channel(c).is_none()
        {
            return Err(invalid(concat!("no such channel on this ", trs_platform::computer!())));
        }
        {
            let mut cfg = lock(&self.inner.cfg);
            let e = cfg.folders.iter_mut().find(|f| f.id == id.to_string()).ok_or(SyncError::NotFound)?;
            e.channel = channel.map(|c| c.to_string()).unwrap_or_default();
        }
        self.save_config()?;
        Ok(())
    }

    /// This device may invite to `f` at `role`: owners at any role, admins at Read & write or Read only.
    fn may_invite(&self, f: &Space, role: Role, max_uses: u32) -> Result<(), SyncError> {
        let noun = crate::roles::noun(f);
        let mine = f.role_of(self.inner.ctx.key.public_key());
        match mine {
            Some(Role::Owner) => {}
            Some(Role::Admin) if role <= Role::ReadWrite => {}
            Some(Role::Admin) => return Err(invalid(format!("only the {noun}'s owner can invite admins or owners"))),
            _ => return Err(invalid(format!("only the {noun}'s owner or an admin can share it"))),
        }
        if max_uses > trs_sync_proto::members::MAX_INVITE_USES {
            return Err(invalid(format!("a link can have at most {} uses (or unlimited)", trs_sync_proto::members::MAX_INVITE_USES)));
        }
        if mine == Some(Role::Owner) && f.secret.read().unwrap_or_else(|e| e.into_inner()).is_none() {
            return Err(invalid(format!(concat!("this ", trs_platform::computer!(), " is an owner but has not received the {noun}'s owner key yet"), noun = noun)));
        }
        Ok(())
    }

    /// Makes an invitation (see [`SyncHandle::create_invite`]) made at `now` (Unix seconds); with `bundle`, the
    /// folder's link also carries that channel's invitation.
    fn make_invite(&self, id: &FolderId, role: Role, max_uses: u32, valid_days: u32, now: u64, bundle: Option<&InviteLink>) -> Result<InviteInfo, SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        self.may_invite(&f, role, max_uses)?;
        let me = *self.inner.ctx.key.public_key();
        let mine = f.role_of(&me);
        let expires_s = if valid_days == 0 { 0 } else { now + u64::from(valid_days) * 86_400 };
        let mut seed = [0u8; 32];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut seed).map_err(|_| invalid("no random numbers"))?;
        // An owner signs with the folder key, an admin with its device key.
        let secret = f.secret.read().unwrap_or_else(|e| e.into_inner()).clone();
        let issuer = if secret.is_some() && mine == Some(Role::Owner) { Signer::Folder } else { Signer::Device(me) };
        let key = self.inner.ctx.key.clone();
        let sign = |m: &[u8]| match (&issuer, &secret) {
            (Signer::Folder, Some(s)) => s.sign(m),
            _ => key.sign(m),
        };
        let invite = new_invite(*f.public.public_key(), random_id(), seed, role, max_uses, expires_s, now, issuer, &sign);
        let name = lock(&f.name).clone();
        let kind = if f.kind == SpaceKind::Channel { trs_sync_proto::members::LinkKind::Channel } else { trs_sync_proto::members::LinkKind::Folder };
        // A channel open to agents says so in its link, so agents on the joining device may use it before admission.
        let agents = f.kind == SpaceKind::Channel && f.membership.read().unwrap_or_else(|e| e.into_inner()).agents();
        // Where this device is now, so the joining device can dial it without discovery (sealed in the link).
        let l = InviteLink { invite, token_seed: seed, name: Some(name), agents, reach: Some(self.own_reach()) };
        let link = match bundle {
            Some(c) if kind == trs_sync_proto::members::LinkKind::Folder => l.format_bundle(c),
            _ => l.format_as(kind),
        };
        {
            // (Kept with the links unlocked: the Share dialog reads them, and the secret store may be slow.)
            let _turn = lock(&f.saving);
            let all = {
                let mut links = lock(&f.links);
                links.insert(invite.id, link.clone());
                links.values().map(String::as_str).collect::<Vec<_>>().join("\n")
            };
            self.inner.secrets.put(&crate::roles::links_name(id), all.as_bytes())?;
        }
        // Published, so every owner and admin can show and count it.
        let st = match issuer {
            Signer::Folder => crate::roles::by_owner(&f, Body::Invite(invite)),
            Signer::Device(_) => Some(Statement::by_device(&self.inner.ctx.key, id, Body::Invite(invite))),
        };
        if crate::roles::add(&f, st) {
            self.spawn_changed(&f);
        }
        self.invites(id)?.into_iter().find(|i| i.id == hex(&invite.id)).ok_or(SyncError::NotFound)
    }

    /// The folder's invitations (with their links, for those this device made), newest first. Never waits for
    /// the folder's core.
    pub fn invites(&self, id: &FolderId) -> Result<Vec<InviteInfo>, SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let links = lock(&f.links).clone();
        // A link for a folder and its channel: the channel's id and name (as it is called here, if it is here).
        let bundle = |iid: &[u8; 16]| -> (Option<String>, Option<String>) {
            let Some((_, Some(c))) = links.get(iid).and_then(|l| InviteLink::parse_bundle(l).ok()) else { return (None, None) };
            let cid = c.invite.folder_id();
            let name = self.inner.ctx.registry.channel(&cid).map(|ch| lock(&ch.name).clone()).or(c.name);
            (Some(cid.to_string()), name)
        };
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<InviteInfo> = m
            .invites()
            .map(|st| {
                let (channel_id, channel_name) = bundle(&st.invite.id);
                InviteInfo {
                    id: hex(&st.invite.id),
                    link: links.get(&st.invite.id).cloned().unwrap_or_default(),
                    role: st.invite.role,
                    max_uses: st.invite.max_uses,
                    uses: st.uses,
                    expires_s: (st.invite.expires_s != 0).then_some(st.invite.expires_s),
                    created_s: st.invite.at_s,
                    revoked: st.revoked,
                    channel_id,
                    channel_name,
                }
            })
            .collect();
        // Made here but not yet in the membership (an admin's, before it was published): listed too.
        for (iid, link) in &links {
            if m.invite(iid).is_none()
                && let Ok((l, _)) = InviteLink::parse_any(link)
            {
                let i = l.invite;
                let (channel_id, channel_name) = bundle(iid);
                out.push(InviteInfo {
                    id: hex(iid),
                    link: link.clone(),
                    role: i.role,
                    max_uses: i.max_uses,
                    uses: 0,
                    expires_s: (i.expires_s != 0).then_some(i.expires_s),
                    created_s: i.at_s,
                    revoked: false,
                    channel_id,
                    channel_name,
                });
            }
        }
        out.sort_by(|a, b| b.created_s.cmp(&a.created_s).then_with(|| a.id.cmp(&b.id)));
        Ok(out)
    }

    /// Revokes an invitation: it admits nobody from now on (devices it admitted stay). A folder's link that also
    /// invites to a channel revokes that channel's invitation too.
    pub fn revoke_invite(&self, id: &FolderId, invite_id: &str) -> Result<(), SyncError> {
        let bundled = {
            let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
            let iid = trs_sync_proto::members::invite_id_from_hex(invite_id).ok_or_else(|| invalid("no such link"))?;
            lock(&f.links).get(&iid).and_then(|l| InviteLink::parse_bundle(l).ok()).and_then(|(_, c)| c)
        };
        self.revoke_one(id, invite_id)?;
        if let Some(c) = bundled
            && self.inner.ctx.registry.channel(&c.invite.folder_id()).is_some()
        {
            self.revoke_one(&c.invite.folder_id(), &hex(&c.invite.id))?;
        }
        Ok(())
    }

    fn revoke_one(&self, id: &FolderId, invite_id: &str) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let iid = trs_sync_proto::members::invite_id_from_hex(invite_id).ok_or_else(|| invalid("no such link"))?;
        let me = *self.inner.ctx.key.public_key();
        let body = Body::Revoke { id: iid, at_s: crate::roles::now_s() };
        let st = match f.role_of(&me) {
            Some(Role::Owner) => crate::roles::by_owner(&f, body),
            Some(Role::Admin) => Some(Statement::by_device(&self.inner.ctx.key, id, body)),
            _ => return Err(invalid(format!("only the {}'s owner or an admin can revoke a link", crate::roles::noun(&f)))),
        };
        let st = st.ok_or_else(|| invalid(format!(concat!("this ", trs_platform::computer!(), " has not received the {}'s owner key yet"), crate::roles::noun(&f))))?;
        if crate::roles::add(&f, [st]) {
            self.spawn_changed(&f);
        }
        Ok(())
    }

    /// Changes a device's role (owners only). A folder keeps at least one owner.
    pub fn set_role(&self, id: &FolderId, device: &DeviceId, role: Role) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let pk = self.owner_change(&f, device, role != Role::Owner)?;
        let serial = f.membership.read().unwrap_or_else(|e| e.into_inner()).next_serial();
        let st = crate::roles::by_owner(&f, Body::Grant { device: pk, role, serial, at_s: crate::roles::now_s(), invite: None });
        if crate::roles::add(&f, st) {
            self.spawn_changed(&f);
        }
        Ok(())
    }

    /// Removes a device from the folder (owners only): it is cut off at once and its later changes are ignored. It
    /// keeps the files it has. This device leaves a folder with [`SyncHandle::remove_folder`] instead.
    pub fn remove_device(&self, id: &FolderId, device: &DeviceId) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        if *device == self.inner.ctx.me {
            return Err(invalid(match f.kind {
                SpaceKind::Folder => concat!("to stop syncing on this ", trs_platform::computer!(), ", remove the folder instead"),
                SpaceKind::Channel | SpaceKind::Devices => concat!("to leave the channel on this ", trs_platform::computer!(), ", leave it instead"),
            }));
        }
        let pk = self.owner_change(&f, device, true)?;
        let serial = f.membership.read().unwrap_or_else(|e| e.into_inner()).next_serial();
        let st = crate::roles::by_owner(&f, Body::Remove { device: pk, serial, at_s: crate::roles::now_s() });
        if crate::roles::add(&f, st) {
            self.spawn_changed(&f);
        }
        Ok(())
    }

    /// Shares a folder again from this device when nobody can admit devices to it or change roles any more (SPEC
    /// §7G.5): this device is a read-write or read-only member and none of the folder's owners and admins is
    /// connected. The folder becomes a new folder, with a new id, owned by this device, at the same place: its files
    /// stay where they are, as they are (nothing is deleted, moved or downloaded again; the first scan reads them in
    /// as this device's), and so does `.seedwell/Archive`. The old folder is left: its id, keys, index, membership and
    /// the devices met in it are forgotten here (the others keep it). Its settings on this device (paused, local
    /// network only, archive, local ignore patterns) carry over; its channel link does not. Then the usual Share
    /// makes a link. Not for an on-demand folder (its files are online only, in this device's cache).
    pub async fn reshare(&self, id: &FolderId) -> Result<FolderStatus, SyncError> {
        self.refuse_held(id)?;
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        let f = self.inner.ctx.registry.folder(id).ok_or_else(|| invalid("the folder is not running here: start Seedwell again, or remove it"))?;
        if entry.on_demand {
            return Err(invalid(concat!(
                "an on-demand folder's files are online only: switch it to keep every file on this ",
                trs_platform::computer!(),
                " (mode full) first, then share it again"
            )));
        }
        let me = *self.inner.ctx.key.public_key();
        match f.role_of(&me) {
            Some(Role::Owner | Role::Admin) => return Err(invalid(concat!("this ", trs_platform::computer!(), " can share the folder itself: make a link with Share"))),
            None if f.joining(&me) => return Err(invalid(concat!("this ", trs_platform::computer!(), " is not admitted to the folder yet: there is nothing to share again"))),
            None => return Err(invalid(concat!("this ", trs_platform::computer!(), " is no longer a member of the folder: add the folder as a new one instead"))),
            Some(_) => {}
        }
        // An owner or admin connected now can admit devices and change roles: nothing is stuck.
        let names: BTreeMap<DeviceId, Names> = f.profile_names().into_iter().collect();
        if let Some(p) = self.inner.ctx.registry.peers().into_iter().find(|p| p.access(id).is_some() && f.role_of(&p.device_pk).is_some_and(Role::admits)) {
            let name = names.get(&p.device).map(|n| n.device.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| p.name.clone());
            let role = if f.role_of(&p.device_pk) == Some(Role::Owner) { "an owner" } else { "an admin" };
            return Err(invalid(format!(
                concat!("{name}, {role} of this folder, is connected: ask it to share the folder, or to make this ", trs_platform::computer!(), " an admin"),
                name = name,
                role = role
            )));
        }
        drop(f);
        let (_, closed) = self.stop_folder(id).await?;
        if !closed.load(Ordering::SeqCst) {
            // Its core still runs somewhere (a scan on a loaded Mac): nothing changes, and the folder goes on.
            self.reopen_when_closed(closed, entry);
            return Err(invalid("the folder is still busy (a scan or a transfer); it stays as it is, try again in a moment"));
        }
        // The new folder's key and membership first: until the configuration names it, nothing refers to them.
        let new = match self.new_owned_entry(&entry) {
            Ok(e) => e,
            Err(err) => {
                // Nothing changed: the old folder goes on.
                let _ = self.open_folder(&entry).await;
                return Err(invalid(format!("the folder could not be shared again ({err}); nothing changed")));
            }
        };
        let new_id: FolderId = new.id.parse().map_err(|_| invalid("bad folder id"))?;
        // One write of the configuration swaps the entries; then the old folder's keys, index and membership go.
        {
            let mut cfg = lock(&self.inner.cfg);
            if let Some(slot) = cfg.folders.iter_mut().find(|e| e.id == entry.id) {
                *slot = new.clone();
            }
        }
        lock(&self.inner.ctx.ours).insert(new_id);
        self.forget_folder(id)?;
        self.open_folder(&new).await?;
        self.folder_status(&new_id).ok_or(SyncError::NotFound)
    }

    /// A new folder owned by this device at `old`'s place, with `old`'s settings on this device: its key (in the
    /// secret store) and membership (this device its owner) are made; its entry is returned, not configured yet.
    fn new_owned_entry(&self, old: &FolderEntry) -> Result<FolderEntry, SyncError> {
        let me = *self.inner.ctx.key.public_key();
        let secret = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
        let id = FolderId::of_public_key(secret.public_key());
        let grant = Statement::by_owner(&secret, Body::Grant { device: me, role: Role::Owner, serial: 1, at_s: crate::roles::now_s(), invite: None });
        let mut m = Membership::new(*secret.public_key());
        m.add([grant]);
        crate::roles::save(&self.members_path(&id), &m)?;
        self.inner.secrets.put(&crate::roles::secret_name(&id), secret.as_bytes())?;
        let mut e = FolderEntry::new(id.to_string(), old.path.clone());
        e.folder_pk = hex(secret.public_key());
        e.paused = old.paused;
        e.lan_only = old.lan_only;
        e.on_demand_others = old.on_demand_others;
        e.archive = old.archive;
        e.archive_days = old.archive_days;
        e.ignore = old.ignore.clone();
        Ok(e)
    }

    /// Checks an owner's change of `device` (`demotes`: it may stop being an owner) and returns its key.
    fn owner_change(&self, f: &Space, device: &DeviceId, demotes: bool) -> Result<[u8; 32], SyncError> {
        let me = *self.inner.ctx.key.public_key();
        let noun = crate::roles::noun(f);
        if f.role_of(&me) != Some(Role::Owner) || f.secret.read().unwrap_or_else(|e| e.into_inner()).is_none() {
            return Err(invalid(format!("only the {noun}'s owner can change roles")));
        }
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        let (pk, mem) = m.members().find(|(pk, _)| DeviceId::of_public_key(pk) == *device).ok_or_else(|| invalid(format!("no such device in this {noun}")))?;
        let owners = m.members().filter(|(_, x)| x.role == Role::Owner).count();
        if demotes && mem.role == Role::Owner && owners == 1 {
            return Err(invalid(format!("a {noun} needs an owner: make another device an owner first")));
        }
        Ok(*pk)
    }

    /// Sets how long the folder is shared (owners only): `until_s` Unix seconds, 0 = no end. After it every device
    /// but the owners is cut off (it keeps its files, and changes it makes later are ignored) and no link admits
    /// anyone; a later end, or none, lets them back in.
    pub fn set_share_until(&self, id: &FolderId, until_s: u64) -> Result<(), SyncError> {
        let now = crate::roles::now_s();
        if until_s != 0 && until_s <= now {
            return Err(invalid("Pick an end in the future."));
        }
        self.share_until_at(id, until_s, now)
    }

    /// Test switch: the folder's sharing ended a second ago (without waiting for a day).
    pub fn test_end_sharing(&self, id: &FolderId) -> Result<(), SyncError> {
        let now = crate::roles::now_s();
        self.share_until_at(id, now - 1, now)
    }

    fn share_until_at(&self, id: &FolderId, until_s: u64, now: u64) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let me = *self.inner.ctx.key.public_key();
        if f.role_of(&me) != Some(Role::Owner) || f.secret.read().unwrap_or_else(|e| e.into_inner()).is_none() {
            return Err(invalid(format!("Only the {}'s owner can change how long it is shared.", crate::roles::noun(&f))));
        }
        let serial = f.membership.read().unwrap_or_else(|e| e.into_inner()).next_serial();
        let st = crate::roles::by_owner(&f, Body::ShareUntil { until_s, serial, at_s: now });
        if crate::roles::add(&f, st) {
            self.spawn_changed(&f);
        }
        Ok(())
    }

    pub(crate) fn spawn_changed(&self, f: &Arc<Space>) {
        let (ctx, f) = (self.inner.ctx.clone(), f.clone());
        self.inner.rt.spawn(async move { crate::roles::changed(&ctx, &f).await });
    }

    /// Adds an address (`host:port`) where devices of this folder (or channel) can be reached.
    pub fn add_address(&self, id: &FolderId, addr: SocketAddr) -> Result<(), SyncError> {
        self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        {
            // Given now (by the user, an agent): dialed at once, whatever an earlier dial there met.
            let mut book = lock(&self.inner.book);
            book.hints.entry(*id).or_default().insert(addr);
            book.tried.remove(&addr);
            book.refused.remove(&addr);
        }
        {
            let mut cfg = lock(&self.inner.cfg);
            let a = addr.to_string();
            if let Some(e) = cfg.folder_mut(&id.to_string())
                && !e.addresses.contains(&a)
            {
                e.addresses.push(a.clone());
            }
            if let Some(e) = cfg.channel_mut(&id.to_string())
                && !e.addresses.contains(&a)
            {
                e.addresses.push(a);
            }
        }
        self.save_config()?;
        self.inner.wake_dialer.notify_one();
        Ok(())
    }

    /// The device-wide settings.
    pub fn settings(&self) -> Settings {
        lock(&self.inner.cfg).settings.clone()
    }

    /// Changes the device-wide settings (the port takes effect at the next start; `trs_control::sync::SyncSlot::move_port` starts the service again on it).
    pub fn set_settings(&self, s: Settings) -> Result<(), SyncError> {
        if !s.default_location.trim().is_empty() {
            self.refuse_forbidden(Path::new(&s.default_location))?;
        }
        let renamed = !s.device_name.is_empty() && {
            let name: String = s.device_name.chars().take(64).collect();
            std::mem::replace(&mut *lock(&self.inner.ctx.device_name), name.clone()) != name
        };
        let user = clean_user_name(&s.user_name);
        let renamed = std::mem::replace(&mut *lock(&self.inner.ctx.user_name), user.clone()) != user || renamed;
        self.inner.ctx.limits.set_kib(s.up_kib, s.down_kib);
        lock(&self.inner.cfg).settings = s;
        self.save_config()?;
        if renamed {
            self.publish_profiles();
            self.send_direct_joins();
        }
        Ok(())
    }

    /// Syncs a folder only with devices on the local network (or everywhere again). Sessions with other
    /// devices stop sharing it at once; they keep the folders that are still allowed.
    pub fn set_lan_only(&self, id: &FolderId, lan_only: bool) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        f.lan_only.store(lan_only, Ordering::SeqCst);
        self.folders_moved();
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.lan_only = lan_only;
        }
        self.save_config()?;
        if lan_only {
            for p in self.inner.ctx.registry.peers().into_iter().filter(|p| !p.lan) {
                p.close_folder(id);
            }
        } else {
            self.inner.wake_dialer.notify_one();
            // Sessions where it went stale start anew; the others link it now.
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                let (me, id) = (self.clone(), *id);
                rt.spawn(async move { me.offer_folder(id).await });
            }
        }
        Ok(())
    }

    /// This device's own ignore patterns for a folder (in addition to its `.seedwellignore`; Syncthing's syntax, one
    /// per entry). Saved, and applied at once (SPEC §7G.2): paths they ignore now are no longer offered or needed, and
    /// a full scan picks up what they no longer ignore.
    pub async fn set_ignore_patterns(&self, id: &FolderId, patterns: Vec<String>) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.ignore = patterns.clone();
        }
        self.save_config()?;
        // (The core is locked only on a blocking thread.) The full scan that follows announces what changed.
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let mut core = lock(&f.core);
            core.set_local_ignore(&patterns)?;
            if core.take_ignore_changed() {
                f.rescan.notify_one();
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)??;
        Ok(())
    }

    fn refuse_overlap(&self, path: &Path) -> Result<(), SyncError> {
        self.refuse_overlap_other(path, None)
    }

    /// [`Self::refuse_overlap`] but for the folder `own` itself (a new place for it).
    pub(crate) fn refuse_overlap_other(&self, path: &Path, own: Option<&FolderId>) -> Result<(), SyncError> {
        let own = own.map(ToString::to_string);
        for e in lock(&self.inner.cfg).folders.iter().filter(|e| Some(&e.id) != own.as_ref()) {
            if path.starts_with(&e.path) || e.path.starts_with(path) {
                return Err(invalid(format!("{} overlaps the synced folder {}", path.display(), e.path.display())));
            }
        }
        Ok(())
    }

    fn folder_status(&self, id: &FolderId) -> Option<FolderStatus> {
        let Some(f) = self.inner.ctx.registry.folder(id) else {
            let held = lock(&self.inner.held).get(id).cloned()?;
            return self.held_status(id, &held);
        };
        Some(self.status_of(&f))
    }

    /// Devices of the folder are connected, none has sent its list of the folder yet (its first index), and this device
    /// lists nothing of its own: whom it waits for, in words ("Waiting for Mac mini to list Movies"), so a folder that shows nothing yet says why
    /// (SPEC §7G.5). Read from the index's read-only handle: never waits.
    fn unlisted(&self, f: &FolderShared) -> Option<String> {
        let view = f.index.view();
        // A device with files of its own listed (an owner) waits for nobody's list.
        if trs_sync::index::IndexStore::local_paths(&view, "", None, 1).is_ok_and(|p| !p.is_empty()) {
            return None;
        }
        let mut names: Vec<String> = Vec::new();
        for p in self.inner.ctx.registry.peers().iter().filter(|p| p.access(&f.id).is_some()) {
            if trs_sync::index::IndexStore::peer(&view, p.device.short()).ok().flatten().is_some_and(|s| s.index_id != 0) {
                return None;
            }
            if !names.contains(&p.name) {
                names.push(p.name.clone());
            }
        }
        let who = match names.as_slice() {
            [] => return None,
            [one] => one.clone(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        };
        Some(format!("Waiting for {who} to list {}", lock(&f.name)))
    }

    fn status_of(&self, f: &Arc<FolderShared>) -> FolderStatus {
        // On all the user's devices, the statement says it (the last of the user's devices to change it), else this
        // device's own mark.
        let shared = self.follow_on_demand(&f.id);
        let (channel_id, on_demand_others) = {
            let cfg = lock(&self.inner.cfg);
            let e = cfg.folder(&f.id.to_string());
            (
                e.map(|e| e.channel.clone()).filter(|c| !c.is_empty() && cfg.channels.iter().any(|ch| ch.id == *c)),
                e.is_some_and(|e| !e.on_demand && shared.unwrap_or(e.on_demand_others)),
            )
        };
        let mut live = {
            let mut l = lock(&f.live);
            if !l.kept.is_empty() {
                // Folders the host kept (SPEC §7G.11, *Kept folders*) that are in the folder again (another device made
                // them anew) are over: read from the index's read-only handle, which never waits.
                let view = f.index.view();
                let back: Vec<String> = l
                    .kept
                    .keys()
                    .filter(|p| trs_sync::index::IndexStore::local(&view, p).ok().flatten().is_some_and(|r| trs_sync::ondemand::live(&r)))
                    .cloned()
                    .collect();
                for p in back {
                    l.drop_kept(&p);
                }
                // Whether the others still are is a look at the disk: off this thread, the next snapshot shows it.
                if !l.kept.is_empty() && l.kept_checked.is_none_or(|t| t.elapsed() >= crate::shared::KEPT_CHECK_EVERY) {
                    l.kept_checked = Some(std::time::Instant::now());
                    let f = f.clone();
                    self.inner.rt.spawn_blocking(move || ondemand::check_kept(&f));
                }
            }
            l.clone()
        };
        live.issues.extend(f.note_issues());
        if !f.on_demand && lock(&self.inner.cfg).folder(&f.id.to_string()).is_some_and(|e| e.full_here) {
            live.issues.push((String::new(), FULL_HERE.into()));
        }
        let refused = lock(&f.notes).refused.clone();
        let role = f.role_of(self.inner.ctx.key.public_key());
        // (Also one following the user's own devices, which joins with no invitation.)
        let joining = lock(&f.invite).is_some() || f.following.load(Ordering::SeqCst);
        let state = if f.paused.load(Ordering::Relaxed) {
            FolderState::Paused
        } else if role.is_some() {
            live.state
        } else if let Some(why) = refused.filter(|_| joining) {
            live.error = Some(why);
            FolderState::Error
        } else if joining {
            FolderState::WaitingForAdmin
        } else {
            let ended = f.membership.read().unwrap_or_else(|e| e.into_inner()).expired(self.inner.ctx.key.public_key());
            live.error = Some(if ended { "this folder is no longer shared" } else { concat!("this ", trs_platform::computer!(), " was removed from the folder") }.into());
            FolderState::Error
        };
        let scanning = state == FolderState::Scanning;
        let share_until = f.membership.read().unwrap_or_else(|e| e.into_inner()).share_until();
        let (need_files, need_bytes) = live.need;
        let connected = self.inner.ctx.registry.peers().iter().filter(|p| p.access(&f.id).is_some()).count() as u32;
        let members = f.members.read().unwrap_or_else(|e| e.into_inner()).len() as u32;
        let (progress, problem, join_attempts) = self.join_status(f);
        let away = self.away(f);
        let shows = role.is_some() && !matches!(state, FolderState::Paused | FolderState::Missing | FolderState::Error);
        let reshare = shows && !f.on_demand && away.stuck(role);
        // (Before the status below: its name field holds the name's lock to the end of it.)
        let waiting = away.waiting().filter(|_| shows).or_else(|| shows.then(|| self.unlisted(f)).flatten());
        FolderStatus {
            id: f.id.to_string(),
            name: lock(&f.name).clone(),
            path: lock(&f.place).display().to_string(),
            access: role.map_or(Access::Read, Role::access),
            role,
            share_until,
            state,
            files: live.local.0,
            bytes: live.local.1,
            need_files,
            need_bytes,
            rate_in: f.fetched.rate(),
            rate_out: f.served.rate(),
            connected,
            members,
            conflicts: live.conflicts.len() as u32,
            lan_only: f.lan_only.load(Ordering::Relaxed),
            issues: live.issues,
            error: live.error,
            last_synced_s: live.last_synced_s,
            channel_id,
            on_demand: f.on_demand,
            on_demand_others,
            unavailable: live.unavailable,
            progress,
            problem,
            join_attempts,
            waiting,
            gone: away.removable(),
            reshare,
            scan: live.scan.filter(|_| scanning),
        }
    }

    /// A folder's or channel's join, for its status: what it does now and its last problem while joining (a refusal
    /// is its problem), and the join attempts this device refused in it.
    pub(crate) fn join_status(&self, f: &Space) -> (Option<String>, Option<String>, Vec<JoinAttemptStatus>) {
        let attempts = lock(&f.notes).attempts.iter().map(JoinAttemptStatus::from).collect();
        if !f.joining(self.inner.ctx.key.public_key()) {
            return (None, None, attempts);
        }
        let reach = lock(&f.invite).as_ref().and_then(|l| l.reach.clone());
        let n = lock(&f.notes).clone();
        if let Some(why) = n.refused {
            return (None, Some(why), attempts);
        }
        // From the user's own devices: waiting for one of them that can admit this device.
        if f.following.load(Ordering::SeqCst) {
            let by = crate::devices::followed_by(&self.inner.ctx, f);
            return (Some(crate::joining::from_your_devices(&lock(&f.name), by.as_deref())), n.join.problem, attempts);
        }
        let addrs: Vec<SocketAddr> = reach.iter().flat_map(|r| r.addresses()).collect();
        let dht = self.inner.discovery.is_some() && lock(&self.inner.cfg).settings.dht_discovery;
        let ways = crate::joining::Ways { link: !addrs.is_empty(), lan: self.inner.lan_runs, internet: dht && !f.lan_only.load(Ordering::Relaxed) };
        let progress = crate::joining::progress(crate::roles::noun(f), reach.as_ref().map(|r| r.name.as_str()), &addrs, &n.join, ways);
        (Some(progress), n.join.problem, attempts)
    }

    /// A disk was mounted or unmounted (the app says so; a daemon tries again on [`Options::held_retry`]): a folder held
    /// back by a move cut off is tried again, so plugging the disk with its original files in finishes or undoes the
    /// move, and the folder starts, without a restart.
    pub async fn volumes_changed(&self) {
        let _one = self.inner.held_retrying.lock().await;
        let held: Vec<FolderId> = lock(&self.inner.held).iter().filter(|(_, h)| h.waiting).map(|(i, _)| *i).collect();
        for id in held {
            // Removed meanwhile (the user stopped syncing it): nothing to finish.
            if !lock(&self.inner.held).contains_key(&id) || lock(&self.inner.cfg).folder(&id.to_string()).is_none() {
                continue;
            }
            let j: Option<Journal> = std::fs::read(self.journal_path(&id)).ok().and_then(|b| serde_json::from_slice(&b).ok());
            match self.recover(&id, j).await {
                Ok(switched) => {
                    self.drop_journal(&id);
                    lock(&self.inner.held).remove(&id);
                    // Finished or undone: the folder starts where its entry says (a removal forgot it).
                    let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned();
                    if let Some(e) = entry
                        && let Err(err) = self.open_folder(&e).await
                    {
                        eprintln!("seedwell sync: folder {id}: {err}");
                    }
                    if switched {
                        self.follow_mode_changed(&id).await;
                    }
                }
                Err(err) => {
                    let held = self.held_of(&id, &err);
                    lock(&self.inner.held).insert(id, held);
                }
            }
        }
    }

    /// A held-back folder is not moved, renamed or switched: its unfinished move comes first.
    pub(crate) fn refuse_held(&self, id: &FolderId) -> Result<(), SyncError> {
        match lock(&self.inner.held).get(id) {
            Some(h) if h.waiting => Err(invalid("the folder is waiting for the disk with its original files (an unfinished move): plug it in first, or remove the folder")),
            Some(h) => Err(invalid(format!("{}: remove the folder, or start Seedwell again", h.why))),
            None => Ok(()),
        }
    }

    /// A folder held back ([`Inner::held`]) as the UI shows it: missing, with why, from its entry.
    fn held_status(&self, id: &FolderId, held: &Held) -> Option<FolderStatus> {
        let e = lock(&self.inner.cfg).folder(&id.to_string()).cloned()?;
        let role = held.role;
        Some(FolderStatus {
            id: e.id.clone(),
            name: e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            path: e.path.display().to_string(),
            access: role.map_or(Access::Read, Role::access),
            role,
            share_until: None,
            state: if held.waiting { FolderState::Missing } else { FolderState::Error },
            files: 0,
            bytes: 0,
            need_files: 0,
            need_bytes: 0,
            rate_in: 0,
            rate_out: 0,
            connected: 0,
            members: 0,
            conflicts: 0,
            lan_only: e.lan_only,
            issues: Vec::new(),
            error: Some(held.why.clone()),
            last_synced_s: None,
            channel_id: None,
            on_demand: e.on_demand,
            on_demand_others: e.on_demand_others && !e.on_demand,
            unavailable: 0,
            progress: None,
            problem: None,
            join_attempts: Vec::new(),
            waiting: None,
            gone: Vec::new(),
            reshare: false,
            scan: None,
        })
    }

    /// Everything, for the UI.
    pub fn snapshot(&self) -> Snapshot {
        let held: Vec<(FolderId, Held)> = lock(&self.inner.held).iter().map(|(i, w)| (*i, w.clone())).collect();
        let folders = self
            .inner
            .ctx
            .registry
            .folders()
            .iter()
            .map(|f| self.status_of(f))
            .chain(held.iter().filter_map(|(id, why)| self.held_status(id, why)))
            .collect();
        let devices = self
            .inner
            .ctx
            .registry
            .peers()
            .iter()
            .map(|p| DeviceStatus {
                id: p.device.to_string(),
                name: p.name.clone(),
                client: p.client.clone(),
                address: p.remote.to_string(),
                via: if p.via == Via::Relay {
                    "Relay".into()
                } else if p.lan {
                    "LAN".into()
                } else {
                    "Internet".into()
                },
                folders: p
                    .folders
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .keys()
                    .filter(|f| self.inner.ctx.registry.folder(f).is_some())
                    .map(|f| f.to_string())
                    .collect(),
                rx_bytes: p.rx_bytes.load(Ordering::Relaxed),
                tx_bytes: p.tx_bytes.load(Ordering::Relaxed),
            })
            .collect();
        Snapshot {
            device_id: self.inner.ctx.me.to_string(),
            device_name: lock(&self.inner.ctx.device_name).clone(),
            port: self.inner.port,
            folders,
            devices,
            offline: self.offline_devices(),
        }
    }

    /// A folder's or channel's members other than this device: which are connected for it, and for the others when
    /// each was last seen and why the last attempt to reach it failed (from its own records).
    pub(crate) fn away(&self, f: &Space) -> crate::absence::Away {
        let me = self.inner.ctx.me;
        let my_pk = *self.inner.ctx.key.public_key();
        let connected: BTreeSet<DeviceId> = self.inner.ctx.registry.peers().iter().filter(|p| p.access(&f.id).is_some()).map(|p| p.device).collect();
        let (others, may_remove): (Vec<(DeviceId, Role)>, bool) = {
            let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
            let others = m.members().map(|(pk, mem)| (DeviceId::of_public_key(pk), mem.role)).filter(|(d, _)| *d != me).collect();
            (others, m.role(&my_pk) == Some(Role::Owner))
        };
        let may_remove = may_remove && f.secret.read().unwrap_or_else(|e| e.into_inner()).is_some();
        let kept: Vec<crate::config::SpaceDevice> = {
            let cfg = lock(&self.inner.cfg);
            let id = f.id.to_string();
            cfg.folders.iter().filter(|e| e.id == id).flat_map(|e| e.devices.clone()).chain(cfg.channels.iter().filter(|e| e.id == id).flat_map(|e| e.devices.clone())).collect()
        };
        let names: BTreeMap<DeviceId, Names> = f.profile_names().into_iter().collect();
        let unreachable = lock(&f.notes).unreachable.clone();
        let mut away = crate::absence::Away {
            now: crate::roles::now_s(),
            gone_after_s: self.inner.gone_after_s.load(Ordering::Relaxed),
            may_remove,
            ..Default::default()
        };
        for (d, role) in others {
            if connected.contains(&d) {
                away.connected += 1;
                away.admitter_connected |= role.admits();
                continue;
            }
            let entry = kept.iter().find(|e| e.id == d.to_string());
            let name = names.get(&d).map(|n| n.device.clone()).filter(|n| !n.is_empty()).or_else(|| entry.map(|e| e.name.clone())).unwrap_or_default();
            let last_seen_s = entry.map(|e| e.last_seen).filter(|s| *s > 0);
            let problem = unreachable.get(&d).map(|(_, why)| why.clone());
            away.absent.push((crate::absence::Absent { id: d, name, last_seen_s, problem }, role));
        }
        away
    }

    /// The devices of the folders and channels here (members, and devices met in them) that are not connected: when
    /// each was last seen and why the last attempt to reach it failed, from each folder's and channel's own records.
    fn offline_devices(&self) -> Vec<OfflineDevice> {
        let me = self.inner.ctx.me;
        let connected: BTreeSet<DeviceId> = self.inner.ctx.registry.peers().iter().map(|p| p.device).collect();
        let mut out: BTreeMap<DeviceId, OfflineDevice> = BTreeMap::new();
        // Each folder's and channel's own device records (its entry in `sync.toml`).
        let kept: BTreeMap<String, Vec<crate::config::SpaceDevice>> = {
            let cfg = lock(&self.inner.cfg);
            cfg.folders.iter().map(|e| (e.id.clone(), e.devices.clone())).chain(cfg.channels.iter().map(|e| (e.id.clone(), e.devices.clone()))).collect()
        };
        // (Not the devices group's: its devices are on Settings › Devices, and agents never see them.)
        for f in self.inner.ctx.registry.spaces().into_iter().filter(|f| f.kind != SpaceKind::Devices) {
            let mut devices: BTreeSet<DeviceId> = f.members.read().unwrap_or_else(|e| e.into_inner()).iter().copied().collect();
            devices.extend(f.membership.read().unwrap_or_else(|e| e.into_inner()).members().map(|(pk, _)| DeviceId::of_public_key(pk)));
            let names: BTreeMap<DeviceId, Names> = f.profile_names().into_iter().collect();
            let kept = kept.get(&f.id.to_string());
            let unreachable = lock(&f.notes).unreachable.clone();
            for d in devices.into_iter().filter(|d| *d != me && !connected.contains(d)) {
                let entry = kept.and_then(|k| k.iter().find(|e| e.id == d.to_string()));
                let o = out.entry(d).or_insert_with(|| OfflineDevice { id: d.to_string(), name: String::new(), last_seen_s: None, problem: None, folders: Vec::new() });
                if o.name.is_empty() {
                    o.name = names.get(&d).map(|n| n.device.clone()).filter(|n| !n.is_empty()).or_else(|| entry.map(|e| e.name.clone())).unwrap_or_default();
                }
                if let Some(seen) = entry.map(|e| e.last_seen).filter(|s| *s > 0) {
                    o.last_seen_s = Some(o.last_seen_s.map_or(seen, |s| s.max(seen)));
                }
                if let Some((_, why)) = unreachable.get(&d) {
                    o.problem = Some(why.clone());
                }
                o.folders.push(f.id.to_string());
            }
        }
        out.into_values().collect()
    }

    /// A session with `device` began or ended now: each folder's and channel's record of it says so.
    fn seen_now(&self, device: DeviceId) {
        let id = device.to_string();
        let now = crate::roles::now_s();
        let mut cfg = lock(&self.inner.cfg);
        let mut changed = false;
        for d in cfg.all_devices_mut().filter(|d| d.id == id) {
            d.last_seen = now;
            changed = true;
        }
        drop(cfg);
        if changed {
            let _ = self.save_config();
        }
    }

    /// Remembers where a device can be reached (LAN discovery, peer exchange) and dials it if needed.
    pub(crate) fn learn_address(&self, device: Option<DeviceId>, folder: Option<FolderId>, addr: SocketAddr) {
        if device == Some(self.inner.ctx.me) {
            return;
        }
        // A device found for a folder (LAN discovery: its tag needs the folder's key) is one of its members,
        // so the dial loop wants it (and offers it the folder). Before the book's lock: `open_folder` takes
        // the members' lock first.
        if let (Some(d), Some(f)) = (device, folder.and_then(|f| self.inner.ctx.registry.space(&f))) {
            f.members.write().unwrap_or_else(|e| e.into_inner()).insert(d);
        }
        let mut book = lock(&self.inner.book);
        match (device, folder) {
            (Some(d), _) => {
                book.by_device.entry(d).or_default().insert(addr);
            }
            (None, Some(f)) => {
                book.hints.entry(f).or_default().insert(addr);
            }
            (None, None) => return,
        }
        drop(book);
        self.inner.wake_dialer.notify_one();
    }

    /// Forgets the dial backoff of `addr` (LAN discovery saw a device start, or met it for the first time).
    pub(crate) fn retry_now(&self, addr: SocketAddr) {
        lock(&self.inner.book).tried.remove(&addr);
    }

    /// What LAN discovery needs.
    pub(crate) fn lan(&self) -> &crate::lan::Lan {
        &self.inner.lan
    }

    // ----- what relays and hole punching need (crate::relay, crate::punch, crate::relay_server) -----

    /// The session context.
    pub(crate) fn ctx(&self) -> &Arc<Ctx> {
        &self.inner.ctx
    }

    /// Relay and hole-punch state.
    pub(crate) fn net(&self) -> &crate::relay::Net {
        &self.inner.net
    }

    /// Our uTP socket, if uTP runs.
    pub(crate) fn utp(&self) -> Option<&trs_utp::UtpSocket> {
        self.inner.utp.as_ref()
    }

    /// The service is stopping.
    pub(crate) fn is_stopping(&self) -> bool {
        self.inner.stopping.load(Ordering::Relaxed)
    }

    /// Wakes the dial loop.
    pub(crate) fn wake_dialer(&self) {
        self.inner.wake_dialer.notify_one();
    }

    /// Runs a task until the service stops.
    pub(crate) fn spawn_task(&self, f: impl Future<Output = ()> + Send + 'static) {
        let t = tokio::spawn(f);
        if self.is_stopping() {
            t.abort();
        } else {
            lock(&self.inner.tasks).push(t);
        }
    }

    /// The folders to prove when dialing `device`.
    pub(crate) fn candidates(&self, device: DeviceId) -> Vec<FolderId> {
        candidates_for(&self.inner.ctx, Some(device), None)
    }

    /// Runs a session that finished its handshake (dialed, accepted, punched or relayed). A direct one with
    /// a device the test switch refuses is dropped.
    pub(crate) async fn run_session(&self, conn: session::Connected, candidates: Vec<FolderId>) {
        self.run_session_then(conn, candidates, || {}).await;
    }

    /// [`Self::run_session`], `on_up` run once the session is up (not for one turned down in its setup).
    async fn run_session_then(&self, conn: session::Connected, candidates: Vec<FolderId>, on_up: impl FnOnce() + Send) {
        if self.is_stopping() || (conn.via != Via::Relay && self.is_blocked(conn.device())) {
            return;
        }
        let _ = crate::peer::run(self.inner.ctx.clone(), conn, candidates, on_up).await;
    }

    /// Direct sessions with `device` are refused (the test switch).
    pub(crate) fn is_blocked(&self, device: DeviceId) -> bool {
        self.inner.ctx.registry.refuses_direct(&device)
    }

    /// Direct dials to `device` are not getting through: it has no known address, or each was tried and
    /// has not connected since.
    pub(crate) fn direct_failing(&self, device: DeviceId) -> bool {
        if self.is_blocked(device) {
            return true;
        }
        let book = lock(&self.inner.book);
        let failing = |a: &SocketAddr| book.tried.get(a).is_some_and(|t| t.1 > 0) && !book.in_setup.contains(a);
        book.by_device.get(&device).is_none_or(|addrs| addrs.iter().all(failing))
    }

    /// The addresses `device`'s signed record lists.
    pub(crate) fn record_addrs(&self, device: DeviceId) -> Vec<String> {
        lock(&self.inner.book).records.get(&device).map(|r| r.addrs.iter().filter(|a| !crate::rendezvous::is_entry(a)).cloned().collect()).unwrap_or_default()
    }

    /// Every relay server (`relay://…`) in some folder's or channel's addresses.
    pub(crate) fn relay_urls(&self) -> Vec<String> {
        let cfg = lock(&self.inner.cfg);
        let mut out: Vec<String> = Vec::new();
        for a in cfg.folders.iter().flat_map(|f| f.addresses.iter()).chain(cfg.channels.iter().flat_map(|c| c.addresses.iter())) {
            if crate::relay_server::is_relay_url(a) && !out.contains(a) {
                out.push(a.clone());
            }
        }
        out
    }

    /// The relay servers `device` may be registered at: those of the folders and channels it is a member of, and
    /// the one its signed address record names.
    pub(crate) fn relays_for(&self, device: DeviceId) -> Vec<String> {
        let ids: BTreeSet<String> = self
            .inner
            .ctx
            .registry
            .spaces()
            .iter()
            .filter(|f| f.members.read().unwrap_or_else(|e| e.into_inner()).contains(&device))
            .map(|f| f.id.to_string())
            .collect();
        let mut out: Vec<String> = Vec::new();
        {
            let cfg = lock(&self.inner.cfg);
            let folders = cfg.folders.iter().filter(|f| ids.contains(&f.id) && !f.lan_only).flat_map(|f| f.addresses.iter());
            let channels = cfg.channels.iter().filter(|c| ids.contains(&c.id)).flat_map(|c| c.addresses.iter());
            for a in folders.chain(channels) {
                if crate::relay_server::is_relay_url(a) && !out.contains(a) {
                    out.push(a.clone());
                }
            }
        }
        if let Some(r) = lock(&self.inner.book).records.get(&device).map(|r| r.relay.clone())
            && crate::relay_server::is_relay_url(&r)
            && !out.contains(&r)
        {
            out.push(r);
        }
        out
    }

    /// Adds a relay server (`relay://host:port#<relay device id>`) to a folder's (or channel's) addresses: this
    /// device keeps a registration there, and the members are also tried through it.
    pub fn add_relay(&self, id: &FolderId, url: &str) -> Result<(), SyncError> {
        self.inner.ctx.registry.user_space(id).ok_or(SyncError::NotFound)?;
        let url = url.trim();
        if !crate::relay_server::is_relay_url(url) {
            return Err(invalid("not a relay address (relay://host:port#<relay device id>)"));
        }
        {
            let mut cfg = lock(&self.inner.cfg);
            if let Some(e) = cfg.folder_mut(&id.to_string())
                && !e.addresses.iter().any(|a| a == url)
            {
                e.addresses.push(url.to_string());
            }
            if let Some(e) = cfg.channel_mut(&id.to_string())
                && !e.addresses.iter().any(|a| a == url)
            {
                e.addresses.push(url.to_string());
            }
        }
        self.save_config()?;
        crate::relay_server::ensure_registered(self, url);
        self.inner.wake_dialer.notify_one();
        Ok(())
    }

    /// What this device relays, for diagnostics.
    pub fn relay_stats(&self) -> crate::relay::RelayStats {
        self.inner.net.stats()
    }

    /// What each connected device's session runs over (`TCP`, `uTP` or `Relay`).
    pub fn transports(&self) -> Vec<(DeviceId, Via)> {
        self.inner.ctx.registry.peers().iter().map(|p| (p.device, p.via)).collect()
    }

    /// **Tests only:** something that holds the folder (as a scan in progress does) until it is dropped.
    #[doc(hidden)]
    pub fn test_hold_folder(&self, id: &FolderId) -> Option<Box<dyn std::any::Any + Send + Sync>> {
        self.inner.ctx.registry.folder(id).map(|f| Box::new(f) as Box<dyn std::any::Any + Send + Sync>)
    }

    /// **Tests only:** how long a device asked whether it holds files may take to answer.
    #[doc(hidden)]
    pub fn test_hold_wait(&self, wait: Duration) {
        *lock(&self.inner.ctx.hold_wait) = wait;
    }

    /// **Tests only:** each scan step is followed by a pause this long, the folder's core let go (a long first scan).
    #[doc(hidden)]
    pub fn test_scan_step_wait(&self, wait: Duration) {
        *lock(&self.inner.ctx.scan_step_wait) = wait;
    }

    /// **Tests only:** how long sessions may take to come up before a fetch whose file no connected device has gives
    /// up ([`FETCH_GRACE`], counted again from now), and how long a fetch may get no byte further ([`FETCH_STALL`]).
    #[doc(hidden)]
    pub fn test_fetch_times(&self, grace: Duration, stall: Duration) {
        *lock(&self.inner.ctx.fetch_grace) = grace;
        *lock(&self.inner.ctx.fetch_stall) = stall;
        *lock(&self.inner.ctx.settling) = std::time::Instant::now() + grace;
    }

    /// **Tests only:** each batch folder `id`'s index sessions announce starts with a pause this long (a slow link: a
    /// session is still at it when the next change comes).
    #[doc(hidden)]
    pub fn test_announce_pause(&self, id: &FolderId, pause: Duration) {
        if let Some(f) = self.inner.ctx.registry.folder(id) {
            *lock(&f.announce_pause) = pause;
        }
    }

    /// **Tests only:** each host's write to folder `id` (`od_write`) holds the folder's core this much longer once the
    /// content is in place, before it is read in: a big file whose hashing takes longer than the save-by-replace window.
    #[doc(hidden)]
    pub fn test_write_wait(&self, id: &FolderId, wait: Duration) {
        if let Some(f) = self.inner.ctx.registry.folder(id) {
            *lock(&f.write_wait) = wait;
        }
    }

    /// **Tests only:** folder `id`'s save-by-replace window ([`trs_sync::ondemand::REPLACE_WINDOW`]) and the longest a
    /// write keeps a delete waiting ([`trs_sync::ondemand::WRITE_HOLD_MAX`]).
    #[doc(hidden)]
    pub fn test_replace_window(&self, id: &FolderId, window: Duration, write_max: Duration) {
        if let Some(f) = self.inner.ctx.registry.folder(id) {
            f.replacing.set_window(window);
            f.replacing.set_write_max(write_max);
        }
    }

    /// **Tests only:** each fetch of a pull starts with a pause this long, the folder's core held (a round that takes
    /// long, as on a slow disk).
    #[doc(hidden)]
    pub fn test_fetch_wait(&self, wait: Duration) {
        *lock(&self.inner.ctx.fetch_wait) = wait;
    }

    /// **Tests only:** each scan step holds the folder's core this much longer (a slow disk under a scan): what the hosts
    /// and the app read meanwhile must not wait for it.
    #[doc(hidden)]
    pub fn test_scan_hold(&self, hold: Duration) {
        *lock(&self.inner.ctx.scan_hold) = hold;
    }

    /// **Tests only:** this device lets go of every read of its unhashed files it keeps for others (as a restart does):
    /// a device asking for the next piece is answered busy, and starts again.
    #[doc(hidden)]
    pub fn test_forget_streams(&self) {
        for f in self.inner.ctx.registry.folders() {
            lock(&f.streams).clear();
        }
    }

    /// **Tests only:** each chunk this device serves is answered after a pause this long (a slow link: a fetch that
    /// stands on a chunk).
    #[doc(hidden)]
    pub fn test_serve_wait(&self, wait: Duration) {
        *lock(&self.inner.ctx.serve_wait) = wait;
    }

    /// **Tests only:** each piece of an unhashed file this device serves (SPEC §7G.11) is read after a pause this long:
    /// a first download that takes long, as of a big file over a slow link.
    #[doc(hidden)]
    pub fn test_stream_wait(&self, wait: Duration) {
        *lock(&self.inner.ctx.stream_wait) = wait;
    }

    /// **Tests only:** the speed limits apply to LAN sessions too, as [`crate::limit::QA_LIMIT_LAN`] makes them in an
    /// isolated run (without changing the process's environment).
    #[doc(hidden)]
    pub fn test_limit_lan(&self, on: bool) {
        self.inner.ctx.limit_lan.store(on, Ordering::Relaxed);
    }

    /// **Tests only:** files of a folder with a free in progress here.
    #[doc(hidden)]
    pub fn test_freeing(&self, id: &FolderId) -> usize {
        self.inner.ctx.registry.folder(id).map_or(0, |f| lock(&f.freeing).len())
    }

    /// **Tests only:** holds the folder's core (as a long pull does) for `d`, on a thread of its own.
    #[doc(hidden)]
    pub fn test_lock_core(&self, id: &FolderId, d: Duration) {
        if let Some(f) = self.inner.ctx.registry.folder(id) {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _core = lock(&f.core);
                let _ = tx.send(());
                std::thread::sleep(d);
            });
            let _ = rx.recv();
        }
    }

    /// **Tests only:** hold queries of ours still waiting for an answer, over every session.
    #[doc(hidden)]
    pub fn test_holds_waiting(&self) -> usize {
        self.inner.ctx.registry.peers().iter().map(|p| p.holds_waiting()).sum()
    }

    /// **Tests only:** rename requests of ours still waiting for an answer, over every session.
    #[doc(hidden)]
    pub fn test_renames_waiting(&self) -> usize {
        self.inner.ctx.registry.peers().iter().map(|p| p.renames_waiting()).sum()
    }

    /// **Tests only:** leave hold queries and rename requests unanswered, as an earlier Seedwell does (it reads them as
    /// message types it does not know, and skips them).
    #[doc(hidden)]
    pub fn test_silent_ondemand(&self, on: bool) {
        self.inner.ctx.silent_ondemand.store(on, Ordering::SeqCst);
    }

    /// **Tests only:** answer every hold query yes, whatever is held (a member that lies).
    #[doc(hidden)]
    pub fn test_yes_to_holds(&self, on: bool) {
        self.inner.ctx.yes_to_holds.store(on, Ordering::SeqCst);
    }

    /// **Tests only:** how many sessions this device has set up so far.
    #[doc(hidden)]
    pub fn test_sessions(&self) -> u64 {
        self.inner.ctx.sessions.load(Ordering::SeqCst)
    }

    /// **Tests only:** how many direct dials this device made so far.
    #[doc(hidden)]
    pub fn test_dials_made(&self) -> u64 {
        self.inner.dials_made.load(Ordering::SeqCst)
    }

    /// **Tests only:** this device makes at most `n` more direct dials (`None`: as many as it wants).
    #[doc(hidden)]
    pub fn test_dials_left(&self, n: Option<u32>) {
        *lock(&self.inner.dials_left) = n;
        self.inner.wake_dialer.notify_one();
    }

    /// **Tests only:** an answering session waits this long after making its Join (the session is not up yet).
    #[doc(hidden)]
    pub fn test_setup_wait(&self, wait: Duration) {
        *lock(&self.inner.ctx.setup_wait) = wait;
    }

    /// **Tests only:** how many answering sessions wait in [`Self::test_setup_wait`]'s wait now.
    #[doc(hidden)]
    pub fn test_setups_waiting(&self) -> usize {
        self.inner.ctx.setups_waiting.load(Ordering::SeqCst)
    }

    /// **Tests only:** how long stopping a folder waits for it to let go.
    #[doc(hidden)]
    pub fn test_stop_wait(&self, wait: Duration) {
        *lock(&self.inner.stop_wait) = wait;
    }

    /// **Tests only:** a member not connected counts as gone after this long (a day otherwise).
    #[doc(hidden)]
    pub fn test_gone_after(&self, after: Duration) {
        self.inner.gone_after_s.store(after.as_secs(), Ordering::Relaxed);
    }

    /// **Tests only:** a moved folder stays closed this long before it opens again, and a resumed one waits this long
    /// before its stale sessions start anew (what peers send meanwhile is not taken, and comes again).
    #[doc(hidden)]
    pub fn test_reopen_wait(&self, wait: Duration) {
        *lock(&self.inner.reopen_wait) = wait;
    }

    /// **Tests only:** from now on, neither dial nor accept direct (TCP or uTP) sessions with `device`, and
    /// close the direct one that runs; relayed sessions still work. This is how tests make two devices on
    /// one loopback "unable to reach each other".
    #[doc(hidden)]
    pub fn test_refuse_direct(&self, device: DeviceId) {
        self.inner.ctx.registry.refuse_direct(device);
        if let Some(p) = self.inner.ctx.registry.peer(&device)
            && p.via != Via::Relay
        {
            p.close();
        }
    }

    /// Where this device can be reached now, for a link it makes: its addresses on its networks (those the sync port
    /// listens on: IPv4 for the usual all-interfaces IPv4 socket; only that address for one bound to a single address),
    /// and its public address when the port mapping knows it.
    pub(crate) fn own_reach(&self) -> LinkReach {
        let bind = self.inner.bind;
        let ips: Vec<IpAddr> = if bind.is_unspecified() {
            trs_platform::net::lan_addresses().into_iter().filter(|ip| ip.is_ipv4() || bind.is_ipv6()).collect()
        } else {
            vec![bind]
        };
        let lan = ips.into_iter().take(trs_sync_proto::reach::MAX_LAN).map(|ip| SocketAddr::new(ip, self.inner.port)).collect();
        let public = if bind.is_unspecified() { self.inner.discovery.as_ref().and_then(|d| d.public_address()) } else { None };
        let rendezvous = Some(self.rendezvous_url()).filter(|u| !u.is_empty());
        LinkReach { device: self.inner.ctx.me, name: lock(&self.inner.ctx.device_name).clone(), lan, public, rendezvous }
    }

    /// A joining folder's or channel's link says where the device that made it was: that device is one to dial, at
    /// those addresses (besides LAN discovery and the DHT), expecting that device there.
    pub(crate) fn learn_from_link(&self, space: &Space) {
        let Some(r) = lock(&space.invite).as_ref().and_then(|l| l.reach.clone()) else { return };
        if let Some(url) = r.rendezvous.as_deref().filter(|_| r.device != self.inner.ctx.me) {
            // Where the maker publishes its addresses: it is looked up there when the ones below do not answer.
            space.members.write().unwrap_or_else(|e| e.into_inner()).insert(r.device);
            crate::rendezvous::learn_url(self, r.device, url);
            self.inner.wake_dialer.notify_one();
        }
        if r.device == self.inner.ctx.me || r.addresses().next().is_none() {
            return;
        }
        space.members.write().unwrap_or_else(|e| e.into_inner()).insert(r.device);
        {
            // A link just put to use (joined, or its folder opened again): its addresses are dialed at once, whatever
            // an earlier dial there met (a link used up or revoked, the folder closed there).
            let mut book = lock(&self.inner.book);
            book.by_device.entry(r.device).or_default().extend(r.addresses());
            for a in r.addresses() {
                book.tried.remove(&a);
                book.refused.remove(&a);
            }
        }
        self.inner.wake_dialer.notify_one();
    }

    /// Our own signed address record: our port at the address a peer reached us on (`local`).
    fn own_record(&self, local: SocketAddr) -> AddressRecord {
        let mut addrs = vec![SocketAddr::new(local.ip(), self.inner.port).to_string()];
        // Where this device publishes its addresses (older versions skip the entry: it is not an address).
        addrs.extend(crate::rendezvous::record_entry(self));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        // Where we keep a registration: others reach us through it.
        let relay = self.relay_urls().into_iter().next().unwrap_or_default();
        AddressRecord::sign(&self.inner.ctx.key, addrs, relay, now)
    }

    /// Remembers a device met in a folder or channel (`folder`: it was met there now), and its name and an address
    /// in every folder and channel it was met in: in their own entries in `sync.toml`, nowhere else.
    fn remember_device(&self, device: DeviceId, name: Option<&str>, folder: Option<FolderId>, addr: Option<SocketAddr>) {
        let id = device.to_string();
        let mut cfg = lock(&self.inner.cfg);
        let mut changed = false;
        if let Some(list) = folder.and_then(|f| cfg.devices_of_mut(&f.to_string()))
            && !list.iter().any(|d| d.id == id)
        {
            // Met there now: with the name and addresses it has in this session.
            let name = self.inner.ctx.registry.peer(&device).map(|p| p.name.clone()).unwrap_or_default();
            let mut addresses: Vec<String> = lock(&self.inner.book).by_device.get(&device).into_iter().flatten().map(|a| a.to_string()).collect();
            addresses.truncate(8);
            list.push(crate::config::SpaceDevice { id: id.clone(), name, addresses, last_seen: crate::roles::now_s(), rendezvous: String::new() });
            changed = true;
        }
        for d in cfg.all_devices_mut().filter(|d| d.id == id) {
            if let Some(n) = name.filter(|n| !n.is_empty() && *n != d.name) {
                d.name = n.to_string();
                changed = true;
            }
            if let Some(a) = addr.map(|a| a.to_string()).filter(|a| d.addresses.first() != Some(a)) {
                d.addresses.retain(|x| *x != a);
                d.addresses.insert(0, a);
                d.addresses.truncate(8);
                changed = true;
            }
        }
        drop(cfg);
        if self.inner.ctx.direct.note(&device, name, None, addr) {
            let _ = self.save_conversations();
        }
        if changed {
            let _ = self.save_config();
        }
    }

    /// After a folder, channel or conversation is gone from this device: what it knew of devices that share no
    /// other one with it goes too (their addresses and address records), so nothing about a device outlives every
    /// place it was met in.
    pub(crate) fn forget_strangers(&self) {
        let known = self.known_devices();
        let mut book = lock(&self.inner.book);
        book.by_device.retain(|d, _| known.contains(d));
        book.records.retain(|d, _| known.contains(d));
    }

    /// Every device in a folder, channel or conversation on this device: a member of it, or met there.
    fn known_devices(&self) -> BTreeSet<DeviceId> {
        let mut out: BTreeSet<DeviceId> = self.inner.ctx.direct.devices().into_iter().map(|(d, _)| d).collect();
        for f in self.inner.ctx.registry.spaces() {
            out.extend(f.membership.read().unwrap_or_else(|e| e.into_inner()).members().map(|(pk, _)| DeviceId::of_public_key(pk)));
        }
        for f in self.inner.ctx.registry.folders() {
            out.extend(f.members.read().unwrap_or_else(|e| e.into_inner()).iter().copied());
        }
        for c in self.inner.ctx.registry.channels() {
            out.extend(c.members.read().unwrap_or_else(|e| e.into_inner()).iter().copied());
        }
        out
    }

    /// Dials `addr` (for a known `device`, or for the folders in `candidates`), unless its backoff holds it back:
    /// then when it may be dialed again.
    fn dial(&self, addr: SocketAddr, device: Option<DeviceId>, candidates: Vec<FolderId>) -> Option<Instant> {
        if let Some(left) = lock(&self.inner.dials_left).as_mut() {
            if *left == 0 {
                return None;
            }
            *left -= 1;
        }
        {
            let mut book = lock(&self.inner.book);
            // Connected, its session being set up: the end of the setup wakes the dial loop.
            if book.in_setup.contains(&addr) {
                return None;
            }
            let now = Instant::now();
            if let Some(due) = held_until(&book, addr, now) {
                return Some(due);
            }
            let fails = book.tried.get(&addr).map_or(0, |t| t.1);
            book.tried.insert(addr, (now, fails + 1));
        }
        self.inner.dials_made.fetch_add(1, Ordering::SeqCst);
        let h = self.clone();
        tokio::spawn(async move {
            // A failed dial is retried with backoff by the dial loop.
            if device.is_some_and(|d| h.is_blocked(d)) {
                return;
            }
            // The folders and channels this dial is for that are joining show it while it runs.
            let joining = h.joining_spaces(&candidates);
            for f in &joining {
                lock(&f.notes).join.trying = Some(addr);
            }
            let plan = session::DialPlan { utp: h.inner.utp.as_ref(), tcp: h.inner.dial_tcp };
            let dialed = session::dial(&h.inner.ctx.tls, plan, addr, device).await;
            for f in &joining {
                let mut n = lock(&f.notes);
                if n.join.trying == Some(addr) {
                    n.join.trying = None;
                }
            }
            match dialed {
                Ok(conn) => {
                    if h.is_blocked(conn.device()) {
                        return;
                    }
                    let dev = conn.device();
                    // Marked before anything wakes the dial loop (learning the address does): while the session is
                    // set up the address is neither dialed again nor failing.
                    lock(&h.inner.book).in_setup.insert(addr);
                    h.learn_address(Some(dev), None, addr);
                    h.remember_device(dev, None, None, Some(addr));
                    // The backoff goes once the session is up: one that ends after that is dialed again at once. A
                    // session turned down in its setup (nothing in common, a refused link, the device stopping) waits
                    // as a dial that did not connect does, at most [`REFUSED_WAIT_MAX`]; it was dialed again at once,
                    // forever (hundreds of times a second, each successful dial waking the dial loop).
                    let up = Arc::new(AtomicBool::new(false));
                    let (h2, up2) = (h.clone(), up.clone());
                    h.run_session_then(conn, candidates, move || {
                        let mut book = lock(&h2.inner.book);
                        book.tried.remove(&addr);
                        book.refused.remove(&addr);
                        book.in_setup.remove(&addr);
                        up2.store(true, Ordering::SeqCst);
                    })
                    .await;
                    {
                        let mut book = lock(&h.inner.book);
                        book.in_setup.remove(&addr);
                        // Turned down as a second session with a device that is connected: no backoff for it either.
                        if up.load(Ordering::SeqCst) || h.inner.ctx.registry.peer(&dev).is_some() {
                            book.tried.remove(&addr);
                            book.refused.remove(&addr);
                        } else {
                            book.refused.insert(addr);
                        }
                    }
                    // The dial loop skipped the address during the setup: it learns its wait now.
                    h.inner.wake_dialer.notify_one();
                }
                Err(e) => h.note_dial_failed(addr, device, &joining, &e),
            }
        });
        None
    }

    /// The spaces among `ids` that this device is joining.
    fn joining_spaces(&self, ids: &[FolderId]) -> Vec<Arc<Space>> {
        let me = self.inner.ctx.key.public_key();
        ids.iter().filter_map(|id| self.inner.ctx.registry.space(id)).filter(|f| f.joining(me)).collect()
    }

    /// A dial of `addr` (expecting `device`) failed: the joining spaces it was for say so, and the spaces `device`
    /// is in keep it as the last reason it could not be reached.
    fn note_dial_failed(&self, addr: SocketAddr, device: Option<DeviceId>, joining: &[Arc<Space>], e: &io::Error) {
        for f in joining {
            let maker = lock(&f.invite).as_ref().and_then(|l| l.reach.clone()).filter(|r| r.addresses().any(|a| a == addr)).map(|r| r.name);
            f.join_problem(crate::joining::dial_problem(maker.as_deref(), addr, e));
        }
        if let Some(d) = device {
            let why = crate::absence::reach_problem(addr, e);
            let now = crate::roles::now_s();
            for f in self.inner.ctx.registry.spaces() {
                if f.members.read().unwrap_or_else(|e| e.into_inner()).contains(&d) {
                    lock(&f.notes).unreachable.insert(d, (now, why.clone()));
                }
            }
        }
    }
}

/// The longest an address whose sessions were turned down in their setup waits: a refused dial costs only a
/// handshake, and what made the device refuse (a link used up, its folder paused or still opening) may be over at
/// any moment; over the internet only the joining device dials.
pub(crate) const REFUSED_WAIT_MAX: Duration = Duration::from_secs(30);

/// How long an address waits after `fails` dials that did not connect: a second at first (a device that is just
/// starting, a busy Mac), then longer, at most five minutes.
/// When `addr` may be dialed again, if not now.
pub(crate) fn held_until(book: &AddressBook, addr: SocketAddr, now: Instant) -> Option<Instant> {
    let (last, fails) = book.tried.get(&addr).copied()?;
    let wait = retry_after(fails);
    let due = last + if book.refused.contains(&addr) { wait.min(REFUSED_WAIT_MAX) } else { wait };
    (now < due).then_some(due)
}

fn retry_after(fails: u32) -> Duration {
    const STEPS: [u64; 8] = [0, 1, 2, 5, 10, 30, 60, 120];
    Duration::from_secs(STEPS.get(fails as usize).copied().unwrap_or(300))
}

/// The folders and channels shared with `device` as far as we know (it was seen as a member), and `hint`.
fn candidates_for(ctx: &Ctx, device: Option<DeviceId>, hint: Option<FolderId>) -> Vec<FolderId> {
    let mut out: Vec<FolderId> = ctx
        .registry
        .spaces()
        .iter()
        .filter(|f| device.is_some_and(|d| f.members.read().unwrap_or_else(|e| e.into_inner()).contains(&d)))
        .map(|f| f.id)
        .collect();
    if let Some(h) = hint
        && !out.contains(&h)
    {
        out.push(h);
    }
    out
}

fn absolute(p: &Path) -> io::Result<PathBuf> {
    if p.is_absolute() { Ok(p.to_path_buf()) } else { Ok(std::env::current_dir()?.join(p)) }
}

/// iCloud Drive and File Provider folders are refused: their files may be placeholders, and two sync
/// engines on one folder fight (SPEC §7G.2).
/// A folder an isolated run must not use: inside `forbidden` (the real home) and not in a temporary folder (Windows
/// keeps those in the user's profile, under the home it forbids).
pub fn inside_forbidden(p: &Path, forbidden: &Path) -> bool {
    !p.as_os_str().is_empty() && p.starts_with(forbidden) && !trs_platform::dirs::in_temp_folder(p)
}

impl SyncHandle {
    /// An isolated (test) run uses no folder inside its forbidden root.
    fn refuse_forbidden(&self, p: &Path) -> Result<(), SyncError> {
        match &self.inner.forbidden_root {
            Some(root) if inside_forbidden(p, root) => {
                Err(invalid(format!("an isolated (test) run uses no folder inside {}: {}", root.display(), p.display())))
            }
            _ => Ok(()),
        }
    }
}

/// Why a folder's files are not moved: the torrents (`names`, at least one) that use files in it, the first three
/// named. "Uses", not "seeds": a torrent still downloading there counts as much as one seeding.
fn in_use_text(names: &[String]) -> String {
    let shown: Vec<String> = names.iter().take(3).map(|n| format!("“{n}”")).collect();
    let more = if names.len() > 3 { format!(" and {} more", names.len() - 3) } else { String::new() };
    let (what, them) = if names.len() == 1 { ("a torrent uses".to_string(), "remove the torrent or move it elsewhere") } else { (format!("{} torrents use", names.len()), "remove the torrents or move them elsewhere") };
    format!("{what} files in this folder ({}{more}): its files stay where they are; {them} first", shown.join(", "))
}

fn refuse_cloud_root(p: &Path) -> Result<(), SyncError> {
    if trs_platform::dirs::in_cloud_storage(p) {
        return Err(invalid("folders in iCloud Drive or other cloud storage cannot be synced"));
    }
    Ok(())
}

fn load_ignore(fs: &StdFs, local: &[String]) -> Ignore {
    let synced = trs_sync::fs::read_all(fs, ".seedwellignore").ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
    let local = local.join("\n");
    Ignore::parse_all(&[&synced, &local]).0
}

/// A folder not opened because a change cut off could not be finished at the start.
#[derive(Clone, Debug)]
pub(crate) struct Held {
    /// For its row.
    why: String,
    /// The original's disk is not there: tried again when volumes change and on a timer. Otherwise (any other error)
    /// the folder is shown in error with the reason, and not tried again until the next start.
    waiting: bool,
    /// This device's role in it, read from its membership file when it was held back (it does not change while the
    /// folder is not open): the app's calls show it without a read of the disk.
    role: Option<Role>,
}

impl SyncHandle {
    /// Why folder `id` is held back, for its row: the move cut off waits for the original's disk, or failed otherwise.
    fn held_of(&self, id: &FolderId, err: &SyncError) -> Held {
        let waiting = matches!(err, SyncError::Io(e) if e.kind() == io::ErrorKind::NotFound);
        let why = if waiting { format!("Waiting for the disk with its original files ({err})") } else { format!("An interrupted move could not be finished: {err}") };
        Held { why, waiting, role: self.held_role(id) }
    }

    /// This device's role in folder `id`, which is not open: from its membership file (a read of the disk, so never on
    /// the app's calls: [`Held::role`] keeps it).
    fn held_role(&self, id: &FolderId) -> Option<Role> {
        let pk = lock(&self.inner.cfg).folder(&id.to_string()).and_then(|e| public_key_from_hex(&e.folder_pk))?;
        crate::roles::load(&self.members_path(id), pk).ok()?.role(self.inner.ctx.key.public_key())
    }
}

/// Tries the held-back folders again every `every` (a headless daemon hears of no mounts).
async fn held_loop(h: SyncHandle, every: Duration) {
    loop {
        tokio::time::sleep(every).await;
        if lock(&h.inner.held).values().any(|h| h.waiting) {
            h.volumes_changed().await;
        }
    }
}

/// A change of a folder's files noted before it starts (`<id>.journal` in the sync state), so one cut off is finished
/// or undone at the next start. `copied`: the files were copied to another volume and the copy is whole (from then on
/// the move is finished, never undone).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Journal {
    /// An on-demand folder is being removed: its files move from `from` to `to`, and end at `place` (or next to it).
    Remove {
        from: PathBuf,
        to: PathBuf,
        place: PathBuf,
        copied: bool,
        #[serde(default)]
        renamed: bool,
        /// `to` was not there when the journal was written: an empty folder left there is the move's own.
        #[serde(default)]
        made_to: bool,
        /// The move is over (the files are at `to`): what is at `from` now is not the folder's.
        #[serde(default)]
        done: bool,
    },
    /// A switch to `on` demand (true) or full: the files move from `from` to `to`; `place` is the folder's place after.
    Switch {
        on: bool,
        from: PathBuf,
        to: PathBuf,
        place: PathBuf,
        copied: bool,
        #[serde(default)]
        renamed: bool,
        /// `to` was not there when the journal was written: an empty folder left there is the move's own.
        #[serde(default)]
        made_to: bool,
        /// The move is over (the files are at `to`): what is at `from` now is not the folder's.
        #[serde(default)]
        done: bool,
    },
    /// A full folder moves from `from` to `to`.
    Move {
        from: PathBuf,
        to: PathBuf,
        copied: bool,
        #[serde(default)]
        renamed: bool,
        /// `to` was not there when the journal was written: an empty folder left there is the move's own.
        #[serde(default)]
        made_to: bool,
        /// The move is over (the files are at `to`): what is at `from` now is not the folder's.
        #[serde(default)]
        done: bool,
    },
}

impl Journal {
    fn paths(&self) -> (PathBuf, PathBuf) {
        match self {
            Journal::Remove { from, to, .. } | Journal::Switch { from, to, .. } | Journal::Move { from, to, .. } => (from.clone(), to.clone()),
        }
    }

    /// The same, done: its copy whole, or (`by_rename`) renamed in one step (the original is gone: whatever is at
    /// `from` later is not it).
    fn copied(&self, by_rename: bool) -> Journal {
        let mut j = self.clone();
        match &mut j {
            Journal::Remove { copied, renamed, .. } | Journal::Switch { copied, renamed, .. } | Journal::Move { copied, renamed, .. } => {
                *copied = true;
                *renamed = by_rename;
            }
        }
        j
    }

    /// The same, its move over: recovery finishes what came after it (the configuration) and never touches `from`.
    fn finished(&self) -> Journal {
        let mut j = self.clone();
        match &mut j {
            Journal::Remove { done, .. } | Journal::Switch { done, .. } | Journal::Move { done, .. } => *done = true,
        }
        j
    }

}

/// Writes a journal durably (written aside, synced, renamed).
pub(crate) fn write_journal_at(path: &Path, j: &Journal) -> io::Result<()> {
    let tmp = path.with_extension("journal.tmp");
    std::fs::write(&tmp, serde_json::to_vec(j).map_err(io::Error::other)?)?;
    trs_platform::fs::sync_file(&tmp)?;
    std::fs::rename(&tmp, path)?;
    if let Some(d) = path.parent() {
        trs_platform::fs::sync_dir(d)?;
    }
    Ok(())
}

/// Moves `p` (if it is there) into the folder's archive, as a replaced file is kept.
fn to_archive(fs: &dyn Fs, p: &str) -> io::Result<()> {
    if fs.lstat(p)?.is_none() {
        return Ok(());
    }
    let stamp = trs_sync::clock::compact_stamp(trs_sync::clock::Clock::now_ns(&SystemClock));
    let mut dest = trs_sync::conflict::archive_path(p, &stamp);
    let mut n = 2;
    while fs.lstat(&dest)?.is_some() {
        dest = trs_sync::conflict::archive_path(p, &format!("{stamp}-{n}"));
        n += 1;
    }
    if let Some((dir, _)) = dest.rsplit_once('/') {
        let mut at = String::new();
        for c in dir.split('/') {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(c);
            fs.create_dir(&at)?;
        }
    }
    fs.rename(p, &dest)
}

/// The plain name a conflict copy gets when both are kept ("Report (MacBook, 2026-10-25 14.32).docx"), numbered while
/// `taken` says a name is.
fn plain_name(copy: &str, mut taken: impl FnMut(&str) -> io::Result<bool>) -> io::Result<String> {
    let at = copy.rfind(" (conflict, ").ok_or_else(|| io::Error::other("not a conflict copy"))?;
    let base = format!("{} ({}", &copy[..at], &copy[at + " (conflict, ".len()..]);
    let mut name = base.clone();
    let mut n = 2;
    while taken(&name)? {
        name = match base.rsplit_once('.') {
            Some((stem, ext)) if !ext.contains(['/', ' ', ')']) => format!("{stem} {n}.{ext}"),
            _ => format!("{base} {n}"),
        };
        n += 1;
    }
    Ok(name)
}

/// A move whose copy is whole and noted but that could not be finished: its journal stays (the next start finishes it),
/// and the folder is held until then.
#[derive(Debug)]
pub(crate) struct Unfinished(String);

impl std::fmt::Display for Unfinished {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unfinished {}

/// The move failed as [`Unfinished`].
pub(crate) fn unfinished(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|i| i.is::<Unfinished>())
}

/// A removed folder's marker: its archive may hold the only copy of something, so only the marker's other parts go.
fn prune_state(files: &Path) {
    let state = files.join(STATE_DIR);
    if state.join("Archive").exists() {
        let _ = std::fs::remove_file(state.join("case-probe.tmp"));
    } else {
        let _ = std::fs::remove_dir_all(&state);
    }
}

/// `path` if nothing is there (or an empty folder, which is taken over), else the first free `<name> 2`, `<name> 3`, …
/// next to it. The empty folder is removed ([`free_place_name`] only picks).
fn free_place(path: &Path) -> PathBuf {
    let to = free_place_name(path);
    let _ = take_over_empty(&to);
    to
}

/// [`free_place`]'s choice, touching nothing.
fn free_place_name(path: &Path) -> PathBuf {
    let free = |p: &Path| !p.exists() || std::fs::read_dir(p).is_ok_and(|mut d| d.next().is_none());
    if free(path) {
        return path.to_path_buf();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (2..).map(|n| path.with_file_name(format!("{name} {n}"))).find(|p| !p.exists()).expect("a free name")
}

/// An empty folder at `to` goes, so a move can put a folder there.
fn take_over_empty(to: &Path) -> io::Result<()> {
    if to.is_dir() && std::fs::read_dir(to)?.next().is_none() {
        std::fs::remove_dir(to)?;
    }
    Ok(())
}

/// Moves a directory: a rename on the same volume, else a copy then a delete of the original ([`move_tree_noted`],
/// with nothing to note).
fn move_tree(from: &Path, to: &Path) -> io::Result<Option<PathBuf>> {
    move_tree_noted(from, to, |_| Ok(()), false)
}

/// Where an original goes once its copy is whole, before it is deleted: beside it, on its own volume.
pub(crate) fn aside_of(from: &Path) -> PathBuf {
    let name = from.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    from.with_file_name(format!("{name}.seedwell-moved"))
}

/// Marks a copy in progress: written into `to`'s state folder (`.seedwell`, never scanned or synced, so no user's file
/// can be it) before anything is copied, removed once the copy is noted whole. Only a `to` that carries it is ever
/// removed as a partial copy, and one that carries it is never taken as the folder.
const PARTIAL: &str = ".seedwell/partial-copy";

/// Marks `to` as a partial copy ([`PARTIAL`]).
fn mark_partial(to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to.join(STATE_DIR))?;
    std::fs::write(to.join(PARTIAL), b"")?;
    trs_platform::fs::sync_dir(&to.join(STATE_DIR))
}

/// `to` carries the partial-copy mark.
fn is_partial(to: &Path) -> bool {
    to.join(PARTIAL).exists()
}

#[cfg(test)]
thread_local! {
    /// **Unit tests:** taking a mark off fails (on this thread).
    static UNMARK_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Takes the mark off `to` (gone already is fine).
fn unmark_partial(to: &Path) -> io::Result<()> {
    #[cfg(test)]
    if UNMARK_FAILS.get() {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "test: the mark stays"));
    }
    match std::fs::remove_file(to.join(PARTIAL)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    trs_platform::fs::sync_dir(&to.join(STATE_DIR)).or_else(|_| trs_platform::fs::sync_dir(to))
}

/// Moves a directory: a rename on the same volume (or with `copy`, always) else a copy, made durable, then `copied`
/// (the caller notes the move is done, or the copy whole), then the original renamed aside on its own volume
/// ([`aside_of`]: its place is whole or empty, never half deleted, so a folder there stops as missing instead of
/// scanning a half-deleted tree), then deleted. A rename is noted done too. A failure before `copied` removes the
/// partial copy and leaves the original as it was (the error); after it the move stands. Returns an original that
/// could not be set aside or deleted, and where it is.
pub(crate) fn move_tree_noted(from: &Path, to: &Path, copied: impl FnOnce(bool) -> io::Result<()>, copy: bool) -> io::Result<Option<PathBuf>> {
    move_tree_kept(from, to, copied, copy, None)
}

/// [`move_tree_noted`]; a copy to another volume also keeps a manifest of the source as it was copied at `manifest`
/// (so a start that finishes the move deletes the original only if it is still exactly that), refuses names that the
/// target volume would take for one another (case, normalization), and checks the copy holds every source entry.
pub(crate) fn move_tree_kept(
    from: &Path,
    to: &Path,
    copied: impl FnOnce(bool) -> io::Result<()>,
    copy: bool,
    manifest: Option<&Path>,
) -> io::Result<Option<PathBuf>> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // The source is an open folder: a mark in it is a stale one (a move of it cut off long ago), never carried along.
    match std::fs::remove_file(from.join(PARTIAL)) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    if !copy {
        match std::fs::rename(from, to) {
            Ok(()) => {
                // Done: noted, so a start after a crash never takes `to` for a partial copy. Not noted: the move is
                // undone (renamed back) and fails; if that cannot be, it stands (a start finds the original gone and
                // `to` there, and finishes it).
                if let Err(e) = copied(true)
                    && std::fs::rename(to, from).is_ok()
                {
                    return Err(e);
                }
                return Ok(None);
            }
            // EXDEV: another volume.
            Err(e) if e.kind() == io::ErrorKind::CrossesDevices => {}
            Err(e) => return Err(e),
        }
    }
    std::fs::create_dir(to)?;
    let made = (|| {
        mark_partial(to)?;
        trs_platform::fs::sync_dir(to)?;
        let source = Manifest::of(from)?;
        let (fold_case, fold_norm) = folds(to)?;
        let clashes = clashes(&source, fold_case, fold_norm);
        if !clashes.is_empty() {
            let named: Vec<String> = clashes.iter().take(5).map(|(a, b)| format!("{a} / {b}")).collect();
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} would merge names that differ only in case or form, and lose files: {}", to.display(), named.join(", ")),
            ));
        }
        if let Some(m) = manifest {
            source.save(m)?;
        }
        copy_tree(from, to)?;
        sync_tree(to)?;
        source.copied_whole(to, fold_case, fold_norm)?;
        // Changed while it was copied (a tool wrote there): the copy may hold an older state. Not this time.
        if Manifest::of(from)? != source {
            return Err(io::Error::other(format!("{} changed while it was copied; nothing was moved", from.display())));
        }
        copied(false)
    })();
    if let Err(e) = made {
        // Not whole, or not noted as whole: the copy goes, the original stays.
        let _ = remove_partial(to);
        return Err(e);
    }
    // Whole and noted, but still marked: not finished. The journal stays (it says the copy is whole), and the folder is
    // held until a start finishes the move ([`Unfinished`]).
    unmark_partial(to).map_err(|e| io::Error::new(e.kind(), Unfinished(format!("the copy at {} could not be marked whole ({e})", to.display()))))?;
    let aside = aside_of(from);
    if std::fs::rename(from, &aside).is_err() {
        return Ok(Some(from.to_path_buf()));
    }
    Ok(std::fs::remove_dir_all(&aside).err().map(|_| aside))
}

/// Removes a partial copy, its mark last: one that cannot be removed all the way still carries the mark, so it is
/// still known for a partial copy (never taken for anything else).
fn remove_partial(to: &Path) -> io::Result<()> {
    let state = to.join(STATE_DIR);
    let mark = to.join(PARTIAL);
    let remove = |dir: &Path| -> io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let p = e.path();
            if p == state || p == mark {
                continue;
            }
            if e.file_type()?.is_dir() {
                std::fs::remove_dir_all(&p)?;
            } else {
                std::fs::remove_file(&p)?;
            }
        }
        Ok(())
    };
    remove(to)?;
    if state.is_dir() {
        remove(&state)?;
    }
    match std::fs::remove_file(&mark) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    if state.is_dir() {
        std::fs::remove_dir(&state)?;
    }
    std::fs::remove_dir(to)
}

/// [`finish_move_off`], or for a move that was over (`done`) nothing: the files are at `to`, and whatever is at `from`
/// now is not the folder's (never renamed aside or deleted).
async fn finish_or_done(from: &Path, to: &Path, copied: bool, renamed: bool, made_to: bool, done: bool, manifest: PathBuf) -> io::Result<Option<Option<PathBuf>>> {
    if done {
        return Ok(Some(None));
    }
    finish_move_off(from, to, copied, renamed, made_to, manifest).await
}

/// [`finish_move`] off the async workers (it may delete a tree).
async fn finish_move_off(from: &Path, to: &Path, copied: bool, renamed: bool, made_to: bool, manifest: PathBuf) -> io::Result<Option<Option<PathBuf>>> {
    let (from, to) = (from.to_path_buf(), to.to_path_buf());
    tokio::task::spawn_blocking(move || finish_move_kept(&from, &to, copied, renamed, made_to, Some(&manifest))).await.map_err(io::Error::other)?
}

/// After a move cut off: `copied` says it was noted done (its copy whole, or with `renamed`, renamed in one step).
/// Noted done, it is finished: a copy's original is set aside and deleted (`to` is never deleted then); after a rename
/// whatever is at `from` is left alone. Not noted, it is undone: `to` is removed only when it carries the partial-copy
/// mark ([`PARTIAL`]) and the original is there (a marked `to` with the original missing is an error of kind
/// `NotFound`: the original's disk is not mounted, and nothing is touched), or when it is an empty folder the move made
/// itself (`made_to`); anything else at `to` is never touched. `Ok(None)`: not moved; `Ok(Some(leftover))`: moved,
/// with an original that could not be deleted.
#[cfg(test)]
pub(crate) fn finish_move(from: &Path, to: &Path, copied: bool, renamed: bool, made_to: bool) -> io::Result<Option<Option<PathBuf>>> {
    finish_move_kept(from, to, copied, renamed, made_to, None)
}

/// [`finish_move`]; with the `manifest` the copy kept of the source, an original is deleted only while it is exactly
/// that (each entry's name, kind, size, times, mode and inode, on its own volume); without one (a journal from before
/// manifests), only while it is as the copy holds it.
pub(crate) fn finish_move_kept(
    from: &Path,
    to: &Path,
    copied: bool,
    renamed: bool,
    made_to: bool,
    manifest: Option<&Path>,
) -> io::Result<Option<Option<PathBuf>>> {
    let aside = aside_of(from);
    if renamed {
        // Done in one step: whatever is at `from` now is not the original.
        return Ok(Some(None));
    }
    if copied {
        // The original changed after it was copied (edited while the move waited to be finished): nothing is
        // deleted; both stay, and the error names them.
        let kept = manifest.and_then(Manifest::load);
        let changed = match &kept {
            Some(m) => from.exists() && Manifest::of(from)? != *m,
            None => from.exists() && changed_since_copied(from, to, Path::new(""))?,
        };
        // And the copy still holds all of it (nothing removed from it since): else the original stays too.
        if let Some(m) = &kept
            && from.exists()
        {
            // The copy's disk not mounted: waits (held), never probed (that would make a folder where it is not).
            if !to.exists() {
                return Err(io::Error::new(io::ErrorKind::NotFound, format!("{} is not there (its disk is not mounted?)", to.display())));
            }
            let (fold_case, fold_norm) = folds(to)?;
            m.copied_whole(to, fold_case, fold_norm)?;
        }
        if changed {
            return Err(io::Error::other(format!(
                "{} changed after it was copied to {}: both are kept and nothing is deleted; keep the one you want and remove the folder from Seedwell, then add it again",
                from.display(),
                to.display()
            )));
        }
        unmark_partial(to)?;
        if from.exists() && std::fs::rename(from, &aside).is_err() {
            return Ok(Some(Some(from.to_path_buf())));
        }
        if aside.exists() && std::fs::remove_dir_all(&aside).is_err() {
            return Ok(Some(Some(aside)));
        }
        return Ok(Some(None));
    }
    if is_partial(to) {
        if !from.exists() {
            // A partial copy, and the original not here (its disk not mounted yet): neither is the folder. The move is
            // finished or undone at a start that finds the original.
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{} is not there (its disk is not mounted?), and {} is a copy of it cut off halfway", from.display(), to.display()),
            ));
        }
        // A copy cut off before it was whole: the original is.
        remove_partial(to)?;
        return Ok(None);
    }
    if from.exists() {
        // Nothing moved. A folder the move made at `to` and left as the move leaves it before its mark (empty, or with
        // only the empty state folder the mark goes into) goes; anything else at `to` stays.
        if made_to && only_the_moves(to)? {
            let _ = std::fs::remove_dir(to.join(STATE_DIR));
            std::fs::remove_dir(to)?;
        }
        return Ok(None);
    }
    // Renamed (one volume), not noted: done. Or nothing there at all.
    Ok(to.exists().then_some(None))
}

/// Something in the original `from` (below `rel`) is not in its copy `to` as it was copied (a file's size or time, a
/// link's target, a kind): it changed after the copy. (The copy keeps each file's time.) The mark is left out.
fn changed_since_copied(from: &Path, to: &Path, rel: &Path) -> io::Result<bool> {
    for e in std::fs::read_dir(from.join(rel))? {
        let e = e?;
        let r = rel.join(e.file_name());
        if r == Path::new(PARTIAL) {
            continue;
        }
        let (a, b) = (std::fs::symlink_metadata(from.join(&r))?, std::fs::symlink_metadata(to.join(&r)));
        let Ok(b) = b else { return Ok(true) };
        let t = a.file_type();
        if t.is_dir() != b.file_type().is_dir() || t.is_symlink() != b.file_type().is_symlink() {
            return Ok(true);
        }
        if t.is_dir() {
            if changed_since_copied(from, to, &r)? {
                return Ok(true);
            }
        } else if t.is_symlink() {
            if trs_platform::fs::read_link(&from.join(&r))? != trs_platform::fs::read_link(&to.join(&r))? {
                return Ok(true);
            }
        } else if a.len() != b.len() || a.modified().ok() != b.modified().ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The source of a copy as it was copied, entry by entry (its path below it as name components, each the name's own
/// bytes, so a name holding `\\` or bytes that are not UTF-8 stays itself; kind, size, times, mode, inode), in path
/// order. The partial-copy mark and special files (sockets, devices, FIFOs: never copied or synced) are left out.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Manifest(Vec<ManifestEntry>);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct ManifestEntry {
    path: Vec<Vec<u8>>,
    /// 0 file, 1 folder, 2 link.
    kind: u8,
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
    mode: u32,
    inode: u64,
}

/// A path below a folder, as its names' own bytes.
type Components = Vec<Vec<u8>>;

/// A name's own bytes (on Windows, its UTF-16 units).
fn name_bytes(n: &std::ffi::OsStr) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        n.as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        use std::os::windows::ffi::OsStrExt;
        n.encode_wide().flat_map(u16::to_le_bytes).collect()
    }
}

/// The name of [`name_bytes`].
fn bytes_name(b: &[u8]) -> std::ffi::OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(b.to_vec())
    }
    #[cfg(not(unix))]
    {
        use std::os::windows::ffi::OsStringExt;
        let units: Vec<u16> = b.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        std::ffi::OsString::from_wide(&units)
    }
}

impl ManifestEntry {
    /// Its path below the root, as this system names it.
    fn rel(&self) -> PathBuf {
        self.path.iter().map(|c| bytes_name(c)).collect()
    }

    /// Its path, readable (for messages only).
    fn shown(&self) -> String {
        self.rel().to_string_lossy().into_owned()
    }
}

/// How a volume that folds case (`fold_case`) or Unicode form (`fold_norm`) compares a name: names it takes for one
/// another have one key. A name that is not UTF-8 is its own key.
fn fold_key(name: &std::ffi::OsStr, fold_case: bool, fold_norm: bool) -> Vec<u8> {
    let Some(s) = name.to_str() else { return name_bytes(name) };
    match (fold_norm, fold_case) {
        // As the folder's own names are compared ([`trs_sync::path::key`]).
        (true, _) => trs_sync::path::key(s, fold_case).into_bytes(),
        (false, true) => {
            let mut out = String::with_capacity(s.len());
            s.chars().for_each(|c| trs_sync::path::fold_char(c, &mut out));
            out.into_bytes()
        }
        (false, false) => s.as_bytes().to_vec(),
    }
}

impl Manifest {
    fn of(root: &Path) -> io::Result<Manifest> {
        let mut out = Vec::new();
        Self::walk(root, &mut Vec::new(), &mut out)?;
        Ok(Manifest(out))
    }

    fn walk(root: &Path, rel: &mut Vec<Vec<u8>>, out: &mut Vec<ManifestEntry>) -> io::Result<()> {
        let dir: PathBuf = std::iter::once(root.as_os_str().to_owned()).chain(rel.iter().map(|c| bytes_name(c))).collect();
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&dir)?.map(|e| e.map(|e| e.file_name())).collect::<io::Result<_>>()?;
        names.sort();
        for n in names {
            if rel.len() == 1 && rel[0] == name_bytes(std::ffi::OsStr::new(STATE_DIR)) && n == "partial-copy" {
                continue;
            }
            let m = std::fs::symlink_metadata(dir.join(&n))?;
            let t = m.file_type();
            let kind = if t.is_symlink() {
                2
            } else if t.is_dir() {
                1
            } else if t.is_file() {
                0
            } else {
                // A socket, device or FIFO: never copied (opening one may wait forever), never synced.
                continue;
            };
            let ns = |t: io::Result<std::time::SystemTime>| t.ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i128);
            #[cfg(unix)]
            let (ctime_ns, mode, inode) = {
                use std::os::unix::fs::MetadataExt;
                (i128::from(m.ctime()) * 1_000_000_000 + i128::from(m.ctime_nsec()), m.mode(), m.ino())
            };
            #[cfg(not(unix))]
            let (ctime_ns, mode, inode) = (ns(m.created()), u32::from(m.permissions().readonly()), 0u64);
            rel.push(name_bytes(&n));
            out.push(ManifestEntry { path: rel.clone(), kind, size: if kind == 0 { m.len() } else { 0 }, mtime_ns: ns(m.modified()), ctime_ns, mode, inode });
            if kind == 1 {
                Self::walk(root, rel, out)?;
            }
            rel.pop();
        }
        Ok(())
    }

    /// Written durably (aside, synced, renamed).
    fn save(&self, at: &Path) -> io::Result<()> {
        let tmp = at.with_extension("manifest.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self).map_err(io::Error::other)?)?;
        trs_platform::fs::sync_file(&tmp)?;
        std::fs::rename(&tmp, at)?;
        if let Some(d) = at.parent() {
            trs_platform::fs::sync_dir(d)?;
        }
        Ok(())
    }

    fn load(at: &Path) -> Option<Manifest> {
        std::fs::read(at).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    /// The copy at `to` holds every entry, under its name as the target volume compares names (it may store a name in
    /// another form: HFS+ keeps NFD), with its kind, a file at its size. One folder's listing at a time.
    fn copied_whole(&self, to: &Path, fold_case: bool, fold_norm: bool) -> io::Result<()> {
        // The listings of the folders along the current path (entries come in walk order: a folder, then what is in
        // it), each read once.
        let mut listed: Vec<(Components, BTreeSet<Vec<u8>>)> = Vec::new();
        for e in &self.0 {
            let (parent, name) = e.path.split_at(e.path.len() - 1);
            while listed.last().is_some_and(|(p, _)| !parent.starts_with(p)) {
                listed.pop();
            }
            if listed.last().is_none_or(|(p, _)| p.as_slice() != parent) {
                let dir: PathBuf = std::iter::once(to.as_os_str().to_owned()).chain(parent.iter().map(|c| bytes_name(c))).collect();
                let names = std::fs::read_dir(&dir)?.map(|x| x.map(|x| fold_key(&x.file_name(), fold_case, fold_norm))).collect::<io::Result<_>>()?;
                listed.push((parent.to_vec(), names));
            }
            let key = fold_key(&bytes_name(&name[0]), fold_case, fold_norm);
            let there = std::fs::symlink_metadata(to.join(e.rel())).ok();
            let fine = listed.last().is_some_and(|(_, names)| names.contains(&key))
                && there.is_some_and(|m| {
                    let kind = if m.file_type().is_symlink() { 2 } else if m.is_dir() { 1 } else { 0 };
                    kind == e.kind && (kind != 0 || m.len() == e.size)
                });
            if !fine {
                return Err(io::Error::other(format!("{} is not in the copy at {} as it is in the original; nothing was moved", e.shown(), to.display())));
            }
        }
        Ok(())
    }
}

/// Whether the volume of the folder `at` takes names that differ only in case, or only in Unicode form, for one
/// another (found by trying, in its state folder).
fn folds(at: &Path) -> io::Result<(bool, bool)> {
    let dir = at.join(STATE_DIR);
    std::fs::create_dir_all(&dir)?;
    let probe = |made: &str, other: &str| -> io::Result<bool> {
        let p = dir.join(made);
        std::fs::write(&p, b"")?;
        let same = dir.join(other).exists();
        std::fs::remove_file(&p)?;
        Ok(same)
    };
    Ok((probe("case-probe-a", "CASE-PROBE-A")?, probe("form-probe-\u{e9}", "form-probe-e\u{301}")?))
}

/// Pairs of entries (in one folder) that a volume folding case (`fold_case`) and/or form (`fold_norm`) would take for
/// one name.
fn clashes(m: &Manifest, fold_case: bool, fold_norm: bool) -> Vec<(String, String)> {
    if !fold_case && !fold_norm {
        return Vec::new();
    }
    let mut seen: BTreeMap<(Vec<Vec<u8>>, Vec<u8>), &ManifestEntry> = BTreeMap::new();
    let mut out = Vec::new();
    for e in &m.0 {
        let (parent, name) = e.path.split_at(e.path.len() - 1);
        let key = (parent.to_vec(), fold_key(&bytes_name(&name[0]), fold_case, fold_norm));
        match seen.get(&key) {
            Some(first) => out.push((first.shown(), e.shown())),
            None => {
                seen.insert(key, e);
            }
        }
    }
    out
}

/// `to` is a folder holding nothing, or only an empty state folder (what a move makes before its mark).
fn only_the_moves(to: &Path) -> io::Result<bool> {
    if !to.is_dir() {
        return Ok(false);
    }
    for e in std::fs::read_dir(to)? {
        let e = e?;
        if e.file_name() != STATE_DIR || !e.file_type()?.is_dir() || std::fs::read_dir(e.path())?.next().is_some() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Makes a copied tree durable: every file's data and every folder's entries.
fn sync_tree(p: &Path) -> io::Result<()> {
    for e in std::fs::read_dir(p)? {
        let e = e?;
        let t = e.file_type()?;
        if t.is_dir() {
            sync_tree(&e.path())?;
        } else if t.is_file() {
            trs_platform::fs::sync_file(&e.path())?;
        }
    }
    trs_platform::fs::sync_dir(p)
}

/// Copies `from` to `to`, which must not exist yet. A copy that fails halfway (a full disk, a link
/// this system cannot make) removes what it made, so only the original is left.
#[cfg(test)]
fn copy_new_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir(to)?;
    copy_tree(from, to).inspect_err(|_| {
        // Everything below `to` is the copy's: it did not exist before.
        let _ = std::fs::remove_dir_all(to);
    })
}

/// Copies the contents of `from` into the folder `to`.
fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    copy_tree_in(from, to, true)
}

/// [`copy_tree`]; at the `top`, the state folder may be there already (it carries the mark), and the source's own
/// mark (from a move of it cut off earlier) is not copied.
fn copy_tree_in(from: &Path, to: &Path, top: bool) -> io::Result<()> {
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let t = e.file_type()?;
        if top && e.file_name() == STATE_DIR && t.is_dir() {
            std::fs::create_dir_all(&dst)?;
            for s in std::fs::read_dir(&src)? {
                let s = s?;
                if src.join(s.file_name()) == from.join(PARTIAL) {
                    continue;
                }
                let (ss, sd) = (s.path(), dst.join(s.file_name()));
                if s.file_type()?.is_dir() {
                    std::fs::create_dir(&sd)?;
                    copy_tree_in(&ss, &sd, false)?;
                } else {
                    copy_entry(&ss, &sd, s.file_type()?)?;
                }
            }
            continue;
        }
        if t.is_dir() {
            std::fs::create_dir(&dst)?;
            copy_tree_in(&src, &dst, false)?;
        } else {
            copy_entry(&src, &dst, t)?;
        }
    }
    Ok(())
}

/// Copies one file or link, the file's time with it.
fn copy_entry(src: &Path, dst: &Path, t: std::fs::FileType) -> io::Result<()> {
    // A socket, device or FIFO is not copied (opening one can wait forever) nor synced: it stays with the original.
    if !t.is_symlink() && !t.is_file() {
        return Ok(());
    }
    if t.is_symlink() {
        trs_platform::fs::symlink(&trs_platform::fs::read_link(src)?, dst)?;
    } else {
        // A user's file, which a program may be writing during the move: never locked against it (Windows).
        trs_platform::fs::copy_shared(src, dst)?;
        let m = std::fs::metadata(src)?;
        if let Ok(t) = m.modified() {
            std::fs::File::options().write(true).open(dst)?.set_modified(t)?;
        }
    }
    Ok(())
}

type Foreign = mpsc::Receiver<trs_utp::ForeignDatagram>;

/// Binds the sync port: TCP, and with `utp` its UDP twin (same number) for uTP and hole-punch probes. The
/// saved port first; if it (or its UDP twin) is taken, any free pair, which the caller then remembers. If
/// no pair is free after a few tries, the service runs without uTP.
///
/// On `[::]` both sockets take IPv4 as well (`IPV6_V6ONLY` off, set on every system: Windows defaults to on, and
/// its sync port then refused every IPv4 connection). A system that cannot do that (IPv6 turned off) gets
/// `0.0.0.0` instead, said in the log. Returns the address bound.
async fn bind_port(ip: IpAddr, saved: u16, utp: bool) -> io::Result<(IpAddr, TcpListener, Option<trs_utp::UtpSocket>, Option<Foreign>)> {
    let ip = if ip == IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED) && !trs_platform::net::dual_stack_available() {
        eprintln!("seedwell sync: this system cannot listen for IPv4 and IPv6 on one socket (IPv6 off?): listening on IPv4 only");
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        ip
    };
    const TRIES: usize = 8;
    let mut last = None;
    for attempt in 0..TRIES {
        let want = if attempt == 0 { saved } else { 0 };
        let listener = match trs_platform::net::listen_tcp(SocketAddr::new(ip, want)) {
            Ok(l) => l,
            Err(e) if attempt + 1 < TRIES => {
                last = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        if !utp {
            return Ok((ip, listener, None, None));
        }
        let port = listener.local_addr()?.port();
        let (tx, rx) = mpsc::channel(256);
        let cfg = trs_utp::Config::default();
        match utp_socket(SocketAddr::new(ip, port), cfg, tx).await {
            Ok(u) => return Ok((ip, listener, Some(u), Some(rx))),
            Err(e) if attempt + 1 < TRIES => last = Some(e),
            Err(e) => {
                eprintln!("seedwell sync: no uTP (UDP port {port}: {e})");
                return Ok((ip, listener, None, None));
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("could not bind the sync port")))
}

/// The uTP socket on `addr`, bound like the TCP listener (dual-stack on `[::]`).
async fn utp_socket(addr: SocketAddr, cfg: trs_utp::Config, foreign: mpsc::Sender<trs_utp::ForeignDatagram>) -> io::Result<trs_utp::UtpSocket> {
    let udp = trs_platform::net::bind_udp(addr)?;
    udp.writable().await?;
    trs_utp::UtpSocket::from_udp(udp, cfg, Some(foreign))
}

/// Accepts uTP sessions on the sync port's UDP twin, like TCP ones (magic, then TLS).
async fn utp_accept_loop(h: SyncHandle, sock: trs_utp::UtpSocket) {
    loop {
        let Ok((s, remote)) = sock.accept().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        if h.inner.stopping.load(Ordering::Relaxed) {
            return;
        }
        let h = h.clone();
        tokio::spawn(async move {
            let local = s.local_addr();
            if let Ok(conn) = session::accept_over(&h.inner.ctx.tls, Box::new(s), remote, local, Via::Utp).await {
                h.run_session(conn, Vec::new()).await;
            }
        });
    }
}

/// Datagrams on the sync port that are not uTP: hole-punch probes.
async fn foreign_loop(h: SyncHandle, mut rx: Foreign) {
    while let Some(d) = rx.recv().await {
        crate::punch::on_datagram(&h, d.from, &d.data);
    }
}

async fn accept_loop(h: SyncHandle, listener: TcpListener) {
    loop {
        let Ok((tcp, remote)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        if h.inner.stopping.load(Ordering::Relaxed) {
            return;
        }
        let h = h.clone();
        tokio::spawn(async move {
            match session::accept(&h.inner.ctx.tls, tcp, remote).await {
                Ok(Accepted::Session(conn)) => {
                    h.run_session(*conn, Vec::new()).await;
                }
                Ok(Accepted::Relay(tcp, remote)) => crate::relay::serve(h.clone(), tcp, remote).await,
                Err(_) => {}
            }
        });
    }
}

async fn hub_loop(h: SyncHandle, mut rx: mpsc::UnboundedReceiver<HubMsg>) {
    while let Some(msg) = rx.recv().await {
        match msg {
            HubMsg::PeerUp(d) => {
                // Tell the new peer where we are, and pass on what we know of the other members of the folders and
                // channels it shares with us (peer exchange: records are signed by their devices, so passing them
                // on cannot forge them). Devices of other folders and channels stay out of it.
                if let Some(p) = h.inner.ctx.registry.peer(&d) {
                    h.remember_device(d, Some(&p.name), None, None);
                    h.seen_now(d);
                    let shared: Vec<Arc<Space>> = h.inner.ctx.registry.spaces().into_iter().filter(|f| f.role_of(&p.device_pk).is_some()).collect();
                    let fellow = |pk: &[u8; 32]| shared.iter().any(|f| f.role_of(pk).is_some());
                    let mut records = vec![h.own_record(p.local)];
                    records.extend(lock(&h.inner.book).records.values().filter(|r| DeviceId::of_public_key(&r.device_pk) != d && fellow(&r.device_pk)).take(64).cloned());
                    let _ = p.control.send(Message::Addresses(Addresses { records })).await;
                }
            }
            HubMsg::PeerDown(d) => {
                h.seen_now(d);
                h.inner.wake_dialer.notify_one();
            }
            HubMsg::MemberSeen { folder, device } => h.remember_device(device, None, Some(folder), None),
            HubMsg::Conversations => {
                let _ = h.save_conversations();
            }
            HubMsg::Addresses(records) => {
                for r in records {
                    if !r.verify() {
                        continue;
                    }
                    let dev = DeviceId::of_public_key(&r.device_pk);
                    {
                        let mut book = lock(&h.inner.book);
                        let newer = book.records.get(&dev).is_none_or(|old| old.issued_s < r.issued_s);
                        if !newer {
                            continue;
                        }
                        book.records.insert(dev, r.clone());
                    }
                    crate::rendezvous::learn_from_record(&h, dev, &r.addrs);
                    let mut noted = false;
                    for a in &r.addrs {
                        if let Ok(sa) = a.parse::<SocketAddr>() {
                            h.learn_address(Some(dev), None, sa);
                            // Signed by the device itself: where a conversation's device can be reached.
                            noted |= h.inner.ctx.direct.note(&dev, None, None, Some(sa));
                        }
                    }
                    if noted {
                        let _ = h.save_conversations();
                    }
                }
            }
            HubMsg::Relay(from, msg) => crate::relay::on_message(&h, from, *msg).await,
        }
    }
}

/// Ends the folders' sharing when its time is up (each device by its own clock): every device but the owners is
/// then cut off like a removed one.
async fn access_clock(h: SyncHandle) {
    let mut every = tokio::time::interval(Duration::from_secs(30));
    loop {
        every.tick().await;
        for f in h.inner.ctx.registry.spaces() {
            if crate::roles::tick(&f) {
                crate::roles::changed(&h.inner.ctx, &f).await;
            }
        }
    }
}

/// Dials every member of every folder that is not connected, and every address hint, with backoff.
async fn dial_loop(h: SyncHandle) {
    // The next round: when a held-back dial is due, else in 30 s (or when woken).
    let mut next = Instant::now() + Duration::from_secs(30);
    loop {
        tokio::select! {
            _ = h.inner.wake_dialer.notified() => {}
            _ = tokio::time::sleep_until(next.into()) => {}
        }
        next = Instant::now() + Duration::from_secs(30);
        if h.inner.stopping.load(Ordering::Relaxed) {
            return;
        }
        let ctx = h.inner.ctx.clone();
        let mut wanted: BTreeMap<DeviceId, ()> = BTreeMap::new();
        let mut hints: Vec<(SocketAddr, FolderId)> = Vec::new();
        for f in ctx.registry.spaces() {
            if f.paused.load(Ordering::Relaxed) {
                continue;
            }
            for d in f.members.read().unwrap_or_else(|e| e.into_inner()).iter() {
                if ctx.registry.peer(d).is_none() {
                    wanted.insert(*d, ());
                }
            }
            if let Some(addrs) = lock(&h.inner.book).hints.get(&f.id) {
                hints.extend(addrs.iter().map(|a| (*a, f.id)));
            }
        }
        // The devices of the private conversations, at the addresses they last had.
        for (d, addrs) in ctx.direct.devices() {
            if ctx.registry.peer(&d).is_none() {
                wanted.insert(d, ());
                lock(&h.inner.book).by_device.entry(d).or_default().extend(addrs);
            }
        }
        let by_device: Vec<(DeviceId, Vec<SocketAddr>)> = {
            let book = lock(&h.inner.book);
            wanted.keys().filter_map(|d| book.by_device.get(d).map(|a| (*d, a.iter().copied().collect()))).collect()
        };
        // One dial per address, for every folder it is wanted for: a device's address that is also a folder's hint
        // (a device met in one folder, now joining another) is dialed once for both, since the dial's backoff
        // would hold back a second dial of it.
        let mut dials: BTreeMap<SocketAddr, (Option<DeviceId>, Vec<FolderId>)> = BTreeMap::new();
        for (d, addrs) in by_device {
            for a in addrs {
                dials.insert(a, (Some(d), candidates_for(&ctx, Some(d), None)));
            }
        }
        // A hint that turned out to be a connected device's address is not dialed again (each successful
        // dial clears its backoff and wakes this loop, which would otherwise redial it at once, forever).
        let connected: BTreeSet<SocketAddr> = {
            let book = lock(&h.inner.book);
            ctx.registry.peers().iter().filter_map(|p| book.by_device.get(&p.device)).flatten().copied().collect()
        };
        for (a, f) in hints {
            if !connected.contains(&a) {
                let (_, folders) = dials.entry(a).or_insert((None, Vec::new()));
                if !folders.contains(&f) {
                    folders.push(f);
                }
            }
        }
        // A channel joined with a folder's link (one link for both) is wanted wherever its folder is.
        let linked: Vec<(FolderId, FolderId)> = lock(&h.inner.cfg)
            .folders
            .iter()
            .filter_map(|e| Some((e.id.parse().ok()?, e.channel.parse().ok()?)))
            .filter(|(_, c)| ctx.registry.channel(c).is_some_and(|ch| lock(&ch.invite).is_some()))
            .collect();
        for (a, (d, mut folders)) in dials {
            for (f, c) in &linked {
                if folders.contains(f) && !folders.contains(c) {
                    folders.push(*c);
                }
            }
            if let Some(due) = h.dial(a, d, folders) {
                next = next.min(due);
            }
        }
        // Members that direct dials do not reach: their records in a rendezvous, relay servers, hole punching, member
        // relays.
        for d in wanted.keys() {
            crate::rendezvous::look_up(&h, *d);
            crate::relay::reach(&h, *d);
        }
    }
}

/// Announces and looks up each folder's daily DHT topic every 15 minutes, and at once after a network change.
async fn dht_loop(h: SyncHandle) {
    let Some(dht) = h.inner.discovery.clone() else { return };
    loop {
        let day = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) / 86_400) as u32;
        let lan_only: BTreeSet<String> = lock(&h.inner.cfg).folders.iter().filter(|f| f.lan_only || f.paused).map(|f| f.id.clone()).collect();
        if lock(&h.inner.cfg).settings.dht_discovery {
            for f in h.inner.ctx.registry.spaces() {
                if lan_only.contains(&f.id.to_string()) {
                    continue;
                }
                let mut found = dht.announce(f.public.dht_topic(day), h.inner.port).await;
                found.extend(dht.lookup(f.public.dht_topic(day.saturating_sub(1))).await);
                for a in found {
                    h.learn_address(None, Some(f.id), a);
                }
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(15 * 60)) => {}
            _ = h.inner.netwatch.dht_wake.notified() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The refusal names the torrents in the right number and never says one seeds that is still downloading: fails on
    /// "torrents seed ... remove the torrents" for one torrent, or a count that is not the number of torrents.
    #[test]
    fn the_refusal_for_torrents_in_a_folder_counts_them_and_says_uses() {
        let one = in_use_text(&["V for Vendetta (2005)".into()]);
        assert!(one.starts_with("a torrent uses files in this folder (“V for Vendetta (2005)”)") && one.contains("remove the torrent or move it"), "{one}");
        let names: Vec<String> = (1..=5).map(|i| format!("t{i}")).collect();
        let five = in_use_text(&names);
        assert!(five.starts_with("5 torrents use files in this folder (“t1”, “t2”, “t3” and 2 more)") && five.contains("remove the torrents or move them"), "{five}");
        assert!(!one.contains("seed") && !five.contains("seed"));
    }

    #[test]
    fn a_copy_to_another_volume_that_fails_halfway_leaves_nothing_behind() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("from"), t.path().join("to"));
        std::fs::create_dir_all(from.join("a/b")).unwrap();
        std::fs::write(from.join("a/b/f"), b"x").unwrap();
        copy_new_tree(&from, &to).unwrap();
        assert_eq!(std::fs::read(to.join("a/b/f")).unwrap(), b"x");
        // A second copy onto an existing folder is refused and leaves it alone.
        assert_eq!(copy_new_tree(&from, &to).unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(to.join("a/b/f").exists());

        // A source entry that cannot be copied: a folder that cannot be read, after a file that was copied.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let (to2, bad) = (t.path().join("to2"), from.join("a/b"));
            std::fs::write(from.join("a/g"), b"y").unwrap();
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o000)).unwrap();
            let failed = copy_new_tree(&from, &to2);
            std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
            if failed.is_ok() {
                return; // running as root: every folder can be read
            }
            assert!(!to2.exists(), "the half copy is removed");
            assert!(from.join("a/b/f").exists(), "the original stays");
        }
    }

    /// A tree with two files, at `p`.
    fn tree(p: &Path) {
        std::fs::create_dir_all(p.join("sub")).unwrap();
        std::fs::write(p.join("a.txt"), b"a").unwrap();
        std::fs::write(p.join("sub/b.txt"), b"b").unwrap();
    }

    /// The mark is Seedwell's alone: a user's file of its old name moves like any, a source's stale mark is not
    /// copied, and only the mark makes a partial copy.
    #[test]
    fn the_partial_copy_mark_is_in_the_state_folder_and_no_users_file_is_taken_for_it() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("from"), t.path().join("to"));
        tree(&from);
        std::fs::write(from.join(".seedwell-partial-copy"), b"the user's").unwrap();
        std::fs::create_dir_all(from.join(".seedwell/Archive")).unwrap();
        std::fs::write(from.join(".seedwell/Archive/old.txt"), b"kept").unwrap();
        std::fs::write(from.join(PARTIAL), b"").unwrap();
        assert_eq!(move_tree_noted(&from, &to, |_| Ok(()), true).unwrap(), None);
        assert!(whole(&to) && !from.exists());
        assert_eq!(std::fs::read(to.join(".seedwell-partial-copy")).unwrap(), b"the user's");
        assert_eq!(std::fs::read(to.join(".seedwell/Archive/old.txt")).unwrap(), b"kept");
        assert!(!to.join(PARTIAL).exists(), "no mark left");
        // A plain copy of it (the source still marked from a move cut off earlier) does not carry its mark.
        let again = t.path().join("again");
        std::fs::create_dir_all(to.join(".seedwell")).unwrap();
        std::fs::write(to.join(PARTIAL), b"").unwrap();
        copy_new_tree(&to, &again).unwrap();
        assert!(!again.join(PARTIAL).exists() && again.join(".seedwell-partial-copy").exists());
        std::fs::remove_file(to.join(PARTIAL)).unwrap();
    }

    /// A move never carries a stale mark, never leaves one it could not take off unreported, and recovery knows what a
    /// move cut off before its mark leaves (and no other name is a mark).
    #[test]
    #[cfg(unix)]
    fn marks_are_handled_strictly() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("from"), t.path().join("to"));
        // A stale mark in the source, a rename on one volume: not carried along.
        tree(&from);
        std::fs::create_dir_all(from.join(STATE_DIR)).unwrap();
        std::fs::write(from.join(PARTIAL), b"").unwrap();
        assert_eq!(move_tree_noted(&from, &to, |_| Ok(()), false).unwrap(), None);
        assert!(whole(&to) && !is_partial(&to));
        std::fs::remove_dir_all(&to).unwrap();
        // The rename cannot be noted: renamed back, the move failed.
        tree(&from);
        assert!(move_tree_noted(&from, &to, |_| Err(io::Error::other("disk full")), false).is_err());
        assert!(whole(&from) && !to.exists());
        // Noted whole but the mark cannot come off: an error, and the original stays.
        copy_new_tree(&from, &to).unwrap();
        mark_partial(&to).unwrap();
        std::fs::set_permissions(to.join(STATE_DIR), std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(finish_move(&from, &to, true, false, true).is_err());
        assert!(whole(&from), "the original stays");
        std::fs::set_permissions(to.join(STATE_DIR), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(finish_move(&from, &to, true, false, true).unwrap(), Some(None), "then finished");
        assert!(whole(&to) && !from.exists() && !is_partial(&to));
        std::fs::remove_dir_all(&to).unwrap();
        // Noted whole but the mark cannot come off during the move: unfinished (the journal stays, the next start
        // finishes it), the original as it was, the copy whole and marked.
        tree(&from);
        UNMARK_FAILS.set(true);
        let e = move_tree_noted(&from, &to, |_| Ok(()), true).unwrap_err();
        UNMARK_FAILS.set(false);
        assert!(unfinished(&e), "{e}");
        assert!(whole(&from) && whole(&to) && is_partial(&to));
        // Edited in the original meanwhile: nothing is deleted, the error names both.
        std::fs::write(from.join("sub/b.txt"), b"edited after the copy").unwrap();
        let e = finish_move(&from, &to, true, false, true).unwrap_err();
        assert!(e.to_string().contains("changed after it was copied"), "{e}");
        assert_eq!(std::fs::read(from.join("sub/b.txt")).unwrap(), b"edited after the copy");
        assert!(whole(&to));
        std::fs::write(from.join("sub/b.txt"), b"b").unwrap();
        std::fs::write(from.join("new.txt"), b"added after the copy").unwrap();
        assert!(finish_move(&from, &to, true, false, true).is_err(), "a file added there");
        std::fs::remove_file(from.join("new.txt")).unwrap();
        // As it was copied (its times too): the next start finishes it.
        std::fs::remove_dir_all(&from).unwrap();
        copy_new_tree(&to, &from).unwrap();
        assert_eq!(finish_move(&from, &to, true, false, true).unwrap(), Some(None), "the next start finishes it");
        assert!(whole(&to) && !from.exists() && !is_partial(&to));
        std::fs::remove_dir_all(&to).unwrap();
        // A user's file of the name development builds once gave the mark is no mark, whatever the journal says.
        tree(&from);
        copy_new_tree(&from, &to).unwrap();
        std::fs::write(to.join(".seedwell-partial-copy"), b"the user's").unwrap();
        assert_eq!(finish_move(&from, &to, false, false, false).unwrap(), None);
        assert_eq!(std::fs::read(to.join(".seedwell-partial-copy")).unwrap(), b"the user's", "left alone");
        std::fs::remove_dir_all(&to).unwrap();
        // Cut off between making the state folder and writing the mark: the move's own, it goes; not the move's
        // (`made_to` false), it stays.
        std::fs::create_dir_all(to.join(STATE_DIR)).unwrap();
        assert_eq!(finish_move(&from, &to, false, false, false).unwrap(), None);
        assert!(to.exists());
        assert_eq!(finish_move(&from, &to, false, false, true).unwrap(), None);
        assert!(!to.exists() && whole(&from));
    }

    /// A copy keeps a manifest of its source; a start finishing the move deletes the original only while it is exactly
    /// that, even a permission change or a file whose time was put back keeps it.
    #[test]
    #[cfg(unix)]
    fn an_original_is_deleted_only_while_it_is_exactly_as_it_was_copied() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let (from, to, m) = (t.path().join("from"), t.path().join("to"), t.path().join("x.manifest"));
        tree(&from);
        UNMARK_FAILS.set(true);
        assert!(unfinished(&move_tree_kept(&from, &to, |_| Ok(()), true, Some(&m)).unwrap_err()));
        UNMARK_FAILS.set(false);
        assert!(Manifest::load(&m).is_some_and(|x| x == Manifest::of(&from).unwrap()), "kept as copied");
        // A permission change, then a write whose time is put back: still not the original as copied.
        std::fs::set_permissions(from.join("a.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(finish_move_kept(&from, &to, true, false, true, Some(&m)).is_err());
        assert!(whole(&from) && whole(&to));
        // Copied again; then a write of the same size whose time is put back (as some tools do): still kept.
        std::fs::remove_dir_all(&from).unwrap();
        std::fs::remove_dir_all(&to).unwrap();
        tree(&from);
        UNMARK_FAILS.set(true);
        let _ = move_tree_kept(&from, &to, |_| Ok(()), true, Some(&m));
        UNMARK_FAILS.set(false);
        let when = std::fs::metadata(from.join("a.txt")).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(from.join("a.txt"), b"A").unwrap();
        std::fs::File::options().write(true).open(from.join("a.txt")).unwrap().set_modified(when).unwrap();
        assert!(finish_move_kept(&from, &to, true, false, true, Some(&m)).is_err(), "its change time tells");
        std::fs::remove_dir_all(&from).unwrap();
        std::fs::remove_dir_all(&to).unwrap();
        // Unchanged: finished.
        tree(&from);
        UNMARK_FAILS.set(true);
        let _ = move_tree_kept(&from, &to, |_| Ok(()), true, Some(&m));
        UNMARK_FAILS.set(false);
        assert_eq!(finish_move_kept(&from, &to, true, false, true, Some(&m)).unwrap(), Some(None));
        assert!(whole(&to) && !from.exists());
    }

    /// Names a case-insensitive (or form-insensitive) volume would take for one another are found before a copy; a
    /// copy missing an entry is never taken for whole.
    #[test]
    fn a_copy_never_merges_names_or_loses_an_entry() {
        let e = |p: &str| ManifestEntry {
            path: p.split('/').map(|c| name_bytes(std::ffi::OsStr::new(c))).collect(),
            kind: 0,
            size: 1,
            mtime_ns: 0,
            ctime_ns: 0,
            mode: 0,
            inode: 0,
        };
        let m = Manifest(vec![e("Notes/a.txt"), e("Notes/A.txt"), e("caf\u{e9}.txt"), e("cafe\u{301}.txt"), e("x/y"), e("Notes2/a.txt")]);
        assert!(clashes(&m, false, false).is_empty(), "a volume that folds nothing");
        // Shown as this system writes a path ("Notes\\a.txt" on Windows).
        let shown = |p: &str| p.replace('/', std::path::MAIN_SEPARATOR_STR);
        assert_eq!(clashes(&m, true, true), [(shown("Notes/a.txt"), shown("Notes/A.txt")), ("caf\u{e9}.txt".to_string(), "cafe\u{301}.txt".to_string())]);
        assert_eq!(clashes(&m, false, true).len(), 1, "only the forms");
        assert_eq!(clashes(&m, true, false), [(shown("Notes/a.txt"), shown("Notes/A.txt"))], "case only (NTFS): the forms are two names");
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("from"), t.path().join("to"));
        tree(&from);
        let src = Manifest::of(&from).unwrap();
        copy_new_tree(&from, &to).unwrap();
        src.copied_whole(&to, true, true).unwrap();
        std::fs::remove_file(to.join("sub/b.txt")).unwrap();
        assert!(src.copied_whole(&to, true, true).is_err());
        std::fs::write(to.join("sub/b.txt"), b"longer than it was").unwrap();
        assert!(src.copied_whole(&to, true, true).is_err(), "not at its size");
        // A name the target stores in another form (HFS+ keeps NFD) is found by the target's own comparison; on a
        // volume that keeps forms apart (NTFS), it is another name.
        let n = Manifest(vec![e("Ma\u{30a}ke.txt")]);
        std::fs::write(to.join("M\u{e5}ke.txt"), b"").unwrap();
        let m = Manifest(vec![ManifestEntry { size: 0, ..n.0[0].clone() }]);
        if folds(&to).unwrap().1 {
            assert!(m.copied_whole(&to, true, true).is_ok());
        }
        assert!(m.copied_whole(&to, true, false).is_err());
        // Folded as the folder's names are: a final sigma is a sigma, a long s an s.
        assert_eq!(fold_key(std::ffi::OsStr::new("\u{3a3}\u{3c2}"), true, false), fold_key(std::ffi::OsStr::new("\u{3c3}\u{3c3}"), true, false));
        assert_eq!(fold_key(std::ffi::OsStr::new("\u{17f}"), true, true), fold_key(std::ffi::OsStr::new("S"), true, true));
    }

    /// Names are their own bytes: a backslash stays in its name, a name that is not UTF-8 moves as it is, and a FIFO
    /// (never synced) is not copied, so a move never waits on one.
    #[test]
    #[cfg(unix)]
    fn a_move_keeps_names_as_they_are_and_skips_special_files() {
        use std::os::unix::ffi::OsStrExt;
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("from"), t.path().join("to"));
        tree(&from);
        std::fs::write(from.join("back\\slash.txt"), b"bs").unwrap();
        let odd = std::ffi::OsStr::from_bytes(b"latin1-\xe9.txt");
        // (A volume that only takes UTF-8 names refuses it: then that part is left out.)
        let odd_made = std::fs::write(from.join(odd), b"odd").is_ok();
        let fifo = from.join("pipe");
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let m = Manifest::of(&from).unwrap();
        assert!(m.0.iter().any(|e| e.path == [b"back\\slash.txt".to_vec()]), "one name, not a folder and a file");
        assert!(!m.0.iter().any(|e| e.path == [b"pipe".to_vec()]), "the FIFO is no entry");
        assert_eq!(move_tree_noted(&from, &to, |_| Ok(()), true).unwrap(), None);
        assert!(whole(&to) && !from.exists());
        assert_eq!(std::fs::read(to.join("back\\slash.txt")).unwrap(), b"bs");
        if odd_made {
            assert_eq!(std::fs::read(to.join(odd)).unwrap(), b"odd");
        }
        assert!(!to.join("pipe").exists());
    }

    /// Finishing a move after a crash deletes the original only while the copy still holds all of it.
    #[test]
    #[cfg(unix)]
    fn a_start_never_deletes_an_original_whose_copy_lost_files() {
        let t = tempfile::tempdir().unwrap();
        let (from, to, m) = (t.path().join("from"), t.path().join("to"), t.path().join("x.manifest"));
        tree(&from);
        UNMARK_FAILS.set(true);
        assert!(unfinished(&move_tree_kept(&from, &to, |_| Ok(()), true, Some(&m)).unwrap_err()));
        UNMARK_FAILS.set(false);
        std::fs::remove_file(to.join("sub/b.txt")).unwrap();
        assert!(finish_move_kept(&from, &to, true, false, true, Some(&m)).is_err());
        assert!(whole(&from), "the original stays");
        // The copy's disk not mounted: it waits (held, tried again on a mount), and nothing is made where it was.
        let away = t.path().join("unmounted");
        std::fs::rename(&to, &away).unwrap();
        let e = finish_move_kept(&from, &to, true, false, true, Some(&m)).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        assert!(!to.exists() && whole(&from));
    }

    fn whole(p: &Path) -> bool {
        std::fs::read(p.join("a.txt")).ok().as_deref() == Some(b"a") && std::fs::read(p.join("sub/b.txt")).ok().as_deref() == Some(b"b")
    }

    /// A copy to another volume (forced here) is noted whole before the original goes, and the original is set aside on
    /// its own volume first: its place is whole or empty, never half deleted. Each crash point, then the start after it.
    #[test]
    fn a_move_across_volumes_is_finished_or_undone_never_half_deleted() {
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("from"), t.path().join("to"));

        // Whole: the copy is noted, the original goes.
        tree(&from);
        let mut noted = false;
        assert_eq!(move_tree_noted(&from, &to, |_| { noted = true; Ok(()) }, true).unwrap(), None);
        assert!(noted && whole(&to) && !from.exists() && !aside_of(&from).exists());
        std::fs::remove_dir_all(&to).unwrap();

        // Cut off before the copy was noted whole: undone (the partial copy, marked as one, goes; the original stays).
        tree(&from);
        copy_new_tree(&from, &to).unwrap();
        mark_partial(&to).unwrap();
        std::fs::remove_file(to.join("sub/b.txt")).unwrap();
        assert_eq!(finish_move(&from, &to, false, false, true).unwrap(), None);
        assert!(whole(&from) && !to.exists());

        // The journal was written, nothing moved, something is at `to` (the host's empty place, or anything): it is
        // never removed without the mark.
        std::fs::create_dir_all(to.join("host's")).unwrap();
        assert_eq!(finish_move(&from, &to, false, false, true).unwrap(), None);
        assert!(whole(&from) && to.join("host's").exists());
        std::fs::remove_dir_all(&to).unwrap();
        // An empty folder left at `to` that the move made (cut off before its mark): it goes; one that was there
        // before the move (the host's) stays.
        std::fs::create_dir(&to).unwrap();
        assert_eq!(finish_move(&from, &to, false, false, false).unwrap(), None);
        assert!(whole(&from) && to.is_dir(), "not the move's own: left alone");
        assert_eq!(finish_move(&from, &to, false, false, true).unwrap(), None);
        assert!(whole(&from) && !to.exists());
        // A partial copy, and the original not there (its disk not mounted yet): neither is adopted, the move waits.
        copy_new_tree(&from, &to).unwrap();
        mark_partial(&to).unwrap();
        std::fs::rename(&from, t.path().join("unmounted")).unwrap();
        assert_eq!(finish_move(&from, &to, false, false, true).unwrap_err().kind(), io::ErrorKind::NotFound);
        assert!(is_partial(&to), "the copy is left as it is");
        std::fs::rename(t.path().join("unmounted"), &from).unwrap();
        assert_eq!(finish_move(&from, &to, false, false, true).unwrap(), None, "the disk is back: undone");
        assert!(whole(&from) && !to.exists());

        // Noting it fails: the same, at once.
        assert!(move_tree_noted(&from, &to, |_| Err(io::Error::other("disk full")), true).is_err());
        assert!(whole(&from) && !to.exists());

        // Cut off after it was noted whole, the original still in place: finished (the copy is never deleted).
        copy_new_tree(&from, &to).unwrap();
        assert_eq!(finish_move(&from, &to, true, false, true).unwrap(), Some(None));
        assert!(whole(&to) && !from.exists() && !aside_of(&from).exists());

        // Cut off while the set-aside original was being deleted: finished.
        tree(&from);
        std::fs::rename(&to, t.path().join("to-old")).unwrap();
        copy_new_tree(&from, &to).unwrap();
        std::fs::rename(&from, aside_of(&from)).unwrap();
        std::fs::remove_file(aside_of(&from).join("a.txt")).unwrap();
        assert_eq!(finish_move(&from, &to, true, false, true).unwrap(), Some(None));
        assert!(whole(&to) && !from.exists() && !aside_of(&from).exists());

        // A rename on one volume, cut off before it was saved: moved.
        assert_eq!(finish_move(&from, &to, false, false, true).unwrap(), Some(None));
        // Noted as a rename, and something appears at the old place later (a host's root made again): the move is
        // done, the live folder at `to` and the new thing at `from` both stay.
        std::fs::create_dir_all(from.join("new")).unwrap();
        assert_eq!(finish_move(&from, &to, false, true, true).unwrap(), Some(None));
        assert!(whole(&to) && from.join("new").exists());
        assert_eq!(finish_move(&from, &to, true, true, true).unwrap(), Some(None));
        assert!(whole(&to) && from.join("new").exists(), "a rename noted done never sets `from` aside");
    }

    /// The original cannot be deleted all the way (here: a folder in it that cannot be written): the move stands, the
    /// rest of the original is left set aside and named, and its place is empty, never half deleted.
    #[cfg(unix)]
    #[test]
    fn a_delete_of_the_original_that_fails_halfway_leaves_its_place_empty() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let (from, to) = (t.path().join("from"), t.path().join("to"));
        tree(&from);
        std::fs::set_permissions(from.join("sub"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let left = move_tree_noted(&from, &to, |_| Ok(()), true).unwrap();
        let aside = aside_of(&from);
        std::fs::set_permissions(aside.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(left, Some(aside.clone()), "where the rest is");
        assert!(whole(&to) && !from.exists(), "the place is empty, not half deleted");
        assert!(aside.join("sub/b.txt").exists());
        // The same from the journal at the next start.
        std::fs::set_permissions(aside.join("sub"), std::fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(finish_move(&from, &to, true, false, true).unwrap(), Some(Some(aside.clone())));
        std::fs::set_permissions(aside.join("sub"), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(whole(&to) && !from.exists());
    }

    /// The sync port on `[::]` (what the app binds) that refuses IPv4, TCP or its uTP twin: on Windows, where
    /// `IPV6_V6ONLY` is on by default, Seedwell 0.1.18 answered only `::1` and every pairing device found "nothing
    /// answers there". Fails when a dial to 127.0.0.1 or a datagram from it does not get through, or the listener is
    /// left IPv6 only (a Mac is dual-stack by default; the platform tests simulate the Windows default); and when the
    /// uTP socket cannot send to an IPv4 peer, a hole-punch answer or a uTP dial (EINVAL for a plain IPv4 destination
    /// on an IPv6 socket, on the Mac since the sync port moved to `[::]`).
    #[tokio::test]
    async fn the_sync_port_on_any_ipv6_address_takes_ipv4_and_ipv6_over_tcp_and_utp() {
        if !trs_platform::net::dual_stack_available() {
            eprintln!("no IPv6 here: nothing to check");
            return;
        }
        let any6 = IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED);
        let (ip, l, utp, foreign) = bind_port(any6, 0, true).await.unwrap();
        assert_eq!(ip, any6);
        assert!(!socket2::SockRef::from(&l).only_v6().unwrap(), "the sync port is IPv6 only");
        let port = l.local_addr().unwrap().port();
        let (utp, mut foreign) = (utp.expect("uTP twin"), foreign.unwrap());
        let wait = Duration::from_secs(5);
        for to in ["127.0.0.1", "::1"] {
            let to: IpAddr = to.parse().unwrap();
            tokio::time::timeout(wait, tokio::net::TcpStream::connect((to, port))).await.expect("dial timed out").unwrap_or_else(|e| panic!("TCP {to}: {e}"));
            tokio::time::timeout(wait, l.accept()).await.expect("accept timed out").unwrap();
            // Not uTP, so it comes out as a foreign datagram (as a hole-punch probe does).
            let c = tokio::net::UdpSocket::bind((to, 0)).await.unwrap();
            c.send_to(b"seedwell probe", (to, port)).await.unwrap();
            let d = tokio::time::timeout(wait, foreign.recv()).await.unwrap_or_else(|_| panic!("no datagram from {to}")).unwrap();
            assert_eq!(&*d.data, b"seedwell probe");
            // And back, to the address as a peer gives it (an IPv4 one for an IPv4 peer).
            let back = c.local_addr().unwrap();
            utp.send_foreign(back, b"probe back").await.unwrap_or_else(|e| panic!("UDP to {back}: {e}"));
            let mut buf = [0u8; 16];
            let n = tokio::time::timeout(wait, c.recv(&mut buf)).await.unwrap_or_else(|_| panic!("nothing came back to {back}")).unwrap();
            assert_eq!(&buf[..n], b"probe back");
            // A uTP dial from the sync port to a peer on that address.
            let peer = trs_utp::UtpSocket::bind(SocketAddr::new(to, 0), trs_utp::Config::default()).await.unwrap();
            let (dial, accept) = tokio::join!(tokio::time::timeout(wait, utp.connect(peer.local_addr())), tokio::time::timeout(wait, peer.accept()));
            dial.expect("uTP dial timed out").unwrap_or_else(|e| panic!("uTP to {to}: {e}"));
            accept.expect("uTP accept timed out").unwrap();
        }
    }
}
