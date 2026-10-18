//! Shared helpers: synthetic torrents with deterministic content.
#![allow(dead_code)]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use trs_storage::{BLOCK_SIZE, FilePath, FileSpec, Layout, hash};

pub struct Synth {
    pub layout: Arc<Layout>,
    /// Whole torrent byte stream (padding files are zeros).
    pub stream: Vec<u8>,
    /// Flat 20-byte SHA-1 per piece.
    pub hashes: Arc<[u8]>,
    pub names: Vec<String>,
}

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    pub fn fill(&mut self, buf: &mut [u8]) {
        for c in buf.chunks_mut(8) {
            let v = self.next().to_le_bytes();
            c.copy_from_slice(&v[..c.len()]);
        }
    }
}

/// `(relative path components, length, is_padding)` -> synthetic torrent.
pub fn synth_torrent(piece_len: u32, files: &[(&[&str], u64, bool)], seed: u64) -> Synth {
    let specs = files
        .iter()
        .map(|(c, l, p)| {
            (
                FilePath::Relative(c.iter().map(|s| s.to_string()).collect()),
                *l,
                *p,
            )
        })
        .collect();
    synth_with_paths(piece_len, specs, seed)
}

/// Like [`synth_torrent`] but with arbitrary placements (custom absolute targets).
pub fn synth_with_paths(piece_len: u32, files: Vec<(FilePath, u64, bool)>, seed: u64) -> Synth {
    let mut rng = Rng(seed | 1);
    let mut stream = Vec::new();
    let mut specs = Vec::new();
    let mut names = Vec::new();
    for (path, len, pad) in files {
        let start = stream.len();
        stream.resize(start + len as usize, 0);
        if !pad {
            rng.fill(&mut stream[start..]);
        }
        names.push(match &path {
            FilePath::Relative(c) => c.join("/"),
            FilePath::Absolute(p) => p.to_string_lossy().into_owned(),
        });
        specs.push(FileSpec {
            path,
            length: len,
            is_padding: pad,
        });
    }
    let total = stream.len() as u64;
    let count = total.div_ceil(u64::from(piece_len)) as u32;
    let layout = Arc::new(Layout::new(piece_len, total, count, specs).unwrap());
    let mut hashes = Vec::new();
    for p in 0..count {
        let s = layout.piece_offset(p) as usize;
        let e = s + layout.piece_size(p) as usize;
        hashes.extend_from_slice(&hash::sha1(&stream[s..e]));
    }
    Synth {
        layout,
        stream,
        hashes: hashes.into(),
        names,
    }
}

impl Synth {
    pub fn piece(&self, p: u32) -> &[u8] {
        let s = self.layout.piece_offset(p) as usize;
        &self.stream[s..s + self.layout.piece_size(p) as usize]
    }

    /// Content of file `i` as it should be on disk.
    pub fn file_bytes(&self, i: usize) -> &[u8] {
        let s = self.layout.file_start(i) as usize;
        &self.stream[s..s + self.layout.file_len(i) as usize]
    }

    /// Writes every non-padding file below `root` (a complete torrent on disk).
    pub fn write_all_files(&self, root: &Path) {
        for i in 0..self.layout.file_count() {
            if self.layout.file_is_padding(i) {
                continue;
            }
            let p = root.join(&self.names[i]);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, self.file_bytes(i)).unwrap();
        }
    }

    pub fn blocks(&self, piece: u32) -> Vec<(u32, Vec<u8>)> {
        let data = self.piece(piece);
        data.chunks(BLOCK_SIZE as usize)
            .enumerate()
            .map(|(i, c)| (i as u32 * BLOCK_SIZE, c.to_vec()))
            .collect()
    }
}

pub fn path_of(root: &Path, s: &Synth, i: usize) -> PathBuf {
    root.join(&s.names[i])
}

/// Minimal single-future executor (parks the thread), to exercise the `.await` path.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    struct T(std::thread::Thread);
    impl Wake for T {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(T(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::park();
    }
}
