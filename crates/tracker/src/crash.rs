//! Crash-loop breaker (SPEC 7C.2): after 3 launches within 10 minutes *that
//! followed an unclean exit* every announce except `stopped` is delayed.
//!
//! A user who quits and relaunches the app (to change a setting, after an
//! update, at every login) must not lose announces, so a launch only counts
//! when the run before it did not end with a clean-shutdown marker. The marker
//! is written by the engine when it has finished an orderly shutdown; a crash,
//! a kill, a power cut or a panic leave none behind.
//!
//! Two pieces, both pure: [`LaunchLog`] is the persisted history (the engine
//! stores its text form and calls [`LaunchLog::record_launch`] at start and
//! [`LaunchLog::mark_clean_exit`] at the end of an orderly quit), and
//! [`CrashLoopGuard`] turns the launch times that count into a hold, which the
//! engine feeds to `TrackerHub::hold_until`.

use std::time::Duration;

use crate::time::{Millis, secs};

/// Parameters of the breaker.
#[derive(Clone, Copy, Debug)]
pub struct CrashLoopGuard {
    /// Launches within this window count.
    pub window: Duration,
    /// This many launches (including the current one) trip the breaker.
    pub threshold: usize,
    /// Hold applied when the threshold is just reached; doubles per extra launch.
    pub base_hold: Duration,
    /// Longest hold.
    pub max_hold: Duration,
}

impl Default for CrashLoopGuard {
    fn default() -> Self {
        CrashLoopGuard {
            window: secs(600),
            threshold: 3,
            base_hold: secs(600),
            max_hold: secs(3600),
        }
    }
}

impl CrashLoopGuard {
    /// `launches` are the recent launch times that count (see
    /// [`LaunchLog::record_launch`]) including the current one, in any order.
    /// Returns the time until which announces must be held, if the
    /// breaker trips.
    pub fn evaluate(&self, launches: &[Millis], now: Millis) -> Option<Millis> {
        let recent = launches
            .iter()
            .filter(|t| **t <= now && now.since(**t) <= self.window)
            .count();
        if recent < self.threshold {
            return None;
        }
        let extra = (recent - self.threshold).min(8) as u32;
        let hold = self.base_hold.saturating_mul(1 << extra).min(self.max_hold);
        Some(now + hold)
    }
}

/// How long launch history is kept (more than the breaker's window).
const KEEP: Duration = Duration::from_secs(3600);
/// Most entries kept.
const MAX_ENTRIES: usize = 64;

/// The persisted launch history: one entry per launch with a flag that says
/// whether the run it started ended with a clean-shutdown marker.
///
/// Text form, one entry per line: `<unix ms>` (no marker: the run crashed, is
/// still running, or was written by a build that did not know markers) or
/// `<unix ms> clean`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LaunchLog {
    entries: Vec<(Millis, bool)>,
}

impl LaunchLog {
    /// Read the text form; unreadable lines are skipped.
    pub fn parse(text: &str) -> LaunchLog {
        let mut entries: Vec<(Millis, bool)> = text
            .lines()
            .filter_map(|l| {
                let mut it = l.split_whitespace();
                let t: u64 = it.next()?.parse().ok()?;
                let clean = it.next() == Some("clean");
                Some((Millis(t), clean))
            })
            .collect();
        entries.sort_by_key(|e| e.0);
        LaunchLog { entries }
    }

    /// The text form (see the type docs).
    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|(t, clean)| {
                if *clean {
                    format!("{} clean\n", t.0)
                } else {
                    format!("{}\n", t.0)
                }
            })
            .collect()
    }

    /// Record a launch at `now` and return the launch times that count toward
    /// the breaker: this launch, and every earlier launch within the kept
    /// history whose run did not end with a clean-shutdown marker. Feed the
    /// result to [`CrashLoopGuard::evaluate`].
    ///
    /// A first launch, and any launch after an orderly quit, counts once (the
    /// current one), so graceful relaunches can never reach the threshold.
    pub fn record_launch(&mut self, now: Millis) -> Vec<Millis> {
        self.entries
            .retain(|(t, _)| *t <= now && now.since(*t) <= KEEP);
        self.entries.push((now, false));
        self.entries.sort_by_key(|e| e.0);
        if self.entries.len() > MAX_ENTRIES {
            let cut = self.entries.len() - MAX_ENTRIES;
            self.entries.drain(..cut);
        }
        self.entries
            .iter()
            .filter(|(t, clean)| !*clean || *t == now)
            .map(|(t, _)| *t)
            .collect()
    }

    /// The current run ended in an orderly quit: mark its launch (the latest
    /// entry) clean.
    pub fn mark_clean_exit(&mut self) {
        if let Some(last) = self.entries.last_mut() {
            last.1 = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trips_on_the_third_launch_within_ten_minutes() {
        let g = CrashLoopGuard::default();
        let t = |s| Millis::from_secs(s);
        assert_eq!(g.evaluate(&[t(0), t(100)], t(100)), None);
        assert_eq!(
            g.evaluate(&[t(0), t(100), t(200)], t(200)),
            Some(t(200 + 600))
        );
        assert_eq!(
            g.evaluate(&[t(0), t(100), t(200), t(300)], t(300)),
            Some(t(300 + 1200))
        );
        assert_eq!(
            g.evaluate(&[t(0), t(100), t(200), t(300), t(400), t(500)], t(500)),
            Some(t(500 + 3600))
        );
        // old launches fall out of the window
        assert_eq!(g.evaluate(&[t(0), t(100), t(2000)], t(2000)), None);
    }

    fn t(s: u64) -> Millis {
        Millis::from_secs(s)
    }

    /// One launch against a persisted text log, as the engine does it.
    fn launch(text: &mut String, at: u64) -> Option<Millis> {
        let mut log = LaunchLog::parse(text);
        let counted = log.record_launch(t(at));
        *text = log.render();
        CrashLoopGuard::default().evaluate(&counted, t(at))
    }

    fn quit(text: &mut String) {
        let mut log = LaunchLog::parse(text);
        log.mark_clean_exit();
        *text = log.render();
    }

    #[test]
    fn graceful_relaunches_hold_nothing() {
        let mut text = String::new();
        // Five quit-and-relaunch cycles a minute apart, then thirty seconds apart.
        for i in 0..5 {
            assert_eq!(launch(&mut text, 100 + i * 60), None, "launch {i}");
            quit(&mut text);
        }
        for i in 0..20 {
            assert_eq!(launch(&mut text, 1000 + i * 30), None);
            quit(&mut text);
        }
    }

    #[test]
    fn a_crash_loop_still_holds() {
        let mut text = String::new();
        assert_eq!(launch(&mut text, 100), None);
        assert_eq!(launch(&mut text, 160), None);
        // Third launch in a row without a clean exit: same threshold as before.
        assert_eq!(launch(&mut text, 220), Some(t(220 + 600)));
        assert_eq!(launch(&mut text, 280), Some(t(280 + 1200)));
    }

    #[test]
    fn one_crash_among_graceful_launches_is_not_a_loop() {
        let mut text = String::new();
        assert_eq!(launch(&mut text, 100), None);
        quit(&mut text);
        assert_eq!(launch(&mut text, 160), None); // this run crashes: no marker
        assert_eq!(launch(&mut text, 220), None);
        quit(&mut text);
        for i in 0..5 {
            assert_eq!(launch(&mut text, 300 + i * 20), None);
            quit(&mut text);
        }
    }

    #[test]
    fn a_clean_exit_in_the_middle_breaks_the_streak() {
        let mut text = String::new();
        assert_eq!(launch(&mut text, 100), None); // crashes
        assert_eq!(launch(&mut text, 160), None); // crashes
        // (two unclean runs so far) a graceful run now:
        assert_eq!(launch(&mut text, 220), Some(t(220 + 600)));
        quit(&mut text);
        // the earlier two crashes are still in the window: the next launch trips again
        assert_eq!(launch(&mut text, 280), Some(t(280 + 600)));
        quit(&mut text);
        // ... until they fall out of the ten-minute window
        assert_eq!(launch(&mut text, 900), None);
    }

    #[test]
    fn old_logs_without_markers_read_as_unclean_and_garbage_is_ignored() {
        let log = LaunchLog::parse("100000\nnot a number\n160000\n\n220000 clean\n");
        assert_eq!(log.entries.len(), 3);
        let mut log = log;
        // 100 s, 160 s unclean; 220 s clean; now 280 s
        let counted = log.record_launch(t(280));
        assert_eq!(counted, vec![t(100), t(160), t(280)]);
        // time in the future and history older than an hour are dropped
        let mut log = LaunchLog::parse("5000000\n10\n");
        let counted = log.record_launch(t(4000));
        assert_eq!(counted, vec![t(4000)]);
    }

    #[test]
    fn the_log_stays_small() {
        let mut log = LaunchLog::default();
        for i in 0..200 {
            log.record_launch(Millis(1_000_000 + i));
        }
        assert!(log.render().lines().count() <= 64);
    }
}
