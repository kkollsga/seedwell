//! "On battery" (Settings ▸ General, SPEC §8.4): keep going, Limit Speed, or pause downloads. The host only
//! reports the power source ([`crate::Core::power_changed`]: at start and at every change); what that means
//! is decided here, and acted on when the power source or the setting changes, never in between (a download
//! the user resumes on battery keeps running).
//!
//! Both effects are the app's own, for this run only: the engine stores a torrent paused this way as running
//! (`Backend::pause_by_app`) and never writes this Limit Speed to `config.toml` (`set_alt_speed_by_app`). So a
//! quit on battery brings the downloads back running, and the host's report at the next start pauses them
//! again if the Mac is still on battery. Back on AC power only what the app still holds resumes: a download
//! the user paused, resumed or paused again since is the user's.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use trs_state::config::OnBattery;

use crate::backend::Backend;
use crate::settings::SettingsStore;
use crate::types::{AddRequestInfo, AgentActivity, AutoAdded, CoreListener, Snapshot, TransferRow, TransferState};

/// What to do after a report.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Plan {
    /// Switch the app's own Limit Speed on or off.
    pub limit: Option<bool>,
    pub pause: Vec<String>,
    pub resume: Vec<String>,
}

/// The rule's memory: what it was told last and what it paused.
#[derive(Debug, Default)]
pub(crate) struct Rule {
    last: Option<(bool, OnBattery)>,
    paused: Vec<String>,
}

/// The downloads "pause downloads" pauses: every one that runs or waits to run (downloading, waiting in the
/// queue or the start-up stagger, verifying, fetching its metadata), not one held for a choice or counting down.
fn running_downloads(s: &Snapshot) -> impl Iterator<Item = &TransferRow> {
    s.downloading.iter().chain(&s.hidden_downloading).chain(&s.queued).chain(&s.hidden_queued).filter(|r| {
        !r.awaiting_choice
            && r.pending.is_none()
            && matches!(r.state, TransferState::Downloading | TransferState::Queued | TransferState::Checking | TransferState::Metadata)
    })
}

impl Rule {
    /// The power source and the setting now; `downloads` is read only when downloads are to be paused.
    pub fn step(&mut self, on_battery: bool, mode: OnBattery, downloads: impl FnOnce() -> Vec<String>) -> Plan {
        let now = (on_battery, mode);
        let before = self.last.replace(now);
        if before == Some(now) {
            return Plan::default();
        }
        let wants = |(battery, mode): (bool, OnBattery)| (battery && mode == OnBattery::Limit, battery && mode == OnBattery::Pause);
        let (was_limit, was_pause) = before.map_or((false, false), wants);
        let (limit, pause) = wants(now);
        let mut plan = Plan { limit: (limit != was_limit).then_some(limit), ..Plan::default() };
        if pause && !was_pause {
            plan.pause = downloads();
            for id in &plan.pause {
                if !self.paused.contains(id) {
                    self.paused.push(id.clone());
                }
            }
        } else if !pause {
            plan.resume = std::mem::take(&mut self.paused);
        }
        plan
    }
}

/// The rule with what it acts through; shared by the core and the listener that hears of settings changes.
pub(crate) struct Battery {
    backend: Arc<dyn Backend>,
    dir: PathBuf,
    /// Held while a plan is carried out, so two reports never interleave.
    rule: Mutex<Rule>,
}

impl Battery {
    pub fn new(backend: Arc<dyn Backend>, dir: PathBuf) -> Self {
        Self { backend, dir, rule: Mutex::new(Rule::default()) }
    }

    /// The host's report of the power source.
    pub fn power_changed(&self, on_battery: bool) {
        self.apply(Some(on_battery));
    }

    /// `config.toml` changed (Settings, the CLI, an edit): the setting may have.
    pub fn settings_changed(&self) {
        self.apply(None);
    }

    fn apply(&self, on_battery: Option<bool>) {
        let mut rule = self.rule.lock().unwrap_or_else(|e| e.into_inner());
        let Some(on_battery) = on_battery.or(rule.last.map(|(b, _)| b)) else { return };
        let mode = SettingsStore::new(&self.dir).config().general.on_battery;
        let plan = rule.step(on_battery, mode, || running_downloads(&self.backend.snapshot()).map(|r| r.id.clone()).collect());
        if let Some(on) = plan.limit {
            self.backend.set_alt_speed_by_app(on);
        }
        if !plan.pause.is_empty() {
            self.backend.pause_by_app(&plan.pause);
        }
        if !plan.resume.is_empty() {
            self.backend.resume_by_app(&plan.resume);
        }
    }
}

/// The host's listener, with the rule told when `config.toml` changed (the setting may come from the CLI or
/// an edit of the file as well as from Settings).
pub(crate) struct Listener {
    pub inner: Arc<dyn CoreListener>,
    pub battery: Arc<Battery>,
}

impl CoreListener for Listener {
    fn on_transfer_finished(&self, row: TransferRow) {
        self.inner.on_transfer_finished(row);
    }
    fn on_error(&self, id: Option<String>, message: String) {
        self.inner.on_error(id, message);
    }
    fn on_goal_reached(&self, row: TransferRow) {
        self.inner.on_goal_reached(row);
    }
    fn on_needs_choice(&self, row: TransferRow) {
        self.inner.on_needs_choice(row);
    }
    fn on_auto_added(&self, added: AutoAdded) {
        self.inner.on_auto_added(added);
    }
    fn on_duplicate(&self, name: String, trackers_added: u32) {
        self.inner.on_duplicate(name, trackers_added);
    }
    fn on_add_request(&self, request: AddRequestInfo) {
        self.inner.on_add_request(request);
    }
    fn on_agent_activity(&self, activity: AgentActivity) {
        self.inner.on_agent_activity(activity);
    }
    fn on_settings_changed(&self, file: String, warnings: Vec<String>, restart_required: Vec<String>) {
        if file == "config.toml" {
            // Not on this thread: events come from inside the engine's runtime, where reading the
            // snapshot (a blocking call into that runtime) would panic.
            let battery = self.battery.clone();
            std::thread::spawn(move || battery.settings_changed());
        }
        self.inner.on_settings_changed(file, warnings, restart_required);
    }
    fn on_settings_rejected(&self, file: String, error: String) {
        self.inner.on_settings_rejected(file, error);
    }
    fn on_locations_changed(&self, torrents: u32, bytes: u64) {
        self.inner.on_locations_changed(torrents, bytes);
    }
    fn on_attention_changed(&self) {
        self.inner.on_attention_changed();
    }
    fn on_pending_changed(&self) {
        self.inner.on_pending_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use OnBattery::{KeepGoing, Limit, Pause};

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn pause_mode_pauses_on_battery_and_resumes_what_it_paused_on_ac() {
        let mut r = Rule::default();
        assert_eq!(r.step(false, Pause, || panic!("not read on AC")), Plan::default(), "a start on AC power does nothing");
        assert_eq!(r.step(true, Pause, || ids(&["a", "b"])), Plan { pause: ids(&["a", "b"]), ..Plan::default() });
        assert_eq!(r.step(true, Pause, || panic!("not read again")), Plan::default(), "the same report again does nothing");
        assert_eq!(r.step(false, Pause, Vec::new), Plan { resume: ids(&["a", "b"]), ..Plan::default() });
        assert_eq!(r.step(false, Pause, Vec::new), Plan::default());
    }

    #[test]
    fn a_start_on_battery_applies_the_setting() {
        // After a quit on battery the app's pause is gone (stored as running) and Limit Speed is off.
        assert_eq!(Rule::default().step(true, Pause, || ids(&["a"])), Plan { pause: ids(&["a"]), ..Plan::default() });
        assert_eq!(Rule::default().step(true, Limit, Vec::new), Plan { limit: Some(true), ..Plan::default() });
        assert_eq!(Rule::default().step(true, KeepGoing, Vec::new), Plan::default());
    }

    #[test]
    fn limit_mode_switches_the_apps_limit_on_battery_only() {
        let mut r = Rule::default();
        assert_eq!(r.step(false, Limit, Vec::new), Plan::default());
        assert_eq!(r.step(true, Limit, Vec::new), Plan { limit: Some(true), ..Plan::default() });
        assert_eq!(r.step(false, Limit, Vec::new), Plan { limit: Some(false), ..Plan::default() });
    }

    #[test]
    fn a_change_of_the_setting_on_battery_undoes_the_other_modes_effect() {
        let mut r = Rule::default();
        r.step(true, Pause, || ids(&["a"]));
        assert_eq!(r.step(true, Limit, Vec::new), Plan { limit: Some(true), resume: ids(&["a"]), ..Plan::default() });
        assert_eq!(r.step(true, Pause, || ids(&["b"])), Plan { limit: Some(false), pause: ids(&["b"]), ..Plan::default() });
        assert_eq!(r.step(true, KeepGoing, Vec::new), Plan { resume: ids(&["b"]), ..Plan::default() });
        assert_eq!(r.step(false, KeepGoing, Vec::new), Plan::default());
    }

    use crate::row_text::tests::row;

    #[test]
    fn every_download_that_runs_or_waits_to_run_is_paused() {
        use TransferState::*;
        let mut s = crate::library::tests::snap();
        s.downloading = vec![row("dl", Downloading), row("check", Checking), row("magnet", Metadata), row("err", Error)];
        s.downloading.push(TransferRow { awaiting_choice: true, ..row("held", Downloading) });
        s.hidden_downloading = vec![row("hidden", Downloading)];
        s.queued = vec![row("queued", Queued), row("paused", Paused)];
        s.hidden_queued = vec![row("hidden-q", Queued)];
        let got: Vec<&str> = running_downloads(&s).map(|r| r.id.as_str()).collect();
        assert_eq!(got, ["dl", "check", "magnet", "hidden", "queued", "hidden-q"]);
    }
}
