//! Compact piece bitset (1 bit per piece).

/// Fixed-size bitset indexed by piece number.
// size: 24 bytes + len/8 bytes heap
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PieceSet {
    bits: Box<[u64]>,
    len: u32,
}

impl PieceSet {
    /// All-clear set of `len` pieces.
    pub fn new(len: u32) -> Self {
        Self {
            bits: vec![0; (len as usize).div_ceil(64)].into_boxed_slice(),
            len,
        }
    }

    /// Number of pieces tracked.
    pub fn len(&self) -> u32 {
        self.len
    }

    /// True when it tracks zero pieces.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether piece `i` is set (false when out of range).
    pub fn get(&self, i: u32) -> bool {
        i < self.len && self.bits[(i / 64) as usize] >> (i % 64) & 1 == 1
    }

    /// Sets piece `i` (ignored when out of range).
    pub fn set(&mut self, i: u32, v: bool) {
        if i < self.len {
            let w = &mut self.bits[(i / 64) as usize];
            if v {
                *w |= 1 << (i % 64);
            } else {
                *w &= !(1 << (i % 64));
            }
        }
    }

    /// Number of set pieces.
    pub fn count_ones(&self) -> u32 {
        self.bits.iter().map(|w| w.count_ones()).sum()
    }

    /// Iterates set pieces in order.
    pub fn iter_ones(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.len).filter(|&i| self.get(i))
    }

    pub(crate) fn from_words(words: Vec<u64>, len: u32) -> Self {
        Self {
            bits: words.into_boxed_slice(),
            len,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_count() {
        let mut s = PieceSet::new(130);
        for i in [0, 63, 64, 129] {
            s.set(i, true);
        }
        s.set(500, true);
        assert_eq!(s.count_ones(), 4);
        assert!(s.get(64) && !s.get(65) && !s.get(500));
        s.set(64, false);
        assert_eq!(s.iter_ones().collect::<Vec<_>>(), vec![0, 63, 129]);
    }
}
