#![no_main]
use libfuzzer_sys::fuzz_target;
use trs_classify::{FileEntry, Input, classify};

// Input layout: name \0 hint \0 path:size \n path:size ...  (all lossy UTF-8)
fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let mut parts = text.splitn(3, '\0');
    let name = parts.next().unwrap_or("");
    let hint = parts.next();
    let files: Vec<(String, u64)> = parts
        .next()
        .unwrap_or("")
        .lines()
        .map(|l| match l.rsplit_once(':') {
            Some((p, s)) => (p.to_owned(), s.parse().unwrap_or(u64::MAX)),
            None => (l.to_owned(), 1),
        })
        .collect();
    let entries: Vec<FileEntry> = files.iter().map(|(p, s)| FileEntry { path: p, size: *s }).collect();
    let c = classify(&Input { name, files: &entries, tracker_source_hint: hint, content_type_hint: hint });
    assert!((0.0..=1.0).contains(&c.confidence));
    assert!(c.junk_files.iter().all(|j| j.index < entries.len()));
});
