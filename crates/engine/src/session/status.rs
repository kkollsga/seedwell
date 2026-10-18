//! Organizing finished torrents by sharing state (SPEC 7A.3).
//!
//! With `folders.by_status` on, a finished torrent lives in `Active/`, `Dormant/`, `Stopped/` or
//! `Dead/` below its finished location. The folders follow the Sharing groups (SPEC 8.3), checked
//! at most once an hour (`EngineConfig::status_interval`), so that nothing moves back and forth:
//!
//! * a torrent that is uploading at that moment stays where it is;
//! * a torrent moves to another status folder only when two checks in a row agree (Stopped and Dead
//!   do not wait for the second check);
//! * a torrent that is in no status folder (it finished before the switch was turned on, and the
//!   user declined the move) is left alone: filing existing torrents is the confirmed move of
//!   SPEC 7A.5, and a download that finishes is already in `Active/`;
//! * Error is transient: such a torrent stays where it is;
//! * downloads in progress are never touched (they live in the incomplete location), and neither
//!   are torrents the user placed (pinned) or earlier versions placed file by file.
//!
//! A move is the ordinary relocation of SPEC 7A.5: journaled, same-volume renames, never an
//! overwrite, a millisecond pause of that torrent.

use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use super::cold::*;
use super::{Job, Session};
use crate::moveplan::MoveTarget;
use crate::persist::Placed;
use crate::status::{StatusFolder, split_status};
use crate::util::unix_secs;

impl Session {
    /// The status folder `id` belongs in by its sharing state right now (`None`: Error, or not a
    /// finished torrent).
    pub(super) fn live_status_folder(&self, id: &crate::api::TorrentId) -> Option<StatusFolder> {
        let e = self.table.get(id)?;
        let c = &e.cold;
        if !c.finished() || c.has(F_STAGED) || c.awaiting() || self.directs.contains_key(id) {
            return None;
        }
        let stopped = matches!(c.want, Want::Paused | Want::GoalReached);
        let group = self.seed_group(c, e.live.as_ref().map(|l| &*l.stats), unix_secs());
        StatusFolder::of(group, stopped)
    }

    /// One check: queue the moves that are due, remember what was seen, arm the next check.
    pub(super) fn status_check(&mut self) {
        let cfg = self.sh.cfg();
        if !self.shutting_down {
            self.schedule_after(cfg.status_interval.max(Duration::from_millis(50)), Job::StatusCheck);
        }
        if !cfg.by_status || self.shutting_down {
            self.status_seen.clear();
            return;
        }
        let mut seen = HashMap::new();
        let mut due = Vec::new();
        self.mover.auto_failed.retain(|id, _| self.table.contains_key(id));
        for (id, e) in &self.table {
            let c = &e.cold;
            if c.has2(G_PINNED) || c.placed() == Placed::Renamed || self.is_moving(id) {
                continue;
            }
            let Some(want) = self.live_status_folder(id) else { continue };
            seen.insert(*id, want);
            let (_, current) = split_status(&self.save_path(c));
            if current.is_none_or(|cur| cur == want) {
                self.mover.auto_failed.remove(id);
                continue;
            }
            let settled = matches!(want, StatusFolder::Stopped | StatusFolder::Dead) || self.status_seen.get(id) == Some(&want);
            let uploading = e.live.as_ref().is_some_and(|l| l.stats.uploading.load(Relaxed) > 0);
            if settled && !uploading {
                due.push((*id, want));
            }
        }
        self.status_seen = seen;
        for (id, folder) in due {
            self.queue_moves(vec![id], MoveTarget::Status(folder));
        }
    }
}
