//! State shared by the session and every torrent actor.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use tokio::sync::{broadcast, mpsc};
use trs_storage::DiskIo;

use crate::api::EngineEvent;
use crate::organize::DestReservations;
use crate::policy::{PolicyData, SharedPolicy};
use crate::budget::ConnectionBudget;
use crate::choker::UploadAllocator;
use crate::config::EngineConfig;
use crate::peer::{BufPool, Diag};
use crate::persist::Store;
use crate::ratelimit::{GroupLimits, RateLimiter};
use crate::session::SessionMsg;

/// How many torrents load and validate at once (see `Shared::startup_slots`).
const STARTUP_SLOTS: usize = 8;

/// Everything global that torrent actors use. Cheap to clone behind an `Arc`.
pub(crate) struct Shared {
    cfg: RwLock<Arc<EngineConfig>>,
    pub disk: Arc<DiskIo>,
    pub store: Arc<dyn Store>,
    pub limiter: Arc<RateLimiter>,
    /// Shared speed caps of limited favorite trackers (SPEC 7E.4).
    pub groups: GroupLimits,
    pub budget: ConnectionBudget,
    pub pool: Arc<BufPool>,
    pub alloc: Arc<UploadAllocator>,
    pub diag: Arc<Diag>,
    /// Our peer id: one per process, the same for every torrent and across pause/resume.
    pub our_id: [u8; 20],
    /// The port in use now; shared with every torrent (the extended handshake reads it at send time).
    pub listen_port: Arc<AtomicU32>,
    /// Torrents loading their metadata and checking their resume data at the same time. A seeder
    /// with hundreds of torrents wakes them all at start-up; without a bound that many parses, layouts
    /// and blocking-pool threads exist at once, and the allocator keeps the scatter they leave behind.
    pub startup_slots: Arc<tokio::sync::Semaphore>,
    /// Hashing slots: at most `EngineConfig::verify_slots` torrents run a full verification at
    /// once, whatever started it (add with existing data, Recheck, reconnect after an unplug, the
    /// launch). The session keeps the others without an actor; this is the hard backstop for the
    /// verifications an actor only decides on at start-up (stale resume data).
    pub verify_slots: Arc<tokio::sync::Semaphore>,
    /// A DHT node runs (the handshake advertises the DHT bit only then).
    pub dht_running: std::sync::atomic::AtomicBool,
    /// Set when the wanted port was taken (at start, or by a live change that could not bind).
    pub port_fallback: std::sync::Mutex<Option<crate::api::PortFallback>>,
    /// The last port-mapping answer (`seedwell debug perf`); written when the mapper reports.
    pub portmap_status: std::sync::Mutex<Option<crate::api::PortMapStatus>>,
    /// Incoming TCP connections from public IPv4 addresses on the current port, and the Unix
    /// second of the last one ([`crate::api::Reachability`]). A port change starts them over.
    pub incoming_public: AtomicU64,
    pub last_incoming_public: AtomicU64,
    /// Torrents added by `seedwell sim`: only these may announce to a tracker on 127.0.0.1. Set through one
    /// internal control method (permission full plus Developer mode); a `.torrent` file cannot set it.
    pub simulated: std::sync::Mutex<std::collections::HashSet<crate::api::TorrentId>>,
    /// The loopback streaming server (SPEC 7F), started by the first `stream_url`: until then no
    /// listener and no task exist. Closed at shutdown.
    pub streams: tokio::sync::OnceCell<Arc<trs_stream::StreamServer>>,
    /// The UDP socket shared by uTP and the DHT (`None` when uTP is off or it did not bind). Replaced
    /// by a live port change; torrents hold the same slot and dial through the current socket.
    pub utp: Arc<RwLock<Option<trs_utp::UtpSocket>>>,
    /// Concurrent encrypted handshakes (each costs two modular exponentiations).
    pub mse_slots: tokio::sync::Semaphore,
    /// Peers that did not answer an encrypted handshake but did answer a plain one.
    pub plain_peers: Arc<std::sync::Mutex<crate::net::AddrSet>>,
    /// Peers known to speak uTP.
    pub utp_peers: std::sync::Mutex<crate::net::AddrSet>,
    pub events: broadcast::Sender<EngineEvent>,
    pub session: mpsc::Sender<SessionMsg>,
    /// Rules and tracker profiles; `None` = the engine runs without a policy (tests, tools).
    pub policy: SharedPolicy,
    /// Library paths promised to torrents that are not finalized yet.
    pub reserved: DestReservations,
    /// Baseline of each watched folder that was handed over without one (see
    /// `WatchConfig::since`): the moment it joined the list in this process. Entries of folders
    /// that leave the list are dropped, so re-adding a folder starts a new baseline.
    pub watch_baselines: std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, std::time::SystemTime>>,
    /// Where the `.torrent` file of a torrent that is still waiting for a decision (a choice, or the
    /// grace period) lies, so that Cancel can offer to trash it (SPEC 7A.6). In memory only; an entry
    /// goes when the torrent is released or removed.
    pub held_sources: std::sync::Mutex<std::collections::HashMap<crate::api::TorrentId, std::path::PathBuf>>,
    /// The developer log of tracker exchanges (SPEC 8.7); off unless `developer_mode`.
    pub devlog: Arc<trs_devlog::DevLog>,
    /// Connectivity as the tracker driver decided it (SPEC 7C.4c).
    pub net: crate::netstate::NetFlags,
}

impl Shared {
    pub(crate) fn new(
        cfg: EngineConfig,
        disk: Arc<DiskIo>,
        store: Arc<dyn Store>,
        our_id: [u8; 20],
        events: broadcast::Sender<EngineEvent>,
        session: mpsc::Sender<SessionMsg>,
    ) -> Self {
        let devlog = trs_devlog::DevLog::new(cfg.devlog_dir.clone());
        devlog.set_enabled(cfg.developer_mode);
        Self {
            devlog,
            limiter: Arc::new(RateLimiter::new(cfg.effective_down(), cfg.effective_up())),
            groups: GroupLimits::default(),
            budget: ConnectionBudget::new(cfg.max_connections, cfg.max_half_open),
            pool: Arc::new(BufPool::new(16)),
            alloc: Arc::new(UploadAllocator::new()),
            diag: Arc::new(Diag::default()),
            listen_port: Arc::new(AtomicU32::new(0)),
            startup_slots: Arc::new(tokio::sync::Semaphore::new(STARTUP_SLOTS)),
            verify_slots: Arc::new(tokio::sync::Semaphore::new(cfg.verify_slots.max(1))),
            dht_running: std::sync::atomic::AtomicBool::new(false),
            port_fallback: std::sync::Mutex::new(None),
            portmap_status: std::sync::Mutex::new(None),
            incoming_public: AtomicU64::new(0),
            last_incoming_public: AtomicU64::new(0),
            simulated: std::sync::Mutex::new(std::collections::HashSet::new()),
            streams: tokio::sync::OnceCell::const_new(),
            utp: Arc::new(RwLock::new(None)),
            mse_slots: tokio::sync::Semaphore::new(4),
            plain_peers: Arc::new(std::sync::Mutex::new(crate::net::AddrSet::default())),
            utp_peers: std::sync::Mutex::new(crate::net::AddrSet::default()),
            cfg: RwLock::new(Arc::new(cfg)),
            disk,
            store,
            our_id,
            events,
            session,
            policy: std::sync::RwLock::new(None),
            reserved: std::sync::Mutex::new(std::collections::HashSet::new()),
            watch_baselines: std::sync::Mutex::new(std::collections::HashMap::new()),
            held_sources: std::sync::Mutex::new(std::collections::HashMap::new()),
            net: crate::netstate::NetFlags::default(),
        }
    }

    /// Remember the file a held torrent came from.
    pub(crate) fn hold_source(&self, id: crate::api::TorrentId, path: std::path::PathBuf) {
        self.held_sources.lock().unwrap_or_else(|e| e.into_inner()).insert(id, path);
    }

    /// The torrent is released or gone: forget its file.
    pub(crate) fn release_source(&self, id: &crate::api::TorrentId) {
        self.held_sources.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    /// The installed policy, if any.
    pub(crate) fn policy(&self) -> Option<Arc<PolicyData>> {
        self.policy.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    pub(crate) fn set_policy(&self, p: Option<Arc<PolicyData>>) {
        // The tracker caps follow the profiles at once; torrents holding a bucket see the new rate.
        match &p {
            Some(p) => self.groups.sync(p.profiles.profiles.iter().map(|f| (f.id.as_str(), f.down_cap(), f.up_cap()))),
            None => self.groups.sync([]),
        }
        *self.policy.write().unwrap_or_else(|e| e.into_inner()) = p;
    }

    /// The configuration in effect.
    pub(crate) fn cfg(&self) -> Arc<EngineConfig> {
        self.cfg.read().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Install a new configuration and apply what can be applied in place.
    pub(crate) fn set_cfg(&self, cfg: EngineConfig) {
        self.limiter.set_limits(cfg.effective_down(), cfg.effective_up());
        self.budget.set_caps(cfg.max_connections, cfg.max_half_open);
        // Off stops the log at once: the next exchange already finds the flag cleared.
        self.devlog.set_enabled(cfg.developer_mode);
        *self.cfg.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(cfg);
    }

    /// Wait for one of the start-up slots (`None` only if the semaphore were closed; it never is).
    pub(crate) fn startup_slot(&self) -> impl std::future::Future<Output = Option<tokio::sync::OwnedSemaphorePermit>> + use<> {
        let s = self.startup_slots.clone();
        async move { s.acquire_owned().await.ok() }
    }

    /// Wait for one of the hashing slots.
    pub(crate) fn verify_slot(&self) -> impl std::future::Future<Output = Option<tokio::sync::OwnedSemaphorePermit>> + use<> {
        let s = self.verify_slots.clone();
        async move { s.acquire_owned().await.ok() }
    }

    pub(crate) fn fallback(&self) -> Option<crate::api::PortFallback> {
        self.port_fallback.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) fn set_fallback(&self, f: Option<crate::api::PortFallback>) {
        *self.port_fallback.lock().unwrap_or_else(|e| e.into_inner()) = f;
    }

    pub(crate) fn port(&self) -> u16 {
        u16::try_from(self.listen_port.load(Ordering::Relaxed)).unwrap_or(0)
    }

    pub(crate) fn emit(&self, e: EngineEvent) {
        // No subscribers is normal; the send error only says that.
        let _ = self.events.send(e);
    }
}
