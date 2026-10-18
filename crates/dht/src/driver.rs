//! Thin tokio UDP driver around [`DhtCore`].
//!
//! One task owns the socket and the core. It sleeps on exactly one timer (the core's
//! `next_wakeup`), or on nothing at all when the core is idle, so an idle DHT costs zero
//! wake-ups beyond incoming packets and the 15-minute bucket refreshes.

use crate::core::{
    Config, DhtCore, Event, Input, Intent, LookupStats, PublicInfoHash, Refused, Stats,
};
use crate::id::Id;
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, oneshot};

/// Well-known bootstrap routers.
pub const DEFAULT_BOOTSTRAP: [&str; 4] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
];

/// Driver settings.
#[derive(Clone, Debug)]
pub struct DriverConfig {
    /// Core tunables.
    pub core: Config,
    /// UDP bind address (use port 0 for tests).
    pub bind: SocketAddrV4,
    /// `host:port` strings resolved when the table is empty.
    pub bootstrap_hosts: Vec<String>,
    /// Persisted state from [`DhtHandle::save_state`].
    pub saved_state: Option<Vec<u8>>,
    /// Called for every datagram that arrives: a sign that the network works (SPEC 7C.4c).
    pub alive: Option<Alive>,
}

/// A callback for [`DriverConfig::alive`].
#[derive(Clone)]
pub struct Alive(pub Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for Alive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Alive")
    }
}

impl Default for DriverConfig {
    fn default() -> Self {
        Self {
            core: Config::default(),
            bind: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0),
            bootstrap_hosts: DEFAULT_BOOTSTRAP.iter().map(|s| (*s).to_owned()).collect(),
            saved_state: None,
            alive: None,
        }
    }
}

/// Progress of one lookup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupUpdate {
    /// New peers.
    Peers(Vec<SocketAddrV4>),
    /// Lookup ended (an announce may still follow).
    Finished(LookupStats),
    /// Announce round ended; the stream closes after this.
    Announced {
        /// Acknowledging nodes.
        accepted: u8,
        /// Nodes tried.
        attempted: u8,
    },
}

/// Receiver of lookup progress. Closed after `Finished` (get_peers) or `Announced` (announce).
pub struct PeerStream {
    rx: mpsc::Receiver<LookupUpdate>,
}

impl PeerStream {
    /// Next update, or `None` when the lookup is over.
    pub async fn next(&mut self) -> Option<LookupUpdate> {
        self.rx.recv().await
    }
}

/// Snapshot of the node.
#[derive(Clone, Copy, Debug)]
pub struct Info {
    /// Current node id.
    pub node_id: Id,
    /// External IP if learned.
    pub external_ip: Option<Ipv4Addr>,
    /// Routing table size.
    pub table_nodes: usize,
    /// Good nodes in the table.
    pub good_nodes: usize,
    /// Peers stored for other nodes.
    pub stored_peers: usize,
    /// Counters.
    pub stats: Stats,
    /// Bound UDP address.
    pub local_addr: SocketAddr,
}

enum Cmd {
    GetPeers(PublicInfoHash, oneshot::Sender<Result<PeerStream, Refused>>),
    Announce(
        PublicInfoHash,
        u16,
        oneshot::Sender<Result<PeerStream, Refused>>,
    ),
    Info(oneshot::Sender<Info>),
    Save(oneshot::Sender<Vec<u8>>),
    Forbid(Id),
    ReadOnly(bool),
    /// The network is down: send nothing and let no timer run until it is restored.
    Pause(bool),
    Restored,
    Resolved(Vec<SocketAddrV4>),
    Shutdown(oneshot::Sender<Vec<u8>>),
}

/// Cloneable handle to a running DHT task.
#[derive(Clone)]
pub struct DhtHandle {
    tx: mpsc::Sender<Cmd>,
    local: SocketAddr,
}

fn gone() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "dht task has stopped")
}

impl DhtHandle {
    /// UDP address the node listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Starts (or refuses, see [`Refused`]) a get_peers lookup. Peers arrive on the stream.
    pub async fn get_peers(
        &self,
        info_hash: PublicInfoHash,
    ) -> io::Result<Result<PeerStream, Refused>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::GetPeers(info_hash, tx))
            .await
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())
    }

    /// Looks up peers and then announces `port` to the closest nodes.
    pub async fn announce(
        &self,
        info_hash: PublicInfoHash,
        port: u16,
    ) -> io::Result<Result<PeerStream, Refused>> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Announce(info_hash, port, tx))
            .await
            .map_err(|_| gone())?;
        rx.await.map_err(|_| gone())
    }

    /// Node snapshot.
    pub async fn info(&self) -> io::Result<Info> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Info(tx)).await.map_err(|_| gone())?;
        rx.await.map_err(|_| gone())
    }

    /// Serialized routing table for persistence.
    pub async fn save_state(&self) -> io::Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Save(tx)).await.map_err(|_| gone())?;
        rx.await.map_err(|_| gone())
    }

    /// Permanently refuse an info hash (defence in depth for private torrents).
    pub async fn forbid(&self, info_hash: Id) -> io::Result<()> {
        self.tx
            .send(Cmd::Forbid(info_hash))
            .await
            .map_err(|_| gone())
    }

    /// Enables or disables BEP 43 read-only mode (see [`DhtCore::set_read_only`]).
    pub async fn set_read_only(&self, on: bool) -> io::Result<()> {
        self.tx.send(Cmd::ReadOnly(on)).await.map_err(|_| gone())
    }

    /// While `on`, the node sends nothing and runs no timers (an outage: SPEC 7C.4c); packets that
    /// arrive are still handled. `false` resumes.
    pub async fn set_paused(&self, on: bool) -> io::Result<()> {
        self.tx.send(Cmd::Pause(on)).await.map_err(|_| gone())
    }

    /// The network is back: the bootstrap backoff starts over (see [`DhtCore::network_restored`]).
    pub async fn network_restored(&self) -> io::Result<()> {
        self.tx.send(Cmd::Restored).await.map_err(|_| gone())
    }

    /// Stops the task and returns the final persisted state.
    pub async fn shutdown(&self) -> io::Result<Vec<u8>> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Cmd::Shutdown(tx)).await.map_err(|_| gone())?;
        rx.await.map_err(|_| gone())
    }
}

/// Best-effort datagram sender for [`SharedIo`]: must not block; a datagram that cannot be sent
/// right now is dropped (the core retries through its own timeouts).
pub type SharedSend = Arc<dyn Fn(SocketAddrV4, &[u8]) + Send + Sync>;

/// Datagram I/O owned by someone else: the engine shares one UDP port between uTP and the DHT, so
/// the uTP socket receives every datagram, keeps the uTP ones and hands the rest to the DHT.
pub struct SharedIo {
    /// Datagrams that are not uTP, as `(sender, payload)`. IPv6 senders are filtered by the caller.
    pub incoming: mpsc::Receiver<(SocketAddrV4, Box<[u8]>)>,
    /// Sends one datagram from the shared socket.
    pub send: SharedSend,
    /// Address of the shared socket (reported by [`DhtHandle::local_addr`] and [`Info`]).
    pub local: SocketAddr,
}

/// Where datagrams come from and go to.
enum Sock {
    Own(UdpSocket),
    Shared { rx: mpsc::Receiver<(SocketAddrV4, Box<[u8]>)>, send: SharedSend },
}

impl Sock {
    /// Next datagram into `buf`. `None`: an ignorable receive error, or the shared feed closed.
    async fn recv(&mut self, buf: &mut [u8]) -> Recv {
        match self {
            Sock::Own(s) => match s.recv_from(buf).await {
                Ok((n, SocketAddr::V4(from))) => Recv::Packet(from, n),
                Ok(_) => Recv::Ignore,
                Err(_) => Recv::Error,
            },
            Sock::Shared { rx, .. } => match rx.recv().await {
                Some((from, data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Recv::Packet(from, n)
                }
                None => Recv::Closed,
            },
        }
    }

    fn try_recv(&mut self, buf: &mut [u8]) -> Option<(SocketAddrV4, usize)> {
        match self {
            Sock::Own(s) => match s.try_recv_from(buf) {
                Ok((n, SocketAddr::V4(from))) => Some((from, n)),
                _ => None,
            },
            Sock::Shared { rx, .. } => {
                let (from, data) = rx.try_recv().ok()?;
                let n = data.len().min(buf.len());
                buf[..n].copy_from_slice(&data[..n]);
                Some((from, n))
            }
        }
    }

    async fn send(&self, to: SocketAddrV4, data: &[u8]) {
        match self {
            Sock::Own(s) => {
                // Best effort: UDP is lossy and the core retries via its own timeouts, so a send that
                // cannot complete quickly (full socket buffer) is dropped rather than stalling the loop.
                let _ = tokio::time::timeout(Duration::from_millis(50), s.send_to(data, SocketAddr::V4(to))).await;
            }
            Sock::Shared { send, .. } => send(to, data),
        }
    }
}

enum Recv {
    Packet(SocketAddrV4, usize),
    /// A receive error to ignore (ICMP-induced errors are normal for UDP) or a non-IPv4 sender.
    Ignore,
    Error,
    /// The shared feed ended: the socket owner is gone.
    Closed,
}

/// Binds the socket and spawns the DHT task on the current tokio runtime.
pub async fn spawn(cfg: DriverConfig) -> io::Result<DhtHandle> {
    let socket = UdpSocket::bind(SocketAddr::V4(cfg.bind)).await?;
    let local = socket.local_addr()?;
    let (tx, rx) = mpsc::channel(64);
    let handle = DhtHandle {
        tx: tx.clone(),
        local,
    };
    tokio::spawn(run(cfg, Sock::Own(socket), local, rx, tx.downgrade()));
    Ok(handle)
}

/// Like [`spawn`], but on a socket somebody else owns (see [`SharedIo`]). `cfg.bind` is ignored.
pub fn spawn_shared(cfg: DriverConfig, io: SharedIo) -> DhtHandle {
    let (tx, rx) = mpsc::channel(64);
    let handle = DhtHandle {
        tx: tx.clone(),
        local: io.local,
    };
    let sock = Sock::Shared { rx: io.incoming, send: io.send };
    tokio::spawn(run(cfg, sock, io.local, rx, tx.downgrade()));
    handle
}

/// Resolves one `host:port` to its IPv4 addresses (empty on failure or after 8 s).
async fn resolve(host: String) -> Vec<SocketAddrV4> {
    match tokio::time::timeout(Duration::from_secs(8), tokio::net::lookup_host(host)).await {
        Ok(Ok(addrs)) => addrs
            .filter_map(|a| match a {
                SocketAddr::V4(v4) => Some(v4),
                SocketAddr::V6(_) => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

type Subs = HashMap<Id, Vec<mpsc::Sender<LookupUpdate>>>;

async fn dispatch(
    core_events: Vec<Intent>,
    socket: &Sock,
    subs: &mut Subs,
    tx: &mpsc::WeakSender<Cmd>,
    hosts: &[String],
) {
    for i in core_events {
        match i {
            Intent::Send { to, data } => socket.send(to, &data).await,
            Intent::Event(ev) => match ev {
                Event::Peers { info_hash, peers } => {
                    if let Some(v) = subs.get(&info_hash) {
                        for s in v {
                            let _ = s.try_send(LookupUpdate::Peers(peers.clone()));
                        }
                    }
                }
                Event::LookupFinished { info_hash, stats } => {
                    if let Some(v) = subs.get(&info_hash) {
                        for s in v {
                            let _ = s.try_send(LookupUpdate::Finished(stats));
                        }
                    }
                }
                Event::Announced {
                    info_hash,
                    accepted,
                    attempted,
                } => {
                    if let Some(v) = subs.get(&info_hash) {
                        for s in v {
                            let _ = s.try_send(LookupUpdate::Announced {
                                accepted,
                                attempted,
                            });
                        }
                    }
                }
                Event::NeedBootstrap => {
                    // One task per host so a slow resolver never delays the fast ones; each answer is
                    // delivered as soon as it is known. Weak sender: a pending DNS lookup must not
                    // keep the task alive after the last handle drops.
                    for host in hosts.iter() {
                        let tx = tx.clone();
                        let host = host.clone();
                        tokio::spawn(async move {
                            let addrs = resolve(host).await;
                            if addrs.is_empty() {
                                return;
                            }
                            if let Some(tx) = tx.upgrade() {
                                let _ = tx.send(Cmd::Resolved(addrs)).await;
                            }
                        });
                    }
                }
                Event::Refused { .. } | Event::ExternalIp(_) | Event::NodeIdChanged { .. } => {}
            },
        }
    }
}

async fn run(
    cfg: DriverConfig,
    mut socket: Sock,
    local: SocketAddr,
    mut rx: mpsc::Receiver<Cmd>,
    tx: mpsc::WeakSender<Cmd>,
) {
    let mut core = DhtCore::new(cfg.core.clone(), Instant::now());
    let mut subs: Subs = HashMap::new();
    let hosts = cfg.bootstrap_hosts.clone();
    let mut buf = vec![0u8; 2048];
    let alive = cfg.alive.clone();
    let mut paused = false;

    if let Some(state) = &cfg.saved_state {
        if let Ok(intents) = core.load_state(state, Instant::now()) {
            dispatch(intents, &socket, &mut subs, &tx, &hosts).await;
        }
    }
    let first = core.handle(Input::Tick, Instant::now());
    dispatch(first, &socket, &mut subs, &tx, &hosts).await;

    loop {
        let wake = if paused { None } else { core.next_wakeup() };
        let sleep = async {
            match wake {
                Some(t) => tokio::time::sleep_until(tokio::time::Instant::from_std(t)).await,
                None => std::future::pending::<()>().await,
            }
        };
        let intents = tokio::select! {
            r = socket.recv(&mut buf) => match r {
                Recv::Packet(from, n) => {
                    if let Some(a) = &alive {
                        (a.0)();
                    }
                    let mut all = core.handle(Input::Packet { from, data: &buf[..n] }, Instant::now());
                    // Drain whatever else is already queued to amortise the wake-up.
                    for _ in 0..63 {
                        match socket.try_recv(&mut buf) {
                            Some((from, n)) => {
                                all.extend(core.handle(Input::Packet { from, data: &buf[..n] }, Instant::now()));
                            }
                            None => break,
                        }
                    }
                    all
                }
                Recv::Ignore => Vec::new(),
                Recv::Error => {
                    // ICMP-induced errors (e.g. connection refused) are normal for UDP; keep going.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Vec::new()
                }
                Recv::Closed => break,
            },
            _ = sleep => core.handle(Input::Tick, Instant::now()),
            cmd = rx.recv() => {
                let Some(cmd) = cmd else { break };
                match cmd {
                    Cmd::GetPeers(h, reply) => {
                        let ih = h.info_hash();
                        let intents = core.handle(Input::GetPeers(h), Instant::now());
                        answer_lookup(&mut subs, &intents, ih, reply);
                        intents
                    }
                    Cmd::Announce(h, port, reply) => {
                        let ih = h.info_hash();
                        let intents = core.handle(Input::Announce { info_hash: h, port }, Instant::now());
                        answer_lookup(&mut subs, &intents, ih, reply);
                        intents
                    }
                    Cmd::Info(reply) => {
                        let _ = reply.send(Info {
                            node_id: core.node_id(),
                            external_ip: core.external_ip(),
                            table_nodes: core.table_len(),
                            good_nodes: core.good_nodes(),
                            stored_peers: core.stored_peers(),
                            stats: *core.stats(),
                            local_addr: local,
                        });
                        Vec::new()
                    }
                    Cmd::Save(reply) => {
                        let _ = reply.send(core.save_state());
                        Vec::new()
                    }
                    Cmd::Forbid(h) => {
                        core.forbid(h);
                        Vec::new()
                    }
                    Cmd::ReadOnly(on) => {
                        core.set_read_only(on);
                        Vec::new()
                    }
                    Cmd::Pause(on) => {
                        paused = on;
                        Vec::new()
                    }
                    Cmd::Restored => {
                        paused = false;
                        core.network_restored(Instant::now())
                    }
                    Cmd::Resolved(addrs) => {
                        if paused {
                            Vec::new()
                        } else {
                            core.handle(Input::Bootstrap(&addrs), Instant::now())
                        }
                    }
                    Cmd::Shutdown(reply) => {
                        let _ = reply.send(core.save_state());
                        break;
                    }
                }
            }
        };
        let intents = if paused {
            // Nothing leaves while the network is down (the core's queries time out unblamed
            // when it is restored).
            intents.into_iter().filter(|i| !matches!(i, Intent::Send { .. } | Intent::Event(Event::NeedBootstrap))).collect()
        } else {
            intents
        };
        // `LookupFinished` (and `Announced`, in the same batch) is the last update of a lookup.
        let ended: Vec<Id> = intents
            .iter()
            .filter_map(|i| match i {
                Intent::Event(Event::LookupFinished { info_hash, .. }) => Some(*info_hash),
                _ => None,
            })
            .collect();
        dispatch(intents, &socket, &mut subs, &tx, &hosts).await;
        for ih in ended {
            subs.remove(&ih);
        }
        subs.retain(|_, v| {
            v.retain(|s| !s.is_closed());
            !v.is_empty()
        });
    }
}

fn answer_lookup(
    subs: &mut Subs,
    intents: &[Intent],
    ih: Id,
    reply: oneshot::Sender<Result<PeerStream, Refused>>,
) {
    for i in intents {
        if let Intent::Event(Event::Refused { info_hash, reason }) = i {
            if *info_hash == ih {
                let _ = reply.send(Err(*reason));
                return;
            }
        }
    }
    let (stx, srx) = mpsc::channel(32);
    subs.entry(ih).or_default().push(stx);
    let _ = reply.send(Ok(PeerStream { rx: srx }));
}
