//! BEP 11 peer exchange. Follows one rule (`policy::pex_allowed`): the global switch, and for a
//! private torrent (BEP 27) also its tracker's `allow_pex`, off by default.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use trs_wire::{PexBuilder, PexFlags};

use super::Torrent;
use crate::peer::PeerCmd;

/// Changes remembered so peers can be sent deltas.
const LOG_CAP: usize = 128;
/// Delay between PEX messages to one peer (BEP 11 asks for at most one per minute).
const PEX_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
struct Change {
    seq: u64,
    added: bool,
    addr: SocketAddr,
    flags: u8,
}

/// Recent connects and disconnects.
// size: ~40 bytes + 40 bytes per entry (<= 128)
pub(crate) struct PexLog {
    seq: u64,
    log: VecDeque<Change>,
}

impl PexLog {
    pub(crate) fn new() -> Self {
        Self { seq: 0, log: VecDeque::new() }
    }

    fn push(&mut self, added: bool, addr: SocketAddr, flags: u8) {
        self.seq += 1;
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(Change { seq: self.seq, added, addr, flags });
    }

    pub(super) fn added(&mut self, addr: SocketAddr, flags: u8) {
        self.push(true, addr, flags);
    }

    pub(super) fn dropped(&mut self, addr: SocketAddr) {
        self.push(false, addr, 0);
    }

    /// Changes after `seq`, or `None` if the log no longer reaches back that far.
    fn since(&self, seq: u64) -> Option<Vec<Change>> {
        let first = self.log.front().map_or(self.seq + 1, |c| c.seq);
        if seq + 1 < first {
            return None;
        }
        Some(self.log.iter().filter(|c| c.seq > seq).copied().collect())
    }
}

impl Torrent {
    /// The one PEX rule (`policy::pex_allowed`), as last applied.
    pub(super) fn pex_allowed(&self) -> bool {
        self.ctx.pex.load(Ordering::Relaxed)
    }

    /// The global switch or the tracker's setting changed: apply the rule at once. Turning it
    /// off stops sending and using PEX on every connection immediately; turning it on tells the
    /// connected peers (a new extended handshake) and lets them start receiving ours.
    pub(super) fn refresh_pex(&mut self) {
        let now = crate::policy::pex_allowed(self.cfg.pex_enabled, self.private, self.tracker_pex);
        if self.ctx.pex.swap(now, Ordering::Relaxed) == now {
            return;
        }
        if now {
            for p in self.peers.values().filter(|p| p.connected) {
                let _ = p.tx.try_send(PeerCmd::ReadvertiseExt);
            }
        }
    }

    /// A peer's listening address became known.
    pub(super) fn pex_note_connected(&mut self, addr: SocketAddr, seed: bool, utp: bool) {
        if self.pex_allowed() {
            let flags = if seed { PexFlags::SEED } else { 0 } | if utp { PexFlags::UTP } else { 0 };
            self.pex_log.added(addr, flags);
            self.pool.add(addr);
        }
    }

    pub(super) fn on_pex(&mut self, addrs: Vec<(SocketAddr, bool)>) {
        if !self.pex_allowed() {
            return;
        }
        self.sh.diag.pex_received.fetch_add(1, Ordering::Relaxed);
        for (a, utp) in addrs {
            if utp {
                self.sh.utp_peers.lock().unwrap_or_else(|e| e.into_inner()).insert(a);
            }
            if !self.addrs_in_use.contains(&a) && !self.is_banned(a) {
                self.pool.add(a);
            }
        }
        self.connect_more(Instant::now());
    }

    pub(super) fn pex_tick(&mut self, now: Instant) {
        if !self.pex_allowed() || self.meta.is_none() {
            return;
        }
        let due: Vec<u32> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected && p.has_pex && now >= p.next_pex)
            .map(|(k, _)| *k)
            .collect();
        for k in due {
            let Some(p) = self.peers.get(&k) else { continue };
            let mut b = PexBuilder::new();
            let delta = if p.pex_seq == 0 { None } else { self.pex_log.since(p.pex_seq) };
            match delta {
                Some(changes) => {
                    for c in changes {
                        if Some(c.addr) == p.listen_addr {
                            continue;
                        }
                        if c.added {
                            b.add(c.addr, PexFlags(c.flags));
                        } else {
                            b.drop_peer(c.addr);
                        }
                    }
                }
                None => {
                    // First message (or the log moved on): a sample of who we are connected to.
                    let mut others: Vec<(SocketAddr, u8)> = self
                        .peers
                        .iter()
                        .filter(|(kk, q)| **kk != k && q.connected)
                        .filter_map(|(_, q)| {
                            q.listen_addr.map(|a| (a, if q.upload_only { PexFlags::SEED } else { 0 } | if q.utp { PexFlags::UTP } else { 0 }))
                        })
                        .collect();
                    self.rng.shuffle(&mut others);
                    for (a, f) in others.into_iter().take(50) {
                        b.add(a, PexFlags(f));
                    }
                }
            }
            let seq = self.pex_log.seq;
            let jitter = Duration::from_millis(self.rng.below(5000));
            if let Some(p) = self.peers.get_mut(&k) {
                p.pex_seq = seq.max(1);
                p.next_pex = now + PEX_INTERVAL + jitter;
            }
            if b.is_empty() {
                continue;
            }
            let mut payload = Vec::with_capacity(256);
            b.encode_payload(&mut payload);
            if let Some(p) = self.peers.get(&k) {
                let _ = p.tx.try_send(PeerCmd::Pex(payload));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 6881))
    }

    #[test]
    fn deltas_cover_exactly_the_changes_since() {
        let mut l = PexLog::new();
        l.added(a(1), 0);
        l.added(a(2), 0);
        let after_two = l.seq;
        l.dropped(a(1));
        l.added(a(3), 2);
        let d = l.since(after_two).unwrap();
        assert_eq!(d.len(), 2);
        assert!(!d[0].added && d[0].addr == a(1));
        assert!(d[1].added && d[1].addr == a(3) && d[1].flags == 2);
        assert!(l.since(l.seq).unwrap().is_empty());
    }

    #[test]
    fn log_is_bounded_and_reports_a_gap() {
        let mut l = PexLog::new();
        for i in 0..(LOG_CAP as u32 + 50) {
            l.added(SocketAddr::from(([10, 1, (i / 250) as u8, (i % 250) as u8], 1000)), 0);
        }
        assert_eq!(l.log.len(), LOG_CAP);
        assert!(l.since(1).is_none(), "peers that missed too much get a fresh sample");
        assert!(l.since(l.seq - 3).is_some());
    }
}
