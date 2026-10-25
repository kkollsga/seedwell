//! Decoding hostile input: never a panic, and nothing allocated beyond the caps (the fuzz target `msg_decode`
//! runs the same check without end; these runs keep it in every `cargo test`).

use proptest::collection::vec;
use proptest::prelude::*;
use trs_sync_proto::codec::{DecodeError, Frame};
use trs_sync_proto::msg::{MAX_INDEX_ENTRIES, Message};

use crate::roundtrip::message;

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(512)
}

/// What the fuzz target does: decode, and if that works, encode and decode again to the same message.
fn check(kind: u8, flags: u8, payload: &[u8]) {
    if let Ok(m) = Message::decode(Frame { kind, flags, payload }) {
        let mut out = Vec::new();
        m.encode(&mut out);
        let (frame, _) = Frame::split(&out).unwrap().unwrap();
        assert_eq!(Message::decode(frame).unwrap(), m);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn random_bytes_never_panic(kind in 0..20u8, flags in any::<u8>(), payload in vec(any::<u8>(), 0..400)) {
        check(kind, flags, &payload);
    }

    /// Valid messages with bytes flipped, cut short or extended: close to valid, so the decoder goes deep.
    #[test]
    fn damaged_messages_never_panic(
        m in message(),
        flips in vec((any::<prop::sample::Index>(), any::<u8>()), 0..4),
        cut in any::<prop::sample::Index>(),
    ) {
        let mut payload = Vec::new();
        m.encode_payload(&mut payload);
        for (at, x) in flips {
            if !payload.is_empty() {
                let i = at.index(payload.len());
                payload[i] ^= x;
            }
        }
        check(m.kind(), 0, &payload);
        let n = cut.index(payload.len() + 1);
        check(m.kind(), 0, &payload[..n]);
    }

    #[test]
    fn random_streams_never_panic(stream in vec(any::<u8>(), 0..64)) {
        let mut rest = &stream[..];
        while let Ok(Some((frame, used))) = Frame::split(rest) {
            let _ = Message::decode(frame);
            rest = &rest[used..];
        }
    }
}

#[test]
fn a_huge_claimed_count_is_refused_without_allocating() {
    // Index: folder id, index id, full, then an entry count of u64::MAX.
    let mut p = vec![0u8; 32];
    p.extend_from_slice(&[0, 1]);
    p.extend_from_slice(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
    let err = Message::decode(Frame { kind: 4, flags: 0, payload: &p }).unwrap_err();
    assert_eq!(err, DecodeError::TooLong { what: "index entries", max: MAX_INDEX_ENTRIES });
    // Within the cap, but more than the bytes that follow could hold.
    let mut p = vec![0u8; 32];
    p.extend_from_slice(&[0, 1, 200, 7]);
    let err = Message::decode(Frame { kind: 4, flags: 0, payload: &p }).unwrap_err();
    assert_eq!(err, DecodeError::Truncated("index entries"));
}

#[test]
fn errors_read_as_sentences() {
    let err = Message::decode(Frame { kind: 10, flags: 0, payload: &[5, b'a'] }).unwrap_err();
    assert_eq!(err.to_string(), "message ends inside reason");
}
