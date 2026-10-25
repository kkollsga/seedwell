//! Turns [`RawEvent`]s into scan batches. Pure: no I/O, no clock (the caller passes `now` in [`Millis`]).
//!
//! Rules (all in [`Config`], defaults in brackets):
//! * A path is ready after `quiet` [2 s] without a new event for it, so a file being written is scanned once,
//!   when the writer is done.
//! * A path that keeps changing is still emitted `max_delay` [60 s] after its first pending event.
//! * Removes and renames are held: they go out together, once `hold` [10 s] has passed since the last remove
//!   or rename anywhere (and they are quiet), so both halves of a rename land in the same batch and the
//!   scanner can see a move instead of a delete plus an add. `max_delay` caps the hold too.
//! * More than `dir_limit` [128] pending paths directly in one directory collapse to a rescan of that
//!   directory; more than `total_limit` [512] pending entries collapse to a rescan of everything.
//! * A rescan wins over the paths (and smaller rescans) it covers; they are absorbed into it.
//!
//! Resume tokens are held too: [`Aggregator::take_resume`] hands out a token only once every event that came
//! before it has gone out in a batch, so a token persisted after scanning those batches never skips a change.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use crate::{Millis, RawEvent, ResumeToken};

/// Aggregator timings and limits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Config {
    /// Quiet time before a path is ready.
    pub quiet: Millis,
    /// The longest a pending path waits.
    pub max_delay: Millis,
    /// How long removes and renames are held after the last one.
    pub hold: Millis,
    /// More pending paths than this directly in one directory become a rescan of that directory.
    pub dir_limit: usize,
    /// More pending entries than this become a rescan of everything.
    pub total_limit: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self { quiet: 2_000, max_delay: 60_000, hold: 10_000, dir_limit: 128, total_limit: 512 }
    }
}

/// Work for the scanner.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Batch {
    /// Stat these paths (relative to the root, sorted, no duplicates). A path that is a directory is
    /// stat-ed, not walked; directory-level work comes as [`Batch::Dir`].
    Paths(Vec<PathBuf>),
    /// Rescan this subtree (relative to the root).
    Dir(PathBuf),
    /// Rescan the whole root.
    All,
}

/// One pending item: a path, a directory rescan or the full rescan.
#[derive(Clone, Copy, Debug)]
struct Entry {
    /// First event since it became pending.
    first: Millis,
    /// Latest event.
    last: Millis,
    /// Saw a remove or rename: waits for the hold group.
    held: bool,
    /// Sequence number of the first event (for resume tokens).
    seq: u64,
}

impl Entry {
    fn absorb(&mut self, o: Entry) {
        self.first = self.first.min(o.first);
        self.last = self.last.max(o.last);
        self.held |= o.held;
        self.seq = self.seq.min(o.seq);
    }
}

fn merge(a: Option<Entry>, b: Entry) -> Entry {
    match a {
        Some(mut a) => {
            a.absorb(b);
            a
        }
        None => b,
    }
}

/// Tokens waiting for the events before them; older ones beyond this are dropped (a later token covers them).
const MAX_TOKENS: usize = 64;

/// The aggregator. See the module docs for the rules.
#[derive(Debug, Default)]
pub struct Aggregator {
    cfg: Config,
    paths: BTreeMap<PathBuf, Entry>,
    dirs: BTreeMap<PathBuf, Entry>,
    all: Option<Entry>,
    /// Pending paths per parent directory.
    children: HashMap<PathBuf, usize>,
    /// Time of the latest remove or rename while any held entry is pending.
    last_hold: Option<Millis>,
    seq: u64,
    tokens: VecDeque<(u64, ResumeToken)>,
    released: Option<ResumeToken>,
}

impl Aggregator {
    /// An aggregator with the default [`Config`].
    pub fn new() -> Self {
        Self::default()
    }

    /// An aggregator with these timings and limits.
    pub fn with_config(cfg: Config) -> Self {
        Self { cfg, ..Self::default() }
    }

    /// The timings and limits in use.
    pub fn config(&self) -> Config {
        self.cfg
    }

    /// Pending entries (paths, directory rescans and the full rescan).
    pub fn pending(&self) -> usize {
        self.paths.len() + self.dirs.len() + usize::from(self.all.is_some())
    }

    /// Nothing pending.
    pub fn is_empty(&self) -> bool {
        self.pending() == 0
    }

    /// Takes one event. [`RawEvent::RootChanged`] counts as a full rescan; [`RawEvent::HistoryDone`] is
    /// ignored (it is for the caller); [`RawEvent::Resume`] is held for [`Aggregator::take_resume`].
    pub fn push(&mut self, ev: RawEvent, now: Millis) {
        self.seq += 1;
        let mut entry = Entry { first: now, last: now, held: false, seq: self.seq };
        match ev {
            RawEvent::Path { rel, kind, .. } => {
                if kind.is_removal() {
                    entry.held = true;
                    self.last_hold = Some(self.last_hold.map_or(now, |t| t.max(now)));
                }
                self.add_path(rel, entry);
            }
            RawEvent::Rescan { rel: Some(rel), .. } if !rel.as_os_str().is_empty() => self.add_dir(rel, Some(entry)),
            RawEvent::Rescan { .. } | RawEvent::RootChanged => self.make_all(Some(entry)),
            RawEvent::HistoryDone => {}
            RawEvent::Resume(t) => {
                self.tokens.push_back((self.seq, t));
                if self.tokens.len() > MAX_TOKENS {
                    self.tokens.pop_front();
                }
            }
        }
    }

    /// The batches that are ready at `now`. At most one [`Batch::Paths`]; [`Batch::All`] comes alone.
    pub fn poll(&mut self, now: Millis) -> Vec<Batch> {
        let mut out = Vec::new();
        let release = self.hold_released(now);
        let ready = |e: &Entry| if e.held { release } else { self.ready(e, now) };
        if let Some(e) = self.all {
            if ready(&e) {
                self.all = None;
                self.last_hold = None;
                out.push(Batch::All);
            }
            return out;
        }
        let dirs: Vec<PathBuf> = self.dirs.iter().filter(|(_, e)| ready(e)).map(|(p, _)| p.clone()).collect();
        let paths: Vec<PathBuf> = self.paths.iter().filter(|(_, e)| ready(e)).map(|(p, _)| p.clone()).collect();
        for d in dirs {
            self.dirs.remove(&d);
            out.push(Batch::Dir(d));
        }
        for p in &paths {
            self.remove_path(p);
        }
        if !paths.is_empty() {
            out.push(Batch::Paths(paths));
        }
        if release {
            self.last_hold = None;
        }
        out
    }

    /// The earliest time at which [`Aggregator::poll`] will return something, if anything is pending.
    pub fn next_deadline(&self) -> Option<Millis> {
        let cfg = self.cfg;
        let mut best: Option<Millis> = None;
        let mut held_quiet: Option<Millis> = None;
        let mut held_aged: Option<Millis> = None;
        for e in self.entries() {
            if e.held {
                held_quiet = Some(held_quiet.unwrap_or(0).max(e.last + cfg.quiet));
                held_aged = Some(held_aged.map_or(e.first + cfg.max_delay, |t| t.min(e.first + cfg.max_delay)));
            } else {
                let t = (e.last + cfg.quiet).min(e.first + cfg.max_delay);
                best = Some(best.map_or(t, |b| b.min(t)));
            }
        }
        if let (Some(q), Some(a)) = (held_quiet, held_aged) {
            let hold_end = self.last_hold.map_or(0, |h| h + cfg.hold);
            let t = hold_end.max(q).min(a);
            best = Some(best.map_or(t, |b| b.min(t)));
        }
        best
    }

    /// The newest resume token all of whose earlier events have gone out in batches, if it is newer than the
    /// last one returned. Persist it after the batches from [`Aggregator::poll`] have been scanned.
    pub fn take_resume(&mut self) -> Option<ResumeToken> {
        let oldest_pending = self.entries().map(|e| e.seq).min().unwrap_or(u64::MAX);
        let mut newest = None;
        while let Some(&(seq, t)) = self.tokens.front() {
            if seq >= oldest_pending {
                break;
            }
            newest = Some(t);
            self.tokens.pop_front();
        }
        let t = newest?;
        if self.released == Some(t) {
            return None;
        }
        self.released = Some(t);
        Some(t)
    }

    fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.all.iter().chain(self.dirs.values()).chain(self.paths.values())
    }

    fn ready(&self, e: &Entry, now: Millis) -> bool {
        now >= e.last + self.cfg.quiet || now >= e.first + self.cfg.max_delay
    }

    /// The held group goes out together: when `hold` has passed since the last remove or rename and every
    /// held entry is quiet, or as soon as any held entry reaches `max_delay`.
    fn hold_released(&self, now: Millis) -> bool {
        let mut any = false;
        let mut all_quiet = true;
        for e in self.entries().filter(|e| e.held) {
            any = true;
            if now >= e.first + self.cfg.max_delay {
                return true;
            }
            all_quiet &= now >= e.last + self.cfg.quiet;
        }
        let hold_over = self.last_hold.is_none_or(|h| now >= h + self.cfg.hold);
        any && all_quiet && hold_over
    }

    /// The pending directory rescan that covers `p`, if any.
    fn covering_dir(&self, p: &Path) -> Option<PathBuf> {
        p.ancestors().find(|a| self.dirs.contains_key(*a)).map(Path::to_path_buf)
    }

    fn add_path(&mut self, rel: PathBuf, entry: Entry) {
        if let Some(all) = &mut self.all {
            all.absorb(entry);
            return;
        }
        if let Some(d) = self.covering_dir(&rel) {
            if let Some(e) = self.dirs.get_mut(&d) {
                e.absorb(entry);
            }
            return;
        }
        if let Some(e) = self.paths.get_mut(&rel) {
            e.absorb(entry);
            return;
        }
        let parent = rel.parent().map(Path::to_path_buf);
        self.paths.insert(rel, entry);
        if let Some(parent) = parent {
            let n = self.children.entry(parent.clone()).or_insert(0);
            *n += 1;
            // The root is never collapsed to a directory rescan: that would be a full rescan, which is what
            // `total_limit` decides.
            if *n > self.cfg.dir_limit && !parent.as_os_str().is_empty() {
                self.add_dir(parent, None);
                return;
            }
        }
        self.check_total();
    }

    /// A rescan of `dir` (non-empty): absorbs everything under it, or is absorbed by a rescan above it.
    fn add_dir(&mut self, dir: PathBuf, entry: Option<Entry>) {
        if let Some(all) = &mut self.all {
            if let Some(e) = entry {
                all.absorb(e);
            }
            return;
        }
        if let Some(d) = self.covering_dir(&dir) {
            if let (Some(e), Some(new)) = (self.dirs.get_mut(&d), entry) {
                e.absorb(new);
            }
            return;
        }
        let mut merged = entry;
        let under: Vec<PathBuf> = self.paths.range(dir.clone()..).map(|(p, _)| p).take_while(|p| p.starts_with(&dir)).cloned().collect();
        for p in under {
            if let Some(e) = self.remove_path(&p) {
                merged = Some(merge(merged, e));
            }
        }
        let sub: Vec<PathBuf> = self.dirs.range(dir.clone()..).map(|(p, _)| p).take_while(|p| p.starts_with(&dir)).cloned().collect();
        for d in sub {
            if let Some(e) = self.dirs.remove(&d) {
                merged = Some(merge(merged, e));
            }
        }
        if let Some(m) = merged {
            self.dirs.insert(dir, m);
        }
        self.check_total();
    }

    fn make_all(&mut self, entry: Option<Entry>) {
        let mut merged = self.all.take();
        if let Some(e) = entry {
            merged = Some(merge(merged, e));
        }
        for e in std::mem::take(&mut self.dirs).into_values().chain(std::mem::take(&mut self.paths).into_values()) {
            merged = Some(merge(merged, e));
        }
        self.children.clear();
        self.all = merged;
    }

    fn check_total(&mut self) {
        if self.paths.len() + self.dirs.len() > self.cfg.total_limit {
            self.make_all(None);
        }
    }

    fn remove_path(&mut self, p: &Path) -> Option<Entry> {
        let e = self.paths.remove(p)?;
        if let Some(parent) = p.parent()
            && let Some(n) = self.children.get_mut(parent)
        {
            *n -= 1;
            if *n == 0 {
                self.children.remove(parent);
            }
        }
        Some(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Hint, RescanReason};
    use proptest::prelude::*;
    use std::collections::HashMap;

    fn path(rel: &str, kind: Hint) -> RawEvent {
        RawEvent::Path { rel: PathBuf::from(rel), kind, is_dir: Some(false) }
    }

    fn rescan(rel: Option<&str>) -> RawEvent {
        RawEvent::Rescan { rel: rel.map(PathBuf::from), reason: RescanReason::Dropped }
    }

    fn paths(v: &[&str]) -> Batch {
        Batch::Paths(v.iter().map(PathBuf::from).collect())
    }

    fn token(id: u64) -> ResumeToken {
        ResumeToken { event_id: id, volume_uuid: [1; 16] }
    }

    #[test]
    fn a_path_is_ready_after_two_quiet_seconds() {
        let mut a = Aggregator::new();
        a.push(path("f", Hint::Modified), 0);
        a.push(path("f", Hint::Modified), 1_500);
        assert_eq!(a.next_deadline(), Some(3_500));
        assert!(a.poll(3_499).is_empty());
        assert_eq!(a.poll(3_500), vec![paths(&["f"])]);
        assert!(a.is_empty());
        assert_eq!(a.next_deadline(), None);
    }

    #[test]
    fn a_path_that_keeps_changing_goes_out_after_sixty_seconds() {
        let mut a = Aggregator::new();
        let mut out = Vec::new();
        for t in (0..=70_000).step_by(1_000) {
            out.extend(a.poll(t).into_iter().map(|b| (t, b)));
            a.push(path("busy", Hint::Modified), t);
        }
        assert_eq!(out.first(), Some(&(60_000, paths(&["busy"]))));
    }

    #[test]
    fn removes_are_held_ten_seconds() {
        let mut a = Aggregator::new();
        a.push(path("gone", Hint::Removed), 0);
        a.push(path("edited", Hint::Modified), 0);
        assert_eq!(a.poll(2_000), vec![paths(&["edited"])]);
        assert_eq!(a.next_deadline(), Some(10_000));
        assert!(a.poll(9_999).is_empty());
        assert_eq!(a.poll(10_000), vec![paths(&["gone"])]);
    }

    #[test]
    fn both_halves_of_a_rename_go_out_together() {
        let mut a = Aggregator::new();
        a.push(path("old", Hint::Renamed), 0);
        a.push(path("x/new", Hint::Renamed), 4_000);
        assert!(a.poll(10_000).is_empty(), "the hold restarts with the second half");
        assert_eq!(a.poll(14_000), vec![paths(&["old", "x/new"])]);
    }

    #[test]
    fn the_hold_cannot_starve_past_max_delay() {
        let mut a = Aggregator::new();
        let mut first = None;
        for t in (0..=80_000).step_by(5_000) {
            if first.is_none() && !a.poll(t).is_empty() {
                first = Some(t);
            }
            a.push(path(&format!("r{t}"), Hint::Renamed), t);
        }
        assert_eq!(first, Some(60_000));
    }

    #[test]
    fn a_crowded_directory_collapses_to_a_dir_rescan() {
        let mut a = Aggregator::new();
        for i in 0..129 {
            a.push(path(&format!("d/e/f{i}"), Hint::Created), i);
        }
        a.push(path("d/e/sub/x", Hint::Created), 200);
        a.push(path("other", Hint::Created), 200);
        assert_eq!(a.pending(), 2);
        assert_eq!(a.poll(2_200), vec![Batch::Dir(PathBuf::from("d/e")), paths(&["other"])]);
    }

    #[test]
    fn exactly_the_limit_does_not_collapse() {
        let mut a = Aggregator::new();
        for i in 0..128 {
            a.push(path(&format!("d/f{i}"), Hint::Created), 0);
        }
        assert_eq!(a.pending(), 128);
        let b = a.poll(2_000);
        assert!(matches!(&b[..], [Batch::Paths(p)] if p.len() == 128));
    }

    #[test]
    fn the_root_directory_is_not_collapsed_but_the_total_is() {
        let mut a = Aggregator::new();
        for i in 0..512 {
            a.push(path(&format!("f{i}"), Hint::Created), 0);
        }
        assert_eq!(a.pending(), 512);
        a.push(path("f512", Hint::Created), 0);
        assert_eq!(a.pending(), 1);
        assert_eq!(a.poll(2_000), vec![Batch::All]);
    }

    #[test]
    fn a_rescan_wins_over_the_paths_it_covers() {
        let mut a = Aggregator::new();
        a.push(path("d/a", Hint::Modified), 0);
        a.push(path("d/sub/b", Hint::Modified), 0);
        a.push(path("dx/c", Hint::Modified), 0);
        a.push(rescan(Some("d/sub")), 0);
        a.push(rescan(Some("d")), 0);
        a.push(path("d/later", Hint::Modified), 1_000);
        assert_eq!(a.pending(), 2);
        assert_eq!(a.poll(2_000), vec![paths(&["dx/c"])]);
        assert_eq!(a.poll(3_000), vec![Batch::Dir(PathBuf::from("d"))]);
    }

    #[test]
    fn a_full_rescan_absorbs_everything_and_comes_alone() {
        let mut a = Aggregator::new();
        a.push(path("a", Hint::Removed), 0);
        a.push(rescan(Some("d")), 0);
        a.push(rescan(None), 500);
        a.push(path("b", Hint::Modified), 600);
        assert_eq!(a.pending(), 1);
        // The absorbed remove keeps the hold.
        assert!(a.poll(2_600).is_empty());
        assert_eq!(a.poll(10_000), vec![Batch::All]);
        assert!(a.is_empty());
    }

    #[test]
    fn root_changed_is_a_full_rescan_and_history_done_is_ignored() {
        let mut a = Aggregator::new();
        a.push(RawEvent::HistoryDone, 0);
        assert!(a.is_empty());
        a.push(RawEvent::RootChanged, 0);
        a.push(rescan(Some("")), 0);
        assert_eq!(a.poll(2_000), vec![Batch::All]);
    }

    #[test]
    fn resume_tokens_wait_for_earlier_events() {
        let mut a = Aggregator::new();
        a.push(RawEvent::Resume(token(1)), 0);
        assert_eq!(a.take_resume(), Some(token(1)));
        assert_eq!(a.take_resume(), None, "only once");
        a.push(path("f", Hint::Modified), 0);
        a.push(RawEvent::Resume(token(2)), 0);
        a.push(path("g", Hint::Modified), 1_000);
        a.push(RawEvent::Resume(token(3)), 1_000);
        assert_eq!(a.take_resume(), None);
        a.poll(2_000); // f goes out
        assert_eq!(a.take_resume(), Some(token(2)));
        a.poll(3_000); // g goes out
        assert_eq!(a.take_resume(), Some(token(3)));
    }

    #[test]
    fn an_event_absorbed_into_an_older_entry_keeps_the_older_sequence() {
        let mut a = Aggregator::new();
        a.push(path("f", Hint::Modified), 0);
        a.push(RawEvent::Resume(token(1)), 0);
        a.push(path("f", Hint::Modified), 1_000);
        assert_eq!(a.take_resume(), None);
        a.poll(3_000);
        assert_eq!(a.take_resume(), Some(token(1)));
    }

    // ---- properties ----

    #[derive(Clone, Debug)]
    enum Op {
        Path(String, Hint),
        Rescan(Option<String>),
        Resume,
    }

    fn rel_strategy() -> impl Strategy<Value = String> {
        let dirs = prop::sample::select(vec!["", "a", "a/b", "c"]);
        (dirs, 0u32..180).prop_map(|(d, i)| if d.is_empty() { format!("f{i}") } else { format!("{d}/f{i}") })
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let hint = prop::sample::select(vec![Hint::Created, Hint::Modified, Hint::Removed, Hint::Renamed, Hint::Meta]);
        prop_oneof![
            40 => (rel_strategy(), hint).prop_map(|(p, h)| Op::Path(p, h)),
            1 => prop::option::of(prop::sample::select(vec!["a", "a/b", "c", ""]).prop_map(String::from)).prop_map(Op::Rescan),
            2 => Just(Op::Resume),
        ]
    }

    fn dt_strategy() -> impl Strategy<Value = Millis> {
        prop_oneof![8 => 0u64..300, 2 => 0u64..3_000, 1 => 0u64..20_000]
    }

    /// What the test knows about a path that is not yet covered by a batch.
    #[derive(Clone, Copy)]
    struct Outstanding {
        first: Millis,
        last: Millis,
        held: bool,
        /// Sim sequence number of the first push since the path was last covered.
        seq: u64,
    }

    fn covered(b: &Batch, p: &Path) -> bool {
        match b {
            Batch::All => true,
            Batch::Dir(d) => p.starts_with(d),
            Batch::Paths(v) => v.iter().any(|x| x == p),
        }
    }

    struct Sim {
        a: Aggregator,
        out: HashMap<PathBuf, Outstanding>,
        /// Token pushes not yet released, with their sim sequence numbers.
        tokens: Vec<(ResumeToken, u64)>,
        next_token: u64,
        seq: u64,
    }

    impl Sim {
        fn deliver(&mut self, now: Millis) -> Result<(), TestCaseError> {
            let cfg = self.a.config();
            let batches = self.a.poll(now);
            let n_paths = batches.iter().filter(|b| matches!(b, Batch::Paths(_))).count();
            prop_assert!(n_paths <= 1);
            if batches.contains(&Batch::All) {
                prop_assert_eq!(batches.len(), 1);
            }
            for b in &batches {
                match b {
                    Batch::Paths(v) => {
                        let mut s = v.clone();
                        s.dedup();
                        prop_assert_eq!(s.len(), v.len(), "duplicates");
                        prop_assert!(v.windows(2).all(|w| w[0] < w[1]), "sorted");
                        for p in v {
                            prop_assert!(!batches.iter().any(|o| matches!(o, Batch::Dir(d) if p.starts_with(d))));
                            if let Some(o) = self.out.get(p)
                                && !o.held
                            {
                                prop_assert!(now >= o.last + cfg.quiet || now >= o.first + cfg.max_delay, "too early: {p:?} at {now}");
                            }
                        }
                    }
                    Batch::Dir(d) => {
                        prop_assert!(!batches.iter().any(|o| matches!(o, Batch::Dir(e) if e != d && d.starts_with(e))), "nested dirs");
                    }
                    Batch::All => {}
                }
            }
            let mut done = Vec::new();
            for (p, o) in &self.out {
                if batches.iter().any(|b| covered(b, p)) {
                    prop_assert!(now <= o.first + cfg.max_delay, "late: {p:?} first {} emitted {now}", o.first);
                    done.push(p.clone());
                }
            }
            for p in done {
                self.out.remove(&p);
            }
            if let Some(t) = self.a.take_resume() {
                let i = self.tokens.iter().position(|(x, _)| *x == t);
                prop_assert!(i.is_some(), "released a token that was never pushed");
                let (_, tseq) = self.tokens[i.unwrap_or(0)];
                self.tokens.drain(..=i.unwrap_or(0));
                for (p, o) in &self.out {
                    prop_assert!(o.seq > tseq, "token released while {p:?} (pushed before it) is pending");
                }
            }
            Ok(())
        }

        fn advance(&mut self, now: &mut Millis, to: Millis) -> Result<(), TestCaseError> {
            while let Some(d) = self.a.next_deadline() {
                if d > to {
                    break;
                }
                let d = d.max(*now);
                let before = self.a.pending();
                *now = d;
                self.deliver(d)?;
                prop_assert!(self.a.pending() < before, "a deadline with nothing to emit at {d}");
            }
            *now = to;
            Ok(())
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

        #[test]
        fn every_event_is_covered_in_time_and_in_order(ops in prop::collection::vec((dt_strategy(), op_strategy()), 1..1200)) {
            let cfg = Config::default();
            let mut sim = Sim { a: Aggregator::new(), out: HashMap::new(), tokens: Vec::new(), next_token: 0, seq: 0 };
            let mut now: Millis = 0;
            for (dt, op) in ops {
                let to = now + dt;
                sim.advance(&mut now, to)?;
                sim.seq += 1;
                let seq = sim.seq;
                match op {
                    Op::Path(p, h) => {
                        let p = PathBuf::from(p);
                        let o = sim.out.entry(p.clone()).or_insert(Outstanding { first: now, last: now, held: false, seq });
                        o.last = now;
                        o.held |= h.is_removal();
                        sim.a.push(RawEvent::Path { rel: p, kind: h, is_dir: None }, now);
                    }
                    Op::Rescan(d) => sim.a.push(RawEvent::Rescan { rel: d.map(PathBuf::from), reason: RescanReason::Dropped }, now),
                    Op::Resume => {
                        sim.next_token += 1;
                        let t = token(sim.next_token);
                        sim.tokens.push((t, seq));
                        sim.a.push(RawEvent::Resume(t), now);
                    }
                }
                prop_assert!(sim.a.pending() <= cfg.total_limit + 1);
            }
            sim.advance(&mut now, Millis::MAX / 4)?;
            prop_assert!(sim.a.is_empty());
            prop_assert!(sim.out.is_empty(), "never covered: {:?}", sim.out.keys().collect::<Vec<_>>());
        }

        #[test]
        fn rename_halves_within_the_hold_share_a_batch(gap in 0u64..9_999, noise in prop::collection::vec((0u64..15_000, rel_strategy()), 0..40)) {
            let mut a = Aggregator::new();
            let mut evs: Vec<(Millis, RawEvent)> = noise.into_iter().map(|(t, p)| (t, path(&format!("n/{p}"), Hint::Modified))).collect();
            evs.push((1_000, path("from", Hint::Renamed)));
            evs.push((1_000 + gap, path("to/here", Hint::Renamed)));
            evs.sort_by_key(|(t, _)| *t);
            let mut batches = Vec::new();
            for (t, e) in evs {
                batches.extend(a.poll(t));
                a.push(e, t);
            }
            while let Some(d) = a.next_deadline() {
                batches.extend(a.poll(d));
            }
            let from = Path::new("from");
            let to = Path::new("to/here");
            let together = batches.iter().any(|b| covered(b, from) && covered(b, to));
            prop_assert!(together, "{batches:?}");
        }
    }
}
