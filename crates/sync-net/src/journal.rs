//! The file of a chat's messages (a channel's, a private conversation's), written outside the lock of its store.
//!
//! The app's calls read a store's messages often (the unread counts, the topics, a page of messages), and a disk may
//! take long to save (Windows' Defender reads each new file before the write is done). So a store never writes under
//! its lock: while it holds it, it queues what to write ([`Journal::append`], [`Journal::rewrite`]) in the order it
//! changed, and once it let go, its caller writes the queue ([`Journal::flush`]). The writes take turns under a lock of
//! their own, oldest first, so no record is written twice and a whole rewrite never goes back to an older store.
//!
//! **A write that fails loses nothing for good.** Each queued write has a number, and the journal knows up to which one
//! everything is on disk. A caller's flush answers for what it queued (written by it or by a flush that took its turn
//! first), so a send whose record did not land is told so, whoever wrote the batch. After a failure (a virus scanner
//! that holds the file while it reads it, a full disk) the file may lack records or end in a torn one: appends are
//! refused until the whole file is written anew, which the store's callers do at once ([`Journal::flush`]'s `Err`
//! means: queue a [`Journal::rewrite`] and flush again). The next write that succeeds holds every message kept.

use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::shared::lock;

/// One write, as queued, with its number.
enum Op {
    /// A record added at the end.
    Append(u64, Vec<u8>),
    /// The whole file anew (a store that dropped its oldest messages, or one mending a failed write).
    Rewrite(u64, Vec<u8>),
}

/// What is still to be written.
#[derive(Default)]
struct Queue {
    ops: Vec<Op>,
    /// The number of the last write queued.
    last: u64,
}

/// Where the file stands.
#[derive(Default)]
struct Written {
    /// Every write up to this number is on disk.
    upto: u64,
    /// A write failed since the last whole file: the file may lack records or end in a torn one, so only a whole
    /// rewrite may go next. Why it failed.
    broken: Option<String>,
    /// The store is gone ([`Journal::close`]): nothing is written any more.
    closed: bool,
}

/// The queued writes of one store's file.
pub(crate) struct Journal {
    path: PathBuf,
    /// Taken only for a moment, also under the store's lock.
    queue: Mutex<Queue>,
    /// Taken for the whole write ([`Journal::flush`]), never by a read.
    written: Mutex<Written>,
    /// **Tests:** the next this many writes fail (as a rename a virus scanner holds up fails on Windows).
    pub(crate) test_fail: AtomicU32,
}

impl Journal {
    /// The journal of the file at `path`.
    pub(crate) fn new(path: PathBuf) -> Journal {
        Journal { path, queue: Mutex::new(Queue::default()), written: Mutex::new(Written::default()), test_fail: AtomicU32::new(0) }
    }

    /// Queues a record to add at the end. Called under the store's lock, so the queue keeps the store's order.
    /// Returns its number (what [`Self::flush`] is asked for).
    pub(crate) fn append(&self, rec: Vec<u8>) -> u64 {
        let mut q = lock(&self.queue);
        q.last += 1;
        let n = q.last;
        q.ops.push(Op::Append(n, rec));
        n
    }

    /// Queues the whole file anew: what was queued before it is in it, so it goes. Returns its number.
    pub(crate) fn rewrite(&self, whole: Vec<u8>) -> u64 {
        let mut q = lock(&self.queue);
        q.last += 1;
        let n = q.last;
        q.ops.clear();
        q.ops.push(Op::Rewrite(n, whole));
        n
    }

    /// The number of the last write queued: under the store's lock, what its caller's flush must answer for.
    pub(crate) fn queued(&self) -> u64 {
        lock(&self.queue).last
    }

    /// Writes what is queued, oldest first, and answers for every write numbered up to `upto`: `Ok` when they are on
    /// disk (written by this call or by one that took its turn first). Called after the store's lock is let go. `Err`:
    /// one of them is not on disk; the caller queues a [`Self::rewrite`] of the store and flushes again.
    pub(crate) fn flush(&self, upto: u64) -> io::Result<()> {
        let mut w = lock(&self.written);
        let ops = std::mem::take(&mut lock(&self.queue).ops);
        if w.closed {
            return Ok(());
        }
        let mut tail: Vec<u8> = Vec::new();
        let mut tail_upto = 0;
        for op in ops {
            match op {
                Op::Rewrite(n, whole) => match self.write_whole(&whole) {
                    Ok(()) => {
                        tail.clear();
                        w.upto = n;
                        w.broken = None;
                    }
                    Err(e) => {
                        w.broken = Some(e.to_string());
                        tail.clear();
                    }
                },
                Op::Append(n, rec) => {
                    tail.extend_from_slice(&rec);
                    tail_upto = n;
                }
            }
        }
        if !tail.is_empty() && w.broken.is_none() {
            match self.append_all(&tail) {
                Ok(()) => w.upto = tail_upto,
                Err(e) => w.broken = Some(e.to_string()),
            }
        }
        if w.upto >= upto {
            return Ok(());
        }
        Err(io::Error::other(format!("the messages could not be saved: {}", w.broken.as_deref().unwrap_or("a write failed"))))
    }

    /// The store is gone (a channel left, a conversation deleted): a write still queued or under way never makes its
    /// file again. Returns once one under way is done.
    pub(crate) fn close(&self) {
        lock(&self.written).closed = true;
        lock(&self.queue).ops.clear();
    }

    /// **Tests:** a write fails, as the test asked.
    fn test_failure(&self) -> io::Result<()> {
        match self.test_fail.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)) {
            Ok(_) => Err(io::Error::other("the file is in use (a test)")),
            Err(_) => Ok(()),
        }
    }

    fn write_whole(&self, whole: &[u8]) -> io::Result<()> {
        let tmp = self.path.with_extension("chat.tmp");
        std::fs::write(&tmp, whole)?;
        crate::shared::slow_disk();
        self.test_failure()?;
        std::fs::rename(&tmp, &self.path)
    }

    fn append_all(&self, tail: &[u8]) -> io::Result<()> {
        let mut f = std::fs::OpenOptions::new().append(true).open(&self.path)?;
        // Half of it written, as a write cut off by a full disk leaves it.
        if let Err(e) = self.test_failure() {
            f.write_all(&tail[..tail.len() / 2])?;
            return Err(e);
        }
        f.write_all(tail)?;
        crate::shared::slow_disk();
        Ok(())
    }
}

/// Writes a store's whole file atomically (aside, then renamed), while it is opened: nothing else uses it yet.
pub(crate) fn write_whole(path: &Path, whole: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("chat.tmp");
    std::fs::write(&tmp, whole)?;
    crate::shared::slow_disk();
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure: two callers queue a record each, and the first one's flush writes both and fails. The second
    /// finds an empty queue: it must not say its record was saved.
    #[test]
    fn a_caller_whose_record_was_in_another_callers_failed_write_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.chat");
        std::fs::write(&p, b"").unwrap();
        let j = Journal::new(p.clone());
        let mine = j.append(b"one".to_vec());
        let theirs = j.append(b"two".to_vec());
        j.test_fail.store(1, Ordering::SeqCst);
        assert!(j.flush(mine).is_err());
        assert!(j.flush(theirs).is_err(), "its record is not on disk");
        // Appends after a failure are refused until the file is written anew (its tail may be torn).
        let more = j.append(b"three".to_vec());
        assert!(j.flush(more).is_err());
        let whole = j.rewrite(b"onetwothree".to_vec());
        j.flush(whole).unwrap();
        let last = j.append(b"four".to_vec());
        j.flush(last).unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"onetwothreefour");
    }
}
