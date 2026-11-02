//! The calls the app makes on its main thread never wait for long work: a first scan of a big folder, a folder's core
//! held (as a long pull holds it), a rescan, a move, a chat message whose big file is being read, an engine whose
//! actor is busy. Each call is timed while that work runs. The FFI's `async` calls (`async_calls`) are timed to their
//! first poll, which is what a host's UI thread waits for: they must say "pending" at once.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::calls::testing::{first_poll, wait};
use crate::chat_images::ChatImageInput;
use crate::settings::SettingsStore;
use crate::tracker_page::TrackerInfoCommand;
use crate::*;

/// What one call may take, measured around the call alone. A call waits at most `UI_WAIT` (40 ms) for a fresh read
/// and then shows the last one; the rest is room for the system to wake the waiting thread (its waits are made of short
/// slices, so a runner that coalesces timers adds little: `calls::wait_until`). The work a call must not wait for lasts
/// seconds here, so a call that waited for it shows as that.
const BOUND: Duration = Duration::from_millis(100);

/// How late this process's timers fire: the latest of a few plain 40 ms waits that nothing ends early. Said when a
/// call was slow, so that a runner that delays timers shows as one.
fn timer_lateness() -> Duration {
    let (m, cv) = (std::sync::Mutex::new(()), std::sync::Condvar::new());
    let wait = Duration::from_millis(40);
    (0..5)
        .map(|_| {
            let at = Instant::now();
            let g = m.lock().unwrap();
            drop(cv.wait_timeout(g, wait).unwrap());
            at.elapsed().saturating_sub(wait)
        })
        .max()
        .unwrap_or_default()
}

struct Clock;

impl LocalClock for Clock {
    fn time_of_day(&self, s: u64) -> String {
        s.to_string()
    }
    fn weekday_time(&self, s: u64) -> String {
        s.to_string()
    }
    fn day(&self, s: u64, _: bool) -> String {
        s.to_string()
    }
    fn date_time(&self, s: u64) -> String {
        s.to_string()
    }
    fn utc_offset_s(&self, _: u64) -> i32 {
        0
    }
}

/// A real core (engine and folder sync) in temporary folders.
fn real_core() -> (Arc<Core>, PathBuf, [tempfile::TempDir; 2]) {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // SAFETY: set before the core starts any thread; nextest runs every test in its own process.
    unsafe { std::env::set_var("HOME", home.path()) };
    let dir = work.path().join("state");
    let store = SettingsStore::new(&dir);
    let mut st = store.load_settings();
    st.folders.download_dir = work.path().join("done").display().to_string();
    st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
    st.folders.watch.clear();
    st.network.port_mapping = false;
    store.save_settings(&st).unwrap();
    let options = CoreOptions { config_dir: dir.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None };
    let core = Core::new(options).unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    while !core.sync_snapshot().running {
        assert!(Instant::now() < end, "folder sync did not start: {:?}", core.sync_snapshot().problem);
        std::thread::sleep(Duration::from_millis(50));
    }
    let work_dir = work.path().to_path_buf();
    (core, work_dir, [home, work])
}

/// A folder of `small` small files and `big` files of 6 MiB (hashed apart from the core).
fn tree(at: &Path, small: usize, big: usize) {
    for d in 0..small.div_ceil(100) {
        let dir = at.join(format!("d{d:03}"));
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..100.min(small - d * 100) {
            std::fs::write(dir.join(format!("f{i:03}.txt")), format!("file {d} {i}\n").repeat(50)).unwrap();
        }
    }
    for i in 0..big {
        let bytes: Vec<u8> = (0..6u32 << 20).map(|n| (n.wrapping_mul(2_654_435_761).wrapping_add(i as u32) >> 13) as u8).collect();
        std::fs::write(at.join(format!("big{i}.bin")), bytes).unwrap();
    }
}

/// Every call the app makes on its main thread while a window, the menu or Settings is open, timed: the slowest of each.
struct Calls {
    core: Arc<Core>,
    library: Arc<Library>,
    favorites: Arc<FavoriteEditor>,
    slowest: BTreeMap<&'static str, Duration>,
    rounds: u32,
}

impl Calls {
    fn new(core: &Arc<Core>) -> Self {
        let library = Library::new(core.clone(), Vec::new(), Arc::new(Clock));
        Calls { core: core.clone(), library, favorites: FavoriteEditor::new(core.clone()), slowest: BTreeMap::new(), rounds: 0 }
    }

    fn time<T>(&mut self, name: &'static str, f: impl FnOnce(&Core) -> T) -> T {
        let at = Instant::now();
        let out = f(&self.core);
        self.note(name, at.elapsed());
        out
    }

    fn note(&mut self, name: &'static str, took: Duration) {
        let e = self.slowest.entry(name).or_default();
        *e = (*e).max(took);
    }

    /// An `async` call, timed to its first poll (what the host's UI thread waits for). It runs on and its result is
    /// dropped.
    fn time_async<F: Future>(&mut self, name: &'static str, f: impl FnOnce(Arc<Core>) -> F) {
        let fut = f(self.core.clone());
        let mut fut = std::pin::pin!(fut);
        let (took, _) = first_poll(fut.as_mut());
        self.note(name, took);
    }

    /// The polls of a tracker's page, Settings and the starting notifier, and a row's menu, for the held torrent `id`.
    fn held_round(&mut self, id: &str, choices: &Arc<Choices>, starting: &Arc<Starting>) {
        let id = id.to_string();
        self.time("info", |c| c.info(id.clone()).ok());
        let row = self.core.snapshot().downloading.into_iter().find(|r| r.id == id);
        if let Some(row) = row {
            self.time("play_offer", |c| c.play_offer(row.clone()));
            self.time("play_main_video", |c| c.play_main_video(row));
        }
        self.time("play_players", |c| c.play_players());
        self.time("play_with_choices", |c| c.play_with_choices("auto".into()));
        let (ch, st) = (choices.clone(), starting.clone());
        let i = id.clone();
        self.time("Choices.chooser", move |_| ch.chooser(i, false));
        let (ch, i) = (choices.clone(), id.clone());
        self.time("Choices.has_source", move |_| ch.has_source(i));
        self.time("Starting.refresh", move |c| st.refresh(c.snapshot().downloading, 1_792_938_725_000, false));
        let i = id.clone();
        self.time_async("async tracker_stats", |c| c.tracker_stats_async("public".into()));
        self.time_async("async tracker_daily", |c| c.tracker_daily_async("public".into(), 30));
        self.time_async("async volume_warnings", |c| c.volume_warnings_async());
        self.time_async("async has_held_source_file", |c| c.has_held_source_file_async(i.clone()));
        self.time_async("async choice", |c| c.choice_async(i.clone(), false));
        self.time_async("async files", |c| c.files_async(i.clone()));
        // The settings of folder sync and chat, and a favorite's saves: the host awaits them too.
        let settings = self.core.sync_settings();
        let port = settings.port;
        self.time_async("async set_sync_settings", |c| c.set_sync_settings_async(settings));
        self.time_async("async set_sync_port", |c| c.set_sync_port_async(port));
        self.time_async("async chat_set_muted", |c| c.chat_set_muted_async("00".repeat(16), true));
        self.time_async("async chat_rename", |c| c.chat_rename_async("00".repeat(16), "Renamed".into()));
        let f = self.favorites.clone();
        self.time_async("async FavoriteEditor.set_limits", |_| f.set_limits_async("public".into(), 1_000_000, 0));
        let f = self.favorites.clone();
        self.time_async("async FavoriteEditor.tracker_info_run", |_| f.tracker_info_run_async("public".into(), TrackerInfoCommand::SaveFolders));
        let f = self.favorites.clone();
        self.time("FavoriteEditor.view", move |_| f.view());
    }

    /// One round of the app's polls (the menu, the library window, Finder's badges, the chat tick, Settings).
    fn round(&mut self, folder: Option<&str>, place: Option<&str>) {
        self.rounds += 1;
        self.time("snapshot", |c| c.snapshot());
        self.time("sync_snapshot", |c| c.sync_snapshot());
        let q = RowQuery { group: None, tracker_id: None, text: String::new(), category: None, offset: 0, limit: 200 };
        self.time("sharing_rows", |c| c.sharing_rows(q));
        self.time("sharing_counts", |c| c.sharing_counts(None, None, String::new()));
        self.time("chat_channels", |c| c.chat_channels());
        self.time("chat_directs", |c| c.chat_directs());
        self.time("devices_view", |c| c.devices_view());
        self.time("sync_settings", |c| c.sync_settings());
        self.time("sync_reachability", |c| c.sync_reachability());
        self.time("agents_awake", |c| c.agents_awake());
        self.time("listen_port", |c| c.listen_port());
        let lib = self.library.clone();
        let at = Instant::now();
        lib.model(1_792_938_725_000);
        let e = self.slowest.entry("Library.model").or_default();
        *e = (*e).max(at.elapsed());
        if let Some(id) = folder {
            let id = id.to_string();
            self.time("sync_activity", |c| c.sync_activity(id.clone()));
            self.time("sync_members", |c| c.sync_members(id.clone()));
            self.time("sync_invites", |c| c.sync_invites(id.clone()));
        }
        if let Some(p) = place {
            let p = p.to_string();
            self.time("chat_topics", |c| c.chat_topics(p.clone()));
            self.time("chat_pending_sends", |c| c.chat_pending_sends(p.clone()));
            self.time("chat_page_read", |c| c.chat_page_read(LibraryPlace::Chat { channel_id: p.clone(), topic_id: None }));
        }
    }

    /// Rounds for `d`, or until `done` says the work ended (after `min_rounds` at least).
    fn rounds_while(&mut self, d: Duration, folder: Option<&str>, place: Option<&str>, done: impl Fn(&Core) -> bool) {
        let end = Instant::now() + d;
        while Instant::now() < end && !(self.rounds >= 5 && done(&self.core)) {
            self.round(folder, place);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_quick(&self, what: &str) {
        assert!(self.rounds >= 5, "{what}: only {} rounds", self.rounds);
        let slow: Vec<_> = self.slowest.iter().filter(|(_, d)| **d > BOUND).collect();
        assert!(
            slow.is_empty(),
            "{what}: calls of the app's main thread waited: {slow:?} (all: {:?}; a plain 40 ms timer fires up to {:?} late here)",
            self.slowest,
            timer_lateness()
        );
    }
}

fn folder_state(core: &Core, id: &str) -> Option<SyncFolder> {
    core.sync_snapshot().folders.into_iter().find(|f| f.id == id)
}

#[test]
fn the_apps_calls_never_wait_for_a_first_scan_a_held_folder_a_rescan_or_a_busy_engine() {
    let (core, work, _tmp) = real_core();
    let place = work.join("Big");
    tree(&place, 3000, 6);
    // Primed as the app is, then the folder is added: its first scan hashes everything.
    Calls::new(&core).round(None, None);
    let f = core.sync_add_folder(place.display().to_string()).unwrap();
    let id = f.id.clone();

    // The first scan, and meanwhile the engine's actor busy for seconds.
    core.backend.test_stall_engine(Duration::from_secs(3));
    let mut calls = Calls::new(&core);
    let mut grew = Vec::new();
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        calls.round(Some(&id), None);
        let s = folder_state(&core, &id).unwrap();
        if s.state == SyncState::Scanning || grew.is_empty() {
            grew.push(s.files);
        } else {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    calls.assert_quick("during the first scan and a busy engine");
    // The count of files grows while the scan runs (each step is committed and shown).
    let mid: Vec<u64> = grew.iter().copied().filter(|n| *n > 0 && *n < 3006).collect();
    assert!(!mid.is_empty() || grew.len() < 3, "the files are counted as the scan goes: {grew:?}");

    // The folder's core held (as a long pull holds it) for three seconds.
    let (h, _) = core.backend.sync().service().unwrap();
    let fid: trs_sync_proto::ids::FolderId = id.parse().unwrap();
    let mut calls = Calls::new(&core);
    h.test_lock_core(&fid, Duration::from_secs(3));
    calls.rounds_while(Duration::from_millis(2500), Some(&id), None, |_| false);
    calls.assert_quick("while the folder's core is held");

    // A rescan with new files to hash.
    tree(&place.join("more"), 1000, 2);
    let mut calls = Calls::new(&core);
    core.sync_rescan(id.clone()).unwrap();
    calls.rounds_while(Duration::from_secs(20), Some(&id), None, |c| folder_state(c, &id).is_some_and(|f| f.state != SyncState::Scanning && f.files >= 4000));
    calls.assert_quick("during a rescan");
    core.shutdown(5);
}

/// The engine busy from the start: the app's first library read waits for it (there is nothing to show meanwhile),
/// but once that wait ran out, the calls after it do not wait again while that read is still under way.
#[test]
fn a_first_library_read_that_ran_out_is_not_waited_for_again() {
    let (core, _work, _tmp) = real_core();
    core.backend.test_stall_engine(Duration::from_secs(7));
    std::thread::sleep(Duration::from_millis(50));
    let at = Instant::now();
    core.snapshot();
    let first = at.elapsed();
    assert!(first >= Duration::from_secs(1), "the first read waited {first:?}: a read was made before it (this test needs none)");
    let mut calls = Calls::new(&core);
    for _ in 0..5 {
        calls.round(None, None);
    }
    calls.assert_quick("after a first read that ran out");
    core.shutdown(5);
}

/// The host makes the first library read off its UI thread at start (it may wait for a busy engine). A menu or a window
/// opened while that read is still awaited never waits for it: it is bounded as every later read is (an empty library
/// meanwhile), and only the start's read waits.
#[test]
fn calls_made_while_the_first_library_read_is_awaited_do_not_wait_for_it() {
    let (core, _work, _tmp) = real_core();
    core.backend.test_stall_engine(Duration::from_secs(7));
    std::thread::sleep(Duration::from_millis(50));
    let c = core.clone();
    let first = std::thread::spawn(move || {
        let at = Instant::now();
        c.snapshot();
        at.elapsed()
    });
    std::thread::sleep(Duration::from_millis(100));
    assert!(!first.is_finished(), "the start's first read waits for the busy engine");
    let mut calls = Calls::new(&core);
    for _ in 0..5 {
        calls.round(None, None);
    }
    calls.assert_quick("while the first read is awaited");
    assert!(core.snapshot().downloading.is_empty(), "nothing read yet: an empty library");
    let waited = first.join().unwrap();
    assert!(waited >= Duration::from_secs(1), "the start's read waited {waited:?}");
    core.shutdown(5);
}

#[test]
fn the_apps_calls_never_wait_for_a_folder_move_or_a_big_file_being_sent() {
    let (core, work, _tmp) = real_core();
    let place = work.join("Moving");
    tree(&place, 2000, 4);
    let f = core.sync_add_folder(place.display().to_string()).unwrap();
    let id = f.id.clone();

    // A move while the first scan runs (it stops the folder, moves the files, opens it again), from another thread as
    // the app's Edit Folder does.
    let to = work.join("Moved");
    let (c2, id2, to2) = (core.clone(), id.clone(), to.display().to_string());
    let mover = std::thread::spawn(move || c2.sync_move(id2, to2));
    let mut calls = Calls::new(&core);
    calls.rounds_while(Duration::from_secs(30), Some(&id), None, |_| mover.is_finished());
    let moved = mover.join().unwrap();
    calls.assert_quick("during a move");
    // (The first scan, still running, ends at its next step: the folder is free to move at once.)
    assert!(moved.is_ok(), "{moved:?}");
    assert!(to.join("big0.bin").exists() && !place.exists());

    // A chat message with a big file: the file is read (hashed) before the message goes.
    let (h, rt) = core.backend.sync().service().unwrap();
    let channel = rt.block_on(h.chat_new("Files", false)).unwrap();
    let channel = channel.id;
    let big = work.join("film.bin");
    let bytes: Vec<u8> = (0..96u32 << 20).map(|n| (n.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    std::fs::write(&big, bytes).unwrap();
    let input = ChatImageInput { name: "film.bin".into(), path: Some(big.display().to_string()), bytes: None };
    let mut calls = Calls::new(&core);
    core.chat_send_attachments(channel.clone(), None, "the film".into(), None, vec![input]).unwrap();
    let ch = channel.clone();
    calls.rounds_while(Duration::from_secs(30), None, Some(&channel), move |c| c.chat_pending_sends(ch.clone()).is_empty());
    calls.assert_quick("while a big file is prepared for a chat message");
    core.shutdown(5);
}

/// A `.torrent` file of one 40 kB file named `name`, nobody serving it.
fn torrent_file(dir: &Path, name: &str, seed: u8) -> String {
    let mut t = format!("d4:infod6:lengthi40000e4:name{}:{name}12:piece lengthi65536e6:pieces20:", name.len()).into_bytes();
    t.extend_from_slice(&[seed; 20]);
    t.extend_from_slice(b"ee");
    let f = dir.join(format!("{name}.torrent"));
    std::fs::write(&f, &t).unwrap();
    f.display().to_string()
}

#[test]
fn the_notifier_the_menus_and_the_pages_never_wait_for_a_busy_engine() {
    let (core, work, _tmp) = real_core();
    // A torrent held in its countdown (the default grace period), its `.torrent` file still in place.
    let id = core.add_source(torrent_file(&work, "Sintel.2010.1080p.BluRay.x264-GRP.mkv", 3)).unwrap();
    let choices = Choices::new(core.clone());
    let starting = Starting::new(core.clone(), choices.clone());
    // Primed as the app is (a library read made, the add's command answered).
    let mut calls = Calls::new(&core);
    calls.round(None, None);
    calls.held_round(&id, &choices, &starting);
    calls.round(None, None);

    // The engine's actor busy for seconds (a commit, a stat of a hung disk).
    core.backend.test_stall_engine(Duration::from_secs(3));
    std::thread::sleep(Duration::from_millis(50));
    let mut calls = Calls::new(&core);
    let end = Instant::now() + Duration::from_millis(2500);
    while Instant::now() < end {
        calls.round(None, None);
        calls.held_round(&id, &choices, &starting);
        std::thread::sleep(Duration::from_millis(20));
    }
    calls.assert_quick("while the engine is busy");
    core.shutdown(5);
}

#[test]
fn the_users_commands_return_at_once_and_reach_a_busy_engine_in_order() {
    let (core, work, _tmp) = real_core();
    let id = core.add_source(torrent_file(&work, "Big.Buck.Bunny.2008.1080p.BluRay.x264-GRP.mkv", 4)).unwrap();
    core.pending_start_now(id.clone()).unwrap();
    core.backend.test_stall_engine(Duration::from_secs(2));
    std::thread::sleep(Duration::from_millis(50));
    // Pause, resume and pause again, then commands that wait for the engine's answer, while the engine cannot answer:
    // each call returns at once (its first poll; one whose work is done by then is ready).
    let ids = vec![id.clone()];
    let mut pending: Vec<std::pin::Pin<Box<dyn Future<Output = ()> + Send>>> = Vec::new();
    let mut give = |name: &str, mut f: std::pin::Pin<Box<dyn Future<Output = ()> + Send>>| {
        let (took, p) = first_poll(f.as_mut());
        assert!(took < BOUND, "{name} waited {took:?}");
        if p.is_pending() {
            pending.push(f);
        }
    };
    let c = core.clone();
    give("pause", Box::pin(c.clone().pause_async(ids.clone())));
    give("resume", Box::pin(c.clone().resume_async(ids.clone())));
    give("pause again", Box::pin(c.clone().pause_async(ids.clone())));
    give("set_limits", Box::pin(c.clone().set_limits_async(None, None)));
    give("ask_tracker_again", Box::pin(async move { c.ask_tracker_again_async(vec![]).await; }));
    let c = core.clone();
    give("tracker_stats", Box::pin(async move { c.tracker_stats_async("public".into()).await; }));
    // They all finish, in the order they were given: the torrent ends paused.
    for f in pending {
        wait(f);
    }
    let paused = (0..100).any(|_| {
        std::thread::sleep(Duration::from_millis(50));
        let s = core.snapshot();
        s.downloading.iter().chain(&s.queued).any(|r| r.id == id && r.state == TransferState::Paused)
    });
    assert!(paused, "the last command (pause) is the one in force");
    // A result comes back through the future: a move of a torrent that does not exist is refused.
    let moved = wait(core.clone().move_to_async(vec!["00".repeat(20)], work.join("x").display().to_string()));
    assert!(moved.as_ref().map_or(true, |m| m.queued == 0), "{moved:?}");
    core.shutdown(5);
}
