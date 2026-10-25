//! The puller's [`ChunkSource`] over the network: requests go to the connected devices that hold the
//! version, spread by how much each has in flight, LAN first.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::runtime::Handle;
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::msg::{Request, ResponseCode};

use crate::shared::{Ctx, PeerHandle};

/// A request not answered in this time is given up (and asked of another device next round).
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
            // Send everything first (pipelined), then collect.
            let mut waiting = Vec::with_capacity(reqs.len());
            for r in reqs {
                waiting.push(self.ask(r, None).await);
            }
            let mut out = Vec::with_capacity(reqs.len());
            for (r, w) in reqs.iter().zip(waiting) {
                let first = match w {
                    Some((peer, rx)) => Some((peer.clone(), collect(&peer, r, rx).await)),
                    None => None,
                };
                let res = match first {
                    None => Err(FetchError::Unavailable),
                    Some((_, Ok(data))) => {
                        if let Some(f) = self.ctx.registry.folder(&self.folder) {
                            f.fetched.add(data.len() as u64);
                        }
                        Ok(data)
                    }
                    // One more try with another holder.
                    Some((peer, Err(e))) => match self.ask(r, Some(&peer)).await {
                        Some((p2, rx)) => collect(&p2, r, rx).await,
                        None => Err(e),
                    },
                };
                out.push(res);
            }
            out
        })
    }
}

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
) -> Result<Vec<u8>, FetchError> {
    let got = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
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
