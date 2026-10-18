//! The persistence boundary.
//!
//! The engine never touches `redb` or the config files. It talks to a [`Store`], a small trait the
//! app implements on top of `crates/state` (ENGINE.md 7):
//!
//! * **Reads** happen at start-up ([`Store::load_torrents`]) and lazily when a torrent wakes
//!   (resume data, the original `.torrent`, cached peers). They are synchronous and cheap.
//! * **Writes** are batched. The engine collects changes, waits [`crate::EngineConfig::persist_debounce`]
//!   and hands one [`Batch`] to a dedicated writer thread, so a slow commit never blocks the
//!   runtime. A batch must be applied atomically.
//! * **Tracker timing** goes through [`Store::timing_store`]: `trs-tracker` needs each `save` to
//!   be durable *before* it releases a request (at-most-once `completed`), so those writes are
//!   synchronous and not batched.
//!
//! [`MemStore`] is the in-memory implementation used by the tests.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use trs_tracker::store::{StoreError as TimingError, TimingKey, TimingRecord, TimingStore};

use crate::api::{Category, SeedGoal, TorrentId};

/// Error from the backing store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for StoreError {}

/// What the user asked the torrent to be doing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredState {
    /// Running (downloading, seeding or queued).
    Active,
    /// Paused by the user.
    Paused,
    /// Stopped because the seeding goal was reached.
    GoalReached,
    /// Failed; the text is shown to the user.
    Error(String),
}

/// The compact per-torrent record (`torrents` table).
#[derive(Clone, Debug, PartialEq)]
pub struct TorrentRecord {
    /// Swarm id.
    pub id: TorrentId,
    /// Upload history for the inactive stop condition (SPEC §7.5); `None` = the condition is off.
    pub idle: Option<crate::idle::IdleRing>,
    /// Display name.
    pub name: String,
    /// False for a magnet whose metadata has not arrived.
    pub has_metadata: bool,
    /// The magnet URI the torrent was added with, if any.
    pub magnet: Option<String>,
    /// Announce tiers. Raw URLs: they may hold passkeys, never log them. `None` in a batch means
    /// "unchanged": the engine does not keep the tiers of Cold torrents in memory.
    pub announce: Option<Vec<Vec<String>>>,
    /// Whole torrent size in bytes (0 until metadata).
    pub total_length: u64,
    /// Bytes of the files the user wants (0 until metadata).
    pub wanted_length: u64,
    /// Verified wanted bytes when a finished torrent became a download again (0 = none). Progress of the
    /// download counts from here; cleared when it finishes.
    pub session_base: u64,
    /// Piece length (0 until metadata).
    pub piece_length: u32,
    /// BEP 27 private flag.
    pub private: bool,
    /// Directory the torrent is saved in.
    pub save_path: PathBuf,
    /// Desired state.
    pub state: StoredState,
    /// Per-file priorities (`0` = skip); empty means all Normal.
    pub file_priorities: Vec<u8>,
    /// `0` High, `1` Normal, `2` Low. The effective value (rule-derived unless `priority_user`).
    pub priority: u8,
    /// The user chose `priority` explicitly; rule edits do not touch it.
    pub priority_user: bool,
    /// Queue position.
    pub queue_pos: u32,
    /// Detected or chosen category.
    pub category: Option<Category>,
    /// The user chose the category (do not re-detect).
    pub category_user: bool,
    /// Matched tracker profile id (`trackers.toml`).
    pub tracker_profile: Option<String>,
    /// Rule id forced for this torrent.
    pub rule: Option<String>,
    /// Unix seconds added.
    pub added_at: u64,
    /// Unix seconds finished.
    pub finished_at: Option<u64>,
    /// Seeding goal in effect.
    pub seed_goal: SeedGoal,
    /// The user chose `seed_goal` explicitly; rule edits do not touch it.
    pub goal_user: bool,
    /// The torrent was complete when first added (never send `completed`).
    pub added_complete: bool,
    /// Wrote its data below `.incomplete` that still has to be finalized.
    pub staged: bool,
    /// Every piece was verified when this was written.
    pub complete: bool,
    /// Placement state (see [`Placement`]).
    pub placed: Placed,
    /// Unix seconds of the last upload or interested peer (0 = never seen; the engine then
    /// starts from the finish time). Written coarsely (every ten minutes at most).
    pub last_activity: u64,
    /// Unix seconds when the current error began (0 = none).
    pub error_since: u64,
    /// The current error is the trackers failing (cleared when one answers).
    pub error_tracker: bool,
    /// Staging root of unfinished data (`<root>/<info-hash>/`), as chosen when the torrent was
    /// added or last moved; `None` = below `<save_path>/.incomplete` (and for records written
    /// before locations were per category: the engine then uses the configured folder).
    pub stage_path: Option<PathBuf>,
    /// The user chose where this torrent lives (a custom save path, "Move to…"): changes of the
    /// app-level or category locations do not move it.
    pub pinned: bool,
    /// Waiting for the user to choose what to download (SPEC 7A.6): not announced, no peers.
    pub awaiting_choice: bool,
    /// Held for the grace period before an auto-add starts (SPEC 7A.7); implies `awaiting_choice`.
    /// After a restart the countdown begins again.
    pub pending_start: bool,
    /// Cleaned display title, computed once by the engine (`None` = not yet: records of older versions).
    pub title: Option<String>,
}

/// Whether a torrent's files have paths of their own.
///
/// New torrents always keep their own layout below the save path (SPEC 7A.3, flat). `Renamed`
/// survives for torrents placed by earlier versions (destination templates), and for one whose
/// move found a name already taken: their recorded paths are kept as they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Placed {
    /// Torrent layout below the save path.
    #[default]
    No,
    /// Files are at the paths of their [`Placement`].
    Renamed,
}

/// Per-file paths of a torrent that does not keep its own layout (SPEC 7A.3). Persisted so the
/// layout is rebuilt identically every time the torrent wakes.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Placement {
    /// Absolute destination per file in torrent order; `None` = the default location.
    pub files: Vec<Option<PathBuf>>,
    /// Folder above which empty directories are never removed (the folder the files were placed in).
    pub library_root: PathBuf,
}

/// State of a direct download as stored.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DirectState {
    /// Should be running (or queued).
    Active,
    /// Paused by the user.
    Paused,
    /// Finished; the file is in place.
    Completed,
    /// Failed; the text is shown to the user.
    Failed(String),
}

/// A direct download record (SPEC 7D). The resumable bytes live in trs-http's `.part` + sidecar.
#[derive(Clone, Debug, PartialEq)]
pub struct DirectRecord {
    /// Id (also the trs-http download id, in hex).
    pub id: TorrentId,
    /// Source URL.
    pub url: String,
    /// Display name (file name once known).
    pub name: String,
    /// Destination folder.
    pub dest_dir: PathBuf,
    /// Requested file name.
    pub file_name: Option<String>,
    /// Parallel connections requested.
    pub connections: u32,
    /// Expected SHA-256.
    pub sha256: Option<[u8; 32]>,
    /// Per-download limit, bytes/s.
    pub rate_limit: Option<u64>,
    /// Desired state.
    pub state: DirectState,
    /// Category chosen by the user, if any.
    pub category: Option<Category>,
    /// Unix seconds added.
    pub added_at: u64,
    /// Unix seconds finished.
    pub finished_at: Option<u64>,
    /// Where the finished file is.
    pub final_path: Option<PathBuf>,
    /// Total size when known.
    pub total: Option<u64>,
    /// Bytes safely on disk at the last write.
    pub done: u64,
    /// Queue position (shared ordering with torrents).
    pub queue_pos: u32,
}

/// Lifetime counters (`counters` table). Monotonic; losing a few seconds on a crash is accepted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LifetimeCounters {
    /// Payload bytes uploaded, ever.
    pub uploaded: u64,
    /// Payload bytes downloaded, ever (hash-failed bytes included).
    pub downloaded: u64,
    /// Seconds spent seeding, ever.
    pub seeded_secs: u64,
    /// Verified bytes at the time of writing.
    pub done_bytes: u64,
    /// Verified bytes of the wanted files, when known (read from the resume data on load).
    pub wanted_done: Option<u64>,
}

/// Resume data (`resume` table), kept for incomplete torrents and for the file fingerprints of
/// complete ones.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ResumeRecord {
    /// Every piece is verified (the bitfield is then empty).
    pub complete: bool,
    /// BEP 3 bitfield bytes of the verified pieces; empty when `complete`.
    pub bitfield: Vec<u8>,
    /// One 20-byte `FileFingerprint` per file (`None` for padding or missing files).
    pub fingerprints: Vec<Option<[u8; 20]>>,
    /// Unix seconds written.
    pub saved_at: u64,
}

/// A statistics delta for one tracker profile (`stats_daily` is aggregated by the store).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatDelta {
    /// Registrable domain of the torrent's primary announce host: the automatic tracker the
    /// counts belong to, favorite or not (SPEC 7E.2). `None` (no tracker) is Public, or Other
    /// private when `private`.
    pub domain: Option<String>,
    /// The torrent is private (selects "Other private" when there is no domain).
    pub private: bool,
    /// Unix seconds of the event (the store buckets it into days).
    pub at: u64,
    /// Payload bytes downloaded.
    pub downloaded: u64,
    /// Payload bytes uploaded.
    pub uploaded: u64,
    /// Wanted files completed.
    pub files_completed: u32,
    /// Bytes of those files.
    pub files_bytes: u64,
    /// Torrents added.
    pub torrents_added: u32,
    /// Torrents completed.
    pub torrents_completed: u32,
    /// Torrents removed.
    pub torrents_removed: u32,
    /// Seconds spent seeding.
    pub seed_secs: u64,
}

/// One atomic write.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    /// Original `.torrent` bytes, written before the records that refer to them.
    pub torrent_files: Vec<(TorrentId, Arc<[u8]>)>,
    /// Insert or replace records.
    pub torrents: Vec<TorrentRecord>,
    /// Lifetime counters.
    pub counters: Vec<(TorrentId, LifetimeCounters)>,
    /// Resume data; `None` deletes it.
    pub resume: Vec<(TorrentId, Option<ResumeRecord>)>,
    /// Remove these torrents with everything attached (record, counters, resume, file, peers,
    /// tracker timings). Statistics are kept.
    pub removed: Vec<TorrentId>,
    /// Peers worth reconnecting to (bounded by the engine to 64 per torrent).
    pub peers: Vec<(TorrentId, Vec<SocketAddr>)>,
    /// Statistics deltas.
    pub stat_deltas: Vec<StatDelta>,
    /// Serialized DHT routing table.
    pub dht_state: Option<Vec<u8>>,
    /// Placement tables; `None` deletes.
    pub placements: Vec<(TorrentId, Option<Placement>)>,
    /// Direct download records to insert or replace.
    pub directs: Vec<DirectRecord>,
    /// Direct downloads to forget.
    pub removed_directs: Vec<TorrentId>,
}

impl Batch {
    /// Nothing to write.
    pub fn is_empty(&self) -> bool {
        self.torrent_files.is_empty()
            && self.torrents.is_empty()
            && self.counters.is_empty()
            && self.resume.is_empty()
            && self.removed.is_empty()
            && self.peers.is_empty()
            && self.stat_deltas.is_empty()
            && self.dht_state.is_none()
            && self.placements.is_empty()
            && self.directs.is_empty()
            && self.removed_directs.is_empty()
    }
}

/// A record with its counters, as loaded at start-up.
#[derive(Clone, Debug)]
pub struct StoredTorrent {
    /// The record.
    pub record: TorrentRecord,
    /// Its lifetime counters.
    pub counters: LifetimeCounters,
}

/// Everything the engine persists. Implementations must be thread-safe; reads may run on any
/// runtime thread and must not block for long.
pub trait Store: Send + Sync + 'static {
    /// All torrents, for start-up (cold start must need no rehash).
    fn load_torrents(&self) -> Result<Vec<StoredTorrent>, StoreError>;
    /// Resume data of one torrent, loaded when it wakes.
    fn load_resume(&self, id: &TorrentId) -> Result<Option<ResumeRecord>, StoreError>;
    /// The original `.torrent` bytes.
    fn load_torrent_file(&self, id: &TorrentId) -> Result<Option<Arc<[u8]>>, StoreError>;
    /// Cached peers of a torrent.
    fn load_peers(&self, id: &TorrentId) -> Result<Vec<SocketAddr>, StoreError>;
    /// Saved DHT routing table.
    fn load_dht_state(&self) -> Result<Option<Vec<u8>>, StoreError>;
    /// Apply a batch atomically.
    fn commit(&self, batch: Batch) -> Result<(), StoreError>;
    /// The durable timing store for `trs-tracker`. Every `save` must be durable when it returns.
    fn timing_store(&self) -> Box<dyn TimingStore + Send>;
    /// The per-file paths of a placed torrent, loaded when it wakes.
    fn load_placement(&self, _id: &TorrentId) -> Result<Option<Placement>, StoreError> {
        Ok(None)
    }
    /// Direct download records, for start-up.
    fn load_directs(&self) -> Result<Vec<DirectRecord>, StoreError> {
        Ok(Vec::new())
    }
    /// Folder for the journals of moves in progress (SPEC 7A.5); `None` = the engine uses a
    /// temporary folder (nothing survives a restart then, which is fine for an in-memory store).
    fn moves_dir(&self) -> Option<PathBuf> {
        None
    }
    /// Record this launch (Unix ms) and return the launch times that count for the crash-loop
    /// breaker (3 launches in 10 minutes hold all announces): this one and every recent launch
    /// whose run did not end with [`Store::record_clean_exit`]. The default keeps no history,
    /// which disables the breaker.
    fn record_launch(&self, now_ms: u64) -> Result<Vec<u64>, StoreError> {
        Ok(vec![now_ms])
    }
    /// The run that called `record_launch` ended in an orderly shutdown: a relaunch is not a crash
    /// (`trs_tracker::crash::LaunchLog`).
    fn record_clean_exit(&self) -> Result<(), StoreError> {
        Ok(())
    }
}

#[derive(Default)]
struct MemInner {
    torrents: HashMap<TorrentId, StoredTorrent>,
    resume: HashMap<TorrentId, ResumeRecord>,
    files: HashMap<TorrentId, Arc<[u8]>>,
    peers: HashMap<TorrentId, Vec<SocketAddr>>,
    stats: Vec<StatDelta>,
    dht: Option<Vec<u8>>,
    commits: u64,
    fail_commits: bool,
    launches: trs_tracker::crash::LaunchLog,
    placements: HashMap<TorrentId, Placement>,
    directs: HashMap<TorrentId, DirectRecord>,
}

/// In-memory [`Store`] for tests. Clones share the same data, so a test can "restart" the engine
/// on the same store.
#[derive(Clone, Default)]
pub struct MemStore {
    inner: Arc<Mutex<MemInner>>,
    timing: Arc<Mutex<HashMap<TimingKey, TimingRecord>>>,
}

impl MemStore {
    /// Empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn g(&self) -> std::sync::MutexGuard<'_, MemInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Number of stored torrents.
    pub fn torrent_count(&self) -> usize {
        self.g().torrents.len()
    }

    /// A stored torrent, if present.
    pub fn torrent(&self, id: &TorrentId) -> Option<StoredTorrent> {
        self.g().torrents.get(id).cloned()
    }

    /// Stored resume data.
    pub fn resume(&self, id: &TorrentId) -> Option<ResumeRecord> {
        self.g().resume.get(id).cloned()
    }

    /// All statistics deltas written so far.
    pub fn stat_deltas(&self) -> Vec<StatDelta> {
        self.g().stats.clone()
    }

    /// Successful commits so far.
    pub fn commits(&self) -> u64 {
        self.g().commits
    }

    /// Make every commit fail (fault injection).
    pub fn set_fail_commits(&self, fail: bool) {
        self.g().fail_commits = fail;
    }

    /// Insert a record directly, as if a previous run had written it.
    pub fn put(&self, record: TorrentRecord, counters: LifetimeCounters, resume: Option<ResumeRecord>, file: Option<Arc<[u8]>>) {
        let mut g = self.g();
        let id = record.id;
        g.torrents.insert(id, StoredTorrent { record, counters });
        if let Some(r) = resume {
            g.resume.insert(id, r);
        }
        if let Some(f) = file {
            g.files.insert(id, f);
        }
    }

    /// Stored placement, if any.
    pub fn placement(&self, id: &TorrentId) -> Option<Placement> {
        self.g().placements.get(id).cloned()
    }

    /// Stored direct download records.
    pub fn directs(&self) -> Vec<DirectRecord> {
        self.g().directs.values().cloned().collect()
    }

    /// Timing record of a tracker, if any.
    pub fn timing(&self, key: &TimingKey) -> Option<TimingRecord> {
        self.timing.lock().unwrap_or_else(|p| p.into_inner()).get(key).cloned()
    }
}

struct MemTiming(Arc<Mutex<HashMap<TimingKey, TimingRecord>>>);

impl TimingStore for MemTiming {
    fn load(&self, key: &TimingKey) -> Option<TimingRecord> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).get(key).cloned()
    }

    fn save(&mut self, key: &TimingKey, record: &TimingRecord) -> Result<(), TimingError> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(key.clone(), record.clone());
        Ok(())
    }
}

impl Store for MemStore {
    fn load_torrents(&self) -> Result<Vec<StoredTorrent>, StoreError> {
        Ok(self.g().torrents.values().cloned().collect())
    }

    fn load_resume(&self, id: &TorrentId) -> Result<Option<ResumeRecord>, StoreError> {
        Ok(self.g().resume.get(id).cloned())
    }

    fn load_torrent_file(&self, id: &TorrentId) -> Result<Option<Arc<[u8]>>, StoreError> {
        Ok(self.g().files.get(id).cloned())
    }

    fn load_peers(&self, id: &TorrentId) -> Result<Vec<SocketAddr>, StoreError> {
        Ok(self.g().peers.get(id).cloned().unwrap_or_default())
    }

    fn load_dht_state(&self) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.g().dht.clone())
    }

    fn load_placement(&self, id: &TorrentId) -> Result<Option<Placement>, StoreError> {
        Ok(self.g().placements.get(id).cloned())
    }

    fn load_directs(&self) -> Result<Vec<DirectRecord>, StoreError> {
        Ok(self.g().directs.values().cloned().collect())
    }

    fn commit(&self, batch: Batch) -> Result<(), StoreError> {
        let mut g = self.g();
        if g.fail_commits {
            return Err(StoreError("injected commit failure".into()));
        }
        // Removals first (see `RedbStore::commit`).
        for id in batch.removed_directs {
            g.directs.remove(&id);
        }
        for id in batch.removed {
            g.torrents.remove(&id);
            g.resume.remove(&id);
            g.files.remove(&id);
            g.peers.remove(&id);
            g.placements.remove(&id);
            self.timing
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|k, _| k.info_hash != id);
        }
        for (id, bytes) in batch.torrent_files {
            g.files.insert(id, bytes);
        }
        for mut rec in batch.torrents {
            let id = rec.id;
            let (counters, old_announce) = match g.torrents.get(&id) {
                Some(t) => (t.counters, t.record.announce.clone()),
                None => (LifetimeCounters::default(), None),
            };
            if rec.announce.is_none() {
                rec.announce = old_announce;
            }
            g.torrents.insert(id, StoredTorrent { record: rec, counters });
        }
        for (id, c) in batch.counters {
            if let Some(t) = g.torrents.get_mut(&id) {
                t.counters = c;
            }
        }
        for (id, r) in batch.resume {
            match r {
                Some(r) => {
                    g.resume.insert(id, r);
                }
                None => {
                    g.resume.remove(&id);
                }
            }
        }
        for (id, p) in batch.peers {
            g.peers.insert(id, p);
        }
        for (id, p) in batch.placements {
            match p {
                Some(p) => {
                    g.placements.insert(id, p);
                }
                None => {
                    g.placements.remove(&id);
                }
            }
        }
        for d in batch.directs {
            g.directs.insert(d.id, d);
        }
        g.stats.extend(batch.stat_deltas);
        if batch.dht_state.is_some() {
            g.dht = batch.dht_state;
        }
        g.commits += 1;
        Ok(())
    }

    fn timing_store(&self) -> Box<dyn TimingStore + Send> {
        Box::new(MemTiming(self.timing.clone()))
    }

    fn record_launch(&self, now_ms: u64) -> Result<Vec<u64>, StoreError> {
        let counted = self.g().launches.record_launch(trs_tracker::Millis(now_ms));
        Ok(counted.into_iter().map(|t| t.0).collect())
    }

    fn record_clean_exit(&self) -> Result<(), StoreError> {
        self.g().launches.mark_clean_exit();
        Ok(())
    }
}

/// Dedicated writer thread: the runtime hands it batches and never waits for a commit.
pub(crate) struct Writer {
    tx: Option<std::sync::mpsc::Sender<WriterMsg>>,
    join: Option<std::thread::JoinHandle<()>>,
}

enum WriterMsg {
    Batch(Box<Batch>),
    Flush(std::sync::mpsc::Sender<()>),
}

impl Writer {
    pub(crate) fn start(store: Arc<dyn Store>) -> Self {
        let (tx, rx) = std::sync::mpsc::channel::<WriterMsg>();
        let join = std::thread::Builder::new()
            .name("trs-persist".into())
            .spawn(move || {
                // A failed commit must not kill the thread; the engine re-sends state it still
                // considers dirty with its next batch.
                while let Ok(msg) = rx.recv() {
                    match msg {
                        WriterMsg::Batch(b) => {
                            let _ = store.commit(*b);
                        }
                        WriterMsg::Flush(done) => {
                            let _ = done.send(());
                        }
                    }
                }
            })
            .ok();
        Self { tx: Some(tx), join }
    }

    pub(crate) fn submit(&self, b: Batch) {
        if b.is_empty() {
            return;
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(WriterMsg::Batch(Box::new(b)));
        }
    }

    /// Wait until everything submitted so far was committed.
    pub(crate) fn flush(&self) {
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(t) = &self.tx
            && t.send(WriterMsg::Flush(tx)).is_ok() {
                let _ = rx.recv();
            }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // Closing the sender ends the thread after it drained the queue.
        self.tx = None;
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: u8) -> TorrentRecord {
        TorrentRecord {
            id: [id; 20],
            name: format!("t{id}"),
            has_metadata: true,
            magnet: None,
            announce: Some(vec![vec!["http://t.example/announce".into()]]),
            total_length: 100,
            wanted_length: 100,
            session_base: 0,
            piece_length: 16384,
            private: false,
            save_path: "/tmp".into(),
            state: StoredState::Active,
            file_priorities: vec![],
            priority: 1,
            priority_user: false,
            queue_pos: 0,
            category: None,
            category_user: false,
            tracker_profile: None,
            rule: None,
            added_at: 1,
            finished_at: None,
            seed_goal: SeedGoal::FOREVER,
            goal_user: false,
            added_complete: false,
            staged: false,
            complete: false,
            placed: Placed::No,
            last_activity: 0,
            error_since: 0,
            error_tracker: false,
            stage_path: None,
            pinned: false,
            awaiting_choice: false,
            pending_start: false,
            title: None,
            idle: None,
        }
    }

    #[test]
    fn commit_applies_all_parts_atomically_and_remove_cleans_up() {
        let s = MemStore::new();
        let id = [1u8; 20];
        s.commit(Batch {
            torrent_files: vec![(id, Arc::from(&b"file"[..]))],
            torrents: vec![rec(1)],
            counters: vec![(id, LifetimeCounters { uploaded: 5, ..Default::default() })],
            resume: vec![(id, Some(ResumeRecord { complete: true, ..Default::default() }))],
            peers: vec![(id, vec!["1.2.3.4:5".parse().unwrap()])],
            stat_deltas: vec![StatDelta { downloaded: 9, ..Default::default() }],
            ..Batch::default()
        })
        .unwrap();
        assert_eq!(s.torrent_count(), 1);
        assert_eq!(s.torrent(&id).unwrap().counters.uploaded, 5);
        assert!(s.load_resume(&id).unwrap().unwrap().complete);
        assert_eq!(&*s.load_torrent_file(&id).unwrap().unwrap(), b"file");
        assert_eq!(s.load_peers(&id).unwrap().len(), 1);
        s.commit(Batch { removed: vec![id], ..Batch::default() }).unwrap();
        assert_eq!(s.torrent_count(), 0);
        assert!(s.load_resume(&id).unwrap().is_none());
        assert_eq!(s.stat_deltas().len(), 1, "history survives removal");
    }

    #[test]
    fn failed_commit_changes_nothing() {
        let s = MemStore::new();
        s.set_fail_commits(true);
        assert!(s.commit(Batch { torrents: vec![rec(1)], ..Batch::default() }).is_err());
        assert_eq!(s.torrent_count(), 0);
    }

    #[test]
    fn timing_store_is_shared_between_handles() {
        let s = MemStore::new();
        let key = TimingKey { info_hash: [1; 20], tracker_url: "http://x/a".into() };
        let mut a = s.timing_store();
        a.save(&key, &TimingRecord { completed_sent: true, ..Default::default() }).unwrap();
        assert!(s.timing_store().load(&key).unwrap().completed_sent);
    }

    #[test]
    fn writer_commits_in_order_and_flushes() {
        let s = MemStore::new();
        let w = Writer::start(Arc::new(s.clone()));
        w.submit(Batch { torrents: vec![rec(1)], ..Batch::default() });
        w.submit(Batch { torrents: vec![rec(2)], ..Batch::default() });
        w.flush();
        assert_eq!(s.torrent_count(), 2);
        assert_eq!(s.commits(), 2);
    }
}
