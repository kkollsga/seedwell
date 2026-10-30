//! A swarm in a box: a `PieceSource` whose pieces "arrive" at a configurable rate, in the order a
//! real picker would use: urgent hints first (nearest deadline, round-robin between streams),
//! then read-ahead, then everything else in random (rarest-first-like) order. Pieces can be stalled.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::io;
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use trs_stream::{Geometry, PieceSource, Priority, StreamId, UrgentRange};

pub fn synth_byte(i: u64) -> u8 {
    (i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8
}

enum Content {
    File(std::fs::File),
    Synthetic,
}

#[derive(Clone, Debug)]
pub enum HintEvent {
    Set(StreamId, Vec<UrgentRange>),
    Clear(StreamId),
}

#[derive(Default)]
struct State {
    hints: HashMap<StreamId, Vec<UrgentRange>>,
    partial: HashMap<u32, u64>,
    stalled: HashSet<u32>,
    served: HashMap<StreamId, u64>,
    log: Vec<HintEvent>,
    bg: Option<u32>,
    rng: u64,
    /// Order in which pieces were completed.
    arrival: Vec<u32>,
    killed: bool,
}

pub struct FakeSource {
    geom: Geometry,
    content: Content,
    have: Vec<std::sync::atomic::AtomicBool>,
    epoch: Mutex<Option<watch::Sender<u64>>>,
    state: Mutex<State>,
    rate: AtomicU64,
    unverified_reads: AtomicUsize,
    reads: AtomicUsize,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl FakeSource {
    pub fn from_file(path: &Path, piece_length: u32, rate: u64) -> Arc<Self> {
        let f = std::fs::File::open(path).unwrap();
        let len = f.metadata().unwrap().len();
        Self::new(Content::File(f), len, piece_length, rate)
    }

    pub fn synthetic(total_len: u64, piece_length: u32, rate: u64) -> Arc<Self> {
        Self::new(Content::Synthetic, total_len, piece_length, rate)
    }

    fn new(content: Content, total_len: u64, piece_length: u32, rate: u64) -> Arc<Self> {
        let geom = Geometry { piece_length, total_len };
        let n = geom.piece_count() as usize;
        let (epoch, _) = watch::channel(0);
        let s = Arc::new(Self {
            geom,
            content,
            have: (0..n).map(|_| std::sync::atomic::AtomicBool::new(false)).collect(),
            epoch: Mutex::new(Some(epoch)),
            state: Mutex::new(State { rng: 0x1234_5678_9ABC_DEF1, ..State::default() }),
            rate: AtomicU64::new(rate),
            unverified_reads: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            task: Mutex::new(None),
        });
        let w = Arc::downgrade(&s);
        let h = tokio::spawn(async move {
            let mut last = tokio::time::Instant::now();
            let mut budget = 0f64;
            loop {
                tokio::time::sleep(Duration::from_millis(4)).await;
                let Some(s) = w.upgrade() else { return };
                let now = tokio::time::Instant::now();
                let dt = (now - last).as_secs_f64();
                last = now;
                let rate = s.rate.load(Ordering::Relaxed);
                budget = (budget + rate as f64 * dt).min(rate as f64 * 0.05 + 1.0);
                s.step(&mut budget);
            }
        });
        *s.task.lock().unwrap() = Some(h);
        s
    }

    pub fn set_rate(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }
    pub fn stall(&self, piece: u32) {
        self.state.lock().unwrap().stalled.insert(piece);
    }
    pub fn unstall(&self, piece: u32) {
        self.state.lock().unwrap().stalled.remove(&piece);
    }
    /// Pieces covering torrent bytes become available immediately.
    pub fn prefill(&self, r: Range<u64>) {
        for p in self.geom.pieces_of(r) {
            self.complete(p);
        }
    }
    pub fn have_all(&self) {
        self.prefill(0..self.geom.total_len);
    }
    pub fn unverified_reads(&self) -> usize {
        self.unverified_reads.load(Ordering::Relaxed)
    }
    pub fn read_count(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
    pub fn active_ids(&self) -> Vec<StreamId> {
        let mut v: Vec<_> = self.state.lock().unwrap().hints.iter().filter(|(_, r)| !r.is_empty()).map(|(k, _)| *k).collect();
        v.sort();
        v
    }
    pub fn hints_of(&self, id: StreamId) -> Vec<UrgentRange> {
        self.state.lock().unwrap().hints.get(&id).cloned().unwrap_or_default()
    }
    pub fn hint_log(&self) -> Vec<HintEvent> {
        self.state.lock().unwrap().log.clone()
    }
    pub fn arrival_order(&self) -> Vec<u32> {
        self.state.lock().unwrap().arrival.clone()
    }
    pub fn has_piece_at(&self, torrent_offset: u64) -> bool {
        self.has(self.geom.piece_of(torrent_offset))
    }
    pub fn piece_count_have(&self) -> usize {
        self.have.iter().filter(|b| b.load(Ordering::Relaxed)).count()
    }
    /// Drop the notification channel, as the engine does when the torrent is removed.
    pub fn kill(&self) {
        self.state.lock().unwrap().killed = true;
        self.epoch.lock().unwrap().take();
        if let Some(h) = self.task.lock().unwrap().take() {
            h.abort();
        }
    }
    pub fn expected(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut v = vec![0u8; len];
        match &self.content {
            Content::File(f) => trs_platform::fs::read_exact_at(f, &mut v, offset).unwrap(),
            Content::Synthetic => {
                for (i, b) in v.iter_mut().enumerate() {
                    *b = synth_byte(offset + i as u64);
                }
            }
        }
        v
    }

    fn complete(&self, p: u32) {
        if self.have[p as usize].swap(true, Ordering::SeqCst) {
            return;
        }
        {
            let mut st = self.state.lock().unwrap();
            st.partial.remove(&p);
            st.arrival.push(p);
        }
        if let Some(tx) = &*self.epoch.lock().unwrap() {
            tx.send_modify(|e| *e += 1);
        }
    }

    fn piece_size(&self, p: u32) -> u64 {
        let start = self.geom.piece_start(p);
        (self.geom.total_len - start).min(u64::from(self.geom.piece_length))
    }

    fn pick(&self, st: &mut State) -> Option<u32> {
        // Urgent and read-ahead, from the hints of all streams.
        let mut best: Option<((u8, Duration, u64, StreamId), u32)> = None;
        for (&sid, ranges) in &st.hints {
            let served = st.served.get(&sid).copied().unwrap_or(0);
            for r in ranges {
                let rank = if r.priority == Priority::Urgent { 0 } else { 1 };
                
                for p in r.range() {
                    if p >= self.have.len() as u32 {
                        break;
                    }
                    if self.have[p as usize].load(Ordering::Relaxed) || st.stalled.contains(&p) {
                        continue;
                    }
                    let key = (rank, r.deadline_of(p), served, sid);
                    if best.as_ref().is_none_or(|(k, _)| key < *k) {
                        best = Some((key, p));
                    }
                    break; // deadlines grow along a range: its first missing piece is its best
                }
            }
        }
        if let Some(((_, _, _, sid), p)) = best {
            *st.served.entry(sid).or_default() += 1;
            st.bg = None;
            return Some(p);
        }
        // Everything else: random order, sticking to a piece once started.
        if let Some(p) = st.bg
            && !self.have[p as usize].load(Ordering::Relaxed)
            && !st.stalled.contains(&p)
        {
            return Some(p);
        }
        let n = self.have.len() as u32;
        let missing = (0..n).filter(|&p| !self.have[p as usize].load(Ordering::Relaxed) && !st.stalled.contains(&p)).count() as u64;
        if missing == 0 {
            return None;
        }
        st.rng ^= st.rng << 13;
        st.rng ^= st.rng >> 7;
        st.rng ^= st.rng << 17;
        let mut k = st.rng % missing;
        for p in 0..n {
            if !self.have[p as usize].load(Ordering::Relaxed) && !st.stalled.contains(&p) {
                if k == 0 {
                    st.bg = Some(p);
                    return Some(p);
                }
                k -= 1;
            }
        }
        None
    }

    fn step(&self, budget: &mut f64) {
        loop {
            if *budget < 1.0 {
                return;
            }
            let done = {
                let mut st = self.state.lock().unwrap();
                if st.killed {
                    return;
                }
                let Some(p) = self.pick(&mut st) else { return };
                let need = self.piece_size(p) - st.partial.get(&p).copied().unwrap_or(0);
                let give = (*budget as u64).min(need);
                *budget -= give as f64;
                let got = st.partial.entry(p).or_default();
                *got += give;
                if *got < self.piece_size(p) {
                    return; // budget exhausted mid-piece
                }
                p
            };
            self.complete(done);
        }
    }
}

impl Drop for FakeSource {
    fn drop(&mut self) {
        if let Some(h) = self.task.lock().unwrap().take() {
            h.abort();
        }
    }
}

impl PieceSource for FakeSource {
    fn geometry(&self) -> Geometry {
        self.geom
    }
    fn has(&self, piece: u32) -> bool {
        self.have.get(piece as usize).is_some_and(|b| b.load(Ordering::SeqCst))
    }
    fn subscribe(&self) -> watch::Receiver<u64> {
        match &*self.epoch.lock().unwrap() {
            Some(tx) => tx.subscribe(),
            None => {
                let (tx, rx) = watch::channel(0);
                drop(tx);
                rx
            }
        }
    }
    fn piece_progress(&self, piece: u32) -> u32 {
        self.state.lock().unwrap().partial.get(&piece).copied().unwrap_or(0) as u32
    }
    fn read(&self, offset: u64, len: usize, mut buf: Vec<u8>) -> impl std::future::Future<Output = io::Result<Vec<u8>>> + Send {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let ok = len > 0 && offset + len as u64 <= self.geom.total_len && self.geom.pieces_of(offset..offset + len as u64).all(|p| self.has(p));
        if !ok {
            self.unverified_reads.fetch_add(1, Ordering::Relaxed);
        }
        let r = if ok {
            buf.clear();
            buf.resize(len, 0);
            match &self.content {
                Content::File(f) => trs_platform::fs::read_exact_at(f, &mut buf, offset).map(|()| buf),
                Content::Synthetic => {
                    for (i, b) in buf.iter_mut().enumerate() {
                        *b = synth_byte(offset + i as u64);
                    }
                    Ok(buf)
                }
            }
        } else {
            Err(io::Error::other("read of unverified data"))
        };
        std::future::ready(r)
    }
    fn set_urgent(&self, stream: StreamId, ranges: &[UrgentRange]) {
        let mut st = self.state.lock().unwrap();
        st.hints.insert(stream, ranges.to_vec());
        st.log.push(HintEvent::Set(stream, ranges.to_vec()));
    }
    fn clear(&self, stream: StreamId) {
        let mut st = self.state.lock().unwrap();
        st.hints.remove(&stream);
        st.log.push(HintEvent::Clear(stream));
    }
}
