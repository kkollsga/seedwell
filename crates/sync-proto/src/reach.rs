//! Where the device that made a link can be reached, sealed inside the link (SPEC §7G.5, 0.1.13).
//!
//! A joining device that has only the folder's key finds an owner or admin through LAN discovery and the DHT. When
//! both fail (another subnet, a router that drops multicast, a DHT that is slow to answer) the join waits with
//! nothing to show for it. So a link also says where the device that made it was when it made it: its addresses on
//! its local networks and its public address, and its device id and name. That block is sealed with a key derived
//! from the link's own token seed, so only someone holding the whole link can read it, and it travels in a query
//! field (`&a=`) that the versions before it ignore.
//!
//! ```text
//! field   = a=<base32(sealed)>
//! sealed  = version u8 (1) ‖ nonce(12) ‖ ChaCha20-Poly1305(key, nonce, aad = invite id, plain)
//! prk     = HKDF-SHA256-Extract(salt = "seedwell-sync/v2/addresses", ikm = token seed(32))
//! key     = HKDF-Expand(prk, "key" ‖ invite id, 32)
//! nonce   = HMAC-SHA256(HKDF-Expand(prk, "nonce" ‖ invite id, 32), plain)[..12]
//! plain   = device id(32) ‖ name (varint length ‖ UTF-8, ≤ 40 bytes) ‖ count u8 ‖ count × address ‖ field*
//! address = kind u8 ‖ ip (4 or 16) ‖ port u16 (big-endian)
//!           kind: 4 = IPv4 on a local network, 6 = IPv6 on a local network, 0x84 / 0x86 = public IPv4 / IPv6
//! field   = tag u8 ‖ varint length ‖ bytes (0.1.13)
//!           tag 1: the issuer's rendezvous URL (UTF-8, ≤ 200 bytes; SPEC §7G.3), where it publishes its addresses
//! ```
//!
//! The nonce is derived from the plaintext (a synthetic IV), so formatting the same link twice gives the same text,
//! and two different blocks under one token never share a nonce. A reader takes at most [`MAX_LAN`] local
//! addresses and one public one, stops at an address kind it does not know, and ignores bytes after the list (a
//! later version may add fields there: 0.1.12 and earlier read none of them). This version reads the fields after
//! a list it read whole, keeps the ones it knows and skips the others by their length. A block it cannot open (damaged, another link's, a later version) is left
//! out: the link still joins, through discovery.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use ring::aead::{Aad, CHACHA20_POLY1305, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hkdf::{HKDF_SHA256, KeyType, Salt};
use ring::hmac;

use crate::codec::{Reader, put_str};
use crate::ids::DeviceId;

const CONTEXT: &str = "seedwell-sync/v2/addresses";
/// The sealed block's version.
const VERSION: u8 = 1;
/// Addresses on the issuer's local networks a link carries, at most.
pub const MAX_LAN: usize = 4;
/// The issuer's name in a link, at most this many bytes (cut at a character boundary).
pub const MAX_NAME: usize = 40;
const PUBLIC: u8 = 0x80;
/// The longest rendezvous URL a link carries.
pub const MAX_URL: usize = 200;
/// The field tag of the rendezvous URL.
const FIELD_RENDEZVOUS: u8 = 1;

/// Where the device that made a link can be reached, and who it is.
#[derive(Clone, PartialEq, Eq)]
pub struct LinkReach {
    /// The device that made the link (a dial to one of its addresses must reach this device).
    pub device: DeviceId,
    /// Its name, for the joining device to say whom it is looking for (may be empty).
    pub name: String,
    /// Its addresses on its local networks, with the Folder Sync port (at most [`MAX_LAN`]).
    pub lan: Vec<SocketAddr>,
    /// Its public address and port, as its router maps them, when it knows them.
    pub public: Option<SocketAddr>,
    /// Where it publishes its addresses (its user's rendezvous, an `https` URL), when it does: a joining device looks
    /// it up there when the addresses above do not answer. A URL longer than [`MAX_URL`] is left out.
    pub rendezvous: Option<String>,
}

impl std::fmt::Debug for LinkReach {
    // The addresses stay out of logs: a debug print says how many there are.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LinkReach({:?}, {} local, public {}, rendezvous {})", self.device, self.lan.len(), self.public.is_some(), self.rendezvous.is_some())
    }
}

struct Len32;

impl KeyType for Len32 {
    fn len(&self) -> usize {
        32
    }
}

fn keys(token_seed: &[u8; 32], invite_id: &[u8; 16]) -> Option<(LessSafeKey, hmac::Key)> {
    let prk = Salt::new(HKDF_SHA256, CONTEXT.as_bytes()).extract(token_seed);
    let mut k = [0u8; 32];
    prk.expand(&[b"key".as_slice(), invite_id], Len32).ok()?.fill(&mut k).ok()?;
    let mut n = [0u8; 32];
    prk.expand(&[b"nonce".as_slice(), invite_id], Len32).ok()?.fill(&mut n).ok()?;
    Some((LessSafeKey::new(UnboundKey::new(&CHACHA20_POLY1305, &k).ok()?), hmac::Key::new(hmac::HMAC_SHA256, &n)))
}

/// Appends one address in the link block's form (`kind ‖ ip ‖ port`); the rendezvous record uses the same.
pub fn put_addr(out: &mut Vec<u8>, a: &SocketAddr, public: bool) {
    let flag = if public { PUBLIC } else { 0 };
    match a.ip() {
        IpAddr::V4(ip) => {
            out.push(4 | flag);
            out.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            out.push(6 | flag);
            out.extend_from_slice(&ip.octets());
        }
    }
    out.extend_from_slice(&a.port().to_be_bytes());
}

/// `name` cut to at most [`MAX_NAME`] bytes, at a character boundary.
fn short_name(name: &str) -> &str {
    let mut end = name.len().min(MAX_NAME);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// Reads one address in the link block's form: `None` when the input ends or is damaged, `Some(None)` for a kind of a
/// later version (whose length is unknown: a list ends there), else the address and whether it is a public one.
pub fn read_addr(r: &mut Reader<'_>) -> Option<Option<(SocketAddr, bool)>> {
    let kind = r.u8("address kind").ok()?;
    let ip = match kind & !PUBLIC {
        4 => IpAddr::V4(Ipv4Addr::from(r.array::<4>("IPv4").ok()?)),
        6 => IpAddr::V6(Ipv6Addr::from(r.array::<16>("IPv6").ok()?)),
        _ => return Some(None),
    };
    let port = u16::from_be_bytes(r.array("port").ok()?);
    Some(Some((SocketAddr::new(ip, port), kind & PUBLIC != 0)))
}

impl LinkReach {
    /// Every address, the local ones first.
    pub fn addresses(&self) -> impl Iterator<Item = SocketAddr> + '_ {
        self.lan.iter().copied().chain(self.public)
    }

    /// The plaintext; addresses past the caps are left out.
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(64);
        out.extend_from_slice(&self.device.0);
        put_str(&mut out, short_name(&self.name));
        let lan: Vec<&SocketAddr> = self.lan.iter().filter(|a| a.port() != 0).take(MAX_LAN).collect();
        let public = self.public.filter(|a| a.port() != 0);
        out.push((lan.len() + usize::from(public.is_some())) as u8);
        for a in lan {
            put_addr(&mut out, a, false);
        }
        if let Some(a) = public {
            put_addr(&mut out, &a, true);
        }
        if let Some(url) = self.rendezvous.as_deref().filter(|u| !u.is_empty() && u.len() <= MAX_URL) {
            out.push(FIELD_RENDEZVOUS);
            put_str(&mut out, url);
        }
        out
    }

    fn decode(b: &[u8]) -> Option<LinkReach> {
        let r = &mut Reader::new(b);
        let device = DeviceId(r.array("device id").ok()?);
        let name = r.str(MAX_NAME, "name").ok()?;
        let n = r.u8("addresses").ok()?;
        let mut out = LinkReach { device, name, lan: Vec::new(), public: None, rendezvous: None };
        for _ in 0..n {
            let Some((a, public)) = read_addr(r)? else {
                // A kind of a later version: its length is unknown, so the list ends here, and with it what can be
                // read.
                return Some(out);
            };
            if a.port() == 0 {
                continue;
            }
            if public {
                out.public = out.public.or(Some(a));
            } else if out.lan.len() < MAX_LAN {
                out.lan.push(a);
            }
        }
        // The fields after the list: the known ones are read, the others skipped by their length; a damaged one ends
        // them.
        while r.remaining() > 0 {
            let Ok(tag) = r.u8("field") else { break };
            let Ok(len) = r.varint("field length") else { break };
            let Ok(bytes) = r.take(usize::try_from(len).unwrap_or(usize::MAX), "field") else { break };
            if tag == FIELD_RENDEZVOUS && bytes.len() <= MAX_URL {
                out.rendezvous = std::str::from_utf8(bytes).ok().filter(|u| !u.is_empty()).map(str::to_owned);
            }
        }
        Some(out)
    }

    /// The sealed block for the link with this token seed and invitation id.
    pub fn seal(&self, token_seed: &[u8; 32], invite_id: &[u8; 16]) -> Vec<u8> {
        let plain = self.encode();
        let Some((key, nk)) = keys(token_seed, invite_id) else { return Vec::new() };
        let tag = hmac::sign(&nk, &plain);
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&tag.as_ref()[..NONCE_LEN]);
        let mut body = plain;
        if key.seal_in_place_append_tag(Nonce::assume_unique_for_key(nonce), Aad::from(invite_id), &mut body).is_err() {
            return Vec::new();
        }
        let mut out = Vec::with_capacity(1 + NONCE_LEN + body.len());
        out.push(VERSION);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&body);
        out
    }

    /// Opens a sealed block; `None` when it is damaged, made for another link, or of a later version.
    pub fn open(sealed: &[u8], token_seed: &[u8; 32], invite_id: &[u8; 16]) -> Option<LinkReach> {
        let (&version, rest) = sealed.split_first()?;
        if version != VERSION || rest.len() < NONCE_LEN {
            return None;
        }
        let (nonce, body) = rest.split_at(NONCE_LEN);
        let (key, _) = keys(token_seed, invite_id)?;
        let mut body = body.to_vec();
        let plain = key.open_in_place(Nonce::try_assume_unique_for_key(nonce).ok()?, Aad::from(invite_id), &mut body).ok()?;
        LinkReach::decode(plain)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> LinkReach {
        LinkReach {
            device: DeviceId([7; 32]),
            name: "Kristian's Mac mini".into(),
            lan: vec!["192.168.68.57:55799".parse().unwrap(), "[fd00::1234]:55799".parse().unwrap()],
            public: Some("84.212.3.4:41000".parse().unwrap()),
            rendezvous: None,
        }
    }

    #[test]
    fn a_sealed_block_round_trips_and_is_the_same_each_time() {
        let r = sample();
        let s = r.seal(&[1; 32], &[2; 16]);
        assert_eq!(LinkReach::open(&s, &[1; 32], &[2; 16]), Some(r.clone()));
        assert_eq!(s, r.seal(&[1; 32], &[2; 16]), "deterministic: the same link formats the same");
        let other = LinkReach { lan: vec![], ..r.clone() };
        assert_ne!(s[1..13], other.seal(&[1; 32], &[2; 16])[1..13], "another block, another nonce");
        assert!(s.len() < 1 + 12 + 32 + 1 + 40 + 1 + 3 * 19 + 16);
    }

    #[test]
    fn a_tampered_block_or_the_wrong_link_opens_to_nothing() {
        let s = sample().seal(&[1; 32], &[2; 16]);
        assert_eq!(LinkReach::open(&s, &[9; 32], &[2; 16]), None, "another link's token");
        assert_eq!(LinkReach::open(&s, &[1; 32], &[3; 16]), None, "another invitation");
        for i in 0..s.len() {
            let mut t = s.clone();
            t[i] ^= 0x40;
            assert_eq!(LinkReach::open(&t, &[1; 32], &[2; 16]), None, "byte {i} changed");
        }
        assert_eq!(LinkReach::open(&s[..s.len() - 1], &[1; 32], &[2; 16]), None);
        assert_eq!(LinkReach::open(&[], &[1; 32], &[2; 16]), None);
        assert_eq!(LinkReach::open(&[1, 2, 3], &[1; 32], &[2; 16]), None);
    }

    #[test]
    fn addresses_are_capped_and_the_name_is_cut_at_a_character() {
        let many = LinkReach {
            device: DeviceId([3; 32]),
            name: "æ".repeat(30),
            lan: (1..=9).map(|i| SocketAddr::from(([10, 0, 0, i], 5000))).collect(),
            public: Some(SocketAddr::from(([1, 2, 3, 4], 0))),
            rendezvous: Some("x".repeat(MAX_URL + 1)),
        };
        let back = LinkReach::open(&many.seal(&[1; 32], &[2; 16]), &[1; 32], &[2; 16]).unwrap();
        assert_eq!(back.lan, many.lan[..MAX_LAN].to_vec());
        assert_eq!(back.public, None, "no port, no address");
        assert_eq!(back.name, "æ".repeat(MAX_NAME / 2));
        assert_eq!(back.addresses().count(), MAX_LAN);
        assert_eq!(back.rendezvous, None, "too long a URL is left out");
    }

    /// 0.1.12's `decode`, as released: what an older Seedwell reads of a block.
    fn decode_0_1_12(b: &[u8]) -> Option<LinkReach> {
        let r = &mut Reader::new(b);
        let device = DeviceId(r.array("device id").ok()?);
        let name = r.str(MAX_NAME, "name").ok()?;
        let n = r.u8("addresses").ok()?;
        let mut out = LinkReach { device, name, lan: Vec::new(), public: None, rendezvous: None };
        for _ in 0..n {
            let kind = r.u8("address kind").ok()?;
            let ip = match kind & !PUBLIC {
                4 => IpAddr::V4(Ipv4Addr::from(r.array::<4>("IPv4").ok()?)),
                6 => IpAddr::V6(Ipv6Addr::from(r.array::<16>("IPv6").ok()?)),
                _ => break,
            };
            let port = u16::from_be_bytes(r.array("port").ok()?);
            let a = SocketAddr::new(ip, port);
            if port == 0 {
                continue;
            }
            if kind & PUBLIC != 0 {
                out.public = out.public.or(Some(a));
            } else if out.lan.len() < MAX_LAN {
                out.lan.push(a);
            }
        }
        Some(out)
    }

    #[test]
    fn the_rendezvous_url_round_trips_and_an_older_reader_ignores_it() {
        let with = LinkReach { rendezvous: Some("https://seedwell-rendezvous.example.workers.dev".into()), ..sample() };
        let back = LinkReach::open(&with.seal(&[1; 32], &[2; 16]), &[1; 32], &[2; 16]).unwrap();
        assert_eq!(back, with);
        // The block without the URL is a prefix of the one with it: an older reader stops before the field.
        let (plain, bare) = (with.encode(), sample().encode());
        assert!(plain.starts_with(&bare) && plain.len() > bare.len());
        assert_eq!(decode_0_1_12(&plain), Some(sample()), "0.1.12 reads the same addresses and no URL");
        // An unknown field before it is skipped by its length; a damaged one ends the fields.
        let mut more = bare.clone();
        more.extend_from_slice(&[9, 3, 1, 2, 3]);
        more.extend_from_slice(&plain[bare.len()..]);
        assert_eq!(LinkReach::decode(&more).unwrap().rendezvous, with.rendezvous);
        let mut cut = plain.clone();
        cut.truncate(plain.len() - 3);
        assert_eq!(LinkReach::decode(&cut).unwrap(), sample(), "a cut field is left out");
    }

    #[test]
    fn a_later_versions_address_kinds_and_fields_are_skipped() {
        let mut plain = sample().encode();
        // One more address of an unknown kind (it ends the list), and a field after it.
        plain[32 + 1 + sample().name.len()] += 1;
        plain.extend_from_slice(&[0x07, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        let back = LinkReach::decode(&plain).unwrap();
        assert_eq!(back, sample());
        // Too many local addresses from another writer: the first ones count.
        let mut many = sample();
        many.lan = (1..=6).map(|i| SocketAddr::from(([10, 0, 0, i], 5000))).collect();
        let mut plain = Vec::new();
        plain.extend_from_slice(&many.device.0);
        put_str(&mut plain, "x");
        plain.push(6);
        for a in &many.lan {
            put_addr(&mut plain, a, false);
        }
        assert_eq!(LinkReach::decode(&plain).unwrap().lan, many.lan[..MAX_LAN].to_vec());
    }
}
