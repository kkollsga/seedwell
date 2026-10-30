//! App updates from GitHub releases (SPEC 12A), for every host: the `latest.json` a release carries, whether it is
//! on offer, when to check (once a day, a minute after launch at the soonest), when to install by itself, the
//! installer command, what is remembered between launches (`updates.json` in the state folder) and every text of
//! Settings › General › Updates. The host runs a timer at `next_due_ms`, calls `check` off its main thread, starts
//! the command `install` returns (detached, output to `log_path`) and draws `model`. What differs per system (the
//! installer, the zip, this system's entry in `latest.json`) is `trs_platform::update`'s.
//!
//! Development builds (SPEC 12A): with "Allow development builds" on, a build handed over (`seedwell update
//! install`, the host's Install Build…) or one that appears in a watched folder (read on file-change notifications,
//! `trs-fswatch`, never on a timer) is installed; the core starts that installer itself. Every install keeps the app it replaces (when that one started fine) and goes back to it when the
//! new one does not start (`started`, `launch_guard`, `recovery_actions`); the installer does that part.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use trs_platform::update as os;

use crate::{Core, CoreError};
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
/// Written by `started`: the version that started fine (the installer's watchdog waits for it).
const STARTED_FILE: &str = "launch-ok";
/// The app kept to go back to (written by the installer): a zip, its installer and `previous.json`.
const KEPT_DIR: &str = "previous";
/// This many launches in a row without `started`, on a development build: back to the kept app.
const UNSTARTED_LIMIT: u32 = 3;
/// Failed development builds remembered (the newest).
const FAILED_KEEP: usize = 20;
/// The most a build zip may be (the app is about 40 MB).
const MAX_BUILD_BYTES: u64 = 1 << 30;

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

/// A release `X.Y.Z` or a development build `X.Y.Z.N` (`N` from 1), in order: a build sorts after the release it
/// builds on and before the next (`0.1.10` < `0.1.10.1` < `0.1.11`).
pub(crate) fn version_order(s: &str) -> Option<(u32, u32, u32, u32)> {
    match s.rsplit_once('.') {
        Some((head, n)) if parse_version(head).is_some() => {
            let (a, b, c) = parse_version(head)?;
            let n: u32 = (!n.is_empty() && n.len() <= 9 && n.bytes().all(|b| b.is_ascii_digit())).then(|| n.parse().ok()).flatten()?;
            (n > 0).then_some((a, b, c, n))
        }
        _ => parse_version(s).map(|(a, b, c)| (a, b, c, 0)),
    }
}

/// A development build's version (`X.Y.Z.N`).
pub(crate) fn is_development(v: &str) -> bool {
    version_order(v).is_some_and(|o| o.3 > 0)
}

/// A version as the app shows it: "0.1.10", "0.1.10.3 (development build)".
pub(crate) fn shown(v: &str) -> String {
    if is_development(v) { format!("{v} (development build)") } else { v.to_owned() }
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
    pub sha256: String,
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
    parse_doc(data, section, min_key, zip_for, false)
}

/// A watched folder's `latest.json`: a release's, but the version may be a development build's and the tag may be
/// left out.
pub(crate) fn parse_build(data: &[u8], section: Option<&str>, min_key: &str, zip_for: impl Fn(&str) -> String) -> Option<Latest> {
    parse_doc(data, section, min_key, zip_for, true)
}

fn parse_doc(data: &[u8], section: Option<&str>, min_key: &str, zip_for: impl Fn(&str) -> String, build: bool) -> Option<Latest> {
    use serde_json::Value;
    if data.len() > MAX_LATEST_BYTES {
        return None;
    }
    let top: Value = serde_json::from_slice(data).ok()?;
    let version = top.get("version")?.as_str()?;
    if build {
        version_order(version)?;
    } else {
        parse_version(version)?;
    }
    match top.get("tag") {
        None if build => {}
        t => {
            if t?.as_str()? != format!("v{version}") {
                return None;
            }
        }
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
    Some(Latest { version: version.to_owned(), sha256: sha.to_ascii_lowercase(), notes_url, minimum_system })
}

impl Latest {
    /// Newer than `current`, and this system is new enough for it.
    fn offered(&self, current: &str, system: &str) -> bool {
        let (Some(v), Some(c)) = (version_order(&self.version), version_order(current)) else { return false };
        v > c && self.runs_on(system)
    }

    /// This system is new enough for it.
    fn runs_on(&self, system: &str) -> bool {
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
    /// Following a folder; `Some`: why nothing can be installed from it ("no build there yet").
    Watching(Option<String>),
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
    /// Development builds that were installed and did not start: never installed from a watched folder again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    failed: Vec<String>,
    /// Launches since the last one that started fine or quit cleanly (`launch_guard` counts; `started` and `quitting`
    /// reset).
    #[serde(skip_serializing_if = "is_zero")]
    unstarted: u32,
    /// "Allow development builds" was turned off while one ran (at this time): the latest release is installed,
    /// whatever its version.
    back_to_release_ms: Option<u64>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

fn read_saved(file: &Path) -> (Option<Saved>, bool) {
    match std::fs::read(file) {
        Ok(b) => (serde_json::from_slice(&b).ok(), true),
        Err(_) => (None, false),
    }
}

fn write_saved(file: &Path, s: &Saved) {
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        let tmp = file.with_extension("json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }
}

/// A build found in a watched folder, ready to install.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FolderBuild {
    version: String,
    zip: PathBuf,
    sha256: String,
}

struct St {
    status: Status,
    saved: Saved,
    checking: bool,
    launched_ms: u64,
    /// No state of its own yet: what an earlier version of the app kept may be taken over (once).
    may_import: bool,
    /// "Allow development builds" as last seen (turning it off while a build runs goes back to the release).
    dev_on_seen: bool,
    /// The newest build of the watched folder, when it is on offer.
    folder_build: Option<FolderBuild>,
}

/// The watched folder's watcher: its consumer thread reads the folder on each notification and ends when the guard
/// is dropped (the channel closes).
struct FolderWatch {
    folder: PathBuf,
    _guard: Option<trs_fswatch::WatchGuard>,
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
    /// "Allow development builds" and its hint.
    pub dev_label: String,
    /// What the switch does.
    pub dev_hint: String,
    /// "Install Build…" (a file chooser for a build's zip), while the switch is on.
    pub install_build: Option<UpdateButton>,
    /// The folder whose builds are installed as they appear, while the switch is on and one is set.
    pub watching: Option<String>,
    /// "Stop Watching", with `watching`.
    pub stop_watching: Option<UpdateButton>,
    /// The running version as shown ("0.1.10.3 (development build)").
    pub version: String,
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
    /// Where builds are installed and how the app is reopened (registered by the host).
    install_dir: String,
    /// The running app's own installer (`install.sh` in the bundle's Resources, `install.ps1` next to the exe).
    installer: String,
    relaunch: Vec<String>,
    st: Mutex<St>,
    /// The watched folder's watcher, while one is watched.
    watch: Mutex<Option<FolderWatch>>,
    me: Weak<Updates>,
}

fn base(repo: &str) -> &str {
    repo.trim_end_matches('/')
}

/// `SEEDWELL_UPDATE_REPO` (tests), else the build's own setting, else GitHub; and whether it came from the variable.
fn resolve_repo(repo_url: Option<String>) -> (String, bool) {
    let from_env = std::env::var("SEEDWELL_UPDATE_REPO").ok().filter(|s| !s.is_empty());
    let repo_from_env = from_env.is_some();
    (from_env.or(repo_url.filter(|s| !s.is_empty())).unwrap_or_else(|| DEFAULT_REPO.to_owned()), repo_from_env)
}

fn log_in(dir: &Path) -> PathBuf {
    dir.join("logs").join("update.log")
}

/// The app kept to go back to, in `dir` (the state folder): its version and its installer.
fn kept_in(dir: &Path) -> Option<(String, PathBuf)> {
    let d = dir.join(KEPT_DIR);
    let doc: serde_json::Value = serde_json::from_slice(&std::fs::read(d.join("previous.json")).ok()?).ok()?;
    let version = doc.get("version")?.as_str()?.to_owned();
    version_order(&version)?;
    let installer = d.join(os::INSTALLER);
    (installer.is_file() && d.join("Seedwell.zip").is_file()).then_some((version, installer))
}

fn command(c: os::Command, log: &Path) -> InstallCommand {
    InstallCommand {
        argv: c.argv,
        env: c.env.into_iter().map(|(name, value)| EnvVar { name, value }).collect(),
        log: log.display().to_string(),
    }
}

/// The version a build's zip carries in its name (`Seedwell-0.1.10.3.zip`; `-windows-x64.zip` on Windows).
fn zip_version(name: &str) -> Option<String> {
    let v = name.strip_prefix("Seedwell-")?.strip_suffix(".zip")?;
    let v = v.strip_suffix("-windows-x64").unwrap_or(v);
    (version_order(v).is_some() && os::release_zip(v) == name).then(|| v.to_owned())
}

fn sha256_of(p: &Path) -> std::io::Result<String> {
    use sha2::Digest as _;
    use std::io::Read as _;
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

impl Updates {
    fn lock(&self) -> std::sync::MutexGuard<'_, St> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn save(&self, s: &Saved) {
        write_saved(&self.file, s);
    }

    /// The `[updates]` switches of `config.toml`.
    fn prefs(&self) -> Prefs {
        let p = self.core.settings_load().updates;
        Prefs { check: p.check, auto_install: p.auto_install, dev_builds: p.dev_builds, folder: p.dev_folder }
    }

    /// Automatic checks: a release build with checking on. An isolated (test) run checks only a server the test
    /// names (`SEEDWELL_UPDATE_REPO`), never the real one.
    fn automatic(&self) -> bool {
        self.release_build && self.prefs().check && self.may_reach_server()
    }

    fn may_reach_server(&self) -> bool {
        !self.core.isolated || self.repo_from_env
    }

    /// The folder followed: "Allow development builds" is on and a folder is set.
    fn watched(&self) -> Option<PathBuf> {
        let p = self.prefs();
        (p.dev_builds && !p.folder.is_empty()).then(|| PathBuf::from(p.folder))
    }

    /// Turning "Allow development builds" off while a development build runs starts the way back to the release;
    /// turning it on again ends it.
    fn note_switch(&self, st: &mut St, now_ms: u64) {
        let on = self.prefs().dev_builds;
        let mut changed = false;
        if st.dev_on_seen && !on && is_development(&self.current) && st.saved.back_to_release_ms.is_none() {
            st.saved.back_to_release_ms = Some(now_ms);
            changed = true;
        }
        if on && st.saved.back_to_release_ms.take().is_some() {
            changed = true;
        }
        if on != st.dev_on_seen {
            st.dev_on_seen = on;
            st.folder_build = None;
            if matches!(st.status, Status::Watching(_)) || (on && matches!(st.status, Status::Available { .. })) {
                st.status = Self::restored(&mut st.saved.clone(), &self.current);
            }
        }
        if changed {
            self.save(&st.saved);
        }
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

    /// The status after a restore: a pending install either worked (this is its version) or did not (a development
    /// build that did not is remembered as failed); else what the last check found.
    fn restored(saved: &mut Saved, current: &str) -> Status {
        if let Some(p) = saved.pending.take() {
            return if p == current {
                saved.offered = None;
                Status::Updated(p)
            } else {
                if is_development(&p) && !saved.failed.contains(&p) {
                    saved.failed.push(p.clone());
                    let over = saved.failed.len().saturating_sub(FAILED_KEEP);
                    saved.failed.drain(..over);
                }
                Status::InstallFailed(p)
            };
        }
        match (&saved.offered, saved.last_check_ms) {
            (Some(v), _) if version_order(v) > version_order(current) => {
                Status::Available { version: v.clone(), notes: saved.notes.clone() }
            }
            (_, Some(_)) => Status::UpToDate,
            _ => Status::Idle,
        }
    }

    fn fetch(&self) -> Result<Option<Latest>, String> {
        if !self.may_reach_server() {
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
                    sha256: String::new(),
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
        let v = shown(&self.current);
        match &st.status {
            Status::Idle => format!("{APP_NAME} {v} · not checked yet"),
            Status::Checking => "Checking…".into(),
            Status::UpToDate => match st.saved.last_check_ms {
                Some(at) => format!("{APP_NAME} {v} is up to date · checked {}", ago(at, now_ms)),
                None => format!("{APP_NAME} {v} is up to date"),
            },
            Status::Available { version, .. } => format!("{APP_NAME} {} is available", shown(version)),
            Status::NoReleases => "No releases yet".into(),
            Status::CheckFailed(why) => format!("Couldn't check: {why}"),
            Status::Installing { version, .. } => format!("Installing {}…", shown(version)),
            Status::InstallFailed(version) if is_development(version) => {
                format!("{version} didn't start; back on {}", self.current)
            }
            Status::InstallFailed(version) => format!("Update to {version} failed"),
            Status::Updated(version) => format!("Updated to {}", shown(version)),
            Status::Watching(None) => format!("{APP_NAME} {v} · watching for newer builds"),
            Status::Watching(Some(why)) => format!("{APP_NAME} {v} · {why}"),
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

    fn place(&self) -> (String, Vec<String>) {
        (self.core.dir.display().to_string(), self.relaunch.clone())
    }

    /// Starts the running app's installer on `zip` (already checked: `sha256` is its checksum) for `version`.
    fn start_build(&self, st: &mut St, version: &str, zip: &Path, sha256: &str, now_ms: u64) -> Result<(), String> {
        if self.installer.is_empty() || !Path::new(&self.installer).is_file() {
            return Err("this copy of Seedwell has no installer to install builds with".into());
        }
        let (state_dir, relaunch) = self.place();
        let i = os::Install { install_dir: &self.install_dir, state_dir: &state_dir, relaunch: &relaunch };
        let c = os::local_installer_command(&self.installer, &zip.display().to_string(), sha256, &i);
        st.saved.pending = Some(version.to_owned());
        self.save(&st.saved);
        if let Err(e) = os::spawn(&c, &log_in(&self.core.dir)) {
            st.saved.pending = None;
            self.save(&st.saved);
            st.status = Status::InstallFailed(version.to_owned());
            return Err(format!("the installer did not start: {e}"));
        }
        st.status = Status::Installing { version: version.to_owned(), since_ms: now_ms };
        st.folder_build = None;
        Ok(())
    }

    /// The watched folder's build, when it is newer, not failed before and its zip is there.
    fn read_folder(&self, folder: &Path) -> Result<Option<FolderBuild>, String> {
        let doc = folder.join("latest.json");
        let data = match std::fs::metadata(&doc) {
            Ok(m) if m.len() as usize > MAX_LATEST_BYTES => return Err("its latest.json is too large".into()),
            Ok(_) => std::fs::read(&doc).map_err(|e| format!("its latest.json cannot be read ({e})"))?,
            Err(_) if !folder.is_dir() => return Err(format!("{} is missing", folder.display())),
            Err(_) => return Err("no build there yet".into()),
        };
        let l = parse_build(&data, os::RELEASE_SECTION, os::MINIMUM_SYSTEM_KEY, os::release_zip)
            .ok_or_else(|| "its latest.json is not a build's".to_owned())?;
        let zip = folder.join(os::release_zip(&l.version));
        let st = self.lock();
        if !l.offered(&self.current, &self.system) || st.saved.failed.contains(&l.version) || !zip.is_file() {
            return Ok(None);
        }
        Ok(Some(FolderBuild { version: l.version, zip, sha256: l.sha256 }))
    }

    /// One look at the watched folder: a newer build there is installed now, or as soon as nothing downloads.
    fn check_folder(&self, folder: &Path, now_ms: u64) {
        let found = self.read_folder(folder);
        let busy = matches!(found, Ok(Some(_))) && self.busy();
        let mut st = self.lock();
        if matches!(st.status, Status::Installing { .. }) {
            return;
        }
        match found {
            Err(why) => {
                st.folder_build = None;
                if !matches!(st.status, Status::InstallFailed(_) | Status::Updated(_)) {
                    st.status = Status::Watching(Some(why));
                }
            }
            Ok(None) => {
                st.folder_build = None;
                if !matches!(st.status, Status::InstallFailed(_) | Status::Updated(_)) {
                    st.status = Status::Watching(None);
                }
            }
            Ok(Some(b)) => {
                st.status = Status::Available { version: b.version.clone(), notes: None };
                st.folder_build = Some(b.clone());
                if !busy {
                    // A zip that changed since latest.json named it is refused by the installer's checksum.
                    let _ = self.start_build(&mut st, &b.version, &b.zip, &b.sha256, now_ms);
                }
            }
        }
    }

    /// Watches the folder the settings name (while "Allow development builds" is on), or none: starts, moves or
    /// stops the watcher. Called at launch, when the switch or the folder changes, and whenever the host asks for
    /// the model or the next check (which is how a change made in Settings arrives).
    fn sync_watch(&self) {
        let want = self.watched();
        let mut w = self.watch.lock().unwrap_or_else(|p| p.into_inner());
        if w.as_ref().map(|f| &f.folder) == want.as_ref() {
            return;
        }
        *w = None; // stops the old watcher, which ends its thread
        let Some(folder) = want else { return };
        // The watcher first, then the first read, so a build put there in between is not missed.
        let (events, guard) = match trs_fswatch::watch(&folder, trs_fswatch::WatchOptions::default()) {
            Ok(watcher) => {
                let (e, g) = watcher.split();
                (Some(e), Some(g))
            }
            Err(_) => (None, None), // a missing folder: the first read says so
        };
        *w = Some(FolderWatch { folder: folder.clone(), _guard: guard });
        drop(w);
        let me = self.me.clone();
        let _ = std::thread::Builder::new().name("seedwell-build-watch".into()).spawn(move || {
            let read = |me: &Weak<Updates>| me.upgrade().map(|u| u.check_folder(&folder, unix_ms())).is_some();
            if !read(&me) {
                return;
            }
            let Some(events) = events else { return };
            // Events are hints: any of them (a Rescan too) means read latest.json again and believe the disk. A burst
            // (the zip, then latest.json) is read once it has been quiet for a moment.
            while events.recv().is_ok() {
                while events.recv_timeout(Duration::from_millis(300)).is_ok() {}
                if !read(&me) {
                    return;
                }
            }
        });
    }

    fn model_now(&self, now_ms: u64) -> UpdatesModel {
        self.sync_watch();
        let mut st = self.lock();
        self.note_switch(&mut st, now_ms);
        self.expire_install(&mut st, now_ms);
        let prefs = self.prefs();
        let watching = self.watched();
        let idle = !st.checking && !matches!(st.status, Status::Installing { .. });
        let available = match &st.status {
            Status::Available { version, .. } => Some(version.clone()),
            _ => None,
        };
        let back = st.saved.back_to_release_ms.is_some();
        UpdatesModel {
            check_label: "Check automatically".into(),
            check_hint: if self.release_build { "Once a day".into() } else { "Local build: checks only when you ask".into() },
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
            install: available.as_ref().filter(|_| st.folder_build.is_none()).map(|v| UpdateButton {
                title: if back { format!("Install {v}") } else { format!("Update to {v}") },
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
            dev_label: "Allow development builds".into(),
            dev_hint: "Install builds handed over with Install Build… or `seedwell update install`; one that does not start is replaced by the one before".into(),
            install_build: prefs.dev_builds.then(|| UpdateButton {
                title: "Install Build…".into(),
                help: format!("Choose a build of {APP_NAME} (Seedwell-….zip) to install"),
                enabled: idle,
            }),
            stop_watching: watching.as_ref().map(|_| UpdateButton {
                title: "Stop Watching".into(),
                help: "Stop installing the builds that appear in this folder".into(),
                enabled: true,
            }),
            watching: watching.map(|w| w.display().to_string()),
            version: shown(&self.current),
            refresh_ms: if matches!(st.status, Status::Installing { .. } | Status::Checking) || !prefs.check {
                1_000
            } else {
                60_000
            },
        }
    }
}

/// The `[updates]` section as the updater reads it.
struct Prefs {
    check: bool,
    auto_install: bool,
    dev_builds: bool,
    folder: String,
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

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The control server's view of the updater (`update.*`).
struct Hook(Weak<Updates>);

impl trs_control::Builds for Hook {
    fn status(&self) -> trs_control::types::UpdateStatusDto {
        let Some(u) = self.0.upgrade() else { return Default::default() };
        let m = u.model_now(unix_ms());
        trs_control::types::UpdateStatusDto {
            version: u.current.clone(),
            development: is_development(&u.current),
            dev_builds: u.prefs().dev_builds,
            watching: m.watching,
            kept: kept_in(&u.core.dir).map(|(v, _)| v),
            status: m.status,
            note: String::new(),
        }
    }

    fn install(&self, zip: &Path, sha256: Option<&str>) -> Result<String, String> {
        let u = self.0.upgrade().ok_or("Seedwell is quitting")?;
        u.install_build_at(zip, sha256, unix_ms())
    }

    fn watch(&self, folder: Option<&Path>) -> Result<String, String> {
        let u = self.0.upgrade().ok_or("Seedwell is quitting")?;
        u.watch_folder(folder)
    }
}

impl Updates {
    fn gate(&self) -> Result<(), String> {
        if self.prefs().dev_builds {
            Ok(())
        } else {
            Err("development builds are off: turn on Settings › General › Updates › Allow development builds".into())
        }
    }

    fn install_build_at(&self, zip: &Path, sha256: Option<&str>, now_ms: u64) -> Result<String, String> {
        self.gate()?;
        let name = zip.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let version = zip_version(name)
            .ok_or_else(|| format!("{name} is not a build of {APP_NAME} for this system (expected {})", os::release_zip("X.Y.Z.N")))?;
        let meta = std::fs::metadata(zip).map_err(|_| format!("no such file: {}", zip.display()))?;
        if !meta.is_file() || meta.len() > MAX_BUILD_BYTES {
            return Err(format!("{} is not a build's zip", zip.display()));
        }
        if version == self.current {
            return Err(format!("{APP_NAME} {} is already running", shown(&version)));
        }
        let have = sha256_of(zip).map_err(|e| format!("{} cannot be read: {e}", zip.display()))?;
        if let Some(want) = sha256
            && !want.eq_ignore_ascii_case(&have)
        {
            return Err(format!("{name} has SHA-256 {have}, not {want}; nothing was installed"));
        }
        let mut st = self.lock();
        if st.checking || matches!(st.status, Status::Installing { .. }) {
            return Err("an install is already running".into());
        }
        self.start_build(&mut st, &version, zip, &have, now_ms)?;
        // The installer keeps the running app when it started fine (launch-ok names it); else the one kept before stays.
        let started = std::fs::read_to_string(self.core.dir.join(STARTED_FILE)).is_ok_and(|t| t.lines().next() == Some(self.current.as_str()));
        let back = if started { Some(self.current.clone()) } else { kept_in(&self.core.dir).map(|(v, _)| v) };
        let fallback = match back {
            Some(v) => format!("if it does not start within 2 minutes, {v} comes back"),
            None => "no earlier version is kept to go back to".to_owned(),
        };
        Ok(format!(
            "Installing {}: {APP_NAME} quits, is replaced and reopens; {fallback} (log: {})",
            shown(&version),
            log_in(&self.core.dir).display()
        ))
    }

    fn watch_folder(&self, folder: Option<&Path>) -> Result<String, String> {
        self.gate()?;
        if let Some(f) = folder
            && !f.is_dir()
        {
            return Err(format!("no such folder: {}", f.display()));
        }
        let mut s = self.core.settings_load();
        s.updates.dev_folder = folder.map(|f| f.display().to_string()).unwrap_or_default();
        self.core.settings_save(s).map_err(|e| e.to_string())?;
        {
            let mut st = self.lock();
            st.folder_build = None;
            if matches!(st.status, Status::Watching(_)) {
                st.status = Self::restored(&mut st.saved.clone(), &self.current);
            }
        }
        self.sync_watch();
        Ok(match folder {
            Some(f) => format!("Watching {}: each newer build put there is installed as it appears (zip first, then latest.json)", f.display()),
            None => "Stopped watching".into(),
        })
    }
}

#[uniffi::export]
impl Updates {
    /// `release_build`: a published build, which checks and installs by itself; a local build checks only when asked.
    /// `repo_url`: where the releases are (the build's own setting; the SEEDWELL_UPDATE_REPO variable, a loopback
    /// server in tests, wins). `system_version`: this system's version ("15.4.1", "10.0.19045"). `dev_version`: the
    /// development build's version (`X.Y.Z.N`, the Mac's `SeedwellDevVersion`), `None` for a release.
    /// `install_dir`: the folder that holds the app; `installer`: the app's own installer script; `relaunch`: the
    /// arguments to reopen the app with. The control server's `update.*` methods use this updater from now on.
    #[uniffi::constructor]
    #[allow(clippy::too_many_arguments, reason = "the host's facts, each once")]
    pub fn new(
        core: Arc<Core>,
        release_build: bool,
        repo_url: Option<String>,
        system_version: String,
        dev_version: Option<String>,
        install_dir: String,
        installer: String,
        relaunch: Vec<String>,
        now_ms: u64,
    ) -> Arc<Self> {
        let (repo, repo_from_env) = resolve_repo(repo_url);
        let file = core.dir.join(STATE_FILE);
        let (stored, exists) = read_saved(&file);
        let may_import = !exists;
        let mut saved = stored.unwrap_or_default();
        let current = dev_version
            .filter(|v| is_development(v) && v.starts_with(&format!("{}.", env!("CARGO_PKG_VERSION"))))
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned());
        let before = saved.clone();
        if !is_development(&current) {
            saved.back_to_release_ms = None;
        }
        let status = Self::restored(&mut saved, &current);
        let dev_on = core.settings_load().updates.dev_builds;
        let u = Arc::new_cyclic(|me| Updates {
            core,
            current,
            system: system_version,
            release_build,
            repo,
            repo_from_env,
            file,
            install_dir,
            installer,
            relaunch,
            st: Mutex::new(St {
                status,
                saved,
                checking: false,
                launched_ms: now_ms,
                may_import,
                dev_on_seen: dev_on,
                folder_build: None,
            }),
            watch: Mutex::new(None),
            me: me.clone(),
        });
        {
            let st = u.lock();
            if st.saved != before {
                u.save(&st.saved);
            }
        }
        u.core.builds.set(Arc::new(Hook(Arc::downgrade(&u))));
        u.sync_watch();
        u
    }

    /// The running version (`X.Y.Z`, or `X.Y.Z.N` for a development build).
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
        let mut saved = Saved { last_check_ms, offered, notes, pending, ..st.saved.clone() };
        st.status = Self::restored(&mut saved, &self.current);
        st.saved = saved;
        self.save(&st.saved);
    }

    /// Settings › General › Updates.
    pub fn model(&self, now_ms: u64) -> UpdatesModel {
        self.model_now(now_ms)
    }

    /// The version on offer, if any (the menu bar's update row; not a watched folder's build, which installs itself).
    pub fn available_version(&self) -> Option<String> {
        let st = self.lock();
        match &st.status {
            Status::Available { version, .. } if st.folder_build.is_none() => Some(version.clone()),
            _ => None,
        }
    }

    /// An automatic check is due now: a release build with checking on, a minute after launch at the soonest, and a
    /// day since the last check (or the clock went back), or a version on offer that may have been withdrawn; the
    /// way back to the release at once. A watched folder needs no timer: its watcher reads it on each change.
    pub fn due(&self, now_ms: u64) -> bool {
        self.next_due_ms(now_ms) == Some(0)
    }

    /// How long until `due` is true (0: now); `None` while no automatic check will come (a local build or checking
    /// off; a folder watched; a check or an install running). The host sets its timer to it.
    pub fn next_due_ms(&self, now_ms: u64) -> Option<u64> {
        self.sync_watch();
        let watched = self.watched().is_some();
        let automatic = self.automatic();
        let mut st = self.lock();
        self.note_switch(&mut st, now_ms);
        if watched || st.checking || matches!(st.status, Status::Installing { .. }) {
            return None;
        }
        // On the way back to the release: at once, unless a check since the switch failed (then as backed off below).
        let back = st.saved.back_to_release_ms.filter(|t| st.saved.last_check_ms.is_none_or(|l| l < *t)).filter(|_| self.may_reach_server());
        if back.is_none() && !automatic {
            return None;
        }
        let earliest = st.launched_ms + FIRST_CHECK_DELAY_MS;
        let mut next = match st.saved.last_check_ms {
            _ if back.is_some() => now_ms,
            _ if matches!(st.status, Status::Available { .. }) && st.saved.last_check_ms.is_none_or(|l| l < st.launched_ms) => earliest,
            None => earliest,
            Some(last) if last > now_ms => earliest,
            Some(last) => (last + CHECK_EVERY_MS).max(earliest),
        };
        // Never sooner than a minute after the last attempt, and after failures later and later: a check that keeps
        // failing (no network, a server that refuses) must not be repeated at once, forever. The way back to the
        // release waits only for attempts made since the switch.
        if let Some(attempt) = st.saved.last_attempt_ms.filter(|a| *a <= now_ms && back.is_none_or(|t| *a >= t)) {
            next = next.max(attempt + retry_after(st.saved.failures));
        }
        Some(next.saturating_sub(now_ms))
    }

    /// One look: at the watched folder (a newer build there is installed by the core itself), else one request for
    /// `latest.json` (blocking, up to 15 s: call it off the main thread). `silent`: an automatic check, which keeps
    /// what it knew when it fails.
    pub fn check(&self, silent: bool, now_ms: u64) {
        self.sync_watch();
        if let Some(folder) = self.watched() {
            return self.check_folder(&folder, now_ms);
        }
        let before = {
            let mut st = self.lock();
            self.note_switch(&mut st, now_ms);
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
        let back = st.saved.back_to_release_ms.is_some();
        match outcome {
            Ok(Some(l)) => {
                st.saved.last_check_ms = Some(now_ms);
                // On the way back to the release, the latest release is offered whatever its version.
                if l.offered(&self.current, &self.system) || (back && l.version != self.current && l.runs_on(&self.system)) {
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
    /// moved, no install running. Seeding does not stop it. On the way back to the release (the development-builds
    /// switch turned off while a build ran) neither switch is needed: turning it off was the choice.
    pub fn should_install(&self) -> bool {
        let p = self.prefs();
        let back = self.lock().saved.back_to_release_ms.is_some();
        if self.lock().folder_build.is_some() || !(back || (self.release_build && p.check && p.auto_install)) {
            return false;
        }
        self.confirmed_offer() && !self.busy()
    }

    /// Starts the install of the release on offer into `install_dir` (the folder that holds the app): the command
    /// for the host to start detached. `None` when nothing is on offer, it is a watched folder's build (the core
    /// installs those) or an install runs. The next launch reports whether it worked; if the app still runs after
    /// 90 s, the model says it failed.
    pub fn install(&self, install_dir: String, now_ms: u64) -> Option<InstallCommand> {
        let mut st = self.lock();
        let Status::Available { version, .. } = &st.status else { return None };
        if st.folder_build.is_some() {
            return None;
        }
        let version = version.clone();
        let installer = format!("{}/releases/download/v{version}/{}", base(&self.repo), os::INSTALLER);
        let (state_dir, relaunch) = self.place();
        let i = os::Install { install_dir: &install_dir, state_dir: &state_dir, relaunch: &relaunch };
        let c = os::installer_command(&installer, Some(&version), base(&self.repo), &i);
        st.saved.pending = Some(version.clone());
        st.status = Status::Installing { version, since_ms: now_ms };
        self.save(&st.saved);
        Some(command(c, &log_in(&self.core.dir)))
    }

    /// Install the build in `zip` (the host's Install Build…), checked against `sha256` when given: the core starts
    /// the installer. A sentence for the person: what happens now, or why not.
    pub fn install_build(&self, zip: String, sha256: Option<String>, now_ms: u64) -> Result<String, CoreError> {
        self.install_build_at(Path::new(&zip), sha256.as_deref(), now_ms).map_err(|message| CoreError::Failed { message })
    }

    /// Stop following the watched folder (Settings' Stop Watching).
    pub fn stop_watching(&self) {
        let _ = self.watch_folder(None);
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

    /// The app is up and has run a while (the host calls it 30 s after launch): this version started fine. The
    /// installer's watchdog waits for this, and the next install keeps this app to go back to.
    pub fn started(&self) {
        let mut st = self.lock();
        let tmp = self.core.dir.join(format!("{STARTED_FILE}.tmp"));
        if std::fs::write(&tmp, format!("{}\n", self.current)).is_ok() {
            let _ = std::fs::rename(&tmp, self.core.dir.join(STARTED_FILE));
        }
        if st.saved.unstarted != 0 {
            st.saved.unstarted = 0;
            self.save(&st.saved);
        }
    }

    /// The app quits the proper way (its own quit, SIGTERM included): this launch does not count toward a crash loop,
    /// whether it reached `started` or not.
    pub fn quitting(&self) {
        let mut st = self.lock();
        if st.saved.unstarted != 0 {
            st.saved.unstarted = 0;
            self.save(&st.saved);
        }
    }

    /// The installer's log (`logs/update.log` in the state folder).
    pub fn log_path(&self) -> String {
        log_in(&self.core.dir).display().to_string()
    }
}

/// A way out of a start that failed (the recovery window): its button and the installer to start.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RecoveryAction {
    pub title: String,
    pub help: String,
    pub command: InstallCommand,
}

/// Before the core starts (it may be what fails): counts this launch. The third launch in a row of a development
/// build (`version` `X.Y.Z.N`) that ended neither in `Updates::started` nor in a clean quit (`Updates::quitting`),
/// with an app kept to go back to, returns the
/// command that puts that app back: the host starts it and quits instead of starting. `install_dir` and `relaunch` as
/// for `Updates::new`.
#[uniffi::export]
pub fn launch_guard(state_dir: String, version: String, install_dir: String, relaunch: Vec<String>) -> Option<InstallCommand> {
    let dir = PathBuf::from(&state_dir);
    let file = dir.join(STATE_FILE);
    let (stored, exists) = read_saved(&file);
    if exists && stored.is_none() {
        return None; // unreadable: leave it to the updater
    }
    let mut saved = stored.unwrap_or_default();
    saved.unstarted = saved.unstarted.saturating_add(1);
    let back = (is_development(&version) && saved.unstarted >= UNSTARTED_LIMIT).then(|| kept_in(&dir)).flatten();
    if back.is_some() {
        saved.unstarted = 0;
        saved.pending = None;
    }
    let _ = std::fs::create_dir_all(&dir);
    write_saved(&file, &saved);
    back.map(|(_, installer)| {
        let i = os::Install { install_dir: &install_dir, state_dir: &state_dir, relaunch: &relaunch };
        command(os::rollback_command(&installer.display().to_string(), &i), &log_in(&dir))
    })
}

/// The recovery window's ways out when the core cannot start (any reason), besides Quit: **Go Back to <kept
/// version>** while an app is kept, and **Install the Release** (the latest from GitHub). Neither touches the data.
#[uniffi::export]
pub fn recovery_actions(state_dir: String, install_dir: String, repo_url: Option<String>, relaunch: Vec<String>) -> Vec<RecoveryAction> {
    let dir = PathBuf::from(&state_dir);
    let i = os::Install { install_dir: &install_dir, state_dir: &state_dir, relaunch: &relaunch };
    let mut out = Vec::new();
    if let Some((v, installer)) = kept_in(&dir) {
        out.push(RecoveryAction {
            title: format!("Go Back to {v}"),
            help: format!("Put back {APP_NAME} {}, the last version that started fine; your settings and torrents stay as they are", shown(&v)),
            command: command(os::rollback_command(&installer.display().to_string(), &i), &log_in(&dir)),
        });
    }
    let (repo, _) = resolve_repo(repo_url);
    let url = format!("{}/releases/latest/download/{}", base(&repo), os::INSTALLER);
    out.push(RecoveryAction {
        title: "Install the Release".into(),
        help: format!("Download and install the latest release of {APP_NAME}; your settings and torrents stay as they are"),
        command: command(os::installer_command(&url, None, base(&repo), &i), &log_in(&dir)),
    });
    out
}

#[cfg(test)]
#[path = "updates_tests.rs"]
mod tests;
