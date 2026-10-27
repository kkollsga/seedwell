//! Messages encrypted at rest (SPEC §7G.7 *On disk*): a channel's messages and the private messages are kept in
//! files only this device's Seedwell can read, so a search, a backup or another program reading the state folder
//! finds no text.
//!
//! Each file has its own random key, kept with the other secrets ([`crate::secrets`]: the Keychain in the app): one
//! per channel ([`channel_key_name`]) and one for the private messages ([`DIRECT_KEY`]). Every record is sealed on
//! its own (ChaCha20-Poly1305, a random 96-bit nonce, the file's magic as associated data), so the files stay
//! append-only. Leaving a channel deletes its key with its file: a copy of the file left in a backup can no longer
//! be read.

use std::io;

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};
use trs_sync_proto::ids::FolderId;

use crate::secrets::SecretStore;

/// The secret that holds the key of the private messages' file.
pub(crate) const DIRECT_KEY: &str = "direct-store";

/// The secret that holds the key of a channel's messages file.
pub(crate) fn channel_key_name(id: &FolderId) -> String {
    format!("store-{id}")
}

/// Seals and opens the records of one file.
pub(crate) struct Sealer {
    key: LessSafeKey,
    aad: &'static [u8],
}

impl Sealer {
    /// The key stored as `name` in `secrets`, or a new one stored there. A store that cannot be read is an error
    /// (never a new key, which would make the file unreadable).
    pub(crate) fn load(secrets: &dyn SecretStore, name: &str, aad: &'static [u8]) -> io::Result<Sealer> {
        let key: [u8; 32] = match secrets.get(name)? {
            Some(b) => b.try_into().map_err(|_| io::Error::new(io::ErrorKind::InvalidData, format!("the key {name} is damaged")))?,
            None => {
                let mut k = [0u8; 32];
                SystemRandom::new().fill(&mut k).map_err(|_| io::Error::other("no random numbers"))?;
                secrets.put(name, &k)?;
                k
            }
        };
        Ok(Sealer::new(key, aad))
    }

    pub(crate) fn new(key: [u8; 32], aad: &'static [u8]) -> Sealer {
        Sealer { key: LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &key).expect("a 32-byte key")), aad }
    }

    /// `nonce ‖ ciphertext ‖ tag`.
    pub(crate) fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_LEN];
        SystemRandom::new().fill(&mut nonce).expect("random numbers");
        let mut out = Vec::with_capacity(NONCE_LEN + plain.len() + 16);
        out.extend_from_slice(&nonce);
        let mut body = plain.to_vec();
        self.key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(self.aad), &mut body).expect("sealing in memory");
        out.extend_from_slice(&body);
        out
    }

    /// The plain record, or `None` when it was not sealed with this key (or was changed).
    pub(crate) fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        let (nonce, body) = sealed.split_at_checked(NONCE_LEN)?;
        let mut body = body.to_vec();
        let n = Nonce::try_assume_unique_for_key(nonce).ok()?;
        let plain = self.key.open_in_place(n, Aad::from(self.aad), &mut body).ok()?;
        Some(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::FileSecrets;

    #[test]
    fn a_record_opens_only_with_its_key_and_file() {
        let s = Sealer::new([1; 32], b"SWCH");
        let sealed = s.seal(b"hello there");
        assert!(!sealed.windows(5).any(|w| w == b"hello"), "no text on disk");
        assert_eq!(s.open(&sealed).as_deref(), Some(&b"hello there"[..]));
        assert_ne!(s.seal(b"hello there"), sealed, "a new nonce each time");
        assert_eq!(Sealer::new([2; 32], b"SWCH").open(&sealed), None, "another key");
        assert_eq!(Sealer::new([1; 32], b"SWDM").open(&sealed), None, "another file");
        let mut changed = sealed.clone();
        changed[14] ^= 1;
        assert_eq!(s.open(&changed), None, "changed");
        assert_eq!(s.open(&sealed[..5]), None, "cut off");
    }

    #[test]
    fn the_key_is_made_once_and_kept() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = FileSecrets::new(dir.path().join("secrets"));
        let sealed = Sealer::load(&secrets, "store-x", b"SWCH").unwrap().seal(b"kept");
        assert_eq!(Sealer::load(&secrets, "store-x", b"SWCH").unwrap().open(&sealed).as_deref(), Some(&b"kept"[..]));
        secrets.put("store-x", b"short").unwrap();
        assert!(Sealer::load(&secrets, "store-x", b"SWCH").is_err(), "a damaged key is not replaced");
    }
}
