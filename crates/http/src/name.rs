//! File-name derivation: `Content-Disposition` (RFC 6266 / RFC 5987), URL
//! path fallback, macOS-safe sanitization, and collision suffixes.
//!
//! All inputs come from the network; nothing here panics or indexes blindly.

/// Longest file name we create, in UTF-8 bytes. APFS allows 255; we leave room
/// for `.part`, `.part.json` and ` (N)` suffixes.
pub const MAX_NAME_BYTES: usize = 200;

const FALLBACK: &str = "download";
/// Header values longer than this are ignored.
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// Make an arbitrary string safe as a single file-name component on macOS.
///
/// Path separators (and `:`, which Finder shows as `/`), control characters and
/// bidi overrides (extension spoofing) are removed or replaced; leading dots
/// (hidden files, `..`) and trailing dots/spaces are stripped; the result is
/// capped at [`MAX_NAME_BYTES`] keeping a short extension. Never empty, never
/// `.` or `..`.
pub fn sanitize_file_name(raw: &str) -> String {
    let mut s = String::with_capacity(raw.len().min(1024));
    // Bounded work on hostile input (headers are capped far below this anyway).
    for c in raw.chars().take(64 * 1024) {
        match c {
            '/' | '\\' | ':' => s.push('_'),
            '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' => {}
            c if c.is_control() => {}
            c => s.push(c),
        }
    }
    let trimmed = s
        .trim_matches(|c: char| c.is_whitespace())
        .trim_start_matches('.')
        .trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let mut out = truncate_keep_ext(trimmed, MAX_NAME_BYTES);
    if out.is_empty() {
        out = FALLBACK.to_owned();
    }
    out
}

/// Split `name` into stem and extension (including the dot). Known double
/// extensions (`.tar.gz`) stay together; "extensions" longer than 16 bytes or
/// a leading dot are not extensions.
fn split_ext(name: &str) -> (&str, &str) {
    const DOUBLE: [&str; 5] = [".tar.gz", ".tar.bz2", ".tar.xz", ".tar.zst", ".tar.lz"];
    let lower = name.to_ascii_lowercase();
    for d in DOUBLE {
        if lower.ends_with(d) && name.len() > d.len() {
            let at = name.len() - d.len();
            if name.is_char_boundary(at) {
                return name.split_at(at);
            }
        }
    }
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i <= 16 => name.split_at(i),
        _ => (name, ""),
    }
}

fn truncate_keep_ext(name: &str, max: usize) -> String {
    if name.len() <= max {
        return name.to_owned();
    }
    let (stem, ext) = split_ext(name);
    let budget = max.saturating_sub(ext.len());
    let mut end = budget.min(stem.len());
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    let stem = stem[..end].trim_end_matches(|c: char| c == '.' || c.is_whitespace());
    let mut out = String::with_capacity(max);
    out.push_str(stem);
    out.push_str(ext);
    if out.len() > max || stem.is_empty() {
        // Pathological: huge extension. Hard cut on a char boundary.
        let mut end = max.min(name.len());
        while end > 0 && !name.is_char_boundary(end) {
            end -= 1;
        }
        return name[..end].to_owned();
    }
    out
}

/// Pick a name that `exists` rejects: `name`, then `stem (2).ext`, `(3)`, ...
pub fn unique_name(name: &str, mut exists: impl FnMut(&str) -> bool) -> String {
    if !exists(name) {
        return name.to_owned();
    }
    let (stem, ext) = split_ext(name);
    for n in 2..10_000u32 {
        let cand = format!("{stem} ({n}){ext}");
        if !exists(&cand) {
            return cand;
        }
    }
    format!("{stem} ({}){ext}", crate::util::random_u64())
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Percent-decode to raw bytes (malformed escapes are kept literally).
fn percent_decode(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%' && i + 2 < s.len() {
            if let (Some(h), Some(l)) = (hex(s[i + 1]), hex(s[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// Last non-empty path segment of `url`, percent-decoded. `None` if the path
/// is empty or ends in `/`.
pub fn file_name_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let path_start = rest.find('/')?;
    let path = &rest[path_start..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    if path.ends_with('/') {
        return None;
    }
    let seg = path.rsplit('/').next().unwrap_or("");
    if seg.is_empty() {
        return None;
    }
    let decoded = percent_decode(seg.as_bytes());
    let s = String::from_utf8_lossy(&decoded).into_owned();
    let s = sanitize_file_name(&s);
    (s != FALLBACK || seg == FALLBACK).then_some(s)
}

/// Decode header bytes: UTF-8 if valid, else Latin-1 (what old servers send).
fn decode_header(v: &[u8]) -> String {
    match std::str::from_utf8(v) {
        Ok(s) => s.to_owned(),
        Err(_) => v.iter().map(|&b| b as char).collect(),
    }
}

/// Parse an RFC 5987 `ext-value`: `charset'lang'pct-encoded`.
fn decode_ext_value(v: &str) -> Option<String> {
    let mut parts = v.splitn(3, '\'');
    let charset = parts.next()?.trim().to_ascii_lowercase();
    let _lang = parts.next()?;
    let data = parts.next()?;
    let bytes = percent_decode(data.as_bytes());
    match charset.as_str() {
        "utf-8" | "utf8" => String::from_utf8(bytes).ok(),
        "iso-8859-1" | "latin1" => Some(bytes.iter().map(|&b| b as char).collect()),
        _ => None,
    }
}

/// Extract the file name from a `Content-Disposition` value (RFC 6266).
///
/// `filename*` (RFC 5987, UTF-8 or ISO-8859-1) wins over `filename`. The
/// result is **sanitized**; `None` when the header carries no usable name.
pub fn parse_content_disposition(value: &[u8]) -> Option<String> {
    if value.len() > MAX_HEADER_BYTES {
        return None;
    }
    let text = decode_header(value);
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    // Skip the disposition type ("attachment", "inline", ...).
    while i < chars.len() && chars[i] != ';' {
        i += 1;
    }
    let mut plain: Option<String> = None;
    let mut ext: Option<String> = None;
    while i < chars.len() {
        // at ';' or end
        i += 1;
        while i < chars.len() && (chars[i].is_whitespace() || chars[i] == ';') {
            i += 1;
        }
        let ns = i;
        while i < chars.len() && chars[i] != '=' && chars[i] != ';' {
            i += 1;
        }
        let name: String = chars[ns..i].iter().collect::<String>().trim().to_ascii_lowercase();
        if i >= chars.len() || chars[i] != '=' {
            continue; // parameter without value
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut val = String::new();
        if i < chars.len() && chars[i] == '"' {
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                val.push(chars[i]);
                i += 1;
            }
            i += 1; // closing quote
            while i < chars.len() && chars[i] != ';' {
                i += 1; // junk after the quoted string
            }
        } else {
            let vs = i;
            while i < chars.len() && chars[i] != ';' {
                i += 1;
            }
            val = chars[vs..i].iter().collect::<String>().trim().to_owned();
        }
        match name.as_str() {
            "filename*" if ext.is_none() => {
                ext = decode_ext_value(&val).filter(|s| !sanitize_file_name(s).is_empty());
            }
            "filename" if plain.is_none() => plain = Some(val),
            _ => {}
        }
    }
    let chosen = ext.or(plain)?;
    // A name made only of separators/dots carries no information.
    let clean = sanitize_file_name(&chosen);
    let meaningful = chosen.chars().any(|c| c.is_alphanumeric());
    (meaningful && !clean.is_empty()).then_some(clean)
}

/// Final file name: `Content-Disposition`, else the redirect target's last
/// path segment (unless the original URL's segment looks more like a file
/// name), else `download`.
pub fn derive_file_name(content_disposition: Option<&[u8]>, original_url: &str, final_url: &str) -> String {
    if let Some(n) = content_disposition.and_then(parse_content_disposition) {
        return n;
    }
    let orig = file_name_from_url(original_url);
    let fin = file_name_from_url(final_url);
    let has_ext = |n: &str| split_ext(n).1.len() > 1;
    match (orig, fin) {
        (Some(o), Some(f)) => {
            if !has_ext(&f) && has_ext(&o) {
                o
            } else {
                f
            }
        }
        (Some(n), None) | (None, Some(n)) => n,
        (None, None) => FALLBACK.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_basics() {
        assert_eq!(sanitize_file_name("a/b\\c:d.iso"), "a_b_c_d.iso");
        assert_eq!(sanitize_file_name(".."), FALLBACK);
        assert_eq!(sanitize_file_name("../../etc/passwd"), "_.._etc_passwd");
        assert_eq!(sanitize_file_name("."), FALLBACK);
        assert_eq!(sanitize_file_name(""), FALLBACK);
        assert_eq!(sanitize_file_name("   "), FALLBACK);
        assert_eq!(sanitize_file_name("..hidden"), "hidden");
        assert_eq!(sanitize_file_name("a\0b\x07c"), "abc");
        assert_eq!(sanitize_file_name("name. . "), "name");
        assert_eq!(sanitize_file_name("evil\u{202E}fdp.exe"), "evilfdp.exe");
        assert_eq!(sanitize_file_name("ünï.txt"), "ünï.txt");
    }

    #[test]
    fn sanitize_length_cap_keeps_extension_and_boundaries() {
        let long = format!("{}.tar.gz", "é".repeat(500));
        let s = sanitize_file_name(&long);
        assert!(s.len() <= MAX_NAME_BYTES);
        assert!(s.ends_with(".tar.gz"));
        let long2 = "x".repeat(1000);
        assert_eq!(sanitize_file_name(&long2).len(), MAX_NAME_BYTES);
        let long3 = format!("a.{}", "y".repeat(1000));
        assert!(sanitize_file_name(&long3).len() <= MAX_NAME_BYTES);
    }

    #[test]
    fn collision_suffix() {
        let taken = ["a.iso", "a (2).iso", "b.tar.gz"];
        let ex = |n: &str| taken.contains(&n);
        assert_eq!(unique_name("a.iso", ex), "a (3).iso");
        assert_eq!(unique_name("b.tar.gz", ex), "b (2).tar.gz");
        assert_eq!(unique_name("c.bin", ex), "c.bin");
        assert_eq!(unique_name("noext", |n| n == "noext"), "noext (2)");
    }

    #[test]
    fn url_segment() {
        assert_eq!(file_name_from_url("https://h/a/b/My%20File%C3%A9.iso?x=1#f").unwrap(), "My Fileé.iso");
        assert_eq!(file_name_from_url("https://h/").as_deref(), None);
        assert_eq!(file_name_from_url("https://h").as_deref(), None);
        assert_eq!(file_name_from_url("https://h/dir/").as_deref(), None);
        assert_eq!(file_name_from_url("https://h/%2e%2e%2f%2e%2e%2fpasswd").unwrap(), "_.._passwd");
        assert_eq!(file_name_from_url("https://h/bad%zz%").unwrap(), "bad%zz%");
        assert_eq!(file_name_from_url("https://h/%").unwrap(), "%");
    }

    #[test]
    fn content_disposition_variants() {
        let p = |s: &str| parse_content_disposition(s.as_bytes());
        assert_eq!(p("attachment; filename=\"a.iso\"").as_deref(), Some("a.iso"));
        assert_eq!(p("attachment; filename=a.iso").as_deref(), Some("a.iso"));
        assert_eq!(p("inline; filename = \"x y.bin\" ; size=3").as_deref(), Some("x y.bin"));
        assert_eq!(p("attachment; filename=\"a;b.iso\"").as_deref(), Some("a;b.iso"));
        assert_eq!(p("attachment; filename=\"a\\\"b.iso\"").as_deref(), Some("a\"b.iso"));
        assert_eq!(p("ATTACHMENT; FILENAME=\"Up.ISO\"").as_deref(), Some("Up.ISO"));
        // RFC 5987 wins, either order
        assert_eq!(
            p("attachment; filename=\"fallback.txt\"; filename*=UTF-8''%E2%82%AC%20rates.txt").as_deref(),
            Some("€ rates.txt")
        );
        assert_eq!(
            p("attachment; filename*=UTF-8''na%C3%AFve.txt; filename=\"naive.txt\"").as_deref(),
            Some("naïve.txt")
        );
        assert_eq!(p("attachment; filename*=iso-8859-1'en'%A3%20rates").as_deref(), Some("£ rates"));
        // unknown charset falls back to plain
        assert_eq!(p("attachment; filename*=klingon''x; filename=\"ok.bin\"").as_deref(), Some("ok.bin"));
        // traversal attempts
        assert_eq!(p("attachment; filename=\"../../etc/passwd\"").as_deref(), Some("_.._etc_passwd"));
        assert_eq!(p("attachment; filename*=UTF-8''..%2F..%2Fx").as_deref(), Some("_.._x"));
        // nothing usable
        assert_eq!(p("attachment"), None);
        assert_eq!(p("attachment; filename=\"\""), None);
        assert_eq!(p("attachment; filename=\"..\""), None);
        assert_eq!(p("attachment; filename"), None);
        assert_eq!(p(""), None);
        // raw UTF-8 and Latin-1 bytes
        assert_eq!(parse_content_disposition("attachment; filename=\"é.txt\"".as_bytes()).as_deref(), Some("é.txt"));
        assert_eq!(parse_content_disposition(b"attachment; filename=\"\xe9.txt\"").as_deref(), Some("é.txt"));
    }

    #[test]
    fn derive_prefers_header_then_final_url() {
        let cd = b"attachment; filename=\"h.iso\"";
        assert_eq!(derive_file_name(Some(cd), "https://a/x", "https://b/y"), "h.iso");
        assert_eq!(derive_file_name(None, "https://a/x.iso", "https://b/blob/4f3a"), "x.iso");
        assert_eq!(derive_file_name(None, "https://a/dl", "https://b/ubuntu.iso"), "ubuntu.iso");
        assert_eq!(derive_file_name(None, "https://a/", "https://b/"), "download");
    }

    /// Cheap in-tree stand-in for the cargo-fuzz target: mutate real-world
    /// headers at random and check every output is a harmless single component.
    #[test]
    fn mutated_headers_never_break_the_invariants() {
        let seeds: [&[u8]; 6] = [
            b"attachment; filename=\"a.iso\"",
            b"attachment; filename*=UTF-8''%E2%82%AC%20rates.txt; filename=\"x\"",
            b"inline; filename=../../etc/passwd",
            b"attachment; filename=\"a\\\"b;c.bin\"; size=3",
            b"attachment; filename*=iso-8859-1'en'%A3%2f..%2f",
            b"form-data; name=\"f\"; filename=\"\xe9\xff.txt\"",
        ];
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let mut v = seeds[(next() % seeds.len() as u64) as usize].to_vec();
            for _ in 0..(next() % 6) {
                match next() % 4 {
                    0 if !v.is_empty() => {
                        let i = (next() % v.len() as u64) as usize;
                        v[i] = next() as u8;
                    }
                    1 => v.insert((next() % (v.len() as u64 + 1)) as usize, next() as u8),
                    2 if !v.is_empty() => {
                        v.remove((next() % v.len() as u64) as usize);
                    }
                    _ => v.truncate((next() % (v.len() as u64 + 1)) as usize),
                }
            }
            if let Some(n) = parse_content_disposition(&v) {
                assert!(!n.is_empty() && n.len() <= MAX_NAME_BYTES, "{n:?}");
                assert!(n != "." && n != ".." && !n.starts_with('.'), "{n:?}");
                assert!(!n.contains(['/', '\\', ':', '\0']) && !n.chars().any(char::is_control), "{n:?}");
            }
        }
    }
}
