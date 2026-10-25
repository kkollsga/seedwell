//! Linux inotify backend, through libc.
//!
//! inotify is not recursive: every directory gets its own watch. A directory that appears (created or moved
//! in) is watched and then reported as a [`RescanReason::NewDirectory`] rescan, because whatever happened
//! inside it before the watch was added was not seen. A directory moved away loses its watches. The thread
//! waits in `poll(2)` on the inotify descriptor and on a pipe; stopping closes the pipe's write end.
//!
//! The kernel's event records are parsed from bytes in safe code; `unsafe` is only around the system calls.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::sink::Sink;
use crate::{Hint, RawEvent, RescanReason, ResumeToken, Started};

// <sys/inotify.h>; the values are kernel ABI.
const IN_MODIFY: u32 = 0x0000_0002;
const IN_ATTRIB: u32 = 0x0000_0004;
const IN_CLOSE_WRITE: u32 = 0x0000_0008;
const IN_MOVED_FROM: u32 = 0x0000_0040;
const IN_MOVED_TO: u32 = 0x0000_0080;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;
const IN_DELETE_SELF: u32 = 0x0000_0400;
const IN_MOVE_SELF: u32 = 0x0000_0800;
const IN_UNMOUNT: u32 = 0x0000_2000;
const IN_Q_OVERFLOW: u32 = 0x0000_4000;
const IN_IGNORED: u32 = 0x0000_8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_DONT_FOLLOW: u32 = 0x0200_0000;
const IN_EXCL_UNLINK: u32 = 0x0400_0000;
const IN_ISDIR: u32 = 0x4000_0000;

const MASK: u32 = IN_CREATE
    | IN_DELETE
    | IN_MODIFY
    | IN_CLOSE_WRITE
    | IN_ATTRIB
    | IN_MOVED_FROM
    | IN_MOVED_TO
    | IN_DELETE_SELF
    | IN_MOVE_SELF
    | IN_DONT_FOLLOW
    | IN_ONLYDIR
    | IN_EXCL_UNLINK;

/// `struct inotify_event` without its name: wd, mask, cookie, len.
const HEADER: usize = 16;

/// While the root is missing, how often to look for it again (ms).
const ROOT_RETRY_MS: i32 = 1000;

struct Inotify {
    fd: OwnedFd,
}

impl Inotify {
    fn new() -> io::Result<Inotify> {
        // SAFETY: a plain system call with flag arguments.
        let fd = unsafe { libc::inotify_init1(libc::O_NONBLOCK | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` was just returned by the kernel and nothing else owns it.
        Ok(Inotify { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    fn add(&self, path: &Path) -> io::Result<i32> {
        let c = CString::new(path.as_os_str().as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: `c` is a NUL-terminated string that outlives the call; the descriptor is open.
        let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
        if wd < 0 { Err(io::Error::last_os_error()) } else { Ok(wd) }
    }

    fn remove(&self, wd: i32) {
        // SAFETY: a plain system call on our descriptor; an unknown `wd` only returns EINVAL.
        unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
    }

    /// Reads whatever records are queued into `buf`; `Ok(0)` when none are.
    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: `buf` is valid for writes of `buf.len()` bytes for the call.
            let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            match e.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => return Ok(0),
                _ => return Err(e),
            }
        }
    }
}

/// A pipe; closing the write end wakes the reader's `poll` (POLLHUP).
fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors the call writes.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just created by the kernel and are owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Waits for inotify records or the stop pipe. Returns (records ready, stop requested).
fn wait(inotify: RawFd, stop: RawFd, timeout_ms: i32) -> io::Result<(bool, bool)> {
    let mut fds = [
        libc::pollfd { fd: inotify, events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: stop, events: libc::POLLIN, revents: 0 },
    ];
    // SAFETY: `fds` is a valid array of two pollfd structs for the call.
    let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        return if e.kind() == io::ErrorKind::Interrupted { Ok((false, false)) } else { Err(e) };
    }
    Ok((fds[0].revents != 0, fds[1].revents != 0))
}

/// One record from the kernel.
#[derive(Debug, PartialEq, Eq)]
struct Record<'a> {
    wd: i32,
    mask: u32,
    name: &'a [u8],
}

/// Splits a buffer of `struct inotify_event` records. A truncated tail is ignored (the kernel never writes
/// one; this is just bounds checking).
fn parse(buf: &[u8]) -> Vec<Record<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    let word = |at: usize| -> Option<[u8; 4]> { buf.get(at..at + 4)?.try_into().ok() };
    while i + HEADER <= buf.len() {
        let (Some(wd), Some(mask), Some(len)) = (word(i), word(i + 4), word(i + 12)) else { break };
        let len = u32::from_ne_bytes(len) as usize;
        let Some(raw) = buf.get(i + HEADER..i + HEADER + len) else { break };
        let name = raw.split(|&b| b == 0).next().unwrap_or(&[]);
        out.push(Record { wd: i32::from_ne_bytes(wd), mask: u32::from_ne_bytes(mask), name });
        i += HEADER + len;
    }
    out
}

fn hint(mask: u32) -> Hint {
    if mask & IN_DELETE != 0 {
        Hint::Removed
    } else if mask & (IN_MOVED_FROM | IN_MOVED_TO) != 0 {
        Hint::Renamed
    } else if mask & IN_CREATE != 0 {
        Hint::Created
    } else if mask & (IN_MODIFY | IN_CLOSE_WRITE) != 0 {
        Hint::Modified
    } else if mask & IN_ATTRIB != 0 {
        Hint::Meta
    } else {
        Hint::Unknown
    }
}

/// The watches: one per directory, keyed both ways.
struct Tree {
    ino: Inotify,
    root: PathBuf,
    by_wd: HashMap<i32, PathBuf>,
    by_path: HashMap<PathBuf, i32>,
    root_wd: Option<i32>,
}

impl Tree {
    /// Watches `rel` and every directory below it. Fails only if `rel` is the root and cannot be watched;
    /// other failures are reported as rescans.
    fn add_tree(&mut self, rel: PathBuf, sink: &mut Sink) -> io::Result<()> {
        let mut stack = vec![rel];
        while let Some(d) = stack.pop() {
            let is_root = d.as_os_str().is_empty();
            match self.ino.add(&self.root.join(&d)) {
                Ok(wd) => {
                    // The same inode reached again (a directory moved before we saw it go): one watch.
                    if let Some(old) = self.by_wd.insert(wd, d.clone())
                        && old != d
                    {
                        self.by_path.remove(&old);
                    }
                    self.by_path.insert(d.clone(), wd);
                    if is_root {
                        self.root_wd = Some(wd);
                    }
                }
                Err(e) if is_root => return Err(e),
                // Gone again before we got to it: its parent reports that.
                Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ENOTDIR)) => continue,
                Err(e) => {
                    sink.send(RawEvent::Rescan { rel: Some(d), reason: RescanReason::WatchFailed });
                    // Out of watches (fs.inotify.max_user_watches): adding more would only fail again.
                    if e.raw_os_error() == Some(libc::ENOSPC) {
                        return Ok(());
                    }
                    continue;
                }
            }
            let Ok(rd) = std::fs::read_dir(self.root.join(&d)) else { continue };
            for ent in rd.flatten() {
                if ent.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(d.join(ent.file_name()));
                }
            }
        }
        Ok(())
    }

    /// Forgets (and removes) the watches on `rel` and below.
    fn drop_tree(&mut self, rel: &Path) {
        let gone: Vec<PathBuf> = self.by_path.keys().filter(|p| p.starts_with(rel)).cloned().collect();
        for p in gone {
            if let Some(wd) = self.by_path.remove(&p) {
                self.by_wd.remove(&wd);
                self.ino.remove(wd);
            }
        }
    }

    fn drop_all(&mut self) {
        for &wd in self.by_wd.keys() {
            self.ino.remove(wd);
        }
        self.by_wd.clear();
        self.by_path.clear();
        self.root_wd = None;
    }

    /// Handles one record. Returns `false` once the receiver is gone.
    fn handle(&mut self, r: &Record<'_>, sink: &mut Sink) -> bool {
        if r.mask & IN_Q_OVERFLOW != 0 {
            return sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::Dropped });
        }
        let Some(dir) = self.by_wd.get(&r.wd).cloned() else { return true };
        let is_root = Some(r.wd) == self.root_wd;
        if r.mask & IN_IGNORED != 0 || (is_root && r.mask & (IN_DELETE_SELF | IN_MOVE_SELF) != 0) {
            if is_root {
                self.drop_all();
                return sink.send(RawEvent::RootChanged);
            }
            self.by_wd.remove(&r.wd);
            if self.by_path.get(&dir) == Some(&r.wd) {
                self.by_path.remove(&dir);
            }
            return true;
        }
        if r.mask & (IN_DELETE_SELF | IN_MOVE_SELF) != 0 {
            return true; // the parent's watch reports it
        }
        if r.mask & IN_UNMOUNT != 0 {
            let rel = (!dir.as_os_str().is_empty()).then_some(dir);
            return sink.send(RawEvent::Rescan { rel, reason: RescanReason::MustScanSubDirs });
        }
        let rel = if r.name.is_empty() { dir } else { dir.join(OsStr::from_bytes(r.name)) };
        let is_dir = r.mask & IN_ISDIR != 0;
        if is_dir && r.mask & IN_MOVED_FROM != 0 {
            self.drop_tree(&rel);
        }
        if !sink.send(RawEvent::Path { rel: rel.clone(), kind: hint(r.mask), is_dir: Some(is_dir) }) {
            return false;
        }
        if is_dir && r.mask & (IN_CREATE | IN_MOVED_TO) != 0 {
            // add_tree only fails for the root, which this is not.
            let _ = self.add_tree(rel.clone(), sink);
            return sink.send(RawEvent::Rescan { rel: Some(rel), reason: RescanReason::NewDirectory });
        }
        true
    }

    /// While the root is missing: watch it again once it is back.
    fn retry_root(&mut self, sink: &mut Sink) -> bool {
        if self.root_wd.is_some() || !std::fs::metadata(&self.root).is_ok_and(|m| m.is_dir()) {
            return true;
        }
        if self.add_tree(PathBuf::new(), sink).is_ok() {
            return sink.send(RawEvent::RootChanged);
        }
        true
    }
}

pub(crate) fn start(root: PathBuf, since: Option<ResumeToken>) -> io::Result<Started> {
    let (mut sink, events) = Sink::new();
    let mut tree = Tree { ino: Inotify::new()?, root, by_wd: HashMap::new(), by_path: HashMap::new(), root_wd: None };
    // Watches are in place before `watch` returns, so a change made right after it is seen.
    tree.add_tree(PathBuf::new(), &mut sink)?;
    if since.is_some() {
        sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::NoHistory });
        sink.send(RawEvent::HistoryDone);
    }
    let (stop_rx, stop_tx) = pipe()?;
    let thread = std::thread::Builder::new().name("fswatch-inotify".into()).spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let timeout = if tree.root_wd.is_none() || sink.owes_rescan() { ROOT_RETRY_MS } else { -1 };
            // poll(2) only fails for bad arguments or no memory; ending the thread closes the channel, which
            // the consumer sees as the watcher ending.
            let (ready, stop) = wait(tree.ino.fd.as_raw_fd(), stop_rx.as_raw_fd(), timeout).unwrap_or((false, true));
            if stop {
                return;
            }
            sink.flush();
            if !tree.retry_root(&mut sink) {
                return;
            }
            if !ready {
                continue;
            }
            loop {
                let n = match tree.ino.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => {
                        sink.send(RawEvent::Rescan { rel: None, reason: RescanReason::Dropped });
                        break;
                    }
                };
                for r in parse(&buf[..n]) {
                    if !tree.handle(&r, &mut sink) {
                        return;
                    }
                }
            }
        }
    })?;
    Ok(Started { events, stop: Box::new(move || drop(stop_tx)), thread })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(wd: i32, mask: u32, name: &[u8], pad: usize) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&wd.to_ne_bytes());
        v.extend_from_slice(&mask.to_ne_bytes());
        v.extend_from_slice(&0u32.to_ne_bytes());
        v.extend_from_slice(&((name.len() + pad) as u32).to_ne_bytes());
        v.extend_from_slice(name);
        v.extend(std::iter::repeat_n(0, pad));
        v
    }

    #[test]
    fn parses_records_and_strips_name_padding() {
        let mut buf = record(1, IN_CREATE, b"a.txt", 3);
        buf.extend(record(2, IN_Q_OVERFLOW, b"", 0));
        buf.extend(record(3, IN_DELETE, b"cut", 1)[..10].iter());
        assert_eq!(
            parse(&buf),
            vec![Record { wd: 1, mask: IN_CREATE, name: b"a.txt" }, Record { wd: 2, mask: IN_Q_OVERFLOW, name: b"" }]
        );
    }

    #[test]
    fn masks_map_to_hints() {
        assert_eq!(hint(IN_CREATE), Hint::Created);
        assert_eq!(hint(IN_DELETE | IN_ISDIR), Hint::Removed);
        assert_eq!(hint(IN_MOVED_FROM), Hint::Renamed);
        assert_eq!(hint(IN_MOVED_TO), Hint::Renamed);
        assert_eq!(hint(IN_CLOSE_WRITE), Hint::Modified);
        assert_eq!(hint(IN_ATTRIB), Hint::Meta);
    }
}
