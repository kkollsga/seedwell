//! Failure backoff math (SPEC 7C.4). The pace depends on the torrent's
//! [`TorrentRole`]: a download retries after 1, 2, 4, 8, 16, 32 min (cap 1 h), a
//! seed after 15 min, 30 min, 1 h, 2 h, 4 h (cap 6 h), a dormant seed after
//! 30 min, 1 h, 2 h, 4 h, 8 h (cap 12 h); always with 0-20 % jitter on top.

use std::time::Duration;

use crate::response::{MAX_INTERVAL, SLOW_RETRY_SCHEDULE};
use crate::rng::Rng;
use crate::types::TorrentRole;

/// First retry delay.
pub const BACKOFF_BASE: Duration = Duration::from_secs(60);
/// Nominal delay cap.
pub const BACKOFF_CAP: Duration = Duration::from_secs(3600);
/// Jitter is added on top of the nominal delay, uniformly in `[0, 20%)`.
/// Jitter never makes a retry *earlier* than nominal, so every test can use the
/// nominal delay as a hard lower bound. The cap applies to the nominal value;
/// the worst case delay is therefore `1.2 * 1 h`.
pub const BACKOFF_JITTER_PERCENT: u64 = 20;

/// Nominal (jitter-free) delay of a *downloading* torrent after `failures`
/// consecutive failures (>= 1): 1, 2, 4, 8, 16, 32 min, then 1 h.
pub fn nominal_delay(failures: u32) -> Duration {
    nominal_delay_for(TorrentRole::Downloading, failures)
}

/// Nominal (jitter-free) delay for `role` after `failures` consecutive
/// failures (>= 1; 0 counts as 1).
pub fn nominal_delay_for(role: TorrentRole, failures: u32) -> Duration {
    let (base, cap) = match role {
        TorrentRole::Downloading => (BACKOFF_BASE, BACKOFF_CAP),
        TorrentRole::Seeding => (SEEDING_BASE, SEEDING_CAP),
        TorrentRole::SeedingDormant => (DORMANT_BASE, DORMANT_CAP),
    };
    let exp = failures.saturating_sub(1).min(16);
    base.saturating_mul(1u32 << exp).min(cap)
}

/// First retry delay of a seeding torrent.
pub const SEEDING_BASE: Duration = Duration::from_secs(15 * 60);
/// Nominal delay cap of a seeding torrent.
pub const SEEDING_CAP: Duration = Duration::from_secs(6 * 3600);
/// First retry delay of a dormant seed.
pub const DORMANT_BASE: Duration = Duration::from_secs(30 * 60);
/// Nominal delay cap of a dormant seed.
pub const DORMANT_CAP: Duration = Duration::from_secs(12 * 3600);

/// Delay before the next attempt of a *downloading* torrent. See
/// [`retry_delay_for`].
pub fn retry_delay<R: Rng + ?Sized>(
    failures: u32,
    server_hint: Option<Duration>,
    rng: &mut R,
) -> Duration {
    retry_delay_for(TorrentRole::Downloading, failures, server_hint, rng)
}

/// Delay before the next attempt for `role`.
///
/// Without a server hint: the role's nominal schedule plus 0-20 % jitter. With
/// a hint (`Retry-After`, BEP 31 `retry in`, capped at 24 h) the hint is a
/// floor with a role-dependent meaning (SPEC 7C.4): a downloading torrent waits
/// *exactly* the hint (never less than the first backoff step, 1 min, so a
/// `Retry-After: 0` cannot make a retry storm), a seeding torrent waits for
/// whichever is later, the hint or its own schedule.
pub fn retry_delay_for<R: Rng + ?Sized>(
    role: TorrentRole,
    failures: u32,
    server_hint: Option<Duration>,
    rng: &mut R,
) -> Duration {
    let nominal = nominal_delay_for(role, failures);
    let jitter_ms = (nominal.as_millis() as u64) * BACKOFF_JITTER_PERCENT / 100;
    let scheduled = nominal + Duration::from_millis(rng.below(jitter_ms.max(1)));
    match (role, server_hint) {
        (_, None) => scheduled,
        (TorrentRole::Downloading, Some(h)) => h.clamp(BACKOFF_BASE, MAX_INTERVAL),
        (_, Some(h)) => scheduled.max(h.min(MAX_INTERVAL)),
    }
}

/// Nominal delay after `failures` consecutive non-permanent `failure reason`s
/// (>= 1): 30 min, 1 h, 2 h, 4 h, 8 h, then 24 h.
pub fn slow_nominal_delay(failures: u32) -> Duration {
    let i = (failures.max(1) - 1) as usize;
    SLOW_RETRY_SCHEDULE[i.min(SLOW_RETRY_SCHEDULE.len() - 1)]
}

/// Delay before the next attempt after a non-permanent `failure reason`: the
/// slow schedule plus 0-20 % jitter, never less than the server's hint
/// (`retry in`).
pub fn slow_retry_delay<R: Rng + ?Sized>(
    failures: u32,
    server_hint: Option<Duration>,
    rng: &mut R,
) -> Duration {
    let nominal = slow_nominal_delay(failures);
    let jitter_ms = (nominal.as_millis() as u64) * BACKOFF_JITTER_PERCENT / 100;
    let d = nominal + Duration::from_millis(rng.below(jitter_ms.max(1)));
    match server_hint {
        Some(h) => d.max(h.min(MAX_INTERVAL)),
        None => d,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rng::SplitMix64;

    #[test]
    fn nominal_sequence() {
        let secs: Vec<u64> = (1..=9).map(|n| nominal_delay(n).as_secs()).collect();
        assert_eq!(secs, [60, 120, 240, 480, 960, 1920, 3600, 3600, 3600]);
        assert_eq!(nominal_delay(0).as_secs(), 60);
        assert_eq!(nominal_delay(u32::MAX).as_secs(), 3600);
    }

    #[test]
    fn slow_schedule() {
        let h: Vec<u64> = (1..=8)
            .map(|n| slow_nominal_delay(n).as_secs() / 60)
            .collect();
        assert_eq!(h, [30, 60, 120, 240, 480, 1440, 1440, 1440]);
        assert_eq!(slow_nominal_delay(0).as_secs(), 1800);
        assert_eq!(slow_nominal_delay(u32::MAX).as_secs(), 86_400);
        let mut rng = SplitMix64::new(3);
        for n in 1..12 {
            for _ in 0..50 {
                let d = slow_retry_delay(n, None, &mut rng);
                let nom = slow_nominal_delay(n);
                assert!(d >= nom && d < nom + nom / 5 + Duration::from_millis(1));
            }
        }
        assert!(
            slow_retry_delay(1, Some(Duration::from_secs(7200)), &mut rng)
                >= Duration::from_secs(7200)
        );
    }

    #[test]
    fn jitter_bounds() {
        let mut rng = SplitMix64::new(7);
        for n in 1..40 {
            for _ in 0..50 {
                let d = retry_delay(n, None, &mut rng);
                let nom = nominal_delay(n);
                assert!(d >= nom && d < nom + nom / 5 + Duration::from_millis(1));
            }
        }
    }

    #[test]
    fn role_schedules() {
        let mins = |r: TorrentRole| -> Vec<u64> {
            (1..=9)
                .map(|n| nominal_delay_for(r, n).as_secs() / 60)
                .collect()
        };
        assert_eq!(
            mins(TorrentRole::Downloading),
            [1, 2, 4, 8, 16, 32, 60, 60, 60]
        );
        assert_eq!(
            mins(TorrentRole::Seeding),
            [15, 30, 60, 120, 240, 360, 360, 360, 360]
        );
        assert_eq!(
            mins(TorrentRole::SeedingDormant),
            [30, 60, 120, 240, 480, 720, 720, 720, 720]
        );
        let mut rng = SplitMix64::new(9);
        for role in [
            TorrentRole::Downloading,
            TorrentRole::Seeding,
            TorrentRole::SeedingDormant,
        ] {
            for n in 0..30 {
                for _ in 0..30 {
                    let d = retry_delay_for(role, n, None, &mut rng);
                    let nom = nominal_delay_for(role, n);
                    assert!(d >= nom && d <= nom + nom / 5 + Duration::from_millis(1));
                }
            }
        }
    }

    #[test]
    fn hints_downloading_exact_seeding_max() {
        let mut rng = SplitMix64::new(4);
        let secs = Duration::from_secs;
        // downloading: exactly the hint (even below its own schedule), 1 min floor
        assert_eq!(
            retry_delay_for(TorrentRole::Downloading, 5, Some(secs(120)), &mut rng),
            secs(120)
        );
        assert_eq!(
            retry_delay_for(TorrentRole::Downloading, 1, Some(secs(0)), &mut rng),
            secs(60)
        );
        assert_eq!(
            retry_delay_for(
                TorrentRole::Downloading,
                1,
                Some(secs(5 * 86_400)),
                &mut rng
            ),
            MAX_INTERVAL
        );
        // seeding roles: whichever is later
        for role in [TorrentRole::Seeding, TorrentRole::SeedingDormant] {
            let nom = nominal_delay_for(role, 2);
            let d = retry_delay_for(role, 2, Some(secs(60)), &mut rng);
            assert!(
                d >= nom,
                "{role:?}: short hint must not shorten the schedule"
            );
            let d = retry_delay_for(role, 2, Some(secs(20 * 3600)), &mut rng);
            assert_eq!(d, secs(20 * 3600), "{role:?}: a long hint wins");
        }
    }

    #[test]
    fn server_hint_wins_but_is_capped() {
        let mut rng = SplitMix64::new(1);
        assert!(
            retry_delay(1, Some(Duration::from_secs(7200)), &mut rng) >= Duration::from_secs(7200)
        );
        let huge = retry_delay(1, Some(Duration::from_secs(u64::MAX / 4)), &mut rng);
        assert_eq!(huge, MAX_INTERVAL);
    }
}
