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
use crate::shared::{Ctx, FolderShared, HubMsg, Live, Registry, Space, SpaceKind, lock};
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
    /// LAN discovery's UDP port ([`crate::lan::PORT`]; tests pick their own).
    pub lan_port: u16,
    /// Where every LAN announcement also goes, by unicast (tests: the other services on loopback). With
    /// `bind` on one address these are the only destinations; broadcast and multicast need all interfaces.
    pub lan_targets: Vec<SocketAddr>,
    /// Run uTP on the sync port's UDP twin (dialed with TCP for Internet addresses; hole punching).
    pub utp: bool,
    /// Dial over TCP (tests turn it off to force uTP).
    pub dial_tcp: bool,
    /// Seconds east of UTC at a Unix time (daylight saving included), for the names of conflict copies.
    /// `None`: read once per folder start from `date +%z`.
    pub utc_offset: Option<UtcOffset>,
    /// The protocol versions to speak, lowest and highest ([`trs_sync_proto::msg::MIN_PROTOCOL`],
    /// [`trs_sync_proto::msg::PROTOCOL`]); tests play older or newer devices with others.
    pub protocol: (u32, u32),
    /// The feature bits to offer ([`trs_sync_proto::msg::FEATURES`]).
    pub features: u64,
}

/// The local UTC offset at a Unix time, in seconds (the app passes the engine's).
pub type UtcOffset = Arc<dyn Fn(i64) -> i32 + Send + Sync>;

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
            lan_port: crate::lan::PORT,
            lan_targets: Vec::new(),
            utp: true,
            dial_tcp: true,
            utc_offset: None,
            protocol: (trs_sync_proto::msg::MIN_PROTOCOL, trs_sync_proto::msg::PROTOCOL),
            features: trs_sync_proto::msg::FEATURES,
        }
    }
}

/// Why a request was refused.
#[derive(Debug)]
pub enum SyncError {
    /// No such folder.
    NotFound,
    /// The request is not acceptable (a bad key, a path in use, …), with the reason.
    Invalid(String),
    /// Reading or writing failed.
    Io(io::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::NotFound => f.write_str("no such folder"),
            SyncError::Invalid(why) => f.write_str(why),
            SyncError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<io::Error> for SyncError {
    fn from(e: io::Error) -> Self {
        SyncError::Io(e)
    }
}

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
    /// Its name (from its Hello, or as remembered; empty when never met).
    pub name: String,
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
    /// The local UTC offset, when the caller gave one.
    pub(crate) utc_offset: Option<UtcOffset>,
    /// The runtime the service runs on (its synchronous methods are called from other threads too).
    pub(crate) rt: tokio::runtime::Handle,
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

pub(crate) fn random_id() -> [u8; 16] {
    let mut b = [0u8; 16];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    b
}

/// The UTC offset in seconds, read from `date` once a start (Rust has no local time without `unsafe`;
/// the value only names conflict copies).
fn local_offset_s() -> i32 {
    let out = std::process::Command::new("date").arg("+%z").output();
    let Ok(out) = out else { return 0 };
    let s = String::from_utf8_lossy(&out.stdout);
    let s = s.trim();
    if s.len() != 5 {
        return 0;
    }
    let sign = if s.starts_with('-') { -1 } else { 1 };
    let h: i32 = s[1..3].parse().unwrap_or(0);
    let m: i32 = s[3..5].parse().unwrap_or(0);
    sign * (h * 3600 + m * 60)
}

/// The system clock with the caller's time zone (the offset follows daylight saving).
struct ZonedClock(UtcOffset);

impl trs_sync::clock::Clock for ZonedClock {
    fn now_ns(&self) -> i64 {
        trs_sync::clock::Clock::now_ns(&SystemClock::default())
    }

    fn local_offset_s(&self, unix_ns: i64) -> i32 {
        (self.0)(unix_ns.div_euclid(1_000_000_000))
    }
}

impl SyncHandle {
    /// Starts the service: loads the device key (made on first start), the folders, binds the port.
    pub async fn start(opts: Options) -> io::Result<SyncHandle> {
        let dir = opts.state_dir.join("sync");
        std::fs::create_dir_all(&dir)?;
        let cfg_path = opts.state_dir.join("sync.toml");
        let mut cfg = SyncConfig::load(&cfg_path)?;

        let key = match opts.secrets.get(DEVICE_KEY)? {
            Some(pkcs8) => DeviceKey::from_pkcs8(&pkcs8).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("device key: {e:?}")))?,
            None => {
                let k = DeviceKey::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
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
        let direct = crate::direct::Direct::open(&dir, &key, &cfg.direct)?;
        let ctx = Arc::new(Ctx {
            me: key.device_id(),
            key,
            client: opts.client.clone(),
            device_name: Mutex::new(name),
            tls,
            registry: Registry::default(),
            hub: hub_tx,
            limits: crate::limit::Limits::default(),
            secrets: opts.secrets.clone(),
            protocol: opts.protocol,
            features: opts.features,
            chat_events: tokio::sync::broadcast::channel(1024).0,
            direct,
        });
        ctx.limits.set_kib(cfg.settings.up_kib, cfg.settings.down_kib);
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
            utc_offset: opts.utc_offset,
            rt: tokio::runtime::Handle::current(),
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
            eprintln!("seedwell sync: channel {} was made by a test build; it is no longer on this Mac", e.name);
        }
        let entries = lock(&h.inner.cfg).folders.clone();
        for e in entries {
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
        Ok(h)
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
        let writers = membership.writers();
        let root = e.path.clone();
        let index = self.index_path(&id);
        let ctx = self.inner.ctx.clone();
        let entry = e.clone();
        let tz = self.inner.utc_offset.clone();
        let core = tokio::task::spawn_blocking(move || -> Result<(FolderCore, RedbReader), SyncError> {
            let fs = StdFs::new(root.clone());
            let case_insensitive = match fs.lstat(STATE_DIR) {
                Ok(Some(_)) => trs_sync::path::probe_case_insensitive(&fs).unwrap_or(true),
                _ => cfg!(target_os = "macos"),
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
                archive: entry.archive,
            };
            let ignore = load_ignore(&fs, &entry.ignore);
            let clock: Arc<dyn trs_sync::clock::Clock> = match tz {
                Some(tz) => Arc::new(ZonedClock(tz)),
                None => Arc::new(SystemClock { offset_s: local_offset_s() }),
            };
            let mut core = FolderCore::new(cfg, Box::new(store), Arc::new(fs), Some(ctx.key.clone()), clock, Arc::new(random_id), ignore)
                .map_err(|e| invalid(e.to_string()))?;
            core.set_membership(&writers, access).map_err(|e| invalid(e.to_string()))?;
            Ok((core, reader))
        })
        .await
        .map_err(io::Error::other)??;
        let (core, reader) = core;

        let (seq_tx, _) = watch::channel(core.index_state().map(|(_, s)| s).unwrap_or(0));
        let name = e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let space = Space::new(id, SpaceKind::Folder, FolderKey::Member(pk), secret, membership, members_path, invite, links, name);
        space.paused.store(e.paused, Ordering::Relaxed);
        space.lan_only.store(e.lan_only, Ordering::Relaxed);
        let shared = Arc::new(FolderShared {
            space: Arc::new(space),
            root: e.path.clone(),
            core: Mutex::new(core),
            index: reader,
            local_seq: seq_tx,
            remote_changed: Notify::new(),
            live: Mutex::new(Live::default()),
            rescan: Notify::new(),
            retention: Retention { max_age_s: i64::from(e.archive_days) * 86_400, ..Retention::default() },
            fetched: crate::limit::Meter::default(),
            served: crate::limit::Meter::default(),
            closed: crate::shared::Closed::default(),
        });
        // Devices met in this folder before are members to dial, at the addresses they had.
        {
            let cfg = lock(&self.inner.cfg);
            let mut members = shared.members.write().unwrap_or_else(|e| e.into_inner());
            let mut book = lock(&self.inner.book);
            for d in cfg.devices.iter().filter(|d| d.folders.contains(&e.id)) {
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
        for p in self.inner.ctx.registry.peers() {
            if p.access(&folder).is_some() {
                continue;
            }
            let join = crate::peer::own_join(&self.inner.ctx, &[folder], &p.exporter, p.device, p.lan).await;
            if !join.folders.is_empty() {
                let _ = p.control.send(Message::Join(join)).await;
            }
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
        self.refuse_overlap(&path)?;
        let secret = FolderSecret::generate(&mut ring::rand::SystemRandom::new()).map_err(io::Error::other)?;
        let id = FolderId::of_public_key(secret.public_key());
        let me = *self.inner.ctx.key.public_key();
        let grant = Statement::by_owner(&secret, Body::Grant { device: me, role: Role::Owner, serial: 1, at_s: crate::roles::now_s(), invite: None });
        let mut m = Membership::new(*secret.public_key());
        m.add([grant]);
        crate::roles::save(&self.members_path(&id), &m)?;
        self.inner.secrets.put(&crate::roles::secret_name(&id), secret.as_bytes())?;
        self.install(*secret.public_key(), path, None).await
    }

    /// Joins a shared folder with an invitation link, at `path` (created when missing). The folder waits until
    /// one of its owners or admins is online and admits this device.
    ///
    /// A link for a folder and its channel joins both (the channel only when it is not here yet) and links them on
    /// this device. Both invitations are checked first: an expired one refuses the whole join; one that is used up
    /// or revoked is refused by whoever admits, for each.
    pub async fn join_folder(&self, key_or_link: &str, path: PathBuf) -> Result<FolderStatus, SyncError> {
        let (link, chat) = InviteLink::parse_bundle(key_or_link).map_err(|e| invalid(e.to_string()))?;
        let now = crate::roles::now_s();
        if link.invite.expired(now) || chat.as_ref().is_some_and(|c| c.invite.expired(now)) {
            return Err(invalid("this link has expired"));
        }
        let path = absolute(&path)?;
        refuse_cloud_root(&path)?;
        self.refuse_overlap(&path)?;
        let id = link.invite.folder_id();
        if self.inner.ctx.registry.folder(&id).is_some() {
            return Err(invalid("this folder is already synced on this Mac"));
        }
        let channel = chat.as_ref().map(|c| c.invite.folder_id());
        if let Some(c) = chat
            && self.inner.ctx.registry.channel(&c.invite.folder_id()).is_none()
        {
            self.chat_join_link(c).await?;
        }
        std::fs::create_dir_all(&path)?;
        self.inner.secrets.put(&crate::roles::invite_name(&id), link.format().as_bytes())?;
        self.install(link.invite.folder_pk, path, channel).await
    }

    async fn install(&self, folder_pk: [u8; 32], path: PathBuf, channel: Option<FolderId>) -> Result<FolderStatus, SyncError> {
        let id = FolderId::of_public_key(&folder_pk);
        std::fs::create_dir_all(path.join(STATE_DIR))?;
        let mut entry = FolderEntry::new(id.to_string(), path);
        entry.folder_pk = hex(&folder_pk);
        entry.channel = channel.map(|c| c.to_string()).unwrap_or_default();
        {
            let mut cfg = lock(&self.inner.cfg);
            cfg.folders.retain(|f| f.id != entry.id);
            cfg.folders.push(entry.clone());
        }
        self.save_config()?;
        self.open_folder(&entry).await?;
        self.folder_status(&id).ok_or(SyncError::NotFound)
    }

    /// Stops syncing a folder on this device. The files stay; its index and `.seedwell` go. (Moving the
    /// files to the Trash is the app's job, which knows the user's Trash.)
    pub async fn remove_folder(&self, id: &FolderId) -> Result<PathBuf, SyncError> {
        let path = match self.stop_folder(id).await {
            // Still open: its index file goes all the same (an open file can be removed), the rest as usual.
            Ok((p, _)) => p,
            // In the configuration but not running (it could not open): only its entry and state go.
            Err(SyncError::NotFound) => lock(&self.inner.cfg).folder(&id.to_string()).map(|e| e.path.clone()).ok_or(SyncError::NotFound)?,
            Err(e) => return Err(e),
        };
        {
            let mut cfg = lock(&self.inner.cfg);
            cfg.folders.retain(|f| f.id != id.to_string());
        }
        self.save_config()?;
        for name in [crate::roles::secret_name(id), crate::roles::invite_name(id), crate::roles::links_name(id)] {
            self.inner.secrets.delete(&name)?;
        }
        let _ = std::fs::remove_file(self.index_path(id));
        let _ = std::fs::remove_file(self.members_path(id));
        let state = path.join(STATE_DIR);
        // The archive may hold the only copy of something: keep it, only the marker's other parts go.
        if state.join("Archive").exists() {
            let _ = std::fs::remove_file(state.join("case-probe.tmp"));
        } else {
            let _ = std::fs::remove_dir_all(&state);
        }
        Ok(path)
    }

    /// Stops a folder's runtime and takes it out of the registry. Its root, and a flag that is up once its index is
    /// closed: it waits (a minute at most) until nothing holds the folder any more, which on a loaded Mac takes a while.
    async fn stop_folder(&self, id: &FolderId) -> Result<(PathBuf, Arc<AtomicBool>), SyncError> {
        let f = self.inner.ctx.registry.remove_folder(id).ok_or(SyncError::NotFound)?;
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
        let new_path = absolute(&new_path)?;
        if new_path.exists() {
            return Err(invalid(format!("{} already exists", new_path.display())));
        }
        refuse_cloud_root(&new_path)?;
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if new_path.starts_with(&entry.path) {
            return Err(invalid("a folder cannot move into itself"));
        }
        let (_, closed) = self.stop_folder(id).await?;
        if !closed.load(Ordering::SeqCst) {
            // Its core still runs somewhere (a scan on a loaded Mac): moving the files under it would read as
            // deletions. They stay, and the folder starts again where it is once its index is free.
            let (me, entry) = (self.clone(), entry.clone());
            tokio::spawn(async move {
                let end = Instant::now() + Duration::from_secs(600);
                while !closed.load(Ordering::SeqCst) && Instant::now() < end {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                if let Err(e) = me.open_folder(&entry).await {
                    eprintln!("seedwell sync: {} could not start again after a refused move: {e}", entry.path.display());
                }
            });
            return Err(invalid("the folder is still busy (a scan or a transfer); it stays where it is, try again in a moment"));
        }
        let from = entry.path.clone();
        let to = new_path.clone();
        let moved = tokio::task::spawn_blocking(move || move_tree(&from, &to)).await.map_err(io::Error::other)?;
        let mut entry2 = entry.clone();
        if moved.is_ok() {
            entry2.path = new_path;
            if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
                e.path = entry2.path.clone();
            }
            self.save_config()?;
        }
        // Back to syncing, at the new place or (if the move failed) the old one.
        self.open_folder(&entry2).await?;
        moved?;
        self.folder_status(id).ok_or(SyncError::NotFound)
    }

    /// Renames a folder on this Mac (its directory). Other devices keep their own names.
    pub async fn rename_folder(&self, id: &FolderId, name: &str) -> Result<FolderStatus, SyncError> {
        let name = name.trim();
        if name.is_empty() || name.contains('/') || name == "." || name == ".." || name.starts_with('.') {
            return Err(invalid("not a usable folder name"));
        }
        let path = lock(&self.inner.cfg).folder(&id.to_string()).map(|e| e.path.clone()).ok_or(SyncError::NotFound)?;
        let parent = path.parent().ok_or_else(|| invalid("the folder has no parent"))?;
        self.move_folder(id, parent.join(name)).await
    }

    /// Pauses or resumes a folder.
    pub async fn set_paused(&self, id: &FolderId, paused: bool) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        f.paused.store(paused, Ordering::Relaxed);
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.paused = paused;
        }
        self.save_config()?;
        if paused {
            lock(&f.live).state = FolderState::Paused;
        } else {
            lock(&f.live).state = FolderState::Starting;
            f.rescan.notify_one();
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
        tokio::task::spawn_blocking(move || -> io::Result<()> {
            let copy = copy_path;
            let fs = StdFs::new(root);
            if fs.lstat(STATE_DIR)?.is_none() {
                return Err(io::Error::other("the folder is missing"));
            }
            let stamp = trs_sync::clock::compact_stamp(trs_sync::clock::Clock::now_ns(&SystemClock::default()));
            let to_archive = |p: &str| -> io::Result<()> {
                if fs.lstat(p)?.is_none() {
                    return Ok(());
                }
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
            };
            match keep {
                Keep::Original => to_archive(&copy),
                Keep::Copy => {
                    to_archive(&original)?;
                    fs.rename(&copy, &original)
                }
                Keep::Both => {
                    // The copy becomes an ordinary file: "Report (MacBook, 2026-10-25 14.32).docx".
                    let at = copy.rfind(" (conflict, ").ok_or_else(|| io::Error::other("not a conflict copy"))?;
                    let base = format!("{} ({}", &copy[..at], &copy[at + " (conflict, ".len()..]);
                    let mut name = base.clone();
                    let mut n = 2;
                    while fs.lstat(&name)?.is_some() {
                        name = match base.rsplit_once('.') {
                            Some((stem, ext)) if !ext.contains(['/', ' ', ')']) => format!("{stem} {n}.{ext}"),
                            _ => format!("{base} {n}"),
                        };
                        n += 1;
                    }
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
        let remembered: BTreeMap<String, String> = lock(&self.inner.cfg).devices.iter().map(|d| (d.id.clone(), d.name.clone())).collect();
        let my_name = lock(&self.inner.ctx.device_name).clone();
        let m = f.membership.read().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<MemberInfo> = m
            .members()
            .map(|(pk, mem)| {
                let device = DeviceId::of_public_key(pk);
                let name = if device == me {
                    my_name.clone()
                } else {
                    online.get(&device).cloned().or_else(|| remembered.get(&device.to_string()).cloned()).unwrap_or_default()
                };
                MemberInfo {
                    device,
                    name,
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
        self.make_invite(id, role, max_uses, valid_days, None)
    }

    /// Makes one link for a folder and a channel: an invitation to each, with the same role, uses and expiry. This
    /// device must be allowed to share both at that role (both are checked before either is made). The folder is
    /// linked to the channel here when it has no channel yet.
    pub fn create_bundle_invite(&self, id: &FolderId, role: Role, max_uses: u32, valid_days: u32, channel: &FolderId) -> Result<InviteInfo, SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        let c = self.inner.ctx.registry.channel(channel).ok_or_else(|| invalid("no such channel on this Mac"))?;
        self.may_invite(&f, role, max_uses)?;
        let name = lock(&c.name).clone();
        self.may_invite(&c, role, max_uses).map_err(|e| invalid(format!("this Mac may not share the channel {name} at that role ({e})")))?;
        let ci = self.make_invite(channel, role, max_uses, valid_days, None)?;
        let cl = InviteLink::parse_chat(&ci.link).map_err(|e| invalid(e.to_string()))?;
        let fi = match self.make_invite(id, role, max_uses, valid_days, Some(&cl)) {
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
            return Err(invalid("no such channel on this Mac"));
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
            return Err(invalid(format!("this Mac is an owner but has not received the {noun}'s owner key yet")));
        }
        Ok(())
    }

    /// Makes an invitation (see [`SyncHandle::create_invite`]); with `bundle`, the folder's link also carries that
    /// channel's invitation.
    fn make_invite(&self, id: &FolderId, role: Role, max_uses: u32, valid_days: u32, bundle: Option<&InviteLink>) -> Result<InviteInfo, SyncError> {
        let f = self.inner.ctx.registry.space(id).ok_or(SyncError::NotFound)?;
        self.may_invite(&f, role, max_uses)?;
        let me = *self.inner.ctx.key.public_key();
        let mine = f.role_of(&me);
        let now = crate::roles::now_s();
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
        let st = st.ok_or_else(|| invalid(format!("this Mac has not received the {}'s owner key yet", crate::roles::noun(&f))))?;
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
                SpaceKind::Folder => "to stop syncing on this Mac, remove the folder instead",
                SpaceKind::Channel => "to leave the channel on this Mac, leave it instead",
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

    /// Changes the device-wide settings (the port takes effect at the next start).
    pub fn set_settings(&self, s: Settings) -> Result<(), SyncError> {
        if !s.device_name.is_empty() {
            *lock(&self.inner.ctx.device_name) = s.device_name.chars().take(64).collect();
        }
        self.inner.ctx.limits.set_kib(s.up_kib, s.down_kib);
        lock(&self.inner.cfg).settings = s;
        self.save_config()?;
        Ok(())
    }

    /// Syncs a folder only with devices on the local network (or everywhere again). Sessions with other
    /// devices stop sharing it at once; they keep the folders that are still allowed.
    pub fn set_lan_only(&self, id: &FolderId, lan_only: bool) -> Result<(), SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        f.lan_only.store(lan_only, Ordering::Relaxed);
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.lan_only = lan_only;
        }
        self.save_config()?;
        if lan_only {
            for p in self.inner.ctx.registry.peers().into_iter().filter(|p| !p.lan) {
                p.folders.write().unwrap_or_else(|e| e.into_inner()).remove(id);
            }
        } else {
            self.inner.wake_dialer.notify_one();
        }
        Ok(())
    }

    fn refuse_overlap(&self, path: &Path) -> Result<(), SyncError> {
        for e in &lock(&self.inner.cfg).folders {
            if path.starts_with(&e.path) || e.path.starts_with(path) {
                return Err(invalid(format!("{} overlaps the synced folder {}", path.display(), e.path.display())));
            }
        }
        Ok(())
    }

    fn folder_status(&self, id: &FolderId) -> Option<FolderStatus> {
        let f = self.inner.ctx.registry.folder(id)?;
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
            live.error = Some(if ended { "this folder is no longer shared" } else { "this Mac was removed from the folder" }.into());
            FolderState::Error
        };
        let share_until = f.membership.read().unwrap_or_else(|e| e.into_inner()).share_until();
        let (need_files, need_bytes) = live.need;
        let connected = self.inner.ctx.registry.peers().iter().filter(|p| p.access(&f.id).is_some()).count() as u32;
        let members = f.members.read().unwrap_or_else(|e| e.into_inner()).len() as u32;
        FolderStatus {
            id: f.id.to_string(),
            name: lock(&f.name).clone(),
            path: f.root.display().to_string(),
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
        }
    }

    /// Everything, for the UI.
    pub fn snapshot(&self) -> Snapshot {
        let folders = self.inner.ctx.registry.folders().iter().map(|f| self.status_of(f)).collect();
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

    /// **Tests only:** how long stopping a folder waits for it to let go.
    #[doc(hidden)]
    pub fn test_stop_wait(&self, wait: Duration) {
        *lock(&self.inner.stop_wait) = wait;
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

    /// Remembers a device met in folders (its name, folders and an address), in `sync.toml`.
    fn remember_device(&self, device: DeviceId, name: Option<&str>, folder: Option<FolderId>, addr: Option<SocketAddr>) {
        let mut cfg = lock(&self.inner.cfg);
        let d = cfg.device_mut(&device.to_string());
        let mut changed = false;
        if let Some(n) = name.filter(|n| !n.is_empty() && *n != d.name) {
            d.name = n.to_string();
            changed = true;
        }
        if let Some(f) = folder.map(|f| f.to_string()).filter(|f| !d.folders.contains(f)) {
            d.folders.push(f);
            changed = true;
        }
        if let Some(a) = addr.map(|a| a.to_string()).filter(|a| d.addresses.first() != Some(a)) {
            d.addresses.retain(|x| *x != a);
            d.addresses.insert(0, a);
            d.addresses.truncate(8);
            changed = true;
        }
        drop(cfg);
        if changed {
            let _ = self.save_config();
        }
    }

    /// Dials `addr` (for a known `device`, or for the folders in `candidates`), unless its backoff holds it back:
    /// then when it may be dialed again.
    fn dial(&self, addr: SocketAddr, device: Option<DeviceId>, candidates: Vec<FolderId>) -> Option<Instant> {
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
fn refuse_cloud_root(p: &Path) -> Result<(), SyncError> {
    let s = p.to_string_lossy();
    if s.contains("/Library/Mobile Documents") || s.contains("/Library/CloudStorage") {
        return Err(invalid("folders in iCloud Drive or other cloud storage cannot be synced"));
    }
    Ok(())
}

fn load_ignore(fs: &StdFs, local: &[String]) -> Ignore {
    let synced = trs_sync::fs::read_all(fs, ".seedwellignore").ok().and_then(|b| String::from_utf8(b).ok()).unwrap_or_default();
    let local = local.join("\n");
    Ignore::parse_all(&[&synced, &local]).0
}

/// Moves a directory: a rename on the same volume, else a copy then a delete of the original.
fn move_tree(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(e) if e.raw_os_error() == Some(18) => {
            // EXDEV: another volume.
            copy_tree(from, to)?;
            std::fs::remove_dir_all(from)
        }
        Err(e) => Err(e),
    }
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let t = e.file_type()?;
        if t.is_dir() {
            copy_tree(&src, &dst)?;
        } else if t.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else {
            std::fs::copy(&src, &dst)?;
            let m = std::fs::metadata(&src)?;
            if let Ok(t) = m.modified() {
                std::fs::File::options().write(true).open(&dst)?.set_modified(t)?;
            }
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
                // Tell the new peer where we are, and pass on what we know of other devices (peer
                // exchange: records are signed by their devices, so passing them on cannot forge them).
                if let Some(p) = h.inner.ctx.registry.peer(&d) {
                    h.remember_device(d, Some(&p.name), None, None);
                    let mut records = vec![h.own_record(p.local)];
                    records.extend(lock(&h.inner.book).records.values().filter(|r| DeviceId::of_public_key(&r.device_pk) != d).take(64).cloned());
                    let _ = p.control.send(Message::Addresses(Addresses { records })).await;
                }
            }
            HubMsg::PeerDown(_) => h.inner.wake_dialer.notify_one(),
            HubMsg::MemberSeen { folder, device } => h.remember_device(device, None, Some(folder), None),
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
                    for a in &r.addrs {
                        if let Ok(sa) = a.parse::<SocketAddr>() {
                            h.learn_address(Some(dev), None, sa);
                        }
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
