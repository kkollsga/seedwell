use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::read::{Follower, Query, tail};

const PASSKEY: &str = "0123456789abcdef0123456789abcdef";
const B64: &str = "Zm9vYmFyQmF6MTIzNDU2Nzg5MEFCQw";
const IH: &str = "%12%34%56%78%9A%BC%DE%F0%12%34%56%78%9A%BC%DE%F0%12%34%56%78";

fn entry(n: u64) -> Entry {
    Entry {
        ts_ms: 1_700_000_000_000 + n,
        torrent: "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".into(),
        title: Some("Ubuntu 24.04".into()),
        tracker: format!("https://t.example.org/announce?passkey={PASSKEY}"),
        transport: "https",
        event: "started",
        request: Some(Request {
            url: format!("https://t.example.org/announce?passkey={PASSKEY}&info_hash={IH}&peer_id=-SW0001-abcdefghijkl&port=51413&uploaded=0&downloaded=0&left=1&numwant=50&key=DEADBEEF&compact=1&event=started"),
            headers: vec!["Host: t.example.org".into(), "User-Agent: Seedwell/0.0.1".into()],
            fields: vec![("key".into(), "DEADBEEF".into()), ("port".into(), "51413".into())],
        }),
        response: Some(Response {
            status: Some(200),
            fields: vec![("interval".into(), "1800".into()), ("warning".into(), format!("see https://t.example.org/a/{PASSKEY}/rules"))],
            error: None,
        }),
        duration_ms: Some(143),
        family: Some("IPv4"),
        ip: Some("93.184.216.34".into()),
        next: Some(Next { at_ms: 1_700_000_000_000 + n + 1_800_000, why: "interval 1800 s".into() }),
        ..Entry::default()
    }
}

fn read_all(dir: &std::path::Path) -> String {
    let mut s = String::new();
    for f in ["trackers.log", "trackers.log.1", "trackers.log.2", "trackers.log.3"] {
        s.push_str(&std::fs::read_to_string(dir.join(f)).unwrap_or_default());
    }
    s
}

#[test]
fn iso_time_is_utc() {
    assert_eq!(iso_time(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso_time(1_700_000_000_123), "2023-11-14T22:13:20.123Z");
    assert_eq!(iso_time(951_782_400_000), "2000-02-29T00:00:00.000Z");
}

#[test]
fn prefixed_and_unprefixed_secret_fields_are_masked_and_the_info_hash_is_kept() {
    let mut e = entry(0);
    let ih = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string();
    e.request.as_mut().unwrap().fields = vec![
        ("announce.key".into(), "F722CFA5".into()),
        ("key".into(), "F722CFA5".into()),
        ("announce.passkey".into(), PASSKEY.into()),
        ("announce.info_hash".into(), ih.clone()),
        ("info_hash".into(), ih.clone()),
        ("other".into(), format!("see {PASSKEY}")),
    ];
    let line = e.to_line();
    assert!(!line.contains("F722CFA5") && !line.contains(PASSKEY), "{line}");
    assert_eq!(line.matches(&ih).count(), 3, "the torrent field and the two info_hash fields: {line}");
}

#[test]
fn nothing_is_written_when_off() {
    let d = tempfile::tempdir().unwrap();
    let log = DevLog::new(Some(d.path().join("logs")));
    assert!(!log.enabled());
    for i in 0..50 {
        log.log(&entry(i));
    }
    assert!(!log.flush_wait(50), "no writer thread exists while off");
    assert_eq!((log.written(), log.dropped()), (0, 0));
    assert!(!d.path().join("logs").exists(), "no folder, no file");
}

#[test]
fn secrets_never_reach_the_file_but_info_hash_and_peer_id_do() {
    let d = tempfile::tempdir().unwrap();
    let log = DevLog::new(Some(d.path().to_path_buf()));
    log.set_enabled(true);
    let mut e = entry(0);
    e.response.as_mut().unwrap().error = Some(format!("error sending request for url (https://t.example.org/a/{B64}/announce?authkey={PASSKEY}): reset"));
    e.note = Some(format!("failover from https://t.example.org/{PASSKEY}/announce pid={B64}"));
    e.title = Some(format!("Show https://x.example/dl?torrent_pass={PASSKEY}"));
    log.log(&e);
    // private-tracker URL shapes, in the tracker field as well as the request
    for shape in [
        format!("https://t.example.org/{PASSKEY}/announce"),
        format!("http://t.example.org:2710/announce/{B64}"),
        format!("https://t.example.org/announce.php?authkey={B64}&torrent_pass={PASSKEY}"),
        format!("https://u:{PASSKEY}@t.example.org/announce"),
    ] {
        let mut e = entry(1);
        e.tracker = shape.clone();
        e.request.as_mut().unwrap().url = format!("{shape}?info_hash={IH}&key=DEADBEEF");
        log.log(&e);
    }
    assert!(log.flush_wait(2000));
    let text = read_all(d.path());
    assert_eq!(text.lines().count(), 5);
    for secret in [PASSKEY, B64, "DEADBEEF"] {
        assert!(!text.contains(secret), "{secret} leaked:\n{text}");
    }
    assert!(text.contains(IH), "info_hash is fine to log");
    assert!(text.contains("peer_id=-SW0001-abcdefghijkl"));
    assert!(text.contains("\"key\":\"••••••\""));
    assert!(text.contains("t.example.org"));
    for l in text.lines() {
        serde_json::from_str::<serde_json::Value>(l).expect("one JSON object per line");
    }
}

#[test]
fn switching_off_stops_logging_at_once_and_the_log_stays_until_cleared() {
    let d = tempfile::tempdir().unwrap();
    let log = DevLog::new(Some(d.path().to_path_buf()));
    log.set_enabled(true);
    log.log(&entry(0));
    assert!(log.flush_wait(2000));
    log.set_enabled(false);
    log.log(&entry(1));
    log.log(&entry(2));
    assert!(log.flush_wait(2000));
    assert_eq!(read_all(d.path()).lines().count(), 1);
    log.clear();
    assert_eq!(read_all(d.path()), "");
    log.set_enabled(true);
    log.log(&entry(3));
    assert!(log.flush_wait(2000));
    assert_eq!(read_all(d.path()).lines().count(), 1, "writes resume after a clear");
}

#[test]
fn rotation_keeps_three_files_and_bounds_the_size() {
    let d = tempfile::tempdir().unwrap();
    let limits = Limits { max_bytes: 4000, keep: 3, queue: 4096 };
    let log = DevLog::with_limits(Some(d.path().to_path_buf()), limits);
    log.set_enabled(true);
    for i in 0..400 {
        log.log(&entry(i));
    }
    assert!(log.flush_wait(5000));
    assert!(d.path().join("trackers.log").exists());
    assert!(d.path().join("trackers.log.1").exists());
    assert!(d.path().join("trackers.log.2").exists());
    assert!(!d.path().join("trackers.log.3").exists(), "three files at most");
    for f in ["trackers.log", "trackers.log.1", "trackers.log.2"] {
        let len = std::fs::metadata(d.path().join(f)).unwrap().len();
        assert!(len <= limits.max_bytes, "{f}: {len}");
    }
    // the newest line is in the newest file
    let cur = std::fs::read_to_string(d.path().join("trackers.log")).unwrap();
    assert!(cur.contains(&(1_700_000_000_000u64 + 399).to_string()));
}

struct Stalled(Arc<Mutex<()>>, Arc<Mutex<u64>>);

impl LineSink for Stalled {
    fn write_line(&mut self, _: &str) {
        let _g = self.0.lock().unwrap();
        *self.1.lock().unwrap() += 1;
    }
    fn flush(&mut self) {}
    fn clear(&mut self) {}
}

#[test]
fn a_stalled_writer_never_blocks_the_sender_and_drops_are_counted() {
    let gate = Arc::new(Mutex::new(()));
    let count = Arc::new(Mutex::new(0u64));
    let held = gate.lock().unwrap(); // the writer stalls on its first line
    let log = DevLog::with_sink(Box::new(Stalled(gate.clone(), count.clone())), Limits { max_bytes: 1 << 20, keep: 3, queue: 16 });
    log.set_enabled(true);
    let t0 = Instant::now();
    for i in 0..20_000 {
        log.log(&entry(i));
    }
    assert!(t0.elapsed() < Duration::from_secs(5), "log() must not wait for the writer: {:?}", t0.elapsed());
    assert!(log.dropped() >= 19_000, "dropped {}", log.dropped());
    drop(held);
    assert!(log.flush_wait(5000));
    assert!(*count.lock().unwrap() <= 20, "only the queue's worth was written");
    assert_eq!(log.dropped() + log.written(), 20_000);
}

#[test]
fn tail_filters_by_torrent_tracker_event_and_errors() {
    let d = tempfile::tempdir().unwrap();
    let log = DevLog::new(Some(d.path().to_path_buf()));
    log.set_enabled(true);
    let mut a = entry(0);
    a.event = "regular";
    log.log(&a);
    let mut b = entry(1);
    b.torrent = "ffffffffffffffffffffffffffffffffffffffff".into();
    b.title = Some("Debian".into());
    b.tracker = "udp://tracker.other.test:6969/announce".into();
    b.transport = "udp";
    b.response = Some(Response { status: None, fields: vec![], error: Some("timed out".into()) });
    log.log(&b);
    let mut c = entry(2);
    c.kind = Kind::Decision;
    c.decision = Some("retire");
    c.note = Some("passed over by BEP 12 failover".into());
    c.request = None;
    c.response = None;
    log.log(&c);
    assert!(log.flush_wait(2000));
    let q = |f: &dyn Fn(&mut Query)| {
        let mut q = Query { limit: 50, ..Query::default() };
        f(&mut q);
        tail(d.path(), &q).len()
    };
    assert_eq!(q(&|_| {}), 3);
    assert_eq!(q(&|q| q.torrent = Some("a1b2".into())), 2);
    assert_eq!(q(&|q| q.torrent = Some("debian".into())), 1);
    assert_eq!(q(&|q| q.tracker = Some("other.test".into())), 1);
    assert_eq!(q(&|q| q.event = Some("regular".into())), 1);
    assert_eq!(q(&|q| q.errors_only = true), 1);
    assert_eq!(q(&|q| q.limit = 2), 2);
    let rows: Vec<_> = tail(d.path(), &Query { limit: 50, ..Query::default() }).iter().filter_map(|l| view::Row::parse(l)).collect();
    assert!(rows[1].error && rows[1].summary.contains("timed out"), "{:?}", rows[1]);
    assert!(rows[0].detail.contains("GET https://t.example.org/announce?passkey=••••••"), "{}", rows[0].detail);
    assert!(rows[0].detail.contains("next: in 30 min"), "{}", rows[0].detail);
    assert!(rows[2].summary.contains("retire"));
}

#[test]
fn follower_sees_new_lines_across_rotation() {
    let d = tempfile::tempdir().unwrap();
    let log = DevLog::with_limits(Some(d.path().to_path_buf()), Limits { max_bytes: 3000, keep: 3, queue: 4096 });
    log.set_enabled(true);
    log.log(&entry(0));
    assert!(log.flush_wait(2000));
    let mut f = Follower::new(d.path());
    assert!(f.poll().is_empty(), "starts at the end");
    let mut seen = 0;
    for i in 1..=30 {
        log.log(&entry(i));
        assert!(log.flush_wait(2000));
        seen += f.poll().len();
    }
    assert_eq!(seen, 30);
}
