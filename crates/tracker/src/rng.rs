//! Injected randomness. The core never touches an OS RNG, which keeps every
//! test and simulation reproducible.

/// A source of random 64-bit values.
pub trait Rng {
    /// Next uniformly distributed value.
    fn next_u64(&mut self) -> u64;

    /// Uniform value in `0..bound` (0 when `bound == 0`). The modulo bias is
    /// below 2^-32 for the bounds used here, which is irrelevant for jitter.
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

/// SplitMix64: tiny, fast, and good enough for jitter and shuffles. It is not
/// used for anything security relevant (the engine supplies `key`/`peer_id`).
#[derive(Clone, Debug)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Seeded generator.
    pub const fn new(seed: u64) -> Self {
        SplitMix64 { state: seed }
    }
}

impl Rng for SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// In-place Fisher-Yates shuffle with an injected RNG.
pub fn shuffle<T, R: Rng + ?Sized>(items: &mut [T], rng: &mut R) {
    for i in (1..items.len()).rev() {
        let j = rng.below(i as u64 + 1) as usize;
        items.swap(i, j);
    }
}
