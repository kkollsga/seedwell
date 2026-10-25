//! One session with another device, after TLS: Hello, the folder proofs (Join), then index updates and
//! chunk requests both ways until either side closes.
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
    Close, Hello, Index, Join, JoinFolder, MAX_INDEX_ENTRIES, Message, PROTOCOL, Request, Response, ResponseCode,
    Writers,
};

use crate::framed::{Reader, Writer};
use crate::session::{BoxIo, Connected};
use crate::shared::{Ctx, FolderShared, HubMsg, PeerHandle, lock};

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
        writer
            .send(&Message::Hello(Hello { protocol: PROTOCOL, client: ctx.client.clone(), device_name: name }))
            .await?;
        let hello = match reader.next().await? {
            Some(Message::Hello(h)) => h,
            _ => return Err(bad("expected Hello")),
        };
        if hello.protocol != PROTOCOL {
            return Err(bad(format!("protocol {} is not supported", hello.protocol)));
        }
        let mut sent: BTreeSet<FolderId> = BTreeSet::new();
        let shared;
        if dialed {
            let join = own_join(&ctx, &candidates, &est.exporter, device, lan).await;
            sent.extend(join.folders.iter().map(|f| f.folder_id));
            writer.send(&Message::Join(join)).await?;
            match reader.next().await? {
                Some(Message::Join(j)) => shared = check_join(&ctx, &j, &est, &sent, lan),
                _ => return Err(bad("expected Join")),
            }
        } else {
            match reader.next().await? {
                Some(Message::Join(j)) => {
                    let proved = check_join(&ctx, &j, &est, &BTreeSet::new(), lan);
                    let ids: Vec<FolderId> = proved.iter().map(|(f, _, _)| *f).collect();
                    let join = own_join(&ctx, &ids, &est.exporter, device, lan).await;
                    sent.extend(join.folders.iter().map(|f| f.folder_id));
                    writer.send(&Message::Join(join)).await?;
                    shared = proved;
                }
                _ => return Err(bad("expected Join")),
            }
        }
        Ok::<_, io::Error>((hello, shared, sent))
    };
    let (hello, shared, sent) = tokio::time::timeout(SETUP_TIMEOUT, setup).await.map_err(|_| bad("setup timed out"))??;
    if shared.is_empty() {
        let _ = writer.send(&Message::Close(Close { reason: "no folders in common".into() })).await;
        return Err(bad("no folders in common"));
    }

    let (peer, crx, drx) = PeerHandle::new(device, hello.device_name, hello.client, remote, local, est.exporter, dialed, via);
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
    let result = read_loop(&ctx, &peer, &mut reader, &serve, &est, sent, &mut links).await;

    peer.close();
    ctx.registry.remove_peer(&peer);
    for l in links {
        l.abort();
    }
    writer_task.abort();
    let _ = ctx.hub.send(HubMsg::PeerDown(device));
    result
}

/// Our Join for these folders (those we have), with every proof our key can make, bound to the session
/// with this exporter value.
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
        out.push(JoinFolder {
            folder_id: f.id,
            proofs: f.key.proofs(ctx.key.public_key(), exporter),
            my_index_id,
            your_index_id: theirs.index_id,
            your_max_seq: theirs.max_seq,
        });
    }
    Join { folders: out }
}

/// What the peer proved: (folder, its access, its Join entry). Storage-only access is refused: encrypted
/// members are not supported yet, and a storage member must never be sent plaintext. `answered_only`
/// (non-empty for a dialer) limits the answer to folders we asked about.
fn check_join(
    ctx: &Ctx,
    j: &Join,
    est: &crate::tls::Established,
    answered_only: &BTreeSet<FolderId>,
    lan: bool,
) -> Vec<(FolderId, Access, JoinFolder)> {
    let mut out = Vec::new();
    for jf in &j.folders {
        if !answered_only.is_empty() && !answered_only.contains(&jf.folder_id) {
            continue;
        }
        let Some(f) = ctx.registry.folder(&jf.folder_id).filter(|f| f.allowed_with(lan)) else { continue };
        match f.key.verify_proofs(&est.peer_pk, &est.exporter, &jf.proofs) {
            Some(a) if a >= Access::Read => out.push((jf.folder_id, a, jf.clone())),
            _ => {}
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
    // Writers first: the peer checks every entry against them.
    let f2 = f.clone();
    let (writers, (index_id, _)) = tokio::task::spawn_blocking(move || {
        let core = lock(&f2.core);
        (core.writers(), core.index_state())
    })
    .await
    .map_err(io::Error::other)
    .and_then(|(w, s)| s.map(|s| (w, s)))?;
    if peer.control.send(Message::Writers(Writers { folder_id: f.id, writers })).await.is_err() {
        return Ok(());
    }

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
    tokio::spawn(apply_loop(peer.device.short(), queue, failed_tx));
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
            Message::Writers(w) => {
                let Some(f) = shared_folder(ctx, peer, &w.folder_id) else { continue };
                apply.send(Apply::Writers(f, w.writers)).await.map_err(|_| applier_gone())?;
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
                // A folder added while connected.
                for (folder, access, their) in check_join(ctx, &j, est, &BTreeSet::new(), peer.lan) {
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
            Message::Ping | Message::Progress(_) | Message::Unknown(_) => {}
            Message::Rendezvous(_) | Message::Introduce(_) | Message::RelayOpen(_) | Message::RelayData(_) | Message::RelayClose(_) => {
                let _ = ctx.hub.send(HubMsg::Relay(peer.device, Box::new(msg)));
            }
        }
    }
}

/// What the peer sent that changes the folder's core.
enum Apply {
    Writers(Arc<FolderShared>, Vec<trs_sync_proto::sign::Writer>),
    Index(Arc<FolderShared>, RemoteIndex),
}

/// Applies the peer's index and writers in the order they came, off the read loop: applying waits for the
/// folder's core, which a pull may hold while it waits for this very peer's answers. The first that fails
/// ends the session with its reason.
async fn apply_loop(who: ShortId, mut queue: mpsc::Receiver<Apply>, failed: oneshot::Sender<io::Error>) {
    while let Some(a) = queue.recv().await {
        let res = tokio::task::spawn_blocking(move || match a {
            Apply::Writers(f, w) => lock(&f.core).add_writers(&w).map(|_| ()).map_err(|e| e.to_string()),
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
