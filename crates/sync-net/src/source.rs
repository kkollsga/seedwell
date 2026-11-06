//! The puller's [`ChunkSource`] over the network: requests go to the connected devices that hold the
//! version, spread by how much each has in flight, LAN first.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinSet;
use tokio::time::Instant;
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError, StreamAsk, StreamFail, StreamGot};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::msg::{Request, ResponseCode, StreamRequest};

use crate::shared::{Ctx, PeerHandle};

/// Requests not answered in this time (from when each was asked) are given up (and asked of another device, or again
/// next round).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The slowest link a piece of a stream is waited for at: below it, a piece times out (and is asked again).
const STREAM_FLOOR_RATE: u64 = 16 << 10;

/// How long a piece of `len` bytes of a stream is waited for: [`REQUEST_TIMEOUT`] plus its bytes at
/// [`STREAM_FLOOR_RATE`]. The time includes the maker's wait for its upload limit and its serving budget, so a piece of
/// 8 MiB has about 9 minutes; a session that dies ends the wait at once anyway.
pub(crate) fn stream_timeout(len: u32) -> Duration {
    REQUEST_TIMEOUT + Duration::from_secs(u64::from(len).div_ceil(STREAM_FLOOR_RATE))
}

/// The connected device to stream an unhashed version from: its maker, the one device that can (it signs the hashed
/// form); never another that announces the unhashed form as held, however close.
pub(crate) fn stream_peer(peers: Vec<Arc<PeerHandle>>, maker: trs_sync_proto::ids::ShortId) -> Option<Arc<PeerHandle>> {
    peers.into_iter().find(|p| p.device.short() == maker)
}

/// Fetches chunks of one folder from connected peers. Used on a blocking thread.
pub struct NetSource {
    ctx: Arc<Ctx>,
    folder: FolderId,
    rt: Handle,
}

impl NetSource {
    /// A source for `folder`, running its requests on `rt`.
    pub fn new(ctx: Arc<Ctx>, folder: FolderId, rt: Handle) -> Self {
        NetSource { ctx, folder, rt }
    }

    /// The connected peers that share the folder and hold the version, best first.
    fn holders(&self, req: &ChunkRequest, avoid: Option<&Arc<PeerHandle>>) -> Vec<Arc<PeerHandle>> {
        let mut peers: Vec<Arc<PeerHandle>> = self
            .ctx
            .registry
            .peers()
            .into_iter()
            .filter(|p| p.access(&self.folder).is_some() && req.holders.contains(&p.device.short()) && trusted(p, &self.folder))
            .filter(|p| avoid.is_none_or(|a| !Arc::ptr_eq(a, p)))
            .collect();
        peers.sort_by_key(|p| (!p.lan, p.inflight.load(Ordering::Relaxed)));
        peers
    }
}

impl ChunkSource for NetSource {
    /// Asks the version's maker for the next piece ([`stream_peer`]) and waits for it at most [`stream_timeout`]. A
    /// piece whose answer did not come in time is [`StreamFail::Unavailable`]: asked again, the maker serves it once
    /// more. The maker answering busy (it has no read there any more) is [`StreamFail::Restart`].
    fn stream(&self, ask: &StreamAsk) -> Result<StreamGot, StreamFail> {
        self.rt.block_on(async {
            let peers: Vec<Arc<PeerHandle>> = self.ctx.registry.peers().into_iter().filter(|p| p.access(&self.folder).is_some()).collect();
            let Some(peer) = stream_peer(peers, ask.maker) else { return Err(StreamFail::Unavailable) };
            // A folder that stops (a pause, a move, a removal) is not kept waiting for a piece minutes long.
            let Some(mut stopped) = self.ctx.registry.folder(&self.folder).map(|f| f.stopped.subscribe()) else { return Err(StreamFail::Unavailable) };
            if self.ctx.limited(peer.lan) {
                self.ctx.limits.down.take(u64::from(ask.len)).await;
            }
            let req = StreamRequest { id: 0, folder_id: self.folder, path: ask.path.clone(), version: ask.version.clone(), offset: ask.offset, len: ask.len };
            peer.inflight.fetch_add(u64::from(ask.len), Ordering::Relaxed);
            let asked = peer.stream(req).await;
            let got = match asked {
                None => None,
                Some((id, rx)) => {
                    let r = tokio::select! {
                        r = tokio::time::timeout(stream_timeout(ask.len), rx) => Some(r),
                        _ = stopped.wait_for(|s| *s) => None,
                    };
                    if !matches!(r, Some(Ok(_))) {
                        peer.forget_stream(id);
                    }
                    r
                }
            };
            peer.inflight.fetch_sub(u64::from(ask.len), Ordering::Relaxed);
            match got {
                // Not reading now, timed out, or the session closed: the same piece again later.
                None | Some(Err(_)) | Some(Ok(Err(_))) => Err(StreamFail::Unavailable),
                Some(Ok(Ok(d))) => match d.code {
                    ResponseCode::Ok => {
                        if let Some(f) = self.ctx.registry.folder(&self.folder) {
                            f.fetched.add(d.data.len() as u64);
                        }
                        Ok(StreamGot { data: d.data, chunks: d.chunks, sig: d.sig })
                    }
                    ResponseCode::Busy => Err(StreamFail::Restart),
                    ResponseCode::Changed => Err(StreamFail::Changed),
                    code => Err(StreamFail::Failed(format!("{} answered {code:?}", peer.name))),
                },
            }
        })
    }

    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
        let mut out: Vec<Option<Result<Vec<u8>, FetchError>>> = vec![None; reqs.len()];
        self.fetch_each(reqs, &mut |i, r| out[i] = Some(r));
        out.into_iter().map(|r| r.unwrap_or_else(|| Err(FetchError::Failed("no answer".into())))).collect()
    }

    /// Asks for each in turn (pipelined: the next is asked while earlier ones are on their way, the download limit
    /// pacing the asking) and hands each answer over in order as soon as it and those before it are in, so the puller
    /// writes it at once and a fetch's partial file grows with the transfer, not at the end of a window (SPEC
    /// §7G.11). A request that failed at a device is asked of another holder once, at once.
    fn fetch_each(&self, reqs: &[ChunkRequest], got: &mut dyn FnMut(usize, Result<Vec<u8>, FetchError>)) {
        self.rt.block_on(async {
            let wait = *crate::shared::lock(&self.ctx.fetch_wait);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            let folder = self.ctx.registry.folder(&self.folder);
            let mut running: JoinSet<Answer> = JoinSet::new();
            // Answers in but not handed over yet (one before them is still on its way).
            let mut ready: BTreeMap<usize, Result<Vec<u8>, FetchError>> = BTreeMap::new();
            let (mut asked, mut next) = (0usize, 0usize);
            while next < reqs.len() {
                // The next request is asked (what came meanwhile is taken without waiting), or, all asked, the next
                // answer is waited for.
                let mut answer = if asked < reqs.len() {
                    let i = asked;
                    asked += 1;
                    match self.ask(&reqs[i], None).await {
                        None => {
                            ready.insert(i, Err(FetchError::Unavailable));
                        }
                        Some(p) => spawn_collect(&mut running, i, true, p, &reqs[i], self.folder),
                    }
                    running.try_join_next()
                } else {
                    running.join_next().await
                };
                while let Some(a) = answer {
                    match a {
                        Ok((i, _, Ok(data))) => {
                            ready.insert(i, Ok(data));
                        }
                        // Failed at a device: once more, of another holder.
                        Ok((i, true, Err((Some(peer), e)))) => match self.ask(&reqs[i], Some(&peer)).await {
                            Some(p) => spawn_collect(&mut running, i, false, p, &reqs[i], self.folder),
                            None => {
                                ready.insert(i, Err(e));
                            }
                        },
                        Ok((i, _, Err((_, e)))) => {
                            ready.insert(i, Err(e));
                        }
                        // A collecting task that panicked: its request fails below, once nothing else runs.
                        Err(_) => {}
                    }
                    answer = running.try_join_next();
                }
                if asked == reqs.len() && running.is_empty() {
                    for i in next..reqs.len() {
                        ready.entry(i).or_insert_with(|| Err(FetchError::Failed("the request was lost".into())));
                    }
                }
                while let Some(r) = ready.remove(&next) {
                    if let (Ok(data), Some(f)) = (&r, &folder) {
                        f.fetched.add(data.len() as u64);
                    }
                    got(next, r);
                    next += 1;
                }
            }
        });
    }
}

/// A request's data, or why not and the device that failed it (to ask another; `None`: asked of no other).
type Got = Result<Vec<u8>, (Option<Arc<PeerHandle>>, FetchError)>;

/// A collected request: its index, whether it may be asked of another holder, and its data or why not.
type Answer = (usize, bool, Got);

type Pending = (Arc<PeerHandle>, tokio::sync::oneshot::Receiver<trs_sync_proto::msg::Response>);

/// Waits for the answer to request `i` (of `len` bytes) on a task of its own, at most [`REQUEST_TIMEOUT`] from now.
fn spawn_collect(running: &mut JoinSet<Answer>, i: usize, retry: bool, (peer, rx): Pending, req: &ChunkRequest, folder: FolderId) {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let (len, hash) = (req.len, req.hash);
    running.spawn(async move {
        let r = collect(&peer, len, rx, deadline).await.and_then(|data| checked(&peer, &folder, len, &hash, data));
        (i, retry, r.map_err(|e| (Some(peer), e)))
    });
}

/// `data` as `peer` sent it for a chunk of `len` bytes hashing to `hash` in `folder`: other bytes (a copy it changed,
/// or corrupted) are [`FetchError::Corrupt`], and the peer is asked for none of the folder's chunks again in this
/// session ([`trusted`]); the chunk is asked of another holder. (The puller checks every chunk as well.)
fn checked(peer: &PeerHandle, folder: &FolderId, len: u32, hash: &[u8; 32], data: Vec<u8>) -> Result<Vec<u8>, FetchError> {
    if data.len() == len as usize && crate::blobs::sha256(&data) == *hash {
        return Ok(data);
    }
    crate::shared::lock(&peer.distrusted).insert(*folder);
    Err(FetchError::Corrupt(peer.device.short()))
}

/// `peer` has sent no wrong chunk of `folder` in this session.
fn trusted(peer: &PeerHandle, folder: &FolderId) -> bool {
    !crate::shared::lock(&peer.distrusted).contains(folder)
}

impl NetSource {
    async fn ask(&self, r: &ChunkRequest, avoid: Option<&Arc<PeerHandle>>) -> Option<Pending> {
        for peer in self.holders(r, avoid) {
            let req = Request {
                id: 0,
                folder_id: self.folder,
                path: r.path.clone(),
                offset: r.offset,
                len: r.len,
                hash: r.hash,
                from_temp: false,
            };
            if self.ctx.limited(peer.lan) {
                self.ctx.limits.down.take(u64::from(r.len)).await;
            }
            peer.inflight.fetch_add(u64::from(r.len), Ordering::Relaxed);
            if let Some(rx) = peer.request(req).await {
                return Some((peer, rx));
            }
            peer.inflight.fetch_sub(u64::from(r.len), Ordering::Relaxed);
        }
        None
    }
}

async fn collect(
    peer: &PeerHandle,
    len: u32,
    rx: tokio::sync::oneshot::Receiver<trs_sync_proto::msg::Response>,
    deadline: Instant,
) -> Result<Vec<u8>, FetchError> {
    let got = tokio::time::timeout_at(deadline, rx).await;
    peer.inflight.fetch_sub(u64::from(len), Ordering::Relaxed);
    match got {
        Err(_) => Err(FetchError::Failed(format!("{} did not answer in time", peer.name))),
        Ok(Err(_)) => Err(FetchError::Failed(format!("the connection to {} closed", peer.name))),
        Ok(Ok(resp)) => match resp.code {
            ResponseCode::Ok => Ok(resp.data),
            ResponseCode::Busy => Err(FetchError::Unavailable),
            code => Err(FetchError::Failed(format!("{} answered {code:?}", peer.name))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(seed: u8, ip: &str) -> Arc<PeerHandle> {
        let addr: std::net::SocketAddr = format!("{ip}:1").parse().unwrap();
        let pk = *trs_sync_proto::device::DeviceKey::from_seed(&[seed; 32]).public_key();
        let agreed = trs_sync_proto::msg::Agreed { protocol: trs_sync_proto::msg::PROTOCOL, features: 0 };
        PeerHandle::new(pk, format!("device {seed}"), "Seedwell".into(), agreed, addr, addr, [0; 32], true, crate::session::Via::Tcp).0
    }

    /// The failure: a device that announces the maker's unhashed version as held (a copy it took without its hashes)
    /// is on the LAN and ranked first, so every stream goes to it, which cannot serve one, and the download never
    /// happens. The maker is asked, wherever it is.
    /// The failure: a device that serves bytes other than the signed chunk (its own changed copy of a Read only folder's
    /// file, SPEC §7G.1, or corrupted ones) is asked again and again, each time for nothing. Its wrong chunk is
    /// refused and it is asked for no chunk of that folder again in the session; other folders, and other devices, are
    /// not touched.
    #[test]
    fn a_device_that_sends_a_wrong_chunk_is_asked_for_none_of_the_folders_again() {
        let (member, other) = (peer(1, "192.168.1.5"), peer(2, "192.168.1.6"));
        let (f, g) = (FolderId::of_public_key(&[1; 32]), FolderId::of_public_key(&[2; 32]));
        let right = b"the owner's chunk".to_vec();
        let hash = crate::blobs::sha256(&right);
        assert_eq!(checked(&member, &f, right.len() as u32, &hash, right.clone()), Ok(right.clone()));
        assert!(trusted(&member, &f), "right bytes keep it trusted");
        let wrong = b"a member's chunk".to_vec();
        assert_eq!(checked(&member, &f, wrong.len() as u32, &hash, wrong), Err(FetchError::Corrupt(member.device.short())));
        assert!(!trusted(&member, &f), "asked again after a wrong chunk");
        assert!(trusted(&member, &g) && trusted(&other, &f), "another folder or device distrusted");
        assert_eq!(checked(&other, &f, 3, &hash, right), Err(FetchError::Corrupt(other.device.short())), "a length that does not match");
    }

    #[test]
    fn a_stream_goes_to_the_maker_never_to_another_holder() {
        let (near, maker) = (peer(1, "192.168.1.5"), peer(2, "203.0.113.9"));
        let id = maker.device.short();
        let chosen = stream_peer(vec![near.clone(), maker.clone()], id).expect("the maker");
        assert!(Arc::ptr_eq(&chosen, &maker));
        assert!(stream_peer(vec![near], id).is_none(), "the maker not connected: nobody");
    }

    /// The failure: a piece of 8 MiB over a slow link (or behind the maker's upload limit) times out every time, so a
    /// big file never arrives. A piece is waited for as long as it takes at 16 KiB/s.
    #[test]
    fn a_piece_is_waited_for_as_long_as_a_slow_link_takes() {
        let slow = Duration::from_secs(u64::from(STREAM_PIECE_BYTES) / (16 << 10));
        assert!(stream_timeout(STREAM_PIECE_BYTES) >= slow, "{:?}", stream_timeout(STREAM_PIECE_BYTES));
        assert!(stream_timeout(1) >= REQUEST_TIMEOUT);
    }

    const STREAM_PIECE_BYTES: u32 = trs_sync_proto::msg::STREAM_PIECE;
}
