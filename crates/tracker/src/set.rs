//! `TrackerSet`: BEP 12 tiers for one torrent, built from `AnnounceMachine`s.
//!
//! * Trackers are shuffled **once** within each tier (injected RNG).
//! * **Every tier is independent** (SPEC 7C.4b): nothing in one tier starts, stops or
//!   reschedules a tracker of another. Inside a tier the walk is BEP 12 failover: the
//!   first tracker that is not failing is active, the ones behind it are *retired*
//!   (`Input::Retire`: no request at all if they never acknowledged `started`), the ones
//!   ahead of it keep retrying on their own backoff. A dead first tracker never delays
//!   the second one of its tier.
//! * http/https twins of one tracker in different tiers are folded into one tier.
//! * A tracker that works is promoted to the front of its tier.
//! * One logical announce per (torrent, tracker) per interval, always: the set
//!   never announces to two address families in parallel (SPEC 7C.4a rule 1).

use std::cell::RefCell;
use std::sync::Arc;

use crate::machine::{AnnounceMachine, Health, Input, Intent, MachineConfig, Report, Shared};
use crate::rng::{Rng, shuffle};
use crate::store::TimingRecord;
use crate::time::Millis;
use crate::types::{Stats, TorrentEvent, TorrentRole};
use crate::url::TrackerUrl;

/// Same tracker under http and https: equal host, path and query, different scheme.
pub fn is_scheme_twin(a: &TrackerUrl, b: &TrackerUrl) -> bool {
    use crate::url::Scheme;
    matches!(
        (a.scheme(), b.scheme()),
        (Scheme::Http, Scheme::Https) | (Scheme::Https, Scheme::Http)
    ) && a.host() == b.host()
        && a.path_and_query() == b.path_and_query()
}

/// Fold tiers that belong to one tracker into a single tier (SPEC 7C.4b), so that BEP 12
/// failover applies inside it and the tracker is never announced to in parallel under two
/// names. `group` names the tracker a URL belongs to (the engine passes the user's tracker
/// profile); URLs it returns `None` for stay as they are. A tier belongs to the group of its
/// first grouped URL; a later tier of the same group is appended to the earlier one.
pub fn fold_tiers<G: PartialEq>(
    tiers: Vec<Vec<TrackerUrl>>,
    mut group: impl FnMut(&TrackerUrl) -> Option<G>,
) -> Vec<Vec<TrackerUrl>> {
    let mut out: Vec<(Option<G>, Vec<TrackerUrl>)> = Vec::new();
    for tier in tiers {
        let key = tier.iter().find_map(&mut group);
        match key.as_ref().and_then(|k| out.iter().position(|(g, _)| g.as_ref() == Some(k))) {
            Some(i) => out[i].1.extend(tier),
            None => out.push((key, tier)),
        }
    }
    out.into_iter().map(|(_, t)| t).collect()
}

/// Most trackers a torrent may have; extras are ignored (bounded growth).
pub const MAX_TRACKERS: usize = 32;

/// Inputs addressed to a set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SetInput {
    /// Lifecycle event. `Start`/`Resume` are user actions and clear halts.
    Torrent(TorrentEvent),
    /// The process is quitting: like `Stop`, but each tracker gets a single `stopped` attempt (a
    /// retry would only come after the first backoff step, a minute or more, and quit does not
    /// wait for that).
    Quit,
    /// Fresh counters.
    Stats(Stats),
    /// Time passed.
    Timer,
    /// User re-announce.
    Manual,
    /// User re-announce that also overrides the tracker's minimum interval (rate limited).
    ManualForce,
    /// Network path changed.
    NetworkChanged,
    /// Woke from sleep.
    Wake,
    /// What the torrent is doing now (retry pace, SPEC 7C.4).
    Role(TorrentRole),
    /// An outage was declared: hold every tracker (see [`Input::Hold`]).
    Hold,
    /// The outage is over (see [`Input::Release`]).
    Release {
        /// Window the due announces are spread over.
        spread: std::time::Duration,
    },
    /// Take back network failures from `since` on (see [`Input::Forgive`]).
    Forgive {
        /// Start of the window.
        since: Millis,
    },
    /// Something for one tracker slot: `Dispatched`, `Response`, `GateRefused`.
    Slot {
        /// Slot number (stable for the life of the set).
        slot: u16,
        /// The input.
        input: Input,
    },
}

/// An intent tagged with the slot that produced it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SetIntent {
    /// Tracker slot.
    pub slot: u16,
    /// The machine's intent.
    pub intent: Intent,
}

struct Slot {
    machine: AnnounceMachine,
    url: Arc<TrackerUrl>,
}

thread_local! {
    /// Scratch buffer for the intents of one machine call. Shared by all sets
    /// of a thread so that no set carries (or allocates) one of its own.
    static SCRATCH: RefCell<Vec<Intent>> = const { RefCell::new(Vec::new()) };
}

/// The tiers of a set, as slices of slot numbers in try-order.
#[derive(Clone, Copy, Debug)]
pub struct Tiers<'a> {
    layout: &'a [u16],
}

impl<'a> Tiers<'a> {
    /// Number of tiers.
    pub fn len(&self) -> usize {
        self.layout.first().map_or(0, |n| usize::from(*n))
    }
    /// True when there are no tiers.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Tier `i` (slot numbers in try-order).
    pub fn get(&self, i: usize) -> Option<&'a [u16]> {
        let n = self.len();
        if i >= n {
            return None;
        }
        let start: usize = self.layout[1..1 + i].iter().map(|l| usize::from(*l)).sum();
        let len = usize::from(self.layout[1 + i]);
        let order = &self.layout[1 + n..];
        order.get(start..start + len)
    }
    /// Iterate over the tiers.
    pub fn iter(&self) -> impl Iterator<Item = &'a [u16]> + 'a {
        let t = *self;
        (0..t.len()).filter_map(move |i| t.get(i))
    }
}

impl<'a> IntoIterator for Tiers<'a> {
    type Item = &'a [u16];
    type IntoIter = std::vec::IntoIter<&'a [u16]>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter().collect::<Vec<_>>().into_iter()
    }
}

/// BEP 12 tracker set of one torrent.
///
/// Compact on purpose (one set per torrent, thousands of torrents): the slots
/// and the tier layout are exact-size boxed slices and the machines share one
/// configuration.
pub struct TrackerSet {
    slots: Box<[Slot]>,
    /// `[number of tiers, length of each tier..., slot numbers in try-order]`;
    /// `None` for the trivial layouts (no tracker, or one tracker), which need
    /// no allocation.
    #[allow(clippy::box_collection)] // a thin pointer on purpose: 8 bytes per torrent, not 24
    layout: Option<Box<Vec<u16>>>,
    session_wanted: bool,
    /// BEP 27 private flag (kept for reporting; set by the hub).
    pub(crate) private: bool,
}

impl TrackerSet {
    /// Build a set from announce tiers. Duplicate URLs are dropped. `restore`
    /// supplies the persisted record for a URL. The per-tracker machine config
    /// is derived from `template` (same identity everywhere; jitter seeds differ).
    pub fn new<R: Rng + ?Sized>(
        template: &MachineConfig,
        tiers: Vec<Vec<TrackerUrl>>,
        restore: impl FnMut(&TrackerUrl) -> Option<TimingRecord>,
        rng: &mut R,
    ) -> TrackerSet {
        Self::with_shared(
            Arc::new(template.shared()),
            template.info_hash,
            tiers,
            restore,
            rng,
        )
    }

    /// Like [`new`](Self::new) with an already shared configuration (the hub
    /// pools equal ones).
    pub(crate) fn with_shared<R: Rng + ?Sized>(
        cfg: Arc<Shared>,
        info_hash: crate::types::InfoHash20,
        tiers: Vec<Vec<TrackerUrl>>,
        mut restore: impl FnMut(&TrackerUrl) -> Option<TimingRecord>,
        rng: &mut R,
    ) -> TrackerSet {
        let mut slots: Vec<Slot> = Vec::new();
        let mut tier_slots: Vec<Vec<u16>> = Vec::new();
        // Which tier each slot went into (for the scheme-twin rule below).
        let mut tier_of: Vec<usize> = Vec::new();
        let mut tiers = tiers;
        for tier in tiers.drain(..) {
            let mut ids = Vec::new();
            for url in tier {
                if slots.len() >= MAX_TRACKERS || slots.iter().any(|s| *s.url == url) {
                    continue;
                }
                // Tiers are independent, so the http and https spellings of one tracker in two
                // tiers would both be announced to (two announces per interval from one peer).
                // The later twin becomes a failover of the earlier one's tier instead.
                if let Some(twin) = slots.iter().position(|s| is_scheme_twin(&s.url, &url)) {
                    let seed = rng.next_u64();
                    let rec = restore(&url);
                    let id = slots.len() as u16;
                    slots.push(Slot {
                        machine: AnnounceMachine::with_shared_config(cfg.clone(), info_hash, seed, rec),
                        url: Arc::new(url),
                    });
                    tier_of.push(tier_of[twin]);
                    if tier_of[twin] < tier_slots.len() {
                        tier_slots[tier_of[twin]].push(id);
                    } else {
                        ids.push(id); // twin lives in the tier being built
                    }
                    continue;
                }
                tier_of.push(tier_slots.len());
                let seed = rng.next_u64();
                let rec = restore(&url);
                ids.push(slots.len() as u16);
                slots.push(Slot {
                    machine: AnnounceMachine::with_shared_config(cfg.clone(), info_hash, seed, rec),
                    url: Arc::new(url),
                });
            }
            if !ids.is_empty() {
                shuffle(&mut ids, rng);
                tier_slots.push(ids);
            }
        }
        let mut layout = Vec::with_capacity(1 + tier_slots.len() + slots.len());
        layout.push(tier_slots.len() as u16);
        layout.extend(tier_slots.iter().map(|t| t.len() as u16));
        layout.extend(tier_slots.iter().flatten().copied());
        TrackerSet {
            layout: (slots.len() > 1).then(|| Box::new(layout)),
            slots: slots.into_boxed_slice(),
            session_wanted: false,
            private: false,
        }
    }

    /// Add one tracker as a tier of its own, after a torrent was registered (a duplicate add
    /// merged its trackers in). Returns the new slot, or `None` when the URL is already there (or
    /// is the http/https twin of one that is), or the set is full. Slot numbers of the others
    /// do not change. A running session starts the new tracker at the next input (`Timer`).
    pub(crate) fn add_tracker<R: Rng + ?Sized>(
        &mut self,
        cfg: Arc<Shared>,
        info_hash: crate::types::InfoHash20,
        url: TrackerUrl,
        rec: Option<TimingRecord>,
        rng: &mut R,
    ) -> Option<u16> {
        if self.slots.len() >= MAX_TRACKERS || self.slots.iter().any(|s| *s.url == url || is_scheme_twin(&s.url, &url)) {
            return None;
        }
        let id = self.slots.len() as u16;
        let seed = rng.next_u64();
        let mut slots = std::mem::take(&mut self.slots).into_vec();
        slots.push(Slot {
            machine: AnnounceMachine::with_shared_config(cfg, info_hash, seed, rec),
            url: Arc::new(url),
        });
        self.slots = slots.into_boxed_slice();
        let old: Vec<Vec<u16>> = self.tiers().iter().map(<[u16]>::to_vec).collect();
        let mut layout = Vec::with_capacity(2 + old.len() + self.slots.len());
        layout.push(old.len() as u16 + 1);
        layout.extend(old.iter().map(|t| t.len() as u16));
        layout.push(1);
        layout.extend(old.iter().flatten().copied());
        layout.push(id);
        self.layout = (self.slots.len() > 1).then(|| Box::new(layout));
        Some(id)
    }

    /// The shared machine configuration of this set (`None` for an empty set).
    /// Point every machine of the set at `cfg` (see `AnnounceMachine::replace_shared`).
    pub(crate) fn replace_shared(&mut self, cfg: &Arc<Shared>) {
        for s in self.slots.iter_mut() {
            s.machine.replace_shared(cfg.clone());
        }
    }

    pub(crate) fn shared_config(&self) -> Option<&Arc<Shared>> {
        self.slots.first().map(|s| s.machine.shared())
    }

    /// Replace each tracker URL by an equal one from a shared pool (the hub
    /// stores one `Arc` per distinct URL text). `f` receives the current `Arc`
    /// and returns the one to keep.
    pub(crate) fn share_urls(&mut self, mut f: impl FnMut(Arc<TrackerUrl>) -> Arc<TrackerUrl>) {
        for s in self.slots.iter_mut() {
            s.url = f(s.url.clone());
        }
    }

    /// Number of trackers.
    pub fn len(&self) -> usize {
        self.slots.len()
    }
    /// True when there are no trackers.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
    /// URL of a slot.
    pub fn url(&self, slot: u16) -> Option<&Arc<TrackerUrl>> {
        self.slots.get(usize::from(slot)).map(|s| &s.url)
    }
    /// All URLs by slot.
    pub fn urls(&self) -> Vec<Arc<TrackerUrl>> {
        self.slots.iter().map(|s| s.url.clone()).collect()
    }
    /// Machine of a slot.
    pub fn machine(&self, slot: u16) -> Option<&AnnounceMachine> {
        self.slots.get(usize::from(slot)).map(|s| &s.machine)
    }
    /// Current try-order as slot numbers (tier-major).
    pub fn order(&self) -> Vec<u16> {
        self.order_slice().to_vec()
    }
    /// Tiers as slot numbers (`tiers().iter()` yields one `&[u16]` per tier).
    pub fn tiers(&self) -> Tiers<'_> {
        Tiers {
            layout: self.layout(),
        }
    }

    fn layout(&self) -> &[u16] {
        match &self.layout {
            Some(l) => l,
            None if self.slots.is_empty() => &[0],
            None => &[1, 1, 0],
        }
    }

    fn order_slice(&self) -> &[u16] {
        let l = self.layout();
        let n = l.first().map_or(0, |n| usize::from(*n));
        l.get(1 + n..).unwrap_or(&[])
    }

    /// Earliest wake over all trackers.
    pub fn next_wake(&self) -> Option<Millis> {
        self.slots
            .iter()
            .filter_map(|s| s.machine.next_wake())
            .min()
    }

    /// True if any tracker has a request queued or in flight.
    pub fn has_outstanding(&self) -> bool {
        self.slots.iter().any(|s| s.machine.has_outstanding())
    }

    /// True while no tracker has an open session or outstanding request.
    pub fn is_quiescent(&self) -> bool {
        self.slots.iter().all(|s| {
            // A tripped machine never sends again; it must not hold a torrent
            // (and its removal, and a re-add after `reset_host`) hostage.
            s.machine.tripped().is_some()
                || (!s.machine.session_open() && !s.machine.has_outstanding())
        })
    }

    fn feed(&mut self, slot: u16, input: Input, now: Millis, out: &mut Vec<SetIntent>) {
        let Some(s) = self.slots.get_mut(usize::from(slot)) else {
            return;
        };
        let mut scratch = SCRATCH.with(|c| std::mem::take(&mut *c.borrow_mut()));
        scratch.clear();
        s.machine.handle_into(input, now, &mut scratch);
        out.extend(scratch.drain(..).map(|intent| SetIntent { slot, intent }));
        SCRATCH.with(|c| *c.borrow_mut() = scratch);
    }

    /// Process an input; intents are appended to `out`.
    pub fn handle(&mut self, input: SetInput, now: Millis, out: &mut Vec<SetIntent>) {
        let n = self.slots.len() as u16;
        match input {
            SetInput::Torrent(ev) => match ev {
                TorrentEvent::Start | TorrentEvent::Resume => {
                    self.session_wanted = true;
                    for i in 0..n {
                        self.feed(i, Input::ClearHalt, now, out);
                    }
                }
                TorrentEvent::Stop | TorrentEvent::Pause => {
                    self.session_wanted = false;
                    for i in 0..n {
                        if self.slots[usize::from(i)].machine.session_open() {
                            self.feed(i, Input::Torrent(ev), now, out);
                        }
                    }
                }
                TorrentEvent::Complete => {
                    for i in 0..n {
                        self.feed(i, Input::Torrent(TorrentEvent::Complete), now, out);
                    }
                }
            },
            SetInput::Quit => {
                self.session_wanted = false;
                for i in 0..n {
                    if self.slots[usize::from(i)].machine.session_open() && !self.slots[usize::from(i)].machine.stopping() {
                        self.feed(i, Input::Suspend, now, out);
                    }
                }
            }
            SetInput::Stats(st) => {
                for i in 0..n {
                    self.feed(i, Input::Stats(st), now, out);
                }
            }
            SetInput::Timer => {
                for i in 0..n {
                    self.feed(i, Input::Timer, now, out);
                }
            }
            SetInput::Manual => {
                for i in 0..n {
                    self.feed(i, Input::ManualReannounce, now, out);
                }
            }
            SetInput::ManualForce => {
                for i in 0..n {
                    self.feed(i, Input::ManualForce, now, out);
                }
            }
            SetInput::NetworkChanged => {
                for i in 0..n {
                    self.feed(i, Input::NetworkChanged, now, out);
                }
            }
            SetInput::Wake => {
                for i in 0..n {
                    self.feed(i, Input::Wake, now, out);
                }
            }
            SetInput::Role(r) => {
                for i in 0..n {
                    self.feed(i, Input::SetRole(r), now, out);
                }
            }
            SetInput::Hold => {
                for i in 0..n {
                    self.feed(i, Input::Hold, now, out);
                }
            }
            SetInput::Release { spread } => {
                for i in 0..n {
                    self.feed(i, Input::Release { spread }, now, out);
                }
            }
            SetInput::Forgive { since } => {
                for i in 0..n {
                    self.feed(i, Input::Forgive { since }, now, out);
                }
            }
            SetInput::Slot { slot, input } => {
                let success_slot =
                    matches!(&input, Input::Response { result: Ok(_), .. }).then_some(slot);
                self.feed(slot, input, now, out);
                if let Some(s) = success_slot {
                    self.promote(s);
                }
            }
        }
        self.reconcile(now, out);
    }

    /// Move a working tracker to the front of its tier (BEP 12).
    fn promote(&mut self, slot: u16) {
        if self.slots[usize::from(slot)].machine.health() != Health::Healthy {
            return;
        }
        let Some(layout) = self.layout.as_deref_mut() else {
            return; // a single tracker: nothing to reorder
        };
        let n = layout.first().map_or(0, |n| usize::from(*n));
        let mut start = 1 + n;
        for t in 0..n {
            let len = usize::from(layout[1 + t]);
            let tier = &mut layout[start..start + len];
            if let Some(pos) = tier.iter().position(|&s| s == slot) {
                tier[..=pos].rotate_right(1);
                return;
            }
            start += len;
        }
    }

    /// Start the trackers the walk allows, suspend the rest.
    fn reconcile(&mut self, now: Millis, out: &mut Vec<SetIntent>) {
        // Starting a tracker can change its health (e.g. it is still in
        // backoff), which moves the walk on; iterate to a fixed point.
        for _ in 0..=self.slots.len() {
            if !self.reconcile_once(now, out) {
                break;
            }
        }
    }

    fn reconcile_once(&mut self, now: Millis, out: &mut Vec<SetIntent>) -> bool {
        if !self.session_wanted {
            return false;
        }
        let mut changed = false;
        // Stack-only: this runs on every input of every torrent.
        let mut order = [0u16; MAX_TRACKERS];
        let mut lens = [0u16; MAX_TRACKERS];
        let (mut n, mut nt) = (0, 0);
        for tier in self.tiers() {
            lens[nt] = tier.len() as u16;
            nt += 1;
            for &s in tier {
                order[n] = s;
                n += 1;
            }
        }
        // Every tier is independent (Transmission's model): what happens in one tier never
        // starts, stops or reschedules a tracker of another tier. Inside a tier the walk is
        // BEP 12 failover: the first tracker that is not failing is the active one, the ones
        // behind it wait, the ones ahead of it (failing) keep retrying on their own backoff.
        let mut at = 0usize;
        for &len in &lens[..nt] {
            let tier = &order[at..at + usize::from(len)];
            at += usize::from(len);
            let mut allowed = 0u32;
            for &s in tier {
                allowed |= 1 << s;
                match self.slots[usize::from(s)].machine.health() {
                    Health::Idle | Health::Trying | Health::Healthy => break,
                    Health::Failing | Health::Halted | Health::Tripped => {}
                }
            }
            for &s in tier {
                let i = usize::from(s);
                let m = &self.slots[i].machine;
                if allowed & (1 << s) != 0 {
                    if !m.session_open()
                        && m.health() != Health::Tripped
                        && m.health() != Health::Halted
                    {
                        self.feed(s, Input::Torrent(TorrentEvent::Start), now, out);
                        changed = true;
                    }
                } else if m.session_open() && !m.stopping() {
                    self.feed(s, Input::Retire, now, out);
                    changed = true;
                }
            }
        }
        changed
    }

    /// Reports of a batch, for convenience.
    pub fn reports(intents: &[SetIntent]) -> impl Iterator<Item = (u16, &Report)> {
        intents.iter().filter_map(|i| match &i.intent {
            Intent::Report(r) => Some((i.slot, r)),
            _ => None,
        })
    }
}
