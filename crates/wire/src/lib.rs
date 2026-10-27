//! trs-wire: the BitTorrent peer wire protocol (sans-I/O).
//!
//! Covers the handshake, all BEP 3 messages, BEP 6 Fast, the BEP 10 extension
//! protocol with BEP 9 `ut_metadata` and BEP 11 `ut_pex`. Nothing here touches a
//! socket or a clock; the engine feeds bytes in and writes bytes out.
//!
//! Layering:
//! - [`Handshake`] -> [`Decoder`] / [`Encoder`] for framed [`Message`]s.
//! - [`ExtendedMessage`] parses the payload of [`Message::Extended`].
//! - [`MetadataAssembler`] turns `ut_metadata` pieces into verified metadata.
//! - [`Bitfield`], [`allowed_fast_set`], [`block_requests`] are helpers for the engine.
//!
//! Untrusted input never panics: every parser returns [`Result`] and every length is
//! capped before anything is buffered or allocated.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bencode;
pub mod bitfield;
pub mod codec;
pub mod error;
pub mod extended;
pub mod fast;
pub mod handshake;
pub mod message;
pub mod metadata;
pub mod mse;
pub mod pex;
pub mod pipeline;
pub mod state;

pub use bitfield::Bitfield;
pub use codec::{Decoder, Encoder, Limits, MAX_FRAME_LEN, MAX_REQUEST_LEN};
pub use error::{Error, Result};
pub use extended::{
    ExtPolicy, ExtendedHandshake, ExtendedMessage, LOCAL_UT_METADATA_ID, LOCAL_UT_PEX_ID,
    LocalExtendedHandshake,
};
pub use fast::allowed_fast_set;
pub use handshake::{HANDSHAKE_LEN, Handshake, Reserved};
pub use message::{BLOCK_SIZE, BlockRequest, Message, MessageRef, OwnedMessage, piece_header};
pub use metadata::{MAX_METADATA_SIZE, MetadataAssembler, MetadataMessage};
pub use pex::{PexBuilder, PexFlags, PexMessage, PexPeer};
pub use pipeline::{RequestWindow, block_requests, target_depth};
pub use state::{PeerState, RemoteClient};
