//! The puller's [`ChunkSource`] over the network: requests go to the connected devices that hold the
//! version, spread by how much each has in flight, LAN first.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::runtime::Handle;
use tokio::time::Instant;
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::msg::{Request, ResponseCode};

use crate::shared::{Ctx, PeerHandle};

/// Requests not answered in this time are given up (and asked of another device, or again next round).
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
        self.rt.block_on(async {
            let wait = *crate::shared::lock(&self.ctx.fetch_wait);
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            // Everything is asked first (pipelined), then collected. One deadline for all (they went out together): a
            // device that answers none holds the pull up a minute at most, not a minute per request.
            let mut waiting = Vec::with_capacity(reqs.len());
            for r in reqs {
                waiting.push(self.ask(r, None).await);
            }
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            let mut got: Vec<Got> = Vec::with_capacity(reqs.len());
            for (r, w) in reqs.iter().zip(waiting) {
                got.push(match w {
                    None => Err((None, FetchError::Unavailable)),
                    Some((peer, rx)) => collect(&peer, r, rx, deadline).await.map_err(|e| (Some(peer), e)),
                });
            }
            // One more try with another holder for each that failed, again all asked first.
            let mut again = Vec::new();
            for (i, res) in got.iter().enumerate() {
                if let Err((Some(peer), _)) = res
                    && let Some(p) = self.ask(&reqs[i], Some(peer)).await
                {
                    again.push((i, p));
                }
            }
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            for (i, (p2, rx)) in again {
                got[i] = collect(&p2, &reqs[i], rx, deadline).await.map_err(|e| (None, e));
            }
            let fetched: usize = got.iter().filter_map(|r| r.as_ref().ok()).map(Vec::len).sum();
            if fetched > 0
                && let Some(f) = self.ctx.registry.folder(&self.folder)
            {
                f.fetched.add(fetched as u64);
            }
            got.into_iter().map(|r| r.map_err(|(_, e)| e)).collect()
        })
    }
}

/// A request's data, or why not and the device that failed it (to ask another; `None`: asked of no other).
type Got = Result<Vec<u8>, (Option<Arc<PeerHandle>>, FetchError)>;

type Pending = (Arc<PeerHandle>, tokio::sync::oneshot::Receiver<trs_sync_proto::msg::Response>);

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
            if !peer.lan {
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
    r: &ChunkRequest,
    rx: tokio::sync::oneshot::Receiver<trs_sync_proto::msg::Response>,
    deadline: Instant,
) -> Result<Vec<u8>, FetchError> {
    let got = tokio::time::timeout_at(deadline, rx).await;
    peer.inflight.fetch_sub(u64::from(r.len), Ordering::Relaxed);
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
