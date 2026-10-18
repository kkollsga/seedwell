//! The sans-I/O DHT node: routing table, KRPC transactions, lookups, token/peer-store server side.
//!
//! The core never touches a socket or a clock. Feed it [`Input`]s with the current `Instant`
//! and perform the returned [`Intent`]s (send datagrams, deliver events). [`DhtCore::next_wakeup`]
//! says when it next needs a `Tick`; `None` means "fully idle, wake me only for packets/commands".

use crate::codec::{
    self, Body, Envelope, Message, NODE_LEN, Query, QueryOut, Response, ResponseOut,
};
use crate::id::{self, Id, is_local_ip, is_valid_for_ip};
use crate::limits::{IpLimiter, TokenBucket};
use crate::lookup::{Lookup, Purpose, Token};
use crate::rng::Rng;
use crate::store::{PeerStore, StoreLimits};
use crate::table::{Insert, K, RoutingTable};
use crate::token::Tokens;
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};

/// Seconds added to the core clock so a loaded `last_seen = 0` reads as "long ago".
const TIME_BASE_S: u32 = 3600;
/// Minimum spacing between maintenance passes.
const MAINT_SPACING_MS: u64 = 30_000;
/// Distinct /16 reporters that must agree on our external IP.
const IP_VOTE_THRESHOLD: usize = 4;
/// Max peers returned in one `get_peers` response.
const MAX_VALUES: usize = 50;
/// Max cooldown records.
const MAX_COOLDOWNS: usize = 1024;
/// Rounds of sparse-bucket filling after a bootstrap.
const MAX_FILL_ROUNDS: u8 = 3;
/// Concurrent refresh/fill lookups.
const MAX_REFRESH_LOOKUPS: usize = 3;
/// Max persisted nodes.
pub const MAX_SAVED_NODES: usize = 200;
const STATE_VERSION: u8 = 1;

/// Tunables. Defaults are conservative (see README for rationale).
#[derive(Clone, Debug)]
pub struct Config {
    /// BEP 43: never respond, tell others not to add us. Also refuses announces.
    pub read_only: bool,
    /// Our `v` string (BEP 20 style, e.g. `b"TD\x00\x01"`).
    pub version: Option<[u8; 4]>,
    /// Seed for the deterministic RNG; `None` uses OS entropy.
    pub seed: Option<u64>,
    /// Use this node id instead of a random one (also used by simulations).
    pub initial_node_id: Option<Id>,
    /// Generate a BEP 42 id once the external IP is known.
    pub secure_node_id: bool,
    /// Ignore nodes whose id does not match their IP (BEP 42 enforcement). Off by default
    /// because a large part of the network does not comply.
    pub enforce_node_id: bool,
    /// Keep at most one routing-table node per IP address (Sybil/eclipse damping).
    pub unique_ip_per_table: bool,
    /// Accept private/loopback/CGNAT addresses as contacts (tests and LAN-only use).
    pub allow_local_addrs: bool,
    /// Per-query timeout.
    pub query_timeout: Duration,
    /// Retransmissions of a lookup/verify query before it counts as failed (clamped to 3).
    pub max_retries: u8,
    /// Lookup parallelism.
    pub alpha: u8,
    /// Outgoing query token bucket: burst.
    pub out_burst: u32,
    /// Outgoing query token bucket: sustained queries per second.
    pub out_rate: u32,
    /// Responses per remote IP: burst.
    pub resp_ip_burst: u32,
    /// Responses per remote IP: sustained per second.
    pub resp_ip_rate: u32,
    /// Responses overall: burst.
    pub resp_global_burst: u32,
    /// Responses overall: sustained per second.
    pub resp_global_rate: u32,
    /// Announce-store caps and TTL.
    pub store: StoreLimits,
    /// Minimum time between `get_peers` lookups for one info hash.
    pub get_peers_cooldown: Duration,
    /// Minimum time between announces for one info hash.
    pub announce_cooldown: Duration,
    /// Concurrent public-torrent lookups; extra requests wait in a queue.
    pub max_concurrent_lookups: usize,
    /// Queue length for waiting lookups.
    pub max_queue: usize,
    /// Per-lookup query cap.
    pub max_queries_per_lookup: u16,
    /// Per-lookup time cap.
    pub lookup_timeout: Duration,
    /// Per-lookup peer cap (dedupe set size).
    pub max_peers_per_lookup: usize,
    /// A bucket unchanged this long is refreshed (BEP 5: 15 minutes).
    pub refresh_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            read_only: false,
            version: Some(*b"TD\x00\x01"),
            seed: None,
            initial_node_id: None,
            secure_node_id: true,
            enforce_node_id: false,
            unique_ip_per_table: true,
            allow_local_addrs: false,
            query_timeout: Duration::from_secs(2),
            max_retries: 1,
            alpha: 3,
            out_burst: 50,
            out_rate: 10,
            resp_ip_burst: 10,
            resp_ip_rate: 5,
            resp_global_burst: 100,
            resp_global_rate: 100,
            store: StoreLimits::default(),
            get_peers_cooldown: Duration::from_secs(60),
            announce_cooldown: Duration::from_secs(10 * 60),
            max_concurrent_lookups: 4,
            max_queue: 64,
            max_queries_per_lookup: 128,
            lookup_timeout: Duration::from_secs(45),
            max_peers_per_lookup: 200,
            refresh_interval: Duration::from_secs(15 * 60),
        }
    }
}

/// An info hash that is allowed on the DHT. The only constructor takes the torrent's `private`
/// flag, so the engine cannot hand a private torrent's hash to the DHT without explicitly
/// writing `private = false` (SPEC 7B.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublicInfoHash(Id);

impl PublicInfoHash {
    /// `None` for private torrents (BEP 27): their hash must never reach the DHT.
    pub fn new(info_hash: Id, private: bool) -> Option<Self> {
        if private { None } else { Some(Self(info_hash)) }
    }

    /// The info hash.
    pub fn info_hash(&self) -> Id {
        self.0
    }
}

/// Everything the core can be asked or told.
#[derive(Debug)]
pub enum Input<'a> {
    /// A datagram arrived.
    Packet {
        /// Source address.
        from: SocketAddrV4,
        /// Payload.
        data: &'a [u8],
    },
    /// Time passed (call at or after [`DhtCore::next_wakeup`]).
    Tick,
    /// Find peers for a torrent.
    GetPeers(PublicInfoHash),
    /// Find peers, then announce ourselves to the closest nodes.
    Announce {
        /// Torrent.
        info_hash: PublicInfoHash,
        /// Our BitTorrent listening port.
        port: u16,
    },
    /// Resolved bootstrap addresses (answer to [`Event::NeedBootstrap`]).
    Bootstrap(&'a [SocketAddrV4]),
}

/// What the driver must do.
#[derive(Debug, PartialEq, Eq)]
pub enum Intent {
    /// Send a UDP datagram.
    Send {
        /// Destination.
        to: SocketAddrV4,
        /// Payload.
        data: Vec<u8>,
    },
    /// Notify the application.
    Event(Event),
}

/// Why a command was not accepted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// Asked again too soon.
    Cooldown {
        /// Time left until the request would be accepted.
        retry_after: Duration,
    },
    /// A lookup for this hash is already running or queued; its results will arrive as events.
    InProgress,
    /// Queue or bookkeeping is full; try later.
    Busy,
    /// The hash was marked forbidden with [`DhtCore::forbid`].
    Forbidden,
    /// Read-only nodes do not announce.
    ReadOnly,
    /// Port 0 cannot be announced.
    BadPort,
}

/// Summary of a finished lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LookupStats {
    /// Queries sent.
    pub queries: u16,
    /// Replies received.
    pub replied: u16,
    /// Distinct peers found.
    pub peers: u32,
    /// Wall (virtual) time taken, milliseconds.
    pub elapsed_ms: u64,
}

/// Application-facing events.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// New peers for a torrent.
    Peers {
        /// Torrent.
        info_hash: Id,
        /// Previously unreported peers.
        peers: Vec<SocketAddrV4>,
    },
    /// A get_peers lookup ended.
    LookupFinished {
        /// Torrent.
        info_hash: Id,
        /// Stats.
        stats: LookupStats,
    },
    /// An announce round ended.
    Announced {
        /// Torrent.
        info_hash: Id,
        /// Nodes that acknowledged.
        accepted: u8,
        /// Nodes we tried.
        attempted: u8,
    },
    /// A command was refused.
    Refused {
        /// Torrent.
        info_hash: Id,
        /// Reason.
        reason: Refused,
    },
    /// The table is empty: resolve bootstrap hosts and answer with [`Input::Bootstrap`].
    NeedBootstrap,
    /// The external IPv4 address was learned or changed (BEP 42 voting).
    ExternalIp(Ipv4Addr),
    /// Our node id changed (to a BEP 42 id for the new external IP).
    NodeIdChanged {
        /// Previous id.
        old: Id,
        /// New id.
        new: Id,
    },
}

/// Counters for observability and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Queries sent (first transmissions).
    pub queries_sent: u64,
    /// Retransmissions.
    pub retransmits: u64,
    /// Responses matched to a query.
    pub responses_received: u64,
    /// Queries that timed out for good.
    pub timeouts: u64,
    /// Responses with an unexpected sender id.
    pub id_mismatch: u64,
    /// Responses with no matching transaction.
    pub unmatched: u64,
    /// Queries received.
    pub queries_received: u64,
    /// Responses and errors sent.
    pub responses_sent: u64,
    /// Datagrams dropped by the response rate limiters.
    pub rate_limited: u64,
    /// Datagrams that failed to parse.
    pub malformed: u64,
    /// Outgoing queries refused by the global query budget.
    pub budget_denied: u64,
    /// Announces stored on behalf of other nodes.
    pub announces_stored: u64,
    /// Announces rejected (bad token, caps).
    pub announces_rejected: u64,
}

/// Reasons a persisted state blob was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateError {
    /// Wrong length or unknown version.
    Invalid,
}

#[derive(Clone, Copy, Debug)]
enum Owner {
    Lookup(u32),
    Verify(Id),
    Probe,
}

#[derive(Clone, Copy, Debug)]
enum Q {
    Ping,
    FindNode(Id),
    GetPeers(Id),
    Announce(Id, u16, Token),
}

// size: ~112 bytes
#[derive(Clone, Copy, Debug)]
struct Pending {
    tid: u16,
    to: SocketAddrV4,
    expect: Option<Id>,
    deadline_ms: u64,
    tries: u8,
    owner: Owner,
    q: Q,
}

struct Queued {
    info_hash: Id,
    announce_port: Option<u16>,
}

#[derive(Clone, Copy, Default)]
struct Cooldown {
    last_get: Option<u64>,
    last_announce: Option<u64>,
}

/// The DHT node state machine.
pub struct DhtCore {
    cfg: Config,
    epoch: Instant,
    now_ms: u64,
    rng: Rng,
    id: Id,
    table: RoutingTable,
    tokens: Tokens,
    store: PeerStore,
    out_budget: TokenBucket,
    resp_global: TokenBucket,
    resp_ip: IpLimiter,
    pending: Vec<Pending>,
    lookups: Vec<Lookup>,
    queue: VecDeque<Queued>,
    next_lookup_id: u32,
    cooldowns: HashMap<Id, Cooldown>,
    forbidden: HashSet<Id>,
    ext_ip: Option<Ipv4Addr>,
    ip_votes: Vec<(u16, Ipv4Addr)>,
    next_bootstrap_ms: u64,
    bootstrap_backoff_ms: u64,
    wait_table_until_ms: u64,
    last_maint_ms: u64,
    last_sweep_ms: u64,
    budget_blocked: bool,
    fill_targets: VecDeque<Id>,
    fill_rounds_left: u8,
    out: Vec<Intent>,
    stats: Stats,
}

impl DhtCore {
    /// New node; `now` becomes the core's time origin.
    pub fn new(mut cfg: Config, now: Instant) -> Self {
        cfg.max_retries = cfg.max_retries.min(3);
        cfg.alpha = cfg.alpha.clamp(1, 8);
        let mut rng = match cfg.seed {
            Some(s) => Rng::from_seed(s),
            None => Rng::from_entropy(),
        };
        let id = cfg.initial_node_id.unwrap_or_else(|| rng.id());
        let tokens = Tokens::new(0, &mut rng);
        let salt = rng.next_u64() as u32;
        let mut table = RoutingTable::new(id, TIME_BASE_S);
        table.set_unique_ip(cfg.unique_ip_per_table);
        Self {
            table,
            tokens,
            store: PeerStore::new(cfg.store),
            out_budget: TokenBucket::new(cfg.out_burst, cfg.out_rate, 0),
            resp_global: TokenBucket::new(cfg.resp_global_burst, cfg.resp_global_rate, 0),
            resp_ip: IpLimiter::new(cfg.resp_ip_burst, cfg.resp_ip_rate, salt),
            cfg,
            epoch: now,
            now_ms: 0,
            rng,
            id,
            pending: Vec::new(),
            lookups: Vec::new(),
            queue: VecDeque::new(),
            next_lookup_id: 1,
            cooldowns: HashMap::new(),
            forbidden: HashSet::new(),
            ext_ip: None,
            ip_votes: Vec::new(),
            next_bootstrap_ms: 0,
            bootstrap_backoff_ms: 15_000,
            wait_table_until_ms: 30_000,
            last_maint_ms: 0,
            last_sweep_ms: 0,
            budget_blocked: false,
            fill_targets: VecDeque::new(),
            fill_rounds_left: 0,
            out: Vec::new(),
            stats: Stats::default(),
        }
    }

    // ------------------------------------------------------------ accessors

    /// Our current node id.
    pub fn node_id(&self) -> Id {
        self.id
    }

    /// Learned external IPv4 address, if any.
    pub fn external_ip(&self) -> Option<Ipv4Addr> {
        self.ext_ip
    }

    /// Nodes in the routing table.
    pub fn table_len(&self) -> usize {
        self.table.len()
    }

    /// Nodes in the routing table that are currently good.
    pub fn good_nodes(&self) -> usize {
        self.table.good_count(self.secs())
    }

    /// Counters.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Peers currently stored for other nodes.
    pub fn stored_peers(&self) -> usize {
        self.store.total_peers()
    }

    /// Active lookups (all purposes) plus queued ones.
    pub fn active_lookups(&self) -> usize {
        self.lookups.len() + self.queue.len()
    }

    /// Outstanding queries.
    pub fn pending_queries(&self) -> usize {
        self.pending.len()
    }

    /// Approximate resident bytes of this node (table, stores, in-flight state, limiters).
    pub fn memory_bytes(&self) -> usize {
        size_of::<Self>()
            + self.table.heap_bytes()
            + self.store.heap_bytes()
            + IpLimiter::size_bytes()
            + self.pending.capacity() * size_of::<Pending>()
            + self.lookups.capacity() * size_of::<Lookup>()
            + self
                .lookups
                .iter()
                .map(|l| {
                    l.cands.capacity() * size_of::<crate::lookup::Cand>()
                        + l.peers_seen.capacity() * (size_of::<SocketAddrV4>() + 1)
                })
                .sum::<usize>()
            + self.cooldowns.capacity() * (size_of::<Id>() + size_of::<Cooldown>() + 1)
            + self.queue.capacity() * size_of::<Queued>()
            + self.forbidden.capacity() * (size_of::<Id>() + 1)
            + self.ip_votes.capacity() * size_of::<(u16, Ipv4Addr)>()
    }

    /// Switches BEP 43 read-only mode at runtime. Read-only nodes stop answering queries and tag
    /// their own queries `ro=1`, so other nodes stop adding them to routing tables and inbound
    /// traffic (and wake-ups) fade out within hours. Use it while no torrent needs the DHT.
    pub fn set_read_only(&mut self, on: bool) {
        self.cfg.read_only = on;
    }

    /// Never serve or announce this hash (belt and braces on top of [`PublicInfoHash`]).
    pub fn forbid(&mut self, info_hash: Id) {
        self.forbidden.insert(info_hash);
    }

    /// Undo [`forbid`](Self::forbid).
    pub fn unforbid(&mut self, info_hash: &Id) {
        self.forbidden.remove(info_hash);
    }

    fn secs(&self) -> u32 {
        TIME_BASE_S.saturating_add((self.now_ms / 1000) as u32)
    }

    fn ms(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_millis() as u64
    }

    fn at(&self, ms: u64) -> Instant {
        self.epoch + Duration::from_millis(ms)
    }

    /// When the core next needs a [`Input::Tick`]; `None` when it is fully idle.
    pub fn next_wakeup(&mut self) -> Option<Instant> {
        let mut t = u64::MAX;
        for p in &self.pending {
            t = t.min(p.deadline_ms);
        }
        if self.budget_blocked {
            t = t.min(self.now_ms + self.out_budget.wait_ms(self.now_ms, 1).max(1));
        }
        if !self.queue.is_empty() && self.table.usable_count() == 0 {
            t = t.min(self.wait_table_until_ms.max(self.now_ms + 1));
        }
        if let Some(m) = self.next_maintenance_ms() {
            t = t.min(m.max(self.last_maint_ms + MAINT_SPACING_MS));
        }
        if t == u64::MAX {
            None
        } else {
            Some(self.at(t))
        }
    }

    fn needs_bootstrap(&self) -> bool {
        self.table.usable_count() < 2
            && !self.lookups.iter().any(|l| l.purpose == Purpose::Bootstrap)
    }

    fn next_maintenance_ms(&self) -> Option<u64> {
        let mut t = u64::MAX;
        if self.needs_bootstrap() {
            t = t.min(self.next_bootstrap_ms);
        }
        let interval = self.cfg.refresh_interval.as_secs() as u32;
        if let Some(s) = self.table.next_stale_at(interval) {
            t = t.min(u64::from(s.saturating_sub(TIME_BASE_S)) * 1000);
        }
        if t == u64::MAX { None } else { Some(t) }
    }

    // ------------------------------------------------------------ main entry

    /// The network came back after an outage (SPEC 7C.4c): the bootstrap backoff, which doubles
    /// up to an hour while nothing answers, starts over, and a table that has emptied
    /// bootstraps at the next tick (once). Nodes are not blamed for queries that could not go out.
    pub fn network_restored(&mut self, now: Instant) -> Vec<Intent> {
        self.now_ms = self.now_ms.max(self.ms(now));
        self.bootstrap_backoff_ms = 15_000;
        self.next_bootstrap_ms = self.now_ms;
        self.last_maint_ms = 0;
        // Queries that were in flight when the network went away are not the nodes' failures.
        self.pending.clear();
        self.maintenance();
        self.drive();
        std::mem::take(&mut self.out)
    }

    /// Advances the machine. All effects are returned; nothing is performed.
    pub fn handle(&mut self, input: Input<'_>, now: Instant) -> Vec<Intent> {
        self.now_ms = self.now_ms.max(self.ms(now));
        self.tokens.maybe_rotate(self.now_ms, &mut self.rng);
        if self.now_ms.saturating_sub(self.last_sweep_ms) >= 5 * 60_000 {
            self.last_sweep_ms = self.now_ms;
            self.store.sweep(self.secs());
        }
        match input {
            Input::Packet { from, data } => self.on_packet(from, data),
            Input::Tick => {}
            Input::GetPeers(h) => self.cmd_get_peers(h.0),
            Input::Announce { info_hash, port } => self.cmd_announce(info_hash.0, port),
            Input::Bootstrap(addrs) => self.cmd_bootstrap(addrs),
        }
        self.expire_pending();
        self.maintenance();
        self.drive();
        std::mem::take(&mut self.out)
    }

    // ------------------------------------------------------------ transport helpers

    fn valid_remote(&self, a: &SocketAddrV4) -> bool {
        let ip = *a.ip();
        a.port() != 0
            && !ip.is_unspecified()
            && !ip.is_multicast()
            && !ip.is_broadcast()
            && (self.cfg.allow_local_addrs || !is_local_ip(ip))
    }

    fn envelope<'a>(
        &'a self,
        tid: &'a [u8],
        ip: Option<SocketAddrV4>,
        query: bool,
    ) -> Envelope<'a> {
        Envelope {
            tid,
            version: self.cfg.version.as_ref().map(|v| &v[..]),
            read_only: query && self.cfg.read_only,
            ip,
        }
    }

    fn take_budget(&mut self, low_priority: bool) -> bool {
        // Background work (probes, refreshes) must leave half the burst for lookups the user asked for.
        let ok = !(low_priority && self.out_budget.available(self.now_ms) < self.cfg.out_burst / 2)
            && self.out_budget.try_take(self.now_ms, 1);
        if !ok {
            self.stats.budget_denied += 1;
        }
        ok
    }

    fn alloc_tid(&mut self) -> u16 {
        for _ in 0..32 {
            let t = self.rng.next_u64() as u16;
            if !self.pending.iter().any(|p| p.tid == t) {
                return t;
            }
        }
        // Pathological: nearly all ids busy. Linear scan always terminates because pending is
        // bounded far below 65536 by the query budget.
        (0..=u16::MAX)
            .find(|t| !self.pending.iter().any(|p| p.tid == *t))
            .unwrap_or(0)
    }

    fn encode_q(&self, tid: u16, q: &Q) -> Vec<u8> {
        let tid = tid.to_be_bytes();
        let env = self.envelope(&tid, None, true);
        let mut out = Vec::with_capacity(128);
        let me = &self.id;
        match q {
            Q::Ping => codec::encode_query(&mut out, &env, &QueryOut::Ping { id: me }),
            Q::FindNode(t) => {
                codec::encode_query(&mut out, &env, &QueryOut::FindNode { id: me, target: t })
            }
            Q::GetPeers(h) => codec::encode_query(
                &mut out,
                &env,
                &QueryOut::GetPeers {
                    id: me,
                    info_hash: h,
                },
            ),
            Q::Announce(h, port, tok) => codec::encode_query(
                &mut out,
                &env,
                &QueryOut::AnnouncePeer {
                    id: me,
                    info_hash: h,
                    port: *port,
                    token: tok.as_slice(),
                },
            ),
        }
        out
    }

    /// Sends a query (budget must already be taken) and registers the transaction.
    fn send_query(&mut self, to: SocketAddrV4, expect: Option<Id>, owner: Owner, q: Q) {
        let tid = self.alloc_tid();
        let data = self.encode_q(tid, &q);
        self.out.push(Intent::Send { to, data });
        self.pending.push(Pending {
            tid,
            to,
            expect,
            deadline_ms: self.now_ms + self.cfg.query_timeout.as_millis() as u64,
            tries: 0,
            owner,
            q,
        });
        self.stats.queries_sent += 1;
    }

    fn respond(
        &mut self,
        to: SocketAddrV4,
        tid: &[u8],
        token: Option<&[u8]>,
        nodes: &[u8],
        values: &[SocketAddrV4],
    ) {
        let mut out = Vec::with_capacity(256);
        let env = self.envelope(tid, Some(to), false);
        codec::encode_response(
            &mut out,
            &env,
            &ResponseOut {
                id: &self.id,
                token,
                nodes,
                values,
            },
        );
        self.out.push(Intent::Send { to, data: out });
        self.stats.responses_sent += 1;
    }

    fn respond_error(&mut self, to: SocketAddrV4, tid: &[u8], code: i64, msg: &str) {
        let mut out = Vec::with_capacity(64);
        let env = self.envelope(tid, None, false);
        codec::encode_error(&mut out, &env, code, msg);
        self.out.push(Intent::Send { to, data: out });
        self.stats.responses_sent += 1;
    }

    fn compact_closest(&self, target: &Id) -> Vec<u8> {
        let mut v = Vec::with_capacity(K * NODE_LEN);
        for n in self.table.closest(target, K, self.secs()) {
            codec::encode_node(&n.id, &n.addr, &mut v);
        }
        v
    }

    // ------------------------------------------------------------ incoming packets

    fn on_packet(&mut self, from: SocketAddrV4, data: &[u8]) {
        if data.len() > 2048 || !self.valid_remote(&from) {
            return;
        }
        match codec::parse(data) {
            Err(rej) => {
                self.stats.malformed += 1;
                if rej.was_query && !self.cfg.read_only {
                    if let Some(tid) = rej.tid {
                        if self.allow_response(&from) {
                            self.respond_error(from, tid, 203, "Protocol Error");
                        }
                    }
                }
            }
            Ok(msg) => match msg.body {
                Body::Query(q) => self.on_query(from, &msg, &q),
                Body::Response(r) => self.on_response(from, &msg, &r),
                Body::Error(_) => self.on_error(from, &msg),
            },
        }
    }

    fn allow_response(&mut self, from: &SocketAddrV4) -> bool {
        let ok = self.resp_ip.allow(u32::from(*from.ip()), self.now_ms)
            && self.resp_global.try_take(self.now_ms, 1);
        if !ok {
            self.stats.rate_limited += 1;
        }
        ok
    }

    fn on_query(&mut self, from: SocketAddrV4, msg: &Message<'_>, q: &Query<'_>) {
        self.stats.queries_received += 1;
        if self.cfg.read_only || !self.allow_response(&from) {
            return;
        }
        let tid = msg.tid;
        match *q {
            Query::Ping { id } => {
                self.learn(id, from, msg.read_only);
                self.respond(from, tid, None, &[], &[]);
            }
            Query::FindNode { id, target } => {
                self.learn(id, from, msg.read_only);
                let nodes = self.compact_closest(&target);
                self.respond(from, tid, None, &nodes, &[]);
            }
            Query::GetPeers { id, info_hash } => {
                self.learn(id, from, msg.read_only);
                let token = self.tokens.issue(*from.ip());
                let values = self
                    .store
                    .get(&info_hash, self.secs(), MAX_VALUES, &mut self.rng);
                let nodes = self.compact_closest(&info_hash);
                self.respond(from, tid, Some(&token[..]), &nodes, &values);
            }
            Query::AnnouncePeer {
                id,
                info_hash,
                port,
                token,
                implied_port,
            } => {
                self.learn(id, from, msg.read_only);
                if !self.tokens.validate(*from.ip(), token) {
                    self.stats.announces_rejected += 1;
                    self.respond_error(from, tid, 203, "Bad Token");
                    return;
                }
                let port = if implied_port { from.port() } else { port };
                if port == 0 || self.forbidden.contains(&info_hash) {
                    self.stats.announces_rejected += 1;
                    self.respond_error(from, tid, 203, "Protocol Error");
                    return;
                }
                if self
                    .store
                    .add(info_hash, SocketAddrV4::new(*from.ip(), port), self.secs())
                {
                    self.stats.announces_stored += 1;
                    self.respond(from, tid, None, &[], &[]);
                } else {
                    self.stats.announces_rejected += 1;
                    self.respond_error(from, tid, 202, "Server Error");
                }
            }
            Query::Unknown { .. } => self.respond_error(from, tid, 204, "Method Unknown"),
        }
    }

    /// A node queried us: refresh it if known, otherwise verify it with a ping before insertion.
    fn learn(&mut self, id: Id, from: SocketAddrV4, read_only: bool) {
        if read_only
            || id == self.id
            || (self.cfg.enforce_node_id && !is_valid_for_ip(&id, *from.ip()))
        {
            return;
        }
        let secs = self.secs();
        if self.table.touch_from(&id, &from, secs) {
            return;
        }
        if self.table.would_accept(&id, &from, secs)
            && !self
                .pending
                .iter()
                .any(|p| p.to == from && matches!(p.owner, Owner::Probe))
            && self.take_budget(true)
        {
            self.send_query(from, Some(id), Owner::Probe, Q::Ping);
        }
    }

    fn take_pending(&mut self, from: &SocketAddrV4, tid: &[u8]) -> Option<Pending> {
        let tid: [u8; 2] = tid.try_into().ok()?;
        let tid = u16::from_be_bytes(tid);
        let i = self
            .pending
            .iter()
            .position(|p| p.tid == tid && p.to == *from)?;
        Some(self.pending.swap_remove(i))
    }

    fn on_error(&mut self, from: SocketAddrV4, msg: &Message<'_>) {
        let Some(p) = self.take_pending(&from, msg.tid) else {
            self.stats.unmatched += 1;
            return;
        };
        // The node is alive but declined; don't count it against the table.
        self.fail_pending(p, false);
    }

    fn on_response(&mut self, from: SocketAddrV4, msg: &Message<'_>, r: &Response<'_>) {
        let Some(p) = self.take_pending(&from, msg.tid) else {
            self.stats.unmatched += 1;
            return;
        };
        if p.expect.is_some_and(|e| e != r.id) {
            self.stats.id_mismatch += 1;
            self.fail_pending(p, true);
            return;
        }
        self.stats.responses_received += 1;
        if let Some(ip) = msg.ip {
            self.vote_ip(*from.ip(), *ip.ip());
        }
        self.node_replied(r.id, from);
        match p.owner {
            Owner::Lookup(lid) => self.lookup_reply(lid, &p, from, r),
            Owner::Verify(_) | Owner::Probe => {}
        }
    }

    fn node_replied(&mut self, id: Id, addr: SocketAddrV4) {
        if id == self.id || (self.cfg.enforce_node_id && !is_valid_for_ip(&id, *addr.ip())) {
            return;
        }
        if let Insert::Verify(stale) = self.table.insert(id, addr, self.secs()) {
            if !self
                .pending
                .iter()
                .any(|p| matches!(p.owner, Owner::Verify(v) if v == stale.id))
                && self.take_budget(true)
            {
                self.send_query(stale.addr, Some(stale.id), Owner::Verify(stale.id), Q::Ping);
            }
        }
    }

    fn lookup_reply(&mut self, lid: u32, p: &Pending, from: SocketAddrV4, r: &Response<'_>) {
        let own = self.id;
        let Some(li) = self.lookups.iter().position(|l| l.id == lid) else {
            return;
        };
        let allow_local = self.cfg.allow_local_addrs;
        let enforce = self.cfg.enforce_node_id;
        let lk = &mut self.lookups[li];
        if let Q::Announce(..) = p.q {
            lk.announce_outstanding = lk.announce_outstanding.saturating_sub(1);
            lk.announce_accepted += 1;
            return;
        }
        let token = if matches!(p.q, Q::GetPeers(_)) {
            r.token.and_then(Token::new)
        } else {
            None
        };
        if !lk.on_reply(&from, r.id, token) {
            return;
        }
        for (nid, naddr) in r.nodes().take(32) {
            let ip = *naddr.ip();
            let ok = naddr.port() != 0
                && !ip.is_unspecified()
                && !ip.is_multicast()
                && !ip.is_broadcast()
                && (allow_local || !is_local_ip(ip))
                && nid != own
                && (!enforce || is_valid_for_ip(&nid, ip));
            if ok {
                lk.add_candidate(nid, naddr, true);
            }
        }
        if matches!(lk.purpose, Purpose::Peers { .. }) {
            let found = lk.new_peers(
                r.values()
                    .take(MAX_VALUES * 2)
                    .filter(|a| a.port() != 0 && !a.ip().is_unspecified()),
            );
            if !found.is_empty() {
                let info_hash = lk.target;
                self.out.push(Intent::Event(Event::Peers {
                    info_hash,
                    peers: found,
                }));
            }
        }
    }

    // ------------------------------------------------------------ timeouts

    /// Retransmit only to nodes we have already talked to: a contact that merely appeared in
    /// somebody's `nodes` list is dead about half the time, and a retry just doubles the wait.
    fn retries_for(&self, p: &Pending) -> u8 {
        match (p.owner, p.q) {
            (_, Q::Announce(..)) | (Owner::Probe, _) => 0,
            (Owner::Lookup(_), _) if !p.expect.is_some_and(|e| self.table.contains(&e)) => 0,
            _ => self.cfg.max_retries,
        }
    }

    fn expire_pending(&mut self) {
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i].deadline_ms > self.now_ms {
                i += 1;
                continue;
            }
            let p = self.pending[i];
            if p.tries < self.retries_for(&p) && self.take_budget(false) {
                let data = self.encode_q(p.tid, &p.q);
                self.out.push(Intent::Send { to: p.to, data });
                let pm = &mut self.pending[i];
                pm.tries += 1;
                pm.deadline_ms = self.now_ms + self.cfg.query_timeout.as_millis() as u64;
                self.stats.retransmits += 1;
                i += 1;
            } else {
                self.pending.swap_remove(i);
                self.stats.timeouts += 1;
                self.fail_pending(p, true);
            }
        }
    }

    /// A query ended without a usable answer. `silent` = the peer did not answer at all.
    fn fail_pending(&mut self, p: Pending, silent: bool) {
        match p.owner {
            Owner::Lookup(lid) => {
                if let Some(lk) = self.lookups.iter_mut().find(|l| l.id == lid) {
                    if let Q::Announce(..) = p.q {
                        lk.announce_outstanding = lk.announce_outstanding.saturating_sub(1);
                    } else {
                        lk.on_failure(&p.to);
                    }
                }
                if silent {
                    if let Some(e) = p.expect {
                        self.table.fail(&e);
                    }
                }
            }
            Owner::Verify(id) => {
                if silent {
                    self.table.mark_dead(&id);
                }
            }
            Owner::Probe => {}
        }
    }

    // ------------------------------------------------------------ external IP / node id

    fn vote_ip(&mut self, voter: Ipv4Addr, reported: Ipv4Addr) {
        if is_local_ip(reported) && !self.cfg.allow_local_addrs {
            return;
        }
        let key = u16::from_be_bytes([voter.octets()[0], voter.octets()[1]]);
        if let Some(v) = self.ip_votes.iter_mut().find(|v| v.0 == key) {
            v.1 = reported;
        } else {
            if self.ip_votes.len() >= 32 {
                self.ip_votes.remove(0);
            }
            self.ip_votes.push((key, reported));
        }
        let agree = self.ip_votes.iter().filter(|v| v.1 == reported).count();
        if agree >= IP_VOTE_THRESHOLD && self.ext_ip != Some(reported) {
            self.ext_ip = Some(reported);
            self.out.push(Intent::Event(Event::ExternalIp(reported)));
            if self.cfg.secure_node_id && !is_valid_for_ip(&self.id, reported) {
                let old = self.id;
                let new = id::secure_id(reported, &mut self.rng);
                self.id = new;
                self.table.rekey(new, self.secs());
                self.out
                    .push(Intent::Event(Event::NodeIdChanged { old, new }));
            }
        }
    }

    // ------------------------------------------------------------ commands

    fn refuse(&mut self, info_hash: Id, reason: Refused) {
        self.out
            .push(Intent::Event(Event::Refused { info_hash, reason }));
    }

    fn cooldown_left(&self, last: Option<u64>, window: Duration) -> Option<Duration> {
        let last = last?;
        let end = last + window.as_millis() as u64;
        (end > self.now_ms).then(|| Duration::from_millis(end - self.now_ms))
    }

    fn cooldown_entry(&mut self, ih: Id) -> Result<&mut Cooldown, Refused> {
        if !self.cooldowns.contains_key(&ih) && self.cooldowns.len() >= MAX_COOLDOWNS {
            let horizon = self
                .cfg
                .get_peers_cooldown
                .max(self.cfg.announce_cooldown)
                .as_millis() as u64;
            let now = self.now_ms;
            self.cooldowns.retain(|_, c| {
                [c.last_get, c.last_announce]
                    .into_iter()
                    .flatten()
                    .any(|t| t + horizon > now)
            });
            if self.cooldowns.len() >= MAX_COOLDOWNS {
                return Err(Refused::Busy);
            }
        }
        Ok(self.cooldowns.entry(ih).or_default())
    }

    fn lookup_in_progress(&self, ih: &Id) -> bool {
        self.queue.iter().any(|q| q.info_hash == *ih)
            || self
                .lookups
                .iter()
                .any(|l| l.target == *ih && matches!(l.purpose, Purpose::Peers { .. }))
    }

    fn cmd_get_peers(&mut self, ih: Id) {
        if self.forbidden.contains(&ih) {
            return self.refuse(ih, Refused::Forbidden);
        }
        if self.lookup_in_progress(&ih) {
            return self.refuse(ih, Refused::InProgress);
        }
        let cd = self.cooldowns.get(&ih).copied().unwrap_or_default();
        if let Some(retry_after) = self.cooldown_left(cd.last_get, self.cfg.get_peers_cooldown) {
            return self.refuse(ih, Refused::Cooldown { retry_after });
        }
        if self.queue.len() >= self.cfg.max_queue {
            return self.refuse(ih, Refused::Busy);
        }
        let now = self.now_ms;
        match self.cooldown_entry(ih) {
            Ok(c) => c.last_get = Some(now),
            Err(r) => return self.refuse(ih, r),
        }
        self.queue.push_back(Queued {
            info_hash: ih,
            announce_port: None,
        });
    }

    fn cmd_announce(&mut self, ih: Id, port: u16) {
        if self.forbidden.contains(&ih) {
            return self.refuse(ih, Refused::Forbidden);
        }
        if self.cfg.read_only {
            return self.refuse(ih, Refused::ReadOnly);
        }
        if port == 0 {
            return self.refuse(ih, Refused::BadPort);
        }
        let cd = self.cooldowns.get(&ih).copied().unwrap_or_default();
        if let Some(retry_after) = self.cooldown_left(cd.last_announce, self.cfg.announce_cooldown)
        {
            return self.refuse(ih, Refused::Cooldown { retry_after });
        }
        // Upgrade a lookup that is already queued or running instead of starting a second one.
        let mut upgraded = false;
        if let Some(q) = self.queue.iter_mut().find(|q| q.info_hash == ih) {
            if q.announce_port.is_none() {
                q.announce_port = Some(port);
                upgraded = true;
            }
        } else if let Some(l) = self
            .lookups
            .iter_mut()
            .find(|l| l.target == ih && matches!(l.purpose, Purpose::Peers { .. }))
        {
            if !l.announcing
                && matches!(
                    l.purpose,
                    Purpose::Peers {
                        announce_port: None
                    }
                )
            {
                l.purpose = Purpose::Peers {
                    announce_port: Some(port),
                };
                upgraded = true;
            }
        }
        let now = self.now_ms;
        if upgraded {
            if let Ok(c) = self.cooldown_entry(ih) {
                c.last_announce = Some(now);
            }
            return;
        }
        if self.lookup_in_progress(&ih) {
            return self.refuse(ih, Refused::InProgress);
        }
        if self.queue.len() >= self.cfg.max_queue {
            return self.refuse(ih, Refused::Busy);
        }
        match self.cooldown_entry(ih) {
            Ok(c) => {
                c.last_announce = Some(now);
                c.last_get = Some(now);
            }
            Err(r) => return self.refuse(ih, r),
        }
        self.queue.push_back(Queued {
            info_hash: ih,
            announce_port: Some(port),
        });
    }

    fn cmd_bootstrap(&mut self, addrs: &[SocketAddrV4]) {
        self.wait_table_until_ms = self.now_ms + 20_000;
        let seeds: Vec<SocketAddrV4> = addrs
            .iter()
            .copied()
            .filter(|a| self.valid_remote(a))
            .collect();
        let li = match self
            .lookups
            .iter()
            .position(|l| l.purpose == Purpose::Bootstrap)
        {
            Some(i) => i,
            None => {
                let mut lk = self.new_lookup(
                    self.id,
                    Purpose::Bootstrap,
                    self.cfg.alpha,
                    64,
                    self.cfg.lookup_timeout,
                );
                for n in self.table.closest(&self.id, K, self.secs()) {
                    lk.add_candidate(n.id, n.addr, true);
                }
                self.lookups.push(lk);
                self.lookups.len() - 1
            }
        };
        let target = self.lookups[li].target;
        for a in seeds {
            self.lookups[li].add_candidate(target, a, false);
        }
    }

    fn new_lookup(
        &mut self,
        target: Id,
        purpose: Purpose,
        alpha: u8,
        max_queries: u16,
        timeout: Duration,
    ) -> Lookup {
        let id = self.next_lookup_id;
        self.next_lookup_id = self.next_lookup_id.wrapping_add(1);
        let deadline = self.now_ms + timeout.as_millis() as u64;
        Lookup::new(
            id,
            target,
            purpose,
            alpha,
            max_queries,
            self.now_ms,
            deadline,
            self.cfg.max_peers_per_lookup,
        )
    }

    // ------------------------------------------------------------ lookup driving

    fn start_queued(&mut self) {
        loop {
            let active = self
                .lookups
                .iter()
                .filter(|l| matches!(l.purpose, Purpose::Peers { .. }))
                .count();
            if active >= self.cfg.max_concurrent_lookups {
                return;
            }
            if self.queue.is_empty() {
                return;
            }
            // While joining, give the table a chance to fill: a lookup seeded with 2 nodes is a bad lookup.
            let usable = self.table.usable_count();
            let joining =
                self.lookups.iter().any(|l| l.purpose == Purpose::Bootstrap) && usable < K;
            if joining || (usable == 0 && self.now_ms < self.wait_table_until_ms) {
                return;
            }
            let Some(q) = self.queue.pop_front() else {
                return;
            };
            let mut lk = self.new_lookup(
                q.info_hash,
                Purpose::Peers {
                    announce_port: q.announce_port,
                },
                self.cfg.alpha,
                self.cfg.max_queries_per_lookup,
                self.cfg.lookup_timeout,
            );
            for n in self.table.closest(&q.info_hash, 2 * K, self.secs()) {
                lk.add_candidate(n.id, n.addr, true);
            }
            self.lookups.push(lk);
        }
    }

    fn drive(&mut self) {
        loop {
            self.start_queued();
            self.start_fills();
            self.budget_blocked = false;
            let mut lks = std::mem::take(&mut self.lookups);
            let mut finished = false;
            lks.retain_mut(|lk| {
                if self.advance(lk) {
                    self.finish(lk);
                    finished = true;
                    false
                } else {
                    true
                }
            });
            self.lookups = lks;
            // A finished lookup frees a slot for a queued one.
            if !finished || self.queue.is_empty() {
                break;
            }
        }
    }

    /// Sends what can be sent. Returns true when the lookup is complete.
    fn advance(&mut self, lk: &mut Lookup) -> bool {
        if !lk.announcing {
            if self.now_ms >= lk.deadline_ms {
                lk.stop = true;
            }
            loop {
                while lk.in_flight < lk.alpha {
                    let Some(ci) = lk.next_to_query() else { break };
                    if !self.take_budget(!matches!(lk.purpose, Purpose::Peers { .. })) {
                        self.budget_blocked = true;
                        break;
                    }
                    let c = lk.cands[ci];
                    let q = match lk.purpose {
                        Purpose::Peers { .. } => Q::GetPeers(lk.target),
                        _ => Q::FindNode(lk.target),
                    };
                    self.send_query(c.addr, c.id_known.then_some(c.id), Owner::Lookup(lk.id), q);
                    lk.mark_sent(ci);
                }
                if lk.search_finished() && lk.try_widen() {
                    continue;
                }
                break;
            }
            if !lk.search_finished() {
                return false;
            }
            match lk.purpose {
                Purpose::Peers {
                    announce_port: Some(_),
                } => {
                    lk.announce_queue = lk.announce_targets();
                    lk.announce_attempted = lk.announce_queue.len() as u8;
                    lk.announcing = true;
                }
                _ => return true,
            }
        }
        let Purpose::Peers {
            announce_port: Some(port),
        } = lk.purpose
        else {
            return true;
        };
        while let Some(&(addr, id, tok)) = lk.announce_queue.last() {
            if !self.take_budget(false) {
                self.budget_blocked = true;
                break;
            }
            lk.announce_queue.pop();
            self.send_query(
                addr,
                Some(id),
                Owner::Lookup(lk.id),
                Q::Announce(lk.target, port, tok),
            );
            lk.announce_outstanding += 1;
        }
        lk.announce_queue.is_empty() && lk.announce_outstanding == 0
    }

    /// After a bootstrap, probe the sparse far buckets once so the table fills out quickly
    /// instead of waiting for incoming traffic. Not repeated in steady state.
    fn queue_fills(&mut self) {
        for idx in 0..self.table.bucket_count().saturating_sub(1) {
            if self.table.bucket_len(idx) < K && self.fill_targets.len() < 32 {
                let t = self.table.random_id_in_bucket(idx, &mut self.rng);
                self.fill_targets.push_back(t);
            }
        }
    }

    fn start_fills(&mut self) {
        let running = self
            .lookups
            .iter()
            .filter(|l| l.purpose == Purpose::Refresh)
            .count();
        if self.fill_targets.is_empty() && running == 0 && self.fill_rounds_left > 0 {
            self.fill_rounds_left -= 1;
            self.queue_fills();
            if self.fill_targets.is_empty() {
                self.fill_rounds_left = 0; // nothing sparse left
            }
        }
        while !self.fill_targets.is_empty()
            && self
                .lookups
                .iter()
                .filter(|l| l.purpose == Purpose::Refresh)
                .count()
                < MAX_REFRESH_LOOKUPS
        {
            let Some(target) = self.fill_targets.pop_front() else {
                break;
            };
            let secs = self.secs();
            let mut lk = self.new_lookup(
                target,
                Purpose::Refresh,
                self.cfg.alpha,
                12,
                Duration::from_secs(20),
            );
            for n in self.table.closest(&target, K, secs) {
                lk.add_candidate(n.id, n.addr, true);
            }
            self.lookups.push(lk);
        }
    }

    fn finish(&mut self, lk: &Lookup) {
        if lk.purpose == Purpose::Bootstrap && !self.table.is_empty() {
            // Up to three rounds: each round's replies split buckets and expose new sparse ones.
            self.fill_rounds_left = MAX_FILL_ROUNDS;
        }
        if let Purpose::Peers { announce_port } = lk.purpose {
            self.out.push(Intent::Event(Event::LookupFinished {
                info_hash: lk.target,
                stats: LookupStats {
                    queries: lk.queries_sent,
                    replied: lk.replied,
                    peers: lk.peers_seen.len() as u32,
                    elapsed_ms: self.now_ms.saturating_sub(lk.started_ms),
                },
            }));
            if announce_port.is_some() {
                self.out.push(Intent::Event(Event::Announced {
                    info_hash: lk.target,
                    accepted: lk.announce_accepted,
                    attempted: lk.announce_attempted,
                }));
            }
        }
    }

    // ------------------------------------------------------------ maintenance

    fn maintenance(&mut self) {
        let Some(due) = self.next_maintenance_ms() else {
            return;
        };
        if self.now_ms < due
            || self.now_ms < self.last_maint_ms + MAINT_SPACING_MS && self.last_maint_ms != 0
        {
            return;
        }
        self.last_maint_ms = self.now_ms.max(1);
        if self.needs_bootstrap() && self.now_ms >= self.next_bootstrap_ms {
            self.out.push(Intent::Event(Event::NeedBootstrap));
            self.next_bootstrap_ms = self.now_ms + self.bootstrap_backoff_ms;
            self.bootstrap_backoff_ms = (self.bootstrap_backoff_ms * 2).min(3_600_000);
        }
        if self.table.usable_count() >= K {
            self.bootstrap_backoff_ms = 15_000;
        }
        let secs = self.secs();
        let interval = self.cfg.refresh_interval.as_secs() as u32;
        for idx in self.table.stale_buckets(secs, interval) {
            let running = self
                .lookups
                .iter()
                .filter(|l| l.purpose == Purpose::Refresh)
                .count();
            if running >= MAX_REFRESH_LOOKUPS {
                break;
            }
            let target = self.table.random_id_in_bucket(idx, &mut self.rng);
            self.table.mark_refreshed(idx, secs);
            let mut lk = self.new_lookup(target, Purpose::Refresh, 1, 3, Duration::from_secs(20));
            for n in self.table.closest(&target, K, secs) {
                lk.add_candidate(n.id, n.addr, true);
            }
            self.lookups.push(lk);
        }
    }

    // ------------------------------------------------------------ persistence

    /// Compact snapshot of the routing table: version, own id, external IP, then up to
    /// [`MAX_SAVED_NODES`] 26-byte node entries (best nodes first). About 5.3 KiB at the cap.
    pub fn save_state(&self) -> Vec<u8> {
        let mut nodes: Vec<_> = self.table.iter().filter(|n| n.fails == 0).collect();
        nodes.sort_by_key(|n| std::cmp::Reverse(n.last_seen));
        nodes.truncate(MAX_SAVED_NODES);
        let mut v = Vec::with_capacity(28 + nodes.len() * NODE_LEN);
        v.push(STATE_VERSION);
        v.extend_from_slice(&self.id);
        v.extend_from_slice(&self.ext_ip.map_or([0u8; 4], |i| i.octets()));
        v.extend_from_slice(&(nodes.len() as u16).to_be_bytes());
        for n in nodes {
            codec::encode_node(&n.id, &n.addr, &mut v);
        }
        v
    }

    /// Restores a snapshot: adopts the saved node id and starts a bootstrap lookup seeded with the
    /// saved nodes (they enter the table only after they answer).
    pub fn load_state(&mut self, data: &[u8], now: Instant) -> Result<Vec<Intent>, StateError> {
        if data.len() < 27 || data[0] != STATE_VERSION {
            return Err(StateError::Invalid);
        }
        let mut id = [0u8; 20];
        id.copy_from_slice(&data[1..21]);
        let ip = Ipv4Addr::new(data[21], data[22], data[23], data[24]);
        let count = usize::from(u16::from_be_bytes([data[25], data[26]]));
        let body = &data[27..];
        if count > MAX_SAVED_NODES || body.len() != count * NODE_LEN {
            return Err(StateError::Invalid);
        }
        self.now_ms = self.now_ms.max(self.ms(now));
        if self.table.is_empty() {
            self.id = id;
            self.table = RoutingTable::new(id, self.secs());
            self.table.set_unique_ip(self.cfg.unique_ip_per_table);
        }
        if !ip.is_unspecified() && self.ext_ip.is_none() {
            self.ext_ip = Some(ip);
        }
        let mut lk = self.new_lookup(
            self.id,
            Purpose::Bootstrap,
            self.cfg.alpha,
            64,
            self.cfg.lookup_timeout,
        );
        for c in body.chunks_exact(NODE_LEN) {
            let mut nid = [0u8; 20];
            nid.copy_from_slice(&c[..20]);
            let addr = SocketAddrV4::new(
                Ipv4Addr::new(c[20], c[21], c[22], c[23]),
                u16::from_be_bytes([c[24], c[25]]),
            );
            if nid != self.id && self.valid_remote(&addr) {
                lk.add_candidate(nid, addr, true);
            }
        }
        if lk.cands.is_empty() {
            return Ok(Vec::new());
        }
        self.wait_table_until_ms = self.now_ms + 20_000;
        if !self.lookups.iter().any(|l| l.purpose == Purpose::Bootstrap) {
            self.lookups.push(lk);
        }
        self.drive();
        Ok(std::mem::take(&mut self.out))
    }
}
