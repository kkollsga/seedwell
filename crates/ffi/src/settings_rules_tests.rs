use super::*;
use crate::settings::{OnGoalAction, SettingsStore};
use crate::types::Category;

fn rule(id: &str, name: &str) -> RuleRecord {
    RuleRecord { id: id.into(), name: name.into(), ..settings_blank_rule() }
}

fn profile(id: &str, name: &str) -> ProfileRecord {
    ProfileRecord { id: id.into(), name: name.into(), urls: vec![format!("{id}.example.org")], ..settings_blank_profile() }
}

fn input() -> RulesEditorInput {
    let dir = tempfile::tempdir().unwrap();
    let mut rules = SettingsStore::new(dir.path()).load_rules();
    rules.rules = vec![
        RuleRecord { tracker_ids: vec!["tz".into()], private_match: PrivateMatch::PrivateOnly, ..rule("movies", "Movies") },
        RuleRecord { enabled: false, categories: vec![Category::Tv, Category::Movie], goal: Some(GoalKind::Time { secs: 3 * 86_400 }), ..rule("tv", "TV") },
    ];
    RulesEditorInput {
        state: rules_editor_state(),
        rules,
        profiles: vec![profile("tz", "Tracker Z"), profile("ty", "Tracker Y")],
        preview: RulePreview { affected: 218, changed: 0, change_note: String::new() },
        counts: vec![RuleCount { rule_id: "movies".into(), count: 12 }, RuleCount { rule_id: "*".into(), count: 218 }],
        problems: Vec::new(),
    }
}

fn set(key: SettingKey, value: SettingsValue) -> RulesChange {
    RulesChange::Set { key, value }
}

fn pick(id: &str) -> SettingsValue {
    SettingsValue::Choice { id: id.into() }
}

fn with(mut i: RulesEditorInput, change: RulesChange) -> (RulesEditorInput, RulesApplied) {
    let a = rules_editor_apply(i.clone(), change);
    i.state = a.state.clone();
    (i, a)
}

fn runs(f: &RulesFooter) -> String {
    f.runs.iter().map(|r| if r.bold { format!("**{}**", r.text) } else { r.text.clone() }).collect()
}

fn find<'a>(m: &'a RulesEditorModel, id: &str) -> &'a SettingsRow {
    m.groups.iter().flat_map(|g| g.rows.iter()).find(|r| r.id == id).unwrap_or_else(|| panic!("no row {id}"))
}

#[test]
fn the_rules_list_numbers_the_rules_and_keeps_the_default_apart() {
    let m = rules_editor_model(input());
    assert_eq!(m.sections.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["Rules", "Trackers"]);
    assert_eq!(m.note, "Checked top to bottom. The first match wins.");
    assert!(m.reorderable);
    let r = &m.rows;
    assert_eq!((r[0].badge.as_deref(), r[0].title.as_str(), r[0].count), (Some("1"), "Movies", Some(12)));
    assert_eq!(r[0].subtitle, "tz · private");
    assert_eq!(r[1].subtitle, "TV/Movie · 3 days");
    assert!(!r[1].enabled);
    let d = m.default_row.unwrap();
    assert!(d.selected, "the Default rule is shown first");
    assert_eq!((d.badge.as_deref(), d.count), (Some("·"), Some(218)));
    assert!(d.subtitle.starts_with("everything else · "));
    assert!(!m.list_buttons.remove.enabled, "the Default rule cannot be deleted");
    assert_eq!(m.groups.iter().map(|g| g.title.as_str()).collect::<Vec<_>>(), ["Default rule", "When the download finishes", "Priority & location"]);
    let f = m.footer.unwrap();
    assert_eq!(runs(&f), "Applies to **218 torrents**.");
    assert!(!f.save.enabled && !f.revert.enabled, "nothing to save");
}

#[test]
fn a_rules_editor_shows_its_match_finish_and_place() {
    let (i, _) = with(input(), RulesChange::SelectRule { id: Some("movies".into()) });
    let m = rules_editor_model(i);
    assert!(m.rows[0].selected);
    assert_eq!(m.groups.iter().map(|g| g.title.as_str()).collect::<Vec<_>>(), ["Match", "When the download finishes", "Priority & location"]);
    let SettingsControl::Checklist { title, checks, .. } = &find(&m, "rule.trackers").controls[0] else { panic!() };
    assert_eq!(title, "Tracker Z");
    assert_eq!(checks.iter().map(|c| (c.title.as_str(), c.checked)).collect::<Vec<_>>(), [
        ("Tracker Z", true),
        ("Tracker Y", false),
        ("Public (no tracker profile)", false)
    ]);
    let SettingsControl::Chips { checks, .. } = &find(&m, "rule.types").controls[0] else { panic!() };
    assert!(checks[0].checked, "Any while no type is ticked");
    assert_eq!(find(&m, "rule.goal").controls.len(), 1, "Same as Default: no goal fields");
    let SettingsControl::Toggle { checkbox, on, .. } = &find(&m, "rule.min-seed").controls[0] else { panic!() };
    assert!(*checkbox && !*on);
    let SettingsControl::Choice { options, selected, .. } = &find(&m, "rule.priority").controls[0] else { panic!() };
    assert_eq!((options.len(), selected.as_str()), (4, "default"));
    let f = m.footer.unwrap();
    assert_eq!(f.save.title, "Save Rule");
    assert_eq!(runs(&f), "Applies to **218 torrents**.");
}

#[test]
fn an_edit_is_a_draft_until_saved() {
    let (i, _) = with(input(), RulesChange::SelectRule { id: Some("movies".into()) });
    let (i, _) = with(i, set(SettingKey::RuleGoalMode, pick("custom")));
    let (i, _) = with(i, set(SettingKey::RuleCategories, pick("music")));
    let (i, _) = with(i, set(SettingKey::RuleTrackers, pick("tz")));
    let (i, _) = with(i, set(SettingKey::RuleMinSeed, SettingsValue::Flag { on: true }));
    let (i, _) = with(i, set(SettingKey::RuleLimitMode, pick("limit")));
    let draft = i.state.rule_draft.clone().unwrap();
    assert_eq!(draft.goal, Some(i.rules.default_rule.goal), "Custom starts from the Default rule's goal");
    assert_eq!(draft.categories, [Category::Music]);
    assert!(draft.tracker_ids.is_empty(), "unticked");
    assert_eq!((draft.min_seed_secs, draft.up_limit), (Some(72 * 3600), Some(1_000_000)));
    assert_eq!(i.rules.rules[0].goal, None, "nothing is saved yet");
    let mut i = i;
    i.preview = RulePreview { affected: 1, changed: 3, change_note: "seed until ratio 2.0".into() };
    let m = rules_editor_model(i.clone());
    let f = m.footer.clone().unwrap();
    assert!(f.save.enabled && f.revert.enabled);
    assert_eq!(runs(&f), "Applies to **1 torrent**. Saving changes **3**: seed until ratio 2.0.");
    assert_eq!(find(&m, "rule.limit").controls.len(), 2, "a limit has its field");
    let q = rules_editor_preview_query(i.state.clone(), i.rules.clone());
    assert_eq!((q.rule_id.as_str(), &q.rules.rules[0]), ("movies", &draft), "the preview is of the draft");

    let (i2, a) = with(i.clone(), RulesChange::Save);
    let Some(RulesEffect::SaveRules { rules }) = a.effect else { panic!() };
    assert_eq!(rules.rules[0], draft);
    let (i2, a) = with(i2, RulesChange::RulesSaved { rules });
    assert!(a.clear_problems && i2.state.rule_draft.is_none());
    assert_eq!(i2.state.rule.as_deref(), Some("movies"), "the rule stays shown");

    let (r, _) = with(i, RulesChange::Revert);
    assert!(r.state.rule_draft.is_none(), "Revert drops the draft");
}

#[test]
fn values_that_cannot_be_read_leave_the_draft_alone() {
    let (i, _) = with(input(), RulesChange::SelectRule { id: Some("movies".into()) });
    let (i, _) = with(i, set(SettingKey::RuleIdleMode, pick("custom")));
    assert_eq!(i.state.rule_draft.as_ref().unwrap().idle_below_bytes, Some(1_000_000_000));
    let (i, _) = with(i, set(SettingKey::RuleIdleSize, SettingsValue::Measure { text: "lots".into(), unit: "GB".into() }));
    assert_eq!(i.state.rule_draft.as_ref().unwrap().idle_below_bytes, Some(1_000_000_000));
    let (i, _) = with(i, set(SettingKey::RuleIdleHours, SettingsValue::Typed { text: "72".into() }));
    assert_eq!(i.state.rule_draft.as_ref().unwrap().idle_hours, 72);
    let (i, _) = with(i, set(SettingKey::RulePriority, pick("urgent")));
    assert_eq!(i.state.rule_draft.as_ref().unwrap().priority, None);
    let m = rules_editor_model(i);
    let idle = find(&m, "rule.idle");
    assert!(idle.controls.iter().any(|c| matches!(c, SettingsControl::Label { text, .. } if text == "Less than")));
}

#[test]
fn the_default_rules_edits_keep_its_fields_whole() {
    let i = input();
    let (i, _) = with(i, set(SettingKey::RuleGoal, pick("forever")));
    let (i, _) = with(i, set(SettingKey::RulePriority, pick("high")));
    let (i, _) = with(i, set(SettingKey::RuleLimitMode, pick("unlimited")));
    let (i, _) = with(i, set(SettingKey::RuleOnGoal, pick("remove-keep-files")));
    let d = i.state.default_draft.clone().unwrap();
    assert_eq!((d.goal, d.priority, d.up_limit, d.on_goal), (GoalKind::Forever, Priority::High, None, OnGoalAction::RemoveKeepFiles));
    let (i, _) = with(i, set(SettingKey::RulePriority, pick("default")));
    assert_eq!(i.state.default_draft.as_ref().unwrap().priority, Priority::High, "the Default rule has a priority of its own");
    let m = rules_editor_model(i.clone());
    let SettingsControl::Choice { options, .. } = &find(&m, "default.limit").controls[0] else { panic!() };
    assert_eq!(options.iter().map(|o| o.title.as_str()).collect::<Vec<_>>(), ["Unlimited", "Limit"]);
    let mut i = i;
    i.preview.changed = 4;
    assert_eq!(runs(&rules_editor_model(i).footer.unwrap()), "Applies to **218 torrents**. Saving changes 4.");
}

#[test]
fn adding_removing_and_moving_rules_are_saves() {
    let (i, a) = with(input(), RulesChange::AddRule);
    let Some(RulesEffect::SaveRules { mut rules }) = a.effect else { panic!() };
    assert_eq!(rules.rules.last().unwrap().name, "New rule");
    rules.rules.last_mut().unwrap().id = "new-rule".into();
    let (i, _) = with(i, RulesChange::RulesSaved { rules: rules.clone() });
    assert_eq!(i.state.rule.as_deref(), Some("new-rule"), "the new rule is shown");

    let mut i = i;
    i.rules = rules;
    let (i, a) = with(i, RulesChange::RemoveRule);
    let Some(RulesEffect::SaveRules { rules }) = a.effect else { panic!() };
    assert_eq!(rules.rules.len(), 2);
    let (i, _) = with(i, RulesChange::RulesSaved { rules });
    assert_eq!(i.state.rule, None, "back to the Default rule");

    let a = rules_editor_apply(input(), RulesChange::MoveRule { from: 1, to: 0 });
    let Some(RulesEffect::SaveRules { rules }) = a.effect else { panic!() };
    assert_eq!(rules.rules.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["tv", "movies"]);
    let a = rules_editor_apply(input(), RulesChange::MoveRule { from: 0, to: 2 });
    let Some(RulesEffect::SaveRules { rules }) = a.effect else { panic!() };
    assert_eq!(rules.rules.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["tv", "movies"]);
    assert_eq!(rules_editor_apply(input(), RulesChange::MoveRule { from: 0, to: 1 }).effect, None, "where it is");
    assert_eq!(rules_editor_apply(input(), RulesChange::RemoveRule).effect, None, "not the Default rule");
}

#[test]
fn the_trackers_list_and_editor() {
    let (i, _) = with(input(), RulesChange::Section { section: RulesSection::Trackers });
    let m = rules_editor_model(i.clone());
    assert_eq!(m.selected, "trackers");
    assert_eq!(m.note, "Torrents match a tracker by their announce addresses.");
    assert!(m.rows[0].selected, "the first tracker is shown");
    assert_eq!(m.rows[0].subtitle, "tz.example.org");
    assert_eq!(m.groups.iter().map(|g| g.title.as_str()).collect::<Vec<_>>(), ["Tracker", "Hidden", "Announce addresses"]);
    assert_eq!(m.folders_after.as_deref(), Some("tracker.tracker"));
    assert_eq!(m.folders_title, "Download location");
    assert!(find(&m, "tracker.show").controls.len() == 1);
    let SettingsControl::Button { action } = &find(&m, "tracker.address.0").controls[1] else { panic!() };
    assert!(!action.enabled, "the last address stays");
    let SettingsCommand::Ask { question } = &m.list_buttons.remove.command else { panic!() };
    assert_eq!(question.title, "Delete “Tracker Z”?");
    assert_eq!(runs(&m.footer.unwrap()), "Passkeys in announce URLs are never stored or shown.");

    let (i, _) = with(i, set(SettingKey::TrackerHidden, SettingsValue::Flag { on: true }));
    let (i, _) = with(i, RulesChange::AddAddress);
    let (i, _) = with(i, set(SettingKey::TrackerAddress { index: 1 }, SettingsValue::Typed { text: "*.tz.org".into() }));
    let m = rules_editor_model(i.clone());
    assert!(find(&m, "tracker.only-seeding").label == "Hide only what is sharing");
    assert!(find(&m, "tracker.hidden-note").label.starts_with("Its torrents are left out"));
    assert!(m.footer.unwrap().save.enabled);
    let (_, a) = with(i, RulesChange::Save);
    let Some(RulesEffect::SaveProfiles { profiles }) = a.effect else { panic!() };
    assert_eq!(profiles[0].urls, ["tz.example.org", "*.tz.org"]);
    assert!(profiles[0].hidden);
}

#[test]
fn a_new_tracker_needs_a_name_and_is_shown_once_saved() {
    let (i, a) = with(input(), RulesChange::NewTracker { host: Some("tracker.example.net".into()) });
    assert!(a.clear_problems);
    let m = rules_editor_model(i.clone());
    assert!(m.rows.last().unwrap().new);
    assert!(m.folders_profile.is_some());
    assert!(!m.groups.iter().flat_map(|g| g.rows.iter()).any(|r| r.id == "tracker.show"), "no torrents yet");
    let (i, _) = with(i, set(SettingKey::TrackerName, SettingsValue::Typed { text: "  ".into() }));
    assert!(!rules_editor_model(i.clone()).footer.unwrap().save.enabled, "a name first");
    assert_eq!(rules_editor_apply(i.clone(), RulesChange::Save).effect, None);
    let (i, _) = with(i, set(SettingKey::TrackerName, SettingsValue::Typed { text: "Net".into() }));
    let (i, a) = with(i, RulesChange::Save);
    let Some(RulesEffect::SaveProfiles { mut profiles }) = a.effect else { panic!() };
    assert_eq!(profiles.last().unwrap().urls, ["tracker.example.net"]);
    profiles.last_mut().unwrap().id = "net".into();
    let (i, _) = with(i, RulesChange::ProfilesSaved { profiles });
    assert_eq!((i.state.tracker.as_deref(), i.state.new_tracker), (Some("net"), false));
}

#[test]
fn removing_a_tracker_shows_the_first_one_after() {
    let (i, _) = with(input(), RulesChange::EditTracker { id: "ty".into() });
    assert_eq!(i.state.section, RulesSection::Trackers);
    let (i, a) = with(i, RulesChange::RemoveTracker);
    let Some(RulesEffect::SaveProfiles { profiles }) = a.effect else { panic!() };
    assert_eq!(profiles.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["tz"]);
    let (i, _) = with(i, RulesChange::ProfilesSaved { profiles });
    assert_eq!(i.state.tracker.as_deref(), Some("tz"));
    let mut none = input();
    none.profiles.clear();
    none.state.section = RulesSection::Trackers;
    let m = rules_editor_model(none);
    assert_eq!(m.empty.as_deref(), Some("No trackers yet"));
    assert!(m.placeholder.is_some() && m.footer.is_none() && m.groups.is_empty());
}

#[test]
fn the_folder_editor_hands_the_folders_back() {
    let (i, _) = with(input(), RulesChange::Section { section: RulesSection::Trackers });
    let mut p = i.profiles[0].clone();
    p.finished_dir = Some("~/Z".into());
    p.name = "ignored".into();
    let (i, _) = with(i, RulesChange::TrackerFolders { profile: p });
    let d = i.state.tracker_draft.unwrap();
    assert_eq!((d.finished_dir.as_deref(), d.name.as_str()), (Some("~/Z"), "Tracker Z"), "only the folders");
}
