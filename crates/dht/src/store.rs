//! Peers announced to us via `announce_peer`, with expiry and hard caps.
//!
//! Memory: one entry is 12 bytes (see `Peer`), so the default caps (4096 peers total) bound the
//! payload at 48 KiB, plus about 100 B of map/Vec overhead per info hash (256 hashes max).

use crate::id::Id;
use crate::rng::Rng;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddrV4};

/// Limits for the store.
#[derive(Clone, Copy, Debug)]
pub struct StoreLimits {
    /// Distinct info hashes held.
    pub max_infohashes: usize,
    /// Peers per info hash.
    pub max_peers_per_hash: usize,
    /// Peers overall.
    pub max_total_peers: usize,
    /// Entry lifetime in seconds.
    pub ttl_s: u32,
}

impl Default for StoreLimits {
    fn default() -> Self {
        Self {
            max_infohashes: 256,
            max_peers_per_hash: 100,
            max_total_peers: 4096,
            ttl_s: 30 * 60,
        }
    }
}

// size: 12 bytes (expiry is in seconds since core start)
#[derive(Clone, Copy, Debug)]
struct Peer {
    ip: [u8; 4],
    port: u16,
    expires: u32,
}

/// Bounded announce store.
#[derive(Debug, Default)]
pub struct PeerStore {
    map: HashMap<Id, Vec<Peer>>,
    total: usize,
    limits: StoreLimits,
}

impl PeerStore {
    /// Empty store with `limits`.
    pub fn new(limits: StoreLimits) -> Self {
        Self {
            map: HashMap::new(),
            total: 0,
            limits,
        }
    }

    /// Approximate bytes held: map slots (key + Vec header + control byte) plus Vec capacities.
    pub fn heap_bytes(&self) -> usize {
        size_of::<Self>()
            + self.map.capacity() * (size_of::<Id>() + size_of::<Vec<Peer>>() + 1)
            + self
                .map
                .values()
                .map(|v| v.capacity() * size_of::<Peer>())
                .sum::<usize>()
    }

    /// Number of stored peers (including not-yet-swept expired ones).
    pub fn total_peers(&self) -> usize {
        self.total
    }

    /// Number of info hashes held.
    pub fn infohashes(&self) -> usize {
        self.map.len()
    }

    /// Records an announce. Returns false if refused because of a cap.
    pub fn add(&mut self, info_hash: Id, addr: SocketAddrV4, now: u32) -> bool {
        let expires = now.saturating_add(self.limits.ttl_s);
        if !self.map.contains_key(&info_hash) {
            if self.map.len() >= self.limits.max_infohashes {
                self.sweep(now);
                if self.map.len() >= self.limits.max_infohashes {
                    return false;
                }
            }
            self.map.insert(info_hash, Vec::new());
        }
        let mut total = self.total;
        let list = self.map.entry(info_hash).or_default();
        let ip = addr.ip().octets();
        if let Some(p) = list
            .iter_mut()
            .find(|p| p.ip == ip && p.port == addr.port())
        {
            p.expires = expires;
            return true;
        }
        if list.len() >= self.limits.max_peers_per_hash {
            // Evict the entry closest to expiry (oldest announce) rather than refusing:
            // fresh announces are the most useful ones.
            if let Some(pos) = list
                .iter()
                .enumerate()
                .min_by_key(|(_, p)| p.expires)
                .map(|(i, _)| i)
            {
                list[pos] = Peer {
                    ip,
                    port: addr.port(),
                    expires,
                };
            }
            return true;
        }
        if total >= self.limits.max_total_peers {
            return false;
        }
        list.push(Peer {
            ip,
            port: addr.port(),
            expires,
        });
        total += 1;
        self.total = total;
        true
    }

    /// Up to `max` live peers for `info_hash`, starting at a random offset for fairness.
    pub fn get(&self, info_hash: &Id, now: u32, max: usize, rng: &mut Rng) -> Vec<SocketAddrV4> {
        let Some(list) = self.map.get(info_hash) else {
            return Vec::new();
        };
        let n = list.len();
        if n == 0 {
            return Vec::new();
        }
        let start = rng.below(n as u64) as usize;
        (0..n)
            .map(|i| &list[(start + i) % n])
            .filter(|p| p.expires > now)
            .take(max)
            .map(|p| SocketAddrV4::new(Ipv4Addr::from(p.ip), p.port))
            .collect()
    }

    /// Drops expired entries and empty hashes.
    pub fn sweep(&mut self, now: u32) {
        let mut total = 0;
        self.map.retain(|_, list| {
            list.retain(|p| p.expires > now);
            total += list.len();
            !list.is_empty()
        });
        self.total = total;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(1, 1, 1, n), 1000 + n as u16)
    }

    #[test]
    fn add_get_dedupe_expire() {
        let mut s = PeerStore::new(StoreLimits::default());
        let mut rng = Rng::from_seed(1);
        let h = [7u8; 20];
        assert!(s.add(h, a(1), 0));
        assert!(s.add(h, a(2), 0));
        assert!(s.add(h, a(1), 10));
        assert_eq!(s.total_peers(), 2);
        assert_eq!(s.get(&h, 5, 50, &mut rng).len(), 2);
        // a(2) expires at 1800; a(1) was refreshed at t=10 and lives to 1810.
        assert_eq!(s.get(&h, 1805, 50, &mut rng), vec![a(1)]);
        assert!(s.get(&h, 1811, 50, &mut rng).is_empty());
        s.sweep(1811);
        assert_eq!(s.total_peers(), 0);
        assert_eq!(s.infohashes(), 0);
    }

    #[test]
    fn caps_hold() {
        let lim = StoreLimits {
            max_infohashes: 3,
            max_peers_per_hash: 4,
            max_total_peers: 10,
            ttl_s: 100,
        };
        let mut s = PeerStore::new(lim);
        for h in 0..3u8 {
            for p in 0..10u8 {
                let _ = s.add([h; 20], a(p), 0);
            }
        }
        // per-hash cap 4 -> 12 peers wanted, but total 10... per-hash caps apply first
        assert!(s.total_peers() <= 10);
        assert!(s.map.values().all(|l| l.len() <= 4));
        // 4th info hash refused while others are live.
        assert!(!s.add([9; 20], a(1), 0));
        // After expiry there's room again.
        assert!(s.add([9; 20], a(1), 101));
        assert_eq!(s.infohashes(), 1);
    }

    #[test]
    fn eviction_prefers_oldest_when_hash_full() {
        let lim = StoreLimits {
            max_infohashes: 3,
            max_peers_per_hash: 2,
            max_total_peers: 10,
            ttl_s: 100,
        };
        let mut s = PeerStore::new(lim);
        let h = [1u8; 20];
        s.add(h, a(1), 0);
        s.add(h, a(2), 10);
        s.add(h, a(3), 20);
        let mut rng = Rng::from_seed(1);
        let got = s.get(&h, 25, 10, &mut rng);
        assert_eq!(got.len(), 2);
        assert!(!got.contains(&a(1)));
    }

    #[test]
    fn memory_at_default_caps_is_bounded() {
        let mut s = PeerStore::new(StoreLimits::default());
        let mut n = 0u32;
        'outer: for h in 0..256u32 {
            let mut ih = [0u8; 20];
            ih[..4].copy_from_slice(&h.to_be_bytes());
            for p in 0..100u32 {
                let ip = Ipv4Addr::from(0x0b00_0000 + n);
                n += 1;
                if !s.add(ih, SocketAddrV4::new(ip, 1000 + p as u16), 0) {
                    break 'outer;
                }
            }
        }
        assert_eq!(s.total_peers(), 4096);
        eprintln!(
            "full store: {} peers in {} hashes = {} bytes",
            s.total_peers(),
            s.infohashes(),
            s.heap_bytes()
        );
        assert!(s.heap_bytes() < 128 * 1024);
    }

    #[test]
    fn get_caps_result_size() {
        let mut s = PeerStore::new(StoreLimits::default());
        let h = [3u8; 20];
        for p in 0..100u8 {
            s.add(h, a(p), 0);
        }
        let mut rng = Rng::from_seed(1);
        assert_eq!(s.get(&h, 1, 50, &mut rng).len(), 50);
    }
}
