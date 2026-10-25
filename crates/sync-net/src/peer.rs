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
    Close, Hello, Incompatible, Index, Join, JoinFolder, MAX_INDEX_ENTRIES, Members, Message, Request, Response,
    ResponseCode, agree,
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
            if checked.is_empty() {
                // Nothing to share: say why instead of answering (a refused invitation is shown there).
                let reason = close_reason(&checked);
                let _ = writer.send(&Message::Close(Close { reason: reason.clone() })).await;
                return Err(bad(reason));
            }
            let ids: Vec<FolderId> = checked.shared.iter().map(|(f, _, _)| *f).collect();
            let join = own_join(&ctx, &ids, &est.exporter, device, lan).await;
            sent.extend(join.folders.iter().map(|f| f.folder_id));
            writer.send(&Message::Join(join)).await?;
            if chat {
                let ids: Vec<FolderId> = checked.channels.iter().map(|(c, _)| *c).collect();
                let cj = own_chat_join(&ctx, &ids, &est.exporter);
                sent.extend(cj.channels.iter().map(|c| c.channel_id));
                writer.send(&Message::ChatJoin(cj)).await?;
            }
        }
        Ok::<_, io::Error>((hello, agreed, checked, sent))
    };
    let setup = tokio::time::timeout(SETUP_TIMEOUT, setup).await.map_err(|_| bad("setup timed out"))?;
    let (hello, agreed, checked, sent) = setup.inspect_err(|e| note_refusal(&ctx, &candidates, device, e))?;
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
    if !ctx.registry.add_peer(&ctx.me, peer.clone()) {
        let _ = writer.send(&Message::Close(Close { reason: "already connected".into() })).await;
        return Ok(());
    }
    let _ = ctx.hub.send(HubMsg::PeerUp(device));

    let writer_task = tokio::spawn(write_loop(writer, crx, drx, peer.clone()));
    let serve = Arc::new(Semaphore::new(SERVE_BUDGET_KIB));
    let mut links = Vec::new();
    for (folder, access, their) in shared {
        links.push(start_link(&ctx, &peer, folder, access, their));
    }
    for (channel, access) in channels {
        links.push(start_chat_link(&ctx, &peer, channel, access));
    }
    offer_joining(&ctx, &peer, &sent).await;
    let result = read_loop(&ctx, &peer, &mut reader, &serve, &est, sent, &mut links).await;
    if let Err(e) = &result {
        note_refusal(&ctx, &candidates, device, e);
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
        if sent.contains(&f.id) || lock(&f.invite).is_none() || f.role_of(ctx.key.public_key()).is_some() {
            continue;
        }
        match f.kind {
            SpaceKind::Folder => {
                let join = own_join(ctx, &[f.id], &peer.exporter, peer.device, peer.lan).await;
                if !join.folders.is_empty() {
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
        Incompatible::PeerTooNew => format!("{who} runs a newer Seedwell that this one cannot sync with: update this Mac"),
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
}

impl Checked {
    fn add_chat(&mut self, c: CheckedChat) {
        self.channels.extend(c.shared);
        self.refused.extend(c.refused);
        self.changed.extend(c.changed);
    }

    /// Nothing in common.
    fn is_empty(&self) -> bool {
        self.shared.is_empty() && self.channels.is_empty()
    }
}

/// What the peer proved, folder by folder ([`crate::roles::check`]); a device redeeming an invitation is admitted
/// when this device is an owner or admin. `answered_only` (non-empty for a dialer) limits the answer to folders
/// we asked about.
fn check_join(ctx: &Ctx, j: &Join, est: &crate::tls::Established, answered_only: &BTreeSet<FolderId>, lan: bool) -> Checked {
    let mut out = Checked { shared: Vec::new(), channels: Vec::new(), refused: Vec::new(), changed: Vec::new() };
    for jf in &j.folders {
        if !answered_only.is_empty() && !answered_only.contains(&jf.folder_id) {
            continue;
        }
        let Some(f) = ctx.registry.folder(&jf.folder_id).filter(|f| f.allowed_with(lan)) else { continue };
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
fn start_link(
    ctx: &Arc<Ctx>,
    peer: &Arc<PeerHandle>,
    folder: FolderId,
    access: Access,
    their: JoinFolder,
) -> tokio::task::JoinHandle<()> {
    peer.folders.write().unwrap_or_else(|e| e.into_inner()).insert(folder, access);
    let (ctx, peer) = (ctx.clone(), peer.clone());
    tokio::spawn(async move {
        let Some(f) = ctx.registry.folder(&folder) else { return };
        f.members.write().unwrap_or_else(|e| e.into_inner()).insert(peer.device);
        let _ = ctx.hub.send(HubMsg::MemberSeen { folder, device: peer.device });
        if let Err(e) = send_index(&f, &peer, their).await {
            let _ = peer.control.send(Message::Close(Close { reason: format!("index: {e}") })).await;
        }
    })
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
    loop {
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
            Message::DirectKeys(k) if peer.chat() => crate::direct::on_keys(ctx, peer, k.records).await,
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
            Message::Join(j) => {
                // A folder added while connected (a refused invitation is not worth ending the session over).
                let checked = check_join(ctx, &j, est, &BTreeSet::new(), peer.lan);
                for id in &checked.changed {
                    if let Some(f) = ctx.registry.space(id) {
                        crate::roles::changed(ctx, &f).await;
                    }
                }
                for (folder, access, their) in checked.shared {
                    if peer.access(&folder).is_some() {
                        continue;
                    }
                    if !sent.contains(&folder) {
                        let join = own_join(ctx, &[folder], &est.exporter, peer.device, peer.lan).await;
                        sent.extend(join.folders.iter().map(|f| f.folder_id));
                        let _ = peer.control.send(Message::Join(join)).await;
                    }
                    links.push(start_link(ctx, peer, folder, access, their));
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
                crate::direct::on_messages(&ctx, peer.device, msgs).await;
                continue;
            }
            a => a,
        };
        let res = tokio::task::spawn_blocking(move || match a {
            Apply::Members(..) | Apply::OwnerKey(..) | Apply::Chat(..) | Apply::Direct(..) => Ok::<(), String>(()),
            Apply::Index(f, ri) => {
                let done = ri.done;
                let stored = lock(&f.core).apply_remote(who, ri).map_err(|e| e.to_string())?;
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
    if rec.invalid || info.deleted || info.kind != FileKind::File {
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
