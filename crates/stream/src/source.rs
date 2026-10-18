//! The contract between the streaming server and the engine.
//!
//! The server holds no file data. Everything it sends comes from [`PieceSource::read`], and it
//! only reads bytes of pieces for which [`PieceSource::has`] was true. The engine implements this
//! trait once per torrent (a thin adapter over its bitfield, the storage read cache and the
//! picker); see `ENGINE_INTEGRATION.md`.

use std::future::Future;
use std::io;
use std::ops::Range;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::watch;

/// Torrent-wide geometry. Offsets in this crate are bytes from the start of the torrent
/// (storage `Layout` coordinates) unless a name says "file offset".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    /// Bytes per piece (the last piece may be shorter).
    pub piece_length: u32,
    /// Total torrent size in bytes.
    pub total_len: u64,
}

impl Geometry {
    pub fn piece_count(&self) -> u32 {
        self.total_len.div_ceil(u64::from(self.piece_length.max(1))) as u32
    }
    /// Piece containing torrent offset `off`.
    pub fn piece_of(&self, off: u64) -> u32 {
        (off / u64::from(self.piece_length.max(1))) as u32
    }
    /// Pieces covering the torrent byte range (empty range gives an empty piece range).
    pub fn pieces_of(&self, r: Range<u64>) -> Range<u32> {
        if r.start >= r.end {
            return 0..0;
        }
        self.piece_of(r.start)..self.piece_of(r.end - 1) + 1
    }
    /// First byte of `piece`.
    pub fn piece_start(&self, piece: u32) -> u64 {
        u64::from(piece) * u64::from(self.piece_length)
    }
}

/// One file inside the torrent, in torrent coordinates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRange {
    /// Index in the torrent's file list (for the engine's own bookkeeping).
    pub index: usize,
    /// Display name (last path component), used in the URL and for the content type.
    pub name: String,
    /// Offset of the file's first byte within the torrent.
    pub offset: u64,
    /// File size in bytes.
    pub len: u64,
}

/// Identifies one stream (one `open`). Unique per server; the engine groups picker hints by it and
/// round-robins deadline ties between ids.
pub type StreamId = u64;

/// Who owns a stream, so removing a torrent closes all of its streams. The engine passes its
/// own torrent id.
pub type OwnerId = u64;

/// How the picker should treat the pieces of an [`UrgentRange`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    /// A player is waiting on these (the live window and the index). Requested from several fast
    /// peers and web seeds at once; ordered by deadline across all streams.
    Urgent,
    /// The next ~2 minutes: higher than normal rarest-first, but no duplicates.
    ReadAhead,
}

/// A run of pieces the player will need, with deadlines relative to "now".
///
/// Piece `first + k` has deadline `deadline + k * step`. Deadlines are nearest-first. The
/// engine may skip pieces it already has. The slice handed to [`PieceSource::set_urgent`] is
/// the stream's complete current picture: it replaces whatever was set before for that id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UrgentRange {
    pub pieces: (u32, u32),
    pub deadline: Duration,
    pub step: Duration,
    pub priority: Priority,
}

impl UrgentRange {
    pub fn range(&self) -> Range<u32> {
        self.pieces.0..self.pieces.1
    }
    pub fn deadline_of(&self, piece: u32) -> Duration {
        self.deadline + self.step * piece.saturating_sub(self.pieces.0)
    }
}

/// What the engine implements. Everything is non-blocking except `read`.
pub trait PieceSource: Send + Sync + 'static {
    /// Piece size and torrent size. Constant for the life of the source.
    fn geometry(&self) -> Geometry;

    /// Is `piece` hash-verified and readable? Must be O(1) (a bitfield lookup).
    fn has(&self, piece: u32) -> bool;

    /// A counter that changes whenever any piece becomes verified. The stream wakes on
    /// `changed()` and re-checks `has`. **Ordering contract:** the engine must set the bit
    /// first, then `send_modify` the counter, so a waiter that saw `has == false` and then awaits
    /// `changed()` cannot miss it. Dropping the sender (torrent removed) ends every response.
    fn subscribe(&self) -> watch::Receiver<u64>;

    /// Bytes of `piece` received so far (partial piece progress). A response waiting on a piece
    /// resets its stall timer when this grows. Default: no partial information.
    fn piece_progress(&self, _piece: u32) -> u32 {
        0
    }

    /// Read `len` bytes at torrent offset `offset` into `buf` (cleared, then filled, then returned
    /// so the caller can reuse the allocation). Only called for bytes whose pieces `has` reported
    /// true, `len` never exceeds 1 MiB. The engine should route this through the storage read cache.
    fn read(&self, offset: u64, len: usize, buf: Vec<u8>) -> impl Future<Output = io::Result<Vec<u8>>> + Send;

    /// Replace the urgency picture of `stream`. Must not block (send on a channel). Called when a
    /// read cursor crosses a piece boundary, a request starts or ends, or the index set changes.
    fn set_urgent(&self, stream: StreamId, ranges: &[UrgentRange]);

    /// Drop everything set for `stream`. Its outstanding requests become normal priority (they are
    /// not cancelled unless they are duplicates).
    fn clear(&self, stream: StreamId);
}

type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Object-safe view of [`PieceSource`]; blanket-implemented. One boxed future per chunk read.
pub(crate) trait DynSource: Send + Sync {
    fn geometry(&self) -> Geometry;
    fn has(&self, piece: u32) -> bool;
    fn subscribe(&self) -> watch::Receiver<u64>;
    fn piece_progress(&self, piece: u32) -> u32;
    fn read<'a>(&'a self, offset: u64, len: usize, buf: Vec<u8>) -> BoxFut<'a, io::Result<Vec<u8>>>;
    fn set_urgent(&self, stream: StreamId, ranges: &[UrgentRange]);
    fn clear(&self, stream: StreamId);
}

impl<T: PieceSource> DynSource for T {
    fn geometry(&self) -> Geometry {
        PieceSource::geometry(self)
    }
    fn has(&self, piece: u32) -> bool {
        PieceSource::has(self, piece)
    }
    fn subscribe(&self) -> watch::Receiver<u64> {
        PieceSource::subscribe(self)
    }
    fn piece_progress(&self, piece: u32) -> u32 {
        PieceSource::piece_progress(self, piece)
    }
    fn read<'a>(&'a self, offset: u64, len: usize, buf: Vec<u8>) -> BoxFut<'a, io::Result<Vec<u8>>> {
        Box::pin(PieceSource::read(self, offset, len, buf))
    }
    fn set_urgent(&self, stream: StreamId, ranges: &[UrgentRange]) {
        PieceSource::set_urgent(self, stream, ranges)
    }
    fn clear(&self, stream: StreamId) {
        PieceSource::clear(self, stream)
    }
}
