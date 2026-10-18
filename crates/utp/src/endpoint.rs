//! Sans-I/O socket multiplexer: many uTP connections behind one UDP socket.
//!
//! The caller feeds datagrams in with [`Endpoint::handle_datagram`] (which also tells it whether
//! the datagram was uTP at all, so DHT and UDP-tracker traffic can share the port), drains
//! outgoing datagrams with [`Endpoint::poll_transmit`], and runs [`Endpoint::handle_timeout`] at
//! [`Endpoint::poll_timeout`]. One ordered timer set serves every connection: far-future
//! deadlines (keep-alive, idle, linger) are rounded to 1 s boundaries so a thousand idle
//! connections cost about one wake-up per second in total, not one timer each.
//!
//! Abuse limits (all in [`Config`]): total connections, half-open inbound connections, per-IP
//! connections, accept backlog, RESET reply rate, and a 3x amplification budget for inbound
//! connections that have not yet acknowledged our first packet.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io;
use std::net::{IpAddr, SocketAddr};

use crate::config::Config;
use crate::conn::{Connection, End, ReadOutcome, State, Stats, WriteOutcome};
use crate::packet::{self, Header, Packet, PacketType};
use crate::rng::Rng;

/// Opaque, generation-checked reference to a connection inside an [`Endpoint`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ConnHandle {
    index: u32,
    epoch: u32,
}

impl ConnHandle {
    /// Dense slot index, usable to index side tables (e.g. wakers). Reused after a close.
    pub fn index(&self) -> usize {
        self.index as usize
    }
}

/// What [`Endpoint::handle_datagram`] did with a datagram.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    /// It was uTP (valid or not) and has been consumed.
    Utp,
    /// It is not uTP: the caller should route it elsewhere (DHT, UDP tracker, ...).
    Foreign,
}

/// Coarse connection status for the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnStatus {
    /// SYN in flight.
    Connecting,
    /// Established (possibly half-closed).
    Open,
    /// Finished or failed.
    Closed(End),
}

const S_TX_QUEUED: u8 = 1;
const S_EV_QUEUED: u8 = 2;
const S_ATTACHED: u8 = 4;
const S_IN_MAP: u8 = 8;
const S_HALF_OPEN: u8 = 16;
const S_INBOUND: u8 = 32;

/// One entry of the connection table.
// size: 56 bytes (+ the boxed Connection, see its own size note)
struct Slot {
    conn: Option<Box<Connection>>,
    remote: SocketAddr,
    timer: u64,
    epoch: u32,
    flags: u8,
    pending_events: u8,
}

struct ResetReply {
    to: SocketAddr,
    conn_id: u16,
    ack_nr: u16,
}

/// Cap on queued RESET replies (they are also rate limited).
const MAX_PENDING_RESETS: usize = 16;
/// Deadlines further away than this are rounded up to a whole second.
const COARSE_TIMER_AFTER_US: u64 = 5_000_000;

/// The uTP multiplexer.
pub struct Endpoint {
    cfg: Config,
    rng: Rng,
    slots: Vec<Slot>,
    free: Vec<u32>,
    by_key: HashMap<(SocketAddr, u16), u32>,
    per_ip: HashMap<IpAddr, u16>,
    timers: BTreeSet<(u64, u32)>,
    tx_queue: VecDeque<u32>,
    ev_queue: VecDeque<u32>,
    accept_queue: VecDeque<ConnHandle>,
    resets: VecDeque<ResetReply>,
    reset_tokens: u32,
    reset_last: u64,
    n_live: usize,
    n_half_open: usize,
    accepting: bool,
    same_host_check: Option<fn(std::net::IpAddr) -> bool>,
    last_df: bool,
}

impl Endpoint {
    /// New endpoint. `seed` feeds connection ids and initial sequence numbers.
    pub fn new(cfg: Config, seed: u64) -> Self {
        let cfg = cfg.sanitized();
        Endpoint {
            reset_tokens: cfg.resets_per_sec,
            accepting: cfg.accept_incoming,
            same_host_check: None,
            last_df: false,
            cfg,
            rng: Rng::new(seed),
            slots: Vec::new(),
            free: Vec::new(),
            by_key: HashMap::new(),
            per_ip: HashMap::new(),
            timers: BTreeSet::new(),
            tx_queue: VecDeque::new(),
            ev_queue: VecDeque::new(),
            accept_queue: VecDeque::new(),
            resets: VecDeque::new(),
            reset_last: 0,
            n_live: 0,
            n_half_open: 0,
        }
    }

    /// The (sanitised) configuration in use.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// Tell the endpoint how to recognise peers on this host beyond `127.0.0.0/8` and `::1`
    /// (the tokio driver installs a check for our own interface addresses). Without it only
    /// loopback addresses count.
    pub fn set_same_host_check(&mut self, f: fn(std::net::IpAddr) -> bool) {
        self.same_host_check = Some(f);
    }

    fn same_host(&self, ip: std::net::IpAddr) -> bool {
        ip.is_loopback() || self.same_host_check.is_some_and(|f| f(ip))
    }

    /// Enable or disable creating connections from incoming SYNs (e.g. while shutting down).
    pub fn set_accepting(&mut self, on: bool) {
        self.accepting = on;
    }

    /// Connections currently routable (excludes closed ones the application still holds).
    pub fn live_connections(&self) -> usize {
        self.n_live
    }

    /// Inbound connections that have not yet confirmed their address.
    pub fn half_open(&self) -> usize {
        self.n_half_open
    }

    /// Bytes a datagram buffer must have for [`poll_transmit`](Self::poll_transmit): header +
    /// maximal SACK extension + payload.
    pub fn transmit_buffer_len(&self) -> usize {
        self.cfg.max_datagram()
    }

    // ---- slab -------------------------------------------------------------------------------

    fn handle_of(&self, idx: u32) -> ConnHandle {
        ConnHandle { index: idx, epoch: self.slots.get(idx as usize).map_or(0, |s| s.epoch) }
    }

    fn slot_mut(&mut self, h: ConnHandle) -> Option<&mut Slot> {
        self.slots
            .get_mut(h.index as usize)
            .filter(|s| s.epoch == h.epoch && s.conn.is_some())
    }

    fn conn_of(slots: &mut [Slot], h: ConnHandle) -> Option<&mut Connection> {
        slots
            .get_mut(h.index as usize)
            .filter(|s| s.epoch == h.epoch)
            .and_then(|s| s.conn.as_deref_mut())
    }

    fn alloc(&mut self, conn: Connection, remote: SocketAddr, flags: u8) -> Option<u32> {
        let slot = Slot {
            conn: Some(Box::new(conn)),
            remote,
            timer: 0,
            epoch: 0,
            flags,
            pending_events: 0,
        };
        let idx = match self.free.pop() {
            Some(i) => {
                let s = self.slots.get_mut(i as usize)?;
                let epoch = s.epoch;
                *s = Slot { epoch, ..slot };
                i
            }
            None => {
                let i = u32::try_from(self.slots.len()).ok()?;
                self.slots.push(slot);
                i
            }
        };
        Some(idx)
    }

    fn free_slot(&mut self, idx: u32) {
        let Some(s) = self.slots.get_mut(idx as usize) else { return };
        if s.timer != 0 {
            self.timers.remove(&(s.timer, idx));
        }
        if s.flags & S_IN_MAP != 0 {
            // Normally already unmapped when the connection closed; belt and braces.
            let key = s.conn.as_ref().map(|c| (s.remote, c.recv_id()));
            if let Some(k) = key {
                self.by_key.remove(&k);
            }
        }
        s.conn = None;
        s.timer = 0;
        s.flags = 0;
        s.pending_events = 0;
        s.epoch = s.epoch.wrapping_add(1);
        self.free.push(idx);
        // Slab memory is reusable but never returned; shrink the free list's Vec when it is the
        // whole table so an endpoint that saw a burst does not keep tens of KiB forever.
        if self.free.len() == self.slots.len() {
            // No live handle can exist (only unattached slots are freed), so generations may
            // restart; stale accept-queue entries must go with them.
            self.slots = Vec::new();
            self.free = Vec::new();
            self.tx_queue = VecDeque::new();
            self.ev_queue = VecDeque::new();
            self.accept_queue = VecDeque::new();
            self.by_key.shrink_to_fit();
            self.per_ip.shrink_to_fit();
        }
    }

    fn ip_inc(&mut self, ip: IpAddr) {
        let c = self.per_ip.entry(ip).or_insert(0);
        *c = c.saturating_add(1);
    }

    fn ip_dec(&mut self, ip: IpAddr) {
        if let Some(c) = self.per_ip.get_mut(&ip) {
            *c = c.saturating_sub(1);
            if *c == 0 {
                self.per_ip.remove(&ip);
            }
        }
    }

    // ---- bookkeeping after any connection activity --------------------------------------------

    /// Settle a connection after it ran: accept-queue, unmapping, events, timers. Does not
    /// schedule transmission.
    fn settle(&mut self, idx: u32, now: u64) {
        let handle = self.handle_of(idx);
        let Some(slot) = self.slots.get_mut(idx as usize) else { return };
        let Some(conn) = slot.conn.as_mut() else { return };
        let mut confirmed_inbound = false;
        if conn.take_confirmed() {
            if slot.flags & S_HALF_OPEN != 0 {
                slot.flags &= !S_HALF_OPEN;
                self.n_half_open = self.n_half_open.saturating_sub(1);
            }
            confirmed_inbound = slot.flags & S_INBOUND != 0;
        }
        let closed = conn.state() == State::Closed;
        let ev = conn.take_events();
        let key = (slot.remote, conn.recv_id());
        let remote_ip = slot.remote.ip();
        if closed {
            if slot.flags & S_HALF_OPEN != 0 {
                slot.flags &= !S_HALF_OPEN;
                self.n_half_open = self.n_half_open.saturating_sub(1);
            }
            if slot.flags & S_IN_MAP != 0 {
                slot.flags &= !S_IN_MAP;
                self.by_key.remove(&key);
                self.n_live = self.n_live.saturating_sub(1);
                self.ip_dec(remote_ip);
            }
        }
        let Some(slot) = self.slots.get_mut(idx as usize) else { return };
        if closed && slot.flags & S_ATTACHED == 0 {
            self.free_slot(idx);
            return;
        }
        if confirmed_inbound {
            self.accept_queue.push_back(handle);
        }
        if ev != 0 && slot.flags & S_ATTACHED != 0 {
            slot.pending_events |= ev;
            if slot.flags & S_EV_QUEUED == 0 {
                slot.flags |= S_EV_QUEUED;
                self.ev_queue.push_back(idx);
            }
        }
        self.retimer(idx, now);
    }

    fn retimer(&mut self, idx: u32, now: u64) {
        let Some(slot) = self.slots.get_mut(idx as usize) else { return };
        let Some(conn) = slot.conn.as_ref() else { return };
        let want = conn
            .poll_timeout(&self.cfg)
            .map(|t| {
                let t = t.max(now + 1);
                if t > now + COARSE_TIMER_AFTER_US { (t / 1_000_000 + 1) * 1_000_000 } else { t }
            })
            .unwrap_or(0);
        if slot.timer == want {
            return;
        }
        if slot.timer != 0 {
            self.timers.remove(&(slot.timer, idx));
        }
        if want != 0 {
            self.timers.insert((want, idx));
        }
        slot.timer = want;
    }

    /// `settle` plus a transmit opportunity.
    fn after(&mut self, idx: u32, now: u64) {
        self.settle(idx, now);
        if let Some(slot) = self.slots.get_mut(idx as usize)
            && let Some(conn) = slot.conn.as_ref()
                && slot.flags & S_TX_QUEUED == 0 && conn.wants_transmit(&self.cfg, now) {
                    slot.flags |= S_TX_QUEUED;
                    self.tx_queue.push_back(idx);
                }
    }

    // ---- network input ------------------------------------------------------------------------

    /// Offer one received datagram. Non-uTP datagrams are left untouched for the caller.
    pub fn handle_datagram(&mut self, now: u64, from: SocketAddr, data: &[u8]) -> Disposition {
        if !packet::looks_like_utp(data) {
            return Disposition::Foreign;
        }
        let Ok(pkt) = Packet::parse(data) else { return Disposition::Utp };
        let h = pkt.header;
        match h.ty {
            PacketType::Syn => self.on_syn(now, from, &pkt, data.len()),
            PacketType::Reset => {
                // libutp semantics: the id may be our recv id, or our send id (then our recv id
                // is id+1 when we initiated, id-1 when we accepted).
                let id = h.conn_id;
                let idx = self
                    .lookup_reset(from, id, id)
                    .or_else(|| self.lookup_reset(from, id.wrapping_add(1), id))
                    .or_else(|| self.lookup_reset(from, id.wrapping_sub(1), id));
                if let Some(idx) = idx {
                    self.deliver(idx, now, data.len(), &pkt);
                }
            }
            _ => match self.by_key.get(&(from, h.conn_id)).copied() {
                Some(idx) => self.deliver(idx, now, data.len(), &pkt),
                None => self.queue_reset(now, from, &h),
            },
        }
        Disposition::Utp
    }

    /// Slot for `(from, key_id)`; when `send_id` differs from `key_id` the slot must use it as
    /// its send id (checked through the connection's wire id invariant).
    fn lookup_reset(&self, from: SocketAddr, key_id: u16, id: u16) -> Option<u32> {
        let idx = *self.by_key.get(&(from, key_id))?;
        if key_id == id {
            return Some(idx);
        }
        let conn = self.slots.get(idx as usize)?.conn.as_ref()?;
        // initiator: send = recv + 1; acceptor: send = recv - 1.
        let send = if conn.is_initiator() { key_id.wrapping_add(1) } else { key_id.wrapping_sub(1) };
        (send == id).then_some(idx)
    }

    fn deliver(&mut self, idx: u32, now: u64, len: usize, pkt: &Packet<'_>) {
        if let Some(conn) = self.slots.get_mut(idx as usize).and_then(|s| s.conn.as_mut()) {
            conn.handle_packet(&self.cfg, now, len, pkt);
        }
        self.after(idx, now);
    }

    fn on_syn(&mut self, now: u64, from: SocketAddr, pkt: &Packet<'_>, len: usize) {
        let h = pkt.header;
        if let Some(&idx) = self.by_key.get(&(from, h.conn_id.wrapping_add(1))) {
            // Retransmitted SYN for a connection we already created.
            self.deliver(idx, now, len, pkt);
            return;
        }
        if !self.accepting
            || self.n_live >= self.cfg.max_connections
            || self.n_half_open >= self.cfg.max_half_open
            || self.accept_queue.len() >= self.cfg.accept_backlog
            || self.per_ip.get(&from.ip()).copied().unwrap_or(0) as usize >= self.cfg.max_per_ip
            || from.port() == 0
        {
            return; // silent: answering would only help a flooder
        }
        let seq = self.rng.next_u16();
        let mut conn = Connection::accept(&self.cfg, now, &h, seq, len);
        let same = self.same_host(from.ip());
        conn.set_path_ceiling(&self.cfg, self.cfg.path_ceiling(same));
        let flags = S_IN_MAP | S_HALF_OPEN | S_INBOUND;
        let Some(idx) = self.alloc(conn, from, flags) else { return };
        if crate::trace::on() {
            eprintln!("utp {} SYN accepted from {} id={}", crate::trace::wall_ms(), from, h.conn_id.wrapping_add(1));
        }
        self.by_key.insert((from, h.conn_id.wrapping_add(1)), idx);
        self.n_live += 1;
        self.n_half_open += 1;
        self.ip_inc(from.ip());
        self.after(idx, now);
    }

    fn queue_reset(&mut self, now: u64, to: SocketAddr, h: &Header) {
        let rate = self.cfg.resets_per_sec;
        let add = (now.saturating_sub(self.reset_last) as u128 * rate as u128 / 1_000_000) as u32;
        if add > 0 {
            self.reset_tokens = self.reset_tokens.saturating_add(add).min(rate);
            self.reset_last = now;
        }
        if self.reset_tokens == 0 || self.resets.len() >= MAX_PENDING_RESETS {
            return;
        }
        self.reset_tokens -= 1;
        self.resets.push_back(ResetReply { to, conn_id: h.conn_id, ack_nr: h.seq_nr });
    }

    // ---- network output -----------------------------------------------------------------------

    /// Next datagram to send, written into `buf` (see [`transmit_buffer_len`](Self::transmit_buffer_len)).
    /// Returns the destination and length. Call until `None` after every input, app operation
    /// and timeout.
    pub fn poll_transmit(&mut self, now: u64, buf: &mut [u8]) -> Option<(SocketAddr, usize)> {
        self.last_df = false;
        if let Some(r) = self.resets.pop_front() {
            let hdr = Header {
                ty: PacketType::Reset,
                conn_id: r.conn_id,
                timestamp: now as u32,
                timestamp_diff: 0,
                wnd_size: 0,
                seq_nr: self.rng.next_u16(),
                ack_nr: r.ack_nr,
            };
            if let Some(n) = packet::encode(&hdr, &[], buf) {
                return Some((r.to, n));
            }
        }
        while let Some(idx) = self.tx_queue.pop_front() {
            let Some(slot) = self.slots.get_mut(idx as usize) else { continue };
            slot.flags &= !S_TX_QUEUED;
            let remote = slot.remote;
            let Some(conn) = slot.conn.as_mut() else { continue };
            match conn.poll_transmit(&self.cfg, now, buf) {
                Some(n) => {
                    self.last_df = conn.dont_fragment();
                    // The connection may have more to say; keep its place at the back of the line
                    // so a busy connection cannot starve the others.
                    slot.flags |= S_TX_QUEUED;
                    self.tx_queue.push_back(idx);
                    self.settle(idx, now);
                    return Some((remote, n));
                }
                None => self.settle(idx, now),
            }
        }
        None
    }

    /// Whether the datagram the last [`poll_transmit`](Self::poll_transmit) returned must be sent
    /// with the Don't-Fragment bit set (a path-MTU probe). Every other datagram is sent with DF
    /// clear, so a retransmitted probe can still get through a smaller path by fragmenting.
    pub fn dont_fragment(&self) -> bool {
        self.last_df
    }

    /// True if a call to [`poll_transmit`](Self::poll_transmit) might yield something now.
    pub fn has_pending_transmit(&self) -> bool {
        !self.tx_queue.is_empty() || !self.resets.is_empty()
    }

    /// Earliest time [`handle_timeout`](Self::handle_timeout) has work.
    pub fn poll_timeout(&self) -> Option<u64> {
        self.timers.first().map(|&(t, _)| t)
    }

    /// Run every timer due at `now`.
    pub fn handle_timeout(&mut self, now: u64) {
        while let Some(&(t, idx)) = self.timers.first() {
            if t > now {
                break;
            }
            self.timers.remove(&(t, idx));
            if let Some(slot) = self.slots.get_mut(idx as usize) {
                slot.timer = 0;
                if let Some(conn) = slot.conn.as_mut() {
                    conn.handle_timeout(&self.cfg, now);
                }
            }
            self.after(idx, now);
        }
    }

    // ---- application API ----------------------------------------------------------------------

    /// Start an outgoing connection to `remote`.
    pub fn connect(&mut self, now: u64, remote: SocketAddr) -> io::Result<ConnHandle> {
        if self.n_live >= self.cfg.max_connections {
            return Err(io::Error::other("uTP connection limit reached"));
        }
        let mut id = self.rng.next_u16();
        let mut tries = 0;
        while self.by_key.contains_key(&(remote, id)) {
            id = self.rng.next_u16();
            tries += 1;
            if tries > 64 {
                return Err(io::Error::other("no free uTP connection id"));
            }
        }
        let mut conn = Connection::connect(&self.cfg, now, id);
        let same = self.same_host(remote.ip());
        conn.set_path_ceiling(&self.cfg, self.cfg.path_ceiling(same));
        let Some(idx) = self.alloc(conn, remote, S_IN_MAP | S_ATTACHED) else {
            return Err(io::Error::other("uTP connection table full"));
        };
        self.by_key.insert((remote, id), idx);
        self.n_live += 1;
        self.ip_inc(remote.ip());
        self.after(idx, now);
        Ok(self.handle_of(idx))
    }

    /// Next confirmed inbound connection, now owned by the caller (call [`close`](Self::close)
    /// when done with it).
    pub fn poll_accept(&mut self) -> Option<ConnHandle> {
        while let Some(h) = self.accept_queue.pop_front() {
            if let Some(slot) = self.slot_mut(h) {
                slot.flags |= S_ATTACHED;
                return Some(h);
            }
        }
        None
    }

    /// True when `poll_accept` would return a connection.
    pub fn has_pending_accept(&self) -> bool {
        !self.accept_queue.is_empty()
    }

    /// Next connection with new read/write readiness: `(handle, EV_READ | EV_WRITE bits)`.
    pub fn poll_event(&mut self) -> Option<(ConnHandle, u8)> {
        while let Some(idx) = self.ev_queue.pop_front() {
            let Some(slot) = self.slots.get_mut(idx as usize) else { continue };
            slot.flags &= !S_EV_QUEUED;
            let ev = std::mem::take(&mut slot.pending_events);
            if slot.conn.is_some() && ev != 0 {
                return Some((self.handle_of(idx), ev));
            }
        }
        None
    }

    /// Remote address of a connection.
    pub fn peer_addr(&self, h: ConnHandle) -> Option<SocketAddr> {
        self.slots.get(h.index as usize).filter(|s| s.epoch == h.epoch && s.conn.is_some()).map(|s| s.remote)
    }

    /// Status of a connection (a stale handle reads as reset).
    pub fn status(&self, h: ConnHandle) -> ConnStatus {
        match self.slots.get(h.index as usize).filter(|s| s.epoch == h.epoch).and_then(|s| s.conn.as_ref()) {
            None => ConnStatus::Closed(End::Reset),
            Some(c) => match c.state() {
                State::SynSent => ConnStatus::Connecting,
                State::Closed => ConnStatus::Closed(c.end()),
                _ => ConnStatus::Open,
            },
        }
    }

    /// Congestion/loss counters for a connection.
    pub fn conn_stats(&self, h: ConnHandle) -> Option<Stats> {
        self.slots
            .get(h.index as usize)
            .filter(|s| s.epoch == h.epoch)
            .and_then(|s| s.conn.as_ref())
            .map(|c| c.stats())
    }

    /// Print a state line for every busy connection (opt-in trace; see `TRS_UTP_TRACE`).
    pub(crate) fn trace_dump(&self, now: u64) {
        let ms = crate::trace::wall_ms();
        eprintln!("utp {} ep txq={} timers={} live={}", ms, self.tx_queue.len(), self.timers.len(), self.n_live);
        for s in &self.slots {
            if let Some(c) = s.conn.as_ref().filter(|c| c.is_busy()) {
                eprintln!(
                    "utp {} snap {} id={} {} want_tx={} queued={} timer_in={}ms",
                    ms,
                    s.remote,
                    c.recv_id(),
                    c.trace_line(),
                    c.wants_transmit(&self.cfg, now) as u8,
                    (s.flags & S_TX_QUEUED != 0) as u8,
                    (s.timer as i64 - now as i64) / 1000
                );
            }
        }
    }

    /// Heap bytes held by one connection's buffers.
    pub fn conn_heap_bytes(&self, h: ConnHandle) -> usize {
        self.slots
            .get(h.index as usize)
            .filter(|s| s.epoch == h.epoch)
            .and_then(|s| s.conn.as_ref())
            .map_or(0, |c| c.heap_bytes())
    }

    fn io_err(end: End) -> io::Error {
        match end {
            End::Graceful => io::ErrorKind::BrokenPipe.into(),
            End::Reset => io::ErrorKind::ConnectionReset.into(),
            End::TimedOut => io::ErrorKind::TimedOut.into(),
            End::Refused => io::ErrorKind::ConnectionRefused.into(),
        }
    }

    /// Read in-order bytes. `Ok(0)` is EOF (for non-empty `out`); `WouldBlock` means wait for
    /// an `EV_READ` event.
    pub fn read(&mut self, h: ConnHandle, now: u64, out: &mut [u8]) -> io::Result<usize> {
        let Some(conn) = Self::conn_of(&mut self.slots, h) else {
            return Err(io::ErrorKind::ConnectionReset.into());
        };
        let r = conn.read(&self.cfg, now, out);
        self.after(h.index, now);
        match r {
            ReadOutcome::Data(n) => Ok(n),
            ReadOutcome::Eof => Ok(0),
            ReadOutcome::WouldBlock => Err(io::ErrorKind::WouldBlock.into()),
            ReadOutcome::Err(e) => Err(Self::io_err(e)),
        }
    }

    /// Bytes that a [`read`](Self::read) would return right now (0 for stale handles).
    pub fn readable_len(&mut self, h: ConnHandle) -> usize {
        Self::conn_of(&mut self.slots, h).map_or(0, |c| c.readable_len())
    }

    /// Whether [`write`](Self::write) would not return `WouldBlock` right now (true for stale
    /// handles, whose `write` reports the error).
    pub fn write_ready(&mut self, h: ConnHandle) -> bool {
        match Self::conn_of(&mut self.slots, h) {
            Some(c) => c.write_ready(&self.cfg),
            None => true,
        }
    }

    /// Queue bytes to send; returns how many were accepted (`WouldBlock` when the buffer is full).
    pub fn write(&mut self, h: ConnHandle, now: u64, data: &[u8]) -> io::Result<usize> {
        let Some(conn) = Self::conn_of(&mut self.slots, h) else {
            return Err(io::ErrorKind::ConnectionReset.into());
        };
        let r = conn.write(&self.cfg, data);
        self.after(h.index, now);
        match r {
            WriteOutcome::Wrote(n) => Ok(n),
            WriteOutcome::WouldBlock => Err(io::ErrorKind::WouldBlock.into()),
            WriteOutcome::Closed => Err(io::ErrorKind::BrokenPipe.into()),
            WriteOutcome::Err(e) => Err(Self::io_err(e)),
        }
    }

    /// Half-close: FIN after queued data.
    pub fn shutdown_write(&mut self, h: ConnHandle, now: u64) {
        if let Some(conn) = Self::conn_of(&mut self.slots, h) {
            conn.shutdown_write();
            self.after(h.index, now);
        }
    }

    /// Abort with a RESET.
    pub fn reset(&mut self, h: ConnHandle, now: u64) {
        if let Some(conn) = Self::conn_of(&mut self.slots, h) {
            conn.abort();
            self.after(h.index, now);
        }
    }

    /// The application is done with the connection: flush, FIN, linger, free. Idempotent.
    pub fn close(&mut self, h: ConnHandle, now: u64) {
        let Some(slot) = self.slots.get_mut(h.index as usize).filter(|s| s.epoch == h.epoch) else {
            return;
        };
        slot.flags &= !S_ATTACHED;
        if let Some(conn) = slot.conn.as_mut() {
            conn.abandon(&self.cfg, now);
        }
        self.after(h.index, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_size_budget() {
        println!("size_of Slot = {}", std::mem::size_of::<Slot>());
        assert!(std::mem::size_of::<Slot>() <= 64);
    }
}
