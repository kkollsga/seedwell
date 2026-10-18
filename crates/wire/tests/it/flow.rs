//! End-to-end exchange between two in-memory peers: handshake, extended handshake,
//! magnet-style metadata fetch with verification, and the private-torrent PEX rule.

use bytes::BytesMut;
use sha1::{Digest, Sha1};
use trs_wire::metadata::{METADATA_PIECE_LEN, piece_slice};
use trs_wire::*;

struct Peer {
    enc: Encoder,
    rx: BytesMut,
    dec: Decoder,
    state: PeerState,
    policy: ExtPolicy,
}

fn connect(private: bool) -> (Peer, Peer) {
    let ih = [9u8; 20];
    let a = Handshake::new(ih, *b"-SX0001-aaaaaaaaaaaa", private);
    let b = Handshake::new(ih, *b"-SX0001-bbbbbbbbbbbb", private);
    let hb = Handshake::decode(&a.to_bytes()).unwrap().unwrap();
    let ha = Handshake::decode(&b.to_bytes()).unwrap().unwrap();
    let mk = |ours: &Handshake, theirs: &Handshake| Peer {
        enc: Encoder::new(),
        rx: BytesMut::new(),
        dec: Decoder::new(Limits::default()).negotiated(&ours.reserved, &theirs.reserved),
        state: PeerState::new(theirs.reserved),
        policy: ExtPolicy::for_torrent(private),
    };
    (mk(&a, &hb), mk(&b, &ha))
}

fn pump(from: &mut Peer, to: &mut Peer) -> Vec<OwnedMessage> {
    let n = from.enc.pending().len();
    to.rx.extend_from_slice(from.enc.pending());
    from.enc.advance(n);
    let mut out = Vec::new();
    while let Some(m) = to.dec.decode(&mut to.rx).unwrap() {
        out.push(m);
    }
    out
}

#[test]
fn magnet_metadata_fetch_over_the_wire() {
    let metadata: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
    let ih: [u8; 20] = Sha1::digest(&metadata).into();
    let (mut leech, mut seed) = connect(false);

    // Both send extended handshakes; the seed advertises metadata_size.
    let mut hs = LocalExtendedHandshake::new("0.1.0", leech.policy);
    leech.enc.push_extended(0, |b| hs.encode_payload(b));
    hs.metadata_size = Some(metadata.len() as u32);
    seed.enc.push_extended(0, |b| hs.encode_payload(b));
    for m in pump(&mut seed, &mut leech) {
        let Message::Extended { id, payload } = m else {
            panic!()
        };
        let ExtendedMessage::Handshake(h) =
            ExtendedMessage::parse(id, &payload, leech.policy).unwrap()
        else {
            panic!()
        };
        leech.state.apply_extended_handshake(&h);
    }
    for m in pump(&mut leech, &mut seed) {
        let Message::Extended { id, payload } = m else {
            panic!()
        };
        let ExtendedMessage::Handshake(h) =
            ExtendedMessage::parse(id, &payload, seed.policy).unwrap()
        else {
            panic!()
        };
        seed.state.apply_extended_handshake(&h);
    }
    assert_eq!(leech.state.remote_metadata_size, metadata.len() as u32);
    assert_eq!(
        leech.state.remote_client.unwrap().to_string_lossy(),
        "Seedwell 0.1.0"
    );

    // Leech requests every piece using the id the seed announced.
    let mut asm = MetadataAssembler::new(ih, leech.state.remote_metadata_size).unwrap();
    for p in asm.missing().collect::<Vec<_>>() {
        leech
            .enc
            .push_extended(leech.state.remote_ut_metadata, |b| {
                MetadataMessage::Request { piece: p }.encode_payload(b)
            });
    }
    for m in pump(&mut leech, &mut seed) {
        let Message::Extended { id, payload } = m else {
            panic!()
        };
        // Seed receives on *its* local id.
        assert_eq!(id, LOCAL_UT_METADATA_ID);
        let ExtendedMessage::Metadata(MetadataMessage::Request { piece }) =
            ExtendedMessage::parse(id, &payload, seed.policy).unwrap()
        else {
            panic!()
        };
        let data = piece_slice(&metadata, piece).unwrap();
        seed.enc.push_extended(seed.state.remote_ut_metadata, |b| {
            MetadataMessage::Data {
                piece,
                total_size: metadata.len() as u32,
                data,
            }
            .encode_payload(b)
        });
    }
    for m in pump(&mut seed, &mut leech) {
        let Message::Extended { id, payload } = m else {
            panic!()
        };
        let ExtendedMessage::Metadata(MetadataMessage::Data {
            piece,
            data,
            total_size,
        }) = ExtendedMessage::parse(id, &payload, leech.policy).unwrap()
        else {
            panic!()
        };
        assert_eq!(total_size, 50_000);
        assert!(data.len() <= METADATA_PIECE_LEN);
        asm.insert(piece, data).unwrap();
    }
    assert_eq!(&*asm.finish().unwrap(), &metadata[..]);
}

#[test]
fn private_torrent_has_no_pex_anywhere() {
    let (mut a, mut b) = connect(true);
    let hs = LocalExtendedHandshake::new("0.1.0", a.policy);
    a.enc.push_extended(0, |o| hs.encode_payload(o));
    for m in pump(&mut a, &mut b) {
        let Message::Extended { id, payload } = m else {
            panic!()
        };
        let ExtendedMessage::Handshake(h) = ExtendedMessage::parse(id, &payload, b.policy).unwrap()
        else {
            panic!()
        };
        assert_eq!(h.ut_pex, None, "private torrent advertised ut_pex");
        // Even if the remote sends PEX anyway, we refuse to parse it.
        assert!(matches!(
            ExtendedMessage::parse(LOCAL_UT_PEX_ID, b"de", b.policy),
            Err(Error::ExtensionDisabled(_))
        ));
        // And the reserved bits do not advertise DHT.
        assert!(!b.state.remote_reserved.dht());
    }
}

#[test]
fn fast_extension_flow() {
    let (mut a, mut b) = connect(false);
    let ip = "10.1.2.3".parse().unwrap();
    a.enc.push(&Message::<&[u8]>::HaveAll);
    for p in allowed_fast_set(&[9; 20], ip, 100, 4) {
        a.enc.push(&Message::<&[u8]>::AllowedFast(p));
    }
    let msgs = pump(&mut a, &mut b);
    assert_eq!(msgs[0], Message::HaveAll);
    assert_eq!(msgs.len(), 5);
    // A peer that did not negotiate Fast cannot use it.
    let plain = Decoder::new(Limits::default());
    let mut raw = BytesMut::new();
    Message::<&[u8]>::HaveAll.encode(&mut raw);
    assert_eq!(
        plain.decode(&mut raw).unwrap_err(),
        Error::NotNegotiated(14)
    );
}
