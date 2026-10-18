//! Rules and tracker profiles applied to torrents (SPEC 7, 7E).
//!
//! The data (`rules.toml`, `trackers.toml`) lives in [`PolicyData`] behind an `Arc` in the shared
//! state. A Cold torrent keeps only two small integers (interned profile id, interned forced-rule
//! id); the effective priority, goal and upload limit are *derived* from them whenever the torrent,
//! the rules or the profiles change, and stored back into the Cold record only as plain numbers.

use std::sync::Arc;

use trs_classify::Category;
use trs_state::rules::{OnGoal, PolicySource, ResolvedPolicy, TorrentAttrs};
use trs_state::trackers::Bucket;
use trs_state::{RuleSet, TrackerProfiles};

use crate::api::{AtGoal, Priority, ProfileBucket, SeedGoal};

/// The two policy documents, replaced as a whole on every reload.
#[derive(Clone, Debug, Default)]
pub struct PolicyData {
    /// `rules.toml`.
    pub rules: RuleSet,
    /// `trackers.toml`.
    pub profiles: TrackerProfiles,
    /// `transfers.private_min_seed_secs` of `config.toml` (SPEC §7): the hit-and-run floor of
    /// private torrents whose rule has none. `None` = off.
    pub private_min_seed: Option<trs_state::duration::Secs>,
    /// `transfers.idle_*` of `config.toml` (SPEC §7.5): the global inactive stop condition a rule
    /// may override. `None` = off.
    pub idle: Option<trs_state::rules::IdleStop>,
}

impl PolicyData {
    /// The policy documents plus the global private floor in seconds (`0` = off).
    pub fn new(rules: RuleSet, profiles: TrackerProfiles, private_min_seed_secs: u64) -> Self {
        PolicyData {
            rules,
            profiles,
            private_min_seed: (private_min_seed_secs > 0).then_some(trs_state::duration::Secs(private_min_seed_secs)),
            idle: None,
        }
    }

    /// With the global inactive condition of `transfers` (`idle_min_upload_bytes` 0 = off).
    pub fn with_idle(mut self, t: &trs_state::config::Transfers) -> Self {
        self.idle = (t.idle_min_upload_bytes > 0)
            .then_some(trs_state::rules::IdleStop { below: t.idle_min_upload_bytes, hours: t.idle_window_hours });
        self
    }
}

/// Most distinct profile and rule ids interned (ids come from our own files; the cap only guards
/// against a runaway editor).
const MAX_IDS: usize = 4096;

/// Interned profile / rule ids: Cold torrents store a `u32` index.
#[derive(Default)]
pub(crate) struct IdTable {
    names: Vec<Box<str>>,
}

impl IdTable {
    /// Index of `s`, adding it if new. `None` when the table is full.
    pub(crate) fn intern(&mut self, s: &str) -> Option<u32> {
        if let Some(i) = self.names.iter().position(|n| &**n == s) {
            return Some(i as u32);
        }
        if self.names.len() >= MAX_IDS {
            return None;
        }
        self.names.push(s.into());
        Some((self.names.len() - 1) as u32)
    }

    pub(crate) fn get(&self, i: u32) -> Option<&str> {
        self.names.get(i as usize).map(|s| &**s)
    }
}

/// Result of matching a torrent's announce URLs against the profiles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ProfileMatch {
    /// Matched profile id.
    pub profile: Option<String>,
    /// The rule linked from that profile.
    pub linked_rule: Option<String>,
}

/// Match announce URLs (any order) against the profiles. Passkeys are never stored or logged:
/// `match_announce` works on parsed URLs and returns only the profile.
pub(crate) fn match_profile<'a>(
    profiles: &TrackerProfiles,
    urls: impl IntoIterator<Item = &'a str>,
) -> ProfileMatch {
    match profiles.match_announce(urls) {
        Some(m) => ProfileMatch { profile: Some(m.profile.id.clone()), linked_rule: m.profile.rule.clone() },
        None => ProfileMatch::default(),
    }
}

/// Statistics bucket as an owned value.
pub(crate) fn bucket_of<'a>(
    profiles: &TrackerProfiles,
    urls: impl IntoIterator<Item = &'a str>,
    private: bool,
) -> ProfileBucket {
    match profiles.bucket_for(urls, private) {
        Bucket::Profile(id) => ProfileBucket::Profile(id.to_owned()),
        Bucket::Public => ProfileBucket::Public,
        Bucket::OtherPrivate => ProfileBucket::OtherPrivate,
    }
}

/// What the engine applies to a torrent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Resolved {
    /// Queue and bandwidth priority.
    pub priority: Priority,
    /// Seeding goal including the hit-and-run floor and the action at goal.
    pub goal: SeedGoal,
    /// Per-torrent upload limit, bytes/s.
    pub up_limit: Option<u64>,
    /// Finished-location override of the rule (a plain folder).
    pub finished_dir: Option<String>,
    /// `finished_dir` is the matched rule's own, not the Default rule's: it is stronger than a
    /// tracker's folders, which in turn beat the Default rule's (SPEC 7A.4).
    pub own_finished_dir: bool,
    /// The rule that produced this (matched, linked or forced).
    pub rule: Option<String>,
}

impl Default for Resolved {
    fn default() -> Self {
        Self { priority: Priority::Normal, goal: SeedGoal::FOREVER, up_limit: None, finished_dir: None, own_finished_dir: false, rule: None }
    }
}

fn engine_priority(p: trs_state::rules::Priority) -> Priority {
    match p {
        trs_state::rules::Priority::High => Priority::High,
        trs_state::rules::Priority::Normal => Priority::Normal,
        trs_state::rules::Priority::Low => Priority::Low,
    }
}

pub(crate) fn state_priority(p: Priority) -> trs_state::rules::Priority {
    match p {
        Priority::High => trs_state::rules::Priority::High,
        Priority::Normal => trs_state::rules::Priority::Normal,
        Priority::Low => trs_state::rules::Priority::Low,
    }
}

fn secs32(s: u64) -> u32 {
    u32::try_from(s).unwrap_or(u32::MAX)
}

/// Engine goal from a rule's goal.
pub(crate) fn engine_goal(p: &ResolvedPolicy) -> SeedGoal {
    use trs_state::rules::SeedingGoal as G;
    let mut g = SeedGoal::FOREVER;
    match p.goal {
        G::StopWhenFinished => g.stop_on_finish = true,
        G::Ratio { ratio } => g.ratio = Some(ratio as f32),
        G::Time { time } => g.seed_secs = Some(secs32(time.0)),
        G::RatioOrTime { ratio, time } => {
            g.ratio = Some(ratio as f32);
            g.seed_secs = Some(secs32(time.0));
            g.any = true;
        }
        G::RatioAndTime { ratio, time } => {
            g.ratio = Some(ratio as f32);
            g.seed_secs = Some(secs32(time.0));
            g.any = false;
        }
        G::Forever => {}
    }
    g.min_seed_secs = p.min_seed_time.map_or(0, |m| secs32(m.0));
    if let Some(i) = p.idle {
        g = g.with_idle(i.below, i.hours);
    }
    g.at_goal = match p.on_goal {
        OnGoal::StopKeep => AtGoal::Stop,
        OnGoal::RemoveKeepFiles => AtGoal::Remove,
        OnGoal::RemoveDeleteFiles => AtGoal::RemoveAndDelete,
    };
    g
}

/// The goal *kind* of an engine goal, for persisting a user override. The floor and the action at
/// goal always come from the rule.
pub(crate) fn state_goal(g: &SeedGoal) -> trs_state::rules::SeedingGoal {
    use trs_state::duration::Secs;
    use trs_state::rules::SeedingGoal as S;
    if g.stop_on_finish {
        return S::StopWhenFinished;
    }
    let time = g.seed_secs.map(|s| Secs(u64::from(s.max(1))));
    let ratio = g.ratio.map(|r| f64::from(r).clamp(0.001, 1000.0));
    match (ratio, time) {
        (Some(ratio), Some(time)) if g.any => S::RatioOrTime { ratio, time },
        (Some(ratio), Some(time)) => S::RatioAndTime { ratio, time },
        (Some(ratio), None) => S::Ratio { ratio },
        (None, Some(time)) => S::Time { time },
        (None, None) => S::Forever,
    }
}

/// Engine goal from a persisted goal kind (the floor and action are filled in by the rule later).
pub(crate) fn goal_from_state(g: &trs_state::rules::SeedingGoal) -> SeedGoal {
    let p = ResolvedPolicy {
        source: PolicySource::Default,
        goal: *g,
        min_seed_time: None,
        priority: trs_state::rules::Priority::Normal,
        on_goal: OnGoal::StopKeep,
        finished_dir: None,
        up_limit: None,
        idle: None,
    };
    engine_goal(&p)
}

/// Resolve the policy of a torrent. A forced rule id wins over matching; otherwise the profile's
/// linked rule and the ordered list decide (`RuleSet::resolve`).
pub(crate) fn resolve(
    rules: &RuleSet,
    m: &ProfileMatch,
    forced: Option<&str>,
    private: bool,
    category: Category,
    private_floor: Option<trs_state::duration::Secs>,
    idle: Option<trs_state::rules::IdleStop>,
) -> Resolved {
    let attrs = TorrentAttrs {
        tracker: m.profile.as_deref(),
        linked_rule: m.linked_rule.as_deref(),
        private,
        category,
    };
    let r = match forced.and_then(|id| rules.get(id)) {
        Some(rule) => {
            // The same field-by-field inheritance as `RuleSet::resolve`, for a rule picked by hand.
            let d = &rules.default;
            ResolvedPolicy {
                source: PolicySource::Rule(rule.id.clone()),
                goal: rule.goal.unwrap_or(d.goal),
                min_seed_time: trs_state::rules::effective_min_seed(rule.min_seed_time, d.min_seed_time, private, private_floor),
                priority: rule.priority.unwrap_or(d.priority),
                on_goal: rule.on_goal.unwrap_or(d.on_goal),
                finished_dir: rule.finished_dir.clone().or_else(|| d.finished_dir.clone()),
                up_limit: rule.up_limit.or(d.up_limit).filter(|v| *v != 0),
                idle: match rule.stop_when_idle {
                    Some(i) => i.stop(),
                    None => idle,
                },
            }
        }
        None => rules.resolve_full(&attrs, private_floor, idle),
    };
    let rule = match &r.source {
        PolicySource::Rule(id) | PolicySource::LinkedRule(id) => Some(id.clone()),
        PolicySource::Default => None,
    };
    let own_finished_dir = rule.as_deref().and_then(|id| rules.get(id)).is_some_and(|r| r.finished_dir.is_some());
    Resolved {
        priority: engine_priority(r.priority),
        goal: engine_goal(&r),
        up_limit: r.up_limit,
        finished_dir: r.finished_dir.clone(),
        own_finished_dir,
        rule,
    }
}

/// Shared, replaceable policy.
pub(crate) type SharedPolicy = std::sync::RwLock<Option<Arc<PolicyData>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use trs_state::duration::Secs;
    use trs_state::rules::{Rule, SeedingGoal};

    fn rules() -> RuleSet {
        let mut rs = RuleSet::default();
        rs.add_rule(
            Rule {
                id: "tz".into(),
                name: "Z".into(),
                // Matches only one tracker, so it applies to others only when forced.
                when: trs_state::rules::Match { tracker: vec!["z".into()], ..Default::default() },
                goal: Some(SeedingGoal::Ratio { ratio: 2.0 }),
                min_seed_time: Some(Secs::hours(72)),
                priority: Some(trs_state::rules::Priority::High),
                on_goal: Some(OnGoal::RemoveKeepFiles),
                up_limit: Some(1_000_000),
                ..Rule::default()
            },
            None,
        )
        .unwrap();
        rs
    }

    #[test]
    fn id_table_interns_and_caps() {
        let mut t = IdTable::default();
        assert_eq!(t.intern("a"), Some(0));
        assert_eq!(t.intern("b"), Some(1));
        assert_eq!(t.intern("a"), Some(0));
        assert_eq!(t.get(1), Some("b"));
        assert_eq!(t.get(9), None);
        for i in 0..MAX_IDS + 10 {
            t.intern(&format!("x{i}"));
        }
        assert_eq!(t.intern("never"), None);
    }

    #[test]
    fn forced_rule_is_resolved_with_default_inheritance() {
        let rs = rules();
        let r = resolve(&rs, &ProfileMatch::default(), Some("tz"), false, Category::Movie, None, None);
        assert_eq!(r.priority, Priority::High);
        assert_eq!(r.goal.ratio, Some(2.0));
        assert_eq!(r.goal.min_seed_secs, 72 * 3600);
        assert_eq!(r.goal.at_goal, AtGoal::Remove);
        assert_eq!(r.up_limit, Some(1_000_000));
        assert_eq!(r.rule.as_deref(), Some("tz"));
        // An unknown forced id falls back to normal matching (here: the default rule).
        let r = resolve(&rs, &ProfileMatch::default(), Some("nope"), false, Category::Movie, None, None);
        assert_eq!(r.rule, None);
    }

    #[test]
    fn private_floor_reaches_the_engine_goal_unless_the_rule_has_its_own() {
        let rs = rules();
        let ten = Some(Secs::days(10));
        let by = |forced, private| resolve(&rs, &ProfileMatch::default(), forced, private, Category::Movie, ten, None).goal.min_seed_secs;
        assert_eq!(by(None, true), 10 * 86_400, "private, Default rule: the global floor");
        assert_eq!(by(None, false), 0, "public: none");
        assert_eq!(by(Some("tz"), true), 72 * 3600, "the rule's own floor wins");
        let off = resolve(&rs, &ProfileMatch::default(), None, true, Category::Movie, None, None);
        assert_eq!(off.goal.min_seed_secs, 0);
    }

    #[test]
    fn goal_kinds_round_trip() {
        let mut g = SeedGoal::FOREVER;
        assert_eq!(goal_from_state(&state_goal(&g)), g);
        g.ratio = Some(1.5);
        assert_eq!(goal_from_state(&state_goal(&g)), g);
        g.seed_secs = Some(3600);
        assert_eq!(goal_from_state(&state_goal(&g)), g);
        g.any = false;
        assert_eq!(goal_from_state(&state_goal(&g)), g);
        let s = SeedGoal::stop_when_finished();
        assert_eq!(goal_from_state(&state_goal(&s)), s);
    }
}
