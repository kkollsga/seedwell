//! Watch folders (SPEC 7A.1) without FSEvents and without unsafe code.
//!
//! One task ticks every 2 s. A tick costs one `stat` per folder: only when the folder's mtime
//! moved (or a candidate is still settling, or a minute passed as a safety net for coarse
//! network-volume timestamps) is the directory listed. A `.torrent` is picked up once its size
//! and mtime stayed the same for a full tick, which keeps half-downloaded browser files out.
//!
//! **Baseline.** Only files that *arrived after the folder joined the watch list* are looked at
//! (`WatchConfig::since`). Real users have old `.torrent` files in `~/Downloads`; adding them all
//! at first launch would flood the client and contact trackers (private ones too) for torrents
//! nobody asked for. A file's arrival time is the latest of its birth time, modification time
//! and status-change time, so a file copied or moved in later counts as new while one that has
//! been sitting there does not. The files before the baseline are the folder's *backlog*
//! ([`backlog`], `EngineHandle::watch_backlog`), which the app may offer to add once.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use tokio::task::JoinHandle;

use crate::api::{AddOptions, AddOrigin, AddRequest, AskReason, EngineError, EngineEvent};
use crate::config::{WatchConfig, WatchMode};
use crate::engine::EngineHandle;
use crate::organize;

/// Poll interval.
pub(crate) const TICK: Duration = Duration::from_secs(2);
/// Full re-list even if the directory mtime did not move.
const RELIST_EVERY: u32 = 30;
/// A folder's mtime is only trusted once it is this much older than the listing that recorded it.
/// Directory times come from a coarse kernel clock (4 to 10 ms ticks on Linux, whole seconds on some
/// file systems), so a file that lands in the same tick as the change the last listing saw leaves
/// the mtime unchanged and would not be noticed until the forced re-list.
#[cfg(not(test))]
const MTIME_GRAIN: Duration = Duration::from_secs(2);
#[cfg(test)]
const MTIME_GRAIN: Duration = Duration::from_millis(40);
/// A file's times come from the coarse kernel clock (up to one 10 ms tick behind real time on Linux)
/// while a folder's baseline is a fine-grained `SystemTime::now()`, so a file that lands right after
/// the folder joined the watch list can carry a time slightly *before* its baseline. A file is only
/// backlog when it is older than the baseline by more than this.
const BASELINE_SLACK: Duration = Duration::from_millis(15);
/// Files larger than this are not torrents we would add.
const MAX_FILE: u64 = trs_state::store::MAX_TORRENT_FILE as u64;
/// Most files remembered as "already looked at" per folder.
const MAX_SEEN: usize = 20_000;
/// Most candidates settling at once per folder.
const MAX_PENDING: usize = 512;

type Stamp = (u64, Option<SystemTime>);

struct Pending {
    stamp: Stamp,
    /// First tick at which the stamp was seen unchanged.
    stable_since: Option<Instant>,
}

#[derive(Default)]
struct FolderState {
    /// Files that arrived at or before this moment are backlog, not news (`None`: everything is news).
    since: Option<SystemTime>,
    dir_mtime: Option<SystemTime>,
    /// The last listing was made so soon after the folder's mtime that a later change may carry the
    /// same mtime: list again next tick.
    mtime_unsure: bool,
    ticks_since_list: u32,
    pending: HashMap<PathBuf, Pending>,
    seen: HashMap<PathBuf, Stamp>,
    /// Problems already reported (so a broken file is reported once).
    reported: HashMap<PathBuf, Stamp>,
}

/// A file ready to be looked at.
struct Ready {
    path: PathBuf,
    result: Result<Vec<u8>, String>,
}

fn stamp_of(m: &std::fs::Metadata) -> Stamp {
    (m.len(), m.modified().ok())
}

/// When a file arrived in its folder: the latest of birth, modification and status change time.
fn arrived_at(path: &Path, m: &std::fs::Metadata) -> SystemTime {
    [m.modified().ok(), m.created().ok(), trs_platform::fs::change_time(path, m)]
        .into_iter()
        .flatten()
        .max()
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The watch list with every baseline filled in. A folder handed over without one gets the moment
/// it first appears in this process (and keeps it while it stays in the list).
pub(crate) fn with_baselines(sh: &crate::shared::Shared, folders: &[WatchConfig]) -> Vec<WatchConfig> {
    let mut map = sh.watch_baselines.lock().unwrap_or_else(|e| e.into_inner());
    map.retain(|p, _| folders.iter().any(|f| f.since.is_none() && &f.path == p));
    let now = SystemTime::now();
    folders
        .iter()
        .map(|f| {
            let mut f = f.clone();
            if f.since.is_none() {
                f.since = Some(*map.entry(f.path.clone()).or_insert(now));
            }
            f
        })
        .collect()
}

/// The `.torrent` files in `dir` that were already there when the folder joined the watch list
/// (arrival time at or before `since`): the backlog. Sorted by name, at most 1,000.
pub(crate) fn backlog(dir: &Path, since: SystemTime) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_torrent_name(name) {
            continue;
        }
        let Ok(md) = entry.metadata() else { continue };
        if md.is_file() && arrived_at(&entry.path(), &md) + BASELINE_SLACK <= since {
            out.push(entry.path());
            if out.len() >= 1000 {
                break;
            }
        }
    }
    out.sort();
    out
}

fn is_torrent_name(name: &str) -> bool {
    !name.starts_with('.') && name.len() > 8 && name[name.len() - 8..].eq_ignore_ascii_case(".torrent")
}

/// Nothing to look at this tick: the folder's mtime did not move, nothing is settling and the
/// safety re-list is not due. One `stat`, cheap enough for the async task itself, which keeps
/// the blocking pool (and its thread start-up) out of an idle tick.
fn is_idle(dir: &Path, st: &mut FolderState) -> bool {
    let Ok(dm) = std::fs::metadata(dir) else {
        st.dir_mtime = None;
        return true;
    };
    let idle = dm.modified().ok() == st.dir_mtime
        && !st.mtime_unsure
        && st.pending.is_empty()
        && st.ticks_since_list + 1 < RELIST_EVERY;
    if idle {
        st.ticks_since_list += 1;
    }
    idle
}

/// One tick for one folder. Blocking (file-system calls); returns the files that settled.
fn scan(dir: &Path, st: &mut FolderState, now: Instant) -> Vec<Ready> {
    let Ok(dm) = std::fs::metadata(dir) else {
        // Folder missing or volume unplugged: nothing to do until it is back.
        st.dir_mtime = None;
        return Vec::new();
    };
    let mtime = dm.modified().ok();
    st.ticks_since_list += 1;
    let relist = mtime != st.dir_mtime || st.mtime_unsure || st.ticks_since_list >= RELIST_EVERY;
    if !relist && st.pending.is_empty() {
        return Vec::new();
    }
    if relist {
        st.dir_mtime = mtime;
        st.ticks_since_list = 0;
        let now_wall = SystemTime::now();
        // (An mtime far in the future is a clock problem, not a reason to list every tick.)
        st.mtime_unsure = mtime.is_some_and(|m| now_wall < m + MTIME_GRAIN && m < now_wall + Duration::from_secs(3600));
        let mut present: Vec<PathBuf> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let name = entry.file_name();
                let Some(name) = name.to_str() else { continue };
                if !is_torrent_name(name) {
                    continue;
                }
                let Ok(md) = entry.metadata() else { continue };
                if !md.is_file() {
                    continue;
                }
                let path = entry.path();
                let stamp = stamp_of(&md);
                present.push(path.clone());
                // Older than the folder's place on the watch list: backlog, never news.
                if st.since.is_some_and(|s| arrived_at(&path, &md) + BASELINE_SLACK <= s) {
                    continue;
                }
                if st.seen.get(&path) == Some(&stamp) {
                    continue;
                }
                if st.pending.len() >= MAX_PENDING && !st.pending.contains_key(&path) {
                    continue;
                }
                st.pending.entry(path).or_insert(Pending { stamp, stable_since: None });
            }
        }
        // Forget what vanished (moved to the Trash, deleted by the user).
        st.pending.retain(|p, _| present.contains(p));
        st.seen.retain(|p, _| present.contains(p));
        st.reported.retain(|p, _| present.contains(p));
    }
    let mut ready = Vec::new();
    let paths: Vec<PathBuf> = st.pending.keys().cloned().collect();
    for path in paths {
        let Ok(md) = std::fs::metadata(&path) else {
            st.pending.remove(&path);
            continue;
        };
        let cur = stamp_of(&md);
        let Some(p) = st.pending.get_mut(&path) else { continue };
        if p.stamp != cur {
            p.stamp = cur;
            p.stable_since = None;
            continue;
        }
        match p.stable_since {
            None => p.stable_since = Some(now),
            Some(t) if now.saturating_duration_since(t) >= Duration::from_secs(1) => {
                st.pending.remove(&path);
                if st.seen.len() >= MAX_SEEN {
                    st.seen.clear();
                }
                st.seen.insert(path.clone(), cur);
                let result = if cur.0 > MAX_FILE {
                    Err("the file is larger than 16 MiB, so it is not a torrent".to_owned())
                } else {
                    std::fs::read(&path).map_err(|e| e.to_string())
                };
                ready.push(Ready { path, result });
            }
            Some(_) => {}
        }
    }
    ready
}

/// Start the watcher task for `folders`.
pub(crate) fn spawn(h: EngineHandle, folders: Vec<WatchConfig>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut states: Vec<FolderState> =
            folders.iter().map(|f| FolderState { since: f.since, ..FolderState::default() }).collect();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            for (i, f) in folders.iter().enumerate() {
                let dir = f.path.clone();
                if is_idle(&dir, &mut states[i]) {
                    continue;
                }
                let mut st = std::mem::take(&mut states[i]);
                let joined = tokio::task::spawn_blocking(move || {
                    let r = scan(&dir, &mut st, Instant::now());
                    (st, r)
                })
                .await;
                let Ok((st, ready)) = joined else { continue };
                states[i] = st;
                for r in ready {
                    process(&h, f, r, &mut states[i]).await;
                }
            }
        }
    })
}

async fn process(h: &EngineHandle, folder: &WatchConfig, r: Ready, st: &mut FolderState) {
    let Ready { path, result } = r;
    let sh = &h.sh;
    let bytes = match result {
        Ok(b) => b,
        Err(message) => {
            sh.emit(EngineEvent::WatchError { path, message });
            return;
        }
    };
    let opts = AddOptions { category: folder.category, rule: folder.rule.clone(), ..AddOptions::default() };
    let prepared = match h.prepare(bytes, &opts, true, folder.mode == WatchMode::Ask).await {
        Ok(p) => p,
        Err(e) => {
            // Not a torrent we can use. Say so once per version of the file.
            let stamp = std::fs::metadata(&path).ok().map(|m| stamp_of(&m));
            if stamp.is_none() || st.reported.get(&path) != stamp.as_ref() {
                if let Some(s) = stamp {
                    st.reported.insert(path.clone(), s);
                }
                sh.emit(EngineEvent::WatchError { path, message: e.to_string() });
            }
            return;
        }
    };
    let cfg = sh.cfg();
    let uncertain = prepared.plan.uncertain && cfg.detect.is_some_and(|d| d.ask_when_unsure);
    let reason = match (folder.mode, uncertain) {
        (WatchMode::Ask, _) => Some(AskReason::FolderMode),
        (WatchMode::Auto, true) => Some(AskReason::LowConfidence),
        (WatchMode::Auto, false) => None,
    };
    if let Some(reason) = reason {
        // Hand the decision to the app: nothing is added and the file stays where it is.
        let bytes = prepared.bytes.clone();
        let dup = h.exists(prepared.id).await;
        let preview = tokio::task::spawn_blocking(move || {
            trs_metainfo::Metainfo::from_bytes(&bytes).ok().map(|mi| (prepared, mi))
        })
        .await
        .ok()
        .flatten();
        if let Some((p, mi)) = preview {
            let preview = organize::preview(&p, &mi, dup);
            sh.emit(EngineEvent::AddRequest(Box::new(AddRequest {
                path,
                reason,
                preview,
                forced_category: folder.category,
                forced_rule: folder.rule.clone(),
            })));
        }
        return;
    }
    match h.add_prepared(prepared, opts, AddOrigin::Watch(path.clone())).await {
        Ok(added) => {
            let (action, trash) = (folder.after_add.clone(), cfg.trash_dir.clone());
            let p = path.clone();
            match tokio::task::spawn_blocking(move || organize::finish_file(&p, &action, trash.as_deref())).await {
                Ok(Err(e)) => {
                    sh.emit(EngineEvent::WatchError { path, message: format!("added, but the .torrent could not be moved: {e}") });
                }
                // Held (grace period or a choice): remember the file that is left, for Cancel (SPEC 7A.6).
                Ok(Ok(Some(left))) if added.needs_choice || added.pending.is_some() => sh.hold_source(added.id, left),
                _ => {}
            }
        }
        // The session already announced the duplicate; the file stays put.
        Err(EngineError::Duplicate(_) | EngineError::TrackersMerged { .. }) => {}
        Err(e) => sh.emit(EngineEvent::WatchError { path, message: e.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_visible_torrent_files_count() {
        assert!(is_torrent_name("a.torrent"));
        assert!(is_torrent_name("A.TORRENT"));
        assert!(!is_torrent_name(".hidden.torrent"));
        assert!(!is_torrent_name("a.torrent.crdownload"));
        assert!(!is_torrent_name(".torrent"));
        assert!(!is_torrent_name("x.download"));
    }

    #[test]
    fn a_file_is_ready_only_after_its_size_settled() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.torrent");
        std::fs::write(&f, b"d4:infod4:name1:aee").unwrap();
        let mut st = FolderState::default();
        let t0 = Instant::now();
        assert!(scan(d.path(), &mut st, t0).is_empty(), "first sight");
        assert!(scan(d.path(), &mut st, t0 + Duration::from_millis(500)).is_empty(), "not stable for 1 s yet");
        // The file grows (a download in progress): the clock restarts.
        std::fs::write(&f, b"d4:infod4:name1:aee0000").unwrap();
        assert!(scan(d.path(), &mut st, t0 + Duration::from_secs(2)).is_empty());
        let r = scan(d.path(), &mut st, t0 + Duration::from_secs(4));
        assert!(r.is_empty(), "stable since t0+2 s only: {}", r.len());
        let r = scan(d.path(), &mut st, t0 + Duration::from_secs(6));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].result.as_ref().unwrap().len(), 23);
        // Seen once, not reported again.
        assert!(scan(d.path(), &mut st, t0 + Duration::from_secs(8)).is_empty());
    }

    #[test]
    fn an_idle_tick_is_decided_without_listing() {
        let d = tempfile::tempdir().unwrap();
        let mut st = FolderState::default();
        scan(d.path(), &mut st, Instant::now());
        // The folder was just created: its mtime is within one clock tick of the listing and
        // cannot be trusted yet.
        assert!(!is_idle(d.path(), &mut st));
        std::thread::sleep(MTIME_GRAIN * 2);
        scan(d.path(), &mut st, Instant::now());
        assert!(is_idle(d.path(), &mut st), "nothing changed");
        std::fs::write(d.path().join("x.torrent"), b"x").unwrap();
        // The directory's mtime moved: work to do.
        assert!(!is_idle(d.path(), &mut st));
        // A vanished folder is idle (and forgotten, so it is re-read when it is back).
        assert!(is_idle(&d.path().join("gone"), &mut st));
    }

    /// A file that lands in the same coarse clock tick as the change the last listing saw leaves the
    /// directory's mtime unchanged (simulated by putting the old mtime back): it must be found at
    /// the next tick, not at the forced re-list a minute later.
    #[test]
    fn a_file_in_the_same_clock_tick_is_found() {
        let d = tempfile::tempdir().unwrap();
        let mut st = FolderState::default();
        let t0 = Instant::now();
        scan(d.path(), &mut st, t0);
        let before = std::fs::metadata(d.path()).unwrap().modified().unwrap();
        std::fs::write(d.path().join("late.torrent"), b"d4:infod4:name1:aee").unwrap();
        trs_platform::fs::set_modified(d.path(), before).unwrap();
        assert_eq!(std::fs::metadata(d.path()).unwrap().modified().unwrap(), before, "the coarse tick");
        assert!(!is_idle(d.path(), &mut st), "an mtime this fresh is not trusted");
        scan(d.path(), &mut st, t0 + Duration::from_secs(2));
        assert!(st.pending.keys().any(|p| p.ends_with("late.torrent")), "found without waiting for the forced re-list");
    }

    #[test]
    fn an_idle_folder_costs_one_stat() {
        let d = tempfile::tempdir().unwrap();
        let mut st = FolderState::default();
        let t0 = Instant::now();
        scan(d.path(), &mut st, t0);
        assert!(st.pending.is_empty());
        // Once the folder's mtime is older than the clock grain, a tick with nothing changed
        // returns without listing (pending stays empty, counter grows).
        std::thread::sleep(MTIME_GRAIN * 2);
        scan(d.path(), &mut st, t0);
        scan(d.path(), &mut st, t0);
        assert_eq!(st.ticks_since_list, 1);
    }
}
