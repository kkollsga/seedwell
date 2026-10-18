//! Minimal self-contained bencode reader/writer.
//!
//! Deliberately independent of `crates/bencode`: this tool is a reference
//! observer and must not share bugs with the code it is checking.
//! Dictionary entries keep their original order (trackers and torrent
//! files in the wild are not always sorted) and the top-level decoder
//! reports the raw byte span of each dictionary value so callers can
//! preserve e.g. the `info` dictionary byte-for-byte.

use std::ops::Range;

const MAX_DEPTH: usize = 64;

/// A bencode value. Dictionary order is preserved as parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Signed integer.
    Int(i64),
    /// Byte string (not necessarily UTF-8).
    Bytes(Vec<u8>),
    /// List of values.
    List(Vec<Value>),
    /// Dictionary, in source order.
    Dict(Vec<(Vec<u8>, Value)>),
}

/// Decode failure with the byte offset where it was detected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// Offset into the input.
    pub pos: usize,
    /// Short description.
    pub msg: &'static str,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bencode error at byte {}: {}", self.pos, self.msg)
    }
}

impl std::error::Error for Error {}

impl Value {
    /// Look a key up in a dictionary value.
    #[cfg(test)]
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(d) => d.iter().find(|(k, _)| k == key.as_bytes()).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Integer value, if this is an integer.
    #[cfg(test)]
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Byte-string value, if this is a byte string.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// List value, if this is a list.
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    /// Encode to canonical bencode, keeping dictionary order as stored.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Value::Bytes(b) => encode_bytes(b, out),
            Value::List(l) => {
                out.push(b'l');
                for v in l {
                    v.encode_into(out);
                }
                out.push(b'e');
            }
            Value::Dict(d) => {
                out.push(b'd');
                for (k, v) in d {
                    encode_bytes(k, out);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }
}

/// Append a bencoded byte string.
pub fn encode_bytes(b: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(b.len().to_string().as_bytes());
    out.push(b':');
    out.extend_from_slice(b);
}

struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Parser<'_> {
    fn err<T>(&self, msg: &'static str) -> Result<T, Error> {
        Err(Error { pos: self.pos, msg })
    }

    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn int_body(&mut self, end: u8) -> Result<i64, Error> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        if self.pos == digits_start {
            return self.err("expected digits");
        }
        let text = &self.buf[start..self.pos];
        // Reject leading zeros and "-0": not canonical, and a classic
        // source of parser differentials.
        let digits = &self.buf[digits_start..self.pos];
        if (digits.len() > 1 && digits[0] == b'0') || (text[0] == b'-' && digits == b"0") {
            return self.err("non-canonical integer");
        }
        if self.peek() != Some(end) {
            return self.err("unterminated integer");
        }
        // Digits only, so UTF-8 is guaranteed; parse overflow is an error.
        let s = std::str::from_utf8(text).map_err(|_| Error { pos: start, msg: "bad utf8" })?;
        let v = s.parse::<i64>().map_err(|_| Error { pos: start, msg: "integer overflow" })?;
        self.pos += 1;
        Ok(v)
    }

    fn bytes(&mut self) -> Result<Vec<u8>, Error> {
        let len = self.int_body(b':')?;
        let len = usize::try_from(len).map_err(|_| Error { pos: self.pos, msg: "negative length" })?;
        let end = self.pos.checked_add(len).filter(|e| *e <= self.buf.len());
        match end {
            Some(end) => {
                let v = self.buf[self.pos..end].to_vec();
                self.pos = end;
                Ok(v)
            }
            None => self.err("string exceeds input"),
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        if depth > MAX_DEPTH {
            return self.err("nesting too deep");
        }
        match self.peek() {
            None => self.err("unexpected end"),
            Some(b'i') => {
                self.pos += 1;
                Ok(Value::Int(self.int_body(b'e')?))
            }
            Some(b'l') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    match self.peek() {
                        None => return self.err("unterminated list"),
                        Some(b'e') => {
                            self.pos += 1;
                            return Ok(Value::List(items));
                        }
                        Some(_) => items.push(self.value(depth + 1)?),
                    }
                }
            }
            Some(b'd') => {
                self.pos += 1;
                let mut items = Vec::new();
                loop {
                    match self.peek() {
                        None => return self.err("unterminated dict"),
                        Some(b'e') => {
                            self.pos += 1;
                            return Ok(Value::Dict(items));
                        }
                        Some(b'0'..=b'9') => {
                            let k = self.bytes()?;
                            let v = self.value(depth + 1)?;
                            items.push((k, v));
                        }
                        Some(_) => return self.err("dict key must be a string"),
                    }
                }
            }
            Some(b'0'..=b'9') => Ok(Value::Bytes(self.bytes()?)),
            Some(_) => self.err("unexpected byte"),
        }
    }
}

/// Decode exactly one value; trailing bytes are an error.
pub fn decode(buf: &[u8]) -> Result<Value, Error> {
    let mut p = Parser { buf, pos: 0 };
    let v = p.value(0)?;
    if p.pos != buf.len() {
        return Err(Error { pos: p.pos, msg: "trailing data" });
    }
    Ok(v)
}

/// A top-level dictionary entry with the exact byte span of its value.
#[derive(Debug, Clone)]
pub struct RawEntry {
    /// Dictionary key.
    pub key: Vec<u8>,
    /// Byte range of the encoded value within the input.
    pub span: Range<usize>,
}

/// Split a top-level dictionary into keys and raw value spans without
/// re-encoding anything, so callers can copy values byte-for-byte.
pub fn split_top_dict(buf: &[u8]) -> Result<Vec<RawEntry>, Error> {
    let mut p = Parser { buf, pos: 0 };
    if p.peek() != Some(b'd') {
        return p.err("top level is not a dictionary");
    }
    p.pos += 1;
    let mut entries = Vec::new();
    loop {
        match p.peek() {
            None => return p.err("unterminated dict"),
            Some(b'e') => {
                p.pos += 1;
                break;
            }
            Some(b'0'..=b'9') => {
                let key = p.bytes()?;
                let start = p.pos;
                p.value(1)?;
                entries.push(RawEntry { key, span: start..p.pos });
            }
            Some(_) => return p.err("dict key must be a string"),
        }
    }
    if p.pos != buf.len() {
        return Err(Error { pos: p.pos, msg: "trailing data" });
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_basic() {
        let src = b"d3:bar4:spam3:fooi42e4:listl1:a1:bee";
        let v = decode(src).unwrap();
        assert_eq!(v.get("foo").and_then(Value::as_int), Some(42));
        assert_eq!(v.get("bar").and_then(Value::as_bytes), Some(&b"spam"[..]));
        assert_eq!(v.get("list").and_then(Value::as_list).map(<[Value]>::len), Some(2));
        assert_eq!(v.encode(), src);
    }

    #[test]
    fn unsorted_dict_order_preserved() {
        let src = b"d1:zi1e1:ai2ee";
        assert_eq!(decode(src).unwrap().encode(), src);
    }

    #[test]
    fn rejects_garbage_without_panicking() {
        for bad in [
            &b""[..],
            b"i",
            b"ie",
            b"i-0e",
            b"i01e",
            b"i99999999999999999999e",
            b"5:abc",
            b"-1:a",
            b"l",
            b"d1:a",
            b"di1ei2ee",
            b"i1etrailing",
            b"x",
            b"99999999999999999999:a",
        ] {
            assert!(decode(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
    }

    #[test]
    fn depth_limit() {
        let mut s = vec![b'l'; 200];
        s.extend(vec![b'e'; 200]);
        assert!(decode(&s).is_err());
    }

    #[test]
    fn spans_are_exact() {
        let src = b"d8:announce3:abc4:infod4:name1:xee";
        let e = split_top_dict(src).unwrap();
        assert_eq!(e.len(), 2);
        assert_eq!(&src[e[1].span.clone()], b"d4:name1:xe");
    }

    #[test]
    fn binary_strings_survive() {
        let mut src = b"d1:k4:".to_vec();
        src.extend_from_slice(&[0, 255, 10, 13]);
        src.push(b'e');
        let v = decode(&src).unwrap();
        assert_eq!(v.encode(), src);
    }
}
