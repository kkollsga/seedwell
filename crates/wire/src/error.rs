//! Error type shared by every parser in this crate.

use core::fmt;

/// Everything that can go wrong while parsing or validating untrusted wire data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The handshake does not start with `\x13BitTorrent protocol`.
    BadProtocolString,
    /// A frame announced a length above the cap for its message type.
    MessageTooLarge {
        /// Announced frame length (without the 4-byte prefix).
        len: u32,
        /// Cap that applied.
        max: u32,
    },
    /// A frame length is impossible for its message id.
    InvalidLength {
        /// Message id.
        id: u8,
        /// Announced frame length.
        len: u32,
    },
    /// A Fast (BEP 6) or extended (BEP 10) message arrived without being negotiated.
    NotNegotiated(u8),
    /// A bitfield has the wrong byte length or non-zero spare bits.
    BadBitfield,
    /// Malformed bencode in an extension payload.
    Bencode(&'static str),
    /// A required key is missing from an extension dictionary.
    MissingField(&'static str),
    /// A key has an out-of-range value.
    InvalidField(&'static str),
    /// A peer used an extension that policy disabled (e.g. PEX on a private torrent).
    ExtensionDisabled(&'static str),
    /// `ut_metadata` size is zero or above [`crate::metadata::MAX_METADATA_SIZE`].
    MetadataSize(u32),
    /// A `ut_metadata` piece index is out of range.
    MetadataPieceIndex(u32),
    /// A `ut_metadata` piece has the wrong length.
    MetadataPieceLength {
        /// Piece index.
        piece: u32,
        /// Received length.
        got: usize,
    },
    /// `finish` was called before all pieces arrived.
    MetadataIncomplete,
    /// The assembled metadata does not hash to the info hash. State was reset.
    MetadataHashMismatch,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::BadProtocolString => f.write_str("bad protocol string in handshake"),
            Error::MessageTooLarge { len, max } => {
                write!(f, "message length {len} exceeds cap {max}")
            }
            Error::InvalidLength { id, len } => {
                write!(f, "invalid length {len} for message id {id}")
            }
            Error::NotNegotiated(id) => write!(f, "message id {id} was not negotiated"),
            Error::BadBitfield => f.write_str("bitfield length or spare bits invalid"),
            Error::Bencode(m) => write!(f, "bencode: {m}"),
            Error::MissingField(k) => write!(f, "missing field `{k}`"),
            Error::InvalidField(k) => write!(f, "invalid field `{k}`"),
            Error::ExtensionDisabled(n) => write!(f, "extension `{n}` is disabled"),
            Error::MetadataSize(n) => write!(f, "metadata size {n} out of range"),
            Error::MetadataPieceIndex(p) => write!(f, "metadata piece {p} out of range"),
            Error::MetadataPieceLength { piece, got } => {
                write!(f, "metadata piece {piece} has wrong length {got}")
            }
            Error::MetadataIncomplete => f.write_str("metadata incomplete"),
            Error::MetadataHashMismatch => f.write_str("metadata hash mismatch"),
        }
    }
}

impl std::error::Error for Error {}

/// Crate result alias.
pub type Result<T, E = Error> = core::result::Result<T, E>;
