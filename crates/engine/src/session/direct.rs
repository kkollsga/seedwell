//! Direct downloads (SPEC 7D) inside the session: they share the "active downloads" queue and the
//! global download limiter with torrents, survive restarts, and appear in `snapshot()` as rows of
//! kind [`RowKind::Direct`]. They are never seeded and never counted as Sharing.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use trs_http::{BoxFuture, DownloadRequest, DownloadTask, Downloader, DownloaderConfig, Progress, RateGate, State};

use super::{Job, Session, SessionMsg};
use crate::api::*;
use crate::persist::{DirectRecord, DirectState};
use crate::ratelimit::{Dir, RateLimiter};
use crate::util::{hex, unix_secs};

/// Most direct downloads in the table (running, queued and finished together).
const MAX_DIRECTS: usize = 2000;
/// Finished downloads kept for "Recently Finished".
const KEEP_FINISHED: usize = 50;
/// Longest URL accepted.
const MAX_URL: usize = 8192;

/// The global download limiter as trs-http's admission gate: torrent and direct traffic share one budget.
pub(crate) struct DownGate(pub Arc<RateLimiter>);

impl RateGate for DownGate {
    fn acquire(&self, bytes: usize) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            if !self.0.is_limited(Dir::Down) {
                return;
            }
            let wait = self.0.charge(Dir::Down, Priority::Normal, bytes as u64, Instant::now());
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
        })
    }
}

/// Messages from the per-download forwarder and from placement jobs.
pub(crate) enum DirectMsg {
    /// Progress changed in a way worth a record or a row update.
    Changed(TorrentId, Progress),
    /// The server file changed and the download started over.
    Restarted(TorrentId, String),
    /// The finished file was moved to its library place.
    Placed { id: TorrentId, path: PathBuf, category: Option<Category> },
}

pub(crate) struct DirectEntry {
    pub rec: DirectRecord,
    pub task: Option<DownloadTask>,
    /// Wants a slot but the queue holds it back.
    pub queued: bool,
    pub error: Option<String>,
    forwarder: Option<JoinHandle<()>>,
    last_saved_done: u64,
}

impl Drop for DirectEntry {
    fn drop(&mut self) {
        if let Some(f) = self.forwarder.take() {
            f.abort();
        }
    }
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|r| r.split(['/', '?', '#']).next())
        .map(|h| h.rsplit('@').next().unwrap_or(h).to_owned())
        .unwrap_or_default()
}

fn name_from_url(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    path.rsplit('/').find(|s| !s.is_empty() && !s.contains(':')).unwrap_or("download").to_owned()
}

impl DirectEntry {
    fn state(&self) -> TorrentState {
        match &self.rec.state {
            DirectState::Completed => TorrentState::Completed,
            DirectState::Failed(_) => TorrentState::Error,
            DirectState::Paused => TorrentState::Paused,
            DirectState::Active => {
                if self.queued {
                    return TorrentState::Queued;
                }
                match self.task.as_ref().map(|t| t.progress().state) {
                    Some(State::Verifying) => TorrentState::Checking,
                    Some(State::Paused) => TorrentState::Paused,
                    Some(State::Failed) => TorrentState::Error,
                    Some(State::Completed) => TorrentState::Completed,
                    _ => TorrentState::Downloading,
                }
            }
        }
    }

    /// The row for the snapshot.
    pub(crate) fn row(&self) -> TorrentRow {
        let p = self.task.as_ref().map(DownloadTask::progress);
        let state = self.state();
        let total = p.as_ref().and_then(|p| p.total).or(self.rec.total).unwrap_or(0);
        let done = p.as_ref().map_or(self.rec.done, |p| p.done.max(self.rec.done.min(total)));
        let rate = if state == TorrentState::Downloading { p.as_ref().map_or(0, |p| p.rate) } else { 0 };
        let name = p
            .as_ref()
            .and_then(|p| p.file_name.clone())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.rec.name.clone());
        let error = match &self.rec.state {
            DirectState::Failed(m) => Some(m.clone()),
            _ => p.as_ref().and_then(|p| p.error.as_ref()).map(|e| e.to_string()).or_else(|| self.error.clone()),
        };
        TorrentRow {
            id: self.rec.id,
            name,
            state,
            total_bytes: total,
            wanted_bytes: total,
            done_bytes: done.min(total.max(done)),
            session_done: done.min(total),
            session_size: total,
            down_rate: rate,
            up_rate: 0,
            peers: p.as_ref().map_or(0, |p| p.connections),
            seeds: 0,
            swarm_seeds: None,
            swarm_leechers: None,
            known_peers: 0,
            eta_secs: (state == TorrentState::Downloading && rate > 0).then(|| total.saturating_sub(done) / rate),
            ratio: 0.0,
            tracker_host: host_of(&self.rec.url),
            tier: crate::tier::Tier::Cold,
            private: false,
            finished_at: self.rec.finished_at,
            added_at: self.rec.added_at,
            error,
            save_path: self.rec.final_path.clone().unwrap_or_else(|| self.rec.dest_dir.clone()),
            priority: Priority::Normal,
            queue_pos: self.rec.queue_pos,
            uploaded_total: 0,
            downloaded_total: done,
            category: self.rec.category,
            seed_goal: SeedGoal::FOREVER,
            tracker_profile: None,
            rule: None,
            rule_in_effect: None,
            kind: RowKind::Direct,
            url: Some(self.rec.url.clone()),
            up_limit: self.rec.rate_limit,
            organized: false,
            seed_group: None,
            last_activity: None,
            error_since: None,
            next_retry_ms: None,
            moving: None,
            move_note: None,
            title: String::new(),
            seeded_secs: 0,
            seed_goal_user: false,
            pending: None,
            tracker_error: None,
            tracker_detail: None,
            tracker_failures: 0,
            announcing_since_ms: None,
            disk_error: None,
            error_transient: false,
            tracker_halted: false,
            stalled: false,
            hold_note: None,
            idle: None,
        }
    }

    fn wants_slot(&self) -> bool {
        self.rec.state == DirectState::Active
    }
}

impl Session {
    fn downloader(&mut self) -> Downloader {
        if self.downloader.is_none() {
            let cfg = self.sh.cfg();
            let dc = DownloaderConfig {
                version: cfg.client_version.clone(),
                max_connections_per_host: cfg.direct.max_connections_per_host.max(1),
                ..DownloaderConfig::default()
            };
            self.downloader = Some(Downloader::new(dc, Arc::new(DownGate(self.sh.limiter.clone()))));
        }
        self.downloader.clone().unwrap_or_else(|| Downloader::unlimited(DownloaderConfig::default()))
    }

    fn incomplete_for(&self, dest: &Path) -> PathBuf {
        self.sh.cfg().incomplete_dir.clone().unwrap_or_else(|| dest.join(".incomplete"))
    }

    pub(super) fn add_download(&mut self, url: String, opts: DownloadOptions) -> Result<Added, EngineError> {
        if self.shutting_down {
            return Err(EngineError::ShuttingDown);
        }
        let lower = url.get(..8).unwrap_or(&url).to_ascii_lowercase();
        if url.len() > MAX_URL || !(lower.starts_with("http://") || lower.starts_with("https://")) || url.chars().any(char::is_control) {
            return Err(EngineError::Invalid("only http:// and https:// links are supported".into()));
        }
        if url.split("://").nth(1).and_then(|r| r.split(['/', '?', '#']).next()).is_some_and(|a| a.contains('@')) {
            return Err(EngineError::Invalid("links with a user name or password are not supported".into()));
        }
        if self.directs.len() >= MAX_DIRECTS {
            return Err(EngineError::Invalid("too many downloads; clear the finished ones first".into()));
        }
        let cfg = self.sh.cfg();
        let mut id = [0u8; 20];
        if getrandom::fill(&mut id).is_err() {
            id[..8].copy_from_slice(&crate::util::unix_ms().to_le_bytes());
        }
        let dest = opts.dest_dir.clone().unwrap_or_else(|| cfg.default_save_dir.clone());
        let name = opts.file_name.clone().unwrap_or_else(|| name_from_url(&url));
        let pos = self.next_pos;
        self.next_pos += 1;
        let rec = DirectRecord {
            id,
            url,
            name: name.clone(),
            dest_dir: dest,
            file_name: opts.file_name,
            connections: opts.connections.unwrap_or(cfg.direct.connections_per_download).clamp(1, 16),
            sha256: opts.sha256,
            rate_limit: opts.rate_limit.filter(|r| *r > 0),
            state: if opts.paused { DirectState::Paused } else { DirectState::Active },
            category: opts.category,
            added_at: unix_secs(),
            finished_at: None,
            final_path: None,
            total: None,
            done: 0,
            queue_pos: pos,
        };
        self.commit_now(crate::persist::Batch { directs: vec![rec.clone()], ..Default::default() })?;
        self.directs.insert(id, DirectEntry { rec, task: None, queued: false, error: None, forwarder: None, last_saved_done: 0 });
        self.sh.emit(EngineEvent::Added { id, name: name.clone() });
        self.replan();
        self.schedule_after(Duration::from_secs(5), Job::DirectTick);
        Ok(Added { id, name, needs_choice: false, choice: None, pending: None })
    }

    /// Restore the records of a previous run. Nothing is started here; the queue decides.
    pub(crate) fn load_directs(&mut self, recs: Vec<DirectRecord>) {
        let mut recs = recs;
        recs.sort_by_key(|r| (r.queue_pos, r.added_at, r.id));
        for rec in recs.into_iter().take(MAX_DIRECTS) {
            self.next_pos = self.next_pos.max(rec.queue_pos + 1);
            let done = rec.done;
            self.directs.insert(rec.id, DirectEntry { rec, task: None, queued: false, error: None, forwarder: None, last_saved_done: done });
        }
        if self.directs.values().any(DirectEntry::wants_slot) {
            self.schedule_after(Duration::from_secs(5), Job::DirectTick);
        }
    }

    /// Start, resume or pause the transfer so it matches what it should be doing.
    pub(crate) fn direct_reconcile(&mut self, id: TorrentId) {
        if self.shutting_down {
            return;
        }
        let Some(e) = self.directs.get_mut(&id) else { return };
        let run = e.rec.state == DirectState::Active && !e.queued;
        if run {
            match &e.task {
                Some(t) => {
                    if t.progress().state == State::Paused {
                        t.resume();
                    }
                }
                None => self.direct_start(id),
            }
        } else if let Some(t) = &e.task
            && !matches!(t.progress().state, State::Paused | State::Completed | State::Failed | State::Cancelled) {
                t.pause();
            }
    }

    fn direct_start(&mut self, id: TorrentId) {
        let dl = self.downloader();
        let Some(e) = self.directs.get(&id) else { return };
        let r = &e.rec;
        let incomplete = self.incomplete_for(&r.dest_dir);
        let req = DownloadRequest {
            id: hex(&id),
            url: r.url.clone(),
            dest_dir: r.dest_dir.clone(),
            incomplete_dir: incomplete,
            file_name: r.file_name.clone(),
            connections: r.connections,
            sha256: r.sha256,
            rate_limit: r.rate_limit,
            start_paused: false,
        };
        match dl.start(req) {
            Ok(task) => {
                let tx = self.sh.session.clone();
                let mut rx = task.subscribe();
                let mut events = task.events();
                let fwd = tokio::spawn(async move {
                    let mut last: Option<(State, Option<String>, u32)> = None;
                    loop {
                        tokio::select! {
                            r = rx.changed() => {
                                if r.is_err() {
                                    break;
                                }
                                let p = rx.borrow_and_update().clone();
                                let key = (p.state, p.file_name.clone(), p.restarts);
                                if last.as_ref() != Some(&key) {
                                    last = Some(key);
                                    if tx.send(SessionMsg::Direct(DirectMsg::Changed(id, p))).await.is_err() {
                                        break;
                                    }
                                }
                            }
                            Some(ev) = async {
                                match events.as_mut() {
                                    Some(e) => e.recv().await,
                                    None => std::future::pending().await,
                                }
                            } => {
                                if let trs_http::Event::Restarted(why) = ev {
                                    let text = match why {
                                        trs_http::RestartReason::ValidatorChanged => "the file changed on the server",
                                        trs_http::RestartReason::RangeIgnored => "the server does not support resuming",
                                        trs_http::RestartReason::SizeChanged => "the file size changed on the server",
                                        trs_http::RestartReason::NoValidator => "the server gave no way to verify a resume",
                                        trs_http::RestartReason::PartMismatch => "the partial file did not match its record",
                                    };
                                    let _ = tx.send(SessionMsg::Direct(DirectMsg::Restarted(id, text.to_owned()))).await;
                                }
                            }
                        }
                    }
                });
                if let Some(e) = self.directs.get_mut(&id) {
                    e.task = Some(task);
                    e.forwarder = Some(fwd);
                    e.error = None;
                }
            }
            Err(err) => {
                if let Some(e) = self.directs.get_mut(&id) {
                    e.rec.state = DirectState::Failed(err.to_string());
                    let rec = e.rec.clone();
                    self.dirty_direct(rec);
                    self.sh.emit(EngineEvent::Error { id, message: err.to_string() });
                }
            }
        }
    }

    pub(super) fn direct_set_want(&mut self, id: TorrentId, active: bool) {
        self.direct_set_want_with(id, active, true);
    }

    /// `replan: false` for the bulk callers (Pause all / Resume all), which replan once.
    pub(super) fn direct_set_want_with(&mut self, id: TorrentId, active: bool, replan: bool) {
        let Some(e) = self.directs.get_mut(&id) else { return };
        match (&e.rec.state, active) {
            (DirectState::Completed, _) => return,
            (DirectState::Active, true) | (DirectState::Paused, false) => return,
            (_, true) => {
                e.rec.state = DirectState::Active;
                e.error = None;
            }
            (_, false) => e.rec.state = DirectState::Paused,
        }
        if let Some(t) = &e.task
            && active && t.progress().state == State::Failed {
                // A failed task retries on `resume()`.
                t.resume();
            }
        let rec = e.rec.clone();
        let st = e.state();
        self.dirty_direct(rec);
        self.sh.emit(EngineEvent::StateChanged { id, state: st });
        if replan {
            self.replan();
        }
        self.direct_reconcile(id);
    }

    pub(super) fn direct_remove(&mut self, id: TorrentId, delete_files: bool) {
        let Some(mut e) = self.directs.remove(&id) else { return };
        let incomplete = self.incomplete_for(&e.rec.dest_dir);
        let finished = e.rec.state == DirectState::Completed;
        let waits = self.remove_waits.remove(&id).unwrap_or_default();
        let mut async_done = false;
        if delete_files && finished
            && let Some(p) = e.rec.final_path.clone() {
                async_done = true;
                let waits = waits.clone();
                tokio::task::spawn_blocking(move || {
                    let _ = std::fs::remove_file(p);
                    for w in waits {
                        w.done();
                    }
                });
            }
        if !finished {
            async_done = true;
            let waits = waits.clone();
            // Unfinished: the partial file goes with the download. Dropping a task aborts it like
            // a crash, so let it finish cancelling before it is dropped.
            let task = e.task.take();
            let hex_id = hex(&id);
            tokio::spawn(async move {
                if let Some(t) = task {
                    t.cancel(false);
                    let _ = tokio::time::timeout(Duration::from_secs(10), t.wait()).await;
                }
                trs_http::discard_staged(&incomplete, &hex_id);
                for w in waits {
                    w.done();
                }
            });
        }
        if !async_done {
            for w in waits {
                w.done();
            }
        }
        drop(e);
        self.dirty_direct_removed(id);
        self.sh.emit(EngineEvent::Removed { id });
        self.replan();
    }

    pub(super) fn set_download_limit(&mut self, id: TorrentId, limit: Option<u64>) {
        let Some(e) = self.directs.get_mut(&id) else { return };
        e.rec.rate_limit = limit.filter(|r| *r > 0);
        if let Some(t) = &e.task {
            t.set_rate_limit(e.rec.rate_limit);
        }
        let rec = e.rec.clone();
        self.dirty_direct(rec);
    }

    pub(super) fn forget_finished(&mut self, ids: Vec<TorrentId>) {
        for id in ids {
            if self.directs.get(&id).is_some_and(|e| e.rec.state == DirectState::Completed) {
                self.directs.remove(&id);
                self.dirty_direct_removed(id);
                self.sh.emit(EngineEvent::Removed { id });
            }
        }
    }

    /// Queue items of the running and waiting direct downloads: `(id, pos)`.
    pub(crate) fn direct_queue_items(&self) -> Vec<(TorrentId, u32)> {
        let mut v: Vec<(TorrentId, u32)> =
            self.directs.iter().filter(|(_, e)| e.wants_slot()).map(|(i, e)| (*i, e.rec.queue_pos)).collect();
        v.sort_unstable();
        v
    }

    /// Apply the queue's verdict for a direct download.
    pub(crate) fn direct_queue_verdict(&mut self, id: TorrentId, run: bool) {
        let Some(e) = self.directs.get_mut(&id) else { return };
        if e.queued != !run {
            e.queued = !run;
            let st = e.state();
            self.sh.emit(EngineEvent::StateChanged { id, state: st });
        }
        self.direct_reconcile(id);
    }

    /// Drop the queued flag of downloads that no longer want a slot.
    pub(crate) fn direct_queue_cleanup(&mut self) {
        let stale: Vec<TorrentId> =
            self.directs.iter().filter(|(_, e)| e.queued && !e.wants_slot()).map(|(i, _)| *i).collect();
        for id in stale {
            if let Some(e) = self.directs.get_mut(&id) {
                e.queued = false;
            }
        }
    }

    pub(super) fn on_direct(&mut self, m: DirectMsg) {
        match m {
            DirectMsg::Restarted(id, why) => self.sh.emit(EngineEvent::DirectRestarted { id, reason: why }),
            DirectMsg::Changed(id, p) => self.on_direct_changed(id, p),
            DirectMsg::Placed { id, path, category } => {
                if let Some(e) = self.directs.get_mut(&id) {
                    e.rec.final_path = Some(path);
                    if category.is_some() {
                        e.rec.category = category;
                    }
                    let rec = e.rec.clone();
                    self.dirty_direct(rec);
                }
            }
        }
    }

    fn on_direct_changed(&mut self, id: TorrentId, p: Progress) {
        let Some(e) = self.directs.get_mut(&id) else { return };
        if let Some(n) = p.file_name.clone().filter(|n| !n.is_empty())
            && e.rec.name != n {
                e.rec.name = n;
            }
        if p.total.is_some() {
            e.rec.total = p.total;
        }
        e.rec.done = p.done;
        match p.state {
            State::Completed if e.rec.state != DirectState::Completed => {
                e.rec.state = DirectState::Completed;
                e.rec.finished_at = Some(unix_secs());
                e.rec.final_path = p.final_path.clone();
                e.rec.done = e.rec.total.unwrap_or(p.done);
                let rec = e.rec.clone();
                let content_type = p.content_type.clone();
                self.dirty_direct(rec.clone());
                self.sh.emit(EngineEvent::Finished { id });
                self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::Completed });
                self.replan();
                self.on_direct_completed(rec, content_type);
                self.trim_finished();
            }
            State::Failed => {
                let msg = p.error.as_ref().map_or_else(|| "download failed".to_owned(), |e| e.to_string());
                if e.rec.state == DirectState::Active {
                    e.rec.state = DirectState::Failed(msg.clone());
                    let rec = e.rec.clone();
                    self.dirty_direct(rec);
                    self.sh.emit(EngineEvent::Error { id, message: msg });
                    self.replan();
                }
            }
            _ => {}
        }
    }

    /// A finished download: a `.torrent` becomes a torrent; anything else is
    /// moved into its category's finished folder, when it has one.
    fn on_direct_completed(&mut self, rec: DirectRecord, content_type: Option<String>) {
        let Some(path) = rec.final_path.clone() else { return };
        let ct = content_type.unwrap_or_default().to_ascii_lowercase();
        let is_torrent = path.extension().is_some_and(|x| x.eq_ignore_ascii_case("torrent")) || ct.contains("bittorrent");
        let sh = self.sh.clone();
        let id = rec.id;
        if is_torrent {
            tokio::spawn(async move {
                let h = crate::engine::EngineHandle::from_shared(sh.clone());
                let bytes = tokio::task::spawn_blocking({
                    let path = path.clone();
                    move || read_capped(&path)
                })
                .await
                .ok()
                .flatten();
                if let Some(bytes) = bytes {
                    match h.add_torrent_file(bytes, AddOptions::default()).await {
                        Ok(_) | Err(EngineError::Duplicate(_)) => {
                            let _ = tokio::fs::remove_file(&path).await;
                            h.forget_finished(&[id]);
                        }
                        Err(e) => sh.emit(EngineEvent::Error { id, message: format!("not a usable torrent: {e}") }),
                    }
                }
            });
            return;
        }
        self.direct_place(rec, None);
    }

    /// Put a finished direct download into its category's finished folder (SPEC 7A.4), when the
    /// category has one. `old` is the category it had before a change, so that a file in the old
    /// category's folder goes back to the download folder when the new category has no location of
    /// its own.
    fn direct_place(&mut self, rec: DirectRecord, old: Option<Category>) {
        let sh = self.sh.clone();
        let id = rec.id;
        tokio::task::spawn_blocking(move || {
            let cfg = sh.cfg();
            if let Some(new_path) = crate::organize::place_direct(&cfg, &rec, old) {
                let _ = sh.session.blocking_send(SessionMsg::Direct(DirectMsg::Placed { id, path: new_path, category: None }));
            }
        });
    }

    /// The category of a direct download changed (SPEC 7A.2: "this re-applies placement"): a
    /// finished one moves like a torrent's files would, an unfinished one is placed by its new
    /// category when it completes.
    pub(super) fn direct_set_category(&mut self, id: TorrentId, cat: Option<Category>) {
        let Some(e) = self.directs.get_mut(&id) else { return };
        let old = e.rec.category;
        if old == cat {
            return;
        }
        e.rec.category = cat;
        let rec = e.rec.clone();
        self.dirty_direct(rec.clone());
        if rec.state == DirectState::Completed && rec.final_path.is_some() {
            self.direct_place(rec, old);
        }
    }

    fn trim_finished(&mut self) {
        let mut done: Vec<(u64, TorrentId)> = self
            .directs
            .iter()
            .filter(|(_, e)| e.rec.state == DirectState::Completed)
            .map(|(i, e)| (e.rec.finished_at.unwrap_or(0), *i))
            .collect();
        if done.len() <= KEEP_FINISHED {
            return;
        }
        done.sort_unstable();
        let excess = done.len() - KEEP_FINISHED;
        for (_, id) in done.into_iter().take(excess) {
            self.directs.remove(&id);
            self.dirty_direct_removed(id);
        }
    }

    /// Persist progress of running downloads now and then (the `.part` sidecar is trs-http's own).
    pub(super) fn direct_tick(&mut self) {
        let mut dirty = Vec::new();
        let mut any = false;
        for e in self.directs.values_mut() {
            if e.rec.state != DirectState::Active {
                continue;
            }
            any = true;
            if let Some(t) = &e.task {
                let p = t.progress();
                e.rec.done = p.done;
                if p.total.is_some() {
                    e.rec.total = p.total;
                }
                if p.done.abs_diff(e.last_saved_done) >= (1 << 20) {
                    e.last_saved_done = p.done;
                    dirty.push(e.rec.clone());
                }
            }
        }
        for r in dirty {
            self.dirty_direct(r);
        }
        if any && !self.shutting_down {
            self.schedule_after(Duration::from_secs(10), Job::DirectTick);
        }
    }

    /// Pause every transfer losslessly and write the final records.
    pub(crate) fn shutdown_directs(&mut self) {
        let mut recs = Vec::new();
        for e in self.directs.values_mut() {
            if let Some(t) = &e.task {
                let p = t.progress();
                e.rec.done = p.done;
                if p.total.is_some() {
                    e.rec.total = p.total;
                }
                if !matches!(p.state, State::Completed | State::Failed | State::Cancelled) {
                    t.pause();
                }
            }
            recs.push(e.rec.clone());
        }
        for r in recs {
            self.dirty_direct(r);
        }
    }

    pub(crate) fn direct_rows(&self) -> impl Iterator<Item = TorrentRow> + '_ {
        self.directs.values().map(DirectEntry::row)
    }
}

fn read_capped(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let f = std::fs::File::open(path).ok()?;
    let mut buf = Vec::new();
    f.take(trs_state::store::MAX_TORRENT_FILE as u64 + 1).read_to_end(&mut buf).ok()?;
    (buf.len() <= trs_state::store::MAX_TORRENT_FILE).then_some(buf)
}
