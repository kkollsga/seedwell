//! Global LRU cache of open file descriptors.
//!
//! One cache is shared by every torrent (SPEC §5.2): a seeding client with hundreds of torrents
//! and tens of thousands of files must not hold one fd per file. Entries are `Arc<File>`, so an
//! eviction never closes a descriptor that an I/O thread is mid-`pread` on — the fd closes when
//! the last user drops it, and the *cache* never holds more than `capacity` entries.
//!
//! Implementation: slab of nodes + intrusive doubly linked list (indices, no per-op allocation)
//! behind one mutex. The mutex is held for a hash lookup and a few index writes only; the
//! `open(2)` syscall happens outside it.

use std::collections::HashMap;
use std::fs::File;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

const NIL: u32 = u32::MAX;

struct Node {
    key: u64,
    file: Arc<File>,
    writable: bool,
    prev: u32,
    next: u32,
}

struct Inner {
    map: HashMap<u64, u32>,
    nodes: Vec<Option<Node>>,
    free: Vec<u32>,
    head: u32, // most recently used
    tail: u32, // least recently used
}

/// Counters exposed for tests and diagnostics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FdCacheStats {
    /// Lookups served from the cache.
    pub hits: u64,
    /// Lookups that needed an `open(2)`.
    pub misses: u64,
    /// Entries evicted to stay within capacity.
    pub evictions: u64,
    /// Entries currently cached.
    pub open: usize,
}

/// Shared fd cache keyed by `(torrent id, file index)`.
pub struct FdCache {
    inner: Mutex<Inner>,
    capacity: usize,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
}

/// Cache key for a file of a torrent.
pub(crate) fn key(torrent: u32, file: u32) -> u64 {
    (u64::from(torrent) << 32) | u64::from(file)
}

impl FdCache {
    /// Creates a cache holding at most `capacity` (≥ 1) descriptors.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: Mutex::new(Inner {
                map: HashMap::with_capacity(capacity),
                nodes: Vec::with_capacity(capacity),
                free: Vec::new(),
                head: NIL,
                tail: NIL,
            }),
            capacity,
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Configured capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Current counters.
    pub fn stats(&self) -> FdCacheStats {
        FdCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            open: self.lock().map.len(),
        }
    }

    /// Returns the cached descriptor for `key`, or opens one with `open`.
    ///
    /// A read-only entry is replaced when `want_write` is set; a writable entry serves reads.
    pub fn get_or_open(
        &self,
        key: u64,
        want_write: bool,
        open: impl FnOnce() -> std::io::Result<File>,
    ) -> std::io::Result<Arc<File>> {
        {
            let mut g = self.lock();
            if let Some(&slot) = g.map.get(&key) {
                let (file, writable) = match &g.nodes[slot as usize] {
                    Some(n) => (n.file.clone(), n.writable),
                    None => unreachable!("map points at live slot"),
                };
                if writable || !want_write {
                    g.touch(slot);
                    self.hits.fetch_add(1, Ordering::Relaxed);
                    return Ok(file);
                }
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        let file = Arc::new(open()?);
        let mut g = self.lock();
        if let Some(&slot) = g.map.get(&key) {
            // Another thread raced us. Keep the better (writable) one.
            let node = g.nodes[slot as usize].as_mut().expect("live slot");
            if node.writable || !want_write {
                let f = node.file.clone();
                g.touch(slot);
                return Ok(f);
            }
            node.file = file.clone();
            node.writable = true;
            g.touch(slot);
            return Ok(file);
        }
        if g.map.len() >= self.capacity {
            let evicted = g.evict_lru();
            if evicted {
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
        g.push_front(key, file.clone(), want_write);
        Ok(file)
    }

    /// Drops the entry for `key` if present.
    pub fn invalidate(&self, key: u64) {
        let mut g = self.lock();
        if let Some(slot) = g.map.get(&key).copied() {
            g.remove(slot);
        }
    }

    /// Drops every entry of one torrent (mode/location change, removal).
    pub fn invalidate_torrent(&self, torrent: u32) {
        let mut g = self.lock();
        let mut cur = g.head;
        while cur != NIL {
            let (next, k) = match &g.nodes[cur as usize] {
                Some(n) => (n.next, n.key),
                None => break,
            };
            if (k >> 32) as u32 == torrent {
                g.remove(cur);
            }
            cur = next;
        }
    }
}

impl Inner {
    fn unlink(&mut self, slot: u32) {
        let (prev, next) = match &self.nodes[slot as usize] {
            Some(n) => (n.prev, n.next),
            None => return,
        };
        if prev != NIL {
            if let Some(p) = self.nodes[prev as usize].as_mut() {
                p.next = next;
            }
        } else {
            self.head = next;
        }
        if next != NIL {
            if let Some(n) = self.nodes[next as usize].as_mut() {
                n.prev = prev;
            }
        } else {
            self.tail = prev;
        }
    }

    fn link_front(&mut self, slot: u32) {
        let old = self.head;
        if let Some(n) = self.nodes[slot as usize].as_mut() {
            n.prev = NIL;
            n.next = old;
        }
        if old != NIL {
            if let Some(o) = self.nodes[old as usize].as_mut() {
                o.prev = slot;
            }
        } else {
            self.tail = slot;
        }
        self.head = slot;
    }

    fn touch(&mut self, slot: u32) {
        if self.head != slot {
            self.unlink(slot);
            self.link_front(slot);
        }
    }

    fn push_front(&mut self, key: u64, file: Arc<File>, writable: bool) {
        let node = Node {
            key,
            file,
            writable,
            prev: NIL,
            next: NIL,
        };
        let slot = match self.free.pop() {
            Some(s) => {
                self.nodes[s as usize] = Some(node);
                s
            }
            None => {
                self.nodes.push(Some(node));
                (self.nodes.len() - 1) as u32
            }
        };
        self.map.insert(key, slot);
        self.link_front(slot);
    }

    fn remove(&mut self, slot: u32) {
        self.unlink(slot);
        if let Some(n) = self.nodes[slot as usize].take() {
            self.map.remove(&n.key);
            self.free.push(slot);
        }
    }

    fn evict_lru(&mut self) -> bool {
        if self.tail == NIL {
            return false;
        }
        let t = self.tail;
        self.remove(t);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_dev_null() -> std::io::Result<File> {
        File::open(if cfg!(windows) { "NUL" } else { "/dev/null" })
    }

    #[test]
    fn lru_order_and_capacity() {
        let c = FdCache::new(3);
        for k in 0..3 {
            c.get_or_open(k, false, open_dev_null).unwrap();
        }
        // touch 0 so 1 becomes LRU
        c.get_or_open(0, false, || panic!("must hit")).unwrap();
        c.get_or_open(3, false, open_dev_null).unwrap();
        assert_eq!(c.stats().open, 3);
        assert_eq!(c.stats().evictions, 1);
        c.get_or_open(0, false, || panic!("0 must still be cached"))
            .unwrap();
        c.get_or_open(2, false, || panic!("2 must still be cached"))
            .unwrap();
        let mut reopened = false;
        c.get_or_open(1, false, || {
            reopened = true;
            open_dev_null()
        })
        .unwrap();
        assert!(reopened, "1 was the LRU entry");
    }

    #[test]
    fn upgrade_to_writable_and_invalidate() {
        let c = FdCache::new(4);
        let k = key(7, 1);
        c.get_or_open(k, false, open_dev_null).unwrap();
        let mut opened = false;
        c.get_or_open(k, true, || {
            opened = true;
            open_dev_null()
        })
        .unwrap();
        assert!(opened);
        // writable entry serves reads without reopening
        c.get_or_open(k, false, || panic!("hit")).unwrap();
        c.get_or_open(key(8, 1), false, open_dev_null).unwrap();
        c.invalidate_torrent(7);
        assert_eq!(c.stats().open, 1);
        c.invalidate(key(8, 1));
        assert_eq!(c.stats().open, 0);
    }
}
