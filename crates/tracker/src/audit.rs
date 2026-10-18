//! Independent runtime audit of the wire-level invariants (SPEC 7C.3: 1-4, 6).
//!
//! The `AnnounceMachine` audits itself before emitting; the `HostGate` runs this
//! second, separately written check on every request it is about to release
//! and on every outcome. Two implementations that must agree make a silent
//! logic bug much less likely. On a violation the gate fails closed.

use crate::machine::{MANUAL_FLOOR, SendKind};
use crate::response::Timing;
use crate::time::Millis;
use crate::types::{AnnounceParams, Event, PeerId20};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Session {
    Closed,
    Open { started_ok: bool },
    Stopping,
}

/// What is only needed while a session is open; allocated by `Started` and
/// dropped when the session is over, so an idle pair costs no extra block.
#[derive(Clone, Debug, Default)]
struct Detail {
    last_up: u64,
    last_down: u64,
    tracker_id: Option<Box<[u8]>>,
    last_success: Option<(Millis, Timing)>,
}

/// Per-(torrent, tracker) audit state.
///
/// size: 16 bytes while idle (checked by a test), plus ~80 bytes for the
/// duration of a session.
#[derive(Clone, Debug)]
pub struct Auditor {
    session: Session,
    completed_ever: bool,
    /// Hash of `(key, peer_id)`; 0 = not seen yet. Stable across sessions
    /// (invariant 6: constant for the process session). 32 bits are plenty to
    /// notice a *change*.
    identity: u16,
    /// Opaque number the owner (the gate) keeps with the auditor; it lives
    /// here because the struct has room for it.
    tag: u32,
    detail: Option<Box<Detail>>,
}

impl Default for Auditor {
    fn default() -> Self {
        Auditor {
            session: Session::Closed,
            completed_ever: false,
            identity: 0,
            tag: 0,
            detail: None,
        }
    }
}

fn identity_hash(key: u32, peer_id: &PeerId20) -> u16 {
    // FNV-1a folded to 16 bits: an equality check for a value that never
    // changes, not a security boundary.
    let mut h = 0x811c_9dc5u32;
    for b in key.to_le_bytes().iter().chain(peer_id.iter()) {
        h = (h ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    ((h >> 16) as u16 ^ h as u16).max(1)
}

impl Auditor {
    pub(crate) fn tag(&self) -> u32 {
        self.tag
    }
    pub(crate) fn set_tag(&mut self, tag: u32) {
        self.tag = tag;
    }

    /// Fresh auditor. `completed_ever` seeds the persisted `completed_sent` flag.
    pub fn new(completed_ever: bool) -> Self {
        Auditor {
            completed_ever,
            ..Self::default()
        }
    }

    /// Check a request about to be released at `now`; on success record it.
    pub fn on_release(
        &mut self,
        p: &AnnounceParams,
        kind: SendKind,
        now: Millis,
    ) -> Result<(), &'static str> {
        let id = identity_hash(p.key, &p.peer_id);
        match self.identity {
            0 => self.identity = id,
            seen if seen == id => {}
            _ => return Err("key or peer_id changed"),
        }
        match p.event {
            Some(Event::Started) => {
                if matches!(self.session, Session::Open { started_ok: true }) {
                    return Err("second started in one session");
                }
                if self.session != (Session::Open { started_ok: false }) {
                    // A brand-new session: counters and tracker id restart.
                    self.detail = Some(Box::default());
                }
                self.session = Session::Open { started_ok: false };
            }
            Some(Event::Completed) => {
                if self.session != (Session::Open { started_ok: true }) {
                    return Err("completed outside an acknowledged session");
                }
                if self.completed_ever {
                    return Err("second completed");
                }
                if p.left != 0 {
                    return Err("completed with left > 0");
                }
                self.completed_ever = true;
            }
            Some(Event::Stopped) => {
                if !matches!(self.session, Session::Open { .. } | Session::Stopping) {
                    return Err("stopped without started");
                }
                self.session = Session::Stopping;
            }
            None => {
                if self.session != (Session::Open { started_ok: true }) {
                    return Err("announce outside an acknowledged session");
                }
                let (at, t) = self
                    .detail
                    .as_deref()
                    .and_then(|d| d.last_success)
                    .ok_or("announce without a previous success")?;
                let regular = at + t.interval.max(t.min_interval.unwrap_or_default());
                let manual = at + t.min_interval.unwrap_or(MANUAL_FLOOR).min(t.interval);
                let ok = now >= regular || (kind == SendKind::Manual && now >= manual);
                if !ok {
                    return Err("announce earlier than interval/min interval");
                }
            }
        }
        let d = self.detail.get_or_insert_with(Box::default);
        if p.uploaded < d.last_up || p.downloaded < d.last_down {
            return Err("uploaded/downloaded decreased within a session");
        }
        d.last_up = p.uploaded;
        d.last_down = p.downloaded;
        match (&d.tracker_id, &p.tracker_id) {
            (Some(a), Some(b)) if a != b => return Err("tracker id changed within a session"),
            (Some(_), None) => return Err("tracker id dropped within a session"),
            (None, Some(b)) => d.tracker_id = Some(b.clone()),
            _ => {}
        }
        Ok(())
    }

    /// The machine closed its session without a `stopped` (the tracker had
    /// rejected us, so there is nothing to stop): a new `started` is legitimate.
    pub fn session_closed(&mut self) {
        self.session = Session::Closed;
        self.detail = None;
    }

    /// The `completed` released last provably never reached the tracker (see
    /// `Outcome::Unsent`): forget it, so the single re-send is not taken for a
    /// second `completed`. A later answer that proves delivery sets it again.
    pub fn completed_unsent(&mut self) {
        self.completed_ever = false;
    }

    /// The effective announce interval of the last success, if any:
    /// `max(interval, min interval)`.
    pub fn last_interval(&self) -> Option<std::time::Duration> {
        self.detail
            .as_deref()
            .and_then(|d| d.last_success)
            .map(|(_, t)| t.interval.max(t.min_interval.unwrap_or_default()))
    }

    /// Record the outcome of a released request. `timing` is `Some` for a
    /// success.
    pub fn on_outcome(&mut self, event: Option<Event>, timing: Option<Timing>, now: Millis) {
        let Some(t) = timing else { return };
        match event {
            Some(Event::Started) => {
                if matches!(self.session, Session::Open { .. }) {
                    self.session = Session::Open { started_ok: true };
                }
                self.detail.get_or_insert_with(Box::default).last_success = Some((now, t));
            }
            Some(Event::Stopped) => {
                if self.session == Session::Stopping {
                    self.session = Session::Closed;
                    self.detail = None;
                }
            }
            Some(Event::Completed) => {
                // A success proves the tracker has it (also after a rollback).
                self.completed_ever = true;
                self.detail.get_or_insert_with(Box::default).last_success = Some((now, t));
            }
            None => {
                self.detail.get_or_insert_with(Box::default).last_success = Some((now, t));
            }
        }
    }
}
