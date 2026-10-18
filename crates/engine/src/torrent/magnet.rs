//! BEP 9: fetching metadata for a magnet link, and serving it to others.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use trs_wire::{MAX_METADATA_SIZE, MetadataAssembler, metadata};

use super::{Torrent, TorrentMsg, TorrentReport};
use crate::peer::{MetaOut, PeerCmd, PeerKey};

/// Outstanding metadata requests per peer.
const PER_PEER: usize = 3;
/// A request unanswered for this long is retried elsewhere.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Requests for metadata queued while we load it from the store.
const MAX_QUEUED_REQUESTS: usize = 128;
/// Hash mismatches after which we stop trusting the first advertised size and start over.
const MAX_FAILURES: u32 = 5;

/// Magnet metadata download state.
// size: ~100 bytes + metadata size while fetching
pub(crate) struct MetaFetch {
    asm: Option<MetadataAssembler>,
    size: u32,
    inflight: HashMap<u32, (PeerKey, Instant)>,
    failures: u32,
    /// How many peers advertised each size; the most popular wins (an attacker cannot make us
    /// allocate more than the cap, and a lone liar loses to the majority).
    votes: HashMap<u32, u32>,
}

impl MetaFetch {
    pub(crate) fn new() -> Self {
        Self { asm: None, size: 0, inflight: HashMap::new(), failures: 0, votes: HashMap::new() }
    }
}

impl Torrent {
    /// A peer told us it has metadata of some size.
    pub(super) fn meta_offer(&mut self, key: PeerKey) {
        let max = self.cfg.max_metadata_size.min(MAX_METADATA_SIZE);
        let Some(p) = self.peers.get(&key) else { return };
        let (size, has) = (p.meta_size, p.has_metadata);
        let Some(f) = self.meta_fetch.as_mut() else { return };
        if !has || size == 0 || size > max {
            return;
        }
        let v = f.votes.entry(size).or_insert(0);
        *v += 1;
        if f.asm.is_none() {
            // Take the size most peers agree on (ties: the first seen).
            let best = f.votes.iter().max_by_key(|(s, c)| (**c, std::cmp::Reverse(**s))).map(|(s, _)| *s);
            if let Some(s) = best {
                if let Ok(a) = MetadataAssembler::new(self.id, s) {
                    f.size = s;
                    f.asm = Some(a);
                }
            }
        }
    }

    /// Hand out missing pieces to peers that can serve them.
    pub(super) fn meta_request_more(&mut self) {
        let Some(f) = self.meta_fetch.as_mut() else { return };
        let Some(asm) = f.asm.as_ref() else { return };
        let missing: Vec<u32> = asm.missing().filter(|p| !f.inflight.contains_key(p)).collect();
        if missing.is_empty() {
            return;
        }
        let mut next = missing.into_iter();
        let now = Instant::now();
        let mut keys: Vec<PeerKey> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected && p.has_metadata && p.meta_size == f.size)
            .map(|(k, _)| *k)
            .collect();
        keys.sort_unstable();
        'outer: for k in keys {
            let mut load = f.inflight.values().filter(|(pk, _)| *pk == k).count();
            while load < PER_PEER {
                let Some(piece) = next.next() else { break 'outer };
                if let Some(p) = self.peers.get(&k) {
                    if p.tx.try_send(PeerCmd::Metadata(Box::new(MetaOut::Request(piece)))).is_err() {
                        continue 'outer;
                    }
                    f.inflight.insert(piece, (k, now));
                    load += 1;
                }
            }
        }
    }

    pub(super) fn on_meta_data(&mut self, key: PeerKey, piece: u32, total: u32, data: &[u8]) {
        let Some(f) = self.meta_fetch.as_mut() else { return };
        let Some(asm) = f.asm.as_mut() else { return };
        if total != f.size {
            return;
        }
        match f.inflight.get(&piece) {
            Some((k, _)) if *k == key => {
                f.inflight.remove(&piece);
            }
            // Unrequested data: ignore it rather than let a peer overwrite verified pieces.
            _ => return,
        }
        if asm.insert(piece, data).is_err() {
            self.strike(key, 1);
            return;
        }
        if asm.is_complete() {
            match asm.finish() {
                Ok(info) => {
                    // finish() verified SHA-1 against the info hash.
                    let peers = self.good_peers();
                    self.meta_fetch = None;
                    self.report(TorrentReport::MetadataReady { info: info.into_vec(), peers });
                    return;
                }
                Err(_) => {
                    f.failures += 1;
                    f.inflight.clear();
                    if f.failures >= MAX_FAILURES {
                        // Start over with a fresh vote: the first size may have come from a liar.
                        f.asm = None;
                        f.votes.clear();
                        f.failures = 0;
                    }
                }
            }
        }
        self.meta_request_more();
    }

    pub(super) fn on_meta_reject(&mut self, key: PeerKey, piece: u32) {
        if let Some(f) = self.meta_fetch.as_mut() {
            if f.inflight.get(&piece).is_some_and(|(k, _)| *k == key) {
                f.inflight.remove(&piece);
            }
        }
        if let Some(p) = self.peers.get_mut(&key) {
            p.has_metadata = false;
        }
        self.meta_request_more();
    }

    pub(super) fn meta_peer_gone(&mut self, key: PeerKey) {
        if let Some(f) = self.meta_fetch.as_mut() {
            f.inflight.retain(|_, (k, _)| *k != key);
        }
        self.meta_requests.retain(|(k, _)| *k != key);
    }

    pub(super) fn meta_tick(&mut self, now: Instant) {
        if let Some(f) = self.meta_fetch.as_mut() {
            f.inflight.retain(|_, (_, at)| now.saturating_duration_since(*at) < REQUEST_TIMEOUT);
            self.meta_request_more();
        }
    }

    // ---- serving ------------------------------------------------------------------------

    pub(super) fn on_meta_request(&mut self, key: PeerKey, piece: u32) {
        let reject = |this: &Self| {
            if let Some(p) = this.peers.get(&key) {
                let _ = p.tx.try_send(PeerCmd::Metadata(Box::new(MetaOut::Reject(piece))));
            }
        };
        // Private torrents do not hand out metadata, and a torrent without it cannot.
        if self.private || self.meta.is_none() {
            reject(self);
            return;
        }
        if let Some((_, info)) = &self.info_cache {
            let info = info.clone();
            self.send_meta_piece(key, piece, &info);
            return;
        }
        if self.meta_requests.len() >= MAX_QUEUED_REQUESTS {
            reject(self);
            return;
        }
        self.meta_requests.push((key, piece));
        if !self.info_loading {
            self.info_loading = true;
            let store = self.sh.store.clone();
            let id = self.id;
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let r = tokio::task::spawn_blocking(move || {
                    store
                        .load_torrent_file(&id)
                        .map_err(|e| e.to_string())
                        .and_then(|f| f.ok_or_else(|| "torrent file missing".to_string()))
                        .and_then(|bytes| {
                            trs_bencode::raw_dict_get(&bytes, b"info")
                                .map_err(|e| e.to_string())?
                                .map(Arc::<[u8]>::from)
                                .ok_or_else(|| "no info dictionary".to_string())
                        })
                })
                .await
                .unwrap_or_else(|e| Err(e.to_string()));
                let _ = tx.send(TorrentMsg::InfoBytes(r)).await;
            });
        }
    }

    pub(super) fn on_info_bytes(&mut self, r: Result<Arc<[u8]>, String>) {
        self.info_loading = false;
        let queued = std::mem::take(&mut self.meta_requests);
        match r {
            Ok(info) => {
                self.info_cache = Some((Instant::now(), info.clone()));
                for (k, piece) in queued {
                    self.send_meta_piece(k, piece, &info);
                }
            }
            Err(_) => {
                for (k, piece) in queued {
                    if let Some(p) = self.peers.get(&k) {
                        let _ = p.tx.try_send(PeerCmd::Metadata(Box::new(MetaOut::Reject(piece))));
                    }
                }
            }
        }
    }

    fn send_meta_piece(&self, key: PeerKey, piece: u32, info: &Arc<[u8]>) {
        let Some(p) = self.peers.get(&key) else { return };
        let out = match metadata::piece_slice(info, piece) {
            Some(s) => MetaOut::Data {
                piece,
                total: info.len() as u32,
                data: Bytes::copy_from_slice(s),
            },
            None => MetaOut::Reject(piece),
        };
        let _ = p.tx.try_send(PeerCmd::Metadata(Box::new(out)));
    }
}
