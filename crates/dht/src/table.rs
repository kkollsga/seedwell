//! Kademlia routing table (BEP 5): k=8 buckets, split only on the bucket covering our own id.
//!
//! Layout: `buckets[i]` for `i < last` holds nodes whose common prefix length with our id is
//! exactly `i`; the last bucket holds everything with prefix length >= `last` and is the only
//! one that may split. Only nodes that have *answered us* are inserted, so every entry has been
//! verified to own its address (a spoofed source cannot get into the table).
//!
//! Memory: `size_of::<Node>() == 32`. A bucket is a `Vec<Node>` with capacity 8 (256 B) plus a
//! one-slot replacement cache (40 B) and a timestamp. The worst case is 160 buckets (the id
//! length) which is < 52 KiB; a realistic table (10M-node network) has ~20 buckets and ~150 nodes,
//! about 6 KiB. See `RoutingTable::worst_case_bytes` and its test.

use crate::id::{Id, bit, cmp_distance, common_prefix_len};
use crate::rng::Rng;
use std::net::SocketAddrV4;

/// Bucket size (BEP 5).
pub const K: usize = 8;
/// A node silent for this long is "questionable" (BEP 5).
pub const GOOD_WINDOW_S: u32 = 15 * 60;
/// Consecutive unanswered queries after which a node is "bad".
pub const BAD_FAILS: u8 = 2;

/// A verified contact.
// size: 32 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Node {
    /// Node id.
    pub id: Id,
    /// Seconds since core start at which we last heard from the node (reply or query).
    pub last_seen: u32,
    /// Contact address.
    pub addr: SocketAddrV4,
    /// Consecutive unanswered queries.
    pub fails: u8,
}

/// BEP 5 node liveness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Heard from within the last 15 minutes.
    Good,
    /// Silent for 15+ minutes but not failing.
    Questionable,
    /// Repeatedly failed to answer.
    Bad,
}

impl Node {
    /// Liveness at time `now` (seconds).
    pub fn status(&self, now: u32) -> Status {
        if self.fails >= BAD_FAILS {
            Status::Bad
        } else if now.saturating_sub(self.last_seen) < GOOD_WINDOW_S {
            Status::Good
        } else {
            Status::Questionable
        }
    }
}

#[derive(Clone, Debug)]
struct Bucket {
    nodes: Vec<Node>,
    repl: Option<Node>,
    changed: u32,
}

impl Bucket {
    fn new(changed: u32) -> Self {
        Self {
            nodes: Vec::with_capacity(K),
            repl: None,
            changed,
        }
    }
}

/// Result of [`RoutingTable::insert`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Insert {
    /// New node stored.
    Added,
    /// Known node refreshed.
    Updated,
    /// Replaced a bad node.
    Replaced,
    /// Bucket is full of questionable nodes: ping `stale`; the candidate waits in the replacement slot.
    Verify(Node),
    /// Not stored (own id, full bucket of good nodes, duplicate IP, conflicting address).
    Rejected,
}

/// The routing table.
#[derive(Clone, Debug)]
pub struct RoutingTable {
    own: Id,
    buckets: Vec<Bucket>,
    unique_ip: bool,
}

impl RoutingTable {
    /// Empty table for `own`, created at `now` (seconds).
    pub fn new(own: Id, now: u32) -> Self {
        Self {
            own,
            buckets: vec![Bucket::new(now)],
            unique_ip: true,
        }
    }

    /// Whether at most one node per IP address is stored (default true). Disable only for
    /// loopback tests where many nodes share an address.
    pub fn set_unique_ip(&mut self, on: bool) {
        self.unique_ip = on;
    }

    /// Our node id.
    pub fn own_id(&self) -> &Id {
        &self.own
    }

    /// Number of buckets.
    pub fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// Number of nodes in bucket `idx`.
    pub fn bucket_len(&self, idx: usize) -> usize {
        self.buckets.get(idx).map_or(0, |b| b.nodes.len())
    }

    /// Number of stored nodes.
    pub fn len(&self) -> usize {
        self.buckets.iter().map(|b| b.nodes.len()).sum()
    }

    /// True if there are no nodes.
    pub fn is_empty(&self) -> bool {
        self.buckets.iter().all(|b| b.nodes.is_empty())
    }

    /// Number of nodes that are currently good.
    pub fn good_count(&self, now: u32) -> usize {
        self.iter()
            .filter(|n| n.status(now) == Status::Good)
            .count()
    }

    /// Iterates all nodes (no particular order).
    pub fn iter(&self) -> impl Iterator<Item = &Node> {
        self.buckets.iter().flat_map(|b| b.nodes.iter())
    }

    /// Bytes currently held (inline struct + bucket vector + every node vector's capacity).
    pub fn heap_bytes(&self) -> usize {
        size_of::<Self>()
            + self.buckets.capacity() * size_of::<Bucket>()
            + self
                .buckets
                .iter()
                .map(|b| b.nodes.capacity() * size_of::<Node>())
                .sum::<usize>()
    }

    /// Upper bound on heap + inline bytes for a full table (every bucket full + replacement).
    pub const fn worst_case_bytes() -> usize {
        160 * (K * size_of::<Node>() + size_of::<Option<Node>>() + size_of::<Bucket>())
    }

    fn index(&self, id: &Id) -> usize {
        common_prefix_len(&self.own, id).min(self.buckets.len() - 1)
    }

    /// True if a verified node with this id/address could be stored (space, split, or bad node to evict).
    pub fn would_accept(&self, id: &Id, addr: &SocketAddrV4, now: u32) -> bool {
        if *id == self.own
            || (self.unique_ip && self.iter().any(|n| n.addr.ip() == addr.ip() && n.id != *id))
        {
            return false;
        }
        let b = &self.buckets[self.index(id)];
        if b.nodes.iter().any(|n| n.id == *id) {
            return true;
        }
        let last = self.buckets.len() - 1;
        b.nodes.len() < K
            || (self.index(id) == last && self.buckets.len() < 160)
            || b.nodes.iter().any(|n| n.status(now) != Status::Good)
    }

    /// Inserts or refreshes a node that has just answered us (or queried us after having answered).
    pub fn insert(&mut self, id: Id, addr: SocketAddrV4, now: u32) -> Insert {
        if id == self.own {
            return Insert::Rejected;
        }
        let idx = self.index(&id);
        if let Some(n) = self.buckets[idx].nodes.iter_mut().find(|n| n.id == id) {
            if n.addr != addr {
                return Insert::Rejected;
            }
            n.last_seen = now;
            n.fails = 0;
            self.buckets[idx].changed = now;
            return Insert::Updated;
        }
        // One node per IP: cheap Sybil/eclipse damping and avoids NAT'd duplicates.
        if self.unique_ip && self.iter().any(|n| n.addr.ip() == addr.ip()) {
            return Insert::Rejected;
        }
        let node = Node {
            id,
            last_seen: now,
            addr,
            fails: 0,
        };
        loop {
            let idx = self.index(&id);
            let can_split = idx == self.buckets.len() - 1 && self.buckets.len() < 160;
            let b = &mut self.buckets[idx];
            if b.nodes.len() < K {
                b.nodes.push(node);
                b.changed = now;
                return Insert::Added;
            }
            if can_split {
                self.split(&node);
                continue;
            }
            if let Some(pos) = b.nodes.iter().position(|n| n.status(now) == Status::Bad) {
                b.nodes[pos] = node;
                b.changed = now;
                return Insert::Replaced;
            }
            let oldest = b
                .nodes
                .iter()
                .filter(|n| n.status(now) == Status::Questionable)
                .min_by_key(|n| n.last_seen)
                .copied();
            return match oldest {
                Some(stale) => {
                    b.repl = Some(node);
                    Insert::Verify(stale)
                }
                None => Insert::Rejected,
            };
        }
    }

    /// Splits the last bucket. `incoming` is the node being inserted, which the cached candidate
    /// must not collide with either.
    fn split(&mut self, incoming: &Node) {
        let last = self.buckets.len() - 1;
        let old = std::mem::replace(&mut self.buckets[last], Bucket::new(0));
        let mut stay = Bucket::new(old.changed);
        let mut deeper = Bucket::new(old.changed);
        for n in old.nodes {
            if common_prefix_len(&self.own, &n.id) == last {
                stay.nodes.push(n);
            } else {
                deeper.nodes.push(n);
            }
        }
        let repl = old.repl;
        self.buckets[last] = stay;
        self.buckets.push(deeper);
        if let Some(r) = repl {
            let idx = self.index(&r.id);
            let clash = r.id == incoming.id || (self.unique_ip && r.addr.ip() == incoming.addr.ip());
            if !clash && self.buckets[idx].nodes.len() < K && self.fits(&r, None) {
                self.buckets[idx].nodes.push(r);
            }
        }
    }

    /// True if `n` could join the table now without breaking its uniqueness rules (one entry per
    /// id, one per IP while `unique_ip`), ignoring the node `except`, which is about to leave.
    ///
    /// A cached replacement candidate was checked when it was cached; by the time it is promoted
    /// the table can hold another node with its id or its IP.
    fn fits(&self, n: &Node, except: Option<&Id>) -> bool {
        self.iter().filter(|o| except != Some(&o.id)).all(|o| {
            o.id != n.id && !(self.unique_ip && o.addr.ip() == n.addr.ip())
        })
    }

    /// Nodes that are not bad.
    pub fn usable_count(&self) -> usize {
        self.iter().filter(|n| n.fails < BAD_FAILS).count()
    }

    /// Like [`touch`](Self::touch) but only if the stored address matches, so a spoofed query
    /// cannot keep someone else's entry alive.
    pub fn touch_from(&mut self, id: &Id, addr: &SocketAddrV4, now: u32) -> bool {
        let idx = self.index(id);
        let b = &mut self.buckets[idx];
        if let Some(n) = b.nodes.iter_mut().find(|n| n.id == *id && n.addr == *addr) {
            n.last_seen = now;
            b.changed = now;
            true
        } else {
            false
        }
    }

    /// Marks a node as heard-from (resets its failure count). Returns false if unknown.
    pub fn touch(&mut self, id: &Id, now: u32) -> bool {
        let idx = self.index(id);
        let b = &mut self.buckets[idx];
        if let Some(n) = b.nodes.iter_mut().find(|n| n.id == *id) {
            n.last_seen = now;
            n.fails = 0;
            b.changed = now;
            true
        } else {
            false
        }
    }

    /// True if `id` is stored.
    pub fn contains(&self, id: &Id) -> bool {
        self.buckets[self.index(id)]
            .nodes
            .iter()
            .any(|n| n.id == *id)
    }

    /// Records an unanswered query. A node that turns bad is replaced by the bucket's cached candidate if any.
    pub fn fail(&mut self, id: &Id) {
        self.fail_by(id, 1);
    }

    /// Marks a node definitively dead (e.g. it ignored a verification ping with retries).
    pub fn mark_dead(&mut self, id: &Id) {
        self.fail_by(id, BAD_FAILS);
    }

    fn fail_by(&mut self, id: &Id, by: u8) {
        let idx = self.index(id);
        let b = &mut self.buckets[idx];
        if let Some(pos) = b.nodes.iter().position(|n| n.id == *id) {
            let n = &mut b.nodes[pos];
            n.fails = n.fails.saturating_add(by);
            if n.fails >= BAD_FAILS
                && let Some(r) = b.repl.take() {
                    // The candidate is dropped when the table took its id or IP meanwhile.
                    if self.fits(&r, Some(id)) {
                        self.buckets[idx].nodes[pos] = r;
                    }
                }
        }
    }

    /// Removes a node.
    pub fn remove(&mut self, id: &Id) {
        let idx = self.index(id);
        self.buckets[idx].nodes.retain(|n| n.id != *id);
    }

    /// Up to `n` non-bad nodes closest to `target`, nearest first.
    pub fn closest(&self, target: &Id, n: usize, _now: u32) -> Vec<Node> {
        let mut v: Vec<Node> = self
            .iter()
            .filter(|x| x.fails < BAD_FAILS)
            .copied()
            .collect();
        v.sort_by(|a, b| cmp_distance(target, &a.id, &b.id));
        v.truncate(n);
        v
    }

    /// Indices of buckets unchanged for `interval` seconds.
    pub fn stale_buckets(&self, now: u32, interval: u32) -> Vec<usize> {
        self.buckets
            .iter()
            .enumerate()
            .filter(|(_, b)| now.saturating_sub(b.changed) >= interval && !b.nodes.is_empty())
            .map(|(i, _)| i)
            .collect()
    }

    /// Earliest time (seconds) at which any non-empty bucket becomes stale.
    pub fn next_stale_at(&self, interval: u32) -> Option<u32> {
        self.buckets
            .iter()
            .filter(|b| !b.nodes.is_empty())
            .map(|b| b.changed.saturating_add(interval))
            .min()
    }

    /// Marks a bucket as just refreshed so it is not re-queued until it ages again.
    pub fn mark_refreshed(&mut self, idx: usize, now: u32) {
        if let Some(b) = self.buckets.get_mut(idx) {
            b.changed = now;
        }
    }

    /// A random id falling inside bucket `idx`'s range.
    pub fn random_id_in_bucket(&self, idx: usize, rng: &mut Rng) -> Id {
        let mut id = rng.id();
        let last = self.buckets.len() - 1;
        let idx = idx.min(last);
        for i in 0..idx {
            set_bit(&mut id, i, bit(&self.own, i));
        }
        if idx < last {
            set_bit(&mut id, idx, !bit(&self.own, idx));
        }
        id
    }

    /// Re-keys the table to a new own id, keeping all nodes that still fit.
    pub fn rekey(&mut self, new_own: Id, now: u32) {
        let old: Vec<Node> = self.iter().copied().collect();
        let unique_ip = self.unique_ip;
        *self = Self::new(new_own, now);
        self.unique_ip = unique_ip;
        for n in old {
            let _ = self.insert(n.id, n.addr, n.last_seen);
        }
    }

    /// Checks structural invariants; used by tests.
    #[cfg(test)]
    pub(crate) fn check_invariants(&self) {
        assert!(!self.buckets.is_empty() && self.buckets.len() <= 160);
        let last = self.buckets.len() - 1;
        let mut ips = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for (i, b) in self.buckets.iter().enumerate() {
            assert!(b.nodes.len() <= K, "bucket {i} overfull");
            for n in &b.nodes {
                let cpl = common_prefix_len(&self.own, &n.id);
                assert_ne!(n.id, self.own);
                if i < last {
                    assert_eq!(cpl, i);
                } else {
                    assert!(cpl >= last);
                }
                assert!(ips.insert(*n.addr.ip()), "duplicate ip");
                assert!(ids.insert(n.id), "duplicate id");
            }
        }
    }
}

fn set_bit(id: &mut Id, i: usize, v: bool) {
    let m = 0x80u8 >> (i % 8);
    if v {
        id[i / 8] |= m;
    } else {
        id[i / 8] &= !m;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::Rng;
    use proptest::prelude::{ProptestConfig, any, prop_assert_eq, proptest};
    use std::net::Ipv4Addr;

    fn addr(n: u32) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::from(0x0100_0000u32 + n), 6881)
    }

    #[test]
    fn sizes_documented() {
        assert_eq!(size_of::<Node>(), 32);
        assert!(
            RoutingTable::worst_case_bytes() < 64 * 1024,
            "{}",
            RoutingTable::worst_case_bytes()
        );
    }

    #[test]
    fn memory_of_full_and_realistic_tables() {
        // Realistic: ~25 buckets fully populated (a network of tens of millions of nodes).
        let own = [0u8; 20];
        let mut t = RoutingTable::new(own, 0);
        let mut n = 0u32;
        for b in 0..25usize {
            for k in 0..K as u32 {
                let mut id = own;
                id[b / 8] |= 0x80 >> (b % 8);
                id[19] = k as u8;
                id[18] = b as u8;
                id[17] = 1;
                assert!(
                    matches!(t.insert(id, addr(n), 0), Insert::Added | Insert::Updated),
                    "bucket {b}"
                );
                n += 1;
            }
        }
        // Fill the final (own) bucket as well.
        for k in 0..K as u32 {
            let mut id = own;
            id[12] = 1;
            id[19] = k as u8;
            t.insert(id, addr(n), 0);
            n += 1;
        }
        t.check_invariants();
        eprintln!(
            "realistic table: {} nodes in {} buckets = {} bytes",
            t.len(),
            t.bucket_count(),
            t.heap_bytes()
        );
        assert!(t.len() >= 25 * K);
        assert!(t.heap_bytes() < 16 * 1024);
    }

    #[test]
    fn theoretical_worst_case_stays_under_64_kib() {
        // 159 split buckets, each full: needs 2^159 nodes in reality; this is the hard ceiling.
        let own = [0u8; 20];
        let mut t = RoutingTable::new(own, 0);
        let mut n = 0u32;
        for b in 0..159usize {
            for k in 0..K as u32 {
                let mut id = own;
                id[b / 8] |= 0x80 >> (b % 8);
                id[19] = k as u8;
                id[18] = 1;
                t.insert(id, addr(n), 0);
                n += 1;
            }
        }
        t.check_invariants();
        eprintln!(
            "ceiling: {} nodes, {} buckets, {} bytes",
            t.len(),
            t.bucket_count(),
            t.heap_bytes()
        );
        assert!(t.bucket_count() >= 140);
        assert!(t.heap_bytes() < 64 * 1024, "{}", t.heap_bytes());
    }

    #[test]
    fn splits_only_own_bucket_and_caps_far_buckets() {
        let own = [0x55u8; 20];
        let mut t = RoutingTable::new(own, 0);
        let mut rng = Rng::from_seed(1);
        for i in 0..5000 {
            t.insert(rng.id(), addr(i), 0);
        }
        t.check_invariants();
        // Far half of the space can hold only K nodes.
        assert_eq!(t.buckets[0].nodes.len(), K);
        assert!(t.bucket_count() >= 8);
        assert!(t.len() <= 160 * K);
    }

    #[test]
    fn same_ip_rejected_and_conflicting_addr_rejected() {
        let mut t = RoutingTable::new([0u8; 20], 0);
        assert_eq!(t.insert([1; 20], addr(1), 0), Insert::Added);
        assert_eq!(t.insert([2; 20], addr(1), 0), Insert::Rejected);
        assert_eq!(t.insert([1; 20], addr(2), 0), Insert::Rejected);
        assert_eq!(t.insert([1; 20], addr(1), 5), Insert::Updated);
        assert_eq!(t.insert([0; 20], addr(3), 0), Insert::Rejected);
    }

    #[test]
    fn full_bucket_good_nodes_reject_questionable_verify_bad_replace() {
        let own = [0u8; 20];
        let mut t = RoutingTable::new(own, 0);
        // Fill bucket 0 (ids starting with bit 1) with 8 nodes, forcing own bucket to split first.
        let mk = |i: u8| {
            let mut id = [0u8; 20];
            id[0] = 0x80 | i;
            id
        };
        for i in 0..8u8 {
            assert_eq!(t.insert(mk(i), addr(i as u32), 0), Insert::Added);
        }
        // Full of good nodes: reject.
        assert_eq!(t.insert(mk(100), addr(100), 10), Insert::Rejected);
        // After 15 minutes the nodes are questionable: ask to verify the oldest.
        let later = GOOD_WINDOW_S + 100;
        t.touch(&mk(0), later - 10);
        match t.insert(mk(100), addr(100), later) {
            Insert::Verify(stale) => assert_ne!(stale.id, mk(0)),
            o => panic!("{o:?}"),
        }
        // Stale node confirmed dead: the cached candidate takes its place.
        let stale = t
            .iter()
            .filter(|n| n.status(later) == Status::Questionable)
            .min_by_key(|n| n.last_seen)
            .copied()
            .unwrap();
        t.mark_dead(&stale.id);
        assert!(t.contains(&mk(100)));
        assert!(!t.contains(&stale.id));
        // A bad node is replaced directly.
        let v = t.iter().find(|n| n.id != mk(100)).copied().unwrap();
        t.fail(&v.id);
        t.fail(&v.id);
        assert_eq!(t.insert(mk(101), addr(101), later), Insert::Replaced);
        t.check_invariants();
    }

    #[test]
    fn closest_excludes_bad_and_orders() {
        let mut t = RoutingTable::new([0u8; 20], 0);
        let mut rng = Rng::from_seed(4);
        for i in 0..200 {
            t.insert(rng.id(), addr(i), 0);
        }
        let target = rng.id();
        let c = t.closest(&target, 8, 0);
        assert_eq!(c.len(), 8);
        for w in c.windows(2) {
            assert_ne!(
                cmp_distance(&target, &w[0].id, &w[1].id),
                std::cmp::Ordering::Greater
            );
        }
        t.mark_dead(&c[0].id);
        let c2 = t.closest(&target, 8, 0);
        assert!(!c2.iter().any(|n| n.id == c[0].id));
    }

    #[test]
    fn stale_and_refresh_targets() {
        let own = [0xaau8; 20];
        let mut t = RoutingTable::new(own, 0);
        let mut rng = Rng::from_seed(2);
        for i in 0..400 {
            t.insert(rng.id(), addr(i), 100);
        }
        assert!(t.stale_buckets(100, 900).is_empty());
        assert_eq!(t.stale_buckets(1000, 900).len(), t.bucket_count());
        assert_eq!(t.next_stale_at(900), Some(1000));
        t.mark_refreshed(0, 1000);
        assert!(!t.stale_buckets(1000, 900).contains(&0));
        for idx in 0..t.bucket_count() {
            for _ in 0..20 {
                let id = t.random_id_in_bucket(idx, &mut rng);
                assert_eq!(t.index(&id), idx, "bucket {idx}");
            }
        }
    }

    /// A full bucket of questionable nodes (bucket 0, not the last bucket) holding a cached
    /// replacement candidate. Returns the table, the candidate, and the ids of the full bucket.
    fn full_bucket_with_candidate() -> (RoutingTable, Node, Vec<Id>) {
        let own = [0u8; 20];
        let mut t = RoutingTable::new(own, 0);
        let mut ids = Vec::new();
        for i in 0..K as u8 {
            let mut id = [0u8; 20];
            id[0] = 0x80 | (i + 1);
            ids.push(id);
            assert_eq!(t.insert(id, addr(u32::from(i) + 1), 0), Insert::Added);
        }
        // The ninth node of the one bucket splits it; every node stays at cpl 0, so the bucket
        // is full of questionable nodes at t = 2000 and the newcomer waits as the candidate.
        let mut cand_id = [0u8; 20];
        cand_id[0] = 0xC0;
        let r = t.insert(cand_id, addr(100), 2000);
        let Insert::Verify(_) = r else {
            panic!("expected Verify, got {r:?}")
        };
        assert!(t.bucket_count() > 1, "bucket 0 must not be the last bucket");
        let cand = t.buckets[0].repl.expect("candidate cached");
        (t, cand, ids)
    }

    /// Regression (found by `random_inserts_keep_invariants`, "duplicate ip"): a cached
    /// replacement candidate was checked against the table only when it was cached. Another
    /// node with the same IP could join a different bucket afterwards, and promoting the
    /// candidate then put one IP in the table twice.
    #[test]
    fn promoted_candidate_does_not_duplicate_an_ip() {
        let (mut t, cand, ids) = full_bucket_with_candidate();
        // Same IP as the candidate, other bucket (cpl 1): accepted, the candidate is not stored.
        let mut other = [0u8; 20];
        other[0] = 0x40;
        assert_eq!(t.insert(other, cand.addr, 2000), Insert::Added);
        t.fail(&ids[0]);
        t.fail(&ids[0]);
        t.check_invariants();
        // The candidate was dropped rather than promoted: the bad node keeps its place and is
        // replaced by the next newcomer that fits.
        assert!(!t.contains(&cand.id));
        assert!(t.contains(&ids[0]));
    }

    /// Same hole for the id: the candidate's id was stored in the meantime.
    #[test]
    fn promoted_candidate_does_not_duplicate_an_id() {
        let (mut t, cand, ids) = full_bucket_with_candidate();
        t.remove(&ids[7]);
        assert_eq!(t.insert(cand.id, addr(200), 2000), Insert::Added);
        t.fail(&ids[0]);
        t.fail(&ids[0]);
        t.check_invariants();
        assert!(t.contains(&cand.id));
    }

    /// A candidate that still fits is promoted as before.
    #[test]
    fn promoted_candidate_replaces_the_failed_node() {
        let (mut t, cand, ids) = full_bucket_with_candidate();
        t.fail(&ids[0]);
        t.fail(&ids[0]);
        t.check_invariants();
        assert!(t.contains(&cand.id));
        assert!(!t.contains(&ids[0]));
    }

    #[test]
    fn rekey_keeps_nodes() {
        let mut t = RoutingTable::new([1u8; 20], 0);
        let mut rng = Rng::from_seed(5);
        for i in 0..10 {
            t.insert(rng.id(), addr(i), 0);
        }
        let before = t.len();
        assert!(before >= 8);
        t.rekey(rng.id(), 0);
        t.check_invariants();
        // A young table (<= one bucket per prefix bit) survives re-keying intact.
        assert!(t.len() >= before - 3, "{} vs {before}", t.len());
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(
            std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(64)
        ))]

        #[test]
        fn random_inserts_keep_invariants(
            own in any::<[u8; 20]>(),
            ops in proptest::collection::vec((any::<[u8; 20]>(), 0u32..400, 0u32..4000, 0u8..4), 1..600),
        ) {
            let mut t = RoutingTable::new(own, 0);
            for (id, ip, now, op) in ops {
                match op {
                    0 | 1 => { t.insert(id, addr(ip), now); }
                    2 => t.fail(&id),
                    _ => { t.touch(&id, now); }
                }
                // Occasionally fail an existing node so bad/replacement paths run.
                if ip % 7 == 0 {
                    let first = t.iter().next().copied();
                    if let Some(n) = first { t.fail(&n.id); }
                }
            }
            t.check_invariants();
        }

        #[test]
        fn closest_matches_brute_force(
            own in any::<[u8; 20]>(),
            ids in proptest::collection::vec(any::<[u8; 20]>(), 1..300),
            target in any::<[u8; 20]>(),
            n in 1usize..20,
        ) {
            let mut t = RoutingTable::new(own, 0);
            for (i, id) in ids.iter().enumerate() { t.insert(*id, addr(i as u32), 0); }
            let got: Vec<Id> = t.closest(&target, n, 0).iter().map(|x| x.id).collect();
            let mut all: Vec<Id> = t.iter().map(|x| x.id).collect();
            all.sort_by(|a, b| cmp_distance(&target, a, b));
            all.truncate(n);
            prop_assert_eq!(got, all);
        }

        #[test]
        fn random_bucket_target_lands_in_bucket(own in any::<[u8; 20]>(), seed in any::<u64>()) {
            let mut t = RoutingTable::new(own, 0);
            let mut rng = Rng::from_seed(seed);
            for i in 0..200 { t.insert(rng.id(), addr(i), 0); }
            for idx in 0..t.bucket_count() {
                let id = t.random_id_in_bucket(idx, &mut rng);
                prop_assert_eq!(t.index(&id), idx);
            }
        }
    }
}
