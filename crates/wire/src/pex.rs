//! BEP 11 `ut_pex`: peer exchange. **Never used for private torrents**: build the
//! handshake with [`crate::ExtPolicy::for_torrent`] and PEX is neither advertised
//! nor parsed.

use crate::bencode::{self, put_kv_bytes};
use crate::error::Result;
use bytes::BufMut;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Most peers per list we put into one outgoing message (BEP 11 guidance).
pub const MAX_PEX_SEND: usize = 50;
/// Most peers per list we read from one incoming message; extra entries are ignored.
pub const MAX_PEX_RECV: usize = 200;
/// Minimum seconds between PEX messages to one peer (BEP 11: no more than once a minute).
pub const MIN_PEX_INTERVAL_SECS: u64 = 60;

/// Per-peer flags (`added.f`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PexFlags(pub u8);

impl PexFlags {
    /// Prefers encrypted connections.
    pub const PREFERS_ENCRYPTION: u8 = 0x01;
    /// Seed / upload only.
    pub const SEED: u8 = 0x02;
    /// Supports uTP.
    pub const UTP: u8 = 0x04;
    /// Supports ut_holepunch.
    pub const HOLEPUNCH: u8 = 0x08;
    /// We connected to it (outgoing), so it is known to be reachable.
    pub const REACHABLE: u8 = 0x10;

    /// Test a flag bit.
    pub fn has(&self, bit: u8) -> bool {
        self.0 & bit != 0
    }
}

/// One peer from a PEX message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PexPeer {
    /// Address and port.
    pub addr: SocketAddr,
    /// Flags (`0` for `dropped`).
    pub flags: PexFlags,
}

/// A parsed `ut_pex` message borrowing the compact lists.
// size: 96 bytes
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PexMessage<'a> {
    added: &'a [u8],
    added_f: &'a [u8],
    added6: &'a [u8],
    added6_f: &'a [u8],
    dropped: &'a [u8],
    dropped6: &'a [u8],
}

impl<'a> PexMessage<'a> {
    /// Parse a payload. Compact lists whose length is not a multiple of 6 (v4) or
    /// 18 (v6) are rejected as malformed.
    pub fn parse(payload: &'a [u8]) -> Result<Self> {
        let (d, _) = bencode::dict_prefix(payload)?;
        let list = |k: &[u8], stride: usize| -> Result<&'a [u8]> {
            match d.get(k).and_then(|v| v.as_bytes()) {
                None => Ok(&[]),
                Some(b) if b.len() % stride == 0 => Ok(b),
                Some(_) => Err(crate::Error::InvalidField("pex list length")),
            }
        };
        Ok(Self {
            added: list(b"added", 6)?,
            added_f: d.get(b"added.f").and_then(|v| v.as_bytes()).unwrap_or(&[]),
            added6: list(b"added6", 18)?,
            added6_f: d.get(b"added6.f").and_then(|v| v.as_bytes()).unwrap_or(&[]),
            dropped: list(b"dropped", 6)?,
            dropped6: list(b"dropped6", 18)?,
        })
    }

    /// Peers to add (v4 then v6), capped at [`MAX_PEX_RECV`] per family; port 0 skipped.
    pub fn added(&self) -> impl Iterator<Item = PexPeer> + 'a {
        let v4 = iter_v4(self.added, self.added_f);
        let v6 = iter_v6(self.added6, self.added6_f);
        v4.chain(v6)
    }

    /// Peers that went away (v4 then v6), same caps.
    pub fn dropped(&self) -> impl Iterator<Item = PexPeer> + 'a {
        iter_v4(self.dropped, &[]).chain(iter_v6(self.dropped6, &[]))
    }
}

fn iter_v4<'a>(list: &'a [u8], flags: &'a [u8]) -> impl Iterator<Item = PexPeer> + 'a {
    list.chunks_exact(6)
        .take(MAX_PEX_RECV)
        .enumerate()
        .filter_map(move |(i, c)| {
            let port = u16::from_be_bytes([c[4], c[5]]);
            (port != 0).then(|| PexPeer {
                addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(c[0], c[1], c[2], c[3])), port),
                flags: PexFlags(flags.get(i).copied().unwrap_or(0)),
            })
        })
}

fn iter_v6<'a>(list: &'a [u8], flags: &'a [u8]) -> impl Iterator<Item = PexPeer> + 'a {
    list.chunks_exact(18)
        .take(MAX_PEX_RECV)
        .enumerate()
        .filter_map(move |(i, c)| {
            let port = u16::from_be_bytes([c[16], c[17]]);
            let mut ip = [0u8; 16];
            ip.copy_from_slice(&c[..16]);
            (port != 0).then(|| PexPeer {
                addr: SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port),
                flags: PexFlags(flags.get(i).copied().unwrap_or(0)),
            })
        })
}

/// Builds an outgoing PEX message with the per-list caps applied.
#[derive(Debug, Default)]
pub struct PexBuilder {
    added: Vec<(SocketAddr, PexFlags)>,
    dropped: Vec<SocketAddr>,
}

impl PexBuilder {
    /// Empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a peer. Returns `false` (and ignores it) once its family has
    /// [`MAX_PEX_SEND`] entries.
    pub fn add(&mut self, addr: SocketAddr, flags: PexFlags) -> bool {
        if self
            .added
            .iter()
            .filter(|(a, _)| a.is_ipv4() == addr.is_ipv4())
            .count()
            >= MAX_PEX_SEND
        {
            return false;
        }
        self.added.push((addr, flags));
        true
    }

    /// Report a dropped peer; capped like [`PexBuilder::add`].
    pub fn drop_peer(&mut self, addr: SocketAddr) -> bool {
        if self
            .dropped
            .iter()
            .filter(|a| a.is_ipv4() == addr.is_ipv4())
            .count()
            >= MAX_PEX_SEND
        {
            return false;
        }
        self.dropped.push(addr);
        true
    }

    /// Nothing to send.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.dropped.is_empty()
    }

    /// Write the bencoded payload (sorted keys; empty lists omitted).
    pub fn encode_payload<B: BufMut>(&self, out: &mut B) {
        let (mut a4, mut f4, mut a6, mut f6) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for (addr, fl) in &self.added {
            match addr.ip() {
                IpAddr::V4(ip) => {
                    a4.extend_from_slice(&ip.octets());
                    a4.extend_from_slice(&addr.port().to_be_bytes());
                    f4.push(fl.0);
                }
                IpAddr::V6(ip) => {
                    a6.extend_from_slice(&ip.octets());
                    a6.extend_from_slice(&addr.port().to_be_bytes());
                    f6.push(fl.0);
                }
            }
        }
        let (mut d4, mut d6) = (Vec::new(), Vec::new());
        for addr in &self.dropped {
            match addr.ip() {
                IpAddr::V4(ip) => {
                    d4.extend_from_slice(&ip.octets());
                    d4.extend_from_slice(&addr.port().to_be_bytes());
                }
                IpAddr::V6(ip) => {
                    d6.extend_from_slice(&ip.octets());
                    d6.extend_from_slice(&addr.port().to_be_bytes());
                }
            }
        }
        out.put_u8(b'd');
        // Keys in byte order: added < added.f < added6 < added6.f < dropped < dropped6.
        for (k, v) in [
            ("added", &a4),
            ("added.f", &f4),
            ("added6", &a6),
            ("added6.f", &f6),
            ("dropped", &d4),
            ("dropped6", &d6),
        ] {
            if !v.is_empty() {
                put_kv_bytes(out, k, v);
            }
        }
        out.put_u8(b'e');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(a: u8, port: u16) -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, a)), port)
    }
    fn v6(a: u16, port: u16) -> SocketAddr {
        SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, a)),
            port,
        )
    }

    #[test]
    fn byte_exact_v4() {
        let mut b = PexBuilder::new();
        b.add(v4(1, 6881), PexFlags(PexFlags::UTP));
        b.drop_peer(v4(2, 80));
        let mut out = Vec::new();
        b.encode_payload(&mut out);
        let exp = b"d5:added6:\x0a\x00\x00\x01\x1a\xe17:added.f1:\x047:dropped6:\x0a\x00\x00\x02\x00\x50e";
        assert_eq!(out, exp);
    }

    #[test]
    fn roundtrip_v4_and_v6() {
        let mut b = PexBuilder::new();
        b.add(v4(1, 6881), PexFlags(PexFlags::SEED | PexFlags::UTP));
        b.add(v6(7, 51413), PexFlags(PexFlags::REACHABLE));
        b.add(v4(3, 1), PexFlags(0));
        b.drop_peer(v6(9, 9));
        let mut out = Vec::new();
        b.encode_payload(&mut out);
        let m = PexMessage::parse(&out).unwrap();
        let added: Vec<_> = m.added().collect();
        assert_eq!(added.len(), 3);
        assert_eq!(added[0].addr, v4(1, 6881));
        assert!(added[0].flags.has(PexFlags::SEED) && added[0].flags.has(PexFlags::UTP));
        assert_eq!(added[1].addr, v4(3, 1));
        assert_eq!(added[2].addr, v6(7, 51413));
        assert!(added[2].flags.has(PexFlags::REACHABLE));
        let dropped: Vec<_> = m.dropped().map(|p| p.addr).collect();
        assert_eq!(dropped, vec![v6(9, 9)]);
    }

    #[test]
    fn send_caps() {
        let mut b = PexBuilder::new();
        for i in 0..60u16 {
            let ok = b.add(v4((i % 250) as u8, 1000 + i), PexFlags(0));
            assert_eq!(ok, i < 50, "v4 {i}");
        }
        // v6 has its own budget.
        for i in 0..60u16 {
            assert_eq!(b.add(v6(i, 1000), PexFlags(0)), i < 50);
        }
        for i in 0..60u16 {
            assert_eq!(b.drop_peer(v4(i as u8, 9)), i < 50);
        }
        let mut out = Vec::new();
        b.encode_payload(&mut out);
        let m = PexMessage::parse(&out).unwrap();
        assert_eq!(m.added().count(), 100);
        assert_eq!(m.dropped().count(), 50);
    }

    #[test]
    fn recv_cap_and_port_zero() {
        let mut list = Vec::new();
        for i in 0..1000u32 {
            list.extend_from_slice(&[1, 2, 3, 4]);
            let port: u16 = if i == 0 { 0 } else { 1 + i as u16 };
            list.extend_from_slice(&port.to_be_bytes());
        }
        let mut payload = b"d5:added6000:".to_vec();
        payload.extend_from_slice(&list);
        payload.push(b'e');
        let m = PexMessage::parse(&payload).unwrap();
        // 200 entries considered, the first has port 0 and is skipped.
        assert_eq!(m.added().count(), MAX_PEX_RECV - 1);
    }

    #[test]
    fn malformed_rejected() {
        assert!(PexMessage::parse(b"d5:added5:\x01\x02\x03\x04\x05e").is_err());
        assert!(PexMessage::parse(b"d6:added618:\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00e").is_err());
        assert!(PexMessage::parse(b"not bencode").is_err());
        assert!(PexMessage::parse(b"li1ee").is_err());
        let m = PexMessage::parse(b"de").unwrap();
        assert_eq!(m.added().count() + m.dropped().count(), 0);
    }

    #[test]
    fn short_flags_default_to_zero() {
        let m = PexMessage::parse(
            b"d5:added12:\x01\x01\x01\x01\x00\x50\x02\x02\x02\x02\x00\x517:added.f1:\x02e",
        )
        .unwrap();
        let v: Vec<_> = m.added().collect();
        assert_eq!(v[0].flags, PexFlags(2));
        assert_eq!(v[1].flags, PexFlags(0));
    }
}
