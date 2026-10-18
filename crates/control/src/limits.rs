//! Abuse limits at the socket boundary: a bounded line reader and a per-connection rate limiter.

use std::io;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// What [`read_line`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum LineRead {
    /// A complete line is in the buffer (without `\n`, `\r` trimmed).
    Line,
    /// The peer closed the connection (a trailing partial line is discarded).
    Eof,
    /// The line is longer than the cap. The connection should be closed.
    TooLong,
}

/// Append one `\n`-terminated line to `buf`, never holding more than `max` bytes. The caller
/// clears `buf` after handling a line. Cancel-safe: bytes read before a cancellation stay in
/// `buf` and the next call continues the same line.
pub async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>, max: usize) -> io::Result<LineRead> {
    loop {
        let avail = r.fill_buf().await?;
        if avail.is_empty() {
            return Ok(LineRead::Eof);
        }
        match avail.iter().position(|&b| b == b'\n') {
            Some(i) => {
                if buf.len() + i > max {
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(&avail[..i]);
                r.consume(i + 1);
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                return Ok(LineRead::Line);
            }
            None => {
                let n = avail.len();
                if buf.len() + n > max {
                    return Ok(LineRead::TooLong);
                }
                buf.extend_from_slice(avail);
                r.consume(n);
            }
        }
    }
}

/// Token bucket: `rate` calls per second with room for `burst`.
#[derive(Debug)]
pub struct RateLimiter {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    /// New limiter, full.
    pub fn new(rate_per_sec: u32, burst: u32, now: Instant) -> Self {
        let rate = f64::from(rate_per_sec.max(1));
        let burst = f64::from(burst.max(1));
        Self { rate, burst, tokens: burst, last: now }
    }

    /// Take one token, or say how long until one is available.
    pub fn acquire(&mut self, now: Instant) -> Result<(), Duration> {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.rate).min(self.burst);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - self.tokens) / self.rate))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[test]
    fn bucket_limits_and_refills() {
        let t0 = Instant::now();
        let mut rl = RateLimiter::new(10, 3, t0);
        assert!(rl.acquire(t0).is_ok());
        assert!(rl.acquire(t0).is_ok());
        assert!(rl.acquire(t0).is_ok());
        let wait = rl.acquire(t0).unwrap_err();
        assert!(wait > Duration::ZERO && wait <= Duration::from_millis(100), "{wait:?}");
        assert!(rl.acquire(t0 + Duration::from_millis(150)).is_ok());
        // Never exceeds the burst after a long idle.
        let later = t0 + Duration::from_secs(100);
        for _ in 0..3 {
            assert!(rl.acquire(later).is_ok());
        }
        assert!(rl.acquire(later).is_err());
    }

    #[tokio::test]
    async fn reads_lines_and_caps_size() {
        let data: &[u8] = b"hello\r\nworld\npartial";
        let mut r = BufReader::with_capacity(4, data);
        let mut buf = Vec::new();
        assert_eq!(read_line(&mut r, &mut buf, 100).await.unwrap(), LineRead::Line);
        assert_eq!(buf, b"hello");
        buf.clear();
        assert_eq!(read_line(&mut r, &mut buf, 100).await.unwrap(), LineRead::Line);
        assert_eq!(buf, b"world");
        buf.clear();
        assert_eq!(read_line(&mut r, &mut buf, 100).await.unwrap(), LineRead::Eof);

        let long = vec![b'x'; 1000];
        let mut r = BufReader::with_capacity(16, &long[..]);
        let mut buf = Vec::new();
        assert_eq!(read_line(&mut r, &mut buf, 100).await.unwrap(), LineRead::TooLong);
        assert!(buf.len() <= 100, "buffer never exceeds the cap");

        // A line of exactly max bytes is fine; one more is not.
        let mut ok = vec![b'y'; 10];
        ok.push(b'\n');
        let mut r = BufReader::new(&ok[..]);
        let mut buf = Vec::new();
        assert_eq!(read_line(&mut r, &mut buf, 10).await.unwrap(), LineRead::Line);
        let mut r = BufReader::new(&ok[..]);
        let mut buf = Vec::new();
        assert_eq!(read_line(&mut r, &mut buf, 9).await.unwrap(), LineRead::TooLong);
    }
}
