//! Micro-benchmark: `cargo run --release --example bench`.
//!
//! Classifies 10,000 synthetic torrents (all categories, realistic file lists) and prints
//! per-torrent latency percentiles. The SPEC budget is < 1 ms per torrent.

use std::hint::black_box;
use std::time::Instant;
use trs_classify::{FileEntry, Input, classify};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn pick<'a>(&mut self, v: &[&'a str]) -> &'a str {
        v[(self.next() % v.len() as u64) as usize]
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next() % (hi - lo + 1)
    }
}

const TITLES: [&str; 12] = [
    "Harbor Lights",
    "The Night Shift",
    "Brass Lantern",
    "Cedar Falls",
    "Quiet Hours",
    "Iron Valley",
    "The Last Cartographer",
    "Salt Road",
    "Glass Orchard",
    "Winter Kingdom",
    "Paper Moon Rising",
    "Blue Mountain",
];
const GROUPS: [&str; 6] = ["GRP", "NIMBUS", "FJORD", "KESTREL", "SPRUCE", "HALCYON"];

fn synth(rng: &mut Rng) -> (String, Vec<(String, u64)>) {
    let t = rng.pick(&TITLES);
    let dot = t.replace(' ', ".");
    let g = rng.pick(&GROUPS);
    let year = rng.range(1950, 2024);
    match rng.range(0, 9) {
        0..=2 => {
            let name = format!("{dot}.{year}.1080p.BluRay.x264-{g}");
            let files = vec![
                (format!("{name}.mkv"), 8 << 30),
                ("Sample/sample.mkv".into(), 50 << 20),
                (format!("{name}.nfo"), 3000),
            ];
            (name, files)
        }
        3..=4 => {
            let n = rng.range(6, 24);
            let s = rng.range(1, 9);
            let name = format!("{dot}.S{s:02}.1080p.WEB-DL.DDP5.1.H.264-{g}");
            let files = (1..=n)
                .map(|e| (format!("{name}.S{s:02}E{e:02}.mkv"), 2 << 30))
                .collect();
            (name, files)
        }
        5 => {
            let name = format!("{t} - Album Name ({year}) [FLAC]");
            let files = (1..=rng.range(8, 16))
                .map(|k| (format!("{k:02} - Track {k}.flac"), 30 << 20))
                .chain([
                    ("cover.jpg".to_owned(), 500_000),
                    ("rip.log".to_owned(), 4000),
                ])
                .collect();
            (name, files)
        }
        6 => (
            format!("{t} - Some Author (Unabridged) [M4B]"),
            vec![("book.m4b".into(), 400 << 20)],
        ),
        7 => (
            format!("Some Author - {t} ({year}).epub"),
            vec![(format!("Some Author - {t} ({year}).epub"), 2 << 20)],
        ),
        8 => (
            format!(
                "App {}.{}.{} (arm64).dmg",
                rng.range(1, 9),
                rng.range(0, 9),
                rng.range(0, 20)
            ),
            vec![("App.dmg".into(), 200 << 20)],
        ),
        _ => (
            format!("misc stuff {year}"),
            vec![("a.bin".into(), 1 << 20), ("b.dat".into(), 2 << 20)],
        ),
    }
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    sorted[((sorted.len() as f64 - 1.0) * p) as usize]
}

fn main() {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let cases: Vec<(String, Vec<(String, u64)>)> = (0..10_000).map(|_| synth(&mut rng)).collect();

    // warm-up
    for (name, files) in cases.iter().take(500) {
        let e: Vec<FileEntry> = files
            .iter()
            .map(|(p, s)| FileEntry { path: p, size: *s })
            .collect();
        black_box(classify(&Input::new(name, &e)));
    }

    let mut ns = Vec::with_capacity(cases.len());
    let total = Instant::now();
    for (name, files) in &cases {
        let e: Vec<FileEntry> = files
            .iter()
            .map(|(p, s)| FileEntry { path: p, size: *s })
            .collect();
        let t = Instant::now();
        let c = classify(&Input::new(name, &e));
        black_box(&c);
        ns.push(t.elapsed().as_nanos() as u64);
    }
    let wall = total.elapsed();
    ns.sort_unstable();
    println!(
        "classify x{}: total {:.1} ms",
        cases.len(),
        wall.as_secs_f64() * 1e3
    );
    println!(
        "  per torrent: mean {:.1} us, p50 {:.1} us, p99 {:.1} us, max {:.1} us",
        ns.iter().sum::<u64>() as f64 / ns.len() as f64 / 1e3,
        pct(&ns, 0.50) as f64 / 1e3,
        pct(&ns, 0.99) as f64 / 1e3,
        *ns.last().unwrap() as f64 / 1e3
    );

    // pathological: 20,000-file torrent
    let big: Vec<(String, u64)> = (0..20_000)
        .map(|i| {
            (
                format!("dir{}/Show.S01E{:03}.1080p.mkv", i % 50, i),
                1 << 30,
            )
        })
        .collect();
    let e: Vec<FileEntry> = big
        .iter()
        .map(|(p, s)| FileEntry { path: p, size: *s })
        .collect();
    let t = Instant::now();
    black_box(classify(&Input::new("Huge.Show.S01.1080p", &e)));
    println!(
        "classify 20,000-file torrent: {:.2} ms",
        t.elapsed().as_secs_f64() * 1e3
    );
}
