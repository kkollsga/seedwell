//! trs-storage — the disk layer of Seedwell. See `README.md` and `docs/ENGINEERING.md` §3.
//!
//! * [`Layout`]: allocation-free piece/block ↔ file mapping (padding files, per-file custom paths).
//! * [`DiskIo`] / [`TorrentHandle`]: explicit I/O + hashing thread pools, shared fd LRU cache,
//!   bounded write cache that holds blocks until a piece verifies, async request/response API.
//! * Resume validation, [`TorrentHandle::recheck`], throttled [`TorrentHandle::scrub`].
//! * [`TorrentHandle::finalize`]: crash-safe move from `.incomplete` staging to final paths.
//! * [`TorrentHandle::relocate_prepare`] and friends: journaled, verified relocation after a
//!   location change (rename on the same volume, hashed streaming copy across volumes).

#![warn(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

mod bitset;
mod disk;
mod error;
mod fdcache;
mod finalize;
mod fsutil;
pub mod hash;
mod layout;
mod oneshot;
mod relocate;
mod verify;
mod mapped;

pub use bitset::PieceSet;
pub use disk::{
    DiskConfig, DiskIo, DiskStats, Location, MAX_READ_LEN, Mode, PieceOutcome, TorrentConfig,
    TorrentHandle, WriteAck,
};
pub use error::{Result, StorageError, leaf, os_error_is_write_blocked};
pub use fdcache::{FdCache, FdCacheStats};
pub use finalize::{FaultHook, FinalizeOptions, FinalizeReport, Journal};
pub use fsutil::{
    NOFILE_SAFE_CAP, available_space, copy_metadata, dev_of_nearest_existing, perf_core_count, preallocate,
    raise_nofile_limit, release_free_memory, remove_dir_if_empty, rename_excl, same_volume, volume_mount_point,
};
pub use layout::{
    BLOCK_SIZE, FilePath, FileSpec, Layout, LayoutError, MAX_FILES, PathRef, Segment, Segments,
};
pub use oneshot::Pending;
pub use mapped::{MAP_MIN, MappedAlloc};
pub use relocate::{
    DEFAULT_COPY_BUFFER, DEFAULT_RESERVE, MAX_COPY_BUFFER, RecoverState, Recovered, RelocateEntry,
    RelocateMethod, RelocateOptions, RelocateProgressFn, RelocateReport, preflight_move,
};
pub use verify::{
    CancelToken, DEFAULT_SCRUB_BYTES_PER_SEC, FileFingerprint, FileStatus, Progress, ProgressFn,
    RecheckOptions, RecheckReport, ResumeReport,
};
