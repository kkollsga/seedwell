#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_state::{Config, Document};

// Any text: parsing must not panic; anything accepted must render and parse back to itself.
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    if let Ok(p) = Config::parse_str(&text) {
        let out = p.value.render().expect("a validated config must render");
        let back = Config::parse_str(&out).expect("rendered config must parse");
        assert_eq!(back.value, p.value);
    }
});
