//! `.torrent` rewriting: swap tracker URLs for proxy URLs while keeping the
//! `info` dictionary (and every other entry) byte-for-byte identical.

use crate::bencode::{self, RawEntry, Value};
use crate::record::redact_url;
use anyhow::{Context, Result, bail};
use sha1::{Digest, Sha1};
use std::path::PathBuf;

/// Arguments for the `rewrite` subcommand.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Input .torrent file.
    pub input: PathBuf,
    /// Proxy base URL, e.g. http://127.0.0.1:7070.
    #[arg(long)]
    pub proxy: String,
    /// Base id for the tracker mapping(s).
    #[arg(long)]
    pub id: String,
    /// Output .torrent file.
    #[arg(short = 'o', long)]
    pub output: PathBuf,
    /// Also drop `url-list` / `httpseeds` (web seeds), so a local experiment cannot pull data
    /// from the Internet. The info-hash is unaffected. Off by default: nothing else is removed.
    #[arg(long)]
    pub strip_webseeds: bool,
}

/// Result of a rewrite.
#[derive(Debug)]
pub struct Rewritten {
    /// New torrent file bytes.
    pub bytes: Vec<u8>,
    /// `(id, upstream_url)` pairs to pass to `proxy --map`.
    pub maps: Vec<(String, String)>,
    /// Trackers that cannot be proxied (non-HTTP) and were removed.
    pub dropped: Vec<String>,
    /// SHA-1 of the input `info` dictionary.
    pub hash_in: [u8; 20],
    /// SHA-1 of the output `info` dictionary.
    pub hash_out: [u8; 20],
}

/// SHA-1 of the raw `info` value of a torrent.
pub fn info_hash(torrent: &[u8]) -> Result<[u8; 20]> {
    let entries = bencode::split_top_dict(torrent).map_err(|e| anyhow::anyhow!("{e}"))?;
    let info = entries.iter().find(|e| e.key == b"info").context("torrent has no info dictionary")?;
    let mut h = Sha1::new();
    h.update(&torrent[info.span.clone()]);
    Ok(h.finalize().into())
}

fn is_http(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Rewrite the tracker URLs of `input`.
pub fn rewrite_bytes(input: &[u8], proxy: &str, id: &str, strip_webseeds: bool) -> Result<Rewritten> {
    if !is_http(proxy) {
        bail!("--proxy must be an http:// URL");
    }
    let proxy = proxy.trim_end_matches('/');
    let entries: Vec<RawEntry> = bencode::split_top_dict(input).map_err(|e| anyhow::anyhow!("{e}"))?;
    let find = |k: &[u8]| entries.iter().find(|e| e.key == k);

    // Original tracker set in order of first appearance: announce, then tiers.
    let announce = match find(b"announce") {
        Some(e) => bencode::decode(&input[e.span.clone()]).ok().and_then(|v| v.as_bytes().map(|b| String::from_utf8_lossy(b).into_owned())),
        None => None,
    };
    let tiers: Vec<Vec<String>> = match find(b"announce-list") {
        Some(e) => {
            let v = bencode::decode(&input[e.span.clone()]).map_err(|e| anyhow::anyhow!("announce-list: {e}"))?;
            v.as_list()
                .context("announce-list is not a list")?
                .iter()
                .map(|t| t.as_list().unwrap_or(&[]).iter().filter_map(|u| u.as_bytes().map(|b| String::from_utf8_lossy(b).into_owned())).collect())
                .collect()
        }
        None => Vec::new(),
    };
    if announce.is_none() && tiers.is_empty() {
        bail!("torrent has no announce or announce-list (trackerless): nothing to rewrite");
    }

    let mut unique: Vec<String> = Vec::new();
    let mut dropped = Vec::new();
    for u in announce.iter().chain(tiers.iter().flatten()) {
        if unique.contains(u) || dropped.contains(u) {
            continue;
        }
        if is_http(u) {
            unique.push(u.clone());
        } else {
            dropped.push(u.clone());
        }
    }
    if unique.is_empty() {
        bail!("no HTTP(S) trackers in this torrent; only HTTP(S) can be proxied");
    }
    let id_for = |n: usize| if unique.len() == 1 { id.to_owned() } else { format!("{id}-{}", n + 1) };
    let proxied = |u: &str| -> Option<String> {
        unique.iter().position(|x| x == u).map(|n| format!("{proxy}/t/{}/announce", id_for(n)))
    };
    let maps: Vec<(String, String)> = unique.iter().enumerate().map(|(n, u)| (id_for(n), u.clone())).collect();

    let new_announce = announce.as_deref().and_then(proxied).or_else(|| unique.first().and_then(|u| proxied(u))).context("no announce")?;
    let new_tiers: Vec<Value> = tiers
        .iter()
        .map(|t| t.iter().filter_map(|u| proxied(u)).map(|u| Value::Bytes(u.into_bytes())).collect::<Vec<_>>())
        .filter(|t| !t.is_empty())
        .map(Value::List)
        .collect();

    // Rebuild: copy every entry's raw bytes except the two tracker keys.
    let mut out = vec![b'd'];
    for e in &entries {
        if strip_webseeds && matches!(e.key.as_slice(), b"url-list" | b"httpseeds") {
            continue;
        }
        bencode::encode_bytes(&e.key, &mut out);
        match e.key.as_slice() {
            b"announce" => bencode::encode_bytes(new_announce.as_bytes(), &mut out),
            b"announce-list" => out.extend_from_slice(&Value::List(new_tiers.clone()).encode()),
            _ => out.extend_from_slice(&input[e.span.clone()]),
        }
    }
    // A torrent with only `announce-list` keeps it that way; one without
    // `announce` is not given one (we add nothing).
    out.push(b'e');

    let hash_in = info_hash(input)?;
    let hash_out = info_hash(&out)?;
    if hash_in != hash_out {
        bail!("internal error: info-hash changed during rewrite");
    }
    Ok(Rewritten { bytes: out, maps, dropped, hash_in, hash_out })
}

/// CLI entry point.
pub fn run(a: Args) -> Result<()> {
    let input = std::fs::read(&a.input).with_context(|| format!("reading {}", a.input.display()))?;
    let r = rewrite_bytes(&input, &a.proxy, &a.id, a.strip_webseeds)?;
    std::fs::write(&a.output, &r.bytes).with_context(|| format!("writing {}", a.output.display()))?;
    // Re-read from disk: verify what is actually on disk, not what we meant to write.
    let on_disk = std::fs::read(&a.output)?;
    let h_out = info_hash(&on_disk)?;
    let h_in = info_hash(&input)?;
    if h_in != h_out || r.hash_in != r.hash_out {
        let _ = std::fs::remove_file(&a.output);
        bail!("info-hash mismatch: input {} output {}", crate::record::hex(&h_in), crate::record::hex(&h_out));
    }
    println!("info-hash input : {}", crate::record::hex(&h_in));
    println!("info-hash output: {}  (identical: OK)", crate::record::hex(&h_out));
    println!("wrote {}", a.output.display());
    for d in &r.dropped {
        println!("removed non-HTTP tracker (cannot be proxied): {}", redact_url(d));
    }
    println!("\nUse with `tracker-probe proxy`:");
    for (id, url) in &r.maps {
        println!("  --map {id}={url}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(with_list: bool) -> Vec<u8> {
        let mut v = b"d8:announce31:http://tracker.example/announce".to_vec();
        if with_list {
            v.extend_from_slice(
                b"13:announce-listll31:http://tracker.example/announceel25:udp://t2.example:1337/annel27:https://t3.example/announceee",
            );
        }
        v.extend_from_slice(b"7:comment3:hi!4:infod6:lengthi5e4:name1:x12:piece lengthi16384e6:pieces20:AAAAAAAAAAAAAAAAAAAA7:privatei1ee5:zzzzzi7ee");
        v
    }

    #[test]
    fn info_hash_equal_and_other_keys_untouched() {
        let src = sample(false);
        let r = rewrite_bytes(&src, "http://127.0.0.1:7070", "deb", false).unwrap();
        assert_eq!(r.hash_in, r.hash_out);
        assert_eq!(r.maps, vec![("deb".to_owned(), "http://tracker.example/announce".to_owned())]);
        let v = bencode::decode(&r.bytes).unwrap();
        assert_eq!(v.get("announce").and_then(Value::as_bytes), Some(&b"http://127.0.0.1:7070/t/deb/announce"[..]));
        assert_eq!(v.get("comment").and_then(Value::as_bytes), Some(&b"hi!"[..]));
        assert_eq!(v.get("zzzzz").and_then(Value::as_int), Some(7));
        assert!(v.get("announce-list").is_none());
        // Raw info bytes identical.
        let a = bencode::split_top_dict(&src).unwrap();
        let b = bencode::split_top_dict(&r.bytes).unwrap();
        let ia = a.iter().find(|e| e.key == b"info").unwrap();
        let ib = b.iter().find(|e| e.key == b"info").unwrap();
        assert_eq!(&src[ia.span.clone()], &r.bytes[ib.span.clone()]);
    }

    #[test]
    fn announce_list_tiers_and_udp_dropped() {
        let src = sample(true);
        let r = rewrite_bytes(&src, "http://127.0.0.1:7070/", "x", false).unwrap();
        assert_eq!(r.hash_in, r.hash_out);
        assert_eq!(r.maps.len(), 2);
        assert_eq!(r.maps[0].0, "x-1");
        assert_eq!(r.maps[1], ("x-2".to_owned(), "https://t3.example/announce".to_owned()));
        assert_eq!(r.dropped, vec!["udp://t2.example:1337/ann".to_owned()]);
        let v = bencode::decode(&r.bytes).unwrap();
        let tiers = v.get("announce-list").and_then(Value::as_list).unwrap();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[1].as_list().unwrap()[0].as_bytes(), Some(&b"http://127.0.0.1:7070/t/x-2/announce"[..]));
    }

    #[test]
    fn non_canonical_info_is_preserved_exactly() {
        // Unsorted info keys: a re-encoding rewriter would sort them and change the hash.
        let src = b"d8:announce20:http://tracker.x/ann4:infod4:name1:x6:lengthi5eee".to_vec();
        let r = rewrite_bytes(&src, "http://127.0.0.1:1", "a", false).unwrap();
        assert_eq!(r.hash_in, r.hash_out);
    }

    #[test]
    fn strip_webseeds_keeps_info_hash() {
        let mut src = b"d8:announce20:http://tracker.x/ann4:infod4:name1:x6:lengthi5ee8:url-listl9:http://wsee".to_vec();
        // keys must stay sorted-agnostic; our parser does not require order
        src.truncate(src.len());
        let kept = rewrite_bytes(&src, "http://127.0.0.1:1", "a", false).unwrap();
        let stripped = rewrite_bytes(&src, "http://127.0.0.1:1", "a", true).unwrap();
        assert!(bencode::decode(&kept.bytes).unwrap().get("url-list").is_some());
        assert!(bencode::decode(&stripped.bytes).unwrap().get("url-list").is_none());
        assert_eq!(kept.hash_out, stripped.hash_out);
    }

    #[test]
    fn rejects_bad_input() {
        assert!(rewrite_bytes(b"garbage", "http://127.0.0.1:1", "a", false).is_err());
        assert!(rewrite_bytes(b"d4:infod4:name1:xee", "http://127.0.0.1:1", "a", false).is_err());
        assert!(rewrite_bytes(&sample(false), "ftp://x", "a", false).is_err());
        let udp_only = b"d8:announce22:udp://t.example:1/ann4:infod4:name1:xee";
        assert!(rewrite_bytes(udp_only, "http://127.0.0.1:1", "a", false).is_err());
    }
}
