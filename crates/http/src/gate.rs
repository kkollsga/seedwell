//! Rate limiting. The engine injects its global download limiter as a
//! [`RateGate`]; the downloader asks it for every few KiB it is about to
//! accept, so direct downloads and torrent traffic share one budget.

use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Boxed future, so [`RateGate`] stays object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Admission control for downloaded bytes.
pub trait RateGate: Send + Sync + 'static {
    /// Resolve once `bytes` more may be accepted. Called with at most 64 KiB.
    fn acquire(&self, bytes: usize) -> BoxFuture<'_, ()>;
}

/// No limit.
#[derive(Debug, Default, Clone, Copy)]
pub struct Unlimited;

impl RateGate for Unlimited {
    fn acquire(&self, _bytes: usize) -> BoxFuture<'_, ()> {
        Box::pin(std::future::ready(()))
    }
}

/// Smooth pacing limiter (GCRA, "virtual clock"): each grant pushes a virtual
/// queue-drain time forward by `bytes / rate`. Accurate to a single grant over
/// any window and free of timer-granularity drift, unlike a refilled bucket.
#[derive(Debug)]
pub struct TokenBucket {
    rate: AtomicU64,
    burst: Duration,
    next_free: Mutex<Option<Instant>>,
}

impl TokenBucket {
    /// `bytes_per_sec == 0` means unlimited. Up to 100 ms of credit may be
    /// banked while idle, so interactive bursts are not delayed.
    pub fn new(bytes_per_sec: u64) -> Self {
        Self::with_burst(bytes_per_sec, Duration::from_millis(100))
    }

    /// Same with an explicit idle credit window.
    pub fn with_burst(bytes_per_sec: u64, burst: Duration) -> Self {
        TokenBucket {
            rate: AtomicU64::new(bytes_per_sec),
            burst,
            next_free: Mutex::new(None),
        }
    }

    /// Change the limit on the fly (`0` = unlimited).
    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    /// Current limit in bytes per second (`0` = unlimited).
    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Reserve `bytes`; returns how long the caller must wait.
    fn reserve(&self, bytes: usize) -> Duration {
        let rate = self.rate();
        if rate == 0 || bytes == 0 {
            return Duration::ZERO;
        }
        let now = Instant::now();
        let earliest = now.checked_sub(self.burst).unwrap_or(now);
        let cost = Duration::from_secs_f64(bytes as f64 / rate as f64);
        let mut guard = match self.next_free.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let start = guard.map_or(earliest, |t| t.max(earliest));
        *guard = Some(start + cost);
        start.saturating_duration_since(now)
    }
}

impl RateGate for TokenBucket {
    fn acquire(&self, bytes: usize) -> BoxFuture<'_, ()> {
        let wait = self.reserve(bytes);
        Box::pin(async move {
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        })
    }
}
