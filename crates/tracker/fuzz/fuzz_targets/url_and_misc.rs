#![no_main]
//! Tracker URL parsing, announce URL building, scrape URL derivation and
//! Retry-After parsing on arbitrary text.
use libfuzzer_sys::fuzz_target;
use trs_tracker::url::{TrackerUrl, build_announce_url};
use trs_tracker::{AnnounceParams, retry_after, scrape};

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    let _ = retry_after::parse_retry_after(s, 1_700_000_000);
    if let Ok(u) = TrackerUrl::parse(s) {
        // Display must never leak the raw URL (passkeys).
        let shown = format!("{u} {u:?}");
        assert!(!shown.contains('?') || shown.contains('…'));
        let p = AnnounceParams {
            info_hash: [0xAB; 20],
            peer_id: [0x01; 20],
            port: 1,
            uploaded: 0,
            downloaded: 0,
            corrupt: 0,
            left: 0,
            key: 1,
            event: None,
            numwant: 50,
            tracker_id: Some(
                data.iter()
                    .copied()
                    .take(300)
                    .collect::<Vec<u8>>()
                    .into_boxed_slice(),
            ),
            ipv4: None,
            ipv6: None,
            crypto: Default::default(),
        };
        let built = build_announce_url(&u, &p);
        // The existing path and query are preserved byte for byte as a prefix.
        assert!(built.starts_with(u.raw()));
        assert_eq!(built.matches("info_hash=%AB%AB").count(), 1);
        let _ = scrape::scrape_url(&u, &[[1; 20]]);
    }
});
