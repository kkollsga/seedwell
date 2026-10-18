//! Deterministic virtual-time network of `DhtCore`s. No sockets, no threads, no wall clock.

#![allow(dead_code)]

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::time::{Duration, Instant};
use trs_dht::rng::Rng;
use trs_dht::{Config, DhtCore, Event, Id, Input, Intent, PublicInfoHash};

#[derive(Debug)]
enum Ev {
    Deliver {
        to: usize,
        from: SocketAddrV4,
        data: Vec<u8>,
    },
    Tick(usize),
}

pub struct Sim {
    pub epoch: Instant,
    pub now_ms: u64,
    pub nodes: Vec<DhtCore>,
    pub addrs: Vec<SocketAddrV4>,
    index: HashMap<SocketAddrV4, usize>,
    queue: BinaryHeap<Reverse<(u64, u64, usize)>>,
    evs: HashMap<usize, Ev>,
    seq: u64,
    wake_at: Vec<Option<u64>>,
    pub dead: HashSet<usize>,
    pub loss_pct: u64,
    rng: Rng,
    pub events: Vec<(u64, usize, Event)>,
    pub packets: u64,
    pub started: Vec<bool>,
    pub seeds: Vec<usize>,
    /// FNV-1a over every delivered datagram, for determinism checks.
    pub transcript: u64,
    pub queries_by_node: Vec<u64>,
}

pub fn node_addr(k: usize) -> SocketAddrV4 {
    let k = k as u32 + 1;
    SocketAddrV4::new(
        Ipv4Addr::new(
            11 + (k >> 16) as u8,
            (k >> 8) as u8,
            k as u8,
            1 + (k % 200) as u8,
        ),
        6881,
    )
}

impl Sim {
    /// Creates `n` nodes (not started). `cfg` customises the shared config.
    pub fn new(n: usize, seed: u64, cfg: impl Fn(usize) -> Config) -> Self {
        let epoch = Instant::now();
        let mut rng = Rng::from_seed(seed);
        let mut nodes = Vec::new();
        let mut addrs = Vec::new();
        let mut index = HashMap::new();
        for k in 0..n {
            let a = node_addr(k);
            let mut c = cfg(k);
            c.seed = Some(seed.wrapping_mul(1_000_003).wrapping_add(k as u64));
            c.initial_node_id = Some(trs_dht::id::secure_id(*a.ip(), &mut rng));
            nodes.push(DhtCore::new(c, epoch));
            index.insert(a, k);
            addrs.push(a);
        }
        Self {
            epoch,
            now_ms: 0,
            nodes,
            addrs,
            index,
            queue: BinaryHeap::new(),
            evs: HashMap::new(),
            seq: 0,
            wake_at: vec![None; n],
            dead: HashSet::new(),
            loss_pct: 0,
            rng,
            events: Vec::new(),
            packets: 0,
            started: vec![false; n],
            seeds: vec![0, 1, 2],
            transcript: 0xcbf2_9ce4_8422_2325,
            queries_by_node: vec![0; n],
        }
    }

    fn push(&mut self, at: u64, ev: Ev) {
        self.seq += 1;
        self.evs.insert(self.seq as usize, ev);
        self.queue.push(Reverse((at, self.seq, self.seq as usize)));
    }

    fn fnv(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.transcript = (self.transcript ^ u64::from(b)).wrapping_mul(0x100_0000_01b3);
        }
    }

    fn latency(&mut self) -> u64 {
        20 + self.rng.below(180)
    }

    fn apply(&mut self, i: usize, intents: Vec<Intent>) {
        for it in intents {
            match it {
                Intent::Send { to, data } => {
                    self.packets += 1;
                    if self.loss_pct > 0 && self.rng.below(100) < self.loss_pct {
                        continue;
                    }
                    if let Some(&j) = self.index.get(&to) {
                        let lat = self.latency();
                        let from = self.addrs[i];
                        self.push(self.now_ms + lat, Ev::Deliver { to: j, from, data });
                    }
                }
                Intent::Event(Event::NeedBootstrap) => {
                    let seeds: Vec<SocketAddrV4> = self
                        .seeds
                        .iter()
                        .filter(|&&s| s != i)
                        .map(|&s| self.addrs[s])
                        .collect();
                    let now = self.epoch + Duration::from_millis(self.now_ms);
                    let out = self.nodes[i].handle(Input::Bootstrap(&seeds), now);
                    self.apply(i, out);
                }
                Intent::Event(e) => self.events.push((self.now_ms, i, e)),
            }
        }
        self.reschedule(i);
    }

    fn reschedule(&mut self, i: usize) {
        let w = self.nodes[i]
            .next_wakeup()
            .map(|t| t.saturating_duration_since(self.epoch).as_millis() as u64);
        if w != self.wake_at[i] {
            self.wake_at[i] = w;
            if let Some(t) = w {
                self.push(t.max(self.now_ms), Ev::Tick(i));
            }
        }
    }

    pub fn instant(&self) -> Instant {
        self.epoch + Duration::from_millis(self.now_ms)
    }

    /// Starts node `i` now.
    pub fn start(&mut self, i: usize) {
        self.started[i] = true;
        let now = self.instant();
        let out = self.nodes[i].handle(Input::Tick, now);
        self.apply(i, out);
    }

    /// Issues a command to node `i`.
    pub fn command(&mut self, i: usize, input: Input<'_>) {
        let now = self.instant();
        let out = self.nodes[i].handle(input, now);
        self.apply(i, out);
    }

    pub fn get_peers(&mut self, i: usize, ih: Id) {
        self.command(i, Input::GetPeers(PublicInfoHash::new(ih, false).unwrap()));
    }

    pub fn announce(&mut self, i: usize, ih: Id, port: u16) {
        self.command(
            i,
            Input::Announce {
                info_hash: PublicInfoHash::new(ih, false).unwrap(),
                port,
            },
        );
    }

    /// Runs until virtual time `until_ms` (events at exactly `until_ms` are processed).
    pub fn run_until(&mut self, until_ms: u64) {
        while let Some(&Reverse((t, _, key))) = self.queue.peek() {
            if t > until_ms {
                break;
            }
            self.queue.pop();
            self.now_ms = self.now_ms.max(t);
            let Some(ev) = self.evs.remove(&key) else {
                continue;
            };
            match ev {
                Ev::Deliver { to, from, data } => {
                    if self.dead.contains(&to) || !self.started[to] {
                        continue;
                    }
                    self.fnv(&data);
                    self.fnv(&to.to_le_bytes());
                    let now = self.instant();
                    let out = self.nodes[to].handle(Input::Packet { from, data: &data }, now);
                    self.apply(to, out);
                }
                Ev::Tick(i) => {
                    if self.wake_at[i] != Some(t) || self.dead.contains(&i) {
                        continue;
                    }
                    self.wake_at[i] = None;
                    let now = self.instant();
                    let out = self.nodes[i].handle(Input::Tick, now);
                    self.apply(i, out);
                }
            }
        }
        self.now_ms = self.now_ms.max(until_ms);
    }

    pub fn run_for(&mut self, ms: u64) {
        self.run_until(self.now_ms + ms);
    }

    /// Brings up all nodes, `gap_ms` apart, then lets things settle for `settle_ms`.
    pub fn join_all(&mut self, gap_ms: u64, settle_ms: u64) {
        for k in 0..self.nodes.len() {
            self.start(k);
            self.run_for(gap_ms);
        }
        self.run_for(settle_ms);
    }

    pub fn total_queries(&self) -> u64 {
        self.nodes.iter().map(|n| n.stats().queries_sent).sum()
    }

    pub fn avg_table(&self) -> f64 {
        self.nodes.iter().map(|n| n.table_len()).sum::<usize>() as f64 / self.nodes.len() as f64
    }

    /// All `Peers` events for `ih` delivered to node `i` at or after `since_ms`.
    pub fn peers_found(&self, i: usize, ih: &Id, since_ms: u64) -> HashSet<SocketAddrV4> {
        let mut s = HashSet::new();
        for (t, n, e) in &self.events {
            if *n == i && *t >= since_ms {
                if let Event::Peers { info_hash, peers } = e {
                    if info_hash == ih {
                        s.extend(peers.iter().copied());
                    }
                }
            }
        }
        s
    }

    pub fn finished(
        &self,
        i: usize,
        ih: &Id,
        since_ms: u64,
    ) -> Option<(u64, trs_dht::LookupStats)> {
        self.events.iter().find_map(|(t, n, e)| match e {
            Event::LookupFinished { info_hash, stats }
                if *n == i && info_hash == ih && *t >= since_ms =>
            {
                Some((*t, *stats))
            }
            _ => None,
        })
    }
}

pub fn sim_config() -> Config {
    Config {
        allow_local_addrs: false,
        ..Config::default()
    }
}
