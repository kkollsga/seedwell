#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_bencode::{decode, decode_prefix, encode, raw_dict_entries, raw_list_items};

fuzz_target!(|data: &[u8]| {
    if let Ok(v) = decode(data) {
        let enc = encode(&v);
        // Canonical inputs must re-encode byte for byte.
        if v.is_canonical() {
            assert_eq!(enc, data);
        }
        // Whatever we emit must decode, be canonical, and be a fixed point.
        // (The tree itself may differ from `v` when `v` had unsorted keys.)
        let back = decode(&enc).expect("own output must decode");
        assert!(back.is_canonical());
        assert_eq!(encode(&back), enc);
    }
    let _ = decode_prefix(data);
    // The raw-span API must agree with the tree decoder on validity.
    if let Ok(entries) = raw_dict_entries(data) {
        assert!(decode(data).is_ok());
        for (_, raw) in entries {
            assert!(decode(raw).is_ok());
        }
    }
    let _ = raw_list_items(data);
});
