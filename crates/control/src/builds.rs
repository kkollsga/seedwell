//! Installing builds over the control socket (SPEC 12A, Development builds): `update.status`, `update.install` (a
//! zip handed over), `update.watch` (a folder to follow), `update.dev_builds` (the switch itself) and `update.now` (check and install the
//! latest release). The app's updater does the work and decides what is
//! allowed (the "Allow development builds" switch); this is only where the server finds it, filled in once the
//! updater runs. A daemon has none, and says so.

use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::types::UpdateStatusDto;

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
}

/// Where the control server finds the updater (empty until the app's updater registers itself).
#[derive(Clone, Default)]
pub struct BuildsSlot(Arc<Mutex<Option<Arc<dyn Builds>>>>);

impl BuildsSlot {
    /// An empty slot.
    pub fn new() -> Self {
        Self::default()
    }

    /// The updater from now on (replacing any earlier one).
    pub fn set(&self, b: Arc<dyn Builds>) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(b);
    }

    /// The updater, if one registered.
    pub fn get(&self) -> Option<Arc<dyn Builds>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
}

impl std::fmt::Debug for BuildsSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if self.get().is_some() { "BuildsSlot(set)" } else { "BuildsSlot(empty)" })
    }
}
