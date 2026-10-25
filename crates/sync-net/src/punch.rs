//! Hole punching through a common peer (SPEC §7G.3), in the style of BEP 55.
//!
//! A wants to reach B; both have a session with C, and both share a folder with C.
//!
//! 1. A → C: `Rendezvous { target: B, candidates }`, A's own addresses (its port at each address peers
//!    reached it on).
//! 2. C checks that B is connected (directly) and that A and B both share a folder with C, then sends
//!    `Introduce { from: A, candidates, punch_at_ms }` to B and the mirror `Introduce { from: B, … }` to A.
//!    The candidates start with the address C sees the device at (its uTP address, or the IP of its session
//!    with its sync port), then what it claims (A) or its signed address record says (B). `punch_at_ms` is
//!    a moment just ahead, so both sides start together.
//! 3. At `punch_at` (or at once, when that is past or the clocks disagree by more than a few seconds) both
//!    send [`PROBES`] probes, `SWPUNCH1 ‖ device id (32) ‖ nonce (8)`, from their uTP socket to each of the
//!    other's candidates: each side's NAT then lets the other's packets in.
//! 4. The device with the lower id dials uTP, to the address a probe came from (when one arrived within
//!    [`PROBE_WAIT`]) and to the candidates, in parallel; the first TLS handshake wins. The other side
//!    accepts as usual. A probe proves nothing: the session's TLS and Join decide, as for any connection.
//! 5. If no session came up within [`PUNCH_WAIT`], A asks C to relay ([`crate::relay::open_via`]).
//!
//! Introductions are rate limited per pair at C and per device at the receiver, and a device sends at
//! most [`PROBES`] × 8 probes per introduction, so this cannot be used to make a device flood an address.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{oneshot, watch};
use trs_sync_proto::codec::Magic;
use trs_sync_proto::ids::DeviceId;
use trs_sync_proto::msg::{Introduce, MAX_ADDRS, Message, Rendezvous};

use crate::service::SyncHandle;
use crate::session::{self, Via};
use crate::shared::{PeerHandle, lock};

/// Probe rounds per introduction.
pub const PROBES: usize = 4;
/// Between probe rounds.
const PROBE_GAP: Duration = Duration::from_millis(100);
/// How long the dialing side waits for the other's first probe before it dials anyway.
pub const PROBE_WAIT: Duration = Duration::from_millis(600);
/// How long after `punch_at` a session may take to come up before A falls back to the relay.
pub const PUNCH_WAIT: Duration = Duration::from_secs(4);
/// How far ahead C sets `punch_at`.
const PUNCH_LEAD: Duration = Duration::from_millis(300);
/// How long A waits for C's introduction.
const INTRODUCE_WAIT: Duration = Duration::from_secs(5);
/// Candidates used per introduction.
const MAX_CANDIDATES: usize = 8;
/// One introduction per pair (at C) or per device (at the receiver) this often.
const INTRODUCE_EVERY: Duration = Duration::from_secs(2);
/// The length of a probe.
pub const PROBE_LEN: usize = 8 + 32 + 8;

/// Hole-punch state of one service.
#[derive(Default)]
pub struct Punch {
    /// Our rendezvous waiting for their introduction, by target.
    waiting: Mutex<HashMap<DeviceId, oneshot::Sender<Introduce>>>,
    /// Punches in progress: where the other's first probe came from.
    probes: Mutex<HashMap<DeviceId, watch::Sender<Option<SocketAddr>>>>,
    /// Introductions made (at C, per pair) and received (per device), for rate limits.
    introduced: Mutex<HashMap<(DeviceId, DeviceId), Instant>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// True at most once per [`INTRODUCE_EVERY`] for this key.
fn rate_ok(p: &Punch, key: (DeviceId, DeviceId)) -> bool {
    let mut m = lock(&p.introduced);
    let now = Instant::now();
    m.retain(|_, t| now.duration_since(*t) < INTRODUCE_EVERY * 10);
    if m.get(&key).is_some_and(|t| now.duration_since(*t) < INTRODUCE_EVERY) {
        return false;
    }
    m.insert(key, now);
    true
}

fn push(out: &mut Vec<String>, a: String) {
    if !out.contains(&a) && out.len() < MAX_ADDRS {
        out.push(a);
    }
}

/// The addresses C gives for `p`: where C sees it first, then what it claims or its record says.
fn candidates_seen(p: &PeerHandle, claimed: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if p.via == Via::Utp {
        push(&mut out, p.remote.to_string());
    }
    for c in claimed {
        if let Ok(sa) = c.parse::<SocketAddr>() {
            push(&mut out, SocketAddr::new(p.remote.ip(), sa.port()).to_string());
        }
    }
    for c in claimed {
        push(&mut out, c.clone());
    }
    out
}

/// Our own candidates for a rendezvous: our port at each address a peer reached us on.
fn own_candidates(h: &SyncHandle) -> Vec<String> {
    let mut out = Vec::new();
    for p in h.ctx().registry.peers() {
        if p.via != Via::Relay {
            push(&mut out, SocketAddr::new(p.local.ip(), h.port()).to_string());
        }
    }
    out
}

/// At C: A asks to be introduced to `m.target`.
pub(crate) fn on_rendezvous(h: &SyncHandle, a: &Arc<PeerHandle>, m: Rendezvous) {
    let ctx = h.ctx();
    if m.target == ctx.me || m.target == a.device || a.via == Via::Relay {
        return;
    }
    let Some(b) = ctx.registry.peer(&m.target) else { return };
    if b.via == Via::Relay {
        return;
    }
    let shared = ctx.registry.folders().iter().any(|f| a.access(&f.id).is_some() && b.access(&f.id).is_some());
    if !shared || !rate_ok(&h.net().punch, (a.device, b.device)) {
        return;
    }
    let at = now_ms() + PUNCH_LEAD.as_millis() as u64;
    let claimed_a: Vec<String> = m.candidates.into_iter().take(MAX_ADDRS).collect();
    let cand_a = candidates_seen(a, &claimed_a);
    let cand_b = candidates_seen(&b, &h.record_addrs(b.device));
    let to_b = Message::Introduce(Introduce { from: a.device, candidates: cand_a, punch_at_ms: at });
    let to_a = Message::Introduce(Introduce { from: b.device, candidates: cand_b, punch_at_ms: at });
    let (a, b) = (a.clone(), b);
    tokio::spawn(async move {
        let _ = b.control.send(to_b).await;
        let _ = a.control.send(to_a).await;
    });
}

/// At A or B: C introduces `m.from`; punch towards it.
pub(crate) fn on_introduce(h: &SyncHandle, c: &Arc<PeerHandle>, m: Introduce) {
    let me = h.ctx().me;
    if m.from == me || m.from == c.device || h.ctx().registry.peer(&m.from).is_some() {
        return;
    }
    let punch = &h.net().punch;
    // Our own rendezvous learns that C made the introduction.
    if let Some(tx) = lock(&punch.waiting).remove(&m.from) {
        let _ = tx.send(m.clone());
    }
    if !rate_ok(punch, (me, m.from)) {
        return;
    }
    tokio::spawn(run(h.clone(), m));
}

/// A datagram on the sync port that is not uTP: a probe, or noise.
pub(crate) fn on_datagram(h: &SyncHandle, from: SocketAddr, data: &[u8]) {
    if data.len() != PROBE_LEN || data[..8] != Magic::Punch.bytes() {
        return;
    }
    let Ok(id) = <[u8; 32]>::try_from(&data[8..40]) else { return };
    if let Some(seen) = lock(&h.net().punch.probes).get(&DeviceId(id)) {
        seen.send_if_modified(|s| {
            if s.is_none() {
                *s = Some(from);
                true
            } else {
                false
            }
        });
    }
}

fn probe(me: DeviceId) -> [u8; PROBE_LEN] {
    let mut p = [0u8; PROBE_LEN];
    p[..8].copy_from_slice(&Magic::Punch.bytes());
    p[8..40].copy_from_slice(&me.0);
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut p[40..]);
    p
}

/// Punches towards the introduced device; the lower id dials.
async fn run(h: SyncHandle, m: Introduce) {
    let Some(utp) = h.utp().cloned() else { return };
    let me = h.ctx().me;
    let other = m.from;
    let cands: Vec<SocketAddr> = m.candidates.iter().filter_map(|c| c.parse().ok()).take(MAX_CANDIDATES).collect();
    if cands.is_empty() {
        return;
    }
    let (seen_tx, mut seen) = watch::channel(None);
    lock(&h.net().punch.probes).insert(other, seen_tx);

    // Start together; a far-off time (clocks that disagree) is not waited for.
    let wait = m.punch_at_ms.saturating_sub(now_ms()).min(3000);
    tokio::time::sleep(Duration::from_millis(wait)).await;

    let pkt = probe(me);
    let probing = async {
        for _ in 0..PROBES {
            for c in &cands {
                let _ = utp.try_send_foreign(*c, &pkt);
            }
            tokio::time::sleep(PROBE_GAP).await;
        }
    };
    if me < other && !h.is_blocked(other) {
        let dialing = async {
            let _ = tokio::time::timeout(PROBE_WAIT, seen.wait_for(|s| s.is_some())).await;
            let mut addrs: Vec<SocketAddr> = seen.borrow().iter().copied().collect();
            for c in &cands {
                if !addrs.contains(c) && addrs.len() < 4 {
                    addrs.push(*c);
                }
            }
            dial_first(&h, &utp, addrs, other).await
        };
        let (_, conn) = tokio::join!(probing, dialing);
        if let Some(conn) = conn {
            lock(&h.net().punch.probes).remove(&other);
            let candidates = h.candidates(other);
            h.run_session(conn, candidates).await;
            return;
        }
    } else {
        probing.await;
    }
    // Keep the entry a little for late probes, then forget it.
    tokio::time::sleep(PUNCH_WAIT).await;
    lock(&h.net().punch.probes).remove(&other);
}

/// Dials uTP to each address at once; the first finished handshake with `expect` wins.
async fn dial_first(h: &SyncHandle, utp: &trs_utp::UtpSocket, addrs: Vec<SocketAddr>, expect: DeviceId) -> Option<session::Connected> {
    let mut set = tokio::task::JoinSet::new();
    for a in addrs {
        let (tls, utp) = (h.ctx().tls.clone(), utp.clone());
        set.spawn(async move {
            let c = tokio::time::timeout(PUNCH_WAIT, session::dial_utp(&tls, &utp, a)).await.ok()?.ok()?;
            (c.device() == expect).then_some(c)
        });
    }
    while let Some(r) = set.join_next().await {
        if let Ok(Some(c)) = r {
            set.abort_all();
            return Some(c);
        }
    }
    None
}

/// At A: rendezvous with `target` through `via`, then punch; if that brings no session, ask `via` to relay.
pub(crate) async fn attempt(h: &SyncHandle, target: DeviceId, via: Arc<PeerHandle>) {
    let (tx, rx) = oneshot::channel();
    lock(&h.net().punch.waiting).insert(target, tx);
    let ask = Message::Rendezvous(Rendezvous { target, candidates: own_candidates(h) });
    if via.control.send(ask).await.is_err() {
        lock(&h.net().punch.waiting).remove(&target);
        return;
    }
    // No introduction: `via` cannot reach the target either (or will not introduce us).
    let intro = tokio::time::timeout(INTRODUCE_WAIT, rx).await;
    lock(&h.net().punch.waiting).remove(&target);
    let Ok(Ok(intro)) = intro else { return };

    let lead = intro.punch_at_ms.saturating_sub(now_ms()).min(3000);
    let deadline = Instant::now() + Duration::from_millis(lead) + PROBE_WAIT + PUNCH_WAIT;
    if h.utp().is_some() {
        while Instant::now() < deadline {
            if h.ctx().registry.peer(&target).is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    if h.ctx().registry.peer(&target).is_some() || via.is_closed() {
        return;
    }
    // Refused (a RelayClose ends the pipe) or done: the handshake's result says which.
    if let Some(done) = crate::relay::open_via(h, via, target) {
        let _ = tokio::time::timeout(session::HANDSHAKE_TIMEOUT + Duration::from_secs(1), done).await;
    }
}
