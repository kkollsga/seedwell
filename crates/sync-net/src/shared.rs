//! State shared by the service, the folder runtimes and the peer sessions.
//!
//! Folders and channels are both [`Space`]s: a key pair, a membership, invitations and the devices met in it. A
//! folder adds its files ([`FolderShared`]), a channel its messages ([`crate::chat::ChannelShared`]); both
//! dereference to their space, so membership, admission and discovery are written once. The user's own devices are a
//! third kind, a space with nothing more ([`crate::devices`]): its key is the user key.
//!
//! Locks are short: a `FolderCore` is only ever locked inside `spawn_blocking` (its methods do disk I/O),
//! and the registries are locked to clone an `Arc` out, never across an `.await`.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{Notify, mpsc, oneshot, watch};
use seedwell_sync::folder::FolderCore;
use seedwell_sync::store_redb::RedbReader;
use seedwell_sync_proto::ids::{DeviceId, FolderId};
use seedwell_sync_proto::keys::{Access, FolderKey, FolderSecret};
use seedwell_sync_proto::members::{InviteLink, Membership, Role};
use seedwell_sync_proto::msg::{Message, Request, Response};

/// Locks a std mutex, also after a panic elsewhere (the data is still consistent: every critical
/// section here is a plain read or replace).
#[track_caller]
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // Debug builds say where a lock was waited for long: nothing the app's main thread calls may wait behind long work.
    if cfg!(debug_assertions) {
        match m.try_lock() {
            Ok(g) => return g,
            Err(std::sync::TryLockError::Poisoned(e)) => return e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        let at = std::time::Instant::now();
        let g = m.lock().unwrap_or_else(|e| e.into_inner());
        let waited = at.elapsed();
        let caller = std::panic::Location::caller();
        if waited >= LONG_LOCK_WAIT {
            eprintln!("seedwell sync: waited {} ms for a lock at {caller}", waited.as_millis());
        }
        LONGEST_WAIT.with(|w| {
            if w.get().is_none_or(|(d, _)| waited > d) {
                w.set(Some((waited, caller)));
            }
        });
        return g;
    }
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Debug builds: a wait for a lock this long is reported ([`lock`]).
const LONG_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

type LockWait = Option<(std::time::Duration, &'static std::panic::Location<'static>)>;

thread_local! {
    /// Debug builds: this thread's longest wait for a lock ([`lock`]) since [`take_longest_lock_wait`] last asked.
    static LONGEST_WAIT: std::cell::Cell<LockWait> = const { std::cell::Cell::new(None) };
}

/// **Tests (debug builds):** the longest this thread waited for one of folder sync's locks since the last call, and
/// where: a call of the app that took long says whether it waited for a lock or was not run (a busy machine).
#[doc(hidden)]
pub fn take_longest_lock_wait() -> LockWait {
    LONGEST_WAIT.with(|w| w.take())
}

thread_local! {
    /// How many folder and channel statuses this thread made ([`statuses_made_here`]).
    static STATUSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A folder's or channel's status is being made on this thread (its members, absences, index or messages read).
pub(crate) fn note_status() {
    STATUSES.with(|n| n.set(n.get() + 1));
}

/// **Tests:** how many folder and channel statuses (the parts of a snapshot, a channel list) this thread has made: a
/// call the hosts make each time they draw a dialog must make none.
#[doc(hidden)]
pub fn statuses_made_here() -> u64 {
    STATUSES.with(std::cell::Cell::get)
}

/// **Tests only:** milliseconds every save of folder sync's state to disk takes on top of the disk's own time
/// ([`slow_disk`]); process-wide.
static SAVE_WAIT_MS: AtomicU64 = AtomicU64::new(0);

/// **Tests only:** every save of folder sync's state (its configuration, a membership) takes `wait` longer, as on a
/// disk that a virus scanner reads before each write is done (Windows' Defender does). For the whole process.
#[doc(hidden)]
pub fn test_slow_saves(wait: std::time::Duration) {
    SAVE_WAIT_MS.store(wait.as_millis() as u64, Ordering::SeqCst);
}

/// **QA only:** `SEEDWELL_QA_SLOW_SAVES=<ms>` makes every save of folder sync's and chat's state (the configuration, a
/// membership, a channel's or conversation's messages, a room's files and images) take that many milliseconds longer,
/// as on a disk whose virus scanner reads each new file first ([`test_slow_saves`]), so a QA check sees a call that
/// waits for a save as a long wait. Honoured only in an isolated run: a state folder inside a temporary folder (as
/// [`crate::limit::QA_LIMIT_LAN`] is), never in the user's own Seedwell. The log says which.
pub const QA_SLOW_SAVES: &str = "SEEDWELL_QA_SLOW_SAVES";

/// How much longer each save takes in this run: [`QA_SLOW_SAVES`]'s milliseconds (`value`), when the service's state
/// folder (`state`) is inside a temporary folder. `Err`: set but not honoured, and why (for the log).
pub fn qa_slow_saves(value: Option<&std::ffi::OsStr>, state: &std::path::Path) -> Result<Option<std::time::Duration>, &'static str> {
    let Some(v) = value else { return Ok(None) };
    let Some(ms) = v.to_str().and_then(|t| t.trim().parse::<u64>().ok()) else { return Err("not a number of milliseconds") };
    if !seedwell_platform::dirs::in_temp_folder(state) {
        return Err("only an isolated run (a state folder in a temporary folder) slows its saves");
    }
    Ok(Some(std::time::Duration::from_millis(ms.min(60_000))))
}

/// A save to disk is under way (the test hook [`test_slow_saves`] makes it slow). What the app's calls read is never
/// locked across one: they would wait for the disk.
pub(crate) fn slow_disk() {
    let ms = SAVE_WAIT_MS.load(Ordering::Relaxed);
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// A folder or a channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpaceKind {
    /// A synced folder.
    Folder,
    /// A channel.
    Channel,
    /// The user's own devices (0.1.13): a group whose key is the user key, never shown to agents.
    Devices,
}

/// A device's names: its own (Settings › Folder Sync) and its user's in chats (empty: none given).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Names {
    /// The device's name.
    pub device: String,
    /// Its user's name.
    pub user: String,
}

/// What a folder and a channel have in common: who is in it, and how it is found.
pub struct Space {
    /// Its id (the SHA-256 of its public key).
    pub id: FolderId,
    /// A folder or a channel.
    pub kind: SpaceKind,
    /// Its public key (the id, discovery topics, LAN tags).
    pub public: FolderKey,
    /// Its secret, on an owner device (a new co-owner receives it from an owner).
    pub secret: RwLock<Option<FolderSecret>>,
    /// Who is in it and what each device may do. Read without the core: the UI asks often.
    pub membership: RwLock<Membership>,
    /// Where the membership is kept.
    pub members_path: PathBuf,
    /// The invitation this device joins with, until an owner or admin admits it.
    pub invite: Mutex<Option<InviteLink>>,
    /// The links of invitations this device made, by invite id (shown again in the Share dialog).
    pub links: Mutex<BTreeMap<[u8; 16], String>>,
    /// Taken by a save of its membership or its links for the whole write, never by a read: the app's calls read
    /// them, and never wait for a disk ([`crate::roles::add`]).
    pub saving: Mutex<()>,
    /// Devices we have seen in it (from their announcements and sessions): dialed when not connected.
    pub members: RwLock<BTreeSet<DeviceId>>,
    /// Its name on this device.
    pub name: Mutex<String>,
    /// Syncing is paused by the user (nothing is scanned, pulled or served). Never for a channel.
    pub paused: AtomicBool,
    /// Only sync with devices on the local network. Never for a channel.
    pub lan_only: AtomicBool,
    /// What went wrong with its membership and its peers, for the status.
    pub notes: Mutex<Notes>,
    /// A folder or channel that one of the user's own devices put on all of them, which this device joins by itself
    /// (0.1.13): no invitation, it proves it is one of that device's own instead ([`crate::devices`]). Set while it
    /// is not admitted yet.
    pub following: AtomicBool,
    /// The folder is *Read only* (its owners' setting) and this device, not an owner, may otherwise write: it holds its
    /// changes back (sends none) until the setting is off ([`crate::roles::changed`] gives the folder's core this). The
    /// value the core was last given.
    pub held_read_only: AtomicBool,
    /// The removals this device asked the owners for, as an admin ([`crate::removals`]), kept next to the membership.
    pub removals: Mutex<Vec<crate::removals::Pending>>,
}

/// What a space's membership and sessions tell the status.
#[derive(Clone, Debug, Default)]
pub struct Notes {
    /// Joining: an owner or admin refused this device's invitation, for this reason.
    pub refused: Option<String>,
    /// Devices this one cannot sync with (their Seedwell is too old or too new), and why. Shown with the issues;
    /// a device's entry goes when a session with it starts.
    pub incompatible: BTreeMap<DeviceId, String>,
    /// The membership could not be saved or applied (shown with the issues).
    pub problems: Vec<String>,
    /// Joining: what was tried to find an owner or admin, for the progress sentence (`crate::joining`).
    pub join: JoinTrail,
    /// An owner's or admin's record of the attempts to join with a link that it refused, newest last (at most
    /// [`MAX_ATTEMPTS`]; in memory, gone with the folder or channel).
    pub attempts: std::collections::VecDeque<JoinAttempt>,
    /// Why the last connection with each device of this folder or channel failed (Unix seconds, the sentence); a
    /// device's entry goes when a session with it starts.
    pub unreachable: BTreeMap<DeviceId, (u64, String)>,
}

/// Refused join attempts a folder or channel keeps.
pub const MAX_ATTEMPTS: usize = 20;

/// What a joining folder or channel has done to find one of its owners or admins.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinTrail {
    /// The address being dialed now.
    pub trying: Option<SocketAddr>,
    /// A device that answered but did not admit this one (its name).
    pub reached: Option<String>,
    /// What went wrong last, as a sentence.
    pub problem: Option<String>,
}

/// An attempt to join with a link that this device (an owner or admin) refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinAttempt {
    /// When, Unix seconds.
    pub at_s: u64,
    /// The device that tried.
    pub device: DeviceId,
    /// Its name, from its Hello.
    pub name: String,
    /// Its client (`Seedwell 0.1.9`).
    pub client: String,
    /// Why it was refused.
    pub reason: String,
}

impl Space {
    /// A space with nothing joined or paused yet.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: FolderId,
        kind: SpaceKind,
        public: FolderKey,
        secret: Option<FolderSecret>,
        membership: Membership,
        members_path: PathBuf,
        invite: Option<InviteLink>,
        links: BTreeMap<[u8; 16], String>,
        name: String,
    ) -> Self {
        let s = Space {
            id,
            kind,
            public,
            secret: RwLock::new(secret),
            membership: RwLock::new(membership),
            members_path,
            invite: Mutex::new(invite),
            links: Mutex::new(links),
            saving: Mutex::new(()),
            members: RwLock::new(BTreeSet::new()),
            name: Mutex::new(name),
            paused: AtomicBool::new(false),
            lan_only: AtomicBool::new(false),
            notes: Mutex::new(Notes::default()),
            following: AtomicBool::new(false),
            held_read_only: AtomicBool::new(false),
            removals: Mutex::new(Vec::new()),
        };
        *lock(&s.removals) = crate::removals::load(&s);
        s
    }

    /// The names each device gave itself in this folder's or channel's membership (its profile): what every member
    /// shows for it.
    pub fn profile_names(&self) -> Vec<(DeviceId, Names)> {
        let m = self.membership.read().unwrap_or_else(|e| e.into_inner());
        m.profiles().map(|(pk, device, user)| (DeviceId::of_public_key(pk), Names { device: device.into(), user: user.into() })).collect()
    }

    /// A device's role (`device_pk`: its public key), if it is a member.
    pub fn role_of(&self, device_pk: &[u8; 32]) -> Option<Role> {
        self.membership.read().unwrap_or_else(|e| e.into_inner()).role(device_pk)
    }

    /// May the device (`device_pk`) write to the folder?
    pub fn writes(&self, device_pk: &[u8; 32]) -> bool {
        self.role_of(device_pk).is_some_and(|r| r.access() == seedwell_sync_proto::keys::Access::Write)
    }

    /// May it be synced with a peer over this connection (`lan`: the peer is on the LAN, not relayed)? Not when it
    /// is paused, nor a LAN-only folder with a peer elsewhere.
    pub fn allowed_with(&self, lan: bool) -> bool {
        // (SeqCst, as the closers store them: a link checks it after it is in the sessions a closer then looks at.)
        !self.paused.load(Ordering::SeqCst) && (lan || !self.lan_only.load(Ordering::SeqCst))
    }

    /// Notes a problem with the membership (once).
    pub fn problem(&self, why: String) {
        let mut n = lock(&self.notes);
        if !n.problems.contains(&why) {
            n.problems.push(why);
        }
    }

    /// Notes a refused attempt to join with a link (the oldest goes past [`MAX_ATTEMPTS`]).
    pub fn note_attempt(&self, a: JoinAttempt) {
        let mut n = lock(&self.notes);
        if n.attempts.len() >= MAX_ATTEMPTS {
            n.attempts.pop_front();
        }
        n.attempts.push_back(a);
    }

    /// Joining: notes what the search for an owner or admin ran into.
    pub fn join_problem(&self, why: String) {
        lock(&self.notes).join.problem = Some(why);
    }

    /// Joining here (an invitation, or following one of the user's own devices, not admitted yet).
    pub fn joining(&self, me: &[u8; 32]) -> bool {
        (lock(&self.invite).is_some() || self.following.load(Ordering::SeqCst)) && self.role_of(me).is_none()
    }

    /// The issues its notes add to the status: problems, then devices it cannot sync with.
    pub fn note_issues(&self) -> Vec<(String, String)> {
        let n = lock(&self.notes);
        n.problems.iter().chain(n.incompatible.values()).map(|w| (String::new(), w.clone())).collect()
    }
}

/// One folder, as the network side sees it: its space (dereferenced to), and its files.
pub struct FolderShared {
    /// Who is in it.
    pub space: Arc<Space>,
    /// Where its files are on disk: the folder itself, or for an on-demand folder this device's content cache.
    pub root: PathBuf,
    /// Where the user sees it: the folder itself, or for an on-demand folder where the host shows it (renamed in place).
    pub place: Mutex<PathBuf>,
    /// On demand (SPEC §7G.11).
    pub on_demand: bool,
    /// A full folder on demand for the devices without its files (`on_demand_others`): its scans list and do not read
    /// (SPEC §7G.11, *Listed first, hashed when first fetched*). Set when it is marked; each scan step hands it to the
    /// core.
    pub list_first: AtomicBool,
    /// On demand: the paths kept on this device (a copy of the core's, for listing items without it).
    pub pins: Mutex<std::collections::BTreeSet<String>>,
    /// On demand: the items a host lists, read from `index` (never waiting for the core).
    pub items: Mutex<seedwell_sync::ondemand::Items>,
    /// On demand: files with a free in progress here, and how many frees hold each ([`Freeing`]): asked whether it
    /// holds them, this device says no (SPEC §7G.11).
    pub freeing: Mutex<BTreeMap<String, u32>>,
    /// The ignore patterns as they are now (the core keeps them up to date), for answers given without the core.
    pub ignore: seedwell_sync::ignore::Current,
    /// On demand: the deletes the host made a moment ago (the core notes them): the index sessions and the host's change
    /// feed wait for them before announcing, so a save by replace reaches them as one edit (SPEC §7G.11).
    pub replacing: seedwell_sync::ondemand::Replacing,
    /// On demand and kept in place: the files whose content is in the host's placeholders ([`crate::placed`]).
    pub placed: Option<Arc<crate::placed::Placed>>,
    /// The core. Lock only on a blocking thread.
    pub core: Mutex<FolderCore>,
    /// Reads the core's index without its lock: requests from peers are answered while a pull holds the
    /// core (waiting on peers itself), so two devices pulling from each other never wait on each other.
    pub index: RedbReader,
    /// Our latest local sequence: sessions wake on changes and send the new entries.
    pub local_seq: watch::Sender<u64>,
    /// A peer's index changed what we need: the runtime pulls.
    pub remote_changed: Notify,
    /// What the runtime is doing, for the snapshot.
    pub live: Mutex<Live>,
    /// Who listed the folder, as its index says, read off the status's thread ([`ListedLook`]).
    pub listed: ListedLook,
    /// Ask the runtime for a full scan now (the user's "Rescan", or a changed ignore file).
    pub rescan: Notify,
    /// Up when the folder is being stopped (removed, moved, switched): what holds it for a peer (its index sender)
    /// lets go, so the folder's index closes while that peer stays connected.
    pub stopped: watch::Sender<bool>,
    /// How long archived versions are kept, and how much room they may take.
    pub retention: seedwell_sync::archive::Retention,
    /// Bytes fetched from peers for this folder.
    pub fetched: crate::limit::Meter,
    /// Bytes served to peers from this folder.
    pub served: crate::limit::Meter,
    /// The time left while it syncs, fed by each status (a leaf lock: nothing is locked while it is held).
    pub eta: Mutex<crate::eta::Eta>,
    /// On demand: the fetches being waited for (SPEC §7G.11).
    pub fetches: Mutex<Fetches>,
    /// Tests only: each batch an index session announces starts with this pause.
    pub announce_pause: Mutex<std::time::Duration>,
    /// Tests only: each host's write (`od_write`) pauses this long with the content in place and the core held, before
    /// reading it in (as a big file's hashing takes).
    pub write_wait: Mutex<std::time::Duration>,
    /// The reads of this device's unhashed files for the devices fetching them, by path and device (SPEC §7G.11: each
    /// read once, hashed as it goes).
    pub streams: Mutex<HashMap<(String, seedwell_sync_proto::ids::ShortId), StreamSlot>>,
    /// The streams this device runs of others' unhashed files, one at a time, with the core let go
    /// (`runtime::enqueue_jobs`): one lane.
    pub streamer: Mutex<Streamer>,
    /// The other lane: peers' chunks of hashed files (so a stream minutes long holds up no hashed file's fetch).
    pub fetcher: Mutex<Streamer>,
    /// The core's state of those jobs ([`seedwell_sync::folder::FolderCore::streams`]): a job's end is handed back here,
    /// never waiting for the core.
    pub jobs: Arc<Mutex<seedwell_sync::lazy::Streams>>,
    /// Set once everything above is dropped, the index included, and every job that held the index without the folder
    /// ([`Closed::hold`]) ended. Last, because fields drop in order: the last `Arc` of a folder counts as gone before
    /// its fields are, so only this says the index file is closed.
    pub closed: Closed,
}

/// The jobs queued and running in one lane of one folder ([`FolderShared::streamer`], [`FolderShared::fetcher`]).
#[derive(Default)]
pub struct Streamer {
    /// Waiting their turn.
    pub queue: std::collections::VecDeque<seedwell_sync::lazy::Job>,
    /// The path whose job runs now.
    pub current: Option<String>,
    /// A task runs the lane.
    pub running: bool,
}

impl Streamer {
    /// Queues `job`. A job of the same path already queued is replaced (returned: the caller tells the core it will
    /// not run, [`seedwell_sync::lazy::Streams::cancelled`]); one of the path running now is no reason to drop it (the
    /// core took it after that one ended: it runs after it).
    pub fn push(&mut self, job: seedwell_sync::lazy::Job) -> Option<seedwell_sync::lazy::Job> {
        match self.queue.iter_mut().find(|q| q.path() == job.path()) {
            Some(q) => Some(std::mem::replace(q, job)),
            None => {
                self.queue.push_back(job);
                None
            }
        }
    }
}

/// One device's read of one of this device's unhashed files ([`FolderShared::streams`]).
pub struct StreamSlot {
    /// The read (one piece at a time).
    pub stream: Arc<Mutex<seedwell_sync::lazy::Stream>>,
    /// When a piece was last asked for: a read left alone for [`STREAM_IDLE`] is dropped.
    pub used: std::time::Instant,
}

/// How long a read of an unhashed file nobody asks a piece of is kept.
pub const STREAM_IDLE: std::time::Duration = seedwell_sync::lazy::STREAM_IDLE;

/// The fetches of an on-demand folder being waited for ([`FolderShared::fetches`]).
#[derive(Default)]
pub struct Fetches {
    /// How far each got and since when, by path.
    pub watch: std::collections::HashMap<String, FetchWatch>,
    /// The latest ask of each path (its number): a fetch that gives up takes its request back only if no fetch of the
    /// path asked since (a host asking again at once keeps its request).
    pub asks: std::collections::HashMap<String, u64>,
    /// Asks so far.
    pub asked: u64,
    /// Requests on their way to the folder's core (which a pull round holds while it downloads), by path: the ask that
    /// made each. A fetch waits for one without making it again; a cancel removes it, and it is not made then.
    pub requesting: std::collections::HashMap<String, u64>,
    /// Why such a request failed after the fetch that made it had answered: the next wait on it says so.
    pub request_failed: std::collections::HashMap<String, crate::SyncError>,
}

/// How far a fetch being waited for got, and since when ([`Fetches::watch`]).
#[derive(Clone, Copy, Debug)]
pub struct FetchWatch {
    /// What counts as getting further: the file data that came from the devices asked, and the files here.
    pub mark: (u64, u64),
    /// When `mark` last moved (or the wait began).
    pub since: std::time::Instant,
    /// When a caller last looked: a fetch nobody waited on for a while starts its count again.
    pub seen: std::time::Instant,
}

/// A flag that goes up once it is dropped and every [`Hold`] of it is too (see [`FolderShared::closed`]).
#[derive(Debug, Default)]
pub struct Closed(Arc<CloseFlag>);

/// The flag itself: up when the last [`Closed`] or [`Hold`] of it goes.
#[derive(Debug, Default)]
pub struct CloseFlag(Arc<AtomicBool>);

/// Keeps a folder's [`Closed`] down while work that outlives the folder's own handle runs: a lane's job holds the
/// folder's index (and writes its files) with only a weak handle to the folder.
#[derive(Debug)]
pub struct Hold(#[allow(dead_code)] Arc<CloseFlag>);

impl Closed {
    /// Something to watch: true once the owner's earlier fields are dropped, and every hold.
    pub fn watch(&self) -> Arc<AtomicBool> {
        self.0.0.clone()
    }

    /// Keeps the flag down until the hold is dropped.
    pub fn hold(&self) -> Hold {
        Hold(self.0.clone())
    }
}

impl Drop for CloseFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl FolderShared {
    /// The folder's files as the core reads them: its root, or for a folder kept in place the cache with each held file
    /// read from its placeholder.
    pub fn fs(&self) -> Box<dyn seedwell_sync::fs::Fs> {
        match &self.placed {
            Some(p) => Box::new(crate::placed::PlaceFs::new(p.clone())),
            None => Box::new(seedwell_sync::fs::StdFs::new(self.root.clone())),
        }
    }

    /// Our index id and the last sequence given out, from the index's read-only view (never waits for the core).
    pub fn index_state(&self) -> std::io::Result<(u64, u64)> {
        let m = seedwell_sync::index::IndexStore::meta(&self.index.view())?;
        Ok((m.index_id, m.sequence))
    }

    /// Our entries above `after` for a peer ([`seedwell_sync::folder::announce_since`]), from the read-only view: a peer is
    /// sent what each scan step committed while the scan still runs.
    pub fn announce_since(&self, after: u64, limit: usize) -> std::io::Result<Vec<seedwell_sync_proto::model::IndexEntry>> {
        seedwell_sync::folder::announce_since(&self.index.view(), self.on_demand, &self.ignore, after, limit)
    }
}

impl std::ops::Deref for FolderShared {
    type Target = Space;

    fn deref(&self) -> &Space {
        &self.space
    }
}

/// A folder runtime's state, as the snapshot shows it.
#[derive(Clone, Debug)]
pub struct Live {
    /// What it is doing.
    pub state: crate::status::FolderState,
    /// Paths that cannot be synced, with the reason (from the last scan and pull).
    pub issues: Vec<(String, String)>,
    /// The entries of `issues` a pull put there, by path: taken out again once the path no longer
    /// needs pulling (it arrived, or the other device changed or deleted it).
    pub pull_issues: BTreeMap<String, String>,
    /// The error that keeps the folder from syncing, if any.
    pub error: Option<String>,
    /// When it last had nothing left to do, Unix seconds.
    pub last_synced_s: Option<u64>,
    /// Conflict copies not yet resolved: copy → the path it is a copy of. Those this device made are
    /// added as it makes them; the index is read for all of them at start, on every full scan and
    /// when the list is asked for ([`crate::runtime::refresh_conflicts`]).
    pub conflicts: BTreeMap<String, String>,
    /// Paths and bytes still needed (as of the last scan or pull).
    pub need: (u64, u64),
    /// The folder's fetched-bytes counter ([`FolderShared::fetched`]) when `need` was worked out: what came since is
    /// off the bytes still needed for the time left (`need` itself is worked out again only when a pull settles).
    pub need_mark: u64,
    /// Files no device can give right now (not in `need`).
    pub unavailable: u64,
    /// The first needed paths in path order, at most [`PENDING_SAMPLE`], for Finder's badges; `true` when
    /// there are more. Taken where the core is locked anyway (before and during pulls, on settling).
    pub pending: (Vec<String>, bool),
    /// Files this device holds, and their size.
    pub local: (u64, u64),
    /// Bytes fetched and served, for rates.
    pub fetched: u64,
    /// The folder has its own `.seedwellignore` (looked at off the caller's thread, at start and on settling: the
    /// folder may be on a slow or missing disk).
    pub ignore_file: bool,
    /// On demand, kept in place: the folders another device deleted that the host kept (SPEC §7G.11, *Kept folders*),
    /// each with the file that keeps it; each has its issue in `issues`.
    pub kept: BTreeMap<String, String>,
    /// When the kept folders were last looked at on disk (at most every [`KEPT_CHECK_EVERY`] from a snapshot).
    pub kept_checked: Option<std::time::Instant>,
    /// While it scans: the phase and how far it is (SPEC §7G.2: the app shows it, never a bare spinner).
    pub scan: Option<seedwell_sync::scan::ScanProgress>,
}

/// What a folder's index says of who listed it: this device files of its own, and which devices sent their first list
/// of it. A status's "Waiting for Mac mini to list Movies" comes from it ([`crate::SyncHandle`]'s `unlisted`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listed {
    /// This device lists files of its own (it waits for nobody's list).
    pub own: bool,
    /// The devices whose first list of the folder is in the index.
    pub peers: std::collections::BTreeSet<seedwell_sync_proto::ids::ShortId>,
}

impl Listed {
    /// Reads it from the index (a read of the index file: never on the caller's thread of a status).
    pub fn read(view: &dyn seedwell_sync::index::IndexStore) -> std::io::Result<Listed> {
        let own = !view.local_paths("", None, 1)?.is_empty();
        let mut peers = std::collections::BTreeSet::new();
        if !own {
            for id in view.peer_ids()? {
                if view.peer(id)?.is_some_and(|s| s.index_id != 0) {
                    peers.insert(id);
                }
            }
        }
        Ok(Listed { own, peers })
    }
}

/// [`Listed`] as last read, read again on a blocking thread whenever a status asks: the status (the app's UI thread, the
/// CLI's `sync list` and `sync status`) never reads the index file itself. Such a read can wait for the disk: on Windows
/// a read of a file waits for any flush of it in progress (the index's commits flush it), which held `sync list` about
/// half a second while a big folder was added or joined.
#[derive(Default)]
pub struct ListedLook {
    state: Mutex<ListedState>,
    done: std::sync::Condvar,
}

#[derive(Default)]
struct ListedState {
    last: Option<Listed>,
    reading: bool,
    /// Reads finished so far (a caller waits for the one in flight when it asked).
    reads: u64,
}

/// How long a status waits for the read it started before it shows the last one: a fast disk answers well within it,
/// a slow one never holds the status up longer.
pub const LISTED_WAIT: std::time::Duration = std::time::Duration::from_millis(20);

impl ListedLook {
    /// Starts with what the index says now (read where the folder opens, which reads its index anyway).
    pub fn new(first: Option<Listed>) -> Self {
        ListedLook { state: Mutex::new(ListedState { last: first, ..Default::default() }), done: std::sync::Condvar::new() }
    }

    /// What the index says: a read is started on `rt`'s blocking threads (unless one is running), and its answer is
    /// waited for until `until` at most; after that the last answer (`None`: none yet).
    pub fn get(f: &Arc<FolderShared>, rt: &tokio::runtime::Handle, until: std::time::Instant) -> Option<Listed> {
        let look = &f.listed;
        let mut st = lock(&look.state);
        if !st.reading {
            st.reading = true;
            let f2 = f.clone();
            drop(rt.spawn_blocking(move || {
                let read = Listed::read(&f2.index.view());
                let look = &f2.listed;
                let mut st = lock(&look.state);
                st.reading = false;
                st.reads += 1;
                if let Ok(r) = read {
                    st.last = Some(r);
                }
                look.done.notify_all();
            }));
        }
        let want = st.reads + 1;
        while st.reads < want {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            // In short slices: a thread's long timed wait may wake late (macOS coalesces timers below the
            // user-interactive priority).
            st = look.done.wait_timeout(st, left.min(std::time::Duration::from_millis(4))).unwrap_or_else(|e| e.into_inner()).0;
        }
        st.last.clone()
    }
}

/// How often a snapshot has the kept folders looked at on disk (off the caller's thread) while there are any.
pub const KEPT_CHECK_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

impl Live {
    /// Shows the issue of the folder the host kept at `dir`, because of `by` (SPEC §7G.11, *Kept folders*): once, and
    /// beside any other issue there.
    pub fn set_kept(&mut self, dir: &str, by: &str) {
        self.drop_kept(dir);
        self.kept.insert(dir.to_string(), by.to_string());
        self.issues.push((dir.to_string(), seedwell_sync::pull::kept_words(by)));
    }

    /// The kept folder at `dir` is over (in the folder again, gone, or nothing keeps it now): its issue goes, and only
    /// that one.
    pub fn drop_kept(&mut self, dir: &str) {
        if let Some(by) = self.kept.remove(dir) {
            let words = seedwell_sync::pull::kept_words(&by);
            self.issues.retain(|(p, w)| !(p == dir && *w == words));
        }
    }
}

impl Default for Live {
    fn default() -> Self {
        Live {
            state: crate::status::FolderState::Starting,
            issues: Vec::new(),
            pull_issues: BTreeMap::new(),
            error: None,
            last_synced_s: None,
            conflicts: BTreeMap::new(),
            need: (0, 0),
            need_mark: 0,
            unavailable: 0,
            pending: (Vec::new(), false),
            local: (0, 0),
            fetched: 0,
            ignore_file: false,
            kept: BTreeMap::new(),
            kept_checked: None,
            scan: None,
        }
    }
}

/// How many needed paths [`Live::pending`] keeps.
pub const PENDING_SAMPLE: usize = 1000;

/// A connected device.
pub struct PeerHandle {
    /// Its id.
    pub device: DeviceId,
    /// Its public key (what membership statements name).
    pub device_pk: [u8; 32],
    /// Its name, from its Hello.
    pub name: String,
    /// Its client string (`Seedwell 0.1.5`).
    pub client: String,
    /// The protocol version and features the session runs at (from both Hellos).
    pub agreed: seedwell_sync_proto::msg::Agreed,
    /// Where the connection goes.
    pub remote: SocketAddr,
    /// The connection stays on the local network (no Internet limits apply). Never for a relayed one.
    pub lan: bool,
    /// What the session runs over.
    pub via: crate::session::Via,
    /// We opened the connection.
    pub dialed: bool,
    /// Our end of the connection (the address the peer reached us on, for our address record).
    pub local: SocketAddr,
    /// The session's TLS exporter value (folder proofs made later in the session are bound to it).
    pub exporter: [u8; 32],
    /// Small and urgent messages (index, requests, control).
    pub control: mpsc::Sender<Message>,
    /// File data (responses to its requests).
    pub data: mpsc::Sender<Message>,
    next_id: AtomicU32,
    pending: Mutex<HashMap<u32, oneshot::Sender<Response>>>,
    /// The session stopped reading for now (the frames it has to apply wait for a folder's core): our requests are
    /// not made meanwhile ([`Self::hold_reading`]).
    reading_held: AtomicBool,
    /// The folders and channels shared over this connection, with the access the peer proved.
    pub folders: RwLock<BTreeMap<FolderId, Access>>,
    /// Bytes of our requests not answered yet (to spread requests over peers).
    pub inflight: AtomicU64,
    /// Bytes received and sent over this connection.
    pub rx_bytes: AtomicU64,
    /// Bytes sent.
    pub tx_bytes: AtomicU64,
    /// The session ended.
    pub closed: watch::Sender<bool>,
    /// Its key for private messages, from this session (only over a direct one, `crate::direct::direct_ok`).
    pub direct_key: Mutex<Option<seedwell_sync_proto::direct::DirectKey>>,
    /// Its user's name, from its DirectJoin (empty: none given, or an older Seedwell).
    pub user_name: Mutex<String>,
    /// The task sending our index of each folder to the peer: a folder linked again (the peer opened it again)
    /// replaces it.
    pub links: Mutex<BTreeMap<FolderId, tokio::task::AbortHandle>>,
    /// Folders this peer sent a chunk of that was not what was asked (bytes that do not hash to the chunk hash its
    /// version's signer signed): it is asked for none of their chunks again in this session.
    pub distrusted: Mutex<BTreeSet<FolderId>>,
    /// Our [`HoldQuery`](seedwell_sync_proto::msg::HoldQuery)s waiting for their answer.
    holds_pending: Mutex<HashMap<u32, oneshot::Sender<Vec<bool>>>>,
    /// Folders that went out in our Join on this session (at its start, offered since, or answering the peer's
    /// offer). A folder is linked on the session only if it is here and in the peer's ([`Self::got`]).
    pub joined: Mutex<BTreeSet<FolderId>>,
    /// Folders the peer's Joins on this session named (offered or answered).
    pub got: Mutex<BTreeSet<FolderId>>,
    /// Folders this device offered after the session began, its answer not in yet: the peer's Join for one is that
    /// answer, not one to answer again.
    pub offered: Mutex<BTreeSet<FolderId>>,
    /// Folders this device answered a Join for on this session (and linked): the peer's next Join for one is its reply
    /// to our answer, not the folder opened again.
    pub answered: Mutex<BTreeSet<FolderId>>,
    /// Folders that closed here while linked in this session (paused, stopped for a move, a switch, a removal): from
    /// that moment nothing of them goes either way on this session, and once open again the session is started anew,
    /// so the peer sends what it sent meanwhile again from what we hold ([`Self::access`] says none).
    pub stale: Mutex<BTreeSet<FolderId>>,
    /// Our [`RenameRequest`](seedwell_sync_proto::msg::RenameRequest)s waiting for their answer.
    renames_pending: Mutex<HashMap<u32, oneshot::Sender<seedwell_sync_proto::msg::RenameReply>>>,
    /// Our [`BlobWant`](seedwell_sync_proto::msg::BlobWant)s waiting for their part.
    blobs_pending: Mutex<HashMap<u32, oneshot::Sender<seedwell_sync_proto::msg::BlobData>>>,
    /// Our [`StreamRequest`](seedwell_sync_proto::msg::StreamRequest)s waiting for their piece.
    streams_pending: Mutex<HashMap<u32, oneshot::Sender<seedwell_sync_proto::msg::StreamData>>>,
}

impl PeerHandle {
    /// A handle for a new session; the receivers go to its writer task.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_pk: [u8; 32],
        name: String,
        client: String,
        agreed: seedwell_sync_proto::msg::Agreed,
        remote: SocketAddr,
        local: SocketAddr,
        exporter: [u8; 32],
        dialed: bool,
        via: crate::session::Via,
    ) -> (Arc<Self>, mpsc::Receiver<Message>, mpsc::Receiver<Message>) {
        let lan = via != crate::session::Via::Relay && crate::session::is_lan(remote.ip());
        let (control, crx) = mpsc::channel(256);
        let (data, drx) = mpsc::channel(64);
        let (closed, _) = watch::channel(false);
        let h = Arc::new(PeerHandle {
            device: DeviceId::of_public_key(&device_pk),
            device_pk,
            name,
            client,
            agreed,
            remote,
            lan,
            via,
            dialed,
            local,
            exporter,
            control,
            data,
            next_id: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            reading_held: AtomicBool::new(false),
            folders: RwLock::new(BTreeMap::new()),
            inflight: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            closed,
            direct_key: Mutex::new(None),
            user_name: Mutex::new(String::new()),
            links: Mutex::new(BTreeMap::new()),
            distrusted: Mutex::new(BTreeSet::new()),
            holds_pending: Mutex::new(HashMap::new()),
            renames_pending: Mutex::new(HashMap::new()),
            blobs_pending: Mutex::new(HashMap::new()),
            streams_pending: Mutex::new(HashMap::new()),
            stale: Mutex::new(BTreeSet::new()),
            joined: Mutex::new(BTreeSet::new()),
            got: Mutex::new(BTreeSet::new()),
            offered: Mutex::new(BTreeSet::new()),
            answered: Mutex::new(BTreeSet::new()),
        });
        (h, crx, drx)
    }

    /// Both sides have the feature bit `bit` ([`seedwell_sync_proto::msg::FEATURES`]).
    pub fn has(&self, bit: u64) -> bool {
        self.agreed.features & bit != 0
    }

    /// The session carries channels (both sides have [`seedwell_sync_proto::msg::FEATURE_CHAT`]).
    pub fn chat(&self) -> bool {
        self.agreed.features & seedwell_sync_proto::msg::FEATURE_CHAT != 0
    }

    /// The access the peer proved in `folder` (or channel), if it shares it with us over this session (none once the
    /// folder is stale here).
    pub fn access(&self, folder: &FolderId) -> Option<Access> {
        if lock(&self.stale).contains(folder) {
            return None;
        }
        self.folders.read().unwrap_or_else(|e| e.into_inner()).get(folder).copied()
    }

    /// The folder closed here (paused, stopped): if it is linked in this session, it is stale on it from now on
    /// (nothing of it is taken or sent here any more), and its sender stops.
    pub fn close_folder(&self, folder: &FolderId) {
        if self.folders.read().unwrap_or_else(|e| e.into_inner()).contains_key(folder) {
            lock(&self.stale).insert(*folder);
        }
        if let Some(link) = lock(&self.links).remove(folder) {
            link.abort();
        }
    }

    /// The folder is stale on this session.
    pub fn is_stale(&self, folder: &FolderId) -> bool {
        lock(&self.stale).contains(folder)
    }

    /// Sends a request; the response arrives on the returned receiver (dropped if the session ends).
    pub async fn request(&self, mut req: Request) -> Option<oneshot::Receiver<Response>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        req.id = id;
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        // Its answer could not be read before the session reads again (after the insert: a hold that starts now
        // either finds this request waiting and answers it, or is seen here).
        if self.reading_held.load(Ordering::SeqCst) {
            lock(&self.pending).remove(&id);
            return None;
        }
        if self.control.send(Message::Request(req)).await.is_err() {
            lock(&self.pending).remove(&id);
            return None;
        }
        Some(rx)
    }

    /// The session stops reading (`held`) until the frames it has to apply are taken, or reads again. While it does
    /// not, no answer to our requests can arrive (they come behind those frames), and the one who waits for them may
    /// be a pull that holds the very folder core the frames wait for. So the requests waiting are answered busy now
    /// (the puller asks another device, or again in its next round) and new ones are not made until it reads again.
    pub fn hold_reading(&self, held: bool) {
        self.reading_held.store(held, Ordering::SeqCst);
        if held {
            let waiting: Vec<_> = lock(&self.pending).drain().collect();
            for (id, tx) in waiting {
                let _ = tx.send(Response { id, code: seedwell_sync_proto::msg::ResponseCode::Busy, data: vec![] });
            }
            // A piece of a stream is given up the same way (its sender dropped): asked again later, the maker serves it
            // once more, and the stream goes on from there.
            lock(&self.streams_pending).clear();
        }
    }

    /// A response arrived.
    pub fn answer(&self, resp: Response) {
        if let Some(tx) = lock(&self.pending).remove(&resp.id) {
            let _ = tx.send(resp);
        }
    }

    /// Asks for a piece of an unhashed file (SPEC §7G.11; its id set here); the piece arrives on the returned receiver
    /// (dropped if the session ends). `None` while the session does not read (as [`Self::request`]).
    pub async fn stream(&self, mut req: seedwell_sync_proto::msg::StreamRequest) -> Option<(u32, oneshot::Receiver<seedwell_sync_proto::msg::StreamData>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        req.id = id;
        let (tx, rx) = oneshot::channel();
        lock(&self.streams_pending).insert(id, tx);
        if self.reading_held.load(Ordering::SeqCst) || self.control.send(Message::StreamRequest(req)).await.is_err() {
            lock(&self.streams_pending).remove(&id);
            return None;
        }
        Some((id, rx))
    }

    /// A piece that will not be waited for any more.
    pub fn forget_stream(&self, id: u32) {
        lock(&self.streams_pending).remove(&id);
    }

    /// A piece of an unhashed file we asked for arrived.
    pub fn answer_stream(&self, d: seedwell_sync_proto::msg::StreamData) {
        self.rx_bytes.fetch_add(d.data.len() as u64, Ordering::Relaxed);
        if let Some(tx) = lock(&self.streams_pending).remove(&d.id) {
            let _ = tx.send(d);
        }
    }

    /// Asks whether the peer holds these versions now (SPEC §7G.11); the answer, one flag per file, arrives on the
    /// returned receiver (dropped if the session ends; a peer that does not know the question never answers).
    pub async fn ask_holds(&self, folder_id: FolderId, files: Vec<seedwell_sync_proto::msg::HeldFile>) -> Option<(u32, oneshot::Receiver<Vec<bool>>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.holds_pending).insert(id, tx);
        let q = seedwell_sync_proto::msg::HoldQuery { id, folder_id, files };
        if self.control.send(Message::HoldQuery(q)).await.is_err() {
            lock(&self.holds_pending).remove(&id);
            return None;
        }
        Some((id, rx))
    }

    /// A hold query that will not be waited for any more (no answer in time).
    pub fn forget_holds(&self, id: u32) {
        lock(&self.holds_pending).remove(&id);
    }

    /// Hold queries waiting for their answer (tests).
    pub fn holds_waiting(&self) -> usize {
        lock(&self.holds_pending).len()
    }

    /// Asks the peer for part of an image of a channel ([`seedwell_sync_proto::msg::BlobWant`], its id set here): the
    /// request's id, and the receiver its part arrives on (dropped when the session ends).
    pub async fn ask_blob(&self, mut want: seedwell_sync_proto::msg::BlobWant) -> Option<(u32, oneshot::Receiver<seedwell_sync_proto::msg::BlobData>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        want.id = id;
        let (tx, rx) = oneshot::channel();
        lock(&self.blobs_pending).insert(id, tx);
        if self.control.send(Message::BlobWant(want)).await.is_err() {
            lock(&self.blobs_pending).remove(&id);
            return None;
        }
        Some((id, rx))
    }

    /// A part that will not be waited for any more.
    pub fn forget_blob(&self, id: u32) {
        lock(&self.blobs_pending).remove(&id);
    }

    /// A part of an image we asked for arrived.
    pub fn answer_blob(&self, d: seedwell_sync_proto::msg::BlobData) {
        self.rx_bytes.fetch_add(d.data.len() as u64, Ordering::Relaxed);
        if let Some(tx) = lock(&self.blobs_pending).remove(&d.id) {
            let _ = tx.send(d);
        }
    }

    /// The answer to one of our hold queries arrived.
    pub fn answer_holds(&self, reply: seedwell_sync_proto::msg::HoldReply) {
        if let Some(tx) = lock(&self.holds_pending).remove(&reply.id) {
            let _ = tx.send(reply.held);
        }
    }

    /// Asks the peer to rename `from` to `to` in a folder ([`seedwell_sync_proto::msg::RenameRequest`]); the answer arrives
    /// on the receiver (dropped when the session ends). With its id, to [`Self::forget_rename`] it.
    pub async fn ask_rename(
        &self,
        folder_id: FolderId,
        from: String,
        to: String,
        seen: [u8; 32],
        ttl_ms: u32,
    ) -> Option<(u32, oneshot::Receiver<seedwell_sync_proto::msg::RenameReply>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.renames_pending).insert(id, tx);
        let q = seedwell_sync_proto::msg::RenameRequest { id, folder_id, from, to, seen, ttl_ms };
        if self.control.send(Message::RenameRequest(q)).await.is_err() {
            lock(&self.renames_pending).remove(&id);
            return None;
        }
        Some((id, rx))
    }

    /// A rename request that will not be waited for any more.
    pub fn forget_rename(&self, id: u32) {
        lock(&self.renames_pending).remove(&id);
    }

    /// The answer to one of our rename requests arrived.
    pub fn answer_rename(&self, reply: seedwell_sync_proto::msg::RenameReply) {
        if let Some(tx) = lock(&self.renames_pending).remove(&reply.id) {
            let _ = tx.send(reply);
        }
    }

    /// Rename requests waiting for their answer (tests).
    pub fn renames_waiting(&self) -> usize {
        lock(&self.renames_pending).len()
    }

    /// The session is over: pending requests fail (their receivers see the sender dropped). Kept also when nothing
    /// watches the flag yet (`send` would drop it then): a session closed between its registration and its read loop,
    /// as the service's stop does to one coming up, ran on and held its folders, so their indexes stayed open.
    pub fn close(&self) {
        lock(&self.pending).clear();
        lock(&self.holds_pending).clear();
        lock(&self.renames_pending).clear();
        // A file's pieces too: a download waiting for one held its channel (and the session) for the piece's 30 s
        // after the service stopped, and wrote its record over the one of the service started again on the same state.
        lock(&self.blobs_pending).clear();
        self.closed.send_replace(true);
    }

    /// Is the session over?
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
}

/// Every folder, channel and connected device.
#[derive(Default)]
pub struct Registry {
    folders: RwLock<BTreeMap<FolderId, Arc<FolderShared>>>,
    channels: RwLock<BTreeMap<FolderId, Arc<crate::chat::ChannelShared>>>,
    /// The user's own devices, once this device is in such a group.
    devices: RwLock<Option<Arc<Space>>>,
    /// The groups this device's group moved on from (its key changed after a removal), oldest first: kept without
    /// their key, only to tell one of its own devices that was away where the group went.
    ancestors: RwLock<Vec<Arc<Space>>>,
    peers: RwLock<BTreeMap<DeviceId, Arc<PeerHandle>>>,
    /// The service stopped: no session comes up any more (one still being set up when it stopped is refused).
    stopped: AtomicBool,
    /// Test switch: devices with which direct (not relayed) sessions are refused.
    refused: RwLock<BTreeSet<DeviceId>>,
}

impl Registry {
    /// Refuses direct sessions with `device` from now on (the test switch), also one being set up right now.
    pub fn refuse_direct(&self, device: DeviceId) {
        self.refused.write().unwrap_or_else(|e| e.into_inner()).insert(device);
    }

    /// Direct sessions with `device` are refused.
    pub fn refuses_direct(&self, device: &DeviceId) -> bool {
        self.refused.read().unwrap_or_else(|e| e.into_inner()).contains(device)
    }

    /// A channel.
    pub fn channel(&self, id: &FolderId) -> Option<Arc<crate::chat::ChannelShared>> {
        self.channels.read().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
    }

    /// Every channel.
    pub fn channels(&self) -> Vec<Arc<crate::chat::ChannelShared>> {
        self.channels.read().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }

    /// Adds or replaces a channel.
    pub fn add_channel(&self, c: Arc<crate::chat::ChannelShared>) {
        self.channels.write().unwrap_or_else(|e| e.into_inner()).insert(c.id, c);
    }

    /// Removes a channel.
    pub fn remove_channel(&self, id: &FolderId) -> Option<Arc<crate::chat::ChannelShared>> {
        self.channels.write().unwrap_or_else(|e| e.into_inner()).remove(id)
    }

    /// A folder's or a channel's space, or the devices group's (the network side only: what the user and agents ask
    /// for goes through [`Self::user_space`]).
    pub fn space(&self, id: &FolderId) -> Option<Arc<Space>> {
        self.folder(id)
            .map(|f| f.space.clone())
            .or_else(|| self.channel(id).map(|c| c.space.clone()))
            .or_else(|| self.devices().filter(|g| g.id == *id))
            .or_else(|| self.ancestors().into_iter().find(|g| g.id == *id))
    }

    /// A folder's or a channel's space, never the devices group's: what a caller of the service may name.
    pub fn user_space(&self, id: &FolderId) -> Option<Arc<Space>> {
        self.folder(id).map(|f| f.space.clone()).or_else(|| self.channel(id).map(|c| c.space.clone()))
    }

    /// Every folder's and channel's space, and the devices group's (it is found, dialed and announced like them).
    pub fn spaces(&self) -> Vec<Arc<Space>> {
        let mut out: Vec<Arc<Space>> = self.folders().iter().map(|f| f.space.clone()).collect();
        out.extend(self.channels().iter().map(|c| c.space.clone()));
        out.extend(self.devices());
        out
    }

    /// The user's own devices group, if this device is in one.
    pub fn devices(&self) -> Option<Arc<Space>> {
        self.devices.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sets (or clears) the devices group.
    pub fn set_devices(&self, g: Option<Arc<Space>>) {
        *self.devices.write().unwrap_or_else(|e| e.into_inner()) = g;
    }

    /// The groups the devices group moved on from, oldest first.
    pub fn ancestors(&self) -> Vec<Arc<Space>> {
        self.ancestors.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Sets them.
    pub fn set_ancestors(&self, a: Vec<Arc<Space>>) {
        *self.ancestors.write().unwrap_or_else(|e| e.into_inner()) = a;
    }

    /// A folder.
    pub fn folder(&self, id: &FolderId) -> Option<Arc<FolderShared>> {
        self.folders.read().unwrap_or_else(|e| e.into_inner()).get(id).cloned()
    }

    /// Every folder.
    pub fn folders(&self) -> Vec<Arc<FolderShared>> {
        self.folders.read().unwrap_or_else(|e| e.into_inner()).values().cloned().collect()
    }

    /// Adds or replaces a folder.
    pub fn add_folder(&self, f: Arc<FolderShared>) {
        self.folders.write().unwrap_or_else(|e| e.into_inner()).insert(f.id, f);
    }

    /// Removes a folder.
    pub fn remove_folder(&self, id: &FolderId) -> Option<Arc<FolderShared>> {
        self.folders.write().unwrap_or_else(|e| e.into_inner()).remove(id)
    }

    /// A connected device.
    pub fn peer(&self, id: &DeviceId) -> Option<Arc<PeerHandle>> {
        self.peers.read().unwrap_or_else(|e| e.into_inner()).get(id).cloned().filter(|p| !p.is_closed())
    }

    /// Every connected device.
    pub fn peers(&self) -> Vec<Arc<PeerHandle>> {
        self.peers.read().unwrap_or_else(|e| e.into_inner()).values().filter(|p| !p.is_closed()).cloned().collect()
    }

    /// Registers a session. Two sessions with one device (both dialed at once) are one too many: the one
    /// dialed by the device with the lower id stays, so both sides keep the same one. Returns false when
    /// `p` lost and must close.
    pub fn add_peer(&self, me: &DeviceId, p: Arc<PeerHandle>) -> bool {
        if p.via != crate::session::Via::Relay && self.refuses_direct(&p.device) {
            return false;
        }
        let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
        // (Read under the lock `stop_peers` sets it under: a session is either closed by it or refused here.)
        if self.stopped.load(Ordering::SeqCst) {
            return false;
        }
        if let Some(old) = peers.get(&p.device).filter(|o| !o.is_closed()) {
            let dialer_of = |h: &PeerHandle| if h.dialed { *me } else { h.device };
            if dialer_of(old) <= dialer_of(&p) {
                return false;
            }
            old.close();
        }
        peers.insert(p.device, p);
        true
    }

    /// The service stops: every session is closed, and none comes up from now on, also one whose setup is still
    /// running (its answer may have been made before the stop: it is refused when it would come up).
    pub fn stop_peers(&self) {
        let peers: Vec<Arc<PeerHandle>> = {
            let peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            self.stopped.store(true, Ordering::SeqCst);
            peers.values().cloned().collect()
        };
        for p in peers {
            p.close();
        }
    }

    /// The service stopped ([`Self::stop_peers`]).
    pub fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::SeqCst)
    }

    /// Unregisters a session (only if it is still the registered one).
    pub fn remove_peer(&self, p: &Arc<PeerHandle>) {
        let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
        if peers.get(&p.device).is_some_and(|q| Arc::ptr_eq(q, p)) {
            peers.remove(&p.device);
        }
    }
}

/// What sessions and runtimes tell the service.
pub enum HubMsg {
    /// A session with this device started.
    PeerUp(DeviceId),
    /// It ended.
    PeerDown(DeviceId),
    /// A device proved membership of a folder (remembered, and dialed when not connected).
    MemberSeen {
        /// The folder.
        folder: FolderId,
        /// The device.
        device: DeviceId,
    },
    /// Signed address records a peer passed on (peer exchange).
    Addresses(Vec<seedwell_sync_proto::msg::AddressRecord>),
    /// A rendezvous or relay message from a peer.
    Relay(DeviceId, Box<Message>),
    /// The private conversations changed (one started, its device's name or address changed): `sync.toml` gets them.
    Conversations,
}

/// Everything a session needs from the service.
pub struct Ctx {
    /// This device's key.
    pub key: Arc<seedwell_sync_proto::device::DeviceKey>,
    /// This device's id.
    pub me: DeviceId,
    /// `Seedwell <version>`, sent in Hello.
    pub client: String,
    /// The name other devices see.
    pub device_name: Mutex<String>,
    /// The name this device's user goes by in chats, with the device's (empty: none given).
    pub user_name: Mutex<String>,
    /// TLS configurations.
    pub tls: crate::tls::Tls,
    /// Folders and sessions.
    pub registry: Registry,
    /// To the service.
    pub hub: mpsc::UnboundedSender<HubMsg>,
    /// Internet speed limits.
    pub limits: crate::limit::Limits,
    /// **QA:** the speed limits apply to LAN sessions too ([`crate::limit::QA_LIMIT_LAN`], an isolated run only).
    pub limit_lan: AtomicBool,
    /// Where secrets are kept (a new co-owner stores the folder secret it receives).
    pub secrets: Arc<dyn crate::secrets::SecretStore>,
    /// The protocol versions this device speaks, lowest and highest (in its Hello).
    pub protocol: (u32, u32),
    /// Its feature bits (in its Hello).
    pub features: u64,
    /// New channel messages (this device's and others'), for watchers.
    pub chat_events: tokio::sync::broadcast::Sender<crate::chat::ChatEvent>,
    /// Private messages.
    pub direct: crate::direct::Direct,
    /// How long a device asked whether it holds files may take to answer (SPEC §7G.11).
    pub hold_wait: Mutex<std::time::Duration>,
    /// **Tests:** a pause after each scan step, the core let go (a big first scan, slowed down to watch it).
    pub scan_step_wait: Mutex<std::time::Duration>,
    /// **Tests:** a pause at the start of each fetch of a pull, before it asks, the folder's core held (a slow disk
    /// under a pull: a round that takes long).
    pub fetch_wait: Mutex<std::time::Duration>,
    /// **Tests:** a pause before each piece of an unhashed file this device serves is read (a slow first download).
    pub stream_wait: Mutex<std::time::Duration>,
    /// **Tests:** a pause before each chunk this device serves is read (a slow link, a slow device).
    pub serve_wait: Mutex<std::time::Duration>,
    /// **Tests:** a pause in each scan step, the folder's core held (a slow disk under a scan).
    pub scan_hold: Mutex<std::time::Duration>,
    /// **Tests:** a pause at the end of each job of a folder's lanes, its index and files held (a slow disk's flush, a
    /// virus scanner reading the new file).
    pub job_hold: Mutex<std::time::Duration>,
    /// **Tests:** how many jobs are in that pause now.
    pub jobs_held: std::sync::atomic::AtomicUsize,
    /// **Tests:** a fetch's read of where it stands, having read a file's record (not here, asked for), waits until that
    /// record is committed as here before it reads what the file needs: the thread let go of the CPU there while the
    /// pull round committed the file (under load).
    pub fetch_read_gap: AtomicBool,
    /// **Tests:** a pull round with a finished download to place starts after a pause this long, the folder's core let
    /// go (a round late under load: the bytes are here, their record is not yet).
    pub place_wait: Mutex<std::time::Duration>,
    /// **Tests:** hold queries and rename requests go unanswered, as an earlier Seedwell leaves them.
    pub silent_ondemand: std::sync::atomic::AtomicBool,
    /// Bumped (SeqCst) whenever a folder opens or closes here (pause, resume, stop, reopen, LAN-only, removal,
    /// join): a session set up while it moved checks its folders again ([`crate::peer`]'s reconcile).
    pub folder_gen: std::sync::atomic::AtomicU64,
    /// Every folder this device has (its configuration's, open or not: held back, failed to open, stopped for a
    /// move): one a peer offers while it is not open here is stale on that session. (One it does not have is not.)
    pub ours: Mutex<BTreeSet<FolderId>>,
    /// **Tests:** what serving files does to the pieces it sends.
    pub file_hooks: crate::files::Hooks,
    /// **Tests:** sessions set up so far (to see that none started anew).
    pub sessions: std::sync::atomic::AtomicU64,
    /// **Tests:** how long an answering session waits between making its Join and going on.
    pub setup_wait: Mutex<std::time::Duration>,
    /// **Tests:** answering sessions waiting there now.
    pub setups_waiting: std::sync::atomic::AtomicUsize,
    /// **Tests:** hold queries are answered yes, whatever is held (a member that lies).
    pub yes_to_holds: std::sync::atomic::AtomicBool,
    /// Devices that asked to join this device's devices group with a pairing link it made, waiting for the user to
    /// confirm or refuse (in memory only; each lapses with its link).
    pub pairing: Mutex<Vec<crate::devices::PairRequest>>,
    /// The devices group, or a folder or channel, changed: what follows is brought in line ([`crate::devices`]).
    pub devices_wake: Notify,
    /// One change of the devices group at a time (a removal and its key change, a move, leaving).
    pub devices_lock: tokio::sync::Mutex<()>,
    /// **Tests:** the devices group's key is left as it is: no move, no new key ([`crate::devices`]).
    pub hold_key_changes: AtomicBool,
    /// **Tests:** a switch to another devices group waits, once it has its groups ready and before they count.
    pub hold_switch: AtomicBool,
    /// **Tests:** switches waiting there now.
    pub switches_waiting: std::sync::atomic::AtomicUsize,
    /// Sessions may still be coming up until then (the service started, the network changed, a session began): a
    /// fetch whose file no connected device has waits for them until then, and says so after (SPEC §7G.11).
    pub settling: Mutex<std::time::Instant>,
    /// How long that is after each of those ([`crate::service::FETCH_GRACE`]; shorter in tests).
    pub fetch_grace: Mutex<std::time::Duration>,
    /// How long a fetch may get no byte further from the devices it asks before it gives up
    /// ([`crate::service::FETCH_STALL`]; shorter in tests).
    pub fetch_stall: Mutex<std::time::Duration>,
    /// Whether other devices can reach this one ([`crate::inbound`]).
    pub reach: crate::inbound::Reach,
    /// Told what each device this one talks to runs ([`crate::service::Options::peer_client`]).
    pub peer_client: Option<PeerClientHook>,
}

/// Told the `client` (`Seedwell <version>`) of a device from its Hello: of each device a session with this one is
/// set up with, and of a member of a folder, channel or device group here that was refused because the two cannot
/// talk; never of a device that only dialed in. Called on the service's runtime, so it returns at once.
pub type PeerClientHook = Arc<dyn Fn(&str) + Send + Sync>;

impl Ctx {
    /// The speed limits apply to a session (`lan`: it stays on the local network, which is never limited, but in an
    /// isolated QA run that asks for it).
    pub fn limited(&self, lan: bool) -> bool {
        !lan || self.limit_lan.load(Ordering::Relaxed)
    }

    /// Sessions may be coming up from now ([`Self::settling`]): the service started, the network changed, a session
    /// began.
    pub fn settle(&self) {
        let until = std::time::Instant::now() + *lock(&self.fetch_grace);
        let mut s = lock(&self.settling);
        *s = (*s).max(until);
    }

    /// **Tests:** a device offering none of the on-demand feature bits plays Seedwell 0.1.11 in its sessions: it
    /// keeps no stale folders and no session reconciling, links a folder on any Join for it it has not linked,
    /// answers one only if it did not name it itself before, and offers a folder it opened again on the same session.
    pub fn plays_0_1_11(&self) -> bool {
        self.features & seedwell_sync_proto::msg::FEATURE_HOLDS == 0
    }
}

/// Files of a folder with a free in progress, from the first question to the end of the free: while it lives, this
/// device answers no for them. Ends when dropped, also when the free fails or its future is dropped; each free counts
/// only its own files.
pub struct Freeing {
    f: Arc<FolderShared>,
    paths: Vec<String>,
}

impl Freeing {
    /// Starts one for `paths`.
    pub fn new(f: Arc<FolderShared>, paths: Vec<String>) -> Self {
        {
            let mut m = lock(&f.freeing);
            for p in &paths {
                *m.entry(p.clone()).or_insert(0) += 1;
            }
        }
        Freeing { f, paths }
    }
}

impl Drop for Freeing {
    fn drop(&mut self) {
        let mut m = lock(&self.f.freeing);
        for p in &self.paths {
            if let Some(n) = m.get_mut(p) {
                *n -= 1;
                if *n == 0 {
                    m.remove(p);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure: a session closed (the service stopped, the device went away) while a download waited for a
    /// file's piece left that wait standing until the piece's time ran out (30 s): the download held its channel past
    /// the service's stop (`a folder or channel was still in use 10 s after the service stopped`) and, under load, a
    /// service started again on the same state never finished the download (2026-11-07, `a_download_resumes_after_a_
    /// restart_from_the_pieces_it_has`). Closing ends every wait at once.
    #[tokio::test]
    async fn closing_a_session_ends_its_waits_for_a_files_pieces() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let agreed = seedwell_sync_proto::msg::Agreed { protocol: seedwell_sync_proto::msg::PROTOCOL, features: 0 };
        let (peer, _crx, _drx) = PeerHandle::new([7; 32], "X".into(), "Seedwell".into(), agreed, addr, addr, [0; 32], true, crate::session::Via::Tcp);
        let want = seedwell_sync_proto::msg::BlobWant { id: 0, scope: seedwell_sync_proto::msg::BlobScope::Direct, hash: [1; 32], offset: 0, len: 10 };
        let (_, rx) = peer.ask_blob(want).await.expect("asked");
        peer.close();
        let waited = tokio::time::timeout(std::time::Duration::from_secs(2), rx).await;
        assert!(matches!(waited, Ok(Err(_))), "the wait ends with the session, not with the piece's time: {waited:?}");
    }

    /// The failure: a session closed after it was registered and before its read loop watched the flag (the
    /// service's stop met it coming up) was never closed: the flag's send found no receiver and dropped the value,
    /// so the read loop waited for ever, held the session's folders, and their indexes stayed open after the stop
    /// returned (the next start on the same state could not open them).
    #[tokio::test]
    async fn a_session_closed_before_its_read_loop_watches_is_closed() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let agreed = seedwell_sync_proto::msg::Agreed { protocol: seedwell_sync_proto::msg::PROTOCOL, features: 0 };
        let (peer, _crx, _drx) = PeerHandle::new([7; 32], "X".into(), "Seedwell".into(), agreed, addr, addr, [0; 32], true, crate::session::Via::Tcp);
        let reg = Registry::default();
        assert!(reg.add_peer(&DeviceId([1; 32]), peer.clone()));
        reg.stop_peers();
        assert!(peer.is_closed());
        let mut closed = peer.closed.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(5), closed.wait_for(|c| *c)).await.expect("the read loop sees the close").unwrap();
    }

    /// The QA switch never slows the user's own Seedwell (a state folder outside any temporary folder), and a value
    /// it cannot read is said, not taken as some other wait.
    #[test]
    fn slow_saves_only_in_an_isolated_run() {
        let temp = std::env::temp_dir().join("seedwell-qa").join("state");
        let ms = |v: &str| Some(std::ffi::OsString::from(v));
        assert_eq!(qa_slow_saves(ms("500").as_deref(), &temp), Ok(Some(std::time::Duration::from_millis(500))));
        assert_eq!(qa_slow_saves(None, &temp), Ok(None));
        assert!(qa_slow_saves(ms("soon").as_deref(), &temp).is_err());
        let home = std::path::PathBuf::from(if cfg!(windows) { r"C:\Users\someone\AppData\Roaming\Seedwell" } else { "/home/someone/.seedwell" });
        assert!(qa_slow_saves(ms("500").as_deref(), &home).is_err(), "the user's own Seedwell ignores it");
    }

    /// A kept folder's issue is shown once and goes alone: a pull's issue at the same path stays.
    #[test]
    fn a_kept_folders_issue_is_one_and_goes_alone() {
        let mut live = Live::default();
        live.issues.push(("docs/old".into(), "could not be read: busy".into()));
        live.set_kept("docs/old", "docs/old/~keep.tmp");
        live.set_kept("docs/old", "docs/old/~keep.tmp");
        let kept = seedwell_sync::pull::kept_words("docs/old/~keep.tmp");
        assert_eq!(live.issues.iter().filter(|(_, w)| *w == kept).count(), 1);
        live.drop_kept("docs/old");
        assert_eq!(live.issues, [("docs/old".to_string(), "could not be read: busy".to_string())]);
        assert!(live.kept.is_empty());
    }

    fn chunk_job(path: &str) -> seedwell_sync::lazy::Job {
        seedwell_sync::lazy::Job::Chunks(Box::new(seedwell_sync::lazy::ChunkJob {
            path: path.into(),
            tmp: seedwell_sync::pull::temp_path(path),
            version: seedwell_sync_proto::model::Version::new(),
            chunks: Vec::new(),
            old: None,
            known: None,
            holders: Vec::new(),
            local: None,
        }))
    }

    /// The failure (a race): a job's end reaches the core while its lane still names its path as the one running; a
    /// round takes a job for the path again in between (the core marks it running), and the lane drops it as the one it
    /// runs already, so the path stays "running" in the core and no round touches it again until the folder reopens.
    /// A job for the path running now is queued; one for a path queued already replaces it (handed back to be told off).
    #[test]
    fn a_job_for_the_path_running_now_is_queued_never_dropped() {
        let mut lane = Streamer { current: Some("film.bin".into()), running: true, ..Default::default() };
        assert!(lane.push(chunk_job("film.bin")).is_none());
        assert_eq!(lane.queue.len(), 1, "queued behind the one running");
        let replaced = lane.push(chunk_job("film.bin")).expect("the queued one, replaced");
        assert_eq!(replaced.path(), "film.bin");
        assert_eq!(lane.queue.len(), 1);
    }
}

