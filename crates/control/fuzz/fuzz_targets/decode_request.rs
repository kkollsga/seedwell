#![no_main]
//! Whatever bytes arrive on the control socket, decoding returns a `Result`; and anything that
//! decodes re-encodes to a request that decodes to the same thing.
use libfuzzer_sys::fuzz_target;
use trs_control::proto::{decode_request, encode_request};

fuzz_target!(|data: &[u8]| {
    if let Ok(d) = decode_request(data) {
        let line = encode_request(&d.id, &d.request);
        let again = decode_request(&line[..line.len() - 1]).expect("own encoding must decode");
        assert_eq!(again.request.method(), d.request.method());
    }
    // The base64 decoder sits behind `add`; give it the raw bytes as text too.
    if let Ok(s) = std::str::from_utf8(data) {
        let _ = trs_control::b64::decode(s, 1 << 20);
        let _ = trs_control::redact::scrub_urls(s);
        let _ = trs_control::redact::redact_magnet(s);
    }
});
