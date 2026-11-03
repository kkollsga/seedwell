//! The puller's [`ChunkSource`] over the network: requests go to the connected devices that hold the
//! version, spread by how much each has in flight, LAN first.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::task::JoinSet;
use tokio::time::Instant;
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::msg::{Request, ResponseCode};

use crate::shared::{Ctx, PeerHandle};

/// Requests not answered in this time (from when each was asked) are given up (and asked of another device, or again
/// next round).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

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
            .filter(|p| p.access(&self.folder).is_some() && req.holders.contains(&p.device.short()))
            .filter(|p| avoid.is_none_or(|a| !Arc::ptr_eq(a, p)))
            .collect();
        peers.sort_by_key(|p| (!p.lan, p.inflight.load(Ordering::Relaxed)));
        peers
    }
}

impl ChunkSource for NetSource {
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
                        Some(p) => spawn_collect(&mut running, i, true, p, reqs[i].len),
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
                            Some(p) => spawn_collect(&mut running, i, false, p, reqs[i].len),
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
fn spawn_collect(running: &mut JoinSet<Answer>, i: usize, retry: bool, (peer, rx): Pending, len: u32) {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    running.spawn(async move {
        let r = collect(&peer, len, rx, deadline).await;
        (i, retry, r.map_err(|e| (Some(peer), e)))
    });
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
