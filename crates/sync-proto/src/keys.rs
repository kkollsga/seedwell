//! Folder keys: the three access levels, their one-way derivation, their text form and links, and the proofs a
//! device makes after TLS to show which level it holds (SPEC §7G.1).
//!
//! ```text
//! S  : 32 random bytes (Read & write secret)
//! seed = H(S, "seedwell-sync/v1/sign")  → Ed25519 key pair; pk = public
//! R  = H(S, "seedwell-sync/v1/read")
//! A  = H(R, "seedwell-sync/v1/storage")
//! folder_id = SHA-256(pk)
//! auth_r = H(R, "seedwell-sync/v1/auth-read")     auth_a = H(A, "seedwell-sync/v1/auth-storage")
//! lan_key = H(pk, "seedwell-sync/v1/lan")
//! dht_topic(day) = SHA-1("seedwell-sync/v1/dht" ‖ pk ‖ day as u32 BE)
//! lan_tag(hour, nonce8) = HMAC-SHA256(lan_key, "seedwell-sync/v1/lan-tag" ‖ hour u32 BE ‖ nonce8)[..8]
//! ```
//!
//! `H(k, label)` is HKDF-SHA256 with an empty salt, `k` as the input key material and `label` as the info.
//! Secrets never print: every `Debug` here shows the level and the folder id only.

use std::fmt;
use std::str::FromStr;

use ring::digest::{SHA1_FOR_LEGACY_USE_ONLY, SHA256, digest};
use ring::signature::{Ed25519KeyPair, KeyPair};
use ring::{hkdf, hmac};

use crate::base32::{self, Base32Error};
use crate::device;
use crate::ids::FolderId;
use crate::random::{Random, RandomFailed, array};

/// The TLS exporter label both sides use for the 32-byte value a proof is bound to (RFC 8446 §7.5).
pub const EXPORTER_LABEL: &str = "EXPORTER-seedwell-sync-v1";

const SIGN: &str = "seedwell-sync/v1/sign";
const READ: &str = "seedwell-sync/v1/read";
const STORAGE: &str = "seedwell-sync/v1/storage";
const AUTH_READ: &str = "seedwell-sync/v1/auth-read";
const AUTH_STORAGE: &str = "seedwell-sync/v1/auth-storage";
const LAN: &str = "seedwell-sync/v1/lan";
const DHT: &str = "seedwell-sync/v1/dht";
const LAN_TAG: &str = "seedwell-sync/v1/lan-tag";
const KEY_CHECK: &str = "seedwell-sync/v1/key";
const WRITER: &str = "seedwell-sync/v1/writer";
const PROOF_READ: &str = "seedwell-sync/v1/proof-read";
const PROOF_STORAGE: &str = "seedwell-sync/v1/proof-storage";

/// What a device may do in a folder. Ordered: `Storage < Read < Write`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Access {
    /// Store and pass on data it cannot read (the Encrypted key; v1.x).
    Storage,
    /// Receive and pass on changes, never send its own (the Read only key).
    Read,
    /// Change files and derive every key below (the Read & write key).
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

fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut ctx = ring::digest::Context::new(&SHA256);
    parts.iter().for_each(|p| ctx.update(p));
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.finish().as_ref());
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

/// The Read & write secret `S`, with what follows from it.
#[derive(Clone, PartialEq, Eq)]
pub struct FolderSecret {
    s: [u8; 32],
    read: ReadSecret,
}

/// The Read only secret `R` and the folder public key, with what follows from them.
#[derive(Clone, PartialEq, Eq)]
pub struct ReadSecret {
    r: [u8; 32],
    storage: StorageSecret,
}

/// The Encrypted (storage) secret `A` and the folder public key.
#[derive(Clone, PartialEq, Eq)]
pub struct StorageSecret {
    a: [u8; 32],
    pk: [u8; 32],
}

impl FolderSecret {
    /// A new folder's secret.
    pub fn generate(rng: &mut dyn Random) -> Result<Self, RandomFailed> {
        Ok(Self::from_bytes(array(rng)?))
    }

    /// The secret `S` itself (from the Keychain or a key's text form).
    pub fn from_bytes(s: [u8; 32]) -> Self {
        let mut pk = [0u8; 32];
        pk.copy_from_slice(signing_pair(&s).public_key().as_ref());
        let r = h(&s, READ);
        FolderSecret { s, read: ReadSecret::new(r, pk) }
    }

    /// `S`. A secret: for storage only, never log it.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.s
    }

    /// The Read only level below it.
    pub fn read(&self) -> &ReadSecret {
        &self.read
    }

    /// Issues a writer certificate for a device: `sign_folder_sk("seedwell-sync/v1/writer" ‖ folder_id ‖
    /// device_pk)`. Permanent for that device and folder; any holder of `S` can issue it.
    pub fn writer_cert(&self, device_pk: &[u8; 32]) -> [u8; 64] {
        let fid = FolderId::of_public_key(self.read.public_key());
        let sig = signing_pair(&self.s).sign(&writer_payload(&fid, device_pk));
        let mut out = [0u8; 64];
        out.copy_from_slice(sig.as_ref());
        out
    }
}

impl ReadSecret {
    fn new(r: [u8; 32], pk: [u8; 32]) -> Self {
        ReadSecret { r, storage: StorageSecret { a: h(&r, STORAGE), pk } }
    }

    /// The Encrypted level below it.
    pub fn storage(&self) -> &StorageSecret {
        &self.storage
    }

    /// The folder signing public key.
    pub fn public_key(&self) -> &[u8; 32] {
        &self.storage.pk
    }

    fn auth(&self) -> [u8; 32] {
        h(&self.r, AUTH_READ)
    }
}

impl StorageSecret {
    /// The folder signing public key.
    pub fn public_key(&self) -> &[u8; 32] {
        &self.pk
    }

    fn auth(&self) -> [u8; 32] {
        h(&self.a, AUTH_STORAGE)
    }
}

/// A folder key at one of the three levels.
#[derive(Clone, PartialEq, Eq)]
pub enum FolderKey {
    /// `SWW-`: change files; derive the keys below.
    ReadWrite(FolderSecret),
    /// `SWR-`: receive and pass on changes, never send its own.
    ReadOnly(ReadSecret),
    /// `SWE-`: store and pass on data it cannot read (v1.x).
    Encrypted(StorageSecret),
}

/// Why a key or link was refused. The message is meant for the person who pasted it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyError {
    /// Not `SWW-`, `SWR-` or `SWE-`.
    Prefix,
    /// The part after the prefix is not base32.
    Base32(Base32Error),
    /// The key is too short or too long for its prefix.
    Length,
    /// The check bytes do not match: a typo.
    Check,
    /// A `seedwell://` link that is not `seedwell://sync/<key>`.
    Link,
    /// The link's name is not valid percent-encoded UTF-8, or longer than [`MAX_LINK_NAME`] bytes.
    Name,
}

impl fmt::Display for KeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyError::Prefix => f.write_str("a folder key starts with SWW-, SWR- or SWE-"),
            KeyError::Base32(e) => write!(f, "the folder key is damaged: {e}"),
            KeyError::Length => f.write_str("the folder key is incomplete or has extra characters"),
            KeyError::Check => f.write_str("the folder key has a typo"),
            KeyError::Link => f.write_str("not a Seedwell folder link"),
            KeyError::Name => f.write_str("the folder name in the link is damaged"),
        }
    }
}

impl std::error::Error for KeyError {}

impl FolderKey {
    /// A new folder: a fresh Read & write key.
    pub fn generate(rng: &mut dyn Random) -> Result<Self, RandomFailed> {
        FolderSecret::generate(rng).map(FolderKey::ReadWrite)
    }

    /// The level this key gives.
    pub fn access(&self) -> Access {
        match self {
            FolderKey::ReadWrite(_) => Access::Write,
            FolderKey::ReadOnly(_) => Access::Read,
            FolderKey::Encrypted(_) => Access::Storage,
        }
    }

    /// The text prefix: `SWW`, `SWR` or `SWE`.
    pub fn prefix(&self) -> &'static str {
        match self {
            FolderKey::ReadWrite(_) => "SWW",
            FolderKey::ReadOnly(_) => "SWR",
            FolderKey::Encrypted(_) => "SWE",
        }
    }

    fn read(&self) -> Option<&ReadSecret> {
        match self {
            FolderKey::ReadWrite(s) => Some(s.read()),
            FolderKey::ReadOnly(r) => Some(r),
            FolderKey::Encrypted(_) => None,
        }
    }

    fn storage(&self) -> &StorageSecret {
        match self {
            FolderKey::ReadWrite(s) => s.read().storage(),
            FolderKey::ReadOnly(r) => r.storage(),
            FolderKey::Encrypted(a) => a,
        }
    }

    /// The folder signing public key (every level has it).
    pub fn public_key(&self) -> &[u8; 32] {
        &self.storage().pk
    }

    /// The folder id, SHA-256 of the public key.
    pub fn folder_id(&self) -> FolderId {
        FolderId::of_public_key(self.public_key())
    }

    /// The Read only key of this folder; `None` from an Encrypted key (it cannot go up).
    pub fn read_only(&self) -> Option<FolderKey> {
        self.read().map(|r| FolderKey::ReadOnly(r.clone()))
    }

    /// The Encrypted key of this folder.
    pub fn encrypted(&self) -> FolderKey {
        FolderKey::Encrypted(self.storage().clone())
    }

    /// The bytes the text form carries (the table in the README).
    fn payload(&self) -> Vec<u8> {
        match self {
            FolderKey::ReadWrite(s) => s.s.to_vec(),
            FolderKey::ReadOnly(r) => [r.r, r.storage.pk].concat(),
            FolderKey::Encrypted(a) => [a.a, a.pk].concat(),
        }
    }

    /// The text form, `<prefix>-<base32(payload ‖ check)>`. This is the shared secret: show it to the person,
    /// never log it.
    pub fn to_text(&self) -> String {
        let prefix = self.prefix();
        let mut bytes = self.payload();
        let check = key_check(prefix, &bytes);
        bytes.extend_from_slice(&check);
        format!("{prefix}-{}", base32::encode(&bytes))
    }

    /// Parses a key's text form (either case; spaces and extra `-` are ignored).
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        let text = text.trim();
        let (prefix, rest) = text.split_once('-').ok_or(KeyError::Prefix)?;
        let prefix = prefix.trim().to_ascii_uppercase();
        let size = match prefix.as_str() {
            "SWW" => 32,
            "SWR" | "SWE" => 64,
            _ => return Err(KeyError::Prefix),
        };
        let bytes = base32::decode(rest).map_err(KeyError::Base32)?;
        if bytes.len() != size + 2 {
            return Err(KeyError::Length);
        }
        let (payload, check) = bytes.split_at(size);
        // Not secret-dependent timing worth hiding: the check is public (anyone can compute it from the text).
        if key_check(&prefix, payload) != check {
            return Err(KeyError::Check);
        }
        let first = take32(&payload[..32]);
        Ok(match prefix.as_str() {
            "SWW" => FolderKey::ReadWrite(FolderSecret::from_bytes(first)),
            "SWR" => FolderKey::ReadOnly(ReadSecret::new(first, take32(&payload[32..]))),
            _ => FolderKey::Encrypted(StorageSecret { a: first, pk: take32(&payload[32..]) }),
        })
    }

    /// A writer certificate for `device_pk`; `None` below Read & write.
    pub fn writer_cert(&self, device_pk: &[u8; 32]) -> Option<[u8; 64]> {
        match self {
            FolderKey::ReadWrite(s) => Some(s.writer_cert(device_pk)),
            _ => None,
        }
    }

    /// Every proof this key can make for the device `device_pk` in the TLS session with exporter value `exporter`:
    /// a writer sends three, a reader two, a storage member one. Each verifier checks what it can.
    pub fn proofs(&self, device_pk: &[u8; 32], exporter: &[u8; 32]) -> Vec<Proof> {
        let fid = self.folder_id();
        let mut out = Vec::with_capacity(3);
        if let Some(cert) = self.writer_cert(device_pk) {
            out.push(Proof::Writer(cert));
        }
        if let Some(r) = self.read() {
            out.push(Proof::Read(proof_mac(&r.auth(), PROOF_READ, &fid, device_pk, exporter)));
        }
        out.push(Proof::Storage(proof_mac(&self.storage().auth(), PROOF_STORAGE, &fid, device_pk, exporter)));
        out
    }

    /// The highest level the peer `peer_pk` (its TLS raw public key) proved in the session with exporter value
    /// `exporter`, as far as this key can check: writer certificates against the folder public key (any level),
    /// reader proofs with `R` (Read and up), storage proofs with `A` (every level). `None`: drop the peer.
    pub fn verify_proofs(&self, peer_pk: &[u8; 32], exporter: &[u8; 32], proofs: &[Proof]) -> Option<Access> {
        let fid = self.folder_id();
        let check = |auth: &[u8; 32], label: &str, tag: &[u8; 32]| {
            let msg = proof_message(label, &fid, peer_pk, exporter);
            hmac::verify(&hmac::Key::new(hmac::HMAC_SHA256, auth), &msg, tag).is_ok()
        };
        let level = |p: &Proof| match p {
            Proof::Writer(cert) => verify_writer_cert(self.public_key(), peer_pk, cert).then_some(Access::Write),
            Proof::Read(tag) => self.read().filter(|r| check(&r.auth(), PROOF_READ, tag)).map(|_| Access::Read),
            Proof::Storage(tag) => check(&self.storage().auth(), PROOF_STORAGE, tag).then_some(Access::Storage),
            Proof::Unknown { .. } => None,
        };
        proofs.iter().filter_map(level).max()
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

impl FromStr for FolderKey {
    type Err = KeyError;
    fn from_str(s: &str) -> Result<Self, KeyError> {
        FolderKey::parse(s)
    }
}

fn take32(b: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(b);
    out
}

/// The 2 check bytes of a key's text form.
fn key_check(prefix: &str, payload: &[u8]) -> [u8; 2] {
    let d = sha256(&[KEY_CHECK.as_bytes(), prefix.as_bytes(), payload]);
    [d[0], d[1]]
}

fn writer_payload(fid: &FolderId, device_pk: &[u8; 32]) -> Vec<u8> {
    [WRITER.as_bytes(), &fid.0, device_pk].concat()
}

fn proof_message(label: &str, fid: &FolderId, device_pk: &[u8; 32], exporter: &[u8; 32]) -> Vec<u8> {
    [label.as_bytes(), &fid.0, device_pk, exporter].concat()
}

fn proof_mac(auth: &[u8; 32], label: &str, fid: &FolderId, device_pk: &[u8; 32], exporter: &[u8; 32]) -> [u8; 32] {
    take32(mac(auth, &[&proof_message(label, fid, device_pk, exporter)]).as_ref())
}

/// Whether `cert` is a writer certificate for `device_pk` in the folder with public key `folder_pk`.
pub fn verify_writer_cert(folder_pk: &[u8; 32], device_pk: &[u8; 32], cert: &[u8; 64]) -> bool {
    device::verify(folder_pk, &writer_payload(&FolderId::of_public_key(folder_pk), device_pk), cert)
}

/// `SHA-1("seedwell-sync/v1/dht" ‖ pk ‖ day BE)`: where members meet on the DHT that day. It changes daily so
/// the DHT cannot follow a folder over time, and only members (who know `pk`) can compute it.
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

/// One proof of access, as sent in [`crate::msg::Join`].
#[derive(Clone, PartialEq, Eq)]
pub enum Proof {
    /// A writer certificate (checked with the folder public key: by every member).
    Writer([u8; 64]),
    /// A reader proof, HMAC with `auth_r` (checked by writers and readers).
    Read([u8; 32]),
    /// A storage proof, HMAC with `auth_a` (checked by everyone).
    Storage([u8; 32]),
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
            Proof::Writer(_) => f.write_str("Proof::Writer"),
            Proof::Read(_) => f.write_str("Proof::Read"),
            Proof::Storage(_) => f.write_str("Proof::Storage"),
            Proof::Unknown { kind, data } => write!(f, "Proof::Unknown({kind}, {} bytes)", data.len()),
        }
    }
}

/// Longest folder name a link may carry, in bytes.
pub const MAX_LINK_NAME: usize = 255;

const LINK_PREFIX: &str = "seedwell://sync/";

/// A folder link, `seedwell://sync/<key>?name=<percent-encoded UTF-8>`. The name is only a hint for the joining
/// device.
#[derive(Clone, PartialEq, Eq)]
pub struct Link {
    /// The key.
    pub key: FolderKey,
    /// The folder's name, if the link has one.
    pub name: Option<String>,
}

impl Link {
    /// Parses a link, or a bare key. Surrounding whitespace and the `<` `>` quotes mail clients put around links
    /// are allowed; query fields other than `name` are ignored (later versions may add some).
    pub fn parse(text: &str) -> Result<Self, KeyError> {
        let mut text = text.trim();
        if let Some(inner) = text.strip_prefix('<').and_then(|t| t.strip_suffix('>')) {
            text = inner.trim();
        }
        let has_scheme = text.len() >= LINK_PREFIX.len()
            && text.is_char_boundary(LINK_PREFIX.len())
            && text[..LINK_PREFIX.len()].eq_ignore_ascii_case(LINK_PREFIX);
        if !has_scheme {
            if text.contains("://") {
                return Err(KeyError::Link);
            }
            return Ok(Link { key: FolderKey::parse(text)?, name: None });
        }
        let rest = &text[LINK_PREFIX.len()..];
        let (key, query) = rest.split_once('?').unwrap_or((rest, ""));
        let key = FolderKey::parse(key)?;
        let mut name = None;
        for field in query.split('&') {
            if let Some(v) = field.strip_prefix("name=") {
                name = Some(percent_decode(v)?).filter(|n| !n.is_empty());
            }
        }
        Ok(Link { key, name })
    }

    /// The link's text. It carries the key: show it to the person, never log it.
    pub fn format(&self) -> String {
        let mut out = format!("{LINK_PREFIX}{}", self.key.to_text());
        if let Some(name) = self.name.as_deref().filter(|n| !n.is_empty()) {
            out.push_str("?name=");
            out.push_str(&percent_encode(name));
        }
        out
    }
}

impl fmt::Debug for Link {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Link").field("key", &self.key).field("name", &self.name).finish()
    }
}

/// Every byte but RFC 3986's unreserved characters as `%XX`.
fn percent_encode(s: &str) -> String {
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
fn percent_decode(s: &str) -> Result<String, KeyError> {
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
        write!(f, "FolderSecret(SWW, {:?})", FolderId::of_public_key(self.read.public_key()))
    }
}

impl fmt::Debug for ReadSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReadSecret(SWR, {:?})", FolderId::of_public_key(self.public_key()))
    }
}

impl fmt::Debug for StorageSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "StorageSecret(SWE, {:?})", FolderId::of_public_key(&self.pk))
    }
}

impl fmt::Debug for FolderKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FolderKey({}, {:?})", self.prefix(), self.folder_id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::DeviceKey;

    fn key(n: u8) -> FolderKey {
        FolderKey::ReadWrite(FolderSecret::from_bytes([n; 32]))
    }

    fn levels() -> [FolderKey; 3] {
        let w = key(1);
        let r = w.read_only().unwrap();
        let a = w.encrypted();
        [w, r, a]
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
    fn every_level_roundtrips_through_text() {
        for k in levels() {
            let text = k.to_text();
            assert!(text.starts_with(&format!("{}-", k.prefix())), "{text}");
            let back = FolderKey::parse(&text).unwrap();
            assert_eq!(back, k);
            assert_eq!(back.access(), k.access());
            assert_eq!(back.folder_id(), k.folder_id());
            assert_eq!(text.parse::<FolderKey>().unwrap(), k);
        }
    }

    #[test]
    fn text_lengths_follow_the_payload() {
        let [w, r, a] = levels();
        assert_eq!(w.to_text().len(), 4 + 55, "34 bytes");
        assert_eq!(r.to_text().len(), 4 + 106, "66 bytes");
        assert_eq!(a.to_text().len(), 4 + 106);
    }

    #[test]
    fn derived_keys_equal_parsed_keys() {
        let [w, r, a] = levels();
        let r_text = r.to_text();
        assert_eq!(FolderKey::parse(&r_text).unwrap(), w.read_only().unwrap());
        assert_eq!(FolderKey::parse(&r_text).unwrap().encrypted(), a);
        assert_eq!(FolderKey::parse(&a.to_text()).unwrap(), w.encrypted());
        assert!(a.read_only().is_none(), "an Encrypted key cannot go up");
        assert_eq!(r.read_only().unwrap(), r);
        assert!(w.public_key() == r.public_key() && r.public_key() == a.public_key());
    }

    #[test]
    fn the_derivation_is_as_published() {
        let s = [1u8; 32];
        let k = key(1);
        let seed = h(&s, "seedwell-sync/v1/sign");
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        assert_eq!(k.public_key().as_slice(), pair.public_key().as_ref());
        let r = h(&s, "seedwell-sync/v1/read");
        let a = h(&r, "seedwell-sync/v1/storage");
        let FolderKey::ReadOnly(rs) = k.read_only().unwrap() else { panic!() };
        assert_eq!(rs.r, r);
        assert_eq!(rs.storage.a, a);
        assert_eq!(k.folder_id().0, sha256(&[k.public_key().as_slice()]));
    }

    #[test]
    fn text_is_lenient_about_case_and_spacing() {
        let k = key(2).read_only().unwrap();
        let text = k.to_text();
        assert_eq!(FolderKey::parse(&text.to_lowercase()).unwrap(), k);
        let mut spaced = String::new();
        for (i, c) in text.chars().enumerate() {
            if i > 4 && i % 8 == 0 {
                spaced.push_str(" - ");
            }
            spaced.push(c);
        }
        assert_eq!(FolderKey::parse(&format!("  {spaced}\n")).unwrap(), k);
    }

    #[test]
    fn a_typo_is_caught() {
        for k in levels() {
            let text = k.to_text();
            for i in 4..text.len() {
                let mut b = text.clone().into_bytes();
                b[i] = if b[i] == b'A' { b'B' } else { b'A' };
                let typo = String::from_utf8(b).unwrap();
                // A changed last character may only touch padding bits; everything else must be refused.
                match FolderKey::parse(&typo) {
                    Err(KeyError::Check) => {}
                    Ok(same) if i == text.len() - 1 => assert_eq!(same, k),
                    other => panic!("position {i}: {other:?}"),
                }
            }
        }
        let text = key(3).to_text();
        assert_eq!(FolderKey::parse(&text[..text.len() - 2]), Err(KeyError::Length));
        assert_eq!(FolderKey::parse(&text.replacen("SWW", "SWR", 1)), Err(KeyError::Length));
        assert_eq!(FolderKey::parse(&text.replacen("SWW", "SWX", 1)), Err(KeyError::Prefix));
        assert_eq!(FolderKey::parse("SWW"), Err(KeyError::Prefix));
        assert!(matches!(FolderKey::parse("SWW-abc1"), Err(KeyError::Base32(_))));
    }

    #[test]
    fn a_swapped_prefix_is_caught() {
        // Same payload size, other prefix: the check covers the prefix.
        let r = key(4).read_only().unwrap().to_text();
        assert_eq!(FolderKey::parse(&r.replacen("SWR", "SWE", 1)), Err(KeyError::Check));
    }

    #[test]
    fn debug_never_shows_secrets() {
        for k in levels() {
            let shown = format!("{k:?}");
            assert_eq!(shown, format!("FolderKey({}, {:?})", k.prefix(), k.folder_id()));
            assert!(!shown.contains(&k.to_text()[4..14]));
        }
        let link = Link { key: key(1), name: Some("Photos".into()) };
        assert!(!format!("{link:?}").contains(&key(1).to_text()[4..14]));
    }

    #[test]
    fn proofs_verify_at_the_right_level_for_every_verifier() {
        let [w, r, a] = levels();
        let dev = DeviceKey::from_seed(&[9; 32]);
        let x = [7u8; 32];
        let cases = [
            // (prover, verifier, expected)
            (&w, &w, Access::Write),
            (&w, &r, Access::Write),
            (&w, &a, Access::Write),
            (&r, &w, Access::Read),
            (&r, &r, Access::Read),
            (&r, &a, Access::Storage), // a storage member cannot check reader proofs
            (&a, &w, Access::Storage),
            (&a, &r, Access::Storage),
            (&a, &a, Access::Storage),
        ];
        for (prover, verifier, want) in cases {
            let proofs = prover.proofs(dev.public_key(), &x);
            assert_eq!(proofs.len(), prover.access() as usize + 1, "a writer sends 3, a reader 2, storage 1");
            let got = verifier.verify_proofs(dev.public_key(), &x, &proofs);
            assert_eq!(got, Some(want), "{} proving to {}", prover.prefix(), verifier.prefix());
        }
    }

    #[test]
    fn proofs_are_bound_to_the_session_and_the_device() {
        let [w, r, a] = levels();
        let dev = DeviceKey::from_seed(&[9; 32]);
        let other = DeviceKey::from_seed(&[10; 32]);
        let (x, y) = ([7u8; 32], [8u8; 32]);
        for prover in [&w, &r, &a] {
            let proofs = prover.proofs(dev.public_key(), &x);
            for verifier in [&w, &r, &a] {
                assert_eq!(verifier.verify_proofs(other.public_key(), &x, &proofs), None, "another device");
                // Writer certificates are bound to the device (whose TLS key the session authenticates), not to
                // one session; the HMAC proofs are bound to the session.
                let want = (prover.access() == Access::Write).then_some(Access::Write);
                assert_eq!(verifier.verify_proofs(dev.public_key(), &y, &proofs), want, "another session");
            }
            let macs: Vec<Proof> = proofs.into_iter().filter(|p| !matches!(p, Proof::Writer(_))).collect();
            assert_eq!(w.verify_proofs(dev.public_key(), &y, &macs), None, "HMAC proofs, another session");
        }
    }

    #[test]
    fn proofs_from_another_folder_fail() {
        let dev = DeviceKey::from_seed(&[9; 32]);
        let x = [7u8; 32];
        let theirs = key(5).proofs(dev.public_key(), &x);
        for verifier in levels() {
            assert_eq!(verifier.verify_proofs(dev.public_key(), &x, &theirs), None);
        }
        assert_eq!(key(1).verify_proofs(dev.public_key(), &x, &[]), None);
        let unknown = [Proof::Unknown { kind: 9, data: vec![1, 2] }];
        assert_eq!(key(1).verify_proofs(dev.public_key(), &x, &unknown), None);
    }

    #[test]
    fn a_damaged_proof_fails() {
        let w = key(1);
        let dev = DeviceKey::from_seed(&[9; 32]);
        let x = [7u8; 32];
        for p in w.proofs(dev.public_key(), &x) {
            let bad = match p {
                Proof::Writer(mut c) => {
                    c[0] ^= 1;
                    Proof::Writer(c)
                }
                Proof::Read(mut t) => {
                    t[31] ^= 0x80;
                    Proof::Read(t)
                }
                Proof::Storage(mut t) => {
                    t[5] ^= 2;
                    Proof::Storage(t)
                }
                u => u,
            };
            assert_eq!(w.verify_proofs(dev.public_key(), &x, &[bad]), None);
        }
    }

    #[test]
    fn writer_certificates() {
        let w = key(1);
        let dev = DeviceKey::from_seed(&[9; 32]);
        let cert = w.writer_cert(dev.public_key()).unwrap();
        assert!(verify_writer_cert(w.public_key(), dev.public_key(), &cert));
        assert!(!verify_writer_cert(key(2).public_key(), dev.public_key(), &cert), "another folder");
        assert!(!verify_writer_cert(w.public_key(), &[0; 32], &cert), "another device");
        assert_eq!(w.writer_cert(dev.public_key()), Some(cert), "Ed25519 is deterministic: one cert per device");
        assert!(w.read_only().unwrap().writer_cert(dev.public_key()).is_none());
        assert!(w.encrypted().writer_cert(dev.public_key()).is_none());
    }

    #[test]
    fn dht_topics_differ_per_day_and_per_folder() {
        let (a, b) = (key(1), key(2));
        assert_ne!(a.dht_topic(20_000), a.dht_topic(20_001));
        assert_ne!(a.dht_topic(20_000), b.dht_topic(20_000));
        assert_eq!(a.dht_topic(20_000), a.encrypted().dht_topic(20_000), "every level meets at the same topic");
        let msg = [b"seedwell-sync/v1/dht".as_slice(), a.public_key(), &20_000u32.to_be_bytes()].concat();
        assert_eq!(a.dht_topic(20_000).as_slice(), digest(&SHA1_FOR_LEGACY_USE_ONLY, &msg).as_ref());
    }

    #[test]
    fn lan_tags_are_shared_by_every_level_and_change_with_hour_and_nonce() {
        let [w, r, a] = levels();
        let n = [1u8; 8];
        assert_eq!(w.lan_key(), a.lan_key());
        assert_eq!(w.lan_tag(5, &n), r.lan_tag(5, &n));
        assert_eq!(w.lan_tag(5, &n), a.lan_tag(5, &n));
        assert_ne!(w.lan_tag(5, &n), w.lan_tag(6, &n));
        assert_ne!(w.lan_tag(5, &n), w.lan_tag(5, &[2; 8]));
        assert_ne!(w.lan_tag(5, &n), key(2).lan_tag(5, &n));
    }

    #[test]
    fn links_roundtrip_and_tolerate_mail() {
        let k = key(1).read_only().unwrap();
        let link = Link { key: k.clone(), name: Some("Bilder & ting/2026 ✓".into()) };
        let text = link.format();
        assert!(text.starts_with("seedwell://sync/SWR-"), "{text}");
        assert!(!text[16..].contains(' ') && !text[16..].contains('&') && !text[16..].contains('/'));
        assert_eq!(Link::parse(&text).unwrap(), link);
        assert_eq!(Link::parse(&format!("  <{text}>\n")).unwrap(), link);
        assert_eq!(Link::parse(&k.to_text()).unwrap(), Link { key: k.clone(), name: None });
        assert_eq!(Link::parse(&format!("<{}>", k.to_text())).unwrap().key, k);
        let plus = format!("seedwell://sync/{}?v=2&name=My+Folder", k.to_text());
        assert_eq!(Link::parse(&plus).unwrap().name.as_deref(), Some("My Folder"));
        let upper = format!("SEEDWELL://SYNC/{}", k.to_text());
        assert_eq!(Link::parse(&upper).unwrap().key, k);
        let unnamed = Link { key: k.clone(), name: Some(String::new()) };
        assert_eq!(unnamed.format(), format!("seedwell://sync/{}", k.to_text()));
    }

    #[test]
    fn bad_links_are_refused() {
        let k = key(1).to_text();
        assert_eq!(Link::parse(&format!("seedwell://other/{k}")), Err(KeyError::Link));
        assert_eq!(Link::parse(&format!("https://sync/{k}")), Err(KeyError::Link));
        assert_eq!(Link::parse(&format!("seedwell://sync/{k}?name=%G1")), Err(KeyError::Name));
        assert_eq!(Link::parse(&format!("seedwell://sync/{k}?name=%C3")), Err(KeyError::Name));
        assert_eq!(Link::parse(&format!("seedwell://sync/{k}?name=%4")), Err(KeyError::Name));
        assert_eq!(Link::parse(&format!("seedwell://sync/{k}?name=%+1")), Err(KeyError::Name));
        let long = "a".repeat(MAX_LINK_NAME + 1);
        assert_eq!(Link::parse(&format!("seedwell://sync/{k}?name={long}")), Err(KeyError::Name));
        assert_eq!(Link::parse("seedwell://sync/"), Err(KeyError::Prefix));
        assert_eq!(Link::parse("seedwell://syncæ"), Err(KeyError::Link));
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }
}
