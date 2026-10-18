//! Test helpers (no `tempfile` dependency).
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static N: AtomicU64 = AtomicU64::new(0);

/// A unique directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    /// Create `<system temp>/trs-state-<label>-<pid>-<n>`.
    pub fn new(label: &str) -> Self {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("trs-state-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).expect("create temp dir");
        TempDir(p)
    }
    /// The directory.
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
