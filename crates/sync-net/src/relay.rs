//! Reaching a device that cannot be dialed directly (SPEC §7G.3): member relays, and the dispatch of the
//! rendezvous and relay messages (hole punching itself is [`crate::punch`], the `seedwell relay` server and
//! its client [`crate::relay_server`]).
//!
//! # Member relay
//!
//! A (the opener) asks C, a peer it shares a session with, for a byte pipe to B:
//!
//! ```text
//! A → C   RelayOpen { target: B, session: a }     a: random, names the pipe in the A–C session
//! C → B   RelayOpen { target: A, session: b }     b: random, names the pipe in the C–B session
//! A ⇄ C ⇄ B   RelayData { session, data }         C rewrites the session id and forwards
//! any → C RelayClose { session }                   C forwards it to the other end; the pipe is gone
//! ```
//!
//! A then runs a whole session over the pipe, magic and TLS included, as the client; B accepts it as the
//! server. TLS runs end to end between A and B, so C sees only ciphertext.
//!
//! **Flow control.** A `RelayData` carries at most [`PIPE_CHUNK`] bytes, and each side may have at most
//! [`WINDOW`] data messages per pipe that the other end has not acknowledged. An acknowledgement is a
//! `RelayData` with no bytes, sent by the receiving end after it handed the bytes to its session (C forwards
//! acknowledgements like data). C queues at most twice the window per direction and closes a pipe whose
//! sender overruns it, so a slow or hostile side cannot make C buffer more than about 2 MiB per pipe.
//!
//! **What C relays** ([`may_relay`]): with `relay_own_devices`, A and B must both have proved Write access to
//! a folder C also holds with Write access (the user's own devices); with `relay_everyone`, it is enough that
//! both share a folder with C. Never a session that is itself relayed (no chains), at most
//! [`MAX_FORWARDS`] pipes at once and [`MAX_PER_PAIR`] per pair of devices, each direction capped at
//! [`RELAY_RATE`] bytes per second. A refused `RelayOpen` is answered with `RelayClose`.
//!
//! All relay messages travel on a session's data queue, so they keep their order and never hold up index
//! updates.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio::sync::{Semaphore, mpsc, watch};
use trs_sync_proto::ids::DeviceId;
use trs_sync_proto::keys::Access;
use trs_sync_proto::msg::{Message, RelayClose, RelayData, RelayOpen};

use crate::service::SyncHandle;
use crate::session::{self, Via};
use crate::shared::{PeerHandle, lock};

/// Bytes in one relayed `RelayData`.
pub const PIPE_CHUNK: usize = 16 * 1024;
/// Data messages per pipe and direction in flight (sent, not yet acknowledged).
pub const WINDOW: usize = 64;
/// What a member relays per pipe and direction, bytes per second.
pub const RELAY_RATE: u64 = 2 * 1024 * 1024;
/// Pipes a member relays at once.
pub const MAX_FORWARDS: usize = 16;
/// Pipes a member relays between the same two devices.
pub const MAX_PER_PAIR: usize = 2;
/// Relayed sessions this device runs as an end at once.
pub const MAX_ENDS: usize = 16;
/// The buffer between a pipe's pumps and its session.
const PIPE_BUFFER: usize = 64 * 1024;

fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    u64::from_be_bytes(b) >> 1
}

/// A token bucket: `take(n)` waits until `n` more bytes fit the rate.
pub(crate) struct Bucket {
    rate: u64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    /// `rate` bytes per second (0 = unlimited), with a burst of a quarter second.
    pub(crate) fn new(rate: u64) -> Self {
        let burst = (rate as f64 / 4.0).max(PIPE_CHUNK as f64 * 4.0);
        Bucket { rate, burst, tokens: burst, last: Instant::now() }
    }

    pub(crate) async fn take(&mut self, n: usize) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate as f64).min(self.burst);
        self.last = now;
        self.tokens -= n as f64;
        if self.tokens < 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(-self.tokens / self.rate as f64)).await;
        }
    }
}

/// An item in a forwarded direction's queue.
enum Item {
    Data(Vec<u8>),
    Close,
}

/// A pipe this device relays between two others.
struct Forward {
    /// The two ends: the peer and the pipe's id in that session.
    ends: [(Arc<PeerHandle>, u64); 2],
    /// `queues[i]` carries what `ends[i]` sent, to `ends[1 - i]`.
    queues: [mpsc::Sender<Item>; 2],
    dead: watch::Sender<bool>,
}

enum Entry {
    /// We relay this pipe; this is side `.1` of it.
    Forward(Arc<Forward>, usize),
    /// We are an end of this pipe.
    End {
        /// Bytes from the other end, to our session.
        tx: mpsc::Sender<Vec<u8>>,
        /// Data messages we may still send.
        credit: Arc<Semaphore>,
    },
}

/// The relay state of one service.
#[derive(Default)]
pub struct Net {
    /// Pipes by (the peer whose session carries them, their id in it).
    pipes: Mutex<HashMap<(DeviceId, u64), Entry>>,
    forwards: AtomicUsize,
    ends: AtomicUsize,
    /// Pipes refused, and pipes relayed, since start (diagnostics and tests).
    refused: AtomicU64,
    relayed: AtomicU64,
    /// Rendezvous attempts per target, for backoff.
    attempts: Mutex<HashMap<DeviceId, (Instant, u32)>>,
    /// Hole punching.
    pub(crate) punch: crate::punch::Punch,
    /// `seedwell relay` servers we keep a registration at (their URLs).
    pub(crate) registered: Mutex<std::collections::BTreeSet<String>>,
    /// Joins through a relay server, per (url, target), for backoff.
    pub(crate) joins: Mutex<HashMap<(String, DeviceId), (Instant, u32)>>,
}

/// What the relay has done, for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RelayStats {
    /// Pipes this device relays now.
    pub forwarding: usize,
    /// Pipes relayed since start.
    pub relayed: u64,
    /// Pipe requests refused since start (access rules, caps).
    pub refused: u64,
    /// Relayed sessions this device is an end of now.
    pub ends: usize,
}

impl Net {
    /// The counters.
    pub fn stats(&self) -> RelayStats {
        RelayStats {
            forwarding: self.forwards.load(Ordering::Relaxed),
            relayed: self.relayed.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
            ends: self.ends.load(Ordering::Relaxed),
        }
    }
}

/// Sends a relay message on a peer's data queue without waiting (relay handling never blocks the hub).
fn send_soon(p: &Arc<PeerHandle>, m: Message) {
    if let Err(mpsc::error::TrySendError::Full(m)) = p.data.try_send(m) {
        let p = p.clone();
        tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_secs(10), p.data.send(m)).await;
        });
    }
}

fn close_msg(session: u64) -> Message {
    Message::RelayClose(RelayClose { session })
}

/// Serves the relay protocol magic on the sync port. Devices relay for each other inside their sessions
/// (`RelayOpen`); the stand-alone protocol is served by `seedwell relay` ([`crate::relay_server`]), not by the
/// app, so the connection is closed.
pub async fn serve(_h: SyncHandle, tcp: TcpStream, _remote: SocketAddr) {
    drop(tcp);
}

/// A rendezvous or relay message from a connected peer.
pub async fn on_message(h: &SyncHandle, from: DeviceId, msg: Message) {
    let Some(peer) = h.ctx().registry.peer(&from) else { return };
    match msg {
        Message::Rendezvous(m) => crate::punch::on_rendezvous(h, &peer, m),
        Message::Introduce(m) => crate::punch::on_introduce(h, &peer, m),
        Message::RelayOpen(m) => on_open(h, &peer, m),
        Message::RelayData(m) => on_data(h, &peer, m),
        Message::RelayClose(m) => on_close(h, &peer, m.session),
        _ => {}
    }
}

/// May this device relay between `a` and `b`? See the module documentation.
pub(crate) fn may_relay(h: &SyncHandle, a: &PeerHandle, b: &PeerHandle) -> bool {
    if a.via == Via::Relay || b.via == Via::Relay || a.device == b.device {
        return false;
    }
    let s = h.settings();
    if !s.relay_own_devices && !s.relay_everyone {
        return false;
    }
    h.ctx().registry.folders().iter().any(|f| {
        let (Some(fa), Some(fb)) = (a.access(&f.id), b.access(&f.id)) else { return false };
        s.relay_everyone
            || (s.relay_own_devices
                && f.role_of(h.ctx().key.public_key()).is_some_and(|r| r.access() == Access::Write)
                && fa == Access::Write
                && fb == Access::Write)
    })
}

fn on_open(h: &SyncHandle, from: &Arc<PeerHandle>, m: RelayOpen) {
    let ctx = h.ctx();
    let net = h.net();
    let refuse = || {
        net.refused.fetch_add(1, Ordering::Relaxed);
        send_soon(from, close_msg(m.session));
    };
    if lock(&net.pipes).contains_key(&(from.device, m.session)) || m.target == from.device {
        return refuse();
    }
    if m.target == ctx.me {
        return refuse();
    }
    match ctx.registry.peer(&m.target) {
        // Relay to a connected device.
        Some(to) => {
            if !may_relay(h, from, &to) {
                return refuse();
            }
            let pair = {
                let pipes = lock(&net.pipes);
                pipes
                    .values()
                    .filter(|e| match e {
                        Entry::Forward(f, 0) => {
                            let (x, y) = (f.ends[0].0.device, f.ends[1].0.device);
                            (x == from.device && y == to.device) || (x == to.device && y == from.device)
                        }
                        _ => false,
                    })
                    .count()
            };
            if pair >= MAX_PER_PAIR || net.forwards.load(Ordering::Relaxed) >= MAX_FORWARDS {
                return refuse();
            }
            start_forward(h, from.clone(), m.session, to);
        }
        // We are the target's end: the other end is `m.target`, reached through `from`.
        None => {
            if from.via == Via::Relay || net.ends.load(Ordering::Relaxed) >= MAX_ENDS {
                return refuse();
            }
            let other = m.target;
            let Some(app) = open_end(h, from.clone(), m.session, None) else { return refuse() };
            let (h, via) = (h.clone(), from.clone());
            tokio::spawn(async move {
                let conn = session::accept_over(&h.ctx().tls, Box::new(app), via.remote, via.local, Via::Relay).await;
                if let Ok(conn) = conn
                    && conn.device() == other
                {
                    h.run_session(conn, Vec::new()).await;
                }
            });
        }
    }
}

fn start_forward(h: &SyncHandle, a: Arc<PeerHandle>, a_id: u64, b: Arc<PeerHandle>) {
    let net = h.net();
    let b_id = random_u64();
    let (q0, r0) = mpsc::channel(2 * WINDOW + 4);
    let (q1, r1) = mpsc::channel(2 * WINDOW + 4);
    let (dead, _) = watch::channel(false);
    let fwd = Arc::new(Forward { ends: [(a.clone(), a_id), (b.clone(), b_id)], queues: [q0, q1], dead });
    {
        let mut pipes = lock(&net.pipes);
        pipes.insert((a.device, a_id), Entry::Forward(fwd.clone(), 0));
        pipes.insert((b.device, b_id), Entry::Forward(fwd.clone(), 1));
    }
    net.forwards.fetch_add(1, Ordering::Relaxed);
    net.relayed.fetch_add(1, Ordering::Relaxed);
    // The open goes out first on B's data queue, ahead of anything forwarded to it.
    let open = Message::RelayOpen(RelayOpen { target: a.device, session: b_id });
    tokio::spawn(forward(h.clone(), fwd.clone(), 0, r0, Some(open)));
    tokio::spawn(forward(h.clone(), fwd, 1, r1, None));
}

/// Moves one direction of a relayed pipe, at most [`RELAY_RATE`]; `first` goes out before anything else.
async fn forward(h: SyncHandle, fwd: Arc<Forward>, side: usize, mut rx: mpsc::Receiver<Item>, first: Option<Message>) {
    let (from, _) = &fwd.ends[side];
    let (to, to_id) = &fwd.ends[1 - side];
    if let Some(m) = first
        && to.data.send(m).await.is_err()
    {
        return end_forward(&h, &fwd);
    }
    let mut dead = fwd.dead.subscribe();
    let (mut from_closed, mut to_closed) = (from.closed.subscribe(), to.closed.subscribe());
    let mut bucket = Bucket::new(RELAY_RATE);
    loop {
        let item = tokio::select! {
            i = rx.recv() => i,
            _ = dead.wait_for(|d| *d) => None,
            _ = from_closed.wait_for(|c| *c) => None,
            _ = to_closed.wait_for(|c| *c) => None,
        };
        let Some(Item::Data(data)) = item else { break };
        if !data.is_empty() {
            tokio::select! {
                _ = bucket.take(data.len()) => {}
                _ = dead.wait_for(|d| *d) => break,
            }
        }
        let sent = tokio::select! {
            r = to.data.send(Message::RelayData(RelayData { session: *to_id, data })) => r.is_ok(),
            _ = dead.wait_for(|d| *d) => false,
        };
        if !sent {
            break;
        }
    }
    end_forward(&h, &fwd);
}

/// Ends a relayed pipe (once): both ends are told, the entries go.
fn end_forward(h: &SyncHandle, fwd: &Arc<Forward>) {
    if fwd.dead.send_replace(true) {
        return;
    }
    let net = h.net();
    {
        let mut pipes = lock(&net.pipes);
        for (p, id) in &fwd.ends {
            if matches!(pipes.get(&(p.device, *id)), Some(Entry::Forward(f, _)) if Arc::ptr_eq(f, fwd)) {
                pipes.remove(&(p.device, *id));
            }
        }
    }
    net.forwards.fetch_sub(1, Ordering::Relaxed);
    for (p, id) in &fwd.ends {
        if !p.is_closed() {
            send_soon(p, close_msg(*id));
        }
    }
}

fn on_data(h: &SyncHandle, from: &Arc<PeerHandle>, m: RelayData) {
    let net = h.net();
    let mut pipes = lock(&net.pipes);
    let key = (from.device, m.session);
    match pipes.get(&key) {
        None => {}
        Some(Entry::Forward(fwd, side)) => {
            if fwd.queues[*side].try_send(Item::Data(m.data)).is_err() {
                // Past the window: the sender does not keep to the protocol.
                let fwd = fwd.clone();
                drop(pipes);
                end_forward(h, &fwd);
            }
        }
        Some(Entry::End { tx, credit }) => {
            let ok = if m.data.is_empty() {
                // An acknowledgement: one more message may go.
                if credit.available_permits() < WINDOW {
                    credit.add_permits(1);
                    true
                } else {
                    false
                }
            } else {
                tx.try_send(m.data).is_ok()
            };
            if !ok {
                // Dropping the sender ends the pipe; its pumps say RelayClose.
                pipes.remove(&key);
            }
        }
    }
}

fn on_close(h: &SyncHandle, from: &Arc<PeerHandle>, session: u64) {
    let net = h.net();
    let mut pipes = lock(&net.pipes);
    match pipes.remove(&(from.device, session)) {
        // What `from` sent before its close still goes through, then the other end is told.
        Some(Entry::Forward(fwd, side)) => {
            if fwd.queues[side].try_send(Item::Close).is_err() {
                drop(pipes);
                end_forward(h, &fwd);
            }
        }
        // Our end: dropping the sender gives our session end of file.
        Some(Entry::End { .. }) | None => {}
    }
}

/// Opens a pipe end carried by `via`'s session under `id`: the returned stream is what our session runs
/// over; two pumps move its bytes to and from `RelayData` messages, after sending `first`. `None` when the id
/// is taken.
fn open_end(h: &SyncHandle, via: Arc<PeerHandle>, id: u64, first: Option<Message>) -> Option<DuplexStream> {
    let net = h.net();
    let (app, mine) = tokio::io::duplex(PIPE_BUFFER);
    let (tx, rx) = mpsc::channel(WINDOW);
    let credit = Arc::new(Semaphore::new(WINDOW));
    {
        let mut pipes = lock(&net.pipes);
        if pipes.contains_key(&(via.device, id)) {
            return None;
        }
        pipes.insert((via.device, id), Entry::End { tx, credit: credit.clone() });
    }
    net.ends.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(pumps(h.clone(), via, id, mine, rx, credit, first));
    Some(app)
}

async fn pumps(
    h: SyncHandle,
    via: Arc<PeerHandle>,
    id: u64,
    mine: DuplexStream,
    mut rx: mpsc::Receiver<Vec<u8>>,
    credit: Arc<Semaphore>,
    first: Option<Message>,
) {
    let (mut rd, mut wr) = tokio::io::split(mine);
    let out = async {
        if let Some(m) = first {
            via.data.send(m).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        }
        let mut buf = vec![0u8; PIPE_CHUNK];
        loop {
            let n = rd.read(&mut buf).await?;
            if n == 0 {
                return Ok::<_, std::io::Error>(());
            }
            credit.acquire().await.map_err(std::io::Error::other)?.forget();
            let m = Message::RelayData(RelayData { session: id, data: buf[..n].to_vec() });
            via.data.send(m).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        }
    };
    let inn = async {
        while let Some(data) = rx.recv().await {
            wr.write_all(&data).await?;
            let ack = Message::RelayData(RelayData { session: id, data: Vec::new() });
            via.data.send(ack).await.map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        }
        Ok::<_, std::io::Error>(())
    };
    let mut closed = via.closed.subscribe();
    tokio::select! {
        _ = out => {}
        _ = inn => {}
        _ = closed.wait_for(|c| *c) => {}
    }
    let net = h.net();
    {
        let mut pipes = lock(&net.pipes);
        if matches!(pipes.get(&(via.device, id)), Some(Entry::End { .. })) {
            pipes.remove(&(via.device, id));
        }
    }
    net.ends.fetch_sub(1, Ordering::Relaxed);
    if !via.is_closed() {
        send_soon(&via, close_msg(id));
    }
}

/// Opens a relayed session to `target` through `via` (a member that has a session with both of us). The
/// receiver says whether the handshake over the pipe succeeded (the session then runs on its own).
pub(crate) fn open_via(h: &SyncHandle, via: Arc<PeerHandle>, target: DeviceId) -> Option<tokio::sync::oneshot::Receiver<bool>> {
    let net = h.net();
    if net.ends.load(Ordering::Relaxed) >= MAX_ENDS || via.via == Via::Relay {
        return None;
    }
    let id = random_u64();
    // The pumps send the open before any data (same queue, in order).
    let app = open_end(h, via.clone(), id, Some(Message::RelayOpen(RelayOpen { target, session: id })))?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    let h = h.clone();
    tokio::spawn(async move {
        let conn = session::connect_over(&h.ctx().tls, Box::new(app), via.remote, via.local, Via::Relay, Some(target)).await;
        let _ = tx.send(conn.is_ok());
        if let Ok(conn) = conn {
            let candidates = h.candidates(target);
            h.run_session(conn, candidates).await;
        }
    });
    Some(rx)
}

/// A device in our folders that is not connected: try to reach it through others. Called by the dial loop
/// for each such device. When direct dials are failing (or there is no address), and after a grace period
/// in which a direct dial may still succeed, this tries: the `seedwell relay` servers it may be registered
/// at, and a rendezvous through a connected peer that shares a folder with it (hole punch, then that peer
/// as a member relay). Backs off per device.
pub(crate) fn reach(h: &SyncHandle, target: DeviceId) {
    if target == h.ctx().me || h.ctx().registry.peer(&target).is_some() || !h.direct_failing(target) {
        return;
    }
    let net = h.net();
    {
        let mut attempts = lock(&net.attempts);
        let now = Instant::now();
        if let Some((last, n)) = attempts.get(&target).copied() {
            let wait = Duration::from_secs(3u64 << n.min(7)).min(Duration::from_secs(300));
            if now.duration_since(last) < wait {
                return;
            }
        }
        let n = attempts.get(&target).map_or(0, |a| a.1 + 1);
        attempts.insert(target, (now, n));
    }
    let h = h.clone();
    tokio::spawn(async move {
        // A direct dial started just now may still win.
        tokio::time::sleep(Duration::from_secs(2)).await;
        if h.ctx().registry.peer(&target).is_some() {
            lock(&h.net().attempts).remove(&target);
            return;
        }
        for url in h.relays_for(target) {
            crate::relay_server::join_soon(&h, url, target);
        }
        if let Some(via) = common_peer(&h, target) {
            crate::punch::attempt(&h, target, via).await;
        }
        if h.ctx().registry.peer(&target).is_some() {
            lock(&h.net().attempts).remove(&target);
        } else {
            // Come back when the backoff is over.
            let n = lock(&h.net().attempts).get(&target).map_or(0, |a| a.1);
            tokio::time::sleep(Duration::from_secs(3u64 << n.min(7)).min(Duration::from_secs(300))).await;
            h.wake_dialer();
        }
    });
}

/// A connected peer (directly, not relayed) that shares a folder `target` is a member of.
pub(crate) fn common_peer(h: &SyncHandle, target: DeviceId) -> Option<Arc<PeerHandle>> {
    let ctx = h.ctx();
    let folders = ctx.registry.folders();
    ctx.registry.peers().into_iter().find(|p| {
        p.device != target
            && p.via != Via::Relay
            && folders.iter().any(|f| {
                p.access(&f.id).is_some() && f.members.read().unwrap_or_else(|e| e.into_inner()).contains(&target)
            })
    })
}
