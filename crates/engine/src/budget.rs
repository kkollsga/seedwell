//! Connection budget: a global peer cap and a half-open cap (ENGINE.md 2). Per-torrent caps are
//! enforced by the torrent actor, which knows its own peers.
//!
//! The half-open cap is for our own dials. Incoming connections still reading their handshake have a
//! cap of their own ([`MAX_INCOMING_HANDSHAKES`]): a burst of dials to peers that never answer would
//! otherwise turn away the peers that came to us, which a seed lives on.
//!
//! Permits are RAII: dropping one returns the slot, so a task that panics or is cancelled cannot
//! leak capacity.
//!
//! Seeding torrents dial from a share of it ([`ConnectionBudget::try_seed_half_open`],
//! [`ConnectionBudget::try_seed_dial`]): the addresses a tracker hands a seed are mostly seeds, NAT'd
//! leechers and dead hosts, and two hundred seeds answered by their trackers in the same minute must
//! neither dial a storm nor take every dial slot from a download.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug)]
struct Inner {
    max_total: AtomicUsize,
    max_half_open: AtomicUsize,
    total: AtomicUsize,
    half_open: AtomicUsize,
    incoming: AtomicUsize,
    /// Dials seeding torrents may still start now (a token bucket), and when it was last filled.
    seed_tokens: Mutex<(f64, Option<Instant>)>,
}

/// Incoming connections that may be reading their handshake at once.
pub const MAX_INCOMING_HANDSHAKES: usize = 128;
/// Dials per second all seeding torrents together may start (libtorrent's `connection_speed`, 30 a
/// second, is for every torrent; downloads here are bounded by the half-open cap alone).
pub const SEED_DIALS_PER_SEC: f64 = 10.0;
/// Dials seeding torrents may start at once after a quiet spell (about one torrent's boost).
pub const SEED_DIAL_BURST: f64 = 20.0;

/// Shared budget.
#[derive(Clone, Debug)]
pub struct ConnectionBudget(Arc<Inner>);

/// One established-or-establishing peer connection.
#[derive(Debug)]
pub struct ConnPermit(Arc<Inner>);

/// A connection that has not finished its handshake yet.
#[derive(Debug)]
pub struct HalfOpenPermit(Arc<Inner>);

/// An incoming connection that has not sent its handshake yet.
#[derive(Debug)]
pub struct IncomingPermit(Arc<Inner>);

impl ConnectionBudget {
    /// New budget.
    pub fn new(max_total: usize, max_half_open: usize) -> Self {
        Self(Arc::new(Inner {
            max_total: AtomicUsize::new(max_total),
            max_half_open: AtomicUsize::new(max_half_open),
            total: AtomicUsize::new(0),
            half_open: AtomicUsize::new(0),
            incoming: AtomicUsize::new(0),
            seed_tokens: Mutex::new((SEED_DIAL_BURST, None)),
        }))
    }

    /// Change the caps at runtime. Existing connections are not dropped.
    pub fn set_caps(&self, max_total: usize, max_half_open: usize) {
        self.0.max_total.store(max_total, Ordering::Relaxed);
        self.0.max_half_open.store(max_half_open, Ordering::Relaxed);
    }

    fn take(counter: &AtomicUsize, max: usize) -> bool {
        counter
            .try_update(Ordering::AcqRel, Ordering::Acquire, |v| (v < max).then_some(v + 1))
            .is_ok()
    }

    /// Reserve a connection slot, or `None` when the global cap is reached.
    pub fn try_connection(&self) -> Option<ConnPermit> {
        Self::take(&self.0.total, self.0.max_total.load(Ordering::Relaxed))
            .then(|| ConnPermit(self.0.clone()))
    }

    /// Reserve a half-open slot, or `None` when too many handshakes are pending.
    pub fn try_half_open(&self) -> Option<HalfOpenPermit> {
        Self::take(&self.0.half_open, self.0.max_half_open.load(Ordering::Relaxed))
            .then(|| HalfOpenPermit(self.0.clone()))
    }

    /// A half-open slot for a seeding torrent's dial: only while fewer than half the slots are taken, so
    /// that seeds chasing leechers never hold up a download's dials.
    pub fn try_seed_half_open(&self) -> Option<HalfOpenPermit> {
        let share = (self.0.max_half_open.load(Ordering::Relaxed) / 2).max(1);
        Self::take(&self.0.half_open, share).then(|| HalfOpenPermit(self.0.clone()))
    }

    /// May a seeding torrent start a dial now? Takes one of the [`SEED_DIALS_PER_SEC`] tokens.
    pub fn try_seed_dial(&self, now: Instant) -> bool {
        let mut g = self.0.seed_tokens.lock().unwrap_or_else(|e| e.into_inner());
        let (tokens, at) = &mut *g;
        if let Some(t) = *at {
            *tokens = (*tokens + now.saturating_duration_since(t).as_secs_f64() * SEED_DIALS_PER_SEC).min(SEED_DIAL_BURST);
        }
        *at = Some(now);
        if *tokens >= 1.0 {
            *tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Reserve a slot for an incoming connection's handshake, or `None` when too many are pending.
    pub fn try_incoming(&self) -> Option<IncomingPermit> {
        Self::take(&self.0.incoming, MAX_INCOMING_HANDSHAKES).then(|| IncomingPermit(self.0.clone()))
    }

    /// Connections currently open.
    pub fn connections(&self) -> usize {
        self.0.total.load(Ordering::Relaxed)
    }

    /// Connections that finished their handshake: what a status line should call "connections".
    /// (`connections()` also counts the ones still being dialled or handshaking.)
    pub fn established(&self) -> usize {
        self.connections().saturating_sub(self.half_open())
    }

    /// Handshakes currently pending, ours and incoming ones.
    pub fn half_open(&self) -> usize {
        self.0.half_open.load(Ordering::Relaxed) + self.0.incoming.load(Ordering::Relaxed)
    }

    /// Whether another connection would be admitted right now.
    pub fn has_room(&self) -> bool {
        self.connections() < self.0.max_total.load(Ordering::Relaxed)
    }
}

impl Drop for ConnPermit {
    fn drop(&mut self) {
        self.0.total.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for HalfOpenPermit {
    fn drop(&mut self) {
        self.0.half_open.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for IncomingPermit {
    fn drop(&mut self) {
        self.0.incoming.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_cap_is_enforced_and_permits_return_on_drop() {
        let b = ConnectionBudget::new(2, 8);
        let p1 = b.try_connection().unwrap();
        let _p2 = b.try_connection().unwrap();
        assert!(b.try_connection().is_none());
        assert_eq!(b.connections(), 2);
        drop(p1);
        assert!(b.has_room());
        assert!(b.try_connection().is_some());
    }

    #[test]
    fn half_open_cap_is_independent() {
        let b = ConnectionBudget::new(100, 1);
        let h = b.try_half_open().unwrap();
        assert!(b.try_half_open().is_none());
        assert_eq!(b.half_open(), 1);
        drop(h);
        assert_eq!(b.half_open(), 0);
        assert!(b.try_half_open().is_some());
    }

    #[test]
    fn pending_dials_leave_room_for_incoming_handshakes() {
        let b = ConnectionBudget::new(1000, 2);
        let _dials: Vec<_> = (0..2).map(|_| b.try_half_open().unwrap()).collect();
        assert!(b.try_half_open().is_none(), "the dial cap is reached");
        let incoming: Vec<_> = (0..MAX_INCOMING_HANDSHAKES).map(|_| b.try_incoming().unwrap()).collect();
        assert!(b.try_incoming().is_none());
        assert_eq!(b.half_open(), 2 + MAX_INCOMING_HANDSHAKES);
        drop(incoming);
        assert_eq!(b.half_open(), 2);
        assert!(b.try_half_open().is_none(), "incoming slots are not dial slots");
    }

    /// Seeds chasing the leechers a tracker named must not take every dial slot: a download that wants
    /// peers at the same moment still finds half of them free.
    #[test]
    fn seeding_dials_leave_half_the_dial_slots_to_downloads() {
        let b = ConnectionBudget::new(1000, 8);
        let seeds: Vec<_> = std::iter::from_fn(|| b.try_seed_half_open()).take(100).collect();
        assert_eq!(seeds.len(), 4);
        let downloads: Vec<_> = std::iter::from_fn(|| b.try_half_open()).take(100).collect();
        assert_eq!(downloads.len(), 4);
        drop(downloads);
        assert!(b.try_seed_half_open().is_none(), "the seeds' share is still taken");
    }

    /// Two hundred seeding torrents answered at once must not dial a storm: over the whole client their
    /// dials start at no more than `SEED_DIALS_PER_SEC` after the first burst.
    #[test]
    fn seeding_dials_are_paced_over_the_whole_client() {
        let b = ConnectionBudget::new(1000, 1000);
        let t0 = Instant::now();
        let burst = (0..1000).filter(|_| b.try_seed_dial(t0)).count();
        assert_eq!(burst, SEED_DIAL_BURST as usize);
        let later = t0 + std::time::Duration::from_secs(2);
        let more = (0..1000).filter(|_| b.try_seed_dial(later)).count();
        assert_eq!(more, (2.0 * SEED_DIALS_PER_SEC) as usize);
        let much_later = later + std::time::Duration::from_secs(3600);
        let refilled = (0..1000).filter(|_| b.try_seed_dial(much_later)).count();
        assert_eq!(refilled, SEED_DIAL_BURST as usize, "a quiet hour saves up no more than one burst");
    }

    #[test]
    fn caps_can_shrink_without_killing_connections() {
        let b = ConnectionBudget::new(3, 3);
        let _a = b.try_connection().unwrap();
        let _c = b.try_connection().unwrap();
        b.set_caps(1, 1);
        assert!(b.try_connection().is_none());
        assert_eq!(b.connections(), 2);
    }
}
