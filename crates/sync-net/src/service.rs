//! The sync service: folders, the listener, dialing, discovery, and the API the app, the CLI and agents
//! use ([`SyncHandle`]).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
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
use trs_sync_proto::msg::{AddressRecord, Addresses, Message};

use crate::config::{FolderEntry, Settings, SyncConfig};
use crate::secrets::SecretStore;
use crate::session::{self, Accepted, Via};
use crate::shared::{Ctx, FolderShared, HubMsg, Live, Names, Registry, Space, SpaceKind, lock};
use crate::status::{DeviceStatus, FolderState, FolderStatus, Snapshot};
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
    /// A headless daemon's agent switch (`seedwell daemon --chat-agents`), which has no app for the user to set
    /// it in: every channel (those it has, makes or joins) and private messages are at this level for the run, in
    /// memory only (never written to `sync.toml`). The app leaves it `None`: there only the user sets each switch.
    pub chat_agents: Option<crate::chat::AgentAccess>,
    /// The host that shows on-demand folders in the file manager, once there is one (the app sets it when it serves
    /// the Mac's File Provider; a headless daemon has none). Shared, so it outlives a restart of the service.
    pub on_demand_host: OnDemandHostCell,
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
pub use ondemand::{FetchProgress, Fetched};

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
    /// The newest signed address record of each device (passed on to other members).
    pub(crate) records: HashMap<DeviceId, AddressRecord>,
}

pub(crate) struct Inner {
    pub(crate) ctx: Arc<Ctx>,
    pub(crate) dir: PathBuf,
    pub(crate) cfg_path: PathBuf,
    pub(crate) cfg: Mutex<SyncConfig>,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) port: u16,
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
    /// Folders held back by a move cut off that cannot be finished yet (the original's disk not there), with why: shown
    /// as missing, tried again when volumes change and on [`Options::held_retry`].
    pub(crate) held: Mutex<BTreeMap<FolderId, Held>>,
    /// One retry of the held-back folders at a time (a mount and the timer at once).
    held_retrying: tokio::sync::Mutex<()>,
    /// The runtime the service runs on (its synchronous methods are called from other threads too).
    pub(crate) rt: tokio::runtime::Handle,
    /// An isolated (test) run's forbidden root ([`Options::forbidden_root`]).
    pub(crate) forbidden_root: Option<PathBuf>,
    /// A daemon's agent switch for every channel ([`Options::chat_agents`]).
    pub(crate) chat_agents: Option<crate::chat::AgentAccess>,
    /// The file manager's host of on-demand folders ([`Options::on_demand_host`]).
    pub(crate) on_demand_host: OnDemandHostCell,
}

/// The running service. Cheap to clone.
#[derive(Clone)]
pub struct SyncHandle {
    pub(crate) inner: Arc<Inner>,
}

const DEVICE_KEY: &str = "device";

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

        let (listener, utp, foreign_rx) = bind_port(opts.bind, cfg.settings.port, opts.utp).await?;
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
            secrets: opts.secrets.clone(),
            protocol: opts.protocol,
            features: opts.features,
            chat_events: tokio::sync::broadcast::channel(1024).0,
            direct,
            hold_wait: Mutex::new(Duration::from_secs(10)),
            silent_ondemand: AtomicBool::new(false),
            folder_gen: std::sync::atomic::AtomicU64::new(0),
            sessions: std::sync::atomic::AtomicU64::new(0),
            ours: Mutex::new(cfg.folders.iter().filter_map(|f| f.id.parse().ok()).collect()),
            setup_wait: Mutex::new(Duration::ZERO),
            yes_to_holds: AtomicBool::new(false),
        });
        ctx.limits.set_kib(cfg.settings.up_kib, cfg.settings.down_kib);
        if let Some(a) = opts.chat_agents {
            lock(&ctx.direct.settings).agents = a;
        }
        let lan = crate::lan::Lan::new(ctx.clone(), opts.bind, opts.lan_port, opts.lan_targets);
        let inner = Arc::new(Inner {
            ctx,
            dir,
            cfg_path,
            cfg: Mutex::new(cfg),
            secrets: opts.secrets,
            port,
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
            held: Mutex::new(BTreeMap::new()),
            held_retrying: tokio::sync::Mutex::new(()),
            rt: tokio::runtime::Handle::current(),
            forbidden_root: opts.forbidden_root,
            chat_agents: opts.chat_agents,
            on_demand_host: opts.on_demand_host,
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
        let stopped = h.recover_journals().await;
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

        let mut tasks = vec![
            tokio::spawn(accept_loop(h.clone(), listener)),
            tokio::spawn(hub_loop(h.clone(), hub_rx)),
            tokio::spawn(dial_loop(h.clone())),
            tokio::spawn(access_clock(h.clone())),
            tokio::spawn(held_loop(h.clone(), opts.held_retry)),
        ];
        if h.inner.discovery.is_some() {
            tasks.push(tokio::spawn(dht_loop(h.clone())));
        }
        if opts.lan_discovery && lock(&h.inner.cfg).settings.lan_discovery {
            tasks.push(tokio::spawn(crate::lan::run(h.clone())));
        }
        if let Some(u) = h.inner.utp.clone() {
            tasks.push(tokio::spawn(utp_accept_loop(h.clone(), u)));
        }
        if let Some(rx) = foreign_rx {
            tasks.push(tokio::spawn(foreign_loop(h.clone(), rx)));
        }
        *lock(&h.inner.tasks) = tasks;
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
        let runners: Vec<Runner> = lock(&self.inner.runners).drain().map(|(_, r)| r).collect();
        for r in &runners {
            let _ = r.stop.send(true);
        }
        for r in runners {
            let _ = tokio::time::timeout(Duration::from_secs(5), r.task).await;
        }
        for p in self.inner.ctx.registry.peers() {
            p.close();
        }
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
        lock(&self.inner.cfg).save(&self.inner.cfg_path)
    }

    fn index_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.redb"))
    }

    /// Where an on-demand folder keeps the content held on this device.
    pub(crate) fn cache_path(&self, id: &FolderId) -> PathBuf {
        self.inner.dir.join(format!("{id}.cache"))
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
        let index = self.index_path(&id);
        let ctx = self.inner.ctx.clone();
        let entry = e.clone();
        let core = tokio::task::spawn_blocking(move || -> Result<(FolderCore, RedbReader, Ignore), SyncError> {
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
            let mut core = FolderCore::new(cfg, Box::new(store), Arc::new(fs), Some(ctx.key.clone()), clock, Arc::new(random_id), ignore.clone())
                .map_err(|e| invalid(e.to_string()))?;
            core.set_membership(&writers, access).map_err(|e| invalid(e.to_string()))?;
            core.set_awaiting_write(awaiting);
            // Who is no member any more is no source or holder (as `roles::changed` keeps it).
            if role.is_some() {
                core.set_members(&members)?;
            }
            core.set_pins(entry.pins.iter().cloned());
            if entry.mode_switched {
                // Switched: to full, what is still online only is fetched now; to on demand, what is here is
                // announced again as an on-demand device's.
                core.set_on_demand(entry.on_demand)?;
            }
            Ok((core, reader, ignore))
        })
        .await
        .map_err(io::Error::other)??;
        let (core, reader, ignore) = core;
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
        let mut items = trs_sync::ondemand::Items::default();
        items.set_not_members(core.not_members());
        let shared = Arc::new(FolderShared {
            space: Arc::new(space),
            root: if e.on_demand { self.cache_path(&id) } else { e.path.clone() },
            place: Mutex::new(e.path.clone()),
            on_demand: e.on_demand,
            pins: Mutex::new(e.pins.iter().cloned().collect()),
            items: Mutex::new(items),
            freeing: Mutex::new(BTreeMap::new()),
            ignore,
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
            let join = crate::peer::own_join(&self.inner.ctx, &[folder], &p.exporter, p.device, p.lan).await;
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

    /// A folder opened or closed here: sessions being set up check their folders again.
    fn folders_moved(&self) {
        self.inner.ctx.folder_gen.fetch_add(1, Ordering::SeqCst);
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
        self.install(*secret.public_key(), path, None, false).await
    }

    /// Adds a folder of this device on demand (SPEC §7G.11): its files go into this device's cache before the folder
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

    async fn install(&self, folder_pk: [u8; 32], path: PathBuf, channel: Option<FolderId>, on_demand: bool) -> Result<FolderStatus, SyncError> {
        let id = FolderId::of_public_key(&folder_pk);
        if on_demand {
            std::fs::create_dir_all(self.cache_path(&id).join(STATE_DIR))?;
        } else {
            std::fs::create_dir_all(path.join(STATE_DIR))?;
        }
        let mut entry = FolderEntry::new(id.to_string(), path);
        entry.folder_pk = hex(&folder_pk);
        entry.on_demand = on_demand;
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

    /// The file manager's host of on-demand folders, or none ([`Options::on_demand_host`]).
    pub fn set_on_demand_host(&self, host: Option<Arc<dyn OnDemandHost>>) {
        *lock(&self.inner.on_demand_host) = host;
    }

    /// Stops syncing a folder on this device. The files stay; its index and `.seedwell` go. (Moving the
    /// files to the Trash is the app's job, which knows the user's Trash.) Returns where the files are.
    pub async fn remove_folder(&self, id: &FolderId) -> Result<PathBuf, SyncError> {
        self.remove(id, false).await
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
        self.remove(id, true).await
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

    async fn remove(&self, id: &FolderId, for_trash: bool) -> Result<PathBuf, SyncError> {
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned();
        // The file manager's host lets go of an on-demand folder first (told when it is over).
        let _settle = match entry.as_ref().filter(|e| e.on_demand) {
            Some(e) => self.release_host(id, e).await?,
            None => None,
        };
        if lock(&self.inner.held).contains_key(id) {
            return self.remove_held(id).await;
        }
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
        lock(&self.inner.held).insert(*id, Held { why: why.clone(), waiting: false });
        invalid(why)
    }

    /// Moves `j`'s files, its journal written already: noted whole once a copy is ([`move_tree_noted`]), and over once
    /// it is (a start after that only saves what follows the move, whatever is at `from` by then).
    pub(crate) async fn noted_move(&self, id: &FolderId, j: Journal) -> io::Result<Option<PathBuf>> {
        let path = self.journal_path(id);
        let (from, to) = j.paths();
        tokio::task::spawn_blocking(move || {
            let manifest = path.with_extension("manifest");
            let leftover = move_tree_kept(&from, &to, |by_rename| write_journal_at(&path, &j.copied(by_rename)), false, Some(&manifest))?;
            if let Err(e) = write_journal_at(&path, &j.finished()) {
                eprintln!("seedwell sync: {} moved to {}, but that could not be noted ({e})", from.display(), to.display());
            }
            Ok(leftover)
        })
        .await
        .unwrap_or_else(|e| Err(io::Error::other(e)))
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
    /// can.
    async fn recover_journals(&self) -> BTreeMap<FolderId, Held> {
        let mut stopped = BTreeMap::new();
        let Ok(dir) = std::fs::read_dir(&self.inner.dir) else { return stopped };
        for e in dir.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".journal").and_then(|i| i.parse::<FolderId>().ok()) else { continue };
            let j: Option<Journal> = std::fs::read(e.path()).ok().and_then(|b| serde_json::from_slice(&b).ok());
            if let Err(err) = self.recover(&id, j).await {
                eprintln!("seedwell sync: folder {id}: an interrupted change could not be finished ({err}); it stays stopped, and it is tried again when a disk is mounted");
                stopped.insert(id, held_of(&err));
                continue;
            }
            self.drop_journal(&id);
        }
        stopped
    }

    async fn recover(&self, id: &FolderId, j: Option<Journal>) -> Result<(), SyncError> {
        match j {
            None => Ok(()),
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
                self.forget_folder(id)
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
                }
                Ok(())
            }
            Some(Journal::Switch { on, from, to, place, copied, renamed, made_to, done: over }) => {
                let done = lock(&self.inner.cfg).folder(&id.to_string()).is_none_or(|e| e.on_demand == on);
                if done {
                    return Ok(());
                }
                // Finished when its copy was whole (or it was a rename), undone otherwise: never a half-deleted place.
                // Over, only saved.
                if let Some(leftover) = finish_or_done(&from, &to, copied, renamed, made_to, over, self.manifest_path(id)).await? {
                    if let Some(l) = leftover {
                        eprintln!("seedwell sync: folder {id}: a copy left behind could not be deleted: {}", l.display());
                    }
                    if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
                        e.on_demand = on;
                        e.pins = Vec::new();
                        e.mode_switched = true;
                        e.path = place;
                    }
                    self.save_config()?;
                }
                Ok(())
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
        let own_ignores = local || f.root.join(".seedwellignore").exists();
        let live = lock(&f.live);
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
        let root = f.root.clone();
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
                let fs = StdFs::new(root);
                match keep {
                    Keep::Original => {
                        to_archive(&fs, &copy)?;
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

    /// The devices of a folder (or channel) and their roles. Never waits for the folder's core.
    pub fn members(&self, id: &FolderId) -> Result<Vec<MemberInfo>, SyncError> {
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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
        let l = InviteLink { invite, token_seed: seed, name: Some(name) };
        let link = match bundle {
            Some(c) if kind == trs_sync_proto::members::LinkKind::Folder => l.format_bundle(c),
            _ => l.format_as(kind),
        };
        {
            let mut links = lock(&f.links);
            links.insert(invite.id, link.clone());
            let all: Vec<&str> = links.values().map(String::as_str).collect();
            self.inner.secrets.put(&crate::roles::links_name(id), all.join("\n").as_bytes())?;
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
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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
            let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
        if *device == self.inner.ctx.me {
            return Err(invalid(match f.kind {
                SpaceKind::Folder => concat!("to stop syncing on this ", trs_platform::computer!(), ", remove the folder instead"),
                SpaceKind::Channel => concat!("to leave the channel on this ", trs_platform::computer!(), ", leave it instead"),
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
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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

    fn spawn_changed(&self, f: &Arc<Space>) {
        let (ctx, f) = (self.inner.ctx.clone(), f.clone());
        self.inner.rt.spawn(async move { crate::roles::changed(&ctx, &f).await });
    }

    /// Adds an address (`host:port`) where devices of this folder (or channel) can be reached.
    pub fn add_address(&self, id: &FolderId, addr: SocketAddr) -> Result<(), SyncError> {
        self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
        lock(&self.inner.book).hints.entry(*id).or_default().insert(addr);
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

    fn status_of(&self, f: &FolderShared) -> FolderStatus {
        let channel_id = {
            let cfg = lock(&self.inner.cfg);
            cfg.folder(&f.id.to_string()).map(|e| e.channel.clone()).filter(|c| !c.is_empty() && cfg.channels.iter().any(|ch| ch.id == *c))
        };
        let mut live = lock(&f.live).clone();
        live.issues.extend(f.note_issues());
        let refused = lock(&f.notes).refused.clone();
        let role = f.role_of(self.inner.ctx.key.public_key());
        let joining = lock(&f.invite).is_some();
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
        let share_until = f.membership.read().unwrap_or_else(|e| e.into_inner()).share_until();
        let (need_files, need_bytes) = live.need;
        let connected = self.inner.ctx.registry.peers().iter().filter(|p| p.access(&f.id).is_some()).count() as u32;
        let members = f.members.read().unwrap_or_else(|e| e.into_inner()).len() as u32;
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
            unavailable: live.unavailable,
        }
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
                Ok(()) => {
                    self.drop_journal(&id);
                    lock(&self.inner.held).remove(&id);
                    // Finished or undone: the folder starts where its entry says (a removal forgot it).
                    let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned();
                    if let Some(e) = entry
                        && let Err(err) = self.open_folder(&e).await
                    {
                        eprintln!("seedwell sync: folder {id}: {err}");
                    }
                }
                Err(err) => {
                    lock(&self.inner.held).insert(id, held_of(&err));
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
        let pk = public_key_from_hex(&e.folder_pk);
        let role = pk.and_then(|pk| crate::roles::load(&self.members_path(id), pk).ok()).and_then(|m| m.role(self.inner.ctx.key.public_key()));
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
            unavailable: 0,
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
        if self.is_stopping() || (conn.via != Via::Relay && self.is_blocked(conn.device())) {
            return;
        }
        let _ = crate::peer::run(self.inner.ctx.clone(), conn, candidates).await;
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
        book.by_device.get(&device).is_none_or(|addrs| addrs.iter().all(|a| book.tried.get(a).is_some_and(|t| t.1 > 0)))
    }

    /// The addresses `device`'s signed record lists.
    pub(crate) fn record_addrs(&self, device: DeviceId) -> Vec<String> {
        lock(&self.inner.book).records.get(&device).map(|r| r.addrs.clone()).unwrap_or_default()
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
        self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
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

    /// **Tests only:** how long stopping a folder waits for it to let go.
    #[doc(hidden)]
    pub fn test_stop_wait(&self, wait: Duration) {
        *lock(&self.inner.stop_wait) = wait;
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

    /// Our own signed address record: our port at the address a peer reached us on (`local`).
    fn own_record(&self, local: SocketAddr) -> AddressRecord {
        let addrs = vec![SocketAddr::new(local.ip(), self.inner.port).to_string()];
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
            list.push(crate::config::SpaceDevice { id: id.clone(), name, addresses });
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
            let now = Instant::now();
            if let Some((last, fails)) = book.tried.get(&addr).copied() {
                let due = last + retry_after(fails);
                if now < due {
                    return Some(due);
                }
            }
            let fails = book.tried.get(&addr).map_or(0, |t| t.1);
            book.tried.insert(addr, (now, fails + 1));
        }
        let h = self.clone();
        tokio::spawn(async move {
            // A failed dial is retried with backoff by the dial loop.
            if device.is_some_and(|d| h.is_blocked(d)) {
                return;
            }
            let plan = session::DialPlan { utp: h.inner.utp.as_ref(), tcp: h.inner.dial_tcp };
            if let Ok(conn) = session::dial(&h.inner.ctx.tls, plan, addr, device).await {
                if h.is_blocked(conn.device()) {
                    return;
                }
                lock(&h.inner.book).tried.remove(&addr);
                let dev = conn.device();
                h.learn_address(Some(dev), None, addr);
                h.remember_device(dev, None, None, Some(addr));
                h.run_session(conn, candidates).await;
            }
        });
        None
    }
}

/// How long an address waits after `fails` dials that did not connect: a second at first (a device that is just
/// starting, a busy Mac), then longer, at most five minutes.
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
}

/// Why a folder is held back, for its row: the move cut off waits for the original's disk, or failed otherwise.
fn held_of(err: &SyncError) -> Held {
    let waiting = matches!(err, SyncError::Io(e) if e.kind() == io::ErrorKind::NotFound);
    let why = if waiting { format!("Waiting for the disk with its original files ({err})") } else { format!("An interrupted move could not be finished: {err}") };
    Held { why, waiting }
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
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path)?;
    if let Some(d) = path.parent() {
        trs_platform::fs::sync_dir(d)?;
    }
    Ok(())
}

/// Moves `p` (if it is there) into the folder's archive, as a replaced file is kept.
fn to_archive(fs: &StdFs, p: &str) -> io::Result<()> {
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
        std::fs::create_dir_all(fs.root().join(dir))?;
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
        std::fs::File::open(&tmp)?.sync_all()?;
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
            std::fs::File::open(e.path())?.sync_all()?;
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
        std::fs::copy(src, dst)?;
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
async fn bind_port(ip: IpAddr, saved: u16, utp: bool) -> io::Result<(TcpListener, Option<trs_utp::UtpSocket>, Option<Foreign>)> {
    const TRIES: usize = 8;
    let mut last = None;
    for attempt in 0..TRIES {
        let want = if attempt == 0 { saved } else { 0 };
        let listener = match TcpListener::bind(SocketAddr::new(ip, want)).await {
            Ok(l) => l,
            Err(e) if attempt + 1 < TRIES => {
                last = Some(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        if !utp {
            return Ok((listener, None, None));
        }
        let port = listener.local_addr()?.port();
        let (tx, rx) = mpsc::channel(256);
        let cfg = trs_utp::Config::default();
        match trs_utp::UtpSocket::bind_with_foreign(SocketAddr::new(ip, port), cfg, tx).await {
            Ok(u) => return Ok((listener, Some(u), Some(rx))),
            Err(e) if attempt + 1 < TRIES => last = Some(e),
            Err(e) => {
                eprintln!("seedwell sync: no uTP (UDP port {port}: {e})");
                return Ok((listener, None, None));
            }
        }
    }
    Err(last.unwrap_or_else(|| io::Error::other("could not bind the sync port")))
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
                    let shared: Vec<Arc<Space>> = h.inner.ctx.registry.spaces().into_iter().filter(|f| f.role_of(&p.device_pk).is_some()).collect();
                    let fellow = |pk: &[u8; 32]| shared.iter().any(|f| f.role_of(pk).is_some());
                    let mut records = vec![h.own_record(p.local)];
                    records.extend(lock(&h.inner.book).records.values().filter(|r| DeviceId::of_public_key(&r.device_pk) != d && fellow(&r.device_pk)).take(64).cloned());
                    let _ = p.control.send(Message::Addresses(Addresses { records })).await;
                }
            }
            HubMsg::PeerDown(_) => h.inner.wake_dialer.notify_one(),
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
        // Members that direct dials do not reach: relay servers, hole punching, member relays.
        for d in wanted.keys() {
            crate::relay::reach(&h, *d);
        }
    }
}

/// Announces and looks up each folder's daily DHT topic every 15 minutes.
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
        tokio::time::sleep(Duration::from_secs(15 * 60)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(clashes(&m, true, true), [("Notes/a.txt".to_string(), "Notes/A.txt".to_string()), ("caf\u{e9}.txt".to_string(), "cafe\u{301}.txt".to_string())]);
        assert_eq!(clashes(&m, false, true).len(), 1, "only the forms");
        assert_eq!(clashes(&m, true, false), [("Notes/a.txt".to_string(), "Notes/A.txt".to_string())], "case only (NTFS): the forms are two names");
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
}
