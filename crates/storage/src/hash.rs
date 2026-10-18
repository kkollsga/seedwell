//! SHA-1 / SHA-256 helpers.
//!
//! Both RustCrypto crates select the ARMv8 SHA instructions at run time (`cpufeatures`), with a
//! portable fallback. [`sha1_hw_accelerated`] reports whether the CPU offers them; the
//! `storage_bench` benchmark confirms the speed-up empirically.

use sha1::Digest;

/// Length of a SHA-1 digest.
pub const SHA1_LEN: usize = 20;
/// Length of a SHA-256 digest.
pub const SHA256_LEN: usize = 32;

/// One-shot SHA-1.
pub fn sha1(data: &[u8]) -> [u8; SHA1_LEN] {
    sha1::Sha1::digest(data).into()
}

/// One-shot SHA-256 (BEP 52 block/merkle hashing builds on this).
pub fn sha256(data: &[u8]) -> [u8; SHA256_LEN] {
    sha2::Sha256::digest(data).into()
}

/// Incremental SHA-1 used when streaming pieces from disk.
#[derive(Default, Clone)]
pub struct Sha1Stream(sha1::Sha1);

impl Sha1Stream {
    /// Fresh state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds data.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// Finishes and returns the digest.
    pub fn finish(self) -> [u8; SHA1_LEN] {
        self.0.finalize().into()
    }
}

/// Incremental SHA-256.
#[derive(Default, Clone)]
pub struct Sha256Stream(sha2::Sha256);

impl Sha256Stream {
    /// Fresh state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds data.
    pub fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    /// Finishes and returns the digest.
    pub fn finish(self) -> [u8; SHA256_LEN] {
        self.0.finalize().into()
    }
}

/// True when the CPU has the ARMv8 SHA instructions the `sha1`/`sha2` crates use.
pub fn sha1_hw_accelerated() -> bool {
    #[cfg(target_arch = "aarch64")]
    {
        std::arch::is_aarch64_feature_detected!("sha2")
    }
    #[cfg(target_arch = "x86_64")]
    {
        std::arch::is_x86_feature_detected!("sha")
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn known_vectors() {
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let mut s = Sha1Stream::new();
        s.update(b"a");
        s.update(b"bc");
        assert_eq!(s.finish(), sha1(b"abc"));
        let mut s = Sha256Stream::new();
        s.update(b"ab");
        s.update(b"c");
        assert_eq!(s.finish(), sha256(b"abc"));
    }
}
