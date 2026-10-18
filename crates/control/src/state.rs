//! Read/write access to the on-disk state the protocol exposes: tracker profiles, rules,
//! statistics and settings. Everything goes through `trs-state`, which validates.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use trs_state::{Config, DocError, DocStore, RuleSet, StateError, Store, TrackerProfiles};

/// Anything that holds an open `trs_state::Store`. redb opens a file once per process, so the
/// control server borrows the engine's store instead of opening its own.
pub trait StoreRef: Send + Sync {
    /// The database.
    fn store(&self) -> &Store;
}

impl StoreRef for Store {
    fn store(&self) -> &Store {
        self
    }
}

/// The persistent state, shared with the engine's own handle to the same files.
#[derive(Clone)]
pub struct StateAccess {
    dir: PathBuf,
    store: Option<Arc<dyn StoreRef>>,
    tz_offset: Arc<dyn Fn() -> i32 + Send + Sync>,
}

impl StateAccess {
    /// `dir` holds `config.toml`, `rules.toml`, `trackers.toml`; `store` is the open database
    /// (the same `Arc` the engine's store adapter uses: redb opens a file once per process).
    pub fn new(dir: impl Into<PathBuf>, store: Option<Arc<dyn StoreRef>>) -> Self {
        Self { dir: dir.into(), store, tz_offset: Arc::new(|| 0) }
    }

    /// Provide the local UTC offset in seconds (for "today" and day boundaries of statistics).
    /// The default is UTC, which is right for the headless daemon and tests.
    pub fn with_tz_offset(mut self, f: impl Fn() -> i32 + Send + Sync + 'static) -> Self {
        self.tz_offset = Arc::new(f);
        self
    }

    /// The state directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The database, when one is open.
    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref().map(|s| s.store())
    }

    /// Current local UTC offset.
    pub fn tz_offset(&self) -> i32 {
        (self.tz_offset)()
    }

    /// `config.toml` (never fails: last good copy or defaults).
    pub fn config(&self) -> Config {
        DocStore::<Config>::new(&self.dir).load().value
    }

    /// Validate and save `config.toml`; the running app picks it up through its watcher.
    pub fn save_config(&self, cfg: &Config) -> Result<(), DocError> {
        DocStore::<Config>::new(&self.dir).save(cfg)
    }

    /// `rules.toml`.
    pub fn rules(&self) -> RuleSet {
        DocStore::<RuleSet>::new(&self.dir).load().value
    }

    /// `trackers.toml`.
    pub fn profiles(&self) -> TrackerProfiles {
        DocStore::<TrackerProfiles>::new(&self.dir).load().value
    }

    /// Validate and save `trackers.toml`; the running engine picks it up through its watcher.
    pub fn save_profiles(&self, set: &TrackerProfiles) -> Result<(), DocError> {
        DocStore::<TrackerProfiles>::new(&self.dir).save(set)
    }

    /// Run a statistics query, or say there is no database.
    pub fn with_store<T>(&self, f: impl FnOnce(&Store) -> Result<T, StateError>) -> Result<T, String> {
        match &self.store {
            Some(s) => f(s.store()).map_err(|e| e.to_string()),
            None => Err("statistics are not available (no state database open)".into()),
        }
    }
}
