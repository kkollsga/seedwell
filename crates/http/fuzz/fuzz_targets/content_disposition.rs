#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_http::name::{
    MAX_NAME_BYTES, derive_file_name, file_name_from_url, parse_content_disposition, sanitize_file_name,
    unique_name,
};

/// A sanitized name is always a single, harmless path component.
fn check(n: &str) {
    assert!(!n.is_empty() && n.len() <= MAX_NAME_BYTES);
    assert!(n != "." && n != "..");
    assert!(!n.contains(['/', '\\', ':', '\0']));
    assert!(!n.starts_with('.'));
    assert!(!n.chars().any(char::is_control));
}

fuzz_target!(|data: &[u8]| {
    if let Some(n) = parse_content_disposition(data) {
        check(&n);
    }
    let s = String::from_utf8_lossy(data);
    check(&sanitize_file_name(&s));
    if let Some(n) = file_name_from_url(&s) {
        check(&n);
    }
    check(&derive_file_name(Some(data), &s, "http://h/x.bin"));
    // Collision suffixes may add a few bytes but never break the rules.
    let u = unique_name(&sanitize_file_name(&s), |n| n.len() % 2 == 0);
    assert!(u.len() <= MAX_NAME_BYTES + 32 && !u.contains(['/', '\0']) && !u.starts_with('.'));
});
