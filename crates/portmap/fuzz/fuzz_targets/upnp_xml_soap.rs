#![no_main]
//! Arbitrary text into the XML tokenizer, the device-description parser and the SOAP
//! reply parser. Must never panic and must respect the size caps.

use libfuzzer_sys::fuzz_target;
use trs_portmap::{upnp, xml};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut n = 0usize;
    let _ = xml::walk(&text, |_| n += 1);
    assert!(n <= xml::MAX_EVENTS + 1);
    if let Ok(d) = upnp::parse_description(&text) {
        assert!(d.services.len() <= upnp::MAX_SERVICES);
        let _ = upnp::pick_service(&d);
    }
    for status in [200u16, 500, 404] {
        let _ = upnp::parse_reply(status, data);
    }
});
