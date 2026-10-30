//! `rules.toml`: seeding rules (SPEC §7).
//!
//! # Evaluation (deterministic)
//!
//! [`RuleSet::resolve`] is a pure function of the rule set and the torrent's attributes:
//!
//! 1. If the torrent's tracker profile links a rule (`Profile::rule`) and that rule exists, is
//!    enabled and its own `private` / `category` conditions hold, it is the matching rule. (Its
//!    `tracker` condition is waived: the link is the tracker condition.)
//! 2. Otherwise the enabled rules are tried **in file order; the first match wins**.
//! 3. If nothing matches, the **Default rule** applies. It is a separate `[default]` table, so it
//!    is always last and cannot be deleted.
//!
//! A rule only needs to state what it changes: every action it leaves out is taken from the
//! Default rule, field by field. `ResolvedPolicy` is therefore always complete.

use serde::{Deserialize, Serialize};
use trs_classify::Category;

use crate::config::{check_path, fixed_prefix};
use crate::doc::{Document, Parsed, parse_toml, render_table};
use crate::duration::{MAX_SECS, Secs};
use crate::error::{DocError, FieldError, Warning};
use crate::ids::valid_id;
use crate::serde_util::{category_vec, string_or_vec};

/// Highest `version` this build reads.
pub const RULES_VERSION: u32 = 1;
/// Tracker id in a rule's `tracker` list that matches torrents without a profile.
pub const PUBLIC_TRACKER: &str = "public";
const MAX_RULES: usize = 1000;
const HEADER: &str = "# Seeding rules. The first matching rule wins; [default] applies when none matches.\n\
# A rule only needs the actions it changes; the rest come from [default].\n\n";

/// Default window of the "inactive" stop condition, hours of seeding.
pub const DEFAULT_IDLE_HOURS: u32 = 150;
/// Longest window accepted (one year).
pub const MAX_IDLE_HOURS: u32 = 8760;
/// Largest threshold accepted (1 PB).
pub const MAX_IDLE_BYTES: u64 = 1_000_000_000_000_000;

/// "Stop seeding when less than `below` bytes were uploaded in the last `hours` hours of seeding"
/// (SPEC §7.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IdleStop {
    /// Threshold, bytes (decimal: 1 GB = 10^9).
    pub below: u64,
    /// Window, hours of seeding.
    pub hours: u32,
}

/// A rule's say on the inactive condition: absent = inherit, `false` = off, a table = on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleSetting {
    /// Switched off for this rule.
    Off,
    /// On, with its own numbers.
    On(IdleStop),
}

impl IdleSetting {
    /// The stop condition, if on.
    pub fn stop(self) -> Option<IdleStop> {
        match self {
            IdleSetting::Off => None,
            IdleSetting::On(s) => Some(s),
        }
    }
}

/// Parse `"1 GB"`, `"500MB"`, `"1.5 TB"` (decimal) or `"2 GiB"` (binary) or plain digits (bytes).
pub fn parse_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = (t[..split].trim(), t[split..].trim().to_ascii_lowercase());
    let v: f64 = num.parse().map_err(|_| format!("expected a size like \"1 GB\", got `{s}`"))?;
    let mult: f64 = match unit.as_str() {
        "" | "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1048576.0,
        "gib" => 1073741824.0,
        "tib" => 1099511627776.0,
        _ => return Err(format!("unknown size unit in `{s}`")),
    };
    let b = v * mult;
    if !b.is_finite() || b < 0.0 || b > MAX_IDLE_BYTES as f64 * 1000.0 {
        return Err(format!("size out of range: `{s}`"));
    }
    Ok(b.round() as u64)
}

/// `1000000000` -> `"1 GB"`; a size that is not a whole number of a unit stays in bytes.
pub fn format_bytes(b: u64) -> String {
    for (f, u) in [(1_000_000_000_000u64, "TB"), (1_000_000_000, "GB"), (1_000_000, "MB"), (1_000, "KB")] {
        if b >= f && b.is_multiple_of(f) {
            return format!("{} {u}", b / f);
        }
    }
    format!("{b} B")
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum IdleRaw {
    Flag(bool),
    Table {
        below: toml::Value,
        #[serde(default)]
        hours: Option<u32>,
    },
}

impl Serialize for IdleSetting {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            IdleSetting::Off => IdleRaw::Flag(false).serialize(s),
            IdleSetting::On(o) => {
                IdleRaw::Table { below: toml::Value::String(format_bytes(o.below)), hours: Some(o.hours) }.serialize(s)
            }
        }
    }
}

impl<'de> Deserialize<'de> for IdleSetting {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        match IdleRaw::deserialize(d)? {
            IdleRaw::Flag(false) => Ok(IdleSetting::Off),
            IdleRaw::Flag(true) => Err(D::Error::custom("use `false`, or a table { below = \"1 GB\", hours = 150 }")),
            IdleRaw::Table { below, hours } => {
                let below = match below {
                    toml::Value::Integer(i) => u64::try_from(i).map_err(D::Error::custom)?,
                    toml::Value::String(t) => parse_bytes(&t).map_err(D::Error::custom)?,
                    _ => return Err(D::Error::custom("`below` must be a size like \"1 GB\"")),
                };
                Ok(IdleSetting::On(IdleStop { below, hours: hours.unwrap_or(DEFAULT_IDLE_HOURS) }))
            }
        }
    }
}

fn validate_idle(i: Option<IdleSetting>, path: &str, e: &mut Vec<FieldError>) {
    if let Some(IdleSetting::On(s)) = i {
        if s.below == 0 || s.below > MAX_IDLE_BYTES {
            e.push(FieldError::new(format!("{path}.stop_when_idle.below"), "must be more than 0 and at most 1 PB"));
        }
        if s.hours == 0 || s.hours > MAX_IDLE_HOURS {
            e.push(FieldError::new(format!("{path}.stop_when_idle.hours"), format!("must be 1..={MAX_IDLE_HOURS}")));
        }
    }
}

/// Bandwidth and queue priority (SPEC §7.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Weight 1.
    Low,
    /// Weight 2.
    #[default]
    Normal,
    /// Weight 4; jumps the queue, never paused by "max active".
    High,
}

impl Priority {
    /// Bandwidth share weight, 4:2:1.
    pub const fn weight(self) -> u32 {
        match self {
            Priority::High => 4,
            Priority::Normal => 2,
            Priority::Low => 1,
        }
    }
}

/// What happens when the seeding goal is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnGoal {
    /// Stop sharing, keep the torrent in the list.
    #[default]
    StopKeep,
    /// Remove the torrent, keep the files.
    RemoveKeepFiles,
    /// Remove the torrent and delete its files (needs the `full` agent level when an agent sets it).
    RemoveDeleteFiles,
}

/// When to stop seeding.
///
/// Ratio is `uploaded / torrent size` (SPEC §7.2), so cross-seeded torrents work.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SeedingGoal {
    /// Do not seed at all once complete.
    StopWhenFinished,
    /// Until the ratio is reached.
    Ratio {
        /// Target ratio.
        ratio: f64,
    },
    /// Until seeded this long.
    Time {
        /// Target seeding time.
        time: Secs,
    },
    /// Until either is reached.
    RatioOrTime {
        /// Target ratio.
        ratio: f64,
        /// Target seeding time.
        time: Secs,
    },
    /// Until both are reached.
    RatioAndTime {
        /// Target ratio.
        ratio: f64,
        /// Target seeding time.
        time: Secs,
    },
    /// Never stop.
    Forever,
}

impl SeedingGoal {
    /// Has the goal been reached? `min_seed_time` (hit-and-run floor) is *not* applied here, see
    /// [`ResolvedPolicy::goal_reached`].
    pub fn reached(&self, ratio: f64, seeded: Secs) -> bool {
        match *self {
            SeedingGoal::StopWhenFinished => true,
            SeedingGoal::Ratio { ratio: r } => ratio >= r,
            SeedingGoal::Time { time } => seeded >= time,
            SeedingGoal::RatioOrTime { ratio: r, time } => ratio >= r || seeded >= time,
            SeedingGoal::RatioAndTime { ratio: r, time } => ratio >= r && seeded >= time,
            SeedingGoal::Forever => false,
        }
    }

    fn validate(&self, path: &str, e: &mut Vec<FieldError>) {
        let check_ratio = |r: f64, e: &mut Vec<FieldError>| {
            if !(r.is_finite() && r > 0.0 && r <= 1000.0) {
                e.push(FieldError::new(format!("{path}.ratio"), "must be greater than 0 and at most 1000"));
            }
        };
        let check_time = |t: Secs, e: &mut Vec<FieldError>| {
            if t.0 == 0 || t.0 > MAX_SECS {
                e.push(FieldError::new(format!("{path}.time"), "must be at least 1s and at most 100 years"));
            }
        };
        match *self {
            SeedingGoal::StopWhenFinished | SeedingGoal::Forever => {}
            SeedingGoal::Ratio { ratio } => check_ratio(ratio, e),
            SeedingGoal::Time { time } => check_time(time, e),
            SeedingGoal::RatioOrTime { ratio, time } | SeedingGoal::RatioAndTime { ratio, time } => {
                check_ratio(ratio, e);
                check_time(time, e);
            }
        }
    }
}

/// What a rule matches on. Empty / absent conditions match everything.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Match {
    /// Tracker profile ids; `"public"` matches torrents with no profile. A single string is accepted.
    #[serde(deserialize_with = "string_or_vec::deserialize", skip_serializing_if = "Vec::is_empty")]
    pub tracker: Vec<String>,
    /// BEP 27 private flag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private: Option<bool>,
    /// Detected (or user-set) category; a single string is accepted.
    #[serde(with = "category_vec", skip_serializing_if = "Vec::is_empty")]
    pub category: Vec<Category>,
}

/// The facts about a torrent that rules look at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TorrentAttrs<'a> {
    /// Matched tracker profile id (`None` = no profile).
    pub tracker: Option<&'a str>,
    /// Rule id linked from that profile, if any.
    pub linked_rule: Option<&'a str>,
    /// BEP 27 private flag.
    pub private: bool,
    /// Detected or user-chosen category.
    pub category: Category,
}

impl Match {
    fn tracker_ok(&self, a: &TorrentAttrs<'_>) -> bool {
        self.tracker.is_empty()
            || self.tracker.iter().any(|t| match a.tracker {
                Some(id) => t == id,
                None => t == PUBLIC_TRACKER,
            })
    }

    fn rest_ok(&self, a: &TorrentAttrs<'_>) -> bool {
        self.private.is_none_or(|p| p == a.private) && (self.category.is_empty() || self.category.contains(&a.category))
    }

    /// Does this condition set match `a`?
    pub fn matches(&self, a: &TorrentAttrs<'_>) -> bool {
        self.tracker_ok(a) && self.rest_ok(a)
    }

    /// Does `self` match every torrent that `other` matches? (Used to flag unreachable rules.)
    fn covers(&self, other: &Match) -> bool {
        let tracker = self.tracker.is_empty()
            || (!other.tracker.is_empty() && other.tracker.iter().all(|t| self.tracker.contains(t)));
        let private = self.private.is_none() || self.private == other.private;
        let category = self.category.is_empty()
            || (!other.category.is_empty() && other.category.iter().all(|c| self.category.contains(c)));
        tracker && private && category
    }
}

/// One rule. Unset actions fall back to the Default rule.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Rule {
    /// Stable id (`a-z0-9-_`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Disabled rules are skipped.
    pub enabled: bool,
    /// Conditions.
    #[serde(rename = "match")]
    pub when: Match,
    /// Seeding goal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<SeedingGoal>,
    /// Hit-and-run floor: never stop before seeding this long, whatever the goal says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_seed_time: Option<Secs>,
    /// Inactive stop condition (SPEC §7.5): absent = the global setting, `false` = off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_when_idle: Option<IdleSetting>,
    /// Priority.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    /// Action at goal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub on_goal: Option<OnGoal>,
    /// Finished-location override: a plain folder (SPEC §7A.4 precedence: rule > category > app
    /// default); the torrent's own layout goes below it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_dir: Option<String>,
    /// Per-torrent upload limit, bytes/s (0 = unlimited).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_limit: Option<u64>,
}

impl Default for Rule {
    fn default() -> Self {
        Rule {
            id: String::new(),
            name: String::new(),
            enabled: true,
            when: Match::default(),
            goal: None,
            min_seed_time: None,
            stop_when_idle: None,
            priority: None,
            on_goal: None,
            finished_dir: None,
            up_limit: None,
        }
    }
}

/// The fallback rule: every action is defined.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DefaultRule {
    /// Seeding goal (SPEC default: ratio 1.0 or 7 days).
    pub goal: SeedingGoal,
    /// Hit-and-run floor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_seed_time: Option<Secs>,
    /// Priority.
    pub priority: Priority,
    /// Action at goal.
    pub on_goal: OnGoal,
    /// Finished-location override (a plain folder).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_dir: Option<String>,
    /// Per-torrent upload limit, bytes/s (0 = unlimited).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_limit: Option<u64>,
}

impl Default for DefaultRule {
    fn default() -> Self {
        DefaultRule {
            goal: SeedingGoal::RatioOrTime { ratio: 1.0, time: Secs::days(7) },
            min_seed_time: None,
            priority: Priority::Normal,
            on_goal: OnGoal::StopKeep,
            finished_dir: None,
            up_limit: None,
        }
    }
}

/// Where a policy came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PolicySource {
    /// Matched in the ordered list.
    Rule(String),
    /// Reached through the tracker profile's rule link.
    LinkedRule(String),
    /// No rule matched.
    Default,
}

/// The complete, effective policy of a torrent.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedPolicy {
    /// Which rule produced it.
    pub source: PolicySource,
    /// Seeding goal.
    pub goal: SeedingGoal,
    /// Hit-and-run floor.
    pub min_seed_time: Option<Secs>,
    /// Priority.
    pub priority: Priority,
    /// Action at goal.
    pub on_goal: OnGoal,
    /// Finished-location override.
    pub finished_dir: Option<String>,
    /// Upload limit in bytes/s (`None` = unlimited).
    pub up_limit: Option<u64>,
    /// Inactive stop condition (SPEC §7.5), `None` = off.
    pub idle: Option<IdleStop>,
}

impl ResolvedPolicy {
    /// Goal reached *and* the minimum seed time served.
    pub fn goal_reached(&self, ratio: f64, seeded: Secs) -> bool {
        self.min_seed_time.is_none_or(|m| seeded >= m) && self.goal.reached(ratio, seeded)
    }
}

/// The hit-and-run floor of a torrent. A rule's own `min_seed_time` always wins. Without one, a
/// private torrent gets the global floor (`transfers.private_min_seed_secs`; `None`/0 = off), or the
/// Default rule's floor if that is longer; a public torrent gets the Default rule's floor only.
pub fn effective_min_seed(
    rule_floor: Option<Secs>,
    default_floor: Option<Secs>,
    private: bool,
    private_floor: Option<Secs>,
) -> Option<Secs> {
    if rule_floor.is_some() {
        return rule_floor;
    }
    match private_floor.filter(|f| private && f.0 > 0) {
        Some(g) => Some(default_floor.map_or(g, |d| d.max(g))),
        None => default_floor,
    }
}

/// The whole of `rules.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleSet {
    /// File format version.
    pub version: u32,
    /// The Default rule (always last; cannot be removed).
    pub default: DefaultRule,
    /// Ordered rules.
    #[serde(rename = "rule", skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<Rule>,
    /// Keys this build does not know, preserved when saving.
    #[serde(skip)]
    pub unknown: toml::Table,
}

impl Default for RuleSet {
    fn default() -> Self {
        RuleSet { version: RULES_VERSION, default: DefaultRule::default(), rules: Vec::new(), unknown: toml::Table::new() }
    }
}

fn validate_rule(r: &Rule, path: &str, e: &mut Vec<FieldError>) {
    if let Some(u) = r.up_limit {
        crate::config::check_range(e, &format!("{path}.up_limit"), u, 0, crate::config::MAX_RATE);
    }
    if !valid_id(&r.id) {
        e.push(FieldError::new(format!("{path}.id"), "must be 1-64 characters of a-z, 0-9, `-`, `_`"));
    } else if r.id == "default" {
        e.push(FieldError::new(format!("{path}.id"), "`default` is reserved for the Default rule"));
    }
    let n = r.name.chars().count();
    if n == 0 || n > 64 || r.name.chars().any(char::is_control) || r.name.trim().is_empty() {
        e.push(FieldError::new(format!("{path}.name"), "must be 1-64 printable characters"));
    }
    if r.when.tracker.len() > 64 {
        e.push(FieldError::new(format!("{path}.match.tracker"), "more than 64 trackers"));
    }
    for (i, t) in r.when.tracker.iter().enumerate() {
        if !valid_id(t) {
            e.push(FieldError::new(format!("{path}.match.tracker[{i}]"), "not a valid tracker profile id"));
        }
    }
    if let Some(g) = &r.goal {
        g.validate(&format!("{path}.goal"), e);
    }
    validate_idle(r.stop_when_idle, path, e);
    validate_common(r.min_seed_time, r.finished_dir.as_deref(), path, e);
}

fn validate_common(min: Option<Secs>, finished_dir: Option<&str>, path: &str, e: &mut Vec<FieldError>) {
    if let Some(m) = min
        && m.0 > MAX_SECS
    {
        e.push(FieldError::new(format!("{path}.min_seed_time"), "longer than 100 years"));
    }
    if let Some(d) = finished_dir {
        check_path(e, &format!("{path}.finished_dir"), d);
    }
}

impl RuleSet {
    /// Every problem in the set.
    pub fn validate(&self) -> Vec<FieldError> {
        let mut e = Vec::new();
        if self.version == 0 || self.version > RULES_VERSION {
            e.push(FieldError::new("version", format!("must be 1..={RULES_VERSION}")));
        }
        if self.rules.len() > MAX_RULES {
            e.push(FieldError::new("rule", format!("more than {MAX_RULES} rules")));
        }
        self.default.goal.validate("default.goal", &mut e);
        if let Some(u) = self.default.up_limit {
            crate::config::check_range(&mut e, "default.up_limit", u, 0, crate::config::MAX_RATE);
        }
        validate_common(self.default.min_seed_time, self.default.finished_dir.as_deref(), "default", &mut e);
        for (i, r) in self.rules.iter().enumerate() {
            validate_rule(r, &format!("rule[{i}]"), &mut e);
            if self.rules[..i].iter().any(|o| o.id == r.id) {
                e.push(FieldError::new(format!("rule[{i}].id"), format!("duplicate id `{}`", r.id)));
            }
        }
        e
    }

    /// Rules that can never match because an earlier enabled rule already covers them.
    fn shadow_warnings(&self) -> Vec<Warning> {
        let mut w = Vec::new();
        for (i, r) in self.rules.iter().enumerate().filter(|(_, r)| r.enabled) {
            if let Some(above) = self.rules[..i].iter().find(|a| a.enabled && a.when.covers(&r.when)) {
                w.push(Warning {
                    path: format!("rule[{i}]"),
                    message: format!("`{}` never matches: rule `{}` above already covers it", r.id, above.id),
                });
            }
        }
        w
    }

    /// Rule by id.
    pub fn get(&self, id: &str) -> Option<&Rule> {
        self.rules.iter().find(|r| r.id == id)
    }

    /// The rule that applies to `a`, or `None` for the Default rule. See the module docs.
    pub fn find(&self, a: &TorrentAttrs<'_>) -> Option<(&Rule, bool)> {
        if let Some(id) = a.linked_rule
            && let Some(r) = self.get(id)
            && r.enabled
            && r.when.rest_ok(a)
        {
            return Some((r, true));
        }
        self.rules.iter().find(|r| r.enabled && r.when.matches(a)).map(|r| (r, false))
    }

    /// Compute the effective policy without the global private floor. Pure and total.
    pub fn resolve(&self, a: &TorrentAttrs<'_>) -> ResolvedPolicy {
        self.resolve_with_floor(a, None)
    }

    /// Compute the effective policy. `private_floor` is `transfers.private_min_seed_secs` (SPEC §7):
    /// see [`effective_min_seed`]. Pure and total: never fails.
    pub fn resolve_with_floor(&self, a: &TorrentAttrs<'_>, private_floor: Option<Secs>) -> ResolvedPolicy {
        self.resolve_full(a, private_floor, None)
    }

    /// [`resolve_with_floor`](Self::resolve_with_floor) plus the global inactive condition
    /// (`transfers.idle_*`), which a rule's `stop_when_idle` overrides or switches off.
    pub fn resolve_full(&self, a: &TorrentAttrs<'_>, private_floor: Option<Secs>, idle_global: Option<IdleStop>) -> ResolvedPolicy {
        let d = &self.default;
        let (rule, linked) = match self.find(a) {
            Some((r, l)) => (Some(r), l),
            None => (None, false),
        };
        let up = rule.and_then(|r| r.up_limit).or(d.up_limit).filter(|v| *v != 0);
        ResolvedPolicy {
            source: match rule {
                Some(r) if linked => PolicySource::LinkedRule(r.id.clone()),
                Some(r) => PolicySource::Rule(r.id.clone()),
                None => PolicySource::Default,
            },
            goal: rule.and_then(|r| r.goal).unwrap_or(d.goal),
            min_seed_time: effective_min_seed(rule.and_then(|r| r.min_seed_time), d.min_seed_time, a.private, private_floor),
            priority: rule.and_then(|r| r.priority).unwrap_or(d.priority),
            on_goal: rule.and_then(|r| r.on_goal).unwrap_or(d.on_goal),
            finished_dir: rule.and_then(|r| r.finished_dir.clone()).or_else(|| d.finished_dir.clone()),
            up_limit: up,
            idle: match rule.and_then(|r| r.stop_when_idle) {
                Some(i) => i.stop(),
                None => idle_global,
            },
        }
    }

    /// How many of `torrents` would be governed by rule `id` ("affects 48 torrents").
    pub fn count_affected<'a>(&self, id: &str, torrents: impl IntoIterator<Item = TorrentAttrs<'a>>) -> usize {
        torrents.into_iter().filter(|a| self.find(a).is_some_and(|(r, _)| r.id == id)).count()
    }

    fn checked(&mut self, f: impl FnOnce(&mut Self)) -> Result<(), Vec<FieldError>> {
        let backup = (self.rules.clone(), self.default.clone());
        f(self);
        let errs = self.validate();
        if errs.is_empty() {
            Ok(())
        } else {
            (self.rules, self.default) = backup;
            Err(errs)
        }
    }

    /// Insert a rule at `index` (clamped; `None` = end of the list, still before Default).
    pub fn add_rule(&mut self, rule: Rule, index: Option<usize>) -> Result<(), Vec<FieldError>> {
        self.checked(|s| {
            let at = index.unwrap_or(s.rules.len()).min(s.rules.len());
            s.rules.insert(at, rule);
        })
    }

    /// Replace the rule with the same id.
    pub fn update_rule(&mut self, rule: Rule) -> Result<(), Vec<FieldError>> {
        if self.get(&rule.id).is_none() {
            return Err(vec![FieldError::new("id", format!("no rule `{}`", rule.id))]);
        }
        self.checked(|s| {
            if let Some(slot) = s.rules.iter_mut().find(|r| r.id == rule.id) {
                *slot = rule;
            }
        })
    }

    /// Remove a rule. The Default rule is not in the list and cannot be removed.
    pub fn remove_rule(&mut self, id: &str) -> Option<Rule> {
        let i = self.rules.iter().position(|r| r.id == id)?;
        Some(self.rules.remove(i))
    }

    /// Move a rule to `new_index` (clamped). Returns false if the id is unknown.
    pub fn move_rule(&mut self, id: &str, new_index: usize) -> bool {
        let Some(i) = self.rules.iter().position(|r| r.id == id) else { return false };
        let r = self.rules.remove(i);
        let at = new_index.min(self.rules.len());
        self.rules.insert(at, r);
        true
    }

    /// Enable or disable a rule. Returns false if the id is unknown.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        match self.rules.iter_mut().find(|r| r.id == id) {
            Some(r) => {
                r.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Replace the Default rule.
    pub fn set_default(&mut self, d: DefaultRule) -> Result<(), Vec<FieldError>> {
        self.checked(|s| s.default = d)
    }
}

fn has_template_key(t: &toml::Table) -> bool {
    t.get("default").and_then(toml::Value::as_table).is_some_and(|d| d.contains_key("template"))
        || t.get("rule").and_then(toml::Value::as_array).is_some_and(|a| a.iter().any(|r| r.get("template").is_some()))
}

/// Files written while locations could be destination templates have `template = "..."` on a rule.
/// Templates are gone (SPEC §7A.3, 2026-10-14): the key becomes `finished_dir`, a plain folder: the
/// fixed part in front of the first `{`; when nothing is left, the override is dropped.
fn migrate_templates(text: &str) -> Option<String> {
    let mut t: toml::Table = text.parse().ok()?;
    if !has_template_key(&t) {
        return None;
    }
    let mut tables: Vec<&mut toml::Table> = Vec::new();
    for (key, v) in t.iter_mut() {
        match (key.as_str(), v) {
            ("default", toml::Value::Table(d)) => tables.push(d),
            ("rule", toml::Value::Array(a)) => tables.extend(a.iter_mut().filter_map(toml::Value::as_table_mut)),
            _ => {}
        }
    }
    for r in tables {
        let Some(old) = r.remove("template") else { continue };
        let plain = old.as_str().map(fixed_prefix).unwrap_or_default();
        if !plain.is_empty() && !r.contains_key("finished_dir") {
            r.insert("finished_dir".into(), toml::Value::String(plain));
        }
    }
    toml::to_string(&t).ok()
}

impl Document for RuleSet {
    const FILE: &'static str = "rules.toml";

    fn defaults() -> Self {
        RuleSet::default()
    }

    fn parse_str(text: &str) -> Result<Parsed<Self>, DocError> {
        let migrated = migrate_templates(text);
        let text = migrated.as_deref().unwrap_or(text);
        let (mut v, unknown, mut warnings) = parse_toml::<RuleSet>(Self::FILE, text, RULES_VERSION)?;
        v.unknown = unknown;
        let errors = v.validate();
        if !errors.is_empty() {
            return Err(DocError::Invalid { file: Self::FILE, errors });
        }
        warnings.extend(v.shadow_warnings());
        Ok(Parsed { value: v, warnings })
    }

    fn file_is_outdated(text: &str) -> bool {
        text.parse::<toml::Table>().is_ok_and(|t| has_template_key(&t))
    }

    fn render(&self) -> Result<String, DocError> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(DocError::Invalid { file: Self::FILE, errors });
        }
        let table = toml::Table::try_from(self).map_err(|e| DocError::Parse {
            file: Self::FILE,
            line: None,
            column: None,
            message: e.to_string(),
        })?;
        render_table(Self::FILE, table, &self.unknown, HEADER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Unix-style absolute test path made absolute on this system: unchanged on Unix, a drive letter in front on Windows.
    fn abs(p: &str) -> String {
        if cfg!(windows) { format!("C:{p}") } else { p.to_string() }
    }

    /// A TOML test document whose quoted Unix-style absolute paths (`"/x"`) are made absolute on this system.
    fn win(doc: &str) -> String {
        if cfg!(windows) { doc.replace("\"/", "\"C:/") } else { doc.to_string() }
    }

    const EXAMPLE: &str = r##"
version = 1

[default]
goal = { kind = "ratio_or_time", ratio = 1.0, time = "7d" }

[[rule]]
id = "x-download-only"
name = "X: download only"
match = { tracker = "tracker-x" }
goal = { kind = "stop_when_finished" }

[[rule]]
id = "y-ratio"
name = "Y: seed to 1.0"
match = { tracker = ["tracker-y", "tracker-y2"] }
goal = { kind = "ratio", ratio = 1.0 }

[[rule]]
id = "z-high"
name = "Z: priority, H&R safe"
match = { tracker = "tracker-z" }
priority = "high"
goal = { kind = "ratio", ratio = 2.0 }
min_seed_time = "72h"
on_goal = "remove_keep_files"
up_limit = 1000000

[[rule]]
id = "public-software"
name = "Public software: seed longer"
match = { tracker = "public", private = false, category = "software" }
goal = { kind = "ratio", ratio = 3.0 }
finished_dir = "~/Software"
"##;

    fn attrs<'a>(tracker: Option<&'a str>, private: bool, category: Category) -> TorrentAttrs<'a> {
        TorrentAttrs { tracker, linked_rule: None, private, category }
    }

    fn example() -> RuleSet {
        RuleSet::parse_str(EXAMPLE).unwrap().value
    }

    #[test]
    fn spec_example_resolves_as_documented() {
        let rs = example();
        let x = rs.resolve(&attrs(Some("tracker-x"), true, Category::Movie));
        assert_eq!(x.source, PolicySource::Rule("x-download-only".into()));
        assert_eq!(x.goal, SeedingGoal::StopWhenFinished);
        assert_eq!(x.priority, Priority::Normal, "inherited from Default");
        let y = rs.resolve(&attrs(Some("tracker-y2"), true, Category::Tv));
        assert_eq!(y.goal, SeedingGoal::Ratio { ratio: 1.0 });
        let z = rs.resolve(&attrs(Some("tracker-z"), true, Category::Music));
        assert_eq!((z.priority, z.min_seed_time, z.on_goal, z.up_limit), (Priority::High, Some(Secs::hours(72)), OnGoal::RemoveKeepFiles, Some(1_000_000)));
        assert!(!z.goal_reached(5.0, Secs::hours(71)), "H&R floor holds even far past the ratio");
        assert!(z.goal_reached(2.0, Secs::hours(72)));
        let p = rs.resolve(&attrs(None, false, Category::Software));
        assert_eq!(p.finished_dir.as_deref(), Some("~/Software"));
        assert_eq!(p.goal, SeedingGoal::Ratio { ratio: 3.0 });
        let d = rs.resolve(&attrs(None, false, Category::Movie));
        assert_eq!(d.source, PolicySource::Default);
        assert_eq!(d.goal, SeedingGoal::RatioOrTime { ratio: 1.0, time: Secs::days(7) });
        // Public rule requires "no profile": a private software torrent from an unknown tracker misses it.
        assert_eq!(rs.resolve(&attrs(None, true, Category::Software)).source, PolicySource::Default);
    }

    /// Straight-line reference implementation to compare `resolve` against, over a full grid.
    fn reference(rs: &RuleSet, a: &TorrentAttrs<'_>) -> (Option<String>, SeedingGoal, Priority, OnGoal) {
        let mut chosen: Option<&Rule> = None;
        if let Some(l) = a.linked_rule {
            for r in &rs.rules {
                if r.id == l && r.enabled {
                    let priv_ok = match r.when.private {
                        Some(p) => p == a.private,
                        None => true,
                    };
                    let cat_ok = r.when.category.is_empty() || r.when.category.contains(&a.category);
                    if priv_ok && cat_ok {
                        chosen = Some(r);
                    }
                }
            }
        }
        if chosen.is_none() {
            for r in &rs.rules {
                if !r.enabled {
                    continue;
                }
                let t_ok = r.when.tracker.is_empty()
                    || r.when.tracker.iter().any(|t| (t == "public" && a.tracker.is_none()) || Some(t.as_str()) == a.tracker);
                let p_ok = r.when.private.is_none_or(|p| p == a.private);
                let c_ok = r.when.category.is_empty() || r.when.category.contains(&a.category);
                if t_ok && p_ok && c_ok {
                    chosen = Some(r);
                    break;
                }
            }
        }
        (
            chosen.map(|r| r.id.clone()),
            chosen.and_then(|r| r.goal).unwrap_or(rs.default.goal),
            chosen.and_then(|r| r.priority).unwrap_or(rs.default.priority),
            chosen.and_then(|r| r.on_goal).unwrap_or(rs.default.on_goal),
        )
    }

    fn grid_ruleset() -> RuleSet {
        let mut rs = RuleSet::default();
        let mk = |id: &str, t: &[&str], p: Option<bool>, c: &[Category], enabled: bool, prio: Priority| Rule {
            id: id.into(),
            name: id.into(),
            enabled,
            when: Match { tracker: t.iter().map(|s| s.to_string()).collect(), private: p, category: c.to_vec() },
            priority: Some(prio),
            goal: Some(SeedingGoal::Forever),
            ..Rule::default()
        };
        for r in [
            mk("ra", &["a"], None, &[], true, Priority::High),
            mk("off", &["b"], None, &[], false, Priority::Low),
            mk("b-tv", &["b"], Some(true), &[Category::Tv], true, Priority::Low),
            mk("pub-sw", &["public"], Some(false), &[Category::Software, Category::Book], true, Priority::High),
            mk("any-music", &[], None, &[Category::Music], true, Priority::Low),
            mk("b-any", &["b", "a"], None, &[], true, Priority::Normal),
            mk("catch-private", &[], Some(true), &[], true, Priority::High),
        ] {
            rs.add_rule(r, None).unwrap();
        }
        rs
    }

    #[test]
    fn resolve_matches_reference_over_the_whole_grid() {
        let rs = grid_ruleset();
        let mut n = 0;
        for tracker in [None, Some("a"), Some("b"), Some("c")] {
            for linked in [None, Some("ra"), Some("b-tv"), Some("off"), Some("missing")] {
                for private in [false, true] {
                    for category in Category::ALL {
                        let a = TorrentAttrs { tracker, linked_rule: linked, private, category };
                        let got = rs.resolve(&a);
                        let want = reference(&rs, &a);
                        let src = match &got.source {
                            PolicySource::Rule(i) | PolicySource::LinkedRule(i) => Some(i.clone()),
                            PolicySource::Default => None,
                        };
                        assert_eq!((src, got.goal, got.priority, got.on_goal), want, "{a:?}");
                        n += 1;
                    }
                }
            }
        }
        assert_eq!(n, 4 * 5 * 2 * Category::ALL.len());
    }

    #[test]
    fn linked_rule_waives_the_tracker_condition_only() {
        let rs = grid_ruleset();
        // "b-tv" needs private + tv. Linked from profile "zzz" it applies to a private TV torrent...
        let a = TorrentAttrs { tracker: Some("zzz"), linked_rule: Some("b-tv"), private: true, category: Category::Tv };
        assert_eq!(rs.resolve(&a).source, PolicySource::LinkedRule("b-tv".into()));
        // ...but not to a movie: the link does not override the rule's own conditions.
        let a = TorrentAttrs { category: Category::Movie, ..a };
        assert_ne!(rs.resolve(&a).source, PolicySource::LinkedRule("b-tv".into()));
        // A disabled linked rule is ignored.
        let a = TorrentAttrs { tracker: Some("b"), linked_rule: Some("off"), private: false, category: Category::Movie };
        assert_eq!(rs.resolve(&a).source, PolicySource::Rule("b-any".into()));
    }

    #[test]
    fn first_match_wins_not_best_match() {
        let rs = grid_ruleset();
        // "ra" (tracker a) is above "b-any" (b or a): a torrent from tracker a gets ra.
        let r = rs.resolve(&attrs(Some("a"), false, Category::Music));
        assert_eq!(r.source, PolicySource::Rule("ra".into()));
        // Moving the music rule to the top changes the winner, nothing else does.
        let mut rs2 = rs.clone();
        assert!(rs2.move_rule("any-music", 0));
        assert_eq!(rs2.resolve(&attrs(Some("a"), false, Category::Music)).source, PolicySource::Rule("any-music".into()));
    }

    #[test]
    fn inherits_missing_actions_and_filters_zero_limit() {
        let mut rs = RuleSet::default();
        rs.set_default(DefaultRule { priority: Priority::Low, up_limit: Some(500), min_seed_time: Some(Secs::hours(1)), ..DefaultRule::default() }).unwrap();
        rs.add_rule(Rule { id: "r".into(), name: "r".into(), up_limit: Some(0), ..Rule::default() }, None).unwrap();
        let p = rs.resolve(&attrs(None, false, Category::Other));
        assert_eq!(p.source, PolicySource::Rule("r".into()));
        assert_eq!((p.priority, p.min_seed_time, p.up_limit), (Priority::Low, Some(Secs::hours(1)), None), "0 = unlimited overrides the default's 500");
    }

    #[test]
    fn private_floor_applies_to_private_torrents_and_rule_floor_wins() {
        let ten = Some(Secs::days(10));
        let mut rs = RuleSet::default();
        rs.set_default(DefaultRule { goal: SeedingGoal::Ratio { ratio: 1.0 }, ..DefaultRule::default() }).unwrap();
        rs.add_rule(Rule { id: "own".into(), name: "own".into(), when: Match { category: vec![Category::Music], ..Match::default() }, min_seed_time: Some(Secs::hours(72)), ..Rule::default() }, None).unwrap();
        // Private, no rule floor: the global floor holds the goal back.
        let p = rs.resolve_with_floor(&attrs(None, true, Category::Movie), ten);
        assert_eq!(p.min_seed_time, ten);
        assert!(!p.goal_reached(5.0, Secs::days(9)));
        assert!(p.goal_reached(5.0, Secs::days(10)));
        // Public torrents are unaffected.
        let p = rs.resolve_with_floor(&attrs(None, false, Category::Movie), ten);
        assert_eq!(p.min_seed_time, None);
        assert!(p.goal_reached(5.0, Secs::from_secs(0)));
        // A rule's own floor wins, even a shorter one.
        let p = rs.resolve_with_floor(&attrs(None, true, Category::Music), ten);
        assert_eq!(p.min_seed_time, Some(Secs::hours(72)));
        // Off (None or 0) changes nothing.
        for off in [None, Some(Secs(0))] {
            assert_eq!(rs.resolve_with_floor(&attrs(None, true, Category::Movie), off).min_seed_time, None);
        }
        // A longer Default-rule floor is kept; `resolve` is the no-global-floor form.
        rs.set_default(DefaultRule { min_seed_time: Some(Secs::days(14)), ..rs.default.clone() }).unwrap();
        assert_eq!(rs.resolve_with_floor(&attrs(None, true, Category::Movie), ten).min_seed_time, Some(Secs::days(14)));
        assert_eq!(rs.resolve(&attrs(None, true, Category::Movie)).min_seed_time, Some(Secs::days(14)));
    }

    #[test]
    fn goals_reach_correctly() {
        let t = Secs::hours(10);
        let cases = [
            (SeedingGoal::StopWhenFinished, 0.0, 0, true),
            (SeedingGoal::Forever, 1e9, u64::MAX / 2, false),
            (SeedingGoal::Ratio { ratio: 2.0 }, 1.99, 99_999_999, false),
            (SeedingGoal::Ratio { ratio: 2.0 }, 2.0, 0, true),
            (SeedingGoal::Time { time: t }, 0.0, 36_000, true),
            (SeedingGoal::Time { time: t }, 100.0, 35_999, false),
            (SeedingGoal::RatioOrTime { ratio: 2.0, time: t }, 2.0, 0, true),
            (SeedingGoal::RatioOrTime { ratio: 2.0, time: t }, 0.0, 36_000, true),
            (SeedingGoal::RatioOrTime { ratio: 2.0, time: t }, 1.0, 100, false),
            (SeedingGoal::RatioAndTime { ratio: 2.0, time: t }, 2.0, 100, false),
            (SeedingGoal::RatioAndTime { ratio: 2.0, time: t }, 1.0, 36_000, false),
            (SeedingGoal::RatioAndTime { ratio: 2.0, time: t }, 2.0, 36_000, true),
        ];
        for (g, ratio, secs, want) in cases {
            assert_eq!(g.reached(ratio, Secs(secs)), want, "{g:?} {ratio} {secs}");
        }
    }

    #[test]
    fn priority_weights_are_4_2_1() {
        assert_eq!([Priority::High, Priority::Normal, Priority::Low].map(Priority::weight), [4, 2, 1]);
    }

    #[test]
    fn crud_keeps_order_and_rolls_back_invalid_changes() {
        let mut rs = example();
        let before = rs.clone();
        assert!(rs.add_rule(Rule { id: "x-download-only".into(), name: "dup".into(), ..Rule::default() }, None).is_err());
        assert!(rs.add_rule(Rule { id: "default".into(), name: "n".into(), ..Rule::default() }, None).is_err(), "reserved");
        assert!(rs.add_rule(Rule { id: "bad".into(), name: "n".into(), goal: Some(SeedingGoal::Ratio { ratio: f64::NAN }), ..Rule::default() }, None).is_err());
        assert!(rs.add_rule(Rule { id: "bad2".into(), name: "n".into(), finished_dir: Some("relative/dir".into()), ..Rule::default() }, None).is_err());
        assert_eq!(rs, before, "failed edits change nothing");
        rs.add_rule(Rule { id: "first".into(), name: "First".into(), ..Rule::default() }, Some(0)).unwrap();
        assert_eq!(rs.rules[0].id, "first");
        assert!(rs.move_rule("first", 99));
        assert_eq!(rs.rules.last().unwrap().id, "first");
        assert!(!rs.move_rule("nope", 0));
        assert!(rs.set_enabled("first", false) && !rs.set_enabled("nope", true));
        assert!(rs.update_rule(Rule { id: "nope".into(), name: "n".into(), ..Rule::default() }).is_err());
        assert!(rs.remove_rule("first").is_some() && rs.remove_rule("first").is_none());
        assert_eq!(rs.rules.len(), 4);
    }

    #[test]
    fn default_rule_cannot_be_deleted_and_is_always_last() {
        let mut rs = RuleSet::default();
        assert!(rs.remove_rule("default").is_none());
        // An empty or default-less file still has a Default rule.
        assert_eq!(RuleSet::parse_str("").unwrap().value.default, DefaultRule::default());
        assert_eq!(RuleSet::parse_str("[[rule]]\nid=\"a\"\nname=\"A\"\n").unwrap().value.default, DefaultRule::default());
        // A rule with no conditions is a catch-all, but still above Default.
        rs.add_rule(Rule { id: "all".into(), name: "all".into(), priority: Some(Priority::High), ..Rule::default() }, None).unwrap();
        assert_eq!(rs.resolve(&attrs(None, false, Category::Other)).priority, Priority::High);
    }

    #[test]
    fn a_rule_template_of_an_older_file_becomes_a_plain_folder() {
        let old = &win("[default]\ntemplate = \"~/Seeding/{name}/\"\n[[rule]]\nid = \"a\"\nname = \"A\"\ntemplate = \"~/Software/{name}\"\n\
                   [[rule]]\nid = \"b\"\nname = \"B\"\ntemplate = \"{name}\"\n[[rule]]\nid = \"c\"\nname = \"C\"\ntemplate = \"/data/plain\"\n");
        assert!(RuleSet::file_is_outdated(old));
        let p = RuleSet::parse_str(old).unwrap();
        assert!(p.warnings.iter().all(|w| !w.path.contains("template")), "{:?}", p.warnings);
        let rs = p.value;
        assert_eq!(rs.default.finished_dir.as_deref(), Some("~/Seeding"));
        assert_eq!(rs.get("a").unwrap().finished_dir.as_deref(), Some("~/Software"));
        assert_eq!(rs.get("b").unwrap().finished_dir, None, "nothing fixed in front: the default");
        assert_eq!(rs.get("c").unwrap().finished_dir.as_deref(), Some(abs("/data/plain").as_str()));
        let out = rs.render().unwrap();
        assert!(!out.contains("template"), "{out}");
        assert!(!RuleSet::file_is_outdated(&out));
        assert_eq!(RuleSet::parse_str(&out).unwrap().value, rs);
    }

    #[test]
    fn count_affected_previews_a_rule() {
        let rs = example();
        let torrents = [
            attrs(Some("tracker-x"), true, Category::Movie),
            attrs(Some("tracker-x"), true, Category::Tv),
            attrs(Some("tracker-z"), true, Category::Tv),
            attrs(None, false, Category::Software),
        ];
        assert_eq!(rs.count_affected("x-download-only", torrents), 2);
        assert_eq!(rs.count_affected("public-software", torrents), 1);
        assert_eq!(rs.count_affected("nope", torrents), 0);
    }

    #[test]
    fn file_roundtrip_and_validation_errors() {
        let p = RuleSet::parse_str(EXAMPLE).unwrap();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        let out = p.value.render().unwrap();
        assert_eq!(RuleSet::parse_str(&out).unwrap().value, p.value);

        let bad = "[default]\ngoal = { kind = \"ratio\", ratio = -1 }\n[[rule]]\nid = \"A\"\nname = \"\"\ngoal = { kind = \"time\", time = \"0s\" }\nmatch = { tracker = [\"Bad Id\"] }\n";
        let Err(DocError::Invalid { errors, .. }) = RuleSet::parse_str(bad) else { panic!() };
        let paths: Vec<_> = errors.iter().map(|e| e.path.as_str()).collect();
        for want in ["default.goal.ratio", "rule[0].id", "rule[0].name", "rule[0].goal.time", "rule[0].match.tracker[0]"] {
            assert!(paths.contains(&want), "{want} in {paths:?}");
        }
        // A goal whose required field is missing is a parse error with a location.
        let err = RuleSet::parse_str("[default]\ngoal = { kind = \"ratio\" }\n").unwrap_err();
        assert!(matches!(err, DocError::Parse { .. }), "{err}");
        // Unknown category / goal kind name the problem.
        let err = RuleSet::parse_str("[[rule]]\nid=\"a\"\nname=\"a\"\nmatch = { category = \"games\" }\n").unwrap_err();
        assert!(err.to_string().contains("games"), "{err}");
    }

    #[test]
    fn unknown_keys_warn_and_unreachable_rules_are_flagged() {
        let text = "[[rule]]\nid=\"all\"\nname=\"all\"\nstop_wen = \"typo\"\n[[rule]]\nid=\"never\"\nname=\"never\"\nmatch = { private = true }\n";
        let p = RuleSet::parse_str(text).unwrap();
        let msgs: Vec<_> = p.warnings.iter().map(|w| format!("{w}")).collect();
        assert!(msgs.iter().any(|m| m.contains("rule[0].stop_wen")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("never matches") && m.contains("`all`")), "{msgs:?}");
    }

    #[test]
    fn single_string_and_list_are_equivalent() {
        let a = RuleSet::parse_str("[[rule]]\nid=\"a\"\nname=\"a\"\nmatch = { tracker = \"t\", category = \"tv\" }\n").unwrap().value;
        let b = RuleSet::parse_str("[[rule]]\nid=\"a\"\nname=\"a\"\nmatch = { tracker = [\"t\"], category = [\"tv\"] }\n").unwrap().value;
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod idle_tests {
    use super::*;

    const DOC: &str = r##"
version = 1

[[rule]]
id = "busy-only"
name = "Busy only"
match = { tracker = "tracker-a" }
stop_when_idle = { below = "500 MB", hours = 72 }

[[rule]]
id = "never-idle"
name = "Never idle"
match = { tracker = "tracker-b" }
stop_when_idle = false

[[rule]]
id = "plain"
name = "Plain"
match = { tracker = "tracker-c" }
stop_when_idle = { below = 2000000000 }
"##;

    fn attrs(t: &str) -> TorrentAttrs<'_> {
        TorrentAttrs { tracker: Some(t), linked_rule: None, private: false, category: Category::Other }
    }

    #[test]
    fn sizes_parse_and_print() {
        assert_eq!(parse_bytes("1 GB"), Ok(1_000_000_000));
        assert_eq!(parse_bytes("500MB"), Ok(500_000_000));
        assert_eq!(parse_bytes("1.5 tb"), Ok(1_500_000_000_000));
        assert_eq!(parse_bytes("2 GiB"), Ok(2_147_483_648));
        assert_eq!(parse_bytes("42"), Ok(42));
        assert!(parse_bytes("lots").is_err() && parse_bytes("1 parsec").is_err() && parse_bytes("").is_err());
        assert_eq!(format_bytes(1_000_000_000), "1 GB");
        assert_eq!(format_bytes(1_500_000_000), "1500 MB");
        assert_eq!(format_bytes(1234), "1234 B");
        assert_eq!(parse_bytes(&format_bytes(1_234_567)), Ok(1_234_567));
    }

    #[test]
    fn rule_values_override_the_global_one_and_can_turn_it_off() {
        let rs = RuleSet::parse_str(DOC).unwrap().value;
        let global = Some(IdleStop { below: 1_000_000_000, hours: 150 });
        let r = |t: &str| rs.resolve_full(&attrs(t), None, global).idle;
        assert_eq!(r("tracker-a"), Some(IdleStop { below: 500_000_000, hours: 72 }), "the rule's own numbers");
        assert_eq!(r("tracker-b"), None, "`false` switches it off for this rule");
        assert_eq!(r("tracker-c"), Some(IdleStop { below: 2_000_000_000, hours: 150 }), "hours default to 150");
        assert_eq!(r("elsewhere"), global, "no rule: the global setting");
        assert_eq!(rs.resolve_full(&attrs("elsewhere"), None, None).idle, None, "off by default");
        assert_eq!(rs.resolve(&attrs("tracker-a")).idle.map(|i| i.hours), Some(72));
    }

    #[test]
    fn it_round_trips_through_the_file() {
        let rs = RuleSet::parse_str(DOC).unwrap().value;
        let text = rs.render().unwrap();
        assert!(text.contains("stop_when_idle"), "{text}");
        assert!(text.contains("500 MB"), "{text}");
        let again = RuleSet::parse_str(&text).unwrap().value;
        assert_eq!(again.rules, rs.rules);
    }

    #[test]
    fn bad_values_are_refused_with_a_field_path() {
        for bad in ["{ below = \"1 GB\", hours = 0 }", "{ below = \"1 GB\", hours = 99999 }", "{ below = 0 }", "true"] {
            let doc = format!("version = 1\n[[rule]]\nid = \"x\"\nname = \"X\"\nstop_when_idle = {bad}\n");
            assert!(RuleSet::parse_str(&doc).is_err(), "{bad}");
        }
    }
}
