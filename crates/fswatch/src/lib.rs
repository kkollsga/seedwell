//! trs-fswatch: a recursive watcher for one folder root, and a pure aggregator that turns its events into
//! scan batches.
//!
//! * [`watch`] starts a watcher on a std thread: FSEvents on macOS, inotify on Linux, a periodic
//!   metadata walk everywhere else (or when [`Backend::Poll`] is asked for). Its [`RawEvent`]s arrive on a
//!   std `mpsc` channel, so it works the same in the daemon, the app and tests; no async runtime.
//! * [`aggregate::Aggregator`] is a sans-I/O state machine on a virtual clock ([`Millis`]): it waits for a
//!   path to go quiet, holds deletes and renames so both halves of a rename go out together, and collapses
//!   bursts to a directory or the whole root.
//!
//! Events are hints. The consumer stats every path it is given, so a wrong or duplicated hint costs a stat,
//! never correctness. What must not happen is a change that is never reported: every way this crate can lose
//! events (kernel or daemon drops, a full channel, no history for a resume token, a watch that could not be
//! added) is reported as a [`RawEvent::Rescan`].
//!
//! `unsafe` code lives only in the OS binding modules (`fsevents`, `inotify`); everything else is safe.
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod aggregate;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod fsevents;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod inotify;
mod poll;
mod sink;

use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;
use std::time::Duration;

/// Milliseconds on the caller's clock (the aggregator never reads a real clock).
pub type Millis = u64;

/// Where a later [`watch`] can resume: the last event the caller has seen, and the volume its event IDs
/// belong to. Opaque to callers: persist it with [`ResumeToken::to_bytes`] and hand it back in
/// [`WatchOptions::since`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ResumeToken {
    /// The backend's event ID (FSEvents: the host-wide event ID).
    pub event_id: u64,
    /// The volume's event-store UUID (FSEvents: `FSEventsCopyUUIDForDevice`). If it differs at the next
    /// start, the IDs mean nothing any more and the watcher reports a full rescan.
    pub volume_uuid: [u8; 16],
}

impl ResumeToken {
    /// Length of [`ResumeToken::to_bytes`].
    pub const LEN: usize = 25;
    const VERSION: u8 = 1;

    /// A stable byte form: a version byte, the event ID (big-endian), the UUID.
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0] = Self::VERSION;
        b[1..9].copy_from_slice(&self.event_id.to_be_bytes());
        b[9..].copy_from_slice(&self.volume_uuid);
        b
    }

    /// Parses [`ResumeToken::to_bytes`]. `None` for anything else (wrong length or version); the caller
    /// then starts without a token, which means a full scan.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != Self::LEN || b[0] != Self::VERSION {
            return None;
        }
        let event_id = u64::from_be_bytes(b[1..9].try_into().ok()?);
        let volume_uuid = b[9..].try_into().ok()?;
        Some(Self { event_id, volume_uuid })
    }
}

/// What the backend thinks happened to a path. Only a hint: backends coalesce (FSEvents merges the flags
/// of everything that happened to a path within its latency, and sometimes longer), so the consumer stats
/// the path and believes the disk, not the hint.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Hint {
    /// The path was created.
    Created,
    /// The path was removed.
    Removed,
    /// The path was one half of a rename (or moved in or out of the root).
    Renamed,
    /// The contents changed.
    Modified,
    /// Metadata only (mode, owner, extended attributes, times).
    Meta,
    /// Something happened; the backend cannot say what.
    Unknown,
}

impl Hint {
    /// Removes and renames: the aggregator holds these so both halves of a rename are scanned together.
    pub fn is_removal(self) -> bool {
        matches!(self, Hint::Removed | Hint::Renamed)
    }
}

/// Why a [`RawEvent::Rescan`] was sent.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RescanReason {
    /// The backend says this subtree must be scanned (FSEvents `MustScanSubDirs`, or a directory-level event
    /// without file detail, or a volume mounted or unmounted inside the root).
    MustScanSubDirs,
    /// The kernel or the event daemon dropped events (FSEvents `KernelDropped`/`UserDropped`, inotify
    /// `IN_Q_OVERFLOW`).
    Dropped,
    /// The event channel to the consumer was full and events were dropped.
    ChannelFull,
    /// FSEvents event IDs wrapped around.
    EventIdsWrapped,
    /// A resume token was given but the history cannot be replayed (another volume, a reset event store,
    /// a volume without FSEvents history, or a backend without history).
    NoHistory,
    /// A directory appeared and has just been watched (inotify): what happened inside it before the watch
    /// was added was not seen.
    NewDirectory,
    /// A directory could not be watched (inotify: watch limit or permissions); changes in it are not seen
    /// until the next full scan.
    WatchFailed,
    /// An event path that could not be placed under the root.
    UnknownPath,
}

/// One event from a watcher. `rel` paths are relative to the root; the empty path is the root itself.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RawEvent {
    /// Something happened at `rel`: stat it.
    Path {
        /// The path, relative to the root.
        rel: PathBuf,
        /// What the backend thinks happened.
        kind: Hint,
        /// Whether the path is (or was) a directory, if the backend knows.
        is_dir: Option<bool>,
    },
    /// Scan the subtree at `rel` (`None` = the whole root): events were lost or are not detailed.
    Rescan {
        /// The subtree, relative to the root; `None` for the whole root.
        rel: Option<PathBuf>,
        /// Why.
        reason: RescanReason,
    },
    /// The root itself was moved, deleted or replaced (or came back). Stat the root; while it is missing the
    /// folder cannot sync, and when it is back everything must be rescanned.
    RootChanged,
    /// The replay of history since [`WatchOptions::since`] is complete; live events follow. Sent exactly once
    /// when `since` was given (after a `Rescan { rel: None, .. }` if there was no history), never otherwise.
    HistoryDone,
    /// A token the caller can persist. It covers every event sent before it on the channel, so persist it
    /// only after those events have been scanned ([`aggregate::Aggregator::take_resume`] does that
    /// bookkeeping).
    Resume(ResumeToken),
}

/// Which backend to use.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Backend {
    /// FSEvents on macOS, inotify on Linux, polling elsewhere.
    #[default]
    Auto,
    /// A metadata walk of the whole tree every `interval`, compared with the previous walk.
    Poll {
        /// Time between walks.
        interval: Duration,
    },
}

/// The backend a [`Watcher`] actually runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ActiveBackend {
    /// macOS FSEvents.
    FsEvents,
    /// Linux inotify.
    Inotify,
    /// Periodic metadata walk.
    Poll,
}

/// Options for [`watch`].
#[derive(Clone, Debug)]
pub struct WatchOptions {
    /// Resume after this token: events since then are replayed first, then [`RawEvent::HistoryDone`].
    /// `None` = only changes from now on (the caller does its own full scan; start the watcher *before* that
    /// scan so nothing falls between the two).
    pub since: Option<ResumeToken>,
    /// FSEvents latency: how long the daemon gathers events before delivering them (the first event after a
    /// quiet spell is delivered at once). Ignored by the other backends.
    pub latency: Duration,
    /// Which backend.
    pub backend: Backend,
    /// The shortest time between two [`RawEvent::Resume`] tokens. A token is only sent when it changed;
    /// while the root is quiet the token is advanced at most every 12 intervals.
    pub resume_interval: Duration,
}

/// Default time between two walks of the polling backend.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(10);

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            since: None,
            latency: Duration::from_millis(500),
            backend: Backend::Auto,
            resume_interval: Duration::from_secs(5),
        }
    }
}

/// A running watcher. Dropping it stops it, like [`Watcher::stop`].
pub struct Watcher {
    events: Receiver<RawEvent>,
    root: PathBuf,
    backend: ActiveBackend,
    stop: Option<Box<dyn FnOnce() + Send>>,
    thread: Option<JoinHandle<()>>,
}

impl Watcher {
    /// The events. The channel is bounded; if the consumer falls behind, events are dropped and a
    /// `Rescan { rel: None, reason: ChannelFull }` is sent once there is room again.
    pub fn events(&self) -> &Receiver<RawEvent> {
        &self.events
    }

    /// The root as watched: canonical (symlinks resolved), which is what event paths are relative to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The backend in use.
    pub fn backend(&self) -> ActiveBackend {
        self.backend
    }

    /// Stops the watcher and waits for its thread. When this returns, no backend callback is running or
    /// will run.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop();
        }
        if let Some(t) = self.thread.take() {
            // A panic in the watcher thread has already been reported by the panic hook; there is nothing
            // to recover here.
            let _ = t.join();
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

impl std::fmt::Debug for Watcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Watcher").field("root", &self.root).field("backend", &self.backend).finish()
    }
}

/// The parts a backend hands back to [`watch`].
pub(crate) struct Started {
    pub events: Receiver<RawEvent>,
    pub stop: Box<dyn FnOnce() + Send>,
    pub thread: JoinHandle<()>,
}

/// Starts watching `root` (which must be an existing directory) recursively.
///
/// Symlinks inside the root are not followed. Event paths are relative to the canonical root
/// ([`Watcher::root`]).
pub fn watch(root: &Path, opts: WatchOptions) -> io::Result<Watcher> {
    let root = std::fs::canonicalize(root)?;
    if !std::fs::metadata(&root)?.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotADirectory, format!("{} is not a directory", root.display())));
    }
    let (backend, started) = match opts.backend {
        Backend::Poll { interval } => (ActiveBackend::Poll, poll::start(root.clone(), interval, opts.since)?),
        Backend::Auto => start_native(&root, &opts)?,
    };
    Ok(Watcher {
        events: started.events,
        root,
        backend,
        stop: Some(started.stop),
        thread: Some(started.thread),
    })
}

#[cfg(target_os = "macos")]
fn start_native(root: &Path, opts: &WatchOptions) -> io::Result<(ActiveBackend, Started)> {
    Ok((ActiveBackend::FsEvents, fsevents::start(root.to_path_buf(), opts)?))
}

#[cfg(target_os = "linux")]
fn start_native(root: &Path, opts: &WatchOptions) -> io::Result<(ActiveBackend, Started)> {
    Ok((ActiveBackend::Inotify, inotify::start(root.to_path_buf(), opts.since)?))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn start_native(root: &Path, opts: &WatchOptions) -> io::Result<(ActiveBackend, Started)> {
    Ok((ActiveBackend::Poll, poll::start(root.to_path_buf(), DEFAULT_POLL_INTERVAL, opts.since)?))
}

/// `abs` relative to `root`, or `None` if it is not inside it. Exact match first; then, because macOS volumes
/// are usually case-insensitive, an ASCII case-insensitive match on whole components.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub(crate) fn relativize(root: &Path, abs: &Path) -> Option<PathBuf> {
    if let Ok(rel) = abs.strip_prefix(root) {
        return Some(rel.to_path_buf());
    }
    let mut a = abs.components();
    for rc in root.components() {
        let ac = a.next()?;
        let same = match (rc, ac) {
            (Component::Normal(x), Component::Normal(y)) => x.as_encoded_bytes().eq_ignore_ascii_case(y.as_encoded_bytes()),
            (x, y) => x == y,
        };
        if !same {
            return None;
        }
    }
    Some(a.as_path().to_path_buf())
}

#[doc(hidden)]
/// Counters for tests (each nextest test is its own process, so they are not shared between tests).
pub mod debug {
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) static CALLBACKS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static LIVE_STREAMS: AtomicU64 = AtomicU64::new(0);

    /// Backend callbacks entered so far (FSEvents only).
    pub fn callbacks() -> u64 {
        CALLBACKS.load(Ordering::SeqCst)
    }

    /// Native streams whose state is still alive (FSEvents: the stream still holds its context, which it
    /// can let go of shortly after `stop` returns).
    pub fn live_streams() -> u64 {
        LIVE_STREAMS.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_round_trips_and_rejects_garbage() {
        let t = ResumeToken { event_id: 0x0102_0304_0506_0708, volume_uuid: [7; 16] };
        let b = t.to_bytes();
        assert_eq!(ResumeToken::from_bytes(&b), Some(t));
        assert_eq!(ResumeToken::from_bytes(&b[..24]), None);
        let mut bad = b;
        bad[0] = 2;
        assert_eq!(ResumeToken::from_bytes(&bad), None);
        assert_eq!(ResumeToken::from_bytes(&[]), None);
    }

    #[test]
    fn relativize_exact_and_case_insensitive() {
        let root = Path::new("/Users/me/Sync");
        assert_eq!(relativize(root, Path::new("/Users/me/Sync/a/b")), Some(PathBuf::from("a/b")));
        assert_eq!(relativize(root, Path::new("/Users/me/Sync")), Some(PathBuf::new()));
        assert_eq!(relativize(root, Path::new("/Users/me/sync/a")), Some(PathBuf::from("a")));
        assert_eq!(relativize(root, Path::new("/Users/me/Syncx/a")), None);
        assert_eq!(relativize(root, Path::new("/Users/me")), None);
    }
}
