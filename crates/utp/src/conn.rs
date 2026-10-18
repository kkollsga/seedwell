//! One uTP connection as a pure state machine (BEP 29).
//!
//! Nothing here touches a clock, a socket or an allocator-heavy path: time is an injected `u64`
//! of microseconds, packets are parsed [`Packet`]s in and byte slices out, and application data
//! moves through two lazily allocated byte queues. The [`Endpoint`](crate::Endpoint) owns many
//! of these and does the routing.
//!
//! Sequence-number conventions (libutp-compatible):
//! * the initiator's SYN carries `seq_nr = 1`; its first data packet is `seq_nr = 2`;
//! * the acceptor answers with a STATE whose `ack_nr` is the SYN's `seq_nr` and whose `seq_nr` is
//!   its random initial value `S` (a STATE does not consume a number), and its first data packet
//!   also carries `S`; the initiator therefore starts with `ack_nr = S - 1`;
//! * connection ids: SYN carries the initiator's `recv_id`; afterwards the initiator sends with
//!   `recv_id + 1` and the acceptor with `recv_id`.

use std::collections::VecDeque;

use crate::bytes::ByteQueue;
use crate::config::{Config, MTU_RESOLUTION, WIRE_OVERHEAD, micros};
use crate::ledbat::{AckInfo, DelayFilter, Ledbat};
use crate::packet::{self, Header, MAX_SACK_BYTES, Packet, PacketType};
use crate::rtt::Rtt;
use crate::seq;
use crate::trace::utp_trace;

/// Event bit: data, EOF or an error is available to read.
pub const EV_READ: u8 = 1;
/// Event bit: send space opened, the connection was established, or it failed.
pub const EV_WRITE: u8 = 2;

/// How a connection ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// Both directions closed cleanly.
    Graceful,
    /// RESET received or sent.
    Reset,
    /// Retransmission, idle or handshake timeout.
    TimedOut,
    /// The peer answered our SYN with a RESET.
    Refused,
}

/// Connection phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// SYN sent, waiting for the peer's STATE.
    SynSent,
    /// Accepted from a SYN; the peer has not yet proven it received our STATE.
    SynRecv,
    /// Open (possibly half-closed).
    Connected,
    /// Finished cleanly; only re-acks a retransmitted FIN until the linger timer fires.
    Lingering,
    /// Gone; the reason is in [`Connection::end`].
    Closed,
}

/// Result of [`Connection::read`].
#[derive(Debug, PartialEq, Eq)]
pub enum ReadOutcome {
    /// Bytes copied.
    Data(usize),
    /// Peer closed its side and everything was read.
    Eof,
    /// Nothing available yet.
    WouldBlock,
    /// Connection failed.
    Err(End),
}

/// Result of [`Connection::write`].
#[derive(Debug, PartialEq, Eq)]
pub enum WriteOutcome {
    /// Bytes accepted into the send buffer.
    Wrote(usize),
    /// Send buffer full.
    WouldBlock,
    /// Our side is shut down or the connection finished cleanly.
    Closed,
    /// The connection failed.
    Err(End),
}

/// Snapshot of congestion-control and loss state, for logging and tests.
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    /// Smoothed RTT, microseconds (0 before the first sample).
    pub rtt_us: u32,
    /// Current retransmission timeout, microseconds.
    pub rto_us: u32,
    /// Congestion window, bytes.
    pub cwnd: u32,
    /// Bytes in flight.
    pub in_flight: u32,
    /// Estimated queuing delay on our outbound path, microseconds.
    pub our_delay_us: u32,
    /// Receive window last advertised by the peer.
    pub peer_wnd: u32,
    /// Packets retransmitted (timeouts and fast retransmits).
    pub retransmits: u32,
    /// Retransmission timeouts.
    pub timeouts: u32,
    /// Still in LEDBAT slow start.
    pub slow_start: bool,
    /// Largest datagram (UDP payload bytes) confirmed to cross the path; payload per packet is
    /// this minus 54.
    pub path_mtu: u32,
}

// Per in-flight packet flags.
const F_SACKED: u8 = 1; // peer holds it (selective ack)
const F_NEEDS_TX: u8 = 2; // (re)transmission pending
const F_FORCE: u8 = 4; // fast-retransmit: bypass the window gate
const F_COUNTED: u8 = 8; // included in `cur_window`
const F_FIN: u8 = 16;
const F_SYN: u8 = 32;
const F_PROBE: u8 = 64; // first transmission is a path-MTU probe (bigger than anything confirmed)

/// Bookkeeping for one unacknowledged packet. Payload bytes stay in the send buffer (no copy).
// size: 16 bytes
#[derive(Clone, Copy, Debug)]
struct Sent {
    sent_at: u64,
    len: u16,
    flags: u8,
    tx: u8,
}

/// An out-of-order packet held until the gap before it fills.
// size: 24 bytes
#[derive(Debug)]
struct Held {
    seq: u16,
    fin: bool,
    data: Box<[u8]>,
}

/// A uTP connection.
// size: 344 bytes (a unit test fails if it grows past 400); see README "Memory per idle connection".
pub struct Connection {
    // Timestamps and deadlines (microseconds; 0 = unset).
    last_recv: u64,
    last_sent: u64,
    last_data_rx: u64,
    rto_deadline: u64,
    ack_deadline: u64,
    aux_deadline: u64,
    persist_deadline: u64,
    release_at: u64,
    // Send side.
    send: ByteQueue,
    inflight: VecDeque<Sent>,
    inflight_bytes: u32,
    cur_window: u32,
    peer_wnd: u32,
    // Receive side.
    recv: ByteQueue,
    reorder: Vec<Held>,
    reorder_bytes: u32,
    last_adv: u32,
    reply_micro: u32,
    // Path MTU (datagram bytes). `dgram` is confirmed to cross the path, `dgram_ceil` is the best
    // known upper bound, `dgram_max` the configured limit, `probe_dgram` the size of the probe in
    // flight (0 = none). `mtu_at` is when a settled search restarts (0 = search active).
    mtu_at: u64,
    dgram: u16,
    dgram_ceil: u16,
    dgram_max: u16,
    probe_dgram: u16,
    /// The packet `poll_transmit` just produced wants the Don't-Fragment bit.
    df_out: bool,
    // Control.
    cc: Ledbat,
    delay: DelayFilter,
    rtt: Rtt,
    rx_bytes: u32,
    tx_bytes: u32,
    retransmits: u32,
    timeouts: u32,
    recv_id: u16,
    send_id: u16,
    seq_nr: u16,
    ack_nr: u16,
    recover: u16,
    state: State,
    end: End,
    events: u8,
    rto_count: u8,
    unacked_rx: u8,
    initiator: bool,
    shut: bool,
    abandoned: bool,
    fin_queued: bool,
    fin_acked: bool,
    remote_fin: bool,
    ack_now: bool,
    want_reset: bool,
    in_recovery: bool,
    probe: bool,
    pending_tx: bool,
    confirmed_event: bool,
}

impl Connection {
    fn blank(cfg: &Config, now: u64, recv_id: u16, send_id: u16, state: State) -> Self {
        let max = (cfg.max_payload + WIRE_OVERHEAD).min(u16::MAX as usize);
        let start = cfg.initial_datagram(max);
        let mss = (start - WIRE_OVERHEAD) as u32;
        Connection {
            last_recv: now,
            last_sent: now,
            last_data_rx: 0,
            rto_deadline: 0,
            ack_deadline: 0,
            aux_deadline: 0,
            persist_deadline: 0,
            release_at: 0,
            send: ByteQueue::new(),
            inflight: VecDeque::new(),
            inflight_bytes: 0,
            cur_window: 0,
            peer_wnd: cfg.recv_buffer as u32, // optimistic until the first packet tells us
            recv: ByteQueue::new(),
            reorder: Vec::new(),
            reorder_bytes: 0,
            last_adv: cfg.recv_buffer as u32,
            reply_micro: 0,
            mtu_at: 0,
            dgram: start as u16,
            dgram_ceil: max as u16,
            dgram_max: max as u16,
            probe_dgram: 0,
            df_out: false,
            cc: Ledbat::new(mss * cfg.initial_window_packets),
            delay: DelayFilter::new(),
            rtt: Rtt::new(micros(cfg.initial_rto).min(u32::MAX as u64) as u32),
            rx_bytes: 0,
            tx_bytes: 0,
            retransmits: 0,
            timeouts: 0,
            recv_id,
            send_id,
            seq_nr: 0,
            ack_nr: 0,
            recover: 0,
            state,
            end: End::Graceful,
            events: 0,
            rto_count: 0,
            unacked_rx: 0,
            initiator: false,
            shut: false,
            abandoned: false,
            fin_queued: false,
            fin_acked: false,
            remote_fin: false,
            ack_now: false,
            want_reset: false,
            in_recovery: false,
            probe: false,
            pending_tx: false,
            confirmed_event: false,
        }
    }

    /// Start an outgoing connection; the first [`poll_transmit`](Self::poll_transmit) yields the SYN.
    pub fn connect(cfg: &Config, now: u64, recv_id: u16) -> Self {
        let mut c = Self::blank(cfg, now, recv_id, recv_id.wrapping_add(1), State::SynSent);
        c.initiator = true;
        c.seq_nr = 2; // the SYN occupies seq 1
        c.inflight.push_back(Sent { sent_at: 0, len: 0, flags: F_SYN | F_NEEDS_TX, tx: 0 });
        c.pending_tx = true;
        c
    }

    /// Create the passive side of a connection from a received SYN. `initial_seq` is our random
    /// starting sequence number; `dgram_len` seeds the anti-amplification budget.
    pub fn accept(cfg: &Config, now: u64, syn: &Header, initial_seq: u16, dgram_len: usize) -> Self {
        let mut c = Self::blank(
            cfg,
            now,
            syn.conn_id.wrapping_add(1),
            syn.conn_id,
            State::SynRecv,
        );
        c.seq_nr = initial_seq;
        c.ack_nr = syn.seq_nr;
        c.peer_wnd = syn.wnd_size;
        c.rx_bytes = dgram_len as u32;
        c.ack_now = true;
        c.aux_deadline = now + micros(cfg.half_open_timeout);
        c.note_remote_timestamp(now, syn);
        c
    }

    /// Connection id we receive on.
    pub fn recv_id(&self) -> u16 {
        self.recv_id
    }

    /// Current phase.
    pub fn state(&self) -> State {
        self.state
    }

    /// Why the connection ended (meaningful once [`State::Closed`]).
    pub fn end(&self) -> End {
        self.end
    }

    /// True when we initiated (sent the SYN).
    pub fn is_initiator(&self) -> bool {
        self.initiator
    }

    /// True once the handshake completed in either direction.
    pub fn is_established(&self) -> bool {
        matches!(self.state, State::Connected | State::Lingering)
            || (self.state == State::Closed && self.end == End::Graceful)
    }

    /// Congestion and loss snapshot.
    pub fn stats(&self) -> Stats {
        Stats {
            rtt_us: self.rtt.srtt(),
            rto_us: self.rtt.rto(),
            cwnd: self.cc.window(),
            in_flight: self.cur_window,
            our_delay_us: self.delay.our_delay(),
            peer_wnd: self.peer_wnd,
            retransmits: self.retransmits,
            timeouts: self.timeouts,
            slow_start: self.cc.in_slow_start(),
            path_mtu: self.dgram as u32,
        }
    }

    /// One-line state dump for the opt-in trace (`TRS_UTP_TRACE`).
    pub(crate) fn trace_line(&self) -> String {
        format!(
            "st={:?} cwnd={} fl={} pkts={} q={} pw={} rto={}ms srtt={}ms dly={}ms dg={} rec={} rtx={} to={} ss={} unsent={}",
            self.state,
            self.cc.window(),
            self.cur_window,
            self.inflight.len(),
            self.send.len(),
            self.peer_wnd,
            self.rtt.rto() / 1000,
            self.rtt.srtt() / 1000,
            self.delay.our_delay() / 1000,
            self.dgram,
            self.in_recovery as u8,
            self.retransmits,
            self.timeouts,
            self.cc.in_slow_start() as u8,
            self.send.len().saturating_sub(self.inflight_bytes as usize),
        )
    }

    /// True when the connection has data queued or in flight (worth a periodic trace line).
    pub(crate) fn is_busy(&self) -> bool {
        self.state == State::Connected && (!self.inflight.is_empty() || !self.send.is_empty())
    }

    /// Take and clear the pending event bits ([`EV_READ`], [`EV_WRITE`]).
    pub fn take_events(&mut self) -> u8 {
        std::mem::take(&mut self.events)
    }

    /// True (once) after an accepted connection proved its address and may be handed to `accept`.
    pub fn take_confirmed(&mut self) -> bool {
        std::mem::take(&mut self.confirmed_event)
    }

    /// Heap bytes reserved by buffers (for idle-memory accounting).
    pub fn heap_bytes(&self) -> usize {
        self.send.capacity()
            + self.recv.capacity()
            + self.inflight.capacity() * std::mem::size_of::<Sent>()
            + self.reorder.capacity() * std::mem::size_of::<Held>()
    }

    fn free_window(&self, cfg: &Config) -> u32 {
        if self.abandoned {
            return cfg.recv_buffer as u32;
        }
        let used = self.recv.len() as u64 + self.reorder_bytes as u64;
        (cfg.recv_buffer as u64).saturating_sub(used) as u32
    }

    /// Payload bytes of a new data packet: the confirmed datagram size minus header room.
    fn pay(&self) -> usize {
        self.dgram as usize - WIRE_OVERHEAD
    }

    /// Set the largest datagram worth probing towards this peer (loopback peers: the interface MTU
    /// is 16 KiB; everyone else: what a 1500 B path carries). Called by the endpoint, which
    /// knows the remote address.
    pub fn set_path_ceiling(&mut self, cfg: &Config, ceiling: usize) {
        let max = ceiling.clamp(WIRE_OVERHEAD + 256, u16::MAX as usize);
        let start = cfg.initial_datagram(max);
        self.dgram_max = max as u16;
        self.dgram_ceil = max as u16;
        self.dgram = start as u16;
        self.probe_dgram = 0;
        self.mtu_at = 0;
        self.cc = Ledbat::new(self.pay() as u32 * cfg.initial_window_packets);
    }

    /// Whether the packet `poll_transmit` just produced must be sent with Don't-Fragment (a
    /// path-MTU probe's first transmission).
    pub fn dont_fragment(&self) -> bool {
        self.df_out
    }

    /// Size of the next path-MTU probe, if one should go out with a full-size packet now.
    fn next_probe(&mut self, now: u64) -> Option<usize> {
        if self.probe_dgram != 0 || self.state != State::Connected || self.in_recovery {
            return None;
        }
        if self.mtu_at != 0 {
            if now < self.mtu_at {
                return None;
            }
            self.mtu_at = 0;
            self.dgram_ceil = self.dgram_max; // periodic re-probe: the path may have improved
        }
        let (floor, ceil) = (self.dgram as usize, self.dgram_ceil as usize);
        if ceil < floor + MTU_RESOLUTION {
            return None;
        }
        // Try the ceiling first (the common case: the path carries it); after a failure bisect.
        Some(if self.dgram_ceil == self.dgram_max { ceil } else { (floor + ceil).div_ceil(2) })
    }

    fn settle_check(&mut self, cfg: &Config, now: u64) {
        if (self.dgram_ceil as usize) < self.dgram as usize + MTU_RESOLUTION {
            self.mtu_at = now + micros(cfg.mtu_reprobe);
        }
    }

    /// The probe packet was acknowledged: the path carries `probe_dgram`.
    fn probe_ok(&mut self, cfg: &Config, now: u64) {
        utp_trace!(self, "mtu probe ok {} (ceil {})", self.probe_dgram, self.dgram_ceil);
        self.dgram = self.dgram.max(self.probe_dgram);
        self.dgram_ceil = self.dgram_ceil.max(self.dgram);
        self.probe_dgram = 0;
        // The packet got bigger: keep the window at least two of the new packets so a connection
        // with nothing in flight can always send.
        self.cc.ensure_floor(self.pay() as u32);
        self.settle_check(cfg, now);
    }

    /// The probe packet is presumed lost: the path does not carry it. Not a congestion signal.
    fn probe_lost(&mut self, cfg: &Config, now: u64) {
        if self.probe_dgram == 0 {
            return;
        }
        utp_trace!(self, "mtu probe LOST {} (confirmed {})", self.probe_dgram, self.dgram);
        self.dgram_ceil = (self.probe_dgram - 1).max(self.dgram);
        self.probe_dgram = 0;
        self.settle_check(cfg, now);
    }

    fn una(&self) -> u16 {
        self.seq_nr.wrapping_sub(self.inflight.len() as u16)
    }

    // ---- application side -------------------------------------------------------------------

    /// Queue bytes for sending. Data written during `SynSent` is held until the handshake ends.
    pub fn write(&mut self, cfg: &Config, data: &[u8]) -> WriteOutcome {
        match self.state {
            State::Closed if self.end != End::Graceful => return WriteOutcome::Err(self.end),
            State::Closed | State::Lingering => return WriteOutcome::Closed,
            _ => {}
        }
        if self.want_reset {
            return WriteOutcome::Err(End::Reset);
        }
        if self.shut {
            return WriteOutcome::Closed;
        }
        if data.is_empty() {
            return WriteOutcome::Wrote(0);
        }
        let room = cfg.send_buffer.saturating_sub(self.send.len());
        let n = room.min(data.len());
        if n == 0 {
            return WriteOutcome::WouldBlock;
        }
        self.send.push(&data[..n]);
        self.release_at = 0;
        WriteOutcome::Wrote(n)
    }

    /// Whether a `write` would not block (room in the send buffer, or a state in which `write`
    /// reports an error and the caller should find out).
    pub fn write_ready(&self, cfg: &Config) -> bool {
        match self.state {
            State::Closed | State::Lingering => true,
            _ => self.want_reset || self.shut || self.send.len() < cfg.send_buffer,
        }
    }

    /// Read received bytes in order.
    pub fn read(&mut self, cfg: &Config, now: u64, out: &mut [u8]) -> ReadOutcome {
        if !self.recv.is_empty() {
            if out.is_empty() {
                return ReadOutcome::Data(0);
            }
            let n = self.recv.read(out);
            // Reopen a closed-down window promptly instead of waiting for the next data packet.
            let thr = (cfg.recv_buffer as u32 / 2).min(2 * cfg.max_payload as u32);
            if self.last_adv < thr && self.free_window(cfg) >= thr {
                self.ack_now = true;
            }
            self.note_idle(now);
            return ReadOutcome::Data(n);
        }
        if self.remote_fin {
            return ReadOutcome::Eof;
        }
        match self.state {
            State::Closed => match self.end {
                End::Graceful => ReadOutcome::Eof,
                e => ReadOutcome::Err(e),
            },
            _ => ReadOutcome::WouldBlock,
        }
    }

    /// Bytes ready to read right now.
    pub fn readable_len(&self) -> usize {
        self.recv.len()
    }

    /// Half-close: send a FIN after all queued data; reading stays possible.
    pub fn shutdown_write(&mut self) {
        utp_trace!(self, "app shutdown_write {}", self.trace_line());
        self.shut = true;
    }

    /// The application dropped its handle: flush and FIN, discard anything the peer still sends.
    pub fn abandon(&mut self, cfg: &Config, now: u64) {
        utp_trace!(self, "app dropped stream {}", self.trace_line());
        self.shut = true;
        self.abandoned = true;
        self.recv = ByteQueue::new();
        self.reorder = Vec::new();
        self.reorder_bytes = 0;
        match self.state {
            State::SynSent | State::SynRecv => self.fail(End::Reset),
            _ => self.maybe_finish(now, cfg),
        }
    }

    /// Abort: send a RESET and close.
    pub fn abort(&mut self) {
        if self.state != State::Closed {
            self.want_reset = true;
        }
    }

    // ---- packet input -----------------------------------------------------------------------

    fn note_remote_timestamp(&mut self, now: u64, h: &Header) {
        let d = (now as u32).wrapping_sub(h.timestamp);
        self.reply_micro = if d == 0 { 1 } else { d };
    }

    /// Process one packet routed to this connection. `dgram_len` is the full datagram size.
    pub fn handle_packet(&mut self, cfg: &Config, now: u64, dgram_len: usize, pkt: &Packet<'_>) {
        let h = &pkt.header;
        if self.state == State::Closed {
            return;
        }
        self.last_recv = now;
        if self.state == State::SynRecv {
            self.rx_bytes = self.rx_bytes.saturating_add(dgram_len as u32);
        }
        match h.ty {
            PacketType::Reset => {
                self.fail(if self.state == State::SynSent { End::Refused } else { End::Reset });
                return;
            }
            PacketType::Syn => {
                // Retransmitted SYN: our STATE was lost. Re-send it (1:1, so no amplification).
                if !self.initiator && matches!(self.state, State::SynRecv | State::Connected) {
                    self.ack_now = true;
                }
                return;
            }
            _ => {}
        }
        match self.state {
            State::SynSent => {
                // Only the STATE acknowledging our SYN (seq 1) completes the handshake.
                if h.ty != PacketType::State || h.ack_nr != 1 {
                    return;
                }
                self.ack_nr = h.seq_nr.wrapping_sub(1);
                self.state = State::Connected;
                self.events |= EV_WRITE;
                // Confirm to the acceptor right away. It will not hand the connection to its
                // application (nor send data) until it has seen a packet acknowledging its STATE,
                // and we may have nothing to write for a while. Data, if any, rides on this ack.
                self.ack_now = true;
            }
            State::SynRecv => {
                // A packet acking our STATE (seq S, which consumed nothing, so ack S-1) proves the
                // sender really received it: the address is not spoofed.
                if h.ack_nr != self.seq_nr.wrapping_sub(1) {
                    return;
                }
                self.state = State::Connected;
                self.aux_deadline = 0;
                self.confirmed_event = true;
                self.events |= EV_WRITE;
            }
            State::Lingering => {
                if h.ty == PacketType::Fin {
                    self.ack_now = true; // our ack of their FIN was lost
                }
                return;
            }
            _ => {}
        }

        self.note_remote_timestamp(now, h);
        if crate::trace::on() {
            let (was, is) = ((self.peer_wnd as usize) < self.pay(), (h.wnd_size as usize) < self.pay());
            if was != is {
                utp_trace!(self, "peer window {} ({} -> {}) {}", if is { "CLOSED" } else { "open" }, self.peer_wnd, h.wnd_size, self.trace_line());
            }
        }
        self.peer_wnd = h.wnd_size;
        if h.timestamp_diff != 0 {
            self.delay.add(now, h.timestamp_diff);
        }
        if self.peer_wnd as usize >= self.pay() {
            self.persist_deadline = 0;
        }

        self.process_ack(cfg, now, h, pkt.sack);
        if self.state == State::Closed {
            return;
        }
        if matches!(h.ty, PacketType::Data | PacketType::Fin) {
            self.on_data(cfg, now, h, pkt.payload);
        }
        self.maybe_finish(now, cfg);
        self.note_idle(now);
    }

    fn process_ack(&mut self, cfg: &Config, now: u64, h: &Header, sack: Option<&[u8]>) {
        let n = self.inflight.len();
        let una = self.una();
        // Packets cumulatively acknowledged: ack_nr - (una - 1). Anything outside 0..=n is a stale
        // or forged ack and is ignored (it must never move our window).
        let d = h.ack_nr.wrapping_sub(una.wrapping_sub(1)) as usize;
        if d > n {
            return;
        }
        let mss = self.pay() as u32;
        let flight_before = self.cur_window;
        let mut acked: u32 = 0;
        let mut rtt_sample: Option<u64> = None;
        for _ in 0..d {
            let Some(e) = self.inflight.pop_front() else { break };
            let len = e.len as u32;
            if e.flags & F_COUNTED != 0 {
                self.cur_window = self.cur_window.saturating_sub(len);
            }
            if e.flags & F_SACKED == 0 {
                acked = acked.saturating_add(len);
                if e.flags & F_PROBE != 0 {
                    self.probe_ok(cfg, now);
                }
            }
            if e.tx == 1 && e.flags & F_NEEDS_TX == 0 {
                rtt_sample = Some(now.saturating_sub(e.sent_at));
            }
            self.inflight_bytes = self.inflight_bytes.saturating_sub(len);
            self.send.consume(len as usize);
            if e.flags & F_FIN != 0 {
                self.fin_acked = true;
            }
        }
        if d > 0 {
            self.rto_count = 0;
            self.rto_deadline = if self.inflight.is_empty() { 0 } else { now + self.rtt.rto() as u64 };
            self.events |= EV_WRITE;
            if self.in_recovery && seq::diff(h.ack_nr, self.recover) >= 0 {
                self.in_recovery = false;
            }
        }

        // Selective ack: bit i of the mask acknowledges ack_nr + 2 + i, i.e. inflight[i + 1]
        // (inflight[0] is ack_nr + 1, the hole the peer is waiting for).
        if let Some(mask) = sack {
            for off in packet::sack_offsets(mask) {
                let Some(e) = self.inflight.get_mut(off as usize + 1) else { break };
                if e.flags & F_SACKED != 0 {
                    continue;
                }
                let len = e.len as u32;
                if e.flags & F_COUNTED != 0 {
                    self.cur_window = self.cur_window.saturating_sub(len);
                }
                if e.tx == 1 && e.flags & F_NEEDS_TX == 0 {
                    rtt_sample = Some(now.saturating_sub(e.sent_at));
                }
                let probe = e.flags & F_PROBE != 0;
                e.flags = (e.flags | F_SACKED) & !(F_COUNTED | F_NEEDS_TX | F_FORCE | F_PROBE);
                acked = acked.saturating_add(len);
                if probe {
                    self.probe_ok(cfg, now);
                }
            }
        }

        if let Some(s) = rtt_sample {
            let s = s.clamp(1, u32::MAX as u64) as u32;
            self.rtt.sample(s, micros(cfg.min_rto) as u32, micros(cfg.max_rto) as u32);
        }

        // Loss detection: a packet with >= 3 selectively-acked packets after it is presumed lost.
        // Plain duplicate acks are deliberately ignored: a receiver with a real gap always
        // reports it through SACK, whereas a bare duplicate ack is what a duplicated or reordered
        // ack looks like. A packet already retransmitted is only re-presumed lost after two
        // smoothed RTTs, so one reordering burst cannot loop.
        let rtt2 = self.rtt.srtt() as u64 * 2;
        let mut lost = false;
        let mut probe_lost = false;
        if sack.is_some() {
            let mut sacked_after = 0u32;
            for i in (0..self.inflight.len()).rev() {
                let Some(e) = self.inflight.get_mut(i) else { continue };
                if e.flags & F_SACKED != 0 {
                    sacked_after += 1;
                    continue;
                }
                let eligible = e.flags & F_NEEDS_TX == 0
                    && e.flags & (F_SYN | F_FIN) == 0
                    && (e.tx <= 1 || now.saturating_sub(e.sent_at) > rtt2);
                if eligible && sacked_after >= 3 {
                    if e.flags & F_COUNTED != 0 {
                        self.cur_window = self.cur_window.saturating_sub(e.len as u32);
                    }
                    // A lost path-MTU probe says the datagram was too big, not that the
                    // network is congested: retransmit it, leave the window alone.
                    if e.flags & F_PROBE != 0 {
                        probe_lost = true;
                    } else {
                        lost = true;
                    }
                    e.flags = (e.flags | F_NEEDS_TX | F_FORCE) & !(F_COUNTED | F_PROBE);
                    self.pending_tx = true;
                }
            }
        }
        if probe_lost {
            self.probe_lost(cfg, now);
        }
        if lost && !self.in_recovery {
            utp_trace!(self, "fast-retransmit loss {}", self.trace_line());
            self.cc.on_loss(mss);
            self.in_recovery = true;
            self.recover = self.seq_nr.wrapping_sub(1);
        }
        if acked > 0 && !self.in_recovery && !lost {
            self.cc.on_ack(
                AckInfo {
                    acked_bytes: acked,
                    flight_before,
                    mss,
                    target: micros(cfg.target_delay).min(u32::MAX as u64) as u32,
                    max_window: cfg.max_window,
                },
                self.delay.our_delay(),
            );
        }
    }

    fn on_data(&mut self, cfg: &Config, now: u64, h: &Header, payload: &[u8]) {
        let fin = h.ty == PacketType::Fin;
        let delta = h.seq_nr.wrapping_sub(self.ack_nr);
        if delta == 0 || delta >= 0x8000 || self.remote_fin {
            self.ack_now = true; // duplicate or stale: re-ack so the sender stops retransmitting
            return;
        }
        let free = self.free_window(cfg);
        if delta == 1 {
            if payload.len() as u64 > free as u64 {
                self.ack_now = true; // sender overran our advertised window; it will retransmit
                return;
            }
            let had_gap = !self.reorder.is_empty();
            let gap = now.saturating_sub(self.last_data_rx);
            self.last_data_rx = now;
            self.accept_in_order(h.seq_nr, fin, payload);
            while self
                .reorder
                .first()
                .is_some_and(|r| r.seq == self.ack_nr.wrapping_add(1))
            {
                let held = self.reorder.remove(0);
                self.reorder_bytes = self.reorder_bytes.saturating_sub(held.data.len() as u32);
                self.accept_in_order(held.seq, held.fin, &held.data);
            }
            if fin || had_gap || !self.reorder.is_empty() {
                self.ack_now = true;
            } else {
                self.unacked_rx += 1;
                if self.unacked_rx >= 2 {
                    self.ack_now = true;
                } else if self.ack_deadline == 0 {
                    // Wait only as long as the arrival rate suggests a second packet is coming:
                    // 2x the last inter-arrival gap, within [1 ms, delayed_ack]. A fixed 25 ms
                    // stalls a window-limited sender on every odd-length burst (measured against
                    // libutp on loopback: 4x lower throughput).
                    let wait = gap.saturating_mul(2).clamp(1_000, micros(cfg.delayed_ack).max(1_000));
                    self.ack_deadline = now + wait;
                }
            }
        } else if (delta as usize) <= cfg.max_reorder_packets {
            self.ack_now = true; // out of order: ack immediately so the sender sees the gap
            let pos = self
                .reorder
                .partition_point(|r| r.seq.wrapping_sub(self.ack_nr) < delta);
            if self.reorder.get(pos).is_some_and(|r| r.seq == h.seq_nr) {
                return;
            }
            if self.reorder.len() >= cfg.max_reorder_packets || payload.len() as u64 > free as u64 {
                return;
            }
            if !self.abandoned || fin {
                self.reorder_bytes += payload.len() as u32;
                self.reorder.insert(
                    pos,
                    Held { seq: h.seq_nr, fin, data: payload.into() },
                );
            }
        } else {
            self.ack_now = true; // absurdly far ahead
        }
    }

    fn accept_in_order(&mut self, seq: u16, fin: bool, payload: &[u8]) {
        if fin {
            utp_trace!(self, "FIN received {}", self.trace_line());
        }
        if !self.abandoned && !payload.is_empty() {
            self.recv.push(payload);
            self.release_at = 0;
            self.events |= EV_READ;
        }
        self.ack_nr = seq;
        if fin {
            self.remote_fin = true;
            self.events |= EV_READ;
        }
    }

    fn maybe_finish(&mut self, now: u64, cfg: &Config) {
        if self.state == State::Connected && self.fin_acked && (self.remote_fin || self.abandoned) {
            utp_trace!(self, "lingering (remote_fin={} abandoned={}) {}", self.remote_fin, self.abandoned, self.trace_line());
            self.state = State::Lingering;
            self.aux_deadline = now + micros(cfg.linger);
            self.send = ByteQueue::new();
            self.inflight = VecDeque::new();
            self.inflight_bytes = 0;
            self.cur_window = 0;
            self.rto_deadline = 0;
            self.persist_deadline = 0;
            self.release_at = 0;
            self.events |= EV_READ | EV_WRITE;
        }
    }

    fn fail(&mut self, end: End) {
        if self.state == State::Closed {
            return;
        }
        utp_trace!(self, "CLOSED {:?} {}", end, self.trace_line());
        self.state = State::Closed;
        self.end = end;
        self.send = ByteQueue::new();
        if end != End::Graceful {
            // A clean close keeps unread data readable; a failure discards it like a RST.
            self.recv = ByteQueue::new();
        }
        self.reorder = Vec::new();
        self.reorder_bytes = 0;
        self.inflight = VecDeque::new();
        self.inflight_bytes = 0;
        self.cur_window = 0;
        self.rto_deadline = 0;
        self.ack_deadline = 0;
        self.aux_deadline = 0;
        self.persist_deadline = 0;
        self.release_at = 0;
        self.events |= EV_READ | EV_WRITE;
    }

    fn note_idle(&mut self, now: u64) {
        let idle = self.send.is_empty()
            && self.recv.is_empty()
            && self.inflight.is_empty()
            && self.reorder.is_empty();
        if idle && self.heap_bytes() > 0 {
            if self.release_at == 0 {
                self.release_at = now + 1_000_000;
            }
        } else {
            self.release_at = 0;
        }
    }

    // ---- packet output ----------------------------------------------------------------------

    fn build_sack(&self, out: &mut [u8; MAX_SACK_BYTES]) -> usize {
        let mut used = 0usize;
        for r in &self.reorder {
            let delta = r.seq.wrapping_sub(self.ack_nr) as usize;
            // delta >= 2 here: delta == 1 would have been delivered. Bit 0 is ack_nr + 2.
            let Some(bit) = delta.checked_sub(2) else { continue };
            if bit >= MAX_SACK_BYTES * 8 {
                break; // sorted ascending
            }
            out[bit / 8] |= 1 << (bit % 8);
            used = used.max(bit / 8 + 1);
        }
        used.div_ceil(4) * 4
    }

    fn header(&self, cfg: &Config, ty: PacketType, seq_nr: u16, now: u64) -> Header {
        Header {
            ty,
            conn_id: if ty == PacketType::Syn { self.recv_id } else { self.send_id },
            timestamp: now as u32,
            timestamp_diff: if ty == PacketType::Syn { 0 } else { self.reply_micro },
            wnd_size: self.free_window(cfg),
            seq_nr,
            ack_nr: if ty == PacketType::Syn { 0 } else { self.ack_nr },
        }
    }

    fn sent_something(&mut self, cfg: &Config, now: u64, bytes: usize) {
        self.last_sent = now;
        self.ack_now = false;
        self.ack_deadline = 0;
        self.unacked_rx = 0;
        self.last_adv = self.free_window(cfg);
        if self.state == State::SynRecv {
            self.tx_bytes = self.tx_bytes.saturating_add(bytes as u32);
        }
    }

    /// Serialise inflight entry `i` into `buf`.
    fn emit_entry(
        &mut self,
        cfg: &Config,
        now: u64,
        i: usize,
        start_hint: Option<usize>,
        buf: &mut [u8],
    ) -> Option<usize> {
        let e = *self.inflight.get(i)?;
        let ty = if e.flags & F_SYN != 0 {
            PacketType::Syn
        } else if e.flags & F_FIN != 0 {
            PacketType::Fin
        } else {
            PacketType::Data
        };
        let seq_nr = self.una().wrapping_add(i as u16);
        let hdr = self.header(cfg, ty, seq_nr, now);
        let mut sack = [0u8; MAX_SACK_BYTES];
        let sn = if ty == PacketType::Syn { 0 } else { self.build_sack(&mut sack) };
        let len = e.len as usize;
        let off = packet::encode(&hdr, &sack[..sn], buf)?;
        if len > 0 {
            // New data is always the last entry, whose offset is known; retransmissions (usually
            // the head) walk the prefix.
            let start = start_hint
                .unwrap_or_else(|| self.inflight.iter().take(i).map(|x| x.len as usize).sum());
            let dst = buf.get_mut(off..off + len)?;
            if !self.send.copy_at(start, dst) {
                return None;
            }
        }
        let e = self.inflight.get_mut(i)?;
        if e.tx > 0 {
            self.retransmits += 1;
        }
        // The probe flag and Don't-Fragment belong to the first transmission only; a
        // retransmission is plain (and the network may fragment it).
        let first_probe = e.tx == 0 && e.flags & F_PROBE != 0;
        e.tx = e.tx.saturating_add(1);
        e.sent_at = now;
        e.flags = (e.flags & !(F_NEEDS_TX | F_FORCE)) | F_COUNTED;
        // Retransmitted before anything told us the probe's fate (a go-back-N timeout): it was
        // lost as far as we know.
        let stale_probe = e.tx > 1 && e.flags & F_PROBE != 0;
        if e.tx > 1 {
            e.flags &= !F_PROBE;
        }
        self.df_out = first_probe;
        if stale_probe {
            self.probe_lost(cfg, now);
        }
        self.cur_window = self.cur_window.saturating_add(len as u32);
        if self.rto_deadline == 0 {
            self.rto_deadline = now + self.rtt.rto() as u64;
        }
        self.sent_something(cfg, now, off + len);
        Some(off + len)
    }

    fn emit_control(&mut self, cfg: &Config, now: u64, ty: PacketType, buf: &mut [u8]) -> Option<usize> {
        let hdr = self.header(cfg, ty, self.seq_nr, now);
        let mut sack = [0u8; MAX_SACK_BYTES];
        let sn = if ty == PacketType::State { self.build_sack(&mut sack) } else { 0 };
        let n = packet::encode(&hdr, &sack[..sn], buf)?;
        self.sent_something(cfg, now, n);
        Some(n)
    }

    /// Produce at most one packet into `buf` (>= `max_payload + 54` bytes). Call repeatedly until
    /// `None`; also call after any input, write, read or timeout.
    pub fn poll_transmit(&mut self, cfg: &Config, now: u64, buf: &mut [u8]) -> Option<usize> {
        match self.state {
            State::Closed => return None,
            State::SynRecv | State::Lingering => return self.poll_ack_only(cfg, now, buf),
            _ => {}
        }
        if self.want_reset {
            let n = self.emit_control(cfg, now, PacketType::Reset, buf);
            self.fail(End::Reset);
            return n;
        }
        // 1. SYN and retransmissions, in sequence order.
        let mss = self.pay() as u32;
        let mut pick = None;
        let mut pending = false;
        // `pending_tx` is set wherever a packet is marked for (re)transmission, so a steady
        // sender with nothing to retransmit skips the scan over its whole window per packet.
        let scan = if self.pending_tx { self.inflight.len() } else { 0 };
        for (i, e) in self.inflight.iter().enumerate().take(scan) {
            if e.flags & F_NEEDS_TX == 0 || e.flags & F_SACKED != 0 {
                continue;
            }
            pending = true;
            let gate = e.flags & (F_FORCE | F_SYN | F_FIN) != 0
                || self.cur_window == 0
                || self.cur_window as u64 + e.len as u64 <= self.cc.window() as u64;
            if gate {
                pick = Some(i);
            }
            break; // never skip the oldest pending packet
        }
        self.pending_tx = pending;
        if let Some(i) = pick {
            return self.emit_entry(cfg, now, i, None, buf);
        }
        if self.state == State::SynSent {
            return None; // data written early waits for the handshake
        }
        // 2. New data.
        let unsent = self.send.len().saturating_sub(self.inflight_bytes as usize);
        if unsent > 0 && self.inflight.len() < cfg.max_inflight_packets {
            let probing = std::mem::take(&mut self.probe);
            let limit = self.cc.window().min(self.peer_wnd) as u64;
            let normal = if probing { 1 } else { unsent.min(self.pay()) };
            // A full-size packet may become a path-MTU probe: real data, one packet bigger than
            // anything confirmed so far, sent once with Don't-Fragment. If it is lost it is
            // retransmitted as it is, without DF (the network fragments it), exactly like libutp.
            let mut want = normal;
            let mut probe_d = None;
            if !probing && cfg.mtu_discovery && normal == self.pay() {
                if let Some(d) = self.next_probe(now) {
                    // The probe datagram is exactly `d` bytes including the SACK extension it
                    // will carry, so what it proves is what a full later packet needs.
                    let mut tmp = [0u8; MAX_SACK_BYTES];
                    let p = d - packet::header_len(self.build_sack(&mut tmp));
                    if unsent >= p && self.cur_window as u64 + p as u64 <= limit {
                        want = p;
                        probe_d = Some(d);
                    }
                }
            }
            if probing || self.cur_window as u64 + want as u64 <= limit {
                self.persist_deadline = 0;
                self.inflight.push_back(Sent {
                    sent_at: 0,
                    len: want as u16,
                    flags: F_NEEDS_TX | if probe_d.is_some() { F_PROBE } else { 0 },
                    tx: 0,
                });
                if let Some(d) = probe_d {
                    self.probe_dgram = d as u16;
                }
                self.seq_nr = self.seq_nr.wrapping_add(1);
                let start = self.inflight_bytes as usize;
                self.inflight_bytes += want as u32;
                let i = self.inflight.len() - 1;
                let out = self.emit_entry(cfg, now, i, Some(start), buf);
                self.pending_tx |= out.is_none(); // flag left set: let the scan find it
                return out;
            }
            // Blocked. If the *peer's* window is why and nothing is in flight to elicit an ack,
            // schedule a window probe (otherwise a lost window-update would deadlock us).
            if self.cur_window == 0 && self.peer_wnd < mss && self.persist_deadline == 0 {
                let iv = (self.rtt.rto() as u64).clamp(1_000_000, 10_000_000);
                self.persist_deadline = now + iv;
            }
        }
        // 3. FIN once everything queued is on the wire.
        if self.shut
            && !self.fin_queued
            && unsent == 0
            && self.inflight.len() < cfg.max_inflight_packets
        {
            self.fin_queued = true;
            self.inflight.push_back(Sent { sent_at: 0, len: 0, flags: F_FIN | F_NEEDS_TX, tx: 0 });
            self.seq_nr = self.seq_nr.wrapping_add(1);
            let i = self.inflight.len() - 1;
            let out = self.emit_entry(cfg, now, i, Some(self.inflight_bytes as usize), buf);
            self.pending_tx |= out.is_none();
            return out;
        }
        // 4. Acks and keep-alives.
        self.poll_ack_only(cfg, now, buf)
    }

    fn poll_ack_only(&mut self, cfg: &Config, now: u64, buf: &mut [u8]) -> Option<usize> {
        let due = self.ack_now || (self.ack_deadline != 0 && now >= self.ack_deadline);
        let keepalive = self.state == State::Connected
            && now.saturating_sub(self.last_sent) >= micros(cfg.keepalive);
        if !(due || keepalive) {
            return None;
        }
        if self.state == State::SynRecv {
            // Anti-amplification: until the peer proves its address we answer at most 3x what it
            // sent us (a SYN-ACK is the same size as the SYN, so this only blocks floods).
            let cost = packet::header_len(0) as u32;
            if self.tx_bytes.saturating_add(cost) > self.rx_bytes.saturating_mul(3) {
                self.ack_now = false;
                self.ack_deadline = 0;
                return None;
            }
        }
        self.emit_control(cfg, now, PacketType::State, buf)
    }

    /// Cheap, conservative test used by the endpoint to decide whether to poll this connection:
    /// false positives cost one empty `poll_transmit`, false negatives would stall, so every
    /// condition under which `poll_transmit` could produce a packet is covered.
    pub fn wants_transmit(&self, cfg: &Config, now: u64) -> bool {
        match self.state {
            State::Closed => false,
            State::SynRecv | State::Lingering => {
                self.ack_now || (self.ack_deadline != 0 && now >= self.ack_deadline)
            }
            _ => {
                self.want_reset
                    || self.ack_now
                    || self.pending_tx
                    || self.probe
                    || (self.ack_deadline != 0 && now >= self.ack_deadline)
                    || (self.state == State::Connected
                        && (self.send.len() > self.inflight_bytes as usize
                            || (self.shut && !self.fin_queued)
                            || now.saturating_sub(self.last_sent) >= micros(cfg.keepalive)))
            }
        }
    }

    // ---- timers -----------------------------------------------------------------------------

    /// Earliest instant [`handle_timeout`](Self::handle_timeout) needs to run, if any.
    pub fn poll_timeout(&self, cfg: &Config) -> Option<u64> {
        let mut t = u64::MAX;
        let mut take = |v: u64| {
            if v != 0 && v < t {
                t = v;
            }
        };
        match self.state {
            State::Closed => return None,
            State::Lingering | State::SynRecv => take(self.aux_deadline),
            State::SynSent | State::Connected => {
                if !self.inflight.is_empty() {
                    take(self.rto_deadline);
                }
                take(self.persist_deadline);
                if self.state == State::Connected {
                    take(self.last_sent.saturating_add(micros(cfg.keepalive)));
                    if let Some(idle) = cfg.idle_timeout {
                        take(self.last_recv.saturating_add(micros(idle)));
                    }
                }
            }
        }
        take(self.ack_deadline);
        take(self.release_at);
        (t != u64::MAX).then_some(t)
    }

    /// Run whatever timers are due at `now`.
    pub fn handle_timeout(&mut self, cfg: &Config, now: u64) {
        match self.state {
            State::Closed => return,
            State::Lingering => {
                if self.aux_deadline != 0 && now >= self.aux_deadline {
                    self.fail(End::Graceful);
                }
                return;
            }
            State::SynRecv => {
                if self.aux_deadline != 0 && now >= self.aux_deadline {
                    self.fail(End::TimedOut);
                }
                return;
            }
            _ => {}
        }
        if self.state == State::Connected {
            if let Some(idle) = cfg.idle_timeout {
                if now.saturating_sub(self.last_recv) >= micros(idle) {
                    self.fail(End::TimedOut);
                    return;
                }
            }
        }
        if self.rto_deadline != 0 && now >= self.rto_deadline && !self.inflight.is_empty() {
            self.on_rto(cfg, now);
            if self.state == State::Closed {
                return;
            }
        }
        if self.persist_deadline != 0 && now >= self.persist_deadline {
            self.persist_deadline = 0;
            self.probe = true;
            utp_trace!(self, "persist probe {}", self.trace_line());
        }
        if self.release_at != 0 && now >= self.release_at {
            self.release_at = 0;
            self.send.release_if_empty();
            self.recv.release_if_empty();
            if self.inflight.is_empty() {
                self.inflight = VecDeque::new();
            }
            if self.reorder.is_empty() {
                self.reorder = Vec::new();
            }
        }
        self.note_idle(now);
    }

    fn on_rto(&mut self, cfg: &Config, now: u64) {
        let syn = self.state == State::SynSent;
        utp_trace!(self, "RTO #{} {}", self.rto_count + 1, self.trace_line());
        // The oldest unacknowledged packet is a path-MTU probe: the likeliest explanation is that
        // the datagram was too big for the path. Retransmit it plain; this is no congestion and no
        // sign of a dead peer, so the window, the RTO backoff and the retry budget stay as they are.
        if let Some(e) = self.inflight.front_mut().filter(|e| e.flags & F_PROBE != 0) {
            if e.flags & F_COUNTED != 0 {
                self.cur_window = self.cur_window.saturating_sub(e.len as u32);
            }
            e.flags = (e.flags | F_NEEDS_TX | F_FORCE) & !(F_COUNTED | F_PROBE);
            self.pending_tx = true;
            self.rto_deadline = now + self.rtt.rto() as u64;
            self.probe_lost(cfg, now);
            return;
        }
        // A peer that keeps answering with a closed window is alive, just slow to read: its probe
        // timeouts must not kill the connection.
        let peer_blocked = !syn
            && (self.peer_wnd as usize) < self.pay()
            && now.saturating_sub(self.last_recv) < 60_000_000;
        if !peer_blocked {
            self.rto_count = self.rto_count.saturating_add(1);
        }
        let limit = if syn { cfg.syn_retries } else { cfg.max_retransmits };
        if self.rto_count > limit {
            self.fail(End::TimedOut);
            return;
        }
        self.timeouts += 1;
        self.rtt.backoff(micros(cfg.max_rto).min(u32::MAX as u64) as u32);
        for e in self.inflight.iter_mut() {
            if e.flags & F_SACKED == 0 {
                e.flags = (e.flags | F_NEEDS_TX) & !(F_COUNTED | F_FORCE);
            }
        }
        self.cur_window = 0;
        self.pending_tx = true;
        if !syn {
            self.cc.on_timeout(self.pay() as u32);
        }
        self.in_recovery = false;
        self.rto_deadline = now + self.rtt.rto() as u64;
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the "bytes per idle connection" budget documented in the README.
    #[test]
    fn struct_sizes_stay_within_budget() {
        let c = std::mem::size_of::<Connection>();
        println!(
            "size_of Connection = {c}, Sent = {}, Held = {}, Ledbat = {}, DelayFilter = {}",
            std::mem::size_of::<Sent>(),
            std::mem::size_of::<Held>(),
            std::mem::size_of::<Ledbat>(),
            std::mem::size_of::<DelayFilter>()
        );
        assert!(c <= 400, "Connection grew to {c} bytes");
        assert_eq!(std::mem::size_of::<Sent>(), 16);
    }

    #[test]
    fn idle_connection_owns_no_heap_after_release() {
        let cfg = Config::default();
        let mut c = Connection::connect(&cfg, 1_000_000, 10);
        let mut buf = vec![0u8; 2048];
        assert!(c.poll_transmit(&cfg, 1_000_000, &mut buf).is_some()); // SYN
        // Handshake completes.
        let state = Header {
            ty: PacketType::State,
            conn_id: 10,
            timestamp: 1,
            timestamp_diff: 0,
            wnd_size: 1 << 20,
            seq_nr: 50,
            ack_nr: 1,
        };
        c.handle_packet(&cfg, 1_050_000, 20, &Packet { header: state, sack: None, payload: &[] });
        assert_eq!(c.state(), State::Connected);
        // Use it, then let it go idle.
        assert_eq!(c.write(&cfg, &[1u8; 3000]), WriteOutcome::Wrote(3000));
        assert!(c.heap_bytes() > 0);
        let mut now = 1_050_000;
        while let Some(n) = c.poll_transmit(&cfg, now, &mut buf) {
            assert!(n > 0);
        }
        let ack = Header { ack_nr: 4, seq_nr: 50, ..state };
        now += 60_000;
        c.handle_packet(&cfg, now, 20, &Packet { header: ack, sack: None, payload: &[] });
        assert_eq!(c.stats().in_flight, 0);
        let t = c.poll_timeout(&cfg).unwrap();
        // the first timer is the buffer-release timer, one second out
        assert!(t <= now + 1_000_000);
        c.handle_timeout(&cfg, t);
        assert_eq!(c.heap_bytes(), 0, "idle connection must hold no buffers");
    }

    #[test]
    fn nothing_to_say_means_no_transmit() {
        let cfg = Config::default();
        let mut c = Connection::connect(&cfg, 1_000_000, 10);
        let mut buf = vec![0u8; 2048];
        let _ = c.poll_transmit(&cfg, 1_000_000, &mut buf);
        assert!(c.poll_transmit(&cfg, 1_000_000, &mut buf).is_none());
        assert!(!c.wants_transmit(&cfg, 1_000_001));
        assert!(c.poll_transmit(&cfg, 1_000_001, &mut buf).is_none());
    }
}
