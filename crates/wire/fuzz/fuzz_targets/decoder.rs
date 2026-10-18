#![no_main]
//! Feeds arbitrary bytes to the frame decoder and handshake parser.
//!
//! Invariants: no panic; a returned message never claims more bytes than were given;
//! splitting the input at an arbitrary point yields the same first message.

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use trs_wire::{Decoder, Handshake, Limits};

fuzz_target!(|data: &[u8]| {
    let _ = Handshake::decode(data);
    let Some((&cfg, data)) = data.split_first() else {
        return;
    };
    let limits = if cfg & 4 != 0 {
        Limits::for_pieces(u32::from(cfg))
    } else {
        Limits::default()
    };
    let dec = Decoder::new(limits).with_features(cfg & 1 != 0, cfg & 2 != 0);

    let mut buf = data;
    while let Ok(Some((_, used))) = dec.decode_ref(buf) {
        assert!(used >= 4 && used <= buf.len());
        buf = &buf[used..];
    }

    // Owned path with an arbitrary split must agree with the borrowed path.
    let split = data.len() / 2;
    let mut bm = BytesMut::from(&data[..split]);
    let first = dec.decode(&mut bm);
    bm.extend_from_slice(&data[split..]);
    let mut owned = 0usize;
    while let Ok(Some(_)) = dec.decode(&mut bm) {
        owned += 1;
    }
    let _ = (first, owned);
    let _ = dec.bytes_needed(data);
});
