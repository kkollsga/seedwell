//! Power tiers (SPEC 5.1). Pure transition logic; the torrent actor feeds it observations.
//!
//! | Tier | Resident | Task |
//! |---|---|---|
//! | Cold | compact record only (`<= 256 B` + name) | none |
//! | Warm | + layout, <= 8 peers | yes, light |
//! | Hot | + piece hashes, bitfield, picker state, full peer set | yes |
//!
//! ENGINE.md 2 says only Hot torrents have a task. Warm torrents here do, because they own live
//! sockets; the difference that matters for memory is what they keep resident.

use std::time::{Duration, Instant};

use crate::config::TierTimings;

/// Power tier of a torrent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Tier {
    /// Low power: a record and a tracker timer.
    Cold,
    /// Layout loaded, a few peers.
    Warm,
    /// Hashes, bitfield, picker, full peer set.
    Hot,
}

/// What the torrent actor observed.
#[derive(Clone, Copy, Debug)]
pub struct TierInputs {
    /// Current tier (Warm or Hot; Cold torrents have no actor).
    pub tier: Tier,
    /// The torrent still needs data and is allowed to download.
    pub downloading: bool,
    /// Verification (recheck) is running.
    pub verifying: bool,
    /// Peers that are interested in us right now.
    pub interested_peers: usize,
    /// How long uploads have been continuous, if uploading now.
    pub uploading_for: Option<Duration>,
    /// Last moment anything demanded this torrent (interest, download, verification).
    pub last_demand: Instant,
    /// Now.
    pub now: Instant,
}

/// The tier the torrent should be in now.
pub fn next_tier(i: &TierInputs, t: &TierTimings) -> Tier {
    let demand = i.downloading || i.verifying || i.interested_peers > 0;
    let wants_hot = i.downloading
        || i.verifying
        || i.interested_peers >= t.interested_for_hot
        || i.uploading_for.is_some_and(|d| d >= t.upload_for_hot);
    match i.tier {
        Tier::Cold => {
            // A Cold torrent has no actor; the session promotes it. Reported for completeness.
            if wants_hot { Tier::Hot } else { Tier::Warm }
        }
        Tier::Warm => {
            if wants_hot {
                Tier::Hot
            } else if !demand && i.now.saturating_duration_since(i.last_demand) >= t.warm_to_cold {
                Tier::Cold
            } else {
                Tier::Warm
            }
        }
        Tier::Hot => {
            if wants_hot || demand {
                Tier::Hot
            } else if i.now.saturating_duration_since(i.last_demand) >= t.hot_to_warm {
                Tier::Warm
            } else {
                Tier::Hot
            }
        }
    }
}

/// Time until the tier could change on its own (no new demand), so an idle actor can sleep exactly
/// that long instead of ticking.
pub fn next_check(i: &TierInputs, t: &TierTimings) -> Option<Duration> {
    let idle = i.now.saturating_duration_since(i.last_demand);
    match i.tier {
        Tier::Cold => None,
        Tier::Warm => Some(t.warm_to_cold.saturating_sub(idle)),
        Tier::Hot => Some(t.hot_to_warm.saturating_sub(idle)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(tier: Tier, now: Instant) -> TierInputs {
        TierInputs {
            tier,
            downloading: false,
            verifying: false,
            interested_peers: 0,
            uploading_for: None,
            last_demand: now,
            now,
        }
    }

    #[test]
    fn warm_promotes_on_downloading_interest_or_sustained_upload() {
        let t = TierTimings::default();
        let now = Instant::now();
        let mut i = base(Tier::Warm, now);
        assert_eq!(next_tier(&i, &t), Tier::Warm);
        i.downloading = true;
        assert_eq!(next_tier(&i, &t), Tier::Hot);
        i.downloading = false;
        i.interested_peers = 1;
        assert_eq!(next_tier(&i, &t), Tier::Warm, "one interested peer is not enough");
        i.interested_peers = 2;
        assert_eq!(next_tier(&i, &t), Tier::Hot);
        i.interested_peers = 1;
        i.uploading_for = Some(Duration::from_secs(59));
        assert_eq!(next_tier(&i, &t), Tier::Warm);
        i.uploading_for = Some(Duration::from_secs(60));
        assert_eq!(next_tier(&i, &t), Tier::Hot);
    }

    #[test]
    fn demotion_waits_for_the_configured_idle_time() {
        let t = TierTimings {
            hot_to_warm: Duration::from_secs(10),
            warm_to_cold: Duration::from_secs(20),
            ..TierTimings::default()
        };
        let t0 = Instant::now();
        let mut i = base(Tier::Hot, t0);
        i.now = t0 + Duration::from_secs(9);
        assert_eq!(next_tier(&i, &t), Tier::Hot);
        i.now = t0 + Duration::from_secs(10);
        assert_eq!(next_tier(&i, &t), Tier::Warm);
        let mut w = base(Tier::Warm, t0);
        w.now = t0 + Duration::from_secs(19);
        assert_eq!(next_tier(&w, &t), Tier::Warm);
        w.now = t0 + Duration::from_secs(20);
        assert_eq!(next_tier(&w, &t), Tier::Cold);
        // Any demand keeps it up.
        w.interested_peers = 1;
        assert_eq!(next_tier(&w, &t), Tier::Warm);
        i.now = t0 + Duration::from_secs(1000);
        i.downloading = true;
        assert_eq!(next_tier(&i, &t), Tier::Hot);
    }

    #[test]
    fn next_check_counts_down_idle_time() {
        let t = TierTimings {
            hot_to_warm: Duration::from_secs(10),
            warm_to_cold: Duration::from_secs(20),
            ..TierTimings::default()
        };
        let t0 = Instant::now();
        let mut i = base(Tier::Warm, t0);
        i.now = t0 + Duration::from_secs(5);
        assert_eq!(next_check(&i, &t), Some(Duration::from_secs(15)));
        i.tier = Tier::Hot;
        assert_eq!(next_check(&i, &t), Some(Duration::from_secs(5)));
        i.tier = Tier::Cold;
        assert_eq!(next_check(&i, &t), None);
    }
}
