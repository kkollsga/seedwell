//! Folder keys: the folder's secret `S`, which only owner devices hold, and its public key, which every member
//! has; what is derived from them for discovery (SPEC §7G.1). Who may do what is in [`crate::members`].
//!
//! ```text
//! S  : 32 random bytes (owners only)
//! seed = H(S, "seedwell-sync/v1/sign")  → Ed25519 key pair; pk = public
//! folder_id = SHA-256(pk)
//! lan_key = H(pk, "seedwell-sync/v1/lan")
//! dht_topic(day) = SHA-1("seedwell-sync/v1/dht" ‖ pk ‖ day as u32 BE)
//! lan_tag(hour, nonce8) = HMAC-SHA256(lan_key, "seedwell-sync/v1/lan-tag" ‖ hour u32 BE ‖ nonce8)[..8]
//! ```
//!
//! `H(k, label)` is HKDF-SHA256 with an empty salt, `k` as the input key material and `label` as the info.
//! Secrets never print: `Debug` shows the folder id only.

use std::fmt;

use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, digest};
use ring::signature::{Ed25519KeyPair, KeyPair};
use ring::{hkdf, hmac};

use crate::ids::FolderId;
use crate::random::{Random, RandomFailed, array};

/// The TLS exporter label both sides use for the 32-byte value a proof is bound to (RFC 8446 §7.5).
pub const EXPORTER_LABEL: &str = "EXPORTER-seedwell-sync-v1";

const SIGN: &str = "seedwell-sync/v1/sign";
const LAN: &str = "seedwell-sync/v1/lan";
const DHT: &str = "seedwell-sync/v1/dht";
const LAN_TAG: &str = "seedwell-sync/v1/lan-tag";

/// What a device may do with a folder's files. Ordered: `Storage < Read < Write`. A device's
/// [`crate::members::Role`] gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Access {
    /// Store and pass on data it cannot read (encrypted members; v1.x).
    Storage,
    /// Receive and pass on changes, never send its own (Read only).
    Read,
    /// Change files (Read & write, Admin, Owner).
    Write,
}

/// `out` = HKDF-SHA256(salt empty, ikm, info).
fn hkdf_fill(ikm: &[u8], info: &[u8], out: &mut [u8]) {
    struct Len(usize);
    impl hkdf::KeyType for Len {
        fn len(&self) -> usize {
            self.0
        }
    }
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, &[]).extract(ikm);
    let filled = prk.expand(&[info], Len(out.len())).and_then(|okm| okm.fill(out));
    // ring refuses only outputs longer than 255 hash lengths; every caller asks for 32 bytes.
    if filled.is_err() {
        unreachable!("HKDF output of {} bytes", out.len());
    }
}

/// `H(k, label)`: one step of the derivation.
fn h(k: &[u8], label: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    hkdf_fill(k, label.as_bytes(), &mut out);
    out
}

fn mac(key: &[u8; 32], parts: &[&[u8]]) -> hmac::Tag {
    let mut ctx = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    parts.iter().for_each(|p| ctx.update(p));
    ctx.sign()
}

/// The folder signing key pair from `S`.
fn signing_pair(s: &[u8; 32]) -> Ed25519KeyPair {
    match Ed25519KeyPair::from_seed_unchecked(&h(s, SIGN)) {
        Ok(pair) => pair,
        // ring rejects only seeds of the wrong length; HKDF gave exactly 32 bytes.
        Err(e) => unreachable!("ring refused a 32-byte Ed25519 seed: {e}"),
    }
}

/// The folder secret `S` and the signing key it gives: only owner devices hold it.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderSecret {
    s: [u8; 32],
    pk: [u8; 32],
}

impl FolderSecret {
    /// A new folder's secret.
    pub fn generate(rng: &mut dyn Random) -> Result<Self, RandomFailed> {
        Ok(Self::from_bytes(array(rng)?))
    }

    /// The secret `S` itself (from the secret store, or from a co-owner).
    pub fn from_bytes(s: [u8; 32]) -> Self {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(signing_pair(&s).public_key().as_ref());
        FolderSecret { s, pk }
    }

    /// `S`. A secret: for storage and for a new co-owner only, never log it.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.s
    }

    /// The folder signing public key.
    pub fn public_key(&self) -> &[u8; 32] {
        &self.pk
    }

    /// Signs `msg` with the folder key (owner statements and invites, [`crate::members`]).
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        let mut out = [0u8; 64];
        out.copy_from_slice(signing_pair(&self.s).sign(msg).as_ref());
        out
    }
}

/// What this device holds of a folder: the secret (an owner), or the public key only (every other member).
#[derive(Clone, PartialEq, Eq)]
pub enum FolderKey {
    /// An owner device: it can sign for the folder.
    Owner(FolderSecret),
    /// Any other member: the folder's public key.
    Member([u8; 32]),
}

impl FolderKey {
    /// A new folder: a fresh secret, held by its first owner.
    pub fn generate(rng: &mut dyn Random) -> Result<Self, RandomFailed> {
        FolderSecret::generate(rng).map(FolderKey::Owner)
    }

    /// The folder signing public key.
    pub fn public_key(&self) -> &[u8; 32] {
        match self {
            FolderKey::Owner(s) => s.public_key(),
            FolderKey::Member(pk) => pk,
        }
    }

    /// The secret, on an owner device.
    pub fn secret(&self) -> Option<&FolderSecret> {
        match self {
            FolderKey::Owner(s) => Some(s),
            FolderKey::Member(_) => None,
        }
    }

    /// The folder id, SHA-256 of the public key.
    pub fn folder_id(&self) -> FolderId {
        FolderId::of_public_key(self.public_key())
    }

    /// The DHT topic for Unix day `day` (UTC).
    pub fn dht_topic(&self, day: u32) -> [u8; 20] {
        dht_topic(self.public_key(), day)
    }

    /// The key for LAN announcement tags.
    pub fn lan_key(&self) -> [u8; 32] {
        lan_key(self.public_key())
    }

    /// The LAN announcement tag for Unix hour `hour` and an announcement's random nonce.
    pub fn lan_tag(&self, hour: u32, nonce: &[u8; 8]) -> [u8; 8] {
        lan_tag(&self.lan_key(), hour, nonce)
    }
}

/// Why a link was refused. The message is meant for the person who pasted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// A bare `SWW-`/`SWR-` key from a test build: keys are no longer shared, links are.
    OldKey,
    /// Not a `seedwell://sync/` link.
    Link,
    /// The link is damaged (cut off, a typo, or not base32).
    Damaged,
    /// The link's name is not valid percent-encoded UTF-8, or longer than [`MAX_LINK_NAME`] bytes.
    Name,
    /// A link of a later format (its check is good, its invitation version unknown): made by a newer Seedwell.
    Newer,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::OldKey => f.write_str("folder keys are no longer shared: ask for a link to the folder"),
            KeyError::Link => f.write_str("not a Seedwell folder link"),
            KeyError::Damaged => f.write_str("the folder link is damaged or incomplete"),
            KeyError::Name => f.write_str("the folder name in the link is damaged"),
            KeyError::Newer => f.write_str("this link was made by a newer Seedwell: update Seedwell to use it"),
        }
    }
}

impl std::error::Error for KeyError {}

/// Whether `text` looks like a bare folder key of the earlier test builds (`SWW-…`, `SWR-…`, `SWE-…`).
pub fn is_old_key(text: &str) -> bool {
    let t = text.trim().to_ascii_uppercase();
    ["SWW-", "SWR-", "SWE-"].iter().any(|p| t.starts_with(p))
}

/// Whether `sig` is the folder key's signature of `msg`.
pub fn verify_folder(folder_pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    crate::device::verify(folder_pk, msg, sig)
}

/// The DHT topic for the folder with public key `folder_pk` on Unix day `day`.
pub fn dht_topic(folder_pk: &[u8; 32], day: u32) -> [u8; 20] {

    let mut out = [0u8; 20];
    let msg = [DHT.as_bytes(), folder_pk, &day.to_be_bytes()].concat();
    out.copy_from_slice(digest(&SHA1_FOR_LEGACY_USE_ONLY, &msg).as_ref());
    out
}

/// `H(pk, "seedwell-sync/v1/lan")`: the public key is the input so that every level can compute it.
pub fn lan_key(folder_pk: &[u8; 32]) -> [u8; 32] {
    h(folder_pk, LAN)
}

/// `HMAC-SHA256(lan_key, "seedwell-sync/v1/lan-tag" ‖ hour BE ‖ nonce)[..8]`: lets a member recognise its
/// folder in a LAN broadcast without the broadcast naming it.
pub fn lan_tag(lan_key: &[u8; 32], hour: u32, nonce: &[u8; 8]) -> [u8; 8] {
    let tag = mac(lan_key, &[LAN_TAG.as_bytes(), &hour.to_be_bytes(), nonce]);
    let mut out = [0u8; 8];
    out.copy_from_slice(&tag.as_ref()[..8]);
    out
}

/// One proof of membership, as sent in [`crate::msg::Join`]. The TLS session already authenticates the device's
/// key; a proof brings the statements that make it a member, or an invitation it redeems.
#[derive(Clone, PartialEq, Eq)]
pub enum Proof {
    /// A membership statement about the sender (its grant, or the grant of the admin that signed it):
    /// [`crate::members::Statement`] in wire form.
    Grant(Vec<u8>),
    /// An invitation being redeemed: [`crate::members::Invite`] in wire form ‖ the token's 64-byte signature of
    /// the session ([`crate::members::redeem_payload`]).
    Invite(Vec<u8>),
    /// A kind this version does not know (from a newer peer): carried, never trusted.
    Unknown {
        /// Its wire kind.
        kind: u8,
        /// Its bytes.
        data: Vec<u8>,
    },
}

impl fmt::Debug for Proof {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Proofs are not secrets, but a full dump of them is noise in a log.
        match self {
            Proof::Grant(_) => f.write_str("Proof::Grant"),
            Proof::Invite(_) => f.write_str("Proof::Invite"),
            Proof::Unknown { kind, data } => write!(f, "Proof::Unknown({kind}, {} bytes)", data.len()),
        }
    }
}

/// Longest folder name a link may carry, in bytes.
pub const MAX_LINK_NAME: usize = 255;

/// Every byte but RFC 3986's unreserved characters as `%XX`.
pub(crate) fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `%XX` escapes (and `+` as a space, as HTML forms write it) back to UTF-8.
pub(crate) fn percent_decode(s: &str) -> Result<String, KeyError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let digit = |j: usize| b.get(j).and_then(|&c| (c as char).to_digit(16)).ok_or(KeyError::Name);
                out.push((digit(i + 1)? * 16 + digit(i + 2)?) as u8);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
        if out.len() > MAX_LINK_NAME {
            return Err(KeyError::Name);
        }
    }
    String::from_utf8(out).map_err(|_| KeyError::Name)
}

impl fmt::Debug for FolderSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FolderSecret({:?})", FolderId::of_public_key(&self.pk))
    }
}

impl fmt::Debug for FolderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = if self.secret().is_some() { "owner" } else { "member" };
        write!(f, "FolderKey({level}, {:?})", self.folder_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::digest::SHA256;

    fn sha256(parts: &[&[u8]]) -> [u8; 32] {
        let mut ctx = ring::digest::Context::new(&SHA256);
        parts.iter().for_each(|p| ctx.update(p));
        let mut out = [0u8; 32];
        out.copy_from_slice(ctx.finish().as_ref());
        out
    }

    fn key(n: u8) -> FolderKey {
        FolderKey::Owner(FolderSecret::from_bytes([n; 32]))
    }

    #[test]
    fn hkdf_matches_rfc5869_test_case_3() {
        // Empty salt and info, as this crate uses it.
        let mut okm = [0u8; 42];
        hkdf_fill(&[0x0b; 22], b"", &mut okm);
        let want = "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8";
        assert_eq!(hex(&okm), want);
    }

    #[test]
    fn the_derivation_is_as_published() {
        let k = key(1);
        let pair = Ed25519KeyPair::from_seed_unchecked(&h(&[1u8; 32], "seedwell-sync/v1/sign")).unwrap();
        assert_eq!(k.public_key().as_slice(), pair.public_key().as_ref());
        assert_eq!(k.folder_id().0, sha256(&[k.public_key().as_slice()]));
        let member = FolderKey::Member(*k.public_key());
        assert_eq!(member.folder_id(), k.folder_id());
        assert!(member.secret().is_none());
    }

    #[test]
    fn folder_signatures_verify_with_the_public_key() {
        let s = FolderSecret::from_bytes([1; 32]);
        let sig = s.sign(b"statement");
        assert!(verify_folder(s.public_key(), b"statement", &sig));
        assert!(!verify_folder(s.public_key(), b"statemenT", &sig));
        assert!(!verify_folder(FolderSecret::from_bytes([2; 32]).public_key(), b"statement", &sig));
    }

    #[test]
    fn debug_never_shows_secrets() {
        let k = key(1);
        let shown = format!("{k:?} {:?}", k.secret().unwrap());
        assert_eq!(shown, format!("FolderKey(owner, {0:?}) FolderSecret({0:?})", k.folder_id()));
        assert!(!shown.contains(&hex(&[1u8; 32])[..8]));
    }

    #[test]
    fn old_keys_are_recognised() {
        assert!(is_old_key(" swr-ABCD"));
        assert!(is_old_key("SWW-x"));
        assert!(!is_old_key("seedwell://sync/ABCD"));
    }

    #[test]
    fn dht_topics_differ_per_day_and_per_folder() {
        let (a, b) = (key(1), key(2));
        assert_ne!(a.dht_topic(20_000), a.dht_topic(20_001));
        assert_ne!(a.dht_topic(20_000), b.dht_topic(20_000));
        let member = FolderKey::Member(*a.public_key());
        assert_eq!(a.dht_topic(20_000), member.dht_topic(20_000), "owners and members meet at the same topic");
        let msg = [b"seedwell-sync/v1/dht".as_slice(), a.public_key(), &20_000u32.to_be_bytes()].concat();
        assert_eq!(a.dht_topic(20_000).as_slice(), digest(&SHA1_FOR_LEGACY_USE_ONLY, &msg).as_ref());
    }

    #[test]
    fn lan_tags_are_shared_by_every_member_and_change_with_hour_and_nonce() {
        let w = key(1);
        let m = FolderKey::Member(*w.public_key());
        let n = [1u8; 8];
        assert_eq!(w.lan_key(), m.lan_key());
        assert_eq!(w.lan_tag(5, &n), m.lan_tag(5, &n));
        assert_ne!(w.lan_tag(5, &n), w.lan_tag(6, &n));
        assert_ne!(w.lan_tag(5, &n), w.lan_tag(5, &[2; 8]));
        assert_ne!(w.lan_tag(5, &n), key(2).lan_tag(5, &n));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
