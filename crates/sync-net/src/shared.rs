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
use trs_sync::folder::FolderCore;
use trs_sync::store_redb::RedbReader;
use trs_sync_proto::ids::{DeviceId, FolderId};
use trs_sync_proto::keys::{Access, FolderKey, FolderSecret};
use trs_sync_proto::members::{InviteLink, Membership, Role};
use trs_sync_proto::msg::{Message, Request, Response};

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
        if waited >= LONG_LOCK_WAIT {
            eprintln!("seedwell sync: waited {} ms for a lock at {}", waited.as_millis(), std::panic::Location::caller());
        }
        return g;
    }
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Debug builds: a wait for a lock this long is reported ([`lock`]).
const LONG_LOCK_WAIT: std::time::Duration = std::time::Duration::from_millis(250);

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
        Space {
            id,
            kind,
            public,
            secret: RwLock::new(secret),
            membership: RwLock::new(membership),
            members_path,
            invite: Mutex::new(invite),
            links: Mutex::new(links),
            members: RwLock::new(BTreeSet::new()),
            name: Mutex::new(name),
            paused: AtomicBool::new(false),
            lan_only: AtomicBool::new(false),
            notes: Mutex::new(Notes::default()),
            following: AtomicBool::new(false),
        }
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
        self.role_of(device_pk).is_some_and(|r| r.access() == trs_sync_proto::keys::Access::Write)
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
    /// On demand: the paths kept on this device (a copy of the core's, for listing items without it).
    pub pins: Mutex<std::collections::BTreeSet<String>>,
    /// On demand: the items a host lists, read from `index` (never waiting for the core).
    pub items: Mutex<trs_sync::ondemand::Items>,
    /// On demand: files with a free in progress here, and how many frees hold each ([`Freeing`]): asked whether it
    /// holds them, this device says no (SPEC §7G.11).
    pub freeing: Mutex<BTreeMap<String, u32>>,
    /// The ignore patterns the core was opened with (for answers given without it).
    pub ignore: trs_sync::ignore::Ignore,
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
    /// Ask the runtime for a full scan now (the user's "Rescan", or a changed ignore file).
    pub rescan: Notify,
    /// Up when the folder is being stopped (removed, moved, switched): what holds it for a peer (its index sender)
    /// lets go, so the folder's index closes while that peer stays connected.
    pub stopped: watch::Sender<bool>,
    /// How long archived versions are kept, and how much room they may take.
    pub retention: trs_sync::archive::Retention,
    /// Bytes fetched from peers for this folder.
    pub fetched: crate::limit::Meter,
    /// Bytes served to peers from this folder.
    pub served: crate::limit::Meter,
    /// Set once everything above is dropped, the index included. Last, because fields drop in order: the last
    /// `Arc` of a folder counts as gone before its fields are, so only this says the index file is closed.
    pub closed: Closed,
}

/// A flag that goes up when it is dropped (see [`FolderShared::closed`]).
#[derive(Debug, Default)]
pub struct Closed(Arc<AtomicBool>);

impl Closed {
    /// Something to watch: true once the owner's earlier fields are dropped.
    pub fn watch(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }
}

impl Drop for Closed {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

impl FolderShared {
    /// Our index id and the last sequence given out, from the index's read-only view (never waits for the core).
    pub fn index_state(&self) -> std::io::Result<(u64, u64)> {
        let m = trs_sync::index::IndexStore::meta(&self.index.view())?;
        Ok((m.index_id, m.sequence))
    }

    /// Our entries above `after` for a peer ([`trs_sync::folder::announce_since`]), from the read-only view: a peer is
    /// sent what each scan step committed while the scan still runs.
    pub fn announce_since(&self, after: u64, limit: usize) -> std::io::Result<Vec<trs_sync_proto::model::IndexEntry>> {
        trs_sync::folder::announce_since(&self.index.view(), self.on_demand, after, limit)
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
            unavailable: 0,
            pending: (Vec::new(), false),
            local: (0, 0),
            fetched: 0,
            ignore_file: false,
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
    pub agreed: trs_sync_proto::msg::Agreed,
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
    pub direct_key: Mutex<Option<trs_sync_proto::direct::DirectKey>>,
    /// Its user's name, from its DirectJoin (empty: none given, or an older Seedwell).
    pub user_name: Mutex<String>,
    /// The task sending our index of each folder to the peer: a folder linked again (the peer opened it again)
    /// replaces it.
    pub links: Mutex<BTreeMap<FolderId, tokio::task::AbortHandle>>,
    /// Our [`HoldQuery`](trs_sync_proto::msg::HoldQuery)s waiting for their answer.
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
    /// Our [`RenameRequest`](trs_sync_proto::msg::RenameRequest)s waiting for their answer.
    renames_pending: Mutex<HashMap<u32, oneshot::Sender<trs_sync_proto::msg::RenameReply>>>,
    /// Our [`BlobWant`](trs_sync_proto::msg::BlobWant)s waiting for their part.
    blobs_pending: Mutex<HashMap<u32, oneshot::Sender<trs_sync_proto::msg::BlobData>>>,
}

impl PeerHandle {
    /// A handle for a new session; the receivers go to its writer task.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_pk: [u8; 32],
        name: String,
        client: String,
        agreed: trs_sync_proto::msg::Agreed,
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
            holds_pending: Mutex::new(HashMap::new()),
            renames_pending: Mutex::new(HashMap::new()),
            blobs_pending: Mutex::new(HashMap::new()),
            stale: Mutex::new(BTreeSet::new()),
            joined: Mutex::new(BTreeSet::new()),
            got: Mutex::new(BTreeSet::new()),
            offered: Mutex::new(BTreeSet::new()),
            answered: Mutex::new(BTreeSet::new()),
        });
        (h, crx, drx)
    }

    /// Both sides have the feature bit `bit` ([`trs_sync_proto::msg::FEATURES`]).
    pub fn has(&self, bit: u64) -> bool {
        self.agreed.features & bit != 0
    }

    /// The session carries channels (both sides have [`trs_sync_proto::msg::FEATURE_CHAT`]).
    pub fn chat(&self) -> bool {
        self.agreed.features & trs_sync_proto::msg::FEATURE_CHAT != 0
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
                let _ = tx.send(Response { id, code: trs_sync_proto::msg::ResponseCode::Busy, data: vec![] });
            }
        }
    }

    /// A response arrived.
    pub fn answer(&self, resp: Response) {
        if let Some(tx) = lock(&self.pending).remove(&resp.id) {
            let _ = tx.send(resp);
        }
    }

    /// Asks whether the peer holds these versions now (SPEC §7G.11); the answer, one flag per file, arrives on the
    /// returned receiver (dropped if the session ends; a peer that does not know the question never answers).
    pub async fn ask_holds(&self, folder_id: FolderId, files: Vec<trs_sync_proto::msg::HeldFile>) -> Option<(u32, oneshot::Receiver<Vec<bool>>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.holds_pending).insert(id, tx);
        let q = trs_sync_proto::msg::HoldQuery { id, folder_id, files };
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

    /// Asks the peer for part of an image of a channel ([`trs_sync_proto::msg::BlobWant`], its id set here): the
    /// request's id, and the receiver its part arrives on (dropped when the session ends).
    pub async fn ask_blob(&self, mut want: trs_sync_proto::msg::BlobWant) -> Option<(u32, oneshot::Receiver<trs_sync_proto::msg::BlobData>)> {
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
    pub fn answer_blob(&self, d: trs_sync_proto::msg::BlobData) {
        self.rx_bytes.fetch_add(d.data.len() as u64, Ordering::Relaxed);
        if let Some(tx) = lock(&self.blobs_pending).remove(&d.id) {
            let _ = tx.send(d);
        }
    }

    /// The answer to one of our hold queries arrived.
    pub fn answer_holds(&self, reply: trs_sync_proto::msg::HoldReply) {
        if let Some(tx) = lock(&self.holds_pending).remove(&reply.id) {
            let _ = tx.send(reply.held);
        }
    }

    /// Asks the peer to rename `from` to `to` in a folder ([`trs_sync_proto::msg::RenameRequest`]); the answer arrives
    /// on the receiver (dropped when the session ends). With its id, to [`Self::forget_rename`] it.
    pub async fn ask_rename(
        &self,
        folder_id: FolderId,
        from: String,
        to: String,
        seen: [u8; 32],
        ttl_ms: u32,
    ) -> Option<(u32, oneshot::Receiver<trs_sync_proto::msg::RenameReply>)> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        lock(&self.renames_pending).insert(id, tx);
        let q = trs_sync_proto::msg::RenameRequest { id, folder_id, from, to, seen, ttl_ms };
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
    pub fn answer_rename(&self, reply: trs_sync_proto::msg::RenameReply) {
        if let Some(tx) = lock(&self.renames_pending).remove(&reply.id) {
            let _ = tx.send(reply);
        }
    }

    /// Rename requests waiting for their answer (tests).
    pub fn renames_waiting(&self) -> usize {
        lock(&self.renames_pending).len()
    }

    /// The session is over: pending requests fail (their receivers see the sender dropped).
    pub fn close(&self) {
        lock(&self.pending).clear();
        lock(&self.holds_pending).clear();
        lock(&self.renames_pending).clear();
        let _ = self.closed.send(true);
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
    Addresses(Vec<trs_sync_proto::msg::AddressRecord>),
    /// A rendezvous or relay message from a peer.
    Relay(DeviceId, Box<Message>),
    /// The private conversations changed (one started, its device's name or address changed): `sync.toml` gets them.
    Conversations,
}

/// Everything a session needs from the service.
pub struct Ctx {
    /// This device's key.
    pub key: Arc<trs_sync_proto::device::DeviceKey>,
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
    /// **Tests:** hold queries are answered yes, whatever is held (a member that lies).
    pub yes_to_holds: std::sync::atomic::AtomicBool,
    /// Devices that asked to join this device's devices group with a pairing link it made, waiting for the user to
    /// confirm or refuse (in memory only; each lapses with its link).
    pub pairing: Mutex<Vec<crate::devices::PairRequest>>,
    /// The devices group, or a folder or channel, changed: what follows is brought in line ([`crate::devices`]).
    pub devices_wake: Notify,
    /// One change of the devices group at a time (a removal and its key change, a move, leaving).
    pub devices_lock: tokio::sync::Mutex<()>,
}

impl Ctx {
    /// **Tests:** a device offering none of the on-demand feature bits plays Seedwell 0.1.11 in its sessions: it
    /// keeps no stale folders and no session reconciling, links a folder on any Join for it it has not linked,
    /// answers one only if it did not name it itself before, and offers a folder it opened again on the same session.
    pub fn plays_0_1_11(&self) -> bool {
        self.features & trs_sync_proto::msg::FEATURE_HOLDS == 0
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
