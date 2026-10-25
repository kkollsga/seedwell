//! A device's own key: Ed25519, from ring. Its public half is the device's identity ([`DeviceId`]) and its TLS
//! raw public key; it signs the file versions the device makes and its address records.

use std::fmt;

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};

use crate::ids::DeviceId;
use crate::random::{Random, RandomFailed, array};

/// The PKCS#8 v2 document ring writes for an Ed25519 key (RFC 8410 §10.3): this prefix, the 32-byte seed, the
/// `[1] publicKey` header, the 32-byte public key. Writing it here lets a key come from any [`Random`], while
/// ring still parses (and checks) every key that is loaded.
const PKCS8_PREFIX: [u8; 16] =
    [0x30, 0x51, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20];
const PKCS8_MIDDLE: [u8; 3] = [0x81, 0x21, 0x00];

/// A stored device key that ring refused (damaged, not Ed25519, or not PKCS#8 v2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BadDeviceKey(String);

impl fmt::Display for BadDeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the device key cannot be used: {}", self.0)
    }
}

impl std::error::Error for BadDeviceKey {}

/// A device's Ed25519 key pair.
pub struct DeviceKey {
    pair: Ed25519KeyPair,
    /// Kept for [`DeviceKey::to_pkcs8`]: ring cannot export a key pair.
    pkcs8: Vec<u8>,
    public: [u8; 32],
}

impl DeviceKey {
    /// A new key from `rng`.
    pub fn generate(rng: &mut dyn Random) -> Result<Self, RandomFailed> {
        let seed: [u8; 32] = array(rng)?;
        Ok(Self::from_seed(&seed))
    }

    /// The key with this Ed25519 seed (deterministic: tests and simulations).
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        let pair = match Ed25519KeyPair::from_seed_unchecked(seed) {
            Ok(pair) => pair,
            // ring rejects only seeds of the wrong length, and this one is 32 bytes by type.
            Err(e) => unreachable!("ring refused a 32-byte Ed25519 seed: {e}"),
        };
        let public = pk_of(&pair);
        let mut pkcs8 = Vec::with_capacity(83);
        pkcs8.extend_from_slice(&PKCS8_PREFIX);
        pkcs8.extend_from_slice(seed);
        pkcs8.extend_from_slice(&PKCS8_MIDDLE);
        pkcs8.extend_from_slice(&public);
        DeviceKey { pair, pkcs8, public }
    }

    /// A key stored with [`DeviceKey::to_pkcs8`] (or made by ring's `generate_pkcs8`). ring checks that the
    /// public key in the document belongs to the private key.
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<Self, BadDeviceKey> {
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| BadDeviceKey(e.to_string()))?;
        let public = pk_of(&pair);
        Ok(DeviceKey { pair, pkcs8: pkcs8.to_vec(), public })
    }

    /// The key as a PKCS#8 v2 document, for the Keychain or a 0600 file. This is the secret: never log it.
    pub fn to_pkcs8(&self) -> Vec<u8> {
        self.pkcs8.clone()
    }

    /// A secret derived from this key for one purpose (`label`): HMAC-SHA256 keyed with the stored key. The device's
    /// X25519 key for private messages comes from it, so it needs no storage of its own.
    pub fn derive(&self, label: &str) -> [u8; 32] {
        let k = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &self.pkcs8);
        let mut out = [0u8; 32];
        out.copy_from_slice(ring::hmac::sign(&k, label.as_bytes()).as_ref());
        out
    }

    /// The public key (the TLS raw public key, and what [`DeviceId`] hashes).
    pub fn public_key(&self) -> &[u8; 32] {
        &self.public
    }

    /// This device's id.
    pub fn device_id(&self) -> DeviceId {
        DeviceId::of_public_key(&self.public)
    }

    /// An Ed25519 signature over `msg`.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out.copy_from_slice(self.pair.sign(msg).as_ref());
        out
    }
}

impl fmt::Debug for DeviceKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceKey({:?})", self.device_id())
    }
}

fn pk_of(pair: &Ed25519KeyPair) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(pair.public_key().as_ref());
    out
}

/// Whether `sig` is `pk`'s Ed25519 signature over `msg`.
pub fn verify(pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    UnparsedPublicKey::new(&ED25519, pk).verify(msg, sig).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc8032_test_vector_1() {
        // RFC 8032 §7.1, TEST 1: the empty message.
        let seed = hex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let key = DeviceKey::from_seed(&seed.try_into().unwrap());
        let pk = hex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        assert_eq!(key.public_key().to_vec(), pk);
        let sig = key.sign(b"");
        let want = concat!(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155",
            "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
        );
        assert_eq!(sig.to_vec(), hex(want));
        assert!(verify(key.public_key(), b"", &sig));
    }

    #[test]
    fn pkcs8_roundtrips_and_matches_ring() {
        let key = DeviceKey::from_seed(&[3; 32]);
        let again = DeviceKey::from_pkcs8(&key.to_pkcs8()).expect("ring parses the documents from_seed writes");
        assert_eq!(again.public_key(), key.public_key());

        let doc = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
        assert_eq!(doc.as_ref().len(), 83);
        assert_eq!(doc.as_ref()[..16], PKCS8_PREFIX, "the same layout as ring's own documents");
        assert_eq!(doc.as_ref()[48..51], PKCS8_MIDDLE);
        let from_ring = DeviceKey::from_pkcs8(doc.as_ref()).unwrap();
        assert_eq!(from_ring.to_pkcs8(), doc.as_ref());
    }

    #[test]
    fn damaged_pkcs8_is_refused() {
        let mut doc = DeviceKey::from_seed(&[3; 32]).to_pkcs8();
        *doc.last_mut().unwrap() ^= 1; // the public key no longer matches the seed
        assert!(DeviceKey::from_pkcs8(&doc).is_err());
        assert!(DeviceKey::from_pkcs8(b"").is_err());
    }

    #[test]
    fn generate_uses_the_given_randomness() {
        struct Fixed(u8);
        impl Random for Fixed {
            fn fill(&mut self, out: &mut [u8]) -> Result<(), RandomFailed> {
                out.fill(self.0);
                Ok(())
            }
        }
        let a = DeviceKey::generate(&mut Fixed(9)).unwrap();
        assert_eq!(a.public_key(), DeviceKey::from_seed(&[9; 32]).public_key());
        let b = DeviceKey::generate(&mut ring::rand::SystemRandom::new()).unwrap();
        assert_ne!(a.public_key(), b.public_key());
    }

    #[test]
    fn a_changed_message_or_signature_fails() {
        let key = DeviceKey::from_seed(&[5; 32]);
        let sig = key.sign(b"hello");
        assert!(verify(key.public_key(), b"hello", &sig));
        assert!(!verify(key.public_key(), b"hellp", &sig));
        let mut bad = sig;
        bad[10] ^= 4;
        assert!(!verify(key.public_key(), b"hello", &bad));
        assert!(!verify(DeviceKey::from_seed(&[6; 32]).public_key(), b"hello", &sig));
    }

    #[test]
    fn debug_shows_only_the_id() {
        let key = DeviceKey::from_seed(&[5; 32]);
        assert_eq!(format!("{key:?}"), format!("DeviceKey({:?})", key.device_id()));
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }
}
