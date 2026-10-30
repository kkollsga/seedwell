//! UDP tracker transport (BEP 15 + BEP 41) on tokio.
//!
//! One socket per address family, shared by all requests; replies are
//! demultiplexed by a 32-bit transaction id drawn from the OS RNG (`getrandom`)
//! and only accepted from the address the request went to. The destination is
//! resolved once, vetted by the [`DestinationPolicy`] (no loopback, link-local,
//! metadata, or private network unless opted in) and the vetted address is the
//! one we send to. Connection ids are cached for 60 s per tracker
//! address. Retransmission waits `15 * 2^n` s with `n` capped at 4 (see
//! [`udp_proto::MAX_EXPONENT`]).
//!
//! Dual-stack names: IPv4 is tried first. If the *handshake* (BEP 15 `connect`)
//! with that address gets no answer in its first three transmissions (15, 30 and
//! 60 s waits; see [`FALLBACK_HANDSHAKE_EXPONENT`]), the address of the other
//! family is tried within the same call. Only the handshake is ever abandoned this way: nothing
//! announce-shaped has been sent yet, so it is safe for every event including
//! `completed`, and it stays one announce for the gate. Once an announce was
//! sent, no other address is tried (the first one may have processed it). The
//! family that worked is remembered per host for [`FAMILY_MEMORY`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::gate::GatedRequest;
use crate::policy::DestinationPolicy;
use crate::response::{AnnounceError, AnnounceResponse, ParseError, TransportError};
use crate::scrape::ScrapeEntry;
use crate::types::InfoHash20;
use crate::udp_proto::{self, Reply};

/// What happened on the wire, for the developer log. `Copy`, filled either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UdpTrace {
    /// The tracker address of the last try.
    pub addr: Option<SocketAddr>,
    /// A first address failed before anything announce-shaped was sent and the other family was tried.
    pub fallback: bool,
    /// Connection id used for the announce (BEP 15).
    pub connection_id: u64,
    /// It came from the cache (no new connect handshake).
    pub connection_reused: bool,
    /// Transaction id of the announce packet.
    pub transaction_id: u32,
}

/// Tuning (the defaults are the SPEC values; tests shrink the waits).
#[derive(Clone, Copy, Debug)]
pub struct UdpConfig {
    /// First retransmission wait.
    pub base_timeout: Duration,
    /// Cap on the exponent `n`.
    pub max_exponent: u32,
    /// How long a connection id may be reused.
    pub connection_id_ttl: Duration,
    /// Hard limit for one whole announce (connect + announce). Stays below the
    /// gate's UDP deadline so the gate never has to time us out.
    pub total_timeout: Duration,
    /// Where we may send. Default: public addresses only; see
    /// [`DestinationPolicy`].
    pub policy: DestinationPolicy,
    /// The address the sockets are bound to: `None` for every address (an IPv4 and an IPv6
    /// socket), or one IPv4 address only, such as loopback for an engine bound to it.
    pub bind: Option<std::net::Ipv4Addr>,
}

impl Default for UdpConfig {
    fn default() -> Self {
        UdpConfig {
            base_timeout: udp_proto::BASE_TIMEOUT,
            max_exponent: udp_proto::MAX_EXPONENT,
            connection_id_ttl: udp_proto::CONNECTION_ID_TTL,
            total_timeout: Duration::from_secs(540),
            policy: DestinationPolicy::STRICT,
            bind: None,
        }
    }
}

/// Largest retransmission exponent used for the handshake with the first
/// address when another family could still be tried.
pub const FALLBACK_HANDSHAKE_EXPONENT: u32 = 2;

/// How long the family that worked after a fallback is tried first.
pub const FAMILY_MEMORY: Duration = Duration::from_secs(3600);

type Waiter = (SocketAddr, oneshot::Sender<Vec<u8>>);

struct Inner {
    sock4: UdpSocket,
    sock6: Option<UdpSocket>,
    waiters: Mutex<HashMap<u32, Waiter>>,
    conn_ids: Mutex<HashMap<SocketAddr, (u64, Instant)>>,
    /// Families remembered per lower-cased host (`true` = IPv6).
    prefs: Mutex<HashMap<String, (bool, Instant)>>,
    /// Fixed name table instead of DNS (tests).
    hosts: Option<HashMap<String, Vec<IpAddr>>>,
    cfg: UdpConfig,
}

/// UDP tracker client. Cheap to clone.
#[derive(Clone)]
pub struct UdpTransport {
    inner: Arc<Inner>,
    _tasks: Arc<AbortOnDrop>,
}

struct AbortOnDrop(Vec<JoinHandle<()>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl UdpTransport {
    /// Bind the sockets (IPv4 required, IPv6 best effort) and start the
    /// receive tasks. Must be called inside a tokio runtime.
    pub async fn bind(cfg: UdpConfig) -> std::io::Result<Self> {
        Self::bind_inner(cfg, None).await
    }

    /// Same, resolving the given names from a fixed table instead of DNS
    /// (tests of dual-stack behaviour). The destination policy still applies.
    #[doc(hidden)]
    pub async fn bind_with_static_hosts(
        cfg: UdpConfig,
        hosts: &[(&str, Vec<IpAddr>)],
    ) -> std::io::Result<Self> {
        let table = hosts
            .iter()
            .map(|(h, ips)| (h.to_ascii_lowercase(), ips.clone()))
            .collect();
        Self::bind_inner(cfg, Some(table)).await
    }

    async fn bind_inner(
        cfg: UdpConfig,
        hosts: Option<HashMap<String, Vec<IpAddr>>>,
    ) -> std::io::Result<Self> {
        let (sock4, sock6) = match cfg.bind {
            None => (UdpSocket::bind("0.0.0.0:0").await?, UdpSocket::bind("[::]:0").await.ok()),
            Some(ip) => (UdpSocket::bind((ip, 0)).await?, None),
        };
        let inner = Arc::new(Inner {
            sock4,
            sock6,
            waiters: Mutex::new(HashMap::new()),
            conn_ids: Mutex::new(HashMap::new()),
            prefs: Mutex::new(HashMap::new()),
            hosts,
            cfg,
        });
        let mut tasks = vec![tokio::spawn(recv_loop(inner.clone(), false))];
        if inner.sock6.is_some() {
            tasks.push(tokio::spawn(recv_loop(inner.clone(), true)));
        }
        Ok(UdpTransport {
            inner,
            _tasks: Arc::new(AbortOnDrop(tasks)),
        })
    }

    /// The IPv4 socket's local address (tests).
    pub fn local_addr_v4(&self) -> std::io::Result<SocketAddr> {
        self.inner.sock4.local_addr()
    }

    /// A transaction id from the OS RNG that no pending exchange uses. An
    /// off-path attacker has to guess 32 unpredictable bits per reply. There is
    /// deliberately no fallback to a seeded PRNG: if the OS cannot give us
    /// randomness nothing is sent.
    fn next_tid(&self) -> Result<u32, AnnounceError> {
        for _ in 0..8 {
            let mut b = [0u8; 4];
            getrandom::fill(&mut b)
                .map_err(|_| AnnounceError::Transport(TransportError::NotSent))?;
            let tid = u32::from_le_bytes(b);
            if !lock(&self.inner.waiters).contains_key(&tid) {
                return Ok(tid);
            }
        }
        Err(AnnounceError::Transport(TransportError::NotSent))
    }

    /// The addresses to try, in order: at most one per family, vetted, IPv4
    /// first unless a fallback made IPv6 the remembered family.
    async fn resolve(&self, req: &GatedRequest) -> Result<Vec<SocketAddr>, AnnounceError> {
        let url = req.url();
        let host = url.host().to_ascii_lowercase();
        let addrs: Vec<SocketAddr> = match self.inner.hosts.as_ref().and_then(|h| h.get(&host)) {
            Some(ips) => ips
                .iter()
                .map(|ip| SocketAddr::new(*ip, url.port()))
                .collect(),
            None => tokio::net::lookup_host((url.host(), url.port()))
                .await
                .map_err(|_| AnnounceError::Transport(TransportError::Dns))?
                .collect(),
        };
        if addrs.is_empty() {
            return Err(AnnounceError::Transport(TransportError::Dns));
        }
        // Vet after resolution: only addresses the policy allows are candidates.
        let addrs = self
            .inner
            .cfg
            .policy
            .vet(addrs)
            .map_err(|r| AnnounceError::Transport(TransportError::Forbidden(r)))?;
        let v4 = addrs
            .iter()
            .find(|a| a.ip().to_canonical().is_ipv4())
            .copied();
        let v6 = addrs
            .iter()
            .find(|a| !a.ip().to_canonical().is_ipv4() && self.inner.sock6.is_some())
            .copied();
        let v6_first = self.preferred_v6(&host) && v4.is_some() && v6.is_some();
        // IPv4 first otherwise: UDP trackers overwhelmingly listen there.
        let order = if v6_first { [v6, v4] } else { [v4, v6] };
        let out: Vec<SocketAddr> = order.into_iter().flatten().collect();
        if out.is_empty() {
            return Err(AnnounceError::Transport(TransportError::Dns));
        }
        Ok(out)
    }

    fn preferred_v6(&self, host: &str) -> bool {
        let mut prefs = lock(&self.inner.prefs);
        match prefs.get(host) {
            Some((v6, until)) if Instant::now() < *until => *v6,
            Some(_) => {
                prefs.remove(host);
                false
            }
            None => false,
        }
    }

    fn remember(&self, host: &str, v6: bool) {
        let mut prefs = lock(&self.inner.prefs);
        if prefs.len() >= 256 {
            let now = Instant::now();
            prefs.retain(|_, (_, until)| now < *until);
        }
        if prefs.len() < 256 {
            prefs.insert(host.to_owned(), (v6, Instant::now() + FAMILY_MEMORY));
        }
    }

    /// Run `op(address, handshake_max_exponent)` on the candidates in order.
    /// Moves on to the next address only when `op` failed in a way that proves
    /// nothing announce-shaped was sent (the handshake got no answer, or the
    /// local send failed); the first candidate then gets a shorter handshake.
    async fn on_candidates<T, F, Fut>(
        &self,
        req: &GatedRequest,
        mut op: F,
    ) -> Result<T, AnnounceError>
    where
        F: FnMut(SocketAddr, u32) -> Fut,
        Fut: std::future::Future<Output = Result<T, AnnounceError>>,
    {
        let cands = self.resolve(req).await?;
        let host = req.url().host().to_ascii_lowercase();
        let full = self.inner.cfg.max_exponent;
        let mut first_err = None;
        for (i, addr) in cands.iter().enumerate() {
            let more = i + 1 < cands.len();
            let n = if more {
                full.min(FALLBACK_HANDSHAKE_EXPONENT)
            } else {
                full
            };
            match op(*addr, n).await {
                Ok(v) => {
                    if i > 0 {
                        self.remember(&host, !addr.ip().to_canonical().is_ipv4());
                    }
                    return Ok(v);
                }
                Err(
                    e @ AnnounceError::Transport(
                        TransportError::ConnectTimeout | TransportError::Connect,
                    ),
                ) if more => {
                    first_err.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(first_err.unwrap_or(AnnounceError::Transport(TransportError::Dns)))
    }

    /// One request/response exchange with retransmission.
    async fn exchange(
        &self,
        addr: SocketAddr,
        tid: u32,
        packet: &[u8],
        max_exponent: u32,
    ) -> Result<Reply, AnnounceError> {
        let sock = if addr.is_ipv4() {
            &self.inner.sock4
        } else {
            self.inner
                .sock6
                .as_ref()
                .ok_or(AnnounceError::Transport(TransportError::Connect))?
        };
        let (tx, mut rx) = oneshot::channel();
        lock(&self.inner.waiters).insert(tid, (addr, tx));
        let _guard = WaiterGuard {
            inner: &self.inner,
            tid,
        };
        let cfg = self.inner.cfg;
        for n in 0..=max_exponent {
            if sock.send_to(packet, addr).await.is_err() {
                return Err(AnnounceError::Transport(TransportError::Connect));
            }
            let wait = udp_proto::retransmit_timeout(n, cfg.base_timeout, cfg.max_exponent);
            match tokio::time::timeout(wait, &mut rx).await {
                Ok(Ok(data)) => {
                    // The recv loop only forwards well-formed replies from `addr`
                    return udp_proto::decode(&data, addr.is_ipv6())
                        .map_err(|_| AnnounceError::Garbage(ParseError::NotBencode));
                }
                Ok(Err(_)) => return Err(AnnounceError::Transport(TransportError::Io)),
                Err(_) => continue,
            }
        }
        Err(AnnounceError::Transport(TransportError::Timeout))
    }

    /// A connection id for `addr` and whether it came from the cache (`true`)
    /// or from a fresh handshake (`false`).
    async fn connection_id(
        &self,
        addr: SocketAddr,
        force_new: bool,
        handshake_exponent: u32,
    ) -> Result<(u64, bool), AnnounceError> {
        let ttl = self.inner.cfg.connection_id_ttl;
        if !force_new
            && let Some((id, at)) = lock(&self.inner.conn_ids).get(&addr).copied()
                && at.elapsed() < ttl {
                    return Ok((id, true));
                }
        let tid = self.next_tid()?;
        let reply = match self
            .exchange(
                addr,
                tid,
                &udp_proto::encode_connect(tid),
                handshake_exponent,
            )
            .await
        {
            // The handshake never completed, so the announce was never sent.
            Err(AnnounceError::Transport(TransportError::Timeout)) => {
                return Err(AnnounceError::Transport(TransportError::ConnectTimeout));
            }
            other => other?,
        };
        match reply {
            Reply::Connect { connection_id, .. } => {
                let mut ids = lock(&self.inner.conn_ids);
                if ids.len() > 256 {
                    ids.retain(|_, (_, at)| at.elapsed() < ttl);
                }
                ids.insert(addr, (connection_id, Instant::now()));
                Ok((connection_id, false))
            }
            Reply::Error { message, .. } => Err(AnnounceError::Rejected {
                reason: message,
                retry_in: None,
            }),
            _ => Err(AnnounceError::Garbage(ParseError::NotBencode)),
        }
    }

    /// Execute a released announce.
    pub async fn announce(&self, req: &GatedRequest) -> Result<AnnounceResponse, AnnounceError> {
        self.announce_traced(req).await.0
    }

    /// Like [`announce`](Self::announce), also saying which address was used and the ids of the
    /// last exchange (developer log).
    pub async fn announce_traced(&self, req: &GatedRequest) -> (Result<AnnounceResponse, AnnounceError>, UdpTrace) {
        let trace = Mutex::new(UdpTrace::default());
        let r = self.announce_inner(req, &trace).await;
        (r, *lock(&trace))
    }

    async fn announce_inner(
        &self,
        req: &GatedRequest,
        trace: &Mutex<UdpTrace>,
    ) -> Result<AnnounceResponse, AnnounceError> {
        let Some(params) = req.announce_params() else {
            return Err(AnnounceError::Garbage(ParseError::NotDict));
        };
        let total = self.inner.cfg.total_timeout;
        let url_data = req.url().path_and_query().to_owned();
        let work = self.on_candidates(req, |addr, handshake_n| {
            let url_data = &url_data;
            async move {
                {
                    let mut t = lock(trace);
                    t.fallback = t.addr.is_some();
                    t.addr = Some(addr);
                }
                let mut force_new = false;
                for attempt in 0..2 {
                    let (conn, reused) = self.connection_id(addr, force_new, handshake_n).await?;
                    let tid = self.next_tid()?;
                    {
                        let mut t = lock(trace);
                        t.connection_id = conn;
                        t.connection_reused = reused;
                        t.transaction_id = tid;
                    }
                    let packet = udp_proto::encode_announce(conn, tid, params, Some(url_data));
                    match self
                        .exchange(addr, tid, &packet, self.inner.cfg.max_exponent)
                        .await?
                    {
                        Reply::Announce { response, .. } => return Ok(*response),
                        Reply::Error { message, .. } => {
                            // The one error worth a hidden second try is a stale
                            // connection id: it needs a *cached* id (a fresh
                            // handshake cannot be stale) and a message about the
                            // connection. Anything else ("too many connections
                            // from your IP", "unregistered torrent") is the
                            // tracker's answer and is surfaced as it is.
                            if attempt == 0
                                && reused
                                && message.to_ascii_lowercase().contains("connection")
                            {
                                lock(&self.inner.conn_ids).remove(&addr);
                                force_new = true;
                                continue;
                            }
                            return Err(AnnounceError::Rejected {
                                reason: message,
                                retry_in: None,
                            });
                        }
                        _ => return Err(AnnounceError::Garbage(ParseError::NotBencode)),
                    }
                }
                Err(AnnounceError::Transport(TransportError::Io))
            }
        });
        match tokio::time::timeout(total, work).await {
            Ok(r) => r,
            Err(_) => Err(AnnounceError::Transport(TransportError::Timeout)),
        }
    }

    /// Execute a released scrape.
    pub async fn scrape(
        &self,
        req: &GatedRequest,
    ) -> Result<Vec<(InfoHash20, ScrapeEntry)>, AnnounceError> {
        let Some(hashes) = req.scrape_hashes() else {
            return Err(AnnounceError::Garbage(ParseError::NotDict));
        };
        let total = self.inner.cfg.total_timeout;
        let work = self.on_candidates(req, |addr, handshake_n| async move {
            let (conn, _) = self.connection_id(addr, false, handshake_n).await?;
            let tid = self.next_tid()?;
            match self
                .exchange(
                    addr,
                    tid,
                    &udp_proto::encode_scrape(conn, tid, hashes),
                    self.inner.cfg.max_exponent,
                )
                .await?
            {
                Reply::Scrape { entries, .. } => Ok(hashes.iter().copied().zip(entries).collect()),
                Reply::Error { message, .. } => Err(AnnounceError::Rejected {
                    reason: message,
                    retry_in: None,
                }),
                _ => Err(AnnounceError::Garbage(ParseError::NotBencode)),
            }
        });
        match tokio::time::timeout(total, work).await {
            Ok(r) => r,
            Err(_) => Err(AnnounceError::Transport(TransportError::Timeout)),
        }
    }
}

struct WaiterGuard<'a> {
    inner: &'a Inner,
    tid: u32,
}
impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        lock(&self.inner.waiters).remove(&self.tid);
    }
}

async fn recv_loop(inner: Arc<Inner>, v6: bool) {
    let mut buf = vec![0u8; 4096];
    loop {
        let sock = if v6 {
            inner.sock6.as_ref()
        } else {
            Some(&inner.sock4)
        };
        let Some(sock) = sock else { return };
        let Ok((n, from)) = sock.recv_from(&mut buf).await else {
            // Transient socket errors (e.g. ICMP unreachable surfacing on
            // some platforms) must not kill the loop; back off a little.
            tokio::time::sleep(Duration::from_millis(50)).await;
            continue;
        };
        let Some((_, tid)) = udp_proto::peek_header(&buf[..n]) else {
            continue;
        };
        let waiter = {
            let mut w = lock(&inner.waiters);
            match w.get(&tid) {
                // only the address we asked may answer (anti-spoofing)
                Some((addr, _)) if *addr == from => w.remove(&tid),
                _ => None,
            }
        };
        if let Some((_, tx)) = waiter {
            let _ = tx.send(buf[..n].to_vec());
        }
    }
}
