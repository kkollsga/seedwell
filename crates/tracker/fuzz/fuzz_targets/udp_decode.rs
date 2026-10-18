#![no_main]
//! BEP 15 datagram decoding never panics; announce replies are clamped.
use libfuzzer_sys::fuzz_target;
use trs_tracker::udp_proto::{self, Reply};

fuzz_target!(|data: &[u8]| {
    for v6 in [false, true] {
        if let Ok(Reply::Announce { response, .. }) = udp_proto::decode(data, v6) {
            assert!(response.peers.len() <= trs_tracker::response::MAX_PEERS);
            assert!(response.interval <= trs_tracker::response::MAX_INTERVAL);
        }
    }
    let _ = udp_proto::peek_header(data);
});
