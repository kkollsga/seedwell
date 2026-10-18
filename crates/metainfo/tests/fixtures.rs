//! Real distro torrents. Expected info-hashes come from the sources noted next
//! to each constant; the offline tests need no network.
//!
//! The `#[ignore]`d tests go further: they fetch the first piece of the payload
//! from an official mirror and check it against the torrent's piece table, which
//! proves the piece hashes *and* the multi-file offset model on real data.
//! Run them with `cargo test -p trs-metainfo -- --ignored`.

use sha1::{Digest as _, Sha1};
use trs_metainfo::{Magnet, MetaVersion, Metainfo, hex_encode};

fn load(name: &str) -> (Vec<u8>, Metainfo) {
    let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let m = Metainfo::from_bytes(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
    (bytes, m)
}

/// https://archlinux.org/releng/releases/json/ -> `info_hash` and `magnet_uri`
/// of release 2026.10.01.
const ARCH: &str = "archlinux-2026.10.01-x86_64.iso.torrent";
const ARCH_HASH: &str = "9b521219bc48550140a666f74a29048ed78e933b";
const ARCH_MAGNET: &str = "magnet:?xt=urn:btih:9b521219bc48550140a666f74a29048ed78e933b&dn=archlinux-2026.10.01-x86_64.iso";

/// https://torrent.ubuntu.com/tracker_index (Canonical's official tracker listing).
const UBUNTU: &str = "ubuntu-26.04.1-desktop-amd64.iso.torrent";
const UBUNTU_HASH: &str = "5b1e0d988fc7a0c9e99bd852071681a59974b39f";

/// No distro-published hash exists for these two. Computed with an independent
/// bencode implementation (python3 `hashlib.sha1` over the raw `info` span) and
/// re-verified by the piece-0 network tests below.
const DEBIAN: &str = "debian-13.7.0-amd64-netinst.iso.torrent";
const DEBIAN_HASH: &str = "7acf8fb590b2060dd9c3146ef770169d593433b0";
const FEDORA: &str = "Fedora-Workstation-Live-x86_64-44.torrent";
const FEDORA_HASH: &str = "439e24b06daf7255eeeedbc9cddab490fa8caefd";

#[test]
fn arch_matches_published_hash_and_magnet() {
    let (bytes, m) = load(ARCH);
    assert_eq!(m.info_hash.to_string(), ARCH_HASH);
    let magnet = Magnet::parse(ARCH_MAGNET).unwrap();
    assert_eq!(magnet.info_hash, m.info_hash);
    assert_eq!(magnet.display_name.as_deref(), Some(&*m.name));
    assert_eq!(m.version, MetaVersion::V1);
    assert_eq!(m.total_length, 1_640_497_152);
    assert_eq!(m.piece_length, 524_288);
    assert_eq!(m.piece_count(), 3129);
    assert!(!m.is_multi_file() && !m.private);
    // Arch ships no tracker, only HTTP web seeds (the cap must not truncate them).
    assert!(m.announce_tiers.is_empty());
    assert!(m.url_list.len() > 32 && m.url_list.iter().all(|u| u.starts_with("https://")));
    assert_eq!(
        m.comment.as_deref(),
        Some("Arch Linux 2026.10.01 <https://archlinux.org>")
    );
    assert_eq!(
        m.info_bytes(&bytes).map(<[u8]>::len),
        Some(m.info_range.len())
    );
}

#[test]
fn ubuntu_matches_canonical_tracker_index() {
    let (_, m) = load(UBUNTU);
    assert_eq!(m.info_hash.to_string(), UBUNTU_HASH);
    assert_eq!(m.total_length, 6_482_409_472);
    assert_eq!(m.piece_count(), 24729);
    let tiers: Vec<Vec<&str>> = m
        .announce_tiers
        .iter()
        .map(|t| t.iter().map(|u| &**u).collect())
        .collect();
    assert_eq!(
        tiers,
        [
            ["https://torrent.ubuntu.com/announce"],
            ["https://ipv6.torrent.ubuntu.com/announce"]
        ]
    );
    assert!(m.warnings.is_empty(), "{:?}", m.warnings);
}

#[test]
fn debian_hash_and_layout() {
    let (_, m) = load(DEBIAN);
    assert_eq!(m.info_hash.to_string(), DEBIAN_HASH);
    assert_eq!(m.total_length, 792_723_456);
    assert_eq!(m.piece_length, 262_144);
    assert_eq!(m.piece_count(), 3024);
    assert_eq!(m.announce_tiers.len(), 1);
    assert_eq!(
        &*m.announce_tiers[0][0],
        "http://bttracker.debian.org:6969/announce"
    );
    assert_eq!(m.url_list.len(), 2);
}

#[test]
fn fedora_is_multi_file_with_correct_offsets() {
    let (_, m) = load(FEDORA);
    assert_eq!(m.info_hash.to_string(), FEDORA_HASH);
    assert!(m.is_multi_file());
    assert_eq!(&*m.name, "Fedora-Workstation-Live-x86_64-44");
    assert_eq!(m.file_count(), 2);
    let (a, b) = (m.file(0).unwrap(), m.file(1).unwrap());
    assert_eq!(
        (a.path, a.length, a.offset),
        ("Fedora-Workstation-44-1.7-x86_64-CHECKSUM", 1062, 0)
    );
    assert_eq!(b.path, "Fedora-Workstation-Live-44-1.7.x86_64.iso");
    assert_eq!((b.length, b.offset), (2_851_612_672, 1062));
    assert_eq!(m.total_length, 1062 + 2_851_612_672);
    assert_eq!(m.piece_count(), 10879);
}

#[test]
fn all_fixtures_have_consistent_tables_and_canonical_info() {
    for name in [ARCH, UBUNTU, DEBIAN, FEDORA] {
        let (bytes, m) = load(name);
        let raw = m.info_bytes(&bytes).unwrap();
        // These are canonical, so the original span survives a decode/encode cycle.
        let v = trs_bencode::decode(raw).unwrap();
        assert_eq!(trs_bencode::encode(&v), raw, "{name}");
        assert!(m.info_hash.verifies(raw));
        assert_eq!(m.piece_hashes().len(), 20 * m.piece_count());
        let sum: u64 = m.files().map(|f| f.length).sum();
        assert_eq!(sum, m.total_length);
        // Bare-info parsing agrees with full parsing.
        let bare = Metainfo::from_info_bytes(raw).unwrap();
        assert_eq!(bare.info_hash, m.info_hash);
        assert_eq!(bare.total_length, m.total_length);
    }
}

// ------------------------------------------------------------ network (ignored)

fn fetch_range(url: &str, range: std::ops::Range<u64>) -> Vec<u8> {
    let out = std::process::Command::new("curl")
        .args([
            "-sSL",
            "--fail",
            "--max-time",
            "120",
            "-r",
            &format!("{}-{}", range.start, range.end - 1),
            url,
        ])
        .output()
        .expect("curl");
    assert!(
        out.status.success(),
        "curl {url}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout.len() as u64,
        range.end - range.start,
        "short read from {url}"
    );
    out.stdout
}

#[test]
#[ignore = "network: downloads one piece from an official mirror"]
fn arch_piece_zero_matches_mirror() {
    let (_, m) = load(ARCH);
    let data = fetch_range(
        "https://mirrors.kernel.org/archlinux/iso/2026.10.01/archlinux-2026.10.01-x86_64.iso",
        0..u64::from(m.piece_size(0).unwrap()),
    );
    assert_eq!(
        hex_encode(&Sha1::digest(&data)),
        hex_encode(m.piece_hash(0).unwrap())
    );
}

#[test]
#[ignore = "network: downloads one piece from an official mirror"]
fn debian_piece_zero_matches_mirror() {
    let (_, m) = load(DEBIAN);
    let data = fetch_range(&m.url_list[0], 0..u64::from(m.piece_size(0).unwrap()));
    assert_eq!(
        hex_encode(&Sha1::digest(&data)),
        hex_encode(m.piece_hash(0).unwrap())
    );
}

#[test]
#[ignore = "network: downloads one piece from an official mirror"]
fn fedora_piece_zero_spans_both_files() {
    let (_, m) = load(FEDORA);
    let base = "https://dl.fedoraproject.org/pub/fedora/linux/releases/44/Workstation/x86_64/iso";
    let (a, b) = (m.file(0).unwrap(), m.file(1).unwrap());
    let psize = u64::from(m.piece_size(0).unwrap());
    // Piece 0 = all of file 0 followed by the head of file 1: this exercises
    // the cumulative offsets for real.
    let mut data = fetch_range(&format!("{base}/{}", a.path), 0..a.length);
    data.extend(fetch_range(
        &format!("{base}/{}", b.path),
        0..psize - a.length,
    ));
    assert_eq!(b.offset, a.length);
    assert_eq!(
        hex_encode(&Sha1::digest(&data)),
        hex_encode(m.piece_hash(0).unwrap())
    );
}
