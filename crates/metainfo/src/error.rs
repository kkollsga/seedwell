use core::fmt;

use crate::path::PathError;

/// Why a `.torrent` (or bare info dictionary) was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The bencode layer rejected the bytes.
    Bencode(trs_bencode::Error),
    /// The named structure must be a dictionary.
    NotADictionary(&'static str),
    /// A required key is absent.
    MissingKey(&'static str),
    /// A key has the wrong bencode type (or an out-of-range value).
    WrongType(&'static str),
    /// `piece length` is zero or larger than [`MAX_PIECE_LENGTH`](crate::MAX_PIECE_LENGTH).
    InvalidPieceLength,
    /// `pieces` length is not a multiple of 20.
    BadPiecesLength,
    /// `pieces` does not hold exactly `ceil(total_length / piece_length)` hashes.
    PieceCountMismatch {
        /// Hashes required by the file sizes.
        expected: u64,
        /// Hashes present.
        actual: u64,
    },
    /// The torrent contains no data (no files, or total length zero).
    EmptyTorrent,
    /// More files than [`MAX_FILES`](crate::MAX_FILES).
    TooManyFiles,
    /// A file length is negative.
    NegativeLength,
    /// Sum of file lengths (or an offset) overflows `u64`.
    LengthOverflow,
    /// Both `length` and `files` are present, so the layout is ambiguous.
    AmbiguousLayout,
    /// `meta version` is neither 1 nor 2.
    UnsupportedMetaVersion(i64),
    /// A path component failed sanitization; the torrent is rejected, not repaired.
    Path(PathError),
    /// Two files resolve to the same sanitized path.
    DuplicatePath,
    /// Accumulated path data exceeds what the compact file table can address.
    PathsTooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bencode(e) => write!(f, "bencode: {e}"),
            Error::NotADictionary(w) => write!(f, "{w} is not a dictionary"),
            Error::MissingKey(k) => write!(f, "missing key `{k}`"),
            Error::WrongType(k) => write!(f, "key `{k}` has the wrong type or value"),
            Error::InvalidPieceLength => f.write_str("invalid piece length"),
            Error::BadPiecesLength => f.write_str("`pieces` length is not a multiple of 20"),
            Error::PieceCountMismatch { expected, actual } => {
                write!(f, "expected {expected} piece hashes, found {actual}")
            }
            Error::EmptyTorrent => f.write_str("torrent has no data"),
            Error::TooManyFiles => f.write_str("too many files"),
            Error::NegativeLength => f.write_str("negative file length"),
            Error::LengthOverflow => f.write_str("total length overflows u64"),
            Error::AmbiguousLayout => f.write_str("both `length` and `files` present"),
            Error::UnsupportedMetaVersion(v) => write!(f, "unsupported meta version {v}"),
            Error::Path(e) => write!(f, "unsafe path: {e}"),
            Error::DuplicatePath => f.write_str("duplicate file path"),
            Error::PathsTooLarge => f.write_str("path data too large"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Bencode(e) => Some(e),
            Error::Path(e) => Some(e),
            _ => None,
        }
    }
}

impl From<trs_bencode::Error> for Error {
    fn from(e: trs_bencode::Error) -> Self {
        Error::Bencode(e)
    }
}

impl From<PathError> for Error {
    fn from(e: PathError) -> Self {
        Error::Path(e)
    }
}

/// Why a magnet URI was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MagnetError {
    /// Does not start with `magnet:?`.
    NotMagnet,
    /// Longer than [`MAX_MAGNET_LEN`](crate::MAX_MAGNET_LEN).
    TooLong,
    /// A `%` escape is truncated or not hex.
    InvalidPercentEncoding,
    /// No `xt=urn:btih:` or `xt=urn:btmh:` parameter.
    MissingInfoHash,
    /// An `xt` hash is malformed (wrong length, bad hex/base32, bad multihash).
    InvalidInfoHash,
    /// Two `xt` parameters of the same kind disagree.
    ConflictingInfoHash,
    /// The named parameter has an unparseable value.
    InvalidParameter(&'static str),
}

impl fmt::Display for MagnetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MagnetError::NotMagnet => f.write_str("not a magnet URI"),
            MagnetError::TooLong => f.write_str("magnet URI too long"),
            MagnetError::InvalidPercentEncoding => f.write_str("invalid percent-encoding"),
            MagnetError::MissingInfoHash => f.write_str("no btih/btmh info-hash"),
            MagnetError::InvalidInfoHash => f.write_str("malformed info-hash"),
            MagnetError::ConflictingInfoHash => f.write_str("conflicting info-hashes"),
            MagnetError::InvalidParameter(p) => write!(f, "invalid `{p}` parameter"),
        }
    }
}

impl std::error::Error for MagnetError {}
