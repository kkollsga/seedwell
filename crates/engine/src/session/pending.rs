//! The grace period before an auto-added torrent starts (SPEC 7A.7).
//!
//! A torrent that would start without asking is added *held*: the same silent state as a torrent
//! awaiting a choice (`G_AWAITING`: no tracker announce, no DHT, no peer, no metadata fetch, nothing
//! allocated), marked `H3_PENDING` and given a countdown. The deadline lives here, in the session,
//! so it holds with no window open; at the deadline the torrent starts exactly as the add would
//! have. The mark is persisted: after a restart the torrent is held again with a fresh countdown,
//! and nothing ever starts without the countdown having run.

use std::time::{Duration, Instant};

use super::cold::*;
use super::{Job, Session};
use crate::api::*;
use crate::util::unix_ms;

/// A resumed countdown has at least this long to run: a resume that starts the torrent a
/// heartbeat later would feel like an accident.
const RESUME_FLOOR: Duration = Duration::from_secs(3);
/// The countdown after a restart when the setting is off (0): the default.
const RESTART_FALLBACK_SECS: u32 = 10;

/// What a caller can do with a pending torrent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingAct {
    Pause,
    Resume,
    StartNow,
    /// Cancel the countdown and keep the torrent held for the chooser.
    Choose,
}

/// One countdown.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Countdown {
    /// When it ends (while running).
    deadline: Instant,
    /// Time left, while stopped.
    stopped_with: Option<Duration>,
}

impl Session {
    /// The grace period for a `.torrent` that needs no choice, or `None` when it starts at once.
    pub(super) fn grace_for_torrent(&self, opts: &AddOptions) -> Option<Duration> {
        let secs = self.sh.cfg().start_delay_secs;
        let skip = secs == 0 || opts.paused || opts.start_now || opts.hold == Some(false) || !opts.file_priorities.is_empty();
        (!skip).then(|| Duration::from_secs(u64::from(secs)))
    }

    /// The grace period for a magnet link, which has no file list yet. A mode that always asks
    /// shows its chooser anyway, so there is no countdown in front of it.
    pub(super) fn grace_for_magnet(&self, opts: &AddOptions) -> Option<Duration> {
        if self.sh.cfg().smart_add.mode == crate::config::AskMode::Always || opts.hold == Some(true) {
            return None;
        }
        self.grace_for_torrent(opts)
    }

    pub(super) fn restart_delay(&self) -> Duration {
        let s = self.sh.cfg().start_delay_secs;
        Duration::from_secs(u64::from(if s == 0 { RESTART_FALLBACK_SECS } else { s }))
    }

    /// Start the countdown of a torrent whose cold record is marked pending.
    pub(super) fn arm_pending(&mut self, id: TorrentId, delay: Duration) {
        let deadline = Instant::now() + delay;
        self.pending.insert(id, Countdown { deadline, stopped_with: None });
        self.schedule(deadline, Job::PendingDue(id));
    }

    /// The row's view of a countdown.
    pub(super) fn pending_status(&self, id: &TorrentId) -> Option<PendingStatus> {
        if !self.table.get(id).is_some_and(|e| e.cold.pending()) {
            return None;
        }
        let c = self.pending.get(id)?;
        let now = Instant::now();
        let left = c.stopped_with.unwrap_or_else(|| c.deadline.saturating_duration_since(now));
        Some(PendingStatus {
            deadline_ms: unix_ms() + left.as_millis() as u64,
            remaining_ms: left.as_millis() as u64,
            paused: c.stopped_with.is_some(),
        })
    }

    /// Leave the pending mark and the countdown (the torrent stays held until something else
    /// releases it).
    fn clear_pending(&mut self, id: TorrentId) {
        self.pending.remove(&id);
        if let Some(e) = self.table.get_mut(&id) {
            e.cold.flags3 &= !H3_PENDING;
        }
        self.mark_record(id);
    }

    /// Called when a chooser's confirmation releases a pending torrent.
    pub(super) fn forget_pending(&mut self, id: TorrentId) {
        if self.table.get(&id).is_some_and(|e| e.cold.pending()) {
            self.clear_pending(id);
        }
    }

    /// The deadline job: start the torrent if its countdown really is over.
    pub(super) fn pending_due(&mut self, id: TorrentId) {
        if self.shutting_down {
            return;
        }
        let over = self
            .pending
            .get(&id)
            .is_some_and(|c| c.stopped_with.is_none() && c.deadline <= Instant::now() + Duration::from_millis(5));
        if !over || !self.table.get(&id).is_some_and(|e| e.cold.pending()) {
            return;
        }
        self.start_pending(id, true);
    }

    /// The grace period is over (or the user said "start now"): release the torrent. What it does
    /// next is what the add would have done at once.
    fn start_pending(&mut self, id: TorrentId, announce: bool) {
        self.clear_pending(id);
        self.sh.release_source(&id);
        if let Some(e) = self.table.get_mut(&id) {
            e.cold.set2(G_AWAITING, false);
        }
        if announce {
            self.sh.emit(EngineEvent::Pending { id, change: PendingChange::Started });
        }
        self.replan();
        self.reconcile(id);
        if let Some(e) = self.table.get(&id) {
            let st = e.cold.state(e.live.as_ref().map(|l| &*l.stats));
            self.sh.emit(EngineEvent::StateChanged { id, state: st });
        }
        if self.table.get(&id).is_some_and(|e| self.dht_ok(&e.cold)) {
            self.dht_lookup(id, true);
        }
    }

    /// Pause, resume, start now or choose. Returns, for `Choose`, whether the chooser can open
    /// now (the file list is known); a magnet link has to fetch its metadata first and the
    /// chooser follows as soon as it is in.
    pub(super) fn pending_act(&mut self, id: TorrentId, act: PendingAct) -> Result<bool, EngineError> {
        let e = self.table.get(&id).ok_or(EngineError::NotFound)?;
        if !e.cold.pending() {
            return Err(EngineError::Invalid("the torrent is not waiting to start".into()));
        }
        let now = Instant::now();
        match act {
            PendingAct::Pause => {
                if let Some(c) = self.pending.get_mut(&id) {
                    if c.stopped_with.is_none() {
                        c.stopped_with = Some(c.deadline.saturating_duration_since(now));
                        self.sh.emit(EngineEvent::Pending { id, change: PendingChange::Paused });
                    }
                }
                Ok(false)
            }
            PendingAct::Resume => {
                let mut at = None;
                if let Some(c) = self.pending.get_mut(&id) {
                    if let Some(left) = c.stopped_with.take() {
                        c.deadline = now + left.max(RESUME_FLOOR);
                        at = Some(c.deadline);
                    }
                }
                if let Some(at) = at {
                    self.schedule(at, Job::PendingDue(id));
                    self.sh.emit(EngineEvent::Pending { id, change: PendingChange::Resumed });
                }
                Ok(false)
            }
            PendingAct::StartNow => {
                self.start_pending(id, true);
                Ok(false)
            }
            PendingAct::Choose => {
                let has_meta = e.cold.has_metadata();
                if has_meta {
                    // Stays held, now as a plain "awaiting choice": the chooser is the way out.
                    self.clear_pending(id);
                    self.sh.emit(EngineEvent::Pending { id, change: PendingChange::Chosen });
                    self.sh.emit(EngineEvent::StateChanged { id, state: TorrentState::AwaitingChoice });
                    Ok(true)
                } else {
                    // A magnet: the file list needs peers. Fetch the metadata now, with the hold
                    // forced so that the chooser opens as soon as it is in.
                    if let Some(e) = self.table.get_mut(&id) {
                        e.cold.flags3 |= H3_FORCE_HOLD;
                    }
                    self.sh.emit(EngineEvent::Pending { id, change: PendingChange::Chosen });
                    self.start_pending(id, false);
                    Ok(false)
                }
            }
        }
    }
}
