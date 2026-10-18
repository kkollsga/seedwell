//! `.torrent` and magnet parsing plus the shared identity types of Seedwell.
//!
//! * [`Metainfo::from_bytes`]: validated, sanitized torrent description. The
//!   info-hash is computed over the *original* `info` bytes.
//! * [`Magnet::parse`]: magnet URIs (`btih` hex/base32, `btmh`, `dn`, `tr`, `ws`,
//!   `xl`, `so`, `x.pe`).
//! * [`InfoHash`], [`PeerId`] and hex/base32 helpers.
//! * [`sanitize_component`]: the path-safety policy (reject `..`, absolute,
//!   NUL, `/`, empty, over-long; repair invalid UTF-8 and `:`).
#![forbid(unsafe_code)]

mod error;
mod hash;
mod magnet;
mod path;
mod torrent;
mod warning;

pub use error::{Error, MagnetError};
pub use hash::{
    InfoHash, PEER_ID_PREFIX, PeerId, base32_decode, base32_encode, hex_decode, hex_encode,
};
pub use magnet::{FileRange, MAX_MAGNET_LEN, MAX_PEER_HINTS, MAX_SELECT_RANGES, Magnet};
pub use path::{
    MAX_COMPONENT_BYTES, MAX_PATH_BYTES, MAX_PATH_DEPTH, PathError, Repairs, sanitize_component,
};
pub use torrent::{
    FileRef, MAX_FILES, MAX_PIECE_LENGTH, MAX_TIERS, MAX_TRACKERS, MAX_WEB_SEEDS, MetaVersion,
    Metainfo, TORRENT_LIMITS,
};
pub use warning::Warning;

#[cfg(test)]
mod tests;
