//! Raw-span API: the exact original bytes of values inside a document.
//!
//! The BitTorrent info-hash is the SHA-1 of the `info` dictionary *as it
//! appeared on the wire*. Re-encoding a decoded value would silently change the
//! hash for torrents with unsorted keys, so hashing must use these slices.
//!
//! These functions validate with the same grammar as [`decode`](crate::decode)
//! but allocate nothing per value. Slices returned are themselves complete
//! bencode values, so the API composes: `raw_dict_get(raw_dict_get(doc, k1)?, k2)`.

use crate::decode::{Parser, Skip};
use crate::{DecodeLimits, Error};

/// A dictionary entry as raw bytes: `(key, encoded value)`.
pub type RawEntry<'a> = (&'a [u8], &'a [u8]);

/// Raw bytes of every top-level entry of the dictionary `input` as
/// `(key, raw value bytes)` in original order. `input` must be exactly one dict.
pub fn raw_dict_entries(input: &[u8]) -> Result<Vec<RawEntry<'_>>, Error> {
    raw_dict_entries_with_limits(input, DecodeLimits::default())
}

/// Like [`raw_dict_entries`] with explicit limits.
pub fn raw_dict_entries_with_limits(
    input: &[u8],
    limits: DecodeLimits,
) -> Result<Vec<RawEntry<'_>>, Error> {
    let mut p = Parser::new(input, limits)?;
    match p.peek()? {
        b'd' => {}
        _ => return Err(Error::UnexpectedType),
    }
    p.count_item()?;
    p.pos += 1;
    let mut entries: Vec<RawEntry<'_>> = Vec::new();
    let mut sorted = true;
    while p.peek()? != b'e' {
        let key = p.parse_key()?;
        if let Some((prev, _)) = entries.last() {
            if key == *prev {
                return Err(Error::DuplicateKey { pos: p.pos });
            }
            sorted &= key > *prev;
        }
        let start = p.pos;
        p.parse_value::<Skip>(1)?;
        entries.push((key, &input[start..p.pos]));
    }
    p.pos += 1;
    if p.pos != input.len() {
        return Err(Error::TrailingData { pos: p.pos });
    }
    if !sorted {
        let mut keys: Vec<&[u8]> = entries.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        if keys.windows(2).any(|w| w[0] == w[1]) {
            return Err(Error::DuplicateKey { pos: 0 });
        }
    }
    Ok(entries)
}

/// Raw bytes of the value stored under `key` in the dictionary `input`.
/// `Ok(None)` if the key is absent.
pub fn raw_dict_get<'a>(input: &'a [u8], key: &[u8]) -> Result<Option<&'a [u8]>, Error> {
    Ok(raw_dict_entries(input)?
        .into_iter()
        .find(|(k, _)| *k == key)
        .map(|(_, v)| v))
}

/// Raw bytes of every item of the list `input`, in order.
pub fn raw_list_items(input: &[u8]) -> Result<Vec<&[u8]>, Error> {
    let mut p = Parser::new(input, DecodeLimits::default())?;
    match p.peek()? {
        b'l' => {}
        _ => return Err(Error::UnexpectedType),
    }
    p.count_item()?;
    p.pos += 1;
    let mut items = Vec::new();
    while p.peek()? != b'e' {
        let start = p.pos;
        p.parse_value::<Skip>(1)?;
        items.push(&input[start..p.pos]);
    }
    p.pos += 1;
    if p.pos != input.len() {
        return Err(Error::TrailingData { pos: p.pos });
    }
    Ok(items)
}
