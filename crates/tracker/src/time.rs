//! Virtual time. The sans-I/O core never reads a clock; callers pass `Millis`.

use std::ops::Add;
use std::time::Duration;

/// A point in virtual time, in milliseconds since an arbitrary epoch chosen by
/// the caller.
///
/// For persisted timing (restart safety) the epoch must be stable across
/// launches, so the engine passes Unix milliseconds from the wall clock
/// (`SystemTime`, which keeps advancing while the Mac sleeps).
///
/// The wall clock is not monotonic, and the hub does not pretend it is: it
/// keeps a never-decreasing internal time line and re-bases on every backward
/// step, so a corrected clock excursion neither pins waits in the future nor
/// makes anything early (see the time contract in `hub`). A machine or gate
/// driven directly re-bases its own stored instants on a backward step; a
/// forward jump is always taken at face value.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Millis(pub u64);

impl Millis {
    /// The epoch.
    pub const ZERO: Millis = Millis(0);

    /// Whole seconds as a time point.
    pub const fn from_secs(s: u64) -> Millis {
        Millis(s.saturating_mul(1000))
    }

    /// `self - earlier`, or zero if `earlier` is later.
    pub fn since(self, earlier: Millis) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }

    /// The later of two instants.
    pub fn max(self, other: Millis) -> Millis {
        if self >= other { self } else { other }
    }
}

impl Add<Duration> for Millis {
    type Output = Millis;
    fn add(self, rhs: Duration) -> Millis {
        let ms = u64::try_from(rhs.as_millis()).unwrap_or(u64::MAX);
        Millis(self.0.saturating_add(ms))
    }
}

/// Shorthand for whole-second durations.
pub const fn secs(s: u64) -> Duration {
    Duration::from_secs(s)
}
