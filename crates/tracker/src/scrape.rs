//! BEP 48 scrape: URL derivation and response parsing (sans-I/O).
//!
//! Scraping is never automatic: it goes through the same `HostGate` as
//! announces, at most once per `scrape_min_gap` per (torrent, tracker).

use trs_bencode::{DecodeLimits, Value};

use crate::response::{AnnounceError, MAX_BODY, ParseError};
use crate::types::InfoHash20;
use crate::url::{TrackerUrl, percent_encode_into};

/// Most info-hashes in one scrape (the UDP packet limit is 74).
pub const MAX_SCRAPE_HASHES: usize = 74;

/// Per-torrent numbers from a scrape.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScrapeEntry {
    /// Seeders.
    pub complete: u64,
    /// Completed downloads.
    pub downloaded: u64,
    /// Leechers.
    pub incomplete: u64,
}

/// Derive the scrape URL from an announce URL: the last path component must
/// start with `announce`, which is replaced by `scrape`; path prefix and query
/// (passkeys) are kept. `None` when the tracker does not support scraping by
/// this convention.
pub fn scrape_url(base: &TrackerUrl, hashes: &[InfoHash20]) -> Option<String> {
    let raw = base.raw();
    let (before_query, query) = match raw.split_once('?') {
        Some((a, b)) => (a, Some(b)),
        None => (raw, None),
    };
    let slash = before_query.rfind('/')?;
    let (prefix, last) = before_query.split_at(slash + 1);
    // `prefix` must include the path, not just "http://host/"
    if !last.starts_with("announce") || prefix.ends_with("://") {
        return None;
    }
    let mut out = String::with_capacity(raw.len() + 24 * hashes.len() + 16);
    out.push_str(prefix);
    out.push_str("scrape");
    out.push_str(&last["announce".len()..]);
    let mut sep = if query.is_some() { '&' } else { '?' };
    if let Some(q) = query {
        out.push('?');
        out.push_str(q);
        if q.is_empty() || q.ends_with('&') {
            sep = '\0';
        }
    }
    for h in hashes.iter().take(MAX_SCRAPE_HASHES) {
        if sep != '\0' {
            out.push(sep);
        }
        sep = '&';
        out.push_str("info_hash=");
        percent_encode_into(&mut out, h);
    }
    Some(out)
}

/// Parse a scrape response body.
pub fn parse_scrape_response(body: &[u8]) -> Result<Vec<(InfoHash20, ScrapeEntry)>, AnnounceError> {
    if body.len() > MAX_BODY {
        return Err(AnnounceError::Garbage(ParseError::TooLarge));
    }
    let limits = DecodeLimits {
        max_items: 100_000,
        max_bytes: MAX_BODY,
    };
    let (v, _) = trs_bencode::decode_prefix_with_limits(body, limits)
        .map_err(|_| AnnounceError::Garbage(ParseError::NotBencode))?;
    if v.as_dict().is_none() {
        return Err(AnnounceError::Garbage(ParseError::NotDict));
    }
    if let Some(reason) = v.get(b"failure reason").and_then(Value::as_bytes) {
        return Err(AnnounceError::Rejected {
            reason: crate::response::sanitize_text(reason),
            retry_in: None,
        });
    }
    let mut out = Vec::new();
    if let Some(files) = v.get(b"files").and_then(Value::as_dict) {
        for (k, e) in files.iter().take(MAX_SCRAPE_HASHES) {
            let Ok(h) = InfoHash20::try_from(*k) else {
                continue;
            };
            let n = |key: &[u8]| {
                e.get(key)
                    .and_then(Value::as_int)
                    .and_then(|i| u64::try_from(i).ok())
                    .unwrap_or(0)
            };
            out.push((
                h,
                ScrapeEntry {
                    complete: n(b"complete"),
                    downloaded: n(b"downloaded"),
                    incomplete: n(b"incomplete"),
                },
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> TrackerUrl {
        TrackerUrl::parse(s).unwrap()
    }

    #[test]
    fn scrape_url_derivation() {
        let h = [0xAB; 20];
        let enc = "%AB".repeat(20);
        assert_eq!(
            scrape_url(&u("http://t.example/announce"), &[h]).unwrap(),
            format!("http://t.example/scrape?info_hash={enc}")
        );
        assert_eq!(
            scrape_url(&u("http://t.example/PASS/announce.php?passkey=k"), &[h]).unwrap(),
            format!("http://t.example/PASS/scrape.php?passkey=k&info_hash={enc}")
        );
        assert_eq!(
            scrape_url(&u("http://t.example/x/announce?"), &[h]).unwrap(),
            format!("http://t.example/x/scrape?info_hash={enc}")
        );
        assert_eq!(scrape_url(&u("http://t.example/"), &[h]), None);
        assert_eq!(scrape_url(&u("http://t.example/tracker"), &[h]), None);
        assert_eq!(scrape_url(&u("http://t.example"), &[h]), None);
    }

    #[test]
    fn scrape_response() {
        let mut body = b"d5:filesd20:".to_vec();
        body.extend([7u8; 20]);
        body.extend(b"d8:completei5e10:downloadedi50e10:incompletei3eeee");
        let r = parse_scrape_response(&body).unwrap();
        assert_eq!(
            r,
            vec![(
                [7; 20],
                ScrapeEntry {
                    complete: 5,
                    downloaded: 50,
                    incomplete: 3
                }
            )]
        );
        assert!(parse_scrape_response(b"nope").is_err());
        assert!(matches!(
            parse_scrape_response(b"d14:failure reason2:noe"),
            Err(AnnounceError::Rejected { .. })
        ));
        assert_eq!(parse_scrape_response(b"de").unwrap(), vec![]);
        // a key that is not 20 bytes is skipped
        assert_eq!(
            parse_scrape_response(b"d5:filesd3:abcdeee").unwrap(),
            vec![]
        );
    }
}
