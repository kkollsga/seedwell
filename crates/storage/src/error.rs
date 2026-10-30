//! Typed errors for the storage layer.

use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::layout::LayoutError;

/// Everything that can go wrong in `trs-storage`.
///
/// File indices refer to the torrent's [`Layout`](crate::Layout); the engine resolves them to
/// paths for display. Variants that mean "the data is not there" are kept distinct from generic
/// I/O errors so the engine can move the torrent to `Missing data` instead of `Error`.
#[derive(Debug)]
pub enum StorageError {
    /// A file (or the directory/volume holding it) does not exist. Nothing was created.
    MissingData {
        /// Index of the file in the layout.
        file: u32,
    },
    /// A file exists but is shorter than the layout requires (modified on disk).
    Truncated {
        /// Index of the file in the layout.
        file: u32,
    },
    /// The save root (or its volume) is missing; we refuse to re-create it silently.
    MissingRoot,
    /// Underlying OS error.
    Io {
        /// File the error relates to, when known.
        file: Option<u32>,
        /// The OS error.
        source: io::Error,
    },
    /// A write was attempted while the torrent is in seeding (read-only) mode.
    ReadOnly,
    /// Piece/offset/length outside the torrent.
    OutOfRange,
    /// Block is not 16 KiB aligned or has the wrong length for its position.
    InvalidBlock,
    /// Piece hashes are not attached (or have the wrong length).
    NoHashes,
    /// The layout was dropped (Cold tier); rebuild it first.
    LayoutDropped,
    /// A rebuilt layout does not match the one that was dropped.
    LayoutMismatch,
    /// A queue is full; retry later. The request had no effect.
    Busy,
    /// Too many partially received pieces are open for this torrent.
    TooManyPieces,
    /// The disk threads have shut down.
    Shutdown,
    /// The operation was cancelled through its [`CancelToken`](crate::CancelToken).
    Cancelled,
    /// A move destination already exists and is not ours; we never overwrite.
    DestinationExists {
        /// Index of the file in the layout.
        file: u32,
    },
    /// The finalize journal is unreadable or belongs to a different layout.
    CorruptJournal,
    /// Finalize was stopped by a fault-injection hook (tests only).
    Interrupted,
    /// A piece failed verification while its data was being copied (corrupt source).
    VerifyFailed {
        /// Index of the piece that did not match its hash.
        piece: u32,
    },
    /// The destination volume lacks room for the copy plus the safety reserve.
    InsufficientSpace {
        /// Bytes required (data plus reserve).
        needed: u64,
        /// Bytes available on the destination volume.
        available: u64,
    },
    /// The relocation journal is in a state that does not allow this call.
    InvalidState,
    /// A file is locked (macOS `uchg`/`schg`/append-only flag, Finder "Locked"; on Windows open in
    /// another program that does not let it be deleted): it cannot be renamed or removed. Found
    /// before anything was touched, or by the move itself (Windows), which then rolled back.
    Locked {
        /// Index of the file in the layout.
        file: u32,
        /// Where the file is.
        path: PathBuf,
    },
    /// A folder the move has to change cannot be written (permissions, or locked). Found before
    /// anything was touched.
    NoPermission {
        /// The folder.
        path: PathBuf,
    },
    /// Invalid layout input.
    Layout(LayoutError),
}

impl StorageError {
    /// Wraps an I/O error, mapping "does not exist" to [`StorageError::MissingData`].
    pub(crate) fn from_io(file: u32, e: io::Error) -> Self {
        if is_not_found(&e) {
            StorageError::MissingData { file }
        } else if e.kind() == io::ErrorKind::UnexpectedEof {
            StorageError::Truncated { file }
        } else {
            StorageError::Io {
                file: Some(file),
                source: e,
            }
        }
    }

    /// Wraps an I/O error that is not tied to one file.
    pub(crate) fn io(e: io::Error) -> Self {
        StorageError::Io {
            file: None,
            source: e,
        }
    }
}

/// An OS error code that means "this place cannot be written right now, but may be later":
/// disk full, quota, read-only file system, permission denied.
pub fn os_error_is_write_blocked(code: i32) -> bool {
    use io::ErrorKind::*;
    matches!(io::Error::from_raw_os_error(code).kind(), StorageFull | QuotaExceeded | ReadOnlyFilesystem | PermissionDenied)
}

impl StorageError {
    /// A write that failed for a reason that may go away by itself (disk full, read-only,
    /// permission denied): the torrent keeps what it has and tries again later instead of
    /// failing for good.
    pub fn is_write_blocked(&self) -> bool {
        matches!(self, StorageError::Io { source, .. } if source.raw_os_error().is_some_and(os_error_is_write_blocked))
    }
}

/// The last component of a path, for messages.
pub fn leaf(p: &std::path::Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// ENOENT and ENOTDIR both mean the path (or a parent) is not there.
pub(crate) fn is_not_found(e: &io::Error) -> bool {
    trs_platform::fs::is_not_found(e)
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingData { file } => write!(f, "missing data: file #{file} not found"),
            Self::Truncated { file } => write!(f, "file #{file} is shorter than expected"),
            Self::MissingRoot => write!(f, "save location (or its volume) is missing"),
            Self::Io {
                file: Some(n),
                source,
            } => write!(f, "i/o error on file #{n}: {source}"),
            Self::Io { file: None, source } => write!(f, "i/o error: {source}"),
            Self::ReadOnly => write!(f, "torrent storage is read-only (seeding mode)"),
            Self::OutOfRange => write!(f, "range outside torrent"),
            Self::InvalidBlock => write!(f, "invalid block position or length"),
            Self::NoHashes => write!(f, "piece hashes not attached"),
            Self::LayoutDropped => write!(f, "layout is dropped (cold)"),
            Self::LayoutMismatch => write!(f, "rebuilt layout differs from the original"),
            Self::Busy => write!(f, "disk queue full"),
            Self::TooManyPieces => write!(f, "too many partial pieces in flight"),
            Self::Shutdown => write!(f, "disk threads shut down"),
            Self::Cancelled => write!(f, "cancelled"),
            Self::DestinationExists { file } => {
                write!(f, "destination of file #{file} already exists")
            }
            Self::CorruptJournal => write!(f, "finalize journal is corrupt or foreign"),
            Self::Interrupted => write!(f, "interrupted by fault injection"),
            Self::VerifyFailed { piece } => write!(f, "piece {piece} failed verification while copying"),
            Self::InsufficientSpace { needed, available } => {
                write!(f, "not enough free space: need {needed} bytes, {available} available")
            }
            Self::InvalidState => write!(f, "relocation journal is not in a state that allows this"),
            Self::Locked { path, .. } => {
                write!(f, "\u{201c}{}\u{201d} {}", leaf(path), trs_platform::fs::LOCKED_WORDS)
            }
            Self::NoPermission { path } => write!(f, "No permission to change \u{201c}{}\u{201d}", leaf(path)),
            Self::Layout(e) => write!(f, "invalid layout: {e}"),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Layout(e) => Some(e),
            _ => None,
        }
    }
}

impl From<LayoutError> for StorageError {
    fn from(e: LayoutError) -> Self {
        Self::Layout(e)
    }
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, StorageError>;

#[cfg(test)]
mod blocked_tests {
    use super::*;

    #[test]
    fn full_readonly_and_denied_writes_are_blocked_not_fatal() {
        #[cfg(unix)]
        let (blocked, other) = ([libc::ENOSPC, libc::EROFS, libc::EACCES, libc::EDQUOT], libc::EIO);
        // ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL, ERROR_ACCESS_DENIED; ERROR_CRC.
        #[cfg(windows)]
        let (blocked, other) = ([112, 39, 5], 23);
        for code in blocked {
            let e = StorageError::from_io(0, io::Error::from_raw_os_error(code));
            assert!(e.is_write_blocked(), "os error {code}");
        }
        assert!(!StorageError::from_io(0, io::Error::from_raw_os_error(other)).is_write_blocked());
        assert!(!StorageError::MissingData { file: 0 }.is_write_blocked());
        assert!(!StorageError::MissingRoot.is_write_blocked());
    }
}
