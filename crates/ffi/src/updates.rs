//! App updates from GitHub releases (SPEC 12A), for every host: the `latest.json` a release carries, whether it is
//! on offer, when to check (once a day, a minute after launch at the soonest), when to install by itself, the
//! installer command, what is remembered between launches (`updates.json` in the state folder) and every text of
//! Settings › General › Updates. The host runs a timer at `next_due_ms`, calls `check` off its main thread, starts
//! the command `install` returns (detached, output to `log_path`) and draws `model`. What differs per system (the
//! installer, the zip, this system's entry in `latest.json`) is `seedwell_platform::update`'s.
//!
//! Development builds (SPEC 12A): with "Allow development builds" on, a build handed over (`seedwell update
//! install`, the host's Install Build…) or one that appears in a watched folder (read on file-change notifications,
//! `seedwell-fswatch`, never on a timer) is installed; the core starts that installer itself. A watched folder may be
//! shared with others (Folder Sync), so a build there is installed only when its `build.json` is signed with a build
//! key this copy trusts (`seedwell-buildsig`) and the zip is the one signed. Every install keeps the app it replaces (when that one started fine) and goes back to it when the
//! new one does not start (`started`, `launch_guard`, `recovery_actions`); the installer does that part.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use seedwell_platform::update as os;

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
/// How long an installer may still be at work after it started: quitting every Seedwell (up to 30 s each), unpacking,
/// and its own wait of up to 2 minutes for the new app to start (`install.sh`, `install.ps1`). A Seedwell of another
/// version that starts meanwhile (one the system or a person opened) says the install is under way, not that it
/// failed.
const INSTALL_RUNS_MS: u64 = 5 * 60 * 1000;
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
/// A check started by a device that runs a newer Seedwell ([`Updates::peer_runs`]) comes at most once in this time.
const PEER_CHECK_EVERY_MS: u64 = 3600 * 1000;
/// The versions devices claimed that started a check, remembered for the run (the newest this many).
const PEER_CLAIMS_KEEP: usize = 32;

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
    /// `here`: this app started the installer (it is quit within seconds); else it found the install under way at
    /// launch (another version, opened while the installer runs).
    Installing { version: String, since_ms: u64, here: bool },
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
    /// When that install started (Unix ms): until [`INSTALL_RUNS_MS`] after it, another version that starts is one
    /// the installer has not replaced yet. `None` from a version before this was kept: it is judged at once.
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_since_ms: Option<u64>,
    /// The installer's process, when the core started it (a build): once it has ended, nothing is under way any more.
    #[serde(skip_serializing_if = "Option::is_none")]
    pending_pid: Option<u32>,
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

/// The pending install's installer may still be at work at `now_ms`: it started less than [`INSTALL_RUNS_MS`] ago
/// and, when its process is known, that process still runs.
fn under_way(saved: &Saved, now_ms: u64) -> bool {
    saved.pending_since_ms.is_some_and(|since| now_ms >= since && now_ms - since < INSTALL_RUNS_MS)
        && saved.pending_pid.is_none_or(seedwell_platform::running::alive)
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

/// This system's build in a watched folder's `build.json`, once the manifest verified: the version, the zip's name
/// and checksum as signed, and this system is one it runs on. `Err`: why it is not this system's build.
pub(crate) fn folder_entry(b: &seedwell_buildsig::Build, zip_for: impl Fn(&str) -> String) -> Result<Latest, String> {
    if version_order(&b.version).is_none() || b.zip != zip_for(&b.version) {
        return Err(format!("its {} names {} for this system, not a build of {APP_NAME} for it", seedwell_buildsig::FILE, b.zip));
    }
    let minimum_system = match &b.minimum_system {
        None => None,
        Some(m) => Some(parse_system_version(m).ok_or_else(|| format!("its {} names no readable minimum system", seedwell_buildsig::FILE))?),
    };
    Ok(Latest { version: b.version.clone(), sha256: b.sha256.clone(), notes_url: None, minimum_system })
}

/// The build keys a watched folder's `build.json` must be signed with: the ones compiled in (`seedwell-buildsig`); an
/// isolated (test) run also trusts the key in `SEEDWELL_TEST_BUILD_KEY` (its own test key: the user's app never
/// reads it), and unit tests their fixed test key.
fn build_keys(isolated: bool) -> Vec<seedwell_buildsig::PublicKey> {
    let mut keys = seedwell_buildsig::trusted_keys();
    if isolated && let Some(k) = std::env::var("SEEDWELL_TEST_BUILD_KEY").ok().and_then(|k| seedwell_buildsig::parse_key(&k)) {
        keys.push(k);
    }
    #[cfg(test)]
    keys.push(tests::test_key().public_key());
    keys
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
    /// When the last check a newer device started ran ([`PEER_CHECK_EVERY_MS`]).
    peer_check_ms: Option<u64>,
    /// The versions devices claimed that started a check: each starts one at most in a run.
    peer_claims: Vec<(u32, u32, u32, u32)>,
    /// The app is quitting (`quitting`): a check a device started installs nothing any more.
    quitting: bool,
    /// When `restart` started the installer's restart (a second one within [`INSTALL_RUNS_MS`] is refused).
    restart_ms: Option<u64>,
}

/// The watched folder's watcher: its consumer thread reads the folder on each notification and ends when the guard
/// is dropped (the channel closes).
struct FolderWatch {
    folder: PathBuf,
    _guard: Option<seedwell_fswatch::WatchGuard>,
}

/// An installer to start so that it outlives the app, without a window, its output appended to `log`
/// (`seedwell_platform::update::Command` says how on each system).
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
    /// The keys a watched folder's build must be signed with (`build_keys`).
    build_keys: Vec<seedwell_buildsig::PublicKey>,
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
                st.status = Self::restored(&mut st.saved.clone(), &self.current, now_ms);
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

    /// The status after a restore at `now_ms`: a pending install either worked (this is its version), is still under
    /// way (another version started while its installer may still be at work: said as installing, nothing decided),
    /// or did not work (a development build that did not is remembered as failed); else what the last check found.
    fn restored(saved: &mut Saved, current: &str, now_ms: u64) -> Status {
        if let (Some(p), Some(since)) = (&saved.pending, saved.pending_since_ms)
            && *p != current
            && under_way(saved, now_ms)
        {
            return Status::Installing { version: p.clone(), since_ms: since, here: false };
        }
        saved.pending_since_ms = None;
        saved.pending_pid = None;
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
            .block_on(seedwell_http::get_small(&url, &self.current, MAX_LATEST_BYTES, FETCH_TIMEOUT))
            .map_err(|e| match e {
                seedwell_http::Error::Timeout => "timed out".to_owned(),
                seedwell_http::Error::InvalidUrl(_) => "bad address".to_owned(),
                seedwell_http::Error::BadResponse(_) => "unreadable release information".to_owned(),
                seedwell_http::Error::Network(m) if m.contains("dns") || m.contains("resolve") => "server not found".to_owned(),
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

    /// An install started more than the watchdog's time ago, with the app that started it still running, or one this
    /// app found under way at launch whose installer ended (or ran out of time) while this app runs on: it did not
    /// work.
    fn expire_install(&self, st: &mut St, now_ms: u64) {
        if let Status::Installing { version, since_ms, here } = &st.status {
            let over = if *here { now_ms.saturating_sub(*since_ms) >= INSTALL_WATCHDOG_MS } else { !under_way(&st.saved, now_ms) };
            if over {
                // The new version may have started fine meanwhile (it says so in launch-ok) while this one ran on.
                let started = std::fs::read_to_string(self.core.dir.join(STARTED_FILE)).ok().and_then(|t| t.lines().next().map(str::to_owned));
                st.status = if started.as_deref() == Some(version.as_str()) { Status::Updated(version.clone()) } else { Status::InstallFailed(version.clone()) };
                st.saved.pending = None;
                st.saved.pending_since_ms = None;
                st.saved.pending_pid = None;
                self.save(&st.saved);
            }
        }
    }

    fn place(&self) -> (String, Vec<String>) {
        (self.core.dir.display().to_string(), self.relaunch.clone())
    }

    /// Starts the running app's installer on `zip` (already checked: `sha256` is its checksum) for `version`.
    /// `watched`: a build from a watched folder, which on the Mac must also be signed with Seedwell's Developer ID
    /// and carry the app's bundle id (not checked in an isolated test run, whose test builds are signed ad hoc).
    fn start_build(&self, st: &mut St, version: &str, zip: &Path, sha256: &str, watched: bool, now_ms: u64) -> Result<(), String> {
        if self.installer.is_empty() || !Path::new(&self.installer).is_file() {
            return Err("this copy of Seedwell has no installer to install builds with".into());
        }
        let (state_dir, relaunch) = self.place();
        let i = os::Install { install_dir: &self.install_dir, state_dir: &state_dir, relaunch: &relaunch };
        let c = os::local_installer_command(&self.installer, &zip.display().to_string(), sha256, watched && !self.core.isolated, &i);
        st.saved.pending = Some(version.to_owned());
        st.saved.pending_since_ms = Some(now_ms);
        self.save(&st.saved);
        match os::spawn(&c, &log_in(&self.core.dir)) {
            Ok(pid) => {
                st.saved.pending_pid = Some(pid);
                self.save(&st.saved);
            }
            Err(e) => {
                st.saved.pending = None;
                st.saved.pending_since_ms = None;
                self.save(&st.saved);
                st.status = Status::InstallFailed(version.to_owned());
                return Err(format!("the installer did not start: {e}"));
            }
        }
        st.status = Status::Installing { version: version.to_owned(), since_ms: now_ms, here: true };
        st.folder_build = None;
        Ok(())
    }

    /// The watched folder's build, when its `build.json` is signed with a trusted build key, names a build for this
    /// system that is newer than the running one and did not fail before, and the zip there is the one signed. `Err`:
    /// why nothing there can be installed (said in the status line).
    fn read_folder(&self, folder: &Path) -> Result<Option<FolderBuild>, String> {
        use seedwell_buildsig::FILE;
        let doc = folder.join(FILE);
        let data = match std::fs::metadata(&doc) {
            Ok(m) if m.len() as usize > seedwell_buildsig::MAX_BYTES => return Err(format!("its {FILE} is too large")),
            Ok(_) => std::fs::read(&doc).map_err(|e| format!("its {FILE} cannot be read ({e})"))?,
            Err(_) if !folder.is_dir() => return Err(format!("{} is missing", folder.display())),
            Err(_) => return Err(format!("no build there yet (no {FILE})")),
        };
        if self.build_keys.is_empty() {
            return Err(format!("this copy of {APP_NAME} trusts no build key, so it installs no build from a folder"));
        }
        let (m, _) = seedwell_buildsig::verify(&data, &self.build_keys).map_err(|e| format!("{e}: nothing is installed from it"))?;
        let Some(b) = m.builds.get(os::BUILD_SYSTEM) else { return Err("no build for this system there yet".into()) };
        let l = folder_entry(b, os::release_zip)?;
        let zip = folder.join(&b.zip);
        {
            let st = self.lock();
            if !l.offered(&self.current, &self.system) || st.saved.failed.contains(&l.version) {
                return Ok(None);
            }
        }
        // The zip may still be arriving (a synced folder brings the files one by one): the next notification looks
        // again. A zip that is not the one signed is never installed.
        match std::fs::metadata(&zip) {
            Ok(m) if m.is_file() && m.len() <= MAX_BUILD_BYTES => {}
            _ => return Err(format!("waiting for {} (named in {FILE})", b.zip)),
        }
        let have = sha256_of(&zip).map_err(|e| format!("{} cannot be read ({e})", b.zip))?;
        if have != l.sha256 {
            return Err(format!("{} is not the build {FILE} signed (yet): waiting", b.zip));
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
                    // A zip that changed since it was checked is refused by the installer's checksum (of its own copy).
                    let _ = self.start_build(&mut st, &b.version, &b.zip, &b.sha256, true, now_ms);
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
        let (events, guard) = match seedwell_fswatch::watch(&folder, seedwell_fswatch::WatchOptions::default()) {
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
    let t = seedwell_control::display::ago(at_ms / 1000, now_ms / 1000, false);
    if t == "Yesterday" { "yesterday".into() } else { t }
}

fn unix_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The control server's view of the updater (`update.*`).
struct Hook(Weak<Updates>);

impl seedwell_control::Builds for Hook {
    fn status(&self) -> seedwell_control::types::UpdateStatusDto {
        let Some(u) = self.0.upgrade() else { return Default::default() };
        let m = u.model_now(unix_ms());
        seedwell_control::types::UpdateStatusDto {
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

    fn set_dev_builds(&self, on: bool) -> Result<String, String> {
        let u = self.0.upgrade().ok_or("Seedwell is quitting")?;
        u.set_dev_builds(on, unix_ms())
    }

    fn update_now(&self) -> Result<String, String> {
        let u = self.0.upgrade().ok_or("Seedwell is quitting")?;
        u.update_now(unix_ms())
    }

    fn restart(&self) -> Result<seedwell_control::types::RestartDto, String> {
        let u = self.0.upgrade().ok_or("Seedwell is quitting")?;
        u.restart(unix_ms())
    }

    fn peer_runs(&self, client: &str) {
        if let Some(u) = self.0.upgrade() {
            let _ = u.peer_runs(client, unix_ms());
        }
    }
}

impl Updates {
    fn gate(&self) -> Result<(), String> {
        if self.prefs().dev_builds {
            Ok(())
        } else {
            Err("development builds are off: turn on Settings › General › Updates › Allow development builds (or, when the user asks, `seedwell update dev-builds on`); ask the user with `seedwell agents request dev-builds --why \"<what for>\"`".into())
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
        self.start_build(&mut st, &version, zip, &have, false, now_ms)?;
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

    /// "Allow development builds" on or off for `update.dev_builds`, the way the switch in Settings sets it: the
    /// `[updates]` switch saved in `config.toml`. The app follows the saved file as it follows its own switch (the
    /// engine reports the change, the app's updates schedule takes it up: off while a development build runs, it
    /// checks GitHub at once and installs the latest release). The core's own part happens here at once, so the
    /// answer and `update.status` already say it: the watched folder followed or let go, the way back noted.
    fn set_dev_builds(&self, on: bool, now_ms: u64) -> Result<String, String> {
        let mut s = self.core.settings_load();
        let was = s.updates.dev_builds;
        if was != on {
            s.updates.dev_builds = on;
            self.core.settings_save(s).map_err(|e| e.to_string())?;
        }
        self.sync_watch();
        let back = {
            let mut st = self.lock();
            self.note_switch(&mut st, now_ms);
            st.saved.back_to_release_ms.is_some()
        };
        Ok(match (on, was == on, back) {
            (true, true, _) => "Development builds were already allowed".into(),
            (true, false, _) => "Development builds are allowed now: `seedwell update install <zip>` (or a watched folder) installs one".into(),
            (false, _, true) => format!(
                "Development builds are off: {APP_NAME} {} goes back to the latest release, which it checks for and installs now (although it is older)",
                shown(&self.current)
            ),
            (false, true, false) => "Development builds were already off".into(),
            (false, false, false) => "Development builds are off".into(),
        })
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
                st.status = Self::restored(&mut st.saved.clone(), &self.current, unix_ms());
            }
        }
        self.sync_watch();
        Ok(match folder {
            Some(f) => format!(
                "Watching {}: each newer build signed into its {} with the build key is installed as it appears (tools/dev-build.sh --to, tools/build-sign.sh add)",
                f.display(),
                seedwell_buildsig::FILE
            ),
            None => "Stopped watching".into(),
        })
    }
}

impl Updates {
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

    /// Starts the install of the release on offer into `install_dir` (the folder that holds the app): the command
    /// for the host to start detached. `None` when nothing is on offer, it is a watched folder's build (the core
    /// installs those) or an install runs. The next launch reports whether it worked; if the app still runs after
    /// 90 s, the model says it failed.
    pub fn install(&self, install_dir: String, now_ms: u64) -> Option<InstallCommand> {
        let mut st = self.lock();
        let (_, c) = self.release_install(&mut st, &install_dir, now_ms)?;
        // The host starts it: its process is not known here, so only the installer's time says whether it may run.
        st.saved.pending_pid = None;
        self.save(&st.saved);
        Some(command(c, &log_in(&self.core.dir)))
    }

    /// The installer of the release on offer (its version and command), noted as started; `None` when nothing is on
    /// offer or it is a watched folder's build (the core installs those).
    fn release_install(&self, st: &mut St, install_dir: &str, now_ms: u64) -> Option<(String, os::Command)> {
        let Status::Available { version, .. } = &st.status else { return None };
        if st.folder_build.is_some() {
            return None;
        }
        let version = version.clone();
        let installer = format!("{}/releases/download/v{version}/{}", base(&self.repo), os::INSTALLER);
        let (state_dir, relaunch) = self.place();
        let i = os::Install { install_dir, state_dir: &state_dir, relaunch: &relaunch };
        let c = os::installer_command(&installer, Some(&version), base(&self.repo), &i);
        st.saved.pending = Some(version.clone());
        st.saved.pending_since_ms = Some(now_ms);
        st.status = Status::Installing { version: version.clone(), since_ms: now_ms, here: true };
        Some((version, c))
    }

    /// `update.now` (an agent at permission full): one check, as Check Now (with development builds allowed and a
    /// folder watched: that folder, whose newer build the check installs), then the install of the release on offer,
    /// as Install, the core starting the installer itself. Development builds off while one runs: the way back to the
    /// latest release, as the switch does. Blocks for the check (up to 15 s). A sentence for the agent: what happens,
    /// or why nothing does.
    fn update_now(&self, now_ms: u64) -> Result<String, String> {
        {
            let st = self.lock();
            if let Status::Installing { version, .. } = &st.status {
                return Ok(format!("Installing {} already", shown(version)));
            }
        }
        self.check(false, now_ms);
        let mut st = self.lock();
        let text = self.status_text(&st, now_ms);
        if let Status::CheckFailed(why) = &st.status {
            return Err(format!("the check failed: {why}"));
        }
        if !matches!(st.status, Status::Available { .. }) || st.folder_build.is_some() {
            // Up to date, nothing published, or a watched folder's build the check started installing.
            return Ok(text);
        }
        let Some(started) = self.start_release(&mut st, now_ms) else { return Ok(text) };
        let version = started?;
        Ok(format!("Installing {}: {APP_NAME} quits, is replaced and reopens (log: {})", shown(&version), log_in(&self.core.dir).display()))
    }

    /// `restart` (an agent at permission full, `seedwell restart`): the app quits and opens again with the same state,
    /// by the way every install quits and reopens it: the app's own installer, started here apart from the app (as
    /// for a handed-over build), in its restart mode, which replaces nothing, waits a second (the answer goes out
    /// first), quits this process the proper way (SIGTERM on the Mac, `--quit` on Windows: trackers are told
    /// `stopped`) and opens the app again with the relaunch arguments. The hosts add nothing: each already quits
    /// properly for an installer. Like Install, it does not wait for downloads or a move; the answer says what it
    /// interrupts. Refused while an install runs (that one already quits and reopens the app), while the app quits,
    /// within [`INSTALL_RUNS_MS`] of another restart, and in a copy without its installer.
    fn restart(&self, now_ms: u64) -> Result<seedwell_control::types::RestartDto, String> {
        if self.installer.is_empty() || !Path::new(&self.installer).is_file() {
            return Err(format!(
                "this copy of {APP_NAME} has no installer to quit and reopen it with (a build run from its build folder); ask the user to quit and open it"
            ));
        }
        let interrupts = self.interrupts();
        let mut st = self.lock();
        if st.quitting {
            return Err(format!("{APP_NAME} is quitting already"));
        }
        if let Status::Installing { version, .. } = &st.status {
            return Err(format!("installing {} already: {APP_NAME} quits and reopens with it", shown(version)));
        }
        if let Some(at) = st.restart_ms
            && now_ms.saturating_sub(at) < INSTALL_RUNS_MS
        {
            return Err(format!("a restart is already under way (asked {})", ago(at, now_ms)));
        }
        let (state_dir, relaunch) = self.place();
        let i = os::Install { install_dir: &self.install_dir, state_dir: &state_dir, relaunch: &relaunch };
        let c = os::restart_command(&self.installer, std::process::id(), &i);
        let log = log_in(&self.core.dir);
        os::spawn(&c, &log).map_err(|e| format!("the restart did not start: {e}"))?;
        st.restart_ms = Some(now_ms);
        let interrupts = if interrupts.is_empty() { String::new() } else { format!("; this interrupts {}", interrupts.join(" and ")) };
        Ok(seedwell_control::types::RestartDto {
            note: format!("Restarting {APP_NAME}: it quits and opens again with the same settings, torrents, folders and chats{interrupts}"),
            log: log.display().to_string(),
            ..Default::default()
        })
    }

    /// What quitting now interrupts, in words for `restart`'s answer (the work an automatic install waits for,
    /// [`Updates::busy`]): downloads, which go on when the app is open again, and a move of files, which the quit
    /// cancels.
    fn interrupts(&self) -> Vec<String> {
        let s = self.core.snapshot();
        let n = s.downloading.iter().chain(&s.hidden_downloading).filter(|r| !row_is_held((*r).clone())).count();
        let mut out = Vec::new();
        match n {
            0 => {}
            1 => out.push(format!("1 download (it goes on when {APP_NAME} is open again)")),
            n => out.push(format!("{n} downloads (they go on when {APP_NAME} is open again)")),
        }
        if let Some(p) = s.moving_percent {
            out.push(format!("a move of files at {p} % (cancelled: the files stay where they were; move them again after)"));
        }
        out
    }

    /// Starts the installer of the release on offer itself, detached, into the app's own folder (`update.now`, a check
    /// a newer device started): its version, or why it did not start. `None`: nothing to install (nothing on offer, a
    /// watched folder's build, an install running).
    fn start_release(&self, st: &mut St, now_ms: u64) -> Option<Result<String, String>> {
        let install_dir = self.install_dir.clone();
        let (version, c) = self.release_install(st, &install_dir, now_ms)?;
        self.save(&st.saved);
        Some(match os::spawn(&c, &log_in(&self.core.dir)) {
            Ok(pid) => {
                st.saved.pending_pid = Some(pid);
                self.save(&st.saved);
                Ok(version)
            }
            Err(e) => {
                st.saved.pending = None;
                st.saved.pending_since_ms = None;
                st.status = Status::InstallFailed(version);
                self.save(&st.saved);
                Err(format!("the installer did not start: {e}"))
            }
        })
    }

    /// A device this one talks to runs `client` (`Seedwell <version>`, from its Hello; folder sync tells only of
    /// devices it set up a session with and of members it refused, SPEC 12A). A plain version newer than this one
    /// starts a check, on a thread of its own, instead of waiting for the daily one: in a release build with Check
    /// automatically on and no folder watched; at most once an hour; never while a check or an install runs, within a
    /// failed check's back-off or once the app quits; each claimed version once a run; and not when a check of this
    /// run already found that version or a newer one. Within a minute of launch it waits for that minute, as the first
    /// automatic check does. The claim only starts the check: what is offered and installed is GitHub's latest
    /// release, as always, so a device claiming a version that does not exist costs one check. With Install
    /// automatically on, what the check finds is installed at once ([`Updates::peer_check`]); else it waits on offer
    /// (the menu bar's row, Settings). The started thread, or `None`.
    pub(crate) fn peer_runs(&self, client: &str, now_ms: u64) -> Option<std::thread::JoinHandle<()>> {
        let (a, b, c) = parse_version(client.strip_prefix(APP_NAME)?.strip_prefix(' ')?)?;
        let claim = (a, b, c, 0);
        if version_order(&self.current).is_none_or(|current| claim <= current) {
            return None;
        }
        // (The switches are read from the settings file, so before the lock.)
        if !self.automatic() || self.watched().is_some() {
            return None;
        }
        let wait_ms = {
            let mut st = self.lock();
            let recent = st.peer_check_ms.is_some_and(|t| now_ms.abs_diff(t) < PEER_CHECK_EVERY_MS);
            let found = st.saved.last_check_ms.is_some_and(|l| l >= st.launched_ms)
                && matches!(&st.status, Status::Available { version, .. } if version_order(version).is_some_and(|v| v >= claim));
            let backing_off = st.saved.last_attempt_ms.is_some_and(|at| at <= now_ms && now_ms - at < retry_after(st.saved.failures));
            if st.quitting || st.checking || matches!(st.status, Status::Installing { .. }) || recent || found || backing_off || st.peer_claims.contains(&claim) {
                return None;
            }
            st.peer_check_ms = Some(now_ms);
            st.peer_claims.push(claim);
            let over = st.peer_claims.len().saturating_sub(PEER_CLAIMS_KEEP);
            st.peer_claims.drain(..over);
            (st.launched_ms + FIRST_CHECK_DELAY_MS).saturating_sub(now_ms)
        };
        eprintln!("seedwell: a device runs {client}, newer than {}: checking for an update now", shown(&self.current));
        let me = self.me.clone();
        std::thread::Builder::new()
            .name("seedwell-peer-update".into())
            .spawn(move || {
                if wait_ms > 0 {
                    std::thread::sleep(Duration::from_millis(wait_ms));
                }
                if let Some(u) = me.upgrade() {
                    u.peer_check(now_ms + wait_ms);
                }
            })
            .ok()
    }

    /// The check a newer device started ([`Updates::peer_runs`]), silent as an automatic one, then the install of the
    /// release it found when the app would install it by itself now ([`Updates::should_install`]: both switches on,
    /// nothing downloading or being moved), unless the app is quitting meanwhile.
    fn peer_check(&self, now_ms: u64) {
        self.check(true, now_ms);
        if !self.should_install() {
            return;
        }
        let mut st = self.lock();
        if st.quitting {
            return;
        }
        if let Some(Err(why)) = self.start_release(&mut st, now_ms) {
            eprintln!("seedwell: the update a newer device pointed to did not start: {why}");
        }
    }

    /// Install the build in `zip` (the host's Install Build…), checked against `sha256` when given: the core starts
    /// the installer. A sentence for the person: what happens now, or why not.
    pub fn install_build(&self, zip: String, sha256: Option<String>, now_ms: u64) -> Result<String, CoreError> {
        self.install_build_at(Path::new(&zip), sha256.as_deref(), now_ms).map_err(|message| CoreError::Failed { message })
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
        let status = Self::restored(&mut saved, &current, now_ms);
        let dev_on = core.settings_load().updates.dev_builds;
        let build_keys = build_keys(core.isolated);
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
                peer_check_ms: None,
                peer_claims: Vec::new(),
                quitting: false,
                restart_ms: None,
            }),
            watch: Mutex::new(None),
            build_keys,
            me: me.clone(),
        });
        {
            let st = u.lock();
            if st.saved != before {
                u.save(&st.saved);
            }
        }
        u.core.builds.set(Arc::new(Hook(Arc::downgrade(&u))));
        u.core.host_facts.set_running(&u.current);
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
        st.status = Self::restored(&mut saved, &self.current, unix_ms());
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

    /// One look for an update (the FFI's `check`: [`Updates::check`] off the caller's thread; a request may take
    /// seconds). The host reads the `model` again when it is done.
    #[uniffi::method(name = "check")]
    pub async fn check_async(self: Arc<Self>, silent: bool, now_ms: u64) {
        crate::calls::apart(move || self.check(silent, now_ms)).await
    }

    /// Starts the install of the release on offer ([`Updates::install`]): the command for the host to start.
    #[uniffi::method(name = "install")]
    pub async fn install_async(self: Arc<Self>, install_dir: String, now_ms: u64) -> Option<InstallCommand> {
        crate::calls::apart(move || self.install(install_dir, now_ms)).await
    }

    /// Installs the build in `zip` ([`Updates::install_build`]; its checksum is read first).
    #[uniffi::method(name = "install_build")]
    pub async fn install_build_async(self: Arc<Self>, zip: String, sha256: Option<String>, now_ms: u64) -> Result<String, CoreError> {
        crate::calls::apart(move || self.install_build(zip, sha256, now_ms)).await
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
            st.saved.pending_since_ms = None;
            st.saved.pending_pid = None;
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
        st.quitting = true;
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
        saved.pending_since_ms = None;
        saved.pending_pid = None;
    }
    let _ = std::fs::create_dir_all(&dir);
    write_saved(&file, &saved);
    back.map(|(_, installer)| {
        let i = os::Install { install_dir: &install_dir, state_dir: &state_dir, relaunch: &relaunch };
        command(os::rollback_command(&installer.display().to_string(), &i), &log_in(&dir))
    })
}

/// The words of the window that says the app cannot start (the core failed, so there is no core to ask): its title and
/// Quit, before the recovery actions (`recovery_actions`); the core's error is its text.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StartFailureTexts {
    /// "Seedwell could not start"
    pub title: String,
    pub quit_title: String,
    pub quit_help: String,
    /// The title when the app was started with arguments it refuses (a development or QA run), before any core.
    pub refused_title: String,
}

/// The texts of the start failure window. Needs no core: it is what the host shows when the core failed.
#[uniffi::export]
pub fn start_failure_texts() -> StartFailureTexts {
    StartFailureTexts {
        title: format!("{APP_NAME} could not start"),
        quit_title: "Quit".into(),
        quit_help: format!("Quit {APP_NAME}; nothing is changed"),
        refused_title: "Cannot start".into(),
    }
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
