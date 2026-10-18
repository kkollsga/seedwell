//! `HostGate`: the only code that can produce an outgoing tracker request.
//!
//! [`GatedRequest`] has private fields and no public constructor, so the HTTP
//! and UDP transports (which take `&GatedRequest`) cannot be handed anything
//! that did not pass through [`HostGate::poll`]. The gate enforces, per SPEC 7C.2:
//!
//! * at most 1 request in flight or queued per (torrent, tracker);
//! * at most 2 concurrent requests per host;
//! * a per-host token bucket (default 1 request/s sustained, burst 3) as a hard
//!   ceiling, cross-checked by an independent sliding-window audit;
//! * at quit only ([`HostGate::begin_quit`]): `stopped` announces leave outside the
//!   per-host cap and the token bucket, limited by one global cap
//!   ([`GateConfig::quit_max_in_flight`], 50), because the process is leaving, a
//!   `stopped` is the only thing still queued and its answer is irrelevant;
//! * a host circuit breaker: when several *different* torrents fail on a host
//!   with no success in between, the whole host is paused with exponential
//!   backoff and re-opened with a single probe;
//! * private/registered-URL containment (invariant 7);
//! * an independent audit of the wire-level invariants ([`Auditor`]);
//! * fail-closed behaviour: any violation stops the host and raises an event.
//!
//! The gate is sans-I/O: all time is passed in, so it is testable with a
//! virtual clock. An async driver sits on top.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use crate::audit::Auditor;
use crate::fastmap::FastMap;
use crate::machine::SendKind;
use crate::response::Timing;
use crate::rng::{Rng, SplitMix64};
use crate::time::{Millis, secs};
use crate::types::{AnnounceParams, Event, InfoHash20, RequestId};
use crate::url::{Scheme, TrackerUrl, canonical_host};

/// Identifies one (torrent, tracker) pair: the info-hash and the tracker's
/// stable slot number within the torrent's `TrackerSet`.
pub type ReqKey = (InfoHash20, u16);

/// Scrapes live in their own key space (`slot | SCRAPE_SLOT_BIT`) so that a
/// scrape in flight can never make the gate refuse an announce (which would
/// trip the machine). They still pass through the same host bucket and
/// concurrency caps, and are limited to one per `scrape_min_gap`.
pub const SCRAPE_SLOT_BIT: u16 = 0x4000;

/// Gate tuning. Defaults are the SPEC 7C.2 values and should not be loosened.
#[derive(Clone, Debug)]
pub struct GateConfig {
    /// Concurrent requests per host.
    pub max_in_flight_per_host: usize,
    /// One token every this long (sustained rate).
    pub token_interval: Duration,
    /// Bucket capacity in tokens (burst).
    pub burst: u32,
    /// Distinct torrents failing (without an intervening success) to open the breaker.
    pub breaker_threshold: usize,
    /// Failures older than this are forgotten.
    pub breaker_window: Duration,
    /// First pause when the breaker opens; doubles per consecutive opening.
    pub breaker_base: Duration,
    /// Longest breaker pause (nominal).
    pub breaker_cap: Duration,
    /// A released HTTP request not answered within this long is declared timed out.
    pub http_deadline: Duration,
    /// Same for UDP (BEP 15 retransmission can take minutes).
    pub udp_deadline: Duration,
    /// Minimum time between scrapes of one (torrent, tracker): the larger of
    /// this (30 min) and the tracker's last announce interval (SPEC 7B.3:
    /// "no more often than the announce interval"). While no announce has been
    /// answered yet the interval is unknown and twice this value is used.
    pub scrape_min_gap: Duration,
    /// Tokens a host's bucket holds when the host is first seen. Zero in
    /// production: a restart loop must not hand a host a fresh burst each time.
    pub initial_tokens: u32,
    /// Seed for breaker jitter.
    pub seed: u64,
    /// After [`HostGate::begin_quit`]: `stopped` announces in flight at once, over all
    /// hosts together (and so also per host). Outside the per-host cap and the token
    /// bucket, which only regular announces keep. libtorrent's
    /// `max_concurrent_http_announces` default.
    pub quit_max_in_flight: usize,
    /// After [`HostGate::begin_quit`]: a released `stopped` not finished within this long is
    /// declared timed out (a backstop; the transports give up sooner).
    pub quit_stop_deadline: Duration,
}

impl Default for GateConfig {
    fn default() -> Self {
        GateConfig {
            max_in_flight_per_host: 2,
            token_interval: secs(1),
            burst: 3,
            breaker_threshold: 3,
            breaker_window: secs(600),
            breaker_base: secs(60),
            breaker_cap: secs(3600),
            http_deadline: secs(60),
            udp_deadline: secs(600),
            scrape_min_gap: secs(1800),
            initial_tokens: 0,
            seed: 0x5eed,
            quit_max_in_flight: 50,
            quit_stop_deadline: secs(5),
        }
    }
}

/// What a request does.
#[derive(Clone, Debug)]
enum Payload {
    Announce {
        params: AnnounceParams,
        kind: SendKind,
    },
    Scrape {
        hashes: Vec<InfoHash20>,
    },
}

/// Handle quoted back to the gate when a request finishes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Ticket {
    key: ReqKey,
    id: RequestId,
    seq: u64,
    event: Option<Event>,
}

impl Ticket {
    /// The (torrent, tracker slot) it belongs to.
    pub fn key(&self) -> ReqKey {
        self.key
    }
    /// The machine's request id.
    pub fn id(&self) -> RequestId {
        self.id
    }
    /// Event of the announce this ticket belongs to.
    pub fn event(&self) -> Option<Event> {
        self.event
    }
}

/// A request that has passed every check and may now be put on the wire.
///
/// No public constructor, no `Clone`: the only way to get one is
/// [`HostGate::poll`], and each release consumed a token.
#[derive(Debug)]
pub struct GatedRequest {
    ticket: Ticket,
    url: Arc<TrackerUrl>,
    payload: Payload,
    released_at: Millis,
}

impl GatedRequest {
    // Deliberately private: only code in this module can mint a request.
    fn new(ticket: Ticket, url: Arc<TrackerUrl>, payload: Payload, released_at: Millis) -> Self {
        GatedRequest {
            ticket,
            url,
            payload,
            released_at,
        }
    }

    /// Ticket to hand back to [`HostGate::finish`].
    pub fn ticket(&self) -> Ticket {
        self.ticket
    }
    /// Target tracker. Contains the passkey in its raw form: never log it.
    pub fn url(&self) -> &TrackerUrl {
        &self.url
    }
    /// Announce parameters, or `None` for a scrape.
    pub fn announce_params(&self) -> Option<&AnnounceParams> {
        match &self.payload {
            Payload::Announce { params, .. } => Some(params),
            Payload::Scrape { .. } => None,
        }
    }
    /// Info-hashes of a scrape request.
    pub fn scrape_hashes(&self) -> Option<&[InfoHash20]> {
        match &self.payload {
            Payload::Scrape { hashes } => Some(hashes),
            Payload::Announce { .. } => None,
        }
    }
    /// Virtual time of release.
    pub fn released_at(&self) -> Millis {
        self.released_at
    }
    /// The announce event, if this is an announce with one.
    pub fn event(&self) -> Option<Event> {
        self.announce_params().and_then(|p| p.event)
    }
}

/// What the gate needs to know about how a request ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outcome {
    /// Successful announce; `timing` is the clamped schedule.
    Success {
        /// Clamped interval pair.
        timing: Timing,
    },
    /// Scrape succeeded, or the tracker answered but refused (`failure reason`):
    /// the host is alive.
    HostAlive,
    /// Timeout, DNS, connect, TLS, 5xx, 429, garbage: feeds the circuit breaker.
    HostFailure,
    /// Neither (cancelled by us).
    Neutral,
    /// Like [`Outcome::HostFailure`] (DNS, connect or TLS failure) and, in
    /// addition, the request provably never reached the tracker: a `completed`
    /// among them may be sent again (the auditor forgets it).
    HostFailureUnsent,
    /// The request never left the process for a local reason (policy refusal,
    /// failed durable write): neutral for the circuit breaker, and a
    /// `completed` among them may be sent again.
    Unsent,
}

/// Why `submit` said no.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SubmitError {
    /// Torrent never registered.
    UnknownTorrent,
    /// URL is not one of the torrent's registered trackers (invariant 7).
    UrlNotRegistered,
    /// A request for this (torrent, tracker) is already queued or in flight.
    Busy,
    /// The host has been stopped by a failed invariant check.
    HostTripped(String),
    /// Scrape asked for too soon.
    ScrapeTooSoon,
}

/// Notable gate happenings for the engine/UI/diagnostics.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GateEvent {
    /// The circuit breaker opened for `host` until `until`.
    HostPaused {
        /// Host.
        host: String,
        /// Reopen time.
        until: Millis,
    },
    /// The breaker closed again after a successful probe.
    HostResumed {
        /// Host.
        host: String,
    },
    /// An invariant check failed; the host is blocked until `reset_host`. The
    /// listed queued requests were dropped and their machines must be told.
    HostTripped {
        /// Host.
        host: String,
        /// What failed.
        reason: String,
        /// Queued requests that were dropped.
        dropped: Vec<(ReqKey, RequestId)>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BreakerState {
    Closed,
    Open { until: Millis },
    HalfOpen,
}

#[derive(Debug)]
struct Breaker {
    state: BreakerState,
    level: u32,
    recent: VecDeque<(Millis, InfoHash20)>,
}

/// Interned host name.
type HostId = u32;

struct Reg {
    url: Arc<TrackerUrl>,
    /// The independent protocol audit of this (torrent, tracker) pair; its
    /// `tag` holds the interned host id.
    aud: Auditor,
}

impl Reg {
    fn host(&self) -> HostId {
        self.aud.tag()
    }
}

struct Queued {
    key: ReqKey,
    id: RequestId,
    seq: u64,
    url: Arc<TrackerUrl>,
    payload: Payload,
    deadline: Duration,
}

/// Counters per host, for the runtime self-audit view ("Advanced").
#[derive(Clone, Copy, Default, Debug)]
pub struct HostStats {
    /// Requests released.
    pub released: u64,
    /// Requests that ended in a host failure.
    pub failures: u64,
    /// Times the breaker opened.
    pub breaker_opens: u64,
}

struct HostState {
    /// Token credit in milliseconds-of-refill units; a request costs `token_interval`.
    /// Starts at zero (see `register_torrent`).
    credit_ms: i64,
    last_refill: Millis,
    in_flight: usize,
    queues: [VecDeque<Queued>; 3],
    queued_len: usize,
    breaker: Breaker,
    tripped: Option<String>,
    sent_ring: VecDeque<Millis>,
    stats: HostStats,
    /// When the host last answered (any answer): the outage probe prefers such a host.
    last_ok: Millis,
}

struct InFlight {
    host: HostId,
    id: RequestId,
    seq: u64,
    event: Option<Event>,
    deadline_at: Millis,
    is_scrape: bool,
    /// Released under the quit rules: counted in `quit_in_flight`, not in the host's.
    quit: bool,
}

const RING: usize = 64;
/// Queue capacity kept after a host's queue drains (see `poll_host`).
const QUEUE_KEEP: usize = 16;
/// Hash map capacity kept after a map drains.
const MAP_KEEP: usize = 512;

/// One token's cost in milliseconds.
fn interval_ms(cfg: &GateConfig) -> i64 {
    i64::try_from(cfg.token_interval.as_millis()).unwrap_or(i64::MAX)
}

/// The gate. See the module docs.
pub struct HostGate {
    cfg: GateConfig,
    rng: SplitMix64,
    /// Hosts by id (ids are assigned in first-seen order, which keeps
    /// iteration order deterministic).
    hosts: Vec<HostState>,
    host_names: Vec<Arc<str>>,
    host_ids: HashMap<Arc<str>, HostId>,
    active: BTreeSet<HostId>,
    registered: FastMap<InfoHash20, Box<[Reg]>>,
    queued: FastMap<ReqKey, (HostId, u64)>,
    in_flight: FastMap<ReqKey, InFlight>,
    deadlines: BTreeSet<(Millis, ReqKey, u64)>,
    last_scrape: FastMap<ReqKey, Millis>,
    hold_until: Millis,
    seq: u64,
    events: Vec<GateEvent>,
    /// The process is quitting (see [`HostGate::begin_quit`]).
    quit_mode: bool,
    /// `stopped` requests in flight that were released under the quit rules.
    quit_in_flight: usize,
}

fn priority(payload: &Payload) -> usize {
    match payload {
        Payload::Announce { params, .. } => match params.event {
            Some(Event::Stopped) => 0,
            Some(_) => 1,
            None => 2,
        },
        Payload::Scrape { .. } => 2,
    }
}

impl HostGate {
    /// New gate.
    pub fn new(cfg: GateConfig) -> Self {
        HostGate {
            rng: SplitMix64::new(cfg.seed),
            cfg,
            hosts: Vec::new(),
            host_names: Vec::new(),
            host_ids: HashMap::new(),
            active: BTreeSet::new(),
            registered: FastMap::default(),
            queued: FastMap::default(),
            in_flight: FastMap::default(),
            deadlines: BTreeSet::new(),
            last_scrape: FastMap::default(),
            hold_until: Millis::ZERO,
            seq: 0,
            events: Vec::new(),
            quit_mode: false,
            quit_in_flight: 0,
        }
    }

    /// The process is quitting: from now on a queued `stopped` is released as soon as fewer than
    /// [`GateConfig::quit_max_in_flight`] quit-time `stopped` requests are in flight (all
    /// hosts together), without the per-host cap and the token bucket. An open circuit breaker
    /// still holds a host back (it is down; there is nothing to tell it), and quit-time `stopped`
    /// failures never feed the breaker (a timeout is the expected end of a best-effort request).
    /// Everything else keeps the regular rules. Irreversible: the process is leaving.
    pub fn begin_quit(&mut self) {
        self.quit_mode = true;
        // Hosts that already hold a queued `stopped` are active; nothing else to do.
    }

    /// `stopped` requests currently in flight under the quit rules (for assertions).
    pub fn quit_in_flight(&self) -> usize {
        self.quit_in_flight
    }

    /// Declare the only trackers `torrent` may ever contact (invariant 7).
    /// `completed_sent[i]` seeds the auditor with the persisted flag of slot `i`.
    pub fn register_torrent(
        &mut self,
        torrent: InfoHash20,
        urls: Vec<Arc<TrackerUrl>>,
        completed_sent: &[bool],
        now: Millis,
    ) {
        let mut regs = Vec::with_capacity(urls.len());
        for (i, u) in urls.into_iter().enumerate() {
            let done = completed_sent.get(i).copied().unwrap_or(false);
            // Create the host bucket now: a host's bucket starts EMPTY (see
            // `GateConfig::initial_tokens`) and fills while idle, so a process
            // that restarts in a loop cannot hand the host a fresh burst.
            let host = self.intern_host(u.host(), now);
            let mut aud = Auditor::new(done);
            aud.set_tag(host);
            regs.push(Reg { url: u, aud });
        }
        self.registered.insert(torrent, regs.into_boxed_slice());
    }

    /// Register one more tracker URL of a registered torrent (its slot is the next number).
    pub fn add_url(&mut self, torrent: &InfoHash20, url: Arc<TrackerUrl>, now: Millis) {
        let host = self.intern_host(url.host(), now);
        let mut aud = Auditor::new(false);
        aud.set_tag(host);
        if let Some(regs) = self.registered.get_mut(torrent) {
            let mut v = std::mem::take(regs).into_vec();
            v.push(Reg { url, aud });
            *regs = v.into_boxed_slice();
        }
    }

    /// Forget a torrent (its requests must be finished or cancelled first).
    pub fn unregister_torrent(&mut self, torrent: &InfoHash20) {
        if let Some(regs) = self.registered.remove(torrent) {
            for i in 0..regs.len() {
                self.last_scrape
                    .remove(&(*torrent, i as u16 | SCRAPE_SLOT_BIT));
            }
        }
    }

    /// Crash-loop breaker hook: hold every request except `stopped` until `t`.
    pub fn hold_until(&mut self, t: Millis) {
        self.hold_until = self.hold_until.max(t);
    }

    fn intern_host(&mut self, name: &str, now: Millis) -> HostId {
        if let Some(id) = self.host_ids.get(name) {
            return *id;
        }
        let id = self.hosts.len() as HostId;
        let initial = i64::from(self.cfg.initial_tokens.min(self.cfg.burst))
            .saturating_mul(interval_ms(&self.cfg));
        self.hosts.push(HostState {
            credit_ms: initial,
            last_refill: now,
            in_flight: 0,
            queues: [VecDeque::new(), VecDeque::new(), VecDeque::new()],
            queued_len: 0,
            breaker: Breaker {
                state: BreakerState::Closed,
                level: 0,
                recent: VecDeque::new(),
            },
            tripped: None,
            sent_ring: VecDeque::new(),
            stats: HostStats::default(),
            last_ok: Millis::ZERO,
        });
        let name: Arc<str> = Arc::from(name);
        self.host_names.push(name.clone());
        self.host_ids.insert(name, id);
        id
    }

    /// The auditor of an announce key (the scrape bit, if any, is ignored).
    fn aud_of(
        registered: &mut FastMap<InfoHash20, Box<[Reg]>>,
        key: ReqKey,
    ) -> Option<&mut Auditor> {
        registered
            .get_mut(&key.0)
            .and_then(|r| r.get_mut(usize::from(key.1 & !SCRAPE_SLOT_BIT)))
            .map(|r| &mut r.aud)
    }

    fn aud_mut(&mut self, key: ReqKey) -> Option<&mut Auditor> {
        Self::aud_of(&mut self.registered, key)
    }

    /// Host id for a host name in any spelling (`R.TEST`, `r.test.`,
    /// `[::1]`): the name is canonicalised exactly like [`TrackerUrl::host`].
    fn host_id(&self, name: &str) -> Option<HostId> {
        if let Some(id) = self.host_ids.get(name) {
            return Some(*id);
        }
        let bare = name
            .strip_prefix('[')
            .and_then(|n| n.strip_suffix(']'))
            .unwrap_or(name);
        let canon = canonical_host(bare)?;
        self.host_ids.get(canon.as_str()).copied()
    }

    fn name(&self, id: HostId) -> String {
        self.host_names[id as usize].to_string()
    }

    fn enqueue(
        &mut self,
        key: ReqKey,
        id: RequestId,
        url: Arc<TrackerUrl>,
        payload: Payload,
    ) -> Result<(), SubmitError> {
        let regs = self
            .registered
            .get(&key.0)
            .ok_or(SubmitError::UnknownTorrent)?;
        let host = regs
            .iter()
            .find(|r| Arc::ptr_eq(&r.url, &url) || *r.url == *url)
            .map(Reg::host)
            .ok_or(SubmitError::UrlNotRegistered)?;
        if self.queued.contains_key(&key) || self.in_flight.contains_key(&key) {
            return Err(SubmitError::Busy);
        }
        let deadline = if url.scheme() == Scheme::Udp {
            self.cfg.udp_deadline
        } else {
            self.cfg.http_deadline
        };
        let hs = &mut self.hosts[host as usize];
        if let Some(why) = &hs.tripped {
            return Err(SubmitError::HostTripped(why.clone()));
        }
        self.seq += 1;
        let seq = self.seq;
        let prio = priority(&payload);
        hs.queues[prio].push_back(Queued {
            key,
            id,
            seq,
            url,
            payload,
            deadline,
        });
        hs.queued_len += 1;
        self.queued.insert(key, (host, seq));
        self.active.insert(host);
        Ok(())
    }

    /// Queue an announce. It leaves only through [`poll`](Self::poll).
    pub fn submit(
        &mut self,
        key: ReqKey,
        id: RequestId,
        url: Arc<TrackerUrl>,
        params: AnnounceParams,
        kind: SendKind,
        _now: Millis,
    ) -> Result<(), SubmitError> {
        self.enqueue(key, id, url, Payload::Announce { params, kind })
    }

    /// Queue a scrape (BEP 48). Never called on a timer; at most one per
    /// `scrape_min_gap` per (torrent, tracker), and never while an announce for
    /// the same pair is pending.
    pub fn submit_scrape(
        &mut self,
        key: ReqKey,
        id: RequestId,
        url: Arc<TrackerUrl>,
        hashes: Vec<InfoHash20>,
        now: Millis,
    ) -> Result<(), SubmitError> {
        let announce_key = key;
        let key = (key.0, key.1 | SCRAPE_SLOT_BIT);
        if let Some(last) = self.last_scrape.get(&key).copied() {
            // SPEC 7B.3: no more often than the announce interval, and never
            // below `scrape_min_gap`. Before the first answered announce the
            // interval is unknown: be twice as patient.
            let gap = match self.aud_mut(announce_key).and_then(|a| a.last_interval()) {
                Some(iv) => iv.max(self.cfg.scrape_min_gap),
                None => self.cfg.scrape_min_gap.saturating_mul(2),
            };
            if now.since(last) < gap {
                return Err(SubmitError::ScrapeTooSoon);
            }
        }
        self.enqueue(key, id, url, Payload::Scrape { hashes })?;
        self.last_scrape.insert(key, now);
        Ok(())
    }

    /// Withdraw a request that is still queued. Returns false if it is not
    /// queued (already released, dropped or never submitted).
    pub fn cancel(&mut self, key: ReqKey, id: RequestId) -> bool {
        let Some((host, seq)) = self.queued.get(&key).copied() else {
            return false;
        };
        let hs = &mut self.hosts[host as usize];
        for q in hs.queues.iter_mut() {
            if let Some(pos) = q
                .iter()
                .position(|x| x.key == key && x.seq == seq && x.id == id)
            {
                q.remove(pos);
                hs.queued_len -= 1;
                self.queued.remove(&key);
                if hs.queued_len == 0 {
                    self.active.remove(&host);
                }
                return true;
            }
        }
        false
    }

    fn refill(cfg: &GateConfig, hs: &mut HostState, now: Millis) {
        let cost = interval_ms(cfg);
        let burst_ms = i64::from(cfg.burst).saturating_mul(cost);
        if now < hs.last_refill {
            // The clock went backwards (NTP step, a wrong reading that was
            // corrected). Elapsed time is unknowable, so this is a
            // discontinuity: restart the bucket's clock from `now` (never pin
            // it in the future), forget the sliding-window history (it is on
            // the old time line) and give exactly one token so the host is
            // not starved. The hub normalises time before it reaches the gate,
            // so this only matters to code that drives the gate directly.
            hs.last_refill = now;
            hs.sent_ring.clear();
            hs.credit_ms = hs.credit_ms.max(cost.min(burst_ms));
            return;
        }
        let elapsed = i64::try_from(now.0 - hs.last_refill.0).unwrap_or(i64::MAX);
        hs.credit_ms = hs.credit_ms.saturating_add(elapsed).min(burst_ms);
        hs.last_refill = now;
    }

    /// Release every request that is allowed to leave at `now`.
    pub fn poll(&mut self, now: Millis) -> Vec<GatedRequest> {
        let mut released = Vec::new();
        if self.active.is_empty() {
            return released;
        }
        let hosts: Vec<HostId> = self.active.iter().copied().collect();
        let cost = interval_ms(&self.cfg);
        for host in hosts {
            self.poll_host(host, now, cost, &mut released);
        }
        released
    }

    fn poll_host(
        &mut self,
        host: HostId,
        now: Millis,
        cost: i64,
        released: &mut Vec<GatedRequest>,
    ) {
        loop {
            let hold = self.hold_until;
            let max_in_flight = self.cfg.max_in_flight_per_host;
            let quit_mode = self.quit_mode;
            let quit_room = self.quit_in_flight < self.cfg.quit_max_in_flight;
            let hs = &mut self.hosts[host as usize];
            if hs.tripped.is_some() || hs.queued_len == 0 {
                if hs.queued_len == 0 {
                    self.active.remove(&host);
                    // A start-up burst (thousands of torrents queued at once)
                    // must not leave its peak capacity behind for years.
                    for q in hs.queues.iter_mut() {
                        if q.capacity() > QUEUE_KEEP {
                            *q = VecDeque::new();
                        }
                    }
                    if self.queued.is_empty() && self.queued.capacity() > MAP_KEEP {
                        self.queued = FastMap::default();
                    }
                }
                break;
            }
            Self::refill(&self.cfg, hs, now);
            if let BreakerState::Open { until } = hs.breaker.state {
                if now >= until {
                    hs.breaker.state = BreakerState::HalfOpen;
                } else {
                    break;
                }
            }
            if quit_mode && quit_room && !hs.queues[0].is_empty() {
                // Quit rules: a `stopped` leaves outside the host cap and the bucket.
                let mut picked: Option<Queued> = None;
                while let Some(q) = hs.queues[0].pop_front() {
                    hs.queued_len -= 1;
                    if self.queued.get(&q.key).map(|(_, s)| *s) == Some(q.seq) {
                        picked = Some(q);
                        break;
                    }
                }
                let Some(q) = picked else { continue };
                self.queued.remove(&q.key);
                if let Payload::Announce { params, kind } = &q.payload {
                    let Some(aud) = Self::aud_of(&mut self.registered, q.key) else {
                        continue;
                    };
                    if let Err(why) = aud.on_release(params, *kind, now) {
                        let mut dropped = vec![(q.key, q.id)];
                        self.trip_host_inner(host, why.to_owned(), &mut dropped);
                        return;
                    }
                }
                self.hosts[host as usize].stats.released += 1;
                self.quit_in_flight += 1;
                let ticket = Ticket {
                    key: q.key,
                    id: q.id,
                    seq: q.seq,
                    event: Some(Event::Stopped),
                };
                let deadline_at = now + self.cfg.quit_stop_deadline;
                self.in_flight.insert(
                    q.key,
                    InFlight {
                        host,
                        id: q.id,
                        seq: q.seq,
                        event: Some(Event::Stopped),
                        deadline_at,
                        is_scrape: false,
                        quit: true,
                    },
                );
                self.deadlines.insert((deadline_at, q.key, q.seq));
                released.push(GatedRequest::new(ticket, q.url, q.payload, now));
                continue;
            }
            let limit = if hs.breaker.state == BreakerState::HalfOpen {
                1
            } else {
                max_in_flight
            };
            if hs.in_flight >= limit || hs.credit_ms < cost {
                break;
            }
            // Next valid entry by priority; entries cancelled lazily are skipped.
            let mut picked: Option<Queued> = None;
            'prio: for prio in 0..3 {
                if prio > 0 && now < hold {
                    break 'prio;
                }
                if prio == 0 && quit_mode {
                    // Stops leave only through the quit path above (one global cap).
                    continue;
                }
                while let Some(q) = hs.queues[prio].pop_front() {
                    hs.queued_len -= 1;
                    if self.queued.get(&q.key).map(|(_, s)| *s) == Some(q.seq) {
                        picked = Some(q);
                        break 'prio;
                    }
                }
            }
            let Some(q) = picked else {
                if hs.queued_len == 0 {
                    self.active.remove(&host);
                }
                break;
            };
            self.queued.remove(&q.key);
            // Independent protocol audit before anything leaves.
            if let Payload::Announce { params, kind } = &q.payload {
                let Some(aud) = Self::aud_of(&mut self.registered, q.key) else {
                    // The torrent was unregistered while the request waited:
                    // nothing may leave for it (invariant 7).
                    continue;
                };
                if let Err(why) = aud.on_release(params, *kind, now) {
                    let mut dropped = vec![(q.key, q.id)];
                    self.trip_host_inner(host, why.to_owned(), &mut dropped);
                    return;
                }
            }
            let hs = &mut self.hosts[host as usize];
            hs.credit_ms -= cost;
            hs.in_flight += 1;
            hs.stats.released += 1;
            // Sliding-window cross-check of the bucket arithmetic.
            hs.sent_ring.push_back(now);
            if hs.sent_ring.len() > RING {
                hs.sent_ring.pop_front();
            }
            let n = hs.sent_ring.len();
            let burst = i128::from(self.cfg.burst);
            let mut violated = false;
            for k in 1..n.min(16) {
                let span = i128::from(now.0.saturating_sub(hs.sent_ring[n - 1 - k].0));
                // k+1 sends within `span` ms need span >= (k+1-burst) * interval.
                if span < (k as i128 + 1 - burst) * i128::from(cost) {
                    violated = true;
                    break;
                }
            }
            if violated {
                let mut dropped = vec![(q.key, q.id)];
                self.trip_host_inner(host, "token bucket exceeded".to_owned(), &mut dropped);
                return;
            }
            let (event, is_scrape) = match &q.payload {
                Payload::Announce { params, .. } => (params.event, false),
                Payload::Scrape { .. } => (None, true),
            };
            let ticket = Ticket {
                key: q.key,
                id: q.id,
                seq: q.seq,
                event,
            };
            let deadline_at = now + q.deadline;
            self.in_flight.insert(
                q.key,
                InFlight {
                    host,
                    id: q.id,
                    seq: q.seq,
                    event,
                    deadline_at,
                    is_scrape,
                    quit: false,
                },
            );
            self.deadlines.insert((deadline_at, q.key, q.seq));
            released.push(GatedRequest::new(ticket, q.url, q.payload, now));
        }
    }

    fn trip_host_inner(
        &mut self,
        host: HostId,
        reason: String,
        dropped: &mut Vec<(ReqKey, RequestId)>,
    ) {
        let hs = &mut self.hosts[host as usize];
        for q in hs.queues.iter_mut() {
            for item in q.drain(..) {
                if self.queued.get(&item.key).map(|(_, s)| *s) == Some(item.seq) {
                    self.queued.remove(&item.key);
                    dropped.push((item.key, item.id));
                }
            }
        }
        hs.queued_len = 0;
        hs.tripped = Some(reason.clone());
        self.active.remove(&host);
        let name = self.name(host);
        self.events.push(GateEvent::HostTripped {
            host: name,
            reason,
            dropped: std::mem::take(dropped),
        });
    }

    /// Fail closed on `host`: nothing more is released until `reset_host`.
    pub fn trip_host(&mut self, host: &str, reason: &str) {
        if let Some(id) = self.host_id(host) {
            let mut dropped = Vec::new();
            self.trip_host_inner(id, reason.to_owned(), &mut dropped);
        }
    }

    /// The machine for `key` ended its session (possibly without `stopped`).
    pub fn session_closed(&mut self, key: ReqKey) {
        if let Some(a) = Self::aud_of(&mut self.registered, key) {
            a.session_closed();
        }
    }

    /// Lift a trip (a user action after the error was surfaced).
    pub fn reset_host(&mut self, host: &str) {
        if let Some(id) = self.host_id(host) {
            self.hosts[id as usize].tripped = None;
        }
    }

    /// Hosts currently blocked by a failed invariant check (canonical names),
    /// for a UI that offers `reset_host`.
    pub fn tripped_hosts(&self) -> Vec<String> {
        self.hosts
            .iter()
            .enumerate()
            .filter(|(_, h)| h.tripped.is_some())
            .map(|(i, _)| self.name(i as HostId))
            .collect()
    }

    /// A request has ended. Returns false if the ticket is unknown (already
    /// finished or expired).
    ///
    /// A success for an unknown ticket (a duplicate answer, or a reply that
    /// came after the deadline) proves the host is alive and closes the
    /// breaker, but it is **not** shown to the auditor: whether it changes the
    /// schedule is the machine's decision (it usually does not), and an
    /// auditor that re-anchored on it would disagree with the machine and trip
    /// the whole host. When the machine did apply it, the hub says so with
    /// [`confirm_late_success`](Self::confirm_late_success).
    pub fn finish(&mut self, ticket: Ticket, outcome: Outcome, now: Millis) -> bool {
        let known = self
            .in_flight
            .get(&ticket.key)
            .is_some_and(|f| f.seq == ticket.seq);
        if !known {
            if let Outcome::Success { .. } = outcome {
                // A success after we already counted this request as failed
                // (a second address family answering, or a late reply): the
                // host is alive, so it must not stay penalised by that failure.
                let host = self
                    .registered
                    .get(&ticket.key.0)
                    .and_then(|r| r.get(usize::from(ticket.key.1 & !SCRAPE_SLOT_BIT)))
                    .map(Reg::host);
                if let Some(host) = host {
                    let hs = &mut self.hosts[host as usize];
                    let was_open = !matches!(hs.breaker.state, BreakerState::Closed);
                    hs.breaker.state = BreakerState::Closed;
                    hs.breaker.level = 0;
                    hs.breaker.recent.clear();
                    if was_open {
                        let host = self.name(host);
                        self.events.push(GateEvent::HostResumed { host });
                    }
                }
            }
            return false;
        }
        let f = self.in_flight.remove(&ticket.key).expect("checked");
        if self.in_flight.is_empty() && self.in_flight.capacity() > MAP_KEEP {
            self.in_flight = FastMap::default();
        }
        self.deadlines.remove(&(f.deadline_at, ticket.key, f.seq));
        if !f.is_scrape
            && let Some(a) = Self::aud_of(&mut self.registered, ticket.key) {
                match outcome {
                    Outcome::Success { timing } => a.on_outcome(ticket.event, Some(timing), now),
                    Outcome::HostFailureUnsent | Outcome::Unsent
                        if ticket.event == Some(Event::Completed) =>
                    {
                        a.completed_unsent();
                    }
                    _ => {}
                }
            }
        self.settle(f.host, ticket.key.0, outcome, f.quit, now);
        true
    }

    /// The machine applied a success for a ticket the gate no longer knew (a
    /// reply after the deadline for the newest request: the machine undoes the
    /// timeout and accepts the answer). Brings the auditor in line.
    pub fn confirm_late_success(&mut self, ticket: Ticket, timing: Timing, now: Millis) {
        if let Some(a) = Self::aud_of(&mut self.registered, ticket.key) {
            a.on_outcome(ticket.event, Some(timing), now);
        }
    }

    fn settle(&mut self, host: HostId, torrent: InfoHash20, outcome: Outcome, quit: bool, now: Millis) {
        // A quit-time `stopped` that fails says nothing about the host: the answer was never
        // wanted, so a timeout must not open the breaker and strand the stops still queued.
        let outcome = match outcome {
            Outcome::HostFailure | Outcome::HostFailureUnsent if quit => Outcome::Neutral,
            o => o,
        };
        let cfg_threshold = self.cfg.breaker_threshold;
        let window = self.cfg.breaker_window;
        let (base, cap) = (self.cfg.breaker_base, self.cfg.breaker_cap);
        let jitter_roll = self.rng.next_u64();
        let hs = &mut self.hosts[host as usize];
        if quit {
            self.quit_in_flight = self.quit_in_flight.saturating_sub(1);
        } else {
            hs.in_flight = hs.in_flight.saturating_sub(1);
        }
        let mut opened = None;
        let mut resumed = false;
        match outcome {
            Outcome::Success { .. } | Outcome::HostAlive => {
                hs.last_ok = now;
                if hs.breaker.state == BreakerState::HalfOpen {
                    resumed = true;
                }
                hs.breaker.state = BreakerState::Closed;
                hs.breaker.level = 0;
                hs.breaker.recent.clear();
            }
            Outcome::HostFailure | Outcome::HostFailureUnsent => {
                hs.stats.failures += 1;
                let mut open = false;
                match hs.breaker.state {
                    BreakerState::HalfOpen => open = true,
                    BreakerState::Open { .. } => {}
                    BreakerState::Closed => {
                        while hs
                            .breaker
                            .recent
                            .front()
                            .is_some_and(|(t, _)| now.since(*t) > window)
                        {
                            hs.breaker.recent.pop_front();
                        }
                        hs.breaker.recent.push_back((now, torrent));
                        if hs.breaker.recent.len() > 32 {
                            hs.breaker.recent.pop_front();
                        }
                        let distinct: BTreeSet<&InfoHash20> =
                            hs.breaker.recent.iter().map(|(_, t)| t).collect();
                        open = distinct.len() >= cfg_threshold;
                    }
                }
                if open {
                    let nominal = base
                        .saturating_mul(1u32 << hs.breaker.level.min(10))
                        .min(cap);
                    let jitter = Duration::from_millis(
                        jitter_roll % (nominal.as_millis() as u64 / 5).max(1),
                    );
                    let until = now + nominal + jitter;
                    hs.breaker.state = BreakerState::Open { until };
                    hs.breaker.level += 1;
                    hs.breaker.recent.clear();
                    hs.stats.breaker_opens += 1;
                    opened = Some(until);
                }
            }
            Outcome::Neutral | Outcome::Unsent => {}
        }
        let queued = hs.queued_len > 0;
        if resumed {
            let host = self.name(host);
            self.events.push(GateEvent::HostResumed { host });
        }
        if let Some(until) = opened {
            let host = self.name(host);
            self.events.push(GateEvent::HostPaused { host, until });
        }
        if queued {
            self.active.insert(host);
        }
    }

    /// Declare in-flight requests that outlived their deadline as failed.
    /// Returns their tickets so the driver can feed a timeout to the machines.
    /// A genuine answer arriving later is still delivered to the machine.
    pub fn expire(&mut self, now: Millis) -> Vec<Ticket> {
        let mut out = Vec::new();
        while let Some(&(at, key, seq)) = self.deadlines.iter().next() {
            if at > now {
                break;
            }
            self.deadlines.remove(&(at, key, seq));
            if let Some(f) = self.in_flight.remove(&key) {
                if f.seq == seq {
                    self.settle(f.host, key.0, Outcome::HostFailure, f.quit, now);
                    out.push(Ticket {
                        key,
                        id: f.id,
                        seq,
                        event: f.event,
                    });
                } else {
                    self.in_flight.insert(key, f);
                }
            }
        }
        out
    }

    /// Debit the bucket for extra wire requests made on behalf of a released
    /// one (redirect hops). The bucket may go into debt, which delays later
    /// releases, so the long-run ceiling still holds.
    ///
    /// `host` may be spelled in any form (`R.TEST`, `r.test.`); it is
    /// canonicalised like [`TrackerUrl::host`].
    pub fn charge_extra(&mut self, host: &str, extra: u32) {
        let cost = interval_ms(&self.cfg);
        let floor = i64::from(self.cfg.burst)
            .saturating_mul(cost)
            .saturating_neg();
        if let Some(id) = self.host_id(host) {
            let hs = &mut self.hosts[id as usize];
            hs.credit_ms = hs
                .credit_ms
                .saturating_sub(cost.saturating_mul(i64::from(extra)))
                .max(floor);
        }
    }

    /// Earliest time at which `poll` could release something or `expire` would
    /// fire, if any.
    pub fn next_wake(&self, now: Millis) -> Option<Millis> {
        let cost = interval_ms(&self.cfg);
        let mut best: Option<Millis> = self.deadlines.iter().next().map(|(t, _, _)| *t);
        for &id in &self.active {
            let hs = &self.hosts[id as usize];
            if hs.tripped.is_some() || hs.queued_len == 0 {
                continue;
            }
            if self.quit_mode
                && !hs.queues[0].is_empty()
                && self.quit_in_flight < self.cfg.quit_max_in_flight
                && !matches!(hs.breaker.state, BreakerState::Open { .. })
            {
                best = Some(best.map_or(now, |b| b.min(now)));
                continue;
            }
            if self.quit_mode
                && !hs.queues[0].is_empty()
                && hs.queues[1].is_empty()
                && hs.queues[2].is_empty()
                && !matches!(hs.breaker.state, BreakerState::Open { .. })
            {
                continue; // only stops wait, for the global quit cap: freed by finish()/expire()
            }
            let limit = if hs.breaker.state == BreakerState::HalfOpen {
                1
            } else {
                self.cfg.max_in_flight_per_host
            };
            if hs.in_flight >= limit {
                continue; // freed by finish()/expire(), which has its own wake
            }
            let mut at = now;
            if let BreakerState::Open { until } = hs.breaker.state {
                at = at.max(until);
            }
            let burst_ms = i64::from(self.cfg.burst).saturating_mul(cost);
            let credit = if now < hs.last_refill {
                // clock went back: `refill` will grant one token
                hs.credit_ms.max(cost.min(burst_ms))
            } else {
                let elapsed = i64::try_from(now.0 - hs.last_refill.0).unwrap_or(i64::MAX);
                hs.credit_ms.saturating_add(elapsed).min(burst_ms)
            };
            if credit < cost {
                at = at.max(Millis(
                    now.0
                        .saturating_add(u64::try_from(cost - credit).unwrap_or(0)),
                ));
            }
            // Only stopped entries may leave during a crash-loop hold.
            if now < self.hold_until && hs.queues[0].is_empty() {
                at = at.max(self.hold_until);
            }
            best = Some(best.map_or(at, |b| b.min(at)));
        }
        best
    }

    /// Drain gate events.
    pub fn take_events(&mut self) -> Vec<GateEvent> {
        std::mem::take(&mut self.events)
    }

    /// When `host` last answered, if it ever did.
    pub fn host_last_ok(&self, host: &str) -> Option<Millis> {
        self.host_id(host)
            .map(|id| self.hosts[id as usize].last_ok)
            .filter(|t| *t > Millis::ZERO)
    }

    /// Number of distinct hosts the gate knows (registered torrents' trackers).
    pub fn host_count(&self) -> usize {
        self.hosts.len()
    }

    /// Close every circuit breaker (an outage ended: the failures that opened them were
    /// the network's, not the hosts').
    pub fn reset_breakers(&mut self) {
        for id in 0..self.hosts.len() {
            let hs = &mut self.hosts[id];
            if !matches!(hs.breaker.state, BreakerState::Closed) {
                hs.breaker.state = BreakerState::Closed;
                let host = self.host_names[id].to_string();
                self.events.push(GateEvent::HostResumed { host });
            }
            let hs = &mut self.hosts[id];
            hs.breaker.level = 0;
            hs.breaker.recent.clear();
        }
        // Hosts with queued requests are polled again.
        for id in 0..self.hosts.len() {
            if self.hosts[id].queued_len > 0 {
                self.active.insert(id as HostId);
            }
        }
    }

    /// Per-host counters.
    pub fn host_stats(&self, host: &str) -> Option<HostStats> {
        self.host_id(host).map(|id| self.hosts[id as usize].stats)
    }

    /// Is the breaker of `host` open at `now`?
    pub fn breaker_open(&self, host: &str, now: Millis) -> bool {
        self.host_id(host)
            .is_some_and(|id| matches!(self.hosts[id as usize].breaker.state, BreakerState::Open { until } if now < until))
    }

    /// True when `host` is blocked by a failed invariant check.
    pub fn is_tripped(&self, host: &str) -> bool {
        self.host_id(host)
            .is_some_and(|id| self.hosts[id as usize].tripped.is_some())
    }

    /// In-flight requests on `host` (for assertions).
    pub fn in_flight(&self, host: &str) -> usize {
        self.host_id(host)
            .map_or(0, |id| self.hosts[id as usize].in_flight)
    }

    /// Requests queued or in flight in total.
    pub fn pending(&self) -> usize {
        self.queued.len() + self.in_flight.len()
    }

    /// Is a request for `key` queued (not yet released)?
    pub fn is_queued(&self, key: &ReqKey) -> bool {
        self.queued.contains_key(key)
    }
}
