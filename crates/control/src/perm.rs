//! Where the current agent permission level comes from. Read on every call, so flipping the
//! setting in the app takes effect on open connections at once.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use trs_state::config::Permission;
use trs_state::{Config, DocStore};

/// A cheap-to-clone source of the current [`Permission`].
#[derive(Clone)]
pub struct PermissionSource(Arc<dyn Fn() -> Permission + Send + Sync>);

impl PermissionSource {
    /// Always `p`.
    pub fn fixed(p: Permission) -> Self {
        Self(Arc::new(move || p))
    }

    /// Ask `f` on every call (keep it cheap).
    pub fn from_fn(f: impl Fn() -> Permission + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    /// A source the app updates: set the level through the returned handle when Settings change.
    pub fn shared(initial: Permission) -> (Self, PermissionHandle) {
        let cell = Arc::new(AtomicU8::new(encode(initial)));
        let c2 = cell.clone();
        (Self::from_fn(move || decode(c2.load(Ordering::Relaxed))), PermissionHandle(cell))
    }

    /// Follow `agents.permission` in `<dir>/config.toml`. The file is re-read at most once per
    /// second; an unreadable or invalid file keeps the last good value (`off` before the first).
    pub fn from_config_dir(dir: impl Into<PathBuf>) -> Self {
        let store = DocStore::<Config>::new(dir.into());
        let cache: Mutex<(Option<Instant>, Permission)> = Mutex::new((None, Permission::Off));
        Self::from_fn(move || {
            let mut g = cache.lock().unwrap_or_else(|p| p.into_inner());
            let stale = g.0.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
            if stale {
                if let Ok(c) = store.read_strict() {
                    g.1 = c.value.agents.permission;
                }
                g.0 = Some(Instant::now());
            }
            g.1
        })
    }

    /// The level right now.
    pub fn get(&self) -> Permission {
        (self.0)()
    }
}

impl std::fmt::Debug for PermissionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PermissionSource({:?})", self.get())
    }
}

/// Setter for [`PermissionSource::shared`].
#[derive(Clone)]
pub struct PermissionHandle(Arc<AtomicU8>);

impl PermissionHandle {
    /// Change the level; open connections see it on their next call.
    pub fn set(&self, p: Permission) {
        self.0.store(encode(p), Ordering::Relaxed);
    }
}

fn encode(p: Permission) -> u8 {
    match p {
        Permission::Off => 0,
        Permission::ReadOnly => 1,
        Permission::Control => 2,
        Permission::Full => 3,
    }
}

fn decode(v: u8) -> Permission {
    match v {
        1 => Permission::ReadOnly,
        2 => Permission::Control,
        3 => Permission::Full,
        _ => Permission::Off,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_updates_are_seen() {
        let (src, h) = PermissionSource::shared(Permission::ReadOnly);
        assert_eq!(src.get(), Permission::ReadOnly);
        h.set(Permission::Full);
        assert_eq!(src.get(), Permission::Full);
        h.set(Permission::Off);
        assert_eq!(src.clone().get(), Permission::Off);
    }

    #[test]
    fn encode_decode_roundtrip() {
        for p in [Permission::Off, Permission::ReadOnly, Permission::Control, Permission::Full] {
            assert_eq!(decode(encode(p)), p);
        }
    }

    #[test]
    fn config_dir_source_reads_and_keeps_last_good() {
        let dir = tempfile::tempdir().unwrap();
        let store = DocStore::<Config>::new(dir.path());
        let mut c = Config::default();
        c.agents.permission = Permission::Control;
        store.save(&c).unwrap();
        let src = PermissionSource::from_config_dir(dir.path());
        assert_eq!(src.get(), Permission::Control);
        // A broken file keeps the last good level (after the 1 s cache).
        std::fs::write(dir.path().join("config.toml"), "this is = not [valid").unwrap();
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(src.get(), Permission::Control);
    }

    #[test]
    fn missing_config_means_off() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(PermissionSource::from_config_dir(dir.path()).get(), Permission::Off);
    }
}
