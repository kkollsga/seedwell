//! On-demand folders in the service (SPEC §7G.11): what a host (File Provider on the Mac, Cloud Files on Windows) and
//! the CLI call. Item queries read the index through the read-only handle, so a host listing a folder never waits for
//! a pull that holds the folder's core; changes go through the core like a scan's.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync::folder::{FolderCore, HostError, RENAME_NEEDS_HOLDER, Requested};
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
use crate::shared::{FolderShared, Freeing};
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
        other => invalid(other.to_string()),
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
        let p = path.to_string();
        self.with_core(&f, move |c| c.cancel_request(&p)).await?;
        Ok(())
    }

    /// Fetches `path` (a file, or every file below a folder) and waits until it is here, at most `timeout`. Returns
    /// where it is in the cache: the host copies it into place (a clone on APFS costs nothing).
    /// A folder's files no device can give are not waited for: they are reported (`unavailable`).
    pub async fn od_fetch(&self, id: &FolderId, path: &str, timeout: Duration) -> Result<Fetched, SyncError> {
        // A version no device can give is not waited for.
        if self.od_item_at(id, path)?.is_some_and(|i| i.state == trs_sync::ondemand::ItemState::Unavailable) {
            return Err(super::on_demand(OnDemandError::Unavailable, format!("{path}: {}", trs_sync::ondemand::UNAVAILABLE)));
        }
        let r = self.od_request(id, path).await?;
        let f = self.on_demand_folder(id)?;
        let end = Instant::now() + timeout;
        let mut unavailable = Vec::new();
        if r != Requested::Here {
            loop {
                let (here, wanted, missing) = self.fetch_state(&f, path)?;
                // A file that became one no device can give meanwhile says so at once.
                if missing.iter().any(|p| p == path) {
                    return Err(super::on_demand(OnDemandError::Unavailable, format!("{path}: {}", trs_sync::ondemand::UNAVAILABLE)));
                }
                unavailable = missing;
                match (here, wanted) {
                    (true, _) => break,
                    (false, false) => return Err(invalid(format!("{path}: the fetch was cancelled"))),
                    (false, true) => {}
                }
                if Instant::now() >= end {
                    return Err(super::on_demand(OnDemandError::NotHereYet, format!("{path}: not here yet (is a device that has it connected?)")));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(Fetched { path: abs(&f.root, path), unavailable })
    }

    /// (here, still wanted) for a file, or for every file below a folder.
    /// (here, still wanted, unavailable) for a file, or for every file below a folder: a file no device can give
    /// counts as neither here nor waited for, and is listed.
    fn fetch_state(&self, f: &FolderShared, path: &str) -> io::Result<(bool, bool, Vec<String>)> {
        let view = f.index.view();
        let mut here = true;
        let mut wanted = false;
        let mut unavailable = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = view.local_paths(path, after.as_deref(), 1000)?;
            let Some(last) = page.last().cloned() else { break };
            for p in &page {
                if let Some(r) = view.local(p)?.filter(|r| r.dataless && !r.file.info.deleted) {
                    if view.unavailable_at(p)?.is_some() {
                        unavailable.push(p.clone());
                        continue;
                    }
                    here = false;
                    wanted |= r.want;
                }
            }
            after = Some(last);
        }
        Ok((here, wanted, unavailable))
    }

    /// How far the fetch of the file at `path` is (its partial copy in the cache).
    pub fn od_progress(&self, id: &FolderId, path: &str) -> Result<FetchProgress, SyncError> {
        check(path)?;
        let f = self.on_demand_folder(id)?;
        let Some(r) = f.index.local(path)? else { return Err(super::on_demand(OnDemandError::NoItem, format!("{path}: no such item in the folder"))) };
        let total = r.file.info.size;
        if !r.dataless {
            return Ok(FetchProgress { done: total, total, here: true, fetching: false });
        }
        let tmp = abs(&f.root, &trs_sync::pull::temp_path(path));
        let done = std::fs::metadata(tmp).map(|m| m.len().min(total)).unwrap_or(0);
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
    /// the content becomes a conflict copy named as a full folder names one, and the version there stays. Returns the
    /// path written.
    pub async fn od_write(&self, id: &FolderId, path: &str, from: &Path, base: Option<&str>) -> Result<String, SyncError> {
        check(path)?;
        if path.is_empty() {
            return Err(invalid("the folder itself is not a file"));
        }
        let f = self.on_demand_folder(id)?;
        let (root, p, from, base) = (f.root.clone(), path.to_string(), from.to_path_buf(), base.map(str::to_string));
        let device = lock(&self.inner.ctx.device_name).clone();
        self.with_core(&f, move |c| {
            // Checked first: a refused write leaves the cache as it was.
            let to = c.host_target(&p, base.as_deref(), &device)?;
            make_parents(c, &root, &to)?;
            let target = abs(&root, &to);
            let tmp = abs(&root, &trs_sync::pull::temp_path(&to));
            // `std::fs::copy` clones on APFS: the host's copy and the cache share their blocks.
            std::fs::copy(&from, &tmp)?;
            std::fs::rename(&tmp, &target)?;
            if to == p { c.host_written(&to) } else { c.host_written_copy(&to, &p) }?;
            Ok(to)
        })
        .await
    }

    /// The user's new folder at `path` (with any missing folders above it).
    pub async fn od_mkdir(&self, id: &FolderId, path: &str) -> Result<(), SyncError> {
        check(path)?;
        if path.is_empty() {
            return Ok(());
        }
        let f = self.on_demand_folder(id)?;
        let (root, p) = (f.root.clone(), path.to_string());
        self.with_core(&f, move |c| {
            c.host_check_write(&p, true)?;
            // A folder that is there already is nothing new (and what is in it is not the host's to record).
            if c.item_at(&p)?.is_some() {
                return Ok(());
            }
            make_parents(c, &root, &p)?;
            match std::fs::create_dir(abs(&root, &p)) {
                Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
                _ => {}
            }
            c.host_made_dir(&p)
        })
        .await
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
        let here = self
            .with_core(&f, move |c| match c.host_rename(&from2, &to2) {
                Ok(n) => Ok(Ok((n, c.pins().clone()))),
                Err(HostError::NeedsHolder) => Ok(Err((c.rename_seen(&from2)?, c.rename_holders(&from2)?, c.rename_target(&to2)?))),
                Err(e) => Err(e),
            })
            .await?;
        match here {
            Ok((n, pins)) => {
                self.save_pins(id, &f, pins)?;
                Ok(n)
            }
            Err((seen, holders, to)) => self.rename_by_holder(id, &f, from, &to, seen, holders).await,
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
        if !on {
            super::refuse_cloud_root(&place)?;
            self.refuse_forbidden(&place)?;
            self.refuse_overlap_other(&place, Some(id))?;
            // The place must be free (an empty folder there, the host's, is taken over).
            if place.exists() && std::fs::read_dir(&place).map(|mut d| d.next().is_some()).unwrap_or(true) {
                return Err(invalid(format!("{} is not empty: choose another place for the folder", place.display())));
            }
        }
        // The file manager's host lets go of the folder first (told when the switch is over, done or not).
        let _settle = if on { None } else { self.release_host(id, &entry).await? };
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
        entry2.pins = Vec::new();
        entry2.mode_switched = true;
        entry2.path = place;
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
        self.folder_status(id).ok_or(SyncError::NotFound)
    }

    /// Joins a folder by link, on demand: nothing is downloaded until asked for; `place` is where the host shows it
    /// (it is not made here).
    pub async fn join_folder_on_demand(&self, key_or_link: &str, place: PathBuf) -> Result<FolderStatus, SyncError> {
        self.join_with(key_or_link, place, true).await
    }
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
