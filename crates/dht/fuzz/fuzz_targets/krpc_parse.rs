#![no_main]
//! Feeds arbitrary bytes to the KRPC parser and walks every accessor. Must never panic.

use libfuzzer_sys::fuzz_target;
use trs_dht::codec::{self, Body};

fuzz_target!(|data: &[u8]| {
    match codec::parse(data) {
        Ok(m) => {
            let _ = (m.tid.len(), m.version, m.read_only, m.ip);
            if let Body::Response(r) = m.body {
                let _ = r.nodes().count();
                let _ = r.values().count();
            }
        }
        Err(r) => {
            let _ = (r.err, r.tid, r.was_query);
        }
    }
});
