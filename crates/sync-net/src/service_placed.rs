//! On-demand folders kept in place (SPEC §7G.11, [`crate::placed`]): what a host whose placeholders hold the content
//! (Windows' Cloud Files) calls besides the on-demand calls, and the host's change-feed anchor, kept with the folder.

use std::io;
use std::sync::Arc;

use trs_sync::fs::{Fs, StdFs};
use trs_sync::index::IndexStore;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::model::FileKind;

use super::{OnDemandError, SyncError, SyncHandle, invalid, lock, on_demand};
use crate::placed::{Mark, Note, Placed, holds_content, mark_of_stamp};
use crate::shared::FolderShared;

/// What a placeholder holds, against the core's record of its item: the host asks before it reports a change or
/// throws a placeholder's content away.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalChange {
    /// The item's content as the core records it: nothing to report.
    Unchanged,
    /// The user changed it: the host reports it ([`SyncHandle::od_write`] from the placeholder) with this base, the
    /// content version the user started from (`None`: not known; the change then becomes a conflict copy where
    /// something else is now, so nothing is overwritten).
    Modified {
        /// The content version the change started from.
        base: Option<String>,
    },
    /// An earlier version, or content this device let go of, unchanged since the core put it there: the host may
    /// dehydrate it (and fill it again if the item is wanted).
    Stale,
}

/// Why `p` is no path the host's calls name.
fn check(path: &str) -> Result<(), SyncError> {
    if path.is_empty() {
        return Err(invalid("the folder itself is not a file"));
    }
    trs_sync_proto::model::validate_path(path).map_err(|_| invalid(format!("{path:?} is not a path in the folder")))
}

/// The placeholder at `at` of `place`, when it holds content.
fn content_at(place: &StdFs, at: &str) -> io::Result<Option<trs_sync::fs::Meta>> {
    Ok(place.lstat(at)?.filter(|m| m.kind == FileKind::File && !m.dataless && !m.other))
}

impl SyncHandle {
    fn placed_folder(&self, id: &FolderId) -> Result<(Arc<FolderShared>, Arc<Placed>), SyncError> {
        let f = self.inner.ctx.registry.folder(id).ok_or(SyncError::NotFound)?;
        if !f.on_demand {
            return Err(on_demand(OnDemandError::NotOnDemand, "the folder is not on demand"));
        }
        let p = f.placed.clone().ok_or_else(|| invalid("this folder keeps its content in Seedwell's storage, not in place"))?;
        Ok((f.clone(), p))
    }

    /// Whether on-demand folder `id` keeps its content in place (in the host's placeholders), from its settings.
    pub fn od_in_place(&self, id: &FolderId) -> Result<bool, SyncError> {
        let e = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if !e.on_demand {
            return Err(on_demand(OnDemandError::NotOnDemand, "the folder is not on demand"));
        }
        Ok(e.in_place)
    }

    /// Keeps on-demand folder `id`'s content in place (`on`: the host's placeholders hold it, the cache only stages
    /// it) or in the cache. Saved with the folder; the folder starts again to take it. Off, the content of every file
    /// held in a placeholder comes back into the cache first (moved: the host lets go of its placeholders before).
    pub async fn od_set_in_place(&self, id: &FolderId, on: bool) -> Result<(), SyncError> {
        self.refuse_held(id)?;
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if !entry.on_demand {
            return Err(on_demand(OnDemandError::NotOnDemand, "the folder is not on demand"));
        }
        if entry.in_place == on {
            return Ok(());
        }
        if !on {
            self.take_back_placed(id).await?;
        }
        let mut entry2 = entry.clone();
        entry2.in_place = on;
        if let Some(e) = lock(&self.inner.cfg).folder_mut(&id.to_string()) {
            e.in_place = on;
        }
        self.save_config()?;
        if !on {
            Placed::remove_log(&self.placed_path(id));
        }
        match self.stop_folder(id).await {
            Ok((_, closed)) if closed.load(std::sync::atomic::Ordering::SeqCst) => self.open_folder(&entry2).await,
            // Still busy: it opens with the setting once it is closed.
            Ok((_, closed)) => {
                self.reopen_when_closed(closed, entry2);
                Ok(())
            }
            Err(SyncError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Whether the content of `path` is in its placeholder (the core holds no copy of it).
    pub fn od_held_in_place(&self, id: &FolderId, path: &str) -> bool {
        self.inner.ctx.registry.folder(id).and_then(|f| f.placed.clone()).is_some_and(|p| p.is_held(path))
    }

    /// A fetch handed the host the content of `path` as it is now (a folder kept in place: its placeholder is checked
    /// against this version when the host says it is filled, even if a newer one arrives meanwhile).
    pub fn od_hand(&self, id: &FolderId, path: &str) {
        let Some(f) = self.inner.ctx.registry.folder(id) else { return };
        let Some(p) = f.placed.clone() else { return };
        if let Ok(Some(r)) = f.index.view().local(path)
            && !r.dataless
            && r.file.info.kind == FileKind::File
        {
            p.hand(path, r.file.info);
        }
    }

    /// The content of `path` kept in place is not in its placeholder any more (dehydrated or deleted behind the core's
    /// back): the core lets go of it and looks at it again, so it is online only (fetched again when wanted).
    pub(crate) async fn od_lost_in_place(&self, f: &Arc<FolderShared>, path: &str) -> Result<(), SyncError> {
        let Some(placed) = f.placed.clone() else { return Ok(()) };
        let Some(n) = placed.held(path) else { return Ok(()) };
        if placed.locate(path, &n)?.is_some() {
            return Ok(());
        }
        let p = path.to_string();
        self.with_core(f, move |c| {
            placed.let_go(&p)?;
            c.scan(&p)?;
            Ok(())
        })
        .await
    }

    /// The host filled the placeholder of `path` with the content `content_version` a fetch handed it: once the
    /// placeholder is checked to hold exactly that content (every chunk), the core's copy in the cache goes and the
    /// placeholder is where the core reads it from. Refused (`Changed`) when a newer version arrived meanwhile or the
    /// placeholder holds something else (the user writing to it already): the placeholder is noted either way, so
    /// [`Self::od_local_change`] tells an earlier version from the user's change.
    pub async fn od_hydrated(&self, id: &FolderId, path: &str, content_version: &str) -> Result<(), SyncError> {
        check(path)?;
        let (f, placed) = self.placed_folder(id)?;
        let (p, cv) = (path.to_string(), content_version.to_string());
        let handed = placed.handed(&p, &cv);
        // Read first, without the folder's core (a large file takes a while); checked again on it below.
        let (pl2, p2, cv2, f2) = (placed.clone(), p.clone(), cv.clone(), f.clone());
        let checked = tokio::task::spawn_blocking(move || -> io::Result<Option<(Mark, bool)>> {
            let place = StdFs::new(pl2.place());
            let Some(before) = content_at(&place, &p2)? else { return Ok(None) };
            let rec = f2.index.view().local(&p2)?.filter(|r| trs_sync::ondemand::live(r) && !r.dataless && r.file.info.kind == FileKind::File);
            let info = match rec {
                Some(r) if trs_sync::ondemand::content_version_of(&r.file.info) == cv2 => Some(r.file.info),
                _ => handed,
            };
            let same = match &info {
                Some(i) => holds_content(&place, &p2, i)?,
                None => false,
            };
            // Unchanged while it was read.
            let after = content_at(&place, &p2)?;
            let still = after.as_ref().is_some_and(|a| Mark::of(a).same(&Mark::of(&before)));
            Ok(Some((Mark::of(&before), same && still)))
        })
        .await
        .map_err(io::Error::other)??;
        let Some((mark, same)) = checked else {
            return Err(on_demand(OnDemandError::Changed, format!("{path}: its placeholder holds no content")));
        };
        let root = f.root.clone();
        let adopted = self
            .with_core(&f, move |c| {
                let rec = c.store().local(&p)?.filter(|r| trs_sync::ondemand::live(r) && !r.dataless && r.file.info.kind == FileKind::File);
                let current = rec.filter(|r| trs_sync::ondemand::content_version_of(&r.file.info) == cv);
                let cache = StdFs::new(root);
                let note = |real: Option<Mark>, shown: Mark, exec: bool, held: bool| Note { at: p.clone(), real, shown, exec, content: cv.clone(), held };
                let Some(r) = current else {
                    // A newer version came first: the placeholder holds an earlier one (as handed, when that is known).
                    placed.put(&p, note(same.then_some(mark), mark, false, false))?;
                    return Ok(Some(format!("{p}: a newer version arrived meanwhile")));
                };
                if placed.is_held(&p) {
                    return Ok(None);
                }
                let cm = cache.lstat(&p)?;
                let recorded = cm.as_ref().is_some_and(|m| r.stamp == Some(trs_sync::index::Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode }));
                // Still the placeholder that was read (the user may write to it any time).
                let unchanged = content_at(&StdFs::new(placed.place()), &p)?.is_some_and(|m| Mark::of(&m).same(&mark));
                let (Some(cm), true, true, Some(stamp)) = (cm, recorded, same && unchanged, r.stamp) else {
                    placed.put(&p, note(None, mark, false, false))?;
                    return Ok(Some(format!("{p}: its placeholder does not hold this version")));
                };
                // Noted (on disk) before the cache's copy goes: a stop in between leaves both, and the cache's wins.
                placed.put(&p, note(Some(mark), mark_of_stamp(&stamp), cm.exec, true))?;
                if let Err(e) = cache.remove_file(&p) {
                    placed.let_go(&p)?;
                    return Err(e.into());
                }
                Ok(None)
            })
            .await?;
        match adopted {
            None => Ok(()),
            Some(why) => Err(on_demand(OnDemandError::Changed, why)),
        }
    }

    /// What the placeholder of `path` holds against the core's record (the host asks when a filled placeholder was
    /// written and closed, and before it dehydrates or overwrites one the core did not ask it to): see
    /// [`LocalChange`]. A placeholder without content has nothing to say ([`LocalChange::Unchanged`]).
    pub async fn od_local_change(&self, id: &FolderId, path: &str) -> Result<LocalChange, SyncError> {
        check(path)?;
        let (f, placed) = self.placed_folder(id)?;
        let p = path.to_string();
        tokio::task::spawn_blocking(move || -> Result<LocalChange, SyncError> {
            let note = placed.note(&p);
            let found = match &note {
                Some(n) => placed.locate(&p, n)?,
                None => content_at(&StdFs::new(placed.place()), &p)?.map(|m| (p.clone(), m)),
            };
            let Some((at, m)) = found else {
                if note.as_ref().is_some_and(|n| !n.held) {
                    placed.forget(&p)?;
                }
                return Ok(LocalChange::Unchanged);
            };
            let rec = f.index.view().local(&p)?.filter(|r| trs_sync::ondemand::live(r) && r.file.info.kind == FileKind::File);
            let here = rec.as_ref().filter(|r| !r.dataless);
            let mark = Mark::of(&m);
            if let Some(n) = &note
                && n.real.is_some_and(|r| r.same(&mark))
            {
                let current = here.is_some_and(|r| trs_sync::ondemand::content_version_of(&r.file.info) == n.content);
                return Ok(if n.held || current { LocalChange::Unchanged } else { LocalChange::Stale });
            }
            // Changed since it was filled, or never noted: its content says.
            if let Some(r) = &rec
                && holds_content(&StdFs::new(placed.place()), &at, &r.file.info)?
            {
                if r.dataless {
                    return Ok(LocalChange::Stale);
                }
                if let Some(n) = note {
                    // The same content as recorded: the note follows the file as it is now.
                    let shown = if n.held { r.stamp.as_ref().map_or(n.shown, mark_of_stamp) } else { n.shown };
                    placed.put(&p, Note { real: Some(mark), shown, content: trs_sync::ondemand::content_version_of(&r.file.info), ..n })?;
                }
                return Ok(LocalChange::Unchanged);
            }
            Ok(LocalChange::Modified { base: note.map(|n| n.content) })
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Before an on-demand folder kept in place stops being so (switched to full, removed, kept in the cache): the
    /// content held in placeholders comes back into the cache ([`Placed::take_all_back`]), on the folder's core when
    /// it runs. The host has let go of its placeholders by then (they are plain files, or gone).
    pub(crate) async fn take_back_placed(&self, id: &FolderId) -> Result<u64, SyncError> {
        let entry = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if !(entry.on_demand && entry.in_place) {
            return Ok(0);
        }
        if let Some(f) = self.inner.ctx.registry.folder(id)
            && let Some(placed) = f.placed.clone()
        {
            return self.with_core(&f, move |_| Ok(placed.take_all_back()?)).await;
        }
        let log = self.placed_path(id);
        if !log.exists() {
            return Ok(0);
        }
        let placed = Placed::open(entry.path.clone(), self.cache_path(id), log)?;
        Ok(tokio::task::spawn_blocking(move || placed.take_all_back()).await.map_err(io::Error::other)??)
    }

    /// The host's last change-feed anchor of on-demand folder `id` (`None`: it has none; it lists the folder whole).
    pub fn od_host_anchor(&self, id: &FolderId) -> Result<Option<u64>, SyncError> {
        let e = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if !e.on_demand {
            return Err(on_demand(OnDemandError::NotOnDemand, "the folder is not on demand"));
        }
        match std::fs::read_to_string(self.anchor_path(id)) {
            Ok(t) => Ok(t.trim().parse().ok()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Keeps `anchor` as the host's last change-feed anchor of on-demand folder `id` (the host has applied every
    /// change up to it). Kept beside the folder's index, and deleted with the folder.
    pub fn od_set_host_anchor(&self, id: &FolderId, anchor: u64) -> Result<(), SyncError> {
        let e = lock(&self.inner.cfg).folder(&id.to_string()).cloned().ok_or(SyncError::NotFound)?;
        if !e.on_demand {
            return Err(on_demand(OnDemandError::NotOnDemand, "the folder is not on demand"));
        }
        let path = self.anchor_path(id);
        let tmp = path.with_extension("anchor-new");
        std::fs::write(&tmp, anchor.to_string())?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Forgets the host's anchor (its tree goes: switched to full, or to on demand anew).
    pub(crate) fn drop_host_anchor(&self, id: &FolderId) {
        let _ = std::fs::remove_file(self.anchor_path(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_named_by_the_host_is_one_in_the_folder() {
        assert!(check("").is_err());
        assert!(check("/abs").is_err());
        assert!(check("a/../b").is_err());
        assert!(check("a/b.txt").is_ok());
    }
}
