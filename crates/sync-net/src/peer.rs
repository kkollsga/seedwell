//! One session with another device, after TLS: Hello, the folder proofs (Join) and, when both speak channels, the
//! channel proofs (ChatJoin), then index updates, chunk requests and channel messages both ways until either side
//! closes.
//!
//! Privacy of the Join: the device that dialed proves first, for the folders it believes the other device
//! shares (found through that folder's discovery). The other side answers only for folders the dialer
//! proved, so a stranger who connects learns nothing about which folders this device has.

use std::collections::BTreeSet;
use std::io;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_rustls::TlsStream;
use trs_sync::folder::RemoteIndex;
use trs_sync::fs::{Fs, StdFs};
use trs_sync_proto::ids::{DeviceId, FolderId, ShortId};
use trs_sync_proto::keys::Access;
use trs_sync_proto::model::FileKind;
use trs_sync_proto::msg::{
    Close, DirectJoin, Hello, Incompatible, Index, Join, JoinFolder, MAX_INDEX_ENTRIES, Members, Message, Request,
    Response, ResponseCode, agree,
};

use crate::framed::{Reader, Writer};
use crate::session::{BoxIo, Connected};
use crate::chat::{CheckedChat, check_chat_join, own_chat_join, start_chat_link};
use crate::shared::{Ctx, FolderShared, HubMsg, PeerHandle, Space, SpaceKind, lock};

type Rd = Reader<ReadHalf<TlsStream<BoxIo>>>;
type Wr = Writer<WriteHalf<TlsStream<BoxIo>>>;

/// How long the Hello and Join may take.
const SETUP_TIMEOUT: Duration = Duration::from_secs(20);
/// A quiet connection sends a Ping this often (and one with nothing at all for 3× this is dead).
const PING_EVERY: Duration = Duration::from_secs(90);
/// Bytes of file data being read for one peer's requests at a time.
const SERVE_BUDGET_KIB: usize = 32 * 1024;
/// About this much index per frame.
const INDEX_BATCH_BYTES: usize = 1 << 20;
/// Index frames received and waiting to be applied (beyond this the session stops reading).
const APPLY_QUEUE: usize = 8;

fn bad(why: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, why.into())
}

/// Runs a session to its end. `candidates` are the folders a dialer believes the peer shares.
pub async fn run(ctx: Arc<Ctx>, conn: Connected, candidates: Vec<FolderId>) -> io::Result<()> {
    let device = conn.device();
    if device == ctx.me {
        return Err(bad("connected to ourselves"));
    }
    let (est, remote, local, dialed, via) = (conn.est, conn.remote, conn.local, conn.dialed, conn.via);
    // LAN-only folders are shared only over a direct connection on the local network.
    let lan = via != crate::session::Via::Relay && crate::session::is_lan(remote.ip());
    let (rd, wr) = tokio::io::split(conn.stream);
    let mut reader = Reader::new(rd);
    let mut writer = Writer::new(wr);
    // Folders that open or close while this session is set up are checked again once it is up ([`reconcile`]). Read
    // before the folders to offer are, so one opened since is either offered or seen to have moved it.
    let at_gen = ctx.folder_gen.load(Ordering::SeqCst);
    // (What the caller asked for is what a refusal is noted against: all joining folders for an incoming session.)
    let asked = candidates.clone();
    let mut candidates = candidates;
    for f in ctx.registry.spaces() {
        if f.members.read().unwrap_or_else(|e| e.into_inner()).contains(&device) && !candidates.contains(&f.id) {
            candidates.push(f.id);
        }
    }

    let setup = async {
        let name = lock(&ctx.device_name).clone();
        let (min_protocol, protocol) = ctx.protocol;
        let mine = Hello { protocol, client: ctx.client.clone(), device_name: name, min_protocol, features: ctx.features };
        writer.send(&Message::Hello(mine.clone())).await?;
        let hello = match reader.next().await? {
            Some(Message::Hello(h)) => h,
            _ => return Err(bad("expected Hello")),
        };
        // Both sides work this out from the two Hellos, so each can say why.
        let agreed = match agree(&mine, &hello) {
            Ok(a) => a,
            Err(e) => {
                let reason = version_reason(e, &hello.device_name);
                let _ = writer.send(&Message::Close(Close { reason: reason.clone() })).await;
                return Err(bad(format!("{VERSION_REFUSED}{reason}")));
            }
        };
        let chat = agreed.features & trs_sync_proto::msg::FEATURE_CHAT != 0;
        // A conversation may stand for a session only when no other device relays it.
        let direct = agreed.features & trs_sync_proto::msg::FEATURE_DIRECT != 0;
        let conversation = direct && via != crate::session::Via::Relay && ctx.direct.has(&est.peer_pk);
        let user_name = lock(&ctx.user_name).clone();
        let mut sent: BTreeSet<FolderId> = BTreeSet::new();
        let mut checked;
        if dialed {
            let join = own_join(&ctx, &candidates, &est.exporter, device, lan).await;
            sent.extend(join.folders.iter().map(|f| f.folder_id));
            writer.send(&Message::Join(join)).await?;
            if chat {
                let cj = own_chat_join(&ctx, &candidates, &est.exporter);
                sent.extend(cj.channels.iter().map(|c| c.channel_id));
                writer.send(&Message::ChatJoin(cj)).await?;
            }
            if direct {
                writer.send(&Message::DirectJoin(DirectJoin { conversation, user_name: user_name.clone() })).await?;
            }
            match reader.next().await? {
                Some(Message::Join(j)) => checked = check_join(&ctx, &j, &est, &sent, lan),
                Some(Message::Close(c)) => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, c.reason)),
                _ => return Err(bad("expected Join")),
            }
            if chat {
                match reader.next().await? {
                    Some(Message::ChatJoin(j)) => checked.add_chat(check_chat_join(&ctx, &j, &est.peer_pk, &est.exporter, &sent)),
                    Some(Message::Close(c)) => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, c.reason)),
                    _ => return Err(bad("expected ChatJoin")),
                }
            }
            if direct {
                match reader.next().await? {
                    Some(Message::DirectJoin(j)) => {
                        checked.conversation = conversation && j.conversation;
                        checked.user_name = j.user_name;
                    }
                    Some(Message::Close(c)) => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, c.reason)),
                    _ => return Err(bad("expected DirectJoin")),
                }
            }
        } else {
            let proved = match reader.next().await? {
                Some(Message::Join(j)) => check_join(&ctx, &j, &est, &BTreeSet::new(), lan),
                _ => return Err(bad("expected Join")),
            };
            checked = proved;
            if chat {
                match reader.next().await? {
                    Some(Message::ChatJoin(j)) => checked.add_chat(check_chat_join(&ctx, &j, &est.peer_pk, &est.exporter, &BTreeSet::new())),
                    _ => return Err(bad("expected ChatJoin")),
                }
            }
            if direct {
                match reader.next().await? {
                    // Ours is said only to a dialer that said it keeps the conversation too.
                    Some(Message::DirectJoin(j)) => {
                        checked.conversation = conversation && j.conversation;
                        checked.user_name = j.user_name;
                    }
                    _ => return Err(bad("expected DirectJoin")),
                }
            }
            if checked.is_empty() {
                // Nothing to share: say why instead of answering (a refused invitation is shown there).
                let reason = close_reason(&checked);
                let _ = writer.send(&Message::Close(Close { reason: reason.clone() })).await;
                return Err(bad(reason));
            }
            let ids: Vec<FolderId> = checked.shared.iter().map(|(f, _, _)| *f).collect();
            let join = own_join(&ctx, &ids, &est.exporter, device, lan).await;
            // (Tests: the answer is made, the session not up yet.)
            let wait = *lock(&ctx.setup_wait);
            tokio::time::sleep(wait).await;
            sent.extend(join.folders.iter().map(|f| f.folder_id));
            writer.send(&Message::Join(join)).await?;
            if chat {
                let ids: Vec<FolderId> = checked.channels.iter().map(|(c, _)| *c).collect();
                let cj = own_chat_join(&ctx, &ids, &est.exporter);
                sent.extend(cj.channels.iter().map(|c| c.channel_id));
                writer.send(&Message::ChatJoin(cj)).await?;
            }
            if direct {
                writer.send(&Message::DirectJoin(DirectJoin { conversation: checked.conversation, user_name })).await?;
            }
        }
        Ok::<_, io::Error>((hello, agreed, checked, sent))
    };
    let setup = tokio::time::timeout(SETUP_TIMEOUT, setup).await.map_err(|_| bad("setup timed out"))?;
    let (hello, agreed, checked, sent) = setup.inspect_err(|e| note_refusal(&ctx, &asked, device, e))?;
    // The two Seedwells talk: a version problem with this device is over.
    for f in ctx.registry.spaces() {
        lock(&f.notes).incompatible.remove(&device);
    }
    for id in &checked.changed {
        if let Some(f) = ctx.registry.space(id) {
            let ctx = ctx.clone();
            tokio::spawn(async move { crate::roles::changed(&ctx, &f).await });
        }
    }
    if checked.is_empty() {
        // A refused invitation is told to the joining device, which shows why.
        let reason = close_reason(&checked);
        let _ = writer.send(&Message::Close(Close { reason: reason.clone() })).await;
        return Err(bad(reason));
    }
    let shared = checked.shared;
    let channels = checked.channels;

    let (peer, crx, drx) = PeerHandle::new(est.peer_pk, hello.device_name, hello.client, agreed, remote, local, est.exporter, dialed, via);
    *lock(&peer.user_name) = checked.user_name;
    let old = ctx.plays_0_1_11();
    // What the two Joins covered: a folder is linked only if it is in both.
    lock(&peer.joined).extend(sent.iter().copied());
    lock(&peer.got).extend(checked.named.iter().copied());
    if !old {
        lock(&peer.stale).extend(checked.closed);
    }
    if !ctx.registry.add_peer(&ctx.me, peer.clone()) {
        let _ = writer.send(&Message::Close(Close { reason: "already connected".into() })).await;
        return Ok(());
    }
    let _ = ctx.hub.send(HubMsg::PeerUp(device));
    ctx.sessions.fetch_add(1, Ordering::SeqCst);

    let writer_task = tokio::spawn(write_loop(writer, crx, drx, peer.clone()));
    let serve = Arc::new(Semaphore::new(SERVE_BUDGET_KIB));
    let mut links = Vec::new();
    for (folder, access, their) in shared {
        // Offered by the peer, but left out of our Join (it closed while that was made): stale, never linked.
        if !old && !sent.contains(&folder) {
            lock(&peer.stale).insert(folder);
            continue;
        }
        links.push(start_link(&ctx, &peer, folder, access, their));
    }
    // Every folder that opened or closed since setup began is as it should be, or the session starts anew.
    if !old && ctx.folder_gen.load(Ordering::SeqCst) != at_gen {
        reconcile(&ctx, &peer).await;
    }
    for (channel, access) in channels {
        links.push(start_chat_link(&ctx, &peer, channel, access));
    }
    // Private messages, over a direct session only: our key, and what we hold.
    links.push({
        let (ctx, peer) = (ctx.clone(), peer.clone());
        tokio::spawn(async move { crate::direct::start(&ctx, &peer).await })
    });
    offer_joining(&ctx, &peer, &sent).await;
    let result = read_loop(&ctx, &peer, &mut reader, &serve, &est, sent, &mut links).await;
    if let Err(e) = &result {
        note_refusal(&ctx, &asked, device, e);
    }

    peer.close();
    ctx.registry.remove_peer(&peer);
    for l in links {
        l.abort();
    }
    writer_task.abort();
    let _ = ctx.hub.send(HubMsg::PeerDown(device));
    result
}

/// Offers the folders and channels this device is joining that the handshake left out (the session was opened for
/// another one, as when a link for a folder and its channel is used), as when one is opened while connected.
async fn offer_joining(ctx: &Ctx, peer: &PeerHandle, sent: &BTreeSet<FolderId>) {
    for f in ctx.registry.spaces() {
        if sent.contains(&f.id) || lock(&peer.joined).contains(&f.id) || lock(&f.invite).is_none() || f.role_of(ctx.key.public_key()).is_some() {
            continue;
        }
        match f.kind {
            SpaceKind::Folder => {
                let join = own_join(ctx, &[f.id], &peer.exporter, peer.device, peer.lan).await;
                if !join.folders.is_empty() {
                    lock(&peer.joined).insert(f.id);
                    lock(&peer.offered).insert(f.id);
                    let _ = peer.control.send(Message::Join(join)).await;
                }
            }
            SpaceKind::Channel if peer.chat() => {
                let join = own_chat_join(ctx, &[f.id], &peer.exporter);
                if !join.channels.is_empty() {
                    let _ = peer.control.send(Message::ChatJoin(join)).await;
                }
            }
            SpaceKind::Channel => {}
        }
    }
}

/// The start of a Close reason that tells a joining device its invitation was refused.
const LINK_REFUSED: &str = "link: ";
/// The start of the error of a session refused because the two Seedwells cannot talk.
const VERSION_REFUSED: &str = "version: ";

/// Why two devices whose protocol versions do not overlap cannot sync, for the person at this one.
fn version_reason(e: Incompatible, peer_name: &str) -> String {
    let who = if peer_name.is_empty() { "The other device" } else { peer_name };
    match e {
        Incompatible::PeerTooOld => format!("{who} runs an older Seedwell that cannot sync with this one: update it"),
        Incompatible::PeerTooNew => format!(concat!("{who} runs a newer Seedwell that this one cannot sync with: update this ", trs_platform::computer!()), who = who),
    }
}

/// Why a session with nothing in common ends: an invitation's refusal, told to the joining device.
fn close_reason(c: &Checked) -> String {
    // (A channel's refusal has the same form.)
    c.refused.first().map_or_else(|| "no folders in common".to_string(), |r| format!("{LINK_REFUSED}{r}"))
}

/// A session ended with an owner's or admin's refusal of our invitation, or because the two Seedwells cannot
/// talk: the joining folders show why, and a folder the device is a member of lists it with its issues.
fn note_refusal(ctx: &Ctx, candidates: &[FolderId], device: DeviceId, e: &io::Error) {
    let text = e.to_string();
    if let Some(reason) = text.strip_prefix(VERSION_REFUSED) {
        // The folders and channels we dialed it for, and those it was seen in (it may have dialed us).
        for f in ctx.registry.spaces() {
            let seen = f.members.read().unwrap_or_else(|e| e.into_inner()).contains(&device);
            // A joining folder shows it as the reason it waits (below), not as an issue.
            if (seen || candidates.contains(&f.id)) && lock(&f.invite).is_none() {
                lock(&f.notes).incompatible.insert(device, reason.to_string());
            }
        }
    }
    // A version refusal only for the folders we dialed for (any device may connect to us).
    let version = text.strip_prefix(VERSION_REFUSED).filter(|_| !candidates.is_empty());
    let Some(reason) = text.strip_prefix(LINK_REFUSED).or(version) else { return };
    let spaces: Vec<Arc<Space>> = if candidates.is_empty() {
        ctx.registry.spaces()
    } else {
        candidates.iter().filter_map(|id| ctx.registry.space(id)).collect()
    };
    for f in spaces.iter().filter(|f| lock(&f.invite).is_some()) {
        lock(&f.notes).refused = Some(reason.to_string());
    }
}

/// Our Join for these folders (those we are a member of, or join with an invitation), with our proofs bound to
/// the session with this exporter value.
pub(crate) async fn own_join(ctx: &Ctx, folders: &[FolderId], exporter: &[u8; 32], peer: DeviceId, lan: bool) -> Join {
    let mut out = Vec::new();
    for id in folders {
        let Some(f) = ctx.registry.folder(id) else { continue };
        if !f.allowed_with(lan) {
            continue;
        }
        let f2 = f.clone();
        let state = tokio::task::spawn_blocking(move || {
            let core = lock(&f2.core);
            (core.index_state().ok(), core.peer_state(peer.short()).ok())
        })
        .await
        .ok();
        let Some((Some((my_index_id, _)), Some(theirs))) = state else { continue };
        let Some(proofs) = crate::roles::proofs(ctx, &f, exporter) else { continue };
        out.push(JoinFolder {
            folder_id: f.id,
            proofs,
            my_index_id,
            your_index_id: theirs.index_id,
            your_max_seq: theirs.max_seq,
        });
    }
    Join { folders: out }
}

/// What a peer's Join (and ChatJoin) proved.
struct Checked {
    /// (folder, its access, its Join entry).
    shared: Vec<(FolderId, Access, JoinFolder)>,
    /// (channel, its access).
    channels: Vec<(FolderId, Access)>,
    /// Invitations this device refused, and why.
    refused: Vec<String>,
    /// Folders and channels whose membership changed (its grants were new to us, or we admitted it).
    changed: Vec<FolderId>,
    /// Both sides keep a private conversation with each other (and no device relays the session).
    conversation: bool,
    /// The peer's user's name, from its DirectJoin.
    user_name: String,
    /// Folders the peer offered that are not open here (paused, stopped, LAN-only, not here at all): stale on this
    /// session, so opening one starts the session anew (and it links then).
    closed: Vec<FolderId>,
    /// Every folder the peer's Join named (that this check looked at).
    named: Vec<FolderId>,
}

impl Checked {
    fn add_chat(&mut self, c: CheckedChat) {
        self.channels.extend(c.shared);
        self.refused.extend(c.refused);
        self.changed.extend(c.changed);
    }

    /// Nothing in common.
    fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.channels.is_empty() && !self.conversation
    }
}

/// What the peer proved, folder by folder ([`crate::roles::check`]); a device redeeming an invitation is admitted
/// when this device is an owner or admin. `answered_only` (non-empty for a dialer) limits the answer to folders
/// we asked about.
fn check_join(ctx: &Ctx, j: &Join, est: &crate::tls::Established, answered_only: &BTreeSet<FolderId>, lan: bool) -> Checked {
    let mut out = Checked {
        shared: Vec::new(),
        channels: Vec::new(),
        refused: Vec::new(),
        changed: Vec::new(),
        conversation: false,
        user_name: String::new(),
        closed: Vec::new(),
        named: Vec::new(),
    };
    for jf in &j.folders {
        if !answered_only.is_empty() && !answered_only.contains(&jf.folder_id) {
            continue;
        }
        out.named.push(jf.folder_id);
        // Ours but not open here (paused, LAN-only, stopped, held back, failed to open): stale. One this device does
        // not have is nothing.
        let Some(f) = ctx.registry.folder(&jf.folder_id).filter(|f| f.allowed_with(lan)) else {
            if ctx.registry.folder(&jf.folder_id).is_some() || lock(&ctx.ours).contains(&jf.folder_id) {
                out.closed.push(jf.folder_id);
            }
            continue;
        };
        let mut changed = false;
        match crate::roles::check(ctx, &f, &est.peer_pk, &est.exporter, &jf.proofs, &mut changed) {
            crate::roles::Proved::Member(a) => out.shared.push((jf.folder_id, a, jf.clone())),
            crate::roles::Proved::Refused(why) => out.refused.push(why),
            crate::roles::Proved::Nothing => {}
        }
        if changed {
            out.changed.push(jf.folder_id);
        }
    }
    out
}

/// Starts sending our index of `folder` to the peer: everything new since what it holds of it, then
/// every change as it happens.
pub(crate) fn start_link(
    ctx: &Arc<Ctx>,
    peer: &Arc<PeerHandle>,
    folder: FolderId,
    access: Access,
    their: JoinFolder,
) -> tokio::task::JoinHandle<()> {
    peer.folders.write().unwrap_or_else(|e| e.into_inner()).insert(folder, access);
    let (ctx2, peer2) = (ctx.clone(), peer.clone());
    let task = tokio::spawn(async move {
        let (ctx, peer) = (ctx2, peer2);
        let Some(f) = ctx.registry.folder(&folder) else { return };
        f.members.write().unwrap_or_else(|e| e.into_inner()).insert(peer.device);
        let _ = ctx.hub.send(HubMsg::MemberSeen { folder, device: peer.device });
        if let Err(e) = send_index(&f, &peer, their).await {
            let _ = peer.control.send(Message::Close(Close { reason: format!("index: {e}") })).await;
        }
    });
    // One sender per folder: one linked again replaces the one before.
    if let Some(old) = peer.links.lock().unwrap_or_else(|e| e.into_inner()).insert(folder, task.abort_handle()) {
        old.abort();
    }
    // Closed meanwhile (its check passed, then it was paused or stopped)? A closer sets its flag before it looks at
    // the sessions, and this link is in them now: either it saw this link, or this sees its flag. Stale either way.
    if !ctx.plays_0_1_11() && ctx.registry.folder(&folder).filter(|f| f.allowed_with(peer.lan)).is_none() {
        peer.close_folder(&folder);
    }
    task
}

async fn send_index(f: &Arc<FolderShared>, peer: &Arc<PeerHandle>, their: JoinFolder) -> io::Result<()> {
    // The membership first: the peer checks every entry against the writers it gives.
    let statements: Vec<_> = f.membership.read().unwrap_or_else(|e| e.into_inner()).statements().cloned().collect();
    if peer.control.send(Message::Members(Members { folder_id: f.id, statements })).await.is_err() {
        return Ok(());
    }
    let f2 = f.clone();
    let (index_id, _) = tokio::task::spawn_blocking(move || lock(&f2.core).index_state()).await.map_err(io::Error::other)??;

    // What the peer holds of our index decides between changes only and a full send.
    let (mut full, mut sent) = if their.your_index_id == index_id { (false, their.your_max_seq) } else { (true, 0) };
    let mut changes = f.local_seq.subscribe();
    let mut stopped = f.stopped.subscribe();
    loop {
        if *stopped.borrow() {
            return Ok(());
        }
        loop {
            let f2 = f.clone();
            let batch = tokio::task::spawn_blocking(move || lock(&f2.core).announce_since(sent, MAX_INDEX_ENTRIES))
                .await
                .map_err(io::Error::other)??;
            let last_batch = batch.len() < MAX_INDEX_ENTRIES;
            for part in split_by_size(batch) {
                let next = part.last().map_or(sent, |e| e.sequence);
                let msg = Message::Index(Index { folder_id: f.id, index_id, full, entries: part, done: false });
                if peer.control.send(msg).await.is_err() {
                    return Ok(());
                }
                sent = next;
            }
            if last_batch {
                if full {
                    // An empty final batch marks the end of the full send.
                    let end = Message::Index(Index { folder_id: f.id, index_id, full: true, entries: vec![], done: true });
                    if peer.control.send(end).await.is_err() {
                        return Ok(());
                    }
                    full = false;
                }
                break;
            }
        }
        // Wait for our next local change (or the end of the session).
        let mut closed = peer.closed.subscribe();
        tokio::select! {
            r = changes.changed() => if r.is_err() { return Ok(()) },
            _ = closed.wait_for(|c| *c) => return Ok(()),
            // The folder is stopping here: let go of it (the peer stays connected for the others).
            _ = stopped.wait_for(|s| *s) => return Ok(()),
        }
    }
}

/// Splits a batch into frames of about [`INDEX_BATCH_BYTES`] (a big file's chunk list alone can be
/// megabytes).
fn split_by_size(entries: Vec<trs_sync_proto::model::IndexEntry>) -> Vec<Vec<trs_sync_proto::model::IndexEntry>> {
    let mut out = Vec::new();
    let mut cur = Vec::new();
    let mut bytes = 0;
    for e in entries {
        let size = 200 + e.file.info.path.len() + e.file.info.chunks.len() * 36;
        if !cur.is_empty() && bytes + size > INDEX_BATCH_BYTES {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += size;
        cur.push(e);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

async fn write_loop(
    mut w: Wr,
    mut control: mpsc::Receiver<Message>,
    mut data: mpsc::Receiver<Message>,
    peer: Arc<PeerHandle>,
) {
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    loop {
        let msg = tokio::select! {
            biased;
            m = control.recv() => m,
            m = data.recv() => m,
            _ = ping.tick() => Some(Message::Ping),
        };
        let Some(msg) = msg else { break };
        let close = matches!(msg, Message::Close(_));
        let size = match &msg {
            Message::Response(r) => r.data.len() as u64,
            _ => 0,
        };
        if w.send(&msg).await.is_err() {
            break;
        }
        peer.tx_bytes.fetch_add(size, Ordering::Relaxed);
        if close {
            break;
        }
    }
    let _ = w.shutdown().await;
    peer.close();
}

async fn read_loop(
    ctx: &Arc<Ctx>,
    peer: &Arc<PeerHandle>,
    reader: &mut Rd,
    serve: &Arc<Semaphore>,
    est: &crate::tls::Established,
    mut sent: BTreeSet<FolderId>,
    links: &mut Vec<tokio::task::JoinHandle<()>>,
) -> io::Result<()> {
    let mut closed = peer.closed.subscribe();
    let (apply, queue) = mpsc::channel(APPLY_QUEUE);
    let (failed_tx, mut failed) = oneshot::channel();
    tokio::spawn(apply_loop(ctx.clone(), peer.clone(), queue, failed_tx));
    let applier_gone = || io::Error::other("the index applier stopped");
    loop {
        let next = tokio::select! {
            m = tokio::time::timeout(PING_EVERY * 3, reader.next()) => m,
            _ = closed.wait_for(|c| *c) => return Ok(()),
            e = &mut failed => return Err(e.unwrap_or_else(|_| applier_gone())),
        };
        let msg = match next {
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "the peer went quiet")),
            Ok(m) => m?,
        };
        let Some(msg) = msg else { return Ok(()) };
        match msg {
            Message::Members(m) => {
                let Some(f) = shared_space(ctx, peer, &m.folder_id) else { continue };
                apply.send(Apply::Members(f, m.statements)).await.map_err(|_| applier_gone())?;
            }
            Message::OwnerKey(k) => {
                // In order with the membership that makes this device an owner.
                let Some(f) = shared_space(ctx, peer, &k.folder_id) else { continue };
                apply.send(Apply::OwnerKey(f, peer.device_pk, k.secret)).await.map_err(|_| applier_gone())?;
            }
            Message::ChatMessages(m) if peer.chat() => {
                // In order with the membership that says who may post.
                let Some(c) = shared_channel(ctx, peer, &m.channel_id) else { continue };
                apply.send(Apply::Chat(c, m.messages)).await.map_err(|_| applier_gone())?;
            }
            Message::ChatHave(h) if peer.chat() => {
                let Some(c) = shared_channel(ctx, peer, &h.channel_id) else { continue };
                let (ctx2, peer2) = (ctx.clone(), peer.clone());
                tokio::spawn(async move { crate::chat::on_have(&ctx2, &peer2, &c, h).await });
            }
            Message::DirectKeys(k) if peer.chat() => crate::direct::on_keys(peer, k.records),
            Message::DirectJoin(j) => crate::direct::on_join(ctx, peer, j),
            Message::DirectHave(h) if peer.chat() => {
                let (ctx2, peer2) = (ctx.clone(), peer.clone());
                tokio::spawn(async move { crate::direct::on_have(&ctx2, &peer2, h).await });
            }
            Message::DirectMessages(m) if peer.chat() => {
                // In order with the membership that says who shares a channel.
                apply.send(Apply::Direct(m.messages)).await.map_err(|_| applier_gone())?;
            }
            Message::ChatJoin(j) if peer.chat() => {
                // A channel joined while connected.
                let checked = check_chat_join(ctx, &j, &est.peer_pk, &est.exporter, &BTreeSet::new());
                for id in &checked.changed {
                    if let Some(f) = ctx.registry.space(id) {
                        crate::roles::changed(ctx, &f).await;
                    }
                }
                for (channel, access) in checked.shared {
                    if peer.access(&channel).is_some() {
                        continue;
                    }
                    if !sent.contains(&channel) {
                        let cj = own_chat_join(ctx, &[channel], &est.exporter);
                        sent.extend(cj.channels.iter().map(|c| c.channel_id));
                        let _ = peer.control.send(Message::ChatJoin(cj)).await;
                    }
                    links.push(start_chat_link(ctx, peer, channel, access));
                }
            }
            Message::Index(idx) => {
                // (Nothing of a folder stale on this session: it is sent again on the next one.)
                let Some(f) = shared_folder(ctx, peer, &idx.folder_id) else { continue };
                let ri = RemoteIndex { index_id: idx.index_id, full: idx.full, done: idx.done, entries: idx.entries };
                apply.send(Apply::Index(f, ri)).await.map_err(|_| applier_gone())?;
            }
            Message::Request(req) => {
                let Some(f) = shared_folder(ctx, peer, &req.folder_id) else {
                    let _ = peer.data.send(Message::Response(Response { id: req.id, code: ResponseCode::Invalid, data: vec![] })).await;
                    continue;
                };
                let kib = (req.len as usize).div_ceil(1024).clamp(1, SERVE_BUDGET_KIB) as u32;
                let permit = serve.clone().acquire_many_owned(kib).await.map_err(io::Error::other)?;
                let (peer2, ctx2) = (peer.clone(), ctx.clone());
                tokio::spawn(async move {
                    let id = req.id;
                    let f2 = f.clone();
                    let resp = tokio::task::spawn_blocking(move || serve_chunk(&f2, &req))
                        .await
                        .unwrap_or(Response { id, code: ResponseCode::Generic, data: vec![] });
                    let n = resp.data.len() as u64;
                    if !peer2.lan {
                        ctx2.limits.up.take(n).await;
                    }
                    f.served.add(n);
                    let _ = peer2.data.send(Message::Response(resp)).await;
                    drop(permit);
                });
            }
            Message::Response(r) => {
                peer.rx_bytes.fetch_add(r.data.len() as u64, Ordering::Relaxed);
                peer.answer(r);
            }
            // (A build without the feature does not know the type: it skips it.)
            Message::HoldQuery(_) if ctx.silent_ondemand.load(Ordering::SeqCst) || ctx.features & trs_sync_proto::msg::FEATURE_HOLDS == 0 => {}
            Message::HoldQuery(q) => {
                // Answered from our index as it is now, through its read-only handle, so a pull that holds the folder's
                // core never makes the asker wait (a folder not shared with the peer, gone, paused or missing holds
                // nothing for it).
                let f = shared_folder(ctx, peer, &q.folder_id).filter(|f| {
                    let running = !matches!(
                        lock(&f.live).state,
                        crate::status::FolderState::Paused | crate::status::FolderState::Missing | crate::status::FolderState::Error
                    );
                    running && !f.paused.load(Ordering::Relaxed)
                });
                let (peer2, ctx2) = (peer.clone(), ctx.clone());
                let lie = ctx.yes_to_holds.load(Ordering::SeqCst);
                tokio::spawn(async move {
                    let n = q.files.len();
                    let asker = peer2.device.short();
                    let me = ctx2.me.short();
                    let writer = f.as_ref().is_some_and(|f| f.role_of(ctx2.key.public_key()).is_some_and(|r| r.access() == Access::Write));
                    let held = match f {
                        Some(f) => tokio::task::spawn_blocking(move || {
                            let (view, fs) = (f.index.view(), StdFs::new(f.root.clone()));
                            let freeing = |p: &str| lock(&f.freeing).contains_key(p);
                            let holder = trs_sync::ondemand::Holder { store: &view, fs: &fs, me, writer, on_demand: f.on_demand, freeing: &freeing, ignore: &f.ignore };
                            holder.answer(asker, &q.files)
                        })
                        .await
                        .unwrap_or_else(|_| vec![false; n]),
                        None => vec![false; n],
                    };
                    let held = if lie { vec![true; n] } else { held };
                    let _ = peer2.control.send(Message::HoldReply(trs_sync_proto::msg::HoldReply { id: q.id, held })).await;
                });
            }
            Message::HoldReply(r) => peer.answer_holds(r),
            Message::RenameRequest(_) if ctx.silent_ondemand.load(Ordering::SeqCst) || ctx.features & trs_sync_proto::msg::FEATURE_RENAMES == 0 => {}
            Message::RenameRequest(q) => {
                // Made here as this device's own rename, only for a writer of a folder it shares with us that runs here,
                // and only if it can start within the request's time to live (counted from now); answered either way.
                let by = std::time::Instant::now() + Duration::from_millis(u64::from(q.ttl_ms).min(MAX_RENAME_TTL_MS));
                let f = shared_folder(ctx, peer, &q.folder_id).filter(|f| {
                    !matches!(
                        lock(&f.live).state,
                        crate::status::FolderState::Paused | crate::status::FolderState::Missing | crate::status::FolderState::Error
                    )
                });
                let writer = f.as_ref().is_some_and(|f| f.role_of(&peer.device_pk).is_some_and(|r| r.access() == Access::Write));
                let peer2 = peer.clone();
                tokio::spawn(async move {
                    use trs_sync_proto::msg::RenameOutcome;
                    let (outcome, reason) = match f {
                        Some(f) if writer => rename_for_peer(f, q.from, q.to, q.seen, by).await,
                        Some(_) => (RenameOutcome::Refused, "only a member that can write may rename".to_string()),
                        None => (RenameOutcome::Refused, "the folder is not running here".to_string()),
                    };
                    let mut reply = trs_sync_proto::msg::RenameReply { id: q.id, outcome, reason };
                    reply.reason.truncate(reply.reason.floor_char_boundary(trs_sync_proto::msg::MAX_RENAME_REASON));
                    let _ = peer2.control.send(Message::RenameReply(reply)).await;
                });
            }
            Message::RenameReply(r) => peer.answer_rename(r),
            Message::Join(j) => {
                // A folder added while connected (a refused invitation is not worth ending the session over).
                let at_gen = ctx.folder_gen.load(Ordering::SeqCst);
                let old = ctx.plays_0_1_11();
                let checked = check_join(ctx, &j, est, &BTreeSet::new(), peer.lan);
                lock(&peer.got).extend(checked.named.iter().copied());
                if !old {
                    lock(&peer.stale).extend(checked.closed.iter().copied());
                }
                for id in &checked.changed {
                    if let Some(f) = ctx.registry.space(id) {
                        crate::roles::changed(ctx, &f).await;
                    }
                }
                for (folder, access, their) in checked.shared {
                    if old {
                        // As 0.1.11: linked already, ignored; answered unless it named the folder itself before.
                        if peer.access(&folder).is_some() {
                            continue;
                        }
                        if !sent.contains(&folder) {
                            let join = own_join(ctx, &[folder], &est.exporter, peer.device, peer.lan).await;
                            sent.extend(join.folders.iter().map(|f| f.folder_id));
                            let _ = peer.control.send(Message::Join(join)).await;
                        }
                        links.push(start_link(ctx, peer, folder, access, their));
                        continue;
                    }
                    // Stale on this session (it closed here: the session starts anew once it is open).
                    if peer.is_stale(&folder) {
                        continue;
                    }
                    // Linked already: the peer opened it again (moved it, added it again: an earlier build says so
                    // with a Join on the same session). What it dropped meanwhile it takes again only from a new
                    // session's Join: this one starts anew.
                    if peer.access(&folder).is_some() {
                        // (Unless it is the peer's reply to our answer.)
                        if lock(&peer.answered).remove(&folder) {
                            continue;
                        }
                        let reason = "the peer opened a folder again: starting the session anew".to_string();
                        let _ = peer.control.send(Message::Close(Close { reason })).await;
                        continue;
                    }
                    // Answered, and the answer noted (the peer's reply to it is no reopening). A new build answers
                    // every Join for a folder it has not linked, so an answer to our own offer is answered too, and
                    // its reply to that is noted the same way. An earlier build does not reply to the answer of an
                    // offer of ours: its Join for one we offered is that answer.
                    let earlier = !peer.has(trs_sync_proto::msg::FEATURE_HOLDS);
                    let ours_first = lock(&peer.offered).remove(&folder) && earlier;
                    if !ours_first {
                        let join = own_join(ctx, &[folder], &est.exporter, peer.device, peer.lan).await;
                        if join.folders.iter().any(|f| f.folder_id == folder) {
                            lock(&peer.joined).insert(folder);
                            lock(&peer.answered).insert(folder);
                            let _ = peer.control.send(Message::Join(join)).await;
                        }
                    }
                    // Linked only if it went out in our Join on this session; else (it closed meanwhile) stale.
                    if lock(&peer.joined).contains(&folder) {
                        links.push(start_link(ctx, peer, folder, access, their));
                    } else {
                        lock(&peer.stale).insert(folder);
                    }
                }
                // A folder opened or closed while this Join was handled: the session's folders are checked again.
                if !old && ctx.folder_gen.load(Ordering::SeqCst) != at_gen {
                    reconcile(ctx, peer).await;
                }
            }
            Message::Addresses(a) => {
                let _ = ctx.hub.send(HubMsg::Addresses(a.records));
            }
            Message::Close(c) => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, c.reason)),
            Message::Hello(_) => return Err(bad("a second Hello")),
            // Channel messages over a session without the chat feature are not this version's to read.
            Message::Ping
            | Message::Progress(_)
            | Message::Unknown(_)
            | Message::ChatJoin(_)
            | Message::ChatHave(_)
            | Message::ChatMessages(_)
            | Message::DirectKeys(_)
            | Message::DirectHave(_)
            | Message::DirectMessages(_) => {}
            Message::Rendezvous(_) | Message::Introduce(_) | Message::RelayOpen(_) | Message::RelayData(_) | Message::RelayClose(_) => {
                let _ = ctx.hub.send(HubMsg::Relay(peer.device, Box::new(msg)));
            }
        }
    }
}

/// What the peer sent that changes a membership, a folder's core or a channel's messages.
enum Apply {
    Members(Arc<Space>, Vec<trs_sync_proto::members::Statement>),
    /// The folder (or channel) secret from an owner (its key), for this device as a new co-owner.
    OwnerKey(Arc<Space>, [u8; 32], [u8; 32]),
    Index(Arc<FolderShared>, RemoteIndex),
    Chat(Arc<crate::chat::ChannelShared>, Vec<trs_sync_proto::chat::ChatMessage>),
    Direct(Vec<trs_sync_proto::direct::DirectMessage>),
}

/// Applies the peer's membership and index in the order they came, off the read loop: applying waits for the
/// folder's core, which a pull may hold while it waits for this very peer's answers. The first that fails
/// ends the session with its reason.
async fn apply_loop(ctx: Arc<Ctx>, peer: Arc<PeerHandle>, mut queue: mpsc::Receiver<Apply>, failed: oneshot::Sender<io::Error>) {
    let who: ShortId = peer.device.short();
    while let Some(a) = queue.recv().await {
        let a = match a {
            Apply::Members(f, statements) => {
                // Statements a member cannot have made are skipped, not fatal (an older view of the folder).
                if crate::roles::add(&f, statements) {
                    crate::roles::changed(&ctx, &f).await;
                }
                continue;
            }
            Apply::OwnerKey(f, from, secret) => {
                crate::roles::owner_key(&ctx, &f, &from, secret).await;
                continue;
            }
            Apply::Chat(c, msgs) => {
                crate::chat::on_messages(&ctx, peer.device, &c, msgs).await;
                continue;
            }
            Apply::Direct(msgs) => {
                crate::direct::on_messages(&ctx, &peer, msgs).await;
                continue;
            }
            // A session that ended takes nothing more of its peer's index: the next one says what we hold, and the
            // peer sends from there (a late batch of this one would land after that one's).
            Apply::Index(..) if peer.is_closed() => continue,
            a => a,
        };
        let session = peer.clone();
        let res = tokio::task::spawn_blocking(move || match a {
            Apply::Members(..) | Apply::OwnerKey(..) | Apply::Chat(..) | Apply::Direct(..) => Ok::<(), String>(()),
            Apply::Index(f, ri) => {
                let done = ri.done;
                let mut core = lock(&f.core);
                // Checked again under the core's lock: the session may have ended while this waited for it (a new
                // one's batches may be in already), or the folder gone stale on it.
                if session.is_closed() || session.is_stale(&f.id) {
                    return Ok(());
                }
                let stored = core.apply_remote(who, ri).map_err(|e| e.to_string())?;
                drop(core);
                if stored > 0 || done {
                    f.remote_changed.notify_one();
                }
                Ok(())
            }
        })
        .await;
        let e = match res {
            Ok(Ok(())) => continue,
            Ok(Err(why)) => bad(why),
            Err(e) => io::Error::other(e),
        };
        let _ = failed.send(e);
        return;
    }
}

/// A folder or channel this session shares with the peer (and is not paused).
fn shared_space(ctx: &Ctx, peer: &PeerHandle, id: &FolderId) -> Option<Arc<Space>> {
    peer.access(id)?;
    ctx.registry.space(id).filter(|f| f.allowed_with(peer.lan))
}

/// A channel this session shares with the peer.
fn shared_channel(ctx: &Ctx, peer: &PeerHandle, id: &FolderId) -> Option<Arc<crate::chat::ChannelShared>> {
    peer.access(id)?;
    ctx.registry.channel(id)
}

/// A folder this session shares with the peer (and is not paused).
/// Renames `from` to `to` for a writer that asked ([`trs_sync::folder::FolderCore::rename_for_peer`]), on the folder's
/// core, and tells the sessions of the change. The error is the reason sent back.
/// Longest time to live of a rename request this device honours.
const MAX_RENAME_TTL_MS: u64 = 60_000;

/// Renames `from` to `to` for a writer that asked ([`trs_sync::folder::FolderCore::rename_for_peer`]), on the folder's
/// core, if it gets the core by `by` (a pull may hold it): otherwise busy, and it never starts later. Tells the
/// sessions of the change. Returns the outcome and, when it is not done, why.
async fn rename_for_peer(f: Arc<FolderShared>, from: String, to: String, seen: [u8; 32], by: std::time::Instant) -> (trs_sync_proto::msg::RenameOutcome, String) {
    use trs_sync_proto::msg::RenameOutcome;
    let f2 = f.clone();
    let done = tokio::task::spawn_blocking(move || loop {
        let core = match f2.core.try_lock() {
            Ok(c) => Some(c),
            Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => None,
        };
        match core {
            Some(mut core) => {
                let r = core.rename_for_peer(&from, &to, &seen);
                break Some((r, core.index_state().map(|(_, s)| s).unwrap_or(0)));
            }
            None if std::time::Instant::now() >= by => break None,
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    })
    .await;
    match done {
        Ok(Some((r, seq))) => {
            f.local_seq.send_if_modified(|s| std::mem::replace(s, seq) != seq);
            match r {
                Ok(_) => (RenameOutcome::Done, String::new()),
                Err(e) => (RenameOutcome::Refused, e.to_string()),
            }
        }
        Ok(None) => (RenameOutcome::Busy, "busy (a scan or a transfer); try again in a moment".to_string()),
        Err(e) => (RenameOutcome::Refused, e.to_string()),
    }
}

/// A session's folders checked against what is open here and what the Joins covered (a folder opened or closed while
/// it was set up, or while a Join was handled), for every folder the peer is a member of:
/// - closed here but linked: stale ([`PeerHandle::close_folder`]);
/// - open here but stale, and named by the peer's Join: the session starts anew (its Join then says what we hold);
/// - open here, neither linked nor stale, and not in our Join: offered now (the peer answers if it has it open).
pub(crate) async fn reconcile(ctx: &Arc<Ctx>, peer: &Arc<PeerHandle>) {
    let mut restart = false;
    for f in ctx.registry.spaces().into_iter().filter(|f| f.kind == SpaceKind::Folder && f.role_of(&peer.device_pk).is_some()) {
        let open = ctx.registry.folder(&f.id).is_some_and(|x| x.allowed_with(peer.lan));
        let stale = peer.is_stale(&f.id);
        let linked = !stale && peer.folders.read().unwrap_or_else(|e| e.into_inner()).contains_key(&f.id);
        if !open {
            if linked {
                peer.close_folder(&f.id);
            }
        } else if stale {
            restart |= lock(&peer.got).contains(&f.id);
        } else if !linked && !lock(&peer.joined).contains(&f.id) {
            let join = own_join(ctx, &[f.id], &peer.exporter, peer.device, peer.lan).await;
            if join.folders.iter().any(|x| x.folder_id == f.id) {
                lock(&peer.joined).insert(f.id);
                lock(&peer.offered).insert(f.id);
                let _ = peer.control.send(Message::Join(join)).await;
            }
        }
    }
    if restart {
        let reason = "a folder opened while the session was set up: starting it anew".to_string();
        let _ = peer.control.send(Message::Close(Close { reason })).await;
    }
}

fn shared_folder(ctx: &Ctx, peer: &PeerHandle, id: &FolderId) -> Option<Arc<FolderShared>> {
    peer.access(id)?;
    ctx.registry.folder(id).filter(|f| f.allowed_with(peer.lan))
}

/// Reads one requested chunk: only from a file our index holds as a valid file with exactly this chunk at
/// this offset, and only if the bytes still hash right (the file may have changed since the scan).
fn serve_chunk(f: &FolderShared, req: &Request) -> Response {
    let fail = |code| Response { id: req.id, code, data: vec![] };
    let Some(rec) = f.index.local(&req.path).ok().flatten() else { return fail(ResponseCode::NoSuchFile) };
    let info = &rec.file.info;
    if rec.invalid || rec.dataless || info.deleted || info.kind != FileKind::File {
        return fail(ResponseCode::NoSuchFile);
    }
    let listed = trs_sync::index::offsets(&info.chunks).any(|(off, c)| off == req.offset && c.len == req.len && c.hash == req.hash);
    if !listed {
        return fail(ResponseCode::Invalid);
    }
    let fs = StdFs::new(f.root.clone());
    let Ok(fh) = fs.open_read(&req.path) else { return fail(ResponseCode::NoSuchFile) };
    let mut buf = vec![0u8; req.len as usize];
    let mut got = 0;
    while got < buf.len() {
        match fh.read_at(req.offset + got as u64, &mut buf[got..]) {
            Ok(0) | Err(_) => return fail(ResponseCode::Generic),
            Ok(n) => got += n,
        }
    }
    if ring::digest::digest(&ring::digest::SHA256, &buf).as_ref() != req.hash {
        return fail(ResponseCode::NoSuchFile);
    }
    Response { id: req.id, code: ResponseCode::Ok, data: buf }
}
