#![no_main]
//! HTTP response parsing, URL handling and the routing-tool output parsers.

use libfuzzer_sys::fuzz_target;
use trs_portmap::{gateway, http};

fuzz_target!(|data: &[u8]| {
    if let Ok(r) = http::parse_response(data, 64 * 1024) {
        assert!(r.body.len() <= 64 * 1024);
    }
    let _ = http::is_complete(data);
    let text = String::from_utf8_lossy(data);
    if let Some(base) = http::parse_url("http://192.168.1.1:5000/dev/desc.xml") {
        let _ = http::resolve(&base, &text);
    }
    if let Some(u) = http::parse_url(&text) {
        let _ = http::build_get(&u, "fuzz");
        let _ = u.host_header();
    }
    let _ = gateway::parse_bsd_route_get(&text);
    let _ = gateway::parse_linux_ip_route(&text);
});
