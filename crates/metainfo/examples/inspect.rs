//! Prints a summary of a `.torrent` file or magnet URI.
//!
//! ```text
//! cargo run -p trs-metainfo --example inspect -- path/to/file.torrent
//! cargo run -p trs-metainfo --example inspect -- 'magnet:?xt=urn:btih:...'
//! ```

use trs_metainfo::{Magnet, Metainfo};

fn main() {
    let Some(arg) = std::env::args().nth(1) else {
        eprintln!("usage: inspect <file.torrent | magnet:?...>");
        std::process::exit(2);
    };
    let ok = if arg.starts_with("magnet:") {
        magnet(&arg)
    } else {
        torrent(&arg)
    };
    if !ok {
        std::process::exit(1);
    }
}

fn magnet(uri: &str) -> bool {
    match Magnet::parse(uri) {
        Ok(m) => {
            println!("info-hash : {}", m.info_hash);
            if let Some(h) = m.info_hash.v2_hex() {
                println!("v2 hash   : {h}");
            }
            println!("name      : {}", m.display_name.as_deref().unwrap_or("-"));
            println!("trackers  : {}", m.trackers.len());
            for t in &m.trackers {
                println!("  {t}");
            }
            if let Some(xl) = m.exact_length {
                println!("length    : {xl}");
            }
            for w in &m.warnings {
                println!("warning   : {w:?}");
            }
            true
        }
        Err(e) => {
            eprintln!("invalid magnet: {e}");
            false
        }
    }
}

fn torrent(path: &str) -> bool {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return false;
        }
    };
    let m = match Metainfo::from_bytes(&bytes) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("invalid torrent: {e}");
            return false;
        }
    };
    println!("name         : {}", m.name);
    println!("version      : {:?}", m.version);
    println!("info-hash    : {}", m.info_hash);
    if let Some(h) = m.info_hash.v2_hex() {
        println!("v2 hash      : {h}");
    }
    println!("private      : {}", m.private);
    println!(
        "piece length : {} bytes x {} pieces",
        m.piece_length,
        m.piece_count()
    );
    println!(
        "size         : {} bytes ({} wanted, {} padding)",
        m.total_length,
        m.wanted_length(),
        m.padding_length
    );
    println!(
        "files        : {}{}",
        m.file_count(),
        if m.is_multi_file() {
            " (multi-file)"
        } else {
            ""
        }
    );
    for f in m.files().take(10) {
        println!(
            "  {:>14}  {}{}",
            f.length,
            f.path,
            if f.is_padding() { "  [padding]" } else { "" }
        );
    }
    if m.file_count() > 10 {
        println!("  ... {} more", m.file_count() - 10);
    }
    for (i, tier) in m.announce_tiers.iter().enumerate() {
        println!("tier {i}       : {}", tier.join(" "));
    }
    for w in m.url_list.iter() {
        println!("web seed     : {w}");
    }
    if let Some(s) = &m.source {
        println!("source       : {s}");
    }
    if let Some(c) = &m.comment {
        println!("comment      : {c}");
    }
    if let Some(c) = &m.created_by {
        println!("created by   : {c}");
    }
    if let Some(d) = m.creation_date {
        println!("created      : {d} (unix)");
    }
    for w in &m.warnings {
        println!("warning      : {w:?}");
    }
    true
}
