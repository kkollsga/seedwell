//! Cross-document checks. Each TOML file is valid on its own; this finds dangling references
//! between them (a watch folder forcing a rule that was deleted, ...). Findings are warnings:
//! a dangling reference degrades to "not applied", it never blocks startup.

use crate::config::Config;
use crate::error::Warning;
use crate::rules::{PUBLIC_TRACKER, RuleSet};
use crate::trackers::TrackerProfiles;

/// Dangling references among the three documents.
pub fn cross_check(cfg: &Config, rules: &RuleSet, trackers: &TrackerProfiles) -> Vec<Warning> {
    let mut w = Vec::new();
    let mut warn = |path: String, message: String| w.push(Warning { path, message });
    for (i, f) in cfg.folders.watch.iter().enumerate() {
        if let Some(r) = &f.rule {
            match rules.get(r) {
                None => warn(format!("config.toml folders.watch[{i}].rule"), format!("rule `{r}` does not exist in rules.toml")),
                Some(rule) if !rule.enabled => {
                    warn(format!("config.toml folders.watch[{i}].rule"), format!("rule `{r}` is disabled"));
                }
                Some(_) => {}
            }
        }
    }
    for (i, p) in trackers.profiles.iter().enumerate() {
        if let Some(r) = &p.rule {
            match rules.get(r) {
                None => warn(format!("trackers.toml tracker[{i}].rule"), format!("rule `{r}` does not exist in rules.toml")),
                Some(rule) if !rule.enabled => {
                    warn(format!("trackers.toml tracker[{i}].rule"), format!("rule `{r}` is disabled"));
                }
                Some(_) => {}
            }
        }
    }
    for (i, r) in rules.rules.iter().enumerate() {
        for t in &r.when.tracker {
            if t != PUBLIC_TRACKER && trackers.get(t).is_none() {
                warn(
                    format!("rules.toml rule[{i}].match.tracker"),
                    format!("tracker profile `{t}` does not exist in trackers.toml; the rule never matches it"),
                );
            }
        }
    }
    w
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WatchFolder;
    use crate::rules::{Match, Rule};
    use crate::trackers::{Profile, UrlPattern};

    #[test]
    fn finds_every_dangling_reference() {
        let mut cfg = Config::default();
        cfg.folders.watch = vec![WatchFolder { rule: Some("gone".into()), ..WatchFolder::default() }];
        let mut rules = RuleSet::default();
        rules.add_rule(Rule { id: "r".into(), name: "r".into(), enabled: false, when: Match { tracker: vec!["nope".into(), "public".into()], ..Match::default() }, ..Rule::default() }, None).unwrap();
        let mut trackers = TrackerProfiles::default();
        trackers.add(Profile { id: "tl".into(), name: "TL".into(), urls: vec![UrlPattern::parse("t.org").unwrap()], rule: Some("r".into()), ..Profile::default() }).unwrap();
        let w = cross_check(&cfg, &rules, &trackers);
        let msgs: Vec<String> = w.iter().map(|x| x.to_string()).collect();
        assert_eq!(w.len(), 3, "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("`gone` does not exist")));
        assert!(msgs.iter().any(|m| m.contains("`r` is disabled")));
        assert!(msgs.iter().any(|m| m.contains("`nope` does not exist")));
        assert!(cross_check(&Config::default(), &RuleSet::default(), &TrackerProfiles::default()).is_empty());
    }
}
