//! Settings › Rules (SPEC §7, §7E.1) as a view model every host draws (docs/CROSS-PLATFORM.md §1): Rules and Trackers,
//! each a list on the left (the rules in their order with the Default rule under them; the tracker profiles) and the
//! selected one's editor on the right, with its footer (what saving would change, Revert and Save). Rust decides every
//! word, the rows and their controls (the same [`SettingsControl`]s as the rest of Settings), what is dirty, and what
//! each change does ([`rules_editor_apply`]); the host draws, keeps the [`RulesEditorState`], reads the preview and
//! the counts from the core and runs the saves ([`RulesEffect`]).
//!
//! An edit changes a draft; the rule (or tracker) shows as saved while it has none. A tracker's download location is
//! drawn by the host's own folder editor (shared with the tracker's Info page), which hands the edited profile back
//! ([`RulesChange::TrackerFolders`]).

use crate::row_text::category_label;
use crate::settings::{DefaultRuleRecord, GoalKind, PrivateMatch, ProfileRecord, RulePreview, RuleRecord, RulesDoc};
use crate::settings_parse::{self as p, SettingsFieldKind};
use crate::settings_view::{
    category_from_id, choice, duration, folder, goal_controls, group, idle_controls, on_goal_id, quiet_text, rate, row, settings_blank_rule,
    switch_spec, text, SettingKey, SettingsButton, SettingsButtonStyle, SettingsCheck, SettingsChoiceStyle, SettingsCommand, SettingsControl,
    SettingsGroup, SettingsListButtons, SettingsOption, SettingsQuestion, SettingsQuestionChoice, SettingsRow, SettingsRowStyle, SettingsTextRun,
    SettingsValue, ToggleSpec, CATEGORIES, category_id,
};
use crate::settings_view_apply::{goal_edit, id, measure, on, on_goal, read_field};
use crate::types::Priority;

/// The id of a tracker-less torrent in a rule's trackers.
const PUBLIC: &str = "public";
/// A new rule's minimum seed time when it is switched on (72 hours), and a new upload limit (1 MB/s).
const MIN_SEED_DEFAULT: u64 = 72 * 3600;
const LIMIT_DEFAULT: u64 = 1_000_000;
/// A new inactive-seed threshold (1 GB).
const IDLE_DEFAULT: u64 = 1_000_000_000;

// MARK: The records

/// Which list the Rules tab shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RulesSection {
    Rules,
    Trackers,
}

/// The tab's own state: what is selected, and the edits not saved yet. `rules_editor_state()` makes a new one.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulesEditorState {
    pub section: RulesSection,
    /// The rule shown: its id, or `None` for the Default rule.
    pub rule: Option<String>,
    /// The rule's unsaved edits (`None`: it shows as saved).
    pub rule_draft: Option<RuleRecord>,
    pub default_draft: Option<DefaultRuleRecord>,
    /// The tracker shown; `None` and not `new_tracker`: the first one.
    pub tracker: Option<String>,
    pub tracker_draft: Option<ProfileRecord>,
    /// A new tracker is being made (its draft is `tracker_draft`).
    pub new_tracker: bool,
    /// What to select once the host has saved (`RulesSaved`, `ProfilesSaved`).
    pub after_save: Option<RulesAfterSave>,
}

/// What to select once a save went through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum RulesAfterSave {
    /// The rule shown, its edits saved.
    Keep,
    /// The rule added last.
    LastRule,
    /// The Default rule (the rule shown was deleted).
    DefaultRule,
    /// The tracker added last.
    LastTracker,
    /// The first tracker (the one shown was deleted).
    FirstTracker,
}

/// How many torrents a rule governs now (`Core::rule_preview(rules, id).affected`; `*` for the Default rule).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RuleCount {
    pub rule_id: String,
    pub count: u32,
}

/// What the tab is built from.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulesEditorInput {
    pub state: RulesEditorState,
    /// `Core::rules_load`, `Core::profiles_load` (as saved).
    pub rules: RulesDoc,
    pub profiles: Vec<ProfileRecord>,
    /// `Core::rule_preview` of [`rules_editor_preview_query`]: what saving the rule shown would change.
    pub preview: RulePreview,
    /// Every rule's count, and the Default rule's (`*`).
    pub counts: Vec<RuleCount>,
    /// Why the last save was refused.
    pub problems: Vec<String>,
}

/// A rule or a tracker in the list.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulesListRow {
    pub id: String,
    /// A rule's place ("1", "2"; "·" for the Default rule); `None`: a tracker (drawn with a tracker image), or the new
    /// tracker (`new`, drawn with a plus).
    pub badge: Option<String>,
    pub new: bool,
    pub title: String,
    pub subtitle: String,
    /// How many torrents it governs.
    pub count: Option<u32>,
    /// Drawn faded when off.
    pub enabled: bool,
    pub selected: bool,
    pub select: SettingsCommand,
}

/// The editor's footer: what saving would do (or why it was refused), Revert and Save.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulesFooter {
    pub runs: Vec<SettingsTextRun>,
    /// The runs are a refusal (drawn in the error colour).
    pub problem: bool,
    pub revert: SettingsButton,
    pub save: SettingsButton,
}

/// The Rules tab.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulesEditorModel {
    /// "Rules", "Trackers" (a segmented control: `selected` is the section shown).
    pub sections: Vec<SettingsOption>,
    pub selected: String,
    /// Over the list.
    pub note: String,
    pub rows: Vec<RulesListRow>,
    /// The rules can be dragged into another order ([`RulesChange::MoveRule`]).
    pub reorderable: bool,
    /// Under the rules, apart: the Default rule.
    pub default_row: Option<RulesListRow>,
    /// The list's quiet line while it is empty.
    pub empty: Option<String>,
    pub list_buttons: SettingsListButtons,
    /// The editor, top to bottom (drawn compact: hints under the controls).
    pub groups: Vec<SettingsGroup>,
    /// Where the tracker's folder editor goes: after the group with this id (`None`: no folder editor).
    pub folders_after: Option<String>,
    /// The tracker shown or made, for the folder editor.
    pub folders_profile: Option<ProfileRecord>,
    /// The folder editor's title.
    pub folders_title: String,
    /// Nothing selected: this instead of the editor.
    pub placeholder: Option<String>,
    pub footer: Option<RulesFooter>,
}

/// A change in the tab.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum RulesChange {
    Section { section: RulesSection },
    /// A rule clicked (`None`: the Default rule).
    SelectRule { id: Option<String> },
    SelectTracker { id: String },
    /// A control's new value.
    Set { key: SettingKey, value: SettingsValue },
    Revert,
    Save,
    AddRule,
    RemoveRule,
    /// A rule dragged from `from` to before the rule at `to` (`to` = the count: last).
    MoveRule { from: u32, to: u32 },
    /// The + of Trackers, or "Create tracker profile…" for a host (`host` names the new tracker and is its address).
    NewTracker { host: Option<String> },
    /// "Edit Tracker…": shows that tracker.
    EditTracker { id: String },
    /// The tracker shown, deleted (after its question).
    RemoveTracker,
    AddAddress,
    RemoveAddress { index: u32 },
    /// The host's folder editor changed the tracker's folders: the profile as it has it.
    TrackerFolders { profile: ProfileRecord },
    /// The host saved what [`RulesEffect`] asked for: the rules or the profiles as the core returned them.
    RulesSaved { rules: RulesDoc },
    ProfilesSaved { profiles: Vec<ProfileRecord> },
}

/// A save the host runs; then it sends `RulesSaved` or `ProfilesSaved` with what the core returned, or shows the
/// refusal (`CoreError::Invalid` problems) in the input's `problems`.
#[derive(Clone, Debug, PartialEq, uniffi::Enum)]
pub enum RulesEffect {
    /// `Core::rules_save(rules)`.
    SaveRules { rules: RulesDoc },
    /// `Core::profiles_save(profiles)`.
    SaveProfiles { profiles: Vec<ProfileRecord> },
}

/// What [`rules_editor_apply`] made of a change.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulesApplied {
    pub state: RulesEditorState,
    pub effect: Option<RulesEffect>,
    /// The problems shown belong to what was shown before: clear them.
    pub clear_problems: bool,
}

/// The rules and the rule to preview ([`rules_editor_preview_query`]).
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct RulePreviewQuery {
    pub rules: RulesDoc,
    pub rule_id: String,
}

/// A new tab's state: Rules, the Default rule, nothing edited.
#[uniffi::export]
pub fn rules_editor_state() -> RulesEditorState {
    RulesEditorState {
        section: RulesSection::Rules,
        rule: None,
        rule_draft: None,
        default_draft: None,
        tracker: None,
        tracker_draft: None,
        new_tracker: false,
        after_save: None,
    }
}

// MARK: Reading the state

fn saved_rule<'a>(s: &RulesEditorState, doc: &'a RulesDoc) -> Option<&'a RuleRecord> {
    s.rule.as_ref().and_then(|id| doc.rules.iter().find(|r| &r.id == id))
}

/// The rule shown with its edits (`None`: the Default rule is shown).
fn shown_rule(s: &RulesEditorState, doc: &RulesDoc) -> Option<RuleRecord> {
    saved_rule(s, doc).map(|saved| s.rule_draft.clone().filter(|d| d.id == saved.id).unwrap_or_else(|| saved.clone()))
}

fn shown_default(s: &RulesEditorState, doc: &RulesDoc) -> DefaultRuleRecord {
    s.default_draft.clone().unwrap_or_else(|| doc.default_rule.clone())
}

fn rule_dirty(s: &RulesEditorState, doc: &RulesDoc) -> bool {
    match saved_rule(s, doc) {
        Some(saved) => shown_rule(s, doc).as_ref() != Some(saved),
        None => shown_default(s, doc) != doc.default_rule,
    }
}

fn saved_tracker<'a>(s: &RulesEditorState, profiles: &'a [ProfileRecord]) -> Option<&'a ProfileRecord> {
    if s.new_tracker {
        return None;
    }
    match &s.tracker {
        Some(id) => profiles.iter().find(|x| &x.id == id),
        None => profiles.first(),
    }
}

/// The tracker shown or made, with its edits.
fn shown_tracker(s: &RulesEditorState, profiles: &[ProfileRecord]) -> Option<ProfileRecord> {
    if s.new_tracker {
        return s.tracker_draft.clone();
    }
    saved_tracker(s, profiles).map(|saved| s.tracker_draft.clone().filter(|d| d.id == saved.id).unwrap_or_else(|| saved.clone()))
}

fn tracker_dirty(s: &RulesEditorState, profiles: &[ProfileRecord]) -> bool {
    s.new_tracker || shown_tracker(s, profiles).as_ref() != saved_tracker(s, profiles)
}

/// The rules with the rule shown as edited, and its id (`*`: the Default rule): what `Core::rule_preview` takes.
#[uniffi::export]
pub fn rules_editor_preview_query(state: RulesEditorState, rules: RulesDoc) -> RulePreviewQuery {
    let mut doc = rules.clone();
    match shown_rule(&state, &rules) {
        Some(r) => {
            let id = r.id.clone();
            if let Some(x) = doc.rules.iter_mut().find(|x| x.id == id) {
                *x = r;
            }
            RulePreviewQuery { rules: doc, rule_id: id }
        }
        None => {
            doc.default_rule = shown_default(&state, &rules);
            RulePreviewQuery { rules: doc, rule_id: "*".into() }
        }
    }
}

// MARK: Building it

fn rules_cmd(change: RulesChange) -> SettingsCommand {
    SettingsCommand::Rules { change }
}

fn opt(id: &str, title: &str) -> SettingsOption {
    SettingsOption { id: id.into(), title: title.into(), enabled: true }
}

fn styled_button(title: &str, help: &str, style: SettingsButtonStyle, enabled: bool, command: SettingsCommand) -> SettingsButton {
    SettingsButton { title: title.into(), help: help.into(), enabled, style, command, identifier: None }
}

fn private_id(m: PrivateMatch) -> &'static str {
    match m {
        PrivateMatch::Any => "any",
        PrivateMatch::PrivateOnly => "private",
        PrivateMatch::PublicOnly => "public",
    }
}

fn priority_id(p: Option<Priority>) -> &'static str {
    match p {
        None => "default",
        Some(Priority::Low) => "low",
        Some(Priority::Normal) => "normal",
        Some(Priority::High) => "high",
    }
}

fn priority_from(id: &str) -> Option<Option<Priority>> {
    Some(match id {
        "default" => None,
        "low" => Some(Priority::Low),
        "normal" => Some(Priority::Normal),
        "high" => Some(Priority::High),
        _ => return None,
    })
}

fn tracker_name(id: &str, profiles: &[ProfileRecord]) -> String {
    if id == PUBLIC {
        return "Public".into();
    }
    profiles.iter().find(|x| x.id == id).map_or_else(|| id.to_string(), |x| x.name.clone())
}

/// "tracker-a, private · Movie/TV · ratio 1.0 or 7 days · ≥ 72 h · < 1 GB in 150 h".
fn rule_summary(r: &RuleRecord) -> String {
    let mut parts = Vec::new();
    if !r.tracker_ids.is_empty() {
        parts.push(r.tracker_ids.join(", "));
    }
    match r.private_match {
        PrivateMatch::Any => {}
        PrivateMatch::PrivateOnly => parts.push("private".into()),
        PrivateMatch::PublicOnly => parts.push("public".into()),
    }
    if !r.categories.is_empty() {
        parts.push(r.categories.iter().map(|c| category_label(*c)).collect::<Vec<_>>().join("/"));
    }
    if parts.is_empty() {
        parts.push("any torrent".into());
    }
    if let Some(g) = r.goal {
        parts.push(p::settings_goal_summary(g));
    }
    if let Some(m) = r.min_seed_secs.filter(|m| *m > 0) {
        parts.push(format!("≥ {} h", m / 3600));
    }
    if let Some(b) = r.idle_below_bytes.filter(|b| *b > 0) {
        parts.push(format!("< {}", p::settings_idle_summary(b, r.idle_hours)));
    }
    parts.join(" · ")
}

fn count(i: &RulesEditorInput, id: &str) -> Option<u32> {
    i.counts.iter().find(|c| c.rule_id == id).map(|c| c.count)
}

fn rules_list(i: &RulesEditorInput) -> (Vec<RulesListRow>, RulesListRow) {
    let s = &i.state;
    let rows = i
        .rules
        .rules
        .iter()
        .enumerate()
        .map(|(n, r)| RulesListRow {
            id: r.id.clone(),
            badge: Some((n + 1).to_string()),
            new: false,
            title: r.name.clone(),
            subtitle: rule_summary(r),
            count: count(i, &r.id),
            enabled: r.enabled,
            selected: s.rule.as_deref() == Some(r.id.as_str()) && saved_rule(s, &i.rules).is_some(),
            select: rules_cmd(RulesChange::SelectRule { id: Some(r.id.clone()) }),
        })
        .collect();
    let default_row = RulesListRow {
        id: "*".into(),
        badge: Some("·".into()),
        new: false,
        title: "Default".into(),
        subtitle: format!("everything else · {}", p::settings_goal_summary(i.rules.default_rule.goal)),
        count: count(i, "*"),
        enabled: true,
        selected: saved_rule(s, &i.rules).is_none(),
        select: rules_cmd(RulesChange::SelectRule { id: None }),
    };
    (rows, default_row)
}

/// Default, Unlimited, or a limit (with `allow_default`: a rule's, which may follow the Default rule's).
fn limit_controls(limit: Option<u64>, allow_default: bool) -> Vec<SettingsControl> {
    let mode = match limit {
        None if allow_default => "default",
        None | Some(0) => "unlimited",
        Some(_) => "limit",
    };
    let mut options = Vec::new();
    if allow_default {
        options.push(opt("default", "Default"));
    }
    options.push(opt("unlimited", "Unlimited"));
    options.push(opt("limit", "Limit"));
    let mut out = vec![choice(SettingKey::RuleLimitMode, SettingsChoiceStyle::Menu, options, mode)];
    if let Some(b) = limit.filter(|b| *b > 0) {
        out.push(rate(SettingKey::RuleLimit, b, 72));
    }
    out
}

fn min_seed_controls(secs: Option<u64>) -> Vec<SettingsControl> {
    let toggle: SettingsControl = ToggleSpec {
        checkbox: true,
        help: "Keep seeding for a minimum time before the goal can stop it".into(),
        ..switch_spec(SettingKey::RuleMinSeed, secs.is_some())
    }.into();
    match secs {
        Some(s) => vec![toggle, duration(SettingKey::RuleMinSeedTime, s)],
        None => vec![toggle, quiet_text("None")],
    }
}

const LOCATION_HINT: &str = "A folder. Empty = the Finished location (or the content type's).";

fn rule_editor(i: &RulesEditorInput, r: &RuleRecord) -> Vec<SettingsGroup> {
    let trackers = i
        .profiles
        .iter()
        .map(|x| SettingsCheck { id: x.id.clone(), title: x.name.clone(), checked: r.tracker_ids.contains(&x.id) })
        .chain(std::iter::once(SettingsCheck { id: PUBLIC.into(), title: "Public (no tracker profile)".into(), checked: r.tracker_ids.iter().any(|t| t == PUBLIC) }))
        .collect();
    let tracker_title = if r.tracker_ids.is_empty() { "Any".into() } else { r.tracker_ids.iter().map(|t| tracker_name(t, &i.profiles)).collect::<Vec<_>>().join(", ") };
    let mut chips = vec![SettingsCheck { id: "any".into(), title: "Any".into(), checked: r.categories.is_empty() }];
    chips.extend(CATEGORIES.iter().map(|&c| SettingsCheck { id: category_id(c).into(), title: category_label(c), checked: r.categories.contains(&c) }));
    let matching = group(
        "rule.match",
        "Match",
        vec![
            row("rule.name", "Name", None, vec![SettingsControl::Field {
                key: SettingKey::RuleName,
                kind: SettingsFieldKind::Text,
                text: r.name.clone(),
                placeholder: String::new(),
                units: Vec::new(),
                unit: String::new(),
                width: 250,
                trailing: false,
                live: true,
                enabled: true,
                help: None,
                identifier: None,
            }]),
            row("rule.trackers", "Tracker", Some("Torrents of these trackers. None ticked = any."), vec![SettingsControl::Checklist {
                key: SettingKey::RuleTrackers,
                title: tracker_title,
                checks: trackers,
            }]),
            row("rule.types", "Content type", None, vec![SettingsControl::Chips {
                key: SettingKey::RuleCategories,
                checks: chips,
                help_on: "Stop matching {}".into(),
                help_off: "Match {}".into(),
            }]),
            row("rule.private", "Private torrents", None, vec![choice(
                SettingKey::RulePrivate,
                SettingsChoiceStyle::Menu,
                vec![opt("any", "Any"), opt("private", "Private only"), opt("public", "Public only")],
                private_id(r.private_match),
            )]),
            row("rule.enabled", "Enabled", None, vec![ToggleSpec { help: "Turn this rule on or off".into(), ..switch_spec(SettingKey::RuleEnabled, r.enabled) }.into()]),
        ],
    );
    let mut goal = vec![choice(
        SettingKey::RuleGoalMode,
        SettingsChoiceStyle::Menu,
        vec![opt("default", "Same as Default"), opt("custom", "Custom")],
        if r.goal.is_some() { "custom" } else { "default" },
    )];
    if let Some(g) = &r.goal {
        goal.extend(goal_controls(SettingKey::RuleGoal, SettingKey::RuleGoalRatio, SettingKey::RuleGoalTime, g));
    }
    let idle_mode = match r.idle_below_bytes {
        None => "settings",
        Some(0) => "off",
        Some(_) => "custom",
    };
    let mut idle = vec![choice(
        SettingKey::RuleIdleMode,
        SettingsChoiceStyle::Menu,
        vec![opt("settings", "Same as Settings"), opt("off", "Off"), opt("custom", "Custom")],
        idle_mode,
    )];
    if let Some(b) = r.idle_below_bytes.filter(|b| *b > 0) {
        idle.extend(idle_controls(Some("Less than"), SettingKey::RuleIdleSize, SettingKey::RuleIdleHours, b, r.idle_hours));
    }
    let finish = group(
        "rule.finish",
        "When the download finishes",
        vec![
            row("rule.goal", "Goal", Some("Same as Default = this rule changes nothing here"), goal),
            row("rule.min-seed", "Minimum seed time", Some("Never stops earlier, even if the ratio is met. Hit-and-run protection."), min_seed_controls(r.min_seed_secs)),
            row("rule.idle", "Inactive seeds", Some("Stops a seed that uploads less than this in the last hours of seeding. Never before the minimum seed time."), idle),
            row("rule.on-goal", "At goal", None, vec![choice(
                SettingKey::RuleOnGoal,
                SettingsChoiceStyle::Menu,
                vec![opt("default", "Same as Default"), opt("stop-keep", "Stop, keep in list"), opt("remove-keep-files", "Remove, keep files")],
                r.on_goal.map_or("default", on_goal_id),
            )]),
        ],
    );
    let place = group(
        "rule.place",
        "Priority & location",
        vec![
            row("rule.priority", "Priority", Some("Jumps the queue, gets bandwidth and upload slots first."), vec![choice(
                SettingKey::RulePriority,
                SettingsChoiceStyle::Segmented,
                vec![opt("default", "Default"), opt("low", "Low"), opt("normal", "Normal"), opt("high", "★ High")],
                priority_id(r.priority),
            )]),
            row("rule.location", "Files go to", Some(LOCATION_HINT), vec![folder(SettingKey::RuleLocation, r.finished_location.as_deref().unwrap_or(""), "Default", 250, true)]),
            row("rule.limit", "Upload limit", None, limit_controls(r.up_limit, true)),
        ],
    );
    vec![matching, finish, place]
}

fn default_editor(d: &DefaultRuleRecord) -> Vec<SettingsGroup> {
    vec![
        group(
            "default.applies",
            "Default rule",
            vec![row("default.applies", "Applies to", Some("Every torrent no other rule matches. Cannot be deleted or moved."), vec![text("Everything else")])],
        ),
        group(
            "default.finish",
            "When the download finishes",
            vec![
                row("default.goal", "Goal", None, goal_controls(SettingKey::RuleGoal, SettingKey::RuleGoalRatio, SettingKey::RuleGoalTime, &d.goal)),
                row("default.min-seed", "Minimum seed time", Some("Hit-and-run protection."), min_seed_controls(d.min_seed_secs)),
                row("default.on-goal", "At goal", None, vec![choice(
                    SettingKey::RuleOnGoal,
                    SettingsChoiceStyle::Menu,
                    vec![opt("stop-keep", "Stop, keep in list"), opt("remove-keep-files", "Remove, keep files")],
                    on_goal_id(d.on_goal),
                )]),
            ],
        ),
        group(
            "default.place",
            "Priority & location",
            vec![
                row("default.priority", "Priority", None, vec![choice(
                    SettingKey::RulePriority,
                    SettingsChoiceStyle::Segmented,
                    vec![opt("low", "Low"), opt("normal", "Normal"), opt("high", "★ High")],
                    priority_id(Some(d.priority)),
                )]),
                row("default.location", "Files go to", Some(LOCATION_HINT), vec![folder(SettingKey::RuleLocation, d.finished_location.as_deref().unwrap_or(""), "Default", 250, true)]),
                row("default.limit", "Upload limit", None, limit_controls(d.up_limit, false)),
            ],
        ),
    ]
}

fn plain(t: impl Into<String>) -> SettingsTextRun {
    SettingsTextRun { text: t.into(), bold: false }
}

fn bold(t: impl Into<String>) -> SettingsTextRun {
    SettingsTextRun { text: t.into(), bold: true }
}

fn rules_footer(i: &RulesEditorInput, is_default: bool, dirty: bool) -> RulesFooter {
    let pv = &i.preview;
    let runs = if !i.problems.is_empty() {
        vec![plain(i.problems.join(" "))]
    } else if is_default {
        let mut r = vec![plain("Applies to "), bold(format!("{} torrents", pv.affected)), plain(".")];
        if pv.changed > 0 {
            r.push(plain(format!(" Saving changes {}.", pv.changed)));
        }
        r
    } else {
        let mut r = vec![plain("Applies to "), bold(format!("{} torrent{}", pv.affected, if pv.affected == 1 { "" } else { "s" })), plain(".")];
        if pv.changed > 0 {
            r.push(plain(" Saving changes "));
            r.push(bold(pv.changed.to_string()));
            if !pv.change_note.is_empty() {
                r.push(plain(format!(": {}", pv.change_note)));
            }
            r.push(plain("."));
        }
        r
    };
    RulesFooter {
        runs,
        problem: !i.problems.is_empty(),
        revert: styled_button("Revert", "Discard unsaved changes to this rule", SettingsButtonStyle::Normal, dirty, rules_cmd(RulesChange::Revert)),
        save: styled_button("Save Rule", "Save this rule", SettingsButtonStyle::Prominent, dirty, rules_cmd(RulesChange::Save)),
    }
}

fn rules_model(i: &RulesEditorInput, sections: Vec<SettingsOption>) -> RulesEditorModel {
    let s = &i.state;
    let (rows, default_row) = rules_list(i);
    let is_default = saved_rule(s, &i.rules).is_none();
    let groups = match shown_rule(s, &i.rules) {
        Some(r) => rule_editor(i, &r),
        None => default_editor(&shown_default(s, &i.rules)),
    };
    RulesEditorModel {
        sections,
        selected: "rules".into(),
        note: "Checked top to bottom. The first match wins.".into(),
        rows,
        reorderable: true,
        default_row: Some(default_row),
        empty: None,
        list_buttons: SettingsListButtons {
            add: styled_button("Add a rule", "Add a rule", SettingsButtonStyle::Normal, true, rules_cmd(RulesChange::AddRule)),
            remove: styled_button("Delete the rule", "Delete the rule", SettingsButtonStyle::Normal, !is_default, rules_cmd(RulesChange::RemoveRule)),
        },
        groups,
        folders_after: None,
        folders_profile: None,
        folders_title: String::new(),
        placeholder: None,
        footer: Some(rules_footer(i, is_default, rule_dirty(s, &i.rules))),
    }
}

fn hidden_note(t: &ProfileRecord) -> &'static str {
    match (t.hidden, t.hide_only_seeding) {
        (true, true) => "Finished torrents of this tracker are left out of Sharing and the menu bar; downloads show as usual.",
        (true, false) => "Its torrents are left out of Downloads, Sharing and the menu bar, and the tracker is not in the sidebar.",
        (false, _) => "Hide a tracker to keep its torrents out of the lists and the menu bar. Nothing else changes.",
    }
}

fn wide(id: &str, label: &str, key: SettingKey, on: bool) -> SettingsRow {
    SettingsRow {
        id: id.into(),
        label: label.into(),
        hint: None,
        style: SettingsRowStyle::Wide,
        quiet: false,
        controls: vec![ToggleSpec { help: format!("{}: {label}", if on { "On" } else { "Off" }), ..switch_spec(key, on) }.into()],
    }
}

fn note(id: &str, t: &str) -> SettingsRow {
    SettingsRow { id: id.into(), label: t.into(), hint: None, style: SettingsRowStyle::Note, quiet: false, controls: Vec::new() }
}

fn tracker_question(t: &ProfileRecord) -> SettingsQuestion {
    let name = if t.name.is_empty() { t.id.clone() } else { t.name.clone() };
    SettingsQuestion {
        title: format!("Delete “{name}”?"),
        text: "Its torrents move to Public or Other private. Statistics history is kept.".into(),
        choices: vec![SettingsQuestionChoice { title: "Delete".into(), command: rules_cmd(RulesChange::RemoveTracker) }],
        cancel_title: "Cancel".into(),
    }
}

fn tracker_editor(i: &RulesEditorInput, t: &ProfileRecord) -> Vec<SettingsGroup> {
    let s = &i.state;
    let mut rules = vec![opt("", "By the rules' order")];
    rules.extend(i.rules.rules.iter().map(|r| opt(&r.id, &r.name)));
    let tracker = group(
        "tracker.tracker",
        "Tracker",
        vec![
            row("tracker.name", "Name", None, vec![SettingsControl::Field {
                key: SettingKey::TrackerName,
                kind: SettingsFieldKind::Text,
                text: t.name.clone(),
                placeholder: String::new(),
                units: Vec::new(),
                unit: String::new(),
                width: 260,
                trailing: false,
                live: true,
                enabled: true,
                help: None,
                identifier: None,
            }]),
            row("tracker.rule", "Rule", Some("Seeding behaviour for this tracker's torrents."), vec![choice(
                SettingKey::TrackerRule,
                SettingsChoiceStyle::Menu,
                rules,
                t.rule.as_deref().unwrap_or(""),
            )]),
        ],
    );
    let mut hidden = vec![wide("tracker.hidden", "Hide this tracker", SettingKey::TrackerHidden, t.hidden)];
    if t.hidden {
        hidden.push(wide("tracker.only-seeding", "Hide only what is sharing", SettingKey::TrackerHideOnlySeeding, t.hide_only_seeding));
    }
    hidden.push(note("tracker.hidden-note", hidden_note(t)));
    if !s.new_tracker && saved_tracker(s, &i.profiles).is_some() {
        hidden.push(SettingsRow {
            id: "tracker.show".into(),
            label: String::new(),
            hint: None,
            style: SettingsRowStyle::Wide,
            quiet: false,
            controls: vec![SettingsControl::Button {
                action: styled_button("Show Torrents", "Open this tracker's tab with all of its torrents", SettingsButtonStyle::Normal, true, SettingsCommand::ShowTrackerTorrents {
                    tracker_id: t.id.clone(),
                }),
            }],
        });
    }
    let mut addresses: Vec<SettingsRow> = t
        .urls
        .iter()
        .enumerate()
        .map(|(n, u)| SettingsRow {
            id: format!("tracker.address.{n}"),
            label: String::new(),
            hint: None,
            style: SettingsRowStyle::Wide,
            quiet: false,
            controls: vec![
                SettingsControl::Field {
                    key: SettingKey::TrackerAddress { index: n as u32 },
                    kind: SettingsFieldKind::Address,
                    text: u.clone(),
                    placeholder: "https://tracker.example.org/announce".into(),
                    units: Vec::new(),
                    unit: String::new(),
                    width: 0,
                    trailing: false,
                    live: true,
                    enabled: true,
                    help: None,
                    identifier: None,
                },
                SettingsControl::Button {
                    action: styled_button("Remove this address", "Remove this address", SettingsButtonStyle::Remove, t.urls.len() > 1, rules_cmd(RulesChange::RemoveAddress {
                        index: n as u32,
                    })),
                },
            ],
        })
        .collect();
    addresses.push(SettingsRow {
        id: "tracker.add-address".into(),
        label: String::new(),
        hint: Some("A host, *.host, or a full URL. Passkeys are dropped.".into()),
        style: SettingsRowStyle::Wide,
        quiet: false,
        controls: vec![SettingsControl::Button {
            action: styled_button("Add Address", "Add another announce address for this tracker", SettingsButtonStyle::Add, true, rules_cmd(RulesChange::AddAddress)),
        }],
    });
    vec![tracker, group("tracker.hidden", "Hidden", hidden), group("tracker.addresses", "Announce addresses", addresses)]
}

fn trackers_model(i: &RulesEditorInput, sections: Vec<SettingsOption>) -> RulesEditorModel {
    let s = &i.state;
    let selected = saved_tracker(s, &i.profiles).map(|x| x.id.clone());
    let mut rows: Vec<RulesListRow> = i
        .profiles
        .iter()
        .map(|x| RulesListRow {
            id: x.id.clone(),
            badge: None,
            new: false,
            title: x.name.clone(),
            subtitle: x.urls.first().cloned().unwrap_or_default(),
            count: None,
            enabled: true,
            selected: selected.as_deref() == Some(x.id.as_str()),
            select: rules_cmd(RulesChange::SelectTracker { id: x.id.clone() }),
        })
        .collect();
    if s.new_tracker {
        rows.push(RulesListRow {
            id: String::new(),
            badge: None,
            new: true,
            title: "New tracker".into(),
            subtitle: String::new(),
            count: None,
            enabled: true,
            selected: true,
            select: rules_cmd(RulesChange::NewTracker { host: None }),
        });
    }
    let shown = shown_tracker(s, &i.profiles);
    let dirty = tracker_dirty(s, &i.profiles);
    let footer = shown.as_ref().map(|t| RulesFooter {
        runs: vec![plain(if i.problems.is_empty() { "Passkeys in announce URLs are never stored or shown.".into() } else { i.problems.join(" ") })],
        problem: !i.problems.is_empty(),
        revert: styled_button("Revert", "Discard unsaved changes to this tracker", SettingsButtonStyle::Normal, dirty, rules_cmd(RulesChange::Revert)),
        save: styled_button(
            "Save Tracker",
            "Save this tracker profile",
            SettingsButtonStyle::Prominent,
            dirty && !t.name.trim().is_empty(),
            rules_cmd(RulesChange::Save),
        ),
    });
    let remove = match saved_tracker(s, &i.profiles) {
        Some(t) => SettingsCommand::Ask { question: tracker_question(t) },
        None => rules_cmd(RulesChange::RemoveTracker),
    };
    RulesEditorModel {
        sections,
        selected: "trackers".into(),
        note: "Torrents match a tracker by their announce addresses.".into(),
        rows,
        reorderable: false,
        default_row: None,
        empty: (i.profiles.is_empty() && !s.new_tracker).then(|| "No trackers yet".into()),
        list_buttons: SettingsListButtons {
            add: styled_button("Add a tracker", "Add a tracker", SettingsButtonStyle::Normal, true, rules_cmd(RulesChange::NewTracker { host: None })),
            remove: styled_button("Delete the tracker", "Delete the tracker", SettingsButtonStyle::Normal, selected.is_some(), remove),
        },
        groups: shown.as_ref().map(|t| tracker_editor(i, t)).unwrap_or_default(),
        folders_after: shown.as_ref().map(|_| "tracker.tracker".into()),
        folders_profile: shown.clone(),
        folders_title: "Download location".into(),
        placeholder: shown.is_none().then(|| {
            "Select a tracker, or add one with +. A tracker is a name and the announce addresses that belong to it. Torrents are matched by their announce URLs; passkeys are never stored."
                .into()
        }),
        footer,
    }
}

/// The Rules tab for `input`.
#[uniffi::export]
pub fn rules_editor_model(input: RulesEditorInput) -> RulesEditorModel {
    let sections = vec![opt("rules", "Rules"), opt("trackers", "Trackers")];
    match input.state.section {
        RulesSection::Rules => rules_model(&input, sections),
        RulesSection::Trackers => trackers_model(&input, sections),
    }
}

// MARK: Changes

/// A rule's or the Default rule's field after `key` took `v`: `false` when refused.
fn edit_rule(r: &mut RuleRecord, key: &SettingKey, v: &SettingsValue, default_goal: &GoalKind) -> bool {
    match key {
        SettingKey::RuleName => match crate::settings_view_apply::text(v) {
            Some(t) => r.name = t.to_string(),
            None => return false,
        },
        SettingKey::RuleTrackers => match id(v) {
            Some(t) => {
                if let Some(n) = r.tracker_ids.iter().position(|x| x == t) {
                    r.tracker_ids.remove(n);
                } else {
                    r.tracker_ids.push(t.to_string());
                }
            }
            None => return false,
        },
        SettingKey::RuleCategories => match id(v) {
            Some("any") => r.categories.clear(),
            Some(c) => match category_from_id(c) {
                Some(c) => {
                    if let Some(n) = r.categories.iter().position(|x| *x == c) {
                        r.categories.remove(n);
                    } else {
                        r.categories.push(c);
                    }
                }
                None => return false,
            },
            None => return false,
        },
        SettingKey::RulePrivate => match id(v) {
            Some("any") => r.private_match = PrivateMatch::Any,
            Some("private") => r.private_match = PrivateMatch::PrivateOnly,
            Some("public") => r.private_match = PrivateMatch::PublicOnly,
            _ => return false,
        },
        SettingKey::RuleEnabled => match on(v) {
            Some(x) => r.enabled = x,
            None => return false,
        },
        SettingKey::RuleGoalMode => match id(v) {
            Some("default") => r.goal = None,
            Some("custom") => r.goal = Some(r.goal.unwrap_or(*default_goal)),
            _ => return false,
        },
        SettingKey::RuleGoal | SettingKey::RuleGoalRatio | SettingKey::RuleGoalTime => {
            match r.goal.as_ref().and_then(|g| goal_edit(g, key, v)) {
                Some(g) => r.goal = Some(g),
                None => return false,
            }
        }
        SettingKey::RuleMinSeed | SettingKey::RuleMinSeedTime => return edit_min_seed(&mut r.min_seed_secs, key, v),
        SettingKey::RuleIdleMode => match id(v) {
            Some("settings") => r.idle_below_bytes = None,
            Some("off") => r.idle_below_bytes = Some(0),
            Some("custom") => r.idle_below_bytes = Some(r.idle_below_bytes.filter(|b| *b > 0).unwrap_or(IDLE_DEFAULT)),
            _ => return false,
        },
        SettingKey::RuleIdleSize => {
            let current = r.idle_below_bytes.unwrap_or(0);
            match measure(v).and_then(|(t, u)| read_field(SettingsFieldKind::Size, t, u, current)) {
                Some(b) => r.idle_below_bytes = Some(b),
                None => return false,
            }
        }
        SettingKey::RuleIdleHours => match measure(v).and_then(|(t, u)| read_field(SettingsFieldKind::Hours, t, u, r.idle_hours.into())) {
            Some(h) => r.idle_hours = h as u32,
            None => return false,
        },
        SettingKey::RuleOnGoal => match id(v) {
            Some("default") => r.on_goal = None,
            Some(x) => match on_goal(x) {
                Some(a) => r.on_goal = Some(a),
                None => return false,
            },
            None => return false,
        },
        SettingKey::RulePriority => match id(v).and_then(priority_from) {
            Some(p) => r.priority = p,
            None => return false,
        },
        SettingKey::RuleLocation => match crate::settings_view_apply::text(v) {
            Some(t) => r.finished_location = (!t.is_empty()).then(|| t.to_string()),
            None => return false,
        },
        SettingKey::RuleLimitMode | SettingKey::RuleLimit => return edit_limit(&mut r.up_limit, key, v, true),
        _ => return false,
    }
    true
}

fn edit_min_seed(field: &mut Option<u64>, key: &SettingKey, v: &SettingsValue) -> bool {
    match key {
        SettingKey::RuleMinSeed => match on(v) {
            Some(true) => *field = Some(field.unwrap_or(MIN_SEED_DEFAULT)),
            Some(false) => *field = None,
            None => return false,
        },
        _ => match measure(v).and_then(|(t, u)| read_field(SettingsFieldKind::Duration, t, u, field.unwrap_or(MIN_SEED_DEFAULT))) {
            Some(s) => *field = Some(s),
            None => return false,
        },
    }
    true
}

fn edit_limit(field: &mut Option<u64>, key: &SettingKey, v: &SettingsValue, allow_default: bool) -> bool {
    match key {
        SettingKey::RuleLimitMode => match id(v) {
            Some("default") if allow_default => *field = None,
            Some("unlimited") => *field = if allow_default { Some(0) } else { None },
            Some("limit") => *field = Some(field.filter(|b| *b > 0).unwrap_or(LIMIT_DEFAULT)),
            _ => return false,
        },
        _ => match measure(v).and_then(|(t, u)| read_field(SettingsFieldKind::Rate, t, u, field.unwrap_or(0))) {
            // A limit of nothing is no limit: the mode says Unlimited.
            Some(b) => *field = if b == 0 { (allow_default).then_some(0) } else { Some(b) },
            None => return false,
        },
    }
    true
}

fn edit_default(d: &mut DefaultRuleRecord, key: &SettingKey, v: &SettingsValue) -> bool {
    match key {
        SettingKey::RuleGoal | SettingKey::RuleGoalRatio | SettingKey::RuleGoalTime => match goal_edit(&d.goal, key, v) {
            Some(g) => d.goal = g,
            None => return false,
        },
        SettingKey::RuleMinSeed | SettingKey::RuleMinSeedTime => return edit_min_seed(&mut d.min_seed_secs, key, v),
        SettingKey::RuleOnGoal => match id(v).and_then(on_goal) {
            Some(a) => d.on_goal = a,
            None => return false,
        },
        SettingKey::RulePriority => match id(v).and_then(priority_from) {
            Some(Some(p)) => d.priority = p,
            _ => return false,
        },
        SettingKey::RuleLocation => match crate::settings_view_apply::text(v) {
            Some(t) => d.finished_location = (!t.is_empty()).then(|| t.to_string()),
            None => return false,
        },
        SettingKey::RuleLimitMode | SettingKey::RuleLimit => return edit_limit(&mut d.up_limit, key, v, false),
        _ => return false,
    }
    true
}

fn edit_tracker(t: &mut ProfileRecord, key: &SettingKey, v: &SettingsValue) -> bool {
    match key {
        SettingKey::TrackerName => match crate::settings_view_apply::text(v) {
            Some(x) => t.name = x.to_string(),
            None => return false,
        },
        SettingKey::TrackerRule => match id(v) {
            Some(x) => t.rule = (!x.is_empty()).then(|| x.to_string()),
            None => return false,
        },
        SettingKey::TrackerHidden => match on(v) {
            Some(x) => t.hidden = x,
            None => return false,
        },
        SettingKey::TrackerHideOnlySeeding => match on(v) {
            Some(x) => t.hide_only_seeding = x,
            None => return false,
        },
        SettingKey::TrackerAddress { index } => match (crate::settings_view_apply::text(v), t.urls.get_mut(*index as usize)) {
            (Some(x), Some(u)) => *u = x.to_string(),
            _ => return false,
        },
        _ => return false,
    }
    true
}

fn applied(state: RulesEditorState) -> RulesApplied {
    RulesApplied { state, effect: None, clear_problems: false }
}

/// `change` applied to the tab (`input` as the model was built from): the new state, and a save to run.
#[uniffi::export]
pub fn rules_editor_apply(input: RulesEditorInput, change: RulesChange) -> RulesApplied {
    let RulesEditorInput { state: mut s, rules: doc, profiles, .. } = input;
    let fresh = |mut s: RulesEditorState| {
        s.rule_draft = None;
        s.default_draft = None;
        s.tracker_draft = None;
        RulesApplied { state: s, effect: None, clear_problems: true }
    };
    match change {
        RulesChange::Section { section } => {
            s.section = section;
            applied(s)
        }
        RulesChange::SelectRule { id } => {
            s.rule = id;
            fresh(s)
        }
        RulesChange::SelectTracker { id } | RulesChange::EditTracker { id } => {
            s.section = RulesSection::Trackers;
            s.tracker = Some(id);
            s.new_tracker = false;
            fresh(s)
        }
        RulesChange::Set { key, value } => {
            // A value that cannot be read leaves the draft as it is (the field shows the draft's value again).
            if s.section == RulesSection::Trackers {
                if let Some(mut t) = shown_tracker(&s, &profiles)
                    && edit_tracker(&mut t, &key, &value)
                {
                    s.tracker_draft = Some(t);
                }
            } else if let Some(mut r) = shown_rule(&s, &doc) {
                if edit_rule(&mut r, &key, &value, &doc.default_rule.goal) {
                    s.rule_draft = Some(r);
                }
            } else {
                let mut d = shown_default(&s, &doc);
                if edit_default(&mut d, &key, &value) {
                    s.default_draft = Some(d);
                }
            }
            applied(s)
        }
        RulesChange::TrackerFolders { profile } => {
            if let Some(mut t) = shown_tracker(&s, &profiles) {
                t.incomplete_dir = profile.incomplete_dir;
                t.finished_dir = profile.finished_dir;
                t.categories = profile.categories;
                s.tracker_draft = Some(t);
            }
            applied(s)
        }
        RulesChange::AddAddress | RulesChange::RemoveAddress { .. } => {
            if let Some(mut t) = shown_tracker(&s, &profiles) {
                match change {
                    RulesChange::RemoveAddress { index } if t.urls.len() > 1 && (index as usize) < t.urls.len() => {
                        t.urls.remove(index as usize);
                    }
                    RulesChange::AddAddress => t.urls.push(String::new()),
                    _ => {}
                }
                s.tracker_draft = Some(t);
            }
            applied(s)
        }
        RulesChange::Revert => {
            s.new_tracker = false;
            fresh(s)
        }
        RulesChange::Save => {
            if s.section == RulesSection::Trackers {
                let Some(t) = shown_tracker(&s, &profiles) else { return applied(s) };
                if t.name.trim().is_empty() {
                    return applied(s);
                }
                let mut list = profiles.clone();
                if s.new_tracker {
                    list.push(t);
                    s.after_save = Some(RulesAfterSave::LastTracker);
                } else {
                    if let Some(x) = list.iter_mut().find(|x| x.id == t.id) {
                        *x = t;
                    }
                    s.after_save = Some(RulesAfterSave::Keep);
                }
                RulesApplied { state: s, effect: Some(RulesEffect::SaveProfiles { profiles: list }), clear_problems: false }
            } else {
                let q = rules_editor_preview_query(s.clone(), doc);
                s.after_save = Some(RulesAfterSave::Keep);
                RulesApplied { state: s, effect: Some(RulesEffect::SaveRules { rules: q.rules }), clear_problems: false }
            }
        }
        RulesChange::AddRule => {
            let mut d = doc;
            d.rules.push(RuleRecord { name: "New rule".into(), ..settings_blank_rule() });
            s.after_save = Some(RulesAfterSave::LastRule);
            RulesApplied { state: s, effect: Some(RulesEffect::SaveRules { rules: d }), clear_problems: false }
        }
        RulesChange::RemoveRule => {
            let Some(id) = saved_rule(&s, &doc).map(|r| r.id.clone()) else { return applied(s) };
            let mut d = doc;
            d.rules.retain(|r| r.id != id);
            s.after_save = Some(RulesAfterSave::DefaultRule);
            RulesApplied { state: s, effect: Some(RulesEffect::SaveRules { rules: d }), clear_problems: false }
        }
        RulesChange::MoveRule { from, to } => {
            let (from, to) = (from as usize, to as usize);
            if from >= doc.rules.len() || to > doc.rules.len() || to == from || to == from + 1 {
                return applied(s);
            }
            let mut d = doc;
            let r = d.rules.remove(from);
            d.rules.insert(if to > from { to - 1 } else { to }, r);
            s.after_save = Some(RulesAfterSave::Keep);
            RulesApplied { state: s, effect: Some(RulesEffect::SaveRules { rules: d }), clear_problems: false }
        }
        RulesChange::NewTracker { host } => {
            s.section = RulesSection::Trackers;
            s.new_tracker = true;
            s.tracker = None;
            let host = host.unwrap_or_default();
            s.tracker_draft = Some(ProfileRecord {
                name: host.clone(),
                urls: vec![host],
                ..settings_blank_profile()
            });
            RulesApplied { state: s, effect: None, clear_problems: true }
        }
        RulesChange::RemoveTracker => {
            let Some(id) = saved_tracker(&s, &profiles).map(|t| t.id.clone()) else { return applied(s) };
            s.after_save = Some(RulesAfterSave::FirstTracker);
            let list = profiles.into_iter().filter(|t| t.id != id).collect();
            RulesApplied { state: s, effect: Some(RulesEffect::SaveProfiles { profiles: list }), clear_problems: false }
        }
        RulesChange::RulesSaved { rules } => {
            match s.after_save.take() {
                Some(RulesAfterSave::LastRule) => s.rule = rules.rules.last().map(|r| r.id.clone()),
                Some(RulesAfterSave::DefaultRule) => s.rule = None,
                _ => {}
            }
            fresh(s)
        }
        RulesChange::ProfilesSaved { profiles } => {
            match s.after_save.take() {
                Some(RulesAfterSave::LastTracker) => s.tracker = profiles.last().map(|t| t.id.clone()),
                Some(RulesAfterSave::FirstTracker) => s.tracker = profiles.first().map(|t| t.id.clone()),
                _ => {}
            }
            s.new_tracker = false;
            fresh(s)
        }
    }
}

/// A new tracker profile as the editor starts it: no name, one empty address, nothing of its own.
#[uniffi::export]
pub fn settings_blank_profile() -> ProfileRecord {
    ProfileRecord {
        id: String::new(),
        name: String::new(),
        urls: vec![String::new()],
        rule: None,
        color: None,
        icon: None,
        down_limit: None,
        up_limit: None,
        incomplete_dir: None,
        finished_dir: None,
        categories: Vec::new(),
        hidden: false,
        hide_only_seeding: false,
        allow_pex: false,
    }
}

#[cfg(test)]
#[path = "settings_rules_tests.rs"]
mod tests;
