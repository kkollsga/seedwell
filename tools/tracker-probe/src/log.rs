//! Append-only JSONL writer that flushes every record, so a crash or
//! SIGKILL never loses more than the record being written.

use crate::record::Record;
use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

/// Cloneable handle for submitting records.
#[derive(Clone)]
pub struct LogTx(mpsc::UnboundedSender<Record>);

impl LogTx {
    /// Queue a record; silently dropped if the writer is gone (shutdown).
    pub fn send(&self, r: Record) {
        let _ = self.0.send(r);
    }
}

/// Start the writer task. Await the returned handle after dropping every
/// `LogTx` to guarantee everything is on disk.
pub fn start(path: &Path) -> Result<(LogTx, JoinHandle<()>)> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    let (tx, mut rx) = mpsc::unbounded_channel::<Record>();
    let handle = tokio::task::spawn_blocking(move || {
        let mut file = file;
        while let Some(rec) = rx.blocking_recv() {
            if let Ok(mut line) = serde_json::to_vec(&rec) {
                line.push(b'\n');
                let _ = file.write_all(&line);
                let _ = file.flush();
                let _ = file.sync_data();
            }
        }
    });
    Ok((LogTx(tx), handle))
}
