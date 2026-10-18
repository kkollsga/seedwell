#![no_main]
//! Arbitrary bytes into every UDP packet decoder. Must never panic.

use libfuzzer_sys::fuzz_target;
use trs_portmap::{natpmp, pcp, ssdp};

fuzz_target!(|data: &[u8]| {
    let _ = natpmp::decode_response(data);
    let _ = pcp::decode_response(data);
    let _ = ssdp::parse_reply(data);
    if data.len() >= 8 {
        let a = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let b = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
        let _ = natpmp::epoch_reset(a, 1000, b, 5000 + u64::from(a));
    }
});
