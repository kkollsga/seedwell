//! Choking (ENGINE.md 5) and demand-weighted upload slots across torrents (SPEC 5.1).
//!
//! * **Leeching**: tit-for-tat. The fastest uploaders to us get the regular slots; one optimistic
//!   slot rotates every 30 s to discover better partners.
//! * **Seeding**: the peers we upload to fastest keep the regular slots (so the pipe stays full);
//!   the optimistic slot goes to the peer that waited longest, so everyone gets a turn.
//! * **Across torrents**: [`UploadAllocator`] splits the global slot budget by
//!   `interested peers x priority weight`, so a popular torrent is served first and 190 cold ones
//!   cost nothing.
//!
//! Everything here is pure data in, decision out.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::api::TorrentId;
use crate::util::Rng;

/// How often the optimistic slot rotates.
pub const OPTIMISTIC_PERIOD: Duration = Duration::from_secs(30);

/// What the choker needs to know about one peer.
#[derive(Clone, Copy, Debug)]
pub struct PeerView {
    /// Peer key.
    pub key: u32,
    /// The peer wants data from us.
    pub interested: bool,
    /// We currently let it download.
    pub unchoked: bool,
    /// Bytes/s: received from the peer while leeching, sent to it while seeding.
    pub rate: u64,
    /// The peer stopped sending blocks we asked for.
    pub snubbed: bool,
    /// When we last unchoked it (None: never).
    pub last_unchoked: Option<Instant>,
}

/// Whether we still download.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Downloading: reward peers that upload to us.
    Leeching,
    /// Complete: favour the fastest takers, rotate for fairness.
    Seeding,
}

/// Choker state of one torrent.
// size: ~48 bytes
#[derive(Debug)]
pub struct Choker {
    optimistic: Option<u32>,
    rotated_at: Option<Instant>,
    rng: Rng,
}

impl Choker {
    /// New choker.
    pub fn new(seed: u64) -> Self {
        Self {
            optimistic: None,
            rotated_at: None,
            rng: Rng::seeded(seed),
        }
    }

    /// The peer currently holding the optimistic slot.
    pub fn optimistic(&self) -> Option<u32> {
        self.optimistic
    }

    /// Keys that should be unchoked after this round (everyone else is choked).
    pub fn rechoke(&mut self, peers: &[PeerView], slots: usize, mode: Mode, now: Instant) -> Vec<u32> {
        let mut interested: Vec<&PeerView> = peers.iter().filter(|p| p.interested).collect();
        if slots == 0 || interested.is_empty() {
            self.optimistic = None;
            return Vec::new();
        }
        let regular = slots.saturating_sub(1);
        // Fastest first; key as a stable tie-break so equal rates do not flap.
        interested.sort_by(|a, b| b.rate.cmp(&a.rate).then(a.key.cmp(&b.key)));
        let mut out: Vec<u32> = interested
            .iter()
            .filter(|p| !p.snubbed)
            .take(regular)
            .map(|p| p.key)
            .collect();

        let due = self
            .rotated_at
            .is_none_or(|t| now.saturating_duration_since(t) >= OPTIMISTIC_PERIOD);
        let valid = self
            .optimistic
            .is_some_and(|k| interested.iter().any(|p| p.key == k) && !out.contains(&k));
        if due || !valid {
            let pool: Vec<&&PeerView> = interested.iter().filter(|p| !out.contains(&p.key)).collect();
            self.optimistic = match mode {
                Mode::Leeching => {
                    // Prefer peers that are currently choked: unchoking a fresh one is the point.
                    let choked: Vec<&&&PeerView> = pool.iter().filter(|p| !p.unchoked).collect();
                    if choked.is_empty() {
                        let i = self.rng.below(pool.len() as u64) as usize;
                        pool.get(i).map(|p| p.key)
                    } else {
                        let i = self.rng.below(choked.len() as u64) as usize;
                        choked.get(i).map(|p| p.key)
                    }
                }
                Mode::Seeding => pool
                    .iter()
                    .min_by_key(|p| (p.last_unchoked, p.key))
                    .map(|p| p.key),
            };
            self.rotated_at = Some(now);
        }
        if let Some(k) = self.optimistic {
            if !out.contains(&k) {
                out.push(k);
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug)]
struct Demand {
    interested: u32,
    weight: u32,
    at: Instant,
}

/// Splits the global upload-slot budget between torrents by demand.
#[derive(Debug, Default)]
pub struct UploadAllocator {
    demands: Mutex<HashMap<TorrentId, Demand>>,
}

/// Demand older than this is ignored (the torrent went quiet or was demoted).
const DEMAND_TTL: Duration = Duration::from_secs(60);

impl UploadAllocator {
    /// Empty allocator.
    pub fn new() -> Self {
        Self::default()
    }

    /// A torrent reports how many peers want data from it.
    pub fn report(&self, id: TorrentId, interested: u32, weight: u32, now: Instant) {
        let mut g = self.demands.lock().unwrap_or_else(|p| p.into_inner());
        if interested == 0 {
            g.remove(&id);
        } else {
            g.insert(id, Demand { interested, weight, at: now });
        }
    }

    /// A torrent is gone.
    pub fn remove(&self, id: &TorrentId) {
        self.demands.lock().unwrap_or_else(|p| p.into_inner()).remove(id);
    }

    /// Slots for `id` out of `total`.
    pub fn slots_for(&self, id: &TorrentId, total: usize, now: Instant) -> usize {
        let mut g = self.demands.lock().unwrap_or_else(|p| p.into_inner());
        g.retain(|_, d| now.saturating_duration_since(d.at) < DEMAND_TTL);
        let mut ids: Vec<TorrentId> = g.keys().copied().collect();
        ids.sort_unstable();
        let Some(pos) = ids.iter().position(|k| k == id) else {
            return 0;
        };
        let demands: Vec<(u32, u32)> = ids
            .iter()
            .map(|k| g.get(k).map_or((0, 0), |d| (d.interested, d.weight)))
            .collect();
        allocate(total, &demands).get(pos).copied().unwrap_or(0)
    }
}

/// Distribute `total` slots over `(interested, weight)` demands: proportional to
/// `interested x weight`, at least one slot (two when possible) for every torrent that has
/// interested peers, never more than it has interested peers.
pub fn allocate(total: usize, demands: &[(u32, u32)]) -> Vec<usize> {
    let n = demands.len();
    let mut out = vec![0usize; n];
    let active: Vec<usize> = (0..n).filter(|&i| demands[i].0 > 0).collect();
    if active.is_empty() || total == 0 {
        return out;
    }
    let cap = |i: usize| demands[i].0 as usize;
    let score = |i: usize| u64::from(demands[i].0) * u64::from(demands[i].1.max(1));
    // Minimums: heaviest torrents first when slots are scarcer than torrents.
    let mut by_score = active.clone();
    by_score.sort_by_key(|&i| std::cmp::Reverse(score(i)));
    let mut left = total;
    for min in [1usize, 2] {
        for &i in &by_score {
            if left > 0 && out[i] < min.min(cap(i)) {
                out[i] += 1;
                left -= 1;
            }
        }
    }
    // Proportional rest, largest remainder, respecting caps.
    while left > 0 {
        let room: Vec<usize> = active.iter().copied().filter(|&i| out[i] < cap(i)).collect();
        if room.is_empty() {
            break;
        }
        let sum: u64 = room.iter().map(|&i| score(i)).sum();
        let mut given = 0usize;
        let mut rema: Vec<(u64, usize)> = Vec::new();
        for &i in &room {
            let exact = left as u64 * score(i);
            let want = ((exact / sum) as usize).min(cap(i) - out[i]);
            out[i] += want;
            given += want;
            rema.push((exact % sum, i));
        }
        left -= given;
        if given == 0 {
            rema.sort_by_key(|&(r, i)| (std::cmp::Reverse(r), i));
            for (_, i) in rema {
                if left > 0 && out[i] < cap(i) {
                    out[i] += 1;
                    left -= 1;
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pv(key: u32, interested: bool, rate: u64) -> PeerView {
        PeerView {
            key,
            interested,
            unchoked: false,
            rate,
            snubbed: false,
            last_unchoked: None,
        }
    }

    #[test]
    fn fastest_interested_peers_get_regular_slots() {
        let mut c = Choker::new(1);
        let peers = vec![pv(1, true, 100), pv(2, true, 900), pv(3, true, 500), pv(4, false, 9999), pv(5, true, 10)];
        let now = Instant::now();
        let out = c.rechoke(&peers, 3, Mode::Leeching, now);
        assert_eq!(out.len(), 3);
        assert!(out.contains(&2) && out.contains(&3), "two fastest are regular: {out:?}");
        assert!(!out.contains(&4), "uninterested never unchoked");
        let opt = c.optimistic().unwrap();
        assert!([1, 5].contains(&opt));
    }

    #[test]
    fn optimistic_slot_rotates_every_30_seconds() {
        let mut c = Choker::new(2);
        let peers: Vec<PeerView> = (1..=10).map(|k| pv(k, true, u64::from(k) * 10)).collect();
        let t0 = Instant::now();
        c.rechoke(&peers, 2, Mode::Leeching, t0);
        let first = c.optimistic().unwrap();
        c.rechoke(&peers, 2, Mode::Leeching, t0 + Duration::from_secs(29));
        assert_eq!(c.optimistic(), Some(first), "kept inside the period");
        let mut changed = false;
        for i in 1..20 {
            c.rechoke(&peers, 2, Mode::Leeching, t0 + Duration::from_secs(30 * i));
            changed |= c.optimistic() != Some(first);
        }
        assert!(changed, "rotation picks other peers over time");
    }

    #[test]
    fn optimistic_is_replaced_when_the_peer_loses_interest() {
        let mut c = Choker::new(3);
        let mut peers = vec![pv(1, true, 50), pv(2, true, 40), pv(3, true, 30)];
        let t0 = Instant::now();
        c.rechoke(&peers, 2, Mode::Leeching, t0);
        let opt = c.optimistic().unwrap();
        peers.iter_mut().find(|p| p.key == opt).unwrap().interested = false;
        let out = c.rechoke(&peers, 2, Mode::Leeching, t0 + Duration::from_secs(1));
        assert!(!out.contains(&opt));
        assert_ne!(c.optimistic(), Some(opt));
    }

    #[test]
    fn snubbed_peers_do_not_get_regular_slots() {
        let mut c = Choker::new(4);
        let mut peers = vec![pv(1, true, 1000), pv(2, true, 10)];
        peers[0].snubbed = true;
        let out = c.rechoke(&peers, 2, Mode::Leeching, Instant::now());
        // slot 1 regular: peer 2 (peer 1 is snubbed); optimistic: peer 1 may still be tried.
        assert!(out.contains(&2));
    }

    #[test]
    fn seeding_rotates_to_the_longest_waiting_peer() {
        let mut c = Choker::new(5);
        let t0 = Instant::now();
        let mut peers = vec![pv(1, true, 900), pv(2, true, 800), pv(3, true, 5), pv(4, true, 5)];
        peers[2].last_unchoked = Some(t0);
        // peer 4 was never unchoked: it gets the optimistic slot.
        let out = c.rechoke(&peers, 3, Mode::Seeding, t0 + Duration::from_secs(100));
        assert!(out.contains(&1) && out.contains(&2));
        assert_eq!(c.optimistic(), Some(4));
        peers[3].last_unchoked = Some(t0 + Duration::from_secs(100));
        c.rechoke(&peers, 3, Mode::Seeding, t0 + Duration::from_secs(131));
        assert_eq!(c.optimistic(), Some(3), "next in line after 30 s");
    }

    #[test]
    fn zero_slots_or_no_interest_unchokes_nobody() {
        let mut c = Choker::new(6);
        let peers = vec![pv(1, true, 1), pv(2, false, 2)];
        assert!(c.rechoke(&peers, 0, Mode::Seeding, Instant::now()).is_empty());
        assert!(c.rechoke(&[pv(1, false, 1)], 5, Mode::Seeding, Instant::now()).is_empty());
    }

    #[test]
    fn allocation_is_proportional_and_capped() {
        // total 40: A has 30 interested (weight 2), B 10 (weight 2), C 1 (weight 1).
        let a = allocate(40, &[(30, 2), (10, 2), (1, 1)]);
        assert_eq!(a.iter().sum::<usize>(), 40);
        assert!(a[0] > a[1] && a[1] >= 2, "{a:?}");
        assert_eq!(a[2], 1, "capped by its single interested peer");
        // never more slots than interested peers
        let b = allocate(100, &[(3, 2), (2, 2)]);
        assert_eq!(b, vec![3, 2]);
        // priority weights matter
        let c = allocate(10, &[(20, 4), (20, 1)]);
        assert!(c[0] > c[1], "{c:?}");
        // scarce slots go to the heaviest first
        let d = allocate(2, &[(1, 1), (1, 4), (1, 2)]);
        assert_eq!(d, vec![0, 1, 1]);
        // idle torrents get nothing
        assert_eq!(allocate(10, &[(0, 2), (4, 2)]), vec![0, 4]);
        assert_eq!(allocate(0, &[(4, 2)]), vec![0]);
    }

    #[test]
    fn allocator_follows_reports_and_expires_old_demand() {
        let a = UploadAllocator::new();
        let (x, y) = ([1u8; 20], [2u8; 20]);
        let t0 = Instant::now();
        assert_eq!(a.slots_for(&x, 10, t0), 0);
        a.report(x, 5, 2, t0);
        a.report(y, 5, 2, t0);
        assert_eq!(a.slots_for(&x, 10, t0) + a.slots_for(&y, 10, t0), 10);
        a.report(y, 0, 2, t0);
        assert_eq!(a.slots_for(&x, 10, t0), 5);
        assert_eq!(a.slots_for(&x, 10, t0 + Duration::from_secs(61)), 0, "stale demand expires");
    }
}
