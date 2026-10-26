//! The real backend: `trs_engine::Host` in-process on its own runtime, the control server for
//! agents, and the mapping of engine rows and events onto the FFI records.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use trs_control::{AppServerConfig, PermissionSource, ServerHandle, ServerOptions};
use trs_engine::api::{
    self as eng, AddOptions, EngineEvent, RowKind, SeedGoal, TorrentRow, TorrentState,
};
use trs_engine::{EngineHandle, Host, HostOptions, StateStore};
use trs_state::rules::RuleSet;
use trs_state::stats::{Day, StatsScope};
use trs_state::trackers::TrackerProfiles;

use crate::backend::{AttrRow, Backend, ShutdownInfo};
use crate::rows::{self, Totals};
use crate::settings::{SettingsStore, category_from, category_to_ffi};
use crate::types::*;

const RECENT_SHOWN: usize = 10;
/// A finished torrent stays in Recently Finished (window and menu bar) this long, or until Clear (user, 2026-10-14).
const RECENT_SECS: u64 = 15 * 3600;
/// Rows are re-read from the engine at most this often, however often the UI asks.
const CACHE_AGE: Duration = Duration::from_millis(400);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn hex(id: &[u8; 20]) -> String {
    trs_engine::util::hex(id)
}

fn parse_id(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

fn ids(v: &[String]) -> Vec<[u8; 20]> {
    v.iter().filter_map(|s| parse_id(s)).collect()
}

/// Plain sentences for the reasons a stream cannot start (shown as a calm note, not an alert).
fn stream_err(e: &eng::EngineError, name: &str) -> CoreError {
    let message = match e {
        eng::EngineError::NoMetadata => "This download is still fetching its details. Try again in a moment.".to_owned(),
        eng::EngineError::NotFound => "This download is no longer in the list.".to_owned(),
        eng::EngineError::ShuttingDown => "Seedwell is quitting.".to_owned(),
        eng::EngineError::Invalid(m) => format!("Couldn't play “{name}”: {m}"),
        other => format!("Couldn't play “{name}”: {other}"),
    };
    CoreError::Failed { message }
}

fn file_priority(p: eng::FilePriority) -> FilePriority {
    match p {
        eng::FilePriority::Skip => FilePriority::Skip,
        eng::FilePriority::Low => FilePriority::Low,
        eng::FilePriority::Normal => FilePriority::Normal,
        eng::FilePriority::High => FilePriority::High,
    }
}

fn engine_priority(p: FilePriority) -> eng::FilePriority {
    match p {
        FilePriority::Skip => eng::FilePriority::Skip,
        FilePriority::Low => eng::FilePriority::Low,
        FilePriority::Normal => eng::FilePriority::Normal,
        FilePriority::High => eng::FilePriority::High,
    }
}

fn engine_err(e: &eng::EngineError) -> CoreError {
    CoreError::InvalidSource { message: e.to_string() }
}

/// Extensions that get a document icon; everything else is a folder.
const FILE_EXTS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "m4v", "wmv", "webm", "ts", "iso", "dmg", "pkg", "zip", "rar", "7z", "gz",
    "xz", "tar", "bz2", "exe", "msi", "img", "mp3", "flac", "m4a", "m4b", "aac", "ogg", "wav", "opus", "epub",
    "pdf", "mobi", "azw3", "cbz", "cbr", "txt", "bin", "appimage", "deb", "rpm", "jpg", "png", "nfo",
];

fn file_ext(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .filter(|e| FILE_EXTS.contains(&e.as_str()))
        .unwrap_or_default()
}

/// Rules and tracker profiles in force, cached; reloaded when a document changed.
#[derive(Clone, Default)]
struct Policy {
    rules: RuleSet,
    profiles: TrackerProfiles,
}

impl Policy {
    fn profile_name(&self, id: &str) -> Option<&str> {
        self.profiles.get(id).map(|p| p.name.as_str())
    }
}

/// A torrent's automatically identified tracker and its primary host (for "Save as Favorite").
#[derive(Clone, Debug)]
struct GroupFact {
    group: String,
    host: String,
}

struct Loaded {
    at: Instant,
    rows: Vec<TransferRow>,
    groups: Vec<GroupFact>,
    attrs: Vec<AttrRow>,
    recent: Vec<TransferRow>,
    totals: Totals,
}

struct Shutdown {
    started: AtomicBool,
    result: Mutex<Option<ShutdownInfo>>,
    cv: Condvar,
    deadline: Mutex<Option<Instant>>,
}

/// What the engine said about a torrent's files, for the rows: asked once per (wanted size, finished).
#[derive(Clone)]
struct RowFiles {
    has_video: bool,
    count: u32,
    video_ext: String,
    done: u32,
    key: (u64, bool),
    at: Instant,
}

pub struct RealBackend {
    rt: tokio::runtime::Runtime,
    host: Mutex<Option<Host>>,
    engine: EngineHandle,
    store: Arc<StateStore>,
    dir: PathBuf,
    settings: SettingsStore,
    policy: Arc<Mutex<Policy>>,
    cache: Mutex<Option<Arc<Loaded>>>,
    listener: Arc<Mutex<Option<Arc<dyn CoreListener>>>>,
    server: Mutex<Option<ServerHandle>>,
    socket: Option<String>,
    cleared_at: Mutex<u64>,
    sd: Arc<Shutdown>,
    /// Whether a torrent holds a video file, by id: asked of the engine once and remembered.
    videos: Mutex<std::collections::HashMap<String, RowFiles>>,
    /// Folder sync (SPEC 7G), started next to the engine.
    sync: Arc<crate::sync::RealSync>,
}

pub struct StartOptions {
    pub dir: PathBuf,
    pub socket: PathBuf,
    /// QA only: trackers on loopback are allowed (a local mock tracker).
    pub allow_loopback_trackers: bool,
    /// Refuse to start when any folder of the state lies inside this path.
    pub forbidden_root: Option<PathBuf>,
    /// The Keychain for folder sync's keys (`None`: a 0600 file in the state folder).
    pub secret_vault: Option<Arc<dyn crate::sync::SecretVault>>,
}

/// Every folder `trackers.toml` makes the engine touch, `~` expanded.
fn profile_folders(p: &TrackerProfiles) -> Vec<PathBuf> {
    p.profiles.iter().flat_map(|p| p.folders()).map(crate::settings::expand_home).collect()
}

/// Every folder `config.toml` makes the engine touch, `~` expanded.
fn config_folders(c: &trs_state::Config) -> Vec<PathBuf> {
    use crate::settings::expand_home;
    let f = &c.folders;
    let mut v = vec![expand_home(&f.download_dir), expand_home(&f.incomplete_dir)];
    v.extend(f.archive_dir.iter().map(|d| expand_home(d)));
    for w in &f.watch {
        v.push(expand_home(&w.path));
        v.extend(w.archive_dir.iter().map(|d| expand_home(d)));
    }
    for cat in trs_classify::Category::ALL {
        let o = f.categories.get(cat);
        v.extend(o.incomplete_dir.iter().map(|d| expand_home(d)));
        v.extend(o.finished_dir.iter().map(|d| expand_home(d)));
    }
    v
}

impl RealBackend {
    pub fn start(o: StartOptions) -> Result<Arc<dyn Backend>, CoreError> {
        let failed = |m: String| CoreError::Failed { message: m };
        if let Some(forbidden) = &o.forbidden_root {
            let store = SettingsStore::new(&o.dir);
            let cfg = store.config();
            let inside = |p: &Path| !p.as_os_str().is_empty() && p.starts_with(forbidden);
            if inside(&o.dir) || config_folders(&cfg).iter().any(|p| inside(p)) || profile_folders(&store.load_profiles_set()).iter().any(|p| inside(p)) {
                return Err(failed(format!(
                    "Refusing to start: a folder of this isolated run lies inside {}",
                    forbidden.display()
                )));
            }
        }
        let rt = trs_engine::build_runtime().map_err(|e| failed(e.to_string()))?;
        let allow_loopback = o.allow_loopback_trackers;
        let allow_lan = SettingsStore::new(&o.dir).config().network.allow_lan_trackers;
        let opts = HostOptions {
            tweak: Some(Box::new(move |ec| {
                ec.tracker_destinations.allow_lan_trackers = allow_lan;
                if allow_loopback {
                    ec.tracker_destinations = trs_tracker_policy_local();
                    ec.dht_allow_local = true;
                }
            })),
            time_zone: Some(Arc::new(trs_engine::util::local_utc_offset)),
            ..HostOptions::default()
        };
        let host = rt
            .block_on(Host::start(&o.dir, opts))
            .map_err(|e| failed(format!("The engine could not start: {e}")))?;
        let engine = host.handle();
        let store = host.store().clone();
        let settings = SettingsStore::new(&o.dir);
        let cfg = settings.config();

        // Folder sync starts in the background; the control server serves it once it is up.
        let sync = crate::sync::RealSync::start(rt.handle(), &engine, &o.dir, o.secret_vault.clone());
        // The control server for agents (SPEC 9A) lives on the engine's runtime.
        let server = trs_control::start_for_app_with_sync(
            rt.handle(),
            engine.clone(),
            AppServerConfig {
                socket_path: Some(o.socket.clone()),
                state_dir: o.dir.clone(),
                store: Some(store.clone()),
                permission: PermissionSource::from_config_dir(&o.dir),
                options: ServerOptions::default().with_config(&cfg),
                tz_offset: Some(Arc::new(|| trs_engine::util::local_utc_offset(trs_engine::util::unix_secs() as i64))),
            },
            sync.slot(),
        );
        let (server, socket) = match server {
            Ok(s) => (Some(s), Some(o.socket.display().to_string())),
            // A socket problem must not stop the app: agents are optional.
            Err(e) => {
                eprintln!("seedwell: control socket unavailable: {e}");
                (None, None)
            }
        };

        let policy = Arc::new(Mutex::new(Policy {
            rules: settings.load_rules_set(),
            profiles: settings.load_profiles_set(),
        }));
        let listener: Arc<Mutex<Option<Arc<dyn CoreListener>>>> = Arc::new(Mutex::new(None));
        if let Some(s) = &server {
            let l = listener.clone();
            s.activity().set_sink(move |e| {
                if let Some(l) = lock(&l).clone() {
                    l.on_agent_activity(AgentActivity {
                        at_ms: e.at,
                        client: e.client.clone(),
                        method: e.method.clone(),
                        summary: e.summary.clone(),
                        outcome: e.outcome.clone(),
                    });
                }
            });
        }
        let cleared = std::fs::read_to_string(o.dir.join("recent-cleared-at"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);

        let me = Arc::new(Self {
            rt,
            host: Mutex::new(Some(host)),
            engine,
            store,
            dir: o.dir,
            settings,
            policy,
            cache: Mutex::new(None),
            listener,
            server: Mutex::new(server),
            socket,
            cleared_at: Mutex::new(cleared),
            videos: Mutex::new(std::collections::HashMap::new()),
            sync,
            sd: Arc::new(Shutdown {
                started: AtomicBool::new(false),
                result: Mutex::new(None),
                cv: Condvar::new(),
                deadline: Mutex::new(None),
            }),
        });
        me.spawn_event_pump();
        Ok(me)
    }

    // ------------------------------------------------------------ events

    fn spawn_event_pump(self: &Arc<Self>) {
        let mut rx = self.engine.subscribe();
        let engine = self.engine.clone();
        let listener = self.listener.clone();
        let policy = self.policy.clone();
        let settings_dir = self.dir.clone();
        let me = Arc::downgrade(self);
        self.rt.spawn(async move {
            // "Something changed" notifications are coalesced to one per second.
            let mut pending: Option<tokio::time::Instant> = None;
            let mut last_sent = tokio::time::Instant::now() - Duration::from_secs(5);
            loop {
                let wait = async {
                    match pending {
                        Some(t) => tokio::time::sleep_until(t).await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    ev = rx.recv() => {
                        let ev = match ev {
                            Ok(e) => e,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                pending.get_or_insert(tokio::time::Instant::now());
                                continue;
                            }
                            Err(_) => break,
                        };
                        let l = lock(&listener).clone();
                        match ev {
                            EngineEvent::Finished { id } => {
                                if let (Some(l), Ok(d)) = (&l, engine.torrent_detail(id).await) {
                                    // Adding a complete torrent finishes nothing: no notification.
                                    if d.row.kind == RowKind::Direct || d.row.downloaded_total > 0 {
                                        let pol = lock(&policy).clone();
                                        l.on_transfer_finished(convert(&d.row, &pol, now_secs()));
                                    }
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::SeedingGoalReached { id } => {
                                if let (Some(l), Ok(d)) = (&l, engine.torrent_detail(id).await) {
                                    let pol = lock(&policy).clone();
                                    l.on_goal_reached(convert(&d.row, &pol, now_secs()));
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::Error { id, message } => {
                                if let Some(l) = &l {
                                    l.on_error(Some(hex(&id)), message);
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::AutoAdded { id, name, title, category, tracker_profile, destination, .. } => {
                                if let Some(l) = &l {
                                    let tracker = {
                                        let pol = lock(&policy);
                                        tracker_profile
                                            .as_deref()
                                            .and_then(|p| pol.profile_name(p).map(str::to_owned))
                                            .or(tracker_profile)
                                            .unwrap_or_else(|| "Public".into())
                                    };
                                    l.on_auto_added(AutoAdded {
                                        id: hex(&id),
                                        name: if title.is_empty() { trs_classify::clean_name(&name) } else { title },
                                        category: category_to_ffi(category),
                                        tracker,
                                        destination: destination.map(|d| d.display().to_string()),
                                    });
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::ChoiceNeeded { id, .. } => {
                                if let (Some(l), Ok(d)) = (&l, engine.torrent_detail(id).await) {
                                    let pol = lock(&policy).clone();
                                    l.on_needs_choice(convert(&d.row, &pol, now_secs()));
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::LocationsChanged { torrents, bytes } => {
                                if let Some(l) = &l {
                                    l.on_locations_changed(torrents, bytes);
                                }
                            }
                            EngineEvent::MoveFailed { id, reason } => {
                                if let Some(l) = &l {
                                    l.on_error(Some(hex(&id)), reason);
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::TrackerHealthChanged { .. } | EngineEvent::MoveStarted { .. } | EngineEvent::MoveFinished { .. } => {
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::Pending { .. } => {
                                if let Some(l) = &l {
                                    l.on_pending_changed();
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::ChoiceResolved { .. } | EngineEvent::FilesSelected { .. } => {
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::Duplicate { name, trackers_added, .. } => {
                                if let Some(l) = &l {
                                    l.on_duplicate(name, trackers_added as u32);
                                }
                            }
                            EngineEvent::AddRequest(req) => {
                                if let Some(l) = &l {
                                    l.on_add_request(AddRequestInfo {
                                        path: req.path.display().to_string(),
                                        name: req.preview.name.clone(),
                                        total_bytes: req.preview.total_bytes,
                                        category: category_to_ffi(req.preview.category),
                                        tracker: req.preview.tracker_host.clone(),
                                    });
                                }
                            }
                            EngineEvent::ConfigReloaded { file, warnings, restart_required } => {
                                reload_policy(&policy, &settings_dir);
                                if let Some(l) = &l {
                                    l.on_settings_changed(file, warnings, restart_required);
                                }
                                pending.get_or_insert(tokio::time::Instant::now());
                            }
                            EngineEvent::ConfigRejected { file, error } => {
                                if let Some(l) = &l {
                                    l.on_settings_rejected(file, error);
                                }
                            }
                            // Resting and waking seeds change nothing the menu bar icon shows.
                            EngineEvent::StateChanged { state: TorrentState::Seeding | TorrentState::LowPower | TorrentState::Completed, .. } => {}
                            EngineEvent::Added { .. }
                            | EngineEvent::Removed { .. }
                            | EngineEvent::StateChanged { .. }
                            | EngineEvent::MetadataReceived { .. } => {
                                pending.get_or_insert(tokio::time::Instant::now() + Duration::from_millis(300));
                            }
                            _ => {}
                        }
                    }
                    () = wait => {
                        pending = None;
                        if last_sent.elapsed() < Duration::from_secs(1) {
                            pending = Some(last_sent + Duration::from_secs(1));
                            continue;
                        }
                        last_sent = tokio::time::Instant::now();
                        if let Some(me) = me.upgrade() {
                            *lock(&me.cache) = None;
                        }
                        if let Some(l) = lock(&listener).clone() {
                            l.on_attention_changed();
                        }
                    }
                }
            }
        });
    }

    // ------------------------------------------------------------ snapshots

    fn loaded(&self, max_age: Duration) -> Arc<Loaded> {
        if let Some(c) = lock(&self.cache).as_ref()
            && c.at.elapsed() < max_age
        {
            return c.clone();
        }
        let snap = self.rt.block_on(self.engine.snapshot());
        // A profile the engine just made for a private torrent reaches us through the file
        // watcher a moment later; do not show its bare id meanwhile.
        if let Ok(s) = &snap {
            let unknown = {
                let p = lock(&self.policy);
                s.rows.iter().any(|r| r.tracker_profile.as_deref().is_some_and(|id| p.profiles.get(id).is_none()))
            };
            if unknown {
                reload_policy(&self.policy, &self.dir);
            }
        }
        let pol = lock(&self.policy).clone();
        let now = now_secs();
        let loaded = match snap {
            Ok(s) => {
                let mut rows: Vec<TransferRow> = s.rows.iter().map(|r| convert(r, &pol, now)).collect();
                self.mark_videos(&mut rows);
                let groups = s
                    .rows
                    .iter()
                    .filter(|r| r.kind == RowKind::Torrent && r.tracker_profile.is_none())
                    .filter_map(|r| {
                        let g = trs_state::trackers::group_of_host(&r.tracker_host)?;
                        Some(GroupFact { group: g.key, host: r.tracker_host.clone() })
                    })
                    .collect();
                let attrs = s
                    .rows
                    .iter()
                    .filter(|r| r.kind == RowKind::Torrent)
                    .map(|r| AttrRow {
                        id: hex(&r.id),
                        profile: r.tracker_profile.clone(),
                        private: r.private,
                        category: category_to_ffi(r.category.unwrap_or(trs_classify::Category::Other)),
                    })
                    .collect();
                let recent = self.recent_of(&s.rows, &rows, now);
                Loaded {
                    at: Instant::now(),
                    rows,
                    groups,
                    attrs,
                    recent,
                    totals: Totals {
                        down_rate: s.down_rate,
                        up_rate: s.up_rate,
                        alt_speed: s.alt_speed,
                        down_limit: s.down_limit,
                        up_limit: s.up_limit,
                        listen_port: s.listen_port,
                        connections: s.connections,
                        online: s.online,
                        offline_since_ms: s.offline_since_ms,
                    },
                }
            }
            Err(_) => Loaded {
                at: Instant::now(),
                rows: Vec::new(),
                groups: Vec::new(),
                attrs: Vec::new(),
                recent: Vec::new(),
                totals: Totals { down_rate: 0, up_rate: 0, alt_speed: false, down_limit: None, up_limit: None, listen_port: 0, connections: 0, online: true, offline_since_ms: None },
            },
        };
        let loaded = Arc::new(loaded);
        *lock(&self.cache) = Some(loaded.clone());
        loaded
    }

    /// Finished downloads of the last days that the user has not cleared. A torrent that was added
    /// complete (nothing downloaded, only verified) did not "finish" anything.
    /// Sets `has_video` on every row. A single file answers from its extension; a folder is looked up
    /// in the engine once (a few per refresh, so a big library fills in over a few seconds).
    fn mark_videos(&self, rows: &mut [TransferRow]) {
        let mut budget = 6;
        for row in rows.iter_mut().filter(|r| r.kind == TransferKind::Torrent && !r.awaiting_choice) {
            // A dotted folder name ("Movie.2010.1080p") looks like a file name: only a video extension is final.
            if crate::play::is_video(&row.name) {
                row.has_video = true;
                row.file_count = 1;
                row.files_done = u32::from(row.done_bytes >= row.size_bytes);
                continue;
            }
            if row.size_bytes == 0 {
                continue;
            }
            // Known for this wanted size and finished-ness (ticking files, or finishing, asks the engine again).
            if let Some(v) = lock(&self.videos).get(&row.id).filter(|v| v.key == (row.size_bytes, row.done_bytes >= row.size_bytes) && (v.done == v.count || v.at.elapsed() < Duration::from_secs(30))) {
                (row.has_video, row.file_count, row.video_ext, row.files_done) = (v.has_video, v.count, v.video_ext.clone(), v.done);
                continue;
            }
            if budget == 0 {
                continue;
            }
            budget -= 1;
            let found = parse_id(&row.id)
                .and_then(|tid| self.rt.block_on(self.engine.torrent_detail(tid)).ok())
                .map(|d| {
                    let is_vid = |f: &&eng::FileInfo| {
                        !f.padding && crate::play::is_video(f.path.rsplit('/').next().unwrap_or(&f.path))
                    };
                    let has = d.files.iter().any(|f| is_vid(&f));
                    // The row's icon is the video's only when the biggest file is a video (a photo archive
                    // with a reel stays a folder); the largest video names it (a pack of mkv shows mkv).
                    let biggest = d.files.iter().filter(|f| !f.padding).max_by_key(|f| f.length);
                    let main = biggest.filter(|f| is_vid(f));
                    let ext = main.and_then(|f| f.path.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
                    let count = d.files.iter().filter(|f| !f.padding).count() as u32;
                    let done = files_done(&d.files, row.done_bytes >= row.size_bytes);
                    RowFiles { has_video: has, count, video_ext: ext, done, key: (row.size_bytes, row.done_bytes >= row.size_bytes), at: Instant::now() }
                });
            if let Some(v) = found {
                lock(&self.videos).insert(row.id.clone(), v.clone());
                (row.has_video, row.file_count, row.video_ext, row.files_done) = (v.has_video, v.count, v.video_ext, v.done);
            }
        }
    }

    /// The favorites in hidden mode, as the stored profiles say now.
    fn hidden(&self) -> rows::Hidden {
        rows::Hidden::of(&favorites_of(&lock(&self.policy).profiles))
    }

    fn recent_of(&self, engine_rows: &[TorrentRow], rows: &[TransferRow], now: u64) -> Vec<TransferRow> {
        let cleared = *lock(&self.cleared_at);
        let since = cleared.max(now.saturating_sub(RECENT_SECS));
        // Hidden trackers' finished torrents stay out of the shown count (`rows::snapshot` splits
        // them off for the tracker's own tab anyway).
        let hidden = self.hidden();
        let mut v: Vec<&TransferRow> = rows
            .iter()
            .zip(engine_rows)
            .filter(|(r, e)| r.finished_at.is_some_and(|t| t > since) && !hidden.sharing(r) && (e.kind == RowKind::Direct || e.downloaded_total > 0))
            .map(|(r, _)| r)
            .collect();
        v.sort_by_key(|r| std::cmp::Reverse(r.finished_at));
        v.into_iter().take(RECENT_SHOWN).cloned().collect()
    }

    /// The statistics scopes whose sum is this sidebar tracker's history (SPEC 7E.2).
    fn scopes(&self, tracker_id: &str) -> Vec<StatsScope> {
        let pol = lock(&self.policy);
        trs_state::stats::scopes_for(&pol.profiles, tracker_id)
    }

    fn row_by_id(&self, id: &str) -> Option<TransferRow> {
        self.loaded(Duration::from_secs(5)).rows.iter().find(|r| r.id == id).cloned()
    }
}

fn favorites_of(profiles: &TrackerProfiles) -> Vec<rows::Favorite> {
    profiles
        .profiles
        .iter()
        .map(|p| rows::Favorite { id: p.id.clone(), name: p.name.clone(), hidden: p.hidden, hide_only_seeding: p.hide_only_seeding })
        .collect()
}

fn reload_policy(policy: &Arc<Mutex<Policy>>, dir: &Path) {
    let s = SettingsStore::new(dir);
    *lock(policy) = Policy { rules: s.load_rules_set(), profiles: s.load_profiles_set() };
}

fn trs_tracker_policy_local() -> trs_tracker::policy::DestinationPolicy {
    trs_tracker::policy::DestinationPolicy::LOCAL
}

/// Engine row to FFI row.
fn convert(r: &TorrentRow, pol: &Policy, now: u64) -> TransferRow {
    let direct = r.kind == RowKind::Direct;
    let size = if r.wanted_bytes > 0 { r.wanted_bytes } else { r.total_bytes };
    // What the wanted files hold (skipped files do not count): the base a reopened download
    // started from plus this session's progress. Torrents with metadata count wanted bytes only.
    let base = size.saturating_sub(r.session_size.min(size));
    let done = if r.session_size > 0 || size == 0 { (base + r.session_done).min(size) } else { r.done_bytes.min(size) };
    let complete = size > 0 && done >= size;
    let (tracker_id, tracker_name) = if direct {
        ("direct".to_string(), host_of(r.url.as_deref()))
    } else {
        match &r.tracker_profile {
            Some(p) => (p.clone(), pol.profile_name(p).unwrap_or(p).to_string()),
            // Automatically identified: the registrable domain of the primary announce host.
            None => match trs_state::trackers::group_of_host(&r.tracker_host) {
                Some(g) => (g.key, g.name),
                None if r.private => ("other-private".to_string(), "Private tracker".to_string()),
                None => ("public".to_string(), "Public".to_string()),
            },
        }
    };
    let mut state = match r.state {
        TorrentState::Checking => TransferState::Checking,
        TorrentState::FetchingMetadata => TransferState::Metadata,
        TorrentState::Queued => TransferState::Queued,
        TorrentState::Downloading => TransferState::Downloading,
        TorrentState::Seeding => TransferState::Seeding,
        TorrentState::LowPower => TransferState::LowPower,
        TorrentState::Paused => TransferState::Paused,
        TorrentState::MissingData => TransferState::MissingData,
        TorrentState::Error => TransferState::Error,
        TorrentState::Completed => TransferState::Completed,
        // Held until the user chooses: shown in Downloads with `awaiting_choice`.
        TorrentState::AwaitingChoice => TransferState::Queued,
        // Counting down to its start: `pending` has the deadline.
        TorrentState::Pending => TransferState::Queued,
    };
    let mut group = r.seed_group.map(|g| match g {
        eng::SeedGroup::Popular => SeedGroup::Popular,
        eng::SeedGroup::Active => SeedGroup::Active,
        eng::SeedGroup::Dormant => SeedGroup::Dormant,
        eng::SeedGroup::Error => SeedGroup::Error,
        eng::SeedGroup::Dead => SeedGroup::Dead,
    });
    if !direct && group.is_none() && complete {
        group = Some(match state {
            TransferState::Paused => SeedGroup::Stopped,
            TransferState::Error | TransferState::MissingData => SeedGroup::Error,
            _ => SeedGroup::Active,
        });
    }
    // A torrent that is Popular or Active but whose state still says LowPower cannot happen;
    // a Dormant one that the engine reports as Seeding is a rate-less blip: trust the group.
    if group == Some(SeedGroup::Dormant) && state == TransferState::Seeding {
        state = TransferState::LowPower;
    }
    let note = r.waiting_note();
    let seed_secs = r.seeded_secs;
    let category = r.category.unwrap_or(trs_classify::Category::Other);
    let id = hex(&r.id);
    TransferRow {
        display_name: r.display_title(),
        id,
        name: r.name.clone(),
        kind: if direct { TransferKind::Direct } else { TransferKind::Torrent },
        category: category_to_ffi(category),
        size_bytes: size,
        done_bytes: done,
        down_rate: r.down_rate,
        up_rate: r.up_rate,
        eta_secs: r.eta_secs,
        tracker_id,
        tracker_name,
        ratio: r.ratio,
        ratio_goal: r.seed_goal.ratio.map(f64::from),
        seed_secs,
        state,
        finished_at: r.finished_at,
        save_path: r.save_path.display().to_string(),
        file_ext: file_ext(&r.name),
        seed_group: group,
        error: r.error.clone(),
        retry_in_secs: r.next_retry_ms.map(|ms| (ms / 1000).saturating_sub(now)),
        moving_percent: r.moving.map(|f| (f.clamp(0.0, 1.0) * 100.0) as u8),
        awaiting_choice: r.state == TorrentState::AwaitingChoice,
        is_private: r.private,
        pending: r.pending.map(|p| PendingStart { starts_at_ms: p.deadline_ms, remaining_ms: p.remaining_ms, paused: p.paused }),
        peers: r.peers,
        swarm: match (r.swarm_seeds, r.swarm_leechers) {
            (None, None) => None,
            (s, l) => Some(s.unwrap_or(0).saturating_add(l.unwrap_or(0))),
        },
        known_peers: r.known_peers,
        has_video: false,
        video_ext: String::new(),
        file_count: 0,
        last_activity: r.last_activity,
        waiting_note: note.as_ref().map(|n| n.text.clone()),
        idle_stopped: r.idle_reason(),
        waiting_is_error: note.as_ref().is_some_and(|n| n.error),
        tracker_halted: r.tracker_halted,
        error_transient: r.error_transient,
        retry_attempt: r.tracker_failures,
        announcing_since_ms: r.announcing_since_ms,
        uploaded_bytes: r.uploaded_total,
        files_done: 0,
        total_bytes: r.total_bytes,
        held_bytes: r.done_bytes,
        session_done: r.session_done.min(r.session_size),
        session_size: r.session_size,
    }
}

/// The tracker problem of Get Info, from the engine's own words.
fn tracker_problem(r: &eng::TorrentRow) -> Option<TrackerProblem> {
    let message = r.tracker_detail.clone().or_else(|| r.tracker_error.clone())?;
    let temporary = !r.tracker_halted && r.error_transient;
    // Verified in the engine: announcing and listening are separate. A tracker outage stops
    // neither the peers we hold nor those who already know our address from connecting, and
    // a complete torrent keeps uploading.
    let complete = matches!(r.state, eng::TorrentState::Seeding | eng::TorrentState::LowPower);
    let reassurance = (temporary && complete && r.disk_error.is_none()).then(|| {
        "Your files are fine. Seeding to peers you're already connected to continues, and peers that already know about you can still connect. New peers can't find you through this tracker until it answers.".to_owned()
    });
    Some(TrackerProblem {
        host: r.tracker_host.clone(),
        message,
        since: r.error_since,
        next_try_ms: if r.tracker_halted { None } else { r.next_retry_ms },
        temporary,
        reassurance,
    })
}

fn choice_info(id: &str, row: Option<&TransferRow>, c: &eng::ChoicePreview) -> ChoiceInfo {
    use eng::{ChoiceGroupKind as K, ChoiceReason as R, LibraryState};
    let mut unsure = false;
    let reasons: Vec<String> = c
        .reasons
        .iter()
        .filter_map(|r| match r {
            R::SeveralItems { items } => Some(format!("This holds {items} items; choose which you want.")),
            R::InLibrary { items } => Some(format!(
                "{items} item{} already in your library, so {} not selected.",
                if *items == 1 { " is" } else { "s are" },
                if *items == 1 { "it is" } else { "they are" }
            )),
            R::PlatformVariants => Some("This software comes for several systems; this Mac's build is selected.".into()),
            R::VeryLarge { bytes, .. } => Some(format!("A very large download ({:.0} GB).", *bytes as f64 / 1e9)),
            R::NotEnoughSpace { needed, free } => Some(format!(
                "Not enough free space: {:.0} GB needed, {:.0} GB free.",
                *needed as f64 / 1e9,
                *free as f64 / 1e9
            )),
            R::Unsure { confidence } => {
                unsure = true;
                Some(format!("Not sure what this is ({:.0}% confident). Check the type.", confidence * 100.0))
            }
            R::FolderAsk => Some("Added from a watch folder set to Ask first.".into()),
            R::Always => None,
        })
        .collect();
    ChoiceInfo {
        id: id.to_string(),
        name: row.map(|r| r.name.clone()).unwrap_or_default(),
        display_name: row.map(|r| r.display_name.clone()).unwrap_or_default(),
        tracker: row.map(|r| r.tracker_name.clone()).unwrap_or_default(),
        awaiting: c.awaiting,
        reasons,
        unsure,
        structure: c.structure.to_string(),
        groups: c
            .groups
            .iter()
            .map(|g| ChoiceGroupInfo {
                label: g.label.clone(),
                kind: match g.kind {
                    K::Season(_) => ChoiceGroupKind::Season,
                    K::Specials => ChoiceGroupKind::Specials,
                    K::Collection => ChoiceGroupKind::Collection,
                    K::Album => ChoiceGroupKind::Album,
                    K::Platform => ChoiceGroupKind::Platform,
                    K::Extras => ChoiceGroupKind::Extras,
                    K::Other => ChoiceGroupKind::Other,
                },
                items: g
                    .items
                    .iter()
                    .map(|i| ChoiceItemInfo {
                        label: i.label.clone(),
                        bytes: i.bytes,
                        files: i.files.clone(),
                        selected: i.selected,
                        in_library: matches!(i.library, LibraryState::InLibrary { .. }),
                        other_quality: matches!(i.library, LibraryState::OtherQuality { .. }),
                        quality: i.quality.clone(),
                        this_mac: i.this_mac,
                    })
                    .collect(),
            })
            .collect(),
        file_count: c.files.len() as u32,
        total_bytes: c.total_bytes,
        selected_bytes: c.selected_bytes,
        free_bytes: c.free_bytes,
        destination: c.destination.as_ref().map(|d| d.display().to_string()),
        category: category_to_ffi(c.category.unwrap_or(trs_classify::Category::Other)),
        confidence: c.confidence,
        library_checked: c.library_checked,
        rule_name: c.rule_name.clone(),
        then_text: c.goal.then_phrase_for(c.private),
        summary_text: c.summary().text(),
    }
}

fn move_result(r: Result<eng::MoveReport, eng::EngineError>) -> MoveResult {
    match r {
        Ok(r) => MoveResult { queued: r.queued.len() as u32, refused: r.refused.into_iter().map(|(_, why)| why).collect() },
        Err(e) => MoveResult { queued: 0, refused: vec![e.to_string()] },
    }
}

fn host_of(url: Option<&str>) -> String {
    url.and_then(|u| u.split_once("://"))
        .map(|(_, rest)| rest.split(['/', '?', ':']).next().unwrap_or(rest).to_string())
        .unwrap_or_default()
}

impl Backend for RealBackend {
    fn sync(&self) -> Arc<dyn crate::sync::SyncBackend> {
        self.sync.clone()
    }

    fn snapshot(&self) -> Snapshot {
        let l = self.loaded(CACHE_AGE);
        let pol = lock(&self.policy);
        let profiles = favorites_of(&pol.profiles);
        drop(pol);
        rows::snapshot(
            &l.rows,
            l.recent.clone(),
            &profiles,
            Totals {
                down_rate: l.totals.down_rate,
                up_rate: l.totals.up_rate,
                alt_speed: l.totals.alt_speed,
                down_limit: l.totals.down_limit,
                up_limit: l.totals.up_limit,
                listen_port: l.totals.listen_port,
                connections: l.totals.connections,
                online: l.totals.online,
                offline_since_ms: l.totals.offline_since_ms,
            },
        )
    }

    fn set_listener(&self, listener: Arc<dyn CoreListener>) {
        *lock(&self.listener) = Some(listener);
    }

    fn rows(&self, q: &RowQuery) -> Vec<TransferRow> {
        rows::query(&self.loaded(CACHE_AGE).rows, q, &self.hidden())
    }

    fn sharing_counts(&self, tracker_id: Option<&str>, category: Option<Category>, text: &str) -> SharingCounts {
        rows::counts(&self.loaded(CACHE_AGE).rows, tracker_id, category, text, &self.hidden())
    }

    fn tracker_of(&self, id: &str) -> Option<String> {
        self.row_by_id(id).map(|r| r.tracker_id)
    }

    fn tracker_hosts(&self, group_id: &str) -> Vec<String> {
        let mut hosts: Vec<String> = Vec::new();
        for g in self.loaded(CACHE_AGE).groups.iter().filter(|g| g.group == group_id) {
            if !hosts.contains(&g.host) {
                hosts.push(g.host.clone());
            }
        }
        hosts
    }

    fn tracker_stats(&self, tracker_id: &str) -> TrackerStats {
        let scopes = self.scopes(tracker_id);
        let mut b = trs_state::StatsBucket::default();
        for sc in &scopes {
            b.add(&self.store.state().stats_total(sc).unwrap_or_default());
        }
        TrackerStats {
            lifetime_down: b.downloaded,
            lifetime_up: b.uploaded,
            torrents_completed: b.torrents_completed.min(u64::from(u32::MAX)) as u32,
            files_downloaded: b.files_completed.min(u64::from(u32::MAX)) as u32,
            seed_secs: b.seed_secs,
        }
    }

    fn tracker_daily(&self, tracker_id: &str, days: u32) -> Vec<DailyStat> {
        let now = now_secs();
        let today = Day::from_unix(now as i64, trs_engine::util::local_utc_offset(now as i64));
        let mut out: Vec<DailyStat> = Vec::new();
        for sc in self.scopes(tracker_id) {
            let series = self.store.state().stats_series(&sc, today, days).unwrap_or_default();
            for (i, (d, b)) in series.into_iter().enumerate() {
                let (days_ago, down, up) = (today.0.saturating_sub(d.0), b.downloaded, b.uploaded);
                match out.get_mut(i) {
                    Some(o) => {
                        o.down_bytes += down;
                        o.up_bytes += up;
                    }
                    None => out.push(DailyStat { days_ago, down_bytes: down, up_bytes: up }),
                }
            }
        }
        out
    }

    fn info(&self, id: &str) -> Result<TorrentInfo, CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let d = self
            .rt
            .block_on(self.engine.torrent_detail(tid))
            .map_err(|_| CoreError::NotFound { id: id.into() })?;
        let pol = lock(&self.policy).clone();
        let row = convert(&d.row, &pol, now_secs());
        let real = d.files.iter().filter(|f| !f.padding);
        let total = real.clone().count() as u32;
        let wanted = real.clone().filter(|f| f.priority != eng::FilePriority::Skip).count() as u32;
        let skipped: Vec<&str> = real
            .filter(|f| f.priority == eng::FilePriority::Skip)
            .map(|f| f.path.as_str())
            .collect();
        let skipped_note = skipped_note(&skipped);
        let rule_name = match d.row.rule_in_effect.as_deref() {
            Some(id) => pol.rules.get(id).map_or_else(|| id.to_owned(), |r| r.name.clone()),
            None => "Default".into(),
        };
        let goal = d.row.goal_text();
        Ok(TorrentInfo {
            id: row.id.clone(),
            name: row.name.clone(),
            display_name: row.display_name.clone(),
            category: row.category,
            size_bytes: row.size_bytes,
            peers: d.row.peers,
            down_rate: d.row.down_rate,
            save_path: row.save_path.clone(),
            priority: match d.row.priority {
                eng::Priority::High => Priority::High,
                eng::Priority::Normal => Priority::Normal,
                eng::Priority::Low => Priority::Low,
            },
            seeding_goal: goal,
            idle_text: d.row.idle_info(),
            files_wanted: wanted,
            files_total: total,
            skipped_note,
            rule_name,
            tracker_name: row.tracker_name.clone(),
            error: d.row.disk_error.clone(),
            tracker_problem: tracker_problem(&d.row),
            tracker_halted: d.row.tracker_halted,
            goal_is_custom: d.row.seed_goal_user,
            state: row.state,
            added_at: d.row.added_at,
            finished_at: d.row.finished_at,
            seeded_secs: d.row.seeded_secs,
            ratio: d.row.ratio,
            uploaded_total: d.row.uploaded_total,
            downloaded_total: d.row.downloaded_total,
            piece_count: d.piece_count,
            piece_length: d.piece_length,
            info_hash: row.id.clone(),
            is_private: d.row.private,
            seeds_connected: d.row.seeds,
            swarm_seeders: d.row.swarm_seeds,
            swarm_leechers: d.row.swarm_leechers,
            trackers: d
                .trackers
                .iter()
                .map(|t| TrackerLine {
                    url: t.shown_url.clone(),
                    tier: t.tier,
                    status: t.status.clone(),
                    last_announce: t.last_announce_ms.map(|m| m / 1000),
                    next_announce: t.next_announce_ms.map(|m| m / 1000),
                    seeders: t.seeds.map(|n| n.min(u64::from(u32::MAX)) as u32),
                    leechers: t.leechers.map(|n| n.min(u64::from(u32::MAX)) as u32),
                    state: t.state.as_str().to_owned(),
                    failures: t.failures,
                    retry_at: t.retry_at_ms.map(|m| m / 1000),
                    last_success: t.last_success_ms.map(|m| m / 1000),
                    manual_ok_at: t.manual_ok_ms.map(|m| m / 1000),
                })
                .collect(),
        })
    }

    fn tracker_url(&self, id: &str, index: u32) -> Result<String, CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let d = self
            .rt
            .block_on(self.engine.torrent_detail(tid))
            .map_err(|_| CoreError::NotFound { id: id.into() })?;
        d.trackers
            .get(index as usize)
            .map(|t| t.full_url.reveal().to_owned())
            .ok_or_else(|| CoreError::NotFound { id: id.into() })
    }

    fn magnet_link(&self, id: &str) -> Result<String, CoreError> {
        // Only the info-hash and the name: trackers would carry passkeys (SPEC 7B.4).
        let row = self.row_by_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        if row.kind == TransferKind::Direct {
            return Err(CoreError::InvalidSource { message: "A direct download has no magnet link".into() });
        }
        Ok(format!("magnet:?xt=urn:btih:{}&dn={}", row.id, percent_encode(&row.name)))
    }

    fn add_source(&self, source: &str) -> Result<String, CoreError> {
        let s = source.trim();
        let low = s.to_lowercase();
        let engine = self.engine.clone();
        let added = if low.starts_with("magnet:") {
            self.rt.block_on(engine.add_magnet(s, AddOptions::default()))
        } else if low.starts_with("http://") || low.starts_with("https://") {
            self.rt.block_on(engine.add_download(s, eng::DownloadOptions::default()))
        } else {
            let path = PathBuf::from(s);
            if !path.is_file() {
                return Err(CoreError::InvalidSource { message: format!("Not a magnet link, web address or .torrent file: {s}") });
            }
            self.rt.block_on(async move { engine.add_torrent_path(&path, AddOptions::default(), None).await })
        };
        *lock(&self.cache) = None;
        match added {
            Ok(a) => Ok(hex(&a.id)),
            // New trackers were merged into the torrent already here: not an error; the listener
            // heard "Added N trackers to <name>".
            Err(eng::EngineError::TrackersMerged { id, .. }) => Ok(hex(&id)),
            Err(eng::EngineError::Duplicate(_)) => Err(CoreError::InvalidSource { message: "Already added".into() }),
            Err(e) => Err(engine_err(&e)),
        }
    }

    fn pause(&self, ids_: &[String]) {
        self.engine.pause(&ids(ids_));
        *lock(&self.cache) = None;
    }

    fn resume(&self, ids_: &[String]) {
        self.engine.resume(&ids(ids_));
        *lock(&self.cache) = None;
    }

    fn move_to_top(&self, id: &str) {
        self.engine.move_to_top(&ids(&[id.to_string()]));
        *lock(&self.cache) = None;
    }

    fn remove(&self, ids_: &[String], delete_files: bool) {
        let (engine, v) = (self.engine.clone(), ids(ids_));
        self.rt.spawn(async move {
            let _ = engine.remove(&v, delete_files).await;
        });
        *lock(&self.cache) = None;
    }

    fn set_priority(&self, ids_: &[String], p: Priority) {
        let p = match p {
            Priority::High => eng::Priority::High,
            Priority::Normal => eng::Priority::Normal,
            Priority::Low => eng::Priority::Low,
        };
        self.engine.set_priority(&ids(ids_), p);
        *lock(&self.cache) = None;
    }

    fn set_category(&self, ids_: &[String], c: Category) {
        self.engine.set_category(&ids(ids_), Some(category_from(c)));
        *lock(&self.cache) = None;
    }

    fn set_seeding_goal(&self, ids_: &[String], goal: crate::types::SeedingGoal) {
        match goal {
            crate::types::SeedingGoal::Ratio { ratio } => {
                self.engine.set_seed_goal(&ids(ids_), SeedGoal { ratio: Some(ratio as f32), ..SeedGoal::FOREVER });
            }
            crate::types::SeedingGoal::Forever => self.engine.set_seed_goal(&ids(ids_), SeedGoal::FOREVER),
            crate::types::SeedingGoal::RuleDefault => self.engine.clear_seed_goal(&ids(ids_)),
        }
        *lock(&self.cache) = None;
    }

    fn set_rule(&self, ids_: &[String], rule: Option<String>) {
        self.engine.set_rule(&ids(ids_), rule);
        *lock(&self.cache) = None;
    }

    fn verify(&self, ids_: &[String]) {
        self.engine.recheck(&ids(ids_));
    }

    fn ask_tracker_again(&self, ids_: &[String], force: bool) -> AskAgainOutcome {
        let list = ids(ids_);
        let results = self.rt.block_on(self.engine.reannounce_with(&list, force)).unwrap_or_default();
        *lock(&self.cache) = None;
        let now_ms = now_secs() * 1000;
        let sent = results.iter().filter(|r| r.sent).count() as u32;
        let waits: Vec<u64> = results.iter().filter(|r| !r.sent).filter_map(|r| r.not_before_ms).collect();
        AskAgainOutcome {
            sent,
            deferred: results.iter().filter(|r| !r.sent).count() as u32,
            wait_secs: waits.iter().min().map(|t| t.saturating_sub(now_ms).div_ceil(1000)),
        }
    }

    fn pause_all(&self) {
        self.engine.pause_all();
        *lock(&self.cache) = None;
    }

    fn resume_all(&self) {
        self.engine.resume_all();
        *lock(&self.cache) = None;
    }

    fn clear_recent(&self) {
        let now = now_secs();
        *lock(&self.cleared_at) = now;
        let _ = std::fs::write(self.dir.join("recent-cleared-at"), now.to_string());
        let direct: Vec<[u8; 20]> = self
            .loaded(Duration::from_secs(5))
            .rows
            .iter()
            .filter(|r| r.kind == TransferKind::Direct && r.state == TransferState::Completed)
            .filter_map(|r| parse_id(&r.id))
            .collect();
        if !direct.is_empty() {
            self.engine.forget_finished(&direct);
        }
        *lock(&self.cache) = None;
    }

    fn set_limits(&self, down: Option<u64>, up: Option<u64>) {
        // Written to config.toml so the limit survives a restart; the engine applies it at once.
        self.engine.set_limits(down, up);
        let mut c = self.settings.config();
        c.transfers.download_limit = down.unwrap_or(0);
        c.transfers.upload_limit = up.unwrap_or(0);
        let _ = DocStoreSave::save_config(&self.dir, &c);
    }

    fn set_alt_speed(&self, on: bool) {
        self.engine.set_alt_speed(on);
        *lock(&self.cache) = None;
    }

    fn torrent_attrs(&self) -> Vec<AttrRow> {
        self.loaded(Duration::from_secs(2)).attrs.clone()
    }

    fn agent_activity(&self, limit: u32) -> Vec<AgentActivity> {
        lock(&self.server)
            .as_ref()
            .map(|s| {
                s.activity()
                    .recent(limit as usize)
                    .into_iter()
                    .map(|e| AgentActivity { at_ms: e.at, client: e.client, method: e.method, summary: e.summary, outcome: e.outcome })
                    .collect()
            })
            .unwrap_or_default()
    }

    fn agents_awake(&self) -> Option<crate::types::AgentsAwake> {
        let leases = lock(&self.server).as_ref()?.awake().leases();
        let until_ms = leases.iter().map(|l| l.until_ms).max()?;
        let mut who: Vec<String> = Vec::new();
        for l in leases {
            if !who.contains(&l.who) {
                who.push(l.who);
            }
        }
        Some(crate::types::AgentsAwake { who, until_ms })
    }

    fn control_socket(&self) -> Option<String> {
        self.socket.clone()
    }

    fn shutdown(&self, deadline_secs: u32) -> ShutdownInfo {
        let sd = &self.sd;
        if !sd.started.swap(true, Ordering::SeqCst) {
            let host = lock(&self.host).take();
            let server = lock(&self.server).take();
            let sync = self.sync.clone();
            let sd2 = self.sd.clone();
            let secs = u64::from(deadline_secs);
            *lock(&sd.deadline) = Some(Instant::now() + Duration::from_secs(secs + 5));
            self.rt.spawn(async move {
                if let Some(s) = server {
                    s.shutdown().await;
                }
                sync.stop().await;
                let r = match host {
                    Some(h) => h.shutdown(Duration::from_secs(secs)).await,
                    None => eng::ShutdownReport::default(),
                };
                *lock(&sd2.result) = Some(ShutdownInfo {
                    confirmed: r.stopped_confirmed,
                    abandoned: r.stopped_abandoned,
                    finished: true,
                });
                sd2.cv.notify_all();
            });
        }
        let mut g = lock(&sd.result);
        loop {
            if let Some(r) = *g {
                return r;
            }
            let dl = (*lock(&sd.deadline)).unwrap_or_else(Instant::now);
            let now = Instant::now();
            if now >= dl {
                return ShutdownInfo { finished: false, ..ShutdownInfo::default() };
            }
            let step = (dl - now).min(Duration::from_millis(200));
            g = sd.cv.wait_timeout(g, step).unwrap_or_else(PoisonError::into_inner).0;
        }
    }

    fn shorten_shutdown(&self, secs: u32) {
        let mut d = lock(&self.sd.deadline);
        let cap = Instant::now() + Duration::from_secs(u64::from(secs));
        *d = Some(d.map_or(cap, |x| x.min(cap)));
        self.sd.cv.notify_all();
    }

    fn diagnostics(&self) -> DiagnosticsInfo {
        let d = self.engine.diagnostics();
        DiagnosticsInfo {
            wakeups: d.wakeups,
            connections: self.loaded(CACHE_AGE).totals.connections,
            rechecks: d.rechecks,
            hash_failures: d.hash_failures,
            peers_banned: d.peers_banned,
            promotions: d.promotions,
            demotions: d.demotions,
        }
    }

    fn location_moves(&self) -> MoveProposalInfo {
        let Ok(p) = self.rt.block_on(self.engine.location_moves()) else {
            return MoveProposalInfo { items: Vec::new(), total_bytes: 0, cross_volume: 0 };
        };
        MoveProposalInfo {
            total_bytes: p.total_bytes,
            cross_volume: p.cross_volume,
            items: p
                .items
                .into_iter()
                .map(|i| MoveItemInfo {
                    id: hex(&i.id),
                    name: i.name,
                    bytes: i.bytes,
                    from: i.from.display().to_string(),
                    to: i.to.display().to_string(),
                    cross_volume: i.cross_volume,
                })
                .collect(),
        }
    }

    fn files(&self, id: &str) -> Result<Vec<TorrentFile>, CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let d = self
            .rt
            .block_on(self.engine.torrent_detail(tid))
            .map_err(|_| CoreError::NotFound { id: id.into() })?;
        let raw: Vec<String> = d.files.iter().map(|f| f.path.clone()).collect();
        let places = crate::play::tree_paths(&raw, &d.row.save_path);
        let facts: Vec<crate::play::FileFacts<'_>> = d
            .files
            .iter()
            .zip(&places)
            .map(|(f, (path, disk))| crate::play::FileFacts {
                path,
                disk,
                length: f.length,
                done: f.done,
                priority: file_priority(f.priority),
                padding: f.padding,
                uploaded: f.uploaded,
            })
            .collect();
        Ok(crate::play::list(&facts))
    }

    fn set_file_priorities(&self, id: &str, changes: &[FilePriorityChange]) -> Result<(), CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let d = self
            .rt
            .block_on(self.engine.torrent_detail(tid))
            .map_err(|_| CoreError::NotFound { id: id.into() })?;
        let mut prios: Vec<eng::FilePriority> = d.files.iter().map(|f| f.priority).collect();
        for c in changes {
            let i = c.index as usize;
            match d.files.get(i) {
                None => return Err(CoreError::Failed { message: format!("the torrent has no file {}", c.index) }),
                Some(f) if f.padding => {}
                Some(_) => prios[i] = engine_priority(c.priority),
            }
        }
        let r = self.rt.block_on(self.engine.set_file_priorities(tid, prios)).map_err(|e| engine_err(&e));
        *lock(&self.cache) = None;
        r
    }

    fn stream_url(&self, id: &str, file_index: u32) -> Result<StreamInfo, CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let files = self.files(id)?;
        let f = crate::play::check_streamable(&files, file_index)?;
        let name = f.name.clone();
        let i = self.rt.block_on(self.engine.stream_url(tid, file_index)).map_err(|e| stream_err(&e, &name))?;
        if i.selected {
            // The file list and the row's progress change: the UI refreshes.
            *lock(&self.cache) = None;
        }
        Ok(StreamInfo {
            url: i.url.unwrap_or_default(),
            token: i.token.unwrap_or_default(),
            complete: i.complete,
            selected: i.selected,
            content_type: i.content_type,
            is_mp4_family: i.is_mp4_family,
        })
    }

    fn network_path_changed(&self, satisfied: bool) {
        self.engine.network_path_changed(satisfied);
    }

    fn system_will_sleep(&self) {
        self.engine.system_will_sleep();
    }

    fn system_did_wake(&self) {
        self.engine.system_did_wake();
    }

    fn volumes_changed(&self) {
        self.engine.volumes_changed();
    }

    fn listen_port(&self) -> u16 {
        self.engine.listen_port()
    }

    fn reachability(&self) -> Option<ReachabilityInfo> {
        let r = trs_control::reachability_dto(&self.engine.reachability());
        Some(ReachabilityInfo { state: r.state, summary: r.summary })
    }

    fn port_fallback(&self) -> Option<PortFallbackInfo> {
        self.engine.port_fallback().map(|f| PortFallbackInfo { wanted: f.wanted, using: f.using, message: f.message })
    }

    fn stop_stream(&self, token: &str) {
        self.engine.stop_stream(token);
    }

    fn move_torrents(&self, ids_: &[String]) -> MoveResult {
        let r = self.rt.block_on(self.engine.move_torrents(&ids(ids_)));
        *lock(&self.cache) = None;
        move_result(r)
    }

    fn move_to(&self, ids_: &[String], path: &Path) -> Result<MoveResult, CoreError> {
        let r = self.rt.block_on(self.engine.move_to(&ids(ids_), path)).map_err(|e| engine_err(&e));
        *lock(&self.cache) = None;
        r.map(|r| move_result(Ok(r)))
    }

    fn cancel_moves(&self, ids_: &[String]) {
        self.engine.cancel_moves(&ids(ids_));
    }

    fn choice(&self, id: &str, reopen: bool) -> Result<ChoiceInfo, CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let c = if reopen {
            self.rt.block_on(self.engine.reopen_choice(tid))
        } else {
            self.rt.block_on(self.engine.choice(tid))
        }
        .map_err(|e| engine_err(&e))?;
        let row = self.row_by_id(id);
        let info = choice_info(id, row.as_ref(), &c);
        Ok(info)
    }

    fn confirm_choice(&self, id: &str, files: &[bool]) -> Result<(), CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let sel = eng::ChoiceSelection::Files(
            files
                .iter()
                .map(|on| if *on { eng::FilePriority::Normal } else { eng::FilePriority::Skip })
                .collect(),
        );
        let r = self.rt.block_on(self.engine.confirm_choice(tid, sel)).map_err(|e| engine_err(&e));
        *lock(&self.cache) = None;
        r
    }

    fn pending_act(&self, id: &str, act: crate::backend::PendingAct) -> Result<bool, CoreError> {
        use crate::backend::PendingAct as A;
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let e = &self.engine;
        let r = self.rt.block_on(async {
            match act {
                A::Pause => e.pending_pause(tid).await.map(|()| false),
                A::Resume => e.pending_resume(tid).await.map(|()| false),
                A::StartNow => e.start_now(tid).await.map(|()| false),
                A::Choose => e.pending_choose(tid).await,
            }
        });
        *lock(&self.cache) = None;
        r.map_err(|e| engine_err(&e))
    }

    fn cancel_choice(&self, id: &str) -> Result<(), CoreError> {
        let tid = parse_id(id).ok_or_else(|| CoreError::NotFound { id: id.into() })?;
        let r = self.rt.block_on(self.engine.cancel_choice(tid)).map_err(|e| engine_err(&e));
        *lock(&self.cache) = None;
        r
    }

    fn held_source(&self, id: &str) -> Option<String> {
        self.engine.held_source(parse_id(id)?).map(|p| p.display().to_string())
    }

    fn has_held_source(&self, id: &str) -> bool {
        parse_id(id).is_some_and(|t| self.engine.has_held_source(t))
    }

    fn watch_backlog(&self, folder: &Path) -> Vec<String> {
        let files = self.rt.block_on(self.engine.watch_backlog(folder));
        files.into_iter().map(|p| p.display().to_string()).collect()
    }

    fn settings_saved(&self) {
        reload_policy(&self.policy, &self.dir);
        *lock(&self.cache) = None;
        // Developer mode follows the switch at once; the settings poller applies the rest of the
        // file within a couple of seconds and sets the same value.
        self.engine.devlog().set_enabled(self.settings.config().advanced.developer_mode);
    }

    fn tracker_log_clear(&self, _dir: &std::path::Path) {
        self.engine.devlog().clear();
    }

    fn tracker_log_dropped(&self) -> u64 {
        self.engine.devlog().dropped()
    }
}

struct DocStoreSave;

impl DocStoreSave {
    fn save_config(dir: &Path, c: &trs_state::Config) -> Result<(), trs_state::DocError> {
        trs_state::DocStore::<trs_state::Config>::new(dir).save(c)
    }
}

fn skipped_note(paths: &[&str]) -> String {
    if paths.is_empty() {
        return String::new();
    }
    let mut kinds: Vec<&str> = Vec::new();
    for p in paths {
        let lower = p.to_lowercase();
        let k = if lower.contains("sample") {
            "sample"
        } else if lower.ends_with(".nfo") {
            ".nfo"
        } else if lower.ends_with(".txt") {
            ".txt"
        } else if lower.ends_with(".jpg") || lower.ends_with(".png") {
            "screenshots"
        } else {
            "other"
        };
        if !kinds.contains(&k) {
            kinds.push(k);
        }
    }
    if kinds.contains(&"other") {
        return format!("{} skipped", paths.len());
    }
    let joined = match kinds.as_slice() {
        [a] => (*a).to_string(),
        [a, b] => format!("{a} and {b}"),
        [a, rest @ ..] => format!("{a} and {} more", rest.len()),
        [] => String::new(),
    };
    format!("{joined} skipped")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skipped_files_are_described_in_words() {
        assert_eq!(skipped_note(&[]), "");
        assert_eq!(skipped_note(&["Movie/sample.mkv"]), "sample skipped");
        assert_eq!(skipped_note(&["a/sample.mkv", "a/release.nfo"]), "sample and .nfo skipped");
        assert_eq!(skipped_note(&["a/disc2.iso", "a/x.nfo"]), "2 skipped");
    }

    #[test]
    fn magnet_names_are_percent_encoded_and_hashes_round_trip() {
        assert_eq!(percent_encode("Night of the Living Dead (1968)"), "Night%20of%20the%20Living%20Dead%20%281968%29");
        let id = parse_id("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(hex(&id), "0123456789abcdef0123456789abcdef01234567");
        assert!(parse_id("xyz").is_none());
    }

    #[test]
    fn only_known_extensions_get_a_document_icon() {
        assert_eq!(file_ext("Night.of.the.Living.Dead.1968.mkv"), "mkv");
        assert_eq!(file_ext("Fallen.Kingdom.S01"), "");
        assert_eq!(file_ext("Example Show"), "");
    }

    #[test]
    fn host_of_direct_urls() {
        assert_eq!(host_of(Some("https://releases.ubuntu.com/26.04/x.iso?a=b")), "releases.ubuntu.com");
        assert_eq!(host_of(None), "");
    }
}

/// Files that count as downloaded on a row (the ticked ones, as the list's header counts them): those whose
/// verified bytes cover them, and, once everything wanted is here, every ticked file (the engine's per-file figure is by pieces, so a file next to a
/// skipped one can fall a few bytes short although the list shows it done).
fn files_done(files: &[eng::FileInfo], finished: bool) -> u32 {
    files
        .iter()
        .filter(|f| !f.padding && f.priority != eng::FilePriority::Skip && (finished || f.done >= f.length))
        .count() as u32
}

#[cfg(test)]
mod files_done_tests {
    use super::*;

    fn f(len: u64, done: u64, p: eng::FilePriority) -> eng::FileInfo {
        eng::FileInfo { path: "x".into(), length: len, done, priority: p, padding: false, uploaded: 0 }
    }

    #[test]
    fn a_finished_torrent_counts_every_wanted_file() {
        use eng::FilePriority::{Normal, Skip};
        let files = [f(100, 100, Normal), f(100, 97, Normal), f(100, 40, Skip), f(100, 100, Skip)];
        assert_eq!(files_done(&files, true), 2, "the ticked ones; a skipped file does not count");
        assert_eq!(files_done(&files, false), 1, "while downloading only whole files count");
    }
}
