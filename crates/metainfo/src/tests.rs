//! Unit tests with hand-built torrents. Real-world files are covered in
//! `tests/fixtures.rs`.

use super::*;
use proptest::prelude::*;
use sha1::{Digest as _, Sha1};
use trs_bencode as bc;

// ------------------------------------------------------------ tiny DSL

fn s(b: &[u8]) -> Vec<u8> {
    let mut e = bc::Encoder::new();
    e.bytes(b);
    e.finish()
}
fn i(n: i64) -> Vec<u8> {
    let mut e = bc::Encoder::new();
    e.int(n);
    e.finish()
}
fn l(items: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![b'l'];
    for it in items {
        out.extend_from_slice(it);
    }
    out.push(b'e');
    out
}
/// Dict with keys sorted (canonical), values pre-encoded.
fn d(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut sorted: Vec<_> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    d_ordered(&sorted.into_iter().cloned().collect::<Vec<_>>())
}
/// Dict with keys exactly in the given order (may be non-canonical).
fn d_ordered(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut out = vec![b'd'];
    for (k, v) in entries {
        out.extend_from_slice(&s(k.as_bytes()));
        out.extend_from_slice(v);
    }
    out.push(b'e');
    out
}

fn pieces_for(total: u64, piece_length: u64) -> Vec<u8> {
    s(&vec![0xAB; 20 * total.div_ceil(piece_length) as usize])
}

fn file(len: i64, path: &[&[u8]]) -> Vec<u8> {
    d(&[
        ("length", i(len)),
        ("path", l(&path.iter().map(|p| s(p)).collect::<Vec<_>>())),
    ])
}

fn file_attr(len: i64, path: &[&[u8]], attr: &str) -> Vec<u8> {
    d(&[
        ("attr", s(attr.as_bytes())),
        ("length", i(len)),
        ("path", l(&path.iter().map(|p| s(p)).collect::<Vec<_>>())),
    ])
}

const PL: u64 = 16384;

fn info_single(name: &[u8], len: u64) -> Vec<u8> {
    d(&[
        ("length", i(len as i64)),
        ("name", s(name)),
        ("piece length", i(PL as i64)),
        ("pieces", pieces_for(len, PL)),
    ])
}

fn info_multi(name: &[u8], files: Vec<Vec<u8>>, total: u64) -> Vec<u8> {
    d(&[
        ("files", l(&files)),
        ("name", s(name)),
        ("piece length", i(PL as i64)),
        ("pieces", pieces_for(total, PL)),
    ])
}

fn torrent(info: Vec<u8>, extra: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut e: Vec<(&str, Vec<u8>)> = extra.to_vec();
    e.push(("info", info));
    d(&e)
}

fn parse(info: Vec<u8>) -> Metainfo {
    Metainfo::from_bytes(&torrent(info, &[])).expect("should parse")
}

fn parse_err(info: Vec<u8>) -> Error {
    Metainfo::from_bytes(&torrent(info, &[])).expect_err("should fail")
}

fn sha1_hex(b: &[u8]) -> String {
    hex_encode(&Sha1::digest(b))
}

// ------------------------------------------------------------ basics

#[test]
fn single_file_basics() {
    let info = info_single(b"file.bin", 40000);
    let bytes = torrent(info.clone(), &[]);
    let m = Metainfo::from_bytes(&bytes).unwrap();
    assert_eq!(m.version, MetaVersion::V1);
    assert_eq!(&*m.name, "file.bin");
    assert_eq!(m.piece_length, 16384);
    assert_eq!(m.total_length, 40000);
    assert_eq!(m.wanted_length(), 40000);
    assert!(!m.is_multi_file());
    assert!(!m.private);
    assert_eq!(m.piece_count(), 3);
    assert_eq!(m.piece_hashes().len(), 60);
    assert_eq!(m.file_count(), 1);
    let f = m.file(0).unwrap();
    assert_eq!(
        (f.path, f.length, f.offset, f.index),
        ("file.bin", 40000, 0, 0)
    );
    assert!(!f.is_padding() && !f.is_symlink() && !f.is_hidden() && !f.is_executable());
    assert_eq!(m.info_hash.v2, None);
    assert_eq!(m.info_hash.v1_hex().unwrap(), sha1_hex(&info));
    assert_eq!(m.info_bytes(&bytes), Some(&info[..]));
    assert!(m.warnings.is_empty());
}

#[test]
fn piece_accessors() {
    let m = parse(info_single(b"f", 40000));
    assert_eq!(m.piece_hash(0), Some(&[0xAB; 20]));
    assert_eq!(m.piece_hash(2), Some(&[0xAB; 20]));
    assert_eq!(m.piece_hash(3), None);
    assert_eq!(m.piece_hash(usize::MAX), None);
    assert_eq!(m.piece_size(0), Some(16384));
    assert_eq!(m.piece_size(2), Some(40000 - 2 * 16384));
    assert_eq!(m.piece_size(3), None);
    // Exact multiple: last piece is full size.
    let m = parse(info_single(b"f", 32768));
    assert_eq!(m.piece_count(), 2);
    assert_eq!(m.piece_size(1), Some(16384));
}

#[test]
fn multi_file_offsets_and_root() {
    let files = vec![
        file(100, &[b"a.txt"]),
        file(20000, &[b"sub", b"b.bin"]),
        file(7, &[b"sub", b"deep", b"c"]),
    ];
    let m = parse(info_multi(b"root", files, 20107));
    assert!(m.is_multi_file());
    assert_eq!(&*m.name, "root");
    assert_eq!(m.total_length, 20107);
    let all: Vec<_> = m
        .files()
        .map(|f| (f.path.to_string(), f.length, f.offset))
        .collect();
    assert_eq!(
        all,
        vec![
            ("a.txt".to_string(), 100, 0),
            ("sub/b.bin".to_string(), 20000, 100),
            ("sub/deep/c".to_string(), 7, 20100),
        ]
    );
    assert_eq!(m.files().len(), 3);
    assert_eq!(
        m.file(1).unwrap().components().collect::<Vec<_>>(),
        ["sub", "b.bin"]
    );
    assert!(m.file(3).is_none());
}

#[test]
fn zero_length_files_allowed_in_multi_file() {
    let files = vec![file(0, &[b"empty"]), file(10, &[b"x"])];
    let m = parse(info_multi(b"r", files, 10));
    assert_eq!(m.file(0).unwrap().length, 0);
    assert_eq!(m.file(1).unwrap().offset, 0);
}

#[test]
fn padding_files_flagged_and_excluded_from_wanted() {
    // 100 bytes of data + 16284 bytes of padding aligns the next file to a piece.
    let files = vec![
        file_attr(100, &[b"a"], ""),
        file_attr(16284, &[b".pad", b"16284"], "p"),
        file_attr(50, &[b"b"], "x"),
    ];
    let m = parse(info_multi(b"r", files, 16434));
    assert_eq!(m.total_length, 16434);
    assert_eq!(m.padding_length, 16284);
    assert_eq!(m.wanted_length(), 150);
    assert!(m.file(1).unwrap().is_padding());
    assert!(!m.file(0).unwrap().is_padding());
    assert!(m.file(2).unwrap().is_executable());
    assert_eq!(m.file(2).unwrap().offset, 16384);
}

#[test]
fn attr_flags_hidden_and_symlink() {
    let files = vec![file_attr(1, &[b"h"], "h"), file_attr(0, &[b"link"], "l")];
    let m = parse(info_multi(b"r", files, 1));
    assert!(m.file(0).unwrap().is_hidden());
    assert!(m.file(1).unwrap().is_symlink());
    assert!(m.warnings.contains(&Warning::SymlinkEntries(1)));
}

// ------------------------------------------------------------ info-hash

#[test]
fn info_hash_uses_original_bytes_not_reencoding() {
    // Keys deliberately out of canonical order.
    let info = d_ordered(&[
        ("pieces", pieces_for(10, PL)),
        ("piece length", i(PL as i64)),
        ("name", s(b"x")),
        ("length", i(10)),
    ]);
    let bytes = torrent(info.clone(), &[]);
    let m = Metainfo::from_bytes(&bytes).unwrap();
    let reencoded = bc::encode(&bc::decode(&info).unwrap());
    assert_ne!(
        reencoded, info,
        "test precondition: info must be non-canonical"
    );
    assert_eq!(m.info_hash.v1_hex().unwrap(), sha1_hex(&info));
    assert_ne!(m.info_hash.v1_hex().unwrap(), sha1_hex(&reencoded));
}

#[test]
fn info_hash_changes_with_source_and_private() {
    let base = info_single(b"x", 10);
    let with_src = d(&[
        ("length", i(10)),
        ("name", s(b"x")),
        ("piece length", i(PL as i64)),
        ("pieces", pieces_for(10, PL)),
        ("source", s(b"TRACKER")),
        ("private", i(1)),
    ]);
    let (a, b) = (parse(base), parse(with_src));
    assert_ne!(a.info_hash, b.info_hash);
    assert_eq!(b.source.as_deref(), Some("TRACKER"));
    assert!(b.private);
}

#[test]
fn from_info_bytes_and_verification() {
    let info = info_single(b"x", 10);
    let full = Metainfo::from_bytes(&torrent(info.clone(), &[])).unwrap();
    let bare = Metainfo::from_info_bytes(&info).unwrap();
    assert_eq!(full.info_hash, bare.info_hash);
    assert_eq!(bare.info_range, 0..info.len());
    assert!(bare.announce_tiers.is_empty());
    assert!(full.info_hash.verifies(&info));
    assert!(!full.info_hash.verifies(b"d4:name1:xe"));
    assert!(!InfoHash::default().verifies(&info));
    assert!(matches!(
        Metainfo::from_info_bytes(b"i1e"),
        Err(Error::NotADictionary("info"))
    ));
}

// ------------------------------------------------------------ sanitization

#[test]
fn sanitize_rejects_every_unsafe_component() {
    use PathError::*;
    assert_eq!(sanitize_component(b"").unwrap_err(), Empty);
    assert_eq!(sanitize_component(b".").unwrap_err(), CurrentDir);
    assert_eq!(sanitize_component(b"..").unwrap_err(), ParentDir);
    assert_eq!(sanitize_component(b"a\0b").unwrap_err(), Nul);
    assert_eq!(sanitize_component(b"\0").unwrap_err(), Nul);
    assert_eq!(sanitize_component(b"a/b").unwrap_err(), Separator);
    assert_eq!(sanitize_component(b"/etc").unwrap_err(), Separator);
    assert_eq!(sanitize_component(b"../x").unwrap_err(), Separator);
    assert_eq!(sanitize_component(&[b'a'; 256]).unwrap_err(), TooLong);
}

#[test]
fn sanitize_accepts_edge_cases() {
    assert!(sanitize_component(&[b'a'; 255]).is_ok());
    // Dots that are not exactly "." or ".." are ordinary names.
    for ok in [
        &b"..."[..],
        b".hidden",
        b"a..b",
        b"..a",
        b" ",
        b"\\",
        b"con",
    ] {
        assert!(sanitize_component(ok).is_ok(), "{ok:?}");
    }
    let (c, r) = sanitize_component("héllo wörld ✓".as_bytes()).unwrap();
    assert_eq!(c, "héllo wörld ✓");
    assert_eq!(r, Repairs::default());
    assert!(
        matches!(c, std::borrow::Cow::Borrowed(_)),
        "no allocation when unchanged"
    );
}

#[test]
fn sanitize_repairs_colon_and_invalid_utf8() {
    let (c, r) = sanitize_component(b"a:b:c").unwrap();
    assert_eq!(c, "a_b_c");
    assert!(r.colon && !r.lossy);
    let (c, r) = sanitize_component(b"bad\xffbyte").unwrap();
    assert_eq!(c, "bad\u{fffd}byte");
    assert!(r.lossy && !r.colon);
    let (c, r) = sanitize_component(b"x\xfe:y").unwrap();
    assert_eq!(c, "x\u{fffd}_y");
    assert!(r.lossy && r.colon);
}

#[test]
fn sanitize_length_is_checked_after_lossy_expansion() {
    // 100 invalid bytes -> 100 * U+FFFD = 300 bytes, over the 255 limit.
    assert_eq!(
        sanitize_component(&[0xff; 100]).unwrap_err(),
        PathError::TooLong
    );
    assert!(sanitize_component(&[0xff; 85]).is_ok()); // 255 bytes exactly
}

#[test]
fn torrent_rejects_unsafe_paths() {
    let multi = |path: &[&[u8]]| info_multi(b"r", vec![file(1, path)], 1);
    assert_eq!(
        parse_err(multi(&[b"..", b"etc"])),
        Error::Path(PathError::ParentDir)
    );
    assert_eq!(
        parse_err(multi(&[b"a", b"..", b"b"])),
        Error::Path(PathError::ParentDir)
    );
    assert_eq!(
        parse_err(multi(&[b"."])),
        Error::Path(PathError::CurrentDir)
    );
    assert_eq!(
        parse_err(multi(&[b"", b"etc", b"passwd"])),
        Error::Path(PathError::Empty)
    );
    assert_eq!(
        parse_err(multi(&[b"a", b""])),
        Error::Path(PathError::Empty)
    );
    assert_eq!(
        parse_err(multi(&[b"/etc/passwd"])),
        Error::Path(PathError::Separator)
    );
    assert_eq!(
        parse_err(multi(&[b"a/b"])),
        Error::Path(PathError::Separator)
    );
    assert_eq!(parse_err(multi(&[b"a\0b"])), Error::Path(PathError::Nul));
    assert_eq!(
        parse_err(multi(&[&[b'x'; 256]])),
        Error::Path(PathError::TooLong)
    );
    assert_eq!(parse_err(multi(&[])), Error::Path(PathError::NoComponents));
}

#[test]
fn torrent_rejects_unsafe_names() {
    for (name, err) in [
        (&b".."[..], PathError::ParentDir),
        (b".", PathError::CurrentDir),
        (b"", PathError::Empty),
        (b"/etc", PathError::Separator),
        (b"../../x", PathError::Separator),
        (b"a\0", PathError::Nul),
    ] {
        assert_eq!(
            parse_err(info_single(name, 10)),
            Error::Path(err),
            "{name:?}"
        );
        assert_eq!(
            parse_err(info_multi(name, vec![file(10, &[b"f"])], 10)),
            Error::Path(err)
        );
    }
}

#[test]
fn torrent_path_depth_and_total_length_limits() {
    let comps: Vec<Vec<u8>> = (0..65).map(|_| b"d".to_vec()).collect();
    let refs: Vec<&[u8]> = comps.iter().map(|c| c.as_slice()).collect();
    assert_eq!(
        parse_err(info_multi(b"r", vec![file(1, &refs)], 1)),
        Error::Path(PathError::TooDeep)
    );
    // 20 components of 255 bytes = 5100 > 4096.
    let big = vec![b'z'; 255];
    let refs: Vec<&[u8]> = (0..20).map(|_| big.as_slice()).collect();
    assert_eq!(
        parse_err(info_multi(b"r", vec![file(1, &refs)], 1)),
        Error::Path(PathError::PathTooLong)
    );
}

#[test]
fn torrent_repairs_are_reported() {
    let files = vec![
        file(1, &[b"a:b", b"ok"]),
        file(1, &[b"bad\xff"]),
        file(1, &[b"c:d"]),
    ];
    let m = parse(info_multi(b"na:me", files, 3));
    assert_eq!(&*m.name, "na_me");
    let paths: Vec<_> = m.files().map(|f| f.path.to_string()).collect();
    assert_eq!(paths, ["a_b/ok", "bad\u{fffd}", "c_d"]);
    assert!(m.warnings.contains(&Warning::ColonMappedComponents(3)));
    assert!(m.warnings.contains(&Warning::LossyPathComponents(1)));
}

#[test]
fn utf8_variants_are_preferred() {
    let info = d(&[
        (
            "files",
            l(&[d(&[
                ("length", i(1)),
                ("path", l(&[s(b"caf\xe9")])),
                ("path.utf-8", l(&[s("café".as_bytes())])),
            ])]),
        ),
        ("name", s(b"n\xe9")),
        ("name.utf-8", s("né".as_bytes())),
        ("piece length", i(PL as i64)),
        ("pieces", pieces_for(1, PL)),
    ]);
    let m = parse(info);
    assert_eq!(&*m.name, "né");
    assert_eq!(m.file(0).unwrap().path, "café");
    assert!(m.warnings.is_empty());
}

#[test]
fn duplicate_paths_rejected() {
    let dup = vec![file(1, &[b"a"]), file(1, &[b"a"])];
    assert_eq!(parse_err(info_multi(b"r", dup, 2)), Error::DuplicatePath);
    // Distinct on the wire, identical after ':' -> '_' mapping.
    let collide = vec![file(1, &[b"a:b"]), file(1, &[b"a_b"])];
    assert_eq!(
        parse_err(info_multi(b"r", collide, 2)),
        Error::DuplicatePath
    );
    // Same name in different directories is fine.
    let ok = vec![file(1, &[b"x", b"a"]), file(1, &[b"y", b"a"])];
    parse(info_multi(b"r", ok, 2));
}

// ------------------------------------------------------------ malformed input

#[test]
fn malformed_top_level() {
    assert!(matches!(Metainfo::from_bytes(b""), Err(Error::Bencode(_))));
    assert!(matches!(
        Metainfo::from_bytes(b"d4:infod"),
        Err(Error::Bencode(_))
    ));
    assert!(matches!(
        Metainfo::from_bytes(b"not bencode"),
        Err(Error::Bencode(_))
    ));
    assert_eq!(
        Metainfo::from_bytes(b"i5e").unwrap_err(),
        Error::NotADictionary("torrent")
    );
    assert_eq!(
        Metainfo::from_bytes(b"le").unwrap_err(),
        Error::NotADictionary("torrent")
    );
    assert_eq!(
        Metainfo::from_bytes(b"de").unwrap_err(),
        Error::MissingKey("info")
    );
    assert_eq!(
        Metainfo::from_bytes(b"d4:infoi1ee").unwrap_err(),
        Error::NotADictionary("info")
    );
    assert_eq!(
        Metainfo::from_bytes(b"d4:info0:e").unwrap_err(),
        Error::NotADictionary("info")
    );
    // Trailing garbage after the document.
    let mut t = torrent(info_single(b"x", 10), &[]);
    t.extend_from_slice(b"junk");
    assert!(matches!(
        Metainfo::from_bytes(&t),
        Err(Error::Bencode(bc::Error::TrailingData { .. }))
    ));
    // Duplicate "info" keys are an info-hash confusion vector.
    let i1 = info_single(b"x", 10);
    let dup = [&b"d4:info"[..], &i1, b"4:info", &i1, b"e"].concat();
    assert!(matches!(
        Metainfo::from_bytes(&dup),
        Err(Error::Bencode(bc::Error::DuplicateKey { .. }))
    ));
}

#[test]
fn truncation_never_panics_and_always_errors() {
    let t = torrent(
        info_multi(b"r", vec![file(5, &[b"a"]), file(5, &[b"b"])], 10),
        &[("announce", s(b"http://t/a"))],
    );
    for cut in 0..t.len() {
        assert!(Metainfo::from_bytes(&t[..cut]).is_err(), "cut at {cut}");
    }
    assert!(Metainfo::from_bytes(&t).is_ok());
}

#[test]
fn missing_and_wrong_typed_info_keys() {
    let pl = ("piece length", i(PL as i64));
    let pcs = ("pieces", pieces_for(10, PL));
    let nm = ("name", s(b"x"));
    let ln = ("length", i(10));
    assert_eq!(
        parse_err(d(&[pl.clone(), pcs.clone(), ln.clone()])),
        Error::MissingKey("name")
    );
    assert_eq!(
        parse_err(d(&[pcs.clone(), nm.clone(), ln.clone()])),
        Error::MissingKey("piece length")
    );
    assert_eq!(
        parse_err(d(&[pl.clone(), nm.clone(), ln.clone()])),
        Error::MissingKey("pieces")
    );
    assert_eq!(
        parse_err(d(&[pl.clone(), pcs.clone(), nm.clone()])),
        Error::MissingKey("length")
    );
    assert_eq!(
        parse_err(d(&[("name", i(1)), pl.clone(), pcs.clone(), ln.clone()])),
        Error::WrongType("name")
    );
    assert_eq!(
        parse_err(d(&[
            ("piece length", s(b"x")),
            nm.clone(),
            pcs.clone(),
            ln.clone()
        ])),
        Error::WrongType("piece length")
    );
    assert_eq!(
        parse_err(d(&[
            ("length", s(b"x")),
            nm.clone(),
            pl.clone(),
            pcs.clone()
        ])),
        Error::WrongType("length")
    );
    assert_eq!(
        parse_err(d(&[
            ("private", s(b"1")),
            nm.clone(),
            pl.clone(),
            pcs.clone(),
            ln.clone()
        ])),
        Error::WrongType("private")
    );
    assert_eq!(
        parse_err(d(&[("pieces", i(1)), nm.clone(), pl.clone(), ln.clone()])),
        Error::WrongType("pieces")
    );
}

#[test]
fn piece_length_bounds() {
    let mk = |pl: i64| {
        d(&[
            ("length", i(10)),
            ("name", s(b"x")),
            ("piece length", i(pl)),
            (
                "pieces",
                s(&vec![
                    0;
                    20 * 10usize.div_ceil(pl.clamp(1, 1 << 20) as usize)
                ]),
            ),
        ])
    };
    for bad in [
        0,
        -1,
        i64::MIN,
        (MAX_PIECE_LENGTH as i64) + 1,
        i64::MAX,
        1 << 40,
    ] {
        assert_eq!(parse_err(mk(bad)), Error::InvalidPieceLength, "{bad}");
    }
    assert_eq!(
        parse(mk(MAX_PIECE_LENGTH as i64)).piece_length,
        MAX_PIECE_LENGTH
    );
    assert_eq!(parse(mk(1)).piece_length, 1);
}

#[test]
fn pieces_validation() {
    let mk = |pieces: Vec<u8>, len: i64| {
        d(&[
            ("length", i(len)),
            ("name", s(b"x")),
            ("piece length", i(PL as i64)),
            ("pieces", s(&pieces)),
        ])
    };
    assert_eq!(parse_err(mk(vec![0; 21], 10)), Error::BadPiecesLength);
    assert_eq!(parse_err(mk(vec![0; 19], 10)), Error::BadPiecesLength);
    assert_eq!(
        parse_err(mk(vec![0; 40], 10)),
        Error::PieceCountMismatch {
            expected: 1,
            actual: 2
        }
    );
    assert_eq!(
        parse_err(mk(vec![], 10)),
        Error::PieceCountMismatch {
            expected: 1,
            actual: 0
        }
    );
    assert_eq!(
        parse_err(mk(vec![0; 20], 16385)),
        Error::PieceCountMismatch {
            expected: 2,
            actual: 1
        }
    );
    assert!(Metainfo::from_bytes(&torrent(mk(vec![0; 20], 16384), &[])).is_ok());
}

#[test]
fn length_errors() {
    assert_eq!(parse_err(info_single(b"x", 0)), Error::EmptyTorrent);
    let neg = d(&[
        ("length", i(-1)),
        ("name", s(b"x")),
        ("piece length", i(PL as i64)),
        ("pieces", s(&[])),
    ]);
    assert_eq!(parse_err(neg), Error::NegativeLength);
    assert_eq!(
        parse_err(info_multi(b"r", vec![file(-5, &[b"a"])], 1)),
        Error::NegativeLength
    );
    assert_eq!(parse_err(info_multi(b"r", vec![], 1)), Error::EmptyTorrent);
    assert_eq!(
        parse_err(info_multi(b"r", vec![file(0, &[b"a"])], 0)),
        Error::EmptyTorrent
    );
    // i64::MAX * 3 overflows u64.
    let huge = vec![
        file(i64::MAX, &[b"a"]),
        file(i64::MAX, &[b"b"]),
        file(i64::MAX, &[b"c"]),
    ];
    let info = d(&[
        ("files", l(&huge)),
        ("name", s(b"r")),
        ("piece length", i(PL as i64)),
        ("pieces", s(&[])),
    ]);
    assert_eq!(parse_err(info), Error::LengthOverflow);
}

#[test]
fn layout_errors() {
    let both = d(&[
        ("files", l(&[file(1, &[b"a"])])),
        ("length", i(1)),
        ("name", s(b"x")),
        ("piece length", i(PL as i64)),
        ("pieces", pieces_for(1, PL)),
    ]);
    assert_eq!(parse_err(both), Error::AmbiguousLayout);
    let bad_files = |v: Vec<u8>| {
        d(&[
            ("files", v),
            ("name", s(b"x")),
            ("piece length", i(PL as i64)),
            ("pieces", pieces_for(1, PL)),
        ])
    };
    assert_eq!(parse_err(bad_files(i(1))), Error::WrongType("files"));
    assert_eq!(
        parse_err(bad_files(l(&[i(1)]))),
        Error::NotADictionary("file entry")
    );
    assert_eq!(
        parse_err(bad_files(l(&[d(&[("path", l(&[s(b"a")]))])]))),
        Error::MissingKey("length")
    );
    assert_eq!(
        parse_err(bad_files(l(&[d(&[("length", i(1))])]))),
        Error::MissingKey("path")
    );
    assert_eq!(
        parse_err(bad_files(l(&[d(&[("length", i(1)), ("path", s(b"a"))])]))),
        Error::WrongType("path")
    );
    assert_eq!(
        parse_err(bad_files(l(&[d(&[
            ("length", i(1)),
            ("path", l(&[i(1)]))
        ])]))),
        Error::WrongType("path")
    );
}

#[test]
fn meta_version_errors() {
    let mk = |v: Vec<u8>| {
        d(&[
            ("length", i(10)),
            ("meta version", v),
            ("name", s(b"x")),
            ("piece length", i(PL as i64)),
            ("pieces", pieces_for(10, PL)),
        ])
    };
    assert_eq!(parse_err(mk(i(3))), Error::UnsupportedMetaVersion(3));
    assert_eq!(parse_err(mk(i(0))), Error::UnsupportedMetaVersion(0));
    assert_eq!(parse_err(mk(s(b"2"))), Error::WrongType("meta version"));
    // meta version 2 without a file tree is malformed.
    assert_eq!(parse_err(mk(i(2))), Error::MissingKey("file tree"));
    // meta version 1 is plain v1.
    assert_eq!(parse(mk(i(1))).version, MetaVersion::V1);
}

#[test]
fn too_many_items_is_rejected_by_limits() {
    let limits = bc::DecodeLimits {
        max_items: 10,
        max_bytes: 1 << 20,
    };
    let t = torrent(info_single(b"x", 10), &[]);
    assert!(matches!(
        Metainfo::from_bytes_with_limits(&t, limits),
        Err(Error::Bencode(bc::Error::TooManyItems))
    ));
}

// ------------------------------------------------------------ v2 / hybrid

fn tree_leaf(len: i64) -> Vec<u8> {
    d(&[("", d(&[("length", i(len)), ("pieces root", s(&[7; 32]))]))])
}

#[test]
fn v2_only_torrent() {
    let tree = d(&[
        ("a.txt", tree_leaf(100)),
        ("dir", d(&[("b.bin", tree_leaf(20000))])),
    ]);
    let info = d(&[
        ("file tree", tree),
        ("meta version", i(2)),
        ("name", s(b"v2root")),
        ("piece length", i(PL as i64)),
    ]);
    let bytes = torrent(info.clone(), &[("piece layers", d(&[]))]);
    let m = Metainfo::from_bytes(&bytes).unwrap();
    assert_eq!(m.version, MetaVersion::V2);
    assert_eq!(m.info_hash.v1, None);
    assert_eq!(
        hex_encode(&m.info_hash.v2.unwrap()),
        hex_encode(&sha2::Sha256::digest(&info))
    );
    assert_eq!(m.piece_count(), 0);
    assert!(m.is_multi_file());
    let files: Vec<_> = m
        .files()
        .map(|f| (f.path.to_string(), f.length, f.offset))
        .collect();
    // v2 files start on piece boundaries.
    assert_eq!(
        files,
        vec![("a.txt".into(), 100, 0), ("dir/b.bin".into(), 20000, 16384)]
    );
    assert_eq!(m.wanted_length(), 20100);
    assert!(m.warnings.contains(&Warning::V2PieceLayersIgnored));
    assert_eq!(
        m.info_hash.swarm_id().unwrap(),
        m.info_hash.v2.unwrap()[..20]
    );
}

#[test]
fn v2_single_file_and_errors() {
    let single = d(&[
        ("file tree", d(&[("f.iso", tree_leaf(5))])),
        ("meta version", i(2)),
        ("name", s(b"f.iso")),
        ("piece length", i(PL as i64)),
    ]);
    let m = parse(single);
    assert!(!m.is_multi_file());
    assert_eq!(m.file(0).unwrap().path, "f.iso");

    let mk = |tree: Vec<u8>| {
        d(&[
            ("file tree", tree),
            ("meta version", i(2)),
            ("name", s(b"n")),
            ("piece length", i(PL as i64)),
        ])
    };
    assert_eq!(parse_err(mk(i(1))), Error::WrongType("file tree"));
    assert_eq!(
        parse_err(mk(d(&[("a", i(1))]))),
        Error::WrongType("file tree")
    );
    assert_eq!(parse_err(mk(d(&[]))), Error::EmptyTorrent);
    assert_eq!(
        parse_err(mk(d(&[("..", tree_leaf(1))]))),
        Error::Path(PathError::ParentDir)
    );
    // A node cannot be both a file and a directory.
    let mixed = d(&[("a", d(&[("", d(&[("length", i(1))])), ("b", tree_leaf(1))]))]);
    assert_eq!(parse_err(mk(mixed)), Error::WrongType("file tree"));
    assert_eq!(
        parse_err(mk(d(&[("a", d(&[("", d(&[("length", i(-1))]))]))]))),
        Error::NegativeLength
    );
}

#[test]
fn hybrid_torrent_has_both_hashes_and_uses_v1_layout() {
    let info = d(&[
        ("file tree", d(&[("x", tree_leaf(10))])),
        ("length", i(10)),
        ("meta version", i(2)),
        ("name", s(b"x")),
        ("piece length", i(PL as i64)),
        ("pieces", pieces_for(10, PL)),
    ]);
    let m = parse(info.clone());
    assert_eq!(m.version, MetaVersion::Hybrid);
    assert_eq!(m.info_hash.v1_hex().unwrap(), sha1_hex(&info));
    assert!(m.info_hash.v2.is_some());
    assert_eq!(m.piece_count(), 1);
    assert_eq!(m.info_hash.swarm_id(), m.info_hash.v1);
    assert!(m.info_hash.verifies(&info));
}

#[test]
fn half_v1_layout_in_v2_is_malformed() {
    let base = |extra: (&str, Vec<u8>)| {
        d(&[
            ("file tree", d(&[("x", tree_leaf(10))])),
            ("meta version", i(2)),
            ("name", s(b"x")),
            ("piece length", i(PL as i64)),
            extra,
        ])
    };
    assert_eq!(
        parse_err(base(("pieces", pieces_for(10, PL)))),
        Error::MissingKey("length")
    );
    assert_eq!(
        parse_err(base(("length", i(10)))),
        Error::MissingKey("pieces")
    );
}

// ------------------------------------------------------------ announce & extras

fn ann(extra: &[(&str, Vec<u8>)]) -> Metainfo {
    Metainfo::from_bytes(&torrent(info_single(b"x", 10), extra)).unwrap()
}

fn tiers(m: &Metainfo) -> Vec<Vec<&str>> {
    m.announce_tiers
        .iter()
        .map(|t| t.iter().map(|u| &**u).collect())
        .collect()
}

#[test]
fn announce_fallback_and_tiers() {
    let m = ann(&[("announce", s(b"http://a/ann"))]);
    assert_eq!(tiers(&m), [["http://a/ann"]]);

    let m = ann(&[
        ("announce", s(b"http://ignored/ann")),
        (
            "announce-list",
            l(&[
                l(&[s(b"http://a/ann"), s(b"udp://b:80/")]),
                l(&[s(b"https://c/ann")]),
            ]),
        ),
    ]);
    // BEP 12: announce-list wins over announce.
    assert_eq!(
        tiers(&m),
        [vec!["http://a/ann", "udp://b:80/"], vec!["https://c/ann"]]
    );

    // No trackers at all is valid (DHT-only).
    assert!(ann(&[]).announce_tiers.is_empty());
}

#[test]
fn announce_dedupe_and_empty_tiers() {
    let m = ann(&[(
        "announce-list",
        l(&[
            l(&[s(b"http://a/ann"), s(b"http://a/ann")]),
            l(&[]),
            l(&[s(b"http://a/ann"), s(b"http://b/ann")]),
            l(&[s(b"")]),
        ]),
    )]);
    assert_eq!(tiers(&m), [vec!["http://a/ann"], vec!["http://b/ann"]]);
}

#[test]
fn announce_unusable_urls_warn_and_fall_back() {
    let m = ann(&[
        ("announce", s(b"http://fallback/ann")),
        (
            "announce-list",
            l(&[l(&[
                s(b"wss://x/"),
                s(b"ftp://y/"),
                s(b"garbage"),
                s(b"http://"),
            ])]),
        ),
    ]);
    assert_eq!(tiers(&m), [["http://fallback/ann"]]);
    let ignored: Vec<_> = m
        .warnings
        .iter()
        .filter_map(|w| {
            if let Warning::IgnoredTracker(u) = w {
                Some(&**u)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(ignored, ["wss://x/", "ftp://y/", "garbage", "http://"]);
}

#[test]
fn announce_scheme_case_whitespace_and_non_utf8() {
    let m = ann(&[(
        "announce-list",
        l(&[l(&[
            s(b"  HTTP://Upper/ann \n"),
            s(b"UDP://u:1/"),
            s(b"http://bad\xff/"),
        ])]),
    )]);
    assert_eq!(tiers(&m), [vec!["HTTP://Upper/ann", "UDP://u:1/"]]);
    assert!(
        m.warnings
            .iter()
            .any(|w| matches!(w, Warning::IgnoredTracker(u) if u.contains('\u{fffd}')))
    );
}

#[test]
fn announce_flat_list_and_wrong_types_tolerated() {
    let m = ann(&[(
        "announce-list",
        l(&[s(b"http://flat/a"), i(5), l(&[i(1), s(b"http://ok/")])]),
    )]);
    assert_eq!(tiers(&m), [vec!["http://flat/a"], vec!["http://ok/"]]);
    assert!(
        ann(&[("announce", i(5)), ("announce-list", i(5))])
            .announce_tiers
            .is_empty()
    );
}

#[test]
fn tracker_caps() {
    let many: Vec<Vec<u8>> = (0..400)
        .map(|n| s(format!("http://h{n}/a").as_bytes()))
        .collect();
    let m = ann(&[("announce-list", l(&[l(&many)]))]);
    let total: usize = m.announce_tiers.iter().map(|t| t.len()).sum();
    assert_eq!(total, MAX_TRACKERS);
    assert!(m.warnings.contains(&Warning::TooManyUrls));

    let tiers_many: Vec<Vec<u8>> = (0..100)
        .map(|n| l(&[s(format!("http://t{n}/a").as_bytes())]))
        .collect();
    let m = ann(&[("announce-list", l(&tiers_many))]);
    assert_eq!(m.announce_tiers.len(), MAX_TIERS);
    assert!(m.warnings.contains(&Warning::TooManyUrls));
}

#[test]
fn url_list_string_list_and_filtering() {
    let m = ann(&[("url-list", s(b"https://mirror/dir/"))]);
    assert_eq!(
        &*m.url_list,
        ["https://mirror/dir/".into()] as [Box<str>; 1]
    );
    let m = ann(&[(
        "url-list",
        l(&[
            s(b"http://a/"),
            s(b"http://a/"),
            s(b"ftp://b/"),
            s(b"https://c/"),
            i(1),
        ]),
    )]);
    assert_eq!(
        m.url_list.iter().map(|u| &**u).collect::<Vec<_>>(),
        ["http://a/", "https://c/"]
    );
    assert!(
        m.warnings
            .contains(&Warning::IgnoredWebSeed("ftp://b/".into()))
    );
    assert!(ann(&[("url-list", i(3))]).url_list.is_empty());
    let many: Vec<Vec<u8>> = (0..200)
        .map(|n| s(format!("http://w{n}/").as_bytes()))
        .collect();
    assert_eq!(ann(&[("url-list", l(&many))]).url_list.len(), MAX_WEB_SEEDS);
}

#[test]
fn optional_metadata_fields() {
    let m = ann(&[
        ("comment", s(b"hello")),
        ("created by", s(b"mktorrent 1.1")),
        ("creation date", i(1_700_000_000)),
    ]);
    assert_eq!(m.comment.as_deref(), Some("hello"));
    assert_eq!(m.created_by.as_deref(), Some("mktorrent 1.1"));
    assert_eq!(m.creation_date, Some(1_700_000_000));
    assert_eq!(m.source, None);

    let m = ann(&[
        ("comment", s(b"bad\xff")),
        ("comment.utf-8", s(b"good")),
        ("creation date", s(b"x")),
    ]);
    assert_eq!(m.comment.as_deref(), Some("good"));
    assert_eq!(m.creation_date, None);
    let m = ann(&[("comment", s(b"bad\xff"))]);
    assert_eq!(m.comment.as_deref(), Some("bad\u{fffd}"));
    let m = ann(&[("comment", s(b"")), ("created by", i(1))]);
    assert_eq!((m.comment, m.created_by), (None, None));
}

#[test]
fn private_flag_values() {
    let mk = |v: i64| {
        parse(d(&[
            ("length", i(10)),
            ("name", s(b"x")),
            ("piece length", i(PL as i64)),
            ("pieces", pieces_for(10, PL)),
            ("private", i(v)),
        ]))
        .private
    };
    assert!(mk(1));
    assert!(!mk(0));
    // Any non-zero value is treated as private; failing closed protects the user.
    assert!(mk(2));
}

#[test]
fn large_multi_file_torrent_parses() {
    let n = 30_000usize;
    let files: Vec<Vec<u8>> = (0..n)
        .map(|k| {
            file(
                1,
                &[
                    format!("dir{}", k % 100).as_bytes(),
                    format!("f{k}").as_bytes(),
                ],
            )
        })
        .collect();
    let m = parse(info_multi(b"big", files, n as u64));
    assert_eq!(m.file_count(), n);
    assert_eq!(m.file(n - 1).unwrap().offset, n as u64 - 1);
    assert_eq!(m.file(12345).unwrap().path, "dir45/f12345");
}

#[test]
fn struct_size_is_documented_value() {
    // Keep in sync with the `Metainfo` doc comment.
    assert_eq!(std::mem::size_of::<Metainfo>(), SIZE_OF_METAINFO);
    assert_eq!(std::mem::size_of::<InfoHash>(), 54);
    assert_eq!(std::mem::size_of::<PeerId>(), 20);
}
const SIZE_OF_METAINFO: usize = 280;

// ------------------------------------------------------------ hashes & ids

#[test]
fn hex_and_base32_roundtrip_and_vectors() {
    assert_eq!(hex_encode(&[0x00, 0xab, 0xff]), "00abff");
    assert_eq!(hex_decode::<3>("00ABff"), Some([0, 0xab, 0xff]));
    assert_eq!(hex_decode::<3>("00abf"), None);
    assert_eq!(hex_decode::<3>("00abfg"), None);
    assert_eq!(hex_decode::<3>("00abff00"), None);
    // RFC 4648 test vectors (without padding).
    assert_eq!(base32_encode(b""), "");
    assert_eq!(base32_encode(b"f"), "MY");
    assert_eq!(base32_encode(b"fo"), "MZXQ");
    assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
    assert_eq!(base32_decode::<6>("MZXW6YTBOI"), Some(*b"foobar"));
    assert_eq!(base32_decode::<6>("mzxw6ytboi"), Some(*b"foobar"));
    assert_eq!(base32_decode::<1>("MY"), Some(*b"f"));
    assert_eq!(base32_decode::<1>("MZ"), None, "non-zero trailing bits");
    assert_eq!(base32_decode::<1>("M1"), None);
    assert_eq!(base32_decode::<1>("MY======"), None);
    assert_eq!(base32_decode::<1>("M"), None);
}

proptest! {
    #[test]
    fn hex_base32_roundtrip(h in any::<[u8; 20]>()) {
        prop_assert_eq!(hex_decode::<20>(&hex_encode(&h)), Some(h));
        prop_assert_eq!(base32_decode::<20>(&base32_encode(&h)), Some(h));
        prop_assert_eq!(base32_encode(&h).len(), 32);
        prop_assert_eq!(InfoHash::parse_v1(&hex_encode(&h)), Some(InfoHash::from_v1(h)));
        prop_assert_eq!(InfoHash::parse_v1(&base32_encode(&h)), Some(InfoHash::from_v1(h)));
    }
}

#[test]
fn info_hash_helpers() {
    let h = InfoHash::from_v1([0xab; 20]);
    assert_eq!(h.to_string(), "ab".repeat(20));
    assert_eq!(h.v1_hex().unwrap().len(), 40);
    assert_eq!(h.v1_base32().unwrap().len(), 32);
    assert_eq!(h.v2_hex(), None);
    assert_eq!(h.swarm_id(), Some([0xab; 20]));
    let v2 = InfoHash::from_v2([0x11; 32]);
    assert_eq!(v2.to_string(), "11".repeat(32));
    assert_eq!(v2.swarm_id(), Some([0x11; 20]));
    let both = InfoHash {
        v1: h.v1,
        v2: v2.v2,
    };
    assert_eq!(both.to_string(), h.to_string(), "hybrid displays v1");
    assert!(both.overlaps(&h) && both.overlaps(&v2) && h.overlaps(&both));
    assert!(!h.overlaps(&v2));
    assert!(!InfoHash::default().overlaps(&InfoHash::default()));
    assert_eq!(InfoHash::default().swarm_id(), None);
    assert_eq!(InfoHash::default().to_string(), "<empty>");
    assert!(format!("{h:?}").contains(&"ab".repeat(20)));
    assert_eq!(InfoHash::parse_v1("zz"), None);
}

#[test]
fn peer_id_shape() {
    let a = PeerId::generate();
    let b = PeerId::generate();
    assert_ne!(a, b, "two ids collide: entropy source broken");
    for p in [a, b] {
        assert_eq!(&p.0[..8], &PEER_ID_PREFIX);
        assert_eq!(p.as_bytes().len(), 20);
        assert!(p.0[8..].iter().all(u8::is_ascii_alphanumeric));
    }
    // Tests are never release builds.
    assert_eq!(&PEER_ID_PREFIX, b"-SX0001-");
}

#[test]
fn release_peer_id_prefix_follows_the_version() {
    use crate::hash::release_prefix;
    assert_eq!(&release_prefix("0.1.0"), b"-SW0100-");
    assert_eq!(&release_prefix("1.2.3"), b"-SW1230-");
    assert_eq!(&release_prefix("0.1.0-rc1"), b"-SW0100-");
}

#[test]
fn peer_id_rejection_sampling_and_degenerate_rng() {
    // 0..=247 map uniformly onto the alphabet; 248..=255 are rejected.
    let mut n = 0u8;
    let p = PeerId::generate_with(|buf| {
        for b in buf {
            *b = n;
            n = n.wrapping_add(1);
        }
    });
    assert_eq!(&p.0[8..], b"0123456789AB");
    // All-rejected randomness must terminate and still yield valid characters.
    let p = PeerId::generate_with(|buf| buf.fill(0xff));
    assert!(p.0[8..].iter().all(u8::is_ascii_alphanumeric));
    // Boundary values.
    let p = PeerId::generate_with(|buf| buf.fill(247));
    assert!(
        p.0[8..]
            .iter()
            .all(|&c| c == b'z' || c.is_ascii_alphanumeric())
    );
}

// ------------------------------------------------------------ magnet

const H1: &str = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";

#[test]
fn magnet_hex_btih() {
    let m = Magnet::parse(&format!("magnet:?xt=urn:btih:{H1}")).unwrap();
    assert_eq!(m.info_hash.v1_hex().unwrap(), H1);
    assert_eq!(m.info_hash.v2, None);
    assert!(m.trackers.is_empty() && m.display_name.is_none() && m.exact_length.is_none());
    // Upper-case hex, upper-case scheme and urn.
    let m2 = Magnet::parse(&format!("MAGNET:?XT=URN:BTIH:{}", H1.to_uppercase())).unwrap();
    assert_eq!(m.info_hash, m2.info_hash);
}

#[test]
fn magnet_base32_btih() {
    let raw: [u8; 20] = hex_decode(H1).unwrap();
    let b32 = base32_encode(&raw);
    for v in [b32.clone(), b32.to_lowercase()] {
        let m = Magnet::parse(&format!("magnet:?xt=urn:btih:{v}")).unwrap();
        assert_eq!(m.info_hash.v1, Some(raw));
    }
}

#[test]
fn magnet_btmh_and_hybrid() {
    let v2 = "ab".repeat(32);
    let m = Magnet::parse(&format!("magnet:?xt=urn:btmh:1220{v2}")).unwrap();
    assert_eq!(m.info_hash.v1, None);
    assert_eq!(m.info_hash.v2_hex().unwrap(), v2);
    let m = Magnet::parse(&format!("magnet:?xt=urn:btih:{H1}&xt=urn:btmh:1220{v2}")).unwrap();
    assert_eq!(m.info_hash.v1_hex().unwrap(), H1);
    assert_eq!(m.info_hash.v2_hex().unwrap(), v2);
    // Numbered form.
    let m = Magnet::parse(&format!(
        "magnet:?xt.1=urn:btih:{H1}&xt.2=urn:btmh:1220{v2}"
    ))
    .unwrap();
    assert!(m.info_hash.v1.is_some() && m.info_hash.v2.is_some());
}

#[test]
fn magnet_full_parameters() {
    let uri = format!(
        "magnet:?xt=urn:btih:{H1}&dn=My+Cool%20File%C3%A9&tr=udp%3A%2F%2Ftracker.example%3A80%2Fannounce\
         &tr=http%3A%2F%2Fb.example%2Fann%3Fpk%3Da%2Bb&tr.1=wss%3A%2F%2Fno&tr=udp%3A%2F%2Ftracker.example%3A80%2Fannounce\
         &ws=https%3A%2F%2Fmirror.example%2Ff&ws=ftp%3A%2F%2Fx&xl=123456789&so=0,2,4-6,9-&x.pe=1.2.3.4:6881\
         &x.pe=[::1]:51413&x.pe=bogus&unknown=1&kt=foo#frag"
    );
    let m = Magnet::parse(&uri).unwrap();
    assert_eq!(m.display_name.as_deref(), Some("My Cool Fileé"));
    assert_eq!(
        m.trackers,
        [
            "udp://tracker.example:80/announce",
            "http://b.example/ann?pk=a+b"
        ]
    );
    assert_eq!(m.web_seeds, ["https://mirror.example/f"]);
    assert_eq!(m.exact_length, Some(123_456_789));
    assert_eq!(
        m.select_only,
        [
            FileRange { start: 0, end: 0 },
            FileRange { start: 2, end: 2 },
            FileRange { start: 4, end: 6 },
            FileRange {
                start: 9,
                end: u32::MAX
            },
        ]
    );
    assert_eq!(m.peers.len(), 2);
    assert!(
        m.warnings
            .contains(&Warning::IgnoredTracker("wss://no".into()))
    );
    assert!(
        m.warnings
            .contains(&Warning::IgnoredWebSeed("ftp://x".into()))
    );
}

#[test]
fn magnet_percent_encoded_so_and_xl() {
    let m = Magnet::parse(&format!("magnet:?xt=urn:btih:{H1}&so=1%2C3-4&xl=%35")).unwrap();
    assert_eq!(m.select_only.len(), 2);
    assert_eq!(m.exact_length, Some(5));
}

#[test]
fn magnet_errors() {
    use MagnetError::*;
    let p = |s: &str| Magnet::parse(s).unwrap_err();
    assert_eq!(p(""), NotMagnet);
    assert_eq!(p("http://example.com"), NotMagnet);
    assert_eq!(p("magnet:"), NotMagnet);
    assert_eq!(p("magnet:?"), MissingInfoHash);
    assert_eq!(p("magnet:?dn=x"), MissingInfoHash);
    assert_eq!(p("magnet:?xt=urn:sha1:abc"), MissingInfoHash);
    assert_eq!(p("magnet:?xt=urn:btih:"), InvalidInfoHash);
    assert_eq!(p("magnet:?xt=urn:btih:abc"), InvalidInfoHash);
    assert_eq!(p(&format!("magnet:?xt=urn:btih:{}0", H1)), InvalidInfoHash);
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{}", "g".repeat(40))),
        InvalidInfoHash
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{}", "1".repeat(32))),
        InvalidInfoHash
    ); // '1' not in base32
    assert_eq!(
        p(&format!("magnet:?xt=urn:btmh:1220{}", "ab".repeat(31))),
        InvalidInfoHash
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btmh:1114{}", "ab".repeat(32))),
        InvalidInfoHash
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btmh:{}", "ab".repeat(34))),
        InvalidInfoHash
    );
    let other = "ab".repeat(20);
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&xt=urn:btih:{other}")),
        ConflictingInfoHash
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&dn=%zz")),
        InvalidPercentEncoding
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&dn=abc%2")),
        InvalidPercentEncoding
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&dn=abc%")),
        InvalidPercentEncoding
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&xl=abc")),
        InvalidParameter("xl")
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&xl=-1")),
        InvalidParameter("xl")
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&so=a")),
        InvalidParameter("so")
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&so=5-2")),
        InvalidParameter("so")
    );
    assert_eq!(
        p(&format!("magnet:?xt=urn:btih:{H1}&so=-3")),
        InvalidParameter("so")
    );
    let long = format!("magnet:?xt=urn:btih:{H1}&dn={}", "a".repeat(MAX_MAGNET_LEN));
    assert_eq!(p(&long), TooLong);
    // Multi-byte char straddling the prefix boundary must not panic.
    assert_eq!(p("magnet\u{e9}?x"), NotMagnet);
}

#[test]
fn magnet_tolerates_junk_parameters_and_whitespace() {
    let m = Magnet::parse(&format!("  magnet:?&&xt=urn:btih:{H1}&novalue&=x&dn=\n")).unwrap();
    assert_eq!(m.info_hash.v1_hex().unwrap(), H1);
    assert_eq!(m.display_name.as_deref(), Some(""));
    // Non-UTF-8 dn is repaired, non-UTF-8 tracker is dropped with a warning.
    let m = Magnet::parse(&format!(
        "magnet:?xt=urn:btih:{H1}&dn=a%FFb&tr=http%3A%2F%2F%FF"
    ))
    .unwrap();
    assert_eq!(m.display_name.as_deref(), Some("a\u{fffd}b"));
    assert!(m.trackers.is_empty() && !m.warnings.is_empty());
}

#[test]
fn magnet_caps() {
    let mut uri = format!("magnet:?xt=urn:btih:{H1}");
    for n in 0..300 {
        uri.push_str(&format!("&tr=http://h{n}/a"));
    }
    let m = Magnet::parse(&uri).unwrap();
    assert_eq!(m.trackers.len(), MAX_TRACKERS);
    assert!(m.warnings.contains(&Warning::TooManyUrls));
    let mut uri = format!("magnet:?xt=urn:btih:{H1}&so=");
    for n in 0..2000 {
        uri.push_str(&format!("{n},"));
    }
    assert_eq!(
        Magnet::parse(&uri).unwrap_err(),
        MagnetError::InvalidParameter("so")
    );
}

#[test]
fn magnet_to_uri_roundtrip_and_tracker_hiding() {
    let v2 = "cd".repeat(32);
    let uri = format!(
        "magnet:?xt=urn:btih:{H1}&xt=urn:btmh:1220{v2}&dn=A%20B%26C&tr=https%3A%2F%2Fp.example%2Fann%3Fpasskey%3Dsecret&xl=10"
    );
    let m = Magnet::parse(&uri).unwrap();
    let with = m.to_uri(true);
    assert_eq!(Magnet::parse(&with).unwrap(), m);
    let without = m.to_uri(false);
    assert!(!without.contains("secret") && !without.contains("tr="));
    let back = Magnet::parse(&without).unwrap();
    assert_eq!(back.info_hash, m.info_hash);
    assert_eq!(back.display_name.as_deref(), Some("A B&C"));
}

// ------------------------------------------------------------ fuzz-ish properties

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn from_bytes_never_panics_on_garbage(data in prop::collection::vec(any::<u8>(), 0..512)) {
        let _ = Metainfo::from_bytes(&data);
        let _ = Metainfo::from_info_bytes(&data);
    }

    #[test]
    fn from_bytes_never_panics_on_mutated_torrents(
        pos in 0usize..400, byte in any::<u8>(), cut in 0usize..400
    ) {
        let base = torrent(
            info_multi(b"r", vec![file(5, &[b"a"]), file_attr(5, &[b".pad", b"5"], "p")], 10),
            &[("announce", s(b"http://t/a")), ("url-list", s(b"http://w/"))],
        );
        let mut t = base.clone();
        let p = pos % t.len();
        t[p] = byte;
        let _ = Metainfo::from_bytes(&t);
        t.truncate(cut % (base.len() + 1));
        let _ = Metainfo::from_bytes(&t);
    }

    #[test]
    fn magnet_parse_never_panics(s in "\\PC{0,200}") {
        let _ = Magnet::parse(&s);
        let _ = Magnet::parse(&format!("magnet:?{s}"));
    }

    #[test]
    fn sanitize_output_is_always_safe(raw in prop::collection::vec(any::<u8>(), 0..300)) {
        if let Ok((c, _)) = sanitize_component(&raw) {
            prop_assert!(!c.is_empty());
            prop_assert!(!c.contains('/') && !c.contains('\0') && !c.contains(':'));
            prop_assert!(c != "." && c != "..");
            prop_assert!(c.len() <= MAX_COMPONENT_BYTES);
        }
    }

    #[test]
    fn accepted_torrent_paths_are_always_relative_and_clean(
        comps in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..20), 1..5)
    ) {
        let refs: Vec<&[u8]> = comps.iter().map(|c| c.as_slice()).collect();
        let t = torrent(info_multi(b"root", vec![file(1, &refs)], 1), &[]);
        if let Ok(m) = Metainfo::from_bytes(&t) {
            for f in m.files() {
                prop_assert!(!f.path.starts_with('/'));
                for c in f.components() {
                    prop_assert!(!c.is_empty() && c != "." && c != ".." && !c.contains(':'));
                }
            }
        }
    }
}
