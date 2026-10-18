//! What the rest of the engine may ask about connectivity without a message round trip
//! (SPEC 7C.4c). The tracker driver owns the decision (`trs_tracker::outage`); it publishes it
//! here, and the hot paths (a peer handshake, an accepted connection, a DHT datagram, a dial)
//! only touch relaxed atomics.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use crate::tracker_driver::{TrackerCmd, TrackerHandle};
use crate::util::unix_ms;

/// A sign of life triggers a probe at most this often (ms).
const HINT_SPACING_MS: u64 = 2000;

#[derive(Default)]
pub(crate) struct NetFlags {
    /// The hub holds everything: an outage, or the short check after a wake. No dials, no DHT.
    held: AtomicBool,
    /// An outage is shown to the user (a wake check is not one).
    shown: AtomicBool,
    /// Unix ms at which the shown outage began.
    since_ms: AtomicU64,
    /// Unix ms of the latest sign of life.
    alive_ms: AtomicU64,
    /// Unix ms of the latest hint sent to the tracker driver.
    hint_ms: AtomicU64,
    tracker: OnceLock<TrackerHandle>,
}

impl NetFlags {
    pub(crate) fn set_tracker(&self, h: TrackerHandle) {
        let _ = self.tracker.set(h);
    }

    /// The hub holds everything (dials and DHT traffic wait).
    #[inline]
    pub(crate) fn held(&self) -> bool {
        self.held.load(Relaxed)
    }

    pub(crate) fn set_held(&self, on: bool) {
        self.held.store(on, Relaxed);
    }

    /// The shown state: `Some(since unix ms)` while an outage is shown.
    pub(crate) fn offline_since(&self) -> Option<u64> {
        self.shown.load(Relaxed).then(|| self.since_ms.load(Relaxed))
    }

    pub(crate) fn set_shown(&self, since_ms: Option<u64>) {
        match since_ms {
            Some(s) => {
                self.since_ms.store(s, Relaxed);
                self.shown.store(true, Relaxed);
            }
            None => self.shown.store(false, Relaxed),
        }
    }

    /// Latest sign of life, unix ms (0: none yet).
    pub(crate) fn alive_ms(&self) -> u64 {
        self.alive_ms.load(Relaxed)
    }

    /// Something proved the network works: a tracker-independent success (a peer handshake, an
    /// accepted connection, a DHT datagram). While an outage is declared it also asks for a probe
    /// (debounced). Online: one clock read and one relaxed store.
    #[inline]
    pub(crate) fn alive(&self) {
        let now = unix_ms();
        self.alive_ms.store(now, Relaxed);
        if self.held.load(Relaxed) && now.saturating_sub(self.hint_ms.load(Relaxed)) >= HINT_SPACING_MS {
            self.hint_ms.store(now, Relaxed);
            if let Some(t) = self.tracker.get() {
                t.send(TrackerCmd::Alive);
            }
        }
    }
}
