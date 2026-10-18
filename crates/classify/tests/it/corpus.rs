//! Accuracy test over the labelled corpus in `tests/fixtures/corpus.tsv`.
//!
//! Columns (tab-separated, `#` starts a comment line):
//!   1 expected category      movie|tv|music|audiobook|book|software|other
//!   2 torrent name
//!   3 files                  `-` none, `=` one file named like the torrent, else
//!                            `path:size;path:size`. Sizes take K/M/G suffixes (`1.4G`).
//!                            `{01..10}` expands to zero-padded numbers.
//!   4 tracker hint           `-` for none
//!   5 content-type hint      `-` for none
//!   6 expected fields        `-` or `key=value|key=value`; `junk=1,3` lists junk file indexes,
//!                            `suspicious=2` lists those flagged suspicious.

use std::collections::BTreeMap;
use trs_classify::{Category, FileEntry, Input, JunkKind, classify};

struct Row {
    line: usize,
    expect: Category,
    name: String,
    files: Vec<(String, u64)>,
    hint: Option<String>,
    ctype: Option<String>,
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

fn norm(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

struct Outcome {
    acc: f64,
    facc: f64,
    prec: f64,
    report: String,
    failures: String,
    junk_fail: Vec<String>,
}

fn evaluate(rows: &[Row]) -> Outcome {
    let verbose = std::env::var("CORPUS_VERBOSE").is_ok();

    let mut confusion = [[0u32; 8]; 8];
    let mut per_cat_total = [0u32; 8];
    let (mut ok, mut accepted, mut accepted_ok) = (0u32, 0u32, 0u32);
    let (mut fields_total, mut fields_ok) = (0u32, 0u32);
    let mut per_key: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut cat_fail = Vec::new();
    let mut field_fail = Vec::new();
    let mut junk_fail = Vec::new();

    for r in rows {
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
        confusion[r.expect.index()][c.category.index()] += 1;
        per_cat_total[r.expect.index()] += 1;
        if c.category == r.expect {
            ok += 1;
        } else {
            cat_fail.push(format!(
                "line {:>4}: expected {:<9} got {:<9} ({:.2}) {}",
                r.line, r.expect, c.category, c.confidence, r.name
            ));
        }
        if !c.ask {
            accepted += 1;
            if c.category == r.expect {
                accepted_ok += 1;
            }
        }
        // fields are only judged when the category was right (otherwise they are meaningless)
        if c.category == r.expect {
            for (k, want) in &r.fields {
                if k == "junk" || k == "suspicious" {
                    let mut got: Vec<usize> = c
                        .junk_files
                        .iter()
                        .filter(|j| (k == "suspicious") == (j.kind == JunkKind::Suspicious))
                        .map(|j| j.index)
                        .collect();
                    got.sort_unstable();
                    let mut exp: Vec<usize> = want
                        .split(',')
                        .filter(|s| !s.is_empty() && *s != "none")
                        .map(|s| s.parse().unwrap())
                        .collect();
                    exp.sort_unstable();
                    let e = per_key.entry(k.clone()).or_default();
                    e.0 += 1;
                    if got == exp {
                        e.1 += 1;
                    } else {
                        junk_fail.push(format!(
                            "line {:>4}: {k} expected {exp:?} got {got:?}  {}",
                            r.line, r.name
                        ));
                    }
                    continue;
                }
                let got = c.fields.get(k).unwrap_or_default();
                fields_total += 1;
                let e = per_key.entry(k.clone()).or_default();
                e.0 += 1;
                let hit = norm(&got) == norm(want);
                if hit {
                    fields_ok += 1;
                    e.1 += 1;
                } else {
                    field_fail.push(format!(
                        "line {:>4}: {k}: expected {want:?} got {got:?}  [{}]",
                        r.line, r.name
                    ));
                }
            }
        }
    }

    let mut report = String::new();
    use std::fmt::Write;
    let n = rows.len() as f64;
    writeln!(report, "\ncorpus rows: {}", rows.len()).unwrap();
    writeln!(
        report,
        "category accuracy: {:.2}% ({ok}/{})",
        100.0 * f64::from(ok) / n,
        rows.len()
    )
    .unwrap();
    writeln!(
        report,
        "auto-accept (confidence >= 0.8): coverage {:.1}% ({accepted}/{}), precision {:.2}% ({accepted_ok}/{accepted})",
        100.0 * f64::from(accepted) / n,
        rows.len(),
        100.0 * f64::from(accepted_ok) / f64::from(accepted.max(1))
    )
    .unwrap();
    writeln!(
        report,
        "field accuracy: {:.2}% ({fields_ok}/{fields_total})",
        100.0 * f64::from(fields_ok) / f64::from(fields_total.max(1))
    )
    .unwrap();
    for (k, (t, o)) in &per_key {
        writeln!(
            report,
            "  {k:<12} {:>6.1}%  ({o}/{t})",
            100.0 * f64::from(*o) / f64::from(*t)
        )
        .unwrap();
    }
    writeln!(
        report,
        "confusion matrix (rows = expected, cols = predicted)"
    )
    .unwrap();
    write!(report, "{:<10}", "").unwrap();
    for c in Category::ALL {
        write!(report, "{:>10}", c.as_str()).unwrap();
    }
    writeln!(report, "{:>10}", "recall").unwrap();
    for (i, e) in Category::ALL.iter().enumerate() {
        write!(report, "{:<10}", e.as_str()).unwrap();
        for n in confusion[i] {
            write!(report, "{n:>10}").unwrap();
        }
        writeln!(
            report,
            "{:>9.1}%",
            100.0 * f64::from(confusion[i][i]) / f64::from(per_cat_total[i].max(1))
        )
        .unwrap();
    }
    if verbose {
        for l in cat_fail.iter().chain(&field_fail).chain(&junk_fail) {
            writeln!(report, "{l}").unwrap();
        }
    }
    println!("{report}");

    Outcome {
        acc: f64::from(ok) / n,
        facc: f64::from(fields_ok) / f64::from(fields_total.max(1)),
        prec: f64::from(accepted_ok) / f64::from(accepted.max(1)),
        report,
        failures: cat_fail
            .iter()
            .chain(&field_fail)
            .chain(&junk_fail)
            .take(40)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n"),
        junk_fail,
    }
}

#[test]
fn corpus_accuracy() {
    let rows = load("corpus.tsv");
    assert!(
        rows.len() >= 500,
        "corpus must have at least 500 rows, has {}",
        rows.len()
    );
    let o = evaluate(&rows);
    let (acc, facc, prec, report, all_fail) = (o.acc, o.facc, o.prec, &o.report, &o.failures);
    assert!(
        acc >= 0.95,
        "category accuracy {acc:.4} < 0.95\n{report}\n{all_fail}"
    );
    assert!(
        facc >= 0.90,
        "field accuracy {facc:.4} < 0.90\n{report}\n{all_fail}"
    );
    assert!(
        prec >= 0.98,
        "auto-accept precision {prec:.4} < 0.98\n{report}\n{all_fail}"
    );
    assert!(
        o.junk_fail.is_empty(),
        "junk detection mismatches:\n{}",
        o.junk_fail.join("\n")
    );
}

/// Rows written after tuning and never used to tune; guards against over-fitting the main corpus.
#[test]
fn holdout_accuracy() {
    let rows = load("holdout.tsv");
    let o = evaluate(&rows);
    let (acc, facc, prec, report, all_fail) = (o.acc, o.facc, o.prec, &o.report, &o.failures);
    assert!(
        acc >= 0.93,
        "holdout category accuracy {acc:.4} < 0.93\n{report}\n{all_fail}"
    );
    assert!(
        facc >= 0.85,
        "holdout field accuracy {facc:.4} < 0.85\n{report}\n{all_fail}"
    );
    assert!(
        prec >= 0.95,
        "holdout auto-accept precision {prec:.4} < 0.95\n{report}\n{all_fail}"
    );
}

/// Written last and never tuned against; thresholds are deliberately looser.
#[test]
fn blind_accuracy() {
    let rows = load("blind.tsv");
    let o = evaluate(&rows);
    let (acc, facc, prec, report, all_fail) = (o.acc, o.facc, o.prec, &o.report, &o.failures);
    assert!(
        acc >= 0.90,
        "blind category accuracy {acc:.4} < 0.90\n{report}\n{all_fail}"
    );
    assert!(
        facc >= 0.80,
        "blind field accuracy {facc:.4} < 0.80\n{report}\n{all_fail}"
    );
    assert!(
        prec >= 0.93,
        "blind auto-accept precision {prec:.4} < 0.93\n{report}\n{all_fail}"
    );
}

#[test]
fn corpus_has_every_category_and_style() {
    let rows = load("corpus.tsv");
    for cat in Category::ALL {
        let n = rows.iter().filter(|r| r.expect == cat).count();
        assert!(n >= 20, "category {cat} has only {n} rows");
    }
    let with_hint = rows.iter().filter(|r| r.hint.is_some()).count();
    assert!(with_hint >= 10, "need rows exercising tracker hints");
    assert!(
        rows.iter().any(|r| r.ctype.is_some()),
        "need rows exercising content-type hints"
    );
}
