//! Strict, zero-copy bencode (BEP 3) for Seedwell.
//!
//! * [`decode`] borrows from the input and consumes all of it; [`decode_prefix`]
//!   returns the remainder.
//! * [`raw_dict_get`] and friends return the *original* bytes of any value, which
//!   is what the info-hash must be computed over.
//! * Integers must be canonical and fit `i64`; nesting is capped at
//!   [`MAX_DEPTH`]; [`DecodeLimits`] bounds item count and input size.
//! * Unsorted dictionary keys are accepted (real torrents have them) and
//!   reported by [`Value::is_canonical`]; duplicate keys are rejected.
//! * [`Encoder`] / [`encode`] always emit sorted keys.
#![forbid(unsafe_code)]

mod decode;
mod encode;
mod error;
mod raw;
mod value;

pub use decode::{
    DecodeLimits, MAX_DEPTH, decode, decode_prefix, decode_prefix_with_limits, decode_with_limits,
};
pub use encode::{Encoder, encode};
pub use error::Error;
pub use raw::{
    RawEntry, raw_dict_entries, raw_dict_entries_with_limits, raw_dict_get, raw_list_items,
};
pub use value::Value;

#[cfg(test)]
mod tests;
