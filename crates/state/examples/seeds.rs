//! Writes seed corpora for the fuzz targets: `cargo run -p trs-state --example seeds -- fuzz/corpus`.
use std::fs;
use trs_state::record::{FileStamp, PartialPiece};
use trs_state::{Config, Document, ResumeData, RuleSet, TorrentRecord, TrackerProfiles};

fn main() {
    let out = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "fuzz/corpus".into()));
    for d in ["config", "rules", "trackers", "records"] {
        fs::create_dir_all(out.join(d)).unwrap();
    }
    fs::write(out.join("config/default.toml"), Config::default().render().unwrap()).unwrap();
    fs::write(out.join("config/small.toml"), "[network]\nlisten_port = 51413\n[[folders.watch]]\npath = \"~/Torrents\"\nmode = \"ask\"\ncategory = \"book\"\n").unwrap();
    let rules = "[default]\ngoal = { kind = \"ratio_or_time\", ratio = 1.0, time = \"7d\" }\n[[rule]]\nid = \"z\"\nname = \"Z\"\nmatch = { tracker = \"tz\", private = true, category = [\"tv\"] }\npriority = \"high\"\nmin_seed_time = \"72h\"\ngoal = { kind = \"ratio\", ratio = 2.0 }\n";
    let mut seed = rules.as_bytes().to_vec();
    seed.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
    fs::write(out.join("rules/example"), seed).unwrap();
    fs::write(out.join("rules/default.toml"), RuleSet::default().render().unwrap()).unwrap();
    let t = "[[tracker]]\nid = \"tl\"\nname = \"TL\"\nurls = [\"https://t.example.org/a/*/announce\", \"backup.example.org\"]\n\0https://t.example.org/a/0123456789abcdef0123456789abcdef/announce?pk=SECRET\nhttp://x.org/announce\n";
    fs::write(out.join("trackers/example"), t).unwrap();
    fs::write(out.join("trackers/empty.toml"), TrackerProfiles::default().render().unwrap()).unwrap();
    let mut r = TorrentRecord::new("Example.S01E01.1080p", 123_456, 1_791_000_000);
    r.tracker_profile = Some("tl".into());
    fs::write(out.join("records/torrent"), r.encode()).unwrap();
    let res = ResumeData {
        piece_count: 20,
        bitfield: vec![0xf0, 0x0f, 0x00].into_boxed_slice(),
        partial: vec![PartialPiece { index: 4, blocks: vec![3].into_boxed_slice() }],
        files: vec![FileStamp { size: 1, mtime_ns: 2 }],
        saved_at: 5,
    };
    fs::write(out.join("records/resume"), res.encode().unwrap()).unwrap();
}
