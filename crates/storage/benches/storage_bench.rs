//! Storage benchmarks (harness = false): SHA-1/SHA-256 throughput, verified sequential piece
//! writes, and random 16 KiB block reads served from the page cache.
//!
//! Run: `cargo bench -p trs-storage --bench storage_bench`
//! Env: `TRS_BENCH_MB` (write test size, default 2048), `TRS_BENCH_DIRS` (':'-separated dirs;
//! default = `<repo>/bench/downloads/storage-bench` and `$TMPDIR`). Bench files are deleted after use.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use trs_storage::{
    BLOCK_SIZE, DiskConfig, DiskIo, FilePath, FileSpec, Layout, Mode, PieceOutcome, StorageError,
    TorrentConfig, TorrentHandle, WriteAck, hash,
};

fn gbps(bytes: u64, secs: f64) -> f64 {
    bytes as f64 / secs / 1e9
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Plain portable SHA-1, as the "no hardware instructions" reference point.
fn sha1_portable(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x67452301, 0xEFCDAB89, 0x98BADCFE, 0x10325476, 0xC3D2E1F0];
    let (blocks, rem) = data.as_chunks::<64>();
    let mut block = |b: &[u8]| {
        let mut w = [0u32; 80];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([b[4 * i], b[4 * i + 1], b[4 * i + 2], b[4 * i + 3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b_, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b_ & c) | (!b_ & d), 0x5A827999),
                1 => (b_ ^ c ^ d, 0x6ED9EBA1),
                2 => ((b_ & c) | (b_ & d) | (c & d), 0x8F1BBCDC),
                _ => (b_ ^ c ^ d, 0xCA62C1D6u32),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = b_.rotate_left(30);
            b_ = a;
            a = t;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b_);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    };
    for b in blocks {
        block(b);
    }
    let mut last = [0u8; 128];
    last[..rem.len()].copy_from_slice(rem);
    last[rem.len()] = 0x80;
    let n = if rem.len() < 56 { 64 } else { 128 };
    last[n - 8..n].copy_from_slice(&((data.len() as u64) * 8).to_be_bytes());
    for b in last[..n].as_chunks::<64>().0 {
        block(b);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[4 * i..4 * i + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn bench_hashing() {
    println!("== hashing ==");
    println!(
        "cpu feature (SHA instructions) detected: {}  | perf cores: {}",
        hash::sha1_hw_accelerated(),
        trs_storage::perf_core_count()
    );
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut buf = vec![0u8; 4 << 20];
    for c in buf.chunks_mut(8) {
        c.copy_from_slice(&rng.next().to_le_bytes());
    }
    assert_eq!(
        hash::sha1(&buf),
        sha1_portable(&buf),
        "reference implementation must agree"
    );

    let iters = 1024; // 4 GiB
    let time = |f: &dyn Fn(&[u8]) -> u8| {
        let t = Instant::now();
        let mut x = 0u8;
        for _ in 0..iters {
            x ^= f(&buf);
        }
        std::hint::black_box(x);
        gbps(iters as u64 * buf.len() as u64, t.elapsed().as_secs_f64())
    };
    let hw = time(&|b| hash::sha1(b)[0]);
    let sw = {
        let t = Instant::now();
        let n = 64;
        let mut x = 0u8;
        for _ in 0..n {
            x ^= sha1_portable(&buf)[0];
        }
        std::hint::black_box(x);
        gbps(n as u64 * buf.len() as u64, t.elapsed().as_secs_f64())
    };
    let s256 = time(&|b| hash::sha256(b)[0]);
    println!("SHA-1   (sha1 crate, 4 MiB pieces, 1 thread): {hw:6.2} GB/s");
    println!(
        "SHA-1   (portable reference, 1 thread)       : {sw:6.2} GB/s   -> speed-up x{:.1}",
        hw / sw
    );
    println!("SHA-256 (sha2 crate, 1 thread)               : {s256:6.2} GB/s");
    // Aggregate over perf cores.
    let threads = trs_storage::perf_core_count();
    let per = 256;
    let t = Instant::now();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                let mut x = 0u8;
                for _ in 0..per {
                    x ^= hash::sha1(&buf)[0];
                }
                std::hint::black_box(x);
            });
        }
    });
    let agg = gbps(
        (threads * per) as u64 * buf.len() as u64,
        t.elapsed().as_secs_f64(),
    );
    println!("SHA-1   ({threads} threads aggregate)                  : {agg:6.2} GB/s");
    println!(
        "verdict: SHA-1 target > 1.5 GB/s single core: {}",
        if hw > 1.5 { "MET" } else { "NOT MET" }
    );
}

fn single_file_layout(name: &str, len: u64, piece_len: u32) -> Arc<Layout> {
    let spec = FileSpec {
        path: FilePath::Relative(vec![name.into()]),
        length: len,
        is_padding: false,
    };
    Arc::new(
        Layout::new(
            piece_len,
            len,
            len.div_ceil(u64::from(piece_len)) as u32,
            [spec],
        )
        .unwrap(),
    )
}

fn bench_write(dir: &Path, total_mb: u64) {
    let piece_len: u32 = 4 << 20;
    let pieces = (total_mb << 20) / u64::from(piece_len);
    let total = pieces * u64::from(piece_len);
    // A few distinct piece contents; hashing cost does not depend on content.
    let mut rng = Rng(0xfeed_beef_1234_5678);
    let mut contents: Vec<Vec<u8>> = Vec::new();
    for _ in 0..4 {
        let mut v = vec![0u8; piece_len as usize];
        for c in v.chunks_mut(8) {
            c.copy_from_slice(&rng.next().to_le_bytes());
        }
        contents.push(v);
    }
    let mut hashes = Vec::new();
    for p in 0..pieces {
        hashes.extend_from_slice(&hash::sha1(&contents[(p % 4) as usize]));
    }
    let hashes: Arc<[u8]> = hashes.into();

    for (label, window) in [
        ("paced (6 pieces in flight)", 6usize),
        ("unpaced (cache overflows -> write-through)", usize::MAX),
    ] {
        let root = dir.join("write");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let io = DiskIo::new(DiskConfig::default());
        let layout = single_file_layout("data.bin", total, piece_len);
        let h = io.add_torrent(layout, TorrentConfig::download(&root));
        h.attach_hashes(hashes.clone()).unwrap();

        let t = Instant::now();
        let mut inflight: VecDeque<trs_storage::Pending<PieceOutcome>> = VecDeque::new();
        let mut done = 0u64;
        for p in 0..pieces as u32 {
            let data = &contents[(p % 4) as usize];
            for (i, chunk) in data.chunks(BLOCK_SIZE as usize).enumerate() {
                loop {
                    match h.write_block(p, i as u32 * BLOCK_SIZE, chunk) {
                        Ok(WriteAck::Complete(f)) => {
                            inflight.push_back(f);
                            break;
                        }
                        Ok(_) => break,
                        Err(StorageError::Busy) => std::thread::yield_now(),
                        Err(e) => panic!("{e}"),
                    }
                }
            }
            while inflight.len() >= window {
                assert!(matches!(
                    inflight.pop_front().unwrap().wait().unwrap(),
                    PieceOutcome::Verified(_)
                ));
                done += 1;
            }
        }
        while let Some(f) = inflight.pop_front() {
            assert!(matches!(f.wait().unwrap(), PieceOutcome::Verified(_)));
            done += 1;
        }
        let e1 = t.elapsed().as_secs_f64();
        h.sync().unwrap().wait().unwrap();
        let e2 = t.elapsed().as_secs_f64();
        assert_eq!(done, pieces);
        let st = io.stats();
        println!(
            "  {label:<46} {:6.2} GB/s to page cache | {:6.2} GB/s incl. fsync | write-through blocks {} | cache peak {} MiB",
            gbps(total, e1),
            gbps(total, e2),
            st.write_through_blocks,
            st.cache_peak >> 20
        );
        drop(h);
        drop(io);
        std::fs::remove_dir_all(&root).unwrap();
    }
}

fn bench_reads(dir: &Path) {
    let root = dir.join("read");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let file_len: u64 = 512 << 20;
    let piece_len: u32 = 1 << 20;
    let path = root.join("seed.bin");
    {
        let mut rng = Rng(77);
        let f = std::fs::File::create(&path).unwrap();
        let mut chunk = vec![0u8; 1 << 20];
        for i in 0..(file_len >> 20) {
            for c in chunk.chunks_mut(8) {
                c.copy_from_slice(&rng.next().to_le_bytes());
            }
            trs_platform::fs::write_all_at(&f, &chunk, i << 20).unwrap();
        }
    }
    let blocks = file_len / u64::from(BLOCK_SIZE);
    let n_reads: u64 = 400_000;
    let offsets: Vec<u64> = {
        let mut rng = Rng(5);
        (0..n_reads)
            .map(|_| rng.next() % blocks * u64::from(BLOCK_SIZE))
            .collect()
    };

    // Baseline: raw pread on one thread.
    let f = std::fs::File::open(&path).unwrap();
    let mut buf = vec![0u8; BLOCK_SIZE as usize];
    for &o in offsets.iter().take(20_000) {
        trs_platform::fs::read_exact_at(&f, &mut buf, o).unwrap(); // warm
    }
    let t = Instant::now();
    for &o in &offsets {
        trs_platform::fs::read_exact_at(&f, &mut buf, o).unwrap();
    }
    let e = t.elapsed().as_secs_f64();
    println!(
        "  raw pread, 1 thread                : {:6.2} GB/s  {:8.0} kIOPS",
        gbps(n_reads * u64::from(BLOCK_SIZE), e),
        n_reads as f64 / e / 1e3
    );

    let io = DiskIo::new(DiskConfig {
        io_threads: 4,
        ..DiskConfig::default()
    });
    let layout = single_file_layout("seed.bin", file_len, piece_len);
    let h = io.add_torrent(layout, TorrentConfig::seed(&root));
    for (label, window) in [
        ("DiskIo read_block, window 1", 1usize),
        ("DiskIo read_block, window 64", 64),
    ] {
        let t = Instant::now();
        run_reads(&h, &offsets, window, piece_len);
        let e = t.elapsed().as_secs_f64();
        println!(
            "  {label:<35}: {:6.2} GB/s  {:8.0} kIOPS",
            gbps(n_reads * u64::from(BLOCK_SIZE), e),
            n_reads as f64 / e / 1e3
        );
    }
    // Several seeding peers at once (4 submitter threads, window 32 each).
    let h = Arc::new(h);
    let t = Instant::now();
    std::thread::scope(|s| {
        for k in 0..4 {
            let h = h.clone();
            let offs = &offsets[(k * offsets.len() / 4)..((k + 1) * offsets.len() / 4)];
            s.spawn(move || run_reads(&h, offs, 32, piece_len));
        }
    });
    let e = t.elapsed().as_secs_f64();
    println!(
        "  {:<35}: {:6.2} GB/s  {:8.0} kIOPS",
        "DiskIo read_block, 4 peers x 32",
        gbps(n_reads * u64::from(BLOCK_SIZE), e),
        n_reads as f64 / e / 1e3
    );
    h.set_mode(Mode::Seed);
    drop(h);
    drop(io);
    std::fs::remove_dir_all(&root).unwrap();
}

fn run_reads(h: &TorrentHandle, offsets: &[u64], window: usize, piece_len: u32) {
    let mut inflight = VecDeque::new();
    let mut spare: Vec<Vec<u8>> = (0..window)
        .map(|_| Vec::with_capacity(BLOCK_SIZE as usize))
        .collect();
    for &o in offsets {
        let (p, b) = (
            (o / u64::from(piece_len)) as u32,
            (o % u64::from(piece_len)) as u32,
        );
        loop {
            if inflight.len() >= window {
                let f: trs_storage::Pending<Vec<u8>> = inflight.pop_front().unwrap();
                spare.push(f.wait().unwrap());
            }
            let buf = spare.pop().unwrap_or_default();
            match h.read_block(p, b, BLOCK_SIZE as usize, buf) {
                Ok(f) => {
                    inflight.push_back(f);
                    break;
                }
                Err(StorageError::Busy) => {
                    // Queue full: drain one and retry.
                    if let Some(f) = inflight.pop_front() {
                        spare.push(f.wait().unwrap());
                    }
                }
                Err(e) => panic!("{e}"),
            }
        }
    }
    for f in inflight {
        f.wait().unwrap();
    }
}

fn bench_dirs() -> Vec<PathBuf> {
    if let Ok(v) = std::env::var("TRS_BENCH_DIRS") {
        return v.split(':').map(PathBuf::from).collect();
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/downloads/storage-bench");
    let tmp = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into())
        .join("trs-storage-bench");
    vec![repo, tmp]
}

fn main() {
    if std::env::args().any(|a| a == "--list") {
        println!("storage_bench: bench");
        return;
    }
    let total_mb: u64 = std::env::var("TRS_BENCH_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048);
    bench_hashing();
    for dir in bench_dirs() {
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        println!(
            "\n== sequential verified-piece writes ({total_mb} MiB, 4 MiB pieces, 32 MiB write cache) -> {} ==",
            dir.display()
        );
        bench_write(&dir, total_mb);
        println!(
            "== random 16 KiB reads from page cache (512 MiB file) -> {} ==",
            dir.display()
        );
        bench_reads(&dir);
        let _ = std::fs::remove_dir(&dir);
    }
}
