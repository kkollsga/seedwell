//! Growable byte FIFO with O(1) amortised push/consume and contiguous reads.
//!
//! A `Vec` plus a head offset rather than `VecDeque<u8>`: packets are cut straight out of a
//! contiguous slice (one memcpy, no ring-wrap split), and an empty queue owns no heap memory
//! until first use so idle connections cost nothing beyond the 32-byte header.

/// Byte FIFO.
// size: 32 bytes
#[derive(Debug, Default)]
pub struct ByteQueue {
    buf: Vec<u8>,
    head: usize,
}

impl ByteQueue {
    /// Empty queue; allocates nothing.
    pub const fn new() -> Self {
        ByteQueue { buf: Vec::new(), head: 0 }
    }

    /// Bytes queued.
    pub fn len(&self) -> usize {
        self.buf.len() - self.head
    }

    /// True when nothing is queued.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Heap bytes currently reserved.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }

    /// Append bytes.
    pub fn push(&mut self, data: &[u8]) {
        if self.head > 0 && self.head >= self.buf.len() / 2 && self.head >= 4096 {
            self.buf.drain(..self.head);
            self.head = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// Copy `out.len()` bytes starting `offset` bytes from the front without consuming them.
    /// Returns false (copying nothing) if that range is not fully queued.
    pub fn copy_at(&self, offset: usize, out: &mut [u8]) -> bool {
        let start = self.head.saturating_add(offset);
        match start.checked_add(out.len()).and_then(|end| self.buf.get(start..end)) {
            Some(src) => {
                out.copy_from_slice(src);
                true
            }
            None => false,
        }
    }

    /// Drop up to `n` bytes from the front.
    pub fn consume(&mut self, n: usize) {
        self.head = (self.head + n).min(self.buf.len());
        if self.head == self.buf.len() {
            self.buf.clear();
            self.head = 0;
        }
    }

    /// Move up to `out.len()` bytes into `out`; returns how many.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len());
        out[..n].copy_from_slice(&self.buf[self.head..self.head + n]);
        self.consume(n);
        n
    }

    /// Free the allocation if the queue is empty (idle-connection memory release).
    pub fn release_if_empty(&mut self) {
        if self.is_empty() {
            self.buf = Vec::new();
            self.head = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_semantics() {
        let mut q = ByteQueue::new();
        assert_eq!(q.capacity(), 0);
        q.push(b"hello ");
        q.push(b"world");
        assert_eq!(q.len(), 11);
        let mut two = [0u8; 5];
        assert!(q.copy_at(6, &mut two));
        assert_eq!(&two, b"world");
        assert!(!q.copy_at(7, &mut two));
        let mut out = [0u8; 8];
        assert_eq!(q.read(&mut out), 8);
        assert_eq!(&out, b"hello wo");
        assert_eq!(q.len(), 3);
        q.consume(100);
        assert!(q.is_empty());
        q.release_if_empty();
        assert_eq!(q.capacity(), 0);
    }

    #[test]
    fn compaction_keeps_contents() {
        let mut q = ByteQueue::new();
        let data: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        q.push(&data);
        q.consume(15_000);
        q.push(&data[..100]);
        let mut out = vec![0u8; 5_100];
        assert_eq!(q.read(&mut out), 5_100);
        assert_eq!(&out[..5_000], &data[15_000..]);
        assert_eq!(&out[5_000..], &data[..100]);
    }
}
