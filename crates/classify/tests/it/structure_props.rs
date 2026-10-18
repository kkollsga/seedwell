//! Property tests for `structure()`: it never panics, and every file index appears exactly once.

use proptest::prelude::*;
use trs_classify::{Category, FileEntry, Input, Structure, classify, structure};

const COMPS: &[&str] = &[
    "Season 1",
    "Season 02",
    "S03",
    "Specials",
    "Extras",
    "Sample",
    "Subs",
    "CD1",
    "Disc 2",
    "Show",
    "Artist",
    "Album (2007)",
    "BDMV",
    "VIDEO_TS",
    "macOS",
    "Windows",
    "App.app",
    "Contents",
    "Linux",
    "Show.S01E02.1080p",
    "Movie.1999.1080p",
    "2003 - Name",
    "x",
    "..",
    "",
    "a b",
    "ünï",
];
const STEMS: &[&str] = &[
    "Show.S01E02",
    "Show.S01E03E04",
    "Show.S01E01-E03",
    "Show.S02E10.en",
    "E05",
    "Show - 07",
    "Show.2026.10.11",
    "Movie.1999.1080p.BluRay",
    "Movie.cd1",
    "Movie.cd2",
    "01 - Track",
    "Chapter 03",
    "Book Title",
    "App-1.2-arm64",
    "App-1.2-x86_64",
    "setup",
    "cover",
    "poster",
    "sample",
    "readme",
    "S00E01",
    "Title.part01",
    "x",
    "",
];
const EXTS: &[&str] = &[
    "mkv", "mp4", "srt", "flac", "mp3", "m4b", "epub", "pdf", "dmg", "pkg", "exe", "msi",
    "AppImage", "deb", "zip", "iso", "r00", "jpg", "nfo", "txt", "cue", "", "m2ts", "plist",
];

fn path() -> impl Strategy<Value = String> {
    prop_oneof![
        (
            proptest::collection::vec(0..COMPS.len(), 0..4),
            0..STEMS.len(),
            0..EXTS.len()
        )
            .prop_map(|(d, s, e)| {
                let mut p = String::new();
                for i in d {
                    p.push_str(COMPS[i]);
                    p.push('/');
                }
                p.push_str(STEMS[s]);
                if !EXTS[e].is_empty() {
                    p.push('.');
                    p.push_str(EXTS[e]);
                }
                p
            }),
        ".{0,60}",
    ]
}

fn check(s: &Structure, n: usize, sizes: &[u64]) -> Result<(), TestCaseError> {
    let mut seen = vec![0u8; n];
    for (_, it) in s.items() {
        prop_assert!(it.file_indices.windows(2).all(|w| w[0] < w[1]));
        let sum = it
            .file_indices
            .iter()
            .fold(0u64, |a, &i| a.saturating_add(sizes[i as usize]));
        prop_assert_eq!(it.bytes, sum);
        prop_assert!(it.label.len() <= 120);
        for &i in &it.file_indices {
            prop_assert!((i as usize) < n);
            seen[i as usize] += 1;
        }
    }
    for &i in s.extras.iter().chain(&s.unassigned) {
        prop_assert!((i as usize) < n);
        seen[i as usize] += 1;
    }
    prop_assert!(seen.iter().all(|&c| c == 1), "{seen:?}");
    // helpers are consistent with the data
    if let Some((_, it)) = s.items().next() {
        prop_assert_eq!(s.item_of_file(it.file_indices[0]).is_some(), true);
    }
    prop_assert!(
        s.total_bytes()
            == s.groups
                .iter()
                .map(|g| g.bytes())
                .fold(0u64, u64::saturating_add)
    );
    let _ = s.this_mac_items();
    let _ = s.has_platform_variants();
    let _ = s.seasons();
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 500, ..ProptestConfig::default() })]

    #[test]
    fn partition_for_every_category(
        name in prop_oneof!["[a-zA-Z0-9 ._()\\[\\]-]{0,80}", ".{0,100}"],
        files in proptest::collection::vec((path(), 0u64..u64::MAX), 0..60),
    ) {
        let entries: Vec<FileEntry> = files.iter().map(|(p, s)| FileEntry { path: p, size: *s }).collect();
        let sizes: Vec<u64> = files.iter().map(|f| f.1).collect();
        let c = classify(&Input::new(&name, &entries));
        for cat in Category::ALL {
            let mut cc = c.clone();
            cc.category = cat;
            let s = structure(&cc, &entries);
            check(&s, entries.len(), &sizes)?;
            // determinism
            prop_assert_eq!(&s, &structure(&cc, &entries));
        }
    }

    #[test]
    fn hostile_junk_indexes_and_names(
        files in proptest::collection::vec((".{0,40}", any::<u64>()), 0..20),
        junk in proptest::collection::vec(0usize..100, 0..10),
    ) {
        let entries: Vec<FileEntry> = files.iter().map(|(p, s)| FileEntry { path: p, size: *s }).collect();
        let sizes: Vec<u64> = files.iter().map(|f| f.1).collect();
        let mut c = classify(&Input::new("x", &entries));
        for j in junk {
            c.junk_files.push(trs_classify::JunkFile { index: j, kind: trs_classify::JunkKind::Nfo });
        }
        for cat in Category::ALL {
            c.category = cat;
            check(&structure(&c, &entries), entries.len(), &sizes)?;
        }
    }
}
