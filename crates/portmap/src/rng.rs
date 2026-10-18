//! Tiny deterministic PRNG (SplitMix64) so the sans-I/O core is reproducible in tests.
//!
//! Not cryptographic: it picks jitter, port suggestions and PCP nonces. The PCP nonce
//! only has to be unpredictable to off-path hosts, and the production seed comes from
//! the OS-seeded `RandomState` mixed with the clock.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// SplitMix64 generator.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// Deterministic generator.
    pub fn from_seed(seed: u64) -> Self {
        Rng(seed)
    }

    /// Seed from OS entropy (via `RandomState`) and the wall clock.
    pub fn from_entropy() -> Self {
        let mut seed = 0u64;
        for i in 0..4u64 {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(i);
            if let Ok(d) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
                h.write_u128(d.as_nanos());
            }
            seed ^= h.finish().rotate_left((i * 13) as u32);
        }
        Rng(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform-ish value in `0..n` (0 when `n == 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Fill `buf` with random bytes.
    pub fn fill(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_bounded() {
        let (mut a, mut b) = (Rng::from_seed(7), Rng::from_seed(7));
        for _ in 0..20 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::from_seed(7).next_u64(), Rng::from_seed(8).next_u64());
        let mut r = Rng::from_seed(1);
        for _ in 0..1000 {
            assert!(r.below(5) < 5);
        }
        assert_eq!(r.below(0), 0);
        assert_ne!(
            Rng::from_entropy().next_u64(),
            Rng::from_entropy().next_u64()
        );
    }
}
