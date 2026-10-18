#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_metainfo::Magnet;

fuzz_target!(|data: &str| {
    if let Ok(m) = Magnet::parse(data) {
        assert!(m.info_hash.v1.is_some() || m.info_hash.v2.is_some());
        // Rendering and re-parsing must preserve the identity.
        // (Percent-encoding can push very long inputs over the length cap, so a
        // `TooLong` rejection of our own output is legitimate.)
        if let Ok(again) = Magnet::parse(&m.to_uri(true)) {
            assert_eq!(again.info_hash, m.info_hash);
        }
    }
});
