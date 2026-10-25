//! The polling backend: a metadata walk of the whole tree every interval, compared with the previous walk.
//! No OS notification API, so it works on any system and on file systems that do not report changes
//! (network shares, some FUSE mounts). It reads directory entries and `lstat`s them; it never opens a file.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::Duration;

use crate::sink::Sink;
use crate::{Hint, RawEvent, RescanReason, ResumeToken, Started};

/// What a walk records per path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Meta {
    pub is_dir: bool,
    pub len: u64,
    /// Modification time in nanoseconds since the epoch (0 if unknown).
    pub mtime: i128,
    /// Inode change time in nanoseconds (0 if unknown).
    pub ctime: i128,
    pub ino: u64,
    pub mode: u32,
}

impl Meta {
    fn of(m: &std::fs::Metadata) -> Meta {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let ns = |s: i64, n: i64| i128::from(s) * 1_000_000_000 + i128::from(n);
            Meta {
                is_dir: m.is_dir(),
                len: m.len(),
                mtime: ns(m.mtime(), m.mtime_nsec()),
                ctime: ns(m.ctime(), m.ctime_nsec()),
                ino: m.ino(),
                mode: m.mode(),
            }
        }
        #[cfg(not(unix))]
        {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as i128);
            Meta { is_dir: m.is_dir(), len: m.len(), mtime, ctime: 0, ino: 0, mode: u32::from(m.permissions().readonly()) }
        }
    }
}

/// Paths under the root (relative) with their metadata. The root itself is not in it.
pub(crate) type Snapshot = HashMap<PathBuf, Meta>;

/// Walks `root` without following symlinks. Unreadable directories are skipped (their contents then look
/// removed, which costs the consumer a stat each, nothing more). Returns `None` if `stop` was set.
pub(crate) fn walk(root: &Path, stop: &AtomicBool) -> Option<Snapshot> {
    let mut snap = Snapshot::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        if stop.load(Ordering::Relaxed) {
            return None;
        }
        let Ok(rd) = std::fs::read_dir(root.join(&rel)) else { continue };
        for ent in rd.flatten() {
            let child = rel.join(ent.file_name());
            let Ok(m) = std::fs::symlink_metadata(root.join(&child)) else { continue };
            let meta = Meta::of(&m);
            if meta.is_dir {
                stack.push(child.clone());
            }
            snap.insert(child, meta);
        }
    }
    Some(snap)
}

/// The events that turn `old` into `new`. A path that disappeared and another that appeared with the same
/// inode are reported as the two halves of a rename.
pub(crate) fn diff(old: &Snapshot, new: &Snapshot) -> Vec<RawEvent> {
    let mut out = Vec::new();
    let removed: Vec<(&PathBuf, &Meta)> = old.iter().filter(|(p, _)| !new.contains_key(*p)).collect();
    let removed_inodes: HashSet<u64> = removed.iter().map(|(_, m)| m.ino).filter(|&i| i != 0).collect();
    let mut created: Vec<(&PathBuf, &Meta)> = Vec::new();
    for (p, m) in new {
        match old.get(p) {
            None => created.push((p, m)),
            Some(o) => {
                if let Some(kind) = changed(o, m) {
                    out.push(RawEvent::Path { rel: p.clone(), kind, is_dir: Some(m.is_dir) });
                }
            }
        }
    }
    let moved = |m: &Meta| m.ino != 0 && created.iter().any(|(_, c)| c.ino == m.ino);
    for (p, m) in &removed {
        let kind = if moved(m) { Hint::Renamed } else { Hint::Removed };
        out.push(RawEvent::Path { rel: (*p).clone(), kind, is_dir: Some(m.is_dir) });
    }
    for (p, m) in &created {
        let kind = if removed_inodes.contains(&m.ino) { Hint::Renamed } else { Hint::Created };
        out.push(RawEvent::Path { rel: (*p).clone(), kind, is_dir: Some(m.is_dir) });
    }
    out
}

/// What changed between two walks of the same path, if anything. A directory's size and times change with
/// its entries, which are reported themselves, so only its mode and inode count.
fn changed(o: &Meta, n: &Meta) -> Option<Hint> {
    if o.is_dir != n.is_dir || o.ino != n.ino {
        return Some(Hint::Created);
    }
    if n.is_dir {
        return (o.mode != n.mode).then_some(Hint::Meta);
    }
    if o.len != n.len || o.mtime != n.mtime {
        return Some(Hint::Modified);
    }
    (o.mode != n.mode || o.ctime != n.ctime).then_some(Hint::Meta)
}

/// The root's identity, to notice it being replaced.
fn root_id(root: &Path) -> Option<(u64, u64)> {
    let m = std::fs::metadata(root).ok().filter(|m| m.is_dir())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((m.dev(), m.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = m;
        Some((0, 0))
    }
}

pub(crate) fn start(root: PathBuf, interval: Duration, since: Option<ResumeToken>) -> io::Result<Started> {
    let (mut sink, events) = Sink::new();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let (stop_tx, stop_rx) = channel::<()>();
    // The first walk happens before `start` returns, so a change made right after `watch` is seen.
    let mut id = root_id(&root).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the root is not a directory"))?;
    let mut snap = walk(&root, &stop_flag).unwrap_or_default();
    if since.is_some() {
        sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::NoHistory });
        sink.send(RawEvent::HistoryDone);
    }
    let flag = stop_flag.clone();
    let thread = std::thread::Builder::new().name("fswatch-poll".into()).spawn(move || {
        let mut present = true;
        loop {
            match stop_rx.recv_timeout(interval) {
                Err(RecvTimeoutError::Timeout) => {}
                _ => return,
            }
            sink.flush();
            match root_id(&root) {
                None => {
                    if present {
                        present = false;
                        snap.clear();
                        if !sink.send(RawEvent::RootChanged) {
                            return;
                        }
                    }
                    continue;
                }
                Some(now_id) if !present || now_id != id => {
                    // Back, or replaced: the consumer rescans everything; start a fresh baseline.
                    present = true;
                    id = now_id;
                    let Some(s) = walk(&root, &flag) else { return };
                    snap = s;
                    if !sink.send(RawEvent::RootChanged) {
                        return;
                    }
                    continue;
                }
                Some(_) => {}
            }
            let Some(next) = walk(&root, &flag) else { return };
            for ev in diff(&snap, &next) {
                if !sink.send(ev) {
                    return;
                }
            }
            snap = next;
        }
    })?;
    let stop = Box::new(move || {
        stop_flag.store(true, Ordering::Relaxed);
        drop(stop_tx);
    });
    Ok(Started { events, stop, thread })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(ino: u64, len: u64, mtime: i128) -> Meta {
        Meta { is_dir: false, len, mtime, ctime: mtime, ino, mode: 0o644 }
    }

    fn dir(ino: u64) -> Meta {
        Meta { is_dir: true, len: 64, mtime: 1, ctime: 1, ino, mode: 0o755 }
    }

    fn snap(v: &[(&str, Meta)]) -> Snapshot {
        v.iter().map(|(p, m)| (PathBuf::from(p), *m)).collect()
    }

    fn sorted(mut v: Vec<RawEvent>) -> Vec<(String, Hint)> {
        let mut out: Vec<(String, Hint)> = v
            .drain(..)
            .filter_map(|e| match e {
                RawEvent::Path { rel, kind, .. } => Some((rel.to_string_lossy().into_owned(), kind)),
                _ => None,
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    #[test]
    fn diff_reports_create_modify_remove_meta() {
        let old = snap(&[("a", meta(1, 10, 5)), ("b", meta(2, 10, 5)), ("c", meta(3, 1, 1)), ("d", dir(4))]);
        let mut cm = meta(3, 1, 1);
        cm.mode = 0o600;
        cm.ctime = 9;
        let mut dm = dir(4);
        dm.mtime = 99;
        dm.len = 128;
        let new = snap(&[("a", meta(1, 11, 6)), ("c", cm), ("d", dm), ("e", meta(5, 0, 0))]);
        assert_eq!(
            sorted(diff(&old, &new)),
            vec![
                ("a".into(), Hint::Modified),
                ("b".into(), Hint::Removed),
                ("c".into(), Hint::Meta),
                ("e".into(), Hint::Created),
            ]
        );
    }

    #[test]
    fn same_inode_gone_and_appeared_is_a_rename() {
        let old = snap(&[("x/old", meta(7, 3, 3)), ("gone", meta(8, 1, 1))]);
        let new = snap(&[("y/new", meta(7, 3, 3)), ("fresh", meta(9, 1, 1))]);
        assert_eq!(
            sorted(diff(&old, &new)),
            vec![
                ("fresh".into(), Hint::Created),
                ("gone".into(), Hint::Removed),
                ("x/old".into(), Hint::Renamed),
                ("y/new".into(), Hint::Renamed),
            ]
        );
    }

    #[test]
    fn a_replaced_path_is_reported_once() {
        let old = snap(&[("f", meta(1, 3, 3)), ("d", dir(2))]);
        let new = snap(&[("f", meta(11, 3, 3)), ("d", meta(12, 0, 0))]);
        assert_eq!(sorted(diff(&old, &new)), vec![("d".into(), Hint::Created), ("f".into(), Hint::Created)]);
    }

    #[test]
    fn unchanged_trees_produce_nothing() {
        let s = snap(&[("a", meta(1, 1, 1)), ("d", dir(2)), ("d/b", meta(3, 2, 2))]);
        assert!(diff(&s, &s).is_empty());
    }

    #[test]
    fn walk_lists_nested_entries_without_following_symlinks() -> io::Result<()> {
        let t = tempfile::tempdir()?;
        std::fs::create_dir_all(t.path().join("d/e"))?;
        std::fs::write(t.path().join("d/e/f"), b"x")?;
        let outside = tempfile::tempdir()?;
        std::fs::write(outside.path().join("secret"), b"y")?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), t.path().join("link"))?;
        let s = walk(t.path(), &AtomicBool::new(false)).unwrap_or_default();
        assert!(s.contains_key(Path::new("d")));
        assert!(s.contains_key(Path::new("d/e/f")));
        assert!(!s.keys().any(|p| p.ends_with("secret")));
        assert!(walk(t.path(), &AtomicBool::new(true)).is_none());
        Ok(())
    }
}
