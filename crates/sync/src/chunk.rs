//! Content-defined chunking: FastCDC (v2020, Xia et al.) with SHA-256 per chunk.
//!
//! **The cut points are a wire-level contract.** Two devices must cut the same bytes the same way, or
//! a renamed or copied file would be sent again. So the gear table, the masks, the normalization level
//! and the tier rule below never change; the `golden_cut_points` test pins them.
//!
//! * **Gear table:** 256 values of splitmix64 (Steele, Lea and Flood 2014; the `SplittableRandom`
//!   finalizer) started at [`GEAR_SEED`], in order.
//! * **Rolling hash:** `h = (h << 1) + GEAR[byte]`, from 0 at `min` bytes into each chunk (the first
//!   `min` bytes are skipped, as FastCDC does). A chunk ends after the byte where `h & mask == 0`.
//! * **Normalized chunking, level 2:** from `min` up to `avg` the mask has `log2(avg) + 2` bits (cuts
//!   are 4× rarer), from `avg` up to `max` it has `log2(avg) - 2` bits (4× likelier), so chunk sizes
//!   cluster around `avg`. At `max` the chunk is cut regardless. The mask bits are spread evenly over
//!   bits 16 to 62 ([`mask`]); bit 63 stays clear, which the two-byte step needs.
//! * **Two bytes per step** (paper §3.7): `GEAR_LS[b] = GEAR[b] << 1` lets one step do
//!   `h = (h << 2) + GEAR_LS[b0]` and test it against `mask << 1` (the same test as `h' & mask` for
//!   the hash after `b0`, shifted), then add `GEAR[b1]`. It gives exactly the per-byte hash.
//!
//! Sizes scale with the file ([`Tier`]); a file keeps its tier across edits while its size stays
//! within half and double of the size that chose it ([`choose_tier`]).

use std::io;

use sha2::{Digest, Sha256};
use trs_sync_proto::model::{ChunkRef, MAX_CHUNK};

use crate::fs::FileHandle;

/// The seed of the gear table: the bytes of "Seedwell", big-endian.
pub const GEAR_SEED: u64 = 0x5365_6564_7765_6c6c;

/// The normalization level (see the module docs).
pub const NORMALIZATION: u32 = 2;

/// Block size [`chunk_file`] reads with when the caller passes 0.
pub const DEFAULT_BUF: usize = 1 << 20;

const fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const fn gear_table(shift: u32) -> [u64; 256] {
    let mut s = GEAR_SEED;
    let mut t = [0; 256];
    let mut i = 0;
    while i < 256 {
        t[i] = splitmix64(&mut s) << shift;
        i += 1;
    }
    t
}

/// The gear table.
pub static GEAR: [u64; 256] = gear_table(0);
static GEAR_LS: [u64; 256] = gear_table(1);

/// A mask with `bits` ones, spread evenly over bits 16..=62: one in 2^bits hashes matches it.
pub const fn mask(bits: u32) -> u64 {
    let mut m = 0;
    let mut i = 0;
    while i < bits {
        m |= 1 << (62 - i * 47 / bits);
        i += 1;
    }
    m
}

/// A chunk size tier: the average, and from it the minimum (avg / 4) and maximum (avg × 4).
///
/// `avg` is a power of two from [`Tier::MIN_AVG`] to [`Tier::MAX_AVG`]. The field is public so a tier
/// stored in the index can be rebuilt; a value outside that (a damaged record) is rounded to the
/// nearest valid tier by [`Tier::valid`], which [`Chunker::new`] and the size methods apply.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Tier {
    /// Average chunk size.
    pub avg: u32,
}

impl Tier {
    /// The smallest average (64 KiB).
    pub const MIN_AVG: u32 = 64 << 10;
    /// The largest average (4 MiB).
    pub const MAX_AVG: u32 = 4 << 20;

    /// The tier a file of `size` bytes gets: avg = clamp(pow2_ceil(size / 2048), 64 KiB, 4 MiB), about
    /// 2048 chunks for big files.
    pub fn for_size(size: u64) -> Tier {
        let want = (size / 2048).max(1).checked_next_power_of_two().unwrap_or(u64::MAX);
        let avg = want.clamp(u64::from(Self::MIN_AVG), u64::from(Self::MAX_AVG));
        Tier { avg: avg as u32 }
    }

    /// The tier with this average: a power of two from [`Self::MIN_AVG`] to [`Self::MAX_AVG`].
    pub fn from_avg(avg: u32) -> Option<Tier> {
        (avg.is_power_of_two() && (Self::MIN_AVG..=Self::MAX_AVG).contains(&avg)).then_some(Tier { avg })
    }

    /// This tier, with `avg` rounded up to a power of two and clamped to the valid range.
    pub fn valid(self) -> Tier {
        let avg = self.avg.checked_next_power_of_two().unwrap_or(Self::MAX_AVG);
        Tier { avg: avg.clamp(Self::MIN_AVG, Self::MAX_AVG) }
    }

    /// Smallest chunk (except a file's last).
    pub fn min(self) -> u32 {
        self.valid().avg / 4
    }

    /// Largest chunk (at most [`MAX_CHUNK`]).
    pub fn max(self) -> u32 {
        self.valid().avg * 4
    }
}

/// The tier for a new version of a file of `size` bytes.
///
/// `previous` is the tier of the file's previous version and the size that chose it, which the caller
/// stores with its local entry (it is not on the wire; for a file that arrived from another device,
/// pass `None` or that device's tier with the size it arrived at). The previous tier is kept while
/// `size` stays within 0.5× to 2× of that size, so a file that grows a little at a time keeps its
/// chunks. Returns the tier and the size to store with it (unchanged while the tier is kept).
pub fn choose_tier(size: u64, previous: Option<(Tier, u64)>) -> (Tier, u64) {
    match previous {
        Some((tier, at)) if size.saturating_mul(2) >= at && size <= at.saturating_mul(2) => (tier, at),
        _ => (Tier::for_size(size), size),
    }
}

/// Streaming chunker: [`Chunker::feed`] bytes in any pieces, then [`Chunker::finish`]. The pieces do
/// not matter: the chunks are the same as for the whole input at once. Nothing is buffered; the
/// rolling hash and SHA-256 run over the caller's slices.
#[derive(Clone)]
pub struct Chunker {
    min: usize,
    avg: usize,
    max: usize,
    mask_s: u64,
    mask_s_ls: u64,
    mask_l: u64,
    mask_l_ls: u64,
    /// Bytes in the current chunk so far.
    len: usize,
    hash: u64,
    sha: Sha256,
}

impl Chunker {
    /// A chunker for one file.
    pub fn new(tier: Tier) -> Self {
        let tier = tier.valid();
        debug_assert!(tier.max() <= MAX_CHUNK);
        let bits = tier.avg.trailing_zeros();
        let (mask_s, mask_l) = (mask(bits + NORMALIZATION), mask(bits - NORMALIZATION));
        Chunker {
            min: tier.min() as usize,
            avg: tier.avg as usize,
            max: tier.max() as usize,
            mask_s,
            mask_s_ls: mask_s << 1,
            mask_l,
            mask_l_ls: mask_l << 1,
            len: 0,
            hash: 0,
            sha: Sha256::new(),
        }
    }

    /// Feeds the next bytes; every chunk that ends inside them is appended to `out`.
    pub fn feed(&mut self, mut data: &[u8], out: &mut Vec<ChunkRef>) {
        while !data.is_empty() {
            if self.len < self.min {
                // No cut before `min`: hash for SHA-256 only.
                let take = (self.min - self.len).min(data.len());
                self.sha.update(&data[..take]);
                self.len += take;
                data = &data[take..];
                continue;
            }
            let (limit, mask, mask_ls) = if self.len < self.avg {
                (self.avg, self.mask_s, self.mask_s_ls)
            } else {
                (self.max, self.mask_l, self.mask_l_ls)
            };
            let n = (limit - self.len).min(data.len());
            match find_cut(&mut self.hash, &data[..n], mask, mask_ls) {
                Some(i) => {
                    self.sha.update(&data[..=i]);
                    self.len += i + 1;
                    data = &data[i + 1..];
                    out.push(self.emit());
                }
                None => {
                    self.sha.update(&data[..n]);
                    self.len += n;
                    data = &data[n..];
                    if self.len == self.max {
                        out.push(self.emit());
                    }
                }
            }
        }
    }

    /// The last chunk (what is left after the last cut), if any bytes are left.
    pub fn finish(mut self) -> Option<ChunkRef> {
        (self.len > 0).then(|| self.emit())
    }

    fn emit(&mut self) -> ChunkRef {
        let c = ChunkRef { len: self.len as u32, hash: self.sha.finalize_reset().into() };
        self.len = 0;
        self.hash = 0;
        c
    }
}

/// Runs the gear hash over `data`; the index of the first byte after which `h & mask == 0`.
#[inline]
fn find_cut(hash: &mut u64, data: &[u8], mask: u64, mask_ls: u64) -> Option<usize> {
    let mut h = *hash;
    let (pairs, odd) = data.as_chunks::<2>();
    for (p, b) in pairs.iter().enumerate() {
        h = (h << 2).wrapping_add(GEAR_LS[b[0] as usize]);
        if h & mask_ls == 0 {
            return Some(2 * p);
        }
        h = h.wrapping_add(GEAR[b[1] as usize]);
        if h & mask == 0 {
            return Some(2 * p + 1);
        }
    }
    if let [b] = odd {
        h = (h << 1).wrapping_add(GEAR[*b as usize]);
        if h & mask == 0 {
            return Some(data.len() - 1);
        }
    }
    *hash = h;
    None
}

/// The chunks of `data` (in memory).
pub fn chunk_bytes(data: &[u8], tier: Tier) -> Vec<ChunkRef> {
    let mut c = Chunker::new(tier);
    let mut out = Vec::new();
    c.feed(data, &mut out);
    out.extend(c.finish());
    out
}

/// The chunks of the first `size` bytes of an open file, read in blocks of `buf_size` (0:
/// [`DEFAULT_BUF`]) into one buffer. A file shorter than `size` is an error (it changed while being
/// read; the scanner tries again later).
pub fn chunk_file(fh: &dyn FileHandle, size: u64, tier: Tier, buf_size: usize) -> io::Result<Vec<ChunkRef>> {
    let buf_size = if buf_size == 0 { DEFAULT_BUF } else { buf_size };
    let mut buf = vec![0; usize::try_from(size).unwrap_or(usize::MAX).clamp(1, buf_size)];
    let mut chunker = Chunker::new(tier);
    let mut out = Vec::with_capacity(usize::try_from(size / u64::from(tier.valid().avg)).unwrap_or(0) + 1);
    let mut off = 0;
    while off < size {
        let want = usize::try_from(size - off).unwrap_or(usize::MAX).min(buf.len());
        let n = fh.read_at(off, &mut buf[..want])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("the file shrank while it was read: {off} of {size} bytes"),
            ));
        }
        chunker.feed(&buf[..n], &mut out);
        off += n as u64;
    }
    out.extend(chunker.finish());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use proptest::prelude::*;

    use super::*;
    use crate::fs::{Fs, MemFs, write_all};

    /// Deterministic test data: splitmix64 from `seed`, little-endian.
    fn random(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed;
        let mut v = Vec::with_capacity(len + 8);
        while v.len() < len {
            v.extend_from_slice(&splitmix64(&mut s).to_le_bytes());
        }
        v.truncate(len);
        v
    }

    fn lens(chunks: &[ChunkRef]) -> Vec<u32> {
        chunks.iter().map(|c| c.len).collect()
    }

    #[test]
    fn gear_table_and_masks_are_fixed() {
        // splitmix64's well-known first output for seed 0 checks the generator itself.
        assert_eq!(splitmix64(&mut 0), 0xE220_A839_7B1D_CDAF);
        assert_eq!(&GEAR[..2], &[0x3216_4987_A8EC_18CF, 0x1352_73D2_344B_E8F4]);
        for bits in 12..=24 {
            assert_eq!(mask(bits).count_ones(), bits);
            assert_eq!(mask(bits) >> 63, 0);
            assert_eq!(mask(bits) & 0xFFFF, 0);
        }
        assert!(GEAR.iter().zip(&GEAR_LS).all(|(g, l)| g << 1 == *l));
    }

    #[test]
    fn golden_cut_points() {
        let data = random(1 << 20, 0x1234_5678);
        let tier = Tier::for_size(data.len() as u64);
        assert_eq!(tier.avg, 64 << 10);
        let chunks = chunk_bytes(&data, tier);
        #[rustfmt::skip]
        let golden: &[u32] = &[
            153497, 73544, 68951, 78583, 33840, 68110, 72600, 111700, 66901, 69123, 67140, 77634, 67772, 39181,
        ];
        assert_eq!(lens(&chunks), golden, "the cut points are a wire contract: never change them");
    }

    #[test]
    fn tiers() {
        assert_eq!(Tier::for_size(0).avg, 64 << 10);
        assert_eq!(Tier::for_size(128 << 20).avg, 64 << 10);
        assert_eq!(Tier::for_size((128 << 20) + 2047).avg, 64 << 10, "size / 2048 rounds down");
        assert_eq!(Tier::for_size((128 << 20) + 2048).avg, 128 << 10);
        assert_eq!(Tier::for_size(1 << 40).avg, 4 << 20);
        assert_eq!(Tier::for_size(u64::MAX).avg, 4 << 20);
        let t = Tier::from_avg(1 << 20).unwrap();
        assert_eq!((t.min(), t.max()), (256 << 10, 4 << 20));
        assert_eq!(Tier::from_avg(4 << 20).unwrap().max(), MAX_CHUNK);
        assert!(Tier::from_avg(3 << 20).is_none() && Tier::from_avg(32 << 10).is_none());
        assert_eq!(Tier { avg: 3 << 20 }.valid().avg, 4 << 20, "a damaged record is rounded");
        assert_eq!(Tier { avg: 0 }.valid().avg, 64 << 10);
        assert_eq!(Tier { avg: u32::MAX }.max(), MAX_CHUNK);
    }

    #[test]
    fn tier_hysteresis() {
        let gb = 1u64 << 30;
        let (t, at) = choose_tier(gb, None);
        assert_eq!((t.avg, at), (512 << 10, gb));
        // Growing to just under double keeps the tier (a fresh choice would say 1 MiB).
        assert_eq!(choose_tier(2 * gb - 1, Some((t, at))), (t, gb));
        assert_eq!(choose_tier(2 * gb, Some((t, at))), (t, gb));
        assert_eq!(choose_tier(gb / 2, Some((t, at))), (t, gb));
        let (t2, at2) = choose_tier(2 * gb + 2048, Some((t, at)));
        assert_eq!((t2.avg, at2), (2 << 20, 2 * gb + 2048));
        let (t3, _) = choose_tier(gb / 2 - 1, Some((t, at)));
        assert_eq!(t3.avg, 256 << 10);
    }

    #[test]
    fn small_and_empty_files() {
        let tier = Tier::for_size(0);
        assert!(chunk_bytes(&[], tier).is_empty());
        let small = random(tier.min() as usize, 1);
        let c = chunk_bytes(&small, tier);
        assert_eq!(lens(&c), [tier.min()]);
        assert_eq!(c[0].hash, <[u8; 32]>::from(Sha256::digest(&small)));
    }

    #[test]
    fn sizes_stay_within_min_and_max() {
        let tier = Tier::for_size(0);
        for data in [random(4 << 20, 7), vec![0; 3 << 20], random(1 << 20, 8).repeat(3)] {
            let c = chunk_bytes(&data, tier);
            assert_eq!(c.iter().map(|c| u64::from(c.len)).sum::<u64>(), data.len() as u64);
            let (last, rest) = c.split_last().unwrap();
            assert!(rest.iter().all(|c| c.len > tier.min() && c.len <= tier.max()), "{:?}", lens(&c));
            assert!(last.len <= tier.max());
        }
        // Zeros never match a mask: every chunk is a maximum one.
        assert!(chunk_bytes(&vec![0; 3 << 20], tier).iter().all(|c| c.len == tier.max()));
    }

    #[test]
    fn chunk_file_reads_in_blocks() {
        let fs = MemFs::new();
        let data = random(3 << 20, 9);
        write_all(&fs, "f", &data).unwrap();
        let fh = fs.open_read("f").unwrap();
        let tier = Tier::for_size(0);
        let want = chunk_bytes(&data, tier);
        for buf in [0, 1000, 1 << 16, 8 << 20] {
            assert_eq!(chunk_file(fh.as_ref(), data.len() as u64, tier, buf).unwrap(), want, "buf {buf}");
        }
        let e = chunk_file(fh.as_ref(), data.len() as u64 + 1, tier, 0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
    }

    /// The per-byte gear loop the two-byte step must agree with.
    fn find_cut_simple(hash: &mut u64, data: &[u8], mask: u64) -> Option<usize> {
        for (i, &b) in data.iter().enumerate() {
            *hash = (*hash << 1).wrapping_add(GEAR[b as usize]);
            if *hash & mask == 0 {
                return Some(i);
            }
        }
        None
    }

    fn config(cases: u32) -> ProptestConfig {
        ProptestConfig { cases, failure_persistence: None, ..ProptestConfig::default() }
    }

    proptest! {
        #![proptest_config(config(64))]

        #[test]
        fn two_byte_step_equals_per_byte(
            data in proptest::collection::vec(any::<u8>(), 0..600),
            bits in 2u32..9,
            h0: u64,
        ) {
            let m = mask(bits);
            let (mut a, mut b) = (h0, h0);
            let ca = find_cut(&mut a, &data, m, m << 1);
            let cb = find_cut_simple(&mut b, &data, m);
            prop_assert_eq!(ca, cb);
            if ca.is_none() {
                prop_assert_eq!(a, b);
            }
        }

        #[test]
        fn streaming_in_odd_pieces_is_the_same(
            seed: u64,
            pieces in proptest::collection::vec(1usize..100_000, 1..40),
        ) {
            let data = random(2 << 20, seed);
            let tier = Tier::for_size(0);
            let mut c = Chunker::new(tier);
            let mut out = Vec::new();
            let (mut off, mut p) = (0, 0);
            while off < data.len() {
                let n = pieces[p % pieces.len()].min(data.len() - off);
                c.feed(&data[off..off + n], &mut out);
                off += n;
                p += 1;
            }
            out.extend(c.finish());
            prop_assert_eq!(out, chunk_bytes(&data, tier));
        }
    }

    proptest! {
        #![proptest_config(config(8))]

        #[test]
        fn one_inserted_byte_changes_few_chunks(seed: u64, at in 0usize..65_536, byte: u8) {
            let old = random(4 << 20, seed);
            let mut new = old.clone();
            new.insert(at, byte);
            let tier = Tier::for_size(old.len() as u64);
            let before: BTreeSet<[u8; 32]> = chunk_bytes(&old, tier).iter().map(|c| c.hash).collect();
            let after = chunk_bytes(&new, tier);
            let changed = after.iter().filter(|c| !before.contains(&c.hash)).count();
            prop_assert!(changed <= 3, "{} of {} chunks changed", changed, after.len());
        }
    }

    /// `cargo test --release -p trs-sync chunk_bench -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark: run in release by hand"]
    fn chunk_bench() {
        use std::time::Instant;
        const SIZE: usize = 256 << 20;
        let data = random(SIZE, 42);
        let tier = Tier::for_size(SIZE as u64);
        let mbs = |bytes: usize, t: Instant| bytes as f64 / t.elapsed().as_secs_f64() / 1e6;

        let t = Instant::now();
        let mut h = 0;
        let mut cuts = 0;
        let mut rest = &data[..];
        let m = mask(tier.avg.trailing_zeros());
        while let Some(i) = find_cut(&mut h, rest, m, m << 1) {
            cuts += 1;
            rest = &rest[i + 1..];
        }
        println!("gear only:            {:7.0} MB/s ({cuts} cuts)", mbs(SIZE, t));

        let t = Instant::now();
        std::hint::black_box(Sha256::digest(&data));
        println!("SHA-256 only:         {:7.0} MB/s", mbs(SIZE, t));

        let t = Instant::now();
        let mut c = Chunker::new(tier);
        let mut out = Vec::new();
        for block in data.chunks(1 << 20) {
            c.feed(block, &mut out);
        }
        out.extend(c.finish());
        let avg_kib = SIZE / out.len() / 1024;
        println!("chunk + hash, 1 thread: {:7.0} MB/s ({} chunks, avg {avg_kib} KiB)", mbs(SIZE, t), out.len());

        let t = Instant::now();
        std::thread::scope(|s| {
            for half in data.chunks(SIZE / 2) {
                s.spawn(move || std::hint::black_box(chunk_bytes(half, tier)));
            }
        });
        println!("chunk + hash, 2 threads (two files): {:7.0} MB/s", mbs(SIZE, t));
        println!("SHA-256 hardware: {}", sha_hw());
    }

    fn sha_hw() -> bool {
        #[cfg(target_arch = "aarch64")]
        {
            std::arch::is_aarch64_feature_detected!("sha2")
        }
        #[cfg(not(target_arch = "aarch64"))]
        {
            false
        }
    }
}
