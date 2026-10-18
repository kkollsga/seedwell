//! Display titles (`ColdTorrent::title`): computed when a torrent is added or its metadata arrives
//! (`organize::plan`), recomputed when its category changes, and filled in once for torrents stored
//! before titles were persisted. The work reads the stored `.torrent` off the actor.

use std::time::Duration;

use trs_classify::Category;
use trs_metainfo::Metainfo;

use super::{Session, SessionMsg};
use crate::api::TorrentId;
use crate::organize;

/// Titles sent back to the session per message (keeps the actor responsive during the one-time fill).
const BATCH: usize = 128;

/// The title of the stored torrent `id` in `cat`, or `None` when its file cannot be read.
fn compute(
    store: &dyn crate::persist::Store,
    id: &TorrentId,
    cat: Option<Category>,
) -> Option<String> {
    let bytes = store.load_torrent_file(id).ok().flatten()?;
    let mi = Metainfo::from_bytes(&bytes).ok()?;
    let real: Vec<usize> = mi
        .files()
        .filter(|f| !f.is_padding())
        .map(|f| f.index)
        .collect();
    Some(organize::title_of(&mi, &real, cat))
}

impl Session {
    /// Recompute the titles of `ids` (their category changed) off the actor.
    pub(super) fn retitle(&self, ids: Vec<(TorrentId, Option<Category>)>) {
        if ids.is_empty() {
            return;
        }
        let sh = self.sh.clone();
        tokio::task::spawn_blocking(move || {
            for chunk in ids.chunks(BATCH) {
                let done: Vec<_> = chunk
                    .iter()
                    .filter_map(|(id, cat)| {
                        compute(sh.store.as_ref(), id, *cat).map(|t| (*id, *cat, t))
                    })
                    .collect();
                if !done.is_empty() {
                    let _ = sh.session.blocking_send(SessionMsg::Titled(done));
                }
            }
        });
    }

    /// Torrents stored before titles were persisted get theirs once, a moment after start.
    pub(super) fn backfill_titles(&self) {
        let todo: Vec<_> = self
            .table
            .iter()
            .filter(|(_, e)| e.cold.title.is_none() && e.cold.has_metadata())
            .map(|(id, e)| (*id, e.cold.category()))
            .collect();
        if todo.is_empty() {
            return;
        }
        let sh = self.sh.clone();
        tokio::task::spawn_blocking(move || {
            // Not in the way of the start-up work.
            std::thread::sleep(Duration::from_secs(2));
            for chunk in todo.chunks(BATCH) {
                let done: Vec<_> = chunk
                    .iter()
                    .filter_map(|(id, cat)| {
                        compute(sh.store.as_ref(), id, *cat).map(|t| (*id, *cat, t))
                    })
                    .collect();
                if !done.is_empty() {
                    let _ = sh.session.blocking_send(SessionMsg::Titled(done));
                }
            }
        });
    }

    /// Titles arrived: keep those whose category still holds.
    pub(super) fn on_titled(&mut self, v: Vec<(TorrentId, Option<Category>, String)>) {
        for (id, cat, title) in v {
            let Some(e) = self.table.get_mut(&id) else {
                continue;
            };
            if e.cold.category() != cat || e.cold.title.as_deref() == Some(title.as_str()) {
                continue;
            }
            let for_log = self.sh.devlog.enabled().then(|| title.clone());
            e.cold.title = Some(title.into());
            self.mark_record(id);
            if let Some(t) = for_log {
                self.tracker_send(crate::tracker_driver::TrackerCmd::Title(id, t));
            }
        }
    }
}
