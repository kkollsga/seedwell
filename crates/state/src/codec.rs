//! Tiny bounds-checked binary codec (LEB128 varints, length-prefixed bytes).
//!
//! Hand-written instead of serde+bincode: a record is a few dozen bytes, the
//! decoder must never panic on garbage, and we want to control every byte.

use crate::error::DecodeError;

/// Append-only encoder.
#[derive(Default)]
pub(crate) struct Writer(pub Vec<u8>);

impl Writer {
    pub fn new() -> Self {
        Writer(Vec::with_capacity(96))
    }
    pub fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }
    /// Zigzag-encoded signed integer.
    pub fn svarint(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.varint(b.len() as u64);
        self.0.extend_from_slice(b);
    }
    pub fn raw(&mut self, b: &[u8]) {
        self.0.extend_from_slice(b);
    }
    pub fn f64(&mut self, v: f64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
}

/// Bounds-checked decoder over untrusted bytes.
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.pos)
    }
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        let b = *self.buf.get(self.pos).ok_or(DecodeError("truncated"))?;
        self.pos += 1;
        Ok(b)
    }
    pub fn varint(&mut self) -> Result<u64, DecodeError> {
        let mut v: u64 = 0;
        for shift in (0..70).step_by(7) {
            let b = self.u8()?;
            let low = u64::from(b & 0x7f);
            if shift == 63 && low > 1 {
                return Err(DecodeError("varint overflow"));
            }
            v |= low << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError("varint too long"))
    }
    pub fn svarint(&mut self) -> Result<i64, DecodeError> {
        let u = self.varint()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        u32::try_from(self.varint()?).map_err(|_| DecodeError("u32 overflow"))
    }
    /// Length-prefixed bytes, at most `max` long.
    pub fn bytes(&mut self, max: usize) -> Result<&'a [u8], DecodeError> {
        let len = self.varint()?;
        let len = usize::try_from(len).map_err(|_| DecodeError("length overflow"))?;
        if len > max {
            return Err(DecodeError("field longer than its cap"));
        }
        self.raw(len)
    }
    pub fn raw(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if len > self.remaining() {
            return Err(DecodeError("truncated"));
        }
        let s = &self.buf[self.pos..self.pos + len];
        self.pos += len;
        Ok(s)
    }
    pub fn string(&mut self, max: usize) -> Result<&'a str, DecodeError> {
        std::str::from_utf8(self.bytes(max)?).map_err(|_| DecodeError("invalid utf-8"))
    }
    pub fn f64(&mut self) -> Result<f64, DecodeError> {
        let b = self.raw(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(f64::from_le_bytes(a))
    }
    /// Fail if bytes are left over (a longer record than we understand).
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.is_empty() { Ok(()) } else { Err(DecodeError("trailing bytes")) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_edges() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX - 1, u64::MAX] {
            let mut w = Writer::new();
            w.varint(v);
            assert_eq!(Reader::new(&w.0).varint(), Ok(v));
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX, -123456789] {
            let mut w = Writer::new();
            w.svarint(v);
            assert_eq!(Reader::new(&w.0).svarint(), Ok(v));
        }
    }

    #[test]
    fn varint_rejects_overlong_and_overflow() {
        assert!(Reader::new(&[0x80; 11]).varint().is_err());
        let mut over = vec![0xff; 9];
        over.push(0x02);
        assert!(Reader::new(&over).varint().is_err());
        assert!(Reader::new(&[0x80]).varint().is_err());
    }

    #[test]
    fn bytes_respects_cap_and_length() {
        let mut w = Writer::new();
        w.bytes(b"hello");
        assert_eq!(Reader::new(&w.0).bytes(5), Ok(&b"hello"[..]));
        assert!(Reader::new(&w.0).bytes(4).is_err());
        assert!(Reader::new(&w.0[..3]).bytes(10).is_err());
        let mut huge = Writer::new();
        huge.varint(u64::MAX);
        assert!(Reader::new(&huge.0).bytes(usize::MAX).is_err());
    }
}
