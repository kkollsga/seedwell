//! Iterative lookup state machine (pure data; the core drives I/O).
//!
//! Classic Kademlia: candidates sorted by distance to the target; at most `alpha` queries in
//! flight; only candidates inside the closest-`K` window of *non-failed* nodes are queried; the
//! search is complete when every node in that window has replied. Failures slide the window
//! outwards, so dead nodes cannot stall a lookup.
//!
//! Memory per lookup: candidates are capped at [`MAX_CANDS`] (64 * ~64 B = ~4 KiB) plus the
//! peer dedupe set (capped by `peer_cap`).

use crate::codec::MAX_TOKEN;
use crate::id::{Id, cmp_distance};
use crate::table::K;
use std::collections::HashSet;
use std::net::SocketAddrV4;

/// Candidate list cap.
pub const MAX_CANDS: usize = 64;

/// Candidate progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandState {
    /// Not yet queried.
    Fresh,
    /// Query outstanding.
    InFlight,
    /// Answered.
    Replied,
    /// Timed out, errored or mismatched.
    Failed,
}

/// A write token received from a remote node (inline, no allocation).
// size: 33 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    len: u8,
    bytes: [u8; MAX_TOKEN],
}

impl Token {
    /// Copies `b` if it fits (1..=MAX_TOKEN bytes).
    pub fn new(b: &[u8]) -> Option<Self> {
        if b.is_empty() || b.len() > MAX_TOKEN {
            return None;
        }
        let mut bytes = [0u8; MAX_TOKEN];
        bytes[..b.len()].copy_from_slice(b);
        Some(Self {
            len: b.len() as u8,
            bytes,
        })
    }

    /// The token bytes.
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }
}

/// One node in a lookup.
// size: 64 bytes
#[derive(Clone, Copy, Debug)]
pub struct Cand {
    /// Node id (the target itself while unknown, so seeds sort first).
    pub id: Id,
    /// Address.
    pub addr: SocketAddrV4,
    /// Progress.
    pub state: CandState,
    /// False for bootstrap seeds whose id we have not learned yet.
    pub id_known: bool,
    /// Token from its `get_peers` reply.
    pub token: Option<Token>,
}

/// Why the lookup runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purpose {
    /// `get_peers` for a torrent, optionally followed by `announce_peer`.
    Peers {
        /// Port to announce afterwards.
        announce_port: Option<u16>,
    },
    /// Self-lookup populating a fresh table.
    Bootstrap,
    /// Bucket refresh (short).
    Refresh,
}

/// A running lookup.
#[derive(Debug)]
pub struct Lookup {
    /// Identifier within the core.
    pub id: u32,
    /// Target id / info hash.
    pub target: Id,
    /// Reason.
    pub purpose: Purpose,
    /// Candidates, nearest first.
    pub cands: Vec<Cand>,
    /// Parallelism.
    pub alpha: u8,
    /// Query budget for this lookup.
    pub max_queries: u16,
    /// Queries sent (retransmits excluded).
    pub queries_sent: u16,
    /// Outstanding queries (search phase).
    pub in_flight: u8,
    /// Replies received.
    pub replied: u16,
    /// Start time.
    pub started_ms: u64,
    /// Hard stop time.
    pub deadline_ms: u64,
    /// Peers already reported.
    pub peers_seen: HashSet<SocketAddrV4>,
    /// Maximum peers to collect.
    pub peer_cap: usize,
    /// True once the search phase ended and announces are being sent.
    pub announcing: bool,
    /// Announces still to be sent: (addr, node id, token).
    pub announce_queue: Vec<(SocketAddrV4, Id, Token)>,
    /// Announces sent and awaiting an answer.
    pub announce_outstanding: u8,
    /// Announces acknowledged.
    pub announce_accepted: u8,
    /// Announces attempted.
    pub announce_attempted: u8,
    /// Set when the lookup should stop querying (cap reached etc.).
    pub stop: bool,
    /// How many of the closest non-failed candidates are eligible for querying (starts at `K`).
    pub window: usize,
}

impl Lookup {
    /// New lookup with no candidates.
    #[allow(clippy::too_many_arguments)] // plain data constructor; a params struct would only restate the fields
    pub fn new(
        id: u32,
        target: Id,
        purpose: Purpose,
        alpha: u8,
        max_queries: u16,
        now_ms: u64,
        deadline_ms: u64,
        peer_cap: usize,
    ) -> Self {
        Self {
            id,
            target,
            purpose,
            cands: Vec::with_capacity(16),
            alpha,
            max_queries,
            queries_sent: 0,
            in_flight: 0,
            replied: 0,
            started_ms: now_ms,
            deadline_ms,
            peers_seen: HashSet::new(),
            peer_cap,
            announcing: false,
            announce_queue: Vec::new(),
            announce_outstanding: 0,
            announce_accepted: 0,
            announce_attempted: 0,
            stop: false,
            window: K,
        }
    }

    /// Adds a candidate unless its address or id is already present. Keeps the list sorted and capped.
    pub fn add_candidate(&mut self, id: Id, addr: SocketAddrV4, id_known: bool) -> bool {
        if self
            .cands
            .iter()
            .any(|c| c.addr == addr || (id_known && c.id_known && c.id == id))
        {
            return false;
        }
        let pos = self
            .cands
            .partition_point(|c| cmp_distance(&self.target, &c.id, &id).is_le());
        if pos >= MAX_CANDS {
            return false;
        }
        self.cands.insert(
            pos,
            Cand {
                id,
                addr,
                state: CandState::Fresh,
                id_known,
                token: None,
            },
        );
        if self.cands.len() > MAX_CANDS {
            if let Some(i) = self
                .cands
                .iter()
                .rposition(|c| matches!(c.state, CandState::Fresh | CandState::Failed))
            {
                self.cands.remove(i);
            }
        }
        true
    }

    /// Index of the candidate at `addr`.
    pub fn find(&self, addr: &SocketAddrV4) -> Option<usize> {
        self.cands.iter().position(|c| c.addr == *addr)
    }

    /// Next candidate to query, if any is inside the closest-`K` window of non-failed nodes.
    pub fn next_to_query(&self) -> Option<usize> {
        if self.stop || self.queries_sent >= self.max_queries {
            return None;
        }
        let mut counted = 0;
        for (i, c) in self.cands.iter().enumerate() {
            if c.state == CandState::Failed {
                continue;
            }
            if counted >= self.window {
                return None;
            }
            if c.state == CandState::Fresh {
                return Some(i);
            }
            counted += 1;
        }
        None
    }

    /// A plain `get_peers` that found nothing among the closest `K` nodes looks further out
    /// (up to `3K`): announcers see a different, churn-ridden view of "closest", so peers are
    /// often stored a few ranks away. Announce lookups never widen: they must hit the true closest.
    pub fn try_widen(&mut self) -> bool {
        let plain = matches!(
            self.purpose,
            Purpose::Peers {
                announce_port: None
            }
        );
        if plain && !self.stop && self.peers_seen.is_empty() && self.window < 3 * K {
            self.window += K;
            true
        } else {
            false
        }
    }

    /// Marks candidate `i` as queried.
    pub fn mark_sent(&mut self, i: usize) {
        self.cands[i].state = CandState::InFlight;
        self.in_flight += 1;
        self.queries_sent += 1;
    }

    /// Records a reply from `addr`; updates the id of seeds that were unknown. Returns false if
    /// `addr` is not an in-flight candidate.
    pub fn on_reply(&mut self, addr: &SocketAddrV4, id: Id, token: Option<Token>) -> bool {
        let Some(i) = self.find(addr) else {
            return false;
        };
        if self.cands[i].state != CandState::InFlight {
            return false;
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        self.replied += 1;
        let mut c = self.cands.remove(i);
        c.state = CandState::Replied;
        c.token = token;
        if !c.id_known {
            c.id = id;
            c.id_known = true;
        }
        let pos = self
            .cands
            .partition_point(|x| cmp_distance(&self.target, &x.id, &c.id).is_le());
        self.cands.insert(pos, c);
        true
    }

    /// Records a failure for `addr`. Returns false if it was not in flight.
    pub fn on_failure(&mut self, addr: &SocketAddrV4) -> bool {
        let Some(i) = self.find(addr) else {
            return false;
        };
        if self.cands[i].state != CandState::InFlight {
            return false;
        }
        self.in_flight = self.in_flight.saturating_sub(1);
        self.cands[i].state = CandState::Failed;
        true
    }

    /// True if no more search queries can or will be sent and none are outstanding.
    pub fn search_finished(&self) -> bool {
        self.in_flight == 0 && self.next_to_query().is_none()
    }

    /// Filters `peers` to those not yet reported, honouring `peer_cap`. Sets `stop` at the cap.
    pub fn new_peers(&mut self, peers: impl Iterator<Item = SocketAddrV4>) -> Vec<SocketAddrV4> {
        let mut out = Vec::new();
        for p in peers {
            if self.peers_seen.len() >= self.peer_cap {
                self.stop = true;
                break;
            }
            if self.peers_seen.insert(p) {
                out.push(p);
            }
        }
        if self.peers_seen.len() >= self.peer_cap {
            self.stop = true;
        }
        out
    }

    /// The up-to-`K` closest nodes that replied with a token.
    pub fn announce_targets(&self) -> Vec<(SocketAddrV4, Id, Token)> {
        self.cands
            .iter()
            .filter(|c| c.state == CandState::Replied && c.id_known)
            .filter_map(|c| c.token.map(|t| (c.addr, c.id, t)))
            .take(K)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn addr(n: u8) -> SocketAddrV4 {
        SocketAddrV4::new(Ipv4Addr::new(2, 2, 2, n), 1)
    }

    fn id(first: u8) -> Id {
        let mut i = [0u8; 20];
        i[0] = first;
        i
    }

    fn lk() -> Lookup {
        Lookup::new(
            1,
            [0u8; 20],
            Purpose::Peers {
                announce_port: None,
            },
            3,
            100,
            0,
            60_000,
            200,
        )
    }

    #[test]
    fn sorted_dedup_capped() {
        let mut l = lk();
        assert!(l.add_candidate(id(5), addr(1), true));
        assert!(l.add_candidate(id(2), addr(2), true));
        assert!(!l.add_candidate(id(9), addr(2), true), "same addr");
        assert!(!l.add_candidate(id(2), addr(3), true), "same id");
        assert!(l.add_candidate(id(9), addr(4), true));
        let order: Vec<u8> = l.cands.iter().map(|c| c.id[0]).collect();
        assert_eq!(order, vec![2, 5, 9]);
        for n in 0..200u8 {
            l.add_candidate(
                id(100 + n % 100),
                SocketAddrV4::new(Ipv4Addr::new(3, 3, n, 1), 1),
                true,
            );
        }
        assert!(l.cands.len() <= MAX_CANDS);
        assert_eq!(l.cands[0].id[0], 2, "nearest survive trimming");
    }

    #[test]
    fn window_slides_on_failure_and_terminates() {
        let mut l = lk();
        for n in 0..12u8 {
            l.add_candidate(id(n + 1), addr(n + 1), true);
        }
        // Query the first K; the 9th is outside the window until something fails.
        let mut sent = 0;
        while let Some(i) = l.next_to_query() {
            l.mark_sent(i);
            sent += 1;
        }
        assert_eq!(sent, K);
        assert_eq!(l.in_flight as usize, K);
        assert!(!l.search_finished());
        // One failure admits the 9th.
        l.on_failure(&addr(1));
        let i = l.next_to_query().expect("window slid");
        assert_eq!(l.cands[i].id[0], 9);
        l.mark_sent(i);
        for n in 2..=9u8 {
            assert!(l.on_reply(&addr(n), id(n), Token::new(b"tok")));
        }
        assert!(l.search_finished());
        let t = l.announce_targets();
        assert_eq!(t.len(), K);
        assert_eq!(t[0].1[0], 2);
    }

    #[test]
    fn seeds_with_unknown_id_resolve() {
        let mut l = lk();
        l.add_candidate(l.target, addr(1), false);
        l.add_candidate(id(7), addr(2), true);
        let i = l.next_to_query().unwrap();
        assert_eq!(l.cands[i].addr, addr(1));
        l.mark_sent(i);
        assert!(l.on_reply(&addr(1), id(200), None));
        assert_eq!(l.cands.last().unwrap().id[0], 200, "re-sorted by real id");
        assert!(l.cands.last().unwrap().id_known);
        assert!(
            !l.on_reply(&addr(1), id(200), None),
            "duplicate reply ignored"
        );
    }

    #[test]
    fn widens_only_for_plain_lookups_without_results() {
        let mut l = lk();
        for n in 0..30u8 {
            l.add_candidate(id(n + 1), addr(n + 1), true);
        }
        for round in 1..=3usize {
            while let Some(i) = l.next_to_query() {
                l.mark_sent(i);
            }
            assert_eq!(l.in_flight as usize, K, "round {round}");
            for c in 0..l.cands.len() {
                let a = l.cands[c].addr;
                l.on_reply(&a, l.cands[c].id, None);
            }
            assert!(l.search_finished());
            assert_eq!(l.try_widen(), round < 3, "round {round}");
        }
        let mut a = Lookup::new(
            1,
            [0; 20],
            Purpose::Peers {
                announce_port: Some(1),
            },
            3,
            100,
            0,
            1000,
            10,
        );
        assert!(!a.try_widen());
        let mut f = lk();
        f.new_peers([addr(1)].into_iter());
        assert!(!f.try_widen());
    }

    #[test]
    fn query_budget_and_peer_cap() {
        let mut l = Lookup::new(1, [0; 20], Purpose::Bootstrap, 3, 2, 0, 1000, 3);
        for n in 0..5u8 {
            l.add_candidate(id(n + 1), addr(n + 1), true);
        }
        l.mark_sent(0);
        l.mark_sent(1);
        assert!(l.next_to_query().is_none());
        let p: Vec<_> = (1..=5u8).map(addr).collect();
        let got = l.new_peers(p.iter().copied());
        assert_eq!(got.len(), 3);
        assert!(l.stop);
        assert!(l.new_peers(p.iter().copied()).is_empty());
    }

    #[test]
    fn token_bounds() {
        assert!(Token::new(b"").is_none());
        assert!(Token::new(&[0u8; 33]).is_none());
        assert_eq!(Token::new(b"abc").unwrap().as_slice(), b"abc");
    }
}
