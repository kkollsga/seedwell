//! Heap cost of one (torrent, tracker) pair in a `TrackerHub`, measured with a
//! counting global allocator (SPEC 2 / 5A: a library of thousands of idle
//! torrents must stay small).
//!
//! The cost is the *marginal* heap between two hub sizes, so fixed costs cancel.
//! Only allocations made by the measuring thread are counted, so the test
//! harness cannot disturb the numbers. This file must contain a single test
//! that measures (the allocator is global to the binary).
//!
//! History: 2851 B per idle pair before the compaction (the engine's
//! `crates/engine/tests/scale.rs` measured the same), 257 B after.
//!
//! Budget: <= 260 B per idle pair (added, not started) when torrents share a
//! tracker URL (the common public-tracker case) and <= 256 B per pair for
//! torrents with several trackers. Distinct passkey URLs add the URL text
//! itself (it has to be stored once per torrent) and nothing else.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use trs_tracker::gate::GateConfig;
use trs_tracker::hub::{TorrentSpec, TrackerHub};
use trs_tracker::store::{StoreError, TimingKey, TimingRecord, TimingStore};
use trs_tracker::{Millis, Stats, TorrentEvent, TrackerUrl};

struct Counting;

thread_local! {
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

fn bump(delta: isize) {
    // `try_with`: the allocator is also called while thread-locals are torn down
    let _ = LIVE.try_with(|c| c.set(c.get() + delta));
}

// SAFETY: forwards every call to `System` unchanged and only counts bytes.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(l) };
        if !p.is_null() {
            bump(l.size() as isize);
        }
        p
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) };
        bump(-(l.size() as isize));
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, new_size: usize) -> *mut u8 {
        let q = unsafe { System.realloc(p, l, new_size) };
        if !q.is_null() {
            bump(new_size as isize - l.size() as isize);
        }
        q
    }
}

#[global_allocator]
static A: Counting = Counting;

fn live() -> isize {
    LIVE.with(Cell::get)
}

/// The engine's store lives on disk; the hub's own memory is what is measured.
struct NullStore;

impl TimingStore for NullStore {
    fn load(&self, _: &TimingKey) -> Option<TimingRecord> {
        None
    }
    fn save(&mut self, _: &TimingKey, _: &TimingRecord) -> Result<(), StoreError> {
        Ok(())
    }
}

fn info_hash(i: usize) -> [u8; 20] {
    let mut ih = [0u8; 20];
    ih[..8].copy_from_slice(&(i as u64).to_le_bytes());
    ih[8..16].copy_from_slice(&((i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes());
    ih
}

#[derive(Clone, Copy)]
enum Shape {
    /// All torrents announce to one shared URL.
    SharedUrl,
    /// Every torrent has its own passkey URL.
    PasskeyUrl,
    /// Three trackers per torrent, shared URLs.
    ThreeTrackers,
}

fn urls(shape: Shape, i: usize) -> Vec<Vec<TrackerUrl>> {
    let p = |s: &str| TrackerUrl::parse(s).unwrap();
    match shape {
        Shape::SharedUrl => vec![vec![p("http://tracker.example.org:6969/announce")]],
        Shape::PasskeyUrl => vec![vec![p(&format!(
            "https://tracker{}.example.org/announce?passkey={:032x}",
            i % 64,
            (i as u128).wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835)
        ))]],
        Shape::ThreeTrackers => vec![
            vec![p("http://tracker.example.org:6969/announce")],
            vec![p("udp://open.example.net:1337/announce")],
            vec![p("https://bt.example.com/announce")],
        ],
    }
}

/// Live heap after adding `n` torrents (and optionally starting them and
/// answering the first announce, i.e. a seeding torrent).
fn heap_for(n: usize, shape: Shape, seeding: bool) -> isize {
    let before = live();
    let mut hub = TrackerHub::new(GateConfig::default(), NullStore, 1);
    for i in 0..n {
        hub.add_torrent(
            TorrentSpec::new(info_hash(i), [1; 20], 7, 6881, urls(shape, i)),
            Millis(0),
        )
        .unwrap();
    }
    if seeding {
        let mut now = Millis(0);
        for i in 0..n {
            hub.set_stats(
                &info_hash(i),
                Stats {
                    uploaded: 0,
                    downloaded: 0,
                    left: 0,
                    size: 1 << 20,
                },
                now,
            );
            hub.torrent_event(&info_hash(i), TorrentEvent::Start, now);
        }
        // answer every first announce
        for _ in 0..(n + 20) {
            now = Millis(now.0 + 1000);
            // the engine drains these every loop iteration
            let _ = hub.take_reports();
            let _ = hub.take_gate_events();
            for r in hub.tick(now) {
                let t = trs_tracker::clamp_timing(Some(1800), None);
                hub.complete(
                    r.ticket(),
                    Ok(trs_tracker::AnnounceResponse {
                        interval: t.interval,
                        min_interval: t.min_interval,
                        tracker_id: None,
                        complete: None,
                        incomplete: None,
                        downloaded: None,
                        peers: vec![],
                        warning: None,
                        external_ip: None,
                    }),
                    now,
                );
            }
        }
        let _ = hub.take_reports();
        let _ = hub.take_gate_events();
    }
    let used = live() - before;
    std::hint::black_box(&hub);
    drop(hub);
    used
}

fn per_pair(shape: Shape, trackers: usize, seeding: bool) -> f64 {
    let (a, b) = (112, 896);
    let (ha, hb) = (heap_for(a, shape, seeding), heap_for(b, shape, seeding));
    (hb - ha) as f64 / ((b - a) * trackers) as f64
}

#[test]
fn heap_cost_per_torrent_tracker_pair() {
    let idle = per_pair(Shape::SharedUrl, 1, false);
    let idle_passkey = per_pair(Shape::PasskeyUrl, 1, false);
    let idle3 = per_pair(Shape::ThreeTrackers, 3, false);
    let seeding = per_pair(Shape::SharedUrl, 1, true);
    let seeding_passkey = per_pair(Shape::PasskeyUrl, 1, true);
    eprintln!("[memory] idle pair, shared URL:        {idle:.0} B");
    eprintln!("[memory] idle pair, passkey URL:       {idle_passkey:.0} B");
    eprintln!("[memory] idle pair, 3 trackers/torrent: {idle3:.0} B per pair");
    eprintln!("[memory] seeding pair, shared URL:     {seeding:.0} B");
    eprintln!("[memory] seeding pair, passkey URL:    {seeding_passkey:.0} B");
    assert!(
        idle <= 260.0,
        "an idle (torrent, tracker) pair costs {idle:.0} B, budget 260 B"
    );
    assert!(
        idle3 <= 256.0,
        "an idle pair of a 3-tracker torrent costs {idle3:.0} B, budget 256 B"
    );
    // the passkey URL text (~110 B) is the only thing a distinct URL adds
    assert!(
        idle_passkey <= 260.0 + 160.0,
        "idle pair with a passkey URL: {idle_passkey:.0} B"
    );
    assert!(seeding <= 900.0, "a seeding pair costs {seeding:.0} B");
    assert!(
        seeding_passkey <= 900.0 + 160.0,
        "a seeding pair with a passkey URL costs {seeding_passkey:.0} B"
    );
}
