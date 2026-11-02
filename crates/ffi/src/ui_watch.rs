//! The hosts' hang watchdog (docs/CROSS-PLATFORM.md §1: the UI thread never waits): how often a host asks its UI thread
//! for a beat, when a late beat is a stall worth noting, when a stall ends a test run, and the words for it. The host
//! runs the watch on a thread of its own (the Mac: `HangWatchdog`).

use crate::Core;

/// How a host watches its UI thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Record)]
pub struct UiWatchdog {
    /// How often the watch thread asks the UI thread for a beat.
    pub ping_ms: u32,
    /// A beat this late is a stall: noted ([`Core::ui_stall`]) once it ends.
    pub report_ms: u32,
    /// In a snapshot render or a QA run, a stall this long ends the run with an error (a hang never passes unnoticed).
    pub fail_ms: u32,
}

/// The watchdog's timing (a ping every 250 ms; a stall over 1 s is noted; over 2 s a test run fails).
#[uniffi::export]
pub fn ui_watchdog() -> UiWatchdog {
    UiWatchdog { ping_ms: 250, report_ms: 1_000, fail_ms: 2_000 }
}

/// The line for a stall of `stalled_ms` after the UI action `last_action` (empty: none known): "the UI thread did not
/// answer for 1.4 s (last action: Pause)". `failing`: it ends a test run.
#[uniffi::export]
pub fn ui_stall_line(stalled_ms: u64, last_action: String, failing: bool) -> String {
    let secs = stalled_ms as f64 / 1000.0;
    let last = if last_action.trim().is_empty() { "none".to_string() } else { last_action.trim().to_string() };
    let base = format!("the UI thread did not answer for {secs:.1} s (last action: {last})");
    if failing { format!("{base}: the app hangs, so this run fails") } else { base }
}

#[uniffi::export]
impl Core {
    /// The host's UI thread was held up for `stalled_ms` (its watchdog saw a late beat; `last_action`: what the user, QA
    /// or the renderer did last). Noted in the developer log (`Decision`, event `ui-stall`, while developer mode is on).
    /// Cheap and never blocks: called from the watch thread. The line, for the host's own log (stderr in debug builds).
    pub fn ui_stall(&self, stalled_ms: u64, last_action: String) -> String {
        let line = ui_stall_line(stalled_ms, last_action, false);
        self.backend.note_decision("ui-stall", line.clone());
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stall_is_said_with_its_length_and_the_last_action() {
        assert_eq!(ui_stall_line(1_420, "Pause".into(), false), "the UI thread did not answer for 1.4 s (last action: Pause)");
        assert_eq!(ui_stall_line(2_000, " ".into(), true), "the UI thread did not answer for 2.0 s (last action: none): the app hangs, so this run fails");
        let w = ui_watchdog();
        assert!(w.ping_ms < w.report_ms && w.report_ms < w.fail_ms);
    }

    #[test]
    fn a_stall_is_noted_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::new(crate::CoreOptions {
            config_dir: dir.path().display().to_string(),
            mock: true,
            allow_loopback_trackers: false,
            forbidden_root: None,
        })
        .unwrap();
        let at = std::time::Instant::now();
        assert!(core.ui_stall(1_500, "render settings".into()).contains("1.5 s"));
        assert!(at.elapsed() < std::time::Duration::from_millis(50));
    }
}
