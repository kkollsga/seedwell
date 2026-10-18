//! Tiny deterministic PRNG (splitmix64 seeding xoshiro-style output) so the sans-I/O core needs no
//! `rand` dependency and simulations are reproducible. Not cryptographic; connection ids only need
//! to be unpredictable enough to stop blind off-path RESET injection, so the driver seeds it from
//! the OS-randomised `RandomState`.

/// Small non-cryptographic generator.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    /// New generator; any seed (including 0) is fine.
    pub fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// Seed from the OS via the std hasher's random keys.
    pub fn from_os() -> Self {
        use std::hash::{BuildHasher, Hasher, RandomState};
        let mut h = RandomState::new().build_hasher();
        h.write_u64(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0),
        );
        Rng::new(h.finish())
    }

    /// Next 64 random bits (splitmix64).
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Next 16 random bits.
    pub fn next_u16(&mut self) -> u16 {
        (self.next_u64() >> 24) as u16
    }
}
