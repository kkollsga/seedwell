//! Request pipelining helpers.

use crate::message::{BLOCK_SIZE, BlockRequest};

/// Iterate the block requests that cover one piece, in order.
///
/// The last block is shortened to fit `piece_len`.
pub fn block_requests(
    index: u32,
    piece_len: u32,
    block_len: u32,
) -> impl Iterator<Item = BlockRequest> {
    let block_len = block_len.max(1);
    (0..piece_len.div_ceil(block_len)).map(move |i| {
        let begin = i * block_len;
        BlockRequest {
            index,
            begin,
            length: block_len.min(piece_len - begin),
        }
    })
}

/// Number of blocks in a piece of `piece_len` bytes.
pub fn block_count(piece_len: u32, block_len: u32) -> u32 {
    piece_len.div_ceil(block_len.max(1))
}

/// Lower bound on the queue depth, so a fresh or slow peer still has work queued.
pub const MIN_DEPTH: u32 = 2;
/// Upper bound regardless of what the peer advertises.
pub const MAX_DEPTH: u32 = 500;

/// Target number of outstanding requests for a peer.
///
/// Keeps the pipe full: the bandwidth-delay product in blocks (+2 so a block is
/// always in flight when one lands), clamped to `[MIN_DEPTH, min(reqq, MAX_DEPTH)]`.
/// `reqq` is the peer's advertised queue limit (`None` -> 250, libtorrent's default).
pub fn target_depth(bytes_per_sec: u64, rtt_ms: u32, reqq: Option<u32>) -> u32 {
    let bdp = bytes_per_sec.saturating_mul(u64::from(rtt_ms)) / 1000;
    let blocks = bdp.div_ceil(u64::from(BLOCK_SIZE)).saturating_add(2);
    let cap = reqq.unwrap_or(250).clamp(MIN_DEPTH, MAX_DEPTH);
    blocks.min(u64::from(cap)).max(u64::from(MIN_DEPTH)) as u32
}

/// Tracks how many requests are in flight to one peer.
// size: 8 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestWindow {
    outstanding: u32,
    depth: u32,
}

impl RequestWindow {
    /// Window with an initial depth.
    pub fn new(depth: u32) -> Self {
        Self {
            outstanding: 0,
            depth,
        }
    }

    /// Change the target depth (e.g. from [`target_depth`]).
    pub fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    /// Requests that may be sent now.
    pub fn available(&self) -> u32 {
        self.depth.saturating_sub(self.outstanding)
    }

    /// Record `n` requests sent.
    pub fn sent(&mut self, n: u32) {
        self.outstanding = self.outstanding.saturating_add(n);
    }

    /// Record one block received, rejected or cancelled.
    pub fn completed(&mut self) {
        self.outstanding = self.outstanding.saturating_sub(1);
    }

    /// Forget everything in flight (choke without Fast, disconnect).
    pub fn reset(&mut self) {
        self.outstanding = 0;
    }

    /// Requests currently in flight.
    pub fn outstanding(&self) -> u32 {
        self.outstanding
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_cover_piece() {
        let v: Vec<_> = block_requests(3, 40_000, BLOCK_SIZE).collect();
        assert_eq!(v.len(), 3);
        assert_eq!(
            v[0],
            BlockRequest {
                index: 3,
                begin: 0,
                length: 16384
            }
        );
        assert_eq!(
            v[2],
            BlockRequest {
                index: 3,
                begin: 32768,
                length: 7232
            }
        );
        assert_eq!(v.iter().map(|r| r.length).sum::<u32>(), 40_000);
        assert_eq!(block_count(40_000, BLOCK_SIZE), 3);
        assert_eq!(block_requests(0, 0, BLOCK_SIZE).count(), 0);
        assert_eq!(block_requests(0, 16384, BLOCK_SIZE).count(), 1);
        assert_eq!(block_requests(0, 5, 0).count(), 5);
    }

    #[test]
    fn depth_scales_and_clamps() {
        assert_eq!(target_depth(0, 50, None), 2);
        // 10 MB/s * 100 ms = 1 MB = 62 blocks (+2).
        assert_eq!(target_depth(10_000_000, 100, None), 62 + 2);
        assert_eq!(target_depth(1 << 40, 1000, None), 250);
        assert_eq!(target_depth(1 << 40, 1000, Some(16)), 16);
        assert_eq!(target_depth(1 << 40, 1000, Some(100_000)), 500);
        assert_eq!(target_depth(1 << 40, 1000, Some(0)), 2);
        assert_eq!(target_depth(u64::MAX, u32::MAX, None), 250);
    }

    #[test]
    fn window() {
        let mut w = RequestWindow::new(4);
        assert_eq!(w.available(), 4);
        w.sent(3);
        assert_eq!(w.available(), 1);
        w.completed();
        assert_eq!(w.outstanding(), 2);
        w.set_depth(1);
        assert_eq!(w.available(), 0);
        w.reset();
        assert_eq!(w.available(), 1);
        w.completed();
        assert_eq!(w.outstanding(), 0);
    }
}
