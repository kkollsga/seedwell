//! Speed limits for Internet traffic and rate meters for the UI.
//!
//! The LAN is never limited (SPEC §7G.3). A limit is a token bucket: a request may take the bucket into
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

    #[tokio::test]
    async fn unlimited_never_waits() {
        let b = Bucket::default();
        let start = Instant::now();
        b.take(1 << 30).await;
        assert!(start.elapsed() < Duration::from_millis(50));
    }
}
