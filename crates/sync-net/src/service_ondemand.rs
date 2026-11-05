//! On-demand folders in the service (SPEC §7G.11): what a host (File Provider on the Mac, Cloud Files on Windows) and
//! the CLI call. Item queries read the index through the read-only handle, so a host listing a folder never waits for
//! a pull that holds the folder's core; changes go through the core like a scan's.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync::folder::{FolderCore, HostError, RENAME_NEEDS_HOLDER, Requested};
use trs_sync::fs::Fs;
use trs_sync::index::IndexStore;
use trs_sync::ondemand::{ChangePage, Deleted, Evicted, Item, Page};
use trs_sync_proto::ids::{FolderId, ShortId};
use trs_sync_proto::model::STATE_DIR;
use trs_sync_proto::msg::{HeldFile, MAX_HOLD_FILES};

use super::{OnDemandError, SyncError, SyncHandle, invalid, lock};

/// How long a rename another device made for this one may take to arrive here.
const RENAME_ARRIVAL: Duration = Duration::from_secs(60);
/// How long past its time to live a rename request's answer may take (the rename itself, and the way back).
const RENAME_ANSWER_MARGIN: Duration = Duration::from_secs(10);
/// How long a rename whose outcome is not known is watched for, so the pins follow it if it arrives.
const RENAME_LATE_WATCH: Duration = Duration::from_secs(600);
use crate::shared::{FetchWatch, FolderShared, Freeing};
use crate::status::FolderStatus;

/// What a fetch brought: where the content is in the cache (the host copies it into place), and, for a folder, its
/// files no device can give (not waited for).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// The content in the cache.
    pub path: PathBuf,
    /// Files below a folder no device can give right now.
    pub unavailable: Vec<String>,
}

/// What a fetch that waited found ([`SyncHandle::od_fetch_wait`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FetchWait {
    /// It is here.
    Here(Fetched),
    /// Still on its way.
    Coming {
        /// The connected devices it comes from (their names; none while devices may still be coming up, or while its
        /// request is still on its way to the folder's busy core: a pull round holds it, and on a slow disk the
        /// request's own commit can outlast a short wait).
        from: Vec<String>,
        /// A folder's files no device can give right now (not waited for).
        unavailable: Vec<String>,
    },
}

/// How long sessions may take to come up (after the service starts, the network changes or a session begins) before a
/// fetch whose content no connected device has gives up (SPEC §7G.11).
pub const FETCH_GRACE: Duration = Duration::from_secs(3);
/// How long a fetch may get none of the content from the connected devices that have it before it gives up.
pub const FETCH_STALL: Duration = Duration::from_secs(30);
/// A fetch nobody waited on for this long counts its [`FETCH_STALL`] anew when it is waited on again.
const FETCH_FORGOTTEN: Duration = Duration::from_secs(2);

/// Where a fetch stands ([`SyncHandle::fetch_state`]).
#[derive(Default)]
struct FetchState {
    /// Everything is here (but the files no device can give).
    here: bool,
    /// Something not here is still asked for.
    wanted: bool,
    /// Files here (below a folder): one more counts as getting further.
    here_count: u64,
    /// The devices that hold what is asked for and is not here.
    holders: std::collections::BTreeSet<ShortId>,
    /// Files no device can give right now.
    unavailable: Vec<String>,
}

/// How far a fetch of one file is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FetchProgress {
    /// Bytes on this device so far.
    pub done: u64,
    /// The file's size.
    pub total: u64,
    /// The content is here (the fetch is over).
    pub here: bool,
    /// Asked for and on its way (false: online only, not asked for, or here).
    pub fetching: bool,
}

fn host_err(e: HostError) -> SyncError {
    match e {
        HostError::Io(e) => SyncError::Io(e),
        HostError::NotOnDemand => super::on_demand(OnDemandError::NotOnDemand, e.to_string()),
        HostError::NotFound(_) => super::on_demand(OnDemandError::NoItem, e.to_string()),
        HostError::Changed(_) => super::on_demand(OnDemandError::Changed, e.to_string()),
        HostError::Ignored(_) => super::on_demand(OnDemandError::Ignored, e.to_string()),
        HostError::IgnoredNotHere(_) => super::on_demand(OnDemandError::IgnoredNotHere, e.to_string()),
        other => invalid(other.to_string()),
    }
}

/// The same error again (for a second caller): an I/O error keeps its kind and words.
fn copy_err(e: &SyncError) -> SyncError {
    match e {
        SyncError::NotFound => SyncError::NotFound,
        SyncError::OnDemand(k, why) => SyncError::OnDemand(*k, why.clone()),
        SyncError::Invalid(why) => SyncError::Invalid(why.clone()),
        SyncError::Io(e) => SyncError::Io(io::Error::new(e.kind(), e.to_string())),
    }
}

/// The newest sequence of the records at `path` and below it (deleted ones too); with `but`, of the versions another
/// device made (this device's own writes and deletes left out).
fn newest_below(store: &dyn IndexStore, path: &str, but: Option<trs_sync_proto::ids::ShortId>) -> io::Result<u64> {
    let mut newest = 0;
    let mut after: Option<String> = None;
    loop {
        let page = store.local_paths(path, after.as_deref(), 1000)?;
        let Some(last) = page.last().cloned() else { return Ok(newest) };
        for p in &page {
            let inside = path.is_empty() || p == path || p.strip_prefix(path).is_some_and(|r| r.starts_with('/'));
            if inside
                && let Some(r) = store.local(p)?
                && Some(r.file.info.modified_by) != but
            {
                newest = newest.max(r.sequence);
            }
        }
        after = Some(last);
    }
}

/// A path in the folder as the host names it (wire form: `/` between components, NFC).
fn check(path: &str) -> Result<(), SyncError> {
    if path.is_empty() {
        return Ok(());
    }
    trs_sync_proto::model::validate_path(path).map_err(|_| invalid(format!("{path:?} is not a path in the folder")))?;
    trs_sync::path::check_wire_path(path).map_err(|_| invalid(format!("{path:?} is not a path in the folder")))
}

fn abs(root: &Path, path: &str) -> PathBuf {
    path.split('/').filter(|c| !c.is_empty()).fold(root.to_path_buf(), |p, c| p.join(c))
}

impl SyncHandle {
    fn on_demand_folder(&self, id: &FolderId) -> Result<Arc<FolderShared>, SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        if !f.on_demand {
            return Err(super::on_demand(OnDemandError::NotOnDemand, "the folder is not on demand"));
        }
        Ok(f)
    }

    /// Runs `op` on the folder's core (on a blocking thread), then tells the sessions of new local changes.
    pub(crate) async fn with_core<T: Send + 'static>(
        &self,
        f: &Arc<FolderShared>,
        op: impl FnOnce(&mut FolderCore) -> Result<T, HostError> + Send + 'static,
    ) -> Result<T, SyncError> {
        let f2 = f.clone();
        let (r, seq) = tokio::task::spawn_blocking(move || {
            let mut core = lock(&f2.core);
            let r = op(&mut core);
            (r, core.index_state().map(|(_, s)| s).unwrap_or(0))
        })
        .await
        .map_err(io::Error::other)?;
        f.local_seq.send_if_modified(|s| std::mem::replace(s, seq) != seq);
        r.map_err(host_err)
    }

    /// The files of a folder no device can give right now, with their sizes (SPEC §7G.11; any folder, full or on demand).
    ///
    /// Read from the index's read-only handle (never waits for the folder's core). A folder that is not running (held
    /// back) has none to say.
    pub fn unavailable(&self, id: &FolderId) -> Result<Vec<(String, u64)>, SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        Ok(f.index.view().unavailable()?)
    }

    /// The item with this identifier (`root` is the folder), or `None`.
    pub fn od_item(&self, id: &FolderId, item: &str) -> Result<Option<Item>, SyncError> {
        let f = self.on_demand_folder(id)?;
        let pins = lock(&f.pins).clone();
        Ok(lock(&f.items).item(&f.index.view(), &pins, item)?)
    }

    /// The item at `path` (`""` is the folder), or `None`.
    pub fn od_item_at(&self, id: &FolderId, path: &str) -> Result<Option<Item>, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        let pins = lock(&f.pins).clone();
        Ok(lock(&f.items).item_at(&f.index.view(), &pins, path)?)
    }

    /// A page of the children of the directory item `item`, after `cursor` (the page before's `next`).
    pub fn od_children(&self, id: &FolderId, item: &str, cursor: Option<&str>, limit: usize) -> Result<Option<Page>, SyncError> {
        let f = self.on_demand_folder(id)?;
        let pins = lock(&f.pins).clone();
        Ok(lock(&f.items).children(&f.index.view(), &pins, item, cursor, limit.min(1000))?)
    }

    /// What changed since `anchor`.
    pub fn od_changes(&self, id: &FolderId, anchor: u64, limit: usize) -> Result<ChangePage, SyncError> {
        let f = self.on_demand_folder(id)?;
        let pins = lock(&f.pins).clone();
        lock(&f.items).changes(&f.index.view(), &pins, anchor, limit.min(1000)).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound { super::on_demand(OnDemandError::AnchorExpired, format!("anchor expired: {e}")) } else { SyncError::Io(e) }
        })
    }

    /// The anchor for "now": [`Self::od_changes`] from it reports only what comes after.
    pub fn od_anchor(&self, id: &FolderId) -> Result<u64, SyncError> {
        let f = self.on_demand_folder(id)?;
        let m = f.index.view().meta()?;
        Ok(trs_sync::ondemand::anchor_of(m.index_id, m.sequence))
    }

    /// Where the content of `path` is on this device (in the cache), whether it is there or not.
    pub fn od_cache_path(&self, id: &FolderId, path: &str) -> Result<PathBuf, SyncError> {
        check(path)?;
        Ok(abs(&self.on_demand_folder(id)?.root, path))
    }

    /// Asks for the content of `path` (a file, or every file below a folder) without waiting for it.
    pub async fn od_request(&self, id: &FolderId, path: &str) -> Result<Requested, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        // Kept in place, and asked for again: its placeholder lost the content behind the core's back.
        self.od_lost_in_place(&f, path).await?;
        let p = path.to_string();
        let r = self.with_core(&f, move |c| c.request(&p)).await?;
        if matches!(r, Requested::Fetching(_)) {
            f.remote_changed.notify_one();
        }
        Ok(r)
    }

    /// Takes a request back (the host cancelled the fetch); what is pinned stays wanted.
    pub async fn od_cancel(&self, id: &FolderId, path: &str) -> Result<(), SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        // A request still on its way to the core is not made (the core may run it after this cancel).
        lock(&f.fetches).requesting.remove(path);
        let p = path.to_string();
        self.with_core(&f, move |c| c.cancel_request(&p)).await?;
        Ok(())
    }

    /// Fetches `path` (a file, or every file below a folder) and waits until it is here, at most `timeout`. Returns
    /// where it is in the cache: the host copies it into place (a clone on APFS costs nothing).
    /// A folder's files no device can give are not waited for: they are reported (`unavailable`). Still on its way when
    /// `timeout` ends, or given up ([`Self::od_fetch_wait`]): [`OnDemandError::NotHereYet`].
    pub async fn od_fetch(&self, id: &FolderId, path: &str, timeout: Duration) -> Result<Fetched, SyncError> {
        match self.od_fetch_wait(id, path, timeout).await? {
            FetchWait::Here(f) => Ok(f),
            FetchWait::Coming { .. } => Err(super::on_demand(OnDemandError::NotHereYet, format!("{path}: still on its way"))),
        }
    }

    /// Fetches `path` (a file, or every file below a folder) and waits for it at most `wait` (SPEC §7G.11): here
    /// ([`FetchWait::Here`]), or still coming from the connected devices that have it ([`FetchWait::Coming`]: ask again
    /// to wait on). The caller keeps no timer: the fetch gives up itself, with [`OnDemandError::NotHereYet`], and takes
    /// its request back (what is pinned stays wanted), as soon as
    /// - no connected device has the content and none may be coming up: past [`FETCH_GRACE`] after the service
    ///   started, the network changed or the last session began (sessions come up, link their folders, send their
    ///   index);
    /// - or the devices asked sent none of it for [`FETCH_STALL`] while it was waited on.
    ///
    /// A version no device can give at all is [`OnDemandError::Unavailable`] at once.
    ///
    /// Returns within about `wait` whatever the folder's core is doing: the core is needed only to make the request,
    /// and a pull round holds it while it downloads (a whole window of files, minutes on a slow link). A request that
    /// stands already (every file not here is wanted) is not made again; one that has to be made is waited for until
    /// `wait` ends and then left to go in by itself ([`FetchWait::Coming`]; the next call waits on it without giving
    /// up). How far the file is meanwhile: [`Self::od_progress`].
    pub async fn od_fetch_wait(&self, id: &FolderId, path: &str, wait: Duration) -> Result<FetchWait, SyncError> {
        let end = Instant::now() + wait;
        // A version no device can give is not waited for.
        if self.od_item_at(id, path)?.is_some_and(|i| i.state == trs_sync::ondemand::ItemState::Unavailable) {
            return Err(super::on_demand(OnDemandError::Unavailable, format!("{path}: {}", trs_sync::ondemand::UNAVAILABLE)));
        }
        let f = self.on_demand_folder(id)?;
        let (ask, requesting, stands) = {
            let mut x = lock(&f.fetches);
            x.asked += 1;
            let n = x.asked;
            x.asks.insert(path.to_string(), n);
            let requesting = x.requesting.contains_key(path);
            if !requesting {
                // (A failure nobody waited on: this call makes the request again and finds out itself.)
                x.request_failed.remove(path);
            }
            // Read with this ask noted, under the same lock a give-up's take-back holds while it takes the request back:
            // a request seen standing here is never taken back for an earlier ask after all.
            let stands = requesting || self.request_stands(&f, path)?;
            (n, requesting, stands)
        };
        if !requesting && !stands {
            lock(&f.fetches).requesting.insert(path.to_string(), ask);
            let (h, f2, p) = (self.clone(), f.clone(), path.to_string());
            let made = tokio::spawn(async move {
                let r = h.od_request_for(&f2, &p, ask).await;
                let mut x = lock(&f2.fetches);
                if x.requesting.get(&p) == Some(&ask) {
                    x.requesting.remove(&p);
                    // For a later call waiting on it, should this one have answered already.
                    if let Err(e) = &r {
                        x.request_failed.insert(p.clone(), copy_err(e));
                    }
                }
                r
            });
            match tokio::time::timeout_at(end.into(), made).await {
                Ok(r) => match r.map_err(io::Error::other)?.inspect_err(|_| {
                    lock(&f.fetches).request_failed.remove(path);
                })? {
                    Some(Requested::Here) => {
                        Self::fetch_over(&f, path);
                        return Ok(FetchWait::Here(Fetched { path: abs(&f.root, path), unavailable: Vec::new() }));
                    }
                    Some(Requested::Fetching(_)) => {}
                    None => return Err(invalid(format!("{path}: the fetch was cancelled"))),
                },
                // The core is busy (a pull round): the request goes in when it is free.
                Err(_) => return Ok(FetchWait::Coming { from: Vec::new(), unavailable: Vec::new() }),
            }
        }
        loop {
            let st = self.fetch_state(&f, path)?;
            // A file that became one no device can give meanwhile says so at once.
            if st.unavailable.iter().any(|p| p == path) {
                Self::fetch_over(&f, path);
                return Err(super::on_demand(OnDemandError::Unavailable, format!("{path}: {}", trs_sync::ondemand::UNAVAILABLE)));
            }
            if st.here {
                Self::fetch_over(&f, path);
                return Ok(FetchWait::Here(Fetched { path: abs(&f.root, path), unavailable: st.unavailable }));
            }
            // Its request is still on its way to the core (an earlier call made it): waited for, never given up on; and
            // one that failed says why.
            let pending = {
                let mut x = lock(&f.fetches);
                let pending = x.requesting.contains_key(path);
                if !pending && let Some(e) = x.request_failed.remove(path) {
                    return Err(e);
                }
                pending
            };
            if pending {
                if Instant::now() >= end {
                    return Ok(FetchWait::Coming { from: Vec::new(), unavailable: st.unavailable });
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            // Who can give it: the connected devices that hold it and share the folder on their session.
            let asked: Vec<Arc<crate::shared::PeerHandle>> =
                self.inner.ctx.registry.peers().into_iter().filter(|p| st.holders.contains(&p.device.short()) && p.access(&f.id).is_some()).collect();
            let data: u64 = asked.iter().map(|p| p.rx_bytes.load(std::sync::atomic::Ordering::Relaxed)).sum();
            let now = Instant::now();
            let stalled = {
                let stall = *lock(&self.inner.ctx.fetch_stall);
                let mark = (data, st.here_count);
                let mut w = lock(&f.fetches);
                let e = w.watch.entry(path.to_string()).or_insert(FetchWatch { mark, since: now, seen: now });
                // Nobody waited on it for a while (a fetch let go of, a host that came back later): counted anew.
                if now.duration_since(e.seen) > FETCH_FORGOTTEN || e.mark != mark {
                    *e = FetchWatch { mark, since: now, seen: now };
                }
                e.seen = now;
                !asked.is_empty() && now.duration_since(e.since) >= stall
            };
            let settling = now < *lock(&self.inner.ctx.settling);
            let why = if asked.is_empty() && !settling {
                Some("no device that has it is connected")
            } else if stalled {
                Some("the devices that have it sent none of it for a while")
            } else {
                None
            };
            if let Some(why) = why {
                // Taken back, so nothing is fetched for nobody; off the answer's way (the folder's core may be busy), and
                // not if the path was asked for again meanwhile.
                let (h, f2, p) = (self.clone(), f.clone(), path.to_string());
                tokio::spawn(async move {
                    let f3 = f2.clone();
                    let _ = h
                        .with_core(&f2, move |c| {
                            // Held while it is taken back: a fetch asked meanwhile sees it either standing (and this
                            // leaves it) or gone (and makes it again), never standing and then gone.
                            let x = lock(&f3.fetches);
                            if x.asks.get(&p) == Some(&ask) {
                                c.cancel_request(&p)?;
                            }
                            Ok(())
                        })
                        .await;
                });
                return Err(super::on_demand(OnDemandError::NotHereYet, format!("{path}: not here yet ({why})")));
            }
            if !st.wanted {
                Self::fetch_over(&f, path);
                return Err(invalid(format!("{path}: the fetch was cancelled")));
            }
            if now >= end {
                let mut from: Vec<String> = asked.iter().map(|p| p.name.clone()).collect();
                from.sort();
                from.dedup();
                return Ok(FetchWait::Coming { from, unavailable: st.unavailable });
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Makes the request ask number `ask` of [`Self::od_fetch_wait`] stands for, unless a cancel took it back
    /// meanwhile (`None`).
    async fn od_request_for(&self, f: &Arc<FolderShared>, path: &str, ask: u64) -> Result<Option<Requested>, SyncError> {
        // Kept in place, and asked for again: its placeholder lost the content behind the core's back.
        self.od_lost_in_place(f, path).await?;
        let (f2, p) = (f.clone(), path.to_string());
        let r = self
            .with_core(f, move |c| {
                if lock(&f2.fetches).requesting.get(&p) != Some(&ask) {
                    return Ok(None);
                }
                c.request(&p).map(Some)
            })
            .await?;
        if matches!(r, Some(Requested::Fetching(_))) {
            f.remote_changed.notify_one();
        }
        Ok(r)
    }

    /// The request for `path` stands already (read from the index, never waiting for the core): every file at or
    /// below it that is not here is wanted, and a file kept in place still has its content in its placeholder. False
    /// also for a path with no item (the request says why).
    fn request_stands(&self, f: &FolderShared, path: &str) -> Result<bool, SyncError> {
        if let Some(placed) = &f.placed
            && let Some(n) = placed.held(path)
            && placed.locate(path, &n)?.is_none()
        {
            return Ok(false);
        }
        let view = f.index.view();
        if !view.local(path)?.is_some_and(|r| trs_sync::ondemand::live(&r)) {
            return Ok(false);
        }
        let mut after: Option<String> = None;
        loop {
            let page = view.local_paths(path, after.as_deref(), 1000)?;
            let Some(last) = page.last().cloned() else { return Ok(true) };
            for p in &page {
                if view.local(p)?.is_some_and(|r| trs_sync::ondemand::live(&r) && r.dataless && !r.want) {
                    return Ok(false);
                }
            }
            after = Some(last);
        }
    }

    /// The fetch of `path` is over (here, or not to be waited for).
    fn fetch_over(f: &FolderShared, path: &str) {
        let mut x = lock(&f.fetches);
        x.watch.remove(path);
        x.asks.remove(path);
    }

    /// Where a fetch of `path` (a file, or every file below a folder) stands: a file no device can give counts as
    /// neither here nor waited for, and is listed.
    fn fetch_state(&self, f: &FolderShared, path: &str) -> io::Result<FetchState> {
        let view = f.index.view();
        let mut st = FetchState { here: true, ..FetchState::default() };
        let mut after: Option<String> = None;
        loop {
            let page = view.local_paths(path, after.as_deref(), 1000)?;
            let Some(last) = page.last().cloned() else { break };
            for p in &page {
                let Some(r) = view.local(p)?.filter(|r| !r.file.info.deleted) else { continue };
                if !r.dataless {
                    st.here_count += u64::from(r.file.info.kind == trs_sync_proto::model::FileKind::File);
                    continue;
                }
                if view.unavailable_at(p)?.is_some() {
                    st.unavailable.push(p.clone());
                    continue;
                }
                st.here = false;
                st.wanted |= r.want;
                if r.want
                    && let Some(n) = view.need(p)?
                {
                    st.holders.extend(n.holders);
                }
            }
            after = Some(last);
        }
        Ok(st)
    }

    /// How far the fetch of the file at `path` is (its partial copy in the cache). Never goes back while it comes: once
    /// whole, the copy is renamed into place and its record says so only when the pull's round is committed (a flush,
    /// long on a slow disk: Windows' CI showed the 0 of a copy that was gone); meanwhile the file in place, of this
    /// version's size, counts as whole (`done == total`, not `here` yet).
    pub fn od_progress(&self, id: &FolderId, path: &str) -> Result<FetchProgress, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        let Some(r) = f.index.local(path)? else { return Err(super::on_demand(OnDemandError::NoItem, format!("{path}: no such item in the folder"))) };
        let total = r.file.info.size;
        if !r.dataless {
            return Ok(FetchProgress { done: total, total, here: true, fetching: false });
        }
        // (A stream of a file not hashed yet builds a temp file of its own.)
        let built = [trs_sync::pull::temp_path(path), trs_sync::pull::stream_temp_path(path)]
            .iter()
            .filter_map(|t| std::fs::metadata(abs(&f.root, t)).ok())
            .map(|m| m.len())
            .max();
        let done = match built {
            Some(n) => n.min(total),
            None if std::fs::metadata(abs(&f.root, path)).is_ok_and(|m| m.is_file() && m.len() == total) => total,
            None => 0,
        };
        Ok(FetchProgress { done, total, here: false, fetching: r.want })
    }

    /// Keeps `path` on this device (`on`), or no longer; saved with the folder's settings.
    pub async fn od_pin(&self, id: &FolderId, path: &str, on: bool) -> Result<(), SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        let p = path.to_string();
        let pins = self
            .with_core(&f, move |c| {
                if on { c.pin(&p) } else { c.unpin(&p) }?;
                Ok(c.pins().clone())
            })
            .await?;
        self.save_pins(id, &f, pins)?;
        f.remote_changed.notify_one();
        Ok(())
    }

    /// The core's pins, now: kept for item queries and saved with the folder's settings (when they changed).
    fn save_pins(&self, id: &FolderId, f: &FolderShared, pins: std::collections::BTreeSet<String>) -> Result<(), SyncError> {
        if *lock(&f.pins) == pins {
            return Ok(());
        }
        *lock(&f.pins) = pins.clone();
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.pins = pins.into_iter().collect();
        }
        Ok(self.save_config()?)
    }

    /// Removes the content of `path` from this device (files stay, online only); files pinned themselves and the only
    /// copies are kept. A pin of a folder above it is split (saved with the folder's settings).
    pub async fn od_evict(&self, id: &FolderId, path: &str) -> Result<Evicted, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        // (While `freeing` lives, this device says it does not hold the files it is letting go of: it goes with the
        // blocking work, so it ends when the free does, also if this caller is dropped meanwhile.)
        let (confirmed, freeing) = self.confirmed_held(&f, path, false).await?;
        let p = path.to_string();
        // The pins are saved whatever came of it (a split may have happened before an error).
        let (e, pins) = self
            .with_core(&f, move |c| {
                let _freeing = freeing;
                Ok((c.evict(&p, &confirmed), c.pins().clone()))
            })
            .await?;
        self.save_pins(id, &f, pins)?;
        e.map_err(host_err)
    }

    /// What may be freed at `path` (SPEC §7G.11): the files another device, asked right now over its session, says it
    /// holds. Each device that announces a file held is asked (one query per device, [`MAX_HOLD_FILES`] files at
    /// most); a device not connected, one that does not answer in time (an earlier Seedwell ignores the question) or
    /// says no confirms nothing, and the file stays.
    ///
    /// From before the first question until the returned [`Freeing`] is dropped (after the free), this device answers
    /// no for those files itself.
    async fn confirmed_held(&self, f: &Arc<FolderShared>, path: &str, unpinning: bool) -> Result<(Vec<HeldFile>, Freeing), SyncError> {
        let p = path.to_string();
        let candidates = self
            .with_core(f, move |c| {
                let (files, _) = c.free_candidates(&p, unpinning)?;
                let mut out = Vec::with_capacity(files.len());
                for i in files {
                    let holders = c.holders_of(&i)?;
                    out.push((i, holders));
                }
                Ok(out)
            })
            .await?;
        let freeing = Freeing::new(f.clone(), candidates.iter().map(|(i, _)| i.path.clone()).collect());
        let mut by_peer: BTreeMap<ShortId, Vec<usize>> = BTreeMap::new();
        for (n, (_, holders)) in candidates.iter().enumerate() {
            for d in holders {
                by_peer.entry(*d).or_default().push(n);
            }
        }
        let peers = self.inner.ctx.registry.peers();
        let mut held = vec![false; candidates.len()];
        let mut asks = Vec::new();
        for (d, files) in by_peer {
            // Only a writer's answer counts: a read-only member's change is never sent, so its copy is no copy to rely on.
            // Only a device that answers is asked (an earlier build never does).
            let Some(peer) = peers.iter().find(|p| {
                p.device.short() == d && p.access(&f.id).is_some() && f.writes(&p.device_pk) && p.has(trs_sync_proto::msg::FEATURE_HOLDS)
            }) else {
                continue;
            };
            for part in files.chunks(MAX_HOLD_FILES) {
                let q = part.iter().map(|&n| candidates[n].0.clone()).collect();
                if let Some((id, rx)) = peer.ask_holds(f.id, q).await {
                    asks.push((part.to_vec(), peer.clone(), id, rx));
                }
            }
        }
        // Every question is out: the answers are waited for together, at most this long in all.
        let end = Instant::now() + *lock(&self.inner.ctx.hold_wait);
        for (part, peer, id, rx) in asks {
            match tokio::time::timeout_at(end.into(), rx).await {
                // (Its role may have changed while it answered.)
                Ok(Ok(answer)) if f.writes(&peer.device_pk) => {
                    for (n, yes) in part.into_iter().zip(answer) {
                        held[n] |= yes;
                    }
                }
                // No answer in time (an earlier Seedwell), or the session ended: nothing confirmed.
                _ => peer.forget_holds(id),
            }
        }
        Ok((candidates.into_iter().zip(held).filter(|(_, h)| *h).map(|((i, _), _)| i).collect(), freeing))
    }

    /// The user's new or changed file at `path`, whose content the host has at `from`: copied into the cache
    /// (written aside, then renamed), with any missing folders above it, and recorded as this device's version.
    ///
    /// `base` is the item's `content_version` the user's edit started from, `None` for a new file. When the item moved on since (another device's version came first), or a new file's path is taken now,
    /// the content becomes a conflict copy named as a full folder names one, and the version there stays: one copy for
    /// one content, so the same content reported again is the copy made already (its path returned, nothing written),
    /// and the item's current content is no conflict (`path`, no new version). An unhashed version (SPEC §7G.11) holds
    /// that content when the file this device has its bytes in is unchanged since it was recorded and holds the same
    /// bytes (or is `from` itself): so this device's own edit, recorded by its scan before the host reported it, is the
    /// version at the name, and a copy recorded unhashed is that copy. Returns the path written. Kept in place,
    /// a new file that is the item's own placeholder there, filled with its content and unchanged since (the host adopting it a second time), is that item: nothing is written, and its path returned.
    pub async fn od_write(&self, id: &FolderId, path: &str, from: &Path, base: Option<&str>) -> Result<String, SyncError> {
        check(path)?;
        if path.is_empty() {
            return Err(invalid("the folder itself is not a file"));
        }
        let f = self.on_demand_folder(id)?;
        let (root, p, from, base) = (f.root.clone(), path.to_string(), from.to_path_buf(), base.map(str::to_string));
        let device = lock(&self.inner.ctx.device_name).clone();
        let placed = f.placed.clone();
        // A delete at this name a moment ago (a save by replace) waits for this write however long the file takes to
        // read in (SPEC §7G.11): marked from here, the wait for the core included, until the closure is done or dropped.
        let writing = f.replacing.writing(path);
        let wait = *lock(&f.write_wait);
        let written = self
            .with_core(&f, move |c| {
                let _writing = writing;
                // The host adopting a new file a second time (Windows: it took the folder above first, and this file
                // with it): its own placeholder, filled with the item's content and unchanged since, is that item.
                if base.is_none()
                    && let Some(pl) = &placed
                    && adopted_already(pl, c.store(), &p, &from)?
                {
                    return Ok(p);
                }
                // Checked first: a refused write leaves the cache as it was.
                let to = c.host_target(&p, base.as_deref(), &device)?;
                // An edit from an earlier version: content the folder records already is no new edit, so no host can
                // make copy after copy by reporting one again (Windows 0.1.18 did, at every look). The item's current
                // content is no conflict and no new version; content already written as a conflict copy of the item is
                // that copy. (A new file, without a base, is a file of its own wherever it lands: never folded in.)
                let edit_moved_on = base.is_some() && to != p;
                // An unhashed version (SPEC §7G.11) by the bytes this device has of it: this device's own edit, which
                // its scan recorded before the host reported it (Windows: a placeholder written while the app was not
                // running), is the version at the name, never a conflict with itself.
                let core = c.fs();
                let same_file = |q: &str| {
                    placed.as_ref().and_then(|pl| pl.held(q).map(|n| (pl.place(), n))).is_some_and(|(place, n)| {
                        let from = std::fs::canonicalize(&from).ok();
                        from.is_some() && [q, n.at.as_str()].iter().any(|at| std::fs::canonicalize(abs(&place, at)).ok() == from)
                    })
                };
                if edit_moved_on
                    && let Some(r) = c.store().local(&p)?.filter(|r| trs_sync::ondemand::live(r) && r.file.info.kind == trs_sync_proto::model::FileKind::File)
                    && host_file_holds_version(&*core, &r, &from, same_file(&p))?
                {
                    return Ok(p);
                }
                let store = c.store();
                let holds_copy = &mut |i: &trs_sync_proto::model::FileInfo| match store.local(&i.path)? {
                    Some(r) => host_file_holds_version(&*core, &r, &from, false),
                    None => Ok(false),
                };
                if edit_moved_on && let Some(r) = trs_sync::ondemand::copy_holding(c.store(), &p, holds_copy)? {
                    let copy = r.file.info.path.clone();
                    if !r.dataless {
                        return Ok(copy);
                    }
                    // The copy's content is not here any more (freed, or another device's copy): the host's bytes,
                    // checked against its hashes once copied, are its content here again (as `od_local_change` asks:
                    // a copy not here is no record of the host's file), and the host's feed names the item again so
                    // the host lets go of its file. Changed while copied: a new edit, written below.
                    make_parents(c, &root, &copy)?;
                    let tmp_rel = trs_sync::pull::temp_path(&copy);
                    let tmp = abs(&root, &tmp_rel);
                    trs_platform::fs::copy_shared(&from, &tmp)?;
                    if host_file_holds(&tmp, &r.file.info)? {
                        let cache = trs_sync::fs::StdFs::new(root.clone());
                        cache.set_mtime(&tmp_rel, r.file.info.mtime_ns)?;
                        std::fs::rename(&tmp, abs(&root, &copy))?;
                        c.scan(&copy)?;
                        c.host_show(&p)?;
                        return Ok(copy);
                    }
                    let _ = std::fs::remove_file(&tmp);
                }
                make_parents(c, &root, &to)?;
                let target = abs(&root, &to);
                let tmp = abs(&root, &trs_sync::pull::temp_path(&to));
                // The user's file, which another program may be writing: never locked against it (Windows), and a copy
                // that overlapped a write is made again or refused for now, never recorded. On APFS a clone: the
                // host's copy and the cache share their blocks.
                trs_platform::fs::copy_shared(&from, &tmp)?;
                // Kept in place: the new content is the cache's until the host fills the placeholder with it
                // (`od_hydrated`), whose note stays as a record of what it held.
                if let Some(pl) = &placed {
                    pl.let_go(&to)?;
                }
                std::fs::rename(&tmp, &target)?;
                if !wait.is_zero() {
                    std::thread::sleep(wait);
                }
                if to == p { c.host_written(&to) } else { c.host_written_copy(&to, &p) }?;
                Ok(to)
            })
            .await?;
        // A kept folder above it is taken up with it (`make_parents`).
        taken_up(&f, &written);
        Ok(written)
    }

    /// Why `path` (a directory with `dir`) is a name on-demand folder `id` does not sync, if it is: the line that decides
    /// and whether it is deletable (SPEC §7G.2). From the patterns as the core last read them (never waiting for it).
    pub fn od_ignored(&self, id: &FolderId, path: &str, dir: bool) -> Result<Option<trs_sync::ignore::Why>, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        Ok(f.ignore.get().why(path, dir))
    }

    /// What announcing folder `id`'s changes waits for (a delete the host made a moment ago may be a save by replace,
    /// SPEC §7G.11): how long still, and whether a new file is being written at a deleted name, which holds it past the
    /// window until that write ends. `None` when nothing waits.
    pub fn od_replace_hold(&self, id: &FolderId) -> Option<trs_sync::ondemand::Hold> {
        self.inner.ctx.registry.folder(id)?.replacing.hold()
    }

    /// The user's new folder at `path` (with any missing folders above it).
    ///
    /// Kept in place, a folder another device deleted that the host kept (it holds only names this folder does not
    /// sync, as a full folder keeps it; SPEC §7G.11, *Kept folders*) is refused with [`OnDemandError::Ignored`]: nothing
    /// is recorded or sent, the delete stays the record, and the folder shows the full folder's issue ("kept: …"), once.
    /// The host leaves it as its own plain folder. A synced file in it later takes it up again: this call then records
    /// it, and so does [`Self::od_write`] of a file below it. A folder made after the delete (by its creation time) is
    /// new, and recorded even empty. The issue ends as [`check_kept`] says.
    pub async fn od_mkdir(&self, id: &FolderId, path: &str) -> Result<(), SyncError> {
        check(path)?;
        if path.is_empty() {
            return Ok(());
        }
        let f = self.on_demand_folder(id)?;
        let (root, p) = (f.root.clone(), path.to_string());
        // The host's tree, where its folder is (in place only: elsewhere the host's folder is the system's, not seen).
        let place = f.placed.as_ref().map(|pl| (trs_sync::fs::StdFs::new(pl.place()), pl.place()));
        let kept = self
            .with_core(&f, move |c| {
                c.host_check_write(&p, true)?;
                // A folder that is there already is nothing new (and what is in it is not the host's to record).
                if c.item_at(&p)?.is_some() {
                    return Ok(None);
                }
                if let Some((place, at)) = &place
                    && let Some(kept) = c.host_kept_dir(&p, place, &|q: &str| born(&abs(at, q)))?
                {
                    return Ok(Some(kept));
                }
                make_parents(c, &root, &p)?;
                match std::fs::create_dir(abs(&root, &p)) {
                    Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
                    _ => {}
                }
                c.host_made_dir(&p)?;
                Ok(None)
            })
            .await?;
        let Some(kept) = kept else {
            taken_up(&f, path);
            return Ok(());
        };
        {
            let mut live = lock(&f.live);
            match &kept.by {
                Some(by) => live.set_kept(path, by),
                None => live.drop_kept(path),
            }
        }
        Err(super::on_demand(OnDemandError::Ignored, kept.words))
    }

    /// The user deleted `path` (and everything below it). On a read-only member it frees the content instead
    /// ([`Deleted::Freed`]), unpinning what is pinned there (saved with the folder's settings).
    pub async fn od_delete(&self, id: &FolderId, path: &str) -> Result<Deleted, SyncError> {
        self.od_delete_seen(id, path, None).await
    }

    /// The newest sequence of anything at or below `path` (deletes too): a folder's version for the host, which
    /// changes whenever anything below it does. Read from the index's read-only handle.
    pub fn od_newest_below(&self, id: &FolderId, path: &str) -> Result<u64, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        Ok(newest_below(&f.index.view(), path, None)?)
    }

    /// [`Self::od_delete`] of what the host saw: `seen` is the folder's version then ([`Self::od_newest_below`]).
    /// When another device's version arrived below since, nothing is deleted (this device's own writes and deletes
    /// below, as `rm -rf` makes them first, never refuse it; a download of a version listed already counts as an
    /// arrival, since the record of it is new): refused as
    /// [`OnDemandError::Changed`], checked on the folder's core, so nothing arrives between the check and the delete.
    pub async fn od_delete_seen(&self, id: &FolderId, path: &str, seen: Option<super::DeleteSeen>) -> Result<Deleted, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        // A read-only member frees instead: asked as an evict is.
        let frees = self.with_core(&f, |c| Ok(c.frees_on_delete())).await?;
        let (confirmed, freeing) = if frees { self.confirmed_held(&f, path, true).await? } else { (Vec::new(), Freeing::new(f.clone(), Vec::new())) };
        let p = path.to_string();
        let (d, pins) = self
            .with_core(&f, move |c| {
                let _freeing = freeing;
                let changed = match &seen {
                    Some(super::DeleteSeen::Folder(seen)) => (newest_below(c.store(), &p, Some(c.me()))? > *seen)
                        .then(|| format!("{p}: something in it changed on another device since; look again before deleting it")),
                    Some(super::DeleteSeen::File(seen)) => c
                        .store()
                        .local(&p)?
                        .filter(trs_sync::ondemand::live)
                        .filter(|r| &trs_sync::ondemand::content_version_of(&r.file.info) != seen)
                        .map(|_| format!("{p}: a newer version arrived from another device, so it was not deleted")),
                    None => None,
                };
                // The file that left under a name the folder does not sync, as the host saw it then, while a new file
                // took its place (a save by replace): the host lets go of it; the item there now stays.
                // Asked first: a save that changed nothing has the content version the host saw.
                if let Some(super::DeleteSeen::File(seen)) = &seen
                    && c.host_left(&p, seen)?
                {
                    return Ok((Ok(Deleted::Deleted { items: 0, failed: Vec::new() }), c.pins().clone()));
                }
                if let Some(why) = changed {
                    return Ok((Err(HostError::Changed(why)), c.pins().clone()));
                }
                Ok((c.host_delete(&p, &confirmed), c.pins().clone()))
            })
            .await?;
        self.save_pins(id, &f, pins)?;
        d.map_err(host_err)
    }

    /// The user renamed or moved `from` to `to`. Returns how many items moved.
    ///
    /// Made here when this device holds every file there. Otherwise a connected writer that holds them all (a full
    /// device first) is asked to make it, and the call ends when the renamed records arrive here: a device records a
    /// version only if it holds its bytes (SPEC §7G.11). With no such device connected it is refused
    /// ([`RENAME_NEEDS_HOLDER`]).
    pub async fn od_rename(&self, id: &FolderId, from: &str, to: &str) -> Result<u64, SyncError> {
        check(from)?;
        check(to)?;
        let f = self.on_demand_folder(id)?;
        let (from2, to2) = (from.to_string(), to.to_string());
        // Kept in place: the user's placeholders move with the rename, and so do the notes of what is in them.
        let placed = f.placed.clone();
        enum Renamed {
            Here(u64),
            /// Onto a name the folder does not sync: the item left the folder (deleted everywhere), its content the host's.
            Left(String),
            ByHolder([u8; 32], Vec<ShortId>, String),
        }
        // One step on the folder's core: the name it lands at is checked against the ignore rules and renamed to, or
        // left by, with nothing in between (the rules cannot change, nor the name land elsewhere).
        let here = self
            .with_core(&f, move |c| {
                let in_place = placed.as_ref().map(|p| move |path: &str| p.is_held(path));
                if let Some(words) = c.host_leave(&from2, &to2, in_place.as_ref().map(|h| h as &dyn Fn(&str) -> bool))? {
                    return Ok((Renamed::Left(words), c.pins().clone()));
                }
                let r = match &placed {
                    Some(p) => p.host_rename(&from2, &to2, || c.host_rename(&from2, &to2)),
                    None => c.host_rename(&from2, &to2),
                };
                match r {
                    Ok(n) => Ok((Renamed::Here(n), c.pins().clone())),
                    Err(HostError::NeedsHolder) => {
                        Ok((Renamed::ByHolder(c.rename_seen(&from2)?, c.rename_holders(&from2)?, c.rename_target(&to2)?), c.pins().clone()))
                    }
                    Err(e) => Err(e),
                }
            })
            .await?;
        match here {
            (Renamed::Here(n), pins) => {
                self.save_pins(id, &f, pins)?;
                Ok(n)
            }
            (Renamed::Left(words), pins) => {
                self.save_pins(id, &f, pins)?;
                Err(super::on_demand(OnDemandError::Ignored, words))
            }
            (Renamed::ByHolder(seen, holders, to), _) => self.rename_by_holder(id, &f, from, &to, seen, holders).await,
        }
    }

    /// Asks the connected writers among `holders` (in their order) to rename `from` to `to` as this device saw it
    /// (`seen`), each to start within [`Ctx::hold_wait`](crate::shared::Ctx) or say it is busy, and waits for each
    /// answer: the next is asked only after a refusal or a busy one. Done, it waits for the renamed records here and
    /// moves the pins with them. No answer (an earlier Seedwell, a session ended) may still mean done: the host is told
    /// it failed, and the pins follow if the renamed records arrive later.
    async fn rename_by_holder(&self, id: &FolderId, f: &Arc<FolderShared>, from: &str, to: &str, seen: [u8; 32], holders: Vec<ShortId>) -> Result<u64, SyncError> {
        use trs_sync_proto::msg::RenameOutcome;
        let peers = self.inner.ctx.registry.peers();
        let mut refused = None;
        for d in holders {
            // Only a device that answers is asked: an earlier build (full, so first) would leave every rename unanswered.
            let Some(peer) = peers.iter().find(|p| {
                p.device.short() == d && p.access(&f.id).is_some() && f.writes(&p.device_pk) && p.has(trs_sync_proto::msg::FEATURE_RENAMES)
            }) else {
                continue;
            };
            let ttl = *lock(&self.inner.ctx.hold_wait);
            let ttl_ms = u32::try_from(ttl.as_millis()).unwrap_or(u32::MAX);
            let Some((rid, rx)) = peer.ask_rename(f.id, from.to_string(), to.to_string(), seen, ttl_ms).await else { continue };
            match tokio::time::timeout(ttl + RENAME_ANSWER_MARGIN, rx).await {
                Ok(Ok(r)) if r.outcome == RenameOutcome::Done => return self.renamed_here(id, f, from, to).await,
                Ok(Ok(r)) => refused = Some(r.reason),
                _ => {
                    peer.forget_rename(rid);
                    self.follow_rename(id, f, from, to, RENAME_LATE_WATCH);
                    return Err(invalid("The device that has these files did not answer: it may rename them still, and that shows here if it does"));
                }
            }
        }
        Err(invalid(match refused {
            Some(why) => format!("The device that has these files did not rename them: {why}"),
            None => RENAME_NEEDS_HOLDER.to_string(),
        }))
    }

    /// Waits until `to` is here and `from` gone (a rename another device made for this one arrived), then moves the
    /// pins with it. Returns how many records there are under `to`. Not in time: the host is told it failed, and the
    /// pins follow when it arrives.
    async fn renamed_here(&self, id: &FolderId, f: &Arc<FolderShared>, from: &str, to: &str) -> Result<u64, SyncError> {
        match self.wait_renamed(f, from, to, RENAME_ARRIVAL).await? {
            Some(n) => {
                self.move_pins(id, f, from, to).await?;
                Ok(n)
            }
            None => {
                self.follow_rename(id, f, from, to, RENAME_LATE_WATCH);
                Err(invalid("Renamed on the device that has these files; the change shows here when it arrives"))
            }
        }
    }

    /// Moves the pins of `from` to `to` (a rename made for this device arrived), saved with the folder's settings.
    async fn move_pins(&self, id: &FolderId, f: &Arc<FolderShared>, from: &str, to: &str) -> Result<(), SyncError> {
        let (from, to) = (from.to_string(), to.to_string());
        let pins = self
            .with_core(f, move |c| {
                c.move_pins(&from, &to);
                Ok(c.pins().clone())
            })
            .await?;
        self.save_pins(id, f, pins)
    }

    /// In the background, for `watch` at most: once the rename of `from` to `to` arrives, the pins follow it.
    fn follow_rename(&self, id: &FolderId, f: &Arc<FolderShared>, from: &str, to: &str, watch: Duration) {
        let (me, id, f, from, to) = (self.clone(), *id, f.clone(), from.to_string(), to.to_string());
        tokio::spawn(async move {
            if let Ok(Some(_)) = me.wait_renamed(&f, &from, &to, watch).await {
                let _ = me.move_pins(&id, &f, &from, &to).await;
            }
        });
    }

    /// Waits (at most `wait`) until `to` is here and `from` gone. Returns how many records there are under `to`, or
    /// `None` when it did not happen in time.
    async fn wait_renamed(&self, f: &Arc<FolderShared>, from: &str, to: &str, wait: Duration) -> Result<Option<u64>, SyncError> {
        let end = Instant::now() + wait;
        loop {
            let view = f.index.view();
            let live = |p: &str| -> io::Result<bool> { Ok(view.local(p)?.is_some_and(|r| trs_sync::ondemand::live(&r))) };
            if live(to)? && !live(from)? {
                let mut n = 0u64;
                let mut after: Option<String> = None;
                loop {
                    let page = view.local_paths(to, after.as_deref(), 1000)?;
                    let Some(last) = page.last().cloned() else { break };
                    n += page.len() as u64;
                    after = Some(last);
                }
                return Ok(Some(n));
            }
            if Instant::now() >= end || f.stopped.borrow().to_owned() {
                return Ok(None);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Switches a folder between full and on demand. To on demand: its files become the content cache (moved, so
    /// nothing is fetched again) and `place` (default: where it was) is where the host shows it. To full: the cache
    /// moves to `place` (default: the folder's place, which must be free), and the folder runs full from there,
    /// fetching every file that was online only. Nothing changes until the folder is stopped and moved: refused
    /// (still busy) or failed, the folder goes on as it was, at its place, with its pins.
    pub async fn set_on_demand(&self, id: &FolderId, on: bool, place: Option<PathBuf>) -> Result<FolderStatus, SyncError> {
        self.refuse_held(id)?;
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if entry.on_demand == on {
            return self.folder_status(id).ok_or(SyncError::NotFound);
        }
        let place = match place {
            Some(p) => super::absolute(&p)?,
            None => entry.path.clone(),
        };
        let cache = self.cache_path(id);
        let (from, to) = if on { (entry.path.clone(), cache.clone()) } else { (cache.clone(), place.clone()) };
        if on && cache.exists() {
            return Err(invalid("an earlier cache of this folder is still there"));
        }
        let not_empty = |place: &Path| place.exists() && std::fs::read_dir(place).map(|mut d| d.next().is_some()).unwrap_or(true);
        // Kept in place at its own place: the host's placeholders are there until it lets go of them, and the content
        // they hold comes back into the cache first (so the place is checked after that).
        let own_place = entry.in_place && place == entry.path;
        if !on {
            // At its own place, kept in place, the folder is Seedwell's own sync root until the host lets go of it below
            // (Windows' Cloud Files; the system's check calls every sync root cloud storage): what counts is the folder
            // it is in. (A Mac's File Provider domain is never a folder's place: it lives in ~/Library/CloudStorage.)
            super::refuse_cloud_root(if own_place { place.parent().unwrap_or(&place) } else { &place })?;
            self.refuse_forbidden(&place)?;
            self.refuse_overlap_other(&place, Some(id))?;
            // The place must be free (an empty folder there, the host's, is taken over).
            if !own_place && not_empty(&place) {
                return Err(invalid(format!("{} is not empty: choose another place for the folder", place.display())));
            }
        }
        // The file manager's host lets go of the folder first (told when the switch is over, done or not).
        let _settle = if on { None } else { self.release_host(id, &entry).await? };
        if !on && entry.in_place {
            self.take_back_placed(id).await?;
            if not_empty(&place) {
                return Err(invalid(format!(
                    "{} is not empty: what is left there is not the folder's (its files are back in Seedwell's storage); choose another place for the folder",
                    place.display()
                )));
            }
        }
        // The journal first, before anything is touched: a crash from here on finishes the switch at the next start
        // (moved, or its copy whole, not saved) or undoes it (a partial copy goes; the original was never touched).
        let (from_shown, to_shown) = (from.clone(), to.clone());
        let j = super::Journal::Switch { on, from, to: to.clone(), place: place.clone(), copied: false, renamed: false, made_to: !to.exists(), done: false };
        super::write_journal_at(&self.journal_path(id), &j).map_err(|e| invalid(format!("the switch could not be noted ({e}); nothing changed")))?;
        let closed = match self.stop_folder(id).await {
            Ok((_, closed)) => closed,
            Err(e) => {
                self.drop_journal(id);
                return Err(e);
            }
        };
        if !closed.load(std::sync::atomic::Ordering::SeqCst) {
            self.drop_journal(id);
            self.reopen_when_closed(closed, entry);
            return Err(invalid("the folder is still busy (a scan or a transfer); it stays as it is, try again in a moment"));
        }
        // Freeing the local copies moves the files from under any torrent there: asked again now that the folder has
        // stopped (which takes up to a minute), right before the move. Refused, the folder goes on as it was.
        if on && let Err(e) = self.refuse_in_use(&entry.path).await {
            self.drop_journal(id);
            self.open_folder(&entry).await?;
            return Err(e);
        }
        // An empty folder at the place (the host's) is taken over, and back if the move fails.
        let empty = !on && to.is_dir();
        let moved = match if empty { std::fs::remove_dir(&to) } else { Ok(()) } {
            Ok(()) => self.noted_move(id, j).await,
            Err(e) => Err(e),
        };
        if let Err(e) = &moved
            && super::unfinished(e)
        {
            return Err(self.hold_unfinished(id, e));
        }
        let leftover = match moved {
            Ok(leftover) => leftover,
            Err(e) => {
                // Nothing moved: the folder goes on as it was, at its place, with its pins.
                if empty {
                    let _ = std::fs::create_dir(&to);
                }
                self.drop_journal(id);
                self.open_folder(&entry).await?;
                return Err(invalid(format!("the files could not be moved to {} ({e}); the folder stays as it was, its files in {}", to_shown.display(), from_shown.display())));
            }
        };
        let mut entry2 = entry.clone();
        entry2.on_demand = on;
        // Full again is full for the devices that join it too: an earlier mark goes (SPEC §7G.11, *Modes*).
        entry2.on_demand_others &= on;
        entry2.pins = Vec::new();
        entry2.mode_switched = true;
        entry2.path = place;
        // Kept in place is the host's choice for each on-demand folder it shows; whatever the host had of it is gone.
        entry2.in_place = false;
        crate::placed::Placed::remove_log(&self.placed_path(id));
        self.drop_host_anchor(id);
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            *e = entry2.clone();
        }
        // Saved, the journal goes; not saved, it stays and the next start finishes the switch.
        let saved = self.save_config();
        if saved.is_ok() {
            self.drop_journal(id);
        }
        self.open_folder(&entry2).await?;
        saved?;
        if let Some(l) = leftover {
            eprintln!("seedwell sync: the folder's files are in {}; a copy left behind could not be deleted: {}", to_shown.display(), l.display());
        }
        // The *On all my devices* statement says the new mode, for the devices that join it later.
        self.follow_mode_changed(id).await;
        self.folder_status(id).ok_or(SyncError::NotFound)
    }

    /// Joins a folder by link, on demand: nothing is downloaded until asked for; `place` is where the host shows it
    /// (it is not made here).
    pub async fn join_folder_on_demand(&self, key_or_link: &str, place: PathBuf) -> Result<FolderStatus, SyncError> {
        self.join_with(key_or_link, place, true).await
    }
}

/// The kept-folder issues ([`SyncHandle::od_mkdir`]) of `path` and the folders above it go: they are in the folder again.
fn taken_up(f: &FolderShared, path: &str) {
    let mut live = lock(&f.live);
    let over: Vec<String> = live.kept.keys().filter(|q| *q == path || path.strip_prefix(q.as_str()).is_some_and(|r| r.starts_with('/'))).cloned().collect();
    for q in over {
        live.drop_kept(&q);
    }
}

/// When the directory at `at` was made (Unix ns), where the system says.
fn born(at: &Path) -> Option<i64> {
    let t = std::fs::symlink_metadata(at).ok()?.created().ok()?;
    Some(match t.duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        Err(e) => -i64::try_from(e.duration().as_nanos()).unwrap_or(i64::MAX),
    })
}

/// The kept folders of `f` (SPEC §7G.11, *Kept folders*) that are over lose their issue: in the folder again (another
/// device made it anew), gone from the host's disk, or no longer kept by the file that kept it (deleted, or its line now
/// `(?d)` or gone). A few stats of the host's disk and index reads, no walk: off the caller's thread only (settling,
/// and a snapshot's background check). A disk that cannot be read now changes nothing.
pub(crate) fn check_kept(f: &FolderShared) {
    let kept: Vec<(String, String)> = lock(&f.live).kept.iter().map(|(d, b)| (d.clone(), b.clone())).collect();
    if kept.is_empty() {
        return;
    }
    let Some(pl) = &f.placed else { return };
    let place = trs_sync::fs::StdFs::new(pl.place());
    let ignore = f.ignore.get();
    let view = f.index.view();
    let mut over = Vec::new();
    for (dir, by) in kept {
        let back = view.local(&dir).ok().flatten().is_some_and(|r| trs_sync::ondemand::live(&r));
        let (Ok(d), Ok(b)) = (place.lstat(&dir), place.lstat(&by)) else { continue };
        let dir_there = d.is_some_and(|m| m.kind == trs_sync_proto::model::FileKind::Dir);
        let keeps = b.is_some_and(|m| {
            ignore.matches(&by, m.kind == trs_sync_proto::model::FileKind::Dir).is_some_and(|x| x.ignored && !x.deletable)
        });
        if back || !dir_there || !keeps {
            over.push((dir, by));
        }
    }
    let mut live = lock(&f.live);
    for (d, by) in over {
        // (Unless the host offered it again meanwhile, and something else keeps it now.)
        if live.kept.get(&d) == Some(&by) {
            live.drop_kept(&d);
        }
    }
}

/// Whether `from`, the host's new file at `p`, is `p`'s own placeholder, which holds the content of the item there
/// (the host filled it and said so) and is unchanged since: an item the host adopted already.
fn adopted_already(pl: &crate::placed::Placed, store: &dyn IndexStore, p: &str, from: &Path) -> io::Result<bool> {
    let Some(n) = pl.held(p).filter(|n| n.at == p) else { return Ok(false) };
    let Some(rec) = store.local(p)?.filter(|r| trs_sync::ondemand::live(r) && r.file.info.kind == trs_sync_proto::model::FileKind::File) else {
        return Ok(false);
    };
    if n.content != trs_sync::ondemand::content_version_of(&rec.file.info) {
        return Ok(false);
    }
    let at = abs(&pl.place(), p);
    match (std::fs::canonicalize(&at), std::fs::canonicalize(from)) {
        (Ok(a), Ok(b)) if a == b => {}
        _ => return Ok(false),
    }
    let Some(m) = trs_sync::fs::StdFs::new(pl.place()).lstat(p)? else { return Ok(false) };
    Ok(n.real.is_some_and(|r| r.same(&crate::placed::Mark::of(&m))))
}

/// Whether the host's file `from` holds exactly `info`'s content (its size, each chunk's hash; never an unhashed one's).
fn host_file_holds(from: &Path, info: &trs_sync_proto::model::FileInfo) -> io::Result<bool> {
    let (Some(dir), Some(name)) = (from.parent(), from.file_name().and_then(|n| n.to_str())) else { return Ok(false) };
    crate::placed::holds_content(&trs_sync::fs::StdFs::new(dir.to_path_buf()), name, info)
}

/// Whether the host's file `from` holds the content of `rec`, a version recorded here ([`crate::placed::holds_version`]:
/// an unhashed one by the bytes `core`, the folder's filesystem, has of it; `same_file`: `from` is where the core reads
/// it).
fn host_file_holds_version(core: &dyn Fs, rec: &trs_sync::index::LocalRecord, from: &Path, same_file: bool) -> io::Result<bool> {
    let (Some(dir), Some(name)) = (from.parent(), from.file_name().and_then(|n| n.to_str())) else { return Ok(false) };
    crate::placed::holds_version(core, rec, &trs_sync::fs::StdFs::new(dir.to_path_buf()), name, same_file)
}

/// Makes the folders above `path` that are missing in the cache, each recorded as the host's new folder.
fn make_parents(c: &mut FolderCore, root: &Path, path: &str) -> Result<(), HostError> {
    let mut at = String::new();
    let comps: Vec<&str> = path.split('/').collect();
    for comp in &comps[..comps.len().saturating_sub(1)] {
        if !at.is_empty() {
            at.push('/');
        }
        at.push_str(comp);
        if at == STATE_DIR {
            return Err(HostError::BadName(path.to_string()));
        }
        let dir = abs(root, &at);
        if !dir.exists() {
            std::fs::create_dir(&dir)?;
        }
        if c.item_at(&at)?.is_none() {
            c.host_made_dir(&at)?;
        }
    }
    Ok(())
}
