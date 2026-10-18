//! `mktest`: generate small synthetic single-file torrents (unique
//! info-hashes) for control experiments against the local mock tracker.
//! The pieces are random, so nothing can ever be downloaded: these torrents
//! exist only to make a client announce.

use crate::bencode::{Value, encode_bytes};
use anyhow::{Context, Result};
use rand::Rng;
use sha1::{Digest, Sha1};
use std::path::PathBuf;

/// Arguments for `mktest`.
#[derive(clap::Args, Debug)]
pub struct Args {
    /// Announce URL to embed (should point at the local mock).
    #[arg(long)]
    pub announce: String,
    /// Further announce URLs, each one its own tier (`announce-list`), after `--announce`.
    #[arg(long = "tier")]
    pub tiers: Vec<String>,
    /// How many torrents to create.
    #[arg(long, default_value_t = 1)]
    pub count: usize,
    /// Output directory.
    #[arg(long)]
    pub out_dir: PathBuf,
    /// Name prefix.
    #[arg(long, default_value = "probe-test")]
    pub prefix: String,
    /// Mark the torrents private (`info.private = 1`, BEP 27).
    #[arg(long)]
    pub private: bool,
}

/// Build one synthetic torrent, with an `announce-list` of one tier per URL when `more` is not empty: `announce` first, then `more`.
#[cfg(test)]
pub fn make_tiers(announce: &str, more: &[String], name: &str) -> Vec<u8> {
    make_torrent(announce, more, name, false)
}

/// As [`make_tiers`], optionally with `info.private = 1`.
pub fn make_torrent(announce: &str, more: &[String], name: &str, private: bool) -> Vec<u8> {
    let mut pieces = vec![0u8; 20 * 4];
    rand::rng().fill_bytes(&mut pieces);
    let mut fields = vec![
        (b"length".to_vec(), Value::Int(4 * 16384)),
        (b"name".to_vec(), Value::Bytes(name.as_bytes().to_vec())),
        (b"piece length".to_vec(), Value::Int(16384)),
        (b"pieces".to_vec(), Value::Bytes(pieces)),
    ];
    if private {
        fields.push((b"private".to_vec(), Value::Int(1)));
    }
    let info = Value::Dict(fields);
    let mut out = b"d8:announce".to_vec();
    encode_bytes(announce.as_bytes(), &mut out);
    if !more.is_empty() {
        let mut tiers = vec![Value::List(vec![Value::Bytes(announce.as_bytes().to_vec())])];
        tiers.extend(more.iter().map(|u| Value::List(vec![Value::Bytes(u.as_bytes().to_vec())])));
        out.extend_from_slice(b"13:announce-list");
        out.extend_from_slice(&Value::List(tiers).encode());
    }
    out.extend_from_slice(b"4:info");
    out.extend_from_slice(&info.encode());
    out.push(b'e');
    out
}

/// Entry point.
pub fn run(a: &Args) -> Result<()> {
    std::fs::create_dir_all(&a.out_dir).with_context(|| format!("creating {}", a.out_dir.display()))?;
    for i in 1..=a.count {
        let name = format!("{}-{i:02}", a.prefix);
        let t = make_torrent(&a.announce, &a.tiers, &name, a.private);
        let h: [u8; 20] = Sha1::digest(crate::bencode::split_top_dict(&t).map_err(|e| anyhow::anyhow!("{e}"))?.iter().find(|e| e.key == b"info").map(|e| t[e.span.clone()].to_vec()).unwrap_or_default()).into();
        let path = a.out_dir.join(format!("{name}.torrent"));
        std::fs::write(&path, &t)?;
        println!("{}  {}", crate::record::hex(&h), path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_torrents_are_valid_and_unique() {
        let a = make_tiers("http://localhost:1/announce", &[], "a");
        let b = make_tiers("http://localhost:1/announce", &[], "a");
        assert!(crate::bencode::decode(&a).is_ok());
        assert_ne!(crate::rewrite::info_hash(&a).unwrap(), crate::rewrite::info_hash(&b).unwrap());
    }

    #[test]
    fn private_flag_lands_in_the_info_dictionary() {
        let t = make_torrent("http://a.test/announce", &[], "a", true);
        let v = crate::bencode::decode(&t).unwrap();
        assert_eq!(v.get("info").and_then(|i| i.get("private")).and_then(crate::bencode::Value::as_int), Some(1));
        let u = make_torrent("http://a.test/announce", &[], "a", false);
        assert!(crate::bencode::decode(&u).unwrap().get("info").and_then(|i| i.get("private")).is_none());
    }

    #[test]
    fn tiers_become_an_announce_list_in_key_order() {
        let t = make_tiers("http://a.test/announce", &["http://b.test/announce".to_owned()], "a");
        let v = crate::bencode::decode(&t).unwrap();
        let tiers = v.get("announce-list").and_then(crate::bencode::Value::as_list).unwrap();
        assert_eq!(tiers.len(), 2);
        assert_eq!(tiers[1].as_list().unwrap()[0].as_bytes(), Some(&b"http://b.test/announce"[..]));
        assert!(crate::rewrite::info_hash(&t).is_ok());
    }
}
