//! Classify from the command line: `cargo run --example classify -- "Torrent Name" [path:size ...]`
//!
//! Handy for debugging a misclassified release name.

use trs_classify::{FileEntry, Input, classify};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(name) = args.next() else {
        eprintln!("usage: classify <torrent name> [path:size ...]");
        return;
    };
    let files: Vec<(String, u64)> = args
        .map(|a| match a.rsplit_once(':') {
            Some((p, s)) => (p.to_owned(), s.parse().unwrap_or(1_000_000_000)),
            None => (a, 1_000_000_000),
        })
        .collect();
    let entries: Vec<FileEntry> = files
        .iter()
        .map(|(p, s)| FileEntry { path: p, size: *s })
        .collect();
    let single = [FileEntry {
        path: &name,
        size: 1_000_000_000,
    }];
    let input = Input::new(
        &name,
        if entries.is_empty() {
            &single
        } else {
            &entries
        },
    );
    let c = classify(&input);
    println!(
        "{:?} confidence {:.2} ask={}",
        c.category, c.confidence, c.ask
    );
    println!("{:#?}", c.fields);
    println!("junk: {:?}", c.junk_files);
}
