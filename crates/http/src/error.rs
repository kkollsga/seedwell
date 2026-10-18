//! Typed errors. Everything is `Clone + PartialEq` so a task can publish its
//! last error through a `watch` channel and tests can assert on it.

use std::fmt;
use std::io;

/// Why a download (or one request of it) failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The URL is not a usable public `http`/`https` URL (credentials in the
    /// URL are refused: authenticated sources are out of scope in v1).
    InvalidUrl(String),
    /// Non-retryable HTTP status (4xx other than 408/429), or retries ran out.
    Http {
        /// Status code.
        status: u16,
    },
    /// Connection reset, DNS failure, truncated body, ...
    Network(String),
    /// TLS failure (certificate, handshake). Never retried: a bad certificate
    /// does not get better by asking again.
    Tls(String),
    /// No data within the idle timeout.
    Timeout,
    /// More than the allowed number of redirects.
    TooManyRedirects,
    /// The redirect chain revisits a URL.
    RedirectLoop,
    /// `https` redirected to plain `http`.
    InsecureRedirect,
    /// Unusable `Location` (unsupported scheme, unparsable, credentials).
    BadRedirect(String),
    /// `Content-Range` / `Content-Length` does not match what was asked. No
    /// byte of such a response is ever written.
    RangeMismatch(String),
    /// The response is not something we can store (e.g. multipart ranges).
    BadResponse(String),
    /// The finished file does not hash to the expected SHA-256. The `.part`
    /// is kept; nothing was moved into the destination.
    ChecksumMismatch {
        /// Expected digest, lowercase hex.
        expected: String,
        /// Digest of the downloaded bytes, lowercase hex.
        actual: String,
    },
    /// No space left on the device (or quota exceeded).
    DiskFull,
    /// Permission denied / read-only volume.
    PermissionDenied(String),
    /// Any other filesystem error.
    Io(String),
    /// Gave up after the configured number of consecutive failures.
    RetriesExhausted {
        /// Failures in a row without progress.
        attempts: u32,
        /// The last underlying error.
        last: Box<Error>,
    },
    /// The task was cancelled.
    Cancelled,
}

impl Error {
    /// Worth another attempt after a backoff.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Network(_) | Error::Timeout => true,
            Error::Http { status } => matches!(status, 408 | 429) || (500..600).contains(status),
            _ => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidUrl(s) => write!(f, "invalid URL: {s}"),
            Error::Http { status } => write!(f, "server answered HTTP {status}"),
            Error::Network(s) => write!(f, "network error: {s}"),
            Error::Tls(s) => write!(f, "TLS error: {s}"),
            Error::Timeout => f.write_str("timed out waiting for data"),
            Error::TooManyRedirects => f.write_str("too many redirects"),
            Error::RedirectLoop => f.write_str("redirect loop"),
            Error::InsecureRedirect => f.write_str("refused redirect from https to http"),
            Error::BadRedirect(s) => write!(f, "bad redirect: {s}"),
            Error::RangeMismatch(s) => write!(f, "server returned the wrong byte range: {s}"),
            Error::BadResponse(s) => write!(f, "unusable response: {s}"),
            Error::ChecksumMismatch { expected, actual } => {
                write!(f, "checksum mismatch: expected {expected}, got {actual}")
            }
            Error::DiskFull => f.write_str("disk full"),
            Error::PermissionDenied(s) => write!(f, "permission denied: {s}"),
            Error::Io(s) => write!(f, "I/O error: {s}"),
            Error::RetriesExhausted { attempts, last } => {
                write!(f, "gave up after {attempts} failed attempts: {last}")
            }
            Error::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        // ENOSPC = 28, EDQUOT = 69 (macOS/Linux differ, 122 on Linux), EROFS = 30.
        match (e.kind(), e.raw_os_error()) {
            (io::ErrorKind::StorageFull, _) | (_, Some(28 | 69 | 122)) => Error::DiskFull,
            (io::ErrorKind::PermissionDenied, _) | (_, Some(30)) => {
                Error::PermissionDenied(e.to_string())
            }
            _ => Error::Io(e.to_string()),
        }
    }
}
