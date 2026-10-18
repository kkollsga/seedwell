use crate::{Error, Value};

/// Maximum container nesting accepted by the decoder. Real torrents nest ~5
/// levels; 64 keeps worst-case recursion tiny while leaving ample headroom.
pub const MAX_DEPTH: usize = 64;

/// Resource caps applied while decoding untrusted input.
///
/// `max_items` bounds the number of allocated values (every int, string, list,
/// dict and dict key counts as one), which bounds memory to roughly
/// `max_items * 48` bytes regardless of what the input claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLimits {
    /// Maximum number of values (including dictionary keys) in the document.
    pub max_items: usize,
    /// Maximum input length in bytes.
    pub max_bytes: usize,
}

impl Default for DecodeLimits {
    /// 4 Mi items and 64 MiB: generous for the largest real torrents while
    /// capping transient decode memory near 200 MB.
    fn default() -> Self {
        DecodeLimits {
            max_items: 1 << 22,
            max_bytes: 64 << 20,
        }
    }
}

/// Strategy for what the parser produces. Lets one grammar implementation serve
/// both the allocating `Value` tree and the allocation-free validate-and-skip
/// used by the raw-span API, so the two can never disagree on what is valid.
pub(crate) trait Build<'a> {
    type Node;
    type List;
    type Dict;
    fn int(v: i64) -> Self::Node;
    fn bytes(b: &'a [u8]) -> Self::Node;
    fn list() -> Self::List;
    fn list_push(l: &mut Self::List, n: Self::Node);
    fn list_end(l: Self::List) -> Self::Node;
    fn dict() -> Self::Dict;
    fn dict_push(d: &mut Self::Dict, k: &'a [u8], n: Self::Node);
    fn dict_end(d: Self::Dict) -> Self::Node;
}

pub(crate) struct Tree;
pub(crate) struct Skip;

impl<'a> Build<'a> for Tree {
    type Node = Value<'a>;
    type List = Vec<Value<'a>>;
    type Dict = Vec<(&'a [u8], Value<'a>)>;
    fn int(v: i64) -> Self::Node {
        Value::Int(v)
    }
    fn bytes(b: &'a [u8]) -> Self::Node {
        Value::Bytes(b)
    }
    fn list() -> Self::List {
        Vec::new()
    }
    fn list_push(l: &mut Self::List, n: Self::Node) {
        l.push(n);
    }
    fn list_end(l: Self::List) -> Self::Node {
        Value::List(l)
    }
    fn dict() -> Self::Dict {
        Vec::new()
    }
    fn dict_push(d: &mut Self::Dict, k: &'a [u8], n: Self::Node) {
        d.push((k, n));
    }
    fn dict_end(d: Self::Dict) -> Self::Node {
        Value::Dict(d)
    }
}

impl<'a> Build<'a> for Skip {
    type Node = ();
    type List = ();
    type Dict = ();
    fn int(_: i64) {}
    fn bytes(_: &'a [u8]) {}
    fn list() {}
    fn list_push(_: &mut (), _: ()) {}
    fn list_end(_: ()) {}
    fn dict() {}
    fn dict_push(_: &mut (), _: &'a [u8], _: ()) {}
    fn dict_end(_: ()) {}
}

pub(crate) struct Parser<'a> {
    pub(crate) input: &'a [u8],
    pub(crate) pos: usize,
    items: usize,
    max_items: usize,
    /// Scratch stack of the keys of all dictionaries currently being parsed.
    /// Shared so that duplicate detection needs no per-dict allocation.
    keys: Vec<&'a [u8]>,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(input: &'a [u8], limits: DecodeLimits) -> Result<Self, Error> {
        if input.len() > limits.max_bytes {
            return Err(Error::InputTooLarge);
        }
        Ok(Parser {
            input,
            pos: 0,
            items: 0,
            max_items: limits.max_items,
            keys: Vec::new(),
        })
    }

    pub(crate) fn peek(&self) -> Result<u8, Error> {
        self.input
            .get(self.pos)
            .copied()
            .ok_or(Error::UnexpectedEof)
    }

    pub(crate) fn count_item(&mut self) -> Result<(), Error> {
        self.items += 1;
        if self.items > self.max_items {
            Err(Error::TooManyItems)
        } else {
            Ok(())
        }
    }

    pub(crate) fn parse_value<B: Build<'a>>(&mut self, depth: usize) -> Result<B::Node, Error> {
        self.count_item()?;
        match self.peek()? {
            b'i' => Ok(B::int(self.parse_int()?)),
            b'0'..=b'9' => Ok(B::bytes(self.parse_bytes()?)),
            b'l' => {
                if depth >= MAX_DEPTH {
                    return Err(Error::DepthExceeded);
                }
                self.pos += 1;
                let mut list = B::list();
                while self.peek()? != b'e' {
                    let item = self.parse_value::<B>(depth + 1)?;
                    B::list_push(&mut list, item);
                }
                self.pos += 1;
                Ok(B::list_end(list))
            }
            b'd' => {
                if depth >= MAX_DEPTH {
                    return Err(Error::DepthExceeded);
                }
                self.pos += 1;
                let start = self.keys.len();
                let dict_pos = self.pos;
                let mut sorted = true;
                let mut dict = B::dict();
                while self.peek()? != b'e' {
                    let key = self.parse_key()?;
                    if let Some(prev) = self.keys[start..].last() {
                        if key == *prev {
                            return Err(Error::DuplicateKey { pos: self.pos });
                        }
                        if key < *prev {
                            sorted = false;
                        }
                    }
                    self.keys.push(key);
                    let val = self.parse_value::<B>(depth + 1)?;
                    B::dict_push(&mut dict, key, val);
                }
                self.pos += 1;
                if !sorted {
                    // Out-of-order dicts are legal here, but a duplicate could still
                    // hide anywhere in them, so sort the keys once to look.
                    let ks = &mut self.keys[start..];
                    ks.sort_unstable();
                    if ks.windows(2).any(|w| w[0] == w[1]) {
                        return Err(Error::DuplicateKey { pos: dict_pos });
                    }
                }
                self.keys.truncate(start);
                Ok(B::dict_end(dict))
            }
            byte => Err(Error::InvalidByte {
                pos: self.pos,
                byte,
            }),
        }
    }

    /// Parses a dictionary key (always a byte string) and counts it as an item.
    pub(crate) fn parse_key(&mut self) -> Result<&'a [u8], Error> {
        let byte = self.peek()?;
        if !byte.is_ascii_digit() {
            return Err(Error::InvalidByte {
                pos: self.pos,
                byte,
            });
        }
        self.count_item()?;
        self.parse_bytes()
    }

    fn parse_int(&mut self) -> Result<i64, Error> {
        let start = self.pos;
        self.pos += 1; // 'i'
        let neg = self.input.get(self.pos) == Some(&b'-');
        if neg {
            self.pos += 1;
        }
        let digits_start = self.pos;
        while self.input.get(self.pos).is_some_and(u8::is_ascii_digit) {
            self.pos += 1;
        }
        let digits = &self.input[digits_start..self.pos];
        match self.input.get(self.pos) {
            Some(b'e') => {}
            Some(_) => return Err(Error::InvalidInteger { pos: start }),
            None => return Err(Error::UnexpectedEof),
        }
        // Canonical form only: at least one digit, no leading zero, no "-0".
        let Some(&first) = digits.first() else {
            return Err(Error::InvalidInteger { pos: start });
        };
        if first == b'0' && (digits.len() > 1 || neg) {
            return Err(Error::InvalidInteger { pos: start });
        }
        let mut mag: u64 = 0;
        for &d in digits {
            mag = mag
                .checked_mul(10)
                .and_then(|m| m.checked_add(u64::from(d - b'0')))
                .ok_or(Error::IntegerOverflow { pos: start })?;
        }
        // i128 holds both -2^63 and 2^63 - 1 without special-casing i64::MIN.
        let signed = if neg {
            -i128::from(mag)
        } else {
            i128::from(mag)
        };
        let value = i64::try_from(signed).map_err(|_| Error::IntegerOverflow { pos: start })?;
        self.pos += 1; // 'e'
        Ok(value)
    }

    pub(crate) fn parse_bytes(&mut self) -> Result<&'a [u8], Error> {
        let start = self.pos;
        let mut len: usize = 0;
        while let Some(&b) = self.input.get(self.pos) {
            if !b.is_ascii_digit() {
                break;
            }
            len = len
                .checked_mul(10)
                .and_then(|l| l.checked_add(usize::from(b - b'0')))
                .ok_or(Error::InvalidLength { pos: start })?;
            self.pos += 1;
        }
        let ndigits = self.pos - start;
        match self.input.get(self.pos) {
            Some(b':') => {}
            Some(_) => return Err(Error::InvalidLength { pos: start }),
            None => return Err(Error::UnexpectedEof),
        }
        if ndigits == 0 || (self.input[start] == b'0' && ndigits > 1) {
            return Err(Error::InvalidLength { pos: start });
        }
        self.pos += 1; // ':'
        let end = self.pos.checked_add(len).ok_or(Error::UnexpectedEof)?;
        let bytes = self.input.get(self.pos..end).ok_or(Error::UnexpectedEof)?;
        self.pos = end;
        Ok(bytes)
    }
}

/// Decodes exactly one value that must span the whole input, using default limits.
pub fn decode(input: &[u8]) -> Result<Value<'_>, Error> {
    decode_with_limits(input, DecodeLimits::default())
}

/// Like [`decode`] with explicit resource limits.
pub fn decode_with_limits(input: &[u8], limits: DecodeLimits) -> Result<Value<'_>, Error> {
    let (value, rest) = decode_prefix_with_limits(input, limits)?;
    if rest.is_empty() {
        Ok(value)
    } else {
        Err(Error::TrailingData {
            pos: input.len() - rest.len(),
        })
    }
}

/// Decodes the first value and returns the unconsumed remainder (used for
/// stream framing such as BEP 9 `ut_metadata` data messages).
pub fn decode_prefix(input: &[u8]) -> Result<(Value<'_>, &[u8]), Error> {
    decode_prefix_with_limits(input, DecodeLimits::default())
}

/// Like [`decode_prefix`] with explicit resource limits.
pub fn decode_prefix_with_limits(
    input: &[u8],
    limits: DecodeLimits,
) -> Result<(Value<'_>, &[u8]), Error> {
    let mut p = Parser::new(input, limits)?;
    let value = p.parse_value::<Tree>(0)?;
    Ok((value, &input[p.pos..]))
}
