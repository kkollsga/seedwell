//! Compact piece bitfield (BEP 3 bit order: piece 0 is the high bit of byte 0).

use crate::error::{Error, Result};

/// A fixed-length bit set with a cached population count.
///
/// Spare bits in the last byte are always zero, so [`Bitfield::as_bytes`] can be
/// sent on the wire as-is.
// size: 24 bytes (+ ceil(len / 8) heap bytes)
#[derive(Clone, PartialEq, Eq)]
pub struct Bitfield {
    bits: Box<[u8]>,
    len: u32,
    ones: u32,
}

impl core::fmt::Debug for Bitfield {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Bitfield({}/{})", self.ones, self.len)
    }
}

/// Bytes needed to hold `len` bits.
pub const fn byte_len(len: u32) -> usize {
    (len as usize).div_ceil(8)
}

impl Bitfield {
    /// All pieces missing (`have_none`).
    pub fn new(len: u32) -> Self {
        Self {
            bits: vec![0u8; byte_len(len)].into_boxed_slice(),
            len,
            ones: 0,
        }
    }

    /// Alias of [`Bitfield::new`] that reads better next to `have_all`.
    pub fn have_none(len: u32) -> Self {
        Self::new(len)
    }

    /// All pieces present (`have_all`), spare bits left zero.
    pub fn have_all(len: u32) -> Self {
        let mut b = Self {
            bits: vec![0xFFu8; byte_len(len)].into_boxed_slice(),
            len,
            ones: len,
        };
        b.mask_spare();
        b
    }

    /// Validate and copy a wire bitfield for a torrent with `len` pieces.
    ///
    /// Rejects wrong byte length and non-zero spare bits (BEP 3 says they must be
    /// zero; accepting garbage would corrupt the cached count).
    pub fn from_wire(bytes: &[u8], len: u32) -> Result<Self> {
        let mut b = Self::new(len);
        b.assign_from_wire(bytes)?;
        Ok(b)
    }

    /// Replace the contents from wire bytes, reusing the allocation.
    pub fn assign_from_wire(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() != self.bits.len() {
            return Err(Error::BadBitfield);
        }
        if let Some(&last) = bytes.last() {
            let spare = (self.bits.len() * 8) as u32 - self.len;
            if spare > 0 && last & ((1u16 << spare) - 1) as u8 != 0 {
                return Err(Error::BadBitfield);
            }
        }
        self.bits.copy_from_slice(bytes);
        self.ones = self.bits.iter().map(|b| b.count_ones()).sum();
        Ok(())
    }

    /// Number of pieces.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// True when the torrent has zero pieces.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of pieces present. O(1).
    pub fn count_ones(&self) -> u32 {
        self.ones
    }

    /// Number of pieces missing. O(1).
    pub fn count_zeros(&self) -> u32 {
        self.len - self.ones
    }

    /// Every piece present.
    pub fn is_complete(&self) -> bool {
        self.ones == self.len
    }

    /// No piece present.
    pub fn is_none(&self) -> bool {
        self.ones == 0
    }

    /// Whether piece `i` is present; out-of-range is `false`.
    #[inline]
    pub fn has(&self, i: u32) -> bool {
        if i >= self.len {
            return false;
        }
        self.bits
            .get((i >> 3) as usize)
            .is_some_and(|b| b & (0x80 >> (i & 7)) != 0)
    }

    /// Mark piece `i` present. Returns `true` only if it changed.
    #[inline]
    pub fn set(&mut self, i: u32) -> bool {
        if i >= self.len {
            return false;
        }
        let Some(b) = self.bits.get_mut((i >> 3) as usize) else {
            return false;
        };
        let m = 0x80 >> (i & 7);
        if *b & m != 0 {
            return false;
        }
        *b |= m;
        self.ones += 1;
        true
    }

    /// Mark piece `i` missing. Returns `true` only if it changed.
    #[inline]
    pub fn clear(&mut self, i: u32) -> bool {
        if i >= self.len {
            return false;
        }
        let Some(b) = self.bits.get_mut((i >> 3) as usize) else {
            return false;
        };
        let m = 0x80 >> (i & 7);
        if *b & m == 0 {
            return false;
        }
        *b &= !m;
        self.ones -= 1;
        true
    }

    /// Mark every piece present.
    pub fn set_all(&mut self) {
        self.bits.fill(0xFF);
        self.mask_spare();
        self.ones = self.len;
    }

    /// Mark every piece missing.
    pub fn clear_all(&mut self) {
        self.bits.fill(0);
        self.ones = 0;
    }

    /// Wire representation.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bits
    }

    /// Indices of present pieces, ascending.
    pub fn iter_ones(&self) -> BitIter<'_> {
        BitIter::new(self, None, Mode::Ones)
    }

    /// Indices of missing pieces, ascending.
    pub fn iter_zeros(&self) -> BitIter<'_> {
        BitIter::new(self, None, Mode::Zeros)
    }

    /// Pieces that `self` has and `other` lacks (e.g. what a peer could get from us,
    /// or what we want from a peer when called as `peer.iter_ones_missing_from(&ours)`).
    /// Yields nothing when the lengths differ.
    pub fn iter_ones_missing_from<'a>(&'a self, other: &'a Bitfield) -> BitIter<'a> {
        if other.len != self.len {
            return BitIter::new(self, None, Mode::Empty);
        }
        BitIter::new(self, Some(other), Mode::Diff)
    }

    fn mask_spare(&mut self) {
        let spare = (self.bits.len() * 8) as u32 - self.len;
        if spare > 0
            && let Some(last) = self.bits.last_mut() {
                *last &= !(((1u16 << spare) - 1) as u8);
            }
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Ones,
    Zeros,
    Diff,
    Empty,
}

/// Iterator over bit positions; skips whole zero bytes quickly.
#[derive(Clone)]
pub struct BitIter<'a> {
    a: &'a [u8],
    b: Option<&'a [u8]>,
    mode: Mode,
    len: u32,
    byte: usize,
    cur: u8,
}

impl<'a> BitIter<'a> {
    fn new(a: &'a Bitfield, b: Option<&'a Bitfield>, mode: Mode) -> Self {
        let mut it = Self {
            a: &a.bits,
            b: b.map(|b| &*b.bits),
            mode,
            len: a.len,
            byte: 0,
            cur: 0,
        };
        it.cur = it.load(0);
        it
    }

    #[inline]
    fn load(&self, i: usize) -> u8 {
        let Some(&x) = self.a.get(i) else { return 0 };
        match self.mode {
            Mode::Ones => x,
            Mode::Zeros => !x,
            Mode::Diff => x & !self.b.and_then(|b| b.get(i)).copied().unwrap_or(0),
            Mode::Empty => 0,
        }
    }
}

impl Iterator for BitIter<'_> {
    type Item = u32;

    #[inline]
    fn next(&mut self) -> Option<u32> {
        loop {
            if self.cur != 0 {
                let lz = self.cur.leading_zeros();
                self.cur &= !(0x80u8 >> lz);
                let idx = (self.byte as u32) * 8 + lz;
                // Inverted spare bits in `Zeros` mode land here and must be dropped.
                return if idx < self.len { Some(idx) } else { None };
            }
            self.byte += 1;
            if self.byte >= self.a.len() {
                return None;
            }
            self.cur = self.load(self.byte);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        let mut b = Bitfield::new(10);
        assert_eq!(b.as_bytes(), &[0, 0]);
        assert!(b.set(0));
        assert!(!b.set(0));
        assert!(b.set(9));
        assert!(!b.set(10));
        assert_eq!(b.as_bytes(), &[0x80, 0x40]);
        assert_eq!(b.count_ones(), 2);
        assert!(b.has(9) && !b.has(8) && !b.has(100));
        assert!(b.clear(9));
        assert!(!b.clear(9));
        assert_eq!(b.count_ones(), 1);
    }

    #[test]
    fn have_all_masks_spare_bits() {
        let b = Bitfield::have_all(10);
        assert_eq!(b.as_bytes(), &[0xFF, 0xC0]);
        assert!(b.is_complete());
        assert_eq!(b.count_zeros(), 0);
        let n = Bitfield::have_none(10);
        assert!(n.is_none());
        assert_eq!(Bitfield::have_all(0).as_bytes(), &[] as &[u8]);
        assert_eq!(Bitfield::have_all(8).as_bytes(), &[0xFF]);
    }

    #[test]
    fn from_wire_validation() {
        assert!(Bitfield::from_wire(&[0xFF, 0xC0], 10).is_ok());
        assert_eq!(
            Bitfield::from_wire(&[0xFF, 0xC1], 10),
            Err(Error::BadBitfield)
        );
        assert_eq!(Bitfield::from_wire(&[0xFF], 10), Err(Error::BadBitfield));
        assert_eq!(
            Bitfield::from_wire(&[0xFF, 0, 0], 10),
            Err(Error::BadBitfield)
        );
        let b = Bitfield::from_wire(&[0b1010_0000], 3).unwrap();
        assert_eq!(b.count_ones(), 2);
        assert_eq!(
            Bitfield::from_wire(&[0b1010_0001], 3),
            Err(Error::BadBitfield)
        );
        assert!(Bitfield::from_wire(&[], 0).is_ok());
    }

    #[test]
    fn iterators() {
        let b = Bitfield::from_wire(&[0b1000_0001, 0b0100_0000], 10).unwrap();
        assert_eq!(b.iter_ones().collect::<Vec<_>>(), vec![0, 7, 9]);
        assert_eq!(
            b.iter_zeros().collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 8]
        );
        let all = Bitfield::have_all(13);
        assert_eq!(all.iter_zeros().count(), 0);
        assert_eq!(all.iter_ones().count(), 13);
        let none = Bitfield::new(13);
        assert_eq!(none.iter_zeros().count(), 13);
        let ours = Bitfield::from_wire(&[0b1000_0000, 0], 10).unwrap();
        assert_eq!(
            b.iter_ones_missing_from(&ours).collect::<Vec<_>>(),
            vec![7, 9]
        );
        let other_len = Bitfield::new(11);
        assert_eq!(b.iter_ones_missing_from(&other_len).count(), 0);
    }

    #[test]
    fn set_all_clear_all() {
        let mut b = Bitfield::new(9);
        b.set_all();
        assert_eq!(b.as_bytes(), &[0xFF, 0x80]);
        assert_eq!(b.count_ones(), 9);
        b.clear_all();
        assert!(b.is_none());
    }
}
