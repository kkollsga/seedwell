//! Piece picker (SPEC 6, ENGINE.md 5).
//!
//! * **Partial pieces first**, then **rarest first** among pieces the peer has, random tie-break.
//! * Blocks are requested **sequentially within a piece**.
//! * Pieces whose files all have priority 0 are never requested. Boundary pieces shared with a
//!   wanted file are wanted.
//! * **Random first pieces**: until [`RANDOM_FIRST`] pieces are complete a new piece is chosen at
//!   random among those the peer has (within the highest file priority) instead of the rarest, so a
//!   fresh download has something to share quickly (libtorrent `initial_picker_threshold`).
//!   Deadline hints (streaming) are served before this and are not affected.
//! * **Endgame**: when no free block remains anywhere, a peer may duplicate blocks that are
//!   already requested from others (lowest duplicate count first). The caller cancels the
//!   duplicates when one copy arrives.
//! * A piece that failed its hash with several contributors becomes **exclusive**: one peer
//!   downloads all of it, so the next failure can be pinned on that peer.
//! * **Deadline mode** (play while downloading, SPEC 7F.2): live streams hand the picker runs of
//!   pieces they will need. Urgent pieces come first, ordered by absolute deadline across all
//!   streams (ties go round-robin between streams), and bypass `max_open`. The head of a late urgent
//!   window may be requested from a few fast peers at once. Read-ahead pieces follow in piece order.
//!   Everything else stays rarest-first; the torrent is never switched to sequential. A torrent
//!   nobody streams has an empty hint list (24 bytes) and takes none of these paths.
//!
//! The picker is plain data owned by the torrent actor; it never does I/O.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use trs_stream::{Priority as HintPriority, UrgentRange};
use trs_wire::{BLOCK_SIZE, Bitfield, BlockRequest};

use crate::util::Rng;

/// Block state: free.
const FREE: u8 = 0;
/// Block state: received (and handed to storage).
const DONE: u8 = 255;
/// Highest outstanding-request count a block can record.
const MAX_REQ: u8 = 254;
/// Exclusive pieces tracked at most (they only exist after hash failures).
const MAX_EXCLUSIVE: usize = 1024;
/// Most distinct peers an urgent block is requested from (SPEC 7F.2: "several fast peers at once").
pub const URGENT_DUP: u8 = 3;
/// The head of a stream's urgent window is duplicated only when its deadline is this close (or past):
/// a player that is being fed in time costs no redundant traffic.
pub const DUP_HORIZON: Duration = Duration::from_secs(2);
/// Pieces a torrent must have complete before new pieces are picked rarest-first (before that:
/// random). Same as libtorrent's `initial_picker_threshold`.
pub const RANDOM_FIRST: u32 = 4;
/// Most runs one stream may hand over (the stream sends a handful; a hostile caller cannot grow us).
const MAX_RUNS: usize = 32;
/// Most streams the picker follows at once.
const MAX_STREAMS: usize = 16;

/// One run of pieces a stream wants, with absolute deadlines.
#[derive(Clone, Copy, Debug)]
struct Run {
    first: u32,
    end: u32,
    urgent: bool,
    at: Instant,
    deadline: Duration,
    step: Duration,
}

impl Run {
    fn deadline_of(&self, piece: u32) -> Instant {
        self.at + self.deadline + self.step.saturating_mul(piece.saturating_sub(self.first))
    }
}

#[derive(Debug)]
struct StreamHints {
    stream: u64,
    runs: Vec<Run>,
}

/// Who may download an exclusive piece.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    /// Nobody yet; the next peer to start the piece owns it.
    Pending,
    /// Only this peer.
    Peer(u32),
}

// size: 40 bytes + one byte per block
#[derive(Debug)]
struct Partial {
    blocks: Box<[u8]>,
    received: u16,
    /// Blocks in state FREE.
    free: u16,
}

/// Input of one [`Picker::pick`] call.
#[derive(Clone, Copy, Debug)]
pub struct PickCtx<'a> {
    /// Opaque tag of the requesting peer (for exclusive pieces).
    pub peer: u32,
    /// Pieces the peer has; `None` means all of them.
    pub has: Option<&'a Bitfield>,
    /// Maximum number of requests to return.
    pub max: usize,
    /// While choked, only these pieces (Fast extension allowed set) may be requested.
    pub only: Option<&'a [u32]>,
    /// Requests already in flight to this peer (never duplicated).
    pub inflight: &'a [BlockRequest],
    /// The time of this call (deadlines are absolute instants).
    pub now: Instant,
    /// The peer is among the fastest few: it may get a duplicate of a late urgent block.
    pub fast: bool,
}

/// What [`Picker::block_received`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    /// Not a block we are tracking (piece done, unknown, or out of range).
    Unknown,
    /// We already had this block.
    Duplicate,
    /// New block; `complete` is true when it was the piece's last block.
    Accepted {
        /// All blocks of the piece have arrived.
        complete: bool,
    },
}

/// The piece picker.
// size: ~200 bytes + 2 B/piece availability + 1 B/piece priority + have bitfield
#[derive(Debug)]
pub struct Picker {
    piece_len: u32,
    piece_count: u32,
    total_len: u64,
    have: Bitfield,
    verified_bytes: u64,
    avail: Vec<u16>,
    prio: Vec<u8>,
    partial: BTreeMap<u32, Partial>,
    exclusive: HashMap<u32, Owner>,
    wanted_missing: u32,
    /// Bytes of the wanted, missing pieces.
    wanted_missing_bytes: u64,
    max_open: usize,
    rng: Rng,
    /// Per-stream hints; empty unless somebody streams.
    hints: Vec<StreamHints>,
    /// The stream that got the last hinted block (round-robin on equal deadlines).
    last_served: u64,
    /// New pieces are random while fewer than this many are complete (0 = always rarest-first).
    random_first: u32,
}

impl Picker {
    /// New picker. `prio` has one entry per piece (0 = skip). `max_open` caps partial pieces.
    pub fn new(
        piece_len: u32,
        total_len: u64,
        have: Bitfield,
        prio: Vec<u8>,
        max_open: usize,
        seed: u64,
    ) -> Self {
        let piece_count = have.len();
        let mut p = Self {
            piece_len,
            piece_count,
            total_len,
            verified_bytes: 0,
            avail: vec![0; piece_count as usize],
            prio,
            partial: BTreeMap::new(),
            exclusive: HashMap::new(),
            wanted_missing: 0,
            wanted_missing_bytes: 0,
            max_open: max_open.max(1),
            rng: Rng::seeded(seed),
            hints: Vec::new(),
            last_served: 0,
            random_first: RANDOM_FIRST,
            have,
        };
        p.prio.resize(piece_count as usize, 2);
        p.recount();
        p
    }

    /// Override the random-first threshold (tests; 0 disables it).
    pub fn set_random_first(&mut self, pieces: u32) {
        self.random_first = pieces;
    }

    fn recount(&mut self) {
        self.verified_bytes = self.have.iter_ones().map(|i| u64::from(self.piece_size(i))).sum();
        self.wanted_missing = (0..self.piece_count)
            .filter(|&i| !self.have.has(i) && self.prio_of(i) > 0)
            .count() as u32;
        self.wanted_missing_bytes = (0..self.piece_count)
            .filter(|&i| !self.have.has(i) && self.prio_of(i) > 0)
            .map(|i| u64::from(self.piece_size(i)))
            .sum();
    }

    fn prio_of(&self, i: u32) -> u8 {
        self.prio.get(i as usize).copied().unwrap_or(0)
    }

    /// Size of piece `i` in bytes.
    pub fn piece_size(&self, i: u32) -> u32 {
        if i >= self.piece_count {
            return 0;
        }
        if i + 1 == self.piece_count {
            let rem = self.total_len - u64::from(i) * u64::from(self.piece_len);
            rem.min(u64::from(self.piece_len)) as u32
        } else {
            self.piece_len
        }
    }

    /// Blocks in piece `i`.
    pub fn blocks_in(&self, i: u32) -> u16 {
        self.piece_size(i).div_ceil(BLOCK_SIZE) as u16
    }

    /// Verified pieces.
    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    /// Every piece verified.
    pub fn is_complete(&self) -> bool {
        self.have.is_complete()
    }

    /// Every piece we want is verified.
    pub fn wanted_done(&self) -> bool {
        self.wanted_missing == 0
    }

    /// Wanted pieces still missing.
    pub fn wanted_missing(&self) -> u32 {
        self.wanted_missing
    }

    /// Bytes of the wanted pieces that are still missing (piece granularity).
    pub fn wanted_missing_bytes(&self) -> u64 {
        self.wanted_missing_bytes
    }

    /// Whether any wanted file overlaps the piece (verified or not).
    pub fn piece_wanted(&self, piece: u32) -> bool {
        self.prio_of(piece) > 0
    }

    /// Exact bytes not yet verified (`left` for trackers; skipped files still count).
    pub fn left_bytes(&self) -> u64 {
        self.total_len - self.verified_bytes
    }

    /// Replace the per-piece priorities (file priorities changed).
    pub fn set_priorities(&mut self, prio: Vec<u8>) {
        self.prio = prio;
        self.prio.resize(self.piece_count as usize, 2);
        let drop: Vec<u32> = self
            .partial
            .keys()
            .copied()
            .filter(|&i| self.prio_of(i) == 0)
            .collect();
        for i in drop {
            self.partial.remove(&i);
        }
        self.recount();
    }

    /// A new peer's pieces (`None` = has everything).
    pub fn add_peer(&mut self, has: Option<&Bitfield>) {
        for i in 0..self.piece_count {
            if has.is_none_or(|b| b.has(i)) {
                if let Some(a) = self.avail.get_mut(i as usize) {
                    *a = a.saturating_add(1);
                }
            }
        }
    }

    /// A peer left; undo [`Picker::add_peer`] and its `peer_have` calls.
    pub fn remove_peer(&mut self, has: Option<&Bitfield>) {
        for i in 0..self.piece_count {
            if has.is_none_or(|b| b.has(i)) {
                if let Some(a) = self.avail.get_mut(i as usize) {
                    *a = a.saturating_sub(1);
                }
            }
        }
    }

    /// A peer announced one more piece.
    pub fn peer_have(&mut self, piece: u32) {
        if let Some(a) = self.avail.get_mut(piece as usize) {
            *a = a.saturating_add(1);
        }
    }

    /// Availability of a piece among connected peers.
    pub fn availability(&self, piece: u32) -> u16 {
        self.avail.get(piece as usize).copied().unwrap_or(0)
    }

    /// Whether the piece is wanted and missing.
    pub fn is_wanted(&self, piece: u32) -> bool {
        self.prio_of(piece) > 0 && !self.have.has(piece)
    }

    /// Whether every wanted block has been requested at least once.
    pub fn in_endgame(&self) -> bool {
        self.endgame()
    }

    /// Pieces with blocks in flight or buffered (to abort them when the torrent stops).
    pub fn open_piece_ids(&self) -> Vec<u32> {
        self.partial.keys().copied().collect()
    }

    /// Partially downloaded pieces.
    pub fn open_pieces(&self) -> usize {
        self.partial.len()
    }

    fn exclusive_ok(&self, piece: u32, peer: u32) -> bool {
        match self.exclusive.get(&piece) {
            None | Some(Owner::Pending) => true,
            Some(Owner::Peer(p)) => *p == peer,
        }
    }

    fn peer_may(c: &PickCtx<'_>, piece: u32) -> bool {
        c.has.is_none_or(|b| b.has(piece)) && c.only.is_none_or(|o| o.contains(&piece))
    }

    /// Choose up to `c.max` block requests for a peer and mark them requested.
    pub fn pick(&mut self, c: &PickCtx<'_>, out: &mut Vec<BlockRequest>) {
        let start = out.len();
        let mut budget = c.max;
        if !self.hints.is_empty() {
            // Urgent pieces by deadline, a few fast peers on a late head, then read-ahead.
            self.pick_hinted(c, &mut budget, out, true);
            self.take_urgent_duplicates(c, &mut budget, out);
            self.pick_hinted(c, &mut budget, out, false);
        }
        self.take_from_partials(c, &mut budget, out);
        while budget > 0 && self.partial.len() < self.max_open {
            let Some(piece) = self.best_new_piece(c) else { break };
            self.open(piece, c.peer);
            let before = budget;
            self.take_blocks(piece, c, &mut budget, out, false);
            if budget == before {
                break;
            }
        }
        if out.len() == start && budget > 0 && self.endgame() {
            self.take_endgame(c, &mut budget, out);
        }
    }

    // ---- deadline mode (SPEC 7F.2) --------------------------------------------------------

    /// Replace the picture of `stream` (its complete current runs; deadlines are relative to `now`).
    pub fn set_stream_hints(&mut self, stream: u64, ranges: &[UrgentRange], now: Instant) {
        let runs: Vec<Run> = ranges
            .iter()
            .filter(|r| r.pieces.0 < r.pieces.1 && r.pieces.0 < self.piece_count)
            .take(MAX_RUNS)
            .map(|r| Run {
                first: r.pieces.0,
                end: r.pieces.1.min(self.piece_count),
                urgent: r.priority == HintPriority::Urgent,
                at: now,
                deadline: r.deadline,
                step: r.step,
            })
            .collect();
        if runs.is_empty() {
            self.clear_stream_hints(stream);
        } else if let Some(h) = self.hints.iter_mut().find(|h| h.stream == stream) {
            h.runs = runs;
        } else if self.hints.len() < MAX_STREAMS {
            self.hints.push(StreamHints { stream, runs });
        }
    }

    /// Forget a stream. Blocks it caused to be requested stay requested.
    pub fn clear_stream_hints(&mut self, stream: u64) {
        self.hints.retain(|h| h.stream != stream);
        if self.hints.is_empty() {
            self.hints = Vec::new();
        }
    }

    /// Some stream has hints (the torrent is being streamed right now).
    pub fn has_hints(&self) -> bool {
        !self.hints.is_empty()
    }

    /// Whether `piece` lies in an urgent run of a live stream and is still missing: a block of it
    /// that arrives cancels the copies other peers hold (like endgame).
    pub fn is_urgent_piece(&self, piece: u32) -> bool {
        !self.hints.is_empty()
            && !self.have.has(piece)
            && self.hints.iter().any(|h| h.runs.iter().any(|r| r.urgent && (r.first..r.end).contains(&piece)))
    }

    /// Blocks of `piece` received so far (0 when it is not open).
    pub fn received_blocks(&self, piece: u32) -> u16 {
        self.partial.get(&piece).map_or(0, |p| p.received)
    }

    /// The first missing urgent piece of each stream whose deadline is within [`DUP_HORIZON`].
    fn late_heads(&self, now: Instant) -> Vec<u32> {
        let mut heads = Vec::new();
        for h in &self.hints {
            let mut best: Option<(Instant, u32)> = None;
            for r in h.runs.iter().filter(|r| r.urgent) {
                let Some(p) = (r.first..r.end).find(|&p| !self.have.has(p) && self.prio_of(p) > 0) else { continue };
                let d = r.deadline_of(p);
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, p));
                }
            }
            if let Some((d, p)) = best {
                if d <= now + DUP_HORIZON && !heads.contains(&p) {
                    heads.push(p);
                }
            }
        }
        heads
    }

    /// A stream's urgent head is late, so fast peers may race for its blocks.
    pub fn wants_duplicates(&self, now: Instant) -> bool {
        !self.hints.is_empty() && !self.late_heads(now).is_empty()
    }

    fn hinted_eligible(&self, piece: u32, c: &PickCtx<'_>, urgent: bool) -> bool {
        if self.prio_of(piece) == 0
            || self.have.has(piece)
            || !Self::peer_may(c, piece)
            || !self.exclusive_ok(piece, c.peer)
        {
            return false;
        }
        match self.partial.get(&piece) {
            Some(p) => p.free > 0,
            // Only urgent pieces bypass the cap on open pieces.
            None => urgent || self.partial.len() < self.max_open,
        }
    }

    /// Take blocks from the runs of `urgent` (or read-ahead) kind, earliest deadline first.
    fn pick_hinted(&mut self, c: &PickCtx<'_>, budget: &mut usize, out: &mut Vec<BlockRequest>, urgent: bool) {
        // (stream, run, next piece to look at)
        let mut cur: Vec<(u64, Run, u32)> = self
            .hints
            .iter()
            .flat_map(|h| h.runs.iter().filter(move |r| r.urgent == urgent).map(move |r| (h.stream, *r, r.first)))
            .collect();
        while *budget > 0 {
            let mut best: Option<(usize, (Instant, bool, u64))> = None;
            for (slot, (stream, run, pos)) in cur.iter_mut().enumerate() {
                while *pos < run.end && !self.hinted_eligible(*pos, c, urgent) {
                    *pos += 1;
                }
                if *pos >= run.end {
                    continue;
                }
                let key = (run.deadline_of(*pos), *stream == self.last_served, *stream);
                if best.as_ref().is_none_or(|(_, b)| key < *b) {
                    best = Some((slot, key));
                }
            }
            let Some((slot, (_, _, stream))) = best else { break };
            let piece = cur[slot].2;
            if !self.partial.contains_key(&piece) {
                self.open(piece, c.peer);
            }
            let before = *budget;
            self.take_blocks(piece, c, budget, out, false);
            self.last_served = stream;
            if *budget == before {
                cur[slot].2 += 1;
            }
        }
    }

    /// A fast peer with room left may take a late head's blocks that are requested fewer than
    /// [`URGENT_DUP`] times already, so a seek is not hostage to one slow answer.
    fn take_urgent_duplicates(&mut self, c: &PickCtx<'_>, budget: &mut usize, out: &mut Vec<BlockRequest>) {
        if !c.fast || *budget == 0 {
            return;
        }
        for piece in self.late_heads(c.now) {
            if *budget == 0 {
                break;
            }
            if !Self::peer_may(c, piece) || self.exclusive.contains_key(&piece) {
                continue;
            }
            let size = self.piece_size(piece);
            let Some(p) = self.partial.get_mut(&piece) else { continue };
            for (bi, st) in p.blocks.iter_mut().enumerate() {
                if *budget == 0 {
                    break;
                }
                if *st == FREE || *st == DONE || *st >= URGENT_DUP {
                    continue;
                }
                let begin = bi as u32 * BLOCK_SIZE;
                let req = BlockRequest { index: piece, begin, length: BLOCK_SIZE.min(size - begin) };
                if c.inflight.contains(&req) || out.contains(&req) {
                    continue;
                }
                *st += 1;
                *budget -= 1;
                out.push(req);
            }
        }
    }

    fn open(&mut self, piece: u32, peer: u32) {
        let n = self.blocks_in(piece);
        self.partial.insert(
            piece,
            Partial {
                blocks: vec![FREE; usize::from(n)].into_boxed_slice(),
                received: 0,
                free: n,
            },
        );
        if let Some(o) = self.exclusive.get_mut(&piece) {
            if *o == Owner::Pending {
                *o = Owner::Peer(peer);
            }
        }
    }

    fn take_from_partials(&mut self, c: &PickCtx<'_>, budget: &mut usize, out: &mut Vec<BlockRequest>) {
        let mut cands: Vec<(u8, u16, u32)> = self
            .partial
            .iter()
            .filter(|&(&i, p)| p.free > 0 && Self::peer_may(c, i) && self.exclusive_ok(i, c.peer))
            .map(|(&i, p)| (u8::MAX - self.prio_of(i), p.free, i))
            .collect();
        cands.sort_unstable();
        for (_, _, piece) in cands {
            if *budget == 0 {
                break;
            }
            self.take_blocks(piece, c, budget, out, false);
        }
    }

    /// Take free blocks of `piece` in order. In endgame mode take already-requested ones instead.
    fn take_blocks(
        &mut self,
        piece: u32,
        c: &PickCtx<'_>,
        budget: &mut usize,
        out: &mut Vec<BlockRequest>,
        dup: bool,
    ) {
        let size = self.piece_size(piece);
        let Some(p) = self.partial.get_mut(&piece) else { return };
        for (bi, st) in p.blocks.iter_mut().enumerate() {
            if *budget == 0 {
                break;
            }
            let eligible = if dup { *st != FREE && *st != DONE && *st < MAX_REQ } else { *st == FREE };
            if !eligible {
                continue;
            }
            let begin = bi as u32 * BLOCK_SIZE;
            let req = BlockRequest {
                index: piece,
                begin,
                length: BLOCK_SIZE.min(size - begin),
            };
            if dup && (c.inflight.contains(&req) || out.contains(&req)) {
                continue;
            }
            if *st == FREE {
                p.free -= 1;
            }
            *st += 1;
            *budget -= 1;
            out.push(req);
        }
    }

    fn best_new_piece(&mut self, c: &PickCtx<'_>) -> Option<u32> {
        let mut best: Option<(u8, u16, u32)> = None;
        let mut ties = 0u64;
        let random = self.random_first > 0 && self.have.count_ones() < self.random_first;
        // The generator is moved out so the scan can borrow the rest of the picker immutably.
        let mut rng = std::mem::replace(&mut self.rng, Rng::seeded(1));
        {
            let this = &*self;
            let mut consider = |i: u32| {
                if this.prio_of(i) == 0
                    || this.partial.contains_key(&i)
                    || this.have.has(i)
                    || !Self::peer_may(c, i)
                    || !this.exclusive_ok(i, c.peer)
                {
                    return;
                }
                let key = (u8::MAX - this.prio_of(i), if random { 0 } else { this.availability(i) });
                match best {
                    Some((bp, ba, _)) if (bp, ba) < key => {}
                    Some((bp, ba, _)) if (bp, ba) == key => {
                        // Reservoir sampling among equals.
                        ties += 1;
                        if rng.below(ties + 1) == 0 {
                            best = Some((key.0, key.1, i));
                        }
                    }
                    _ => {
                        ties = 0;
                        best = Some((key.0, key.1, i));
                    }
                }
            };
            match c.only {
                Some(list) => list.iter().for_each(|&i| consider(i)),
                None => this.have.iter_zeros().for_each(&mut consider),
            }
        }
        self.rng = rng;
        best.map(|(_, _, i)| i)
    }

    /// No free block anywhere: everything wanted is at least requested once.
    fn endgame(&self) -> bool {
        if self.wanted_missing == 0 {
            return false;
        }
        let untouched = self
            .wanted_missing
            .saturating_sub(self.partial.keys().filter(|&&i| self.prio_of(i) > 0).count() as u32);
        untouched == 0 && self.partial.values().all(|p| p.free == 0)
    }

    fn take_endgame(&mut self, c: &PickCtx<'_>, budget: &mut usize, out: &mut Vec<BlockRequest>) {
        // Fewest duplicates first: spread the redundancy.
        for level in 1..MAX_REQ {
            if *budget == 0 {
                break;
            }
            let pieces: Vec<u32> = self
                .partial
                .iter()
                .filter(|&(&i, p)| {
                    Self::peer_may(c, i)
                        && !self.exclusive.contains_key(&i)
                        && p.blocks.contains(&level)
                })
                .map(|(&i, _)| i)
                .collect();
            if pieces.is_empty() && level > 3 {
                break;
            }
            for piece in pieces {
                let size = self.piece_size(piece);
                let Some(p) = self.partial.get_mut(&piece) else { continue };
                for (bi, st) in p.blocks.iter_mut().enumerate() {
                    if *budget == 0 {
                        break;
                    }
                    if *st != level {
                        continue;
                    }
                    let begin = bi as u32 * BLOCK_SIZE;
                    let req = BlockRequest {
                        index: piece,
                        begin,
                        length: BLOCK_SIZE.min(size - begin),
                    };
                    if c.inflight.contains(&req) || out.contains(&req) {
                        continue;
                    }
                    *st += 1;
                    *budget -= 1;
                    out.push(req);
                }
            }
        }
    }

    /// A block arrived from a peer.
    pub fn block_received(&mut self, piece: u32, begin: u32) -> Received {
        let Some(p) = self.partial.get_mut(&piece) else {
            return Received::Unknown;
        };
        let bi = (begin / BLOCK_SIZE) as usize;
        let Some(st) = p.blocks.get_mut(bi) else {
            return Received::Unknown;
        };
        if *st == DONE {
            return Received::Duplicate;
        }
        if *st == FREE {
            p.free -= 1;
        }
        *st = DONE;
        p.received += 1;
        Received::Accepted {
            complete: usize::from(p.received) == p.blocks.len(),
        }
    }

    /// An outstanding request will not be answered (timeout, reject, choke, disconnect, cancel).
    pub fn request_lost(&mut self, r: BlockRequest) {
        let Some(p) = self.partial.get_mut(&r.index) else { return };
        let Some(st) = p.blocks.get_mut((r.begin / BLOCK_SIZE) as usize) else { return };
        if *st == FREE || *st == DONE {
            return;
        }
        *st -= 1;
        if *st == FREE {
            p.free += 1;
        }
    }

    /// A piece passed verification.
    pub fn piece_verified(&mut self, piece: u32) {
        if self.have.set(piece) {
            self.verified_bytes += u64::from(self.piece_size(piece));
            if self.prio_of(piece) > 0 {
                self.wanted_missing = self.wanted_missing.saturating_sub(1);
                self.wanted_missing_bytes = self.wanted_missing_bytes.saturating_sub(u64::from(self.piece_size(piece)));
            }
        }
        self.partial.remove(&piece);
        self.exclusive.remove(&piece);
    }

    /// A piece failed verification. With `exclusive`, the next attempt is single-source.
    pub fn piece_failed(&mut self, piece: u32, exclusive: bool) {
        self.partial.remove(&piece);
        if exclusive && self.exclusive.len() < MAX_EXCLUSIVE {
            self.exclusive.insert(piece, Owner::Pending);
        } else if !exclusive {
            self.exclusive.remove(&piece);
        }
    }

    /// Whether the piece is exclusive now (for tests and diagnostics).
    pub fn is_exclusive(&self, piece: u32) -> bool {
        self.exclusive.contains_key(&piece)
    }

    /// A peer disconnected: exclusive pieces it owned become unowned again.
    pub fn peer_gone(&mut self, peer: u32) {
        for o in self.exclusive.values_mut() {
            if *o == Owner::Peer(peer) {
                *o = Owner::Pending;
            }
        }
    }

    /// Mark pieces verified from a recheck or resume record, without touching partial state.
    pub fn set_have(&mut self, have: Bitfield) {
        self.have = have;
        self.partial.clear();
        self.recount();
    }
}

/// Piece priorities derived from file priorities: a piece is as important as the most important
/// non-padding file it overlaps; pieces covering only skipped or padding files get 0.
pub fn piece_priorities(
    piece_len: u32,
    piece_count: u32,
    files: impl Iterator<Item = (u64, u64, u8, bool)>,
) -> Vec<u8> {
    let mut prio = vec![0u8; piece_count as usize];
    for (offset, len, p, padding) in files {
        if len == 0 || p == 0 || padding {
            continue;
        }
        let first = (offset / u64::from(piece_len)) as usize;
        let last = ((offset + len - 1) / u64::from(piece_len)) as usize;
        for slot in prio.iter_mut().take(last + 1).skip(first) {
            *slot = (*slot).max(p);
        }
    }
    prio
}

#[cfg(test)]
mod tests {
    use super::*;

    const PL: u32 = 4 * BLOCK_SIZE; // 4 blocks per piece

    fn picker(pieces: u32, have: &[u32]) -> Picker {
        let mut bf = Bitfield::new(pieces);
        for &h in have {
            bf.set(h);
        }
        let mut p = Picker::new(PL, u64::from(pieces) * u64::from(PL), bf, vec![2; pieces as usize], 64, 1);
        p.set_random_first(0); // the tests below are about rarest-first; random-first has its own
        p
    }

    fn first_pieces_picker(pieces: u32, seed: u64) -> Picker {
        Picker::new(PL, u64::from(pieces) * u64::from(PL), Bitfield::new(pieces), vec![2; pieces as usize], 64, seed)
    }

    /// Peer A has everything, peer B everything except piece 0: piece 0 is the strict rarest.
    fn skewed(p: &mut Picker, pieces: u32) {
        p.add_peer(Some(&Bitfield::have_all(pieces)));
        let mut b = Bitfield::have_all(pieces);
        b.clear(0);
        p.add_peer(Some(&b));
        // make piece 0 the rarest: it is on fewer peers than the rest
        let mut c = Bitfield::have_all(pieces);
        c.clear(0);
        p.add_peer(Some(&c));
    }

    fn complete(p: &mut Picker, piece: u32) {
        p.piece_verified(piece);
    }

    #[test]
    fn first_pieces_are_random_not_rarest() {
        // Piece 0 is rarest by far (1 peer vs 3); with rarest-first every seed would take piece 0
        // alone. Random choice must pick other pieces for some seeds, and piece 0 for some.
        let mut seen = std::collections::BTreeSet::new();
        for seed in 1..60 {
            let mut p = first_pieces_picker(16, seed);
            p.add_peer(Some(&Bitfield::have_all(16)));
            let mut b = Bitfield::new(16);
            for i in 1..16 {
                b.set(i);
            }
            p.add_peer(Some(&b));
            p.add_peer(Some(&b));
            let r = all(&mut p, 1, 1);
            seen.insert(r[0].index);
        }
        assert!(seen.len() > 4, "random pieces expected, got {seen:?}");
    }

    #[test]
    fn rarest_first_after_four_pieces() {
        for seed in 1..20 {
            let mut p = first_pieces_picker(16, seed);
            skewed(&mut p, 16);
            for i in 12..16 {
                complete(&mut p, i);
            }
            let r = all(&mut p, 1, 1);
            assert_eq!(r[0].index, 0, "seed {seed}: rarest piece after 4 complete");
        }
    }

    #[test]
    fn three_complete_pieces_are_still_random() {
        let mut seen = std::collections::BTreeSet::new();
        for seed in 1..60 {
            let mut p = first_pieces_picker(16, seed);
            skewed(&mut p, 16);
            for i in 13..16 {
                complete(&mut p, i);
            }
            seen.insert(all(&mut p, 1, 1)[0].index);
        }
        assert!(seen.len() > 3, "{seen:?}");
    }

    #[test]
    fn random_first_keeps_file_priorities_and_skips() {
        for seed in 1..40 {
            let mut p = first_pieces_picker(8, seed);
            // pieces 0..4 skipped, 4..6 normal (2), 6..8 high (7)
            p.set_priorities(vec![0, 0, 0, 0, 2, 2, 7, 7]);
            let r = all(&mut p, 1, 1);
            assert!(r[0].index >= 6, "seed {seed}: high priority first, got {}", r[0].index);
            let mut p = first_pieces_picker(8, seed);
            p.set_priorities(vec![0, 0, 0, 0, 2, 2, 2, 2]);
            for _ in 0..8 {
                let mut v = Vec::new();
                p.pick(&ctx(1, None, 4, &[]), &mut v);
                assert!(v.iter().all(|b| b.index >= 4));
            }
        }
    }

    #[test]
    fn streaming_hints_still_win_over_random_first() {
        let mut p = first_pieces_picker(16, 3);
        p.set_stream_hints(1, &[urgent(9, 11, 100, 100)], Instant::now());
        let r = all(&mut p, 1, 2);
        assert_eq!(piece_order(&r).first(), Some(&9));
    }

    fn ctx<'a>(peer: u32, has: Option<&'a Bitfield>, max: usize, inflight: &'a [BlockRequest]) -> PickCtx<'a> {
        PickCtx { peer, has, max, only: None, inflight, now: Instant::now(), fast: false }
    }

    fn all(p: &mut Picker, peer: u32, max: usize) -> Vec<BlockRequest> {
        let mut v = Vec::new();
        p.pick(&ctx(peer, None, max, &[]), &mut v);
        v
    }

    #[test]
    fn blocks_are_sequential_within_a_piece() {
        let mut p = picker(4, &[]);
        let r = all(&mut p, 1, 4);
        assert_eq!(r.len(), 4);
        assert!(r.iter().all(|b| b.index == r[0].index));
        let begins: Vec<u32> = r.iter().map(|b| b.begin).collect();
        assert_eq!(begins, vec![0, BLOCK_SIZE, 2 * BLOCK_SIZE, 3 * BLOCK_SIZE]);
    }

    #[test]
    fn rarest_piece_first() {
        let mut p = picker(4, &[]);
        let mut a = Bitfield::new(4);
        a.set(0);
        a.set(1);
        a.set(2);
        a.set(3);
        p.add_peer(Some(&a));
        let mut b = Bitfield::new(4);
        b.set(0);
        b.set(1);
        b.set(3);
        p.add_peer(Some(&b));
        // availability: 0:2 1:2 2:1 3:2 -> piece 2 first
        let r = all(&mut p, 1, 1);
        assert_eq!(r[0].index, 2);
    }

    #[test]
    fn partial_pieces_come_before_rarer_new_ones() {
        let mut p = picker(4, &[]);
        let mut a = Bitfield::new(4);
        for i in 0..4 {
            a.set(i);
        }
        p.add_peer(Some(&a));
        let mut only3 = Bitfield::new(4);
        only3.set(3);
        p.add_peer(Some(&only3));
        // Start piece 0 (avail 1) with two blocks, then ask again: must continue piece 0, even
        // though piece 3.. are not rarer; make piece 1 strictly rarer than 0 to prove the point.
        let first = all(&mut p, 1, 2);
        let started = first[0].index;
        p.peer_have(started); // started piece becomes more common than the rest
        p.peer_have(started);
        let next = all(&mut p, 2, 2);
        assert!(next.iter().all(|b| b.index == started), "{next:?}");
    }

    #[test]
    fn skipped_pieces_are_never_requested() {
        let mut bf = Bitfield::new(4);
        bf.set(1);
        let mut p = Picker::new(PL, 4 * u64::from(PL), bf, vec![2, 0, 0, 2], 64, 1);
        assert_eq!(p.wanted_missing(), 2);
        let mut got = Vec::new();
        p.pick(&ctx(1, None, 100, &[]), &mut got);
        let pieces: std::collections::BTreeSet<u32> = got.iter().map(|b| b.index).collect();
        assert_eq!(pieces.into_iter().collect::<Vec<_>>(), vec![0, 3]);
        for i in [0, 3] {
            for b in 0..4 {
                p.block_received(i, b * BLOCK_SIZE);
            }
            p.piece_verified(i);
        }
        assert!(p.wanted_done());
        assert!(!p.is_complete());
        assert_eq!(p.left_bytes(), u64::from(PL), "the skipped, missing piece still counts as left");
    }

    #[test]
    fn higher_priority_files_go_first() {
        let bf = Bitfield::new(4);
        let mut p = Picker::new(PL, 4 * u64::from(PL), bf, vec![1, 1, 3, 2], 64, 1);
        let r = all(&mut p, 1, 1);
        assert_eq!(r[0].index, 2);
    }

    #[test]
    fn lost_requests_become_free_again() {
        let mut p = picker(1, &[]);
        let r = all(&mut p, 1, 4);
        assert_eq!(r.len(), 4);
        assert!(p.endgame(), "everything is requested");
        p.request_lost(r[1]);
        let again = all(&mut p, 2, 4);
        assert_eq!(again, vec![r[1]]);
    }

    #[test]
    fn endgame_duplicates_only_what_the_peer_does_not_have_in_flight() {
        let mut p = picker(1, &[]);
        let mine = all(&mut p, 1, 4);
        assert_eq!(mine.len(), 4);
        // Peer 2 sees no free block: endgame duplicates, but never ones already in its own queue.
        let mut dup = Vec::new();
        p.pick(&ctx(2, None, 10, &mine[..1]), &mut dup);
        assert_eq!(dup.len(), 3);
        assert!(!dup.contains(&mine[0]));
        // Block arrives: the others become duplicates.
        assert_eq!(p.block_received(0, 0), Received::Accepted { complete: false });
        assert_eq!(p.block_received(0, 0), Received::Duplicate);
    }

    #[test]
    fn piece_completion_and_failure() {
        let mut p = picker(2, &[]);
        let r = all(&mut p, 1, 4);
        let piece = r[0].index;
        for (n, b) in r.iter().enumerate() {
            let res = p.block_received(b.index, b.begin);
            assert_eq!(res, Received::Accepted { complete: n == 3 });
        }
        p.piece_failed(piece, false);
        assert!(!p.have().has(piece));
        let again = all(&mut p, 1, 4);
        assert_eq!(again.len(), 4, "the whole piece is requested again");
        assert_eq!(p.block_received(again[0].index, again[0].begin), Received::Accepted { complete: false });
    }

    #[test]
    fn exclusive_pieces_have_a_single_owner() {
        let mut p = picker(1, &[]);
        let r = all(&mut p, 1, 4);
        for b in &r {
            p.block_received(b.index, b.begin);
        }
        p.piece_failed(0, true);
        assert!(p.is_exclusive(0));
        let mine = all(&mut p, 7, 2);
        assert_eq!(mine.len(), 2, "peer 7 becomes the owner");
        assert!(all(&mut p, 8, 4).is_empty(), "nobody else may touch it");
        p.peer_gone(7);
        for b in &mine {
            p.request_lost(*b);
        }
        assert_eq!(all(&mut p, 8, 4).len(), 4, "owner left: next peer takes it");
    }

    #[test]
    fn allowed_fast_restricts_the_pieces() {
        let mut p = picker(4, &[]);
        let mut v = Vec::new();
        p.pick(&PickCtx { peer: 1, has: None, max: 8, only: Some(&[2]), inflight: &[], now: Instant::now(), fast: false }, &mut v);
        assert!(!v.is_empty() && v.iter().all(|b| b.index == 2));
    }

    #[test]
    fn last_piece_is_shorter() {
        let bf = Bitfield::new(2);
        let total = u64::from(PL) + 20_000;
        let mut p = Picker::new(PL, total, bf, vec![2, 2], 64, 1);
        assert_eq!(p.piece_size(1), 20_000);
        assert_eq!(p.blocks_in(1), 2);
        let mut v = Vec::new();
        let bf1 = {
            let mut b = Bitfield::new(2);
            b.set(1);
            b
        };
        p.pick(&ctx(1, Some(&bf1), 10, &[]), &mut v);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].length, 20_000 - BLOCK_SIZE);
    }

    #[test]
    fn open_piece_cap_is_respected() {
        let bf = Bitfield::new(10);
        let mut p = Picker::new(PL, 10 * u64::from(PL), bf, vec![2; 10], 2, 1);
        let mut v = Vec::new();
        p.pick(&ctx(1, None, 100, &[]), &mut v);
        assert_eq!(p.open_pieces(), 2);
        assert_eq!(v.len(), 8);
    }

    #[test]
    fn piece_priorities_from_files() {
        // pieces of 10 bytes; files: [0,15) prio 2, [15,30) prio 0, [30,31) padding, [31,40) prio 1
        let files = vec![(0u64, 15u64, 2u8, false), (15, 15, 0, false), (30, 1, 3, true), (31, 9, 1, false)];
        let p = piece_priorities(10, 4, files.into_iter());
        // piece 0: file0; piece 1: file0 (2) + file1 (0) -> 2; piece 2: file1 only -> 0; piece 3: file3
        assert_eq!(p, vec![2, 2, 0, 1]);
    }

    // ---- deadline mode ----------------------------------------------------------------------

    fn urgent(first: u32, end: u32, deadline_ms: u64, step_ms: u64) -> UrgentRange {
        UrgentRange {
            pieces: (first, end),
            deadline: Duration::from_millis(deadline_ms),
            step: Duration::from_millis(step_ms),
            priority: HintPriority::Urgent,
        }
    }

    fn readahead(first: u32, end: u32, deadline_ms: u64, step_ms: u64) -> UrgentRange {
        UrgentRange { priority: HintPriority::ReadAhead, ..urgent(first, end, deadline_ms, step_ms) }
    }

    /// Pieces in the order their first block was handed out.
    fn piece_order(reqs: &[BlockRequest]) -> Vec<u32> {
        let mut v: Vec<u32> = Vec::new();
        for r in reqs {
            if !v.contains(&r.index) {
                v.push(r.index);
            }
        }
        v
    }

    fn pick_with(p: &mut Picker, peer: u32, max: usize, now: Instant, fast: bool) -> Vec<BlockRequest> {
        let mut v = Vec::new();
        p.pick(&PickCtx { peer, has: None, max, only: None, inflight: &[], now, fast }, &mut v);
        v
    }

    #[test]
    fn urgent_pieces_come_first_by_deadline_then_the_rest_is_rarest_first() {
        let mut p = picker(40, &[]);
        // Everyone has everything, but piece 3 is the rarest of the rest.
        let mut all_b = Bitfield::new(40);
        all_b.set_all();
        p.add_peer(Some(&all_b));
        let mut most = Bitfield::new(40);
        most.set_all();
        most.clear(3);
        p.add_peer(Some(&most));
        let now = Instant::now();
        // Two runs given out of deadline order: piece 21 (deadline 0) before 30..32 (1 s, 2 s).
        p.set_stream_hints(1, &[urgent(30, 32, 1000, 1000), urgent(21, 22, 0, 0)], now);
        let got = pick_with(&mut p, 1, 12 * 4, now, false);
        let order = piece_order(&got);
        assert_eq!(&order[..3], &[21, 30, 31], "urgent by deadline: {order:?}");
        assert_eq!(order[3], 3, "then rarest first: {order:?}");
    }

    #[test]
    fn two_streams_share_the_urgent_bandwidth_round_robin() {
        let mut p = picker(40, &[]);
        let now = Instant::now();
        p.set_stream_hints(1, &[urgent(10, 16, 0, 1000)], now);
        p.set_stream_hints(2, &[urgent(30, 36, 0, 1000)], now);
        // Many peers, one piece's worth each: A0 B0 A1 B1 ...
        let mut order = Vec::new();
        for peer in 0..8 {
            order.extend(pick_with(&mut p, peer, 4, now, false));
        }
        assert_eq!(piece_order(&order), vec![10, 30, 11, 31, 12, 32, 13, 33]);
    }

    #[test]
    fn readahead_follows_urgent_in_piece_order_and_obeys_the_open_cap() {
        let mut bf = Bitfield::new(40);
        let _ = &mut bf;
        let mut p = Picker::new(PL, 40 * u64::from(PL), Bitfield::new(40), vec![2; 40], 3, 1);
        let now = Instant::now();
        p.set_stream_hints(1, &[urgent(5, 6, 0, 0), readahead(6, 20, 1000, 100)], now);
        let got = pick_with(&mut p, 1, 100 * 4, now, false);
        let order = piece_order(&got);
        // Urgent 5, then read-ahead 6 and 7 fill the cap of 3 open pieces; the rest of the budget is
        // not used for further read-ahead (the cap), but nothing else is open either.
        assert_eq!(order, vec![5, 6, 7], "{order:?}");
        assert_eq!(p.open_pieces(), 3);
    }

    #[test]
    fn urgent_pieces_bypass_the_open_cap() {
        let mut p = Picker::new(PL, 40 * u64::from(PL), Bitfield::new(40), vec![2; 40], 2, 1);
        let now = Instant::now();
        p.set_stream_hints(1, &[urgent(10, 18, 0, 500)], now);
        let got = pick_with(&mut p, 1, 100 * 4, now, false);
        assert_eq!(piece_order(&got), (10..18).collect::<Vec<_>>());
        assert_eq!(p.open_pieces(), 8, "eight urgent pieces open with max_open = 2");
        // Without hints the cap holds.
        let mut q = Picker::new(PL, 40 * u64::from(PL), Bitfield::new(40), vec![2; 40], 2, 1);
        let got = pick_with(&mut q, 1, 100 * 4, now, false);
        assert_eq!(piece_order(&got).len(), 2);
    }

    #[test]
    fn skipped_pieces_stay_excluded_even_when_urgent() {
        let mut prio = vec![2u8; 20];
        prio[7] = 0;
        let mut p = Picker::new(PL, 20 * u64::from(PL), Bitfield::new(20), prio, 64, 1);
        let now = Instant::now();
        p.set_stream_hints(1, &[urgent(6, 9, 0, 100)], now);
        let got = pick_with(&mut p, 1, 3 * 4, now, false);
        let order = piece_order(&got);
        assert_eq!(&order[..2], &[6, 8], "{got:?}");
        assert!(!order.contains(&7));
    }

    #[test]
    fn clearing_hints_returns_to_rarest_first_and_keeps_requested_blocks() {
        let mut p = picker(20, &[]);
        let mut a = Bitfield::new(20);
        a.set_all();
        p.add_peer(Some(&a));
        let mut b = Bitfield::new(20);
        b.set_all();
        b.clear(2);
        p.add_peer(Some(&b));
        let now = Instant::now();
        p.set_stream_hints(1, &[urgent(15, 16, 0, 0)], now);
        let first = pick_with(&mut p, 1, 2, now, false);
        assert_eq!(first[0].index, 15);
        p.clear_stream_hints(1);
        assert!(!p.has_hints());
        // The urgent piece is still partial and finishes first (partials before new pieces), then 2.
        let next = pick_with(&mut p, 2, 6, now, false);
        assert_eq!(piece_order(&next), vec![15, 2]);
        assert_eq!(next.iter().filter(|r| r.index == 15).count(), 2, "its other two blocks");
    }

    #[test]
    fn late_urgent_head_is_duplicated_to_fast_peers_only() {
        let mut p = picker(20, &[]);
        let now = Instant::now();
        p.set_stream_hints(1, &[urgent(4, 9, 0, 1000)], now);
        let slow_first = pick_with(&mut p, 1, 4, now, true);
        assert_eq!(slow_first.len(), 4);
        assert!(slow_first.iter().all(|r| r.index == 4));
        // Free blocks exist in later urgent pieces, so a fast peer takes those first; ask for just
        // enough to exhaust every free urgent block (4 more pieces) and then it duplicates the head.
        let rest = pick_with(&mut p, 2, 16 + 3, now, true);
        let dups: Vec<_> = rest.iter().filter(|r| r.index == 4).collect();
        assert_eq!(dups.len(), 3, "fast peer duplicates the late head: {rest:?}");
        // A peer that is not fast gets no duplicates.
        let none = pick_with(&mut p, 3, 8, now, false);
        assert!(none.iter().all(|r| r.index != 4), "{none:?}");
        // At most URGENT_DUP requests per block: a third fast peer gets one more copy, a fourth none.
        let third = pick_with(&mut p, 4, 4, now, true);
        assert_eq!(third.iter().filter(|r| r.index == 4).count(), 4, "{third:?}");
        // Blocks 0-2 now have three requests; block 3 has two, so one more copy, then nothing.
        let fourth = pick_with(&mut p, 5, 4, now, true);
        assert_eq!(fourth.iter().filter(|r| r.index == 4).map(|r| r.begin).collect::<Vec<_>>(), vec![3 * BLOCK_SIZE]);
        let fifth = pick_with(&mut p, 6, 4, now, true);
        assert!(fifth.iter().all(|r| r.index != 4), "{fifth:?}");
    }

    #[test]
    fn urgent_head_that_is_not_late_is_not_duplicated() {
        let mut p = picker(20, &[]);
        let now = Instant::now();
        // The first missing urgent piece is due in 30 s: the player has plenty buffered.
        p.set_stream_hints(1, &[urgent(4, 6, 30_000, 1000)], now);
        let _ = pick_with(&mut p, 1, 8, now, true);
        assert!(!p.wants_duplicates(now));
        let again = pick_with(&mut p, 2, 4, now, true);
        assert!(again.iter().all(|r| r.index != 4 && r.index != 5), "{again:?}");
        // Two seconds before the deadline it is late enough.
        assert!(p.wants_duplicates(now + Duration::from_secs(29)));
    }

    #[test]
    fn urgent_piece_membership_drives_the_cancel_on_arrival() {
        let mut p = picker(20, &[]);
        let now = Instant::now();
        assert!(!p.is_urgent_piece(4));
        p.set_stream_hints(1, &[urgent(4, 6, 0, 0), readahead(6, 9, 0, 0)], now);
        assert!(p.is_urgent_piece(4) && p.is_urgent_piece(5));
        assert!(!p.is_urgent_piece(6), "read-ahead is never duplicated or cancelled");
        assert!(!p.is_urgent_piece(3));
    }

    #[test]
    fn a_picker_nobody_streams_keeps_no_hint_storage() {
        let mut p = picker(4, &[]);
        assert_eq!(p.hints.capacity(), 0);
        p.set_stream_hints(1, &[urgent(1, 2, 0, 0)], Instant::now());
        p.clear_stream_hints(1);
        assert_eq!(p.hints.capacity(), 0);
    }

    #[test]
    fn zero_length_and_empty_inputs_do_not_panic() {
        let p = Picker::new(PL, 0, Bitfield::new(0), vec![], 4, 1);
        assert!(p.is_complete());
        assert_eq!(p.left_bytes(), 0);
        let mut p2 = picker(1, &[]);
        let mut v = Vec::new();
        p2.pick(&ctx(1, None, 0, &[]), &mut v);
        assert!(v.is_empty());
        assert_eq!(p2.block_received(9, 0), Received::Unknown);
        p2.request_lost(BlockRequest { index: 9, begin: 0, length: 1 });
    }
}
