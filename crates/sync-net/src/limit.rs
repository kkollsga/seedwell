//! Speed limits for Internet traffic and rate meters for the UI.
//!
//! The LAN is never limited (SPEC §7G.3), but in an isolated QA run that asks for it ([`QA_LIMIT_LAN`]). A limit is a token bucket: a request may take the bucket into
//! debt (a chunk can be bigger than one second's allowance), and the next one waits until it is paid off,
//! so the average holds without splitting chunks.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// The bucket runs on tokio's clock, so tests with paused time see it move.
use tokio::time::Instant as TokioInstant;

use crate::shared::lock;

/// A token bucket in bytes per second (0 = unlimited).
pub struct Bucket {
    rate: AtomicU64,
    state: Mutex<(f64, TokioInstant)>,
}

impl Default for Bucket {
    fn default() -> Self {
        Bucket { rate: AtomicU64::new(0), state: Mutex::new((0.0, TokioInstant::now())) }
    }
}

impl Bucket {
    /// Sets the rate, bytes per second (0 = unlimited).
    pub fn set_rate(&self, bytes_per_s: u64) {
        self.rate.store(bytes_per_s, Ordering::Relaxed);
    }

    /// The rate, bytes per second (0 = unlimited).
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Waits until `n` bytes may pass.
    pub async fn take(&self, n: u64) {
        loop {
            let wait = {
                let rate = self.rate() as f64;
                if rate == 0.0 {
                    return;
                }
                let mut s = lock(&self.state);
                let now = TokioInstant::now();
                // At most half a second of allowance builds up while idle.
                let burst = (rate / 2.0).max(64.0 * 1024.0);
                s.0 = (s.0 + now.duration_since(s.1).as_secs_f64() * rate).min(burst);
                s.1 = now;
                if s.0 >= 0.0 {
                    s.0 -= n as f64;
                    return;
                }
                Duration::from_secs_f64((-s.0 / rate).min(5.0))
            };
            tokio::time::sleep(wait).await;
        }
    }
}

/// Counts bytes and tells a recent rate (an average over the last few seconds).
#[derive(Default)]
pub struct Meter {
    total: AtomicU64,
    sample: Mutex<Option<(u64, Instant, f64)>>,
}

impl Meter {
    /// `n` more bytes went through.
    pub fn add(&self, n: u64) {
        self.total.fetch_add(n, Ordering::Relaxed);
    }

    /// Bytes so far.
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// Bytes per second recently. Each call at least a second after the last updates the average
    /// (weight 0.5, so a stopped transfer reads 0 within a few seconds).
    pub fn rate(&self) -> u64 {
        let total = self.total();
        let now = Instant::now();
        let mut s = lock(&self.sample);
        match *s {
            None => {
                *s = Some((total, now, 0.0));
                0
            }
            Some((last, at, avg)) => {
                let dt = now.duration_since(at).as_secs_f64();
                if dt < 1.0 {
                    return avg as u64;
                }
                let inst = total.saturating_sub(last) as f64 / dt;
                let avg = if avg == 0.0 { inst } else { avg * 0.5 + inst * 0.5 };
                let avg = if inst == 0.0 && avg < 1024.0 { 0.0 } else { avg };
                *s = Some((total, now, avg));
                avg as u64
            }
        }
    }
}

/// **QA only:** with this set to `1`, the speed limits apply to LAN sessions too, so a check over loopback sees a
/// transfer take its time. Honoured only in an isolated run: a state folder inside a temporary folder (as
/// `SEEDWELL_QA_TRASH_DIR` is), never in the user's own Seedwell.
pub const QA_LIMIT_LAN: &str = "SEEDWELL_QA_LIMIT_LAN";

/// Whether the speed limits apply to LAN sessions too: [`QA_LIMIT_LAN`] is `1` (`value`) and the service's state
/// folder (`state`) is inside a temporary folder.
pub fn qa_limit_lan(value: Option<&std::ffi::OsStr>, state: &std::path::Path) -> bool {
    value.is_some_and(|v| v == "1") && seedwell_platform::dirs::in_temp_folder(state)
}

/// The Internet limits of the whole service.
#[derive(Default)]
pub struct Limits {
    /// Upload (serving chunks to devices that are not on the LAN).
    pub up: Bucket,
    /// Download (fetching chunks from devices that are not on the LAN).
    pub down: Bucket,
}

impl Limits {
    /// From the settings (KiB/s, 0 = none).
    pub fn set_kib(&self, up_kib: u32, down_kib: u32) {
        self.up.set_rate(u64::from(up_kib) * 1024);
        self.down.set_rate(u64::from(down_kib) * 1024);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_bucket_holds_its_average() {
        let b = Bucket::default();
        b.set_rate(100 * 1024);
        let start = tokio::time::Instant::now();
        for _ in 0..10 {
            b.take(64 * 1024).await;
        }
        // 640 KiB at 100 KiB/s with a 64 KiB start: about 5.8 s.
        let took = start.elapsed().as_secs_f64();
        assert!((5.0..7.0).contains(&took), "{took}");
    }

    /// The QA switch counts only in an isolated run: a state folder inside a temporary folder, never the user's own.
    #[test]
    fn the_lan_limit_switch_counts_only_in_an_isolated_run() {
        let temp = tempfile::tempdir().unwrap();
        let state = temp.path().join("state");
        let on = Some(std::ffi::OsStr::new("1"));
        assert!(qa_limit_lan(on, &state));
        assert!(!qa_limit_lan(None, &state), "not asked for");
        assert!(!qa_limit_lan(Some(std::ffi::OsStr::new("0")), &state));
        assert!(!qa_limit_lan(Some(std::ffi::OsStr::new("yes")), &state));
        // A state folder outside any temporary folder (as the user's own is; here one in the source tree, never touched),
        // and a relative path.
        let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("state");
        assert!(!seedwell_platform::dirs::in_temp_folder(&here), "the source tree is not in a temporary folder: {}", here.display());
        assert!(!qa_limit_lan(on, &here));
        assert!(!qa_limit_lan(on, std::path::Path::new("state")));
    }

    #[tokio::test]
    async fn unlimited_never_waits() {
        let b = Bucket::default();
        let start = Instant::now();
        b.take(1 << 30).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
