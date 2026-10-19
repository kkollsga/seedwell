//! `TrackerHub`: the glue an engine drives. It owns every torrent's
//! `TrackerSet`, the `HostGate` and the `TimingStore`, applies machine intents
//! (persist first, then gate), and tells the engine which requests to put on
//! the wire. It is still sans-I/O: all time comes from the caller.
//!
//! # Time contract (for the engine)
//!
//! Every method that takes a `now: Millis` expects **Unix milliseconds from
//! the wall clock** (`SystemTime`), the same epoch in every launch, because
//! persisted timings are stored in it. The hub does not trust the clock to be
//! monotonic:
//!
//! * **Backward steps** (NTP, a corrected wrong reading, manual change): the hub
//!   keeps an internal time line `T = wall + shift` and raises `shift` by the
//!   size of every backward step, so `T` never goes back. Every wait that was
//!   running continues unchanged (nothing becomes early, nothing is pinned in
//!   the future), and everything handed back to the engine or the store
//!   (`next_wake`, report times, persisted records) is converted back to the
//!   wall time line. A forward excursion that is later corrected therefore
//!   costs nothing after the correction.
//! * **Forward jumps** (sleep, hibernation, a clock set forward) are taken at
//!   face value: for the tracker that much time really may have passed. After a
//!   genuine sleep the hub announces what is due, spread by the gate's token
//!   bucket and the wake jitter, and never earlier than `interval` /
//!   `min interval` in elapsed wall time. A bogus forward reading can make
//!   announces due early *once* per tracker (the gate's bucket still applies);
//!   the engine should feed the real clock and not smooth or fabricate values.
//! * `Millis` near `u64::MAX` saturates instead of wrapping.
//!
//! Event loop sketch (see README):
//!
//! ```text
//! loop {
//!     for req in hub.tick(now) { spawn(async { let r = transport.execute(&req).await; done_tx.send((req.ticket(), r)) }) }
//!     select! { (ticket, result) = done_rx.recv() => hub.complete(ticket, result, clock()),
//!               cmd = commands.recv() => apply(cmd),
//!               _ = sleep_until(hub.next_wake(now)) => {} }
//! }
//! ```

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::hash::BuildHasherDefault;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use crate::fastmap::{FastMap, FxHasher};
use crate::gate::{GateConfig, GateEvent, GatedRequest, HostGate, Outcome, SubmitError, Ticket};
use crate::machine::{Input, Intent, MachineConfig, Report, Shared};
use crate::outage::{
    Detector, HINT_DEBOUNCE, INFER_WINDOW, NetEvent, NetView, Off, PROBE_DEADLINE, RELEASE_SPREAD, Reason, WAKE_WAIT,
    jittered, probe_delay,
};
use crate::response::{AnnounceError, AnnounceResponse, TransportError};
use crate::rng::{Rng, SplitMix64};
use crate::set::{SetInput, SetIntent, TrackerSet};
use crate::store::TimingRecord;
use crate::store::{TimingKey, TimingStore};
use crate::time::Millis;
use crate::types::{Crypto, Event, InfoHash20, PeerId20, Stats, TorrentEvent, TorrentRole};
use crate::url::TrackerUrl;

type ReqKey = (InfoHash20, u16);

/// Reports kept for the engine before the oldest are dropped.
pub const MAX_PENDING_REPORTS: usize = 10_000;

/// Everything the hub needs to know about a torrent.
#[derive(Clone, Debug)]
pub struct TorrentSpec {
    /// Info-hash (20 bytes).
    pub info_hash: InfoHash20,
    /// Our peer-id for this process session.
    pub peer_id: PeerId20,
    /// Tracker `key`.
    pub key: u32,
    /// Listening port.
    pub port: u16,
    /// Peers wanted per announce.
    pub numwant: u32,
    /// Explicit IPv4 address to announce (`ipv4=`); opt-in, default off.
    pub ipv4: Option<Ipv4Addr>,
    /// Explicit IPv6 address to announce (`ipv6=`); opt-in, default off.
    pub ipv6: Option<Ipv6Addr>,
    /// `supportcrypto` / `requirecrypto` on HTTP announces.
    pub crypto: Crypto,
    /// Torrent was complete when added (never send `completed`).
    pub added_complete: bool,
    /// BEP 27 private flag: only the listed trackers are ever contacted. (This
    /// is enforced for *every* torrent by the gate's registry; the flag is kept
    /// for reporting and for transports.)
    pub private: bool,
    /// Announce tiers from the `.torrent` (never rewritten).
    pub tiers: Vec<Vec<TrackerUrl>>,
}

impl TorrentSpec {
    /// Spec with the SPEC defaults (numwant 50).
    pub fn new(
        info_hash: InfoHash20,
        peer_id: PeerId20,
        key: u32,
        port: u16,
        tiers: Vec<Vec<TrackerUrl>>,
    ) -> Self {
        TorrentSpec {
            info_hash,
            peer_id,
            key,
            port,
            numwant: 50,
            ipv4: None,
            ipv6: None,
            crypto: Crypto::Off,
            added_complete: false,
            private: false,
            tiers,
        }
    }
}

/// What a [`HubNote`] says (developer log: decisions that send nothing, and why a request waited).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteKind {
    /// BEP 12: another tracker of the tier took over; this one got a single `stopped`.
    Retire,
    /// Same, but the tracker never acknowledged `started`, so nothing was sent to it.
    RetireSilent,
    /// The process is quitting: this tracker gets one `stopped` attempt.
    Suspend,
    /// A tracker session was opened (the walk of its tier reached it).
    SessionOpened,
    /// A network change or wake did not move a healthy tracker's regular announce.
    KeptSchedule,
    /// A network change or wake pulled a failed announce's retry in.
    RetryPulled,
    /// The rate gate holds the request back.
    Deferred,
    /// The request left the gate after waiting.
    Released,
    /// The gate refused the request; the machine stopped announcing.
    Refused,
}

/// A decision of the hub, kept only while tracing is on (see [`TrackerHub::set_trace`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HubNote {
    /// Torrent.
    pub torrent: InfoHash20,
    /// Tracker slot.
    pub slot: u16,
    /// When (wall clock).
    pub at: Millis,
    /// What.
    pub kind: NoteKind,
    /// The announce time this decision leaves in place, when it has one.
    pub next: Option<Millis>,
    /// Why, in words.
    pub detail: String,
}

/// Which input a set got (for the notes).
#[derive(Clone, Copy, PartialEq, Eq)]
enum InputClass {
    Start,
    Stop,
    Quit,
    Network,
    Other,
}

fn class_of(i: &SetInput) -> InputClass {
    match i {
        SetInput::Torrent(TorrentEvent::Start | TorrentEvent::Resume) => InputClass::Start,
        SetInput::Torrent(TorrentEvent::Stop | TorrentEvent::Pause) => InputClass::Stop,
        SetInput::Quit => InputClass::Quit,
        SetInput::NetworkChanged | SetInput::Wake => InputClass::Network,
        _ => InputClass::Other,
    }
}

/// A report tagged with its origin.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct HubReport {
    /// Torrent.
    pub torrent: InfoHash20,
    /// Tracker slot within the torrent.
    pub slot: u16,
    /// What happened.
    pub report: Report,
}

/// Why `add_torrent` failed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum HubError {
    /// Torrent already present.
    Duplicate,
}

struct TorrentEntry {
    set: TrackerSet,
    /// The wake-up registered in `wake_index` (`u64::MAX` = none).
    wake: u64,
}

impl TorrentEntry {
    fn wake(&self) -> Option<Millis> {
        (self.wake != u64::MAX).then_some(Millis(self.wake))
    }
    fn set_wake(&mut self, w: Option<Millis>) {
        self.wake = w.map_or(u64::MAX, |m| m.0.min(u64::MAX - 1));
    }
}

/// One shared `Arc<TrackerUrl>` per distinct URL text: the 800 torrents of a
/// public tracker point at one URL object instead of owning 800 copies.
struct PoolUrl(Arc<TrackerUrl>);

impl std::hash::Hash for PoolUrl {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.0.raw().hash(h);
    }
}
impl PartialEq for PoolUrl {
    fn eq(&self, o: &Self) -> bool {
        self.0.raw() == o.0.raw()
    }
}
impl Eq for PoolUrl {}
impl std::borrow::Borrow<str> for PoolUrl {
    fn borrow(&self) -> &str {
        self.0.raw()
    }
}

/// Maps the caller's wall clock to the hub's internal, never-decreasing time
/// line `T = wall + shift` (see the module docs).
#[derive(Clone, Copy, Debug, Default)]
struct Clock {
    /// Last wall reading.
    last: Millis,
    /// Total size of all backward steps seen so far.
    shift: u64,
}

impl Clock {
    /// Normalise a reading and remember it.
    fn norm(&mut self, wall: Millis) -> Millis {
        if wall < self.last {
            self.shift = self.shift.saturating_add(self.last.0 - wall.0);
        }
        self.last = wall;
        Millis(wall.0.saturating_add(self.shift))
    }

    /// Like [`norm`](Self::norm) without remembering: `(T, shift in effect)`.
    fn peek(&self, wall: Millis) -> (Millis, u64) {
        let shift = if wall < self.last {
            self.shift.saturating_add(self.last.0 - wall.0)
        } else {
            self.shift
        };
        (Millis(wall.0.saturating_add(shift)), shift)
    }

    /// Hub time to wall time.
    fn wall_of(&self, t: Millis) -> Millis {
        Millis(t.0.saturating_sub(self.shift))
    }

    /// Wall time to hub time.
    fn hub_time(&self, w: Millis) -> Millis {
        Millis(w.0.saturating_add(self.shift))
    }
}

/// The hub. See the module docs.
pub struct TrackerHub<S: TimingStore> {
    gate: HostGate,
    store: S,
    torrents: FastMap<InfoHash20, TorrentEntry>,
    url_pool: HashSet<PoolUrl, BuildHasherDefault<FxHasher>>,
    shared_pool: HashSet<Arc<Shared>>,
    wake_index: BTreeSet<(Millis, InfoHash20)>,
    reports: VecDeque<HubReport>,
    rng: SplitMix64,
    scratch: Vec<SetIntent>,
    store_errors: u64,
    gate_log: VecDeque<GateEvent>,
    clock: Clock,
    /// Developer log: record decisions as notes (off: no snapshots, no allocation).
    trace: bool,
    notes: Vec<HubNote>,
    /// Trace only: when each queued request was submitted, and whether its deferral was noted.
    submitted: std::collections::HashMap<(InfoHash20, u16), (Millis, bool)>,
    /// Outage detector (SPEC 7C.4c): a few words while online.
    net: Detector,
    net_events: Vec<NetEvent>,
    /// `apply` saw a machine count a failure (a late failure after a success is not counted).
    saw_failed: bool,
}

impl<S: TimingStore> TrackerHub<S> {
    /// New hub.
    pub fn new(gate: GateConfig, store: S, seed: u64) -> Self {
        TrackerHub {
            gate: HostGate::new(gate),
            store,
            torrents: FastMap::default(),
            url_pool: HashSet::default(),
            shared_pool: HashSet::new(),
            wake_index: BTreeSet::new(),
            reports: VecDeque::new(),
            rng: SplitMix64::new(seed),
            scratch: Vec::new(),
            store_errors: 0,
            gate_log: VecDeque::new(),
            clock: Clock::default(),
            trace: false,
            notes: Vec::new(),
            submitted: std::collections::HashMap::new(),
            net: Detector::default(),
            net_events: Vec::new(),
            saw_failed: false,
        }
    }

    /// Record decisions as [`HubNote`]s (developer mode). Off, the hub keeps nothing extra.
    pub fn set_trace(&mut self, on: bool) {
        self.trace = on;
        if !on {
            self.notes = Vec::new();
            self.submitted = std::collections::HashMap::new();
        }
    }

    /// Drain the notes recorded since the last call.
    pub fn take_notes(&mut self) -> Vec<HubNote> {
        std::mem::take(&mut self.notes)
    }

    fn note(&mut self, torrent: InfoHash20, slot: u16, now: Millis, kind: NoteKind, next: Option<Millis>, detail: String) {
        if self.notes.len() >= MAX_PENDING_REPORTS {
            self.notes.remove(0);
        }
        let at = self.clock.wall_of(now);
        let next = next.map(|n| self.clock.wall_of(n));
        self.notes.push(HubNote { torrent, slot, at, kind, next, detail });
    }

    /// Per slot: session open, stop requested. Only called while tracing.
    fn snapshot(&self, ih: &InfoHash20) -> Vec<(bool, bool)> {
        self.torrents.get(ih).map_or_else(Vec::new, |t| {
            (0..t.set.len() as u16)
                .map(|i| t.set.machine(i).map_or((false, false), |m| (m.session_open(), m.stopping())))
                .collect()
        })
    }

    /// Where a slot sits in its tier walk, in words.
    fn tier_text(&self, ih: &InfoHash20, slot: u16) -> String {
        let Some(t) = self.torrents.get(ih) else { return String::new() };
        for (ti, tier) in t.set.tiers().iter().enumerate() {
            if let Some(p) = tier.iter().position(|s| *s == slot) {
                return format!("tier {}, position {} of {}", ti + 1, p + 1, tier.len());
            }
        }
        String::new()
    }

    fn derive_notes(&mut self, ih: &InfoHash20, class: InputClass, before: &[(bool, bool)], out: &[SetIntent], now: Millis) {
        let n = self.torrents.get(ih).map_or(0, |t| t.set.len() as u16);
        for slot in 0..n {
            let Some((open_b, stop_b)) = before.get(usize::from(slot)).copied() else { continue };
            let Some(m) = self.torrents.get(ih).and_then(|t| t.set.machine(slot)) else { continue };
            let (open_a, stop_a, failures, regular, retry, wake) = (m.session_open(), m.stopping(), m.failures(), m.regular_at(), m.retry_at(), m.next_wake());
            match class {
                InputClass::Quit if open_b && !stop_b => {
                    self.note(*ih, slot, now, NoteKind::Suspend, None, "quit: one stopped attempt, no retries".into());
                }
                InputClass::Network if open_a && !stop_a => {
                    if failures == 0 && regular.is_some() {
                        self.note(*ih, slot, now, NoteKind::KeptSchedule, regular, "network change or wake: a regular announce is never made earlier, nothing sent".into());
                    } else if failures > 0 {
                        self.note(*ih, slot, now, NoteKind::RetryPulled, retry, format!("network change or wake: retry after failure {failures} rescheduled"));
                    }
                }
                InputClass::Stop | InputClass::Quit => {}
                _ => {
                    if open_b && !stop_b && (stop_a || !open_a) {
                        let stopped_sent = out.iter().any(|si| {
                            si.slot == slot
                                && matches!(&si.intent, Intent::SendAnnounce(r) if r.params.event == Some(Event::Stopped))
                        });
                        let tier = self.tier_text(ih, slot);
                        if stopped_sent {
                            self.note(*ih, slot, now, NoteKind::Retire, None, format!("passed over by BEP 12 failover ({tier}); one stopped sent"));
                        } else {
                            self.note(*ih, slot, now, NoteKind::RetireSilent, None, format!("passed over by BEP 12 failover ({tier}); never acknowledged started, nothing sent"));
                        }
                    } else if !open_b && open_a {
                        let tier = self.tier_text(ih, slot);
                        self.note(*ih, slot, now, NoteKind::SessionOpened, wake, format!("session opened ({tier})"));
                    }
                }
            }
        }
    }

    fn rec_to_wall(&self, r: &TimingRecord) -> TimingRecord {
        let m = |t: Option<Millis>| t.map(|t| self.clock.wall_of(t));
        TimingRecord {
            last_announce_at: m(r.last_announce_at),
            next_allowed_at: m(r.next_allowed_at),
            min_allowed_at: m(r.min_allowed_at),
            ..r.clone()
        }
    }

    fn rec_from_wall(clock: &Clock, r: TimingRecord) -> TimingRecord {
        let m = |t: Option<Millis>| t.map(|t| clock.hub_time(t));
        TimingRecord {
            last_announce_at: m(r.last_announce_at),
            next_allowed_at: m(r.next_allowed_at),
            min_allowed_at: m(r.min_allowed_at),
            // the tracker id is per session; a stored one is ignored
            tracker_id: None,
            ..r
        }
    }

    /// The store (e.g. to inspect it in tests).
    pub fn store(&self) -> &S {
        &self.store
    }
    /// Mutable store access (fault injection in tests).
    pub fn store_mut(&mut self) -> &mut S {
        &mut self.store
    }
    /// The gate, read-only (diagnostics, assertions).
    pub fn gate(&self) -> &HostGate {
        &self.gate
    }
    /// Consume the hub, returning the store (simulated crash: all in-memory
    /// state is lost, persisted state survives).
    pub fn into_store(self) -> S {
        self.store
    }
    /// Number of failed store writes so far.
    pub fn store_errors(&self) -> u64 {
        self.store_errors
    }
    /// The set of a torrent.
    pub fn set(&self, ih: &InfoHash20) -> Option<&TrackerSet> {
        self.torrents.get(ih).map(|t| &t.set)
    }
    /// Is the torrent private?
    pub fn is_private(&self, ih: &InfoHash20) -> Option<bool> {
        self.torrents.get(ih).map(|t| t.set.private)
    }
    /// Number of torrents.
    pub fn torrent_count(&self) -> usize {
        self.torrents.len()
    }

    /// Pay for extra wire requests a transport made on behalf of a released
    /// request (redirect hops: `HttpOutcome::extra_hops`) out of the host's
    /// token bucket.
    pub fn charge_extra(&mut self, host: &str, hops: u32) {
        self.gate.charge_extra(host, hops);
    }

    /// Lift a fail-closed host trip after the error was shown and acknowledged
    /// (a user action). Machines that tripped stay tripped (fail closed) but
    /// end their session on `Stop`/`Pause` like any other, so the way back is:
    /// `reset_host`, `Stop`, wait for [`is_quiescent`](Self::is_quiescent),
    /// [`remove_torrent`](Self::remove_torrent), [`add_torrent`](Self::add_torrent),
    /// `Start`. No restart of the process is needed.
    pub fn reset_host(&mut self, host: &str) {
        self.gate.reset_host(host);
    }

    /// Fail closed on `host` on the engine's request, e.g. when it observed
    /// something no tracker interaction may ever do. Everything queued for the
    /// host is refused; [`reset_host`](Self::reset_host) lifts it again. The
    /// host may be spelled in any form.
    pub fn trip_host(&mut self, host: &str, reason: &str) {
        self.gate.trip_host(host, reason);
    }

    /// Crash-loop breaker hook: delay everything except `stopped` until `t`.
    pub fn hold_until(&mut self, t: Millis) {
        self.gate.hold_until(t);
    }

    /// Register a torrent and restore persisted timing. It does nothing until
    /// `TorrentEvent::Start`.
    pub fn add_torrent(&mut self, spec: TorrentSpec, now: Millis) -> Result<(), HubError> {
        let now = self.clock.norm(now);
        if self.torrents.contains_key(&spec.info_hash) {
            return Err(HubError::Duplicate);
        }
        let mut cfg = MachineConfig::new(spec.info_hash, spec.peer_id, spec.key, spec.port);
        cfg.numwant = spec.numwant;
        cfg.ipv4 = spec.ipv4;
        cfg.ipv6 = spec.ipv6;
        cfg.crypto = spec.crypto;
        cfg.added_complete = spec.added_complete;
        let ih = spec.info_hash;
        // One shared configuration object per distinct value (in practice one
        // or two per hub): the engine uses the same identity for all torrents.
        let mut shared = cfg.shared();
        shared.held = self.net.off.is_some();
        let shared = match self.shared_pool.get(&shared) {
            Some(a) => a.clone(),
            None => {
                let a = Arc::new(shared);
                self.shared_pool.insert(a.clone());
                a
            }
        };
        let store = &self.store;
        let clock = self.clock;
        let mut set = TrackerSet::with_shared(
            shared,
            ih,
            spec.tiers,
            |url| {
                store
                    .load(&TimingKey {
                        info_hash: ih,
                        tracker_url: url.raw().to_owned(),
                    })
                    .map(|r| Self::rec_from_wall(&clock, r))
            },
            &mut self.rng,
        );
        let pool = &mut self.url_pool;
        set.share_urls(|u| match pool.get(u.raw()) {
            Some(p) => p.0.clone(),
            None => {
                pool.insert(PoolUrl(u.clone()));
                u
            }
        });
        let flags: Vec<bool> = (0..set.len() as u16)
            .map(|i| set.machine(i).is_some_and(|m| m.completed_sent()))
            .collect();
        self.gate.register_torrent(ih, set.urls(), &flags, now);
        set.private = spec.private;
        self.torrents.insert(
            ih,
            TorrentEntry {
                set,
                wake: u64::MAX,
            },
        );
        Ok(())
    }

    /// Add a tracker to a registered torrent as a tier of its own (a duplicate add merged it in).
    /// A running session announces `started` to it through the normal host gate at once; its
    /// timing record is restored when the store has one. False when the torrent is unknown or
    /// has no tracker yet, or the URL is a duplicate (or its http/https twin), or the set is full.
    pub fn add_tracker(&mut self, ih: &InfoHash20, url: TrackerUrl, stats: Option<Stats>, role: TorrentRole, now: Millis) -> bool {
        let now = self.clock.norm(now);
        let Some(t) = self.torrents.get_mut(ih) else { return false };
        let Some(shared) = t.set.shared_config().cloned() else { return false };
        let key = TimingKey { info_hash: *ih, tracker_url: url.raw().to_owned() };
        let clock = self.clock;
        let rec = self.store.load(&key).map(|r| Self::rec_from_wall(&clock, r));
        let Some(slot) = t.set.add_tracker(shared, *ih, url, rec, &mut self.rng) else { return false };
        let pool = &mut self.url_pool;
        t.set.share_urls(|u| match pool.get(u.raw()) {
            Some(p) => p.0.clone(),
            None => {
                pool.insert(PoolUrl(u.clone()));
                u
            }
        });
        let Some(u) = t.set.url(slot).cloned() else { return false };
        self.gate.add_url(ih, u, now);
        // The new machine learns what the others know before it builds its first request.
        if let Some(st) = stats {
            self.dispatch(ih, SetInput::Slot { slot, input: Input::Stats(st) }, now);
        }
        self.dispatch(ih, SetInput::Slot { slot, input: Input::SetRole(role) }, now);
        self.dispatch(ih, SetInput::Timer, now);
        true
    }

    /// Remove a torrent. Refused (false) while it still has an open session or
    /// requests in flight: send `Stop` and wait for [`is_quiescent`](Self::is_quiescent).
    pub fn remove_torrent(&mut self, ih: &InfoHash20) -> bool {
        match self.torrents.get(ih) {
            Some(t) if t.set.is_quiescent() => {}
            _ => return false,
        }
        if let Some(t) = self.torrents.remove(ih) {
            if let Some(w) = t.wake() {
                self.wake_index.remove(&(w, *ih));
            }
            self.gate.unregister_torrent(ih);
            // Forget pooled URLs nobody else uses (the pool and `urls` hold two).
            let urls = t.set.urls();
            let shared = t.set.shared_config().cloned();
            drop(t);
            if let Some(sh) = shared {
                // pool + `sh`
                if Arc::strong_count(&sh) == 2 {
                    self.shared_pool.remove(&*sh);
                }
            }
            for u in urls {
                if Arc::strong_count(&u) == 2 {
                    self.url_pool.remove(u.raw());
                }
            }
        }
        true
    }

    /// True when nothing is open or pending for the torrent.
    pub fn is_quiescent(&self, ih: &InfoHash20) -> bool {
        self.torrents.get(ih).is_none_or(|t| t.set.is_quiescent())
    }

    /// Lifecycle event from the engine.
    pub fn torrent_event(&mut self, ih: &InfoHash20, ev: TorrentEvent, now: Millis) {
        let now = self.clock.norm(now);
        self.dispatch(ih, SetInput::Torrent(ev), now);
    }

    /// The process is quitting: switch the gate to the quit rules for `stopped` (up to
    /// `GateConfig::quit_max_in_flight` at once over all hosts, outside the per-host cap and the
    /// token bucket; see [`HostGate::begin_quit`]). Call it once, before the [`Self::quit`] calls.
    /// Regular announces keep their pacing.
    pub fn begin_quit(&mut self) {
        self.gate.begin_quit();
    }

    /// The process is quitting: `stopped` to every tracker with an open session, one attempt each
    /// (see [`SetInput::Quit`]). A `stopped` already under way keeps its own retry rules.
    pub fn quit(&mut self, ih: &InfoHash20, now: Millis) {
        let now = self.clock.norm(now);
        self.dispatch(ih, SetInput::Quit, now);
    }

    /// Fresh counters for a torrent. Push before timers fire.
    pub fn set_stats(&mut self, ih: &InfoHash20, stats: Stats, now: Millis) {
        let now = self.clock.norm(now);
        self.dispatch(ih, SetInput::Stats(stats), now);
    }

    /// Tell the hub what the torrent is doing, which sets the pace of retries
    /// after transient failures (SPEC 7C.4; see [`TorrentRole`]): call it when a
    /// download finishes (`Seeding`), when a seed becomes dormant or popular
    /// again (`SeedingDormant` / `Seeding`) and when a seed turns into a
    /// download again (`Downloading`, which may pull a pending retry in). The
    /// default is `Downloading`; the role is not persisted, set it after
    /// `add_torrent`. Regular announces are not affected.
    pub fn set_role(&mut self, ih: &InfoHash20, role: TorrentRole, now: Millis) {
        let now = self.clock.norm(now);
        self.dispatch(ih, SetInput::Role(role), now);
    }

    /// The port peers can reach us on changed (the port mapper renewed with another external
    /// port, or lost its mapping and fell back to the listen port). Every torrent's *next*
    /// announce carries `port`; nothing is sent now and no schedule moves, so trackers see
    /// no extra announces (SPEC 7C.3: a regular announce is never made earlier). A request
    /// already released keeps the port it was built with. `0` is ignored.
    pub fn set_port(&mut self, port: u16) {
        if port == 0 {
            return;
        }
        self.update_shared(|old| (old.port != port).then(|| Shared { port, ..old.clone() }));
    }

    /// The encryption policy changed (`network.encryption`). Every torrent's *next* announce
    /// carries `supportcrypto` / `requirecrypto` accordingly (UDP announces carry nothing);
    /// nothing is sent now and no schedule moves, like [`Self::set_port`].
    pub fn set_crypto(&mut self, crypto: Crypto) {
        self.update_shared(|old| (old.crypto != crypto).then(|| Shared { crypto, ..old.clone() }));
    }

    /// Give every torrent a new shared configuration derived from its current one (`None` = keep).
    fn update_shared(&mut self, f: impl Fn(&Shared) -> Option<Shared>) {
        for k in self.sorted_keys() {
            let Some(t) = self.torrents.get_mut(&k) else { continue };
            let Some(old) = t.set.shared_config().cloned() else { continue };
            let Some(new) = f(&old) else { continue };
            let new = match self.shared_pool.get(&new) {
                Some(a) => a.clone(),
                None => {
                    let a = Arc::new(new);
                    self.shared_pool.insert(a.clone());
                    a
                }
            };
            t.set.replace_shared(&new);
            // Forget the old value when no torrent uses it any more (pool + `old`).
            if Arc::strong_count(&old) == 2 {
                self.shared_pool.remove(&*old);
            }
        }
    }

    /// User "re-announce".
    pub fn manual_reannounce(&mut self, ih: &InfoHash20, now: Millis) {
        let now = self.clock.norm(now);
        self.dispatch(ih, SetInput::Manual, now);
    }

    /// User "Announce anyway": also past the tracker's minimum interval (once per tracker per 5 min).
    pub fn manual_reannounce_forced(&mut self, ih: &InfoHash20, now: Millis) {
        let now = self.clock.norm(now);
        self.dispatch(ih, SetInput::ManualForce, now);
    }

    /// Network path changed: applies to every torrent. Only announces that
    /// already failed (or are in backoff) are rescheduled; a regular announce is
    /// never made earlier by this (SPEC 7C.3 #1).
    pub fn network_changed(&mut self, now: Millis) {
        let now = self.clock.norm(now);
        let keys = self.sorted_keys();
        for k in keys {
            self.dispatch(&k, SetInput::NetworkChanged, now);
        }
    }

    /// All torrents' info-hashes in a fixed (sorted) order, so that a
    /// notification to every torrent is processed deterministically.
    fn sorted_keys(&self) -> Vec<InfoHash20> {
        let mut keys: Vec<InfoHash20> = self.torrents.keys().copied().collect();
        keys.sort_unstable();
        keys
    }

    /// Woke from sleep: applies to every torrent. Same rule as
    /// [`network_changed`](Self::network_changed).
    pub fn wake(&mut self, now: Millis) {
        let now = self.clock.norm(now);
        let keys = self.sorted_keys();
        for k in keys {
            self.dispatch(&k, SetInput::Wake, now);
        }
    }


    // ------------------------------------------------------------- outages (SPEC 7C.4c)

    /// Connectivity for the UI.
    pub fn net_view(&self) -> NetView {
        match &self.net.off {
            Some(o) if o.reason != Reason::Wake => NetView {
                online: false,
                since: Some(self.clock.wall_of(o.since)),
                reason: Some(o.reason),
                probes: o.probes,
            },
            _ => NetView { online: true, since: None, reason: None, probes: 0 },
        }
    }

    /// Drain what the detector decided (for the developer log and the engine's other parts).
    pub fn take_net_events(&mut self) -> Vec<NetEvent> {
        std::mem::take(&mut self.net_events)
    }

    /// The OS says whether a usable network path exists. Unsatisfied: hold everything and
    /// send nothing, not even a probe, until it changes. Satisfied while offline: one probe now.
    pub fn path_changed(&mut self, satisfied: bool, now: Millis) {
        let now = self.clock.norm(now);
        if !satisfied {
            self.net.path_down = true;
            if self.net.off.is_none() {
                self.enter_offline(Reason::Path, now, true);
            } else if let Some(o) = self.net.off.as_mut() {
                o.reason = Reason::Path;
                o.probe_at = None;
            }
            return;
        }
        let was_down = std::mem::replace(&mut self.net.path_down, false);
        let earliest = self.net.last_probe + HINT_DEBOUNCE;
        if let Some(o) = self.net.off.as_mut() {
            if was_down {
                o.step = 0;
                o.trusted = true;
            }
            if o.probe.is_none() {
                // Right away; a flapping interface cannot probe more than once per debounce.
                o.probe_at = Some(now.max(earliest.min(now + HINT_DEBOUNCE)));
            }
        }
    }

    /// The machine woke from sleep. Everything is held; after the OS reports a path (or
    /// [`WAKE_WAIT`]) one probe decides whether the announces that came due go out, spread.
    pub fn did_wake(&mut self, now: Millis) {
        let now = self.clock.norm(now);
        match self.net.off.as_mut() {
            None => self.enter_offline(Reason::Wake, now, false),
            Some(o) => {
                o.step = 0;
                o.trusted = true;
                if !self.net.path_down && o.probe.is_none() {
                    let at = now + WAKE_WAIT;
                    o.probe_at = Some(o.probe_at.map_or(at, |p| p.min(at)));
                }
            }
        }
    }

    /// A sign of life from outside the tracker code (a peer handshake, a DHT reply): failures
    /// so far are not evidence of an outage. Cheap; the engine passes its latest timestamp.
    pub fn note_alive(&mut self, at: Millis) {
        // (A reading from the past stays in the past: no clock normalisation here.)
        let at = self.clock.hub_time(at);
        if at > self.net.last_ok {
            self.net.alive(at);
        }
    }

    /// An unsolicited sign of life (an inbound connection, a DHT packet): while offline it
    /// triggers a probe right away, at most once per [`HINT_DEBOUNCE`].
    pub fn net_hint(&mut self, now: Millis) {
        let now = self.clock.norm(now);
        self.net.alive(now);
        let ready = now.since(self.net.last_probe) >= HINT_DEBOUNCE && !self.net.path_down;
        if let Some(o) = self.net.off.as_mut()
            && ready && o.probe.is_none() {
                o.trusted = true;
                o.probe_at = Some(o.probe_at.map_or(now, |p| p.min(now)));
            }
    }

    fn probe_wake(&self) -> Option<Millis> {
        let o = self.net.off.as_ref()?;
        match (o.probe_at, o.probe) {
            (Some(p), _) => Some(p),
            (None, Some((_, at))) => Some(at + PROBE_DEADLINE),
            _ => None,
        }
    }

    /// Raise or lower the hub-wide hold (one shared value, no per-machine state).
    fn set_held(&mut self, held: bool) {
        self.update_shared(|old| (old.held != held).then(|| Shared { held, ..old.clone() }));
    }

    fn push_net(&mut self, e: NetEvent) {
        // The engine drains these every loop; the cap only guards a caller that never does.
        if self.net_events.len() >= 256 {
            self.net_events.remove(0);
        }
        self.net_events.push(e);
    }

    fn broadcast(&mut self, input: &SetInput, now: Millis) {
        for k in self.sorted_keys() {
            self.dispatch(&k, input.clone(), now);
        }
    }

    fn failure_sum(&self, ih: &InfoHash20) -> u32 {
        self.torrents.get(ih).map_or(0, |t| (0..t.set.len() as u16).filter_map(|i| t.set.machine(i)).map(|m| m.failures()).sum())
    }

    fn count_due(&self, now: Millis) -> u32 {
        self.torrents
            .values()
            .map(|t| (0..t.set.len() as u16).filter_map(|i| t.set.machine(i)).filter(|m| m.held_due(now)).count() as u32)
            .sum()
    }

    fn enter_offline(&mut self, reason: Reason, now: Millis, forgive: bool) {
        if self.net.off.is_some() {
            return;
        }
        self.set_held(true);
        self.broadcast(&SetInput::Hold, now);
        let mut forgiven = 0;
        if forgive {
            let since = Millis(now.0.saturating_sub(INFER_WINDOW.as_millis() as u64));
            for k in self.sorted_keys() {
                let before = self.failure_sum(&k);
                if before == 0 {
                    continue;
                }
                self.dispatch(&k, SetInput::Forgive { since }, now);
                if self.failure_sum(&k) < before {
                    forgiven += 1;
                }
            }
        }
        // The failures that revealed the outage opened breakers that must not outlive it.
        self.gate.reset_breakers();
        self.net.nfails = 0;
        let mut off = Off { since: now, reason, step: 0, probe_at: None, probe: None, probes: 0, trusted: reason != Reason::Inference };
        match reason {
            Reason::Path => {}
            Reason::Wake => {
                if !self.net.path_down {
                    off.probe_at = Some(now + WAKE_WAIT);
                }
            }
            Reason::Inference => {
                let r = self.rng.next_u64();
                off.probe_at = Some(now + jittered(probe_delay(0), r));
                off.step = 1;
            }
        }
        self.net.off = Some(off);
        let due = self.count_due(now);
        let at = self.clock.wall_of(now);
        self.push_net(NetEvent::Offline { at, reason, forgiven, due });
    }

    /// The best announce to test with: one that is due, on the host that answered last.
    fn pick_probe(&self, now: Millis) -> Option<(InfoHash20, u16)> {
        // Best: the host that answered most recently; ties by (torrent, slot) for determinism.
        let mut best: Option<(Millis, std::cmp::Reverse<ReqKey>)> = None;
        for (ih, t) in &self.torrents {
            for i in 0..t.set.len() as u16 {
                let Some(m) = t.set.machine(i) else { continue };
                if !m.held_due(now) {
                    continue;
                }
                let ok = t.set.url(i).and_then(|u| self.gate.host_last_ok(u.host())).unwrap_or(Millis::ZERO);
                let rank = (ok, std::cmp::Reverse((*ih, i)));
                if best.as_ref().is_none_or(|b| rank > *b) {
                    best = Some(rank);
                }
            }
        }
        best.map(|b| b.1.0)
    }

    fn run_probe(&mut self, now: Millis) {
        let Some(o) = self.net.off.as_ref() else { return };
        if let Some((_, at)) = o.probe {
            if now.since(at) >= PROBE_DEADLINE {
                self.probe_failed(now);
            }
            return;
        }
        let Some(p) = o.probe_at else { return };
        if now < p {
            return;
        }
        if self.net.path_down {
            if let Some(o) = self.net.off.as_mut() {
                o.probe_at = None;
            }
            return;
        }
        match self.pick_probe(now) {
            Some(key) => {
                let host = self.torrents.get(&key.0).and_then(|t| t.set.url(key.1)).map(|u| u.host().to_owned()).unwrap_or_default();
                self.dispatch(&key.0, SetInput::Slot { slot: key.1, input: Input::Probe }, now);
                self.net.last_probe = now;
                let o = self.net.off.as_mut().expect("offline");
                o.probes += 1;
                o.probe_at = None;
                o.probe = Some((key, now));
                let n = o.probes;
                let at = self.clock.wall_of(now);
                self.push_net(NetEvent::Probe { at, torrent: key.0, slot: key.1, host, n });
            }
            None => {
                let at = self.clock.wall_of(now);
                self.push_net(NetEvent::ProbeSkipped { at });
                if self.net.off.as_ref().is_some_and(|o| o.trusted) {
                    // The OS (or a sign of life) says the network is back and nothing is
                    // waiting to be announced: there is nothing to test with.
                    self.go_online(now);
                } else {
                    self.schedule_next_probe(now);
                }
            }
        }
    }

    fn schedule_next_probe(&mut self, now: Millis) {
        let r = self.rng.next_u64();
        let path_down = self.net.path_down;
        let Some(o) = self.net.off.as_mut() else { return };
        o.probe = None;
        o.trusted = false;
        if path_down {
            o.probe_at = None;
            return;
        }
        let d = probe_delay(o.step);
        o.step = o.step.saturating_add(1);
        o.probe_at = Some(now + jittered(d, r));
    }

    fn probe_failed(&mut self, now: Millis) {
        self.schedule_next_probe(now);
        let promoted = match self.net.off.as_mut() {
            Some(o) if o.reason == Reason::Wake => {
                o.reason = Reason::Inference;
                Some(o.since)
            }
            _ => None,
        };
        if promoted.is_some() {
            let at = self.clock.wall_of(now);
            let due = self.count_due(now);
            self.push_net(NetEvent::Offline { at, reason: Reason::Inference, forgiven: 0, due });
        }
    }

    fn go_online(&mut self, now: Millis) {
        let Some(off) = self.net.off.take() else { return };
        self.net.alive(now);
        self.gate.reset_breakers();
        // The probe is one of the announces that came due.
        let due = self.count_due(now) + u32::from(off.probe.is_some());
        self.set_held(false);
        self.broadcast(&SetInput::Release { spread: RELEASE_SPREAD }, now);
        let at = self.clock.wall_of(now);
        self.push_net(NetEvent::Online { at, after: now.since(off.since), due, spread: RELEASE_SPREAD, reason: off.reason });
    }

    /// Feed an announce result to its machine and keep the detector informed.
    fn deliver(
        &mut self,
        ih: InfoHash20,
        slot: u16,
        id: crate::types::RequestId,
        event: Option<Event>,
        result: Result<AnnounceResponse, AnnounceError>,
        now: Millis,
    ) -> bool {
        let ok = result.is_ok();
        let (answered, unreachable, network) = match &result {
            Ok(_) => (true, false, false),
            Err(e) => (e.answered(), e.is_unreachable(), e.is_network()),
        };
        let forgive = self.net.off.is_some() && network && event != Some(Event::Stopped);
        let input = match result {
            Err(error) if forgive => Input::Unreachable { id, error },
            result => Input::Response { id, result },
        };
        self.saw_failed = false;
        let applied = self.feed_slot(&ih, slot, input, now);
        if ok && applied && event != Some(Event::Stopped) && self.net.off.is_none() {
            self.host_recovered(ih, slot, now);
        }
        if event != Some(Event::Stopped) {
            // A failure the machine did not count (it came after a success for the same
            // request) is no evidence of anything.
            let unreachable = unreachable && self.saw_failed;
            self.after_result(ih, slot, answered, unreachable, now);
        }
        applied
    }

    /// An announce to this host worked: every other (torrent, tracker) waiting in backoff on the
    /// same host after a host-level failure gets its retry pulled forward (once, jittered; the
    /// gate and the machines' floors still apply). Cheap: only machines with failures are looked at.
    fn host_recovered(&mut self, ih: InfoHash20, slot: u16, now: Millis) {
        let Some(host) = self.torrents.get(&ih).and_then(|t| t.set.url(slot)).map(|u| u.host().to_owned()) else {
            return;
        };
        let mut hits: Vec<(InfoHash20, u16)> = Vec::new();
        for (k, t) in &self.torrents {
            for sl in 0..t.set.len() as u16 {
                if (*k, sl) == (ih, slot) {
                    continue;
                }
                if t.set.machine(sl).is_some_and(|m| m.failures() > 0)
                    && t.set.url(sl).is_some_and(|u| u.host() == host)
                {
                    hits.push((*k, sl));
                }
            }
        }
        hits.sort_unstable();
        for (k, sl) in hits {
            self.feed_slot(&k, sl, Input::HostRecovered, now);
        }
    }

    fn after_result(&mut self, ih: InfoHash20, slot: u16, answered: bool, unreachable: bool, now: Millis) {
        if answered {
            self.net.alive(now);
            if self.net.off.is_some() {
                self.go_online(now);
            }
            return;
        }
        if let Some(o) = self.net.off.as_ref() {
            if o.probe.is_some_and(|(k, _)| k == (ih, slot)) {
                self.probe_failed(now);
            }
            return;
        }
        if !unreachable {
            return;
        }
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // Two distinct hosts failing is the evidence. With a single host in use that is all
        // there is to see, so the evidence is three distinct torrents failing (the gate's
        // breaker threshold): one torrent's tracker being down is not an outage.
        let one_host = self.gate.host_count() <= 1;
        if one_host {
            (ih, slot).hash(&mut h);
        } else {
            self.torrents.get(&ih).and_then(|t| t.set.url(slot)).map(|u| u.host()).hash(&mut h);
        }
        let distinct = self.net.record_failure(h.finish(), now);
        let need = if one_host { 3 } else { 2 };
        if distinct >= need {
            self.enter_offline(Reason::Inference, now, true);
        }
    }

    /// Returns true when the machines applied a successful announce answer.
    fn dispatch(&mut self, ih: &InfoHash20, input: SetInput, now: Millis) -> bool {
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        let traced = self.trace.then(|| (class_of(&input), self.snapshot(ih)));
        if let Some(t) = self.torrents.get_mut(ih) {
            t.set.handle(input, now, &mut out);
        } else {
            self.scratch = out;
            return false;
        }
        if let Some((class, before)) = traced {
            self.derive_notes(ih, class, &before, &out, now);
        }
        let announced = self.apply(ih, &mut out, now);
        self.scratch = out;
        self.refresh_wake(ih);
        announced
    }

    fn refresh_wake(&mut self, ih: &InfoHash20) {
        let Some(t) = self.torrents.get_mut(ih) else {
            return;
        };
        let new = t.set.next_wake();
        if new != t.wake() {
            if let Some(old) = t.wake() {
                self.wake_index.remove(&(old, *ih));
            }
            if let Some(n) = new {
                self.wake_index.insert((n, *ih));
            }
            t.set_wake(new);
        }
    }

    /// Apply machine intents: persist, hand requests to the gate, collect reports.
    /// Returns true if a successful announce answer was applied.
    fn apply(&mut self, ih: &InfoHash20, intents: &mut Vec<SetIntent>, now: Millis) -> bool {
        let mut refused: Vec<(u16, crate::types::RequestId)> = Vec::new();
        let mut announced = false;
        for si in intents.drain(..) {
            match si.intent {
                Intent::SendAnnounce(req) => {
                    let Some(url) = self
                        .torrents
                        .get(ih)
                        .and_then(|t| t.set.url(si.slot))
                        .cloned()
                    else {
                        continue;
                    };
                    let key = (*ih, si.slot);
                    if self
                        .gate
                        .submit(key, req.id, url, req.params, req.kind, now)
                        .is_err()
                    {
                        refused.push((si.slot, req.id));
                    } else if self.trace {
                        self.submitted.insert(key, (now, false));
                    }
                }
                Intent::CancelRequest(id) => {
                    self.gate.cancel((*ih, si.slot), id);
                }
                Intent::ScheduleWake(_) => {}
                Intent::Persist(rec) => {
                    let _ = self.save_record(ih, si.slot, &rec);
                }
                Intent::Report(r) => {
                    if r == Report::SessionClosed {
                        self.gate.session_closed((*ih, si.slot));
                    }
                    if matches!(r, Report::Announced { .. }) {
                        announced = true;
                    }
                    if matches!(r, Report::Failed { .. }) {
                        self.saw_failed = true;
                    }
                    let r = self.report_to_wall(r);
                    if self.reports.len() >= MAX_PENDING_REPORTS {
                        self.reports.pop_front();
                    }
                    self.reports.push_back(HubReport {
                        torrent: *ih,
                        slot: si.slot,
                        report: r,
                    });
                }
            }
        }
        for (slot, id) in refused {
            if self.trace {
                self.note(*ih, slot, now, NoteKind::Refused, None, "the gate refused the request; this tracker stops announcing".into());
            }
            self.feed_slot(ih, slot, Input::GateRefused(id), now);
        }
        announced
    }

    /// Write a machine's record to the store (key built on demand: it holds the
    /// passkey URL, which is not worth a permanent copy per pair).
    fn save_record(&mut self, ih: &InfoHash20, slot: u16, rec: &TimingRecord) -> Result<(), ()> {
        let Some(url) = self.torrents.get(ih).and_then(|t| t.set.url(slot)) else {
            return Ok(());
        };
        let key = TimingKey {
            info_hash: *ih,
            tracker_url: url.raw().to_owned(),
        };
        let rec = self.rec_to_wall(rec);
        if self.store.save(&key, &rec).is_err() {
            self.store_errors += 1;
            return Err(());
        }
        Ok(())
    }

    fn report_to_wall(&self, r: Report) -> Report {
        match r {
            Report::Failed {
                error,
                consecutive,
                retry_at,
            } => Report::Failed {
                error,
                consecutive,
                retry_at: self.clock.wall_of(retry_at),
            },
            Report::ManualDeferred { until } => Report::ManualDeferred {
                until: self.clock.wall_of(until),
            },
            other => other,
        }
    }

    fn feed_slot(&mut self, ih: &InfoHash20, slot: u16, input: Input, now: Millis) -> bool {
        self.dispatch(ih, SetInput::Slot { slot, input }, now)
    }

    /// Advance to `now`: expire stuck requests, fire due timers, and release
    /// every request the gate allows. The returned requests must be executed;
    /// each must eventually be reported with [`complete`](Self::complete).
    pub fn tick(&mut self, now: Millis) -> Vec<GatedRequest> {
        let now = self.clock.norm(now);
        // 0. an outage: the probe is due
        if self.net.off.is_some() {
            self.run_probe(now);
        }
        // 1. requests that outlived their deadline become timeouts
        for t in self.gate.expire(now) {
            let key = t.key();
            self.deliver(key.0, key.1, t.id(), t.event(), Err(AnnounceError::Transport(TransportError::Timeout)), now);
        }
        // 2. due timers
        while let Some(&(at, ih)) = self.wake_index.iter().next() {
            if at > now {
                break;
            }
            self.wake_index.remove(&(at, ih));
            if let Some(t) = self.torrents.get_mut(&ih) {
                t.set_wake(None);
            }
            self.dispatch(&ih, SetInput::Timer, now);
        }
        // 3. release
        let mut out = Vec::new();
        for req in self.gate.poll(now) {
            if self.trace {
                let (ih, slot) = req.ticket().key();
                if let Some((at, deferred)) = self.submitted.remove(&(ih, slot))
                    && deferred && now > at {
                        let waited = now.since(at).as_millis();
                        self.note(ih, slot, now, NoteKind::Released, None, format!("released by the gate after waiting {waited} ms"));
                    }
            }
            if self.mark_dispatched(&req, now) {
                out.push(req);
            }
        }
        if self.trace && !self.submitted.is_empty() {
            self.note_deferred(now);
        }
        // 4. gate events
        for ev in self.gate.take_events() {
            if let GateEvent::HostTripped { dropped, .. } = &ev {
                for (key, id) in dropped.clone() {
                    self.feed_slot(&key.0, key.1, Input::GateRefused(id), now);
                }
            }
            if self.gate_log.len() >= 1000 {
                self.gate_log.pop_front();
            }
            let ev = match ev {
                GateEvent::HostPaused { host, until } => GateEvent::HostPaused {
                    host,
                    until: self.clock.wall_of(until),
                },
                other => other,
            };
            self.gate_log.push_back(ev);
        }
        out
    }

    /// Notes, once, for the requests that are still queued at the gate after a poll.
    fn note_deferred(&mut self, now: Millis) {
        let mut keys: Vec<(InfoHash20, u16)> = self.submitted.iter().filter(|(_, (_, noted))| !noted).map(|(k, _)| *k).collect();
        keys.sort_unstable();
        for key in keys {
            if !self.gate.is_queued(&key) {
                self.submitted.remove(&key);
                continue;
            }
            let host = self.torrents.get(&key.0).and_then(|t| t.set.url(key.1)).map(|u| u.host().to_owned()).unwrap_or_default();
            let why = if self.gate.is_tripped(&host) {
                "host tripped by the gate".to_string()
            } else if self.gate.breaker_open(&host, now) {
                "host circuit breaker is open".to_string()
            } else if self.gate.in_flight(&host) > 0 {
                format!("per-host limit ({} request(s) in flight)", self.gate.in_flight(&host))
            } else {
                "per-host token bucket (1 request/s, burst 3)".to_string()
            };
            if let Some(e) = self.submitted.get_mut(&key) {
                e.1 = true;
            }
            self.note(key.0, key.1, now, NoteKind::Deferred, None, format!("request held back by the rate gate: {why}"));
        }
    }

    /// Tell the machine its request is on the wire; persist before it leaves.
    ///
    /// Returns false (and does not release the request) if a durable write that
    /// the protocol guarantees depend on failed: the `completed` flag (at-most-once)
    /// and the open-session flag written with `started` (crash safety). Nothing
    /// was sent, so this is **not** a reason to trip: the machine is told
    /// `NotSent`, treats it as an ordinary failure that provably never reached
    /// the tracker, backs off and tries again; a `completed` is owed again.
    /// Without a durable write nothing is released, as before.
    fn mark_dispatched(&mut self, req: &GatedRequest, now: Millis) -> bool {
        let t = req.ticket();
        let (ih, slot) = t.key();
        if req.announce_params().is_none() {
            return true; // scrape: no machine involved
        }
        let mut out = std::mem::take(&mut self.scratch);
        out.clear();
        if let Some(e) = self.torrents.get_mut(&ih) {
            e.set.handle(
                SetInput::Slot {
                    slot,
                    input: Input::Dispatched(t.id()),
                },
                now,
                &mut out,
            );
        }
        // Persist intents are applied first, in order, before anything else.
        let mut ok = true;
        for si in &out {
            if let Intent::Persist(rec) = &si.intent
                && self.save_record(&ih, si.slot, rec).is_err()
                    && (req.event() == Some(Event::Completed)
                        || req.event() == Some(Event::Started))
                {
                    ok = false;
                }
        }
        out.retain(|si| !matches!(si.intent, Intent::Persist(_)));
        self.apply(&ih, &mut out, now);
        self.scratch = out;
        if !ok {
            self.gate.finish(t, Outcome::Unsent, now);
            self.feed_slot(&ih, slot, Input::NotSent(t.id()), now);
        }
        self.refresh_wake(&ih);
        ok
    }

    /// A released announce finished. Updates the gate (bucket slot, circuit
    /// breaker, audit) and the machine.
    ///
    /// A transport must report an error that says the request **never reached
    /// the tracker** (`AnnounceError::never_reached_tracker`: DNS, connect, TLS
    /// handshake, policy refusal) only if nothing was written; a `completed`
    /// that ends this way is sent again with the retry, any other failure of a
    /// `completed` is not (the tracker may have counted it).
    pub fn complete(
        &mut self,
        ticket: Ticket,
        result: Result<AnnounceResponse, AnnounceError>,
        now: Millis,
    ) {
        let now = self.clock.norm(now);
        let timing = result.as_ref().ok().map(|r| r.timing().sanitized());
        // During an outage the network's failures say nothing about the host: no breaker.
        let forgive = self.net.off.is_some()
            && ticket.event() != Some(Event::Stopped)
            && matches!(&result, Err(e) if e.is_network());
        let outcome = match &result {
            Ok(_) => Outcome::Success {
                timing: timing.expect("success has timing"),
            },
            Err(e) if forgive => {
                if e.never_reached_tracker() {
                    Outcome::Unsent
                } else {
                    Outcome::Neutral
                }
            }
            Err(e) if e.never_reached_tracker() => {
                if e.is_host_failure() {
                    Outcome::HostFailureUnsent
                } else {
                    Outcome::Unsent
                }
            }
            Err(e) if e.is_host_failure() => Outcome::HostFailure,
            Err(_) => Outcome::HostAlive,
        };
        let known = self.gate.finish(ticket, outcome, now);
        let (ih, slot) = ticket.key();
        let applied = self.deliver(ih, slot, ticket.id(), ticket.event(), result, now);
        if let (false, true, Some(timing)) = (known, applied, timing) {
            // A reply after the deadline that the machine accepted (it undoes
            // the timeout): the auditor must agree with the machine.
            self.gate.confirm_late_success(ticket, timing, now);
        }
    }

    /// A released scrape finished (the engine parsed the body itself).
    pub fn complete_scrape(&mut self, ticket: Ticket, ok: bool, now: Millis) {
        let now = self.clock.norm(now);
        self.gate.finish(
            ticket,
            if ok {
                Outcome::HostAlive
            } else {
                Outcome::HostFailure
            },
            now,
        );
    }

    /// Ask for a scrape of one tracker (BEP 48). Never automatic; rate-limited
    /// by the gate (no more often than the tracker's announce interval, at
    /// least every 30 min); refused while an announce for the pair is pending.
    pub fn request_scrape(
        &mut self,
        ih: &InfoHash20,
        slot: u16,
        now: Millis,
    ) -> Result<(), SubmitError> {
        let now = self.clock.norm(now);
        let url = self
            .torrents
            .get(ih)
            .and_then(|t| t.set.url(slot))
            .cloned()
            .ok_or(SubmitError::UnknownTorrent)?;
        let id = crate::types::RequestId(u64::MAX - self.rng.below(1 << 20));
        self.gate
            .submit_scrape((*ih, slot), id, url, vec![*ih], now)
    }

    /// Earliest time the hub needs `tick` again (wall clock time line, like
    /// `now`).
    pub fn next_wake(&self, now: Millis) -> Option<Millis> {
        let (t, shift) = self.clock.peek(now);
        let a = self.wake_index.iter().next().map(|(t, _)| *t);
        let b = self.gate.next_wake(t);
        let w = match (a, b) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        };
        let w = match (w, self.probe_wake()) {
            (Some(x), Some(y)) => Some(x.min(y)),
            (x, y) => x.or(y),
        };
        w.map(|x| Millis(x.0.saturating_sub(shift)))
    }

    /// Drain reports for the UI/log.
    pub fn take_reports(&mut self) -> Vec<HubReport> {
        self.reports.drain(..).collect()
    }

    /// Drain gate events (breaker opened/closed, host tripped).
    pub fn take_gate_events(&mut self) -> Vec<GateEvent> {
        self.gate_log.drain(..).collect()
    }

    /// How long until a manual re-announce would be honoured, for the UI.
    pub fn manual_wait(&self, ih: &InfoHash20, now: Millis) -> Option<Duration> {
        let (now, _) = self.clock.peek(now);
        let t = self.torrents.get(ih)?;
        let mut best: Option<Duration> = None;
        for i in 0..t.set.len() as u16 {
            let m = t.set.machine(i)?;
            if m.session_open() {
                let r = m
                    .record()
                    .min_allowed_at
                    .map_or(Duration::ZERO, |x| x.since(now));
                best = Some(best.map_or(r, |b| b.min(r)));
            }
        }
        best
    }
}
