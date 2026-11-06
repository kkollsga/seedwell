//! The server of the Mac's File Provider extension (SPEC §7G.11): the extension is sandboxed and keeps no state, so
//! each of its requests is one connection to a Unix socket in the app group's container, answered here from the core.
//! Everything the Finder shows of an item (its capabilities, whether it is kept, what an error means to the system)
//! is decided here; the extension only maps the answer onto File Provider's types.
//!
//! The protocol (version [`PROTOCOL`]): one JSON line per request, `{"v":1,"op":…,"folder":…,…}`; the answer is one
//! JSON line, `{"v":1,"ok":…}` or `{"v":1,"err":{"kind":…,"message":…}}`, before which a fetch writes progress lines
//! (`{"v":1,"progress":{"done":…,"total":…}}`). The connection closes after the answer; a client that closes it first
//! cancels its fetch. Anchors travel as decimal strings (they use all 64 bits). The ops:
//!
//! | op | asks | answers |
//! |---|---|---|
//! | `hello` | | `{"app"}` |
//! | `item` | `item` | an item |
//! | `children` | `item`, `cursor`, `limit` | `{"items","next"}` |
//! | `changes` | `anchor`, `limit` | `{"items","deleted","anchor","more"}` |
//! | `anchor` | | `{"anchor"}` |
//! | `fetch` | `item` | progress lines, then `{"item","path"}`: `path` is a copy in the container's `t/`, the caller's to move |
//! | `cancel` | `item` | `{}` |
//! | `create` | `parent`, `name`, `kind`, `contents`, `may_exist` | `{"item"}` |
//! | `modify` | `item`, `parent`, `name`, `contents`, `base` | `{"item","fetch_content","conflict_copy"}` |
//! | `delete` | `item`, `base`, `recursive` | `{"deleted"}`, or the error `deletion_rejected` (the item comes back) |
//! | `pin` | `item`, `on` | `{"item"}` |
//! | `materialized` | `ids`: every item the system has on disk now | `{"released"}`: files it let go of since the last report |
//!
//! Content handed to the app (`contents`) is a file the extension put in the container's `u/`; the core copies it.
//! Files left in `t/` and `u/` are removed after [`LEFTOVER_AGE`].
// The server runs on the Mac only (Unix sockets, APFS clones); elsewhere only the types the app API names are used.
#![cfg_attr(not(unix), allow(dead_code))]

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::sync::ondemand::{NO_ITEM, NOT_ON_DEMAND, OdError, OdKind, SyncChangePage, SyncDeleted, SyncEvicted, SyncFetch, SyncItem, SyncItemKind, SyncItemPage, SyncItemState};
use crate::sync::{SyncAccess, SyncBackend};
use crate::Core;
use crate::types::CoreError;

/// The protocol's version: the app and its extension ship together, so a request of another version comes only from
/// an extension the system still runs from before an update; it is refused and retried.
pub const PROTOCOL: u32 = 1;
/// The socket's name in the group container: short, since the whole path must fit in 103 bytes (the group's path is
/// already about 97 with a 33-byte home folder); a longer one is reached relative to the folder.
pub const SOCKET_NAME: &str = "fp";
/// Copies handed to the extension (fetched content), and content it hands over (new and changed files).
pub const CLONES: &str = "t";
pub const UPLOADS: &str = "u";
/// Files left in those folders this long (the extension crashed between its steps) are removed.
pub const LEFTOVER_AGE: Duration = Duration::from_secs(600);
/// How often a fetch reports its progress.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// How often the watcher looks at the folders and their change feeds.
const WATCH_EVERY: Duration = Duration::from_secs(1);
/// Pages of children and changes, at most.
const PAGE_MAX: u32 = 1000;
/// The longest request line; only a part of a report of the system's set of items on disk may be longer (the
/// extension sends those in parts of 5000 identifiers).
pub(crate) const REQUEST_MAX: u64 = 64 << 10;
const REPORT_MAX: u64 = 2 << 20;
/// How long a client may take to send its request, in all.
const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// How long a write to a client may wait (one that does not read: a fetch is then taken back).
const SEND_WAIT: Duration = Duration::from_secs(30);
/// Connections served at once; more are turned away (the system retries).
const MAX_CONNECTIONS: usize = 64;
/// While a request is worked on, a line this often keeps the connection alive (the extension waits for one at most
/// a minute; a delegated rename may take longer).
const KEEPALIVE: Duration = Duration::from_secs(15);
/// Showing what is held as downloaded (`sync_host_materialize`): the host asks the system for this many files at a
/// time, and waits this long between batches, so a large folder does not flood the system's queue.
pub const MATERIALIZE_BATCH: usize = 50;
pub const MATERIALIZE_PAUSE: Duration = Duration::from_millis(200);
/// A file asked for that way and not fetched within this long is an ordinary file again (a later fetch of it is the
/// user's).
pub const MATERIALIZE_TTL: Duration = Duration::from_secs(600);

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

// ------------------------------------------------------------------------------------------ what the host is told

/// An on-demand folder as the host shows it: one File Provider domain (identifier = the folder's id, display name =
/// its name).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncHostDomain {
    pub id: String,
    pub name: String,
}

/// Seedwell's one entry in the file manager's sidebar (Windows: File Explorer's navigation pane, a shell folder under
/// its own CLSID), opening the Seedwell folder: where joined folders go (Settings › Folder Sync › Seedwell folder,
/// `sync.toml`'s `default_location`). The folders inside it are ordinary folders; on-demand ones are their own sync
/// roots nested inside. One entry for all of them, so the sidebar stays one line however many folders there are.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncNavPane {
    /// The entry's CLSID, in braces ([`NAV_PANE_CLSID`]): fixed, so a start after a crash finds the same entry and
    /// the uninstaller knows which to remove.
    pub clsid: String,
    /// What the sidebar shows: the product's name.
    pub name: String,
    /// The Seedwell folder, absolute: the setting with `~` expanded, `~/Seedwell` when it is empty.
    pub target: String,
    /// The registry value the host writes under the entry's key (any data): a sweep (at start after a crash, at
    /// uninstall) removes only keys that carry it, so it never touches another program's entry.
    pub marker: String,
    /// Whether there is an entry at all: the Seedwell folder is set in Settings, or a synced folder lies inside it,
    /// and (as the listener is told) the folder is there. Without it (nothing synced, every folder elsewhere, a place
    /// not made yet or on a disk that is away) the host removes the entry: it would open a folder that is not there.
    /// The host never makes the folder for it.
    pub show: bool,
}

/// [`SyncNavPane::clsid`]: generated once for Seedwell, never derived (one entry, so nothing to derive it from).
pub const NAV_PANE_CLSID: &str = "{1F99E88C-875A-477E-9AE4-845298196137}";
/// [`SyncNavPane::marker`].
pub const NAV_PANE_MARKER: &str = "SeedwellNavPane";

/// The entry's CLSID without a `Core`: for the uninstaller (`Seedwell.exe --unregister`), which removes the entry
/// after the app has stopped.
#[uniffi::export]
pub fn sync_nav_pane_clsid() -> String {
    NAV_PANE_CLSID.into()
}

/// The marker value's name without a `Core`, for the uninstaller's sweep (it removes only keys that carry it).
#[uniffi::export]
pub fn sync_nav_pane_marker() -> String {
    NAV_PANE_MARKER.into()
}

/// Whether `path` lies inside `root` (or is it), by their components alone: no disk is read (this runs on the UI
/// thread), and names compare without case, as the default file systems of the Mac and Windows do.
fn lexically_inside(root: &Path, path: &Path) -> bool {
    let key = |p: &Path| -> Vec<String> { plain(p).components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect() };
    let (root, path) = (key(root), key(path));
    path.len() >= root.len() && path[..root.len()] == root[..]
}

/// The entry for the setting `setting` (`default_location`, as stored) and the synced folders' places.
pub(crate) fn nav_pane_of(setting: &str, folders: &[&Path]) -> SyncNavPane {
    let target = PathBuf::from(crate::sync::default_location_of(setting));
    let set = !setting.trim().is_empty();
    let show = target.is_absolute() && (set || folders.iter().any(|f| f.is_absolute() && lexically_inside(&target, f)));
    SyncNavPane {
        clsid: NAV_PANE_CLSID.into(),
        name: crate::menu::APP_NAME.into(),
        target: target.display().to_string(),
        marker: NAV_PANE_MARKER.into(),
        show,
    }
}

/// The entry as folder sync has it now; `None` while folder sync is not running (nothing is known: the host leaves
/// the entry as it is). Reads the configuration in memory only: no disk, no engine.
pub(crate) fn nav_pane(sync: &dyn SyncBackend) -> Option<SyncNavPane> {
    let s = sync.snapshot();
    if !s.running {
        return None;
    }
    let setting = sync.settings().default_location;
    let places: Vec<&Path> = s.folders.iter().map(|f| Path::new(f.path.as_str())).collect();
    Some(nav_pane_of(&setting, &places))
}

/// What the server tells the host, from its own thread (the host hops to its main thread).
#[uniffi::export(with_foreign)]
pub trait SyncHostListener: Send + Sync {
    /// The folder's change feed moved: the host signals the domain's working set, so the system asks for the changes.
    fn feed_moved(&self, folder_id: String);
    /// The domains there should be, at start and whenever the set or a name changes: the host adds the missing ones,
    /// renames, and removes those of folders no longer on demand.
    fn domains(&self, domains: Vec<SyncHostDomain>);
    /// Before an on-demand folder is removed or switched to full (whoever asks: the app, the control socket, an
    /// agent): remove its domain, the system's pending changes handed over first; what the system kept of it the host
    /// puts away with `sync_host_keep_preserved` (also when it comes late). Blocks until done; an error refuses the
    /// removal or switch (nothing changes, and a removal that completes late leaves the domain as it was). A folder
    /// without a domain: nothing to do. Called from the core's threads.
    fn remove_domain(&self, folder_id: String) -> SyncHostRemoval;
    /// Something the user should know (what the system kept of a removed domain, and where it is now).
    fn notice(&self, title: String, text: String);
    /// Seedwell's entry in the file manager's sidebar ([`SyncNavPane`]), once folder sync runs and whenever it changes
    /// (the Seedwell folder set elsewhere in Settings or by the command line, the first folder joined into it, the last
    /// one gone): the host writes the entry, or removes it when `show` is false. Never told in an isolated run (tests,
    /// QA): the CLSID is one per user, so a test's entry would replace the user's own. Called from the core's threads.
    /// The Mac's host has no entry yet and ignores it.
    fn nav_pane(&self, entry: SyncNavPane);
    /// Whether the process at the other end of a connection is this app's own File Provider extension, from its audit
    /// token (32 bytes, `LOCAL_PEERTOKEN`): the host checks its code signature. Asked before anything of the request is
    /// read; anything else is refused unserved. Called from the server's threads.
    fn verify_peer(&self, audit_token: Vec<u8>) -> bool;
}

/// What removing a domain did.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncHostRemoval {
    /// Why it could not be removed (the removal or switch is refused).
    pub error: Option<String>,
}

/// The host's words in errors the core passes on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncHostText {
    /// A removal asked on the app's main thread (it cannot wait there).
    Busy,
    /// The system did not finish removing a domain in time.
    NoAnswer,
}

/// The words of `t`, as they end the core's "could not be let go of first: …".
#[uniffi::export]
pub fn sync_host_text(t: SyncHostText) -> String {
    match t {
        SyncHostText::Busy => "the app is busy; try again".into(),
        SyncHostText::NoAnswer => concat!("the ", trs_platform::file_manager!(), " did not answer in time").into(),
    }
}

/// A removal of a domain under way, noted before the domain goes, so a start after the app died in the middle can tell
/// the user (`sync_host_unfinished_releases`).
const RELEASES: &str = "file-provider-releases";

fn release_note(state: &Path, id: &str) -> PathBuf {
    state.join(RELEASES).join(id)
}

/// The removals of domains the app did not finish (`Core::sync_host_unfinished_releases`); read once: the notes go.
pub(crate) fn unfinished_releases(state: &Path) -> Vec<SyncHostNotice> {
    let dir = state.join(RELEASES);
    let Ok(entries) = std::fs::read_dir(&dir) else { return Vec::new() };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = std::fs::read_to_string(e.path()).unwrap_or_default();
        let _ = std::fs::remove_file(e.path());
        out.push(SyncHostNotice {
            title: format!("Check “{name}” for changes"),
            text: format!(
                concat!(
                    "Seedwell stopped while it removed “{}” from the ",
                    trs_platform::file_manager!(),
                    ". Changes made there that Seedwell had not taken in yet may have been kept by macOS, in a folder it ",
                    "names for the removal (look in ~/Library/CloudStorage); Seedwell was not told where."
                ),
                name
            ),
        });
    }
    out
}

/// A notice for the user: a title and the text.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncHostNotice {
    pub title: String,
    pub text: String,
}

/// What the core says of a folder the host has a domain for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncHostFolderState {
    /// Folder sync is not running (yet): nothing can be said; leave the domain.
    NotRunning,
    /// On demand: the domain stays.
    OnDemand,
    /// Full: the domain goes.
    Full,
    /// Removed: the domain goes.
    Removed,
}

/// What a folder is to its domain, from the configuration alone (`None`: folder sync not running): a folder that could
/// not open, or is stopped for a move, is still there, and only one the configuration lacks is removed.
pub(crate) fn folder_state(configured: Option<Option<bool>>) -> SyncHostFolderState {
    match configured {
        None => SyncHostFolderState::NotRunning,
        Some(None) => SyncHostFolderState::Removed,
        Some(Some(true)) => SyncHostFolderState::OnDemand,
        Some(Some(false)) => SyncHostFolderState::Full,
    }
}

/// Puts what the system kept of a removed domain (`preserved`) next to the folder's place (`<name> (changes not
/// synced)`, numbered when taken), or, with no place, in `~/Seedwell`; says where. `None` when nothing was kept.
pub(crate) fn keep_preserved(name: &str, place: Option<&Path>, preserved: &Path) -> Option<SyncHostNotice> {
    if !preserved.exists() {
        return None;
    }
    let to = kept_dir(name, place)?;
    // On another volume a rename fails: copied, checked whole, then removed (left where it was if that fails).
    let at = if std::fs::rename(preserved, &to).is_ok() || copy_verified(preserved, &to).is_ok() { to } else { preserved.to_path_buf() };
    Some(kept_notice(name, &at))
}

/// A free place for what was kept of folder `name`: `<name> (changes not synced)` next to the folder's place (numbered
/// when taken), or in `~/Seedwell` for a folder with no place. Its parent is made; the place itself is not.
pub(crate) fn kept_dir(name: &str, place: Option<&Path>) -> Option<PathBuf> {
    let dir = place
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .or_else(|| trs_platform::dirs::home().map(|h| h.join("Seedwell")))?;
    let _ = std::fs::create_dir_all(&dir);
    let base = format!("{name} (changes not synced)");
    let mut to = dir.join(&base);
    let mut n = 2;
    while to.symlink_metadata().is_ok() {
        to = dir.join(format!("{base} {n}"));
        n += 1;
    }
    Some(to)
}

/// What the user is told of changes kept from folder `name`, now at `at`.
pub(crate) fn kept_notice(name: &str, at: &Path) -> SyncHostNotice {
    SyncHostNotice {
        title: format!("Changes kept from “{name}”"),
        text: format!(
            concat!(
                "Files changed in the ",
                trs_platform::file_manager!(),
                " that Seedwell had not taken in yet are in {}. Look them over and copy back what you need."
            ),
            at.display()
        ),
    }
}

/// Copies the tree `from` to the new `to` (files, folders, links; on the Mac with their extended attributes, Finder
/// tags among them, and permissions), checks that the copy holds every entry with its size, and only then removes
/// `from`. A copy that fails or does not match is removed, and `from` stays. Once the copy is whole it is the result,
/// also when `from` cannot be removed all the way (that is logged).
pub(crate) fn copy_verified(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        unsafe extern "C" {
            fn copyfile(from: *const std::ffi::c_char, to: *const std::ffi::c_char, state: *mut std::ffi::c_void, flags: u32) -> i32;
        }
        const COPYFILE_ALL: u32 = 0xF;
        const COPYFILE_RECURSIVE: u32 = 1 << 15;
        const COPYFILE_NOFOLLOW: u32 = (1 << 18) | (1 << 19);
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput));
        let (f, t) = (c(from)?, c(to)?);
        // SAFETY: NUL-terminated paths, no state.
        if unsafe { copyfile(f.as_ptr(), t.as_ptr(), std::ptr::null_mut(), COPYFILE_ALL | COPYFILE_RECURSIVE | COPYFILE_NOFOLLOW) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    fn copy(from: &Path, to: &Path) -> std::io::Result<()> {
        let m = std::fs::symlink_metadata(from)?;
        if m.file_type().is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
        } else if m.is_dir() {
            std::fs::create_dir(to)?;
            for e in std::fs::read_dir(from)? {
                let e = e?;
                copy(&e.path(), &to.join(e.file_name()))?;
            }
        } else if m.is_file() {
            std::fs::copy(from, to)?;
        }
        Ok(())
    }
    fn listing(root: &Path) -> std::io::Result<Vec<(PathBuf, u64, bool)>> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d)? {
                let e = e?;
                let m = std::fs::symlink_metadata(e.path())?;
                let rel = e.path().strip_prefix(root).map(Path::to_path_buf).unwrap_or_default();
                if m.is_dir() {
                    stack.push(e.path());
                }
                out.push((rel, if m.is_file() { m.len() } else { 0 }, m.is_dir()));
            }
        }
        out.sort();
        Ok(out)
    }
    if to.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    let copied = copy(from, to).and_then(|()| Ok(listing(from)? == listing(to)?));
    match copied {
        Ok(true) => {
            if let Err(e) = std::fs::remove_dir_all(from) {
                eprintln!("seedwell: file provider: {} is copied whole to {}; the original could not be removed all the way ({e})", from.display(), to.display());
            }
            Ok(())
        }
        Ok(false) => {
            let _ = std::fs::remove_dir_all(to);
            Err(std::io::Error::other("the copy does not match"))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(to);
            Err(e)
        }
    }
}

/// The host as the core's [`trs_sync_net::OnDemandHost`]: asked to let go of a folder before it is removed or
/// switched to full, whoever asks; the folder is left out of the reported domains meanwhile.
struct Release {
    host: Arc<Host>,
    listener: Arc<dyn SyncHostListener>,
}

impl trs_sync_net::OnDemandHost for Release {
    fn release(&self, folder: &str, name: &str, place: &Path) -> Result<(), String> {
        // A domain per folder: left out of the reported domains at once. The hub keeps serving the folder while the
        // system hands over its pending changes; the host's `sync_host_let_go` drops it from the hub after that.
        if !self.host.hub {
            lock(&self.host.releasing).insert(folder.to_string());
        }
        let _ = (name, place);
        let r = self.listener.remove_domain(folder.to_string());
        if let Some(e) = r.error {
            self.settled(folder);
            return Err(e);
        }
        Ok(())
    }

    fn settled(&self, folder: &str) {
        lock(&self.host.releasing).remove(folder);
        lock(&self.host.frozen).remove(folder);
        lock(&self.host.letting_go).remove(folder);
        lock(&self.host.pass_hold).remove(folder);
        self.host.force_report.store(true, Ordering::SeqCst);
    }
}

/// The host of on-demand folders when it calls the core in its own process (Windows: Cloud Files calls back into
/// the provider's process, so there is no extension and no socket): told which folders there should be and when a
/// folder's change feed moved, and asked to let go of a folder before it is removed or switched to full, as the
/// Mac's server tells its host. Stops when dropped.
pub struct Attached {
    host: Arc<Host>,
    stop: Arc<AtomicBool>,
}

impl Attached {
    /// Watches the folders for `listener` (every [`WATCH_EVERY`]) and lets it release a folder first.
    pub(crate) fn start(core: Arc<dyn HostCore>, dir: &Path, listener: Arc<dyn SyncHostListener>) -> std::io::Result<Attached> {
        Self::start_every(core, dir, listener, WATCH_EVERY)
    }

    fn start_every(core: Arc<dyn HostCore>, dir: &Path, listener: Arc<dyn SyncHostListener>, every: Duration) -> std::io::Result<Attached> {
        let host = Arc::new(Host::new(core, dir));
        host.core.set_release(Some(Arc::new(Release { host: host.clone(), listener: listener.clone() })));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, s) = (host.clone(), stop.clone());
        std::thread::Builder::new().name("od-host-watch".into()).spawn(move || {
            let (mut anchors, mut shown) = (HashMap::new(), None);
            while !s.load(Ordering::Relaxed) {
                h.watch_once(listener.as_ref(), &mut anchors, &mut shown);
                std::thread::sleep(every);
            }
        })?;
        Ok(Attached { host, stop })
    }
}

impl Drop for Attached {
    fn drop(&mut self) {
        self.host.core.set_release(None);
        self.stop.store(true, Ordering::Relaxed);
    }
}

// ------------------------------------------------------------------------------------------ the core it asks

/// An on-demand folder, as the server needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostFolder {
    pub id: String,
    pub name: String,
    /// This device may change the folder (not a read-only member).
    pub writable: bool,
}

/// The core calls the server makes (the sync backend's on-demand calls; a fake in tests).
pub(crate) trait HostCore: Send + Sync {
    /// The on-demand folders, `None` while folder sync is not running (nothing is known then).
    fn folders(&self) -> Option<Vec<HostFolder>>;
    /// Who lets go of an on-demand folder before it is removed or switched to full.
    fn set_release(&self, _host: Option<Arc<dyn trs_sync_net::OnDemandHost>>) {}
    fn item(&self, f: &str, id: &str) -> Result<Option<SyncItem>, OdError>;
    fn children(&self, f: &str, id: &str, cursor: Option<&str>, limit: u32) -> Result<SyncItemPage, OdError>;
    fn changes(&self, f: &str, anchor: u64, limit: u32) -> Result<SyncChangePage, OdError>;
    fn anchor(&self, f: &str) -> Result<u64, OdError>;
    fn fetch(&self, f: &str, id: &str, wait_ms: u64) -> Result<SyncFetch, OdError>;
    fn cancel(&self, f: &str, id: &str) -> Result<(), OdError>;
    fn pin(&self, f: &str, id: &str, on: bool) -> Result<SyncItem, OdError>;
    fn evict(&self, f: &str, id: &str) -> Result<SyncEvicted, OdError>;
    fn create(&self, f: &str, parent: &str, name: &str, dir: bool, contents: Option<&str>) -> Result<SyncItem, OdError>;
    fn modify(&self, f: &str, id: &str, parent: Option<&str>, name: Option<&str>, contents: Option<&str>, base: Option<&str>) -> Result<SyncItem, OdError>;
    /// `seen`: the folder's version the host saw (a folder's delete is refused when something below is newer).
    fn delete(&self, f: &str, id: &str, seen: Option<trs_sync_net::DeleteSeen>) -> Result<SyncDeleted, OdError>;
    /// Why `path` is a name the folder does not sync, if it is.
    fn ignored(&self, _f: &str, _path: &str, _dir: bool) -> Result<Option<crate::sync::ondemand::SyncIgnored>, OdError> {
        Ok(None)
    }
    /// What the folder's changes wait for before they are reported (a delete a moment ago may be a save by replace;
    /// `writing`: the new file is being written at its name). `None`: nothing.
    fn replace_hold(&self, _f: &str) -> Option<trs_sync::ondemand::Hold> {
        None
    }
    /// Seedwell's entry in the sidebar ([`nav_pane`]); `None`: none to report (not running, or an isolated run).
    fn nav_pane(&self) -> Option<SyncNavPane> {
        None
    }
    /// Every synced folder, on demand or full, for the Mac's hub ([`hub`]); `None` while folder sync is not running.
    /// (A core that knows only its on-demand folders lists those, without a place.)
    fn hub_folders(&self) -> Option<Vec<hub::HubFolder>> {
        Some(
            self.folders()?
                .into_iter()
                .map(|f| hub::HubFolder { id: f.id, name: f.name, path: String::new(), on_demand: true, writable: f.writable })
                .collect(),
        )
    }
}

/// The sync backend as the server's core.
/// The sync backend as the host's core. `confine`: an isolated (test) run hosts only the on-demand folders that lie
/// inside its own state folder, so a test can never register a sync root on a real folder.
pub(crate) struct BackendCore(pub Arc<dyn SyncBackend>, pub Option<std::path::PathBuf>);

/// Whether `path` lies inside `root`. Both are resolved as far as they exist (the deepest existing ancestor, then the
/// rest of the path with `..` taken lexically), so links and `..` cannot leave `root`, and a folder whose place is gone
/// (an on-demand folder after its switch) still counts. Windows' verbatim prefixes (`\\?\`, `\\?\UNC\`, which
/// `canonicalize` adds) are dropped on both sides, so a resolved and an unresolved path compare alike.
fn inside(root: &Path, path: &Path) -> bool {
    path.is_absolute() && plain(&resolved(path)).starts_with(plain(&resolved(root)))
}

/// `p` resolved through its deepest existing ancestor, the rest appended with `.` and `..` taken lexically.
fn resolved(p: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut rest = Vec::new();
    let mut base = p;
    let real = loop {
        if let Ok(r) = std::fs::canonicalize(base) {
            break r;
        }
        match (base.file_name(), base.parent()) {
            (Some(name), Some(parent)) => {
                rest.push(name.to_os_string());
                base = parent;
            }
            // Nothing of it exists (or it ends in `..`): take it lexically as it is.
            _ => {
                let mut out = std::path::PathBuf::new();
                for c in p.components() {
                    match c {
                        Component::ParentDir => {
                            out.pop();
                        }
                        Component::CurDir => {}
                        c => out.push(c),
                    }
                }
                return out;
            }
        }
    };
    let mut out = real;
    for name in rest.into_iter().rev() {
        match name.to_str() {
            Some("..") => {
                out.pop();
            }
            Some(".") => {}
            _ => out.push(name),
        }
    }
    out
}

/// `p` without a Windows verbatim prefix: `\\?\C:\x` is `C:\x`, `\\?\UNC\server\share` is `\\server\share`.
fn plain(p: &Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return std::path::PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        return std::path::PathBuf::from(rest);
    }
    p.to_path_buf()
}

impl HostCore for BackendCore {
    fn folders(&self) -> Option<Vec<HostFolder>> {
        let s = self.0.snapshot();
        if !s.running {
            return None;
        }
        Some(
            s.folders
                .into_iter()
                .filter(|f| f.on_demand)
                .filter(|f| self.1.as_deref().is_none_or(|root| inside(root, Path::new(&f.path))))
                .map(|f| HostFolder { writable: f.access != SyncAccess::ReadOnly, id: f.id, name: f.name })
                .collect(),
        )
    }
    fn set_release(&self, host: Option<Arc<dyn trs_sync_net::OnDemandHost>>) {
        self.0.set_on_demand_host(host);
    }
    fn hub_folders(&self) -> Option<Vec<hub::HubFolder>> {
        let s = self.0.snapshot();
        if !s.running {
            return None;
        }
        Some(
            s.folders
                .into_iter()
                .filter(|f| self.1.as_deref().is_none_or(|root| inside(root, Path::new(&f.path))))
                .map(|f| hub::HubFolder { writable: f.access != SyncAccess::ReadOnly, on_demand: f.on_demand, id: f.id, name: f.name, path: f.path })
                .collect(),
        )
    }
    fn nav_pane(&self) -> Option<SyncNavPane> {
        // An isolated run (confined to its state folder) never reports one: the CLSID is the user's.
        if self.1.is_some() {
            return None;
        }
        nav_pane(self.0.as_ref())
    }
    fn item(&self, f: &str, id: &str) -> Result<Option<SyncItem>, OdError> {
        self.0.od_item(f, id)
    }
    fn children(&self, f: &str, id: &str, cursor: Option<&str>, limit: u32) -> Result<SyncItemPage, OdError> {
        self.0.od_children(f, id, cursor, limit)
    }
    fn changes(&self, f: &str, anchor: u64, limit: u32) -> Result<SyncChangePage, OdError> {
        self.0.od_changes(f, anchor, limit)
    }
    fn anchor(&self, f: &str) -> Result<u64, OdError> {
        self.0.od_anchor(f)
    }
    fn fetch(&self, f: &str, id: &str, wait_ms: u64) -> Result<SyncFetch, OdError> {
        self.0.od_fetch(f, id, wait_ms)
    }
    fn cancel(&self, f: &str, id: &str) -> Result<(), OdError> {
        self.0.od_cancel(f, id)
    }
    fn pin(&self, f: &str, id: &str, on: bool) -> Result<SyncItem, OdError> {
        self.0.od_pin(f, id, on)
    }
    fn evict(&self, f: &str, id: &str) -> Result<SyncEvicted, OdError> {
        self.0.od_evict(f, id)
    }
    fn create(&self, f: &str, parent: &str, name: &str, dir: bool, contents: Option<&str>) -> Result<SyncItem, OdError> {
        self.0.od_create(f, parent, name, dir, contents)
    }
    fn modify(&self, f: &str, id: &str, parent: Option<&str>, name: Option<&str>, contents: Option<&str>, base: Option<&str>) -> Result<SyncItem, OdError> {
        self.0.od_modify(f, id, parent, name, contents, base)
    }
    fn ignored(&self, f: &str, path: &str, dir: bool) -> Result<Option<crate::sync::ondemand::SyncIgnored>, OdError> {
        self.0.od_ignored(f, path, dir)
    }
    fn replace_hold(&self, f: &str) -> Option<trs_sync::ondemand::Hold> {
        self.0.od_replace_hold(f)
    }
    fn delete(&self, f: &str, id: &str, seen: Option<trs_sync_net::DeleteSeen>) -> Result<SyncDeleted, OdError> {
        self.0.od_delete(f, id, seen)
    }
}

// ------------------------------------------------------------------------------------------ the wire

/// A request. Every field but `v` and `op` belongs to some ops only.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct Request {
    pub v: u32,
    pub op: String,
    pub folder: String,
    pub item: Option<String>,
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub anchor: Option<String>,
    pub parent: Option<String>,
    pub name: Option<String>,
    /// `file`, `dir` or `symlink` (create).
    pub kind: Option<String>,
    pub contents: Option<String>,
    pub base: Option<String>,
    pub on: Option<bool>,
    pub ids: Option<Vec<String>>,
    pub recursive: Option<bool>,
    pub may_exist: Option<bool>,
    /// A report of the system's set of items on disk: its number (later reports have higher ones), and which part of
    /// how many this is (`part` counts from 0).
    pub report: Option<u64>,
    pub part: Option<u32>,
    pub parts: Option<u32>,
}

/// What an error means to the system. The extension maps each onto one File Provider error and shows `message`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ErrKind {
    /// The item is not there (any more): the system deletes it on its side.
    NoSuchItem,
    /// Nothing to get it from right now (no device that has it is connected, the folder paused or not running): the
    /// system backs off until it is signalled.
    Unreachable,
    /// The anchor is from before the folder's index was made anew: the system lists everything again.
    AnchorExpired,
    /// The delete was not done (the item is put back): a read-only member frees instead, a newer version arrived, or
    /// some items could not be removed.
    DeletionRejected,
    /// A folder deleted without its contents that has some.
    NotEmpty,
    /// Not synced at all (a link, a name the folder does not sync): the system keeps it here and stops asking.
    Excluded,
    /// A request this server does not understand (another protocol version).
    BadRequest,
    /// Anything else, said in the core's words: the system shows it and tries again later.
    Failed,
}

/// An error answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostError {
    pub kind: ErrKind,
    pub message: String,
}

impl HostError {
    fn new(kind: ErrKind, message: impl Into<String>) -> HostError {
        HostError { kind, message: message.into() }
    }
}

/// What a core error means to the system, from its kind (never from its words).
pub(crate) fn kind_of(k: OdKind) -> ErrKind {
    match k {
        OdKind::NoItem => ErrKind::NoSuchItem,
        OdKind::AnchorExpired => ErrKind::AnchorExpired,
        OdKind::NotOnDemand | OdKind::Gone | OdKind::NotRunning | OdKind::Unavailable | OdKind::NotHereYet => ErrKind::Unreachable,
        OdKind::Other | OdKind::Changed => ErrKind::Failed,
        // A name the folder does not sync: the system keeps the item here and stops asking (a rename: it downloads the
        // item first, if it has to, then deletes it; the core deleted it already when its content was here).
        OdKind::Ignored | OdKind::IgnoredNotHere => ErrKind::Excluded,
    }
}

impl From<OdError> for HostError {
    fn from(e: OdError) -> HostError {
        HostError { kind: kind_of(e.kind), message: e.error.to_string() }
    }
}

/// An item as the extension shows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct WireItem {
    pub id: String,
    pub parent: String,
    pub name: String,
    /// `file`, `dir` or `symlink`.
    pub kind: &'static str,
    pub size: u64,
    pub mtime_ns: i64,
    pub content_version: String,
    pub metadata_version: String,
    /// `online_only`, `downloading`, `downloaded`, `modified_here`, `unavailable` (for the Finder's decorations and
    /// the actions' rules).
    pub state: &'static str,
    /// Kept on this computer: File Provider's "download eagerly and keep downloaded".
    pub keep: bool,
    /// The user's own pin (the item, not a folder above it): *Always Keep on This Mac* is on.
    pub pinned: bool,
    pub exec: bool,
    pub symlink_target: Option<String>,
    /// What the user may do with it: `read`, `write` (a file's content; a folder's new items), `rename`, `reparent`,
    /// `delete`.
    pub caps: Vec<&'static str>,
}

fn kind_name(k: SyncItemKind) -> &'static str {
    match k {
        SyncItemKind::File => "file",
        SyncItemKind::Dir => "dir",
        SyncItemKind::Symlink => "symlink",
    }
}

fn state_name(s: SyncItemState) -> &'static str {
    match s {
        SyncItemState::OnlineOnly => "online_only",
        SyncItemState::Downloading => "downloading",
        SyncItemState::Downloaded => "downloaded",
        SyncItemState::ModifiedHere => "modified_here",
        SyncItemState::Unavailable => "unavailable",
    }
}

/// What the user may do with an item. A read-only member reads and deletes (a delete frees the content, and the item
/// comes back online only); the folder itself is neither renamed, moved nor deleted from the Finder.
pub(crate) fn caps_of(i: &SyncItem, writable: bool) -> Vec<&'static str> {
    let root = i.id == trs_sync::ondemand::ROOT_ID;
    let mut caps = vec!["read"];
    if writable {
        caps.push("write");
    }
    if root {
        return caps;
    }
    if writable {
        caps.extend(["rename", "reparent"]);
    }
    caps.push("delete");
    caps
}

pub(crate) fn wire_item(i: &SyncItem, writable: bool, pinned_above: bool) -> WireItem {
    WireItem {
        id: i.id.clone(),
        parent: i.parent_id.clone(),
        name: i.name.clone(),
        kind: kind_name(i.kind),
        size: i.size,
        mtime_ns: i.mtime_ns,
        content_version: i.content_version.clone(),
        metadata_version: i.metadata_version.clone(),
        state: state_name(i.state),
        keep: i.pinned,
        pinned: i.pinned && !pinned_above,
        exec: i.exec,
        symlink_target: i.symlink_target.clone(),
        caps: caps_of(i, writable),
    }
}

fn anchor_text(a: u64) -> String {
    a.to_string()
}

fn anchor_of(text: Option<&str>) -> Result<u64, HostError> {
    match text {
        None | Some("") => Ok(0),
        Some(t) => t.parse().map_err(|_| HostError::new(ErrKind::AnchorExpired, "That sync anchor is not one of this folder's.")),
    }
}

// ------------------------------------------------------------------------------------------ answering

/// What the server keeps between requests: the folders' roles, and the set of items the system had on disk at its
/// last report (so a file that left it since is one the system let go of).
pub(crate) struct Host {
    core: Arc<dyn HostCore>,
    /// The group container: `t/` and `u/` are in it.
    dir: PathBuf,
    folders: Mutex<Vec<HostFolder>>,
    materialized: Mutex<HashMap<String, HashSet<String>>>,
    /// Files held here that the system does not have on disk yet, still to be asked for, per folder.
    to_materialize: Mutex<HashMap<String, VecDeque<String>>>,
    /// Files handed to the host to ask the system for, and since when: their fetch only hands over what is held
    /// (it never makes the core download anything).
    materializing: Mutex<HashMap<(String, String), Instant>>,
    serial: AtomicU64,
    /// A report of the system's set coming in parts (its number, how many parts, the parts here), and the number of
    /// the last one taken, per folder.
    reports: Mutex<HashMap<String, Report>>,
    /// One report at a time, from the check that it is not stale through taking its set (two at once could take the
    /// older last).
    report_lock: Mutex<()>,
    last_report: Mutex<HashMap<String, u64>>,
    /// Connections being served.
    open: AtomicUsize,
    /// Folder versions, per folder, kept up from the change feed ([`Self::folder_version`]).
    dir_versions: Mutex<HashMap<String, DirVersions>>,
    /// Folders being let go of (removed or switched to full): not reported as domains (left out of the hub) until that
    /// is over.
    pub(crate) releasing: Mutex<HashSet<String>>,
    /// The hub's folders being let go of whose changes the system handed over already: what it still sends for them
    /// is refused (it stays on disk, where the host's let-go finds it and keeps it). Until that is over.
    pub(crate) frozen: Mutex<HashSet<String>>,
    /// The Mac's hub ([`hub`]): one domain for every synced folder, instead of a domain per on-demand folder.
    pub(crate) hub: bool,
    /// The core's state folder, where the hub's anchors are kept (`None`: in memory only, tests).
    pub(crate) hub_state: Option<PathBuf>,
    /// The hub's anchors, opened at first use.
    pub(crate) hub_anchors: Mutex<Option<hub::AnchorStore>>,
    /// Folders whose feed a save by replace held past its window, and when the hold let through ends: the hub reads
    /// them once anyway (then they are held again while a hold lasts), never for a hold that started since.
    pub(crate) pass_hold: Mutex<HashMap<String, Instant>>,
    /// The hub's folders being let go of: where what is found and refused is kept.
    pub(crate) letting_go: Mutex<HashMap<String, hub::KeptPlace>>,
    /// Run once just before a let-go's look copies what it found (tests: something vanishes meanwhile).
    #[cfg(test)]
    pub(crate) before_copy: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The on-demand folders the hub was last reported with: when one more is, the host is told again (it shows the
    /// new folder's held files downloaded).
    hub_on_demand: Mutex<HashSet<String>>,
    /// Report the domains on the next look even if nothing changed (a release ended: one refused shows again).
    pub(crate) force_report: AtomicBool,
    /// The sidebar entry the listener was last told of.
    nav_pane: Mutex<Option<SyncNavPane>>,
    pub(crate) keepalive: Duration,
    /// The host hears of a folder's changes after a delete at most this long later (a save by replace may follow).
    pub(crate) replace_cap: Duration,
    /// ... and at most this long while the new file is still being written at the delete's name.
    pub(crate) write_cap: Duration,
    /// Since when each folder's feed signal has waited for a save by replace.
    held_since: Mutex<HashMap<String, Instant>>,
    /// [`REQUEST_WAIT`], [`SEND_WAIT`], [`MAX_CONNECTIONS`] (shorter in tests).
    pub(crate) request_wait: Duration,
    pub(crate) send_wait: Duration,
    pub(crate) max_connections: usize,
}

/// A report of the system's set of items on disk, coming in parts.
struct Report {
    n: u64,
    parts: u32,
    got: BTreeMap<u32, Vec<String>>,
}

/// A folder's folder versions: read up to `upto` (an anchor); `base` for a folder nothing below has changed in since,
/// else its own.
struct DirVersions {
    upto: u64,
    base: u64,
    newest: HashMap<String, u64>,
}

/// A batch of files the host asks the system to download (they are held here: the fetch hands back clones).
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SyncHostBatch {
    /// Item identifiers (the core's; `root` never).
    pub ids: Vec<String>,
    /// Wait this long before asking for the next batch.
    pub pause_ms: u64,
    /// Files still to come after this batch.
    pub left: u64,
}

/// A rename as [`Host::rename_aside`] sees it.
enum Aside {
    /// Onto a name the folder syncs: an ordinary rename.
    Synced,
    /// Onto a name it does not sync, and everything there is on the system's disk (or, for a file, not known): the core
    /// decides (it takes the item out of the folder when its content is here).
    OnDisk,
    /// A file not on the system's disk.
    FileNotOnDisk,
    /// A folder with a file not on the system's disk (or not known): its name.
    FolderNotOnDisk(String),
}

/// Held here: the content is in the cache (this device's own change, or another device's version too).
fn held(i: &SyncItem) -> bool {
    i.kind == SyncItemKind::File && matches!(i.state, SyncItemState::Downloaded | SyncItemState::ModifiedHere)
}

impl Host {
    pub(crate) fn new(core: Arc<dyn HostCore>, dir: &Path) -> Host {
        Host {
            core,
            dir: dir.to_path_buf(),
            folders: Mutex::new(Vec::new()),
            materialized: Mutex::new(HashMap::new()),
            to_materialize: Mutex::new(HashMap::new()),
            materializing: Mutex::new(HashMap::new()),
            serial: AtomicU64::new(0),
            reports: Mutex::new(HashMap::new()),
            report_lock: Mutex::new(()),
            last_report: Mutex::new(HashMap::new()),
            open: AtomicUsize::new(0),
            dir_versions: Mutex::new(HashMap::new()),
            releasing: Mutex::new(HashSet::new()),
            frozen: Mutex::new(HashSet::new()),
            hub: false,
            hub_state: None,
            hub_anchors: Mutex::new(None),
            pass_hold: Mutex::new(HashMap::new()),
            letting_go: Mutex::new(HashMap::new()),
            #[cfg(test)]
            before_copy: Mutex::new(None),
            hub_on_demand: Mutex::new(HashSet::new()),
            force_report: AtomicBool::new(false),
            nav_pane: Mutex::new(None),
            held_since: Mutex::new(HashMap::new()),
            keepalive: KEEPALIVE,
            replace_cap: trs_sync::ondemand::REPLACE_WINDOW,
            write_cap: trs_sync::ondemand::WRITE_HOLD_MAX,
            request_wait: REQUEST_WAIT,
            send_wait: SEND_WAIT,
            max_connections: MAX_CONNECTIONS,
        }
    }

    /// The files of folder `f` the core holds that the system does not have on disk (`on_disk`: the system's
    /// materialized set, as the host read it), queued for the host to ask for in batches ([`Self::next_batch`]): a
    /// folder switched to on demand, or one whose domain was added while its files are here, shows them downloaded.
    /// `on_disk` also becomes the baseline of the materialized set when there is none yet, so the reports that follow
    /// see these files arrive, never leave. Returns how many are queued.
    pub(crate) fn queue_materialize(&self, f: &str, on_disk: Vec<String>) -> Result<u64, HostError> {
        let on_disk: HashSet<String> = on_disk.into_iter().collect();
        let mut queue = VecDeque::new();
        let mut anchor = 0;
        loop {
            let page = self.core.changes(f, anchor, PAGE_MAX)?;
            queue.extend(page.items.iter().filter(|i| held(i) && !on_disk.contains(&i.id)).map(|i| i.id.clone()));
            if !page.more || page.anchor == anchor {
                break;
            }
            anchor = page.anchor;
        }
        lock(&self.materialized).entry(f.to_string()).or_insert(on_disk);
        let n = queue.len() as u64;
        lock(&self.to_materialize).insert(f.to_string(), queue);
        Ok(n)
    }

    /// The next batch to ask the system for (`None` when the folder's queue is done), each marked as asked for.
    pub(crate) fn next_batch(&self, f: &str) -> Option<SyncHostBatch> {
        let mut all = lock(&self.to_materialize);
        let queue = all.get_mut(f)?;
        let n = queue.len().min(MATERIALIZE_BATCH);
        if n == 0 {
            all.remove(f);
            return None;
        }
        let ids: Vec<String> = queue.drain(..n).collect();
        let left = queue.len() as u64;
        let now = Instant::now();
        let mut marked = lock(&self.materializing);
        marked.retain(|_, t| t.elapsed() < MATERIALIZE_TTL);
        for id in &ids {
            marked.insert((f.to_string(), id.clone()), now);
        }
        Some(SyncHostBatch { ids, pause_ms: MATERIALIZE_PAUSE.as_millis() as u64, left })
    }

    /// The fetch of `id` is the system acting on the host's request (and not the user's open): taken off the list.
    fn asked_to_materialize(&self, f: &str, id: &str) -> bool {
        lock(&self.materializing).remove(&(f.to_string(), id.to_string())).is_some_and(|t| t.elapsed() < MATERIALIZE_TTL)
    }

    /// The folder's role (looked up again when it is not known yet: a folder made on demand a moment ago).
    fn writable(&self, f: &str) -> Result<bool, HostError> {
        if let Some(w) = lock(&self.folders).iter().find(|x| x.id == f).map(|x| x.writable) {
            return Ok(w);
        }
        let now = self.core.folders().unwrap_or_default();
        let w = now.iter().find(|x| x.id == f).map(|x| x.writable);
        *lock(&self.folders) = now;
        w.ok_or_else(|| HostError::new(ErrKind::Unreachable, NOT_ON_DEMAND))
    }

    /// A folder's content version, `d<anchor>`: an anchor of the folder's index (so a version from before the index was
    /// made anew is told apart) whose sequence is at least the newest below it (where the feed was read up to when
    /// something below last changed, or when the server first looked), so it changes whenever anything
    /// below does, and a delete that carries it is refused by the core when something below is newer. Kept up from the
    /// change feed: serving a folder never walks what is below it.
    fn folder_version(&self, f: &str, i: &SyncItem) -> Option<String> {
        if i.kind != SyncItemKind::Dir || i.id == trs_sync::ondemand::ROOT_ID {
            return None;
        }
        self.catch_up(f);
        let all = lock(&self.dir_versions);
        // Not known (the feed could not be read): `d0`, which no delete passes (the index differs or nothing is
        // older), so the folder only comes back with a fresh version.
        let v = all.get(f).map_or(0, |v| v.newest.get(&i.id).copied().unwrap_or(v.base));
        Some(format!("d{v}"))
    }

    /// What a folder's delete saw: the sequence in its version `d<anchor>` when that is an anchor of this index; else
    /// 0 (no version, one that does not read, or one from before the index was made anew), so that anything below
    /// refuses it. Never "unchecked".
    fn seen_of(&self, f: &str, base: Option<&str>) -> u64 {
        let seen = base.and_then(|b| b.strip_prefix('d')).and_then(|n| n.parse::<u64>().ok());
        match (seen, self.core.anchor(f)) {
            (Some(a), Ok(now)) if trs_sync::ondemand::anchor_index(a) == trs_sync::ondemand::anchor_index(now) => trs_sync::ondemand::anchor_sequence(a),
            _ => 0,
        }
    }

    /// Reads the change feed from where the folder versions were last brought up to date: every folder above a changed
    /// item (and a changed folder) gets the sequence the page reaches. A first look, or an anchor that expired (the
    /// index made anew), starts again from now: every folder at that sequence.
    fn catch_up(&self, f: &str) {
        let Ok(now) = self.core.anchor(f) else { return };
        let mut upto = {
            let mut all = lock(&self.dir_versions);
            match all.get(f) {
                Some(v) if v.upto == now => return,
                Some(v) => v.upto,
                None => {
                    all.insert(f.to_string(), DirVersions { upto: now, base: now, newest: HashMap::new() });
                    return;
                }
            }
        };
        let mut moved: HashMap<String, u64> = HashMap::new();
        let mut parents: HashMap<String, String> = HashMap::new();
        loop {
            let page = match self.core.changes(f, upto, PAGE_MAX) {
                Ok(p) => p,
                Err(_) => {
                    lock(&self.dir_versions).insert(f.to_string(), DirVersions { upto: now, base: now, newest: HashMap::new() });
                    return;
                }
            };
            // The page's anchor: its index and how far it reaches.
            let seq = page.anchor;
            for i in &page.items {
                if i.kind == SyncItemKind::Dir {
                    moved.insert(i.id.clone(), seq);
                }
                let mut parent = i.parent_id.clone();
                while parent != trs_sync::ondemand::ROOT_ID {
                    moved.insert(parent.clone(), seq);
                    let next = match parents.get(&parent) {
                        Some(p) => p.clone(),
                        None => match self.core.item(f, &parent).ok().flatten() {
                            Some(p) => {
                                parents.insert(parent.clone(), p.parent_id.clone());
                                p.parent_id
                            }
                            None => break,
                        },
                    };
                    parent = next;
                }
            }
            let done = !page.more || page.anchor == upto;
            upto = page.anchor;
            if done {
                break;
            }
        }
        let mut all = lock(&self.dir_versions);
        if let Some(v) = all.get_mut(f) {
            for (id, seq) in moved {
                let e = v.newest.entry(id).or_insert(seq);
                *e = (*e).max(seq);
            }
            v.upto = upto;
        }
    }

    fn wire(&self, f: &str, i: &SyncItem) -> Result<WireItem, HostError> {
        let mut i = i.clone();
        if let Some(v) = self.folder_version(f, &i) {
            i.content_version = v;
        }
        let i = &i;
        let writable = self.writable(f)?;
        // Kept through a folder above (its pin, inherited) or its own: only an item whose parent is not kept is pinned
        // itself (one pinned itself below a pinned folder shows as inherited until the folder is unpinned).
        let above = i.pinned && i.id != i.parent_id && self.core.item(f, &i.parent_id).ok().flatten().is_some_and(|p| p.pinned);
        Ok(wire_item(i, writable, above))
    }

    fn wire_all(&self, f: &str, items: &[SyncItem]) -> Result<Vec<WireItem>, HostError> {
        let writable = self.writable(f)?;
        // The parents' pins, once each.
        let mut kept: BTreeMap<String, bool> = BTreeMap::new();
        items
            .iter()
            .map(|i| {
                let above = i.pinned
                    && i.id != i.parent_id
                    && *kept.entry(i.parent_id.clone()).or_insert_with(|| {
                        self.core.item(f, &i.parent_id).ok().flatten().is_some_and(|p| p.pinned)
                    });
                let mut w = wire_item(i, writable, above);
                if let Some(v) = self.folder_version(f, i) {
                    w.content_version = v;
                }
                Ok(w)
            })
            .collect()
    }

    fn item_or_gone(&self, f: &str, id: &str) -> Result<SyncItem, HostError> {
        self.core.item(f, id)?.ok_or_else(|| HostError::new(ErrKind::NoSuchItem, format!("{NO_ITEM}{id} in this folder.")))
    }

    /// The content the extension handed over for a new or changed file: `name` is a plain file name in `u/` (the only
    /// place content is taken from), the file a regular one of this user's with a single link, opened without
    /// following a link. The core then copies from that open file (`/dev/fd/N`), never from a path someone could
    /// swap meanwhile, so no client makes the app copy a file of its choosing into a synced folder.
    fn upload(&self, name: &str) -> Result<std::fs::File, HostError> {
        let refused = |why: &str| HostError::new(ErrKind::BadRequest, format!("Seedwell takes content only from the extension's uploads ({why})."));
        if name.is_empty() || name == "." || name == ".." || name.contains('/') || name.contains('\0') {
            return Err(refused("not a file name there"));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let dir = fsx::own_dir(&self.dir.join(UPLOADS)).map_err(|e| refused(&e.to_string()))?;
            let f = fsx::open_in(&dir, name).map_err(|e| refused(&e.to_string()))?;
            let m = f.metadata().map_err(|e| refused(&e.to_string()))?;
            if !m.file_type().is_file() || m.nlink() != 1 || m.uid() != fsx::uid() {
                return Err(refused("not a plain file of this user's"));
            }
            Ok(f)
        }
        #[cfg(not(unix))]
        {
            Err(refused("not on this system"))
        }
    }

    /// The path the core copies an upload from: the open file itself.
    fn via(f: &std::fs::File) -> String {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            format!("/dev/fd/{}", f.as_raw_fd())
        }
        #[cfg(not(unix))]
        {
            let _ = f;
            String::new()
        }
    }

    /// A fresh file name (for `t/`).
    fn fresh_name(&self) -> String {
        let n = self.serial.fetch_add(1, Ordering::Relaxed);
        let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        format!("{}-{t:x}-{n}", std::process::id())
    }

    /// A copy of the cache's file `from` for the extension, in `t/` (a real folder of ours, made anew if a link or
    /// anything else took its place), under a new name that nothing there may already have: a clone on APFS.
    fn hand_over(&self, from: &str) -> Result<PathBuf, HostError> {
        let failed = |e: std::io::Error| HostError::new(ErrKind::Failed, format!("Seedwell could not hand over the file: {e}"));
        #[cfg(unix)]
        {
            let dir_path = self.dir.join(CLONES);
            let dir = fsx::own_dir(&dir_path).map_err(failed)?;
            let src = std::fs::File::open(from).map_err(failed)?;
            let name = self.fresh_name();
            fsx::clone_into(&src, &dir, &name).map_err(failed)?;
            Ok(dir_path.join(name))
        }
        #[cfg(not(unix))]
        {
            let _ = from;
            Err(failed(std::io::Error::other("not on this system")))
        }
    }

    /// Answers one request (a fetch's progress goes to `progress`; `false` from it means the client is gone).
    pub(crate) fn answer(&self, r: &Request, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<Value, HostError> {
        if r.v != PROTOCOL {
            return Err(HostError::new(ErrKind::BadRequest, format!("This Seedwell speaks version {PROTOCOL} of the extension's protocol, not {}.", r.v)));
        }
        if self.hub && r.folder == hub::HUB_ID {
            return self.hub_answer(r, progress);
        }
        let f = r.folder.as_str();
        let item = || r.item.as_deref().ok_or_else(|| HostError::new(ErrKind::BadRequest, "The request names no item."));
        match r.op.as_str() {
            "hello" => Ok(json!({ "app": format!("Seedwell {}", env!("CARGO_PKG_VERSION")) })),
            "item" => {
                let i = self.item_or_gone(f, item()?)?;
                Ok(json!(self.wire(f, &i)?))
            }
            "children" => {
                let page = self.core.children(f, item()?, r.cursor.as_deref().filter(|c| !c.is_empty()), r.limit.unwrap_or(PAGE_MAX).clamp(1, PAGE_MAX))?;
                Ok(json!({ "items": self.wire_all(f, &page.items)?, "next": page.next }))
            }
            "changes" => {
                let p = self.core.changes(f, anchor_of(r.anchor.as_deref())?, r.limit.unwrap_or(PAGE_MAX).clamp(1, PAGE_MAX))?;
                let items = self.with_folders_above(f, p.items);
                Ok(json!({ "items": self.wire_all(f, &items)?, "deleted": p.deleted, "anchor": anchor_text(p.anchor), "more": p.more }))
            }
            "anchor" => Ok(json!({ "anchor": anchor_text(self.core.anchor(f)?) })),
            "fetch" => self.fetch(f, item()?, progress),
            "cancel" => {
                self.core.cancel(f, item()?)?;
                Ok(json!({}))
            }
            "create" => self.create(f, r),
            "modify" => self.modify(f, item()?, r),
            "delete" => self.delete(f, item()?, r),
            "pin" => {
                let i = self.core.pin(f, item()?, r.on.unwrap_or(true))?;
                Ok(json!({ "item": self.wire(f, &i)? }))
            }
            "materialized" => Ok(json!({ "released": self.report(f, r) })),
            other => Err(HostError::new(ErrKind::BadRequest, format!("Unknown request “{other}”."))),
        }
    }

    /// The changed items with the folders above them (their versions moved with them), each once, the folders first
    /// (outermost first: the system meets every parent before its children).
    fn with_folders_above(&self, f: &str, items: Vec<SyncItem>) -> Vec<SyncItem> {
        let mut seen: HashSet<String> = items.iter().map(|i| i.id.clone()).collect();
        let mut above: Vec<SyncItem> = Vec::new();
        for i in &items {
            let mut chain = Vec::new();
            let mut parent = i.parent_id.clone();
            while parent != trs_sync::ondemand::ROOT_ID && seen.insert(parent.clone()) {
                let Some(p) = self.core.item(f, &parent).ok().flatten() else { break };
                parent = p.parent_id.clone();
                chain.push(p);
            }
            above.extend(chain.into_iter().rev());
        }
        above.sort_by_key(|p| p.path.matches('/').count());
        above.into_iter().chain(items).collect()
    }

    /// Waits for the content, reporting progress, and hands the extension a copy of it (a clone on APFS) in `t/`. The
    /// core decides when it gives up (no device that has the file is connected, or none of it came for a while), and
    /// takes its request back then: its error is the answer.
    fn fetch(&self, f: &str, id: &str, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<Value, HostError> {
        let i = self.item_or_gone(f, id)?;
        if i.kind != SyncItemKind::File {
            return Err(HostError::new(ErrKind::Failed, "Only a file has content to fetch."));
        }
        // Asked for only to show what is held as downloaded: never a download (it went meanwhile: online only).
        if self.asked_to_materialize(f, id) && !held(&i) {
            return Err(HostError::new(ErrKind::Unreachable, "This file is no longer on this computer; opening it downloads it."));
        }
        if i.state == SyncItemState::Unavailable {
            return Err(HostError::new(ErrKind::Unreachable, trs_sync::ondemand::UNAVAILABLE));
        }
        loop {
            let got = self.core.fetch(f, id, PROGRESS_EVERY.as_millis() as u64)?;
            if let Some(from) = got.local_path.as_deref() {
                let to = self.hand_over(from)?;
                return Ok(json!({ "item": self.wire(f, &got.item)?, "path": to.display().to_string() }));
            }
            if !progress(got.done, got.total) {
                // The client went away (the user or the system cancelled).
                let _ = self.core.cancel(f, id);
                return Err(HostError::new(ErrKind::Failed, "The fetch was cancelled."));
            }
        }
    }

    /// The child of `parent` called `name`, if there is one.
    fn child(&self, f: &str, parent: &str, name: &str) -> Result<Option<SyncItem>, HostError> {
        let mut cursor: Option<String> = None;
        loop {
            let page = self.core.children(f, parent, cursor.as_deref(), PAGE_MAX)?;
            if let Some(i) = page.items.into_iter().find(|i| i.name == name) {
                return Ok(Some(i));
            }
            match page.next {
                Some(n) => cursor = Some(n),
                None => return Ok(None),
            }
        }
    }

    fn create(&self, f: &str, r: &Request) -> Result<Value, HostError> {
        let parent = r.parent.as_deref().unwrap_or(trs_sync::ondemand::ROOT_ID);
        let name = r.name.as_deref().ok_or_else(|| HostError::new(ErrKind::BadRequest, "A new item needs a name."))?;
        let dir = match r.kind.as_deref() {
            Some("dir") => true,
            Some("file") | None => false,
            Some(_) => return Err(HostError::new(ErrKind::Excluded, "Links are not synced in an on-demand folder; it stays on this Mac only.")),
        };
        let upload = if dir {
            None
        } else {
            Some(self.upload(r.contents.as_deref().ok_or_else(|| HostError::new(ErrKind::BadRequest, "A new file needs its content."))?)?)
        };
        // The system may not know whether it made this item already (a reimport): the same folder, or a file of the
        // same size and time, is the item there. Anything else is written, and where something else is, the core
        // makes it a conflict copy: nothing is lost either way.
        if r.may_exist == Some(true)
            && let Some(there) = self.child(f, parent, name)?
        {
            let same = match (dir, there.kind) {
                (true, SyncItemKind::Dir) => true,
                (false, SyncItemKind::File) => upload.as_ref().and_then(|u| u.metadata().ok()).is_some_and(|m| {
                    let mtime = m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64);
                    m.len() == there.size && (mtime - there.mtime_ns).abs() < 1_000_000_000
                }),
                _ => false,
            };
            if same {
                return Ok(json!({ "item": self.wire(f, &there)? }));
            }
        }
        let i = self.core.create(f, parent, name, dir, upload.as_ref().map(Self::via).as_deref())?;
        Ok(json!({ "item": self.wire(f, &i)? }))
    }

    fn modify(&self, f: &str, id: &str, r: &Request) -> Result<Value, HostError> {
        if r.parent.is_none() && r.name.is_none() && r.contents.is_none() {
            // Only fields Seedwell does not keep (dates, tags): the item as it is.
            let i = self.item_or_gone(f, id)?;
            return Ok(json!({ "item": self.wire(f, &i)?, "fetch_content": false, "conflict_copy": Value::Null }));
        }
        if r.parent.is_some() || r.name.is_some() {
            match self.rename_aside(f, id, r)? {
                Aside::Synced | Aside::OnDisk => {}
                // A file the system has not got on disk: nothing changes here yet. Excluded, the system downloads it,
                // then deletes it (deleteItem), and the core deletes it then.
                Aside::FileNotOnDisk => return Err(HostError::new(ErrKind::Excluded, "The new name is not synced; the file stays on this Mac only.")),
                // A folder with files the system has not got on disk (or has not said): refused, nothing changes.
                // (Excluded, the system would delete the folder with whatever it did not download.)
                Aside::FolderNotOnDisk(name) => {
                    return Err(HostError::new(
                        ErrKind::Failed,
                        format!("Seedwell doesn't sync the new name, so “{name}” would stay on this Mac only, and some of its files aren't on this Mac yet. Download it first, then rename it."),
                    ));
                }
            }
        }
        let upload = r.contents.as_deref().map(|c| self.upload(c)).transpose()?;
        let got = self.core.modify(f, id, r.parent.as_deref(), r.name.as_deref(), upload.as_ref().map(Self::via).as_deref(), r.base.as_deref())?;
        if got.id == id {
            return Ok(json!({ "item": self.wire(f, &got)?, "fetch_content": false, "conflict_copy": Value::Null }));
        }
        // The content had moved on: the user's edit is a conflict copy (a new item the change feed brings), and the
        // item keeps the other version, which the system fetches again.
        let now = self.item_or_gone(f, id)?;
        Ok(json!({ "item": self.wire(f, &now)?, "fetch_content": true, "conflict_copy": self.wire(f, &got)? }))
    }

    /// Where the rename in `r` takes item `id`, against the system's set of items on disk: the core must not delete what
    /// the system still has to download, and only a file is certain to be downloaded before the system deletes it.
    fn rename_aside(&self, f: &str, id: &str, r: &Request) -> Result<Aside, HostError> {
        let i = self.item_or_gone(f, id)?;
        let parent = match r.parent.as_deref() {
            Some(p) if p != trs_sync::ondemand::ROOT_ID => self.item_or_gone(f, p)?.path,
            Some(_) => String::new(),
            None => i.path.rsplit_once('/').map_or(String::new(), |(p, _)| p.to_string()),
        };
        let name = r.name.as_deref().unwrap_or(&i.name);
        let to = if parent.is_empty() { name.to_string() } else { format!("{parent}/{name}") };
        let dir = i.kind == SyncItemKind::Dir;
        if self.core.ignored(f, &to, dir)?.is_none() {
            return Ok(Aside::Synced);
        }
        let Some(on_disk) = lock(&self.materialized).get(f).cloned() else {
            // No report from the system yet: for a file the core's own check (its cache) decides; a folder is not known.
            return Ok(if dir { Aside::FolderNotOnDisk(i.name) } else { Aside::OnDisk });
        };
        if !dir {
            return Ok(if on_disk.contains(id) { Aside::OnDisk } else { Aside::FileNotOnDisk });
        }
        // Every file below it, folder by folder.
        let mut dirs = vec![id.to_string()];
        while let Some(d) = dirs.pop() {
            let mut cursor: Option<String> = None;
            loop {
                let page = self.core.children(f, &d, cursor.as_deref(), PAGE_MAX)?;
                for c in page.items {
                    match c.kind {
                        SyncItemKind::Dir => dirs.push(c.id),
                        SyncItemKind::File if !on_disk.contains(&c.id) => return Ok(Aside::FolderNotOnDisk(i.name)),
                        _ => {}
                    }
                }
                match page.next {
                    Some(n) => cursor = Some(n),
                    None => break,
                }
            }
        }
        Ok(Aside::OnDisk)
    }

    fn delete(&self, f: &str, id: &str, r: &Request) -> Result<Value, HostError> {
        let i = self.item_or_gone(f, id)?;
        if r.recursive == Some(false) && i.kind == SyncItemKind::Dir && !self.core.children(f, id, None, 1)?.items.is_empty() {
            return Err(HostError::new(ErrKind::NotEmpty, "The folder is not empty."));
        }
        // A folder: the version the system saw is the newest sequence below then; the core refuses it when something
        // below is newer (another device's change the user has not seen), and the folder comes back as it is now.
        // A file: the content version the user saw; a newer one is not deleted unseen. Both are checked on the
        // folder's core with the delete, and the item comes back as it is now.
        let seen = match i.kind {
            SyncItemKind::Dir => Some(trs_sync_net::DeleteSeen::Folder(self.seen_of(f, r.base.as_deref()))),
            _ => r.base.clone().map(trs_sync_net::DeleteSeen::File),
        };
        let d = self.core.delete(f, id, seen).map_err(|e| {
            let changed = e.kind == OdKind::Changed;
            let mut h = HostError::from(e);
            if changed {
                h.kind = ErrKind::DeletionRejected;
            }
            h
        })?;
        if d.freed.is_some() {
            return Err(HostError::new(
                ErrKind::DeletionRejected,
                "You can read this folder but not change it, so the file was removed from this Mac only. It stays in the folder, online only.",
            ));
        }
        if let Some(first) = d.failed.first() {
            return Err(HostError::new(ErrKind::DeletionRejected, format!("“{}” could not be deleted: {}", first.path, first.error)));
        }
        Ok(json!({ "deleted": d.deleted }))
    }

    /// A part of a report of the system's set of items on disk: kept until its last part, then taken as the new set,
    /// unless a later report was taken already (this one is stale). A report without a number is whole.
    pub(crate) fn report(&self, f: &str, r: &Request) -> u64 {
        let _one = lock(&self.report_lock);
        match self.report_whole(f, r) {
            Some(whole) => self.materialized(f, whole),
            None => 0,
        }
    }

    /// The whole set once the last part of a report is in (`None` before that, and for a stale report). Called with
    /// `report_lock` held.
    pub(crate) fn report_whole(&self, f: &str, r: &Request) -> Option<Vec<String>> {
        let ids = r.ids.clone().unwrap_or_default();
        let Some(n) = r.report else { return Some(ids) };
        let (part, parts) = (r.part.unwrap_or(0), r.parts.unwrap_or(1).max(1));
        if part >= parts || lock(&self.last_report).get(f).is_some_and(|last| *last >= n) {
            return None;
        }
        let whole = {
            let mut all = lock(&self.reports);
            let e = all.entry(f.to_string()).or_insert_with(|| Report { n, parts, got: BTreeMap::new() });
            if e.n != n || e.parts != parts {
                // A newer report starts (an unfinished older one is dropped); an older one's part is ignored.
                if n < e.n {
                    return None;
                }
                *e = Report { n, parts, got: BTreeMap::new() };
            }
            e.got.insert(part, ids);
            // Taken only whole: every part of it here (one whose parts went before a restart never is).
            if e.got.len() < parts as usize {
                return None;
            }
            all.remove(f).map(|r| r.got.into_values().flatten().collect::<Vec<_>>()).unwrap_or_default()
        };
        lock(&self.last_report).insert(f.to_string(), n);
        Some(whole)
    }

    /// The system's new set of items on disk: the files that left it since the last report were let go of by the
    /// system (Remove Download, disk pressure), and the core frees them too (it keeps what only this computer has,
    /// and what it cannot confirm another device holds). The first report after a start is the baseline only.
    /// Returns how many files are being released (in the background: asking other devices takes a while).
    pub(crate) fn materialized(&self, f: &str, ids: Vec<String>) -> u64 {
        let now: HashSet<String> = ids.into_iter().collect();
        let before = lock(&self.materialized).insert(f.to_string(), now.clone());
        let Some(before) = before else { return 0 };
        let gone: Vec<String> = before.difference(&now).cloned().collect();
        let gone: Vec<SyncItem> = gone
            .into_iter()
            .filter_map(|id: String| self.core.item(f, &id).ok().flatten())
            .filter(|i| i.kind == SyncItemKind::File && i.state == SyncItemState::Downloaded)
            .collect();
        let n = gone.len() as u64;
        if n > 0 {
            let (core, f) = (self.core.clone(), f.to_string());
            std::thread::spawn(move || {
                for i in gone {
                    match core.evict(&f, &i.id) {
                        Ok(e) if e.kept > 0 || !e.failed.is_empty() => {
                            let why = e.kept_why.iter().chain(&e.failed).map(|x| x.error.as_str()).next().unwrap_or("");
                            eprintln!("seedwell: file provider: {} stays on this computer: {why}", i.path);
                        }
                        Ok(_) => {}
                        Err(e) => eprintln!("seedwell: file provider: releasing {} failed: {e}", i.path),
                    }
                }
            });
        }
        n
    }

    /// Removes the regular files left directly in `t/` and `u/` that came there more than `age` ago (by their change
    /// time: the modification time is the original's, which a clone or copy keeps). Each is a real folder of ours
    /// first (a link in its place is replaced, never followed); nothing else in them is touched.
    pub(crate) fn sweep(&self, age: Duration) {
        #[cfg(unix)]
        for sub in [CLONES, UPLOADS] {
            let path = self.dir.join(sub);
            if fsx::own_dir(&path).is_err() {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(&path) else { continue };
            for e in entries.flatten() {
                // `file_type` and `metadata` of an entry do not follow a link.
                let Ok(m) = e.metadata() else { continue };
                use std::os::unix::fs::MetadataExt;
                let came = SystemTime::UNIX_EPOCH + Duration::new(u64::try_from(m.ctime()).unwrap_or(0), u32::try_from(m.ctime_nsec()).unwrap_or(0));
                let old = came.elapsed().is_ok_and(|a| a >= age);
                if e.file_type().is_ok_and(|t| t.is_file()) && old {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        #[cfg(not(unix))]
        let _ = age;
    }

    /// One look at the folders: the domains when the set or a name changed, a signal for each folder whose change
    /// feed moved (every folder on the first look, so the system catches up after a start). Nothing while folder
    /// sync is not running (a start, a port change: its list is not the folders' then), and a folder being let go of
    /// is left out until that is over. (The host only adds and renames from this list; a domain goes only when the
    /// core lets go of its folder.)
    pub(crate) fn watch_once(&self, listener: &dyn SyncHostListener, anchors: &mut HashMap<String, u64>, shown: &mut Option<Vec<SyncHostDomain>>) {
        let Some(mut folders) = self.core.folders() else { return };
        {
            let releasing = lock(&self.releasing);
            folders.retain(|f| !releasing.contains(&f.id));
        }
        // The hub: one domain while any folder is synced (its tree is the core's, `hub`); else one per on-demand folder.
        let tops = if self.hub { self.hub_tops() } else { None };
        let domains: Vec<SyncHostDomain> = match &tops {
            Some(t) if !t.is_empty() => vec![hub::hub_domain()],
            Some(_) => Vec::new(),
            None if self.hub => return,
            None => folders.iter().map(|f| SyncHostDomain { id: f.id.clone(), name: f.name.clone() }).collect(),
        };
        *lock(&self.folders) = folders.clone();
        // The hub: told again when an on-demand folder joined it (switched or joined here, by the app, the command line
        // or an agent), so the host shows its held files downloaded.
        let mut grew = false;
        if self.hub {
            let now: HashSet<String> = folders.iter().map(|f| f.id.clone()).collect();
            let mut before = lock(&self.hub_on_demand);
            grew = !now.is_subset(&before);
            *before = now;
        }
        if self.force_report.swap(false, Ordering::SeqCst) || grew || shown.as_ref() != Some(&domains) {
            listener.domains(domains.clone());
            *shown = Some(domains);
        }
        // Seedwell's sidebar entry: told at the first look while folder sync runs, then on each change (the location
        // set elsewhere, the first folder in it, the last gone).
        if let Some(mut entry) = self.core.nav_pane() {
            // Only a folder that is there: one chosen in Settings but not made yet, or on a disk that is away, would
            // open nothing. Looked at here, on the core's thread (a disk that is away may be slow to answer), never in
            // `Core::sync_nav_pane`; the host acts on what it is told here.
            entry.show = entry.show && Path::new(&entry.target).is_dir();
            let mut told = lock(&self.nav_pane);
            if told.as_ref() != Some(&entry) {
                listener.nav_pane(entry.clone());
                *told = Some(entry);
            }
        }
        // (The hub's own entries are kept: its report in parts, the top level's state.)
        let keep = |id: &String| folders.iter().any(|f| &f.id == id) || (self.hub && id == hub::HUB_ID);
        anchors.retain(|id, _| keep(id));
        lock(&self.materialized).retain(|id, _| keep(id));
        lock(&self.reports).retain(|id, _| keep(id));
        lock(&self.last_report).retain(|id, _| keep(id));
        lock(&self.dir_versions).retain(|id, _| keep(id));
        lock(&self.pass_hold).retain(|id, _| keep(id));
        // The hub hears of a move of any folder's feed, and of a change at its top level (a folder added, renamed,
        // switched or let go of), as one signal.
        let mut hub_moved = false;
        if let Some(t) = &tops {
            let h = hub::tops_hash(t);
            hub_moved = anchors.insert(hub::HUB_ID.to_string(), h) != Some(h);
        }
        for f in &folders {
            // A delete a moment ago may turn out to be a save by replace: the system hears of it once that is settled
            // (one edit), as the other devices do.
            // At most one window, though: a run of deletes never keeps the system from hearing of changes. Longer only
            // while the new file is still being written at the name (a big file read in): until that write ends, and
            // never past `write_cap`.
            if let Some(hold) = self.core.replace_hold(&f.id) {
                let mut held = lock(&self.held_since);
                let since = *held.entry(f.id.clone()).or_insert_with(Instant::now);
                if since.elapsed() < if hold.writing { self.write_cap } else { self.replace_cap } {
                    continue;
                }
                held.insert(f.id.clone(), Instant::now());
                // The hub reads the folder's feed once now, for this hold (not one a new delete starts).
                lock(&self.pass_hold).insert(f.id.clone(), Instant::now() + hold.left);
            } else {
                lock(&self.held_since).remove(&f.id);
                lock(&self.pass_hold).remove(&f.id);
            }
            let Ok(a) = self.core.anchor(&f.id) else { continue };
            if anchors.insert(f.id.clone(), a) != Some(a) {
                if self.hub {
                    hub_moved = true;
                } else {
                    listener.feed_moved(f.id.clone());
                }
            }
        }
        if hub_moved {
            listener.feed_moved(hub::HUB_ID.to_string());
        }
    }
}

// ------------------------------------------------------------------------------------------ the container's folders

/// The container's folders and files as the server touches them: never through a link, never someone else's.
#[cfg(unix)]
pub(crate) mod fsx {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;

    #[cfg(target_os = "macos")]
    const O_NOFOLLOW: i32 = 0x100;
    #[cfg(target_os = "macos")]
    const O_DIRECTORY: i32 = 0x0010_0000;
    #[cfg(target_os = "macos")]
    const O_CLOEXEC: i32 = 0x0100_0000;
    #[cfg(target_os = "macos")]
    const O_NONBLOCK: i32 = 0x4;
    #[cfg(not(target_os = "macos"))]
    const O_NONBLOCK: i32 = 0o4000;
    #[cfg(not(target_os = "macos"))]
    const O_NOFOLLOW: i32 = 0o400_000;
    #[cfg(not(target_os = "macos"))]
    const O_DIRECTORY: i32 = 0o200_000;
    #[cfg(not(target_os = "macos"))]
    const O_CLOEXEC: i32 = 0o2_000_000;

    #[cfg(target_os = "macos")]
    const O_WRONLY_CREAT_EXCL: i32 = 0x1 | 0x200 | 0x800;
    #[cfg(not(target_os = "macos"))]
    const O_WRONLY_CREAT_EXCL: i32 = 0o1 | 0o100 | 0o200;

    unsafe extern "C" {
        fn openat(fd: i32, path: *const std::ffi::c_char, flags: i32, ...) -> i32;
        fn getuid() -> u32;
        #[cfg(target_os = "macos")]
        fn fclonefileat(src: i32, dst_dir: i32, dst: *const std::ffi::c_char, flags: u32) -> i32;
    }

    /// A copy of `src` as the new file `name` in the open folder `dir`: a clone where the volume can (APFS), else
    /// written out; never through a link, never over something already there.
    pub fn clone_into(src: &File, dir: &File, name: &str) -> io::Result<()> {
        let c = std::ffi::CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        #[cfg(target_os = "macos")]
        {
            // SAFETY: open descriptors and a NUL-terminated name.
            if unsafe { fclonefileat(src.as_raw_fd(), dir.as_raw_fd(), c.as_ptr(), 0) } == 0 {
                return Ok(());
            }
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::AlreadyExists {
                return Err(e);
            }
        }
        // SAFETY: an open folder, a NUL-terminated name, a mode; the descriptor is owned from here.
        let fd = unsafe { openat(dir.as_raw_fd(), c.as_ptr(), O_WRONLY_CREAT_EXCL | O_NOFOLLOW | O_CLOEXEC, 0o600 as std::ffi::c_uint) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just opened.
        let mut out = unsafe { File::from_raw_fd(fd) };
        let mut input = src;
        io::copy(&mut input, &mut out)?;
        Ok(())
    }

    /// This process's user.
    pub fn uid() -> u32 {
        // SAFETY: getuid(2) cannot fail.
        unsafe { getuid() }
    }

    fn not_ours(path: &Path) -> io::Error {
        io::Error::other(format!("{} is not a folder of Seedwell's", path.display()))
    }

    /// A folder of ours at `path`, open: a real directory (never a link) owned by this user, made when missing;
    /// anything else in its place (a link, a file) is removed first.
    pub fn own_dir(path: &Path) -> io::Result<File> {
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => std::fs::remove_file(path)?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        match std::fs::DirBuilder::new().mode(0o700).create(path) {
            Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
            _ => {}
        }
        let f = OpenOptions::new().read(true).custom_flags(O_DIRECTORY | O_NOFOLLOW).open(path).map_err(|_| not_ours(path))?;
        let m = f.metadata()?;
        if !m.is_dir() || m.uid() != uid() {
            return Err(not_ours(path));
        }
        // The user's alone, also one made earlier with looser permissions (through the open folder: no path again).
        if m.mode() & 0o777 != 0o700 {
            f.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(f)
    }

    /// The file `name` in the open folder `dir`, opened for reading without following a link.
    pub fn open_in(dir: &File, name: &str) -> io::Result<File> {
        let c = std::ffi::CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: an open folder, a NUL-terminated name; the descriptor is owned from here.
        // Non-blocking: a named pipe there opens at once (and is then refused: not a regular file).
        let fd = unsafe { openat(dir.as_raw_fd(), c.as_ptr(), O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK) };
        if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { File::from_raw_fd(fd) }) }
    }
}

// ------------------------------------------------------------------------------------------ the socket

/// Reads one request line: at most [`REQUEST_MAX`] bytes, or [`REPORT_MAX`] for a part of a report of the system's set,
/// which is known as one before reading past the limit (the extension writes the op first).
fn read_request(conn: &mut dyn Read) -> Result<Request, HostError> {
    let bad = |why: String| HostError::new(ErrKind::BadRequest, format!("The request could not be read: {why}"));
    let mut line: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 16 << 10];
    let mut limit = REQUEST_MAX;
    while !line.contains(&b'\n') {
        if line.len() as u64 > limit {
            if limit == REQUEST_MAX && line.windows(19).any(|w| w == br#""op":"materialized""#) {
                limit = REPORT_MAX;
            } else {
                return Err(bad("it is too long".into()));
            }
        }
        let n = conn.read(&mut chunk).map_err(|e| bad(e.to_string()))?;
        if n == 0 {
            return Err(bad("it is incomplete".into()));
        }
        line.extend_from_slice(&chunk[..n]);
    }
    let end = line.iter().position(|b| *b == b'\n').unwrap_or(line.len());
    if end as u64 > limit {
        return Err(bad("it is too long".into()));
    }
    let r: Request = serde_json::from_slice(&line[..end]).map_err(|e| bad(e.to_string()))?;
    if end as u64 > REQUEST_MAX && r.op != "materialized" {
        return Err(bad("it is too long".into()));
    }
    Ok(r)
}

fn final_line(r: Result<Value, HostError>) -> String {
    let v = match r {
        Ok(ok) => json!({ "v": PROTOCOL, "ok": ok }),
        Err(e) => json!({ "v": PROTOCOL, "err": { "kind": e.kind, "message": e.message } }),
    };
    format!("{v}\n")
}

/// Serves one connection: one request, its progress, one answer; while the work takes long, a line now and then keeps
/// the connection alive.
pub(crate) fn serve<S: Read + Write + Send>(host: &Host, conn: &mut S) {
    let r = match read_request(conn) {
        Ok(r) => r,
        Err(e) => {
            let _ = conn.write_all(final_line(Err(e)).as_bytes());
            return;
        }
    };
    let out = Mutex::new(conn);
    let answer = std::thread::scope(|sc| {
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let out = &out;
        sc.spawn(move || {
            let alive = format!("{}\n", json!({ "v": PROTOCOL, "alive": true }));
            while let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = stopped.recv_timeout(host.keepalive) {
                if lock(out).write_all(alive.as_bytes()).is_err() {
                    break;
                }
            }
        });
        let mut progress = |done: u64, total: u64| {
            let line = format!("{}\n", json!({ "v": PROTOCOL, "progress": { "done": done, "total": total } }));
            lock(out).write_all(line.as_bytes()).is_ok()
        };
        let a = host.answer(&r, &mut progress);
        drop(stop);
        a
    });
    let conn = out.into_inner().unwrap_or_else(PoisonError::into_inner);
    if let Err(e) = &answer
        && e.kind != ErrKind::NoSuchItem
    {
        eprintln!("seedwell: file provider: {} {}: {}", r.op, r.item.as_deref().unwrap_or(""), e.message);
    }
    let _ = conn.write_all(final_line(answer).as_bytes());
}

#[cfg(unix)]
pub use unix::Server;

#[cfg(unix)]
mod unix {
    use std::os::unix::net::{UnixListener, UnixStream};

    use super::*;

    /// The audit token of the process at the other end (`LOCAL_PEERTOKEN`).
    #[cfg(target_os = "macos")]
    pub(crate) fn peer_token(c: &UnixStream) -> Option<Vec<u8>> {
        use std::os::fd::AsRawFd;
        const SOL_LOCAL: i32 = 0;
        const LOCAL_PEERTOKEN: i32 = 0x006;
        let mut token = [0u8; 32];
        let mut len: u32 = 32;
        // SAFETY: a valid socket, a 32-byte buffer and its length.
        let r = unsafe { at::getsockopt(c.as_raw_fd(), SOL_LOCAL, LOCAL_PEERTOKEN, token.as_mut_ptr().cast(), &mut len) };
        (r == 0 && len == 32).then(|| token.to_vec())
    }

    #[cfg(not(target_os = "macos"))]
    pub(crate) fn peer_token(_: &UnixStream) -> Option<Vec<u8>> {
        None
    }

    /// A connection whose reads share one deadline (a request trickled byte by byte gets no more time).
    pub(crate) struct Deadline<'a> {
        pub c: &'a mut UnixStream,
        pub end: Instant,
    }

    impl Read for Deadline<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = self.end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(std::io::Error::from(std::io::ErrorKind::TimedOut));
            }
            self.c.set_read_timeout(Some(left))?;
            self.c.read(buf)
        }
    }

    impl Write for Deadline<'_> {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.c.write(buf)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.c.flush()
        }
    }

    /// The running server: the socket's listener and the watcher. Stops when dropped.
    pub struct Server {
        pub(crate) host: Arc<Host>,
        stop: Arc<AtomicBool>,
        socket: PathBuf,
        dir: PathBuf,
    }

    impl Server {
        /// Listens at `<dir>/fp` (made anew) and watches the folders for `listener`, a domain per folder (tests; the
        /// app says its mode through `Core::sync_host_start`).
        #[cfg(test)]
        pub(crate) fn start(core: Arc<dyn HostCore>, dir: &Path, listener: Arc<dyn SyncHostListener>) -> std::io::Result<Server> {
            Self::start_host(Host::new(core, dir), dir, listener)
        }

        /// The same with a host made already (tests shorten its waits).
        pub(crate) fn start_host(host: Host, dir: &Path, listener: Arc<dyn SyncHostListener>) -> std::io::Result<Server> {
            std::fs::create_dir_all(dir)?;
            let socket = dir.join(SOCKET_NAME);
            let l = bind(dir, SOCKET_NAME)?;
            let host = Arc::new(host);
            host.core.set_release(Some(Arc::new(Release { host: host.clone(), listener: listener.clone() })));
            host.sweep(LEFTOVER_AGE);
            let stop = Arc::new(AtomicBool::new(false));
            {
                let (host, stop, listener) = (host.clone(), stop.clone(), listener.clone());
                std::thread::Builder::new().name("fp-server".into()).spawn(move || {
                    for conn in l.incoming() {
                        if stop.load(Ordering::Relaxed) {
                            break;
                        }
                        let Ok(mut c) = conn else {
                            // (Out of descriptors, say: not a busy loop.)
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        };
                        // Too many at once: turned away (the system retries), never queued.
                        if host.open.fetch_add(1, Ordering::SeqCst) >= host.max_connections {
                            host.open.fetch_sub(1, Ordering::SeqCst);
                            let _ = c.set_write_timeout(Some(host.send_wait));
                            let _ = c.write_all(final_line(Err(HostError::new(ErrKind::Unreachable, "Seedwell is busy; try again in a moment."))).as_bytes());
                            continue;
                        }
                        // The slot goes back when the connection is done, also when its thread cannot start.
                        struct Open(Arc<Host>);
                        impl Drop for Open {
                            fn drop(&mut self) {
                                self.0.open.fetch_sub(1, Ordering::SeqCst);
                            }
                        }
                        let open = Open(host.clone());
                        let (host, listener) = (host.clone(), listener.clone());
                        let _ = std::thread::Builder::new().name("fp-request".into()).spawn(move || {
                            let _open = open;
                            no_sigpipe(&c);
                            let _ = c.set_write_timeout(Some(host.send_wait));
                            // Only this app's own extension is served: checked before anything is read.
                            if !peer_token(&c).is_some_and(|t| listener.verify_peer(t)) {
                                eprintln!("seedwell: file provider: a connection from another process was refused");
                                let _ = c.write_all(final_line(Err(HostError::new(ErrKind::BadRequest, "Only Seedwell's own extension is served here."))).as_bytes());
                                return;
                            }
                            let end = Instant::now() + host.request_wait;
                            serve(&host, &mut Deadline { c: &mut c, end });
                        });
                    }
                })?;
            }
            {
                let (host, stop) = (host.clone(), stop.clone());
                std::thread::Builder::new().name("fp-watch".into()).spawn(move || {
                    let (mut anchors, mut shown) = (HashMap::new(), None);
                    let mut ticks = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        host.watch_once(listener.as_ref(), &mut anchors, &mut shown);
                        ticks += 1;
                        if ticks.is_multiple_of(60) {
                            host.sweep(LEFTOVER_AGE);
                        }
                        std::thread::sleep(WATCH_EVERY);
                    }
                })?;
            }
            Ok(Server { host, stop, socket, dir: dir.to_path_buf() })
        }

        /// The socket's path.
        pub fn socket(&self) -> &Path {
            &self.socket
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.host.core.set_release(None);
            self.stop.store(true, Ordering::Relaxed);
            // Wakes the listener, which sees the flag; the socket goes, so the extension knows the app is gone.
            let _ = connect(&self.dir, SOCKET_NAME);
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    /// The longest path `sun_path` takes (104 bytes with its NUL on the Mac).
    const SUN_PATH_MAX: usize = 103;

    /// Binds `<dir>/<name>`: by its path when it fits, else relative to the folder.
    pub(crate) fn bind(dir: &Path, name: &str) -> std::io::Result<UnixListener> {
        let path = dir.join(name);
        match std::fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let l = if path.as_os_str().len() <= SUN_PATH_MAX { UnixListener::bind(&path)? } else { at::bind(dir, name)? };
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        Ok(l)
    }

    /// Connects to `<dir>/<name>` the same way (tests, and waking the listener).
    pub(crate) fn connect(dir: &Path, name: &str) -> std::io::Result<UnixStream> {
        let path = dir.join(name);
        if path.as_os_str().len() <= SUN_PATH_MAX { UnixStream::connect(&path) } else { at::connect(dir, name) }
    }

    #[cfg(target_os = "macos")]
    fn no_sigpipe(c: &UnixStream) {
        use std::os::fd::AsRawFd;
        let one: i32 = 1;
        // SAFETY: a valid socket and a 4-byte option value.
        unsafe { at::setsockopt(c.as_raw_fd(), at::SOL_SOCKET, at::SO_NOSIGPIPE, (&raw const one).cast(), 4) };
    }

    #[cfg(not(target_os = "macos"))]
    fn no_sigpipe(_: &UnixStream) {}

    /// A socket path of any length: bound or reached by its name alone from a thread whose own working folder is the
    /// socket's folder (`pthread_fchdir_np`, per thread: the process's working folder never changes). (`bindat` and
    /// `connectat` are not in the SDK.)
    #[cfg(target_os = "macos")]
    mod at {
        use std::os::fd::AsRawFd;
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::Path;

        pub const SOL_SOCKET: i32 = 0xffff;
        pub const SO_NOSIGPIPE: i32 = 0x1022;

        unsafe extern "C" {
            fn pthread_fchdir_np(fd: i32) -> i32;
            pub fn setsockopt(s: i32, level: i32, name: i32, value: *const std::ffi::c_void, len: u32) -> i32;
            pub fn getsockopt(s: i32, level: i32, name: i32, value: *mut std::ffi::c_void, len: *mut u32) -> i32;
        }

        /// Runs `f` on a thread of its own whose working folder is `dir`.
        fn in_dir<T: Send>(dir: &Path, f: impl FnOnce() -> std::io::Result<T> + Send) -> std::io::Result<T> {
            let d = std::fs::File::open(dir)?;
            std::thread::scope(|s| {
                s.spawn(|| {
                    // SAFETY: an open folder; the working folder set is this thread's alone, and the thread ends here.
                    if unsafe { pthread_fchdir_np(d.as_raw_fd()) } != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    f()
                })
                .join()
                .unwrap_or_else(|_| Err(std::io::Error::other("the socket thread panicked")))
            })
        }

        pub fn bind(dir: &Path, name: &str) -> std::io::Result<UnixListener> {
            in_dir(dir, || UnixListener::bind(name))
        }

        pub fn connect(dir: &Path, name: &str) -> std::io::Result<UnixStream> {
            in_dir(dir, || UnixStream::connect(name))
        }
    }

    /// Elsewhere a long path is not reached (the server runs on the Mac only).
    #[cfg(not(target_os = "macos"))]
    mod at {
        use std::os::unix::net::{UnixListener, UnixStream};
        use std::path::Path;

        pub fn bind(_: &Path, _: &str) -> std::io::Result<UnixListener> {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket path too long"))
        }
        pub fn connect(_: &Path, _: &str) -> std::io::Result<UnixStream> {
            Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket path too long"))
        }
    }
}

// ------------------------------------------------------------------------------------------ the app's calls

#[uniffi::export]
impl Core {
    /// Serves the File Provider extension of on-demand folders (SPEC §7G.11) at `<dir>/fp`, `dir` being the app group's
    /// container, and tells `listener` which domains there should be and when a domain's change feed moved. `mode`:
    /// the host says how it shows the folders. `Hub` (the Mac): one domain, `sync_host_hub_domain`, listing every
    /// synced folder (on-demand ones as folders, full ones as links to their place), its feed one signal; `Folders`:
    /// a domain per on-demand folder. Requests for a folder's own domain (one left from before the hub, until the host's
    /// migration removes it) are served in either mode. Only the user's own app runs it: an isolated run (tests, QA)
    /// is refused, so it never registers a domain nor takes the socket. Starting again replaces the server. Returns
    /// the socket's path.
    pub fn sync_host_start(&self, dir: String, listener: Arc<dyn SyncHostListener>, mode: hub::SyncHostMode) -> Result<String, CoreError> {
        if self.isolated {
            return Err(CoreError::Failed { message: "An isolated run does not serve the File Provider extension.".into() });
        }
        #[cfg(unix)]
        {
            let mut slot = lock(&self.host);
            *slot = None;
            let core: Arc<dyn HostCore> = Arc::new(BackendCore(self.backend.sync(), None));
            let mut host = Host::new(core, Path::new(&dir));
            host.hub = mode == hub::SyncHostMode::Hub;
            host.hub_state = Some(self.dir.clone());
            let s = Server::start_host(host, Path::new(&dir), listener)
                .map_err(|e| CoreError::Failed { message: format!("The Finder's on-demand folders could not start: {e}.") })?;
            let path = s.socket().display().to_string();
            *slot = Some(s);
            Ok(path)
        }
        #[cfg(not(unix))]
        {
            let _ = (dir, listener, mode);
            Err(CoreError::Failed { message: "The File Provider extension is the Mac's.".into() })
        }
    }

    /// Files the core holds in on-demand folder `id` but the system does not have on disk (`on_disk`: the domain's
    /// materialized set, as the host read it), queued for the host to ask the system to download, so they show
    /// downloaded (the fetch hands back clones: no space twice, nothing downloaded). Returns how many; the host then
    /// takes them with `sync_host_next_batch`. For the hub (`id` = its domain's), every on-demand folder in it, its
    /// identifiers the hub's. Without the server, nothing.
    pub fn sync_host_materialize(&self, id: String, on_disk: Vec<String>) -> Result<u64, CoreError> {
        #[cfg(unix)]
        if let Some(s) = lock(&self.host).as_ref() {
            if s.host.hub && id == hub::HUB_ID {
                return Ok(s.host.hub_queue_materialize(on_disk));
            }
            return s.host.queue_materialize(&id, on_disk).map_err(|e| CoreError::Failed { message: e.message });
        }
        let _ = (id, on_disk);
        Ok(0)
    }

    /// The next batch of files to ask the system for, and how long to wait after it; `None` when there are no more.
    pub fn sync_host_next_batch(&self, id: String) -> Option<SyncHostBatch> {
        #[cfg(unix)]
        if let Some(s) = lock(&self.host).as_ref() {
            if s.host.hub && id == hub::HUB_ID {
                return s.host.hub_next_batch();
            }
            return s.host.next_batch(&id);
        }
        let _ = id;
        None
    }

    /// The hub's domain (the Mac: one *Seedwell* in the Finder's sidebar for every synced folder).
    pub fn sync_host_hub_domain(&self) -> SyncHostDomain {
        hub::hub_domain()
    }

    /// The hub's item for folder `id`: the folder an on-demand folder is, or the link a full folder is (the folder's
    /// configuration says which). For the host's calls on it (`waitForChanges(below:)`, its place in the Finder). Cheap:
    /// it reads the configuration in memory.
    pub fn sync_host_hub_item(&self, id: String) -> String {
        hub::top_id(&id, self.backend.sync().configured(&id).flatten().unwrap_or(false))
    }

    /// The hub lets go of on-demand folder `id` (the host asked by `remove_domain`, after the system handed over its
    /// pending changes below the folder): from now on what the system sends for it is refused; what is still on disk
    /// below its place in the Finder (`visible`, the folder's user-visible path; `None` when the system has none:
    /// nothing of it is on disk) that the folder does not hold as it is there (made or changed after the hand-over,
    /// a name or a link the folder does not sync) is copied to `<name> (changes not synced)` next to the folder's
    /// place, the place looked at again while the folder is still listed until a look finds nothing new (what was
    /// saved meanwhile is kept too, and what the system still sends is kept before it is refused); only then does the
    /// folder leave the hub (the system deletes what it has of it once the host signals). Returns what to tell the
    /// user, if anything was kept. An error when anything found could not be copied (a full disk, a file it cannot
    /// read), or when the folder kept changing look after look: the folder is then back as it was, and the host
    /// refuses the removal or switch with the error's words (the system would delete what was not kept). Reads the
    /// disk: call it off the main thread. Undone by
    /// `sync_host_let_go_undo`, and over when the core's removal or switch ends.
    pub fn sync_host_let_go(&self, id: String, visible: Option<String>) -> Result<Option<SyncHostNotice>, CoreError> {
        #[cfg(unix)]
        let host = lock(&self.host).as_ref().map(|s| s.host.clone());
        #[cfg(unix)]
        if let Some(s) = host {
            let folder = self.backend.sync().snapshot().folders.into_iter().find(|f| f.id == id);
            let name = folder.as_ref().map_or_else(|| id.clone(), |f| f.name.clone());
            let place = folder.map(|f| PathBuf::from(f.path));
            return s.let_go(&id, &name, place.as_deref(), visible.as_deref().map(Path::new)).map_err(|message| CoreError::Failed { message });
        }
        let _ = (id, visible);
        Ok(None)
    }

    /// The core gave up on a removal while the host let go of the folder (`sync_host_let_go`): the folder comes back in
    /// the hub, and its changes are taken again.
    pub fn sync_host_let_go_undo(&self, id: String) {
        #[cfg(unix)]
        if let Some(s) = lock(&self.host).as_ref() {
            s.host.let_go_undo(&id);
        }
        let _ = id;
    }

    /// The domains of the per-folder era still to be removed before the hub is registered (`registered`: every
    /// domain the system has of this app): each through the safe path (the release note, the system's pending changes
    /// handed over, removed keeping what is unsynced, that put next to its folder and told). Empty once the migration
    /// is done (`sync_host_migration_done`): it runs once. A start after an interruption gets the domains still there.
    pub fn sync_host_migration(&self, registered: Vec<SyncHostDomain>) -> Vec<SyncHostDomain> {
        hub::migration_plan(&self.dir, registered)
    }

    /// Every domain of the plan went: the migration is over and never runs again.
    pub fn sync_host_migration_done(&self) {
        hub::migration_done(&self.dir);
    }

    /// Seedwell's entry in the file manager's sidebar as it should be now ([`SyncNavPane`]); `None` while folder sync
    /// is not running (nothing is known: leave the entry as it is) and in an isolated run (tests, QA: the CLSID is the
    /// user's). The host hears of each change through `SyncHostListener::nav_pane`; this is for a look of its own.
    ///
    /// Promise: cheap, safe on the UI thread at start. It reads folder sync's settings and folder list in memory: no
    /// disk, no wait for the engine, a scan, a held folder or a save in progress (`ui_wait_tests` times it while a
    /// save is slow and the engine stalled).
    pub fn sync_nav_pane(&self) -> Option<SyncNavPane> {
        if self.isolated {
            return None;
        }
        nav_pane(self.backend.sync().as_ref())
    }

    /// What the core says of folder `id`, for a domain the host has that the core did not ask for (one left from
    /// before, its folder removed or switched while the app was not running): only `Removed` and `Full` remove it.
    pub fn sync_host_folder_state(&self, id: String) -> SyncHostFolderState {
        folder_state(self.backend.sync().configured(&id))
    }

    /// What the system kept of a domain the host removed (`preserved`) goes next to the folder's place (or to
    /// `~/Seedwell` for a folder no longer here), named `<name> (changes not synced)`; the notice says where.
    pub fn sync_host_keep_preserved(&self, id: String, name: String, preserved: String) -> Option<SyncHostNotice> {
        let place = self.backend.sync().snapshot().folders.into_iter().find(|f| f.id == id).map(|f| PathBuf::from(f.path));
        keep_preserved(&name, place.as_deref(), Path::new(&preserved))
    }

    /// The host is about to remove folder `id`'s domain (`name`): noted, so that if the app stops before it is done
    /// (`sync_host_release_done`), the next start tells the user (`sync_host_unfinished_releases`).
    pub fn sync_host_release_begin(&self, id: String, name: String) {
        let note = release_note(&self.dir, &id);
        let _ = std::fs::create_dir_all(note.parent().unwrap_or(&self.dir));
        let _ = std::fs::write(note, name);
    }

    /// The removal of `id`'s domain is over, and what the system kept is put away.
    pub fn sync_host_release_done(&self, id: String) {
        let _ = std::fs::remove_file(release_note(&self.dir, &id));
    }

    /// Removals of domains the app did not finish (it stopped in the middle): what the system kept of them is where
    /// the system put it, a place only the removing call is told, so Seedwell cannot move it for the user; each notice
    /// says so and where to look. Read once: the notes go.
    pub fn sync_host_unfinished_releases(&self) -> Vec<SyncHostNotice> {
        unfinished_releases(&self.dir)
    }

    /// For a host of on-demand folders that calls the core in its own process (Windows' Cloud Files sync roots; no
    /// extension, no socket, `verify_peer` never asked): tells `listener` which folders there should be (`domains`:
    /// register and connect a sync root for each, unregister the others), when a folder's change feed moved
    /// (`feed_moved`: read `sync_changes` from `sync_host_anchor`), and asks it to let go of a folder before it is
    /// removed or switched to full (`remove_domain`, which blocks until it has). An isolated run (tests, QA)
    /// hosts only the on-demand folders inside its own state folder, so it can test sync roots for real in its temp
    /// state and never registers one on a real folder. Attaching again replaces it; `sync_host_detach`
    /// (at quit) stops it.
    pub fn sync_host_attach(&self, listener: Arc<dyn SyncHostListener>) -> Result<(), CoreError> {
        // An isolated (test) run hosts only folders inside its own state folder: a QA run can test sync roots for real
        // in its temp state, and can never register one on a real folder.
        let confine = self.isolated.then(|| self.dir.clone());
        let mut slot = lock(&self.host_attached);
        *slot = None;
        let core: Arc<dyn HostCore> = Arc::new(BackendCore(self.backend.sync(), confine));
        *slot = Some(
            Attached::start(core, &self.dir, listener)
                .map_err(|e| CoreError::Failed { message: format!("On-demand folders could not start: {e}.") })?,
        );
        Ok(())
    }

    /// Stops what `sync_host_attach` started (at quit).
    pub fn sync_host_detach(&self) {
        *lock(&self.host_attached) = None;
    }

    /// Stops serving the extension (at quit): its socket goes, so the extension knows the app is not there.
    pub fn sync_host_stop(&self) {
        #[cfg(unix)]
        {
            *lock(&self.host) = None;
        }
    }
}

#[path = "sync_hub.rs"]
pub mod hub;

#[cfg(all(test, unix))]
#[path = "sync_host_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "sync_hub_tests.rs"]
mod hub_tests;

#[cfg(test)]
mod nav_pane_tests {
    use super::*;

    fn home() -> PathBuf {
        trs_platform::dirs::home().expect("a home folder to resolve `~` against")
    }

    /// Catches an entry that opens the wrong folder: `~` left unexpanded or a relative path (File Explorer opens
    /// nothing), an empty setting not meaning `~/Seedwell`, or a chosen place not followed.
    #[test]
    fn the_entry_opens_the_seedwell_folder_the_setting_names() {
        let default = home().join("Seedwell");
        for empty in ["", "  "] {
            let e = nav_pane_of(empty, &[]);
            assert_eq!(Path::new(&e.target), default, "an empty setting is the default");
        }
        assert_eq!(Path::new(&nav_pane_of("~/Seedwell", &[]).target), default, "`~` expanded");
        let chosen = tempfile::tempdir().unwrap();
        let e = nav_pane_of(&chosen.path().display().to_string(), &[]);
        assert_eq!(Path::new(&e.target), chosen.path(), "a chosen place is followed");
        assert!(Path::new(&e.target).is_absolute());
        assert!(!e.name.is_empty());
        assert_eq!(e.marker, NAV_PANE_MARKER);
    }

    /// Catches an entry that opens a folder that is not there (nothing synced, or every folder elsewhere and the
    /// default never made), and one missing while folder sync is set up.
    #[test]
    fn there_is_an_entry_while_folder_sync_is_set_up() {
        let default = home().join("Seedwell");
        assert!(!nav_pane_of("", &[]).show, "nothing synced, no place chosen: no entry");
        let inside = default.join("Team");
        assert!(nav_pane_of("", &[inside.as_path()]).show, "a folder in the Seedwell folder");
        let shouted = PathBuf::from(default.display().to_string().to_uppercase()).join("Team");
        assert!(nav_pane_of("", &[shouted.as_path()]).show, "the same folder in another case (the default file systems fold case)");
        let elsewhere = home().join("Documents").join("Work");
        assert!(!nav_pane_of("", &[elsewhere.as_path()]).show, "every folder elsewhere: the Seedwell folder may not exist");
        let sibling = home().join("Seedwell 2");
        assert!(!nav_pane_of("", &[sibling.as_path()]).show, "a neighbour whose name starts the same is not inside");
        let chosen = tempfile::tempdir().unwrap();
        assert!(nav_pane_of(&chosen.path().display().to_string(), &[]).show, "a place chosen in Settings");
        assert!(!nav_pane_of("relative/place", &[Path::new("relative/place/x")]).show, "never an entry at a relative path");
    }

    /// Catches an entry that changes or doubles: the host keeps no state, so a CLSID that moved with the setting, the
    /// folders or a new version would leave the old entry in File Explorer; and one Windows' registry refuses (braces,
    /// 8-4-4-4-12 hex digits).
    #[test]
    fn the_entry_has_one_fixed_and_valid_clsid() {
        let chosen = tempfile::tempdir().unwrap();
        let place = chosen.path().display().to_string();
        let folder = chosen.path().join("Team");
        let all = [nav_pane_of("", &[]), nav_pane_of(&place, &[folder.as_path()]), nav_pane_of("~/Elsewhere", &[])];
        assert!(all.iter().all(|e| e.clsid == NAV_PANE_CLSID), "{all:?}");
        let a = NAV_PANE_CLSID;
        assert!(a.starts_with('{') && a.ends_with('}'), "{a}");
        let parts: Vec<&str> = a[1..a.len() - 1].split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), [8, 4, 4, 4, 12], "{a}");
        assert!(parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit())), "{a}");
        assert!(a.chars().any(|c| c.is_ascii_hexdigit() && c != '0'), "not the nil GUID");
        assert!(!NAV_PANE_MARKER.is_empty());
        // The uninstaller has no Core: what it removes must be the entry the app made.
        assert_eq!((sync_nav_pane_clsid(), sync_nav_pane_marker()), (NAV_PANE_CLSID.to_string(), NAV_PANE_MARKER.to_string()));
    }

    #[derive(Default)]
    struct Heard {
        entries: Mutex<Vec<SyncNavPane>>,
    }

    impl SyncHostListener for Heard {
        fn feed_moved(&self, _folder_id: String) {}
        fn domains(&self, _domains: Vec<SyncHostDomain>) {}
        fn remove_domain(&self, _folder_id: String) -> SyncHostRemoval {
            SyncHostRemoval { error: None }
        }
        fn notice(&self, _title: String, _text: String) {}
        fn nav_pane(&self, entry: SyncNavPane) {
            lock(&self.entries).push(entry);
        }
        fn verify_peer(&self, _audit_token: Vec<u8>) -> bool {
            true
        }
    }

    /// Catches a host that never learns the Seedwell folder moved (the entry would keep opening the old place), one
    /// told again and again of an entry that did not change (it rewrites the registry each second), and an isolated
    /// run (QA) that reports the user's CLSID (its entry would replace the user's own).
    #[test]
    fn the_host_hears_of_the_entry_at_start_and_when_the_seedwell_folder_moves() {
        let (core, dir) = crate::library::tests::mock();
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let put = |p: &Path| {
            let mut s = core.sync_settings();
            s.default_location = p.display().to_string();
            core.set_sync_settings(s).unwrap();
        };
        put(a.path());
        let h = Host::new(Arc::new(BackendCore(core.backend.sync(), None)), dir.path());
        let heard = Heard::default();
        let (mut anchors, mut shown) = (HashMap::new(), None);
        h.watch_once(&heard, &mut anchors, &mut shown);
        h.watch_once(&heard, &mut anchors, &mut shown);
        let targets = || lock(&heard.entries).iter().map(|e| PathBuf::from(&e.target)).collect::<Vec<_>>();
        assert_eq!(targets(), [a.path()], "told once at the start, not again while nothing changed");
        assert_eq!(core.sync_nav_pane().map(|e| PathBuf::from(e.target)), Some(a.path().to_path_buf()), "the call says the same");
        // Moved in Settings (or by the command line: the same setting).
        put(b.path());
        h.watch_once(&heard, &mut anchors, &mut shown);
        assert_eq!(targets(), [a.path(), b.path()], "the move is heard");
        assert!(lock(&heard.entries).iter().all(|e| e.show && e.clsid == NAV_PANE_CLSID));

        // Catches an entry that opens nothing: a place chosen but not made yet (or on a disk that is away) is told as
        // no entry, and the entry comes back once the folder is there. The host never makes the folder.
        let missing = b.path().join("not yet");
        put(&missing);
        h.watch_once(&heard, &mut anchors, &mut shown);
        assert!(!lock(&heard.entries).last().unwrap().show, "a Seedwell folder that is not there: no entry");
        assert!(!missing.exists(), "nothing made it");
        std::fs::create_dir(&missing).unwrap();
        h.watch_once(&heard, &mut anchors, &mut shown);
        assert!(lock(&heard.entries).last().unwrap().show, "there now: the entry comes back");

        // An isolated run: neither told nor answered.
        let confined = Host::new(Arc::new(BackendCore(core.backend.sync(), Some(dir.path().join("state")))), dir.path());
        let quiet = Heard::default();
        confined.watch_once(&quiet, &mut HashMap::new(), &mut None);
        assert!(lock(&quiet.entries).is_empty(), "an isolated run reports no entry");
        let d = tempfile::tempdir().unwrap();
        let isolated = Core::new(crate::CoreOptions {
            config_dir: d.path().display().to_string(),
            mock: true,
            allow_loopback_trackers: false,
            forbidden_root: Some(d.path().join("home").display().to_string()),
        })
        .unwrap();
        assert_eq!(isolated.sync_nav_pane(), None, "an isolated run answers none");
    }
}

#[cfg(test)]
mod attached_tests {
    use super::*;

    #[derive(Default)]
    struct Heard {
        domains: Mutex<Vec<Vec<SyncHostDomain>>>,
        moved: Mutex<Vec<String>>,
    }

    impl SyncHostListener for Heard {
        fn feed_moved(&self, folder_id: String) {
            lock(&self.moved).push(folder_id);
        }
        fn domains(&self, domains: Vec<SyncHostDomain>) {
            lock(&self.domains).push(domains);
        }
        fn remove_domain(&self, _folder_id: String) -> SyncHostRemoval {
            SyncHostRemoval { error: None }
        }
        fn notice(&self, _title: String, _text: String) {}
        fn nav_pane(&self, _entry: SyncNavPane) {}
        fn verify_peer(&self, _audit_token: Vec<u8>) -> bool {
            panic!("an in-process host has no peer to verify")
        }
    }

    /// On every system: the in-process host hears the folders it should show and each move of their feeds.
    #[test]
    fn an_attached_host_hears_its_folders_and_their_feeds() {
        let (core, dir) = crate::library::tests::mock();
        let id = core.sync_snapshot().folders[0].id.clone();
        core.sync_set_on_demand(id.clone(), true, None).unwrap();
        let heard = Arc::new(Heard::default());
        let a = Attached::start_every(Arc::new(BackendCore(core.backend.sync(), None)), dir.path(), heard.clone(), Duration::from_millis(20)).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while lock(&heard.moved).is_empty() {
            assert!(Instant::now() < end, "nothing heard in 10 s");
            std::thread::sleep(Duration::from_millis(20));
        }
        let domains = lock(&heard.domains).last().cloned().unwrap();
        assert_eq!(domains.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), [id.as_str()]);
        assert_eq!(lock(&heard.moved).first(), Some(&id));
        drop(a);
        // Confined to a root (an isolated run's state folder): only the on-demand folders inside it are shown.
        let path = core.sync_snapshot().folders.into_iter().find(|f| f.id == id).unwrap().path;
        // (0.1.18: the mock's folders were at `/Users/me/…` where `HOME` is unset, which has no drive on Windows.)
        assert!(Path::new(&path).is_absolute(), "the mock's folder is at an absolute path on every system: {path}");
        let elsewhere = BackendCore(core.backend.sync(), Some(dir.path().join("state")));
        assert_eq!(elsewhere.folders().unwrap().len(), 0, "a folder outside the run's own state is never hosted");
        let within = BackendCore(core.backend.sync(), Path::new(&path).parent().map(Path::to_path_buf));
        assert_eq!(within.folders().unwrap().into_iter().map(|f| f.id).collect::<Vec<_>>(), std::slice::from_ref(&id));
        // A place that is gone (an on-demand folder after its switch) still counts by where it was; `..` cannot leave.
        let root = tempfile::tempdir().unwrap();
        assert!(inside(root.path(), &root.path().join("od").join("Docs")), "a missing folder inside the root");
        assert!(!inside(root.path(), &root.path().join("od").join("..").join("..").join("elsewhere")), "`..` out of it");
        assert!(!inside(&root.path().join("state"), &root.path().join("other").join("Docs")), "a missing folder outside");
        assert!(!inside(root.path(), Path::new("relative/Docs")), "only an absolute path");
        // Windows' verbatim prefixes compare like plain paths.
        assert_eq!(plain(Path::new(r"\\?\C:\Users\me\state")), Path::new(r"C:\Users\me\state"));
        assert_eq!(plain(Path::new(r"\\?\UNC\nas\share\x")), Path::new(r"\\nas\share\x"));
        assert_eq!(plain(Path::new("/tmp/x")), Path::new("/tmp/x"));
        // An isolated run attaches (QA tests sync roots in its own temp state), confined to its state folder.
        let d = tempfile::tempdir().unwrap();
        let isolated = Core::new(crate::CoreOptions {
            config_dir: d.path().display().to_string(),
            mock: true,
            allow_loopback_trackers: false,
            forbidden_root: Some(d.path().join("home").display().to_string()),
        })
        .unwrap();
        isolated.sync_host_attach(heard).unwrap();
        isolated.sync_host_detach();
    }
}
