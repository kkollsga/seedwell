//! One open stream: the file, its playhead aggregator and the urgency it pushes to the source.

use crate::mime;
use crate::source::{DynSource, FileRange, Geometry, OwnerId, StreamId, UrgentRange};
use crate::urgency::{self, PlanConfig};
use std::ops::Range;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::time::Instant;

pub(crate) struct CursorState {
    pub id: u64,
    /// File offset of the next byte this response will send (or, lingering, where it ended).
    pub pos: u64,
    piece: u32,
    /// Set while a socket write is pending: the player is not reading.
    blocked_since: Option<Instant>,
    /// Set when the request completed normally: it keeps counting for a short while so a run
    /// of small ranged requests (AVFoundation) is one window, not a series of jumps.
    linger_until: Option<Instant>,
}

#[derive(Default)]
pub(crate) struct Playhead {
    cursors: Vec<CursorState>,
    /// File-offset ranges of the index still wanted (head, tail, moov, Cues...).
    index: Vec<Range<u64>>,
    last: Vec<UrgentRange>,
    /// When the pending wake-up timer fires, if one is armed.
    timer_at: Option<Instant>,
    closed: bool,
}

pub(crate) struct Stream {
    pub id: StreamId,
    pub token: String,
    pub owner: OwnerId,
    pub source: Arc<dyn DynSource>,
    pub geom: Geometry,
    pub file: FileRange,
    pub content_type: &'static str,
    pub etag: String,
    /// Milliseconds; 0 = unknown.
    duration_ms: AtomicU64,
    pub cancel: watch::Sender<bool>,
    head: Mutex<Playhead>,
    pub cfg: Arc<StreamCfg>,
    rt: Handle,
}

pub(crate) struct StreamCfg {
    pub plan: PlanConfig,
    pub linger: Duration,
    pub stale_after: Duration,
    pub stall_timeout: Duration,
    pub chunk_size: usize,
}

impl Stream {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: StreamId,
        token: String,
        owner: OwnerId,
        source: Arc<dyn DynSource>,
        file: FileRange,
        cfg: Arc<StreamCfg>,
        rt: Handle,
    ) -> Arc<Self> {
        let geom = source.geometry();
        let content_type = mime::content_type(&file.name);
        let etag = format!("\"{}-{}\"", &token[..16.min(token.len())], file.len);
        let (cancel, _) = watch::channel(false);
        Arc::new(Self {
            id,
            token,
            owner,
            source,
            geom,
            file,
            content_type,
            etag,
            duration_ms: AtomicU64::new(0),
            cancel,
            head: Mutex::new(Playhead::default()),
            cfg,
            rt,
        })
    }

    pub fn duration(&self) -> Option<Duration> {
        match self.duration_ms.load(Ordering::Relaxed) {
            0 => None,
            ms => Some(Duration::from_millis(ms)),
        }
    }

    pub fn set_duration(self: &Arc<Self>, d: Duration) {
        self.duration_ms.store((d.as_millis() as u64).max(1), Ordering::Relaxed);
        let mut ph = self.head.lock().unwrap();
        self.replan_locked(&mut ph);
    }

    pub fn is_closed(&self) -> bool {
        *self.cancel.borrow()
    }

    /// File-offset range -> is every piece verified?
    pub fn range_present(&self, r: &Range<u64>) -> bool {
        if r.start >= r.end {
            return true;
        }
        self.geom
            .pieces_of(self.file.offset + r.start..self.file.offset + r.end)
            .all(|p| self.source.has(p))
    }

    pub fn add_index(self: &Arc<Self>, r: Range<u64>) {
        let r = r.start.min(self.file.len)..r.end.min(self.file.len);
        if r.start >= r.end {
            return;
        }
        let mut ph = self.head.lock().unwrap();
        if !ph.index.contains(&r) {
            ph.index.push(r);
        }
        self.replan_locked(&mut ph);
    }

    pub fn index_ranges(&self) -> Vec<Range<u64>> {
        self.head.lock().unwrap().index.clone()
    }

    pub fn clear_index(self: &Arc<Self>) {
        let mut ph = self.head.lock().unwrap();
        ph.index.clear();
        self.replan_locked(&mut ph);
    }

    pub fn cursor_begin(self: &Arc<Self>, id: u64, pos: u64) {
        let mut ph = self.head.lock().unwrap();
        ph.cursors.push(CursorState {
            id,
            pos,
            piece: self.geom.piece_of(self.file.offset + pos),
            blocked_since: None,
            linger_until: None,
        });
        self.replan_locked(&mut ph);
        self.arm_timer(&mut ph);
    }

    /// The cursor moved to file offset `pos`. Replans only when it crosses into a new piece.
    pub fn cursor_advance(self: &Arc<Self>, id: u64, pos: u64) {
        let mut ph = self.head.lock().unwrap();
        let piece = self.geom.piece_of(self.file.offset + pos);
        let Some(c) = ph.cursors.iter_mut().find(|c| c.id == id) else { return };
        let was_stale = c.blocked_since.is_some_and(|t| t.elapsed() >= self.cfg.stale_after);
        c.pos = pos;
        c.blocked_since = None;
        if piece != c.piece || was_stale {
            c.piece = piece;
            self.replan_locked(&mut ph);
        }
    }

    pub fn cursor_blocked(self: &Arc<Self>, id: u64, blocked: bool) {
        let mut ph = self.head.lock().unwrap();
        let Some(c) = ph.cursors.iter_mut().find(|c| c.id == id) else { return };
        c.blocked_since = blocked.then(Instant::now);
        if blocked {
            self.arm_timer(&mut ph);
        }
    }

    /// The response ended. `completed`: it sent everything, so it lingers briefly.
    pub fn cursor_end(self: &Arc<Self>, id: u64, completed: bool, end_pos: u64) {
        let mut ph = self.head.lock().unwrap();
        if completed {
            if let Some(c) = ph.cursors.iter_mut().find(|c| c.id == id) {
                c.pos = end_pos;
                c.blocked_since = None;
                c.linger_until = Some(Instant::now() + self.cfg.linger);
            }
        } else {
            ph.cursors.retain(|c| c.id != id);
        }
        self.replan_locked(&mut ph);
        self.arm_timer(&mut ph);
    }

    /// Close: clear all hints, wake every response.
    pub fn close(&self) {
        {
            let mut ph = self.head.lock().unwrap();
            ph.closed = true;
            ph.cursors.clear();
            ph.index.clear();
            ph.last.clear();
        }
        self.source.clear(self.id);
        self.cancel.send_replace(true);
    }

    /// The last urgency pushed to the source (for tests and diagnostics).
    pub fn last_pushed(&self) -> Vec<UrgentRange> {
        self.head.lock().unwrap().last.clone()
    }

    fn replan_locked(&self, ph: &mut Playhead) {
        if ph.closed {
            return;
        }
        let now = Instant::now();
        ph.cursors.retain(|c| c.linger_until.is_none_or(|t| t > now));
        let stale = self.cfg.stale_after;
        let positions: Vec<u64> = ph
            .cursors
            .iter()
            .filter(|c| !c.blocked_since.is_some_and(|t| now >= t + stale))
            .map(|c| c.pos)
            .collect();
        let before = ph.index.len();
        let missing: Vec<Range<u64>> = ph.index.iter().filter(|r| !self.range_present(r)).cloned().collect();
        if missing.len() != before {
            ph.index = missing;
        }
        let bps = urgency::bitrate(self.file.len, self.duration(), &self.cfg.plan);
        let ranges = urgency::plan(self.geom, &self.file, &positions, &ph.index, bps, &self.cfg.plan);
        if ranges == ph.last {
            return;
        }
        if ranges.is_empty() {
            self.source.clear(self.id);
        } else {
            self.source.set_urgent(self.id, &ranges);
        }
        ph.last = ranges;
    }

    /// Wake once at the next lingering/stale expiry so the picture is refreshed without any
    /// further event. One timer task per stream at a time.
    fn arm_timer(self: &Arc<Self>, ph: &mut Playhead) {
        if ph.closed {
            return;
        }
        let stale = self.cfg.stale_after;
        let now = Instant::now();
        let next = ph
            .cursors
            .iter()
            .flat_map(|c| [c.linger_until, c.blocked_since.map(|t| t + stale)])
            .flatten()
            .filter(|t| *t > now)
            .min();
        let Some(at) = next else { return };
        if ph.timer_at.is_some_and(|t| t > now && t <= at) {
            return; // an earlier wake-up is already pending and will re-arm
        }
        ph.timer_at = Some(at);
        let s = self.clone();
        self.rt.spawn(async move {
            tokio::time::sleep_until(at + Duration::from_millis(5)).await;
            let mut ph = s.head.lock().unwrap();
            if ph.timer_at == Some(at) {
                ph.timer_at = None;
            }
            s.replan_locked(&mut ph);
            s.arm_timer(&mut ph);
        });
    }
}

/// Ends the cursor on every exit path of a response, including cancellation.
pub(crate) struct CursorGuard {
    pub stream: Arc<Stream>,
    pub id: u64,
    pub completed: bool,
    pub pos: u64,
    pub live: Arc<std::sync::atomic::AtomicUsize>,
}

impl Drop for CursorGuard {
    fn drop(&mut self) {
        self.stream.cursor_end(self.id, self.completed, self.pos);
        self.live.fetch_sub(1, Ordering::Relaxed);
    }
}
