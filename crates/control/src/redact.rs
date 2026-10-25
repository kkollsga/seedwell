//! Keeping secrets out of everything an agent can read or the activity log keeps: passkeys in
//! tracker URLs and magnet links, query strings of direct-download URLs, folder-sync keys.

use trs_state::url::redact_url;

/// Replace every `scheme://...` URL inside free text by its redacted form. Tracker errors and
/// log lines sometimes quote the announce URL; this makes sure a passkey cannot ride along.
pub fn scrub_urls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(pos) = find_scheme(rest) {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '`'))
            .unwrap_or(tail.len());
        // Trailing punctuation belongs to the sentence, not the URL.
        let url = tail[..end].trim_end_matches(['.', ',', ';', ':', '!', '?']);
        out.push_str(&redact_url(url));
        out.push_str(&tail[url.len()..end]);
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn find_scheme(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    ["https://", "http://", "udp://", "wss://", "ws://"]
        .iter()
        .filter_map(|p| lower.find(p))
        .min()
}

/// A magnet link reduced to what identifies the content: `xt` and `dn`. `tr` (trackers, which
/// may hold passkeys), `ws`, `xs` and everything else are dropped.
pub fn redact_magnet(uri: &str) -> String {
    let Some(q) = uri.strip_prefix("magnet:?").or_else(|| uri.strip_prefix("magnet:")) else {
        return "<not a magnet link>".into();
    };
    let mut keep = Vec::new();
    let mut dropped = false;
    for pair in q.split('&') {
        let key = pair.split('=').next().unwrap_or("");
        match key {
            "xt" | "dn" => keep.push(pair),
            "" => {}
            _ => dropped = true,
        }
    }
    let mut s = format!("magnet:?{}", keep.join("&"));
    if dropped {
        s.push_str("&...");
    }
    crate::proto::clip(&s, 300)
}

/// Redact whatever a user typed as an add source: magnet, URL or path (paths are kept).
pub fn redact_source(src: &str) -> String {
    if src.get(..7).is_some_and(|h| h.eq_ignore_ascii_case("magnet:")) {
        redact_magnet(src)
    } else if src.contains("://") {
        crate::proto::clip(&scrub_urls(src), 300)
    } else {
        crate::proto::clip(src, 300)
    }
}

/// Replace every folder-sync key (`SWW-…`, `SWR-…`, `SWE-…`, any case) and every `seedwell://sync/…`
/// link inside free text by a mask. Holding a key is the permission to a folder (SPEC 7G.1): keys are
/// returned by `sync.share` only and never reach the activity log or any other text.
pub fn scrub_keys(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let b = text.as_bytes();
    let key_char = |c: u8| c.is_ascii_alphanumeric() || c == b'-' || c == b'=';
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        let rest = &lower[i..];
        let boundary = i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'-');
        let (mask, end) = if boundary && rest.starts_with("seedwell://sync/") {
            let end = text[i..].find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | ')' | ']' | '`')).map_or(text.len(), |e| i + e);
            ("seedwell://sync/***", end)
        } else if boundary && (rest.starts_with("sww-") || rest.starts_with("swr-") || rest.starts_with("swe-")) && b.get(i + 4).is_some_and(|c| c.is_ascii_alphanumeric()) {
            let mut end = i + 4;
            while end < b.len() && key_char(b[end]) {
                end += 1;
            }
            (&text[i..i + 4], end)
        } else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        out.push_str(&text[copied..i]);
        out.push_str(mask);
        if !mask.ends_with("***") {
            out.push_str("***");
        }
        i = end;
        copied = end;
    }
    out.push_str(&text[copied..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_keys_and_links_are_masked() {
        let t = scrub_keys("join SWW-ABCDEFGH-IJKLMNOP2345 now, or swr-qrstuvwx234567 (or <seedwell://sync/SWR-ABC?name=My%20Docs>).");
        assert!(!t.contains("ABCDEFGH") && !t.contains("qrstuvwx") && !t.contains("ABC?name"), "{t}");
        assert_eq!(t, "join SWW-*** now, or swr-*** (or <seedwell://sync/***>).");
        assert_eq!(scrub_keys("nothing secret: sww- and answer-sww-x"), "nothing secret: sww- and answer-sww-x");
        assert_eq!(scrub_keys("åäö SWE-AAAA"), "åäö SWE-***");
    }

    #[test]
    fn urls_in_text_lose_secrets() {
        let t = scrub_urls("announce to https://t.example.org/a/0123456789abcdef0123456789abcdef/announce?passkey=SECRET failed.");
        assert!(!t.contains("SECRET") && !t.contains("0123456789abcdef"), "{t}");
        assert!(t.ends_with("?*** failed.") || t.contains("?*** failed."), "{t}");
        assert_eq!(scrub_urls("no urls here"), "no urls here");
        assert_eq!(scrub_urls("udp://tracker.org:6969/announce, ok"), "udp://tracker.org:6969/announce, ok");
        // Unparseable stays out of the text.
        assert!(!scrub_urls("see http://[bad?passkey=zzz").contains("zzz"));
    }

    #[test]
    fn magnets_keep_only_identity() {
        let m = redact_magnet("magnet:?xt=urn:btih:abcdef&dn=Some+Name&tr=https%3A%2F%2Ft.org%2Fannounce%3Fpasskey%3DSECRET&ws=http://x");
        assert!(m.contains("xt=urn:btih:abcdef") && m.contains("dn=Some+Name"));
        assert!(!m.contains("SECRET") && !m.contains("tr="), "{m}");
        assert_eq!(redact_magnet("garbage"), "<not a magnet link>");
    }

    #[test]
    fn sources() {
        assert!(!redact_source("https://dl.example.com/f.iso?token=SECRET").contains("SECRET"));
        assert_eq!(redact_source("/tmp/a.torrent"), "/tmp/a.torrent");
        assert!(!redact_source("MAGNET:?xt=urn:btih:ab&tr=http://t/announce?passkey=SECRET").contains("SECRET"));
    }
}
