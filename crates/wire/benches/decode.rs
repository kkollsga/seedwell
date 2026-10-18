//! Decode throughput for `piece` messages (plain harness, no criterion dependency).
//!
//! Run: `cargo bench -p trs-wire`. Numbers are wire bytes per second through the
//! decoder; the decoder is zero-copy, so this measures header parsing + framing, not
//! memcpy. A memcpy and a SHA-1 line are printed for scale: those, not framing, are
//! what bounds a real download.

use bytes::BytesMut;
use sha1::{Digest, Sha1};
use std::hint::black_box;
use std::time::{Duration, Instant};
use trs_wire::{Decoder, Limits, Message, piece_header};

const BLOCK: usize = 16 * 1024;
const FRAMES: usize = 4096; // 64 MiB of blocks

fn run(name: &str, bytes_per_iter: usize, mut f: impl FnMut() -> usize) {
    // Warm up, then run for ~1 s.
    black_box(f());
    let start = Instant::now();
    let mut iters = 0u64;
    let mut sink = 0usize;
    while start.elapsed() < Duration::from_secs(1) {
        sink = sink.wrapping_add(f());
        iters += 1;
    }
    black_box(sink);
    let secs = start.elapsed().as_secs_f64();
    let total = bytes_per_iter as f64 * iters as f64;
    println!(
        "{name:<44} {:>9.2} GB/s  ({:.0} ns/iter)",
        total / secs / 1e9,
        secs * 1e9 / iters as f64
    );
}

fn main() {
    let block = vec![0xA5u8; BLOCK];
    let mut stream = Vec::with_capacity(FRAMES * (BLOCK + 13));
    for i in 0..FRAMES {
        stream.extend_from_slice(&piece_header(
            i as u32 / 16,
            (i as u32 % 16) * BLOCK as u32,
            BLOCK as u32,
        ));
        stream.extend_from_slice(&block);
    }
    let wire = stream.len();
    let dec = Decoder::new(Limits::default());

    println!(
        "trs-wire decode benchmark: {} frames x {} B blocks = {} MiB",
        FRAMES,
        BLOCK,
        wire >> 20
    );

    run("decode_ref (borrowed views, 16 KiB blocks)", wire, || {
        let mut buf = &stream[..];
        let mut n = 0;
        while let Ok(Some((m, used))) = dec.decode_ref(buf) {
            if let Message::Piece { data, .. } = m {
                n += data.len();
            }
            buf = &buf[used..];
        }
        n
    });

    // Owned path: split_to + freeze + slice (atomic refcounts). The buffer copy-in is
    // done outside the timed region (it is the socket read, not the decoder).
    {
        let mut timed = Duration::ZERO;
        let mut iters = 0u64;
        let mut sink = 0usize;
        while timed < Duration::from_secs(1) {
            let mut buf = BytesMut::from(&stream[..]);
            let t = Instant::now();
            while let Ok(Some(m)) = dec.decode(&mut buf) {
                if let Message::Piece { data, .. } = m {
                    sink += data.len();
                }
            }
            timed += t.elapsed();
            iters += 1;
        }
        black_box(sink);
        println!(
            "{:<44} {:>9.2} GB/s  ({:.0} ns/iter)",
            "decode (Bytes slices from BytesMut)",
            wire as f64 * iters as f64 / timed.as_secs_f64() / 1e9,
            timed.as_secs_f64() * 1e9 / iters as f64
        );
    }

    // Small messages: messages per second matter more than GB/s here.
    let mut small = Vec::new();
    for i in 0..100_000u32 {
        Message::<&[u8]>::Have(i).encode(&mut small);
    }
    let small_len = small.len();
    run("decode_ref (100k `have` messages)", small_len, || {
        let mut buf = &small[..];
        let mut n = 0;
        while let Ok(Some((m, used))) = dec.decode_ref(buf) {
            if let Message::Have(i) = m {
                n += i as usize;
            }
            buf = &buf[used..];
        }
        n
    });
    println!("  (100k messages per iteration => Mmsg/s = 100000 / ns-per-iter * 1000)");

    // Context lines.
    let mut dst = vec![0u8; stream.len()];
    run("baseline: memcpy of the same bytes", wire, || {
        dst.copy_from_slice(&stream);
        dst[wire / 2] as usize
    });
    run("baseline: SHA-1 of the same bytes", wire, || {
        Sha1::digest(&stream)[0] as usize
    });
}
