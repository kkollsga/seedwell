//! `structure()` over the labelled corpus: the partition invariant on every row (with the
//! classifier's category and with the row's labelled category), plus simple oracles that do not
//! depend on the implementation (single files are never "several items", a single installer has
//! no platform variants, a pack of episode-named files is several items).
//!
//! The row format is the one of `tests/corpus.rs`.

use trs_classify::{Category, FileEntry, Input, Structure, StructureKind, classify, structure};

struct Row {
    line: usize,
    expect: Category,
    name: String,
    files: Vec<(String, u64)>,
    hint: Option<String>,
    ctype: Option<String>,
    #[allow(dead_code)]
    fields: Vec<(String, String)>,
}

fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1e3),
        Some('M' | 'm') => (&s[..s.len() - 1], 1e6),
        Some('G' | 'g') => (&s[..s.len() - 1], 1e9),
        Some('T' | 't') => (&s[..s.len() - 1], 1e12),
        _ => (s, 1.0),
    };
    (num.parse::<f64>()
        .unwrap_or_else(|_| panic!("bad size {s:?}"))
        * mul) as u64
}

fn expand(path: &str) -> Vec<String> {
    if let (Some(a), Some(b)) = (path.find('{'), path.find('}')) {
        let inner = &path[a + 1..b];
        if let Some((lo, hi)) = inner.split_once("..") {
            let width = lo.len();
            let (lo, hi): (u32, u32) = (lo.parse().unwrap(), hi.parse().unwrap());
            return (lo..=hi)
                .flat_map(|n| {
                    expand(&format!(
                        "{}{:0w$}{}",
                        &path[..a],
                        n,
                        &path[b + 1..],
                        w = width
                    ))
                })
                .collect();
        }
    }
    vec![path.to_owned()]
}

fn load(file: &str) -> Vec<Row> {
    let text = std::fs::read_to_string(format!(
        "{}/tests/fixtures/{file}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .expect("corpus");
    let mut rows = Vec::new();
    for (ln, line) in text.lines().enumerate() {
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let c: Vec<&str> = line.split('\t').collect();
        assert!(c.len() >= 3, "line {}: need >= 3 columns", ln + 1);
        let expect = Category::parse(c[0])
            .unwrap_or_else(|| panic!("line {}: bad category {:?}", ln + 1, c[0]));
        let name = c[1].to_owned();
        let files = match c[2].trim() {
            "-" => vec![],
            "=" => vec![(name.clone(), 1_000_000_000)],
            spec => spec
                .split(';')
                .filter(|s| !s.trim().is_empty())
                .flat_map(|item| {
                    let (p, sz) = item
                        .rsplit_once(':')
                        .unwrap_or_else(|| panic!("line {}: file item {item:?}", ln + 1));
                    let sz = parse_size(sz);
                    expand(p.trim()).into_iter().map(move |p| (p, sz))
                })
                .collect(),
        };
        let opt = |i: usize| {
            c.get(i)
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && *s != "-")
                .map(str::to_owned)
        };
        let fields = opt(5)
            .map(|f| {
                f.split('|')
                    .map(|kv| {
                        let (k, v) = kv
                            .split_once('=')
                            .unwrap_or_else(|| panic!("line {}: field {kv:?}", ln + 1));
                        (k.trim().to_owned(), v.trim().to_owned())
                    })
                    .collect()
            })
            .unwrap_or_default();
        rows.push(Row {
            line: ln + 1,
            expect,
            name,
            files,
            hint: opt(3),
            ctype: opt(4),
            fields,
        });
    }
    rows
}

fn assert_partition(s: &Structure, n: usize, what: &str) {
    let mut seen = vec![0u8; n];
    for (_, it) in s.items() {
        assert!(
            it.file_indices.windows(2).all(|w| w[0] < w[1]),
            "{what}: unsorted item files"
        );
        for &i in &it.file_indices {
            seen[i as usize] += 1;
        }
    }
    for &i in s.extras.iter().chain(&s.unassigned) {
        seen[i as usize] += 1;
    }
    assert!(
        seen.iter().all(|&c| c == 1),
        "{what}: partition broken {seen:?}"
    );
}

fn has_episode_marker(p: &str) -> bool {
    let b = p.as_bytes();
    (0..b.len().saturating_sub(5)).any(|i| {
        matches!(b[i], b'S' | b's')
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
            && matches!(b[i + 3], b'E' | b'e')
            && b[i + 4].is_ascii_digit()
    })
}

fn is_video(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    [".mkv", ".mp4", ".avi", ".m4v"]
        .iter()
        .any(|e| l.ends_with(e))
}

#[test]
fn corpus_structure_invariants() {
    let mut rows = 0;
    let mut with_items = 0;
    let mut multi_rows = 0;
    for file in ["corpus.tsv", "holdout.tsv", "blind.tsv"] {
        for r in load(file) {
            rows += 1;
            let entries: Vec<FileEntry> = r
                .files
                .iter()
                .map(|(p, s)| FileEntry { path: p, size: *s })
                .collect();
            let input = Input {
                name: &r.name,
                files: &entries,
                tracker_source_hint: r.hint.as_deref(),
                content_type_hint: r.ctype.as_deref(),
            };
            let c = classify(&input);
            let what = format!("{file}:{} {}", r.line, r.name);
            let s = structure(&c, &entries);
            assert_partition(&s, entries.len(), &what);
            if s.item_count() > 0 {
                with_items += 1;
            }
            if s.multi {
                multi_rows += 1;
            }
            // the junk list is honoured: junk is never part of an item
            for j in &c.junk_files {
                assert!(
                    s.extras.contains(&(j.index as u32)),
                    "{what}: junk {} not in extras",
                    j.index
                );
            }
            // every builder survives the labelled category too
            let mut forced = c.clone();
            forced.category = r.expect;
            let sf = structure(&forced, &entries);
            assert_partition(&sf, entries.len(), &format!("{what} (forced {})", r.expect));

            // oracles
            if entries.len() <= 1 {
                assert!(
                    !s.multi && s.item_count() <= 1,
                    "{what}: one file cannot be several items"
                );
            }
            if c.category == Category::Tv {
                let eps = entries
                    .iter()
                    .enumerate()
                    .filter(|(i, e)| {
                        is_video(e.path) && has_episode_marker(e.path) && !c.is_junk(*i)
                    })
                    .count();
                if eps >= 3 {
                    assert_eq!(s.kind, StructureKind::Tv, "{what}");
                    assert!(
                        s.multi,
                        "{what}: {eps} episode files should be several items"
                    );
                }
            }
            if c.category == Category::Movie {
                let vids = entries
                    .iter()
                    .enumerate()
                    .filter(|(i, e)| is_video(e.path) && !c.is_junk(*i))
                    .count();
                if vids == 1 {
                    assert!(!s.multi, "{what}: one video is one film");
                }
            }
            if c.category == Category::Software {
                let installers = entries
                    .iter()
                    .filter(|e| {
                        let l = e.path.to_ascii_lowercase();
                        [".dmg", ".pkg", ".exe", ".msi", ".appimage", ".deb", ".rpm"]
                            .iter()
                            .any(|x| l.ends_with(x))
                    })
                    .count();
                if installers <= 1 {
                    assert!(!s.has_platform_variants(), "{what}");
                }
            }
        }
    }
    eprintln!("structure corpus: {rows} rows, {with_items} with items, {multi_rows} multi");
    assert!(rows > 800);
    assert!(with_items > 300);
}
