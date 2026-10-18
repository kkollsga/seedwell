#![allow(dead_code)]
//! Shared helpers for the integration tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use trs_state::{OpenOptions, Opened, Store};

static N: AtomicU64 = AtomicU64::new(0);

/// A unique scratch directory removed on drop.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let n = N.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("trs-state-it-{label}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TempDir(p)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn fast() -> OpenOptions {
    OpenOptions { debounce: Duration::from_millis(50), ..OpenOptions::default() }
}

pub fn open(dir: &Path) -> Opened {
    Store::open_with(dir, &fast()).expect("open store")
}

pub fn id(n: u32) -> [u8; 20] {
    let mut h = [0u8; 20];
    h[..4].copy_from_slice(&n.to_be_bytes());
    h[19] = 0xAB;
    h
}
