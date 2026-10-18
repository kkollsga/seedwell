#![no_main]
//! Feeds arbitrary bytes to every extension-payload parser and the bitfield validator.

use libfuzzer_sys::fuzz_target;
use trs_wire::{
    Bitfield, ExtPolicy, ExtendedHandshake, ExtendedMessage, MetadataAssembler, MetadataMessage,
    PexMessage,
};

fuzz_target!(|data: &[u8]| {
    let _ = ExtendedHandshake::parse(data).map(|h| h.unknown_extensions().count());
    if let Ok(p) = PexMessage::parse(data) {
        let _ = p.added().count() + p.dropped().count();
    }
    let _ = MetadataMessage::parse(data);
    for id in 0..4u8 {
        let _ = ExtendedMessage::parse(id, data, ExtPolicy::for_torrent(id & 1 == 0));
    }
    if let Some((&n, rest)) = data.split_first() {
        let _ = Bitfield::from_wire(rest, u32::from(n) * 3);
        // Arbitrary pieces into the assembler must only ever error, never panic.
        if let Ok(mut a) = MetadataAssembler::new([0; 20], 40_000) {
            let _ = a.insert(u32::from(n % 5), rest);
            let _ = a.finish();
        }
    }
});
