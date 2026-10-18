//! Mock backend: a small simulated library that looks alive (progress, queue promotion,
//! finishing events, sharing rates). It exists so the Swift shell can be built and
//! snapshot-tested before the real engine lands. A ticker thread advances it once a second;
//! the real engine will not need one because the UI pulls snapshots.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::backend::{AttrRow, Backend, ShutdownInfo};
use crate::rows::{self, Totals};
use crate::types::{
    AskAgainOutcome,
    TrackerLine,
    AgentActivity, Category, ChoiceInfo, CoreError, DiagnosticsInfo, MoveProposalInfo, MoveResult, CoreListener, DailyStat, Priority, RowQuery, SeedGroup,
    FilePriority, FilePriorityChange, SeedingGoal, SharingCounts, Snapshot, StreamInfo, TorrentFile, TorrentInfo, TrackerStats, TransferKind, TransferRow,
    TransferState,
};

const KB: u64 = 1_000;
const MB: u64 = 1_000_000;
const GB: u64 = 1_000_000_000;
// Concept 2 shows two active downloads; the real default (3) comes from settings.
const MAX_ACTIVE: usize = 2;
const MENU_RECENT: usize = 10;
const TRACKERS: [(&str, &str, f64); 3] = [
    ("tracker-z.cc", "tracker-z.cc", 2.0),
    ("tracker-y.net", "tracker-y.net", 1.0),
    ("public", "Public", 1.0),
];

struct Item {
    id: String,
    name: String,
    kind: TransferKind,
    category: Category,
    size: u64,
    done: u64,
    /// Rate the simulation aims for; the shown rate jitters around it.
    base_down: u64,
    base_up: u64,
    down_rate: u64,
    up_rate: u64,
    tracker: usize,
    ratio: f64,
    goal: Option<f64>,
    goal_is_default: bool,
    seed_secs: u64,
    state: TransferState,
    finished_at: Option<u64>,
    recent: bool,
    ext: String,
    priority: Priority,
    order: u64,
    files_total: u32,
    files_wanted: u32,
    skipped_note: String,
    host: String,
    /// (dead, short reason, retry in seconds) for the Error and Dead groups.
    err: Option<(bool, String, u64)>,
    /// Percent done while the files move to a new location (the "Moving… 42%" row).
    moving: Option<u8>,
}

struct State {
    items: Vec<Item>,
    listener: Option<Arc<dyn CoreListener>>,
    limits: (Option<u64>, Option<u64>),
    alt: bool,
    rng: u64,
    next_order: u64,
    lifetime: [(u64, u64); 3],
    /// Priorities set through `set_file_priorities`, by torrent id and file index.
    file_prios: std::collections::HashMap<(String, u32), crate::types::FilePriority>,
}

/// The mock; clones of the `Arc` are shared with the ticker thread via `Weak`.
pub struct MockBackend {
    state: Mutex<State>,
    /// The path monitor said the network is gone (the mock shows the quiet "Offline").
    offline: std::sync::atomic::AtomicBool,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn home() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/Users/me".into())
}

fn fake_id(seed: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut out = String::with_capacity(40);
    for round in 0..3u64 {
        for b in seed.bytes().chain(round.to_le_bytes()) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        out.push_str(&format!("{h:016x}"));
    }
    out.truncate(40);
    out
}

impl State {
    fn noise(&mut self) -> f64 {
        // xorshift64: deterministic, no dependency needed for a mock.
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }

    fn push_order(&mut self) -> u64 {
        self.next_order += 1;
        self.next_order
    }

    fn lock_items<'a>(&'a mut self, ids: &'a [String]) -> impl Iterator<Item = &'a mut Item> {
        self.items.iter_mut().filter(move |i| ids.contains(&i.id))
    }

    fn active_downloads(&self) -> usize {
        self.items
            .iter()
            .filter(|i| i.state == TransferState::Downloading)
            .count()
    }

    /// Starts queued downloads (priority, then queue order) while slots are free.
    fn schedule(&mut self) {
        while self.active_downloads() < MAX_ACTIVE {
            let next = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.state == TransferState::Queued)
                .min_by_key(|(_, i)| (i.priority as u8, i.order))
                .map(|(idx, _)| idx);
            let Some(idx) = next else { break };
            self.items[idx].state = TransferState::Downloading;
        }
    }

    fn tick(&mut self, dt: u64) -> Vec<TransferRow> {
        let (mut dl_scale, mut ul_scale) = (1.0, 1.0);
        let (lim_d, lim_u) = if self.alt {
            (Some(2 * MB), Some(MB / 2))
        } else {
            self.limits
        };
        let base_d: u64 = self
            .items
            .iter()
            .filter(|i| i.state == TransferState::Downloading)
            .map(|i| i.base_down)
            .sum();
        let base_u: u64 = self
            .items
            .iter()
            .filter(|i| i.state == TransferState::Seeding)
            .map(|i| i.base_up)
            .sum();
        if let Some(l) = lim_d.filter(|&l| base_d > l) {
            dl_scale = l as f64 / base_d as f64;
        }
        if let Some(l) = lim_u.filter(|&l| base_u > l) {
            ul_scale = l as f64 / base_u as f64;
        }
        let mut finished = Vec::new();
        for n in 0..self.items.len() {
            let jitter = 0.85 + 0.3 * self.noise();
            let it = &mut self.items[n];
            match it.state {
                TransferState::Downloading => {
                    it.down_rate = (it.base_down as f64 * jitter * dl_scale) as u64;
                    it.up_rate = 0;
                    it.done = (it.done + it.down_rate * dt).min(it.size);
                    let (tr, rate) = (it.tracker, it.down_rate * dt);
                    self.lifetime[tr].0 += rate;
                    if self.items[n].done >= self.items[n].size {
                        let it = &mut self.items[n];
                        it.state = TransferState::Seeding;
                        it.down_rate = 0;
                        it.base_up = 300 * KB;
                        it.finished_at = Some(now());
                        it.recent = true;
                        it.seed_secs = 0;
                        it.goal = it.goal.or(Some(TRACKERS[it.tracker].2));
                        finished.push(n);
                    }
                }
                TransferState::Seeding => {
                    it.up_rate = (it.base_up as f64 * jitter * ul_scale) as u64;
                    it.seed_secs += dt;
                    it.ratio += (it.up_rate * dt) as f64 / it.size.max(1) as f64;
                    let (tr, rate) = (it.tracker, it.up_rate * dt);
                    self.lifetime[tr].1 += rate;
                }
                _ => {
                    it.down_rate = 0;
                    it.up_rate = 0;
                    if it.state == TransferState::LowPower {
                        it.seed_secs += dt;
                    }
                }
            }
        }
        self.schedule();
        finished
            .into_iter()
            .map(|n| self.row(&self.items[n]))
            .collect()
    }

    fn row(&self, it: &Item) -> TransferRow {
        let eta = (it.state == TransferState::Downloading && it.down_rate > 0)
            .then(|| (it.size - it.done) / it.down_rate);
        let (tracker_id, tracker_name) = if it.kind == TransferKind::Direct {
            ("direct".to_string(), it.host.clone())
        } else {
            (TRACKERS[it.tracker].0.to_string(), TRACKERS[it.tracker].1.to_string())
        };
        let complete = it.done >= it.size;
        let seed_group = (it.kind == TransferKind::Torrent && complete)
            .then_some(match (&it.err, it.state) {
                (_, TransferState::Paused) => SeedGroup::Stopped,
                (Some((true, ..)), _) => SeedGroup::Dead,
                (Some(_), _) => SeedGroup::Error,
                (None, TransferState::Seeding) if it.up_rate >= 500 * KB => SeedGroup::Popular,
                (None, TransferState::Seeding) => SeedGroup::Active,
                _ => SeedGroup::Dormant,
            });
        TransferRow {
            id: it.id.clone(),
            name: it.name.clone(),
            display_name: it.name.clone(),
            kind: it.kind,
            category: it.category,
            size_bytes: it.size,
            done_bytes: it.done,
            down_rate: it.down_rate,
            up_rate: it.up_rate,
            eta_secs: eta,
            tracker_id,
            tracker_name,
            ratio: it.ratio,
            ratio_goal: it.goal,
            seed_secs: it.seed_secs,
            state: it.state,
            finished_at: it.finished_at,
            save_path: format!("{}/Downloads/{}", home(), it.name),
            file_ext: it.ext.clone(),
            seed_group,
            error: it.err.as_ref().map(|e| e.1.clone()),
            retry_in_secs: it.err.as_ref().filter(|e| !e.0).map(|e| e.2),
            moving_percent: it.moving,
            awaiting_choice: false,
            pending: None,
            peers: if it.state == TransferState::Downloading { (it.down_rate / (130 * KB)) as u32 } else { 0 },
            swarm: (it.state == TransferState::Downloading).then_some(340),
            known_peers: 0,
            has_video: matches!(it.ext.as_str(), "mkv" | "mp4" | "avi" | "mov" | "m4v" | "webm"),
            video_ext: if it.ext.is_empty() { "mkv".into() } else { String::new() },
            file_count: if it.ext.is_empty() { it.files_total.max(2) } else { 1 },
            last_activity: None,
            waiting_note: None,
            idle_stopped: None,
            waiting_is_error: false,
            tracker_halted: false,
            uploaded_bytes: (it.ratio * it.size as f64) as u64,
            total_bytes: it.size,
            held_bytes: it.done,
            session_done: it.done.min(it.size),
            session_size: it.size,
            files_done: if it.ext.is_empty() { it.files_total.max(2) } else { 1 },
        }
    }

    fn all_rows(&self) -> Vec<TransferRow> {
        self.items.iter().map(|i| self.row(i)).collect()
    }

    fn is_seeding(it: &Item) -> bool {
        matches!(it.state, TransferState::Seeding | TransferState::LowPower) || it.err.is_some()
    }
}

struct Seed<'a>(
    &'a str,
    &'a str,
    Category,
    u64,
    f64,
    Option<f64>,
    u64,
    usize,
);

fn seeding(s: &Seed, active_rate: Option<u64>, order: u64) -> Item {
    let Seed(name, ext, category, size, ratio, goal, days, tracker) = *s;
    Item {
        id: fake_id(name),
        name: name.into(),
        kind: TransferKind::Torrent,
        category,
        size,
        done: size,
        base_down: 0,
        base_up: active_rate.unwrap_or(0),
        down_rate: 0,
        up_rate: active_rate.unwrap_or(0),
        tracker,
        ratio,
        goal,
        goal_is_default: goal.is_some(),
        seed_secs: days * 86_400,
        state: if active_rate.is_some() {
            TransferState::Seeding
        } else {
            TransferState::LowPower
        },
        finished_at: None,
        recent: false,
        ext: ext.into(),
        priority: Priority::Normal,
        order,
        files_total: 1,
        files_wanted: 1,
        skipped_note: String::new(),
        host: String::new(),
        err: None,
        moving: None,
    }
}

#[allow(clippy::too_many_arguments)]
fn download(
    name: &str,
    ext: &str,
    category: Category,
    size: u64,
    pct: u64,
    rate: u64,
    tracker: usize,
    order: u64,
) -> Item {
    let mut it = seeding(
        &Seed(name, ext, category, size, 0.0, None, 0, tracker),
        None,
        order,
    );
    it.done = size / 100 * pct;
    it.base_down = rate;
    it.state = if pct > 0 {
        TransferState::Downloading
    } else {
        TransferState::Queued
    };
    it.goal_is_default = true;
    it
}

fn initial_items() -> Vec<Item> {
    let t = now();
    let mut items = Vec::new();
    let mut o = 0;
    let mut next = || {
        o += 1;
        o
    };
    // Queue order is list order; take the numbers in the order the rows appear.
    // Downloading and queued: concept 2.
    let ubuntu = download(
        "ubuntu-26.04-desktop-arm64.iso",
        "iso",
        Category::Software,
        6_200 * MB,
        74,
        12 * MB,
        2,
        next(),
    );
    let mut sprite = download(
        "Sprite Fright (2021)",
        "mkv",
        Category::Movie,
        5_300 * MB,
        21,
        5 * MB,
        1,
        next(),
    );
    sprite.files_total = 3;
    sprite.files_wanted = 2;
    sprite.skipped_note = "sample skipped".into();
    sprite.goal = Some(1.0);
    sprite.goal_is_default = false;
    items.push(ubuntu);
    items.push(sprite);
    items.push(download(
        "debian-13.2.0-amd64-DVD-1.iso",
        "iso",
        Category::Software,
        3_900 * MB,
        0,
        6 * MB,
        2,
        next(),
    ));
    let mut fedora = download(
        "Fedora-Workstation-Live-43.iso",
        "iso",
        Category::Software,
        2_300 * MB,
        0,
        8 * MB,
        2,
        next(),
    );
    fedora.kind = TransferKind::Direct;
    fedora.host = "download.fedoraproject.org".into();
    items.push(fedora);
    items.push(download(
        "Frankenstein — Mary Shelley (Audiobook)",
        "m4b",
        Category::Audiobook,
        412 * MB,
        0,
        2 * MB,
        2,
        next(),
    ));
    let mut cv = download(
        "Common Voice Corpus 22",
        "",
        Category::Other,
        14 * GB,
        0,
        5 * MB,
        2,
        next(),
    );
    cv.files_total = 4_000;
    cv.files_wanted = 4_000;
    items.push(cv);
    items.push(download(
        "LibriVox — Moby Dick",
        "m4b",
        Category::Audiobook,
        650 * MB,
        0,
        2 * MB,
        2,
        next(),
    ));

    // Sharing, explicit rows from concept 3 (tracker index: 0 z, 1 y, 2 public).
    let explicit_active = [
        (
            Seed(
                "archlinux-2026.10.01-x86_64.iso",
                "iso",
                Category::Software,
                1_300 * MB,
                1.24,
                Some(2.0),
                9,
                0,
            ),
            1200 * KB,
            Some(t - 86_400),
        ),
        (
            Seed(
                "enwiki-20261001-pages-articles",
                "",
                Category::Other,
                24 * GB,
                0.94,
                Some(2.0),
                10,
                0,
            ),
            640 * KB,
            None,
        ),
        (
            Seed(
                "Kevin MacLeod — Royalty Free Vol. 3 (2019)",
                "",
                Category::Music,
                900 * MB,
                0.81,
                Some(1.0),
                3,
                1,
            ),
            410 * KB,
            None,
        ),
        (
            Seed(
                "Night of the Living Dead (1968)",
                "mkv",
                Category::Movie,
                8_300 * MB,
                0.12,
                Some(1.0),
                0,
                1,
            ),
            280 * KB,
            Some(t - 12 * 60),
        ),
        (
            Seed(
                "LibriVox — Pride and Prejudice",
                "m4b",
                Category::Audiobook,
                420 * MB,
                3.10,
                None,
                730,
                0,
            ),
            180 * KB,
            None,
        ),
        (
            Seed(
                "Sintel (2010)",
                "mkv",
                Category::Movie,
                4_100 * MB,
                2.31,
                Some(3.0),
                40,
                2,
            ),
            120 * KB,
            None,
        ),
        (
            Seed(
                "Elephants Dream (2006)",
                "mkv",
                Category::Movie,
                1_900 * MB,
                5.2,
                None,
                400,
                2,
            ),
            100 * KB,
            None,
        ),
        (
            Seed(
                "Fedora-Server-dvd-x86_64-42.iso",
                "iso",
                Category::Software,
                2_700 * MB,
                1.10,
                Some(1.0),
                30,
                1,
            ),
            90 * KB,
            None,
        ),
        (
            Seed(
                "Public Domain Radio Plays",
                "",
                Category::Audiobook,
                3_200 * MB,
                0.40,
                Some(1.0),
                5,
                1,
            ),
            80 * KB,
            None,
        ),
    ];
    for (seed, rate, finished) in &explicit_active {
        let mut it = seeding(seed, Some(*rate), next());
        it.finished_at = *finished;
        it.recent = finished.is_some();
        if let (Some(f), 0) = (finished, seed.6) {
            it.seed_secs = t - f;
        }
        if seed.1.is_empty() {
            it.files_total = 40;
            it.files_wanted = 40;
        }
        // SPEC 7A.5: one torrent is being moved to its new location.
        if seed.0.starts_with("Kevin MacLeod") {
            it.moving = Some(42);
        }
        items.push(it);
    }
    let explicit_low = [
        (
            Seed(
                "Big Buck Bunny 4K (2008)",
                "mkv",
                Category::Movie,
                9 * GB,
                14.7,
                None,
                1095,
                2,
            ),
            None,
        ),
        (
            Seed(
                "Frankenstein — Mary Shelley",
                "epub",
                Category::Book,
                1_200 * KB,
                6.33,
                None,
                730,
                2,
            ),
            Some(t - 3_600),
        ),
        (
            Seed(
                "debian-12.11.0-arm64-netinst.iso",
                "iso",
                Category::Software,
                650 * MB,
                0.96,
                Some(1.0),
                48,
                1,
            ),
            None,
        ),
        (
            Seed(
                "Tears of Steel (2012)",
                "mkv",
                Category::Movie,
                6 * GB,
                4.02,
                None,
                730,
                2,
            ),
            None,
        ),
    ];
    for (seed, finished) in &explicit_low {
        let mut it = seeding(seed, None, next());
        it.finished_at = *finished;
        it.recent = finished.is_some();
        items.push(it);
    }
    // Problem rows (SPEC 8.3 Error and Dead groups).
    let problems = [
        (Seed("Hidden Rivers (2016)", "mkv", Category::Movie, 5_600 * MB, 1.8, Some(2.0), 40, 0), false, "Tracker unreachable", 7_200),
        (Seed("Ubuntu MATE 24.04 amd64", "iso", Category::Software, 3_100 * MB, 0.7, Some(1.0), 12, 1), false, "Files missing: drive not connected", 0),
        (Seed("Common Crawl Sample 2019", "", Category::Other, 12 * GB, 0.2, None, 90, 1), true, "Tracker: Unregistered torrent", 0),
        (Seed("Old Radio Shows 1948", "", Category::Audiobook, 2_000 * MB, 3.3, Some(2.0), 300, 0), true, "Tracker unreachable for 3 days", 0),
    ];
    for (seed, dead, reason, retry) in &problems {
        let mut it = seeding(seed, None, next());
        it.state = TransferState::Error;
        it.err = Some((*dead, (*reason).to_string(), *retry));
        items.push(it);
    }
    // Fill to 212 seeding torrents with tracker counts z=44, y=120, public=48.
    let pool = [
        ("Open Music Archive Vol.", "", Category::Music),
        ("Public Domain Films Part", "mkv", Category::Movie),
        ("Creative Commons Podcast Season", "", Category::Audiobook),
        ("Linux ISO Mirror", "iso", Category::Software),
        ("Gutenberg Books Collection", "epub", Category::Book),
        ("Wikimedia Dump", "", Category::Other),
        ("Open Game Assets Pack", "", Category::Software),
    ];
    let quota = [44u32, 120, 48];
    let mut left = quota;
    for it in &items {
        if State::is_seeding(it) {
            left[it.tracker] = left[it.tracker].saturating_sub(1);
        }
    }
    let mut n = 0usize;
    while left.iter().sum::<u32>() > 0 {
        let tr = (0..3)
            .filter(|&i| left[i] > 0)
            .max_by(|&a, &b| {
                (f64::from(left[a]) / f64::from(quota[a]))
                    .total_cmp(&(f64::from(left[b]) / f64::from(quota[b])))
            })
            .unwrap_or(0);
        left[tr] -= 1;
        let (base, ext, cat) = pool[n % pool.len()];
        let name = format!("{base} {}", n / pool.len() + 1);
        let size = (300 + (n as u64 * 7_919) % 9_000) * MB;
        let ratio = 0.5 + (n as f64 * 0.37) % 9.0;
        let seed = Seed(
            &name,
            ext,
            cat,
            size,
            ratio,
            None,
            20 + (n as u64 * 13) % 900,
            tr,
        );
        items.push(seeding(&seed, None, next()));
        n += 1;
    }
    items
}

impl MockBackend {
    /// Builds the mock and starts its ticker thread (stops when the backend is dropped).
    pub fn start(_config_dir: String) -> Arc<dyn Backend> {
        let items = initial_items();
        let next_order = items.len() as u64 + 1;
        let backend = Arc::new(Self {
            state: Mutex::new(State {
                items,
                listener: None,
                limits: (None, None),
                alt: false,
                rng: 0x9e37_79b9_7f4a_7c15,
                next_order,
                file_prios: std::collections::HashMap::new(),
                lifetime: [
                    (1_820 * GB, 2_410 * GB),
                    (940 * GB, 1_100 * GB),
                    (410 * GB, 1_900 * GB),
                ],
            }),
            offline: std::sync::atomic::AtomicBool::new(false),
        });
        {
            // Prime rates so the first snapshot already shows speeds and ETAs.
            let mut s = backend.lock();
            s.schedule();
            s.tick(0);
        }
        let weak: Weak<Self> = Arc::downgrade(&backend);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(1));
                let Some(b) = weak.upgrade() else { break };
                b.step(1);
            }
        });
        backend
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn step(&self, dt: u64) {
        let (finished, listener) = {
            let mut s = self.lock();
            (s.tick(dt), s.listener.clone())
        };
        if let Some(l) = listener {
            for row in finished {
                l.on_transfer_finished(row);
            }
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if b.get(i + 1..i + 3).is_some() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

impl Backend for MockBackend {
    fn network_path_changed(&self, satisfied: bool) {
        self.offline.store(!satisfied, std::sync::atomic::Ordering::Relaxed);
    }

    fn snapshot(&self) -> Snapshot {
        let s = self.lock();
        let all = s.all_rows();
        let mut recent: Vec<&Item> = s.items.iter().filter(|i| i.recent).collect();
        recent.sort_by_key(|i| std::cmp::Reverse(i.finished_at));
        recent.truncate(MENU_RECENT);
        let recent = recent.into_iter().map(|i| s.row(i)).collect();
        let profiles: Vec<rows::Favorite> = Vec::new();
        rows::snapshot(
            &all,
            recent,
            &profiles,
            Totals {
                down_rate: s.items.iter().map(|i| i.down_rate).sum(),
                up_rate: s.items.iter().map(|i| i.up_rate).sum(),
                alt_speed: s.alt,
                down_limit: None,
                up_limit: None,
                listen_port: 51413,
                connections: 38,
                online: !self.offline.load(std::sync::atomic::Ordering::Relaxed),
                offline_since_ms: self.offline.load(std::sync::atomic::Ordering::Relaxed).then_some(0),
            },
        )
    }

    fn set_listener(&self, listener: Arc<dyn CoreListener>) {
        self.lock().listener = Some(listener);
    }

    fn rows(&self, q: &RowQuery) -> Vec<TransferRow> {
        rows::query(&self.lock().all_rows(), q, &rows::Hidden::default())
    }

    fn sharing_counts(&self, tracker_id: Option<&str>, text: &str) -> SharingCounts {
        rows::counts(&self.lock().all_rows(), tracker_id, text, &rows::Hidden::default())
    }

    fn tracker_hosts(&self, group_id: &str) -> Vec<String> {
        TRACKERS.iter().filter(|t| t.0 == group_id && t.0 != "public").map(|t| t.0.to_string()).collect()
    }

    fn tracker_stats(&self, tracker_id: &str) -> TrackerStats {
        let s = self.lock();
        let n = TRACKERS.iter().position(|t| t.0 == tracker_id).unwrap_or(2);
        TrackerStats {
            lifetime_down: s.lifetime[n].0,
            lifetime_up: s.lifetime[n].1,
            torrents_completed: [61, 118, 86][n],
            files_downloaded: [214, 402, 190][n],
            seed_secs: 0,
        }
    }

    fn tracker_daily(&self, tracker_id: &str, days: u32) -> Vec<DailyStat> {
        let salt = fake_id(tracker_id)
            .bytes()
            .fold(7u64, |a, b| a.wrapping_mul(31).wrapping_add(u64::from(b)));
        (0..days)
            .rev()
            .map(|d| {
                let x = (salt ^ u64::from(d).wrapping_mul(0x9e37_79b9)) % 1000;
                let weekend = if d % 7 < 2 { 1.6 } else { 1.0 };
                let down = (x as f64 / 1000.0) * 6.0 * GB as f64 * weekend / 2.0;
                let up = (((x * 7) % 1000) as f64 / 1000.0) * 5.0 * GB as f64 + 1.0 * GB as f64;
                DailyStat {
                    days_ago: d,
                    down_bytes: down as u64,
                    up_bytes: up as u64,
                }
            })
            .collect()
    }

    fn info(&self, id: &str) -> Result<TorrentInfo, CoreError> {
        let s = self.lock();
        let it = s
            .items
            .iter()
            .find(|i| i.id == id)
            .ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let goal = match (it.goal, it.goal_is_default) {
            (Some(r), true) => format!("Rule default (ratio {r:.1})"),
            (Some(r), false) => format!("Seed to ratio {r:.1}"),
            (None, _) => "Seed forever".into(),
        };
        Ok(TorrentInfo {
            id: it.id.clone(),
            name: it.name.clone(),
            display_name: it.name.clone(),
            category: it.category,
            size_bytes: it.size,
            peers: if it.state == TransferState::Downloading {
                (it.down_rate / (130 * KB)) as u32
            } else {
                0
            },
            down_rate: it.down_rate,
            save_path: format!("{}/Media/{:?}/{}", home(), it.category, it.name),
            priority: it.priority,
            seeding_goal: goal,
            idle_text: None,
            files_wanted: it.files_wanted,
            files_total: it.files_total,
            skipped_note: it.skipped_note.clone(),
            rule_name: if it.goal_is_default {
                "Default".into()
            } else {
                "Custom".into()
            },
            tracker_name: TRACKERS[it.tracker].1.to_string(),
            error: it.err.as_ref().map(|e| e.1.clone()),
            tracker_halted: it.err.as_ref().is_some_and(|e| e.0),
            goal_is_custom: !it.goal_is_default,
            state: it.state,
            added_at: it.finished_at.unwrap_or(0).saturating_sub(3600),
            finished_at: it.finished_at,
            seeded_secs: it.seed_secs,
            ratio: it.ratio,
            uploaded_total: (it.ratio * it.size as f64) as u64,
            downloaded_total: it.size,
            piece_count: (it.size / (4 << 20)).max(1) as u32,
            piece_length: 4 << 20,
            info_hash: it.id.clone(),
            is_private: it.tracker != 0,
            seeds_connected: 0,
            swarm_seeders: Some(120),
            swarm_leechers: Some(30),
            trackers: vec![TrackerLine {
                url: format!("https://{}/announce?passkey=••••••", TRACKERS[it.tracker].1),
                tier: 0,
                status: "announced".into(),
                last_announce: None,
                next_announce: None,
                seeders: Some(120),
                leechers: Some(30),
            }],
        })
    }

    fn tracker_url(&self, id: &str, index: u32) -> Result<String, CoreError> {
        let s = self.lock();
        let it = s.items.iter().find(|i| i.id == id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        if index != 0 {
            return Err(CoreError::NotFound { id: id.into() });
        }
        Ok(format!("https://{}/announce?passkey=mock", TRACKERS[it.tracker].1))
    }

    fn magnet_link(&self, id: &str) -> Result<String, CoreError> {
        let s = self.lock();
        let it = s
            .items
            .iter()
            .find(|i| i.id == id)
            .ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        // Never a tracker URL: those can carry a passkey (SPEC §7B.4).
        Ok(format!(
            "magnet:?xt=urn:btih:{}&dn={}",
            it.id,
            percent_encode(&it.name)
        ))
    }

    fn add_source(&self, source: &str) -> Result<String, CoreError> {
        let src = source.trim();
        let (id, name, kind) = if let Some(rest) = src.strip_prefix("magnet:?") {
            let param = |key: &str| {
                rest.split('&')
                    .find_map(|p| p.strip_prefix(key))
                    .map(str::to_string)
            };
            let xt = param("xt=urn:btih:").ok_or_else(|| CoreError::InvalidSource {
                message: "magnet link has no btih hash".into(),
            })?;
            let name = param("dn=").map_or_else(|| xt.clone(), |d| percent_decode(&d));
            (xt.to_lowercase(), name, TransferKind::Torrent)
        } else if src.ends_with(".torrent") {
            let name = src
                .rsplit('/')
                .next()
                .unwrap_or(src)
                .trim_end_matches(".torrent")
                .to_string();
            (fake_id(src), name, TransferKind::Torrent)
        } else if src.starts_with("http://") || src.starts_with("https://") {
            let name = src
                .rsplit('/')
                .next()
                .filter(|n| !n.is_empty())
                .unwrap_or("download")
                .to_string();
            (fake_id(src), name, TransferKind::Direct)
        } else {
            return Err(CoreError::InvalidSource {
                message: "not a magnet link, .torrent or URL".into(),
            });
        };
        let mut s = self.lock();
        if s.items.iter().any(|i| i.id == id) {
            return Ok(id);
        }
        let order = s.push_order();
        let ext = name
            .rsplit_once('.')
            .map_or(String::new(), |(_, e)| e.to_lowercase());
        let mut it = download(
            &name,
            &ext,
            Category::Other,
            1_500 * MB,
            0,
            4 * MB,
            2,
            order,
        );
        it.id = id.clone();
        it.kind = kind;
        it.host = src.split('/').nth(2).unwrap_or("").to_string();
        s.items.push(it);
        s.schedule();
        Ok(id)
    }

    fn pause(&self, ids: &[String]) {
        use TransferState::{Downloading, LowPower, Paused, Queued, Seeding};
        let mut s = self.lock();
        for it in s
            .lock_items(ids)
            .filter(|i| matches!(i.state, Downloading | Queued | Seeding | LowPower))
        {
            it.state = Paused;
            it.down_rate = 0;
            it.up_rate = 0;
        }
        s.schedule();
    }

    fn resume(&self, ids: &[String]) {
        let mut s = self.lock();
        for it in s
            .lock_items(ids)
            .filter(|i| i.state == TransferState::Paused)
        {
            it.state = if it.done < it.size {
                TransferState::Queued
            } else {
                TransferState::LowPower
            };
        }
        s.schedule();
    }

    fn move_to_top(&self, id: &str) {
        let mut s = self.lock();
        let top = s
            .items
            .iter()
            .map(|i| i.order)
            .min()
            .unwrap_or(1)
            .saturating_sub(1);
        if let Some(it) = s.items.iter_mut().find(|i| i.id == id) {
            it.order = top;
            if it.state == TransferState::Queued {
                // Bump a running download back to the queue so the top item starts now.
                it.state = TransferState::Downloading;
            }
        }
        // Over the slot limit: demote the slowest-ordered running download.
        while s.active_downloads() > MAX_ACTIVE {
            let worst = s
                .items
                .iter_mut()
                .filter(|i| i.state == TransferState::Downloading)
                .max_by_key(|i| i.order);
            if let Some(w) = worst {
                w.state = TransferState::Queued;
                w.down_rate = 0;
            }
        }
    }

    fn remove(&self, ids: &[String], _delete_files: bool) {
        let mut s = self.lock();
        s.items.retain(|i| !ids.contains(&i.id));
        s.schedule();
    }

    fn set_priority(&self, ids: &[String], priority: Priority) {
        let mut s = self.lock();
        for it in s.lock_items(ids) {
            it.priority = priority;
        }
    }

    fn set_category(&self, ids: &[String], category: Category) {
        let mut s = self.lock();
        for it in s.lock_items(ids) {
            it.category = category;
        }
    }

    fn set_seeding_goal(&self, ids: &[String], goal: SeedingGoal) {
        let mut s = self.lock();
        for it in s.lock_items(ids) {
            (it.goal, it.goal_is_default) = match goal {
                SeedingGoal::RuleDefault => (Some(TRACKERS[it.tracker].2), true),
                SeedingGoal::Ratio { ratio } => (Some(ratio), false),
                SeedingGoal::Forever => (None, false),
            };
        }
    }

    fn pause_all(&self) {
        let ids: Vec<String> = self
            .lock()
            .items
            .iter()
            .filter(|i| i.done < i.size)
            .map(|i| i.id.clone())
            .collect();
        self.pause(&ids);
    }

    fn resume_all(&self) {
        let ids: Vec<String> = self.lock().items.iter().map(|i| i.id.clone()).collect();
        self.resume(&ids);
    }

    fn clear_recent(&self) {
        for it in &mut self.lock().items {
            it.recent = false;
        }
    }

    fn set_limits(&self, down: Option<u64>, up: Option<u64>) {
        self.lock().limits = (down, up);
    }

    fn set_alt_speed(&self, on: bool) {
        self.lock().alt = on;
    }

    fn set_rule(&self, _ids: &[String], _rule: Option<String>) {}

    fn verify(&self, _ids: &[String]) {}

    fn ask_tracker_again(&self, _ids: &[String]) -> AskAgainOutcome {
        AskAgainOutcome { sent: 1, deferred: 0, wait_secs: None }
    }

    fn torrent_attrs(&self) -> Vec<AttrRow> {
        let s = self.lock();
        s.items
            .iter()
            .filter(|i| i.kind == TransferKind::Torrent)
            .map(|i| AttrRow {
                id: i.id.clone(),
                profile: (TRACKERS[i.tracker].0 != "public").then(|| TRACKERS[i.tracker].0.to_string()),
                private: TRACKERS[i.tracker].0 != "public",
                category: i.category,
            })
            .collect()
    }

    fn agent_activity(&self, _limit: u32) -> Vec<AgentActivity> {
        let t = now() * 1000;
        [
            (14 * 60 + 32, "Claude", "add", "added Fedora-Workstation-Live-43.iso into Software"),
            (14 * 60 + 30, "Claude", "settings_set", "set Limit Speed schedule: weekdays 08:00-17:00"),
            (14 * 60 + 29, "Claude", "pause", "paused 3 transfers from tracker-y.net"),
            (14 * 60 + 28, "Claude", "remove", "asked to remove debian-12.11 netinst, waiting for confirmation"),
        ]
        .iter()
        .enumerate()
        .map(|(i, (_, c, m, sum))| AgentActivity {
            at_ms: t - (i as u64) * 90_000,
            client: (*c).into(),
            method: (*m).into(),
            summary: (*sum).into(),
            outcome: "ok".into(),
        })
        .collect()
    }

    fn control_socket(&self) -> Option<String> {
        None
    }

    fn shutdown(&self, _deadline_secs: u32) -> ShutdownInfo {
        ShutdownInfo { confirmed: 0, abandoned: 0, finished: true }
    }

    fn shorten_shutdown(&self, _secs: u32) {}

    fn diagnostics(&self) -> DiagnosticsInfo {
        DiagnosticsInfo { wakeups: 0, connections: 38, rechecks: 0, hash_failures: 0, peers_banned: 0, promotions: 0, demotions: 0 }
    }

    fn location_moves(&self) -> MoveProposalInfo {
        MoveProposalInfo { items: Vec::new(), total_bytes: 0, cross_volume: 0 }
    }

    fn move_torrents(&self, _ids: &[String]) -> MoveResult {
        MoveResult { queued: 0, refused: Vec::new() }
    }

    fn move_to(&self, _ids: &[String], _path: &std::path::Path) -> Result<MoveResult, CoreError> {
        Ok(MoveResult { queued: 0, refused: Vec::new() })
    }

    fn cancel_moves(&self, _ids: &[String]) {}

    fn choice(&self, id: &str, _reopen: bool) -> Result<ChoiceInfo, CoreError> {
        Err(CoreError::NotFound { id: id.into() })
    }

    fn confirm_choice(&self, id: &str, _files: &[bool]) -> Result<(), CoreError> {
        Err(CoreError::NotFound { id: id.into() })
    }

    fn cancel_choice(&self, id: &str) -> Result<(), CoreError> {
        Err(CoreError::NotFound { id: id.into() })
    }

    fn watch_backlog(&self, _folder: &std::path::Path) -> Vec<String> {
        Vec::new()
    }

    fn files(&self, id: &str) -> Result<Vec<TorrentFile>, CoreError> {
        let s = self.lock();
        let it = s
            .items
            .iter()
            .find(|i| i.id == id)
            .ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        // A single file when the row has an extension; otherwise an episode or track list that
        // matches the category, so the Play entries have something to show.
        let paths: Vec<String> = if !it.ext.is_empty() {
            vec![it.name.clone()]
        } else if it.category == Category::Tv {
            (1..=it.files_total.clamp(1, 12)).map(|e| format!("{}/{}.E{e:02}.mkv", it.name, it.name)).collect()
        } else {
            vec![format!("{}/{}.mkv", it.name, it.name)]
        };
        let per = it.size / paths.len().max(1) as u64;
        let facts: Vec<crate::play::FileFacts<'_>> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let done = (it.done.saturating_sub(per * i as u64)).min(per);
                let priority = s.file_prios.get(&(id.to_owned(), i as u32)).copied().unwrap_or(FilePriority::Normal);
                crate::play::FileFacts { path: p, disk: p, length: per, done, priority, padding: false, uploaded: 0 }
            })
            .collect();
        Ok(crate::play::list(&facts))
    }

    fn set_file_priorities(&self, id: &str, changes: &[FilePriorityChange]) -> Result<(), CoreError> {
        let mut s = self.lock();
        if !s.items.iter().any(|i| i.id == id) {
            return Err(CoreError::NotFound { id: id.into() });
        }
        for c in changes {
            s.file_prios.insert((id.to_owned(), c.index), c.priority);
        }
        Ok(())
    }

    fn stream_url(&self, id: &str, file_index: u32) -> Result<StreamInfo, CoreError> {
        let files = self.files(id)?;
        let f = crate::play::check_streamable(&files, file_index)?;
        // Demo data has no engine: a well-formed URL that nothing listens on.
        Ok(StreamInfo {
            url: format!("http://127.0.0.1:9/mock-token/{}", f.name),
            token: "mock-token".into(),
            complete: false,
            selected: false,
            content_type: trs_stream::mime::content_type(&f.name).to_owned(),
            is_mp4_family: f.is_mp4_family,
        })
    }
}
