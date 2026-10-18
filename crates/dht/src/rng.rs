//! Small deterministic PRNG (xoshiro256**) so the sans-I/O core is reproducible in tests.
//!
//! Not cryptographic. DHT tokens and node ids only need unpredictability against
//! off-path attackers, and the production seed comes from the OS-seeded
//! `std::collections::hash_map::RandomState` (no extra dependency).

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

/// xoshiro256** generator.
#[derive(Clone, Debug)]
pub struct Rng {
    s: [u64; 4],
}

fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    /// Deterministic generator from a 64-bit seed.
    pub fn from_seed(seed: u64) -> Self {
        let mut st = seed;
        let s = [
            splitmix(&mut st),
            splitmix(&mut st),
            splitmix(&mut st),
            splitmix(&mut st),
        ];
        Self { s }
    }

    /// Generator seeded from OS entropy (via `RandomState`, which is OS-seeded per process
    /// and incremented per instance) mixed with the wall clock.
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
        Self::from_seed(seed)
    }

    /// Next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform value in `0..n` (`n > 0`; returns 0 for `n == 0`). Slight modulo bias is irrelevant here.
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

    /// A random 20-byte id.
    pub fn id(&mut self) -> [u8; 20] {
        let mut b = [0u8; 20];
        self.fill(&mut b);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Rng::from_seed(7);
        let mut b = Rng::from_seed(7);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(Rng::from_seed(7).next_u64(), Rng::from_seed(8).next_u64());
    }

    #[test]
    fn entropy_differs() {
        assert_ne!(
            Rng::from_entropy().next_u64(),
            Rng::from_entropy().next_u64()
        );
    }

    #[test]
    fn below_bounds() {
        let mut r = Rng::from_seed(1);
        for _ in 0..1000 {
            assert!(r.below(5) < 5);
        }
        assert_eq!(r.below(0), 0);
    }
}
