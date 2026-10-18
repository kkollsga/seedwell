//! `Engine` (start/stop) and `EngineHandle` (the control API).

use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, oneshot};
use trs_storage::{DiskConfig, DiskIo};
use trs_tracker::Millis;
use trs_tracker::crash::CrashLoopGuard;
use trs_wire::{HANDSHAKE_LEN, Handshake};

use crate::api::*;
use crate::config::{AfterAdd, EncryptionPolicy, EngineConfig};
use crate::net::PeerIo;
use crate::organize::{self, PlanCtx, Prepared};
use crate::persist::Store;
use crate::policy::PolicyData;
use crate::session::{Cmd, Session, SessionMsg};
use crate::shared::Shared;
use crate::util::{random_key, unix_ms};

/// A running engine. Keep it alive for the life of the app.
pub struct Engine {
    handle: EngineHandle,
    session: tokio::task::JoinHandle<()>,
    runtime: Option<tokio::runtime::Runtime>,
}

/// Cloneable, `Send + Sync` control surface: commands never block, queries are `async`.
#[derive(Clone)]
pub struct EngineHandle {
    tx: mpsc::Sender<SessionMsg>,
    events: broadcast::Sender<EngineEvent>,
    pub(crate) sh: Arc<Shared>,
    rt: tokio::runtime::Handle,
}

/// Runtime sized as ENGINE.md 1 says: `min(4, performance cores)` workers.
pub fn build_runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(trs_storage::perf_core_count().clamp(1, 4))
        .thread_name("trs-rt")
        .enable_all()
        .build()
}

impl Engine {
    /// Start on the current tokio runtime, without rules or tracker profiles.
    pub async fn start(config: EngineConfig, store: Arc<dyn Store>) -> Result<Engine, EngineError> {
        Self::start_inner(config, store, None).await
    }

    /// Start with rules and tracker profiles in force from the first moment.
    pub async fn start_with_policy(
        config: EngineConfig,
        store: Arc<dyn Store>,
        policy: PolicyData,
    ) -> Result<Engine, EngineError> {
        Self::start_inner(config, store, Some(Arc::new(policy))).await
    }

    /// Start with a private runtime (for callers that have none, such as the FFI layer).
    pub fn start_blocking(config: EngineConfig, store: Arc<dyn Store>) -> Result<Engine, EngineError> {
        let rt = build_runtime().map_err(|e| EngineError::Io(e.to_string()))?;
        let handle = rt.handle().clone();
        let mut e = handle.block_on(Self::start_inner(config, store, None))?;
        e.runtime = Some(rt);
        Ok(e)
    }

    /// [`Engine::start_blocking`] with a policy.
    pub fn start_blocking_with_policy(
        config: EngineConfig,
        store: Arc<dyn Store>,
        policy: PolicyData,
    ) -> Result<Engine, EngineError> {
        let rt = build_runtime().map_err(|e| EngineError::Io(e.to_string()))?;
        let handle = rt.handle().clone();
        let mut e = handle.block_on(Self::start_inner(config, store, Some(Arc::new(policy))))?;
        e.runtime = Some(rt);
        Ok(e)
    }

    async fn start_inner(
        config: EngineConfig,
        store: Arc<dyn Store>,
        policy: Option<Arc<PolicyData>>,
    ) -> Result<Engine, EngineError> {
        crate::guard::check_config(&config).map_err(EngineError::Io)?;
        let now_ms = unix_ms();
        // Crash-loop breaker (SPEC 7C.2): 3 launches in 10 minutes hold the announces.
        let launches = store.record_launch(now_ms).unwrap_or_default();
        let launches: Vec<Millis> = launches.into_iter().map(Millis).collect();
        let hold = CrashLoopGuard::default().evaluate(&launches, Millis(now_ms));

        let _ = trs_storage::raise_nofile_limit(8192);
        let hash_threads = if config.hash_threads == 0 {
            trs_storage::perf_core_count().saturating_sub(1).max(1)
        } else {
            config.hash_threads
        };
        let disk = Arc::new(DiskIo::new(DiskConfig {
            io_threads: config.io_threads,
            hash_threads,
            write_cache_bytes: config.write_cache_bytes,
            fd_cache_capacity: config.fd_cache,
            ..DiskConfig::default()
        }));
        let (stx, srx) = mpsc::channel(4096);
        let (events, _) = broadcast::channel(1024);
        let our_id = trs_metainfo::PeerId::generate().0;
        let key = random_key();
        let cfg_copy = config.clone();
        let sh = Arc::new(Shared::new(config, disk, store.clone(), our_id, events.clone(), stx.clone()));
        sh.set_policy(policy);

        // Listener first: announces carry the port.
        let Bound { listener, port, utp, fallback } = bind_ports(&cfg_copy).await?;
        sh.listen_port.store(u32::from(port), Ordering::Relaxed);
        if let Some(f) = fallback {
            let _ = sh.port_fallback.set(f);
        }
        let accept = tokio::spawn(accept_loop(listener, sh.clone()));

        // The UDP socket on the listen port serves uTP and (below) the DHT.
        let mut dht_feed = None;
        let mut utp_accept = None;
        if let Some((sock, frx)) = utp {
            let _ = sh.utp.set(sock.clone());
            utp_accept = Some(tokio::spawn(utp_accept_loop(sock, sh.clone())));
            dht_feed = Some(frx);
        }

        let tracker = cfg_copy
            .trackers_enabled
            .then(|| crate::tracker_driver::spawn(sh.clone(), store.timing_store(), key, hold));

        let share = |bind: std::net::SocketAddrV4| bind.port() == 0 || bind.port() == port || bind.port() == cfg_copy.listen_port;
        let dht = if cfg_copy.dht_enabled {
            match (&sh.utp.get(), dht_feed) {
                (Some(sock), Some(feed)) if share(cfg_copy.dht_bind) => Some(spawn_dht_shared(&cfg_copy, &*store, (*sock).clone(), feed, alive_of(&sh))),
                _ => spawn_dht(&cfg_copy, &*store, alive_of(&sh)).await,
            }
        } else {
            None
        };

        sh.dht_running.store(dht.is_some(), Ordering::Relaxed);

        let stored = store.load_torrents().map_err(|e| EngineError::Store(e.to_string()))?;
        let directs = store.load_directs().map_err(|e| EngineError::Store(e.to_string()))?;
        let mut session = Session::new(sh.clone(), srx, tracker, dht);
        session.listener = Some(accept);
        session.utp_listener = utp_accept;
        session.load(stored);
        session.load_directs(directs);
        session.start_services();
        let task = tokio::spawn(session.run());
        let handle = EngineHandle { tx: stx, events, sh, rt: tokio::runtime::Handle::current() };
        Ok(Engine { handle, session: task, runtime: None })
    }

    /// The control handle.
    pub fn handle(&self) -> EngineHandle {
        self.handle.clone()
    }

    /// Orderly shutdown: close tracker sessions (`stopped`), persist everything, stop the session.
    /// `deadline` bounds the wait for tracker answers (SPEC 12 #16: up to 120 s hidden, 3 s at logout).
    pub async fn shutdown(self, deadline: Duration) -> ShutdownReport {
        let r = self.handle.shutdown(deadline).await;
        let _ = self.session.await;
        r
    }

    /// Blocking form of [`Engine::shutdown`] for callers outside a runtime.
    pub fn shutdown_blocking(self, deadline: Duration) -> ShutdownReport {
        let Engine { handle, session, runtime } = self;
        let report = match &runtime {
            Some(rt) => rt.block_on(async {
                let r = handle.shutdown(deadline).await;
                let _ = session.await;
                r
            }),
            None => handle.rt.block_on(async {
                let r = handle.shutdown(deadline).await;
                let _ = session.await;
                r
            }),
        };
        if let Some(rt) = runtime {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
        report
    }
}

/// Every DHT datagram is a sign that the network works (SPEC 7C.4c).
fn alive_of(sh: &Arc<Shared>) -> trs_dht::Alive {
    let sh = sh.clone();
    trs_dht::Alive(Arc::new(move || sh.net.alive()))
}

fn dht_config(cfg: &EngineConfig, store: &dyn Store, alive: trs_dht::Alive) -> trs_dht::DriverConfig {
    let mut dc = trs_dht::DriverConfig {
        bind: cfg.dht_bind,
        saved_state: store.load_dht_state().ok().flatten(),
        alive: Some(alive),
        ..trs_dht::DriverConfig::default()
    };
    dc.core.allow_local_addrs = cfg.dht_allow_local;
    if !cfg.dht_bootstrap.is_empty() {
        dc.bootstrap_hosts = cfg.dht_bootstrap.clone();
    }
    dc
}

async fn spawn_dht(cfg: &EngineConfig, store: &dyn Store, alive: trs_dht::Alive) -> Option<trs_dht::DhtHandle> {
    trs_dht::spawn(dht_config(cfg, store, alive)).await.ok()
}

/// The DHT on the uTP socket: every datagram that is not uTP arrives on `feed` and the node
/// answers from the same port, so uTP, the DHT and port mapping all use one UDP port.
fn spawn_dht_shared(
    cfg: &EngineConfig,
    store: &dyn Store,
    sock: trs_utp::UtpSocket,
    mut feed: mpsc::Receiver<trs_utp::ForeignDatagram>,
    alive: trs_dht::Alive,
) -> trs_dht::DhtHandle {
    let (tx, rx) = mpsc::channel(256);
    // The DHT is IPv4 only; the pump drops anything else.
    tokio::spawn(async move {
        while let Some(d) = feed.recv().await {
            if let SocketAddr::V4(from) = d.from
                && tx.send((from, d.data)).await.is_err() {
                    break;
                }
        }
    });
    let out = sock.clone();
    let send: trs_dht::SharedSend = Arc::new(move |to, data| {
        let _ = out.try_send_foreign(SocketAddr::V4(to), data);
    });
    let io = trs_dht::SharedIo { incoming: rx, send, local: sock.local_addr() };
    trs_dht::spawn_shared(dht_config(cfg, store, alive), io)
}

struct Bound {
    listener: TcpListener,
    port: u16,
    utp: Option<(trs_utp::UtpSocket, mpsc::Receiver<trs_utp::ForeignDatagram>)>,
    fallback: Option<PortFallback>,
}

/// Bind the TCP listener and, with `network.utp`, the UDP socket of the same number.
///
/// A configured port that is taken (another client still running on it) does not stop the engine:
/// it listens on a free one and says so ([`PortFallback`]); the configuration keeps the user's
/// choice. When the port is chosen by the OS and its UDP twin is taken, another number is tried.
async fn bind_ports(cfg: &EngineConfig) -> Result<Bound, EngineError> {
    let wanted = cfg.listen_port;
    let mut want = wanted;
    let mut last = String::new();
    for attempt in 0..8 {
        let listener = match TcpListener::bind((cfg.listen_addr, want)).await {
            Ok(l) => l,
            Err(e) => {
                last = e.to_string();
                if want == 0 {
                    break;
                }
                want = 0;
                continue;
            }
        };
        let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
        let fallback = (wanted != 0 && port != wanted).then(|| PortFallback {
            wanted,
            using: port,
            message: format!("Port {wanted} is in use by another app; using {port} for now"),
        });
        if !cfg.utp {
            return Ok(Bound { listener, port, utp: None, fallback });
        }
        let (ftx, frx) = mpsc::channel(256);
        let bind = SocketAddr::new(cfg.listen_addr.into(), port);
        // A seeder of hundreds of torrents has one connection per torrent from the same remote
        // IP (the library default of 32 per IP would refuse them); the engine's own budget
        // still caps the total.
        let ucfg = trs_utp::Config { max_per_ip: 256, ..trs_utp::Config::default() };
        match trs_utp::UtpSocket::bind_with_foreign(bind, ucfg, ftx).await {
            Ok(sock) => return Ok(Bound { listener, port, utp: Some((sock, frx)), fallback }),
            // The OS chose the TCP port and somebody has the same UDP port: choose again.
            Err(_) if want == 0 && attempt < 7 => continue,
            // A fixed port whose UDP side is taken: run on TCP only rather than not at all.
            Err(e) => {
                eprintln!("trs-engine: uTP disabled, cannot bind UDP {bind}: {e}");
                return Ok(Bound { listener, port, utp: None, fallback });
            }
        }
    }
    Err(EngineError::Io(format!("cannot listen: {last}")))
}

/// Accept TCP peers.
async fn accept_loop(listener: TcpListener, sh: Arc<Shared>) {
    loop {
        let (stream, addr) = match listener.accept().await {
            Ok(x) => x,
            Err(_) => {
                // Out of descriptors or a transient error: back off instead of spinning.
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        admit(&sh, PeerIo::Tcp(stream), addr);
    }
}

/// Accept uTP peers from the shared UDP socket (when `network.utp` is off they are dropped).
async fn utp_accept_loop(sock: trs_utp::UtpSocket, sh: Arc<Shared>) {
    loop {
        match sock.accept().await {
            Ok((stream, addr)) => {
                if sh.cfg().utp {
                    admit(&sh, PeerIo::Utp(stream), addr);
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Count the connection against the budget, read the 68-byte handshake with a deadline and hand
/// the socket to the session, which finds the torrent and wakes it if it is Cold. Both
/// transports share the budget, so uTP cannot exceed the connection caps.
fn admit(sh: &Arc<Shared>, stream: PeerIo, addr: SocketAddr) {
    // Somebody reached us: the network works (and, during an outage, worth a probe).
    sh.net.alive();
    let Some(permit) = sh.budget.try_connection() else {
        sh.diag.incoming_refused.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(half) = sh.budget.try_half_open() else {
        sh.diag.incoming_refused.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let sh2 = sh.clone();
    tokio::spawn(async move {
        let timeout = sh2.cfg().handshake_timeout;
        let res = tokio::time::timeout(timeout, accept_handshake(stream, &sh2)).await;
        drop(half);
        match res {
            Ok(Some((stream, hs))) => {
                let _ = sh2.session.send(SessionMsg::Incoming { stream, hs, addr, permit }).await;
            }
            _ => {
                sh2.diag.incoming_refused.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
}

/// The BitTorrent handshake of an incoming connection: plain, or inside an encrypted handshake
/// (MSE) when the first bytes are not "BitTorrent protocol" and the policy allows it.
async fn accept_handshake(mut stream: PeerIo, sh: &Arc<Shared>) -> Option<(PeerIo, Handshake)> {
    const PLAIN_PREFIX: &[u8; 20] = b"\x13BitTorrent protocol";
    let policy = sh.cfg().encryption;
    let mut first = [0u8; 20];
    if stream.read_exact(&mut first).await.is_err() {
        return None;
    }
    if &first == PLAIN_PREFIX {
        if policy == EncryptionPolicy::Require {
            reject(stream).await;
            return None;
        }
        let mut buf = [0u8; HANDSHAKE_LEN];
        buf[..20].copy_from_slice(&first);
        stream.read_exact(&mut buf[20..]).await.ok()?;
        return match Handshake::decode(&buf) {
            Ok(Some(h)) => Some((stream, h)),
            _ => None,
        };
    }
    if policy == EncryptionPolicy::Off {
        // Not a BitTorrent handshake and encryption is off: send the peer away as before.
        reject(stream).await;
        return None;
    }
    // Two modular exponentiations per attempt (about a millisecond in a release build): bound the
    // concurrent ones, so that garbage sent to the listen port cannot burn the CPU, but wait for a
    // turn instead of refusing. Thirty leecher connections arrive in the same instant when a
    // client with ten torrents starts, and a refused one is retried only after a back-off.
    // (The caller's handshake deadline bounds the wait; the half-open cap bounds the queue.)
    let _slot = sh.mse_slots.acquire().await.ok()?;
    let tx = sh.session.clone();
    let accepted = crate::mse::respond(stream, &first, policy, |req2| async move {
        let (reply, rx) = oneshot::channel();
        tx.send(SessionMsg::MseLookup { target: req2, reply }).await.ok()?;
        rx.await.ok().flatten()
    })
    .await
    .ok()?;
    sh.diag.mse_accepted.fetch_add(1, Ordering::Relaxed);
    Some((accepted.io, accepted.handshake))
}

/// Close a connection whose first bytes were not a BitTorrent handshake (an encrypted handshake,
/// say; the engine has no MSE). A uTP peer is told by a RESET, a TCP peer by a FIN after we have
/// swallowed what it was still sending: closing a socket with unread data would answer with a RST,
/// which clients such as Transmission count as a failing peer and back off from for minutes
/// before their plaintext retry.
async fn reject(mut s: PeerIo) {
    use tokio::io::AsyncWriteExt;
    if let PeerIo::Utp(u) = &s {
        u.reset();
        return;
    }
    let _ = s.shutdown().await;
    let mut sink = [0u8; 1024];
    let _ = tokio::time::timeout(Duration::from_millis(300), async {
        while let Ok(n) = s.read(&mut sink).await {
            if n == 0 {
                break;
            }
        }
    })
    .await;
}

impl EngineHandle {
    /// Fire-and-forget command; never blocks.
    fn send(&self, c: Cmd) {
        match self.tx.try_send(SessionMsg::Cmd(c)) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => {}
            Err(mpsc::error::TrySendError::Full(m)) => {
                let tx = self.tx.clone();
                self.rt.spawn(async move {
                    let _ = tx.send(m).await;
                });
            }
        }
    }

    async fn call<T>(&self, f: impl FnOnce(oneshot::Sender<T>) -> Cmd) -> Result<T, EngineError> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(SessionMsg::Cmd(f(tx)))
            .await
            .map_err(|_| EngineError::ShuttingDown)?;
        rx.await.map_err(|_| EngineError::ShuttingDown)
    }

    /// A handle onto an engine whose shared state is at hand (watch folders, finished downloads).
    pub(crate) fn from_shared(sh: Arc<Shared>) -> EngineHandle {
        EngineHandle { tx: sh.session.clone(), events: sh.events.clone(), rt: tokio::runtime::Handle::current(), sh }
    }

    /// Parse and plan a `.torrent` off the session actor.
    pub(crate) async fn prepare(
        &self,
        bytes: Vec<u8>,
        opts: &AddOptions,
        choice: bool,
        folder_ask: bool,
    ) -> Result<Prepared, EngineError> {
        let sh = self.sh.clone();
        let opts = opts.clone();
        tokio::task::spawn_blocking(move || {
            let cfg = sh.cfg();
            let pol = sh.policy();
            organize::prepare(
                bytes,
                &opts,
                &PlanCtx { cfg: &cfg, policy: pol.as_deref(), choice, folder_ask },
            )
        })
        .await
        .map_err(|e| EngineError::Io(e.to_string()))?
    }

    pub(crate) async fn exists(&self, id: TorrentId) -> bool {
        self.call(|reply| Cmd::Exists(id, reply)).await.ok().flatten().is_some()
    }

    pub(crate) async fn add_prepared(&self, p: Prepared, opts: AddOptions, origin: AddOrigin) -> Result<Added, EngineError> {
        self.call(|reply| Cmd::AddPrepared { p: Box::new(p), opts: Box::new(opts), origin, reply }).await?
    }

    /// Add a `.torrent` or a magnet link.
    pub async fn add(&self, src: AddSource, opts: AddOptions) -> Result<Added, EngineError> {
        match src {
            AddSource::TorrentFile(bytes) => {
                // Smart add: look at the structure unless the caller already chose what to download.
                let choice = opts.file_priorities.is_empty()
                    && (self.sh.cfg().smart_add.mode != crate::config::AskMode::Never || opts.hold == Some(true));
                let p = self.prepare(bytes, &opts, choice, false).await?;
                self.add_prepared(p, opts, AddOrigin::User).await
            }
            AddSource::Magnet(uri) => self.call(|reply| Cmd::AddMagnet { uri, opts: Box::new(opts), reply }).await?,
        }
    }

    /// Add `.torrent` bytes.
    pub async fn add_torrent_file(&self, bytes: Vec<u8>, opts: AddOptions) -> Result<Added, EngineError> {
        self.add(AddSource::TorrentFile(bytes), opts).await
    }

    /// Add a magnet URI.
    pub async fn add_magnet(&self, uri: &str, opts: AddOptions) -> Result<Added, EngineError> {
        self.add(AddSource::Magnet(uri.to_owned()), opts).await
    }

    /// Add the `.torrent` at `path` and then deal with the file: `finish` if given, else the
    /// action of the watch folder it lies in, else `torrent_after_add` from the settings
    /// (SPEC 7A.1). Use it after an [`EngineEvent::AddRequest`] was answered, and for drops.
    pub async fn add_torrent_path(
        &self,
        path: &std::path::Path,
        opts: AddOptions,
        finish: Option<AfterAdd>,
    ) -> Result<Added, EngineError> {
        let p = path.to_path_buf();
        let bytes = tokio::task::spawn_blocking(move || read_torrent_file(&p))
            .await
            .map_err(|e| EngineError::Io(e.to_string()))??;
        let added = self.add_torrent_file(bytes, opts).await?;
        let cfg = self.sh.cfg();
        let action = finish.unwrap_or_else(|| {
            cfg.watch
                .iter()
                .filter(|w| path.starts_with(&w.path))
                .max_by_key(|w| w.path.components().count())
                .map_or_else(|| cfg.torrent_after_add.clone(), |w| w.after_add.clone())
        });
        let (p, trash) = (path.to_path_buf(), cfg.trash_dir.clone());
        let left = tokio::task::spawn_blocking(move || organize::finish_file(&p, &action, trash.as_deref())).await;
        // A torrent that waits for a decision keeps the file it came from, wherever it ended up.
        if (added.needs_choice || added.pending.is_some())
            && let Ok(Ok(Some(left))) = left {
                self.sh.hold_source(added.id, left);
            }
        Ok(added)
    }

    /// The `.torrent` files that were already in the watched folder `folder` when it joined the
    /// watch list (SPEC 7A.1): the backlog the watcher deliberately ignores, so that old files in
    /// `~/Downloads` are never mass-added. The app may offer "Add 5 existing torrents from
    /// Downloads?" once and then call [`EngineHandle::add_torrent_path`] for the ones the user
    /// accepts (the folder's after-add action applies). `folder` is the expanded path as in
    /// [`crate::config::WatchConfig::path`]; an unknown folder yields an empty list. Sorted by
    /// name, at most 1,000; a single directory listing, run off the actor.
    pub async fn watch_backlog(&self, folder: &std::path::Path) -> Vec<std::path::PathBuf> {
        let cfg = self.sh.cfg();
        let Some(w) = cfg.watch.iter().find(|w| w.path == folder) else { return Vec::new() };
        let since = w
            .since
            .or_else(|| self.sh.watch_baselines.lock().unwrap_or_else(|e| e.into_inner()).get(&w.path).copied());
        let Some(since) = since else { return Vec::new() };
        let dir = w.path.clone();
        tokio::task::spawn_blocking(move || crate::watch::backlog(&dir, since)).await.unwrap_or_default()
    }

    /// The `.torrent` file a torrent that still waits for a decision came from (SPEC 7A.6), as long as it
    /// is a regular `*.torrent` file whose info-hash is the torrent's own (checked now). `None` for a
    /// magnet link, a web address, a file that is gone or changed, and for a torrent not held.
    pub fn held_source(&self, id: TorrentId) -> Option<std::path::PathBuf> {
        let path = self.sh.held_sources.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned()?;
        organize::verified_torrent_file(&path, &id)
    }

    /// Cheap form of [`EngineHandle::held_source`] for showing a button: the same file, without reading
    /// it (no hash check).
    pub fn has_held_source(&self, id: TorrentId) -> bool {
        let Some(path) = self.sh.held_sources.lock().unwrap_or_else(|e| e.into_inner()).get(&id).cloned() else {
            return false;
        };
        std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_file())
    }

    /// The chooser of a torrent (SPEC 7A.6, concept 10): its structure (seasons and episodes,
    /// films, albums, titles, builds, extras), the preselection, library presence, free space and
    /// the reasons to ask. For a torrent in state `AwaitingChoice` the preselection is the
    /// engine's default ("what I don't have", this Mac's build, ...); for a running torrent
    /// (Get Info > Files) the ticks are the files that are wanted now and nothing needs a choice.
    /// Computed on demand from the stored `.torrent`, off the actor; nothing is cached.
    pub async fn choice(&self, id: TorrentId) -> Result<ChoicePreview, EngineError> {
        let job = self.call(|reply| Cmd::ChoiceInfo(id, reply)).await??;
        let sh = self.sh.clone();
        tokio::task::spawn_blocking(move || {
            let bytes = sh
                .store
                .load_torrent_file(&id)
                .map_err(|e| EngineError::Store(e.to_string()))?
                .ok_or(EngineError::NoMetadata)?;
            let mi = trs_metainfo::Metainfo::from_bytes(&bytes).map_err(|e| EngineError::Invalid(e.to_string()))?;
            let cfg = sh.cfg();
            if job.awaiting {
                let pol = sh.policy();
                let ctx = PlanCtx { cfg: &cfg, policy: pol.as_deref(), choice: true, folder_ask: false };
                let mut c = organize::plan(&mi, &job.opts, &ctx).choice.unwrap_or_default();
                c.awaiting = true;
                c.needs_choice = true;
                return Ok(c);
            }
            let placement = if job.placed { sh.store.load_placement(&id).ok().flatten() } else { None };
            let dests: Vec<Option<std::path::PathBuf>> = mi
                .files()
                .map(|f| {
                    if f.is_padding() {
                        return None;
                    }
                    placement
                        .as_ref()
                        .and_then(|p| p.files.get(f.index).cloned().flatten())
                        .or_else(|| Some(organize::source_path(&job.save_path, &mi, f.path)))
                })
                .collect();
            let mut prios = job.prios.clone();
            prios.resize(mi.file_count(), 2);
            let mut preview = crate::choice::build(
                &mi,
                &crate::choice::BuildIn {
                    cfg: &cfg,
                    // The structure follows the category the torrent has (and the app shows).
                    forced_category: job.opts.category.or(job.category),
                    prios: Some(&prios),
                    destination: dests.iter().flatten().next().and_then(|p| p.parent().map(std::path::Path::to_path_buf)),
                    dests: &dests,
                    stage_root: job.stage_root.clone().or_else(|| Some(job.save_path.clone())),
                    awaiting: false,
                    folder_ask: false,
                    check_library: false,
                },
            );
            preview.rule_name = organize::rule_name(sh.policy().as_deref(), job.rule.as_deref());
            preview.goal = job.goal;
            preview.private = mi.private;
            Ok(preview)
        })
        .await
        .map_err(|e| EngineError::Io(e.to_string()))?
    }

    /// Get Info > Files on a running torrent: the same chooser as [`EngineHandle::choice`], with
    /// the current ticks. Answer it with [`EngineHandle::confirm_choice`]
    /// (`ChoiceSelection::Files`); newly ticked items download into the same locations as the
    /// rest of the torrent (their destinations were fixed when the torrent was added).
    pub async fn reopen_choice(&self, id: TorrentId) -> Result<ChoicePreview, EngineError> {
        self.choice(id).await
    }

    /// Answer a chooser. A torrent in state `AwaitingChoice` starts (announces, connects, downloads
    /// only the chosen files); for a running torrent it changes which files are wanted.
    /// `ChoiceSelection::Default` takes the engine's preselection. Selecting nothing is refused.
    pub async fn confirm_choice(&self, id: TorrentId, sel: ChoiceSelection) -> Result<(), EngineError> {
        let prios = match sel {
            ChoiceSelection::Files(p) => p,
            ChoiceSelection::Default => {
                let c = self.choice(id).await?;
                c.files.iter().map(|on| if *on { FilePriority::Normal } else { FilePriority::Skip }).collect()
            }
        };
        self.call(|reply| Cmd::ConfirmChoice { id, prios, reply }).await?
    }

    /// Cancel the chooser of a torrent in state `AwaitingChoice`: it is removed and leaves nothing
    /// on disk (no file is ever deleted by this call: nothing was written).
    pub async fn cancel_choice(&self, id: TorrentId) -> Result<(), EngineError> {
        self.call(|reply| Cmd::CancelChoice { id, reply }).await?
    }

    /// Stop the countdown of a torrent in state `Pending` (SPEC 7A.7). It stays held, silent, and
    /// does not start until [`EngineHandle::pending_resume`] or [`EngineHandle::start_now`].
    pub async fn pending_pause(&self, id: TorrentId) -> Result<(), EngineError> {
        self.pending_act(id, crate::session::pending::PendingAct::Pause).await.map(|_| ())
    }

    /// Run the countdown of a paused pending torrent again (at least 3 s are left).
    pub async fn pending_resume(&self, id: TorrentId) -> Result<(), EngineError> {
        self.pending_act(id, crate::session::pending::PendingAct::Resume).await.map(|_| ())
    }

    /// Start a pending torrent now, as the add would have at once.
    pub async fn start_now(&self, id: TorrentId) -> Result<(), EngineError> {
        self.pending_act(id, crate::session::pending::PendingAct::StartNow).await.map(|_| ())
    }

    /// Cancel the countdown of a pending torrent because the user wants to choose files: it stays
    /// held (state `AwaitingChoice`) until [`EngineHandle::confirm_choice`] or
    /// [`EngineHandle::cancel_choice`]. Returns `true` when the chooser can open now. A magnet link
    /// has no file list yet: its metadata is fetched now and `ChoiceNeeded` follows (`false`).
    pub async fn pending_choose(&self, id: TorrentId) -> Result<bool, EngineError> {
        self.pending_act(id, crate::session::pending::PendingAct::Choose).await
    }

    async fn pending_act(&self, id: TorrentId, act: crate::session::pending::PendingAct) -> Result<bool, EngineError> {
        self.call(|reply| Cmd::PendingAct { id, act, reply }).await?
    }

    /// The torrents that sit elsewhere than the current settings say they belong (SPEC 7A.5):
    /// after a change of the app-level or a category's incomplete or finished location (or of
    /// the status-folder switch), this is the list for "Move the N existing torrents (X GB) to the new
    /// location?" with where each is and where it would go. Pinned torrents (the user chose
    /// their place), torrents waiting for a choice and downloads that are still finishing are
    /// left out. Nothing is moved; planning runs off the actor and reads each stored `.torrent`
    /// once. The engine also emits [`EngineEvent::LocationsChanged`] when a settings change
    /// leaves any.
    pub async fn location_moves(&self) -> Result<MoveProposal, EngineError> {
        self.call(Cmd::ProposeMoves).await
    }

    /// Move torrents to where the current settings say they belong (the answer "Move" to
    /// [`EngineHandle::location_moves`]; also "Re-apply the category's location"). The torrents
    /// are queued; they move one after the other (at most one copy per destination volume at a
    /// time) while seeding goes on, and the row shows `moving`. A torrent that is still
    /// downloading is paused while its partial files move. See the `Move*` events.
    pub async fn move_torrents(&self, ids: &[TorrentId]) -> Result<MoveReport, EngineError> {
        self.call(|reply| Cmd::MoveTorrents { ids: ids.to_vec(), target: crate::moveplan::MoveTarget::Config, reply }).await
    }

    /// "Move to...": move torrents below `path` and pin them there (settings changes and category
    /// changes leave them alone afterwards). A torrent that an earlier version placed file by file keeps
    /// its sub-folders and names below `path`. Refuses a path inside the real home
    /// directory while running under test (see `guard`). Never overwrites: names that exist at
    /// the destination get a ` (2)` suffix.
    pub async fn move_to(&self, ids: &[TorrentId], path: &std::path::Path) -> Result<MoveReport, EngineError> {
        if !path.is_absolute() {
            return Err(EngineError::Invalid("the destination must be an absolute path".into()));
        }
        crate::guard::check_path("the destination", path).map_err(EngineError::Invalid)?;
        let target = crate::moveplan::MoveTarget::Rebase(path.to_path_buf());
        self.call(|reply| Cmd::MoveTorrents { ids: ids.to_vec(), target, reply }).await
    }

    /// Cancel queued and running moves. A copy in progress stops within a moment, its partial
    /// files are removed, and the torrent keeps seeding from where it was.
    pub fn cancel_moves(&self, ids: &[TorrentId]) {
        self.send(Cmd::CancelMoves(ids.to_vec()));
    }

    /// What adding this torrent would do: category and confidence, matched profile and rule,
    /// priority and goal, junk files, and where the files would go. Changes nothing.
    pub async fn preview(&self, src: AddSource, opts: AddOptions) -> Result<AddPreview, EngineError> {
        match src {
            AddSource::TorrentFile(bytes) => {
                let p = self.prepare(bytes.clone(), &opts, true, false).await?;
                let dup = self.call(|reply| Cmd::Exists(p.id, reply)).await?.is_some();
                tokio::task::spawn_blocking(move || {
                    let mi = trs_metainfo::Metainfo::from_bytes(&bytes).map_err(|e| EngineError::Invalid(e.to_string()))?;
                    Ok(organize::preview(&p, &mi, dup))
                })
                .await
                .map_err(|e| EngineError::Io(e.to_string()))?
            }
            AddSource::Magnet(uri) => {
                let m = trs_metainfo::Magnet::parse(&uri).map_err(|e| EngineError::Invalid(e.to_string()))?;
                let id = m.info_hash.swarm_id().ok_or_else(|| EngineError::Invalid("no info hash".into()))?;
                let dup = self.call(|reply| Cmd::Exists(id, reply)).await?.is_some();
                let pol = self.sh.policy();
                let bucket = match &pol {
                    Some(p) => crate::policy::bucket_of(&p.profiles, m.trackers.iter().map(String::as_str), false),
                    None => ProfileBucket::Public,
                };
                Ok(AddPreview {
                    id,
                    name: m.display_name.clone().unwrap_or_else(|| crate::util::hex(&id)),
                    total_bytes: 0,
                    file_count: 0,
                    private: false,
                    tracker_host: m.trackers.first().and_then(|u| trs_tracker::TrackerUrl::parse(u).ok()).map(|u| u.host().to_owned()).unwrap_or_default(),
                    bucket,
                    category: opts.category.unwrap_or(Category::Other),
                    detected: Category::Other,
                    confidence: 0.0,
                    uncertain: true,
                    rule: None,
                    priority: opts.priority.unwrap_or(Priority::Normal),
                    seed_goal: opts.seed_goal.unwrap_or_default(),
                    up_limit: None,
                    destination: None,
                    files: Vec::new(),
                    duplicate: dup,
                    choice: ChoicePreview::default(),
                })
            }
        }
    }

    /// Start a direct HTTP(S) download (SPEC 7D). It shares the active-downloads limit and the
    /// global download limit with torrents; it is never seeded.
    pub async fn add_download(&self, url: &str, opts: DownloadOptions) -> Result<Added, EngineError> {
        self.call(|reply| Cmd::AddDownload { url: url.to_owned(), opts: Box::new(opts), reply }).await?
    }

    /// Per-download rate limit for a direct download, bytes/s (`None` = only the global limit).
    pub fn set_download_limit(&self, id: TorrentId, limit: Option<u64>) {
        self.send(Cmd::SetDownloadLimit(id, limit));
    }

    /// Forget finished direct downloads (they stay listed as "recently finished" until then).
    pub fn forget_finished(&self, ids: &[TorrentId]) {
        self.send(Cmd::ForgetFinished(ids.to_vec()));
    }

    /// Replace the rules and tracker profiles; every torrent is re-evaluated at once.
    pub fn apply_policy(&self, policy: PolicyData) {
        if let Err(error) = crate::guard::check_profiles(&policy.profiles) {
            // A hot reload must not be the way around the guard.
            self.sh.emit(EngineEvent::ConfigRejected { file: "trackers.toml".into(), error });
            return;
        }
        let p = Arc::new(policy);
        // Adds that follow this call already see the new rules; the session re-evaluates the table.
        self.sh.set_policy(Some(p.clone()));
        self.send(Cmd::SetPolicy(Some(p)));
    }

    /// Switch rules off (torrents keep their current priority and goal).
    pub fn clear_policy(&self) {
        self.sh.set_policy(None);
        self.send(Cmd::SetPolicy(None));
    }

    /// Pause (tracker `stopped`, peers dropped).
    pub fn pause(&self, ids: &[TorrentId]) {
        self.send(Cmd::Pause(ids.to_vec()));
    }

    /// Resume (tracker `started`; session counters restart).
    pub fn resume(&self, ids: &[TorrentId]) {
        self.send(Cmd::Resume(ids.to_vec()));
    }

    /// Remove; optionally delete the downloaded files.
    pub async fn remove(&self, ids: &[TorrentId], delete_files: bool) -> Result<(), EngineError> {
        self.call(|reply| Cmd::Remove { ids: ids.to_vec(), delete_files, reply }).await
    }

    /// Verify the files again.
    pub fn recheck(&self, ids: &[TorrentId]) {
        self.send(Cmd::Recheck(ids.to_vec()));
    }

    /// Put torrents at the front of the queue.
    pub fn move_to_top(&self, ids: &[TorrentId]) {
        self.send(Cmd::MoveToTop(ids.to_vec()));
    }

    /// Queue and bandwidth priority.
    pub fn set_priority(&self, ids: &[TorrentId], p: Priority) {
        self.send(Cmd::SetPriority(ids.to_vec(), p));
    }

    /// Per-file priorities in file order (`Skip` = do not download).
    pub async fn set_file_priorities(&self, id: TorrentId, p: Vec<FilePriority>) -> Result<(), EngineError> {
        self.call(|reply| Cmd::SetFilePriorities(id, p, reply)).await?
    }

    /// Seeding goal.
    pub fn set_seed_goal(&self, ids: &[TorrentId], g: SeedGoal) {
        self.send(Cmd::SetSeedGoal(ids.to_vec(), g));
    }

    /// Put the seeding goal of these torrents back to what their rule says (undo
    /// [`EngineHandle::set_seed_goal`]). The goal follows later edits of the rules again.
    pub fn clear_seed_goal(&self, ids: &[TorrentId]) {
        self.send(Cmd::ClearSeedGoal(ids.to_vec()));
    }

    /// Choose the category (`None` hands the choice back to detection). The rule is
    /// re-evaluated at once; files already written stay where they are.
    pub fn set_category(&self, ids: &[TorrentId], c: Option<Category>) {
        self.send(Cmd::SetCategory(ids.to_vec(), c));
    }

    /// Force a rule by id (`None` = let the rules match). The matched profile is not editable
    /// here: it follows the torrent's announce URLs and `trackers.toml`.
    pub fn set_rule(&self, ids: &[TorrentId], rule: Option<String>) {
        self.send(Cmd::SetRule(ids.to_vec(), rule));
    }

    /// Pause everything.
    pub fn pause_all(&self) {
        self.send(Cmd::PauseAll);
    }

    /// Resume everything that is paused.
    pub fn resume_all(&self) {
        self.send(Cmd::ResumeAll);
    }

    /// Global limits in bytes/s (`None` = unlimited).
    pub fn set_limits(&self, down: Option<u64>, up: Option<u64>) {
        self.update_config(move |c| {
            c.down_limit = down;
            c.up_limit = up;
        });
    }

    /// Switch the alternative ("Limit Speed") limits.
    pub fn set_alt_speed(&self, on: bool) {
        self.update_config(move |c| c.alt_speed = on);
    }

    /// Change any setting at runtime; limits, caps and the queue are re-applied at once.
    pub fn update_config(&self, f: impl FnOnce(&mut EngineConfig) + Send + 'static) {
        self.send(Cmd::UpdateConfig(Box::new(f)));
    }

    /// Apply a freshly loaded `config.toml`. Limits, the alt-speed schedule, queue, connection
    /// caps, watch folders, locations and port mapping take effect at once; the returned list
    /// names the settings that need a restart.
    pub async fn apply_settings(&self, cfg: &trs_state::Config) -> Result<Vec<String>, EngineError> {
        let cur = self.config().await?;
        let new = crate::config_map::engine_config(cfg, &cur);
        let restart = crate::config_map::restart_required(&cur, &new);
        let cfg = cfg.clone();
        self.update_config(move |c| *c = crate::config_map::engine_config(&cfg, c));
        Ok(restart)
    }

    pub(crate) fn publish(&self, e: EngineEvent) {
        self.sh.emit(e);
    }

    /// The configuration in effect.
    pub async fn config(&self) -> Result<EngineConfig, EngineError> {
        self.call(Cmd::Config).await
    }

    /// Re-announce now (the tracker gate still enforces `min interval`).
    pub async fn reannounce(&self, ids: &[TorrentId]) -> Result<Vec<ReannounceResult>, EngineError> {
        self.call(|reply| Cmd::Reannounce(ids.to_vec(), reply)).await
    }

    /// Offer peer addresses.
    pub fn add_peers(&self, id: TorrentId, peers: Vec<SocketAddr>) {
        self.send(Cmd::AddPeers(id, peers));
    }

    /// The network path changed (wake, new interface).
    pub fn network_changed(&self) {
        self.send(Cmd::NetworkChanged);
    }

    /// The OS reports whether a usable network path exists (`NWPathMonitor`; SPEC 7C.4c).
    /// Unsatisfied: the engine holds announces, dials and DHT traffic and sends nothing, not
    /// even a probe. Satisfied after an outage: one probe, then the announces that came due leave
    /// spread over a minute. Satisfied while online: the old `network_changed` (port mapping,
    /// failed announces).
    pub fn network_path_changed(&self, satisfied: bool) {
        self.send(Cmd::PathChanged(satisfied));
    }

    /// A volume was mounted or unmounted (the app's `NSWorkspace` observers). Every distinct
    /// folder torrents save to is looked at once, off the engine's task; torrents on a drive that
    /// is gone are marked "Drive not connected" in one pass (finished ones need no actor to be
    /// told), and those on a drive that came back resume: unfinished ones through the start
    /// queue, verifying only what they must, two at a time. The daemon has no such observer and
    /// relies on the retry schedule (1, 5, 15, then every 60 minutes).
    pub fn volumes_changed(&self) {
        self.send(Cmd::VolumesChanged);
    }

    /// The Mac is about to sleep. Nothing expensive happens.
    pub fn system_will_sleep(&self) {
        self.send(Cmd::WillSleep);
    }

    /// The Mac woke: everything waits for the OS to report a path (or 5 s), one probe decides
    /// whether the announces that came due while asleep go out, spread over a minute.
    pub fn system_did_wake(&self) {
        self.send(Cmd::DidWake);
    }

    /// Cheap global view; the UI calls it about once a second while visible.
    pub async fn snapshot(&self) -> Result<Snapshot, EngineError> {
        self.call(Cmd::Snapshot).await
    }

    /// Set when the configured listen port was taken and another one is in use.
    pub fn port_fallback(&self) -> Option<PortFallback> {
        self.sh.port_fallback.get().cloned()
    }

    /// Play while downloading (SPEC 7F): serve `file_index` of the torrent over loopback HTTP with
    /// `Range` support so a player can open it before it is finished.
    ///
    /// * An unselected file is selected for download ([`StreamInfo::selected`],
    ///   [`EngineEvent::FilesSelected`]); a paused torrent is resumed and a queued one jumps the queue.
    /// * A file whose pieces are all present gets `complete: true` and no URL: open the file itself.
    /// * The server (one per session, loopback only) starts with the first call.
    /// * Needs metadata ([`EngineError::NoMetadata`] otherwise). A torrent that is still loading is
    ///   waited for, up to 20 seconds.
    ///
    /// Close it with [`EngineHandle::stop_stream`]; removing, stopping or moving the torrent closes
    /// its streams by itself.
    pub async fn stream_url(&self, id: TorrentId, file_index: u32) -> Result<StreamInfo, EngineError> {
        use crate::session::streams::{OpenFailure, Opened};
        // A torrent that was just added or is waking up has no file list for a moment; a magnet
        // still fetching its metadata does not get one at all.
        let wait_until = Instant::now() + Duration::from_secs(20);
        let detail = loop {
            let d = self.torrent_detail(id).await?;
            if !d.files.is_empty() {
                break d;
            }
            if d.row.state == TorrentState::FetchingMetadata || Instant::now() >= wait_until {
                return Err(EngineError::NoMetadata);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let f = detail
            .files
            .get(file_index as usize)
            .filter(|f| !f.padding && f.length > 0)
            .ok_or_else(|| EngineError::Invalid(format!("the torrent has no streamable file {file_index}")))?;
        let name = f.path.rsplit('/').next().unwrap_or(&f.path).to_owned();
        let mut info = StreamInfo {
            url: None,
            token: None,
            file_index,
            file_len: f.length,
            content_type: trs_stream::mime::content_type(&name).to_owned(),
            container: trs_stream::mime::container_of(&name).into(),
            is_mp4_family: trs_stream::mime::is_mp4_family(&name),
            file_name: name,
            complete: f.done >= f.length,
            selected: false,
        };
        if info.complete {
            return Ok(info);
        }
        let server = self.stream_server().await?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let r = self
                .call(|reply| Cmd::StreamOpen { id, file: file_index as usize, server: server.clone(), reply })
                .await?;
            match r {
                Ok(Opened::Stream { handle, selected }) => {
                    info.url = Some(handle.url);
                    info.token = Some(handle.token);
                    info.selected = selected;
                    return Ok(info);
                }
                Ok(Opened::Complete { selected }) => {
                    info.complete = true;
                    info.selected = selected;
                    return Ok(info);
                }
                Err(OpenFailure::Engine(e)) => return Err(e),
                Err(OpenFailure::NotReady) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                Err(OpenFailure::NotReady) => {
                    return Err(EngineError::Io("the torrent is still starting; try again in a moment".into()));
                }
            }
        }
    }

    /// The session's loopback streaming server, started on first use.
    async fn stream_server(&self) -> Result<Arc<trs_stream::StreamServer>, EngineError> {
        let rt = self.rt.clone();
        self.sh
            .streams
            .get_or_try_init(|| async move {
                // The server spawns its accept loop on the runtime it starts on: the engine's.
                rt.spawn(async { trs_stream::StreamServer::start(trs_stream::Config::default()).await })
                    .await
                    .map_err(|e| EngineError::Io(e.to_string()))?
                    .map(Arc::new)
                    .map_err(|e| EngineError::Io(format!("cannot start the streaming server: {e}")))
            })
            .await
            .cloned()
    }

    /// Close the stream with this `token` (from [`StreamInfo::token`]). Returns whether it existed.
    pub fn stop_stream(&self, token: &str) -> bool {
        let Some(srv) = self.sh.streams.get() else { return false };
        let closed = srv.close(token);
        if closed {
            self.send(Cmd::StreamsClosed);
        }
        closed
    }

    /// Aggregates over all torrents without building a row per torrent (see [`Totals`]).
    pub async fn totals(&self) -> Result<Totals, EngineError> {
        self.call(Cmd::Totals).await
    }

    /// Files, trackers and peers of one torrent.
    pub async fn torrent_detail(&self, id: TorrentId) -> Result<TorrentDetail, EngineError> {
        self.call(|reply| Cmd::Detail(id, reply)).await?
    }

    /// Subscribe to events.
    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    /// The developer log (SPEC 8.7): path, clear, drop counter.
    pub fn devlog(&self) -> Arc<trs_devlog::DevLog> {
        self.sh.devlog.clone()
    }

    /// The TCP port we listen on.
    pub fn listen_port(&self) -> u16 {
        self.sh.port()
    }

    /// Our peer id (one per process).
    pub fn peer_id(&self) -> [u8; 20] {
        self.sh.our_id
    }

    /// Diagnostic counters.
    pub fn diagnostics(&self) -> Diagnostics {
        let d = &self.sh.diag;
        let g = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        Diagnostics {
            dht_lookups: g(&d.dht_lookups),
            dht_announces: g(&d.dht_announces),
            pex_sent: g(&d.pex_sent),
            pex_received: g(&d.pex_received),
            hash_failures: g(&d.hash_failures),
            peers_banned: g(&d.peers_banned),
            promotions: g(&d.promotions),
            demotions: g(&d.demotions),
            wakeups: g(&d.wakeups),
            incoming_accepted: g(&d.incoming_accepted),
            incoming_refused: g(&d.incoming_refused),
            rechecks: g(&d.rechecks),
            utp_accepted: g(&d.utp_accepted),
            utp_dialed: g(&d.utp_dialed),
            mse_accepted: g(&d.mse_accepted),
            mse_dialed: g(&d.mse_dialed),
            mse_fallbacks: g(&d.mse_fallbacks),
            actors_spawned: g(&d.actors_spawned),
            boot_waiting: g(&d.boot_waiting),
            verify_peak: g(&d.verify_peak),
            replans: g(&d.replans),
        }
    }

    /// Write pending state to the store now (tests, "quit soon").
    pub async fn flush(&self) -> Result<(), EngineError> {
        self.call(Cmd::Flush).await
    }

    /// Close tracker sessions and persist. Prefer [`Engine::shutdown`].
    pub async fn shutdown(&self, deadline: Duration) -> ShutdownReport {
        let (reply, rx) = oneshot::channel();
        let msg = SessionMsg::Cmd(Cmd::Shutdown { deadline: Instant::now() + deadline, reply });
        if self.tx.send(msg).await.is_err() {
            return ShutdownReport::default();
        }
        rx.await.unwrap_or_default()
    }

    /// Synchronous wrappers for callers without a runtime (the FFI layer, CLI).
    pub fn blocking(&self) -> BlockingHandle {
        BlockingHandle(self.clone())
    }
}

/// Blocking forms of the queries and awaited commands. Must not be called from inside the
/// engine's runtime.
pub struct BlockingHandle(EngineHandle);

impl BlockingHandle {
    /// See [`EngineHandle::snapshot`].
    pub fn snapshot(&self) -> Result<Snapshot, EngineError> {
        self.0.rt.block_on(self.0.snapshot())
    }

    /// See [`EngineHandle::add`].
    pub fn add(&self, src: AddSource, opts: AddOptions) -> Result<Added, EngineError> {
        self.0.rt.block_on(self.0.add(src, opts))
    }

    /// See [`EngineHandle::torrent_detail`].
    pub fn torrent_detail(&self, id: TorrentId) -> Result<TorrentDetail, EngineError> {
        self.0.rt.block_on(self.0.torrent_detail(id))
    }

    /// See [`EngineHandle::remove`].
    pub fn remove(&self, ids: &[TorrentId], delete_files: bool) -> Result<(), EngineError> {
        self.0.rt.block_on(self.0.remove(ids, delete_files))
    }

    /// See [`EngineHandle::set_file_priorities`].
    pub fn set_file_priorities(&self, id: TorrentId, p: Vec<FilePriority>) -> Result<(), EngineError> {
        self.0.rt.block_on(self.0.set_file_priorities(id, p))
    }
}

/// Read a `.torrent` with the same size cap the store applies.
fn read_torrent_file(path: &std::path::Path) -> Result<Vec<u8>, EngineError> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| EngineError::Io(format!("{}: {e}", path.display())))?;
    let mut buf = Vec::new();
    f.take(trs_state::store::MAX_TORRENT_FILE as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(|e| EngineError::Io(e.to_string()))?;
    if buf.len() > trs_state::store::MAX_TORRENT_FILE {
        return Err(EngineError::Invalid("the .torrent file is larger than 16 MiB".into()));
    }
    Ok(buf)
}

#[allow(dead_code)]
fn _assert_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<EngineHandle>();
}

#[allow(dead_code)]
fn _unused(_: SocketAddrV4) {}
