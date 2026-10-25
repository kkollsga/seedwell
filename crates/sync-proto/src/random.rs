//! Where secrets come from: the OS in production, a seeded source in tests and simulations.
//!
//! ring's own `SecureRandom` is sealed (nothing outside ring can implement it), so key generation here asks for
//! bytes through [`Random`] and builds keys from them.

/// The random source failed (on macOS and Linux this does not happen in practice).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RandomFailed;

impl std::fmt::Display for RandomFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the system random source failed")
    }
}

impl std::error::Error for RandomFailed {}

/// A source of random bytes for keys.
pub trait Random {
    /// Fills `out` with random bytes.
    fn fill(&mut self, out: &mut [u8]) -> Result<(), RandomFailed>;
}

/// The operating system's random source (ring's `SystemRandom`).
impl Random for ring::rand::SystemRandom {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), RandomFailed> {
        ring::rand::SecureRandom::fill(self, out).map_err(|_| RandomFailed)
    }
}

/// `N` random bytes as an array.
pub(crate) fn array<const N: usize>(rng: &mut dyn Random) -> Result<[u8; N], RandomFailed> {
    let mut out = [0u8; N];
    rng.fill(&mut out)?;
    Ok(out)
}
