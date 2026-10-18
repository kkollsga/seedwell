//! Write cache, hash verification, read-back, and budget pressure.

use crate::common;

use std::sync::Arc;

use common::*;
use trs_storage::{
    BLOCK_SIZE, DiskConfig, DiskIo, Mode, PieceOutcome, StorageError, TorrentConfig, WriteAck,
};

fn small_disk(cache: usize) -> DiskIo {
    DiskIo::new(DiskConfig {
        io_threads: 2,
        hash_threads: 2,
        write_cache_bytes: cache,
        fd_cache_capacity: 32,
        ..DiskConfig::default()
    })
}

fn files() -> Vec<(&'static [&'static str], u64, bool)> {
    vec![
        (&["root", "a.bin"][..], 100_000, false),
        (&[".pad", "1"][..], 28_000, true), // padding up to a 128 KiB-ish boundary
        (&["root", "empty"][..], 0, false),
        (&["root", "sub", "b.bin"][..], 250_000, false),
        (&["c.bin"][..], 5, false),
    ]
}

#[test]
fn write_then_read_roundtrip_with_padding() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(65_536, &files(), 7);
    let io = small_disk(32 << 20);
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir.path()));
    h.attach_hashes(s.hashes.clone()).unwrap();

    // Submit pieces in reverse order with shuffled blocks.
    let mut rng = Rng(99);
    let mut pending = Vec::new();
    for p in (0..s.layout.piece_count()).rev() {
        let mut blocks = s.blocks(p);
        for i in (1..blocks.len()).rev() {
            blocks.swap(i, (rng.next() % (i as u64 + 1)) as usize);
        }
        let n = blocks.len();
        for (k, (begin, data)) in blocks.into_iter().enumerate() {
            match h.write_block(p, begin, &data).unwrap() {
                WriteAck::Buffered => assert!(k + 1 < n),
                WriteAck::Complete(f) => {
                    assert_eq!(k + 1, n);
                    pending.push(f);
                }
                WriteAck::Duplicate => panic!("unexpected duplicate"),
            }
        }
    }
    for f in pending {
        assert!(matches!(block_on(f).unwrap(), PieceOutcome::Verified(_)));
    }
    assert_eq!(io.stats().cache_used, 0);
    assert_eq!(h.open_pieces(), 0);

    // On disk: exact contents, no padding file, sparse/complete lengths right.
    for i in 0..s.layout.file_count() {
        let p = path_of(dir.path(), &s, i);
        if s.layout.file_is_padding(i) {
            assert!(!p.exists() && !dir.path().join(".pad").exists());
        } else if s.layout.file_len(i) == 0 {
            assert!(
                !p.exists(),
                "zero-length files are created by finalize only"
            );
        } else {
            assert_eq!(std::fs::read(&p).unwrap(), s.file_bytes(i), "{p:?}");
        }
    }

    // Read back through the pool, including a block that straddles files and one inside padding.
    h.set_mode(Mode::Seed);
    let mut buf = Vec::new();
    for p in 0..s.layout.piece_count() {
        let size = s.layout.piece_size(p) as usize;
        let mut off = 0;
        while off < size {
            let n = (size - off).min(BLOCK_SIZE as usize);
            buf = h.read_block(p, off as u32, n, buf).unwrap().wait().unwrap();
            assert_eq!(&buf[..], &s.piece(p)[off..off + n]);
            off += n;
        }
    }
    let pad_piece = (100_000 / 65_536) as u32;
    let b = h
        .read_block(pad_piece, 40_000 - 1, 2, Vec::new())
        .unwrap()
        .wait()
        .unwrap();
    assert_eq!(b.len(), 2);
}

#[test]
fn duplicate_and_invalid_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(65_536, &files(), 8);
    let io = small_disk(32 << 20);
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir.path()));
    // No hashes yet.
    let b = s.blocks(0);
    assert!(matches!(
        h.write_block(0, 0, &b[0].1),
        Err(StorageError::NoHashes)
    ));
    h.attach_hashes(s.hashes.clone()).unwrap();
    assert!(matches!(
        h.write_block(0, 0, &b[0].1).unwrap(),
        WriteAck::Buffered
    ));
    assert!(matches!(
        h.write_block(0, 0, &b[0].1).unwrap(),
        WriteAck::Duplicate
    ));
    assert!(matches!(
        h.write_block(0, 1, &b[0].1),
        Err(StorageError::InvalidBlock)
    ));
    assert!(matches!(
        h.write_block(0, 0, &b[0].1[1..]),
        Err(StorageError::InvalidBlock)
    ));
    assert!(matches!(
        h.write_block(999, 0, &b[0].1),
        Err(StorageError::OutOfRange)
    ));
    assert!(matches!(
        h.write_block(0, 65_536, &b[0].1),
        Err(StorageError::InvalidBlock)
    ));
    assert!(matches!(
        h.attach_hashes(Arc::from(vec![0u8; 3])),
        Err(StorageError::NoHashes)
    ));
    // Abort frees the buffer.
    assert!(io.stats().cache_used > 0);
    h.abort_piece(0);
    assert_eq!(io.stats().cache_used, 0);
    h.set_mode(Mode::Seed);
    assert!(matches!(
        h.write_block(0, 0, &b[0].1),
        Err(StorageError::ReadOnly)
    ));
}

#[test]
fn hash_failure_discards_and_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(65_536, &files(), 9);
    let io = small_disk(32 << 20);
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir.path()));
    h.attach_hashes(s.hashes.clone()).unwrap();

    let mut blocks = s.blocks(1);
    blocks[2].1[17] ^= 0xff; // corrupt
    let mut done = None;
    for (b, d) in &blocks {
        if let WriteAck::Complete(f) = h.write_block(1, *b, d).unwrap() {
            done = Some(f);
        }
    }
    assert_eq!(
        block_on(done.unwrap()).unwrap(),
        PieceOutcome::HashFailed(1)
    );
    assert_eq!(io.stats().hash_failures, 1);
    assert_eq!(io.stats().cache_used, 0);
    assert_eq!(io.stats().bytes_written, 0);
    // Nothing reached disk: the files were never even created.
    assert!(!path_of(dir.path(), &s, 0).exists());
    assert!(!path_of(dir.path(), &s, 3).exists());

    // Retry with correct data succeeds.
    let mut done = None;
    for (b, d) in s.blocks(1) {
        if let WriteAck::Complete(f) = h.write_block(1, b, &d).unwrap() {
            done = Some(f);
        }
    }
    assert_eq!(done.unwrap().wait().unwrap(), PieceOutcome::Verified(1));
}

#[test]
fn write_cache_budget_pressure_writes_through_and_still_verifies() {
    let dir = tempfile::tempdir().unwrap();
    // 24 pieces of 64 KiB; budget fits only 3 piece buffers.
    let big = [("big.bin", 24 * 65_536u64 - 1000, false)];
    let spec: Vec<(&[&str], u64, bool)> = big
        .iter()
        .map(|(n, l, p)| (std::slice::from_ref(n), *l, *p))
        .collect();
    let s = synth_torrent(65_536, &spec, 21);
    let budget = 3 * 65_536;
    let io = small_disk(budget);
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir.path()));
    h.attach_hashes(s.hashes.clone()).unwrap();

    // Start every piece (all but the last block), then finish them: partial pieces beyond the
    // budget must go through to disk.
    let n = s.layout.piece_count();
    let mut last = Vec::new();
    for p in 0..n {
        let mut blocks = s.blocks(p);
        last.push(blocks.pop().unwrap());
        for (b, d) in blocks {
            assert!(matches!(
                h.write_block(p, b, &d).unwrap(),
                WriteAck::Buffered
            ));
        }
        assert!(io.stats().cache_used <= budget);
    }
    assert_eq!(h.open_pieces() as u32, n);
    assert!(io.stats().write_through_blocks > 0, "{:?}", io.stats());
    let mut pending = Vec::new();
    for (p, (b, d)) in last.into_iter().enumerate() {
        match h.write_block(p as u32, b, &d).unwrap() {
            WriteAck::Complete(f) => pending.push(f),
            other => panic!("{other:?}"),
        }
    }
    for f in pending {
        assert!(matches!(f.wait().unwrap(), PieceOutcome::Verified(_)));
    }
    let st = io.stats();
    assert!(st.cache_peak <= budget, "{st:?}");
    assert_eq!(st.cache_used, 0);
    assert_eq!(st.hash_failures, 0);
    assert_eq!(
        std::fs::read(path_of(dir.path(), &s, 0)).unwrap(),
        s.file_bytes(0)
    );
}

#[test]
fn write_through_piece_with_corruption_reports_hash_failure() {
    let dir = tempfile::tempdir().unwrap();
    let spec: Vec<(&[&str], u64, bool)> = vec![(&["x.bin"][..], 4 * 65_536, false)];
    let s = synth_torrent(65_536, &spec, 5);
    let io = small_disk(0); // everything is write-through
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir.path()));
    h.attach_hashes(s.hashes.clone()).unwrap();
    for p in 0..2 {
        let mut blocks = s.blocks(p);
        if p == 1 {
            blocks[0].1[0] ^= 1;
        }
        let mut done = None;
        for (b, d) in blocks {
            if let WriteAck::Complete(f) = h.write_block(p, b, &d).unwrap() {
                done = Some(f);
            }
        }
        let out = done.unwrap().wait().unwrap();
        if p == 0 {
            assert_eq!(out, PieceOutcome::Verified(0));
        } else {
            assert_eq!(out, PieceOutcome::HashFailed(1));
        }
    }
    assert_eq!(io.stats().cache_peak, 0);
}

#[test]
fn concurrent_writers_many_torrents() {
    let dir = tempfile::tempdir().unwrap();
    let io = Arc::new(small_disk(1 << 20));
    let mut joins = Vec::new();
    for t in 0..6u64 {
        let io = io.clone();
        let root = dir.path().join(format!("t{t}"));
        std::fs::create_dir_all(&root).unwrap();
        joins.push(std::thread::spawn(move || {
            let spec: Vec<(&[&str], u64, bool)> = vec![
                (&["d", "one"][..], 300_000 + t * 1000, false),
                (&["two"][..], 90_000, false),
            ];
            let s = synth_torrent(32_768, &spec, 100 + t);
            let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(&root));
            h.attach_hashes(s.hashes.clone()).unwrap();
            let mut pend = Vec::new();
            for p in 0..s.layout.piece_count() {
                for (b, d) in s.blocks(p) {
                    loop {
                        match h.write_block(p, b, &d) {
                            Ok(WriteAck::Complete(f)) => {
                                pend.push(f);
                                break;
                            }
                            Ok(_) => break,
                            Err(StorageError::Busy) => std::thread::yield_now(),
                            Err(e) => panic!("{e}"),
                        }
                    }
                }
            }
            for f in pend {
                assert!(matches!(f.wait().unwrap(), PieceOutcome::Verified(_)));
            }
            for i in 0..2 {
                assert_eq!(
                    std::fs::read(root.join(&s.names[i])).unwrap(),
                    s.file_bytes(i)
                );
            }
        }));
    }
    for j in joins {
        j.join().unwrap();
    }
    assert_eq!(io.stats().cache_used, 0);
}

#[test]
fn preallocate_option_reserves_full_files() {
    let dir = tempfile::tempdir().unwrap();
    let spec: Vec<(&[&str], u64, bool)> = vec![(&["p.bin"][..], 200_000, false)];
    let s = synth_torrent(65_536, &spec, 3);
    let io = small_disk(1 << 20);
    let mut cfg = TorrentConfig::download(dir.path());
    cfg.preallocate = true;
    let h = io.add_torrent(s.layout.clone(), cfg);
    h.attach_hashes(s.hashes.clone()).unwrap();
    for (b, d) in s.blocks(3) {
        h.write_block(3, b, &d).unwrap();
    }
    // Last piece verified-or-pending; the file must already have its full length.
    let len = loop {
        // The file appears (empty) a moment before preallocation sets its length.
        if let Ok(m) = std::fs::metadata(path_of(dir.path(), &s, 0))
            && m.len() > 0 {
                break m.len();
            }
        std::thread::yield_now();
    };
    assert_eq!(len, 200_000);
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path_of(dir.path(), &s, 0)).unwrap();
    assert!(
        m.blocks() * 512 >= 200_000,
        "not preallocated: {} blocks",
        m.blocks()
    );
}

#[test]
fn blocks_for_an_already_verified_piece_are_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = synth_torrent(65_536, &files(), 21);
    let io = small_disk(32 << 20);
    let h = io.add_torrent(s.layout.clone(), TorrentConfig::download(dir.path()));
    h.attach_hashes(s.hashes.clone()).unwrap();

    // Complete and verify piece 1.
    let mut done = None;
    for (b, d) in s.blocks(1) {
        if let WriteAck::Complete(f) = h.write_block(1, b, &d).unwrap() {
            done = Some(f);
        }
    }
    assert_eq!(done.unwrap().wait().unwrap(), PieceOutcome::Verified(1));
    assert_eq!(h.open_pieces(), 0);

    // An endgame duplicate arrives late: it must not open a new buffer or complete the piece again.
    let b = s.blocks(1);
    assert!(matches!(h.write_block(1, b[0].0, &b[0].1).unwrap(), WriteAck::Duplicate));
    assert_eq!(h.open_pieces(), 0, "no fresh partial piece for a verified piece");
    assert_eq!(io.stats().cache_used, 0);
    let written = io.stats().bytes_written;
    for (begin, d) in &b {
        assert!(matches!(h.write_block(1, *begin, d).unwrap(), WriteAck::Duplicate));
    }
    assert_eq!(io.stats().bytes_written, written, "nothing is written twice");
    assert_eq!(io.stats().pieces_verified, 1);

    // Other pieces are unaffected.
    let b0 = s.blocks(0);
    assert!(matches!(h.write_block(0, b0[0].0, &b0[0].1).unwrap(), WriteAck::Buffered));
    h.abort_piece(0);

    // Resume data accepted by the caller: piece 3 counts as verified too; a recheck result replaces it.
    h.set_verified([3]);
    let b3 = s.blocks(3);
    assert!(matches!(h.write_block(3, b3[0].0, &b3[0].1).unwrap(), WriteAck::Duplicate));
    h.set_verified(std::iter::empty());
    assert!(matches!(h.write_block(3, b3[0].0, &b3[0].1).unwrap(), WriteAck::Buffered));
    h.abort_piece(3);
}
