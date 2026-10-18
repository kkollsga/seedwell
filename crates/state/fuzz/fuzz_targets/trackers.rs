#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_state::url::{AnnounceUrl, redact_url};
use trs_state::{Document, TrackerProfiles};

// Input: trackers.toml text, a NUL, then newline-separated announce URLs to match.
fuzz_target!(|data: &[u8]| {
    let (doc, urls) = match data.iter().position(|b| *b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let urls = String::from_utf8_lossy(urls);
    let list: Vec<&str> = urls.lines().take(16).collect();
    for u in &list {
        if let Ok(a) = AnnounceUrl::parse(u) {
            let _ = format!("{a} {a:?}");
        }
        let _ = redact_url(u);
    }
    if let Ok(p) = TrackerProfiles::parse_str(&String::from_utf8_lossy(doc)) {
        let out = p.value.render().expect("validated profiles must render");
        let back = TrackerProfiles::parse_str(&out).expect("rendered profiles must parse");
        assert_eq!(back.value, p.value);
        let m1 = p.value.match_announce(list.iter().copied()).map(|m| (m.profile.id.clone(), m.tier));
        let m2 = p.value.match_announce(list.iter().copied()).map(|m| (m.profile.id.clone(), m.tier));
        assert_eq!(m1, m2);
    }
});
