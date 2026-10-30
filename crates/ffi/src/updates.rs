//! App updates from GitHub releases (SPEC 12A), for every host: the `latest.json` a release carries, whether it is
//! on offer, when to check (once a day, a minute after launch at the soonest), when to install by itself, the
//! installer command, what is remembered between launches (`updates.json` in the state folder) and every text of
//! Settings › General › Updates. The host runs a timer at `next_due_ms`, calls `check` off its main thread, starts
//! the command `install` returns (detached, output to `log_path`) and draws `model`. What differs per system (the
//! installer, the zip, this system's entry in `latest.json`) is `trs_platform::update`'s.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use trs_platform::update as os;

use crate::Core;
use crate::menu::APP_NAME;
use crate::row_text::row_is_held;

const CHECK_EVERY_MS: u64 = 24 * 3600 * 1000;
/// A check due at launch waits this long: not during the launch.
const FIRST_CHECK_DELAY_MS: u64 = 60 * 1000;
/// A failed automatic check waits this long before the next, doubling with each failure in a row up to a day
/// (`CHECK_EVERY_MS`); no automatic check ever comes sooner than a minute after the last attempt.
const RETRY_AFTER_MS: u64 = 3600 * 1000;
const MIN_GAP_MS: u64 = 60 * 1000;
/// The installer quits the app within seconds; still running after this, the install did not work.
const INSTALL_WATCHDOG_MS: u64 = 90 * 1000;
const DEFAULT_REPO: &str = "https://github.com/kkollsga/seedwell";
const MAX_LATEST_BYTES: usize = 64 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const STATE_FILE: &str = "updates.json";

// MARK: Versions and latest.json

/// A plain `MAJOR.MINOR.PATCH` (no `v`, no suffix, no fourth part).
pub(crate) fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let p: Vec<&str> = s.split('.').collect();
    if p.len() != 3 {
        return None;
    }
    let n = |x: &str| (!x.is_empty() && x.len() <= 9 && x.bytes().all(|b| b.is_ascii_digit())).then(|| x.parse().ok()).flatten();
    Some((n(p[0])?, n(p[1])?, n(p[2])?))
}

/// A system version: `15`, `15.4`, `15.4.1`, `10.0.19041` (more parts than three are cut off).
pub(crate) fn parse_system_version(s: &str) -> Option<(u32, u32, u32)> {
    let p: Vec<u32> = s.split('.').map(|x| x.parse().ok()).collect::<Option<Vec<u32>>>()?;
    match p.as_slice() {
        [a] => Some((*a, 0, 0)),
        [a, b] => Some((*a, *b, 0)),
        [a, b, c, ..] => Some((*a, *b, *c)),
        [] => None,
    }
}

/// This system's release as `latest.json` describes it (written by app/release-zip.sh, the Windows entry added by the
/// release workflow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Latest {
    pub version: String,
    pub notes_url: Option<String>,
    pub minimum_system: Option<(u32, u32, u32)>,
}

/// A release's `latest.json` with no entry for this system (a release without a Windows build): not an error, there
/// is just nothing for this system.
pub(crate) fn release_without_this_system(data: &[u8], section: Option<&str>) -> bool {
    let Some(section) = section else { return false };
    let Ok(top) = serde_json::from_slice::<serde_json::Value>(data) else { return false };
    let plain = top.get("version").and_then(|v| v.as_str()).and_then(parse_version).is_some();
    plain && top.get("tag").is_some_and(|t| t.is_string()) && top.get(section).is_none()
}

/// `None` when the document is not what the release writes: missing or garbage fields offer nothing.
pub(crate) fn parse_latest(data: &[u8], section: Option<&str>, min_key: &str, zip_for: impl Fn(&str) -> String) -> Option<Latest> {
    use serde_json::Value;
    if data.len() > MAX_LATEST_BYTES {
        return None;
    }
    let top: Value = serde_json::from_slice(data).ok()?;
    let version = top.get("version")?.as_str()?;
    parse_version(version)?;
    if top.get("tag")?.as_str()? != format!("v{version}") {
        return None;
    }
    let entry = match section {
        None => &top,
        Some(k) => top.get(k)?,
    };
    if entry.get("zip")?.as_str()? != zip_for(version) {
        return None;
    }
    let sha = entry.get("sha256")?.as_str()?;
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let notes_url = match top.get("notes_url") {
        None => None,
        Some(n) => {
            let s = n.as_str()?;
            if !(s.starts_with("https://") || s.starts_with("http://")) {
                return None;
            }
            Some(s.to_owned())
        }
    };
    let minimum_system = match entry.get(min_key) {
        None => None,
        Some(m) => Some(parse_system_version(m.as_str()?)?),
    };
    Some(Latest { version: version.to_owned(), notes_url, minimum_system })
}

impl Latest {
    /// Newer than `current`, and this system is new enough for it.
    fn offered(&self, current: &str, system: &str) -> bool {
        let (Some(v), Some(c)) = (parse_version(&self.version), parse_version(current)) else { return false };
        if v <= c {
            return false;
        }
        match (self.minimum_system, parse_system_version(system)) {
            (Some(min), Some(sys)) => sys >= min,
            (Some(_), None) => false,
            (None, _) => true,
        }
    }
}

// MARK: State

#[derive(Debug, Clone, PartialEq, Eq)]
enum Status {
    Idle,
    Checking,
    UpToDate,
    Available { version: String, notes: Option<String> },
    NoReleases,
    CheckFailed(String),
    Installing { version: String, since_ms: u64 },
    InstallFailed(String),
    Updated(String),
}

/// What is kept between launches.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
struct Saved {
    last_check_ms: Option<u64>,
    /// The version on offer at the last check, and its notes.
    offered: Option<String>,
    notes: Option<String>,
    /// An install was started for this version: the next launch says whether it worked.
    pending: Option<String>,
    /// The last automatic or manual check, whatever its outcome, and how many failed in a row since the last one
    /// that worked: a failing check is retried later and later (`RETRY_AFTER_MS`), never at once.
    last_attempt_ms: Option<u64>,
    failures: u32,
}

struct St {
    status: Status,
    saved: Saved,
    checking: bool,
    launched_ms: u64,
    /// No state of its own yet: what an earlier version of the app kept may be taken over (once).
    may_import: bool,
}

/// An installer to start so that it outlives the app, without a window, its output appended to `log`
/// (`trs_platform::update::Command` says how on each system).
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct InstallCommand {
    pub argv: Vec<String>,
    /// Added to the app's own environment.
    pub env: Vec<EnvVar>,
    pub log: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// A button of the Updates group.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UpdateButton {
    pub title: String,
    pub help: String,
    pub enabled: bool,
}

/// Settings › General › Updates, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UpdatesModel {
    /// "Check automatically" and its hint ("Once a day").
    pub check_label: String,
    pub check_hint: String,
    /// "Install automatically" and its hint.
    pub install_label: String,
    pub install_hint: String,
    /// The two switches do something (a release build); the install switch also needs the check switch on.
    pub switches_enabled: bool,
    /// "Check for updates" and the status line under it ("Seedwell 0.1.9 is up to date · checked 2 hours ago").
    pub status_label: String,
    pub status: String,
    pub check_now: UpdateButton,
    /// "Update to 0.2.0", while a newer version is on offer.
    pub install: Option<UpdateButton>,
    /// "Release Notes" and where they are, while a version is on offer and has them.
    pub notes_title: String,
    pub notes_url: Option<String>,
    /// "Show Log", after an install that did not work.
    pub show_log: Option<UpdateButton>,
    /// The version on offer (the menu bar's "Update to Seedwell …" row).
    pub available_version: Option<String>,
    /// Ask for the model again within this time (the "checked … ago" moves on, an install can time out).
    pub refresh_ms: u32,
}

/// The app's updates (one per app run).
#[derive(uniffi::Object)]
pub struct Updates {
    core: Arc<Core>,
    current: String,
    system: String,
    release_build: bool,
    repo: String,
    /// The repo came from `SEEDWELL_UPDATE_REPO` (a test's own server).
    repo_from_env: bool,
    file: PathBuf,
    st: Mutex<St>,
}

fn base(repo: &str) -> &str {
    repo.trim_end_matches('/')
}

impl Updates {
    fn lock(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, s: &Saved) {
        if let Ok(json) = serde_json::to_vec_pretty(s) {
            let tmp = self.file.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &self.file);
            }
        }
    }

    /// The check and install switches of `config.toml` (`[updates]`).
    fn prefs(&self) -> (bool, bool) {
        let p = self.core.settings_load().updates;
        (p.check, p.auto_install)
    }

    /// Automatic checks: a release build with checking on. An isolated (test) run checks only a server the test
    /// names (`SEEDWELL_UPDATE_REPO`), never the real one.
    fn automatic(&self) -> bool {
        self.release_build && self.prefs().0 && (!self.core.isolated || self.repo_from_env)
    }

    /// A version is on offer and a check of this run found it (not only remembered from an earlier run).
    fn confirmed_offer(&self) -> bool {
        let st = self.lock();
        matches!(st.status, Status::Available { .. }) && st.saved.last_check_ms.is_some_and(|l| l >= st.launched_ms)
    }

    /// Anything downloading (not held) or being moved: an automatic install waits for it.
    fn busy(&self) -> bool {
        let s = self.core.snapshot();
        s.moving_percent.is_some() || s.downloading.iter().chain(&s.hidden_downloading).any(|r| !row_is_held(r.clone()))
    }

    /// The status after a restore: a pending install either worked (this is its version) or did not; else what the
    /// last check found.
    fn restored(saved: &mut Saved, current: &str) -> Status {
        if let Some(p) = saved.pending.take() {
            return if p == current {
                saved.offered = None;
                Status::Updated(p)
            } else {
                Status::InstallFailed(p)
            };
        }
        match (&saved.offered, saved.last_check_ms) {
            (Some(v), _) if parse_version(v) > parse_version(current) => {
                Status::Available { version: v.clone(), notes: saved.notes.clone() }
            }
            (_, Some(_)) => Status::UpToDate,
            _ => Status::Idle,
        }
    }

    fn fetch(&self) -> Result<Option<Latest>, String> {
        if self.core.isolated && !self.repo_from_env {
            return Err("a test run reaches no update server".to_owned());
        }
        let url = format!("{}/releases/latest/download/latest.json", base(&self.repo));
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "network error".to_owned())?;
        let r = rt
            .block_on(trs_http::get_small(&url, &self.current, MAX_LATEST_BYTES, FETCH_TIMEOUT))
            .map_err(|e| match e {
                trs_http::Error::Timeout => "timed out".to_owned(),
                trs_http::Error::InvalidUrl(_) => "bad address".to_owned(),
                trs_http::Error::BadResponse(_) => "unreadable release information".to_owned(),
                trs_http::Error::Network(m) if m.contains("dns") || m.contains("resolve") => "server not found".to_owned(),
                _ => "network error".to_owned(),
            })?;
        match r.status {
            200 => match parse_latest(&r.body, os::RELEASE_SECTION, os::MINIMUM_SYSTEM_KEY, os::release_zip) {
                Some(l) => Ok(Some(l)),
                // The latest release has no build for this system: nothing on offer (it is up to date as far as
                // releases for it go), not a failure.
                None if release_without_this_system(&r.body, os::RELEASE_SECTION) => Ok(Some(Latest {
                    version: self.current.clone(),
                    notes_url: None,
                    minimum_system: None,
                })),
                None => Err("unreadable release information".to_owned()),
            },
            404 => Ok(None),
            s => Err(format!("server answered {s}")),
        }
    }

    fn status_text(&self, st: &St, now_ms: u64) -> String {
        let v = &self.current;
        match &st.status {
            Status::Idle => format!("{APP_NAME} {v} · not checked yet"),
            Status::Checking => "Checking…".into(),
            Status::UpToDate => match st.saved.last_check_ms {
                Some(at) => format!("{APP_NAME} {v} is up to date · checked {}", ago(at, now_ms)),
                None => format!("{APP_NAME} {v} is up to date"),
            },
            Status::Available { version, .. } => format!("{APP_NAME} {version} is available"),
            Status::NoReleases => "No releases yet".into(),
            Status::CheckFailed(why) => format!("Couldn't check: {why}"),
            Status::Installing { version, .. } => format!("Installing {version}…"),
            Status::InstallFailed(version) => format!("Update to {version} failed"),
            Status::Updated(version) => format!("Updated to {version}"),
        }
    }

    /// An install started more than the watchdog's time ago, with the app still running: it did not work.
    fn expire_install(&self, st: &mut St, now_ms: u64) {
        if let Status::Installing { version, since_ms } = &st.status
            && now_ms.saturating_sub(*since_ms) >= INSTALL_WATCHDOG_MS
        {
            st.status = Status::InstallFailed(version.clone());
            st.saved.pending = None;
            self.save(&st.saved);
        }
    }
}

/// The least time from an attempt to the next automatic check: a minute, or after `failures` failed ones in a row,
/// an hour doubling up to a day.
fn retry_after(failures: u32) -> u64 {
    match failures {
        0 => MIN_GAP_MS,
        n => RETRY_AFTER_MS.saturating_mul(1 << (n - 1).min(5)).min(CHECK_EVERY_MS),
    }
}

/// "just now", "5 minutes ago", "yesterday", "3 days ago": inside a sentence.
fn ago(at_ms: u64, now_ms: u64) -> String {
    let t = trs_control::display::ago(at_ms / 1000, now_ms / 1000, false);
    if t == "Yesterday" { "yesterday".into() } else { t }
}

#[uniffi::export]
impl Updates {
    /// `release_build`: a published build, which checks and installs by itself; a development build checks only
    /// when asked. `repo_url`: where the releases are (the build's own setting; the SEEDWELL_UPDATE_REPO variable, a
    /// loopback server in tests, wins). `system_version`: this system's version ("15.4.1", "10.0.19045").
    #[uniffi::constructor]
    pub fn new(core: Arc<Core>, release_build: bool, repo_url: Option<String>, system_version: String, now_ms: u64) -> Arc<Self> {
        let from_env = std::env::var("SEEDWELL_UPDATE_REPO").ok().filter(|s| !s.is_empty());
        let repo_from_env = from_env.is_some();
        let repo = from_env.or(repo_url.filter(|s| !s.is_empty())).unwrap_or_else(|| DEFAULT_REPO.to_owned());
        let file = core.dir.join(STATE_FILE);
        let stored = std::fs::read(&file).ok();
        let may_import = stored.is_none();
        let mut saved: Saved = stored.and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        let current = env!("CARGO_PKG_VERSION").to_owned();
        let had_pending = saved.pending.is_some();
        let status = Self::restored(&mut saved, &current);
        let u = Arc::new(Updates {
            core,
            current,
            system: system_version,
            release_build,
            repo,
            repo_from_env,
            file,
            st: Mutex::new(St { status, saved, checking: false, launched_ms: now_ms, may_import }),
        });
        if had_pending {
            let st = u.lock();
            u.save(&st.saved);
        }
        u
    }

    /// The running version.
    pub fn current_version(&self) -> String {
        self.current.clone()
    }

    /// What an earlier version of the app kept itself (the Mac's user defaults), taken over once, while this state
    /// has no file of its own yet, so an update installed by that version is reported by this one.
    pub fn import_earlier(&self, last_check_ms: Option<u64>, offered: Option<String>, notes: Option<String>, pending: Option<String>) {
        let mut st = self.lock();
        if !st.may_import {
            return;
        }
        st.may_import = false;
        let mut saved = Saved { last_check_ms, offered, notes, pending, ..Saved::default() };
        st.status = Self::restored(&mut saved, &self.current);
        st.saved = saved;
        self.save(&st.saved);
    }

    /// Settings › General › Updates.
    pub fn model(&self, now_ms: u64) -> UpdatesModel {
        let mut st = self.lock();
        self.expire_install(&mut st, now_ms);
        let (check_on, _) = self.prefs();
        let idle = !st.checking && !matches!(st.status, Status::Installing { .. });
        let available = match &st.status {
            Status::Available { version, .. } => Some(version.clone()),
            _ => None,
        };
        UpdatesModel {
            check_label: "Check automatically".into(),
            check_hint: if self.release_build { "Once a day".into() } else { "Development build: checks only when you ask".into() },
            install_label: "Install automatically".into(),
            install_hint: "When nothing is downloading".into(),
            switches_enabled: self.release_build,
            status_label: "Check for updates".into(),
            status: self.status_text(&st, now_ms),
            check_now: UpdateButton {
                title: "Check Now".into(),
                help: format!("Look for a newer version of {APP_NAME} now"),
                enabled: idle,
            },
            install: available.as_ref().map(|v| UpdateButton {
                title: format!("Update to {v}"),
                help: format!("Download, check and install {APP_NAME} {v}, then reopen it"),
                enabled: idle,
            }),
            notes_title: "Release Notes".into(),
            notes_url: match &st.status {
                Status::Available { notes, .. } => notes.clone(),
                _ => None,
            },
            show_log: matches!(st.status, Status::InstallFailed(_)).then(|| UpdateButton {
                title: "Show Log".into(),
                help: "Show the installer's log".into(),
                enabled: true,
            }),
            available_version: available,
            refresh_ms: if matches!(st.status, Status::Installing { .. } | Status::Checking) || !check_on { 1_000 } else { 60_000 },
        }
    }

    /// The version on offer, if any (the menu bar's update row).
    pub fn available_version(&self) -> Option<String> {
        match &self.lock().status {
            Status::Available { version, .. } => Some(version.clone()),
            _ => None,
        }
    }

    /// An automatic check is due now: a release build with checking on, a minute after launch at the soonest, and a
    /// day since the last check (or the clock went back), or a version on offer that may have been withdrawn.
    pub fn due(&self, now_ms: u64) -> bool {
        self.next_due_ms(now_ms) == Some(0)
    }

    /// How long until `due` is true (0: now); `None` while no automatic check will come (a development build,
    /// checking off, a check or an install running). The host sets its timer to it.
    pub fn next_due_ms(&self, now_ms: u64) -> Option<u64> {
        if !self.automatic() {
            return None;
        }
        let st = self.lock();
        if st.checking || matches!(st.status, Status::Installing { .. }) {
            return None;
        }
        let earliest = st.launched_ms + FIRST_CHECK_DELAY_MS;
        let mut next = match st.saved.last_check_ms {
            _ if matches!(st.status, Status::Available { .. }) && st.saved.last_check_ms.is_none_or(|l| l < st.launched_ms) => earliest,
            None => earliest,
            Some(last) if last > now_ms => earliest,
            Some(last) => (last + CHECK_EVERY_MS).max(earliest),
        };
        // Never sooner than a minute after the last attempt, and after failures later and later: a check that keeps
        // failing (no network, a server that refuses) must not be repeated at once, forever.
        if let Some(attempt) = st.saved.last_attempt_ms.filter(|a| *a <= now_ms) {
            next = next.max(attempt + retry_after(st.saved.failures));
        }
        Some(next.saturating_sub(now_ms))
    }

    /// One request for `latest.json` (blocking, up to 15 s: call it off the main thread). `silent`: an automatic
    /// check, which keeps what it knew when it fails.
    pub fn check(&self, silent: bool, now_ms: u64) {
        let before = {
            let mut st = self.lock();
            if st.checking || matches!(st.status, Status::Installing { .. }) {
                return;
            }
            st.checking = true;
            st.saved.last_attempt_ms = Some(now_ms);
            let before = st.status.clone();
            if !silent {
                st.status = Status::Checking;
            }
            before
        };
        let outcome = self.fetch();
        let mut st = self.lock();
        st.checking = false;
        st.saved.failures = if outcome.is_ok() { 0 } else { st.saved.failures.saturating_add(1) };
        match outcome {
            Ok(Some(l)) => {
                st.saved.last_check_ms = Some(now_ms);
                if l.offered(&self.current, &self.system) {
                    st.saved.offered = Some(l.version.clone());
                    st.saved.notes = l.notes_url.clone();
                    st.status = Status::Available { version: l.version, notes: l.notes_url };
                } else {
                    st.saved.offered = None;
                    st.saved.notes = None;
                    st.status = Status::UpToDate;
                }
            }
            Ok(None) => {
                st.saved.last_check_ms = Some(now_ms);
                st.saved.offered = None;
                st.saved.notes = None;
                st.status = Status::NoReleases;
            }
            Err(why) => {
                st.status = if silent && !matches!(before, Status::CheckFailed(_)) { before } else { Status::CheckFailed(why) };
            }
        }
        self.save(&st.saved);
    }

    /// Install by itself now: a release build, both switches on, a version on offer that a check of this run
    /// confirmed (one remembered from an earlier run may have been withdrawn since), nothing downloading or being
    /// moved, no install running. Seeding does not stop it.
    pub fn should_install(&self) -> bool {
        let (check, auto) = self.prefs();
        if !(self.release_build && check && auto) {
            return false;
        }
        self.confirmed_offer() && !self.busy()
    }

    /// Starts the install of the version on offer into `install_dir` (the folder that holds the app): the command
    /// for the host to start detached. `None` when nothing is on offer or an install runs. The next launch reports
    /// whether it worked; if the app still runs after 90 s, the model says it failed.
    pub fn install(&self, install_dir: String, now_ms: u64) -> Option<InstallCommand> {
        let mut st = self.lock();
        let Status::Available { version, .. } = &st.status else { return None };
        let version = version.clone();
        let installer = format!("{}/releases/download/v{version}/{}", base(&self.repo), os::INSTALLER);
        let c = os::installer_command(&installer, &install_dir, &version, base(&self.repo));
        st.saved.pending = Some(version.clone());
        st.status = Status::Installing { version, since_ms: now_ms };
        self.save(&st.saved);
        Some(InstallCommand {
            argv: c.argv,
            env: c.env.into_iter().map(|(name, value)| EnvVar { name, value }).collect(),
            log: self.log_path(),
        })
    }

    /// The host could not start the installer.
    pub fn install_failed(&self) {
        let mut st = self.lock();
        if let Status::Installing { version, .. } = &st.status {
            st.status = Status::InstallFailed(version.clone());
            st.saved.pending = None;
            self.save(&st.saved);
        }
    }

    /// The installer's log (`logs/update.log` in the state folder).
    pub fn log_path(&self) -> String {
        self.core.dir.join("logs").join("update.log").display().to_string()
    }
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
