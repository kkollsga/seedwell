#![no_main]
//! Header + extension-chain parser: never panics; whatever parses re-encodes identically
//! (modulo the extension chain, which we canonicalise to a single SACK).

use libfuzzer_sys::fuzz_target;
use trs_utp::packet::{self, Packet};

fuzz_target!(|data: &[u8]| {
    let _ = packet::looks_like_utp(data);
    if let Ok(p) = Packet::parse(data) {
        let mut buf = vec![0u8; data.len() + 64];
        let sack = p.sack.filter(|s| !s.is_empty() && s.len() < 256).unwrap_or(&[]);
        let off = packet::encode(&p.header, sack, &mut buf).expect("buffer is large enough");
        let q = Packet::parse(&buf[..off]).expect("our own encoding parses");
        assert_eq!(q.header, p.header);
        assert_eq!(q.sack.unwrap_or(&[]), sack);
        if let Some(s) = p.sack {
            for off in packet::sack_offsets(s) {
                assert!((off as usize) < s.len() * 8);
            }
        }
    }
});
