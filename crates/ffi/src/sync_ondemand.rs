//! On-demand folders for the hosts (SPEC §7G.11): the items a File Provider extension (Mac) or a Cloud Files sync root
//! (Windows) shows, what it asks for (fetch, keep, free) and the user's changes it reports, by item identifier as those
//! systems name items. The rules are the core's (`trs_sync::ondemand`, `trs_sync_net`); this is the shape for hosts,
//! and a mock for design work and app tests.

use std::collections::BTreeMap;
use std::time::Duration;

use trs_sync::ondemand::{Item, ItemState, ROOT_ID};
use trs_sync_proto::model::FileKind;

use super::{CoreError, MockSync, RealSync, SyncFolder, SyncHandle, failed, folder_id, folder_of, folder_path, lock, sync_err};
use crate::Core;

/// What an item is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncItemKind {
    File,
    Dir,
    Symlink,
}

/// Where an item's content is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncItemState {
    /// Not on this computer; opening it fetches it.
    OnlineOnly,
    /// Asked for (or kept here) and on its way.
    Downloading,
    /// Here, and on another device too.
    Downloaded,
    /// Here only (a change no other device holds yet): it cannot be freed until one does.
    ModifiedHere,
    /// No device has this version any more (it was renamed while another device changed it under its old name): a
    /// fetch fails with that; the user can delete or rename it; it becomes fetchable if a device announces it held.
    Unavailable,
}

/// One item of an on-demand folder.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncItem {
    /// Stable across renames and moves; `root` is the folder.
    pub id: String,
    /// The parent's identifier (`root` at the top; the root's is itself).
    pub parent_id: String,
    /// The path in the folder (`/` between names; empty for the root).
    pub path: String,
    pub name: String,
    pub kind: SyncItemKind,
    pub size: u64,
    /// Modification time, Unix nanoseconds.
    pub mtime_ns: i64,
    pub state: SyncItemState,
    /// Kept on this computer (pinned, or below a pinned folder).
    pub pinned: bool,
    /// Changes exactly when the content does (File Provider's content version).
    pub content_version: String,
    /// Changes whenever the version does (File Provider's metadata version).
    pub metadata_version: String,
    pub exec: bool,
    /// A link's target.
    pub symlink_target: Option<String>,
    /// A conflict copy: the path it is a conflict of.
    pub conflict_of: Option<String>,
}

/// A page of a folder's children.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncItemPage {
    pub items: Vec<SyncItem>,
    /// Pass back for the next page; `None` on the last.
    pub next: Option<String>,
}

/// What changed since an anchor (File Provider's sync anchor).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncChangePage {
    /// New or changed, as they are now.
    pub items: Vec<SyncItem>,
    /// Identifiers of deleted items.
    pub deleted: Vec<String>,
    /// Ask from here next time.
    pub anchor: u64,
    /// More follow: ask again from `anchor`.
    pub more: bool,
}

/// What freeing space did.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct SyncEvicted {
    pub files: u64,
    pub bytes: u64,
    /// Kept: pinned, or no other device that has it is connected.
    pub kept: u64,
    /// The files kept, and why (`error` is the reason, for the host to show: the system's own eviction is refused).
    pub kept_why: Vec<SyncFailed>,
    /// Files that could not be freed (open elsewhere, say): they stay downloaded.
    pub failed: Vec<SyncFailed>,
}

/// An item a change could not touch, and why.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct SyncFailed {
    /// Its path in the folder.
    pub path: String,
    pub error: String,
}

/// What deleting an item did.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct SyncDeleted {
    /// Items deleted, on every device.
    pub deleted: u64,
    /// This computer is a read-only member: nothing was deleted, the content was freed instead (as `sync_evict`); the
    /// items stay, online only. The host puts the item back: on the Mac the delete is answered `deletionRejected` (it
    /// comes back dataless); on Windows the delete fails and the files are dehydrated.
    pub freed: Option<SyncEvicted>,
    /// Items that could not be removed (open elsewhere, say): they stay, and so do the folders above them.
    pub failed: Vec<SyncFailed>,
}

fn failed_of(f: Vec<(String, String)>) -> Vec<SyncFailed> {
    f.into_iter().map(|(path, error)| SyncFailed { path, error }).collect()
}

fn evicted_of(e: trs_sync::ondemand::Evicted) -> SyncEvicted {
    SyncEvicted { files: e.files, bytes: e.bytes, kept: e.kept, kept_why: failed_of(e.kept_why), failed: failed_of(e.failed) }
}

/// A fetch: the item, and where its content is once it is here.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncFetch {
    pub item: SyncItem,
    /// The content on this computer (the host copies it into place; a clone on APFS), once it is here.
    pub local_path: Option<String>,
    /// Bytes here so far, and the file's size.
    pub done: u64,
    pub total: u64,
    /// A folder's files no device can give right now (not waited for).
    pub unavailable: Vec<String>,
}

fn kind_of(k: FileKind) -> SyncItemKind {
    match k {
        FileKind::File => SyncItemKind::File,
        FileKind::Dir => SyncItemKind::Dir,
        FileKind::Symlink => SyncItemKind::Symlink,
    }
}

fn state_of(s: ItemState) -> SyncItemState {
    match s {
        ItemState::OnlineOnly => SyncItemState::OnlineOnly,
        ItemState::Downloading => SyncItemState::Downloading,
        ItemState::Downloaded => SyncItemState::Downloaded,
        ItemState::ModifiedHere => SyncItemState::ModifiedHere,
        ItemState::Unavailable => SyncItemState::Unavailable,
    }
}

pub(super) fn item_of(i: &Item) -> SyncItem {
    SyncItem {
        id: i.id.clone(),
        parent_id: i.parent_id.clone(),
        path: i.path.clone(),
        name: i.name.clone(),
        kind: kind_of(i.kind),
        size: i.size,
        mtime_ns: i.mtime_ns,
        state: state_of(i.state),
        pinned: i.pinned,
        content_version: i.content_version.clone(),
        metadata_version: i.metadata_version.clone(),
        exec: i.exec,
        symlink_target: (!i.symlink_target.is_empty()).then(|| i.symlink_target.clone()),
        conflict_of: i.conflict_of.clone(),
    }
}

/// The error of an on-demand call on a folder that is not on demand.
pub(crate) const NOT_ON_DEMAND: &str = "This folder is not on demand.";
/// How the error of an item that is not there (any more) starts.
pub(crate) const NO_ITEM: &str = "No item ";

/// Why an on-demand call failed, typed for a host that acts on it (the File Provider server): never read from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OdKind {
    /// No such item (any more).
    NoItem,
    /// The folder is not on demand.
    NotOnDemand,
    /// The folder is no longer synced here.
    Gone,
    /// Folder sync is not running (yet).
    NotRunning,
    /// The anchor is from before the folder's index was made anew.
    AnchorExpired,
    /// No device has this version right now.
    Unavailable,
    /// A fetch waited and the content is not here yet.
    NotHereYet,
    /// Something changed since the host saw it (a folder deleted with an older version).
    Changed,
    /// Anything else (the words say what).
    Other,
}

/// An on-demand call's error: its kind, and the error the app shows.
#[derive(Debug)]
pub struct OdError {
    pub kind: OdKind,
    pub error: CoreError,
}

impl From<CoreError> for OdError {
    fn from(error: CoreError) -> OdError {
        OdError { kind: OdKind::Other, error }
    }
}

impl From<OdError> for CoreError {
    fn from(e: OdError) -> CoreError {
        e.error
    }
}

impl std::fmt::Display for OdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

/// The service's error, with its kind.
pub(crate) fn od_err(e: trs_sync_net::SyncError) -> OdError {
    use trs_sync_net::{OnDemandError as E, SyncError};
    let kind = match &e {
        SyncError::NotFound => OdKind::Gone,
        SyncError::OnDemand(E::NotOnDemand, _) => OdKind::NotOnDemand,
        SyncError::OnDemand(E::NoItem, _) => OdKind::NoItem,
        SyncError::OnDemand(E::AnchorExpired, _) => OdKind::AnchorExpired,
        SyncError::OnDemand(E::Unavailable, _) => OdKind::Unavailable,
        SyncError::OnDemand(E::NotHereYet, _) => OdKind::NotHereYet,
        SyncError::OnDemand(E::Changed, _) => OdKind::Changed,
        SyncError::Invalid(_) | SyncError::Io(_) => OdKind::Other,
    };
    OdError { kind, error: sync_err(e) }
}

/// Folder sync not running (yet): the handle's error.
fn not_running(error: CoreError) -> OdError {
    OdError { kind: OdKind::NotRunning, error }
}

pub(super) fn not_on_demand() -> OdError {
    OdError { kind: OdKind::NotOnDemand, error: failed(NOT_ON_DEMAND) }
}

fn no_item(id: &str) -> OdError {
    OdError { kind: OdKind::NoItem, error: failed(format!("{NO_ITEM}{id} in this folder (it was deleted or moved away).")) }
}

/// A name the host gives for a new item: one path component.
fn check_name(name: &str) -> Result<(), CoreError> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
        return Err(failed(format!("“{name}” cannot be a name here.")));
    }
    Ok(())
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") }
}

// ------------------------------------------------------------------------------------------ the real service

impl RealSync {
    fn item_path(&self, h: &SyncHandle, fid: &trs_sync_proto::ids::FolderId, id: &str) -> Result<String, OdError> {
        if id == ROOT_ID {
            return Ok(String::new());
        }
        h.od_item(fid, id).map_err(od_err)?.map(|i| i.path).ok_or_else(|| no_item(id))
    }

    fn item_now(&self, h: &SyncHandle, fid: &trs_sync_proto::ids::FolderId, path: &str) -> Result<SyncItem, OdError> {
        h.od_item_at(fid, path).map_err(od_err)?.map(|i| item_of(&i)).ok_or_else(|| no_item(path))
    }

    pub(super) fn od_set_mode(&self, id: &str, on: bool, path: Option<&str>) -> Result<SyncFolder, CoreError> {
        let h = self.handle()?;
        let place = path.filter(|p| !p.trim().is_empty()).map(|p| folder_path(p, "the location").map_err(super::api_err)).transpose()?;
        let f = self.rt.block_on(h.set_on_demand(&folder_id(id)?, on, place)).map_err(sync_err)?;
        Ok(folder_of(&f))
    }

    pub(super) fn od_join(&self, link: &str, place: &str) -> Result<SyncFolder, CoreError> {
        let h = self.handle()?;
        let p = std::path::PathBuf::from(place);
        if !p.is_absolute() {
            return Err(failed("The place of an on-demand folder is an absolute path."));
        }
        let f = self.rt.block_on(h.join_folder_on_demand(link, p)).map_err(sync_err)?;
        Ok(folder_of(&f))
    }

    pub(super) fn od_item(&self, id: &str, item: &str) -> Result<Option<SyncItem>, OdError> {
        let h = self.handle().map_err(not_running)?;
        Ok(h.od_item(&folder_id(id)?, item).map_err(od_err)?.map(|i| item_of(&i)))
    }

    pub(super) fn od_children(&self, id: &str, item: &str, cursor: Option<&str>, limit: u32) -> Result<SyncItemPage, OdError> {
        let h = self.handle().map_err(not_running)?;
        let page = h.od_children(&folder_id(id)?, item, cursor, limit.clamp(1, 1000) as usize).map_err(od_err)?.ok_or_else(|| no_item(item))?;
        Ok(SyncItemPage { items: page.items.iter().map(item_of).collect(), next: page.next })
    }

    pub(super) fn od_changes(&self, id: &str, anchor: u64, limit: u32) -> Result<SyncChangePage, OdError> {
        let h = self.handle().map_err(not_running)?;
        let p = h.od_changes(&folder_id(id)?, anchor, limit.clamp(1, 1000) as usize).map_err(od_err)?;
        Ok(SyncChangePage { items: p.items.iter().map(item_of).collect(), deleted: p.deleted, anchor: p.anchor, more: p.more })
    }

    pub(super) fn od_anchor(&self, id: &str) -> Result<u64, OdError> {
        self.handle().map_err(not_running)?.od_anchor(&folder_id(id)?).map_err(od_err)
    }

    pub(super) fn od_fetch(&self, id: &str, item: &str, wait_ms: u64) -> Result<SyncFetch, OdError> {
        let h = self.handle().map_err(not_running)?;
        let fid = folder_id(id)?;
        let path = self.item_path(&h, &fid, item)?;
        let mut unavailable = Vec::new();
        if wait_ms == 0 {
            self.rt.block_on(h.od_request(&fid, &path)).map_err(od_err)?;
        } else {
            match self.rt.block_on(h.od_fetch(&fid, &path, Duration::from_millis(wait_ms))) {
                Ok(done) => unavailable = done.unavailable,
                Err(trs_sync_net::SyncError::OnDemand(trs_sync_net::OnDemandError::NotHereYet, _)) => {}
                Err(e) => return Err(od_err(e)),
            }
        }
        let now = self.item_now(&h, &fid, &path)?;
        let pr = if now.kind == SyncItemKind::File { h.od_progress(&fid, &path).map_err(od_err)? } else { Default::default() };
        let here = now.kind != SyncItemKind::File || pr.here;
        let local_path = here.then(|| h.od_cache_path(&fid, &path).ok()).flatten().map(|p| p.display().to_string());
        Ok(SyncFetch { item: now, local_path, done: pr.done, total: pr.total, unavailable })
    }

    pub(super) fn od_cancel(&self, id: &str, item: &str) -> Result<(), OdError> {
        let h = self.handle().map_err(not_running)?;
        let fid = folder_id(id)?;
        let path = self.item_path(&h, &fid, item)?;
        self.rt.block_on(h.od_cancel(&fid, &path)).map_err(od_err)
    }

    pub(super) fn od_pin(&self, id: &str, item: &str, on: bool) -> Result<SyncItem, OdError> {
        let h = self.handle().map_err(not_running)?;
        let fid = folder_id(id)?;
        let path = self.item_path(&h, &fid, item)?;
        self.rt.block_on(h.od_pin(&fid, &path, on)).map_err(od_err)?;
        self.item_now(&h, &fid, &path)
    }

    pub(super) fn od_evict(&self, id: &str, item: &str) -> Result<SyncEvicted, OdError> {
        let h = self.handle().map_err(not_running)?;
        let fid = folder_id(id)?;
        let path = self.item_path(&h, &fid, item)?;
        Ok(evicted_of(self.rt.block_on(h.od_evict(&fid, &path)).map_err(od_err)?))
    }

    pub(super) fn od_create(&self, id: &str, parent: &str, name: &str, dir: bool, contents: Option<&str>) -> Result<SyncItem, OdError> {
        check_name(name)?;
        let h = self.handle().map_err(not_running)?;
        let fid = folder_id(id)?;
        let path = join(&self.item_path(&h, &fid, parent)?, name);
        if dir {
            self.rt.block_on(h.od_mkdir(&fid, &path)).map_err(od_err)?;
        } else {
            let from = contents.ok_or_else(|| failed("A new file needs its content."))?;
            // Something there now (another device's, arrived first): the new file becomes a conflict copy.
            let at = self.rt.block_on(h.od_write(&fid, &path, std::path::Path::new(from), None)).map_err(od_err)?;
            return self.item_now(&h, &fid, &at);
        }
        self.item_now(&h, &fid, &path)
    }

    pub(super) fn od_modify(&self, id: &str, item: &str, parent: Option<&str>, name: Option<&str>, contents: Option<&str>, base: Option<&str>) -> Result<SyncItem, OdError> {
        let h = self.handle().map_err(not_running)?;
        let fid = folder_id(id)?;
        let mut path = self.item_path(&h, &fid, item)?;
        // The edit's base against the content as it is now, before any rename: a rename, or a new version with the same
        // content (a merge, an absorbed delete), is no conflict.
        let current = h.od_item_at(&fid, &path).map_err(od_err)?.map(|i| i.content_version);
        let base = match (base, &current) {
            (Some(b), Some(c)) if b == c => Some(c.clone()),
            (b, _) => b.map(str::to_string),
        };
        if parent.is_some() || name.is_some() {
            let (old_parent, old_name) = path.rsplit_once('/').map_or((String::new(), path.clone()), |(p, n)| (p.to_string(), n.to_string()));
            let new_parent = match parent {
                Some(p) => self.item_path(&h, &fid, p)?,
                None => old_parent,
            };
            let new_name = name.map_or(old_name, str::to_string);
            check_name(&new_name)?;
            let to = join(&new_parent, &new_name);
            if to != path {
                self.rt.block_on(h.od_rename(&fid, &path, &to)).map_err(od_err)?;
                path = to;
            }
        }
        if let Some(from) = contents {
            // Based on a version that moved on: the edit becomes a conflict copy (its item is returned).
            path = self.rt.block_on(h.od_write(&fid, &path, std::path::Path::new(from), base.as_deref())).map_err(od_err)?;
        }
        self.item_now(&h, &fid, &path)
    }

    pub(super) fn od_delete(&self, id: &str, item: &str, seen: Option<trs_sync_net::DeleteSeen>) -> Result<SyncDeleted, OdError> {
        let h = self.handle().map_err(not_running)?;
        let fid = folder_id(id)?;
        let path = self.item_path(&h, &fid, item)?;
        if path.is_empty() {
            return Err(failed("The folder itself is removed with Stop Syncing.").into());
        }
        Ok(match self.rt.block_on(h.od_delete_seen(&fid, &path, seen)).map_err(od_err)? {
            trs_sync::ondemand::Deleted::Deleted { items, failed } => SyncDeleted { deleted: items, freed: None, failed: failed_of(failed) },
            trs_sync::ondemand::Deleted::Freed(e) => SyncDeleted { deleted: 0, failed: failed_of(e.failed.clone()), freed: Some(evicted_of(e)) },
        })
    }
}

// ------------------------------------------------------------------------------------------ the mock

/// A mock on-demand folder's items (by identifier), and its change counter.
#[derive(Default)]
pub(super) struct MockOd {
    items: BTreeMap<String, SyncItem>,
    seq: u64,
    /// The sequence each item last changed at, and deletes by identifier.
    changed: BTreeMap<String, u64>,
    deleted: Vec<(u64, String)>,
}

impl MockOd {
    /// A few folders and files in every state the Finder draws.
    fn sample() -> MockOd {
        let mut od = MockOd::default();
        let mut add = |path: &str, kind: SyncItemKind, size: u64, state: SyncItemState, pinned: bool| {
            let (parent, name) = path.rsplit_once('/').map_or(("", path), |(p, n)| (p, n));
            let parent_id = if parent.is_empty() { ROOT_ID.to_string() } else { format!("m:{parent}") };
            od.put(SyncItem {
                id: format!("m:{path}"),
                parent_id,
                path: path.into(),
                name: name.into(),
                kind,
                size,
                mtime_ns: 1_792_000_000_000_000_000,
                state,
                pinned,
                content_version: format!("{size:x}"),
                metadata_version: format!("{size:x}-1"),
                exec: false,
                symlink_target: None,
                conflict_of: None,
            });
        };
        use SyncItemKind::{Dir, File};
        use SyncItemState::{Downloaded, Downloading, ModifiedHere, OnlineOnly};
        add("Photos", Dir, 0, Downloaded, false);
        add("Photos/beach.jpg", File, 3_200_000, OnlineOnly, false);
        add("Photos/sunset.heic", File, 2_100_000, Downloading, false);
        add("Projects", Dir, 0, Downloaded, true);
        add("Projects/plan.md", File, 4_096, Downloaded, true);
        add("Projects/draft.pages", File, 512_000, ModifiedHere, true);
        add("notes.txt", File, 1_024, Downloaded, false);
        od
    }

    fn put(&mut self, i: SyncItem) {
        self.seq += 1;
        self.changed.insert(i.id.clone(), self.seq);
        self.items.insert(i.id.clone(), i);
    }

    fn get(&self, id: &str) -> Result<SyncItem, OdError> {
        self.items.get(id).cloned().ok_or_else(|| no_item(id))
    }

    fn children(&self, id: &str) -> Vec<SyncItem> {
        self.items.values().filter(|i| i.parent_id == id).cloned().collect()
    }

    fn under(&self, id: &str) -> Vec<String> {
        let mut out = vec![id.to_string()];
        let mut i = 0;
        while i < out.len() {
            let parent = out[i].clone();
            out.extend(self.items.values().filter(|c| c.parent_id == parent).map(|c| c.id.clone()));
            i += 1;
        }
        out
    }
}

impl MockSync {
    fn with_od<T>(&self, id: &str, f: impl FnOnce(&mut MockOd) -> Result<T, OdError>) -> Result<T, OdError> {
        let on = lock(&self.state).folders.iter().any(|m| m.f.id == id && m.f.on_demand);
        if !on {
            return Err(not_on_demand());
        }
        let mut od = lock(&self.od);
        f(od.entry(id.to_string()).or_insert_with(MockOd::sample))
    }

    pub(super) fn od_set_mode(&self, id: &str, on: bool) -> Result<SyncFolder, CoreError> {
        let f = self.with(id, |m| {
            m.f.on_demand = on;
            m.f.clone()
        })?;
        if !on {
            lock(&self.od).remove(id);
        }
        Ok(f)
    }

    pub(super) fn od_item(&self, id: &str, item: &str) -> Result<Option<SyncItem>, OdError> {
        self.with_od(id, |od| Ok(od.items.get(item).cloned()))
    }

    pub(super) fn od_children(&self, id: &str, item: &str) -> Result<SyncItemPage, OdError> {
        self.with_od(id, |od| Ok(SyncItemPage { items: od.children(item), next: None }))
    }

    pub(super) fn od_changes(&self, id: &str, anchor: u64) -> Result<SyncChangePage, OdError> {
        self.with_od(id, |od| {
            let items = od.changed.iter().filter(|(_, s)| **s > anchor).filter_map(|(i, _)| od.items.get(i).cloned()).collect();
            let deleted = od.deleted.iter().filter(|(s, _)| *s > anchor).map(|(_, i)| i.clone()).collect();
            Ok(SyncChangePage { items, deleted, anchor: od.seq, more: false })
        })
    }

    /// The mock fetches at once.
    pub(super) fn od_fetch(&self, id: &str, item: &str) -> Result<SyncFetch, OdError> {
        self.with_od(id, |od| {
            let mut ids = od.under(item);
            ids.retain(|i| od.items.get(i).is_some_and(|x| x.kind == SyncItemKind::File && matches!(x.state, SyncItemState::OnlineOnly | SyncItemState::Downloading)));
            for i in ids {
                let mut x = od.get(&i)?;
                x.state = SyncItemState::Downloaded;
                od.put(x);
            }
            let now = od.get(item)?;
            Ok(SyncFetch { local_path: None, done: now.size, total: now.size, item: now, unavailable: Vec::new() })
        })
    }

    pub(super) fn od_pin(&self, id: &str, item: &str, on: bool) -> Result<SyncItem, OdError> {
        self.with_od(id, |od| {
            for i in od.under(item) {
                let mut x = od.get(&i)?;
                x.pinned = on;
                if on && x.state == SyncItemState::OnlineOnly && x.kind == SyncItemKind::File {
                    x.state = SyncItemState::Downloaded;
                }
                od.put(x);
            }
            od.get(item)
        })
    }

    pub(super) fn od_evict(&self, id: &str, item: &str) -> Result<SyncEvicted, OdError> {
        self.with_od(id, |od| {
            let mut e = SyncEvicted::default();
            for i in od.under(item) {
                let mut x = od.get(&i)?;
                if x.kind != SyncItemKind::File || x.state == SyncItemState::OnlineOnly {
                    continue;
                }
                if x.pinned || x.state == SyncItemState::ModifiedHere {
                    e.kept += 1;
                    continue;
                }
                x.state = SyncItemState::OnlineOnly;
                e.files += 1;
                e.bytes += x.size;
                od.put(x);
            }
            Ok(e)
        })
    }

    pub(super) fn od_create(&self, id: &str, parent: &str, name: &str, dir: bool) -> Result<SyncItem, OdError> {
        check_name(name)?;
        self.with_od(id, |od| {
            let parent_path = if parent == ROOT_ID { String::new() } else { od.get(parent)?.path };
            let path = join(&parent_path, name);
            let i = SyncItem {
                id: format!("m:{path}"),
                parent_id: parent.to_string(),
                name: name.into(),
                kind: if dir { SyncItemKind::Dir } else { SyncItemKind::File },
                size: 0,
                mtime_ns: 1_792_000_000_000_000_000,
                state: if dir { SyncItemState::Downloaded } else { SyncItemState::ModifiedHere },
                pinned: false,
                content_version: "0".into(),
                metadata_version: "0-1".into(),
                exec: false,
                symlink_target: None,
                conflict_of: None,
                path,
            };
            od.put(i.clone());
            Ok(i)
        })
    }

    pub(super) fn od_rename(&self, id: &str, item: &str, parent: Option<&str>, name: Option<&str>) -> Result<SyncItem, OdError> {
        if let Some(n) = name {
            check_name(n)?;
        }
        self.with_od(id, |od| {
            let mut x = od.get(item)?;
            if let Some(p) = parent {
                x.parent_id = p.to_string();
            }
            if let Some(n) = name {
                x.name = n.to_string();
            }
            let parent_path = if x.parent_id == ROOT_ID { String::new() } else { od.get(&x.parent_id)?.path };
            x.path = join(&parent_path, &x.name);
            od.put(x.clone());
            Ok(x)
        })
    }

    pub(super) fn od_delete(&self, id: &str, item: &str) -> Result<SyncDeleted, OdError> {
        self.with_od(id, |od| {
            let ids = od.under(item);
            for i in &ids {
                od.items.remove(i);
                od.changed.remove(i);
                od.seq += 1;
                od.deleted.push((od.seq, i.clone()));
            }
            Ok(SyncDeleted { deleted: ids.len() as u64, ..SyncDeleted::default() })
        })
    }
}

// ------------------------------------------------------------------------------------------ the app's calls

/// On-demand folders (SPEC §7G.11): what the File Provider extension (through the app) and Settings call. Items are
/// named by their identifier (`root` is the folder). Calls that change something block (a fetch waits up to
/// `wait_ms`): call them off the main thread.
#[uniffi::export]
impl Core {
    /// Makes a folder on demand (its files become this computer's cache, all of them downloaded) or full again (every
    /// file is fetched; `path`, or the folder's place, must be free).
    pub fn sync_set_on_demand(&self, id: String, on_demand: bool, path: Option<String>) -> Result<SyncFolder, CoreError> {
        self.backend.sync().set_on_demand(&id, on_demand, path.as_deref())
    }

    /// Adds a folder of this computer on demand: its files move into this computer's storage before it ever opens
    /// (all still here), so it is added whole or not at all, and can be tried again (a move cut off is finished at the
    /// next start; one that cannot be finished now keeps the folder, held back, and says so).
    pub fn sync_add_folder_on_demand(&self, path: String) -> Result<SyncFolder, CoreError> {
        self.backend.sync().add_folder_on_demand(&path)
    }

    /// The Add modal's note for `path` made on demand: said differently when the folder is on another disk than
    /// Seedwell's storage (its files are copied there first).
    pub fn sync_on_demand_add_note(&self, path: String) -> String {
        #[cfg(unix)]
        let other = {
            use std::os::unix::fs::MetadataExt;
            let dev = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.dev()).ok();
            matches!((dev(std::path::Path::new(&path)), dev(&self.dir)), (Some(a), Some(b)) if a != b)
        };
        #[cfg(not(unix))]
        let other = {
            let _ = &path;
            false
        };
        crate::sync_text::on_demand_add_note(other)
    }

    /// Joins a folder on demand: nothing is downloaded until asked for; `place` is where the host shows it.
    pub fn sync_join_on_demand(&self, link: String, place: String) -> Result<SyncFolder, CoreError> {
        self.backend.sync().join_on_demand(&link, &place)
    }

    /// One item, or `None` when the identifier names nothing (any more).
    pub fn sync_item(&self, id: String, item: String) -> Result<Option<SyncItem>, CoreError> {
        self.backend.sync().od_item(&id, &item).map_err(CoreError::from)
    }

    /// A page of a folder item's children.
    pub fn sync_children(&self, id: String, item: String, cursor: Option<String>, limit: u32) -> Result<SyncItemPage, CoreError> {
        self.backend.sync().od_children(&id, &item, cursor.as_deref(), limit).map_err(CoreError::from)
    }

    /// What changed since `anchor` (0: everything).
    pub fn sync_changes(&self, id: String, anchor: u64, limit: u32) -> Result<SyncChangePage, CoreError> {
        self.backend.sync().od_changes(&id, anchor, limit).map_err(CoreError::from)
    }

    /// Fetches an item's content (a folder: every file below it) and waits up to `wait_ms` for it (0: only asks).
    pub fn sync_fetch(&self, id: String, item: String, wait_ms: u64) -> Result<SyncFetch, CoreError> {
        self.backend.sync().od_fetch(&id, &item, wait_ms).map_err(CoreError::from)
    }

    /// Takes a fetch back (what is pinned stays wanted).
    pub fn sync_cancel_fetch(&self, id: String, item: String) -> Result<(), CoreError> {
        self.backend.sync().od_cancel(&id, &item).map_err(CoreError::from)
    }

    /// Keeps an item on this computer ("Always Keep on This Device"), or no longer.
    pub fn sync_pin(&self, id: String, item: String, pinned: bool) -> Result<SyncItem, CoreError> {
        self.backend.sync().od_pin(&id, &item, pinned).map_err(CoreError::from)
    }

    /// Frees an item's space ("Remove Download"): it stays, online only. Pinned files and the only copy are kept.
    pub fn sync_evict(&self, id: String, item: String) -> Result<SyncEvicted, CoreError> {
        self.backend.sync().od_evict(&id, &item).map_err(CoreError::from)
    }

    /// The user made a file (its content at `contents`, copied) or folder in `parent`.
    pub fn sync_create_item(&self, id: String, parent: String, name: String, is_dir: bool, contents: Option<String>) -> Result<SyncItem, CoreError> {
        self.backend.sync().od_create(&id, &parent, &name, is_dir, contents.as_deref()).map_err(CoreError::from)
    }

    /// The user moved (`parent`), renamed (`name`) or changed (`contents`, copied) an item. `base_version` is the
    /// item's `content_version` the change of content started from (checked before a rename in the same call): when the item moved on
    /// since (another device's version came first), the content becomes a conflict copy, named as a full folder names
    /// one, whose item is returned; the other version stays. A new file (`sync_create_item`) where something is now
    /// becomes a conflict copy the same way.
    pub fn sync_modify_item(
        &self,
        id: String,
        item: String,
        parent: Option<String>,
        name: Option<String>,
        contents: Option<String>,
        base_version: Option<String>,
    ) -> Result<SyncItem, CoreError> {
        self.backend.sync().od_modify(&id, &item, parent.as_deref(), name.as_deref(), contents.as_deref(), base_version.as_deref()).map_err(CoreError::from)
    }

    /// The user deleted an item (and everything below it), on every device; on a read-only member the delete frees the
    /// content instead (`freed`: the host puts the item back, online only).
    pub fn sync_delete_item(&self, id: String, item: String) -> Result<SyncDeleted, CoreError> {
        self.backend.sync().od_delete(&id, &item, None).map_err(CoreError::from)
    }
}

/// The on-demand calls of [`super::SyncBackend`] for the mock (`--mock`).
pub(super) fn mock_join(m: &MockSync, link: &str, place: &str) -> Result<SyncFolder, CoreError> {
    let mut f = super::SyncBackend::join(m, link, place)?;
    f = m.od_set_mode(&f.id, true)?;
    Ok(f)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use super::*;

    fn core(mock: bool, dir: &std::path::Path) -> Arc<Core> {
        if !mock {
            let store = crate::SettingsStore::new(dir);
            let mut st = store.load_settings();
            st.folders.download_dir = dir.join("done").display().to_string();
            st.folders.incomplete_dir = dir.join("incomplete").display().to_string();
            st.folders.watch.clear();
            st.folders.torrent_after_add = crate::TorrentFileAction::Leave;
            st.network.port_mapping = false;
            st.network.dht = false;
            store.save_settings(&st).unwrap();
        }
        let c = Core::new(crate::CoreOptions { config_dir: dir.display().to_string(), mock, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
        let t0 = Instant::now();
        while !c.sync_snapshot().running {
            assert!(t0.elapsed().as_secs() < 20, "sync did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        c
    }

    #[test]
    fn the_mock_has_an_on_demand_folder_in_every_state() {
        let d = tempfile::tempdir().unwrap();
        let c = core(true, d.path());
        let f = c.sync_snapshot().folders[0].clone();
        assert!(c.sync_children(f.id.clone(), ROOT_ID.into(), None, 100).is_err(), "not on demand yet");
        assert!(c.sync_set_on_demand(f.id.clone(), true, None).unwrap().on_demand);
        let root = c.sync_children(f.id.clone(), ROOT_ID.into(), None, 100).unwrap().items;
        assert_eq!(root.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), ["Photos", "Projects", "notes.txt"]);
        let photos = c.sync_children(f.id.clone(), "m:Photos".into(), None, 100).unwrap().items;
        assert_eq!(photos[0].state, SyncItemState::OnlineOnly);
        let got = c.sync_fetch(f.id.clone(), photos[0].id.clone(), 1000).unwrap();
        assert_eq!(got.item.state, SyncItemState::Downloaded);
        let e = c.sync_evict(f.id.clone(), "m:Projects".into()).unwrap();
        assert_eq!((e.files, e.kept), (0, 2), "pinned, and only here");
        let anchor = c.sync_changes(f.id.clone(), 0, 100).unwrap().anchor;
        let made = c.sync_create_item(f.id.clone(), ROOT_ID.into(), "New".into(), true, None).unwrap();
        let moved = c.sync_modify_item(f.id.clone(), "m:notes.txt".into(), Some(made.id.clone()), Some("n.txt".into()), None, None).unwrap();
        assert_eq!(moved.path, "New/n.txt");
        assert_eq!(c.sync_delete_item(f.id.clone(), "m:Photos".into()).unwrap().deleted, 3);
        let ch = c.sync_changes(f.id.clone(), anchor, 100).unwrap();
        assert_eq!((ch.items.len(), ch.deleted.len()), (2, 3));
    }

    #[test]
    fn the_real_core_runs_an_on_demand_folder_by_item() {
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nothing else here reads the environment.
        unsafe { std::env::set_var("HOME", home.path()) };
        let c = core(false, &work.path().join("state"));
        let docs = work.path().join("Docs");
        std::fs::create_dir_all(docs.join("sub")).unwrap();
        std::fs::write(docs.join("sub/a.txt"), b"hello").unwrap();
        let f = c.sync_add_folder(docs.display().to_string()).unwrap();
        assert!(!f.on_demand);
        let f = c.sync_set_on_demand(f.id.clone(), true, None).unwrap();
        assert!(f.on_demand && !docs.exists());
        let root = c.sync_children(f.id.clone(), ROOT_ID.into(), None, 100).unwrap().items;
        let sub = root.iter().find(|i| i.name == "sub").unwrap().clone();
        assert_eq!(sub.kind, SyncItemKind::Dir);
        let a = c.sync_children(f.id.clone(), sub.id.clone(), None, 100).unwrap().items[0].clone();
        assert_eq!((a.size, a.state), (5, SyncItemState::ModifiedHere));
        let got = c.sync_fetch(f.id.clone(), a.id.clone(), 1000).unwrap();
        assert_eq!(std::fs::read(got.local_path.unwrap()).unwrap(), b"hello");

        // The host's changes, by identifier.
        let src = work.path().join("from-host.txt");
        std::fs::write(&src, b"from the host").unwrap();
        let b = c.sync_create_item(f.id.clone(), sub.id.clone(), "b.txt".into(), false, Some(src.display().to_string())).unwrap();
        assert_eq!((b.path.as_str(), b.size), ("sub/b.txt", 13));
        let n = c.sync_create_item(f.id.clone(), ROOT_ID.into(), "New".into(), true, None).unwrap();
        let moved = c.sync_modify_item(f.id.clone(), a.id.clone(), Some(n.id.clone()), Some("aa.txt".into()), None, None).unwrap();
        assert_eq!((moved.id.as_str(), moved.path.as_str(), moved.parent_id.as_str()), (a.id.as_str(), "New/aa.txt", n.id.as_str()));
        std::fs::write(&src, b"changed by the host").unwrap();
        let changed = c.sync_modify_item(f.id.clone(), b.id.clone(), None, None, Some(src.display().to_string()), Some(b.content_version.clone())).unwrap();
        assert_ne!(changed.content_version, b.content_version);
        // Renamed and changed in one call, from the content it had: no conflict (a rename changes no content).
        std::fs::write(&src, b"changed and renamed").unwrap();
        let both = c
            .sync_modify_item(f.id.clone(), b.id.clone(), None, Some("bb.txt".into()), Some(src.display().to_string()), Some(changed.content_version.clone()))
            .unwrap();
        assert_eq!(both.path, "sub/bb.txt");
        // From content that moved on since: a conflict copy, the other version stays.
        std::fs::write(&src, b"from an old version").unwrap();
        let copy = c.sync_modify_item(f.id.clone(), b.id.clone(), None, None, Some(src.display().to_string()), Some(b.content_version.clone())).unwrap();
        assert!(copy.path.contains("conflict") || copy.conflict_of.is_some(), "{copy:?}");
        assert_eq!(c.sync_delete_item(f.id.clone(), sub.id.clone()).unwrap().deleted, 3, "the folder, bb.txt and the conflict copy");
        assert!(c.sync_item(f.id.clone(), sub.id.clone()).unwrap().is_none());
        assert!(c.sync_delete_item(f.id.clone(), ROOT_ID.into()).is_err(), "the folder itself is not deleted this way");
        assert!(c.sync_create_item(f.id.clone(), ROOT_ID.into(), "a/b".into(), true, None).is_err());
        let p = c.sync_pin(f.id.clone(), n.id.clone(), true).unwrap();
        assert!(p.pinned);
    }

    /// Adding a folder of this computer on demand whose switch is refused takes the add back (its files stay where
    /// they are), so Start can be tried again.
    #[cfg(unix)]
    #[test]
    fn adding_on_demand_that_cannot_switch_adds_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        // SAFETY: set before the core starts any thread; nothing else here reads the environment.
        unsafe { std::env::set_var("HOME", home.path()) };
        let c = core(false, &work.path().join("state"));
        let locked = work.path().join("Locked");
        let docs = locked.join("Docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), b"mine").unwrap();
        // Its files cannot move out of their folder into Seedwell's storage: the switch fails.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = c.sync_add_folder_on_demand(docs.display().to_string());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(r.is_err());
        assert!(c.sync_snapshot().folders.is_empty(), "the add is taken back");
        assert_eq!(std::fs::read(docs.join("a.txt")).unwrap(), b"mine");
        // Tried again: added, on demand.
        assert!(c.sync_add_folder_on_demand(docs.display().to_string()).unwrap().on_demand);
    }

    /// The app's removal with the files to the Trash: an on-demand folder's go from its cache, under its name, never
    /// by way of its place (here one that cannot be written).
    #[cfg(unix)]
    #[test]
    fn the_app_moves_an_on_demand_folder_to_the_trash_from_its_cache() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let trash = work.path().join("Trash");
        std::fs::create_dir_all(&trash).unwrap();
        // SAFETY: set before the core starts any thread; nothing else here reads the environment.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("SEEDWELL_QA_TRASH_DIR", &trash);
        }
        let c = core(false, &work.path().join("state"));
        let locked = work.path().join("Locked");
        let docs = locked.join("Docs");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("a.txt"), b"held here").unwrap();
        let f = c.sync_add_folder(docs.display().to_string()).unwrap();
        c.sync_set_on_demand(f.id.clone(), true, None).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let alert = c.sync_remove(f.id.clone(), true);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(alert.as_ref().is_ok_and(Option::is_none), "{alert:?}");
        assert_eq!(std::fs::read(trash.join("Docs/a.txt")).unwrap(), b"held here");
        assert!(c.sync_snapshot().folders.is_empty());
    }
}
