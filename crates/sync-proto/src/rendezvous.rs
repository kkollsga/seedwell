//! A device's address record in a rendezvous (SPEC §7G.3, 0.1.13): where it can be reached now, sealed so that only a
//! device that knows it can read it, and signed by it.
//!
//! A rendezvous is a small key-value service the user runs on their own Cloudflare account
//! (`tools/rendezvous-worker`). Each device publishes one record there and looks up the records of the devices it
//! shares a folder, a channel or a conversation with when they are not connected. The service stores opaque bytes
//! under an id: it never sees a device key, an address, a folder or a file.
//!
//! ```text
//! id        = SHA-256("seedwell-sync/v2/rendezvous/id" ‖ device id)       (lower-case hex in the URL: /v1/r/<id>)
//! prk       = HKDF-SHA256-Extract(salt = "seedwell-sync/v2/rendezvous", ikm = device id)
//! key       = HKDF-Expand(prk, "key", 32)
//! record    = version u8 (1) ‖ nonce(12, random) ‖ ChaCha20-Poly1305(key, nonce, aad = version ‖ id, plain)
//! plain     = body ‖ signature(64)
//! body      = device public key(32) ‖ issued u64 (Unix milliseconds, big-endian) ‖ count u8 ‖ count × address ‖ field*
//! address   = as in the link's address block ([`crate::reach`]): kind u8 ‖ ip (4 or 16) ‖ port u16
//! field     = tag u8 ‖ varint length ‖ bytes (none yet; a reader skips the ones it does not know)
//! signature = Ed25519 by the device key over "seedwell-sync/v2/rendezvous/record" ‖ id ‖ body
//! ```
//!
//! The device id is the SHA-256 of the device's public key, so the id and the key are a hash of the public key: a
//! device that knows another one (a fellow member, a link's issuer, a conversation's device) can find and open its
//! record; the service, which sees only the id, cannot. The signature inside the seal proves the record is the
//! device's own (a reader checks that the key in it is the device's), and its time lets a reader keep the newest
//! record it has seen, so an old record served again changes nothing.
//!
//! A sealed record is at most [`MAX_RECORD`] bytes.

use std::net::SocketAddr;

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::digest::{SHA256, digest};
use ring::hkdf::{HKDF_SHA256, KeyType, Salt};

use crate::codec::Reader;
#[cfg(test)]
use crate::codec::put_str;
use crate::device::{self, DeviceKey};
use crate::ids::DeviceId;
use crate::random::{Random, RandomFailed};
use crate::reach::{put_addr, read_addr};

const ID_CONTEXT: &[u8] = b"seedwell-sync/v2/rendezvous/id";
const KEY_CONTEXT: &[u8] = b"seedwell-sync/v2/rendezvous";
const SIGNED: &[u8] = b"seedwell-sync/v2/rendezvous/record";
/// The record's version.
const VERSION: u8 = 1;
/// The largest sealed record (the rendezvous refuses larger ones).
pub const MAX_RECORD: usize = 1024;
/// Addresses on local networks a record carries, at most.
pub const MAX_LAN: usize = 8;
const BODY_HEAD: usize = 32 + 8 + 1;

/// Where a device can be reached, as it published it.
#[derive(Clone, PartialEq, Eq)]
pub struct Record {
    /// The device's public key (its id is the SHA-256 of it).
    pub device_pk: [u8; 32],
    /// When it was published, Unix milliseconds (by the device's clock).
    pub issued_ms: u64,
    /// Its addresses on its local networks, with the Folder Sync port.
    pub lan: Vec<SocketAddr>,
    /// Its public address and port, as its router maps them.
    pub public: Option<SocketAddr>,
}

impl std::fmt::Debug for Record {
    // The addresses stay out of logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Record({:?}, at {} ms, {} local, public {})", DeviceId::of_public_key(&self.device_pk), self.issued_ms, self.lan.len(), self.public.is_some())
    }
}

/// Why a record did not open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// Not a record of this version, damaged, or sealed for another device.
    Sealed,
    /// It opened, but its contents are malformed.
    Malformed,
    /// It names another device's key.
    WrongDevice,
    /// The signature does not verify.
    BadSignature,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            OpenError::Sealed => "the record does not open (damaged, or not this device's)",
            OpenError::Malformed => "the record is malformed",
            OpenError::WrongDevice => "the record names another device",
            OpenError::BadSignature => "the record's signature does not verify",
        })
    }
}

impl std::error::Error for OpenError {}

/// The id `device`'s record is kept under.
pub fn record_id(device: &DeviceId) -> [u8; 32] {
    let mut b = ID_CONTEXT.to_vec();
    b.extend_from_slice(&device.0);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest(&SHA256, &b).as_ref());
    out
}

/// [`record_id`] as the rendezvous names it: 64 lower-case hex digits.
pub fn record_id_hex(device: &DeviceId) -> String {
    record_id(device).iter().map(|b| format!("{b:02x}")).collect()
}

struct Len32;

impl KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

fn aead(device: &DeviceId) -> Option<LessSafeKey> {
    let prk = Salt::new(HKDF_SHA256, KEY_CONTEXT).extract(&device.0);
    let mut k = [0u8; 32];
    prk.expand(&[b"key".as_slice()], Len32).ok()?.fill(&mut k).ok()?;
    Some(LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &k).ok()?))
}

fn signed_bytes(id: &[u8; 32], body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIGNED.len() + 32 + body.len());
    m.extend_from_slice(SIGNED);
    m.extend_from_slice(id);
    m.extend_from_slice(body);
    m
}

impl Record {
    /// Every address, the local ones first.
    pub fn addresses(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.lan.iter().copied().chain(self.public)
    }

    /// The device it is for.
    pub fn device(&self) -> DeviceId {
        DeviceId::of_public_key(&self.device_pk)
    }

    fn body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(BODY_HEAD + 6 * 19);
        out.extend_from_slice(&self.device_pk);
        out.extend_from_slice(&self.issued_ms.to_be_bytes());
        let lan: Vec<&SocketAddr> = self.lan.iter().filter(|a| a.port() != 0).take(MAX_LAN).collect();
        let public = self.public.filter(|a| a.port() != 0);
        out.push((lan.len() + usize::from(public.is_some())) as u8);
        for a in lan {
            put_addr(&mut out, a, false);
        }
        if let Some(a) = public {
            put_addr(&mut out, &a, true);
        }
        out
    }

    fn parse_body(b: &[u8]) -> Option<Record> {
        let r = &mut Reader::new(b);
        let device_pk = r.array::<32>("device key").ok()?;
        let issued_ms = u64::from_be_bytes(r.array("issued").ok()?);
        let n = r.u8("addresses").ok()?;
        let mut out = Record { device_pk, issued_ms, lan: Vec::new(), public: None };
        for _ in 0..n {
            // A kind of a later version ends the list (its length is unknown).
            let Some((a, public)) = read_addr(r)? else { break };
            if a.port() == 0 {
                continue;
            }
            if public {
                out.public = out.public.or(Some(a));
            } else if out.lan.len() < MAX_LAN {
                out.lan.push(a);
            }
        }
        // Fields of later versions are skipped.
        Some(out)
    }

    /// The sealed record, signed with `key` (whose public key replaces [`Record::device_pk`]); a random nonce.
    pub fn seal(&self, key: &DeviceKey, rng: &mut dyn Random) -> Result<Vec<u8>, RandomFailed> {
        let me = Record { device_pk: *key.public_key(), ..self.clone() };
        let device = key.device_id();
        let id = record_id(&device);
        let mut plain = me.body();
        let sig = key.sign(&signed_bytes(&id, &plain));
        plain.extend_from_slice(&sig);
        let mut nonce = [0u8; NONCE_LEN];
        rng.fill(&mut nonce)?;
        let aad = aad(&id);
        let Some(k) = aead(&device) else { return Err(RandomFailed) };
        k.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(&aad), &mut plain).map_err(|_| RandomFailed)?;
        let mut out = Vec::with_capacity(1 + NONCE_LEN + plain.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&plain);
        Ok(out)
    }

    /// Opens `device`'s sealed record and checks that `device` signed it.
    pub fn open(sealed: &[u8], device: &DeviceId) -> Result<Record, OpenError> {
        let (&version, rest) = sealed.split_first().ok_or(OpenError::Sealed)?;
        if version != VERSION || rest.len() < NONCE_LEN || sealed.len() > MAX_RECORD {
            return Err(OpenError::Sealed);
        }
        let (nonce, body) = rest.split_at(NONCE_LEN);
        let id = record_id(device);
        let k = aead(device).ok_or(OpenError::Sealed)?;
        let mut body = body.to_vec();
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| OpenError::Sealed)?;
        let plain = k.open_in_place(nonce, Aad::from(&aad(&id)), &mut body).map_err(|_| OpenError::Sealed)?;
        if plain.len() < BODY_HEAD + 64 {
            return Err(OpenError::Malformed);
        }
        let (signed, sig) = plain.split_at(plain.len() - 64);
        let record = Record::parse_body(signed).ok_or(OpenError::Malformed)?;
        if record.device() != *device {
            return Err(OpenError::WrongDevice);
        }
        let sig: [u8; 64] = sig.try_into().map_err(|_| OpenError::Malformed)?;
        if !device::verify(&record.device_pk, &signed_bytes(&id, signed), &sig) {
            return Err(OpenError::BadSignature);
        }
        Ok(record)
    }
}

fn aad(id: &[u8; 32]) -> [u8; 33] {
    let mut a = [0u8; 33];
    a[0] = VERSION;
    a[1..].copy_from_slice(id);
    a
}

/// A later version's field after the addresses: `tag ‖ put_str(bytes)`, like the link block's.
#[cfg(test)]
fn put_field(out: &mut Vec<u8>, tag: u8, value: &str) {
    out.push(tag);
    put_str(out, value);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Seq(u8);
    impl Random for Seq {
        fn fill(&mut self, out: &mut [u8]) -> Result<(), RandomFailed> {
            for b in out {
                self.0 = self.0.wrapping_add(1);
                *b = self.0;
            }
            Ok(())
        }
    }

    fn sample(k: &DeviceKey) -> Record {
        Record {
            device_pk: *k.public_key(),
            issued_ms: 1_792_000_000_123,
            lan: vec!["192.168.68.57:55799".parse().unwrap(), "[fd00::5]:55799".parse().unwrap()],
            public: Some("84.212.3.4:41000".parse().unwrap()),
        }
    }

    #[test]
    fn a_record_round_trips_and_is_sealed_and_signed() {
        let k = DeviceKey::from_seed(&[4; 32]);
        let r = sample(&k);
        let sealed = r.seal(&k, &mut Seq(0)).unwrap();
        assert_eq!(Record::open(&sealed, &k.device_id()), Ok(r.clone()));
        assert!(sealed.len() < 200, "{}", sealed.len());
        // Nothing of the addresses is readable in it.
        assert!(!sealed.windows(4).any(|w| w == [192, 168, 68, 57]));
        // A new nonce each time.
        assert_ne!(sealed, r.seal(&k, &mut Seq(50)).unwrap());
    }

    #[test]
    fn another_device_cannot_open_it_and_a_tampered_one_does_not_open() {
        let k = DeviceKey::from_seed(&[4; 32]);
        let other = DeviceKey::from_seed(&[5; 32]);
        let sealed = sample(&k).seal(&k, &mut Seq(0)).unwrap();
        assert_eq!(Record::open(&sealed, &other.device_id()), Err(OpenError::Sealed), "another device's id: another key");
        for i in 0..sealed.len() {
            let mut t = sealed.clone();
            t[i] ^= 0x01;
            assert!(Record::open(&t, &k.device_id()).is_err(), "byte {i} changed");
        }
        assert_eq!(Record::open(&sealed[..sealed.len() - 1], &k.device_id()), Err(OpenError::Sealed));
        assert_eq!(Record::open(&[], &k.device_id()), Err(OpenError::Sealed));
        assert_eq!(Record::open(&[2; 40], &k.device_id()), Err(OpenError::Sealed), "a later version");
    }

    #[test]
    fn a_record_sealed_for_one_device_but_signed_by_another_is_refused() {
        // Someone who knows device A's id (and so its record key) seals a record naming its own key, or A's key
        // with its own signature: neither passes.
        let a = DeviceKey::from_seed(&[4; 32]);
        let mallory = DeviceKey::from_seed(&[6; 32]);
        let id = record_id(&a.device_id());
        let seal_raw = |plain: Vec<u8>| {
            let mut p = plain;
            aead(&a.device_id()).unwrap().seal_in_place_append_tag(Nonce::assume_unique_for_key([1; 12]), Aad::from(&aad(&id)), &mut p).unwrap();
            let mut out = vec![VERSION];
            out.extend_from_slice(&[1; 12]);
            out.extend_from_slice(&p);
            out
        };
        // Mallory's key inside: the key is not A's.
        let mut body = Record { device_pk: *mallory.public_key(), ..sample(&a) }.body();
        let sig = mallory.sign(&signed_bytes(&id, &body));
        body.extend_from_slice(&sig);
        assert_eq!(Record::open(&seal_raw(body), &a.device_id()), Err(OpenError::WrongDevice));
        // A's key inside, Mallory's signature.
        let mut body = sample(&a).body();
        let sig = mallory.sign(&signed_bytes(&id, &body));
        body.extend_from_slice(&sig);
        assert_eq!(Record::open(&seal_raw(body), &a.device_id()), Err(OpenError::BadSignature));
    }

    #[test]
    fn the_id_is_a_hash_of_the_device_id_with_its_own_context() {
        let d = DeviceId([7; 32]);
        assert_eq!(record_id_hex(&d).len(), 64);
        assert_eq!(record_id_hex(&d), record_id_hex(&DeviceId([7; 32])));
        assert_ne!(record_id(&d), record_id(&DeviceId([8; 32])));
        assert_ne!(record_id(&d), d.0, "not the device id itself");
        let mut b = ID_CONTEXT.to_vec();
        b.extend_from_slice(&d.0);
        assert_eq!(record_id(&d).as_slice(), digest(&SHA256, &b).as_ref());
    }

    #[test]
    fn addresses_are_capped_and_a_later_versions_fields_are_skipped() {
        let k = DeviceKey::from_seed(&[4; 32]);
        let many = Record { lan: (1..=12).map(|i| SocketAddr::from(([10, 0, 0, i], 5000))).collect(), public: None, ..sample(&k) };
        let back = Record::open(&many.seal(&k, &mut Seq(0)).unwrap(), &k.device_id()).unwrap();
        assert_eq!(back.lan, many.lan[..MAX_LAN].to_vec());
        let mut body = sample(&k).body();
        put_field(&mut body, 9, "a later field");
        assert_eq!(Record::parse_body(&body), Some(sample(&k)));
    }
}
