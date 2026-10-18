use std::net::SocketAddr;

use crate::error::MagnetError;
use crate::hash::{InfoHash, base32_decode, hex_decode, hex_encode};
use crate::torrent::{MAX_TRACKERS, MAX_WEB_SEEDS, has_scheme};
use crate::warning::Warning;

/// Longest magnet URI accepted (bytes). Magnets with dozens of trackers reach a
/// few KiB; 16 KiB leaves headroom while bounding work on pasted garbage.
pub const MAX_MAGNET_LEN: usize = 16 * 1024;
/// Cap on `so` ranges kept.
pub const MAX_SELECT_RANGES: usize = 1024;
/// Cap on `x.pe` peer hints kept.
pub const MAX_PEER_HINTS: usize = 64;

/// Inclusive file-index range from a BEP 53 `so` parameter. `end == u32::MAX`
/// means open-ended (`5-`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRange {
    /// First selected file index.
    pub start: u32,
    /// Last selected file index (inclusive).
    pub end: u32,
}

/// A parsed `magnet:` URI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Magnet {
    /// From `xt=urn:btih:` (v1) and/or `xt=urn:btmh:` (v2 multihash).
    pub info_hash: InfoHash,
    /// `dn`, UTF-8 (lossy), `+` read as a space.
    pub display_name: Option<String>,
    /// `tr` (repeatable), http(s)/udp only, deduplicated.
    pub trackers: Vec<String>,
    /// `ws` web seeds (BEP 19), http(s) only.
    pub web_seeds: Vec<String>,
    /// `xl` exact length in bytes.
    pub exact_length: Option<u64>,
    /// `so` file selection (BEP 53); empty means "all files".
    pub select_only: Vec<FileRange>,
    /// `x.pe` peer address hints (BEP 9).
    pub peers: Vec<SocketAddr>,
    /// Ignored or truncated parameters.
    pub warnings: Vec<Warning>,
}

impl Magnet {
    /// Parses `magnet:?xt=...` strictly: malformed hashes, escapes, `xl` and `so`
    /// are errors; unknown parameters and unusable `tr`/`ws` URLs are skipped.
    pub fn parse(uri: &str) -> Result<Magnet, MagnetError> {
        let uri = uri.trim();
        if uri.len() > MAX_MAGNET_LEN {
            return Err(MagnetError::TooLong);
        }
        const PREFIX: &str = "magnet:?";
        if uri.len() < PREFIX.len()
            || !uri.is_char_boundary(PREFIX.len())
            || !uri[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
        {
            return Err(MagnetError::NotMagnet);
        }
        // Fragments are never part of the parameters.
        let query = uri[PREFIX.len()..].split('#').next().unwrap_or("");

        let mut m = Magnet::default();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let Some((key, value)) = pair.split_once('=') else {
                continue;
            };
            // `xt.1`, `tr.2` etc. are the numbered form of repeated parameters.
            let key = match key.split_once('.') {
                Some((base, n)) if !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) => base,
                _ => key,
            }
            .to_ascii_lowercase();
            match key.as_str() {
                "xt" => m.add_xt(&percent_decode(value, false)?)?,
                "dn" => {
                    let raw = percent_decode(value, true)?;
                    m.display_name = Some(String::from_utf8_lossy(&raw).into_owned());
                }
                "tr" => m.add_url(value, true)?,
                "ws" => m.add_url(value, false)?,
                "xl" => {
                    m.exact_length = Some(
                        percent_decode_str(value)?
                            .parse()
                            .map_err(|_| MagnetError::InvalidParameter("xl"))?,
                    );
                }
                "so" => m.add_select(&percent_decode_str(value)?)?,
                "x.pe" => {
                    if let Ok(addr) = percent_decode_str(value).unwrap_or_default().parse()
                        && m.peers.len() < MAX_PEER_HINTS && !m.peers.contains(&addr) {
                            m.peers.push(addr);
                        }
                }
                _ => {}
            }
        }
        if m.info_hash.v1.is_none() && m.info_hash.v2.is_none() {
            return Err(MagnetError::MissingInfoHash);
        }
        Ok(m)
    }

    fn add_xt(&mut self, xt: &[u8]) -> Result<(), MagnetError> {
        let xt = core::str::from_utf8(xt).map_err(|_| MagnetError::InvalidInfoHash)?;
        let lower = |prefix: &str| {
            (xt.len() >= prefix.len()
                && xt.is_char_boundary(prefix.len())
                && xt[..prefix.len()].eq_ignore_ascii_case(prefix))
            .then(|| &xt[prefix.len()..])
        };
        if let Some(h) = lower("urn:btih:") {
            let h = hex_decode::<20>(h)
                .or_else(|| base32_decode::<20>(h))
                .ok_or(MagnetError::InvalidInfoHash)?;
            if self.info_hash.v1.is_some_and(|old| old != h) {
                return Err(MagnetError::ConflictingInfoHash);
            }
            self.info_hash.v1 = Some(h);
        } else if let Some(h) = lower("urn:btmh:") {
            // Multihash: 0x12 (sha2-256), 0x20 (32 bytes), then the digest.
            let mh = hex_decode::<34>(h).ok_or(MagnetError::InvalidInfoHash)?;
            if mh[..2] != [0x12, 0x20] {
                return Err(MagnetError::InvalidInfoHash);
            }
            let mut d = [0u8; 32];
            d.copy_from_slice(&mh[2..]);
            if self.info_hash.v2.is_some_and(|old| old != d) {
                return Err(MagnetError::ConflictingInfoHash);
            }
            self.info_hash.v2 = Some(d);
        }
        // Other `xt` URNs (e.g. urn:sha1 for plain files) are not torrents.
        Ok(())
    }

    fn add_url(&mut self, encoded: &str, tracker: bool) -> Result<(), MagnetError> {
        // `+` is *not* a space here: tracker URLs carry passkeys that may contain it.
        let raw = percent_decode(encoded, false)?;
        let Ok(url) = String::from_utf8(raw) else {
            self.warnings.push(Warning::IgnoredTracker(encoded.into()));
            return Ok(());
        };
        let url = url.trim().to_owned();
        let (ok, list, cap) = if tracker {
            (
                has_scheme(&url, &["http://", "https://", "udp://"]),
                &mut self.trackers,
                MAX_TRACKERS,
            )
        } else {
            (
                has_scheme(&url, &["http://", "https://"]),
                &mut self.web_seeds,
                MAX_WEB_SEEDS,
            )
        };
        if !ok {
            if !url.is_empty() && self.warnings.len() < 64 {
                self.warnings.push(if tracker {
                    Warning::IgnoredTracker(url.into())
                } else {
                    Warning::IgnoredWebSeed(url.into())
                });
            }
        } else if list.len() >= cap {
            if !self.warnings.contains(&Warning::TooManyUrls) {
                self.warnings.push(Warning::TooManyUrls);
            }
        } else if !list.contains(&url) {
            list.push(url);
        }
        Ok(())
    }

    fn add_select(&mut self, so: &str) -> Result<(), MagnetError> {
        let bad = || MagnetError::InvalidParameter("so");
        for part in so.split(',').filter(|p| !p.is_empty()) {
            let (start, end) = match part.split_once('-') {
                None => {
                    let n: u32 = part.parse().map_err(|_| bad())?;
                    (n, n)
                }
                Some((a, "")) => (a.parse().map_err(|_| bad())?, u32::MAX),
                Some((a, b)) => (a.parse().map_err(|_| bad())?, b.parse().map_err(|_| bad())?),
            };
            if start > end {
                return Err(bad());
            }
            if self.select_only.len() >= MAX_SELECT_RANGES {
                return Err(bad());
            }
            self.select_only.push(FileRange { start, end });
        }
        Ok(())
    }

    /// Renders a magnet URI. Trackers are omitted unless `include_trackers`, so a
    /// caller can share a torrent without leaking a private tracker's passkey.
    pub fn to_uri(&self, include_trackers: bool) -> String {
        let mut s = String::from("magnet:?");
        let mut first = true;
        let mut push = |s: &mut String, k: &str, v: &str| {
            if !first {
                s.push('&');
            }
            first = false;
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        };
        if let Some(h) = &self.info_hash.v1 {
            push(&mut s, "xt", &format!("urn:btih:{}", hex_encode(h)));
        }
        if let Some(h) = &self.info_hash.v2 {
            push(&mut s, "xt", &format!("urn:btmh:1220{}", hex_encode(h)));
        }
        if let Some(dn) = &self.display_name {
            push(&mut s, "dn", &percent_encode(dn.as_bytes()));
        }
        if include_trackers {
            for t in &self.trackers {
                push(&mut s, "tr", &percent_encode(t.as_bytes()));
            }
        }
        for w in &self.web_seeds {
            push(&mut s, "ws", &percent_encode(w.as_bytes()));
        }
        if let Some(xl) = self.exact_length {
            push(&mut s, "xl", &xl.to_string());
        }
        s
    }
}

/// Decodes `%XX` escapes; with `plus_is_space`, `+` becomes a space (form
/// encoding, which is how `dn` is usually written).
fn percent_decode(s: &str, plus_is_space: bool) -> Result<Vec<u8>, MagnetError> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = b
                    .get(i + 1..i + 3)
                    .ok_or(MagnetError::InvalidPercentEncoding)?;
                let hex =
                    core::str::from_utf8(hex).map_err(|_| MagnetError::InvalidPercentEncoding)?;
                let [byte] = hex_decode::<1>(hex).ok_or(MagnetError::InvalidPercentEncoding)?;
                out.push(byte);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

fn percent_decode_str(s: &str) -> Result<String, MagnetError> {
    String::from_utf8(percent_decode(s, false)?).map_err(|_| MagnetError::InvalidPercentEncoding)
}

/// Encodes everything except RFC 3986 unreserved characters.
fn percent_encode(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len());
    for &c in b {
        if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
            s.push(char::from(c));
        } else {
            s.push_str(&format!("%{c:02X}"));
        }
    }
    s
}
