//! Outage detection state (SPEC 7C.4c). Pure data and timing; the hub owns it and acts on it.
//!
//! The hub is `Online` (no extra work at all) or `Offline` (one probe timer). It goes offline
//! when the OS says the network path is gone, when the machine wakes from sleep (a short
//! "checking" state that is not shown as an outage), or when announces to enough distinct
//! hosts fail to even reach them with no sign of life anywhere else.

use std::time::Duration;

use crate::time::Millis;
use crate::types::InfoHash20;

/// Unreachable failures on distinct hosts are only evidence inside this window.
pub const INFER_WINDOW: Duration = Duration::from_secs(120);
/// A failure this soon after a sign of life is no evidence: signs of life are sampled (the
/// engine reports payload traffic once a second), so two hosts failing within the same sampling
/// gap proves nothing about the network.
pub const ALIVE_GRACE: Duration = Duration::from_secs(2);
/// Delays of the first probes after going offline; the last one repeats.
pub const PROBE_DELAYS: [Duration; 4] =
    [Duration::from_secs(30), Duration::from_secs(60), Duration::from_secs(120), Duration::from_secs(300)];
/// After a wake the hub waits this long for the OS to report a usable path, then probes.
pub const WAKE_WAIT: Duration = Duration::from_secs(5);
/// Announces that came due during an outage leave over this window.
pub const RELEASE_SPREAD: Duration = Duration::from_secs(60);
/// A sign of life (inbound connection, DHT packet) triggers a probe at most this often.
pub const HINT_DEBOUNCE: Duration = Duration::from_secs(10);
/// A probe that has not been answered by then counts as failed.
pub const PROBE_DEADLINE: Duration = Duration::from_secs(90);

/// Why the hub is offline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The OS reported that no network path is usable.
    Path,
    /// Announces failed to reach several hosts and nothing else showed life.
    Inference,
    /// The machine woke from sleep and the network is being checked (not shown as an outage).
    Wake,
}

impl Reason {
    /// Word for the log.
    pub fn word(self) -> &'static str {
        match self {
            Reason::Path => "path",
            Reason::Inference => "inference",
            Reason::Wake => "wake",
        }
    }
}

/// What the hub tells the engine about connectivity (drained with `take_net_events`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NetEvent {
    /// Offline from `since`.
    Offline {
        /// Declared at (wall clock).
        at: Millis,
        /// Why.
        reason: Reason,
        /// Trackers whose recent failures were rolled back.
        forgiven: u32,
        /// Announces already due when it was declared.
        due: u32,
    },
    /// An announce was released as the probe.
    Probe {
        /// When.
        at: Millis,
        /// Torrent whose next announce is the probe.
        torrent: InfoHash20,
        /// Tracker slot.
        slot: u16,
        /// Host it goes to.
        host: String,
        /// Probe number since the outage began (1-based).
        n: u32,
    },
    /// A probe was due but nothing was waiting to be announced: nothing to test with.
    ProbeSkipped {
        /// When.
        at: Millis,
    },
    /// Online again: the held announces leave spread over `spread`.
    Online {
        /// When.
        at: Millis,
        /// How long the outage lasted.
        after: Duration,
        /// Announces that had come due.
        due: u32,
        /// The spread window.
        spread: Duration,
        /// Reason it had been offline.
        reason: Reason,
    },
}

/// Connectivity as the UI sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetView {
    /// False while an outage is declared (a wake check does not count).
    pub online: bool,
    /// When the outage began (wall clock).
    pub since: Option<Millis>,
    /// Why.
    pub reason: Option<Reason>,
    /// Probes sent in this outage.
    pub probes: u32,
}

/// The release/hold bookkeeping of one outage.
#[derive(Clone, Debug)]
pub(crate) struct Off {
    pub since: Millis,
    pub reason: Reason,
    /// Index into [`PROBE_DELAYS`] of the next delay.
    pub step: u8,
    /// When to probe; `None`: not until the OS reports a path, or a probe is in flight.
    pub probe_at: Option<Millis>,
    /// The probe in flight and when it left.
    pub probe: Option<((InfoHash20, u16), Millis)>,
    pub probes: u32,
    /// Something other than our own failures says the network works (the OS reported a path,
    /// a wake, a sign of life): with nothing waiting to be announced, go online without a test.
    pub trusted: bool,
}

/// Everything the detector remembers: O(1).
#[derive(Clone, Debug, Default)]
pub(crate) struct Detector {
    /// The OS said the path is down (false when it never said anything).
    pub path_down: bool,
    pub off: Option<Off>,
    /// Latest unreachable failure per distinct host (hash), newest few only.
    pub fails: [(u64, Millis); 4],
    pub nfails: u8,
    /// The last sign of life: an answer, a handshake, a DHT reply, an inbound connection.
    pub last_ok: Millis,
    pub last_probe: Millis,
}

impl Detector {
    /// Record an unreachable failure of `host`; returns how many distinct hosts failed inside
    /// the window `[since, now]`.
    pub fn record_failure(&mut self, host: u64, now: Millis) -> usize {
        let since = Millis(now.0.saturating_sub(INFER_WINDOW.as_millis() as u64)).max(self.last_ok);
        let n = usize::from(self.nfails);
        if let Some(e) = self.fails[..n].iter_mut().find(|e| e.0 == host) {
            e.1 = now;
        } else if n < self.fails.len() {
            self.fails[n] = (host, now);
            self.nfails += 1;
        } else {
            let oldest = (0..n).min_by_key(|i| self.fails[*i].1).unwrap_or(0);
            self.fails[oldest] = (host, now);
        }
        if self.last_ok > Millis::ZERO && now.since(self.last_ok) < ALIVE_GRACE {
            return 0;
        }
        self.fails[..usize::from(self.nfails)].iter().filter(|e| e.1 >= since).count()
    }

    /// A sign of life: failures so far are no longer evidence.
    pub fn alive(&mut self, now: Millis) {
        self.last_ok = self.last_ok.max(now);
        self.nfails = 0;
    }
}

/// `base` with +-20 % jitter from `roll`.
pub fn jittered(base: Duration, roll: u64) -> Duration {
    let ms = base.as_millis() as u64;
    let span = ms / 5; // 20 %
    let off = roll % (2 * span + 1);
    Duration::from_millis(ms - span + off)
}

/// Delay before the probe after `step` earlier ones.
pub fn probe_delay(step: u8) -> Duration {
    PROBE_DELAYS[usize::from(step).min(PROBE_DELAYS.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_twenty_percent() {
        for roll in [0u64, 1, 7, 12345, u64::MAX] {
            let d = jittered(Duration::from_secs(300), roll).as_millis();
            assert!((240_000..=360_000).contains(&d), "{d}");
        }
    }

    #[test]
    fn failures_right_after_a_sign_of_life_are_no_evidence() {
        let mut d = Detector::default();
        let ms = Millis;
        d.alive(ms(10_000));
        assert_eq!(d.record_failure(1, ms(10_700)), 0);
        assert_eq!(d.record_failure(2, ms(11_200)), 0, "two hosts inside one beat of the sign of life");
        // the sign of life stops: once it is 2 s old, what failed since counts
        assert_eq!(d.record_failure(3, ms(12_100)), 3);
    }

    #[test]
    fn evidence_is_distinct_hosts_inside_the_window_after_the_last_sign_of_life() {
        let mut d = Detector::default();
        let t = |s| Millis::from_secs(s);
        assert_eq!(d.record_failure(1, t(1000)), 1);
        assert_eq!(d.record_failure(1, t(1010)), 1, "the same host again is not new evidence");
        assert_eq!(d.record_failure(2, t(1050)), 2);
        // 3 minutes later the first two are stale
        assert_eq!(d.record_failure(3, t(1300)), 1);
        // a sign of life wipes what came before it
        d.alive(t(1310));
        assert_eq!(d.record_failure(4, t(1320)), 1);
    }

    #[test]
    fn probe_delays_end_at_five_minutes() {
        let all: Vec<u64> = (0..8).map(|i| probe_delay(i).as_secs()).collect();
        assert_eq!(all, [30, 60, 120, 300, 300, 300, 300, 300]);
    }
}
