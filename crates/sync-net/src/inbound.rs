//! Can other devices reach this one (SPEC §7G.3, *Can other devices reach this one*)?
//!
//! A device whose sync port a firewall blocks still dials out and syncs with every device that listens, so nothing
//! looks wrong on it; but no device can dial it, and those that rely on it (it hosts a folder, they are behind their
//! own NAT) wait for ever. This module says so only with evidence:
//!
//! * **Reached:** a direct TCP session this device *accepted* from another device (not from this computer), or a
//!   device that dialed it back for a [`ReachAsk`] got through.
//! * **Not reached:** a device this one dialed, asked to dial back (a [`ReachAsk`], feature bit
//!   [`FEATURE_REACH`]), could not connect. It dials back only the IP it sees the session come from, so a device on
//!   the same network tests exactly what blocks it (a firewall on this computer: no router is in between), and a
//!   device elsewhere tests the router's mapping of the sync port as well ([`crate::service::PublicDiscovery`]'s
//!   public address; never asked without one, and dialed back there only at a port the asker's signed address record
//!   names).
//!
//! The latest success and the latest failure are kept per network (the local one, the Internet) for
//! [`EVIDENCE_TTL`], and forgotten when the network changes. [`verdict`] says *blocked* only when some device could not
//! reach this one and no device has reached it within that time: a device that any device reached is never called
//! blocked, whatever failed after. Without either (no session dialed to a device that has the feature bit, nothing
//! accepted) it says *unknown*. One ask per network at most every [`ASK_EVERY`], at once after a network change or
//! [`crate::service::SyncHandle::check_inbound_again`].

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use seedwell_sync_proto::ids::DeviceId;
use seedwell_sync_proto::msg::{AddressRecord, FEATURE_REACH, Message, ReachAnswer, ReachAsk, ReachOutcome};

use crate::service::SyncHandle;
use crate::session::{Via, is_lan};
use crate::shared::{Ctx, PeerHandle, lock};

/// How long one dial back may take to connect (Windows gives up on a silent address after about 3 s with Seedwell's
/// SYN settings, `seedwell_platform::net::connect_tcp`). A silent one is tried twice.
pub const DIAL_BACK_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an ask waits for its answer (two dial backs and the way back); after that it says nothing.
pub const ANSWER_WAIT: Duration = Duration::from_secs(20);
/// One ask per network at most this often (at once after a network change or a check asked for).
pub const ASK_EVERY: Duration = Duration::from_secs(10 * 60);
/// Evidence older than this says nothing (asks while sessions last keep it fresh).
pub const EVIDENCE_TTL: Duration = Duration::from_secs(60 * 60);
/// A device answers one ask of each device at a time, and dials it back no sooner than this after the last time it
/// started to (PROTOCOL.md, type 33); an ask that comes sooner is answered "not tried".
pub const ANSWER_GAP: Duration = Duration::from_secs(60);
/// After a "not tried" the network is asked about again this much later (past the other device's [`ANSWER_GAP`]).
pub const NOT_TRIED_RETRY: Duration = Duration::from_secs(65);
/// A check asked for ([`Reach::again`]) counts as under way until new evidence comes, or this long at most (a "not
/// tried" and its retry included).
pub const CHECK_LIMIT: Duration = Duration::from_secs(120);
/// How often the asking loop looks whether an ask is due.
const LOOK_EVERY: Duration = Duration::from_secs(15);

/// Where a device that tried to reach this one was.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Network {
    /// On a local network of this device (a private, link-local or unique-local address): nothing but this computer
    /// stands between it and the sync port.
    Local,
    /// Elsewhere: the router's port mapping stands between them too.
    Internet,
}

impl Network {
    /// The network of a device seen at `ip`.
    pub fn of(ip: IpAddr) -> Network {
        if is_lan(ip) { Network::Local } else { Network::Internet }
    }
}

/// One piece of evidence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evidence {
    /// When.
    pub at: Instant,
    /// The device's name.
    pub by: String,
    /// The address it dialed (this device's, as that device sees it).
    pub addr: SocketAddr,
}

/// What one network said: its latest success and its latest failure (a failure never hides a success).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    /// The latest device that reached this one.
    pub reached: Option<Evidence>,
    /// The latest device that could not.
    pub failed: Option<Evidence>,
}

impl Seen {
    fn note(&mut self, reached: bool, e: Evidence) {
        if reached {
            self.reached = Some(e);
        } else {
            self.failed = Some(e);
        }
    }

    fn latest(&self) -> Option<Instant> {
        self.reached.iter().chain(self.failed.iter()).map(|e| e.at).max()
    }
}

/// Whether other devices can reach this one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Inbound {
    /// No evidence either way.
    Unknown,
    /// A device reached this one within [`EVIDENCE_TTL`].
    Reachable,
    /// A device this one reached could not reach it back, and none has reached it within [`EVIDENCE_TTL`].
    Blocked {
        /// Where that device was.
        network: Network,
        /// Its name.
        by: String,
        /// The address it dialed.
        addr: SocketAddr,
    },
}

/// The verdict at `now`: [`Inbound::Reachable`] when a device of either network reached this one within
/// [`EVIDENCE_TTL`] (whatever failed since); else [`Inbound::Blocked`] by a fresh failure (the local network's first: it
/// names this computer alone); else [`Inbound::Unknown`].
pub fn verdict(local: &Seen, internet: &Seen, now: Instant) -> Inbound {
    let fresh = |e: &Option<Evidence>| e.as_ref().filter(|e| now.saturating_duration_since(e.at) < EVIDENCE_TTL).cloned();
    if fresh(&local.reached).is_some() || fresh(&internet.reached).is_some() {
        return Inbound::Reachable;
    }
    match (fresh(&local.failed), fresh(&internet.failed)) {
        (Some(e), _) => Inbound::Blocked { network: Network::Local, by: e.by, addr: e.addr },
        (None, Some(e)) => Inbound::Blocked { network: Network::Internet, by: e.by, addr: e.addr },
        (None, None) => Inbound::Unknown,
    }
}

/// Where to dial back for an ask for `asked` that came over a session from `seen`: `seen` with the asked port, only
/// when the asked IP is the one the session comes from (a device can have only itself dialed, never a third
/// computer), never over a relayed session (`seen` is the relay's), never port 0. (`seen`, not the asked address, keeps
/// an IPv6 link-local address's scope.)
pub fn dial_target(asked: &str, seen: SocketAddr, via: Via) -> Option<SocketAddr> {
    if via == Via::Relay {
        return None;
    }
    let asked: SocketAddr = asked.parse().ok()?;
    if asked.port() == 0 || asked.ip().to_canonical() != seen.ip().to_canonical() {
        return None;
    }
    let mut target = seen;
    target.set_port(asked.port());
    Some(target)
}

/// Whether an ask from `device` is dialed back now: not while one for it is under way, and not within `gap` of the
/// last one started for it ([`ANSWER_GAP`]). Admitted, it is noted as started (and under way until
/// [`answered`]).
fn admit(answering: &mut HashMap<DeviceId, (Instant, bool)>, device: DeviceId, now: Instant, gap: Duration) -> bool {
    answering.retain(|_, (at, going)| *going || now.saturating_duration_since(*at) < gap);
    if answering.contains_key(&device) {
        return false;
    }
    answering.insert(device, (now, true));
    true
}

/// Whether a dial back over `network` may go to `port`: on the local network any (it is the asker's own computer);
/// over the Internet the IP may be a NAT other computers share, so only a port the asker's own signed address record
/// names (`named`), never any port of whoever else is behind it.
fn port_allowed(network: Network, port: u16, named: Option<&Vec<u16>>) -> bool {
    network == Network::Local || named.is_some_and(|ports| ports.contains(&port))
}

/// The dial back for `device` ended (its start still counts for the gap).
fn answered(answering: &mut HashMap<DeviceId, (Instant, bool)>, device: DeviceId) {
    if let Some(e) = answering.get_mut(&device) {
        e.1 = false;
    }
}

/// Opens a TCP connection to `target` and closes it: [`ReachOutcome::Connected`] when it opened within `timeout`; a
/// silent address is tried once more (a lost SYN is not a firewall). Never longer than twice `timeout`.
pub async fn dial_back(target: SocketAddr, timeout: Duration) -> ReachOutcome {
    for _ in 0..2 {
        match tokio::time::timeout(timeout, seedwell_platform::net::connect_tcp(target)).await {
            Ok(Ok(_)) => return ReachOutcome::Connected,
            Ok(Err(_)) => return ReachOutcome::Failed,
            Err(_) => {}
        }
    }
    ReachOutcome::Failed
}

/// An ask waiting for its answer.
struct Pending {
    network: Network,
    device: DeviceId,
    name: String,
    addr: SocketAddr,
    sent: Instant,
    /// The session it went over: its answer can come only over that one.
    session: std::sync::Weak<PeerHandle>,
}

/// Drops the asks whose time is up ([`ANSWER_WAIT`]: that network says nothing until its next ask), and those whose
/// session ended before the answer came: their answer can no longer come, so their network is asked again at once
/// (over the next session). Two sessions dialed at once to one device, one of them closed as the other stays, lost the
/// ask that went over it, and the network was not asked again for [`ASK_EVERY`].
fn drop_unanswerable(s: &mut State, now: Instant) {
    let mut lost = Vec::new();
    s.pending.retain(|_, p| {
        if now.saturating_duration_since(p.sent) >= ANSWER_WAIT {
            return false;
        }
        let ended = p.session.upgrade().is_none_or(|h| h.is_closed());
        if ended {
            lost.push(p.network);
        }
        !ended
    });
    for network in lost {
        s.next_ask.remove(&network);
    }
}

struct State {
    local: Seen,
    internet: Seen,
    /// When each network may be asked about again.
    next_ask: HashMap<Network, Instant>,
    pending: HashMap<u32, Pending>,
    next_id: u32,
    /// Asks this device answers: when it last started one for each device, and whether that one is still going.
    answering: HashMap<DeviceId, (Instant, bool)>,
    /// How long that gap is ([`ANSWER_GAP`]; shorter in tests).
    answer_gap: Duration,
    /// How long after a "not tried" this device asks again ([`NOT_TRIED_RETRY`], past the other device's gap; shorter
    /// in tests, with the gap).
    not_tried_retry: Duration,
    /// The ports each connected device's own signed address record names: an ask over the Internet is dialed back
    /// only at one of them.
    record_ports: HashMap<DeviceId, Vec<u16>>,
    /// A check asked for ([`Reach::again`]) and not answered by new evidence yet: since when.
    checking: Option<Instant>,
    /// **Tests:** sessions over loopback count (both devices on this computer), and this port is asked about instead
    /// of the sync port (a closed one plays a firewall).
    test_loopback: bool,
    test_port: Option<u16>,
}

impl Default for State {
    fn default() -> Self {
        State {
            local: Seen::default(),
            internet: Seen::default(),
            next_ask: HashMap::new(),
            pending: HashMap::new(),
            next_id: 0,
            answering: HashMap::new(),
            answer_gap: ANSWER_GAP,
            not_tried_retry: NOT_TRIED_RETRY,
            record_ports: HashMap::new(),
            checking: None,
            test_loopback: false,
            test_port: None,
        }
    }
}

impl State {
    fn seen(&mut self, network: Network) -> &mut Seen {
        match network {
            Network::Local => &mut self.local,
            Network::Internet => &mut self.internet,
        }
    }
}

/// What this device knows about being reached (one per service, in [`Ctx`]).
#[derive(Default)]
pub struct Reach {
    state: std::sync::Mutex<State>,
    wake: Notify,
}

impl Reach {
    /// The verdict now.
    pub fn inbound(&self) -> Inbound {
        let s = lock(&self.state);
        verdict(&s.local, &s.internet, Instant::now())
    }

    /// A check asked for ([`Self::again`]) is under way: no new evidence since, and not given up ([`CHECK_LIMIT`]).
    pub fn checking(&self) -> bool {
        let s = lock(&self.state);
        s.checking.is_some_and(|t| {
            let newer = [&s.local, &s.internet].iter().any(|n| n.latest().is_some_and(|at| at >= t));
            !newer && t.elapsed() < CHECK_LIMIT
        })
    }

    fn note(&self, network: Network, reached: bool, e: Evidence) {
        lock(&self.state).seen(network).note(reached, e);
    }

    /// The network changed: what was known is about the old one. Asks go out again at once.
    pub(crate) fn forget(&self) {
        {
            let mut s = lock(&self.state);
            s.local = Seen::default();
            s.internet = Seen::default();
            s.next_ask.clear();
            s.pending.clear();
        }
        self.wake.notify_one();
    }

    /// Ask again now (the user allowed Seedwell through a firewall): asks waiting for an answer are dropped (their
    /// answers would be about before), every network is asked about at once, and [`Self::checking`] holds until new
    /// evidence comes. Evidence stays until new evidence replaces it.
    pub(crate) fn again(&self) {
        {
            let mut s = lock(&self.state);
            s.next_ask.clear();
            s.pending.clear();
            s.checking = Some(Instant::now());
        }
        self.wake.notify_one();
    }

    /// **Tests:** see [`State::test_loopback`].
    pub(crate) fn test(&self, loopback: bool, port: Option<u16>) {
        let mut s = lock(&self.state);
        s.test_loopback = loopback;
        s.test_port = port;
    }

    /// **Tests:** the gap between two dial backs for one device, and the wait after a "not tried" past it (in the same
    /// proportion as [`ANSWER_GAP`] and [`NOT_TRIED_RETRY`]). Both devices of a test get it: an asking device that waited
    /// the full retry after a short gap outlived the test's wait.
    #[cfg(test)]
    pub(crate) fn test_answer_gap(&self, gap: Duration) {
        let mut s = lock(&self.state);
        s.answer_gap = gap;
        s.not_tried_retry = gap + gap / 12;
    }

    fn counts(&self, ip: IpAddr) -> bool {
        !ip.is_loopback() || lock(&self.state).test_loopback
    }

    /// Takes `device`'s answer: evidence for the network its ask was about, unless it was not tried (then that network
    /// is asked about again after [`NOT_TRIED_RETRY`]), or it is not the answer to an ask of ours to that device (an
    /// unknown id, another device's, one dropped). Returns the evidence noted.
    fn take_answer(&self, device: DeviceId, a: &ReachAnswer, now: Instant) -> Option<(Network, bool, Evidence)> {
        let mut s = lock(&self.state);
        let p = match s.pending.get(&a.id) {
            Some(p) if p.device == device => s.pending.remove(&a.id)?,
            _ => return None,
        };
        let reached = match a.outcome {
            ReachOutcome::Connected => true,
            ReachOutcome::Failed => false,
            ReachOutcome::NotTried => {
                let retry = s.not_tried_retry;
                s.next_ask.insert(p.network, now + retry);
                return None;
            }
        };
        let e = Evidence { at: now, by: p.name, addr: p.addr };
        s.seen(p.network).note(reached, e.clone());
        Some((p.network, reached, e))
    }
}

/// A session came up: one this device accepted over TCP is evidence that it can be reached; one it dialed may be
/// asked to dial back (the asking loop is woken).
pub(crate) fn session_up(ctx: &Ctx, peer: &PeerHandle) {
    if peer.via == Via::Tcp && !peer.dialed && ctx.reach.counts(peer.remote.ip()) {
        let addr = SocketAddr::new(peer.local.ip(), peer.local.port());
        ctx.reach.note(Network::of(peer.remote.ip()), true, Evidence { at: Instant::now(), by: peer.name.clone(), addr });
    } else if peer.dialed && peer.has(FEATURE_REACH) {
        ctx.reach.wake.notify_one();
    }
}

/// Address records a peer passed on: the ports of its own (signed by it) are where an ask of its over the Internet may
/// be dialed back.
pub(crate) fn records(ctx: &Ctx, peer: &PeerHandle, records: &[AddressRecord]) {
    let Some(own) = records.iter().find(|r| r.device_pk == peer.device_pk && r.verify()) else { return };
    let ports = own.addrs.iter().filter_map(|a| a.parse::<SocketAddr>().ok()).map(|a| a.port()).collect();
    lock(&ctx.reach.state).record_ports.insert(peer.device, ports);
}

/// Asks while the service runs: whenever woken, and every [`LOOK_EVERY`].
pub(crate) async fn run(h: SyncHandle) {
    loop {
        ask_due(&h, Instant::now());
        let reach = &h.inner.ctx.reach;
        tokio::select! {
            _ = reach.wake.notified() => {}
            _ = tokio::time::sleep(LOOK_EVERY) => {}
        }
    }
}

/// Sends the asks that are due: per network, to a peer this device dialed directly over TCP that has the feature bit,
/// unless one is waiting for its answer or that network was asked about within [`ASK_EVERY`].
fn ask_due(h: &SyncHandle, now: Instant) {
    let ctx = &h.inner.ctx;
    let peers = ctx.registry.peers();
    let public = h.inner.discovery.as_ref().and_then(|d| d.public_address());
    let mut s = lock(&ctx.reach.state);
    drop_unanswerable(&mut s, now);
    for network in [Network::Local, Network::Internet] {
        let waiting = s.pending.values().any(|p| p.network == network);
        if waiting || s.next_ask.get(&network).is_some_and(|t| now < *t) {
            continue;
        }
        let Some(p) = peers.iter().find(|p| {
            (!p.remote.ip().is_loopback() || s.test_loopback)
                && p.dialed
                && p.via == Via::Tcp
                && p.has(FEATURE_REACH)
                && Network::of(p.remote.ip()) == network
        }) else {
            continue;
        };
        let addr = match network {
            // The address the peer reached us from, with the sync port: on the same network it sees us there.
            Network::Local => SocketAddr::new(p.local.ip(), s.test_port.unwrap_or(h.inner.port)),
            Network::Internet => match public {
                Some(a) => a,
                None => continue,
            },
        };
        s.next_id = s.next_id.wrapping_add(1);
        let id = s.next_id;
        s.pending.insert(id, Pending { network, device: p.device, name: p.name.clone(), addr, sent: now, session: Arc::downgrade(p) });
        s.next_ask.insert(network, now + ASK_EVERY);
        let _ = p.control.try_send(Message::ReachAsk(ReachAsk { id, addr: addr.to_string() }));
    }
}

/// Another device asks to be dialed back: answered after one dial back, or at once with [`ReachOutcome::NotTried`]
/// (not its own address; a relayed session; over the Internet, a port its signed address record does not name; one
/// for it under way, or started within [`ANSWER_GAP`]).
pub(crate) fn on_ask(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>, q: ReachAsk) {
    let not_tried = || {
        let _ = peer.control.try_send(Message::ReachAnswer(ReachAnswer { id: q.id, outcome: ReachOutcome::NotTried }));
    };
    let Some(target) = dial_target(&q.addr, peer.remote, peer.via) else { return not_tried() };
    {
        let mut s = lock(&ctx.reach.state);
        if !port_allowed(Network::of(peer.remote.ip()), target.port(), s.record_ports.get(&peer.device)) {
            return not_tried();
        }
        let gap = s.answer_gap;
        if !admit(&mut s.answering, peer.device, Instant::now(), gap) {
            return not_tried();
        }
    }
    let (ctx, peer) = (ctx.clone(), peer.clone());
    tokio::spawn(async move {
        let outcome = dial_back(target, DIAL_BACK_TIMEOUT).await;
        answered(&mut lock(&ctx.reach.state).answering, peer.device);
        let _ = peer.control.send(Message::ReachAnswer(ReachAnswer { id: q.id, outcome })).await;
    });
}

/// The answer to one of this device's asks ([`Reach::take_answer`]).
pub(crate) fn on_answer(ctx: &Ctx, peer: &PeerHandle, a: ReachAnswer) {
    if let Some((network, false, e)) = ctx.reach.take_answer(peer.device, &a, Instant::now()) {
        eprintln!("seedwell sync: {} could not connect to this device at {} ({network:?} network)", e.by, e.addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(at: Instant) -> Evidence {
        Evidence { at, by: "Mac".into(), addr: "192.168.68.73:51710".parse().unwrap() }
    }

    fn seen(reached: Option<Instant>, failed: Option<Instant>) -> Seen {
        Seen { reached: reached.map(ev), failed: failed.map(ev) }
    }

    /// The failure: a device called blocked without a device having failed to reach it (no evidence, or only evidence
    /// that is old), or called blocked while some device reached it within the hour (a firewall that lets the local
    /// network in, a dial back that failed once).
    #[test]
    fn blocked_only_with_a_fresh_failure_and_no_fresh_success() {
        let now = Instant::now() + EVIDENCE_TTL * 3;
        let none = Seen::default();
        let (fail, ok) = (seen(None, Some(now)), seen(Some(now), None));
        let old_ok = seen(Some(now - EVIDENCE_TTL - Duration::from_secs(1)), None);
        assert_eq!(verdict(&none, &none, now), Inbound::Unknown, "no evidence");
        assert_eq!(verdict(&seen(None, Some(now - EVIDENCE_TTL)), &none, now), Inbound::Unknown, "a failure an hour old");
        assert_eq!(verdict(&ok, &none, now), Inbound::Reachable);
        assert_eq!(verdict(&fail, &ok, now), Inbound::Reachable, "reached from the Internet");
        assert_eq!(verdict(&ok, &fail, now), Inbound::Reachable, "reached on the local network");
        let blocked = Inbound::Blocked { network: Network::Local, by: "Mac".into(), addr: "192.168.68.73:51710".parse().unwrap() };
        assert_eq!(verdict(&fail, &old_ok, now), blocked, "a success over an hour ago does not count");
        assert_eq!(verdict(&fail, &seen(None, Some(now)), now), blocked, "the local network's failure first");
        assert!(matches!(verdict(&none, &fail, now), Inbound::Blocked { network: Network::Internet, .. }));
    }

    /// The failure (review): one slot per network, so a failure noted after a success on the same network (another
    /// device's own outbound filter, a member that lies) overwrote it, and a device a device had just dialed into was
    /// called blocked.
    #[test]
    fn a_later_failure_on_the_same_network_does_not_erase_a_fresh_success() {
        let reach = Reach::default();
        let t = Instant::now();
        reach.note(Network::Local, true, ev(t));
        reach.note(Network::Local, false, ev(t + Duration::from_secs(600)));
        assert_eq!(reach.inbound(), Inbound::Reachable);
        let s = lock(&reach.state);
        assert_eq!(verdict(&s.local, &s.internet, t + EVIDENCE_TTL + Duration::from_secs(1)), Inbound::Blocked {
            network: Network::Local,
            by: "Mac".into(),
            addr: "192.168.68.73:51710".parse().unwrap()
        }, "once the success is old, the failure stands");
    }

    /// The failure (review): an answer taken from a device that was not asked (a member answering another's ask, an id
    /// it made up), or one answer counted twice, makes a device blocked or reachable on another's word; a "not tried"
    /// taken as evidence.
    #[test]
    fn only_the_asked_device_answers_its_own_ask_once() {
        let reach = Reach::default();
        let (asked, other) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let now = Instant::now();
        let pending = |id| (id, Pending { network: Network::Local, device: asked, name: "Mac".into(), addr: "192.168.68.73:51710".parse().unwrap(), sent: now, session: std::sync::Weak::new() });
        lock(&reach.state).pending.extend([pending(7), pending(8)]);
        let failed = |id| ReachAnswer { id, outcome: ReachOutcome::Failed };
        assert_eq!(reach.take_answer(other, &failed(7), now), None, "another device");
        assert_eq!(reach.take_answer(asked, &failed(99), now), None, "an unknown id");
        assert_eq!(reach.inbound(), Inbound::Unknown);
        assert_eq!(reach.take_answer(asked, &ReachAnswer { id: 8, outcome: ReachOutcome::NotTried }, now), None, "not tried");
        assert!(lock(&reach.state).next_ask.get(&Network::Local).is_some_and(|t| *t >= now + NOT_TRIED_RETRY), "asked again later");
        assert!(reach.take_answer(asked, &failed(7), now).is_some());
        assert_eq!(reach.take_answer(asked, &failed(7), now), None, "counted once");
        assert!(matches!(reach.inbound(), Inbound::Blocked { .. }));
    }

    /// The failure: two sessions dialed to one device at once (the link's address and one added), the ask went over
    /// the one that was then closed, its answer could never come, and the network was not asked again for
    /// [`ASK_EVERY`]: the device stayed "unknown" though nothing could reach it. An ask whose session ended is dropped
    /// and its network asked again at once; one whose session lives waits for its answer.
    #[test]
    fn an_ask_whose_session_ended_is_asked_again_at_once() {
        let addr: SocketAddr = "192.168.68.73:51710".parse().unwrap();
        let agreed = seedwell_sync_proto::msg::Agreed { protocol: seedwell_sync_proto::msg::PROTOCOL, features: FEATURE_REACH };
        let session = || PeerHandle::new([7; 32], "Mac".into(), "Seedwell".into(), agreed, addr, addr, [0; 32], true, Via::Tcp).0;
        let (gone, live) = (session(), session());
        let now = Instant::now();
        let mut s = State::default();
        let ask = |network, p: &Arc<PeerHandle>| Pending { network, device: p.device, name: "Mac".into(), addr, sent: now, session: Arc::downgrade(p) };
        s.pending.insert(1, ask(Network::Local, &gone));
        s.pending.insert(2, ask(Network::Internet, &live));
        s.next_ask.insert(Network::Local, now + ASK_EVERY);
        s.next_ask.insert(Network::Internet, now + ASK_EVERY);
        gone.close();
        drop_unanswerable(&mut s, now + Duration::from_secs(1));
        assert_eq!(s.pending.keys().copied().collect::<Vec<_>>(), [2], "the live session's ask still waits");
        assert!(!s.next_ask.contains_key(&Network::Local), "asked again at once");
        assert!(s.next_ask.contains_key(&Network::Internet));
        drop_unanswerable(&mut s, now + ANSWER_WAIT);
        assert!(s.pending.is_empty() && s.next_ask.contains_key(&Network::Internet), "unanswered in time: nothing said, not asked again");
    }

    /// The failure (review): a device dialing back as often as it is asked (a member making it connect to its own IP
    /// many times a minute), or one device's asks holding back another's.
    #[test]
    fn a_device_is_dialed_back_at_most_once_a_minute() {
        let mut answering = HashMap::new();
        let (a, b) = (DeviceId([1; 32]), DeviceId([2; 32]));
        let t = Instant::now();
        assert!(admit(&mut answering, a, t, ANSWER_GAP));
        assert!(!admit(&mut answering, a, t + Duration::from_secs(1), ANSWER_GAP), "one under way");
        answered(&mut answering, a);
        assert!(!admit(&mut answering, a, t + ANSWER_GAP - Duration::from_secs(1), ANSWER_GAP), "within the minute");
        assert!(admit(&mut answering, b, t + Duration::from_secs(1), ANSWER_GAP), "another device");
        assert!(admit(&mut answering, a, t + ANSWER_GAP, ANSWER_GAP), "a minute after the last");
    }

    /// The failure (review): an ask over the Internet, where the IP may be a NAT that other computers share, used to
    /// make this device connect to any port of another computer behind it.
    #[test]
    fn over_the_internet_only_a_port_the_askers_record_names_is_dialed() {
        let named = vec![51710];
        assert!(port_allowed(Network::Internet, 51710, Some(&named)));
        assert!(!port_allowed(Network::Internet, 22, Some(&named)), "another port behind the same NAT");
        assert!(!port_allowed(Network::Internet, 51710, None), "no record from it yet");
        assert!(port_allowed(Network::Local, 22, None), "on the local network the IP is the asker's own computer");
    }

    /// The failure (review): after the user's click, an ask already waiting kept the new one from going out, and the
    /// check looked over before any device said so; the host then told the user "something else stops it".
    #[test]
    fn a_check_asked_for_drops_waiting_asks_and_holds_until_new_evidence() {
        let reach = Reach::default();
        let now = Instant::now();
        lock(&reach.state).pending.insert(1, Pending { network: Network::Local, device: DeviceId([1; 32]), name: "Mac".into(), addr: "192.168.68.73:51710".parse().unwrap(), sent: now, session: std::sync::Weak::new() });
        lock(&reach.state).next_ask.insert(Network::Local, now + ASK_EVERY);
        assert!(!reach.checking());
        reach.again();
        assert!(lock(&reach.state).pending.is_empty() && lock(&reach.state).next_ask.is_empty(), "asked at once");
        assert!(reach.checking());
        reach.note(Network::Local, true, ev(Instant::now()));
        assert!(!reach.checking(), "the answer came");
    }

    /// The failure: a device made to dial a third computer (an ask naming another IP than the one its session comes
    /// from: a scan by proxy), a relayed session's relay dialed, or an IPv6 link-local address dialed without its scope.
    #[test]
    fn a_dial_back_goes_only_to_the_address_the_session_comes_from() {
        let seen: SocketAddr = "192.168.68.73:61234".parse().unwrap();
        assert_eq!(dial_target("192.168.68.73:51710", seen, Via::Tcp), Some("192.168.68.73:51710".parse().unwrap()));
        assert_eq!(dial_target("192.168.68.74:51710", seen, Via::Tcp), None, "another computer");
        assert_eq!(dial_target("192.168.68.73:51710", seen, Via::Relay), None, "the relay's address");
        assert_eq!(dial_target("192.168.68.73:0", seen, Via::Tcp), None);
        assert_eq!(dial_target("not an address", seen, Via::Tcp), None);
        let mapped: SocketAddr = "[::ffff:192.168.68.73]:61234".parse().unwrap();
        assert_eq!(dial_target("192.168.68.73:51710", mapped, Via::Tcp).map(|a| a.port()), Some(51710), "IPv4 seen on a dual-stack socket");
        let scoped = SocketAddr::V6(std::net::SocketAddrV6::new("fe80::1".parse().unwrap(), 61234, 0, 7));
        let SocketAddr::V6(t) = dial_target("[fe80::1]:51710", scoped, Via::Tcp).unwrap() else { panic!() };
        assert_eq!((t.port(), t.scope_id()), (51710, 7), "the scope kept");
    }

    async fn device(dir: &std::path::Path, name: &str, features: u64) -> SyncHandle {
        let secrets = Arc::new(crate::secrets::FileSecrets::new(dir.join("secrets")));
        let mut o = crate::service::Options::new(dir.to_path_buf(), "Seedwell test".into(), secrets);
        o.default_device_name = name.into();
        o.bind = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        o.lan_discovery = false;
        o.features = features;
        // Only B dials: a session A dialed would be B reached, whatever the dial back says.
        if name == "Alpha" {
            o.dials = Some(0);
        }
        SyncHandle::start(o).await.unwrap()
    }

    async fn until(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("timed out waiting for: {what}");
    }

    /// A (answering with `a_features`) shares a folder with B; B dials A, A never dials B. B's dial back goes to
    /// `port` (B's own sync port when `None`).
    async fn pair(work: &std::path::Path, a_features: u64, port: Option<u16>) -> (SyncHandle, SyncHandle) {
        let a = device(&work.join("a"), "Alpha", a_features).await;
        let b = device(&work.join("b"), "Beta", seedwell_sync_proto::msg::FEATURES).await;
        a.test_reach(true, None);
        a.inner.ctx.reach.test_answer_gap(Duration::from_millis(500));
        b.inner.ctx.reach.test_answer_gap(Duration::from_millis(500));
        b.test_reach(true, port);
        let docs = work.join("Docs");
        std::fs::create_dir_all(&docs).unwrap();
        let id: seedwell_sync_proto::ids::FolderId = a.add_folder(docs).await.unwrap().id.parse().unwrap();
        let link = a.create_invite(&id, seedwell_sync_proto::members::Role::ReadWrite, 1, 7).unwrap().link;
        b.join_folder(&link, work.join("B")).await.unwrap();
        b.add_address(&id, SocketAddr::new(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), a.port())).unwrap();
        let ad = a.device_id();
        until(20, "B connected to A", || b.inner.ctx.registry.peer(&ad).is_some()).await;
        (a, b)
    }

    /// A loopback port that refuses connections (a firewall's), and nothing else can listen at
    /// ([`seedwell_platform::net::closed_tcp_port`]). (Bound to port 0 and let go, as before, another test listened at it, the
    /// dial back connected, and B counted as reached.)
    fn closed_port() -> u16 {
        seedwell_platform::net::closed_tcp_port(&[IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)])
    }

    /// The failure the user met on Windows: a device whose sync port a firewall blocks syncs with every device it
    /// dials, so nothing told it that no device could dial it. Here B's dial back goes to a closed port (the firewall):
    /// B is blocked, named by the device that tried; A, which B reached, is never called blocked. Once the port answers
    /// (the user allowed Seedwell) a check asked for clears it without waiting for the next round, and says it is
    /// checking until then.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_device_no_peer_can_dial_back_is_blocked_until_one_can() {
        let work = tempfile::tempdir().unwrap();
        let (a, b) = pair(work.path(), seedwell_sync_proto::msg::FEATURES, Some(closed_port())).await;
        until(30, "B blocked", || matches!(b.inbound(), Inbound::Blocked { .. })).await;
        let Inbound::Blocked { network, by, .. } = b.inbound() else { unreachable!() };
        assert_eq!((network, by.as_str()), (Network::Local, "Alpha"));
        assert_eq!(a.inbound(), Inbound::Reachable, "A accepted B's session");
        b.test_reach(true, Some(b.port()));
        // Past A's gap between two dial backs for B (half a second here, a minute for real).
        tokio::time::sleep(Duration::from_millis(600)).await;
        // At once, as the host asks after the user's action.
        b.check_inbound_again();
        assert!(b.inbound_checking());
        until(30, "B reachable", || b.inbound() == Inbound::Reachable).await;
        assert!(!b.inbound_checking());
        a.shutdown().await;
        b.shutdown().await;
    }

    /// The failure: a device whose peers are all older (no dial back to ask for) called blocked because nobody
    /// answered. It stays unknown.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn silence_from_a_device_without_the_feature_is_no_evidence() {
        let work = tempfile::tempdir().unwrap();
        let (a, b) = pair(work.path(), seedwell_sync_proto::msg::FEATURES & !FEATURE_REACH, Some(closed_port())).await;
        b.check_inbound_again();
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(b.inbound(), Inbound::Unknown);
        a.shutdown().await;
        b.shutdown().await;
    }

    /// The failure: a dial back that says connected where nothing listens (a firewall reported as open), or failed
    /// where something does; or one that waits longer than its two tries.
    #[tokio::test]
    async fn a_dial_back_tells_a_listener_from_a_closed_port() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = l.local_addr().unwrap();
        assert_eq!(dial_back(open, DIAL_BACK_TIMEOUT).await, ReachOutcome::Connected);
        let closed = SocketAddr::new(open.ip(), closed_port());
        let started = Instant::now();
        assert_eq!(dial_back(closed, Duration::from_millis(500)).await, ReachOutcome::Failed);
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }
}
