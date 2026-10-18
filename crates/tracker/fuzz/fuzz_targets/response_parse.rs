#![no_main]
//! HTTP announce/scrape response parsing must never panic and must uphold the
//! SPEC 7C.1 clamps on whatever it accepts.
use libfuzzer_sys::fuzz_target;
use std::time::Duration;
use trs_tracker::response::{MAX_INTERVAL, MAX_PEERS, Timing};
use trs_tracker::{parse_announce_response, scrape};

fuzz_target!(|data: &[u8]| {
    if let Ok(r) = parse_announce_response(data) {
        assert!(r.interval <= MAX_INTERVAL);
        assert!(r.interval >= Duration::from_secs(60));
        if let Some(m) = r.min_interval {
            assert!(r.interval >= m && m >= Duration::from_secs(60) && m <= MAX_INTERVAL);
        } else {
            assert!(r.interval >= Duration::from_secs(300));
        }
        assert!(r.peers.len() <= MAX_PEERS);
        let t = Timing {
            interval: r.interval,
            min_interval: r.min_interval,
        };
        assert_eq!(t.sanitized(), t);
    }
    let _ = scrape::parse_scrape_response(data);
});
