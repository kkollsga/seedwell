//! State shared by the service, the folder runtimes and the peer sessions.
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
pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// One folder, as the network side sees it.
pub struct FolderShared {
    /// Its id.
    pub id: FolderId,
    /// The folder's public key (the folder id, discovery topics, LAN tags).
    pub public: FolderKey,
    /// The folder secret, on an owner device (a new co-owner receives it from an owner).
    pub secret: RwLock<Option<FolderSecret>>,
    /// Who is in the folder and what each device may do. Read without the core: the UI asks often.
    pub membership: RwLock<Membership>,
    /// Where the membership is kept (next to the index).
    pub members_path: PathBuf,
    /// The invitation this device joins with, until an owner or admin admits it.
    pub invite: Mutex<Option<InviteLink>>,
    /// The links of invitations this device made, by invite id (shown again in the Share dialog).
    pub links: Mutex<BTreeMap<[u8; 16], String>>,
    /// Where it is on disk.
    pub root: PathBuf,
    /// The core. Lock only on a blocking thread.
    pub core: Mutex<FolderCore>,
    /// Reads the core's index without its lock: requests from peers are answered while a pull holds the
    /// core (waiting on peers itself), so two devices pulling from each other never wait on each other.
    pub index: RedbReader,
    /// Our latest local sequence: sessions wake on changes and send the new entries.
    pub local_seq: watch::Sender<u64>,
    /// A peer's index changed what we need: the runtime pulls.
    pub remote_changed: Notify,
    /// Syncing is paused by the user (nothing is scanned, pulled or served).
    pub paused: AtomicBool,
    /// Devices we have seen in this folder (from their announcements): dialed when not connected.
    pub members: RwLock<BTreeSet<DeviceId>>,
    /// The folder's name on this device.
    pub name: Mutex<String>,
    /// What the runtime is doing, for the snapshot.
    pub live: Mutex<Live>,
    /// Ask the runtime for a full scan now (the user's "Rescan", or a changed ignore file).
    pub rescan: Notify,
    /// Only sync with devices on the local network.
    pub lan_only: AtomicBool,
    /// How long archived versions are kept, and how much room they may take.
    pub retention: trs_sync::archive::Retention,
    /// Bytes fetched from peers for this folder.
    pub fetched: crate::limit::Meter,
    /// Bytes served to peers from this folder.
    pub served: crate::limit::Meter,
}

impl FolderShared {
    /// This device's role (`me`: its public key), if it is a member.
    pub fn role_of(&self, device_pk: &[u8; 32]) -> Option<Role> {
        self.membership.read().unwrap_or_else(|e| e.into_inner()).role(device_pk)
    }

    /// May this folder be synced with a peer over this connection (`lan`: the peer is on the LAN, not
    /// relayed)? Not when it is paused, nor a LAN-only folder with a peer elsewhere.
    pub fn allowed_with(&self, lan: bool) -> bool {
        !self.paused.load(Ordering::Relaxed) && (lan || !self.lan_only.load(Ordering::Relaxed))
    }
}

/// A folder runtime's state, as the snapshot shows it.
#[derive(Clone, Debug)]
pub struct Live {
    /// What it is doing.
    pub state: crate::status::FolderState,
    /// Paths that cannot be synced, with the reason (from the last scan and pull).
    pub issues: Vec<(String, String)>,
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
    /// The first needed paths in path order, at most [`PENDING_SAMPLE`], for Finder's badges; `true` when
    /// there are more. Taken where the core is locked anyway (before and during pulls, on settling).
    pub pending: (Vec<String>, bool),
    /// Files this device holds, and their size.
    pub local: (u64, u64),
    /// Bytes fetched and served, for rates.
    pub fetched: u64,
    /// Joining: an owner or admin refused this device's invitation, for this reason.
    pub refused: Option<String>,
    /// Devices of this folder this one cannot sync with (their Seedwell is too old or too new), and why. Shown
    /// with the issues; a device's entry goes when a session with it starts.
    pub incompatible: BTreeMap<DeviceId, String>,
}

impl Default for Live {
    fn default() -> Self {
        Live {
            state: crate::status::FolderState::Starting,
            issues: Vec::new(),
            error: None,
            last_synced_s: None,
            conflicts: BTreeMap::new(),
            need: (0, 0),
            pending: (Vec::new(), false),
            local: (0, 0),
            fetched: 0,
            refused: None,
            incompatible: BTreeMap::new(),
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
    /// The folders shared over this connection, with the access the peer proved.
    pub folders: RwLock<BTreeMap<FolderId, Access>>,
    /// Bytes of our requests not answered yet (to spread requests over peers).
    pub inflight: AtomicU64,
    /// Bytes received and sent over this connection.
    pub rx_bytes: AtomicU64,
    /// Bytes sent.
    pub tx_bytes: AtomicU64,
    /// The session ended.
    pub closed: watch::Sender<bool>,
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
            folders: RwLock::new(BTreeMap::new()),
            inflight: AtomicU64::new(0),
            rx_bytes: AtomicU64::new(0),
            tx_bytes: AtomicU64::new(0),
            closed,
        });
        (h, crx, drx)
    }

    /// The access the peer proved in `folder`, if it shares it with us.
    pub fn access(&self, folder: &FolderId) -> Option<Access> {
        self.folders.read().unwrap_or_else(|e| e.into_inner()).get(folder).copied()
    }

    /// Sends a request; the response arrives on the returned receiver (dropped if the session ends).
    pub async fn request(&self, mut req: Request) -> Option<oneshot::Receiver<Response>> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        req.id = id;
        let (tx, rx) = oneshot::channel();
        lock(&self.pending).insert(id, tx);
        if self.control.send(Message::Request(req)).await.is_err() {
            lock(&self.pending).remove(&id);
            return None;
        }
        Some(rx)
    }

    /// A response arrived.
    pub fn answer(&self, resp: Response) {
        if let Some(tx) = lock(&self.pending).remove(&resp.id) {
            let _ = tx.send(resp);
        }
    }

    /// The session is over: pending requests fail (their receivers see the sender dropped).
    pub fn close(&self) {
        lock(&self.pending).clear();
        let _ = self.closed.send(true);
    }

    /// Is the session over?
    pub fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }
}

/// Every folder and every connected device.
#[derive(Default)]
pub struct Registry {
    folders: RwLock<BTreeMap<FolderId, Arc<FolderShared>>>,
    peers: RwLock<BTreeMap<DeviceId, Arc<PeerHandle>>>,
}

impl Registry {
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
}
