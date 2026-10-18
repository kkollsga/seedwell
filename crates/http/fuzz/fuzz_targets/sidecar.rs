#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_http::{RangeSet, Sidecar};

fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    if let Some(sc) = Sidecar::parse(&s) {
        // Whatever parses must survive a round trip.
        assert_eq!(Sidecar::parse(&sc.encode()), Some(sc));
    }
    if let Some(r) = RangeSet::parse(&s) {
        assert_eq!(RangeSet::parse(&r.encode()), Some(r));
    }
});
