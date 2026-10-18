//! Property tests: roundtrips and "arbitrary bytes never panic".

use bytes::BytesMut;
use proptest::prelude::*;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use trs_wire::pex::MAX_PEX_SEND;
use trs_wire::*;

fn block() -> impl Strategy<Value = BlockRequest> {
    (any::<u32>(), any::<u32>(), 1u32..=16384).prop_map(|(index, begin, length)| BlockRequest {
        index,
        begin,
        length,
    })
}

fn message() -> impl Strategy<Value = Message<Vec<u8>>> {
    prop_oneof![
        Just(Message::KeepAlive),
        Just(Message::Choke),
        Just(Message::Unchoke),
        Just(Message::Interested),
        Just(Message::NotInterested),
        any::<u32>().prop_map(Message::Have),
        prop::collection::vec(any::<u8>(), 0..300).prop_map(Message::Bitfield),
        block().prop_map(Message::Request),
        (
            any::<u32>(),
            any::<u32>(),
            prop::collection::vec(any::<u8>(), 1..=16384)
        )
            .prop_map(|(index, begin, data)| Message::Piece { index, begin, data }),
        block().prop_map(Message::Cancel),
        any::<u16>().prop_map(Message::Port),
        any::<u32>().prop_map(Message::Suggest),
        Just(Message::HaveAll),
        Just(Message::HaveNone),
        block().prop_map(Message::RejectRequest),
        any::<u32>().prop_map(Message::AllowedFast),
        (any::<u8>(), prop::collection::vec(any::<u8>(), 0..2000))
            .prop_map(|(id, payload)| Message::Extended { id, payload }),
    ]
}

fn decoder() -> Decoder {
    Decoder::new(Limits::default()).with_features(true, true)
}

proptest! {
    #[test]
    fn message_roundtrip(m in message()) {
        let mut v = Vec::new();
        m.encode(&mut v);
        prop_assert_eq!(v.len(), m.encoded_len());
        let (d, used) = decoder().decode_ref(&v).unwrap().unwrap();
        prop_assert_eq!(used, v.len());
        prop_assert_eq!(d.map(|s| s.to_vec()), m);
    }

    #[test]
    fn stream_roundtrip_random_chunking(
        msgs in prop::collection::vec(message(), 0..12),
        cuts in prop::collection::vec(1usize..5000, 1..40),
    ) {
        let mut stream = Vec::new();
        for m in &msgs { m.encode(&mut stream); }
        let d = decoder();
        let mut buf = BytesMut::new();
        let mut got = Vec::new();
        let mut pos = 0;
        let mut ci = 0;
        while pos < stream.len() {
            let n = cuts[ci % cuts.len()].min(stream.len() - pos);
            ci += 1;
            buf.extend_from_slice(&stream[pos..pos + n]);
            pos += n;
            while let Some(m) = d.decode(&mut buf).unwrap() {
                got.push(m.map(|b| b.to_vec()));
            }
        }
        prop_assert_eq!(got, msgs);
        prop_assert!(buf.is_empty());
    }

    #[test]
    fn arbitrary_bytes_never_panic(data in prop::collection::vec(any::<u8>(), 0..2048)) {
        let d = decoder();
        let _ = d.decode_ref(&data);
        let mut b = BytesMut::from(&data[..]);
        let _ = d.decode(&mut b);
        let _ = Handshake::decode(&data);
        let _ = ExtendedHandshake::parse(&data);
        let _ = MetadataMessage::parse(&data);
        if let Ok(p) = PexMessage::parse(&data) {
            let _ = p.added().count() + p.dropped().count();
        }
        for id in 0..4u8 {
            let _ = ExtendedMessage::parse(id, &data, ExtPolicy::for_torrent(false));
        }
        let _ = Bitfield::from_wire(&data, data.len() as u32 * 8);
    }

    /// Frames whose header is plausible but whose body is random must never panic or
    /// consume more than they claim.
    #[test]
    fn plausible_headers_random_bodies(id in 0u8..24, body in prop::collection::vec(any::<u8>(), 0..64)) {
        let mut v = ((body.len() + 1) as u32).to_be_bytes().to_vec();
        v.push(id);
        v.extend_from_slice(&body);
        if let Ok(Some((_, used))) = decoder().decode_ref(&v) {
            prop_assert_eq!(used, v.len());
        }
    }

    #[test]
    fn handshake_roundtrip(r in any::<[u8; 8]>(), ih in any::<[u8; 20]>(), pid in any::<[u8; 20]>()) {
        let h = Handshake { reserved: Reserved(r), info_hash: ih, peer_id: pid };
        prop_assert_eq!(Handshake::decode(&h.to_bytes()).unwrap(), Some(h));
    }

    #[test]
    fn bitfield_matches_reference(len in 0u32..300, ops in prop::collection::vec((any::<bool>(), 0u32..320), 0..200)) {
        let mut b = Bitfield::new(len);
        let mut r = vec![false; len as usize];
        for (set, i) in ops {
            if set {
                let changed = b.set(i);
                let exp = (i as usize) < r.len() && !r[i as usize];
                if (i as usize) < r.len() { r[i as usize] = true; }
                prop_assert_eq!(changed, exp);
            } else {
                let changed = b.clear(i);
                let exp = (i as usize) < r.len() && r[i as usize];
                if (i as usize) < r.len() { r[i as usize] = false; }
                prop_assert_eq!(changed, exp);
            }
        }
        prop_assert_eq!(b.count_ones() as usize, r.iter().filter(|x| **x).count());
        prop_assert_eq!(b.iter_ones().collect::<Vec<_>>(),
            r.iter().enumerate().filter(|(_, x)| **x).map(|(i, _)| i as u32).collect::<Vec<_>>());
        prop_assert_eq!(b.iter_zeros().collect::<Vec<_>>(),
            r.iter().enumerate().filter(|(_, x)| !**x).map(|(i, _)| i as u32).collect::<Vec<_>>());
        // Wire form is always valid for its own length and restores identically.
        let back = Bitfield::from_wire(b.as_bytes(), len).unwrap();
        prop_assert_eq!(back, b);
    }

    #[test]
    fn bitfield_from_wire_accepts_iff_spare_zero(bytes in prop::collection::vec(any::<u8>(), 0..8), len in 0u32..70) {
        let res = Bitfield::from_wire(&bytes, len);
        let len_ok = bytes.len() == (len as usize).div_ceil(8);
        let spare_ok = match bytes.last() {
            Some(&l) if len_ok => {
                let spare = bytes.len() as u32 * 8 - len;
                l & ((1u16 << spare) - 1) as u8 == 0
            }
            _ => true,
        };
        let ok = len_ok && spare_ok;
        prop_assert_eq!(res.is_ok(), ok);
        if let Ok(b) = res {
            prop_assert_eq!(b.count_ones(), bytes.iter().map(|x| x.count_ones()).sum::<u32>());
        }
    }

    #[test]
    fn metadata_message_roundtrip(piece in any::<u32>(), total in any::<u32>(), data in prop::collection::vec(any::<u8>(), 0..=16384), kind in 0u8..3) {
        let m = match kind {
            0 => MetadataMessage::Request { piece },
            1 => MetadataMessage::Data { piece, total_size: total, data: &data },
            _ => MetadataMessage::Reject { piece },
        };
        let mut v = Vec::new();
        m.encode_payload(&mut v);
        prop_assert_eq!(MetadataMessage::parse(&v).unwrap(), m);
    }

    #[test]
    fn metadata_assembly_any_order(len in 1usize..70_000, seed in any::<u64>()) {
        use sha1::{Digest, Sha1};
        let data: Vec<u8> = (0..len).map(|i| (i as u64).wrapping_mul(seed | 1) as u8).collect();
        let hash: [u8; 20] = Sha1::digest(&data).into();
        let mut a = MetadataAssembler::new(hash, len as u32).unwrap();
        let n = a.num_pieces();
        let mut order: Vec<u32> = (0..n).collect();
        // Cheap deterministic shuffle.
        let mut s = seed;
        for i in (1..order.len()).rev() {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            order.swap(i, (s >> 33) as usize % (i + 1));
        }
        for p in order {
            prop_assert!(a.insert(p, trs_wire::metadata::piece_slice(&data, p).unwrap()).unwrap());
        }
        prop_assert_eq!(&*a.finish().unwrap(), &data[..]);
    }

    #[test]
    fn pex_roundtrip(
        v4 in prop::collection::vec((any::<[u8; 4]>(), 1u16.., any::<u8>()), 0..80),
        v6 in prop::collection::vec((any::<[u8; 16]>(), 1u16.., any::<u8>()), 0..80),
    ) {
        let mut b = PexBuilder::new();
        let mut exp4 = Vec::new();
        let mut exp6 = Vec::new();
        for (ip, port, fl) in v4 {
            let a = SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port);
            if b.add(a, PexFlags(fl)) { exp4.push((a, fl)); }
        }
        for (ip, port, fl) in v6 {
            let a = SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port);
            if b.add(a, PexFlags(fl)) { exp6.push((a, fl)); }
        }
        prop_assert!(exp4.len() <= MAX_PEX_SEND && exp6.len() <= MAX_PEX_SEND);
        let mut out = Vec::new();
        b.encode_payload(&mut out);
        let m = PexMessage::parse(&out).unwrap();
        let got: Vec<_> = m.added().map(|p| (p.addr, p.flags.0)).collect();
        let exp: Vec<_> = exp4.into_iter().chain(exp6).collect();
        prop_assert_eq!(got, exp);
    }

    #[test]
    fn allowed_fast_properties(ih in any::<[u8; 20]>(), ip in any::<[u8; 4]>(), n in 0u32..5000, k in 0usize..40) {
        let ip = IpAddr::V4(Ipv4Addr::from(ip));
        let s = allowed_fast_set(&ih, ip, n, k);
        prop_assert_eq!(s.len(), k.min(n as usize));
        let mut d = s.clone(); d.sort_unstable(); d.dedup();
        prop_assert_eq!(d.len(), s.len());
        prop_assert!(s.iter().all(|&i| i < n));
    }

    #[test]
    fn block_requests_tile_the_piece(piece_len in 0u32..2_000_000, block in 1u32..40_000) {
        let v: Vec<_> = block_requests(1, piece_len, block).collect();
        let mut pos = 0u32;
        for r in &v {
            prop_assert_eq!(r.begin, pos);
            prop_assert!(r.length >= 1 && r.length <= block);
            pos += r.length;
        }
        prop_assert_eq!(pos, piece_len);
    }
}
