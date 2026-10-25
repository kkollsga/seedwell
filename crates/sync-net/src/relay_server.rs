//! `seedwell relay`: a small, stateless relay for devices that cannot reach each other (SPEC §7G.3), and the
//! service's client side of it.
//!
//! A relay is addressed as `relay://host:port#<relay device id>`. Put in a folder's addresses, it makes the
//! service keep a registration there, and lets the folder's other members reach this device through it.
//! The relay pairs two TCP streams and copies bytes; the devices then run an ordinary session over the pair
//! (magic and TLS end to end), so the relay sees only ciphertext. It keeps nothing on disk and nothing
//! about a device once its streams are gone.
//!
//! # Protocol
//!
//! Over TCP. Every integer is one byte; keys, ids, nonces and signatures are raw bytes.
//!
//! ```text
//! client → relay   "SWRELAY1"
//! relay  → client  relay_pk (32) ‖ nonce (32)
//! client → relay   op (1)
//!
//! op 1, register (wait to be reached):
//!   client → relay  device_pk (32) ‖ sig (64)
//!                   sig = Ed25519(device key, "seedwell-relay/v1/register" ‖ relay_pk ‖ nonce ‖ device_pk)
//!   relay → client  status (1): 0 ok, 2 bad signature, 3 full
//!   then, while it waits: 0x00 every 30 s (keep-alive), until 0x01: another device joined, and from the
//!   next byte on the stream is the other device's.
//!
//! op 2, join (reach a registered device):
//!   client → relay  target device id (32)
//!   relay → client  status (1): 0 paired (from the next byte on the stream is the target's),
//!                   1 not registered here, 3 full
//! ```
//!
//! The client checks that `relay_pk` belongs to the device id in the address; the nonce makes a
//! registration impossible to replay. A registered stream serves one join: the client registers again
//! right away. The joining side runs the session as the client (it sends `SWSYNC1\n`), the registered side
//! as the server.
//!
//! # Limits ([`RelayOptions`])
//!
//! Waiting registrations in total and per device, paired streams at once, a rate per direction and pair,
//! a time limit for the handshake, and an idle limit for a pair.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use trs_sync_proto::codec::Magic;
use trs_sync_proto::device::{self, DeviceKey};
use trs_sync_proto::ids::DeviceId;

use crate::relay::Bucket;
use crate::service::SyncHandle;
use crate::session::{self, Via};
use crate::shared::lock;

/// What a registration signs, before `relay_pk ‖ nonce ‖ device_pk`.
pub const REGISTER_LABEL: &[u8] = b"seedwell-relay/v1/register";
const OP_REGISTER: u8 = 1;
const OP_JOIN: u8 = 2;
const ST_OK: u8 = 0;
const ST_NOT_HERE: u8 = 1;
const ST_BAD_SIGNATURE: u8 = 2;
const ST_FULL: u8 = 3;
const KEEPALIVE: u8 = 0x00;
const JOINED: u8 = 0x01;
/// A waiting registration gets a keep-alive this often.
const KEEPALIVE_EVERY: Duration = Duration::from_secs(30);

/// How the relay runs.
pub struct RelayOptions {
    /// The relay's own key (its id is in its address).
    pub key: Arc<DeviceKey>,
    /// Registrations waiting at once, in total.
    pub max_waiting: usize,
    /// Registrations waiting at once, per device.
    pub max_waiting_per_device: usize,
    /// Paired streams at once.
    pub max_pairs: usize,
    /// Bytes per second per direction of a pair (0 = unlimited).
    pub rate: u64,
    /// The handshake (magic to pairing request) must finish within this.
    pub handshake_timeout: Duration,
    /// A pair with no bytes either way for this long is closed.
    pub idle_timeout: Duration,
}

impl RelayOptions {
    /// The defaults: 1024 waiting (4 per device), 256 pairs, 8 MiB/s per direction, 10 s handshake, 10 min idle.
    pub fn new(key: DeviceKey) -> Self {
        RelayOptions {
            key: Arc::new(key),
            max_waiting: 1024,
            max_waiting_per_device: 4,
            max_pairs: 256,
            rate: 8 * 1024 * 1024,
            handshake_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(600),
        }
    }

    /// The relay's device id.
    pub fn device_id(&self) -> DeviceId {
        self.key.device_id()
    }

    /// Its address for devices: `relay://<host_port>#<id>`.
    pub fn url(&self, host_port: &str) -> String {
        format!("relay://{host_port}#{}", self.device_id())
    }
}

struct Waiting {
    id: u64,
    tx: oneshot::Sender<TcpStream>,
}

struct Server {
    opts: RelayOptions,
    waiting: Mutex<HashMap<DeviceId, Vec<Waiting>>>,
    n_waiting: AtomicUsize,
    pairs: AtomicUsize,
    next: AtomicU64,
}

fn bad(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.to_string())
}

fn nonce() -> [u8; 32] {
    let mut b = [0u8; 32];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    b
}

fn register_payload(relay_pk: &[u8; 32], nonce: &[u8; 32], device_pk: &[u8; 32]) -> Vec<u8> {
    [REGISTER_LABEL, relay_pk, nonce, device_pk].concat()
}

/// Runs the relay on `listener` until the task is dropped.
pub async fn run_relay(listener: TcpListener, opts: RelayOptions) {
    let server = Arc::new(Server {
        opts,
        waiting: Mutex::new(HashMap::new()),
        n_waiting: AtomicUsize::new(0),
        pairs: AtomicUsize::new(0),
        next: AtomicU64::new(1),
    });
    loop {
        let Ok((tcp, _)) = listener.accept().await else {
            tokio::time::sleep(Duration::from_millis(200)).await;
            continue;
        };
        let _ = tcp.set_nodelay(true);
        tokio::spawn(handle(server.clone(), tcp));
    }
}

async fn handle(s: Arc<Server>, mut tcp: TcpStream) {
    let relay_pk = *s.opts.key.public_key();
    let n = nonce();
    let hello = async {
        let mut magic = [0u8; 8];
        tcp.read_exact(&mut magic).await?;
        if Magic::parse(&magic) != Some(Magic::Relay) {
            return Err(bad("not the relay protocol"));
        }
        tcp.write_all(&[&relay_pk[..], &n[..]].concat()).await?;
        let op = tcp.read_u8().await?;
        let mut rest = [0u8; 96];
        match op {
            OP_REGISTER => tcp.read_exact(&mut rest).await?,
            OP_JOIN => tcp.read_exact(&mut rest[..32]).await?,
            _ => return Err(bad("unknown relay op")),
        };
        Ok((op, rest))
    };
    let Ok(Ok((op, rest))) = tokio::time::timeout(s.opts.handshake_timeout, hello).await else { return };
    if op == OP_REGISTER {
        let pk: [u8; 32] = rest[..32].try_into().unwrap_or([0; 32]);
        let sig: [u8; 64] = rest[32..].try_into().unwrap_or([0; 64]);
        if !device::verify(&pk, &register_payload(&relay_pk, &n, &pk), &sig) {
            let _ = tcp.write_all(&[ST_BAD_SIGNATURE]).await;
            return;
        }
        registered(s, tcp, DeviceId::of_public_key(&pk)).await;
    } else {
        let target = DeviceId(rest[..32].try_into().unwrap_or([0; 32]));
        join(s, tcp, target).await;
    }
}

/// A registered device waits until someone joins it.
async fn registered(s: Arc<Server>, mut tcp: TcpStream, dev: DeviceId) {
    let id = s.next.fetch_add(1, Ordering::Relaxed);
    let (tx, mut rx) = oneshot::channel();
    let full = {
        let mut w = lock(&s.waiting);
        let mine = w.entry(dev).or_default();
        let full = s.n_waiting.load(Ordering::Relaxed) >= s.opts.max_waiting || mine.len() >= s.opts.max_waiting_per_device;
        if !full {
            mine.push(Waiting { id, tx });
            s.n_waiting.fetch_add(1, Ordering::Relaxed);
        } else if mine.is_empty() {
            w.remove(&dev);
        }
        full
    };
    if full {
        let _ = tcp.write_all(&[ST_FULL]).await;
        return;
    }
    let forget = |s: &Server| {
        let mut w = lock(&s.waiting);
        if let Some(v) = w.get_mut(&dev) {
            let before = v.len();
            v.retain(|x| x.id != id);
            if v.len() < before {
                s.n_waiting.fetch_sub(1, Ordering::Relaxed);
            }
            if v.is_empty() {
                w.remove(&dev);
            }
        }
    };
    if tcp.write_all(&[ST_OK]).await.is_err() {
        return forget(&s);
    }
    let mut tick = tokio::time::interval(KEEPALIVE_EVERY);
    tick.tick().await;
    let mut probe = [0u8; 1];
    let other = {
        let (mut rd, mut wr) = tcp.split();
        loop {
            tokio::select! {
                r = &mut rx => match r {
                    Ok(o) => break o,
                    Err(_) => return forget(&s),
                },
                _ = tick.tick() => {
                    if wr.write_all(&[KEEPALIVE]).await.is_err() {
                        return forget(&s);
                    }
                }
                // A waiting client sends nothing: bytes or end of file mean it is gone.
                _ = rd.read(&mut probe) => return forget(&s),
            }
        }
    };
    // `join` took us out of the waiting list and counted the pair.
    let mut other = other;
    if tcp.write_all(&[JOINED]).await.is_err() || other.write_all(&[ST_OK]).await.is_err() {
        s.pairs.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    pipe(tcp, other, s.opts.rate, s.opts.idle_timeout).await;
    s.pairs.fetch_sub(1, Ordering::Relaxed);
}

/// A device asks to be joined to `target`.
async fn join(s: Arc<Server>, mut tcp: TcpStream, target: DeviceId) {
    loop {
        let w = {
            let mut all = lock(&s.waiting);
            let Some(v) = all.get_mut(&target) else { break };
            let w = v.pop();
            if v.is_empty() {
                all.remove(&target);
            }
            w
        };
        let Some(w) = w else { break };
        s.n_waiting.fetch_sub(1, Ordering::Relaxed);
        if s.pairs.fetch_add(1, Ordering::Relaxed) >= s.opts.max_pairs {
            s.pairs.fetch_sub(1, Ordering::Relaxed);
            // The registration is used up either way; its owner registers again.
            drop(w);
            let _ = tcp.write_all(&[ST_FULL]).await;
            return;
        }
        match w.tx.send(tcp) {
            Ok(()) => return,
            // That registration just went away: try the next one.
            Err(back) => {
                s.pairs.fetch_sub(1, Ordering::Relaxed);
                tcp = back;
            }
        }
    }
    let _ = tcp.write_all(&[ST_NOT_HERE]).await;
}

/// Copies bytes both ways until both sides are done or the pair is idle too long.
async fn pipe(a: TcpStream, b: TcpStream, rate: u64, idle: Duration) {
    let start = Instant::now();
    let last = AtomicU64::new(0);
    let (ar, aw) = a.into_split();
    let (br, bw) = b.into_split();
    let both = async { tokio::join!(one_way(ar, bw, rate, &last, start), one_way(br, aw, rate, &last, start)) };
    let watch = async {
        loop {
            tokio::time::sleep(idle / 4).await;
            let since = start.elapsed().saturating_sub(Duration::from_millis(last.load(Ordering::Relaxed)));
            if since >= idle {
                return;
            }
        }
    };
    tokio::select! {
        _ = both => {}
        _ = watch => {}
    }
}

async fn one_way(mut r: impl AsyncRead + Unpin, mut w: impl AsyncWrite + Unpin, rate: u64, last: &AtomicU64, start: Instant) {
    let mut buf = vec![0u8; 16 * 1024];
    let mut bucket = Bucket::new(rate);
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        last.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        bucket.take(n).await;
        if w.write_all(&buf[..n]).await.is_err() {
            break;
        }
    }
    let _ = w.shutdown().await;
}

// ----- the client side, in the service -----

/// A parsed `relay://host:port#<device id>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayUrl {
    /// `host:port`.
    pub host_port: String,
    /// The relay's device id.
    pub id: DeviceId,
}

impl RelayUrl {
    /// Parses `relay://host:port#<device id>`.
    pub fn parse(s: &str) -> Option<RelayUrl> {
        let rest = s.trim().strip_prefix("relay://")?;
        let (host_port, id) = rest.split_once('#')?;
        let (host, port) = host_port.rsplit_once(':')?;
        if host.is_empty() || port.parse::<u16>().is_err() {
            return None;
        }
        Some(RelayUrl { host_port: host_port.to_string(), id: id.parse().ok()? })
    }
}

/// Connects to the relay and runs the hello: the stream, after the relay's key was checked, and its nonce.
async fn hello(url: &RelayUrl) -> io::Result<(TcpStream, [u8; 32], [u8; 32])> {
    let mut tcp = TcpStream::connect(url.host_port.as_str()).await?;
    let _ = tcp.set_nodelay(true);
    tcp.write_all(&Magic::Relay.bytes()).await?;
    let mut b = [0u8; 64];
    tcp.read_exact(&mut b).await?;
    let pk: [u8; 32] = b[..32].try_into().unwrap_or([0; 32]);
    let n: [u8; 32] = b[32..].try_into().unwrap_or([0; 32]);
    if DeviceId::of_public_key(&pk) != url.id {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "the relay is not the one in the address"));
    }
    Ok((tcp, pk, n))
}

/// Registers at the relay and waits until someone joins: the paired stream.
async fn register_once(h: &SyncHandle, url: &RelayUrl) -> io::Result<TcpStream> {
    let (mut tcp, relay_pk, n) = tokio::time::timeout(Duration::from_secs(15), hello(url))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let key = &h.ctx().key;
    let pk = *key.public_key();
    let sig = key.sign(&register_payload(&relay_pk, &n, &pk));
    tcp.write_all(&[&[OP_REGISTER][..], &pk[..], &sig[..]].concat()).await?;
    let st = tokio::time::timeout(Duration::from_secs(15), tcp.read_u8())
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    if st != ST_OK {
        return Err(io::Error::new(io::ErrorKind::ConnectionRefused, format!("relay refused the registration ({st})")));
    }
    loop {
        // Three missed keep-alives: the relay or the path is gone.
        let b = tokio::time::timeout(KEEPALIVE_EVERY * 3, tcp.read_u8())
            .await
            .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
        match b {
            KEEPALIVE => {}
            JOINED => return Ok(tcp),
            _ => return Err(bad("unexpected byte from the relay")),
        }
    }
}

/// Keeps one registration waiting at `url` while it is in some folder's addresses.
pub(crate) async fn register_loop(h: SyncHandle, text: String) {
    let Some(url) = RelayUrl::parse(&text) else { return };
    let mut fails = 0u32;
    while !h.is_stopping() && h.relay_urls().contains(&text) {
        match register_once(&h, &url).await {
            Ok(tcp) => {
                fails = 0;
                let h2 = h.clone();
                tokio::spawn(async move {
                    let (remote, local) = (tcp.peer_addr(), tcp.local_addr());
                    let (Ok(remote), Ok(local)) = (remote, local) else { return };
                    if let Ok(conn) = session::accept_over(&h2.ctx().tls, Box::new(tcp), remote, local, Via::Relay).await {
                        h2.run_session(conn, Vec::new()).await;
                    }
                });
            }
            Err(_) => {
                fails += 1;
                tokio::time::sleep(Duration::from_millis(500u64 << fails.min(7))).await;
            }
        }
    }
    lock(&h.net().registered).remove(&text);
}

/// Starts [`register_loop`] for `url` unless one runs.
pub(crate) fn ensure_registered(h: &SyncHandle, url: &str) {
    if RelayUrl::parse(url).is_none() || !lock(&h.net().registered).insert(url.to_string()) {
        return;
    }
    h.spawn_task(register_loop(h.clone(), url.to_string()));
}

/// Joins `target` through the relay at `url` (with backoff per relay and target) and runs the session.
pub(crate) fn join_soon(h: &SyncHandle, text: String, target: DeviceId) {
    let Some(url) = RelayUrl::parse(&text) else { return };
    {
        let mut joins = lock(&h.net().joins);
        let now = Instant::now();
        if let Some((last, n)) = joins.get(&(text.clone(), target)).copied()
            && now.duration_since(last) < Duration::from_secs(2u64 << n.min(7))
        {
            return;
        }
        let n = joins.get(&(text.clone(), target)).map_or(0, |j| j.1 + 1);
        joins.insert((text.clone(), target), (now, n));
    }
    let h = h.clone();
    tokio::spawn(async move {
        let run = async {
            let (mut tcp, _, _) = hello(&url).await?;
            tcp.write_all(&[&[OP_JOIN][..], &target.0[..]].concat()).await?;
            let st = tcp.read_u8().await?;
            if st != ST_OK {
                return Err(io::Error::new(io::ErrorKind::NotFound, "not registered at the relay"));
            }
            let (remote, local) = (tcp.peer_addr()?, tcp.local_addr()?);
            Ok::<_, io::Error>((tcp, remote, local))
        };
        let Ok(Ok((tcp, remote, local))) = tokio::time::timeout(Duration::from_secs(15), run).await else { return };
        if let Ok(conn) = session::connect_over(&h.ctx().tls, Box::new(tcp), remote, local, Via::Relay, Some(target)).await {
            lock(&h.net().joins).remove(&(text, target));
            let candidates = h.candidates(target);
            h.run_session(conn, candidates).await;
        }
    });
}

/// Is `addr` a relay server's address (`relay://…`)?
pub fn is_relay_url(addr: &str) -> bool {
    RelayUrl::parse(addr).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_parse() {
        let k = DeviceKey::from_seed(&[7; 32]);
        let text = format!("relay://relay.example.net:22070#{}", k.device_id());
        let u = RelayUrl::parse(&text).unwrap();
        assert_eq!(u.host_port, "relay.example.net:22070");
        assert_eq!(u.id, k.device_id());
        assert!(RelayUrl::parse("relay://host#x").is_none());
        assert!(RelayUrl::parse("tcp://host:1#x").is_none());
        assert!(RelayUrl::parse(&format!("relay://:1#{}", k.device_id())).is_none());
    }
}
