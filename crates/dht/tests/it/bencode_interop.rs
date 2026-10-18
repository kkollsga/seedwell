//! Cross-checks the DHT's purpose-built codec against the workspace's reference bencode crate,
//! in both directions, on random messages.

use proptest::prelude::*;
use std::net::{Ipv4Addr, SocketAddrV4};
use trs_bencode::{Encoder, Value, decode};
use trs_dht::codec::{self, Body, Envelope, Query, QueryOut, ResponseOut};

fn addr(a: [u8; 4], p: u16) -> SocketAddrV4 {
    SocketAddrV4::new(Ipv4Addr::from(a), p)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(
        std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(256)
    ))]

    /// Everything we emit is canonical bencode that the reference decoder accepts, with the
    /// fields where BEP 5 says they are.
    #[test]
    fn our_encoding_is_valid_canonical_bencode(
        id in any::<[u8; 20]>(), target in any::<[u8; 20]>(), tid in proptest::collection::vec(any::<u8>(), 0..=16),
        port in any::<u16>(), token in proptest::collection::vec(any::<u8>(), 1..=32),
        ro in any::<bool>(), ver in proptest::option::of(any::<[u8; 4]>()),
        peers in proptest::collection::vec((any::<[u8; 4]>(), any::<u16>()), 0..60),
        nodes in proptest::collection::vec(any::<[u8; 26]>(), 0..9),
        ip in proptest::option::of((any::<[u8; 4]>(), any::<u16>())),
    ) {
        let env = Envelope { tid: &tid, version: ver.as_ref().map(|v| &v[..]), read_only: ro, ip: ip.map(|(a, p)| addr(a, p)) };
        let queries = [
            QueryOut::Ping { id: &id },
            QueryOut::FindNode { id: &id, target: &target },
            QueryOut::GetPeers { id: &id, info_hash: &target },
            QueryOut::AnnouncePeer { id: &id, info_hash: &target, port, token: &token },
        ];
        for q in &queries {
            let mut o = Vec::new();
            codec::encode_query(&mut o, &env, q);
            let v = decode(&o).expect("reference decoder accepts");
            prop_assert!(v.is_canonical());
            prop_assert_eq!(v.get(b"y").and_then(Value::as_bytes), Some(&b"q"[..]));
            prop_assert_eq!(v.get(b"t").and_then(Value::as_bytes), Some(&tid[..]));
            prop_assert_eq!(v.get(b"a").and_then(|a| a.get(b"id")).and_then(Value::as_bytes), Some(&id[..]));
            prop_assert_eq!(v.get(b"ro").is_some(), ro);
            // and our own parser round-trips it
            let m = codec::parse(&o).expect("self parse");
            prop_assert_eq!(m.tid, &tid[..]);
            prop_assert_eq!(m.read_only, ro);
            prop_assert!(matches!(m.body, Body::Query(_)));
        }
        let flat: Vec<u8> = nodes.iter().flatten().copied().collect();
        let values: Vec<SocketAddrV4> = peers.iter().map(|(a, p)| addr(*a, *p)).collect();
        let mut o = Vec::new();
        codec::encode_response(&mut o, &env, &ResponseOut { id: &id, token: Some(&token), nodes: &flat, values: &values });
        let v = decode(&o).expect("reference decoder accepts");
        prop_assert!(v.is_canonical());
        let r = v.get(b"r").unwrap();
        prop_assert_eq!(r.get(b"values").and_then(Value::as_list).map_or(0, <[_]>::len), values.len());
        let Body::Response(pr) = codec::parse(&o).unwrap().body else { panic!() };
        prop_assert_eq!(pr.values().collect::<Vec<_>>(), values);
        prop_assert_eq!(pr.nodes().count(), nodes.len());
        let mut e = Vec::new();
        codec::encode_error(&mut e, &env, 203, "Protocol Error");
        prop_assert!(decode(&e).unwrap().is_canonical());
    }

    /// Messages built by the reference encoder (any key order we are likely to meet on the wire
    /// is sorted; unsorted is covered in unit tests) parse to the same fields.
    #[test]
    fn reference_encoder_output_parses(
        id in any::<[u8; 20]>(), ih in any::<[u8; 20]>(), tid in proptest::collection::vec(any::<u8>(), 0..=16),
        port in any::<u16>(), token in proptest::collection::vec(any::<u8>(), 0..40),
    ) {
        let mut e = Encoder::new();
        e.begin_dict()
            .bytes(b"a").begin_dict()
                .bytes(b"id").bytes(&id)
                .bytes(b"info_hash").bytes(&ih)
                .bytes(b"port").int(i64::from(port))
                .bytes(b"token").bytes(&token)
            .end()
            .bytes(b"q").bytes(b"announce_peer")
            .bytes(b"t").bytes(&tid)
            .bytes(b"y").bytes(b"q")
            .end();
        let bytes = e.finish();
        let m = codec::parse(&bytes).expect("parse");
        prop_assert_eq!(m.tid, &tid[..]);
        match m.body {
            Body::Query(Query::AnnouncePeer { id: i, info_hash, port: p, token: t, implied_port }) => {
                prop_assert_eq!(i, id);
                prop_assert_eq!(info_hash, ih);
                prop_assert_eq!(p, port);
                prop_assert_eq!(t, &token[..]);
                prop_assert!(!implied_port);
            }
            other => prop_assert!(false, "{:?}", other),
        }
    }

    /// Whatever the reference decoder rejects, we must not mistake for a valid message of a
    /// kind that would make us act (no query is produced from non-bencode).
    #[test]
    fn random_bytes_never_panic_and_agree_on_rejection(data in proptest::collection::vec(any::<u8>(), 0..300)) {
        let ours = codec::parse(&data);
        if ours.is_ok() {
            // We tolerate trailing bytes, so only the prefix must be valid for the reference.
            prop_assert!(trs_bencode::decode_prefix(&data).is_ok());
        }
    }
}
