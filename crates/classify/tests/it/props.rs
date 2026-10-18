//! Property tests: `classify` never panics and keeps its invariants on hostile input;
//! collision suffixes stay within the file-name limit.

use proptest::prelude::*;
use trs_classify::{
    Classification, FileEntry, Input, classify, collision_suffix, unique_path,
};

const WORDS: &[&str] = &[
    "The",
    "Night",
    "of",
    "Living",
    "Dead",
    "1968",
    "2019",
    "2160p",
    "1080p",
    "720p",
    "BluRay",
    "WEB-DL",
    "x264",
    "x265",
    "HEVC",
    "S01E02",
    "S01",
    "1x02",
    "Season",
    "2",
    "Complete",
    "Series",
    "FLAC",
    "MP3",
    "320",
    "Artist",
    "-",
    "Album",
    "(2005)",
    "[FLAC]",
    "[1080p]",
    "epub",
    "pdf",
    "Audiobook",
    "Unabridged",
    "v1.2.3",
    "arm64",
    "x64",
    "Setup",
    "dmg",
    "iso",
    "Sample",
    "NFO",
    "by",
    "Narrated",
    "Edition",
    "3rd",
    "[Group]",
    "(US)",
    "2026.10.11",
    "Vol.2",
    "Part",
    "1",
    "07",
    "Extended",
    "Director's",
    "Cut",
    "REPACK",
    "www.site.com",
    "booklib.org",
    "..",
    "/",
    "\\",
    ":",
    "",
];
const SEPS: &[&str] = &[" ", ".", "_", "-", " - ", "  ", ".-."];
const EXTS: &[&str] = &[
    "mkv", "mp4", "flac", "mp3", "m4b", "epub", "pdf", "dmg", "iso", "exe", "nfo", "txt", "srt",
    "jpg", "zip", "r00", "app", "",
];

fn name_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        // structured: vocabulary joined by random separators
        proptest::collection::vec((0..WORDS.len(), 0..SEPS.len()), 0..24).prop_map(|v| {
            let mut s = String::new();
            for (w, sep) in v {
                s.push_str(WORDS[w]);
                s.push_str(SEPS[sep]);
            }
            s
        }),
        // anything at all
        ".{0,300}",
        // long
        "[a-zA-Z0-9 ._()\\[\\]-]{500,3000}",
    ]
}

fn path_strategy() -> impl Strategy<Value = String> {
    prop_oneof![
        (
            proptest::collection::vec(0..WORDS.len(), 1..4),
            0..EXTS.len(),
            0usize..3
        )
            .prop_map(|(w, e, depth)| {
                let mut p = String::new();
                for _ in 0..depth {
                    p.push_str("dir/");
                }
                p.push_str(&w.iter().map(|&i| WORDS[i]).collect::<Vec<_>>().join(" "));
                if !EXTS[e].is_empty() {
                    p.push('.');
                    p.push_str(EXTS[e]);
                }
                p
            }),
        ".{0,80}",
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

    #[test]
    fn classify_never_panics_and_keeps_invariants(
        name in name_strategy(),
        files in proptest::collection::vec((path_strategy(), 0u64..u64::MAX / 4), 0..40),
        hint in proptest::option::of(".{0,30}"),
        ctype in proptest::option::of("[a-z/+.-]{0,40}"),
    ) {
        let entries: Vec<FileEntry> = files.iter().map(|(p, s)| FileEntry { path: p, size: *s }).collect();
        let input = Input { name: &name, files: &entries, tracker_source_hint: hint.as_deref(), content_type_hint: ctype.as_deref() };
        let c = classify(&input);
        prop_assert!((0.0..=1.0).contains(&c.confidence));
        prop_assert_eq!(c.ask, c.confidence < Classification::ASK_THRESHOLD);
        let mut seen = std::collections::HashSet::new();
        for j in &c.junk_files {
            prop_assert!(j.index < entries.len());
            prop_assert!(seen.insert(j.index), "duplicate junk index");
        }
        // deterministic
        prop_assert_eq!(&classify(&input), &c);
    }

    #[test]
    fn collisions_stay_within_limits(stem in "[a-zA-Z0-9 ()]{1,400}", ext in "[a-z0-9]{0,6}", n in 2u32..9999) {
        let file = if ext.is_empty() { stem.clone() } else { format!("{stem}.{ext}") };
        let file = file.trim().to_owned();
        let path = format!("dir/{file}");
        let out = collision_suffix(&path, n);
        prop_assert!(out.rsplit('/').next().unwrap().len() <= 255);
        prop_assert!(out.starts_with("dir/"));
        let u = unique_path(&path, |p| p == path);
        prop_assert!(u != path);
    }
}
