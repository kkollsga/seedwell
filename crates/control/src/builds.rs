//! Installing builds over the control socket (SPEC 12A, Development builds): `update.status`, `update.install` (a
//! zip handed over), `update.watch` (a folder to follow), `update.dev_builds` (the switch itself) and `update.now` (check and install the
//! latest release), and `restart` (quit and reopen the app, the installer's way). The app's updater does the work and decides what is
//! allowed (the "Allow development builds" switch); this is only where the server finds it, filled in once the
//! updater runs. A daemon has none, and says so.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::types::{RestartDto, UpdateStatusDto};

/// The app's updater, as the control server sees it. Each answer is a sentence for the person or agent who asked; an
/// `Err` is a refusal (the switch is off, the file is not a build, an install is running).
pub trait Builds: Send + Sync {
    /// The running version, the switch, the folder watched and what the updater is doing.
    fn status(&self) -> UpdateStatusDto;
    /// Install the build in `zip` (an absolute path), checked against `sha256` when given.
    fn install(&self, zip: &Path, sha256: Option<&str>) -> Result<String, String>;
    /// Follow `folder` (an absolute path), or stop following one (`None`).
    fn watch(&self, folder: Option<&Path>) -> Result<String, String>;
    /// Turn "Allow development builds" on or off, as its switch in Settings does (off while a development build
    /// runs: the way back to the latest release).
    fn set_dev_builds(&self, on: bool) -> Result<String, String>;
    /// Check for the latest release now and start installing it when one is on offer, as Settings' Check Now and
    /// Install do (with development builds allowed and a folder watched: that folder's newest build). Blocks for the
    /// check (up to about 15 s).
    fn update_now(&self) -> Result<String, String>;
    /// Quit the app and open it again with the same state (`restart`, SPEC 9A.10), the way an update's installer
    /// quits and reopens it: started here, apart from the app, it waits a moment so the answer goes out first. Returns
    /// at once with the sentence (what the restart interrupts) and the log it writes (`pid` and `started_ms` are the
    /// server's to fill in); an `Err` says why it cannot (an install already quits the app, a restart is under way, a
    /// copy without its installer).
    fn restart(&self) -> Result<RestartDto, String>;
    /// A device this one talks to runs `client` (`Seedwell <version>`, from its Hello; folder sync says which devices):
    /// a newer one starts a check for the latest release (SPEC 12A). Returns at once (the check runs apart); called
    /// on folder sync's runtime.
    fn peer_runs(&self, _client: &str) {}
}

/// Where the control server finds the updater (empty until the app's updater registers itself).
#[derive(Clone, Default)]
pub struct BuildsSlot(Arc<Mutex<Slot>>);

#[derive(Default)]
struct Slot {
    builds: Option<Arc<dyn Builds>>,
    /// An app's slot: its updater registers a moment after the server answers (Windows measured ~90 ms, more under
    /// load), so a `restart` or `update.*` in that moment waits for it rather than getting a daemon's answer.
    expected: bool,
}

/// What [`BuildsSlot::wait`] waits at most for an app's updater to register.
pub const STARTING: Duration = Duration::from_secs(10);

/// Why there is no updater to ask.
#[derive(Debug, PartialEq, Eq)]
pub enum NoBuilds {
    /// A daemon: it has none.
    Daemon,
    /// An app whose updater has not registered yet.
    Starting,
}

impl BuildsSlot {
    /// An empty slot, a daemon's.
    pub fn new() -> Self {
        Self::default()
    }

    /// An app's slot: empty until its updater registers, which it will.
    pub fn expected() -> Self {
        Self(Arc::new(Mutex::new(Slot { builds: None, expected: true })))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The updater from now on (replacing any earlier one).
    pub fn set(&self, b: Arc<dyn Builds>) {
        self.lock().builds = Some(b);
    }

    /// The updater, if one registered.
    pub fn get(&self) -> Option<Arc<dyn Builds>> {
        self.lock().builds.clone()
    }

    /// The updater; in an app still starting, waiting up to `limit` for it to register.
    pub async fn wait(&self, limit: Duration) -> Result<Arc<dyn Builds>, NoBuilds> {
        let until = tokio::time::Instant::now() + limit;
        loop {
            let (b, expected) = {
                let s = self.lock();
                (s.builds.clone(), s.expected)
            };
            match b {
                Some(b) => return Ok(b),
                None if !expected => return Err(NoBuilds::Daemon),
                None if tokio::time::Instant::now() >= until => return Err(NoBuilds::Starting),
                None => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    }
}

impl std::fmt::Debug for BuildsSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.get().is_some() { "BuildsSlot(set)" } else { "BuildsSlot(empty)" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;

    impl Builds for Stub {
        fn status(&self) -> UpdateStatusDto {
            UpdateStatusDto::default()
        }
        fn install(&self, _: &Path, _: Option<&str>) -> Result<String, String> {
            Err("stub".into())
        }
        fn watch(&self, _: Option<&Path>) -> Result<String, String> {
            Err("stub".into())
        }
        fn set_dev_builds(&self, _: bool) -> Result<String, String> {
            Err("stub".into())
        }
        fn update_now(&self) -> Result<String, String> {
            Err("stub".into())
        }
        fn restart(&self) -> Result<RestartDto, String> {
            Err("stub".into())
        }
    }

    /// Failure: `seedwell restart` in the moment after an app starts answering, before its updater registered, got the
    /// daemon's "cannot restart itself" (PC, win-101: the socket answers at 1,331 ms, the updater at 1,420 ms).
    #[tokio::test]
    async fn an_app_still_starting_waits_for_its_updater_and_a_daemon_says_at_once() {
        let app = BuildsSlot::expected();
        let later = app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(90)).await;
            later.set(Arc::new(Stub));
        });
        assert!(app.wait(STARTING).await.is_ok(), "the updater that registered a moment later");
        assert_eq!(BuildsSlot::expected().wait(Duration::from_millis(50)).await.err(), Some(NoBuilds::Starting));
        let at = std::time::Instant::now();
        assert_eq!(BuildsSlot::new().wait(STARTING).await.err(), Some(NoBuilds::Daemon));
        assert!(at.elapsed() < Duration::from_secs(1), "a daemon never waits");
    }
}
