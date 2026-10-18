//! BEP 3 handshake with reserved-bit feature flags.

use crate::error::{Error, Result};
use bytes::BufMut;

/// Length of a complete handshake on the wire.
pub const HANDSHAKE_LEN: usize = 68;
/// `\x13BitTorrent protocol`: the length byte plus the protocol string.
pub const PREFIX: &[u8; 20] = b"\x13BitTorrent protocol";

/// The eight reserved handshake bytes (feature flags).
// size: 8 bytes
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Reserved(pub [u8; 8]);

impl Reserved {
    /// BEP 10 extension protocol: `reserved[5] & 0x10`.
    pub const EXTENSION: (usize, u8) = (5, 0x10);
    /// BEP 6 Fast extension: `reserved[7] & 0x04`.
    pub const FAST: (usize, u8) = (7, 0x04);
    /// BEP 5 DHT port message: `reserved[7] & 0x01`.
    pub const DHT: (usize, u8) = (7, 0x01);

    /// No features.
    pub const fn none() -> Self {
        Self([0; 8])
    }

    /// What Seedwell advertises: extension protocol + Fast, and DHT unless `private` (BEP 27).
    pub const fn ours(private: bool) -> Self {
        Self::ours_with(private, true)
    }

    /// Like [`ours`](Self::ours), with the DHT bit only if a DHT node is actually running
    /// (`dht`): a peer that sees the bit sends `port` messages, which would go nowhere. A private
    /// torrent never advertises it, whatever `dht` says.
    pub const fn ours_with(private: bool, dht: bool) -> Self {
        let mut b = [0u8; 8];
        b[5] |= 0x10;
        b[7] |= 0x04;
        if dht && !private {
            b[7] |= 0x01;
        }
        Self(b)
    }

    #[inline]
    fn bit(&self, (i, m): (usize, u8)) -> bool {
        self.0.get(i).is_some_and(|b| b & m != 0)
    }

    /// BEP 10 supported.
    pub fn extension_protocol(&self) -> bool {
        self.bit(Self::EXTENSION)
    }

    /// BEP 6 supported.
    pub fn fast(&self) -> bool {
        self.bit(Self::FAST)
    }

    /// BEP 5 (`port` message) supported.
    pub fn dht(&self) -> bool {
        self.bit(Self::DHT)
    }

    /// Both sides support the extension protocol.
    pub fn both_extension(&self, other: &Reserved) -> bool {
        self.extension_protocol() && other.extension_protocol()
    }

    /// Both sides support Fast.
    pub fn both_fast(&self, other: &Reserved) -> bool {
        self.fast() && other.fast()
    }
}

/// A parsed or to-be-sent handshake.
// size: 48 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handshake {
    /// Feature flags.
    pub reserved: Reserved,
    /// 20-byte v1 info hash.
    pub info_hash: [u8; 20],
    /// 20-byte peer id.
    pub peer_id: [u8; 20],
}

impl Handshake {
    /// Build with Seedwell' standard feature flags.
    pub fn new(info_hash: [u8; 20], peer_id: [u8; 20], private: bool) -> Self {
        Self {
            reserved: Reserved::ours(private),
            info_hash,
            peer_id,
        }
    }

    /// Serialize to a fixed array.
    pub fn to_bytes(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        out[..20].copy_from_slice(PREFIX);
        out[20..28].copy_from_slice(&self.reserved.0);
        out[28..48].copy_from_slice(&self.info_hash);
        out[48..68].copy_from_slice(&self.peer_id);
        out
    }

    /// Append to a buffer.
    pub fn encode<B: BufMut>(&self, out: &mut B) {
        out.put_slice(&self.to_bytes());
    }

    /// Incrementally parse from the front of `buf`.
    ///
    /// - `Ok(None)`: need more bytes (the prefix seen so far is valid).
    /// - `Ok(Some(h))`: exactly [`HANDSHAKE_LEN`] bytes were consumed.
    /// - `Err`: the prefix is wrong; reported as soon as the first wrong byte
    ///   arrives so garbage (HTTP, TLS) is dropped early.
    pub fn decode(buf: &[u8]) -> Result<Option<Handshake>> {
        let n = buf.len().min(PREFIX.len());
        if buf.get(..n) != PREFIX.get(..n) {
            return Err(Error::BadProtocolString);
        }
        let Some(b) = buf.get(..HANDSHAKE_LEN) else {
            return Ok(None);
        };
        let mut h = Handshake {
            reserved: Reserved::none(),
            info_hash: [0; 20],
            peer_id: [0; 20],
        };
        h.reserved.0.copy_from_slice(&b[20..28]);
        h.info_hash.copy_from_slice(&b[28..48]);
        h.peer_id.copy_from_slice(&b[48..68]);
        Ok(Some(h))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Handshake {
        Handshake {
            reserved: Reserved([0, 0, 0, 0, 0, 0x10, 0, 0x05]),
            info_hash: [0xAA; 20],
            peer_id: *b"-SX0001-abcdefghijkl",
        }
    }

    #[test]
    fn byte_exact() {
        let b = sample().to_bytes();
        let mut exp = Vec::new();
        exp.extend_from_slice(b"\x13BitTorrent protocol");
        exp.extend_from_slice(&[0, 0, 0, 0, 0, 0x10, 0, 0x05]);
        exp.extend_from_slice(&[0xAA; 20]);
        exp.extend_from_slice(b"-SX0001-abcdefghijkl");
        assert_eq!(b.as_slice(), exp.as_slice());
        assert_eq!(b.len(), 68);
    }

    #[test]
    fn roundtrip_and_flags() {
        let h = sample();
        let d = Handshake::decode(&h.to_bytes()).unwrap().unwrap();
        assert_eq!(d, h);
        assert!(d.reserved.extension_protocol());
        assert!(d.reserved.fast());
        assert!(d.reserved.dht());
        let r = Reserved::ours(true);
        assert!(r.extension_protocol() && r.fast() && !r.dht());
        assert!(!Reserved::none().fast());
        assert!(Reserved::ours(false).both_fast(&r));
        assert!(Reserved::ours(false).both_extension(&r));
        assert!(!Reserved::none().both_extension(&r));
    }

    #[test]
    fn trailing_bytes_not_consumed() {
        let mut v = sample().to_bytes().to_vec();
        v.extend_from_slice(&[0, 0, 0, 0]);
        assert!(Handshake::decode(&v).unwrap().is_some());
    }

    #[test]
    fn partial_at_every_split() {
        let b = sample().to_bytes();
        for i in 0..HANDSHAKE_LEN {
            assert_eq!(Handshake::decode(&b[..i]), Ok(None), "split {i}");
        }
        assert!(Handshake::decode(&b).unwrap().is_some());
    }

    #[test]
    fn bad_prefix_rejected_early() {
        assert_eq!(Handshake::decode(b"G"), Err(Error::BadProtocolString));
        assert_eq!(
            Handshake::decode(b"\x13BitTorrenT"),
            Err(Error::BadProtocolString)
        );
        assert_eq!(Handshake::decode(b"\x00"), Err(Error::BadProtocolString));
        assert_eq!(
            Handshake::decode(b"\x12BitTorrent protocol"),
            Err(Error::BadProtocolString)
        );
        let mut b = sample().to_bytes();
        b[19] = b'X';
        assert_eq!(Handshake::decode(&b), Err(Error::BadProtocolString));
        assert_eq!(Handshake::decode(b""), Ok(None));
    }

    #[test]
    fn unknown_reserved_bits_are_ignored() {
        let mut h = sample();
        h.reserved = Reserved([0xFF; 8]);
        let d = Handshake::decode(&h.to_bytes()).unwrap().unwrap();
        assert_eq!(d.reserved, h.reserved);
    }
}
