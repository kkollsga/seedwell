//! Minimal allocation-free bencode reader/writer for the small extension dictionaries.
//!
//! Why not `trs-bencode`: extension payloads are tiny, flat and arrive at line rate
//! from untrusted peers, and `ut_metadata` data messages append raw bytes after the
//! dictionary. A borrowed, depth-limited walker with no heap use is all we need, and it
//! keeps this crate dependency-free. Semantics follow the same strict integer rules.

use crate::error::{Error, Result};
use bytes::BufMut;

/// Nesting limit; extension messages are at most two levels deep.
const MAX_DEPTH: u32 = 8;

/// A parsed value. `List`/`Dict` carry the raw span (including `l`/`d` and `e`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Val<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(&'a [u8]),
    Dict(&'a [u8]),
}

impl<'a> Val<'a> {
    pub(crate) fn as_int(&self) -> Option<i64> {
        if let Val::Int(i) = self {
            Some(*i)
        } else {
            None
        }
    }
    pub(crate) fn as_u32(&self) -> Option<u32> {
        self.as_int().and_then(|i| u32::try_from(i).ok())
    }
    pub(crate) fn as_bytes(&self) -> Option<&'a [u8]> {
        if let Val::Bytes(b) = self {
            Some(b)
        } else {
            None
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn int_until(&mut self, end: u8) -> Result<i64> {
        let start = self.pos;
        let neg = self.peek() == Some(b'-');
        if neg {
            self.pos += 1;
        }
        let ds = self.pos;
        let mut v: i64 = 0;
        while let Some(c @ b'0'..=b'9') = self.peek() {
            // Accumulate negatively so i64::MIN parses; overflow is an error.
            v = v
                .checked_mul(10)
                .and_then(|v| v.checked_sub(i64::from(c - b'0')))
                .ok_or(Error::Bencode("integer overflow"))?;
            self.pos += 1;
        }
        let digits = self.pos - ds;
        if digits == 0 || (digits > 1 && self.buf.get(ds) == Some(&b'0')) {
            return Err(Error::Bencode("bad integer"));
        }
        if neg && v == 0 {
            return Err(Error::Bencode("negative zero"));
        }
        if self.peek() != Some(end) {
            self.pos = start;
            return Err(Error::Bencode("bad integer terminator"));
        }
        self.pos += 1;
        if neg {
            Ok(v)
        } else {
            v.checked_neg().ok_or(Error::Bencode("integer overflow"))
        }
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        // The length digits reuse the integer parser, terminated by ':'.
        let n = self.int_until(b':')?;
        let n = usize::try_from(n).map_err(|_| Error::Bencode("negative length"))?;
        let end = self
            .pos
            .checked_add(n)
            .ok_or(Error::Bencode("length overflow"))?;
        let s = self
            .buf
            .get(self.pos..end)
            .ok_or(Error::Bencode("truncated string"))?;
        self.pos = end;
        Ok(s)
    }

    fn value(&mut self, depth: u32) -> Result<Val<'a>> {
        if depth > MAX_DEPTH {
            return Err(Error::Bencode("nesting too deep"));
        }
        let start = self.pos;
        match self.peek().ok_or(Error::Bencode("unexpected end"))? {
            b'i' => {
                self.pos += 1;
                Ok(Val::Int(self.int_until(b'e')?))
            }
            b'0'..=b'9' => Ok(Val::Bytes(self.bytes()?)),
            b'l' => {
                self.pos += 1;
                while self.peek().ok_or(Error::Bencode("unterminated list"))? != b'e' {
                    self.value(depth + 1)?;
                }
                self.pos += 1;
                Ok(Val::List(&self.buf[start..self.pos]))
            }
            b'd' => {
                self.pos += 1;
                while self.peek().ok_or(Error::Bencode("unterminated dict"))? != b'e' {
                    if !matches!(self.peek(), Some(b'0'..=b'9')) {
                        return Err(Error::Bencode("dict key must be a string"));
                    }
                    self.bytes()?;
                    self.value(depth + 1)?;
                }
                self.pos += 1;
                Ok(Val::Dict(&self.buf[start..self.pos]))
            }
            _ => Err(Error::Bencode("unexpected byte")),
        }
    }
}

/// Parse one value from the front of `buf`; returns it and the bytes consumed.
pub(crate) fn parse_prefix(buf: &[u8]) -> Result<(Val<'_>, usize)> {
    let mut r = Reader { buf, pos: 0 };
    let v = r.value(0)?;
    Ok((v, r.pos))
}

/// Parse a top-level dictionary followed by optional trailing bytes.
pub(crate) fn dict_prefix(buf: &[u8]) -> Result<(Dict<'_>, &[u8])> {
    match parse_prefix(buf)? {
        (Val::Dict(raw), used) => Ok((Dict(raw), buf.get(used..).unwrap_or(&[]))),
        _ => Err(Error::Bencode("expected dictionary")),
    }
}

/// A validated dictionary span; iteration cannot fail.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Dict<'a>(pub(crate) &'a [u8]);

impl<'a> Dict<'a> {
    /// Iterate `(key, value)` pairs. The span was validated when it was parsed.
    pub(crate) fn iter(&self) -> DictIter<'a> {
        DictIter {
            r: Reader {
                buf: self.0,
                pos: 1,
            },
        }
    }

    pub(crate) fn get(&self, key: &[u8]) -> Option<Val<'a>> {
        self.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }
}

pub(crate) struct DictIter<'a> {
    r: Reader<'a>,
}

impl<'a> Iterator for DictIter<'a> {
    type Item = (&'a [u8], Val<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.r.peek()? == b'e' {
            return None;
        }
        let k = self.r.bytes().ok()?;
        let v = self.r.value(1).ok()?;
        Some((k, v))
    }
}

// ---- writer ----

fn put_u64<B: BufMut>(out: &mut B, mut n: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.put_slice(&tmp[i..]);
}

pub(crate) fn put_int<B: BufMut>(out: &mut B, n: i64) {
    out.put_u8(b'i');
    if n < 0 {
        out.put_u8(b'-');
    }
    put_u64(out, n.unsigned_abs());
    out.put_u8(b'e');
}

pub(crate) fn put_bytes<B: BufMut>(out: &mut B, s: &[u8]) {
    put_u64(out, s.len() as u64);
    out.put_u8(b':');
    out.put_slice(s);
}

/// Dictionary entry with an integer value.
pub(crate) fn put_kv_int<B: BufMut>(out: &mut B, key: &str, n: i64) {
    put_bytes(out, key.as_bytes());
    put_int(out, n);
}

/// Dictionary entry with a byte-string value.
pub(crate) fn put_kv_bytes<B: BufMut>(out: &mut B, key: &str, v: &[u8]) {
    put_bytes(out, key.as_bytes());
    put_bytes(out, v);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ints() {
        assert_eq!(parse_prefix(b"i0e").unwrap().0, Val::Int(0));
        assert_eq!(parse_prefix(b"i-5e").unwrap().0, Val::Int(-5));
        assert_eq!(
            parse_prefix(b"i9223372036854775807e").unwrap().0,
            Val::Int(i64::MAX)
        );
        assert_eq!(
            parse_prefix(b"i-9223372036854775808e").unwrap().0,
            Val::Int(i64::MIN)
        );
        for bad in [
            &b"i-0e"[..],
            b"i03e",
            b"ie",
            b"i-e",
            b"i9223372036854775808e",
            b"i-9223372036854775809e",
            b"i1",
            b"i1x",
            b"i--1e",
            b"",
        ] {
            assert!(parse_prefix(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn strings_and_dicts() {
        assert_eq!(parse_prefix(b"3:abcXX").unwrap(), (Val::Bytes(b"abc"), 5));
        assert!(parse_prefix(b"4:abc").is_err());
        assert!(parse_prefix(b"-1:a").is_err());
        assert!(parse_prefix(b"99999999999999999999:a").is_err());
        let (d, rest) = dict_prefix(b"d1:ai1e1:bl1:xee_tail").unwrap();
        assert_eq!(rest, b"_tail");
        assert_eq!(d.get(b"a"), Some(Val::Int(1)));
        assert!(matches!(d.get(b"b"), Some(Val::List(_))));
        assert_eq!(d.get(b"c"), None);
        assert!(dict_prefix(b"di1ei2ee").is_err());
        assert!(dict_prefix(b"d1:a").is_err());
        assert!(dict_prefix(b"li1ee").is_err());
        assert_eq!(dict_prefix(b"de").unwrap().0.iter().count(), 0);
    }

    #[test]
    fn depth_limit() {
        let mut s = vec![b'l'; 100];
        s.extend(vec![b'e'; 100]);
        assert!(parse_prefix(&s).is_err());
        let mut s = vec![b'l'; 8];
        s.extend(vec![b'e'; 8]);
        assert!(parse_prefix(&s).is_ok());
    }

    #[test]
    fn writer() {
        let mut v = Vec::new();
        put_int(&mut v, -42);
        put_int(&mut v, 0);
        put_int(&mut v, i64::MIN);
        put_bytes(&mut v, b"hello");
        assert_eq!(v, b"i-42ei0ei-9223372036854775808e5:hello");
    }
}
