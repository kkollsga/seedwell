//! BEP 29 packet header and extension-chain codec (version 1, 20-byte header).
//!
//! ```text
//! 0       4       8               16              24              32
//! +-------+-------+---------------+---------------+---------------+
//! | type  | ver   | extension     | connection_id                 |
//! +-------+-------+---------------+---------------+---------------+
//! | timestamp_microseconds                                        |
//! | timestamp_difference_microseconds                             |
//! | wnd_size                                                      |
//! | seq_nr                        | ack_nr                        |
//! +---------------------------------------------------------------+
//! ```
//! All integers are big-endian. Parsing never panics and never allocates.

/// Fixed header size.
pub const HEADER_LEN: usize = 20;
/// The only protocol version we speak (libutp "v1"); v0 peers are ignored.
pub const VERSION: u8 = 1;
/// Extension id of the selective-ACK bitmask.
pub const EXT_SELECTIVE_ACK: u8 = 1;
/// Largest SACK bitmask we emit (256 packets past `ack_nr + 1`).
pub const MAX_SACK_BYTES: usize = 32;
/// Longest extension chain we walk before declaring the packet malformed.
const MAX_EXT_CHAIN: usize = 8;

/// Packet type (high nibble of byte 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketType {
    /// Payload-carrying packet.
    Data = 0,
    /// Graceful close; consumes a sequence number.
    Fin = 1,
    /// Pure acknowledgement; does not consume a sequence number.
    State = 2,
    /// Abort.
    Reset = 3,
    /// Connection request; consumes sequence number 1 on the initiator.
    Syn = 4,
}

impl PacketType {
    fn from_nibble(n: u8) -> Option<Self> {
        Some(match n {
            0 => PacketType::Data,
            1 => PacketType::Fin,
            2 => PacketType::State,
            3 => PacketType::Reset,
            4 => PacketType::Syn,
            _ => return None,
        })
    }
}

/// Why a datagram is not a valid uTP packet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    /// Shorter than the header, or an extension runs past the end.
    Truncated,
    /// Version nibble is not 1.
    BadVersion,
    /// Unknown packet type.
    BadType,
    /// Extension chain loops or is absurdly long.
    BadExtension,
}

/// Decoded fixed header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Packet type.
    pub ty: PacketType,
    /// Connection id as carried on the wire.
    pub conn_id: u16,
    /// Sender's clock, microseconds, wrapping.
    pub timestamp: u32,
    /// Sender's last measured `their_timestamp - our_receive_time` (one-way delay + offset).
    pub timestamp_diff: u32,
    /// Free bytes in the sender's receive buffer.
    pub wnd_size: u32,
    /// Sequence number.
    pub seq_nr: u16,
    /// Last in-order sequence number the sender received.
    pub ack_nr: u16,
}

/// A parsed packet borrowing the datagram.
#[derive(Clone, Copy, Debug)]
pub struct Packet<'a> {
    /// Fixed header.
    pub header: Header,
    /// Selective-ACK bitmask, if the extension was present (bit 0 of byte 0 = `ack_nr + 2`).
    pub sack: Option<&'a [u8]>,
    /// Payload bytes.
    pub payload: &'a [u8],
}

/// Cheap first-byte demultiplexing test: version 1, known type, header-sized. Bencoded DHT
/// (`d...` = 0x64), UDP-tracker replies (first byte 0x00) and NAT-PMP never match.
pub fn looks_like_utp(d: &[u8]) -> bool {
    match d.first() {
        Some(&b0) => d.len() >= HEADER_LEN && (b0 & 0x0f) == VERSION && (b0 >> 4) <= 4,
        None => false,
    }
}

impl<'a> Packet<'a> {
    /// Parse a datagram, walking the extension chain and skipping unknown extensions.
    pub fn parse(d: &'a [u8]) -> Result<Packet<'a>, ParseError> {
        if d.len() < HEADER_LEN {
            return Err(ParseError::Truncated);
        }
        let b0 = d[0];
        if b0 & 0x0f != VERSION {
            return Err(ParseError::BadVersion);
        }
        let ty = PacketType::from_nibble(b0 >> 4).ok_or(ParseError::BadType)?;
        let be32 = |i: usize| u32::from_be_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]]);
        let be16 = |i: usize| u16::from_be_bytes([d[i], d[i + 1]]);
        let header = Header {
            ty,
            conn_id: be16(2),
            timestamp: be32(4),
            timestamp_diff: be32(8),
            wnd_size: be32(12),
            seq_nr: be16(16),
            ack_nr: be16(18),
        };
        let mut ext = d[1];
        let mut off = HEADER_LEN;
        let mut sack = None;
        let mut hops = 0;
        while ext != 0 {
            hops += 1;
            if hops > MAX_EXT_CHAIN {
                return Err(ParseError::BadExtension);
            }
            let (next, len) = match (d.get(off), d.get(off + 1)) {
                (Some(&n), Some(&l)) => (n, l as usize),
                _ => return Err(ParseError::Truncated),
            };
            let body = d.get(off + 2..off + 2 + len).ok_or(ParseError::Truncated)?;
            if ext == EXT_SELECTIVE_ACK && sack.is_none() {
                sack = Some(body);
            }
            off += 2 + len;
            ext = next;
        }
        Ok(Packet { header, sack, payload: &d[off..] })
    }
}

/// Bytes the header plus a SACK extension of `sack_len` bytes occupy.
pub fn header_len(sack_len: usize) -> usize {
    if sack_len == 0 { HEADER_LEN } else { HEADER_LEN + 2 + sack_len }
}

/// Write `h` (+ optional SACK extension) at the start of `buf`; returns where the payload starts,
/// or `None` if `buf` is too small. `sack` must be 1..=255 bytes (we emit multiples of 4).
pub fn encode(h: &Header, sack: &[u8], buf: &mut [u8]) -> Option<usize> {
    let end = header_len(sack.len());
    if buf.len() < end || sack.len() > 255 {
        return None;
    }
    buf[0] = ((h.ty as u8) << 4) | VERSION;
    buf[1] = if sack.is_empty() { 0 } else { EXT_SELECTIVE_ACK };
    buf[2..4].copy_from_slice(&h.conn_id.to_be_bytes());
    buf[4..8].copy_from_slice(&h.timestamp.to_be_bytes());
    buf[8..12].copy_from_slice(&h.timestamp_diff.to_be_bytes());
    buf[12..16].copy_from_slice(&h.wnd_size.to_be_bytes());
    buf[16..18].copy_from_slice(&h.seq_nr.to_be_bytes());
    buf[18..20].copy_from_slice(&h.ack_nr.to_be_bytes());
    if !sack.is_empty() {
        buf[20] = 0; // end of chain
        buf[21] = sack.len() as u8;
        buf[22..end].copy_from_slice(sack);
    }
    Some(end)
}

/// Iterate the packet offsets (relative to `ack_nr`) acknowledged by a SACK bitmask:
/// bit `i` of the mask acknowledges `ack_nr + 2 + i`, least significant bit first within a byte.
pub fn sack_offsets(mask: &[u8]) -> impl Iterator<Item = u32> + '_ {
    mask.iter().enumerate().flat_map(|(j, &b)| {
        (0..8u32).filter(move |i| b & (1 << i) != 0).map(move |i| j as u32 * 8 + i)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            ty: PacketType::Data,
            conn_id: 0xBEEF,
            timestamp: 0x0102_0304,
            timestamp_diff: 0xAABB_CCDD,
            wnd_size: 1_048_576,
            seq_nr: 0xFFFE,
            ack_nr: 7,
        }
    }

    #[test]
    fn roundtrip_plain() {
        let h = sample();
        let mut buf = [0u8; 64];
        let off = encode(&h, &[], &mut buf).unwrap();
        assert_eq!(off, 20);
        buf[off..off + 3].copy_from_slice(b"abc");
        let p = Packet::parse(&buf[..off + 3]).unwrap();
        assert_eq!(p.header, h);
        assert_eq!(p.sack, None);
        assert_eq!(p.payload, b"abc");
    }

    #[test]
    fn exact_wire_bytes() {
        // Hand-assembled from the BEP 29 layout: ST_SYN (4) | ver 1, no ext, conn id 0x1234.
        let wire: [u8; 20] = [
            0x41, 0x00, 0x12, 0x34, 0, 0, 0, 9, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 1, 0, 0,
        ];
        let p = Packet::parse(&wire).unwrap();
        assert_eq!(p.header.ty, PacketType::Syn);
        assert_eq!(p.header.conn_id, 0x1234);
        assert_eq!(p.header.timestamp, 9);
        assert_eq!(p.header.wnd_size, 0x0010_0000);
        assert_eq!(p.header.seq_nr, 1);
        let mut out = [0u8; 20];
        assert_eq!(encode(&p.header, &[], &mut out), Some(20));
        assert_eq!(out, wire);
    }

    #[test]
    fn roundtrip_sack() {
        let h = Header { ty: PacketType::State, ..sample() };
        let mask = [0b0000_0101, 0, 0x80, 0];
        let mut buf = [0u8; 64];
        let off = encode(&h, &mask, &mut buf).unwrap();
        assert_eq!(off, 26);
        let p = Packet::parse(&buf[..off]).unwrap();
        assert_eq!(p.sack, Some(&mask[..]));
        assert!(p.payload.is_empty());
        let offs: Vec<u32> = sack_offsets(&mask).collect();
        assert_eq!(offs, vec![0, 2, 23]);
    }

    #[test]
    fn unknown_extensions_are_skipped_and_chain_walked() {
        let mut d = vec![0u8; 20];
        d[0] = 0x21; // STATE v1
        d[1] = 7; // unknown ext first
        d.extend_from_slice(&[EXT_SELECTIVE_ACK, 2, 0xAA, 0xBB]); // ext 7: next=sack, len 2
        d.extend_from_slice(&[0, 4, 1, 2, 3, 4]); // sack, end of chain
        d.extend_from_slice(b"xy");
        let p = Packet::parse(&d).unwrap();
        assert_eq!(p.sack, Some(&[1u8, 2, 3, 4][..]));
        assert_eq!(p.payload, b"xy");
    }

    #[test]
    fn malformed_is_error_not_panic() {
        assert_eq!(Packet::parse(&[]).unwrap_err(), ParseError::Truncated);
        assert_eq!(Packet::parse(&[0x41; 19]).unwrap_err(), ParseError::Truncated);
        let mut d = [0u8; 20];
        d[0] = 0x40; // version 0
        assert_eq!(Packet::parse(&d).unwrap_err(), ParseError::BadVersion);
        d[0] = 0x51; // type 5
        assert_eq!(Packet::parse(&d).unwrap_err(), ParseError::BadType);
        d[0] = 0x21;
        d[1] = 1; // sack announced but missing
        assert_eq!(Packet::parse(&d).unwrap_err(), ParseError::Truncated);
        let mut e = d.to_vec();
        e.extend_from_slice(&[0, 200, 1]); // length runs past the end
        assert_eq!(Packet::parse(&e).unwrap_err(), ParseError::Truncated);
        // chain that never terminates
        let mut l = d.to_vec();
        for _ in 0..20 {
            l.extend_from_slice(&[1, 0]);
        }
        assert_eq!(Packet::parse(&l).unwrap_err(), ParseError::BadExtension);
    }

    #[test]
    fn demux_first_byte() {
        let mut p = [0u8; 20];
        for ty in 0..=4u8 {
            p[0] = (ty << 4) | 1;
            assert!(looks_like_utp(&p));
        }
        p[0] = b'd'; // bencoded DHT query
        assert!(!looks_like_utp(&p));
        p[0] = 0; // UDP tracker connect response
        assert!(!looks_like_utp(&p));
        p[0] = 0x51;
        assert!(!looks_like_utp(&p));
        assert!(!looks_like_utp(&[0x41; 5]));
        assert!(!looks_like_utp(&[]));
    }

    #[test]
    fn encode_into_small_buffer_is_none() {
        let mut b = [0u8; 10];
        assert_eq!(encode(&sample(), &[], &mut b), None);
        let mut b = [0u8; 24];
        assert_eq!(encode(&sample(), &[1, 2, 3, 4], &mut b), None);
    }

    #[test]
    fn exhaustive_garbage_never_panics() {
        let mut rng = crate::rng::Rng::new(42);
        let mut buf = [0u8; 80];
        for _ in 0..200_000 {
            let n = (rng.next_u64() % 81) as usize;
            for b in buf.iter_mut() {
                *b = rng.next_u64() as u8;
            }
            buf[0] = (buf[0] & 0xf0) | if !rng.next_u64().is_multiple_of(4) { 1 } else { buf[0] & 0x0f };
            if rng.next_u64().is_multiple_of(2) {
                buf[1] = (rng.next_u64() % 4) as u8;
            }
            let _ = Packet::parse(&buf[..n]);
        }
    }
}
