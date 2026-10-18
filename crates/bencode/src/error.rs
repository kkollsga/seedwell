use core::fmt;

/// Everything that can go wrong while decoding bencode. All variants carry a
/// byte offset (where meaningful) so tracker/metainfo errors can point at the
/// offending input without re-parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Input ended in the middle of a value.
    UnexpectedEof,
    /// A byte that cannot start (or continue) a value at this position.
    InvalidByte { pos: usize, byte: u8 },
    /// Integer syntax violation: empty, `-0`, leading zeros, stray characters.
    InvalidInteger { pos: usize },
    /// Integer is syntactically fine but does not fit in an `i64`.
    IntegerOverflow { pos: usize },
    /// Byte-string length prefix is malformed (leading zeros, no digits, too large).
    InvalidLength { pos: usize },
    /// Nesting is deeper than [`MAX_DEPTH`](crate::MAX_DEPTH).
    DepthExceeded,
    /// More values than [`DecodeLimits::max_items`](crate::DecodeLimits::max_items).
    TooManyItems,
    /// Input longer than [`DecodeLimits::max_bytes`](crate::DecodeLimits::max_bytes).
    InputTooLarge,
    /// `decode` was given bytes after the end of the first value.
    TrailingData { pos: usize },
    /// The same dictionary key appears twice. Rejected because two parsers could
    /// otherwise disagree on which value wins (an info-hash confusion vector).
    DuplicateKey { pos: usize },
    /// The raw-span API was asked about a value of the wrong type.
    UnexpectedType,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof => f.write_str("unexpected end of input"),
            Error::InvalidByte { pos, byte } => {
                write!(f, "invalid byte 0x{byte:02x} at offset {pos}")
            }
            Error::InvalidInteger { pos } => write!(f, "malformed integer at offset {pos}"),
            Error::IntegerOverflow { pos } => write!(f, "integer overflows i64 at offset {pos}"),
            Error::InvalidLength { pos } => write!(f, "malformed string length at offset {pos}"),
            Error::DepthExceeded => f.write_str("nesting too deep"),
            Error::TooManyItems => f.write_str("too many items"),
            Error::InputTooLarge => f.write_str("input too large"),
            Error::TrailingData { pos } => write!(f, "trailing data at offset {pos}"),
            Error::DuplicateKey { pos } => write!(f, "duplicate dictionary key at offset {pos}"),
            Error::UnexpectedType => f.write_str("value has an unexpected type"),
        }
    }
}

impl std::error::Error for Error {}
