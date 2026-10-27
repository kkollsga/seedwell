//! What the host keeps the computer from while the app works (docs/CROSS-PLATFORM.md §2, "Keep awake"). The
//! host holds the system's own mechanisms (macOS: a sleep assertion, an activity against App Nap).

use crate::types::{Snapshot, TransferState};

/// The power needs of the moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct PowerNeeds {
    /// Something downloads: keep the computer from idle sleep (when the setting allows). Seeding alone never
    /// does; the display still sleeps and the screen still locks.
    pub downloading: bool,
    /// Peers are connected or data flows: the app must not be slowed down in the background (macOS App Nap
    /// throttles its timers and its disk and network work), but the computer may still sleep.
    pub serving: bool,
}

/// The power needs of `s`.
#[uniffi::export]
pub fn power_needs(s: &Snapshot) -> PowerNeeds {
    let rows = s.downloading.iter().chain(&s.hidden_downloading).map(|r| (r.state, r.awaiting_choice));
    needs(rows, s.connections, s.up_rate.max(s.down_rate))
}

fn needs(mut rows: impl Iterator<Item = (TransferState, bool)>, connections: u32, rate: u64) -> PowerNeeds {
    PowerNeeds {
        downloading: rows.any(|(state, awaiting_choice)| state == TransferState::Downloading && !awaiting_choice),
        serving: connections > 0 || rate > 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_download_keeps_the_computer_awake_and_peers_keep_the_app_at_speed() {
        use TransferState::*;
        let n = |rows: &[(TransferState, bool)], c, r| needs(rows.iter().copied(), c, r);
        assert_eq!(n(&[], 0, 0), PowerNeeds { downloading: false, serving: false });
        assert_eq!(n(&[(Downloading, false)], 0, 0), PowerNeeds { downloading: true, serving: false });
        assert!(!n(&[(Downloading, true), (Checking, false), (Metadata, false)], 0, 0).downloading, "waiting for a choice or not downloading yet");
        assert_eq!(n(&[], 3, 0), PowerNeeds { downloading: false, serving: true }, "seeding to connected peers");
        assert!(n(&[], 0, 1_000).serving);
    }
}
