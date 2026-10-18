//! Keeping secrets out of everything an agent can read or the activity log keeps: passkeys in
//! tracker URLs and magnet links, query strings of direct-download URLs.

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

#[cfg(test)]
mod tests {
    use super::*;

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
