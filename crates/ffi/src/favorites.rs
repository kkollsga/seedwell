//! Favorite trackers (SPEC §7E.1): the stored profiles. Every other tracker is identified
//! automatically from the torrents' announce URLs and is never stored. These calls are the
//! "Save as Favorite" step and the helpers of the inline editor in a favorite's tracker tab.

use crate::settings::{ProfileRecord, profile_to};
use crate::{Core, CoreError};
use trs_state::trackers::UrlPattern;

/// What the editor learns about a line the user typed or pasted.
#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct UrlCheck {
    /// The pattern that would be stored (passkey and query removed); `None` when it is invalid.
    pub pattern: Option<String>,
    /// The line carried a passkey (a query string or a secret-looking path segment) that was dropped.
    pub passkey_removed: bool,
    /// A short reason for the red hint under the field.
    pub error: Option<String>,
}

/// Validate one announce URL / host pattern line the way `trackers.toml` will store it.
pub fn check_url(raw: &str) -> UrlCheck {
    let raw = raw.trim();
    match UrlPattern::parse(raw) {
        Ok(p) => {
            let stored = p.as_str().to_owned();
            let removed = raw.contains('?') || (stored.contains('*') && !raw.contains('*'));
            UrlCheck { pattern: Some(stored), passkey_removed: removed, error: None }
        }
        Err(e) => UrlCheck {
            pattern: None,
            passkey_removed: false,
            error: Some(if raw.is_empty() { "Enter a tracker address".to_string() } else { format!("Not a tracker address ({e})") }),
        },
    }
}

#[uniffi::export]
impl Core {
    /// "Save as Favorite": a profile for an automatic tracker, named after it and holding the
    /// hosts its torrents announce to (hosts only, never a passkey). Saving twice returns the
    /// same favorite.
    pub fn favorite_add(&self, group_id: String) -> Result<ProfileRecord, CoreError> {
        let mut hosts = self.backend.tracker_hosts(&group_id);
        if hosts.is_empty() {
            hosts.push(group_id.clone());
        }
        let mut set = self.settings.load_profiles_set();
        let id = set
            .add_favorite("", &hosts)
            .map_err(|e| CoreError::Invalid { problems: e.iter().map(|f| f.to_string()).collect() })?;
        let records: Vec<ProfileRecord> = set.profiles.iter().map(profile_to).collect();
        let saved = self.profiles_save(records)?;
        saved
            .into_iter()
            .find(|p| p.id == id)
            .ok_or(CoreError::NotFound { id })
    }

    /// How many rules use a favorite (named in a rule's tracker list, or linked from the
    /// favorite itself); the remove prompt says so.
    pub fn favorite_rule_count(&self, id: String) -> u32 {
        let rules = self.settings.load_rules_set();
        let linked = self.settings.load_profiles_set().get(&id).and_then(|p| p.rule.clone());
        rules
            .rules
            .iter()
            .filter(|r| r.when.tracker.contains(&id) || linked.as_deref() == Some(r.id.as_str()))
            .count() as u32
    }

    /// The primary announce hosts of an automatic tracker's torrents (its Info page lists them).
    pub fn tracker_hosts(&self, group_id: String) -> Vec<String> {
        self.backend.tracker_hosts(&group_id)
    }

    /// Whether peer exchange is allowed for the private torrents of this tracker (a favorite's id, an
    /// automatic tracker's domain, or `other-private`). Off unless the user turned it on (SPEC §7B.4).
    pub fn tracker_pex_allowed(&self, tracker_id: String) -> bool {
        self.settings.tracker_pex(&tracker_id)
    }

    /// Allow or forbid peer exchange for this tracker's private torrents. Takes effect at once, also
    /// for torrents that are running.
    pub fn tracker_pex_set(&self, tracker_id: String, allow: bool) -> Result<(), CoreError> {
        self.settings.set_tracker_pex(&tracker_id, allow)?;
        self.backend.settings_saved();
        Ok(())
    }

    /// Validate one line of the inline URL editor.
    pub fn url_check(&self, raw: String) -> UrlCheck {
        check_url(&raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passkeys_are_dropped_and_reported() {
        let pk = "0123456789abcdef0123456789abcdef";
        let c = check_url(&format!("https://tracker.example.org/a/{pk}/announce?passkey=ZZ"));
        assert_eq!(c.pattern.as_deref(), Some("https://tracker.example.org/a/*/announce"));
        assert!(c.passkey_removed);
        let c = check_url("tracker.example.org");
        assert_eq!((c.pattern.as_deref(), c.passkey_removed), (Some("tracker.example.org"), false));
        let c = check_url("*.example.org");
        assert!(!c.passkey_removed, "an explicit wildcard is not a removed passkey");
        assert!(check_url("").error.is_some());
        assert!(check_url("a*b.org").pattern.is_none());
    }
}
