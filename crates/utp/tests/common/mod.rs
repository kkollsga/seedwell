//! Deterministic network simulator with virtual time, driving two sans-I/O [`Endpoint`]s.
//!
//! Everything is single-threaded and seeded: a failing scenario reproduces exactly. The link
//! model has a finite-rate FIFO bottleneck with a byte-limited queue (tail drop), propagation
//! delay, jitter, random loss, reordering, duplication and blackout windows. A Reno-like TCP flow
//! can share the same bottleneck to check that LEDBAT yields to it.
#![allow(dead_code)]

use std::cmp::Reverse;
use std::collections::{BTreeSet, BinaryHeap, HashMap, VecDeque};
use std::net::SocketAddr;

use trs_utp::rng::Rng;
use trs_utp::{Config, ConnHandle, ConnStatus, Disposition, Endpoint};

pub const START: u64 = 1_000_000;

pub fn addr(last: u8, port: u16) -> SocketAddr {
    SocketAddr::from(([10, 0, 0, last], port))
}

pub fn pseudo_data(seed: u64, n: usize) -> Vec<u8> {
    let mut r = Rng::new(seed);
    let mut v = Vec::with_capacity(n + 8);
    while v.len() < n {
        v.extend_from_slice(&r.next_u64().to_le_bytes());
    }
    v.truncate(n);
    v
}

#[derive(Clone, Debug)]
pub struct LinkCfg {
    /// Bits per second; 0 = unlimited.
    pub rate_bps: u64,
    pub delay_us: u64,
    pub jitter_us: u64,
    pub loss: f64,
    pub reorder: f64,
    pub reorder_extra_us: u64,
    pub dup: f64,
    pub queue_bytes: usize,
    /// Largest UDP payload the path carries (a smaller MTU with DF set); bigger datagrams vanish.
    /// 0 = unlimited.
    pub max_dgram: usize,
}

impl LinkCfg {
    pub fn clean(rate_bps: u64, delay_us: u64, queue_bytes: usize) -> Self {
        LinkCfg {
            rate_bps,
            delay_us,
            jitter_us: 0,
            loss: 0.0,
            reorder: 0.0,
            reorder_extra_us: 0,
            dup: 0.0,
            queue_bytes,
            max_dgram: 0,
        }
    }
}

pub struct Link {
    pub cfg: LinkCfg,
    busy_until: u64,
    pub dropped_queue: u64,
    pub dropped_loss: u64,
    pub dropped_mtu: u64,
    pub delivered: u64,
    pub bytes: u64,
    last_arrival: u64,
}

impl Link {
    fn new(cfg: LinkCfg) -> Self {
        Link { cfg, busy_until: 0, dropped_queue: 0, dropped_loss: 0, dropped_mtu: 0, delivered: 0, bytes: 0, last_arrival: 0 }
    }

    /// Queuing delay a packet sent now would see, microseconds.
    pub fn queue_delay(&self, now: u64) -> u64 {
        self.busy_until.saturating_sub(now)
    }

    fn send(&mut self, now: u64, size: usize, df: bool, rng: &mut Rng, blackout: bool) -> [Option<u64>; 2] {
        let c = self.cfg.clone();
        // Over the path MTU: dropped when DF is set, otherwise fragmented (and delivered).
        if df && c.max_dgram > 0 && size > c.max_dgram + 28 {
            self.dropped_mtu += 1;
            return [None, None];
        }
        let depart = if c.rate_bps > 0 {
            let backlog = self.busy_until.saturating_sub(now);
            let backlog_bytes = backlog * c.rate_bps / 8 / 1_000_000;
            if backlog_bytes as usize + size > c.queue_bytes {
                self.dropped_queue += 1;
                return [None, None];
            }
            let start = now.max(self.busy_until);
            self.busy_until = start + size as u64 * 8 * 1_000_000 / c.rate_bps;
            self.busy_until
        } else {
            now
        };
        let f = |r: &mut Rng| (r.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
        if blackout || f(rng) < c.loss {
            self.dropped_loss += 1;
            return [None, None];
        }
        let mut at = depart + c.delay_us;
        if c.jitter_us > 0 {
            // Jitter alone never reorders (a FIFO path); explicit `reorder` does that.
            at += (f(rng) * c.jitter_us as f64) as u64;
            at = at.max(self.last_arrival + 1);
        }
        self.last_arrival = at;
        if f(rng) < c.reorder {
            at += c.reorder_extra_us;
        }
        self.delivered += 1;
        self.bytes += size as u64;
        let dup = if f(rng) < c.dup { Some(at + 1 + (f(rng) * 2000.0) as u64) } else { None };
        [Some(at), dup]
    }
}

#[derive(Clone)]
pub enum Pkt {
    Utp(Vec<u8>),
    TcpData(u32),
    TcpAck(u32),
}

struct Ev {
    at: u64,
    id: u64,
    to_b: bool,
    pkt: Pkt,
}

/// Reno-like bulk TCP sender at A, receiver at B (packets are 1500 B, acks 60 B).
pub struct TcpFlow {
    pub active: bool,
    cwnd: f64,
    ssthresh: f64,
    next_seq: u32,
    snd_una: u32,
    dup: u32,
    recover: u32,
    in_recovery: bool,
    rto_at: Option<u64>,
    rto: u64,
    srtt: f64,
    sent_at: HashMap<u32, u64>,
    retx: std::collections::HashSet<u32>,
    rcv_next: u32,
    ooo: BTreeSet<u32>,
    pub delivered_pkts: u64,
    /// Data packets that reached the receiver (bottleneck throughput, in-order or not).
    pub arrived: u64,
    /// Receiver window cap in packets (models a typical rwnd-limited flow).
    pub rwnd: f64,
    pub cwnd_log: Vec<(u64, f64)>,
}

pub const TCP_PKT: usize = 1500;

impl TcpFlow {
    fn new() -> Self {
        TcpFlow {
            active: false,
            cwnd: 4.0,
            ssthresh: 1e9,
            next_seq: 0,
            snd_una: 0,
            dup: 0,
            recover: 0,
            in_recovery: false,
            rto_at: None,
            rto: 1_000_000,
            srtt: 0.0,
            sent_at: HashMap::new(),
            retx: Default::default(),
            rcv_next: 0,
            ooo: BTreeSet::new(),
            delivered_pkts: 0,
            arrived: 0,
            rwnd: 100.0,
            cwnd_log: Vec::new(),
        }
    }
}

pub struct Sim {
    pub now: u64,
    pub a: Endpoint,
    pub b: Endpoint,
    pub addr_a: SocketAddr,
    pub addr_b: SocketAddr,
    pub ab: Link,
    pub ba: Link,
    pub rng: Rng,
    heap: BinaryHeap<Reverse<(u64, u64)>>,
    evs: HashMap<u64, Ev>,
    next_id: u64,
    pub blackout: Option<(u64, u64)>,
    pub tcp: TcpFlow,
    wakes: BTreeSet<u64>,
    pub utp_bytes_ab: u64,
    pub foreign_seen: u64,
    buf: Vec<u8>,
}

impl Sim {
    pub fn new(cfg: Config, ab: LinkCfg, ba: LinkCfg, seed: u64) -> Self {
        Self::new2(cfg.clone(), cfg, ab, ba, seed)
    }

    pub fn new2(cfg_a: Config, cfg_b: Config, ab: LinkCfg, ba: LinkCfg, seed: u64) -> Self {
        let a = Endpoint::new(cfg_a, seed ^ 0xA);
        let b = Endpoint::new(cfg_b, seed ^ 0xB);
        let buf = vec![0u8; a.transmit_buffer_len().max(b.transmit_buffer_len())];
        Sim {
            now: START,
            a,
            b,
            addr_a: addr(1, 6881),
            addr_b: addr(2, 6881),
            ab: Link::new(ab),
            ba: Link::new(ba),
            rng: Rng::new(seed),
            heap: BinaryHeap::new(),
            evs: HashMap::new(),
            next_id: 0,
            blackout: None,
            tcp: TcpFlow::new(),
            wakes: BTreeSet::new(),
            utp_bytes_ab: 0,
            foreign_seen: 0,
            buf,
        }
    }

    pub fn wake_at(&mut self, t: u64) {
        self.wakes.insert(t);
    }

    fn in_blackout(&self) -> bool {
        self.blackout.is_some_and(|(s, e)| self.now >= s && self.now < e)
    }

    fn push(&mut self, at: u64, to_b: bool, pkt: Pkt) {
        let id = self.next_id;
        self.next_id += 1;
        self.heap.push(Reverse((at, id)));
        self.evs.insert(id, Ev { at, id, to_b, pkt });
    }

    fn inject(&mut self, to_b: bool, pkt: Pkt, size: usize, df: bool) {
        let black = self.in_blackout();
        let now = self.now;
        let times = if to_b {
            self.ab.send(now, size + 28, df, &mut self.rng, black)
        } else {
            self.ba.send(now, size + 28, df, &mut self.rng, black)
        };
        for t in times.into_iter().flatten() {
            self.push(t, to_b, pkt.clone());
        }
    }

    // ---- TCP model ----------------------------------------------------------------------

    fn tcp_fill(&mut self) {
        while self.tcp.active && ((self.tcp.next_seq - self.tcp.snd_una) as f64) < self.tcp.cwnd.min(self.tcp.rwnd) {
            let s = self.tcp.next_seq;
            self.tcp.next_seq += 1;
            self.tcp.sent_at.insert(s, self.now);
            self.inject(true, Pkt::TcpData(s), TCP_PKT - 28, false);
        }
        if self.tcp.rto_at.is_none() && self.tcp.next_seq > self.tcp.snd_una {
            self.tcp.rto_at = Some(self.now + self.tcp.rto);
        }
    }

    fn tcp_on_data(&mut self, seq: u32) {
        let t = &mut self.tcp;
        t.arrived += 1;
        if seq == t.rcv_next {
            t.rcv_next += 1;
            t.delivered_pkts += 1;
            while t.ooo.remove(&t.rcv_next) {
                t.rcv_next += 1;
                t.delivered_pkts += 1;
            }
        } else if seq > t.rcv_next {
            t.ooo.insert(seq);
        }
        let ack = t.rcv_next;
        self.inject(false, Pkt::TcpAck(ack), 60 - 28, false);
    }

    /// NewReno: fast retransmit/recovery with partial-ack handling, RTO with go-back-N.
    fn tcp_on_ack(&mut self, ack: u32) {
        let now = self.now;
        let mut retransmit = None;
        {
            let t = &mut self.tcp;
            if ack > t.snd_una {
                let newly = (ack - t.snd_una) as f64;
                if let Some(&st) = t.sent_at.get(&(ack - 1)) {
                    if !t.retx.contains(&(ack - 1)) {
                        let r = (now - st) as f64;
                        t.srtt = if t.srtt == 0.0 { r } else { 0.875 * t.srtt + 0.125 * r };
                        t.rto = ((t.srtt * 2.0) as u64).clamp(200_000, 60_000_000);
                    }
                }
                for s in t.snd_una..ack {
                    t.sent_at.remove(&s);
                    t.retx.remove(&s);
                }
                t.snd_una = ack;
                t.dup = 0;
                if t.in_recovery {
                    if ack > t.recover {
                        t.in_recovery = false;
                        t.cwnd = t.ssthresh;
                    } else {
                        // partial ack: the next hole is lost too
                        t.cwnd = (t.cwnd - newly + 1.0).max(1.0);
                        retransmit = Some(t.snd_una);
                    }
                } else if t.cwnd < t.ssthresh {
                    t.cwnd += newly;
                } else {
                    t.cwnd += newly / t.cwnd;
                }
                t.rto_at = if t.next_seq > t.snd_una { Some(now + t.rto) } else { None };
            } else if ack == t.snd_una && t.next_seq > t.snd_una {
                t.dup += 1;
                if t.in_recovery {
                    t.cwnd += 1.0;
                } else if t.dup == 3 {
                    let flight = (t.next_seq - t.snd_una) as f64;
                    t.ssthresh = (flight / 2.0).max(2.0);
                    t.cwnd = t.ssthresh + 3.0;
                    t.in_recovery = true;
                    t.recover = t.next_seq;
                    retransmit = Some(t.snd_una);
                }
            }
        }
        if let Some(s) = retransmit {
            self.tcp.retx.insert(s);
            self.inject(true, Pkt::TcpData(s), TCP_PKT - 28, false);
        }
        self.tcp.cwnd_log.push((now, self.tcp.cwnd));
        self.tcp_fill();
    }

    fn tcp_timer(&mut self) {
        let now = self.now;
        if let Some(t) = self.tcp.rto_at {
            if now >= t {
                let tf = &mut self.tcp;
                let flight = (tf.next_seq - tf.snd_una) as f64;
                tf.ssthresh = (flight / 2.0).max(2.0);
                tf.cwnd = 1.0;
                tf.in_recovery = false;
                tf.dup = 0;
                tf.rto = (tf.rto * 2).min(60_000_000);
                tf.rto_at = Some(now + tf.rto);
                for s in tf.snd_una..tf.next_seq {
                    tf.retx.insert(s);
                }
                tf.next_seq = tf.snd_una; // go-back-N
                self.tcp_fill();
            }
        }
    }

    pub fn start_tcp(&mut self) {
        self.tcp.active = true;
        self.tcp_fill();
    }

    // ---- main loop ----------------------------------------------------------------------

    fn deliver_due(&mut self) {
        while let Some(&Reverse((at, id))) = self.heap.peek() {
            if at > self.now {
                break;
            }
            self.heap.pop();
            let Some(ev) = self.evs.remove(&id) else { continue };
            match ev.pkt {
                Pkt::Utp(d) => {
                    let (ep, from) = if ev.to_b { (&mut self.b, self.addr_a) } else { (&mut self.a, self.addr_b) };
                    if ep.handle_datagram(self.now, from, &d) == Disposition::Foreign {
                        self.foreign_seen += 1;
                    }
                }
                Pkt::TcpData(s) => self.tcp_on_data(s),
                Pkt::TcpAck(a) => self.tcp_on_ack(a),
            }
        }
    }

    fn flush(&mut self) {
        let now = self.now;
        loop {
            let mut any = false;
            while let Some((to, n)) = self.a.poll_transmit(now, &mut self.buf) {
                debug_assert_eq!(to, self.addr_b);
                let d = self.buf[..n].to_vec();
                self.utp_bytes_ab += n as u64;
                let df = self.a.dont_fragment();
                self.inject(true, Pkt::Utp(d), n, df);
                any = true;
            }
            while let Some((to, n)) = self.b.poll_transmit(now, &mut self.buf) {
                debug_assert_eq!(to, self.addr_a);
                let d = self.buf[..n].to_vec();
                let df = self.b.dont_fragment();
                self.inject(false, Pkt::Utp(d), n, df);
                any = true;
            }
            if !any {
                break;
            }
        }
    }

    /// Run until `app` returns true or `deadline` (absolute virtual µs) passes. Returns whether
    /// `app` finished.
    pub fn run(&mut self, deadline: u64, app: &mut dyn FnMut(&mut Sim) -> bool) -> bool {
        loop {
            self.deliver_due();
            self.a.handle_timeout(self.now);
            self.b.handle_timeout(self.now);
            self.tcp_timer();
            while self.wakes.first().is_some_and(|&t| t <= self.now) {
                self.wakes.pop_first();
            }
            if app(self) {
                self.flush();
                return true;
            }
            self.flush();
            let mut next = u64::MAX;
            if let Some(&Reverse((at, _))) = self.heap.peek() {
                next = next.min(at);
            }
            if let Some(t) = self.a.poll_timeout() {
                next = next.min(t);
            }
            if let Some(t) = self.b.poll_timeout() {
                next = next.min(t);
            }
            if let Some(t) = self.tcp.rto_at {
                next = next.min(t);
            }
            if let Some(&t) = self.wakes.first() {
                next = next.min(t);
            }
            if next == u64::MAX || next > deadline {
                self.now = deadline.max(self.now);
                return false;
            }
            self.now = next.max(self.now + if next <= self.now { 1 } else { 0 });
            if self.now > deadline {
                return false;
            }
        }
    }

    pub fn run_for(&mut self, dur: u64, app: &mut dyn FnMut(&mut Sim) -> bool) -> bool {
        let d = self.now + dur;
        self.run(d, app)
    }
}

/// One direction of an application-level transfer.
pub struct Side {
    pub handle: Option<ConnHandle>,
    pub tx: Vec<u8>,
    pub pos: usize,
    pub rx: Vec<u8>,
    pub eof: bool,
    pub shut: bool,
    pub err: Option<std::io::ErrorKind>,
    pub done_at: u64,
    /// Reader refuses to read before this time (slow-consumer tests).
    pub read_pause_until: u64,
    /// Do not half-close automatically when `tx` is exhausted.
    pub hold_open: bool,
}

impl Side {
    pub fn new(tx: Vec<u8>) -> Self {
        Side {
            handle: None,
            tx,
            pos: 0,
            rx: Vec::new(),
            eof: false,
            shut: false,
            err: None,
            done_at: 0,
            read_pause_until: 0,
            hold_open: false,
        }
    }
}

pub fn pump(ep: &mut Endpoint, side: &mut Side, now: u64) {
    let Some(h) = side.handle else { return };
    while side.pos < side.tx.len() && side.err.is_none() {
        let end = (side.pos + 65_536).min(side.tx.len());
        match ep.write(h, now, &side.tx[side.pos..end]) {
            Ok(n) => side.pos += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => {
                side.err = Some(e.kind());
            }
        }
    }
    if side.pos == side.tx.len() && !side.shut && side.err.is_none() && !side.hold_open {
        ep.shutdown_write(h, now);
        side.shut = true;
    }
    if now < side.read_pause_until {
        return;
    }
    let mut buf = [0u8; 32 * 1024];
    while !side.eof && side.err.is_none() {
        match ep.read(h, now, &mut buf) {
            Ok(0) => {
                side.eof = true;
                side.done_at = now;
            }
            Ok(n) => side.rx.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => side.err = Some(e.kind()),
        }
    }
}

pub struct Transfer {
    pub a: Side,
    pub b: Side,
    pub start: u64,
}

impl Transfer {
    pub fn new(a_to_b: Vec<u8>, b_to_a: Vec<u8>) -> Self {
        Transfer { a: Side::new(a_to_b), b: Side::new(b_to_a), start: 0 }
    }

    /// App step: connect on first call, accept, pump both sides. True when both received EOF.
    pub fn step(&mut self, sim: &mut Sim) -> bool {
        if self.a.handle.is_none() {
            self.start = sim.now;
            self.a.handle = Some(sim.a.connect(sim.now, sim.addr_b).unwrap());
        }
        if self.b.handle.is_none() {
            self.b.handle = sim.b.poll_accept();
        }
        let now = sim.now;
        pump(&mut sim.a, &mut self.a, now);
        pump(&mut sim.b, &mut self.b, now);
        (self.a.eof || self.a.err.is_some()) && (self.b.eof || self.b.err.is_some())
    }

    pub fn close_both(&mut self, sim: &mut Sim) {
        if let Some(h) = self.a.handle {
            sim.a.close(h, sim.now);
        }
        if let Some(h) = self.b.handle {
            sim.b.close(h, sim.now);
        }
    }
}

pub fn status(sim: &Sim, h: ConnHandle) -> ConnStatus {
    sim.a.status(h)
}

pub fn queue_delays_us(samples: &[u64]) -> (u64, u64) {
    let mut v = samples.to_vec();
    v.sort_unstable();
    if v.is_empty() {
        return (0, 0);
    }
    (v[v.len() / 2], v[v.len() * 95 / 100])
}

pub struct _Unused(VecDeque<u8>);
