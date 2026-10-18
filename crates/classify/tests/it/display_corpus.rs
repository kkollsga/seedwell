//! `display_name` over every row of the labelled corpora: readable, never invents words.
//! `DISPLAY_VERBOSE=1 cargo test -p trs-classify --test display_corpus -- --nocapture` prints every title.

use std::collections::HashSet;
use trs_classify::{Category, FileEntry, Input, classify, display_name};

struct Row {
    expect: Category,
    name: String,
    files: Vec<(String, u64)>,
    hint: Option<String>,
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
    (num.parse::<f64>().unwrap() * mul) as u64
}

fn expand(path: &str) -> Vec<String> {
    if let (Some(a), Some(b)) = (path.find('{'), path.find('}'))
        && let Some((lo, hi)) = path[a + 1..b].split_once("..")
    {
        let width = lo.len();
        let (lo, hi): (u32, u32) = (lo.parse().unwrap(), hi.parse().unwrap());
        return (lo..=hi)
            .flat_map(|n| expand(&format!("{}{:0w$}{}", &path[..a], n, &path[b + 1..], w = width)))
            .collect();
    }
    vec![path.to_owned()]
}

fn load(file: &str) -> Vec<Row> {
    let text = std::fs::read_to_string(format!("{}/tests/fixtures/{file}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|line| {
            let c: Vec<&str> = line.split('\t').collect();
            let name = c[1].to_owned();
            let files = match c[2].trim() {
                "-" => vec![],
                "=" => vec![(name.clone(), 1_000_000_000)],
                spec => spec
                    .split(';')
                    .filter(|s| !s.trim().is_empty())
                    .flat_map(|item| {
                        let (p, sz) = item.rsplit_once(':').unwrap();
                        let sz = parse_size(sz);
                        expand(p.trim()).into_iter().map(move |p| (p, sz))
                    })
                    .collect(),
            };
            let hint = c.get(3).map(|s| s.trim()).filter(|s| !s.is_empty() && *s != "-").map(str::to_owned);
            Row { expect: Category::parse(c[0]).unwrap(), name, files, hint }
        })
        .collect()
}

fn words(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_lowercase).collect()
}

const TAGS: &[&str] = &["1080p", "720p", "2160p", "x264", "x265", "h264", "hevc", "bluray", "web-dl", "webrip", "hdtv", "dts", "flac"];

#[test]
fn display_titles_are_clean_and_honest() {
    let verbose = std::env::var("DISPLAY_VERBOSE").is_ok();
    let mut bad = Vec::new();
    let mut total = 0;
    let mut same_as_raw = 0;
    for file in ["corpus.tsv", "holdout.tsv", "blind.tsv"] {
        for r in load(file) {
            total += 1;
            let entries: Vec<FileEntry> = r.files.iter().map(|(p, s)| FileEntry { path: p, size: *s }).collect();
            let c = classify(&Input {
                name: &r.name,
                files: &entries,
                tracker_source_hint: r.hint.as_deref(),
                content_type_hint: None,
            });
            let d = display_name(&r.name, &c);
            if verbose {
                println!("{:<9} {:.2}  {}\n{:>22}{}", r.expect.as_str(), c.confidence, r.name, "-> ", d);
            }
            if d.trim().is_empty() || d != d.trim() || d.contains("  ") || d.contains('_') {
                bad.push(format!("not clean: {:?} <- {}", d, r.name));
                continue;
            }
            if d == r.name {
                same_as_raw += 1;
            }
            // Release-tag junk must be gone from titles that came from a confident parse.
            let lower = d.to_lowercase();
            if !c.ask && TAGS.iter().any(|t| words(&lower).iter().any(|w| w == t)) && !r.name.contains(' ') {
                bad.push(format!("tag left: {d:?} <- {}", r.name));
            }
            // Never invent: every word comes from the name or a file name (plus the fixed words).
            let mut allowed: HashSet<String> = words(&r.name).into_iter().collect();
            for (p, _) in &r.files {
                allowed.extend(words(p));
            }
            allowed.extend(["complete", "series", "various", "artists"].map(str::to_owned));
            for w in words(&d) {
                let ok = allowed.contains(&w)
                    || w.chars().all(|c| c.is_ascii_digit())
                    || (w.starts_with(['s', 'e']) && w[1..].chars().all(|c| c.is_ascii_digit() || c == 'e'));
                if !ok {
                    bad.push(format!("invented {w:?}: {d:?} <- {}", r.name));
                    break;
                }
            }
        }
    }
    println!("{total} rows, {same_as_raw} unchanged, {} problems", bad.len());
    assert!(bad.len() * 100 <= total, "{} problems:\n{}", bad.len(), bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
    assert!(bad.is_empty(), "{}", bad.iter().take(40).cloned().collect::<Vec<_>>().join("\n"));
}
