//! `AnnounceMachine`: the sans-I/O state machine for one (torrent, tracker URL).
//!
//! Every policy of SPEC 7B/7C lives here as `(state, input, now) -> intents`:
//! event order, intervals, backoff, counters, the Transmission 4.0.6 rules.
//! The machine never touches a socket or a clock and is therefore exhaustively
//! testable. Requests it emits are only *intents*: nothing leaves the process
//! except through the `HostGate`.
//!
//! Life of a request: `SendAnnounce(id)` is emitted (queued at the gate), the
//! gate releases it and the driver reports `Dispatched(id)` (from this moment
//! the request counts as sent and `completed_sent` is persisted), and finally
//! `Response{id, ..}` arrives. A request that is still queued can be withdrawn
//! with `CancelRequest(id)`; the machine forgets it as if it had never been
//! emitted.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use crate::backoff::{retry_delay, retry_delay_for, slow_retry_delay};
use crate::response::{AnnounceError, AnnounceResponse, FailureClass, Timing, TransportError};
use crate::rng::{Rng, SplitMix64};
use crate::store::TimingRecord;
use crate::time::{Millis, secs};
use crate::types::{
    AnnounceParams, Crypto, Event, InfoHash20, PeerId20, RequestId, Stats, TorrentEvent, TorrentRole,
};

/// Manual re-announce floor when the tracker sent no `min interval`.
pub const MANUAL_FLOOR: Duration = secs(300);
/// A user "retry" after a halt (or during the slow retry schedule) waits at
/// least this long since the last attempt. Applies to `ManualReannounce` and
/// to `Start`/`Resume` alike, so an engine that re-sends `Start` on every UI
/// refresh cannot hammer a tracker that just said no.
pub const HALT_RETRY_FLOOR: Duration = secs(60);
/// Network change never retries a failed announce sooner than this after the
/// previous attempt. A network change or wake never makes a *regular* announce
/// earlier: it only reschedules announces that already failed.
pub const NETWORK_RETRY_FLOOR: Duration = secs(30);
/// An announce that was held back by an outage leaves no sooner than this after the release.
pub const RELEASE_FLOOR: Duration = secs(3);
/// Attempts to deliver `stopped` before giving up (best effort by nature).
pub const MAX_STOP_ATTEMPTS: u8 = 3;
/// Upper bound on how long a crash-recovery hold may delay `started`.
pub const MAX_UNCLEAN_HOLD: Duration = secs(3600);

/// Static configuration of one machine.
#[derive(Clone, Debug)]
pub struct MachineConfig {
    /// Info-hash (20 bytes).
    pub info_hash: InfoHash20,
    /// Peer-id: constant for the process session (invariant 6).
    pub peer_id: PeerId20,
    /// `key`: constant for the machine's life (invariant 6).
    pub key: u32,
    /// Listening port.
    pub port: u16,
    /// Peers requested on non-`stopped` announces (default 50).
    pub numwant: u32,
    /// Our IPv4 address, announced as `ipv4=` only if explicitly configured.
    pub ipv4: Option<Ipv4Addr>,
    /// Our IPv6 address, announced as `ipv6=` only if explicitly configured.
    pub ipv6: Option<Ipv6Addr>,
    /// `supportcrypto` / `requirecrypto` on HTTP announces.
    pub crypto: Crypto,
    /// The torrent was complete when added: `completed` is never sent.
    pub added_complete: bool,
    /// Seed of the machine's private jitter generator.
    pub seed: u64,
    /// `started` is delayed by a random `0..start_jitter` (staggered startup).
    pub start_jitter: Duration,
    /// Non-event announces after a wake/network change are spread over this.
    pub wake_jitter: Duration,
}

/// The part of a [`MachineConfig`] that is the same for every torrent of a hub
/// (everything but the info-hash and the jitter seed). Machines share one
/// `Arc` of it; the hub keeps one per distinct value.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Shared {
    pub(crate) peer_id: PeerId20,
    pub(crate) key: u32,
    pub(crate) port: u16,
    pub(crate) numwant: u32,
    pub(crate) ipv4: Option<Ipv4Addr>,
    pub(crate) ipv6: Option<Ipv6Addr>,
    pub(crate) crypto: Crypto,
    pub(crate) added_complete: bool,
    pub(crate) start_jitter: Duration,
    pub(crate) wake_jitter: Duration,
    /// An outage is declared (SPEC 7C.4c): nothing but `stopped` is sent. One value for the whole
    /// hub, so it costs no memory per machine.
    pub(crate) held: bool,
}

impl MachineConfig {
    pub(crate) fn shared(&self) -> Shared {
        Shared {
            peer_id: self.peer_id,
            key: self.key,
            port: self.port,
            numwant: self.numwant,
            ipv4: self.ipv4,
            ipv6: self.ipv6,
            crypto: self.crypto,
            added_complete: self.added_complete,
            start_jitter: self.start_jitter,
            wake_jitter: self.wake_jitter,
            held: false,
        }
    }

    /// Defaults from SPEC 7B.3 for the given identity.
    pub fn new(info_hash: InfoHash20, peer_id: PeerId20, key: u32, port: u16) -> Self {
        MachineConfig {
            info_hash,
            peer_id,
            key,
            port,
            numwant: 50,
            ipv4: None,
            ipv6: None,
            crypto: Crypto::Off,
            added_complete: false,
            seed: u64::from(key)
                ^ u64::from_le_bytes([
                    info_hash[0],
                    info_hash[1],
                    info_hash[2],
                    info_hash[3],
                    0,
                    0,
                    0,
                    0,
                ]),
            start_jitter: secs(5),
            wake_jitter: secs(30),
        }
    }
}

/// Inputs of the machine (the `now` travels beside it).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Input {
    /// Engine-level torrent lifecycle event.
    Torrent(TorrentEvent),
    /// Fresh transfer counters. Push these before timers fire.
    Stats(Stats),
    /// Time passed: evaluate the schedule.
    Timer,
    /// The gate released request `id`: it is now on the wire.
    Dispatched(RequestId),
    /// Outcome of request `id`. The same id may be answered more than once
    /// (several attempts of one logical announce, e.g. two address families);
    /// a success always wins, in any order.
    Response {
        /// Request answered.
        id: RequestId,
        /// What happened.
        result: Result<AnnounceResponse, AnnounceError>,
    },
    /// The gate refused to take request `id` (fail-closed): stop announcing.
    GateRefused(RequestId),
    /// The user pressed "re-announce". Also clears a `failure reason` halt.
    ManualReannounce,
    /// The network path changed.
    NetworkChanged,
    /// The machine woke from sleep.
    Wake,
    /// A user action (start/resume): forget a `failure reason` halt (or the slow
    /// retry schedule), but not sooner than [`HALT_RETRY_FLOOR`] after the last
    /// attempt.
    ClearHalt,
    /// What the torrent is doing now (SPEC 7C.4). Read when a retry is
    /// scheduled; changing to `Downloading` may pull a pending retry in to the
    /// downloading schedule's slot, every other change leaves it alone.
    SetRole(TorrentRole),
    /// The driver took request `id` from the gate but could not put it on the
    /// wire (the durable write it depends on failed). Nothing was sent: this is
    /// an ordinary failure that provably never reached the tracker (backoff and
    /// retry; a `completed` is owed again), not a reason to trip.
    NotSent(RequestId),
    /// The set no longer wants this tracker: like `Stop` but a single `stopped`
    /// attempt (the tracker may be unreachable).
    Suspend,
    /// The set moved on to another tracker of the same tier (BEP 12): this one is retired.
    /// Like `Suspend`, except that a tracker that never acknowledged `started` is closed
    /// without any request (a failing tracker is not contacted outside its own backoff),
    /// and an acknowledged one is not started again before its regular announce is due.
    Retire,
    /// The hub declared an outage (SPEC 7C.4c) and raised the shared `held` flag: nothing but
    /// `stopped` is sent until [`Input::Release`]. A request still queued at the gate is
    /// withdrawn; an announce that comes due is only noted ([`AnnounceMachine::held_due`]).
    Hold,
    /// The outage is over and the flag is lowered. A machine that came due while held sends once
    /// its place in the spread comes; one that did not come due keeps its schedule and sends
    /// nothing.
    Release {
        /// The window the due announces are spread over (jittered per machine).
        spread: Duration,
    },
    /// Like `Response { Err }`, but the failure is the outage's doing: no failure is counted,
    /// no retry is scheduled, the announce stays due (and held). A `stopped` is treated as an
    /// ordinary failure.
    Unreachable {
        /// Request that failed.
        id: RequestId,
        /// What went wrong.
        error: AnnounceError,
    },
    /// Let this one announce through although the outage holds the rest: the probe.
    Probe,
    /// An outage was declared: undo a network failure that happened at or after `since`
    /// (the failures that revealed the outage are not the tracker's fault).
    Forgive {
        /// Failures from this instant on are forgiven.
        since: Millis,
    },
}

/// Why a request is being sent (priority and audit).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SendKind {
    /// `started`, `completed` or `stopped`.
    Event,
    /// Interval-driven announce.
    Regular,
    /// Interval-driven announce after a failure, at the backoff time.
    Retry,
    /// User/network-change triggered, no earlier than the min interval.
    Manual,
}

/// A request to send.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SendAnnounce {
    /// Id to quote in `Dispatched` and `Response`.
    pub id: RequestId,
    /// What to announce.
    pub params: AnnounceParams,
    /// Why.
    pub kind: SendKind,
}

/// Information for the UI/log, never required for correctness.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Report {
    /// Successful announce.
    Announced {
        /// The (clamped) response.
        response: Box<AnnounceResponse>,
        /// Event of the announce.
        event: Option<Event>,
    },
    /// The tracker sent a `warning message`.
    Warning(String),
    /// An announce failed; the next attempt is at `retry_at`.
    Failed {
        /// What failed.
        error: AnnounceError,
        /// Consecutive failures including this one.
        consecutive: u32,
        /// Next attempt.
        retry_at: Millis,
    },
    /// The tracker rejected us with a *permanent-looking* `failure reason`
    /// (unregistered torrent, banned client, bad passkey: see
    /// [`crate::response::PERMANENT_REASONS`]); announcing to it stops until the
    /// user acts (`ManualReannounce`, `Start`, `Resume`). Any other `failure
    /// reason` is retried on the slow schedule and surfaced as a `Warning`.
    Halted {
        /// Verbatim reason.
        reason: String,
    },
    /// A manual re-announce was too early and is deferred until `until`.
    ManualDeferred {
        /// Earliest allowed time.
        until: Millis,
    },
    /// The session ended.
    SessionClosed,
    /// `stopped` could not be delivered; given up.
    StopAbandoned,
    /// A response did not match any request we know about.
    StaleResponse,
    /// A runtime invariant check failed: this machine has stopped announcing.
    InvariantViolation(&'static str),
}

/// Outputs of the machine.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Intent {
    /// Hand this request to the `HostGate`.
    SendAnnounce(SendAnnounce),
    /// Withdraw a request that is still queued at the gate.
    CancelRequest(RequestId),
    /// The machine wants a `Timer` at this time (replaces any earlier wake).
    ScheduleWake(Millis),
    /// Durably store this record *before* releasing any request in the same batch.
    Persist(TimingRecord),
    /// Informational.
    Report(Report),
}

/// Coarse health used by `TrackerSet` for BEP 12 fall-through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Health {
    /// No session.
    Idle,
    /// Session open, `started` not yet acknowledged, no failure so far.
    Trying,
    /// Last announce succeeded.
    Healthy,
    /// Recent failure(s); retrying with backoff.
    Failing,
    /// `failure reason`: waiting for the user.
    Halted,
    /// A runtime invariant failed; fail-closed.
    Tripped,
}

/// An `Option<Millis>` in 8 bytes (`u64::MAX` is "none"; a real time that
/// large is stored one millisecond lower). Idle machines hold several of
/// these, so the 8 bytes matter at thousands of torrents.
#[derive(Clone, Copy, PartialEq, Eq)]
struct T8(u64);

impl T8 {
    const NONE: T8 = T8(u64::MAX);
    #[inline]
    fn get(self) -> Option<Millis> {
        (self.0 != u64::MAX).then_some(Millis(self.0))
    }
    #[inline]
    fn set(&mut self, t: Option<Millis>) {
        self.0 = t.map_or(u64::MAX, |m| m.0.min(u64::MAX - 1));
    }
}

impl std::fmt::Debug for T8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.get(), f)
    }
}

/// `AnnounceMachine::prev_gap_s` when there is no earlier failure to speak of.
const NO_GAP: u8 = u8::MAX;
/// Bit of `prev_gap_s` that marks the earlier failure as a network failure.
const PREV_NETWORK: u8 = 0x80;

#[derive(Clone, Copy, Debug)]
struct Outstanding {
    id: RequestId,
    event: Option<Event>,
    dispatched: bool,
}

/// Schedule state captured before a failure is applied, so that a success for
/// the same logical request arriving later can undo it.
#[derive(Clone, Debug)]
struct Undo {
    failures: u16,
    prev_gap_s: u8,
    retry_at: Option<Millis>,
    last_failure_network: bool,
    last_failure_slow: bool,
    halted: bool,
    completed_sent: bool,
    completion_owed: bool,
}

#[derive(Clone, Debug)]
struct Resolved {
    id: RequestId,
    success: bool,
    event: Option<Event>,
    undo: Undo,
}

/// Why a machine tripped (`AnnounceMachine::tripped`): every reason the
/// machine can raise, so that a tripped machine stores one byte.
const TRIP_REASONS: [&str; 12] = [
    "gate refused a request",
    "send outside a session",
    "identity changed",
    "left exceeds size",
    "counters decreased",
    "started out of order",
    "completed not allowed",
    "stopped before started",
    "announce outside a running session",
    "announce before interval",
    "announce during backoff",
    "request ids exhausted",
];

/// Everything that exists only while a session is open (between `Start` and
/// the end of `Stop`). It is allocated by `Start` and dropped when the session
/// closes, so an idle machine (a paused or never started torrent) stays small.
#[derive(Clone, Debug)]
struct Session {
    /// The last time this machine saw (clock-regression detection).
    now: Millis,
    /// False once the session is over; the box is dropped at the end of the
    /// current `handle_into`.
    active: bool,
    // counters of this session
    up: u64,
    down: u64,
    hw_up: u64,
    hw_down: u64,
    last_sent_up: u64,
    last_sent_down: u64,
    tracker_id: Option<Box<[u8]>>,
    started_dispatched: bool,
    started_acked: bool,
    stopped_dispatched: bool,
    want_stop: bool,
    restart_pending: bool,
    manual_requested: bool,
    stop_max_attempts: u8,
    stop_attempts: u8,
    stop_retry_at: Option<Millis>,
    /// Retired by the set (see `Input::Retire`).
    retired: bool,
    /// The outage probe: one announce may leave although the machine is held.
    allow: bool,
    // schedule
    start_at: Millis,
    last_success: Option<Millis>,
    regular_at: Option<Millis>,
    not_before: Millis,
    jitter_armed_at: Option<Millis>,
    wake: Option<Millis>,
    // requests
    outstanding: Option<Outstanding>,
    resolved: Option<Resolved>,
}

impl Session {
    fn new(now: Millis, start_at: Millis) -> Session {
        Session {
            now,
            active: true,
            up: 0,
            down: 0,
            hw_up: 0,
            hw_down: 0,
            last_sent_up: 0,
            last_sent_down: 0,
            tracker_id: None,
            started_dispatched: false,
            started_acked: false,
            stopped_dispatched: false,
            want_stop: false,
            restart_pending: false,
            manual_requested: false,
            stop_max_attempts: MAX_STOP_ATTEMPTS,
            stop_attempts: 0,
            stop_retry_at: None,
            retired: false,
            allow: false,
            start_at,
            last_success: None,
            regular_at: None,
            not_before: Millis::ZERO,
            jitter_armed_at: None,
            wake: None,
            outstanding: None,
            resolved: None,
        }
    }
}

/// The per-(torrent, tracker) announce state machine.
///
/// size: 104 bytes while idle (checked by a test), plus the session state
/// (~250 bytes, one allocation) while a session is open. The immutable
/// [`MachineConfig`] is shared between the machines of a torrent.
#[derive(Debug, Clone)]
pub struct AnnounceMachine {
    cfg: Arc<Shared>,
    info_hash: InfoHash20,
    /// Low bits of the jitter seed (decorrelates the machines of one torrent).
    /// Jitter is drawn from a generator seeded by this, the info-hash, the
    /// request counter and the time of the draw: no per-machine generator state.
    jit: u8,

    /// Latest `Stats::left` / `Stats::size`. (The session counters live in the
    /// session: they restart at zero with every session.)
    left: u64,
    size: u64,

    // persisted facts and the schedule that outlives a session
    last_announce_at: T8,
    retry_at: T8,
    manual_ok_at: T8,
    /// When the last failure answer arrived (any class). Floors for user
    /// retries and network-change retries count from here; with `hint_secs`
    /// (the server hint that failure carried) it also lets `SetRole` pull a
    /// retry in.
    fail_at: T8,
    /// The hint in units of 2 s (a hint is at most 24 h), rounded up.
    hint_2s: u16,
    /// Seconds (at most 126, so over the 2 minute outage window means 126) between the failure
    /// before the latest one and the latest one (`NO_GAP` when there is none), with `PREV_NETWORK` set when that earlier failure was a network failure: lets an
    /// outage rollback take back exactly the failures inside its window.
    prev_gap_s: u8,
    next_id: u32,
    failures: u16,
    completed_sent: bool,
    completion_owed: bool,
    /// The tracker has been told (or is about to be told) that we still have
    /// something to download: the only case in which a `Complete` is a real
    /// "download finished" for it.
    leech_seen: bool,
    /// The previous run ended without `stopped`: the first `started` waits for
    /// `manual_ok_at` (capped at 1 h).
    unclean: bool,
    /// With `unclean`: the hold is a retired tracker's regular announce time (up to the
    /// 24 h interval cap), not the 1 h cap of a crash.
    hold_full: bool,
    halted: bool,
    last_failure_network: bool,
    last_failure_slow: bool,
    role: TorrentRole,
    /// 0 = fine, n = `TRIP_REASONS[n - 1]`.
    tripped: u8,

    session: Option<Box<Session>>,
}

impl AnnounceMachine {
    /// Create a machine, optionally restoring persisted timing.
    pub fn new(cfg: MachineConfig, restored: Option<TimingRecord>) -> Self {
        Self::with_shared_config(Arc::new(cfg.shared()), cfg.info_hash, cfg.seed, restored)
    }

    /// Like [`new`](Self::new) for a configuration shared with other machines;
    /// `seed` seeds this machine's private jitter generator.
    pub(crate) fn with_shared_config(
        cfg: Arc<Shared>,
        info_hash: InfoHash20,
        seed: u64,
        restored: Option<TimingRecord>,
    ) -> Self {
        let rec = restored.unwrap_or_default();
        let mut last_announce_at = T8::NONE;
        last_announce_at.set(rec.last_announce_at);
        let mut manual_ok_at = T8::NONE;
        manual_ok_at.set(rec.min_allowed_at);
        AnnounceMachine {
            jit: seed as u8,
            unclean: rec.session_open && rec.min_allowed_at.is_some(),
            hold_full: false,
            completed_sent: rec.completed_sent || cfg.added_complete,
            completion_owed: rec.completion_owed && !cfg.added_complete,
            last_announce_at,
            cfg,
            info_hash,
            left: 0,
            size: 0,
            retry_at: T8::NONE,
            manual_ok_at,
            fail_at: T8::NONE,
            hint_2s: 0,
            prev_gap_s: NO_GAP,
            next_id: 1,
            failures: 0,
            leech_seen: false,
            halted: false,
            last_failure_network: false,
            last_failure_slow: false,
            role: TorrentRole::Downloading,
            tripped: 0,
            session: None,
        }
    }

    /// Feed one input; returns the intents.
    pub fn handle(&mut self, input: Input, now: Millis) -> Vec<Intent> {
        let mut out = Vec::new();
        self.handle_into(input, now, &mut out);
        out
    }

    /// Feed one input, appending intents to `out` (no allocation when `out` has room).
    pub fn handle_into(&mut self, input: Input, now: Millis, out: &mut Vec<Intent>) {
        if let Some(s) = self.session.as_deref()
            && now < s.now {
                // The clock went backwards: re-base every stored instant by the
                // same amount so all remaining waits are unchanged (nothing
                // becomes early, nothing is pinned in the future).
                self.rebase(s.now.0 - now.0);
            }
        if let Some(s) = self.session.as_deref_mut() {
            s.now = now;
        }
        if self.tripped != 0 {
            // A tripped machine never sends again, but it must still be able
            // to end its session: otherwise the torrent could never be
            // removed (and re-added after `reset_host`).
            if matches!(
                input,
                Input::Torrent(TorrentEvent::Stop | TorrentEvent::Pause)
                    | Input::Suspend
                    | Input::Retire
            ) {
                self.close_tripped(out);
            }
            return;
        }
        match self.session.take() {
            None => self.handle_idle(input, now, out),
            Some(mut s) => {
                self.handle_session(&mut s, input, now, out);
                if s.active {
                    self.session = Some(s);
                }
            }
        }
    }

    /// An input while no session is open.
    fn handle_idle(&mut self, input: Input, now: Millis, out: &mut Vec<Intent>) {
        match input {
            Input::Torrent(TorrentEvent::Start | TorrentEvent::Resume) => {
                let mut s = Box::new(self.new_session(now));
                self.drive(&mut s, now, out);
                if s.active {
                    self.session = Some(s);
                }
            }
            Input::Torrent(TorrentEvent::Complete) => self.on_complete(None, out),
            Input::Torrent(TorrentEvent::Stop | TorrentEvent::Pause)
            | Input::Suspend
            | Input::Retire => {}
            Input::Stats(st) => {
                self.left = st.left;
                self.size = st.size;
            }
            Input::ClearHalt => self.on_clear_halt(now),
            Input::SetRole(r) => self.on_set_role(r, now),
            Input::Dispatched(_)
            | Input::Response { .. }
            | Input::NotSent(_)
            | Input::Unreachable { .. } => {
                out.push(Intent::Report(Report::StaleResponse));
            }
            Input::Hold | Input::Release { .. } | Input::Probe => {}
            Input::Forgive { since } => self.on_forgive(since),
            Input::Timer
            | Input::GateRefused(_)
            | Input::ManualReannounce
            | Input::NetworkChanged
            | Input::Wake => {}
        }
    }

    /// An input while a session is open.
    fn handle_session(
        &mut self,
        s: &mut Session,
        input: Input,
        now: Millis,
        out: &mut Vec<Intent>,
    ) {
        match input {
            Input::Torrent(ev) => match ev {
                TorrentEvent::Start | TorrentEvent::Resume => self.on_start(s, now, out),
                TorrentEvent::Stop | TorrentEvent::Pause => self.on_stop(s, MAX_STOP_ATTEMPTS, out),
                TorrentEvent::Complete => self.on_complete(Some(s), out),
            },
            Input::Stats(st) => {
                self.left = st.left;
                self.size = st.size;
                s.up = st.uploaded;
                s.down = st.downloaded;
                if !s.started_dispatched {
                    // `started` is not built yet: it will carry these numbers.
                    self.leech_seen |= self.effective_left() > 0;
                }
            }
            Input::Timer => {}
            Input::Dispatched(id) => self.on_dispatched(s, id, now, out),
            Input::Response { id, result } => self.on_response(s, id, result, now, out),
            Input::GateRefused(id) => {
                if s.outstanding.is_some_and(|o| o.id == id) {
                    s.outstanding = None;
                    self.trip("gate refused a request", out);
                }
            }
            Input::NotSent(id) => self.on_response(
                s,
                id,
                Err(AnnounceError::Transport(TransportError::NotSent)),
                now,
                out,
            ),
            Input::ManualReannounce => self.on_manual(s, now, out),
            Input::NetworkChanged | Input::Wake => self.on_network(s, now),
            Input::Suspend => self.on_stop(s, 1, out),
            Input::Retire => {
                self.on_stop(s, 1, out);
                s.retired = true;
            }
            Input::ClearHalt => self.on_clear_halt(now),
            Input::SetRole(r) => self.on_set_role(r, now),
            Input::Hold => self.withdraw_queued(s, out),
            Input::Release { spread } => {
                s.allow = false;
                if self.send_due_at(s).is_some_and(|at| at <= now) {
                    // The announce that came due while held leaves at its place in the spread.
                    // Never at once: the network stack needs a moment, and an answer to the
                    // request that just failed may still be on its way.
                    let ms = u64::try_from(spread.as_millis()).unwrap_or(u64::MAX);
                    let floor = RELEASE_FLOOR.as_millis() as u64;
                    let at = now + Duration::from_millis(floor + self.rng_for(8, now).below(ms.saturating_sub(floor)));
                    s.not_before = s.not_before.max(at);
                }
            }
            Input::Probe => s.allow = true,
            Input::Unreachable { id, error } => self.on_unreachable(s, id, error, now, out),
            Input::Forgive { since } => self.on_forgive(since),
        }
        self.drive(s, now, out);
    }

    /// Shift every stored instant back by `back` ms (saturating at zero).
    fn rebase(&mut self, back: u64) {
        let m = |t: Millis| Millis(t.0.saturating_sub(back));
        let o = |t: Option<Millis>| t.map(m);
        let t8 = |t: &mut T8| {
            let v = t.get().map(m);
            t.set(v);
        };
        t8(&mut self.last_announce_at);
        t8(&mut self.retry_at);
        t8(&mut self.manual_ok_at);
        t8(&mut self.fail_at);
        if let Some(s) = self.session.as_deref_mut() {
            s.now = m(s.now);
            s.stop_retry_at = o(s.stop_retry_at);
            s.start_at = m(s.start_at);
            s.last_success = o(s.last_success);
            s.regular_at = o(s.regular_at);
            s.not_before = m(s.not_before);
            s.jitter_armed_at = o(s.jitter_armed_at);
            s.wake = o(s.wake);
            if let Some(r) = &mut s.resolved {
                r.undo.retry_at = o(r.undo.retry_at);
            }
        }
    }

    // ----------------------------------------------------------------- views

    /// When the machine next wants a `Timer`, if ever.
    pub fn next_wake(&self) -> Option<Millis> {
        if self.tripped != 0 {
            None
        } else {
            self.session.as_deref().and_then(|s| s.wake)
        }
    }

    /// Coarse health.
    pub fn health(&self) -> Health {
        if self.tripped != 0 {
            Health::Tripped
        } else if self.halted {
            Health::Halted
        } else if let Some(s) = self.session.as_deref() {
            if self.failures > 0 {
                Health::Failing
            } else if s.started_acked {
                Health::Healthy
            } else {
                Health::Trying
            }
        } else {
            Health::Idle
        }
    }

    /// True while a session is open (between `Start` and the end of `Stop`).
    pub fn session_open(&self) -> bool {
        self.session.is_some()
    }
    /// True once a stop has been requested for the open session.
    pub fn stopping(&self) -> bool {
        self.session.as_deref().is_some_and(|s| s.want_stop)
    }
    /// True while a request is queued or in flight.
    pub fn has_outstanding(&self) -> bool {
        self.session
            .as_deref()
            .is_some_and(|s| s.outstanding.is_some())
    }
    /// Consecutive failures.
    pub fn failures(&self) -> u32 {
        u32::from(self.failures)
    }
    /// Earliest time of the next regular announce, if known (`None` when no
    /// session is open).
    pub fn regular_at(&self) -> Option<Millis> {
        self.session.as_deref().and_then(|s| s.regular_at)
    }
    /// Time of the last successful announce (not `stopped`) of the open
    /// session; `None` when no session is open.
    pub fn last_success(&self) -> Option<Millis> {
        self.session.as_deref().and_then(|s| s.last_success)
    }
    /// Time of the last backoff retry, if in backoff.
    pub fn retry_at(&self) -> Option<Millis> {
        self.retry_at.get()
    }
    /// `completed` was dispatched (or must never be sent).
    pub fn completed_sent(&self) -> bool {
        self.completed_sent
    }
    /// Why the machine tripped, if it did.
    pub fn tripped(&self) -> Option<&'static str> {
        TRIP_REASONS
            .get(usize::from(self.tripped).wrapping_sub(1))
            .copied()
    }
    /// The tracker id currently echoed.
    pub fn tracker_id(&self) -> Option<&[u8]> {
        self.session
            .as_deref()
            .and_then(|s| s.tracker_id.as_deref())
    }
    /// Swap the shared configuration (the hub does this when the announced port changes). Nothing
    /// is rescheduled: the next announce, whenever the schedule says it is due, carries the new
    /// values.
    pub(crate) fn replace_shared(&mut self, cfg: Arc<Shared>) {
        self.cfg = cfg;
    }

    /// The configuration this machine runs with. (`seed` is reported as 0: the
    /// jitter generator is private to each machine.)
    pub fn config(&self) -> MachineConfig {
        MachineConfig {
            info_hash: self.info_hash,
            peer_id: self.cfg.peer_id,
            key: self.cfg.key,
            port: self.cfg.port,
            numwant: self.cfg.numwant,
            ipv4: self.cfg.ipv4,
            ipv6: self.cfg.ipv6,
            crypto: self.cfg.crypto,
            added_complete: self.cfg.added_complete,
            seed: 0,
            start_jitter: self.cfg.start_jitter,
            wake_jitter: self.cfg.wake_jitter,
        }
    }

    /// The shared configuration handle (the hub pools equal ones).
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.cfg
    }

    /// Snapshot of what must survive a restart.
    pub fn record(&self) -> TimingRecord {
        self.record_of(self.session.as_deref())
    }

    fn record_of(&self, s: Option<&Session>) -> TimingRecord {
        TimingRecord {
            last_announce_at: self.last_announce_at.get(),
            next_allowed_at: s.and_then(|s| s.regular_at),
            min_allowed_at: self.manual_ok_at.get(),
            completed_sent: self.completed_sent,
            completion_owed: self.completion_owed,
            // The tracker id is per session only (not persisted).
            tracker_id: None,
            session_open: s.is_some_and(|s| s.started_dispatched && !s.stopped_dispatched),
        }
    }

    // ------------------------------------------------------------- handlers

    fn on_start(&mut self, s: &mut Session, _now: Millis, out: &mut Vec<Intent>) {
        if s.want_stop {
            if !s.stopped_dispatched {
                // Nothing was said to the tracker yet: just carry on.
                self.withdraw_queued(s, out);
                s.want_stop = false;
                s.stop_attempts = 0;
                s.stop_retry_at = None;
            } else {
                // `stopped` is out: the old session is over, a new
                // `started` must follow it (invariant 2).
                s.restart_pending = true;
            }
        }
    }

    fn on_stop(&mut self, s: &mut Session, max_attempts: u8, out: &mut Vec<Intent>) {
        s.restart_pending = false;
        if s.want_stop {
            return;
        }
        self.withdraw_queued(s, out);
        s.want_stop = true;
        // A tracker that already rejected us gets one `stopped` try, not three.
        s.stop_max_attempts = if self.halted { 1 } else { max_attempts };
        s.stop_attempts = 0;
        s.stop_retry_at = None;
    }

    /// The tripped machine's own end of session: nothing is sent, nothing is
    /// pending, the session is closed so the torrent can be removed.
    fn close_tripped(&mut self, out: &mut Vec<Intent>) {
        if self.session.take().is_some() {
            out.push(Intent::Report(Report::SessionClosed));
        }
    }

    fn on_complete(&mut self, s: Option<&Session>, out: &mut Vec<Intent>) {
        self.left = 0;
        // Also while a stop is in progress or the session is closed: a download
        // that finishes just as the user pauses is still a finished download,
        // and `completed` is sent (once) in the next session.
        let eligible = !self.completed_sent && !self.cfg.added_complete && self.leech_seen;
        if eligible && !self.completion_owed {
            self.completion_owed = true;
            out.push(Intent::Persist(self.record_of(s)));
        }
    }

    /// The engine says what the torrent is doing. A retry that is already
    /// scheduled is never made *later*, and never earlier either, except when a
    /// seed turns into a download: then it is pulled in to the downloading
    /// schedule's slot for the same failure (still honouring the server hint).
    fn on_set_role(&mut self, new: TorrentRole, now: Millis) {
        let old = std::mem::replace(&mut self.role, new);
        if new != TorrentRole::Downloading || old == TorrentRole::Downloading {
            return;
        }
        // (a `failure reason` schedule is role independent; a halt waits for the user)
        if self.failures == 0 || self.last_failure_slow || self.halted {
            return;
        }
        let (Some(cur), Some(failed)) = (self.retry_at.get(), self.fail_at.get()) else {
            return;
        };
        let hint = (self.hint_2s > 0).then(|| Duration::from_secs(u64::from(self.hint_2s) * 2));
        let mut rng = self.rng_for(1, now);
        let d = retry_delay_for(
            TorrentRole::Downloading,
            u32::from(self.failures),
            hint,
            &mut rng,
        );
        let slot = (failed + d).max(now);
        self.retry_at.set(Some(cur.min(slot)));
    }

    /// A user action (`Start`/`Resume`) over a halt or the slow retry schedule.
    fn on_clear_halt(&mut self, now: Millis) {
        if self.halted || self.last_failure_slow {
            self.user_retry(now);
        }
    }

    /// Retry after a halt / slow failure at the user's request, never sooner
    /// than [`HALT_RETRY_FLOOR`] after the last attempt.
    fn user_retry(&mut self, now: Millis) {
        self.halted = false;
        self.failures = 0;
        self.last_failure_slow = false;
        self.last_failure_network = false;
        let floor = self.fail_at.get().map_or(now, |t| t + HALT_RETRY_FLOOR);
        self.retry_at.set(Some(now.max(floor)));
    }

    fn on_manual(&mut self, s: &mut Session, now: Millis, out: &mut Vec<Intent>) {
        if s.want_stop {
            return;
        }
        if self.halted || self.last_failure_slow {
            // A user action: try again, but never faster than the floor.
            self.user_retry(now);
            return;
        }
        // A request already outstanding will carry fresh state anyway.
        if s.started_acked && !s.manual_requested && s.outstanding.is_none() {
            s.manual_requested = true;
            let until = self
                .manual_ok_at
                .get()
                .unwrap_or(now)
                .max(self.retry_at.get().unwrap_or(now));
            if until > now {
                out.push(Intent::Report(Report::ManualDeferred { until }));
            }
        }
    }

    /// Network change or wake. Only announces that already failed (or have not
    /// been acknowledged yet) are rescheduled; a healthy machine waits for its
    /// interval exactly as before (SPEC 7C.3 #1: a non-event announce never
    /// happens before `max(interval, min interval)`, whatever the network does).
    fn on_network(&mut self, s: &mut Session, now: Millis) {
        if s.want_stop || self.halted {
            return;
        }
        if self.failures == 0 && s.started_acked {
            return;
        }
        // Spread the retries after a wake over `wake_jitter`. An event arriving
        // while such a window is already armed does not extend it, and a new
        // window opens at most once per `wake_jitter`: a storm of events (a
        // flapping interface) cannot postpone announces forever.
        let span = self.cfg.wake_jitter;
        let armed = s
            .jitter_armed_at
            .is_some_and(|a| now < a + span && now >= a);
        if !armed {
            let jitter = self.rng_for(2, now).below(span.as_millis() as u64);
            s.not_before = s.not_before.max(now + Duration::from_millis(jitter));
            s.jitter_armed_at = Some(now);
        }
        if self.failures > 0 && self.last_failure_network
            && let Some(r) = self.retry_at.get() {
                let quick = now + Duration::from_millis(1000 + self.rng_for(3, now).below(15_000));
                let floor = self.fail_at.get().map_or(now, |t| t + NETWORK_RETRY_FLOOR);
                self.retry_at.set(Some(r.min(quick.max(floor))));
            }
    }

    /// A generator for one jitter draw, seeded from the machine's identity, its
    /// request counter and the moment of the draw (`salt` tells call sites
    /// apart). Good enough for jitter and shuffling; not for security.
    fn rng_for(&self, salt: u64, now: Millis) -> SplitMix64 {
        let a = u64::from_le_bytes([
            self.info_hash[0],
            self.info_hash[1],
            self.info_hash[2],
            self.info_hash[3],
            self.info_hash[4],
            self.info_hash[5],
            self.info_hash[6],
            self.info_hash[7],
        ]);
        let b = u64::from_le_bytes([
            self.info_hash[8],
            self.info_hash[9],
            self.info_hash[10],
            self.info_hash[11],
            self.info_hash[12],
            self.info_hash[13],
            self.info_hash[14],
            self.info_hash[15],
        ]);
        let mut r = SplitMix64::new(
            a ^ b.rotate_left(21)
                ^ u64::from(self.jit).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ u64::from(self.next_id).wrapping_mul(0xBF58_476D_1CE4_E5B9)
                ^ u64::from(self.failures).wrapping_mul(0x94D0_49BB_1331_11EB)
                ^ now.0.wrapping_mul(0xD6E8_FEB8_6659_FD93)
                ^ salt.wrapping_mul(0xA24B_AED4_963E_E407),
        );
        r.next_u64(); // one round of mixing before the first draw
        r
    }

    /// A fresh session (the machine has none, or the previous one just ended).
    fn new_session(&mut self, now: Millis) -> Session {
        // Counters are per session (FINDINGS.md): `started` carries 0/0 even
        // when the engine has not pushed fresh numbers yet.
        self.leech_seen = self.effective_left() > 0;
        let jitter = self
            .rng_for(4, now)
            .below(self.cfg.start_jitter.as_millis() as u64);
        let mut start_at = now + Duration::from_millis(jitter);
        let full = std::mem::take(&mut self.hold_full);
        if std::mem::take(&mut self.unclean)
            && let Some(hold) = self.manual_ok_at.get() {
                let cap = if full { Duration::from_secs(24 * 3600) } else { MAX_UNCLEAN_HOLD };
                start_at = start_at.max(hold.min(now + cap));
            }
        Session::new(now, start_at)
    }

    fn close_session(&mut self, s: &mut Session, out: &mut Vec<Intent>) {
        if s.retired && s.started_acked {
            // Retired while healthy: a later `started` must not land inside the interval the
            // tracker was given (nothing moves a tracker's schedule but its own answers).
            if let Some(r) = s.regular_at {
                self.manual_ok_at.set(Some(self.manual_ok_at.get().map_or(r, |m| m.max(r))));
                self.unclean = true;
                self.hold_full = true;
            }
        }
        s.active = false;
        s.want_stop = false;
        s.stop_retry_at = None;
        s.started_acked = false;
        s.started_dispatched = false;
        s.regular_at = None;
        s.manual_requested = false;
        s.wake = None;
        out.push(Intent::Report(Report::SessionClosed));
        if s.restart_pending {
            let now = s.now;
            *s = self.new_session(now);
        }
    }

    /// Withdraw a request that is queued but not yet on the wire.
    fn withdraw_queued(&mut self, s: &mut Session, out: &mut Vec<Intent>) {
        if let Some(o) = s.outstanding
            && !o.dispatched {
                out.push(Intent::CancelRequest(o.id));
                s.outstanding = None;
            }
    }

    fn on_dispatched(
        &mut self,
        s: &mut Session,
        id: RequestId,
        now: Millis,
        out: &mut Vec<Intent>,
    ) {
        let Some(o) = s
            .outstanding
            .as_mut()
            .filter(|o| o.id == id && !o.dispatched)
        else {
            out.push(Intent::Report(Report::StaleResponse));
            return;
        };
        o.dispatched = true;
        s.allow = false;
        let event = o.event;
        // Any announce satisfies a pending "re-announce now" request.
        s.manual_requested = false;
        self.last_announce_at.set(Some(now));
        match event {
            Some(Event::Started) => s.started_dispatched = true,
            Some(Event::Completed) => {
                // Persisted before the request leaves (the driver applies
                // Persist intents first): a crash can never cause a second one.
                self.completed_sent = true;
                self.completion_owed = false;
            }
            Some(Event::Stopped) => s.stopped_dispatched = true,
            None => {}
        }
        out.push(Intent::Persist(self.record_of(Some(s))));
    }

    fn on_response(
        &mut self,
        s: &mut Session,
        id: RequestId,
        result: Result<AnnounceResponse, AnnounceError>,
        now: Millis,
        out: &mut Vec<Intent>,
    ) {
        if let Some(o) = s.outstanding.filter(|o| o.id == id) {
            if !o.dispatched {
                out.push(Intent::Report(Report::StaleResponse));
                return;
            }
            s.outstanding = None;
            let undo = self.undo_point();
            let success = result.is_ok();
            match result {
                Ok(resp) => self.apply_success(s, resp, o.event, now, out),
                Err(err) => self.apply_failure(s, err, o.event, now, out),
            }
            s.resolved = Some(Resolved {
                id,
                success,
                event: o.event,
                undo,
            });
            return;
        }
        // A further answer for a request we already resolved.
        match s.resolved.take() {
            Some(r) if r.id == id => {
                let mut keep = r;
                match (keep.success, result) {
                    // Success already won: a late failure changes nothing, in
                    // particular it does not touch the schedule or the counter.
                    (true, _) => {}
                    // Duplicate failure: do not count it twice.
                    (false, Err(_)) => {}
                    // Success arrives after a failure of the same logical
                    // request: undo the failure and apply the success, unless
                    // something newer has been sent in the meantime.
                    (false, Ok(resp)) => {
                        // (`stopped` is excluded: ending the session twice
                        // would be worse than the failure it repairs.)
                        let upgradable =
                            keep.event != Some(Event::Stopped) && s.active && !s.want_stop;
                        if upgradable
                            && u64::from(self.next_id) == id.0 + 1
                            && s.outstanding.is_none()
                        {
                            self.restore(&keep.undo);
                            self.apply_success(s, resp, keep.event, now, out);
                            keep.success = true;
                        }
                    }
                }
                s.resolved = Some(keep);
            }
            other => {
                s.resolved = other;
                out.push(Intent::Report(Report::StaleResponse));
            }
        }
    }

    /// A failure that the outage caused (SPEC 7C.4c): nothing is counted or scheduled, the
    /// announce stays due and held until the hub releases it.
    fn on_unreachable(
        &mut self,
        s: &mut Session,
        id: RequestId,
        err: AnnounceError,
        now: Millis,
        out: &mut Vec<Intent>,
    ) {
        let Some(o) = s.outstanding.filter(|o| o.id == id && o.dispatched) else {
            return self.on_response(s, id, Err(err), now, out);
        };
        if o.event == Some(Event::Stopped) {
            return self.on_response(s, id, Err(err), now, out);
        }
        s.outstanding = None;
        if o.event == Some(Event::Completed) && err.never_reached_tracker() {
            self.completed_sent = false;
            self.completion_owed = !self.cfg.added_complete;
            out.push(Intent::Persist(self.record_of(Some(s))));
        }
        s.resolved = Some(Resolved {
            id,
            success: false,
            event: o.event,
            undo: self.undo_point(),
        });
    }

    /// Remember how long before `now` the failure about to be counted was preceded by another.
    fn note_previous_failure(&mut self, now: Millis) {
        self.prev_gap_s = match (self.failures > 0, self.fail_at.get()) {
            (true, Some(t)) => {
                // Anything over the 2 minute window is stored as "too old" (126 s).
                // Rounded UP: the earlier failure is never remembered as more recent than it
                // was, so the outage rollback cannot take back one just outside its window.
                let gap = now.since(t).as_millis().div_ceil(1000).min(u128::from(PREV_NETWORK - 2)) as u8;
                gap | if self.last_failure_network { PREV_NETWORK } else { 0 }
            }
            _ => NO_GAP,
        };
    }

    /// Take back the network failures that revealed an outage: exactly those inside the window
    /// (`since` on), newest first and contiguous, at most the latest two (a downloading torrent's
    /// first retries are 1 and 2 minutes apart). An older real failure stays counted.
    fn on_forgive(&mut self, since: Millis) {
        let Some(latest) = self.fail_at.get() else { return };
        if self.failures == 0 || !self.last_failure_network || latest < since {
            return;
        }
        let known = self.prev_gap_s != NO_GAP;
        let prev_network = known && self.prev_gap_s & PREV_NETWORK != 0;
        let prev_at = known.then(|| {
            Millis(latest.0.saturating_sub(u64::from(self.prev_gap_s & !PREV_NETWORK) * 1000))
        });
        let prev_in_window = self.failures >= 2 && prev_network && prev_at.is_some_and(|t| t >= since);
        let n = if prev_in_window { 2 } else { 1 };
        self.failures = self.failures.saturating_sub(n);
        self.prev_gap_s = NO_GAP;
        if self.failures == 0 {
            self.retry_at = T8::NONE;
            self.last_failure_network = false;
            self.hint_2s = 0;
        } else if n == 1 {
            // The failure before it is the latest again. `retry_at` stays where the forgiven
            // failure put it (never earlier than the older failure's own backoff).
            self.last_failure_network = prev_network;
            self.fail_at.set(prev_at);
        } else {
            self.last_failure_network = false;
        }
        // `resolved` stays: a late success for the same request still upgrades the failure.
    }

    /// When the next announce (not `stopped`) would leave, ignoring any hold; `None` while a
    /// request is out, the machine is stopping, halted or tripped. Mirrors `drive_inner`.
    fn send_due_at(&self, s: &Session) -> Option<Millis> {
        if !s.active || s.outstanding.is_some() || s.want_stop || self.halted || self.tripped != 0 {
            return None;
        }
        let retry = self.retry_at.get().unwrap_or(Millis::ZERO);
        if !s.started_acked {
            return Some(s.start_at.max(retry).max(s.not_before));
        }
        if self.completion_owed && !self.completed_sent && !self.cfg.added_complete {
            return Some(retry);
        }
        let mut at = s.regular_at.unwrap_or(Millis::ZERO);
        if s.manual_requested {
            at = at.min(self.manual_ok_at.get().unwrap_or(Millis::ZERO));
        }
        Some(at.max(retry).max(s.not_before))
    }

    /// True while an outage holds this machine and an announce would be sent by `now`.
    pub fn held_due(&self, now: Millis) -> bool {
        self.cfg.held
            && self
                .session
                .as_deref()
                .is_some_and(|s| !s.allow && self.send_due_at(s).is_some_and(|at| at <= now))
    }

    fn is_held(&self, s: &Session) -> bool {
        self.cfg.held && !s.allow
    }

    fn undo_point(&self) -> Undo {
        Undo {
            failures: self.failures,
            prev_gap_s: self.prev_gap_s,
            retry_at: self.retry_at.get(),
            last_failure_network: self.last_failure_network,
            last_failure_slow: self.last_failure_slow,
            halted: self.halted,
            completed_sent: self.completed_sent,
            completion_owed: self.completion_owed,
        }
    }

    fn restore(&mut self, u: &Undo) {
        self.failures = u.failures;
        self.prev_gap_s = u.prev_gap_s;
        self.retry_at.set(u.retry_at);
        self.last_failure_network = u.last_failure_network;
        self.last_failure_slow = u.last_failure_slow;
        self.halted = u.halted;
        self.completed_sent = u.completed_sent;
        self.completion_owed = u.completion_owed;
    }

    fn apply_success(
        &mut self,
        s: &mut Session,
        resp: AnnounceResponse,
        event: Option<Event>,
        now: Millis,
        out: &mut Vec<Intent>,
    ) {
        let timing: Timing = resp.timing().sanitized();
        self.failures = 0;
        self.retry_at = T8::NONE;
        self.last_failure_network = false;
        self.last_failure_slow = false;
        self.halted = false;
        if let Some(id) = &resp.tracker_id {
            // Latched for the rest of the session (invariant 6).
            if s.tracker_id.is_none() {
                s.tracker_id = Some(id.clone());
            }
        }
        if let Some(w) = &resp.warning {
            out.push(Intent::Report(Report::Warning(w.clone())));
        }
        if event != Some(Event::Stopped) {
            s.last_success = Some(now);
            // interval >= min interval by construction (clamp_timing).
            s.regular_at = Some(now + timing.interval);
            let min_eff = timing
                .min_interval
                .unwrap_or(MANUAL_FLOOR)
                .min(timing.interval);
            self.manual_ok_at.set(Some(now + min_eff));
        }
        if event == Some(Event::Started) {
            s.started_acked = true;
        }
        out.push(Intent::Report(Report::Announced {
            response: Box::new(resp),
            event,
        }));
        if event == Some(Event::Stopped) {
            self.close_session(s, out);
        }
        out.push(Intent::Persist(self.record_of(Some(s))));
    }

    fn apply_failure(
        &mut self,
        s: &mut Session,
        err: AnnounceError,
        event: Option<Event>,
        now: Millis,
        out: &mut Vec<Intent>,
    ) {
        let class = err.class();
        if event == Some(Event::Stopped) {
            s.stop_attempts += 1;
            let giving_up =
                s.stop_attempts >= s.stop_max_attempts || matches!(class, FailureClass::Halt);
            if giving_up {
                out.push(Intent::Report(Report::StopAbandoned));
                self.close_session(s, out);
            } else {
                let hint = match class {
                    FailureClass::Backoff { hint } => hint,
                    FailureClass::Halt | FailureClass::Slow => None,
                };
                let mut rng = self.rng_for(5, now);
                let d = retry_delay(u32::from(s.stop_attempts), hint, &mut rng);
                s.stop_retry_at = Some(now + d);
            }
            return;
        }
        if event == Some(Event::Completed) && err.never_reached_tracker() {
            // The request provably never reached the tracker (DNS, connect,
            // TLS handshake, policy or local refusal): it is not "sent". The
            // flag is rolled back (durably) so that exactly one more `completed`
            // goes out with the retry. Ambiguous failures (timeout after the
            // write, reset, 5xx) keep it set: at-most-once.
            self.completed_sent = false;
            self.completion_owed = !self.cfg.added_complete;
            out.push(Intent::Persist(self.record_of(Some(s))));
        }
        match class {
            FailureClass::Halt => {
                self.halted = true;
                self.fail_at.set(Some(now));
                self.last_failure_slow = false;
                let AnnounceError::Rejected { reason, .. } = &err else {
                    // Only rejections classify as Halt.
                    return;
                };
                out.push(Intent::Report(Report::Halted {
                    reason: reason.clone(),
                }));
            }
            FailureClass::Slow => {
                // A `failure reason` that does not look permanent
                // (maintenance, "try later"): keep trying on the slow schedule
                // and tell the user. Seeding must not stop for years because
                // of a transient message.
                self.note_previous_failure(now);
                self.failures = self.failures.saturating_add(1);
                let hint = match &err {
                    AnnounceError::Rejected {
                        retry_in: Some(crate::response::RetryIn::Minutes(m)),
                        ..
                    } => Some(Duration::from_secs(u64::from(*m) * 60)),
                    _ => None,
                };
                let mut rng = self.rng_for(6, now);
                let d = slow_retry_delay(u32::from(self.failures), hint, &mut rng);
                let at = now + d;
                self.retry_at.set(Some(at));
                self.fail_at.set(Some(now));
                self.last_failure_network = false;
                self.last_failure_slow = true;
                if let AnnounceError::Rejected { reason, .. } = &err {
                    out.push(Intent::Report(Report::Warning(format!(
                        "tracker said: {reason} (will retry in about {} min)",
                        d.as_secs() / 60
                    ))));
                }
                out.push(Intent::Report(Report::Failed {
                    error: err,
                    consecutive: u32::from(self.failures),
                    retry_at: at,
                }));
            }
            FailureClass::Backoff { hint } => {
                self.note_previous_failure(now);
                self.failures = self.failures.saturating_add(1);
                let mut rng = self.rng_for(7, now);
                let d = retry_delay_for(self.role, u32::from(self.failures), hint, &mut rng);
                let at = now + d;
                self.retry_at.set(Some(at));
                self.fail_at.set(Some(now));
                self.hint_2s = hint.map_or(0, |h| {
                    u16::try_from(h.as_secs().div_ceil(2)).unwrap_or(u16::MAX)
                });
                self.last_failure_network = err.is_network();
                self.last_failure_slow = false;
                out.push(Intent::Report(Report::Failed {
                    error: err,
                    consecutive: u32::from(self.failures),
                    retry_at: at,
                }));
            }
        }
    }

    fn trip(&mut self, why: &'static str, out: &mut Vec<Intent>) {
        let idx = TRIP_REASONS.iter().position(|r| *r == why).unwrap_or(0);
        self.tripped = (idx + 1) as u8;
        out.push(Intent::Report(Report::InvariantViolation(why)));
    }

    // -------------------------------------------------------------- driving

    /// Decide whether to send something now, and when to wake otherwise.
    fn drive(&mut self, s: &mut Session, now: Millis, out: &mut Vec<Intent>) {
        if self.tripped != 0 {
            return;
        }
        let before = s.wake;
        s.wake = None;
        self.drive_inner(s, now, out);
        if s.wake != before
            && let Some(w) = s.wake {
                out.push(Intent::ScheduleWake(w));
            }
    }

    fn drive_inner(&mut self, s: &mut Session, now: Millis, out: &mut Vec<Intent>) {
        if !s.active {
            // (a session that ended and has no restart pending is dropped by
            // the caller)
            return;
        }
        if s.outstanding.is_some() {
            return;
        }
        if s.want_stop {
            if s.restart_pending && s.stopped_dispatched {
                // The user wants to run again; stop retrying and begin anew.
                self.close_session(s, out);
                return self.drive_inner(s, now, out);
            }
            if !s.started_dispatched
                || (self.halted && !s.started_acked)
                || (s.retired && !s.started_acked)
                || s.stop_attempts >= s.stop_max_attempts
            {
                // Nothing was announced (or the tracker rejected our very first
                // `started`): there is nothing to stop at the tracker. Never
                // send `stopped` before `started` (invariant 2). A tracker that
                // accepted `started` and rejected a later announce does get one
                // `stopped`, so the next session's `started` is not a repeat.
                self.close_session(s, out);
                return self.drive_inner(s, now, out);
            }
            let at = s.stop_retry_at.unwrap_or(now);
            if now >= at {
                self.send(s, Some(Event::Stopped), SendKind::Event, now, out);
            } else {
                s.wake = Some(at);
            }
            return;
        }
        if self.halted {
            return;
        }
        if !s.started_acked {
            let at = s
                .start_at
                .max(self.retry_at.get().unwrap_or(Millis::ZERO))
                .max(s.not_before);
            if now >= at {
                if self.is_held(s) {
                    return;
                }
                self.send(s, Some(Event::Started), SendKind::Event, now, out);
            } else {
                s.wake = Some(at);
            }
            return;
        }
        if self.completion_owed && !self.completed_sent && !self.cfg.added_complete {
            let at = self.retry_at.get().unwrap_or(now);
            if now >= at {
                if self.is_held(s) {
                    return;
                }
                self.send(s, Some(Event::Completed), SendKind::Event, now, out);
            } else {
                s.wake = Some(at);
            }
            return;
        }
        // Regular / manual announce.
        let regular = s.regular_at.unwrap_or(now);
        let mut at = regular;
        if s.manual_requested {
            at = at.min(self.manual_ok_at.get().unwrap_or(now));
        }
        at = at
            .max(self.retry_at.get().unwrap_or(Millis::ZERO))
            .max(s.not_before);
        if now >= at {
            let kind = if now >= regular {
                if self.retry_at.get().is_some() {
                    SendKind::Retry
                } else {
                    SendKind::Regular
                }
            } else {
                SendKind::Manual
            };
            if self.is_held(s) {
                return;
            }
            self.send(s, None, kind, now, out);
        } else {
            s.wake = Some(at);
        }
    }

    /// `left` as it would be reported now (before the `completed` special case).
    fn effective_left(&self) -> u64 {
        if self.size > 0 {
            self.left.min(self.size)
        } else if self.cfg.added_complete {
            // A seed we were told about before its size is known.
            0
        } else {
            // Size unknown (magnet before metadata): never claim to be complete.
            self.left.max(1)
        }
    }

    fn effective_stats(&mut self, s: &mut Session, event: Option<Event>) -> (u64, u64, u64) {
        s.hw_up = s.hw_up.max(s.up);
        s.hw_down = s.hw_down.max(s.down);
        let mut left = self.effective_left();
        if event == Some(Event::Completed) {
            left = 0;
        }
        if event != Some(Event::Stopped) && left > 0 {
            // The tracker now believes we are still downloading: a later
            // `Complete` is real news for it.
            self.leech_seen = true;
        }
        (s.hw_up, s.hw_down, left)
    }

    fn build_params(&mut self, s: &mut Session, event: Option<Event>) -> AnnounceParams {
        let (uploaded, downloaded, left) = self.effective_stats(s, event);
        AnnounceParams {
            info_hash: self.info_hash,
            peer_id: self.cfg.peer_id,
            port: self.cfg.port,
            uploaded,
            downloaded,
            left,
            key: self.cfg.key,
            event,
            numwant: if event == Some(Event::Stopped) {
                0
            } else {
                self.cfg.numwant
            },
            tracker_id: s.tracker_id.clone(),
            ipv4: self.cfg.ipv4,
            ipv6: self.cfg.ipv6,
            crypto: self.cfg.crypto,
        }
    }

    /// Runtime self-audit of invariants 1-4 and 6 (SPEC 7C.3), independent of
    /// the scheduling code above. A failure trips the machine (fail closed).
    fn audit(
        &self,
        s: &Session,
        p: &AnnounceParams,
        kind: SendKind,
        now: Millis,
    ) -> Result<(), &'static str> {
        if !s.active {
            return Err("send outside a session");
        }
        if p.key != self.cfg.key || p.peer_id != self.cfg.peer_id || p.info_hash != self.info_hash {
            return Err("identity changed");
        }
        if self.size > 0 && p.left > self.size {
            return Err("left exceeds size");
        }
        if p.uploaded < s.last_sent_up || p.downloaded < s.last_sent_down {
            return Err("counters decreased");
        }
        match p.event {
            Some(Event::Started) => {
                if s.started_acked || s.stopped_dispatched {
                    return Err("started out of order");
                }
            }
            Some(Event::Completed) => {
                if !s.started_acked
                    || self.completed_sent
                    || self.cfg.added_complete
                    || s.stopped_dispatched
                {
                    return Err("completed not allowed");
                }
            }
            Some(Event::Stopped) => {
                if !s.started_dispatched || !s.want_stop {
                    return Err("stopped before started");
                }
            }
            None => {
                if !s.started_acked || s.want_stop || s.stopped_dispatched {
                    return Err("announce outside a running session");
                }
                let ok_regular = s.regular_at.is_some_and(|t| now >= t);
                let ok_manual =
                    kind == SendKind::Manual && self.manual_ok_at.get().is_some_and(|t| now >= t);
                if !ok_regular && !ok_manual {
                    return Err("announce before interval");
                }
            }
        }
        if p.event != Some(Event::Stopped) && self.retry_at.get().is_some_and(|t| now < t) {
            return Err("announce during backoff");
        }
        Ok(())
    }

    fn send(
        &mut self,
        s: &mut Session,
        event: Option<Event>,
        kind: SendKind,
        now: Millis,
        out: &mut Vec<Intent>,
    ) {
        let params = self.build_params(s, event);
        if let Err(why) = self.audit(s, &params, kind, now) {
            self.trip(why, out);
            return;
        }
        let Some(next) = self.next_id.checked_add(1) else {
            self.trip("request ids exhausted", out);
            return;
        };
        s.last_sent_up = params.uploaded;
        s.last_sent_down = params.downloaded;
        let id = RequestId(u64::from(self.next_id));
        self.next_id = next;
        s.outstanding = Some(Outstanding {
            id,
            event,
            dispatched: false,
        });
        out.push(Intent::SendAnnounce(SendAnnounce { id, params, kind }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::response::{RetryIn, TransportError};

    const H: [u8; 20] = [7; 20];

    fn ok(interval: u64, min: Option<u64>) -> AnnounceResponse {
        AnnounceResponse {
            interval: Duration::from_secs(interval),
            min_interval: min.map(Duration::from_secs),
            tracker_id: None,
            complete: None,
            incomplete: None,
            downloaded: None,
            peers: vec![],
            warning: None,
            external_ip: None,
        }
    }

    /// Single-machine rig: dispatches immediately, records sends.
    struct Rig {
        m: AnnounceMachine,
        now: Millis,
        sent: Vec<(Millis, SendAnnounce)>,
        reports: Vec<Report>,
        persisted: Vec<TimingRecord>,
    }

    impl Rig {
        fn new() -> Self {
            let mut cfg = MachineConfig::new(H, [9; 20], 0xABCD, 6881);
            cfg.start_jitter = Duration::ZERO;
            Self::with(cfg, None)
        }
        fn with(cfg: MachineConfig, rec: Option<TimingRecord>) -> Self {
            Rig {
                m: AnnounceMachine::new(cfg, rec),
                now: Millis::ZERO,
                sent: vec![],
                reports: vec![],
                persisted: vec![],
            }
        }
        fn input(&mut self, i: Input) -> Vec<SendAnnounce> {
            let mut sends = Vec::new();
            let mut pending = vec![i];
            while let Some(inp) = pending.pop() {
                let intents = self.m.handle(inp, self.now);
                for it in intents {
                    match it {
                        Intent::SendAnnounce(s) => {
                            self.sent.push((self.now, s.clone()));
                            pending.push(Input::Dispatched(s.id));
                            sends.push(s);
                        }
                        Intent::Persist(r) => self.persisted.push(r),
                        Intent::Report(r) => self.reports.push(r),
                        _ => {}
                    }
                }
            }
            sends
        }
        fn at(&mut self, secs: u64) -> Vec<SendAnnounce> {
            self.now = Millis::from_secs(secs);
            self.input(Input::Timer)
        }
        fn ev(&mut self, e: TorrentEvent) -> Vec<SendAnnounce> {
            self.input(Input::Torrent(e))
        }
        fn respond(
            &mut self,
            s: &SendAnnounce,
            r: Result<AnnounceResponse, AnnounceError>,
        ) -> Vec<SendAnnounce> {
            self.input(Input::Response {
                id: s.id,
                result: r,
            })
        }
        fn stats(&mut self, up: u64, down: u64, left: u64, size: u64) {
            self.input(Input::Stats(Stats {
                uploaded: up,
                downloaded: down,
                left,
                size,
            }));
        }
    }

    fn timeout() -> AnnounceError {
        AnnounceError::Transport(TransportError::Timeout)
    }

    #[test]
    fn started_then_interval() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].params.event, Some(Event::Started));
        assert!(r.respond(&s[0], Ok(ok(1800, None))).is_empty());
        assert!(r.at(1799).is_empty());
        let s = r.at(1800);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].params.event, None);
        assert_eq!(s[0].kind, SendKind::Regular);
    }

    #[test]
    fn nothing_while_request_outstanding() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        assert_eq!(s.len(), 1);
        assert!(r.at(100_000).is_empty());
    }

    #[test]
    fn small_interval_is_clamped_by_parser_but_machine_resanitizes() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        // A hand-built response with an absurd 5 s interval and no min.
        r.respond(&s[0], Ok(ok(5, None)));
        assert!(r.at(299).is_empty());
        assert_eq!(r.at(300).len(), 1);
    }

    #[test]
    fn min_interval_beats_interval() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(600, Some(1200))));
        assert!(r.at(1199).is_empty());
        assert_eq!(r.at(1200).len(), 1);
    }

    #[test]
    fn manual_waits_for_min_interval() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, Some(900))));
        r.now = Millis::from_secs(10);
        assert!(r.input(Input::ManualReannounce).is_empty());
        assert!(r.reports.iter().any(
            |x| matches!(x, Report::ManualDeferred { until } if *until == Millis::from_secs(900))
        ));
        assert!(r.at(899).is_empty());
        let s = r.at(900);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].kind, SendKind::Manual);
    }

    #[test]
    fn backoff_doubles_and_success_resets() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Err(timeout()));
        let t1 = r.m.retry_at().unwrap();
        assert!(t1.0 >= 60_000 && t1.0 < 72_000, "{t1:?}");
        r.now = t1;
        let s = r.input(Input::Timer);
        assert_eq!(
            s[0].params.event,
            Some(Event::Started),
            "started is retried until acknowledged"
        );
        r.respond(&s[0], Err(timeout()));
        let t2 = r.m.retry_at().unwrap();
        assert!(t2.0 - r.now.0 >= 120_000);
        assert_eq!(r.m.failures(), 2);
        r.now = t2;
        let s = r.input(Input::Timer);
        r.respond(&s[0], Ok(ok(1800, None)));
        assert_eq!(r.m.failures(), 0);
        assert_eq!(r.m.retry_at(), None);
    }

    #[test]
    fn retry_after_is_honoured() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(
            &s[0],
            Err(AnnounceError::Http {
                status: 429,
                retry_after: Some(Duration::from_secs(7200)),
            }),
        );
        assert!(r.m.retry_at().unwrap().0 >= 7_200_000);
        let s = r.ev(TorrentEvent::Start);
        assert!(s.is_empty());
        let s = r.at(7199);
        assert!(s.is_empty());
        assert_eq!(r.at(8000).len(), 1);
    }

    #[test]
    fn bep31_retry_in_minutes() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(
            &s[0],
            Err(AnnounceError::Rejected {
                reason: "busy".into(),
                retry_in: Some(RetryIn::Minutes(45)),
            }),
        );
        assert!(r.m.retry_at().unwrap().0 >= 45 * 60_000);
        assert_eq!(r.m.health(), Health::Failing);
    }

    #[test]
    fn failure_reason_halts_until_user_acts() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(
            &s[0],
            Err(AnnounceError::Rejected {
                reason: "unregistered torrent".into(),
                retry_in: None,
            }),
        );
        assert_eq!(r.m.health(), Health::Halted);
        assert!(r.at(100_000).is_empty());
        assert!(r.at(10_000_000).is_empty());
        r.now = Millis::from_secs(10_000_001);
        let s = r.input(Input::ManualReannounce);
        assert_eq!(s.len(), 1, "user action retries");
        assert_eq!(s[0].params.event, Some(Event::Started));
    }

    #[test]
    fn retry_in_never_halts() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(
            &s[0],
            Err(AnnounceError::Rejected {
                reason: "x".into(),
                retry_in: Some(RetryIn::Never),
            }),
        );
        assert_eq!(r.m.health(), Health::Halted);
    }

    /// SPEC 7C.4a: failure of the second family must not overwrite the success,
    /// in either processing order.
    #[test]
    fn transmission_406_success_wins_any_order() {
        for failure_first in [false, true] {
            let mut r = Rig::new();
            r.stats(0, 0, 100, 100);
            let s = r.ev(TorrentEvent::Start);
            let id = s[0].id;
            let good = Ok(ok(1800, None));
            let bad = Err(timeout());
            if failure_first {
                r.input(Input::Response { id, result: bad });
                r.input(Input::Response { id, result: good });
            } else {
                r.input(Input::Response { id, result: good });
                r.input(Input::Response { id, result: bad });
            }
            assert_eq!(r.m.failures(), 0, "failure_first={failure_first}");
            assert_eq!(r.m.retry_at(), None);
            assert_eq!(r.m.health(), Health::Healthy);
            assert!(r.at(1799).is_empty(), "no 20-second retry");
            assert_eq!(r.at(1800).len(), 1);
        }
    }

    #[test]
    fn duplicate_failures_count_once() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        let id = s[0].id;
        r.input(Input::Response {
            id,
            result: Err(timeout()),
        });
        r.input(Input::Response {
            id,
            result: Err(timeout()),
        });
        r.input(Input::Response {
            id,
            result: Err(timeout()),
        });
        assert_eq!(r.m.failures(), 1);
    }

    #[test]
    fn completed_once_and_only_after_started_ack() {
        let mut r = Rig::new();
        r.stats(0, 50, 50, 100);
        let s = r.ev(TorrentEvent::Start);
        assert!(
            r.ev(TorrentEvent::Complete).is_empty(),
            "started not acknowledged yet"
        );
        let s2 = r.respond(&s[0], Ok(ok(1800, None)));
        assert_eq!(s2.len(), 1);
        assert_eq!(s2[0].params.event, Some(Event::Completed));
        assert_eq!(s2[0].params.left, 0);
        r.respond(&s2[0], Ok(ok(1800, None)));
        // A second Complete, even after a restart of the session, is ignored.
        assert!(r.ev(TorrentEvent::Complete).is_empty());
        let st = r.ev(TorrentEvent::Stop);
        r.respond(&st[0], Ok(ok(1800, None)));
        r.stats(0, 0, 0, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        r.ev(TorrentEvent::Complete);
        assert!(
            r.sent
                .iter()
                .filter(|(_, s)| s.params.event == Some(Event::Completed))
                .count()
                == 1
        );
    }

    #[test]
    fn never_completed_for_added_complete() {
        let mut cfg = MachineConfig::new(H, [9; 20], 1, 6881);
        cfg.added_complete = true;
        cfg.start_jitter = Duration::ZERO;
        let mut r = Rig::with(cfg, None);
        r.stats(0, 0, 0, 100);
        let s = r.ev(TorrentEvent::Start);
        assert_eq!(s[0].params.left, 0);
        r.respond(&s[0], Ok(ok(1800, None)));
        assert!(r.ev(TorrentEvent::Complete).is_empty());
        assert!(
            r.sent
                .iter()
                .all(|(_, s)| s.params.event != Some(Event::Completed))
        );
    }

    #[test]
    fn completed_flag_is_persisted_before_the_request_leaves_and_survives_restart() {
        let mut r = Rig::new();
        r.stats(0, 50, 50, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        r.ev(TorrentEvent::Complete);
        let rec = r.persisted.last().unwrap().clone();
        assert!(rec.completed_sent);
        // Crash: new machine from the record.
        let mut cfg = MachineConfig::new(H, [9; 20], 0xABCD, 6881);
        cfg.start_jitter = Duration::ZERO;
        let mut r2 = Rig::with(cfg, Some(rec));
        r2.now = Millis::from_secs(400); // past the post-crash hold
        r2.stats(0, 0, 100, 100);
        let s = r2.ev(TorrentEvent::Start);
        r2.respond(&s[0], Ok(ok(1800, None)));
        r2.ev(TorrentEvent::Complete);
        assert!(
            r2.sent
                .iter()
                .all(|(_, s)| s.params.event != Some(Event::Completed))
        );
    }

    #[test]
    fn crash_with_owed_completed_sends_it_exactly_once_next_session() {
        let mut r = Rig::new();
        r.stats(0, 50, 50, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        // Complete arrives while the started->completed is not yet dispatched
        // because we stop immediately.
        r.input(Input::Torrent(TorrentEvent::Complete));
        // the Rig dispatches instantly, so craft the owed state directly:
        let mut rec = r.m.record();
        rec.completed_sent = false;
        rec.completion_owed = true;
        let mut cfg = MachineConfig::new(H, [9; 20], 0xABCD, 6881);
        cfg.start_jitter = Duration::ZERO;
        let mut r2 = Rig::with(cfg, Some(rec));
        r2.now = Millis::from_secs(400);
        r2.stats(0, 0, 0, 100);
        let s = r2.ev(TorrentEvent::Start);
        let s2 = r2.respond(&s[0], Ok(ok(1800, None)));
        assert_eq!(s2[0].params.event, Some(Event::Completed));
    }

    #[test]
    fn stop_before_started_dispatch_sends_nothing() {
        let mut cfg = MachineConfig::new(H, [9; 20], 1, 6881);
        cfg.start_jitter = secs(10);
        let mut r = Rig::with(cfg, None);
        r.stats(0, 0, 100, 100);
        assert!(r.ev(TorrentEvent::Start).is_empty());
        assert!(r.ev(TorrentEvent::Stop).is_empty());
        assert!(r.at(1000).is_empty());
        assert!(!r.m.session_open());
    }

    #[test]
    fn stopped_follows_started_and_new_started_after_stopped() {
        let mut r = Rig::new();
        r.stats(5, 5, 90, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        let st = r.ev(TorrentEvent::Pause);
        assert_eq!(st[0].params.event, Some(Event::Stopped));
        assert_eq!(st[0].params.numwant, 0);
        // Resume while `stopped` is in flight: wait for it, then `started`.
        assert!(r.ev(TorrentEvent::Resume).is_empty());
        let s = r.respond(&st[0], Ok(ok(1800, None)));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].params.event, Some(Event::Started));
    }

    #[test]
    fn retire_before_started_is_acknowledged_sends_nothing() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        assert_eq!(s[0].params.event, Some(Event::Started));
        // 503: the tracker is in backoff; the set moves on to another tracker of the tier
        assert!(r.respond(&s[0], Err(AnnounceError::Http { status: 503, retry_after: None })).is_empty());
        let before = r.sent.len();
        assert!(r.input(Input::Retire).is_empty(), "no request to a failing tracker");
        assert!(!r.m.session_open());
        assert_eq!(r.sent.len(), before);
        // a later `started` still honours the backoff of the failure
        let retry = r.m.retry_at().expect("backoff kept");
        r.now = r.now + Duration::from_secs(1);
        assert!(r.ev(TorrentEvent::Start).is_empty());
        r.now = retry;
        assert_eq!(r.input(Input::Timer)[0].params.event, Some(Event::Started));
    }

    #[test]
    fn retire_after_started_sends_stopped_and_new_started_waits_for_the_regular_time() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        r.now = Millis::from_secs(100);
        let st = r.input(Input::Retire);
        assert_eq!(st[0].params.event, Some(Event::Stopped));
        r.respond(&st[0], Ok(ok(1800, None)));
        assert!(!r.m.session_open());
        // started again right away would land inside the interval
        r.now = Millis::from_secs(110);
        assert!(r.ev(TorrentEvent::Start).is_empty());
        assert!(r.at(1799).is_empty());
        assert_eq!(r.at(1800)[0].params.event, Some(Event::Started));
    }

    #[test]
    fn nothing_after_stopped() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        let st = r.ev(TorrentEvent::Stop);
        r.respond(&st[0], Ok(ok(1800, None)));
        assert!(r.at(1_000_000).is_empty());
        assert!(r.input(Input::ManualReannounce).is_empty());
        assert!(r.ev(TorrentEvent::Complete).is_empty());
    }

    #[test]
    fn stopped_is_retried_a_bounded_number_of_times() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        let mut st = r.ev(TorrentEvent::Stop);
        let mut attempts = 1;
        while let Some(s) = st.first().cloned() {
            r.respond(&s, Err(timeout()));
            r.now = Millis(r.now.0 + 3_600_000);
            st = r.input(Input::Timer);
            if !st.is_empty() {
                attempts += 1;
            }
        }
        assert_eq!(attempts, MAX_STOP_ATTEMPTS as usize);
        assert!(!r.m.session_open());
        assert!(r.reports.contains(&Report::StopAbandoned));
    }

    #[test]
    fn counters_never_decrease_and_left_is_clamped() {
        let mut r = Rig::new();
        r.stats(100, 200, 500, 1000);
        let s = r.ev(TorrentEvent::Start);
        // Counters are per session: `started` carries 0/0 whatever was pushed
        // before the session opened.
        assert_eq!(
            (
                s[0].params.uploaded,
                s[0].params.downloaded,
                s[0].params.left
            ),
            (0, 0, 500)
        );
        r.respond(&s[0], Ok(ok(300, None)));
        r.stats(100, 200, 400, 1000);
        let s = r.at(300);
        assert_eq!(
            (
                s[0].params.uploaded,
                s[0].params.downloaded,
                s[0].params.left
            ),
            (100, 200, 400)
        );
        r.respond(&s[0], Ok(ok(300, None)));
        r.stats(50, 10, 5000, 1000); // goes backwards, left > size
        let s = r.at(600);
        assert_eq!(
            (
                s[0].params.uploaded,
                s[0].params.downloaded,
                s[0].params.left
            ),
            (100, 200, 1000)
        );
    }

    #[test]
    fn unknown_size_never_claims_complete() {
        let mut r = Rig::new();
        r.stats(0, 0, 0, 0);
        let s = r.ev(TorrentEvent::Start);
        assert!(s[0].params.left >= 1);
    }

    #[test]
    fn tracker_id_is_echoed_and_latched() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        let mut resp = ok(300, None);
        resp.tracker_id = Some(b"abc".to_vec().into());
        r.respond(&s[0], Ok(resp));
        let s = r.at(300);
        assert_eq!(s[0].params.tracker_id.as_deref(), Some(&b"abc"[..]));
        let mut resp = ok(300, None);
        resp.tracker_id = Some(b"zzz".to_vec().into());
        r.respond(&s[0], Ok(resp));
        let s = r.at(600);
        assert_eq!(
            s[0].params.tracker_id.as_deref(),
            Some(&b"abc"[..]),
            "stable within the session"
        );
    }

    #[test]
    fn stale_response_is_ignored() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        r.input(Input::Response {
            id: RequestId(999),
            result: Err(timeout()),
        });
        assert_eq!(r.m.failures(), 0);
        assert!(r.reports.contains(&Report::StaleResponse));
    }

    fn connect_err() -> AnnounceError {
        AnnounceError::Transport(TransportError::Connect)
    }

    /// Two failures of a downloading torrent, `first` at t=0 and the second at t=300 s, then an
    /// outage rollback from `since` on: how many failures are left.
    fn forgive_after(first: AnnounceError, since_s: u64) -> u32 {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Err(first));
        let s = r.at(300);
        assert_eq!(s.len(), 1, "the retry is due");
        r.respond(&s[0], Err(connect_err()));
        assert_eq!(r.m.failures(), 2);
        r.input(Input::Forgive { since: Millis::from_secs(since_s) });
        r.m.failures()
    }

    /// SPEC 7C.4c: the outage takes back the failures that revealed it (inside its window), not an
    /// older real failure. One old failure plus one inside the window leaves exactly one.
    #[test]
    fn forgive_takes_back_only_the_failures_inside_the_window() {
        // old: a 503 at t=0; in window: a connect failure at t=300 (window starts at 180 s)
        let http503 = AnnounceError::Http { status: 503, retry_after: None };
        assert_eq!(forgive_after(http503, 180), 1);
        // an old network failure is just as real once outside the window
        assert_eq!(forgive_after(connect_err(), 180), 1);
        // both inside the window: both go
        assert_eq!(forgive_after(connect_err(), 0), 0);
        // the newest is outside the window: nothing goes
        assert_eq!(forgive_after(connect_err(), 301), 2);
    }

    #[test]
    fn forgive_keeps_the_older_failure_for_the_next_backoff() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Err(connect_err()));
        let s = r.at(300);
        r.respond(&s[0], Err(connect_err()));
        r.input(Input::Forgive { since: Millis::from_secs(180) });
        assert_eq!(r.m.failures(), 1);
        // the next failure is the second one again: a 2 minute step, not 1
        let s = r.at(400);
        assert!(s.is_empty() || r.m.failures() == 1);
        let s = r.at(900);
        assert_eq!(s.len(), 1);
        r.respond(&s[0], Err(connect_err()));
        assert_eq!(r.m.failures(), 2);
    }

    #[test]
    fn wake_spreads_overdue_announces_but_never_early() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, None)));
        r.now = Millis::from_secs(100_000);
        assert!(r.input(Input::Wake).len() <= 1);
        // nothing earlier than the interval: wake at t=100 s does not announce
        let mut q = Rig::new();
        q.stats(0, 0, 100, 100);
        let s = q.ev(TorrentEvent::Start);
        q.respond(&s[0], Ok(ok(1800, None)));
        q.now = Millis::from_secs(100);
        assert!(q.input(Input::Wake).is_empty());
        assert!(q.at(1799).is_empty());
    }

    #[test]
    fn network_change_shortens_network_backoff_only() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Err(AnnounceError::Transport(TransportError::Dns)));
        let before = r.m.retry_at().unwrap();
        r.now = Millis::from_secs(5);
        r.input(Input::NetworkChanged);
        let after = r.m.retry_at().unwrap();
        assert!(after < before);
        assert!(
            after >= Millis::from_secs(30),
            "never within 30 s of the last attempt: {after:?}"
        );
        assert_eq!(r.m.failures(), 1, "failure counter is not reset");
        // A 5xx is not shortened.
        let mut q = Rig::new();
        q.stats(0, 0, 100, 100);
        let s = q.ev(TorrentEvent::Start);
        q.respond(
            &s[0],
            Err(AnnounceError::Http {
                status: 503,
                retry_after: None,
            }),
        );
        let before = q.m.retry_at().unwrap();
        q.input(Input::NetworkChanged);
        assert_eq!(q.m.retry_at().unwrap(), before);
    }

    #[test]
    fn restart_after_crash_waits_for_min_interval() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.now = Millis::from_secs(100);
        r.respond(&s[0], Ok(ok(1800, Some(900))));
        let rec = r.m.record();
        assert!(rec.session_open);
        let mut cfg = MachineConfig::new(H, [9; 20], 0xABCD, 6881);
        cfg.start_jitter = Duration::ZERO;
        let mut r2 = Rig::with(cfg, Some(rec));
        r2.now = Millis::from_secs(200);
        r2.stats(0, 0, 100, 100);
        assert!(r2.ev(TorrentEvent::Start).is_empty());
        assert_eq!(r2.at(999).len(), 0);
        assert_eq!(r2.at(1000).len(), 1);
    }

    #[test]
    fn clean_shutdown_restart_is_immediate() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let s = r.ev(TorrentEvent::Start);
        r.respond(&s[0], Ok(ok(1800, Some(900))));
        let st = r.ev(TorrentEvent::Stop);
        r.respond(&st[0], Ok(ok(1800, None)));
        let rec = r.m.record();
        assert!(!rec.session_open);
        let mut cfg = MachineConfig::new(H, [9; 20], 0xABCD, 6881);
        cfg.start_jitter = Duration::ZERO;
        let mut r2 = Rig::with(cfg, Some(rec));
        r2.stats(0, 0, 100, 100);
        assert_eq!(r2.ev(TorrentEvent::Start).len(), 1);
    }

    #[test]
    fn gate_refusal_trips_the_machine() {
        let mut r = Rig::new();
        r.stats(0, 0, 100, 100);
        let mut it = r.m.handle(Input::Torrent(TorrentEvent::Start), r.now);
        let id = match it.remove(0) {
            Intent::SendAnnounce(s) => s.id,
            other => panic!("{other:?}"),
        };
        r.m.handle(Input::GateRefused(id), r.now);
        assert_eq!(r.m.health(), Health::Tripped);
        assert!(
            r.m.handle(Input::Timer, Millis::from_secs(10_000))
                .is_empty()
        );
    }
}

#[cfg(test)]
mod size_tests {
    use super::*;

    /// Per-torrent state is documented with its size (docs/ENGINEERING.md); this
    /// keeps the numbers honest. A cold torrent costs one machine per tracker;
    /// the heap cost of a whole pair is asserted by `tests/memory.rs`.
    #[test]
    fn per_torrent_structs_stay_small() {
        let m = std::mem::size_of::<AnnounceMachine>();
        let s = std::mem::size_of::<Session>();
        let a = std::mem::size_of::<crate::audit::Auditor>();
        let r = std::mem::size_of::<TimingRecord>();
        let u = std::mem::size_of::<crate::url::TrackerUrl>();
        eprintln!(
            "AnnounceMachine {m} B, Session {s} B, Auditor {a} B, TimingRecord {r} B, TrackerUrl {u} B"
        );
        assert!(
            m <= 104,
            "AnnounceMachine grew to {m} bytes; update its doc comment"
        );
        assert!(s <= 320, "Session grew to {s} bytes");
        assert!(a <= 16, "Auditor grew to {a} bytes");
        assert!(r <= 96, "TimingRecord grew to {r} bytes");
        assert!(u <= 24, "TrackerUrl grew to {u} bytes");
    }
}
