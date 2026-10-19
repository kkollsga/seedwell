//! Peer events and request scheduling.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use crate::net::{Dial, PeerIo};
use trs_wire::{Bitfield, BlockRequest, Handshake, allowed_fast_set};

use super::{PeerEntry, PeerHave, Rate, Torrent, TorrentMsg, TorrentReport};
use crate::api::EngineEvent;
use crate::budget::{ConnPermit, HalfOpenPermit};
use crate::peer::{BlockAck, Close, ExtInfo, HaveSummary, PeerCmd, PeerEvent, PeerKey, PeerShared, PeerSpawn, Welcome};
use crate::picker::{PickCtx, Received};

/// Most peers we ever ban per torrent.
const MAX_BANNED: usize = 1024;
/// Outgoing attempts a seeding torrent may make in a burst (and keeps in reserve).
const SEED_DIAL_BURST: u8 = 4;
/// A seeding torrent earns one more outgoing attempt this often.
const SEED_DIAL_EVERY: Duration = Duration::from_secs(30);
/// Outgoing attempts of a seeding torrent that may be connecting at the same time.
const SEED_MAX_CONNECTING: usize = 2;
/// Allowed-fast pieces kept per peer.
const MAX_ALLOWED_IN: usize = 32;

/// Among the three fastest unchoked peers by recent download rate (ties count as fast, so at the
/// start, when nobody has a rate yet, any peer qualifies).
fn is_fast_peer(peers: &std::collections::HashMap<PeerKey, PeerEntry>, key: PeerKey) -> bool {
    let Some(me) = peers.get(&key) else { return false };
    let faster = peers
        .iter()
        .filter(|(k, p)| **k != key && p.connected && !p.peer_choking && p.down.per_sec > me.down.per_sec)
        .count();
    faster < 3
}

impl Torrent {
    pub(super) fn is_banned(&self, a: SocketAddr) -> bool {
        // On loopback every test peer shares one IP, so only the exact address is banned there.
        self.banned.addrs.contains(&a) || (!a.ip().is_loopback() && self.banned.ips.contains(&a.ip()))
    }

    pub(super) fn accept_incoming(&mut self, stream: PeerIo, hs: Handshake, addr: SocketAddr, permit: ConnPermit) {
        let d = &self.sh.diag;
        if self.shutting_down
            || self.checking
            || self.is_banned(addr)
            || self.banned.ids.contains(&hs.peer_id)
        {
            d.incoming_refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if self.peers.len() >= self.peer_cap() && !self.evict_for_newcomer() {
            self.sh.diag.incoming_refused.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let d = &self.sh.diag;
        d.incoming_accepted.fetch_add(1, Ordering::Relaxed);
        self.spawn_peer(addr, Some((stream, hs)), permit, None);
    }

    pub(super) fn spawn_peer(
        &mut self,
        addr: SocketAddr,
        incoming: Option<(PeerIo, Handshake)>,
        permit: ConnPermit,
        half_open: Option<HalfOpenPermit>,
    ) -> PeerKey {
        let key = self.next_key;
        self.next_key = self.next_key.wrapping_add(1).max(1);
        let (ptx, prx) = crate::chan::channel(256);
        let shared = std::sync::Arc::new(PeerShared::default());
        let now = Instant::now();
        let is_in = incoming.is_some();
        self.peers.insert(
            key,
            PeerEntry {
                tx: ptx,
                shared: shared.clone(),
                addr,
                listen_addr: (!is_in).then_some(addr),
                incoming: is_in,
                utp: incoming.as_ref().is_some_and(|(s, _)| s.is_utp()),
                connected: false,
                peer_id: [0; 20],
                fast: false,
                am_choking: true,
                am_interested: false,
                peer_choking: true,
                peer_interested: false,
                have: PeerHave::Unknown,
                inflight: Vec::new(),
                allowed_in: Vec::new(),
                depth: 16,
                down: Rate::default(),
                up: Rate::default(),
                connected_at: now,
                last_block: now,
                last_have: None,
                last_unchoked: None,
                snubbed_at: None,
                strikes: 0,
                client: None,
                upload_only: false,
                has_pex: false,
                has_metadata: false,
                meta_size: 0,
                reqq: 250,
                pex_seq: 0,
                next_pex: now + Duration::from_secs(2),
                no_refill_until: None,
            },
        );
        self.addrs_in_use.insert(addr);
        let dial = self.dial_for(addr);
        let fut = crate::peer::run(Box::new(PeerSpawn {
            key,
            addr,
            incoming,
            dial,
            ctx: self.ctx.clone(),
            events: self.tx.clone(),
            cmds: prx,
            shared,
            permit,
            half_open,
        }));
        if std::env::var_os("TRS_SIZES").is_some() {
            eprintln!(
                "sizes: peer future {} B, TorrentMsg {} B, PeerCmd {} B, PeerEvent {} B, PeerEntry {} B, Torrent {} B, Torrent::run future n/a",
                std::mem::size_of_val(&fut),
                std::mem::size_of::<TorrentMsg>(),
                std::mem::size_of::<PeerCmd>(),
                std::mem::size_of::<PeerEvent>(),
                std::mem::size_of::<PeerEntry>(),
                std::mem::size_of::<Torrent>(),
            );
            eprintln!("sizes: {}", crate::peer::debug_sizes());
        }
        tokio::spawn(fut);
        key
    }

    /// Which transport to try for an outgoing connection (libtorrent-style: TCP unless the peer
    /// is known to speak uTP, with the other transport as the fallback).
    fn dial_for(&self, addr: SocketAddr) -> Dial {
        if self.ctx.utp().is_none() || !self.cfg.utp {
            return Dial::Tcp;
        }
        if !self.cfg.outgoing_tcp {
            return Dial::Utp;
        }
        let known = self.sh.utp_peers.lock().unwrap_or_else(|p| p.into_inner()).contains(&addr);
        if known { Dial::UtpThenTcp } else { Dial::TcpThenUtp }
    }

    fn send(&self, key: PeerKey, c: PeerCmd) -> bool {
        self.peers.get(&key).is_some_and(|p| p.tx.try_send(c).is_ok())
    }

    pub(super) fn disconnect(&self, key: PeerKey) {
        if let Some(p) = self.peers.get(&key) {
            let _ = p.tx.try_send(PeerCmd::Disconnect);
        }
    }

    pub(super) async fn on_peer_event(&mut self, key: PeerKey, ev: PeerEvent) {
        if !self.peers.contains_key(&key) {
            return;
        }
        match ev {
            PeerEvent::Connected { peer_id, reserved, utp } => self.on_connected(key, peer_id, reserved, utp),
            PeerEvent::Ext(info) => self.on_ext(key, info),
            PeerEvent::Choked => self.on_choked(key),
            PeerEvent::Unchoked => {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.peer_choking = false;
                }
                self.fill_requests(key);
            }
            PeerEvent::Interested => {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.peer_interested = true;
                }
                self.last_demand = Instant::now();
                self.quick_unchoke(key);
            }
            PeerEvent::NotInterested => {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.peer_interested = false;
                }
            }
            PeerEvent::Have(i) => self.on_have(key, i),
            PeerEvent::Bitfield(b) => self.on_bitfield(key, &b),
            PeerEvent::HaveAll => self.set_peer_have_all(key),
            PeerEvent::HaveNone => {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.have = PeerHave::Count(0);
                }
                self.attach_empty_set(key);
            }
            PeerEvent::AllowedFast(i) => {
                if let Some(p) = self.peers.get_mut(&key)
                    && p.allowed_in.len() < MAX_ALLOWED_IN && !p.allowed_in.contains(&i) {
                        p.allowed_in.push(i);
                    }
                self.fill_requests(key);
            }
            PeerEvent::Block { piece, begin, len, ack } => self.on_block(key, piece, begin, len, ack),
            PeerEvent::Rejected(r) => self.on_rejected(key, r),
            PeerEvent::Pex(addrs) => self.on_pex(addrs),
            PeerEvent::MetaRequest(p) => self.on_meta_request(key, p),
            PeerEvent::MetaData { piece, total, data } => self.on_meta_data(key, piece, total, &data),
            PeerEvent::MetaReject(p) => self.on_meta_reject(key, p),
            PeerEvent::StorageFault(msg, crate::peer::FaultKind::Blocked) => self.on_disk_blocked(msg),
            PeerEvent::StorageFault(msg, kind) => {
                self.report(TorrentReport::Fault { kind, message: msg });
            }
        }
    }

    fn on_connected(&mut self, key: PeerKey, peer_id: [u8; 20], reserved: trs_wire::Reserved, utp: bool) {
        self.sh.net.alive();
        if self.banned.ids.contains(&peer_id)
            || self.peers.iter().any(|(k, q)| *k != key && q.connected && q.peer_id == peer_id)
        {
            self.disconnect(key);
            return;
        }
        let fast = self.ctx.reserved.both_fast(&reserved);
        let (bytes, all, none) = self.bitfield_bytes();
        let n = self.meta.as_ref().map(|m| m.piece_count);
        let have = match (all, none) {
            (true, _) if fast => HaveSummary::All,
            (true, _) => HaveSummary::Bitfield(match n {
                Some(n) => Bytes::copy_from_slice(Bitfield::have_all(n).as_bytes()),
                None => Bytes::new(),
            }),
            (_, true) => HaveSummary::None,
            _ => HaveSummary::Bitfield(bytes),
        };
        let mut allowed_fast = Vec::new();
        if fast
            && let (Some(n), Some(p)) = (n, self.peers.get(&key)) {
                allowed_fast = allowed_fast_set(&self.id, p.addr.ip(), n, 4);
                if let Some(d) = self.ctx.disk.get() {
                    allowed_fast.retain(|&i| d.have.get(i));
                }
            }
        if let Some(p) = self.peers.get_mut(&key) {
            p.connected = true;
            p.utp = utp;
            if utp {
                let c = if p.incoming { &self.sh.diag.utp_accepted } else { &self.sh.diag.utp_dialed };
                c.fetch_add(1, Ordering::Relaxed);
            }
            // A uTP connection we dialled proves the address speaks uTP; a TCP one after a
            // failed uTP probe proves the opposite.
            if !p.incoming {
                let mut known = self.sh.utp_peers.lock().unwrap_or_else(|e| e.into_inner());
                if utp {
                    known.insert(p.addr);
                } else {
                    known.forget(&p.addr);
                }
            }
            p.peer_id = peer_id;
            p.fast = fast;
            p.connected_at = Instant::now();
        }
        if !self.send(key, PeerCmd::Welcome(Box::new(Welcome { have, allowed_fast, num_pieces: n }))) {
            self.disconnect(key);
        }
        self.publish_peer_counts();
    }

    fn on_ext(&mut self, key: PeerKey, info: ExtInfo) {
        let upload_only = info.upload_only;
        let mut learned: Option<SocketAddr> = None;
        let utp = self.peers.get(&key).is_some_and(|p| p.utp);
        if let Some(p) = self.peers.get_mut(&key) {
            p.client = info.client.map(String::into_boxed_str);
            p.upload_only = upload_only;
            p.has_pex = info.has_pex;
            p.has_metadata = info.has_metadata;
            p.meta_size = info.metadata_size.unwrap_or(0);
            p.reqq = info.reqq.unwrap_or(250).clamp(2, 500);
            if let Some(port) = info.listen_port.filter(|&p| p != 0) {
                let a = SocketAddr::new(p.addr.ip(), port);
                if p.listen_addr != Some(a) {
                    p.listen_addr = Some(a);
                    learned = Some(a);
                }
                if p.utp && p.incoming {
                    // Peers use one port for TCP and uTP; remember it for the next dial.
                    self.sh.utp_peers.lock().unwrap_or_else(|e| e.into_inner()).insert(a);
                }
            }
        }
        if let Some(a) = learned {
            self.pex_note_connected(a, upload_only, utp);
        }
        if self.meta_fetch.is_some() {
            self.meta_offer(key);
            self.meta_request_more();
        }
        self.drop_if_redundant(key);
    }

    fn on_choked(&mut self, key: PeerKey) {
        let Some(p) = self.peers.get_mut(&key) else { return };
        p.peer_choking = true;
        if !p.fast {
            // Without Fast a choke silently drops our queue.
            let lost: Vec<BlockRequest> = std::mem::take(&mut p.inflight);
            if let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) {
                for r in lost {
                    pk.request_lost(r);
                }
            }
        }
    }

    fn attach_empty_set(&mut self, key: PeerKey) {
        let Some(m) = self.meta.as_ref() else { return };
        if m.picker.is_some()
            && let Some(p) = self.peers.get_mut(&key) {
                p.have = PeerHave::Set(Bitfield::new(m.piece_count));
            }
    }

    fn set_peer_have_all(&mut self, key: PeerKey) {
        if let Some(p) = self.peers.get_mut(&key) {
            // Replace any earlier knowledge so availability stays right.
            let old = std::mem::replace(&mut p.have, PeerHave::All);
            if let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) {
                match old {
                    PeerHave::Set(b) => pk.remove_peer(Some(&b)),
                    PeerHave::All => pk.remove_peer(None),
                    _ => {}
                }
                pk.add_peer(None);
            }
        }
        if self.drop_if_redundant(key) {
            return;
        }
        self.update_interest(key);
        self.fill_requests(key);
    }

    fn on_have(&mut self, key: PeerKey, piece: u32) {
        let Some(m) = self.meta.as_mut() else { return };
        if piece >= m.piece_count {
            self.strike(key, 1);
            return;
        }
        let Some(p) = self.peers.get_mut(&key) else { return };
        p.last_have = Some(Instant::now());
        match (&mut p.have, m.picker.as_mut()) {
            (PeerHave::All, _) => {}
            (PeerHave::Set(b), Some(pk)) => {
                if b.set(piece) {
                    pk.peer_have(piece);
                }
            }
            (PeerHave::Count(n), None) => {
                *n = n.saturating_add(1);
                self.drop_if_redundant(key);
                return;
            }
            (h @ PeerHave::Unknown, Some(pk)) => {
                let mut b = Bitfield::new(m.piece_count);
                b.set(piece);
                pk.peer_have(piece);
                *h = PeerHave::Set(b);
            }
            (h @ PeerHave::Count(_), Some(pk)) => {
                let mut b = Bitfield::new(m.piece_count);
                b.set(piece);
                pk.peer_have(piece);
                *h = PeerHave::Set(b);
            }
            (h @ PeerHave::Unknown, None) => *h = PeerHave::Count(1),
            (_, None) => {}
        }
        if self.drop_if_redundant(key) {
            return;
        }
        self.update_interest(key);
        self.fill_requests(key);
    }

    fn on_bitfield(&mut self, key: PeerKey, bytes: &Bytes) {
        let Some(m) = self.meta.as_ref() else {
            // Metadata still missing: only remember that this peer has pieces.
            return;
        };
        let n = m.piece_count;
        let Ok(bf) = Bitfield::from_wire(bytes, n) else {
            self.strike(key, 3);
            self.disconnect(key);
            return;
        };
        let have_picker = self.meta.as_ref().is_some_and(|m| m.picker.is_some());
        if let Some(p) = self.peers.get_mut(&key) {
            let old = std::mem::replace(&mut p.have, PeerHave::Unknown);
            if let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) {
                match old {
                    PeerHave::Set(b) => pk.remove_peer(Some(&b)),
                    PeerHave::All => pk.remove_peer(None),
                    _ => {}
                }
                pk.add_peer(Some(&bf));
            }
            p.have = if have_picker {
                PeerHave::Set(bf)
            } else {
                PeerHave::Count(bf.count_ones())
            };
        }
        if self.drop_if_redundant(key) {
            return;
        }
        self.update_interest(key);
        self.fill_requests(key);
    }

    /// An upload-only torrent at its cap makes room for a newcomer (a leecher that just got our
    /// address from the tracker) by closing a redundant peer, else the longest-idle peer that is
    /// not interested in us. Returns whether room was made.
    fn evict_for_newcomer(&mut self) -> bool {
        const STILL_DOWNLOADING: Duration = Duration::from_secs(300);
        if !self.upload_only_now() {
            return false;
        }
        let now = Instant::now();
        let idle_for = Duration::from_secs(10);
        let victim = self
            .peers
            .iter()
            .find(|(_, p)| self.peer_redundant(p))
            .map(|(k, _)| *k)
            .or_else(|| {
                self.peers
                    .iter()
                    .filter(|(_, p)| {
                        p.connected
                            && !p.peer_interested
                            && now.saturating_duration_since(p.connected_at) >= idle_for
                            && p.up.per_sec == 0.0
                            // A peer still downloading (from anyone) may want our pieces later:
                            // it stays until it is done (then it is redundant and goes anyway).
                            && p.last_have.is_none_or(|h| now.saturating_duration_since(h) >= STILL_DOWNLOADING)
                    })
                    .min_by_key(|(_, p)| p.connected_at)
                    .map(|(k, _)| *k)
            });
        let Some(k) = victim else { return false };
        self.drop_if_redundant(k);
        self.disconnect(k);
        true
    }

    /// Nothing we want is missing (complete, or a partial seed with every wanted piece): the
    /// torrent only serves, so a peer that can never want anything from us is of no use.
    pub(super) fn upload_only_now(&self) -> bool {
        let Some(m) = self.meta.as_ref() else { return false };
        self.complete() || m.picker.as_ref().is_some_and(|pk| pk.wanted_done())
    }

    /// Can this peer never be interested in us: it has every piece, or every piece we have?
    fn peer_redundant(&self, p: &PeerEntry) -> bool {
        let Some(m) = self.meta.as_ref() else { return false };
        if !p.connected {
            return false;
        }
        match &p.have {
            PeerHave::All => true,
            PeerHave::Count(n) if *n >= m.piece_count => true,
            _ if p.peer_interested => false,
            _ if p.upload_only => true,
            PeerHave::Set(b) => {
                b.is_complete()
                    || m.picker.as_ref().is_some_and(|pk| {
                        let ours = pk.have();
                        ours.count_ones() > 0 && ours.iter_ones_missing_from(b).next().is_none()
                    })
            }
            _ => false,
        }
    }

    /// libtorrent's "close redundant connections": while we are upload-only, drop a peer that has
    /// nothing to gain from us, and remember its address so it is not dialled again meanwhile.
    /// A plain local close: no strike, no log line. Returns whether the peer was dropped.
    pub(super) fn drop_if_redundant(&mut self, key: PeerKey) -> bool {
        if !self.upload_only_now() {
            return false;
        }
        let Some(p) = self.peers.get(&key) else { return false };
        if !self.peer_redundant(p) {
            return false;
        }
        if let Some(a) = p.listen_addr {
            self.pool.mark_seed(a);
        }
        self.disconnect(key);
        true
    }

    /// The same, for every connected peer (we just became upload-only).
    pub(super) fn drop_redundant_all(&mut self) {
        if !self.upload_only_now() {
            return;
        }
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        for k in keys {
            self.drop_if_redundant(k);
        }
    }

    /// Is there anything this peer has that we still want?
    fn peer_has_wanted(&self, p: &PeerEntry) -> bool {
        let Some(pk) = self.meta.as_ref().and_then(|m| m.picker.as_ref()) else {
            return false;
        };
        if !self.downloading {
            return false;
        }
        match &p.have {
            PeerHave::All => pk.wanted_missing() > 0,
            PeerHave::Set(b) => b.iter_ones_missing_from(pk.have()).any(|i| pk.is_wanted(i)),
            _ => false,
        }
    }

    pub(super) fn update_interest(&mut self, key: PeerKey) {
        let Some(p) = self.peers.get(&key) else { return };
        if !p.connected {
            return;
        }
        let want = self.peer_has_wanted(p);
        if want != p.am_interested {
            if self.send(key, PeerCmd::Interested(want)) {
                if let Some(p) = self.peers.get_mut(&key) {
                    p.am_interested = want;
                }
            } else {
                self.disconnect(key);
            }
        }
    }

    pub(super) fn refresh_all_interest(&mut self) {
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        for k in keys {
            self.update_interest(k);
        }
    }

    /// Unchoke at once if a slot is free, so a new leecher need not wait for the next rechoke.
    fn quick_unchoke(&mut self, key: PeerKey) {
        let unchoked = self.peers.values().filter(|p| !p.am_choking).count();
        let now = Instant::now();
        // Demand just changed: ask the allocator again (after the first two unchoked peers at most 4 times a
        // second per torrent, so a peer that toggles `interested` cannot make us sort the demand table all day).
        if unchoked >= self.slots
            && (self.slots == 0 || unchoked < 2 || now.saturating_duration_since(self.slots_at) >= Duration::from_millis(250))
        {
            self.refresh_slots(now);
        }
        if unchoked < self.slots {
            self.unchoke(key);
        }
    }

    pub(super) fn unchoke(&mut self, key: PeerKey) {
        if self.send(key, PeerCmd::Unchoke)
            && let Some(p) = self.peers.get_mut(&key) {
                p.am_choking = false;
                p.last_unchoked = Some(Instant::now());
            }
    }

    pub(super) fn choke(&mut self, key: PeerKey) {
        if self.send(key, PeerCmd::Choke)
            && let Some(p) = self.peers.get_mut(&key) {
                p.am_choking = true;
            }
    }

    /// Top up a peer's request queue.
    pub(super) fn fill_requests(&mut self, key: PeerKey) {
        if !self.downloading {
            return;
        }
        let now = Instant::now();
        let Some(m) = self.meta.as_mut() else { return };
        let Some(pk) = m.picker.as_mut() else { return };
        // Deadline mode: a stream with a late urgent head lets the few fastest peers race for it.
        let streaming = pk.has_hints();
        let fast = streaming && pk.wants_duplicates(now) && is_fast_peer(&self.peers, key);
        let Some(p) = self.peers.get_mut(&key) else { return };
        if !p.connected || !p.am_interested || p.snubbed_at.is_some() {
            return;
        }
        if p.no_refill_until.is_some_and(|t| now < t) {
            return;
        }
        let choked = p.peer_choking;
        if choked && p.allowed_in.is_empty() {
            return;
        }
        let depth = p.depth as usize;
        let room = depth.saturating_sub(p.inflight.len());
        // Refill in batches: a request message per block would double the chatter.
        // (a streamed torrent skips the batching: an urgent piece must not wait for a quarter of the queue)
        if room == 0 || (!p.inflight.is_empty() && room * 4 < depth && !streaming) {
            return;
        }
        let has = match &p.have {
            PeerHave::All => None,
            PeerHave::Set(b) => Some(b),
            _ => return,
        };
        let mut reqs = Vec::with_capacity(room);
        pk.pick(
            &PickCtx {
                peer: key,
                has,
                max: room,
                only: choked.then_some(&p.allowed_in[..]),
                inflight: &p.inflight,
                now,
                fast,
            },
            &mut reqs,
        );
        if reqs.is_empty() {
            return;
        }
        if p.inflight.is_empty() {
            p.last_block = now;
        }
        p.inflight.extend_from_slice(&reqs);
        if p.tx.try_send(PeerCmd::Requests(reqs.clone())).is_err() {
            let keep = p.inflight.len() - reqs.len();
            p.inflight.truncate(keep);
            for r in reqs {
                pk.request_lost(r);
            }
            let _ = p.tx.try_send(PeerCmd::Disconnect);
        }
    }

    pub(super) fn refill_all(&mut self) {
        let keys: Vec<PeerKey> = self.peers.keys().copied().collect();
        for k in keys {
            self.fill_requests(k);
        }
    }

    fn on_block(&mut self, key: PeerKey, piece: u32, begin: u32, len: u32, ack: BlockAck) {
        let req = BlockRequest { index: piece, begin, length: len };
        let now = Instant::now();
        if let Some(p) = self.peers.get_mut(&key) {
            if let Some(pos) = p.inflight.iter().position(|r| *r == req) {
                p.inflight.swap_remove(pos);
            }
            p.last_block = now;
            p.snubbed_at = None;
        }
        self.last_demand = now;
        let accepted = !matches!(ack, BlockAck::Duplicate);
        let mut accepted_new = false;
        if accepted {
            if let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) {
                let r = pk.block_received(piece, begin);
                accepted_new = matches!(r, Received::Accepted { .. });
                if accepted_new
                    && let Some(bus) = &self.stream {
                        bus.note_progress(piece, pk.received_blocks(piece));
                    }
                // Endgame and urgent pieces of a stream both request a block more than once: the
                // copies other peers still owe are cancelled the moment one arrives.
                if matches!(r, Received::Accepted { .. }) && (pk.in_endgame() || pk.is_urgent_piece(piece)) {
                    for (k, q) in self.peers.iter_mut() {
                        if *k == key {
                            continue;
                        }
                        if let Some(pos) = q.inflight.iter().position(|x| *x == req) {
                            q.inflight.swap_remove(pos);
                            let _ = q.tx.try_send(PeerCmd::Cancel(req));
                        }
                    }
                }
            }
            if accepted_new {
                let c = self.contrib.entry(piece).or_default();
                if !c.contains(&key) && c.len() < 8 {
                    c.push(key);
                }
            }
        }
        if let BlockAck::Complete(pending) = ack {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                let result = pending.await.map_err(|e| e.to_string());
                let _ = tx.send(TorrentMsg::PieceHashed { piece, result }).await;
            });
        }
        self.fill_requests(key);
    }

    fn on_rejected(&mut self, key: PeerKey, r: BlockRequest) {
        if let Some(p) = self.peers.get_mut(&key) {
            if let Some(pos) = p.inflight.iter().position(|x| *x == r) {
                p.inflight.swap_remove(pos);
            }
            p.no_refill_until = Some(Instant::now() + Duration::from_secs(1));
        }
        if let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) {
            pk.request_lost(r);
        }
    }

    pub(super) fn strike(&mut self, key: PeerKey, weight: u32) {
        let max = self.cfg.max_hash_failures_per_peer;
        let Some(p) = self.peers.get_mut(&key) else { return };
        p.strikes += weight;
        if p.strikes >= max {
            self.ban(key);
        }
    }

    pub(super) fn ban(&mut self, key: PeerKey) {
        let Some(p) = self.peers.get(&key) else { return };
        let (addr, id) = (p.addr, p.peer_id);
        if self.banned.addrs.len() < MAX_BANNED {
            self.banned.addrs.insert(addr);
            self.banned.ips.insert(addr.ip());
            self.banned.ids.insert(id);
        }
        self.pool.forget(&addr);
        self.sh.diag.peers_banned.fetch_add(1, Ordering::Relaxed);
        self.sh.emit(EngineEvent::PeerBanned { id: self.id, addr });
        self.report(TorrentReport::Banned);
        self.disconnect(key);
    }

    pub(super) fn on_peer_closed(&mut self, key: PeerKey, why: Close, connected: bool) {
        let Some(p) = self.peers.remove(&key) else { return };
        self.addrs_in_use.remove(&p.addr);
        if self.peers.is_empty() {
            // No peer left, so the actor stops ticking (`next_wait`): a last sampled rate would
            // otherwise stay published (and the menu bar arrow stay coloured) until the next tick.
            self.live.down_rate.store(0, Ordering::Relaxed);
            self.live.up_rate.store(0, Ordering::Relaxed);
        }
        if let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) {
            for r in &p.inflight {
                pk.request_lost(*r);
            }
            match &p.have {
                PeerHave::Set(b) => pk.remove_peer(Some(b)),
                PeerHave::All => pk.remove_peer(None),
                _ => {}
            }
            pk.peer_gone(key);
        }
        if let Some(a) = p.listen_addr {
            self.pex_log.dropped(a);
        }
        // Meta requests this peer owed us go back to the pool.
        self.meta_peer_gone(key);
        let now = Instant::now();
        match why {
            Close::Storage(_) | Close::Local => {}
            // The tracker handed us our own address: never dial it again.
            Close::Handshake("connected to ourselves") if !p.incoming => self.pool.forget(&p.addr),
            _ if self.is_banned(p.addr) => {}
            // An outage (SPEC 7C.4c): the dial failed because of the network, not the peer. The
            // address is kept without a strike and tried again when the network is back.
            _ if !p.incoming && !connected && self.sh.net.held() => self.pool.requeue(p.addr),
            _ if !connected => self.pool.retry_later(p.addr, Duration::from_secs(30), now),
            Close::Eof | Close::Io(_) | Close::Timeout if !p.incoming => {
                self.pool.retry_later(p.addr, Duration::from_secs(60), now)
            }
            _ => {}
        }
        self.publish_peer_counts();
        self.refill_all();
        self.connect_more(now);
    }

    /// Dial candidates while there is room under the caps.
    pub(super) fn connect_more(&mut self, now: Instant) {
        if self.shutting_down || self.checking || self.meta.is_none() && self.meta_fetch.is_none() {
            return;
        }
        // An outage holds every dial (SPEC 7C.4c): nothing could connect, and every failure would
        // age the address.
        if self.sh.net.held() {
            return;
        }
        // A complete torrent only needs a few peers; leechers find us through the tracker. Every
        // address a tracker or the DHT hands a seeder is mostly another seed, a NAT'd leecher or a
        // dead host, and dialling them all keeps a connection slot and a task busy for nothing
        // (three seeding torrents held "32 connections" for ten minutes, none of them connected).
        // So a seeder dials from a small allowance: a burst of `SEED_DIAL_BURST`, then one attempt
        // per `SEED_DIAL_EVERY`, and never more than `SEED_MAX_CONNECTING` in flight.
        let upload_only = self.upload_only_now();
        if !upload_only {
            self.pool.release_seeds();
        }
        // Nobody is downloading (the tracker said so): a seeding torrent waits for them to come.
        if upload_only && self.no_leechers {
            return;
        }
        let seeding = self.complete();
        let mut allowance = 8;
        if seeding {
            let earned = now.saturating_duration_since(self.seed_dial_at).as_secs() / SEED_DIAL_EVERY.as_secs();
            if earned > 0 {
                self.seed_dials = (u64::from(self.seed_dials) + earned).min(u64::from(SEED_DIAL_BURST)) as u8;
                self.seed_dial_at = now;
            }
            let connecting = self.peers.values().filter(|p| !p.connected).count();
            allowance = usize::from(self.seed_dials).min(SEED_MAX_CONNECTING.saturating_sub(connecting));
        }
        let cap = self.peer_cap();
        let mut started = 0;
        while self.peers.len() < cap && started < allowance && self.sh.budget.has_room() {
            let Some(addr) = self.pool.next(now, &self.addrs_in_use, &self.banned.ips, upload_only) else { break };
            if self.is_banned(addr) {
                self.pool.forget(&addr);
                continue;
            }
            let Some(permit) = self.sh.budget.try_connection() else { break };
            let Some(ho) = self.sh.budget.try_half_open() else {
                self.pool.add_front(addr);
                break;
            };
            self.spawn_peer(addr, None, permit, Some(ho));
            started += 1;
            if seeding {
                self.seed_dials = self.seed_dials.saturating_sub(1);
            }
        }
    }

    pub(super) fn publish_peer_counts(&self) {
        let connected = self.peers.values().filter(|p| p.connected).count();
        let seeds = self
            .peers
            .values()
            .filter(|p| p.connected && (p.upload_only || matches!(p.have, PeerHave::All)))
            .count();
        self.live.peers.store(connected as u32, Ordering::Relaxed);
        self.live.seeds.store(seeds as u32, Ordering::Relaxed);
        self.live.known.store(connected.max(self.pool.len()) as u32, Ordering::Relaxed);
    }
}
