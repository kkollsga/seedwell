//! The bounded, non-blocking channel and the writer thread.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};

use crate::{Entry, FILE_NAME};

/// Sizes. The defaults are the product's (10 MB, 3 files, 4096 queued lines).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Rotate when the file reaches this many bytes.
    pub max_bytes: u64,
    /// Files kept, the current one included.
    pub keep: usize,
    /// Lines that may wait for the writer; more are dropped.
    pub queue: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_bytes: 10 * 1024 * 1024, keep: 3, queue: 4096 }
    }
}

/// Where lines end up. The file sink is the real one; tests plug in others.
pub trait LineSink: Send {
    /// Append one line (no newline).
    fn write_line(&mut self, line: &str);
    /// Push buffered data out (called when the queue runs empty).
    fn flush(&mut self);
    /// Delete everything written so far.
    fn clear(&mut self);
}

/// `trackers.log` in a folder, rotated by size.
pub struct FileSink {
    dir: PathBuf,
    limits: Limits,
    file: Option<std::io::BufWriter<File>>,
    size: u64,
}

impl FileSink {
    /// A sink writing into `dir`.
    pub fn new(dir: PathBuf, limits: Limits) -> Self {
        FileSink { dir, limits, file: None, size: 0 }
    }

    fn path(&self, n: usize) -> PathBuf {
        if n == 0 { self.dir.join(FILE_NAME) } else { self.dir.join(format!("{FILE_NAME}.{n}")) }
    }

    fn open(&mut self) -> Option<&mut std::io::BufWriter<File>> {
        if self.file.is_none() {
            fs::create_dir_all(&self.dir).ok()?;
            let p = self.path(0);
            let f = OpenOptions::new().create(true).append(true).open(&p).ok()?;
            self.size = f.metadata().map_or(0, |m| m.len());
            self.file = Some(std::io::BufWriter::new(f));
        }
        self.file.as_mut()
    }

    fn rotate(&mut self) {
        if let Some(mut f) = self.file.take() {
            let _ = f.flush();
        }
        let keep = self.limits.keep.max(1);
        let _ = fs::remove_file(self.path(keep - 1));
        for n in (0..keep - 1).rev() {
            let _ = fs::rename(self.path(n), self.path(n + 1));
        }
        self.size = 0;
    }
}

impl LineSink for FileSink {
    fn write_line(&mut self, line: &str) {
        if self.open().is_none() {
            return;
        }
        if self.size > 0 && self.size + line.len() as u64 + 1 > self.limits.max_bytes {
            self.rotate();
        }
        let Some(f) = self.open() else { return };
        if f.write_all(line.as_bytes()).and_then(|()| f.write_all(b"\n")).is_ok() {
            self.size += line.len() as u64 + 1;
        }
    }

    fn flush(&mut self) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.flush();
        }
    }

    fn clear(&mut self) {
        self.file = None;
        self.size = 0;
        for n in 0..self.limits.keep.max(1) + 2 {
            let _ = fs::remove_file(self.path(n));
        }
    }
}

enum Msg {
    Line(u64, String),
    Flush(std::sync::mpsc::Sender<()>),
}

struct Shared {
    on: AtomicBool,
    dropped: AtomicU64,
    written: AtomicU64,
    /// Bumped by `clear`; lines queued before a clear are discarded by the writer.
    epoch: AtomicU64,
}

/// The developer log. Cheap to share; see the crate docs.
pub struct DevLog {
    sh: Arc<Shared>,
    dir: Option<PathBuf>,
    limits: Limits,
    sink: Mutex<Option<Box<dyn LineSink>>>,
    tx: OnceLock<SyncSender<Msg>>,
}

impl DevLog {
    /// A log that writes `trackers.log` into `dir` (`None`: nothing is ever written). Off.
    pub fn new(dir: Option<PathBuf>) -> Arc<DevLog> {
        Self::with_limits(dir, Limits::default())
    }

    /// Same with explicit limits (tests).
    pub fn with_limits(dir: Option<PathBuf>, limits: Limits) -> Arc<DevLog> {
        let sink: Option<Box<dyn LineSink>> = dir.clone().map(|d| Box::new(FileSink::new(d, limits)) as Box<dyn LineSink>);
        Self::build(dir, limits, sink)
    }

    /// A log that hands its lines to `sink` (tests: a stalled or recording writer). Off.
    pub fn with_sink(sink: Box<dyn LineSink>, limits: Limits) -> Arc<DevLog> {
        Self::build(None, limits, Some(sink))
    }

    fn build(dir: Option<PathBuf>, limits: Limits, sink: Option<Box<dyn LineSink>>) -> Arc<DevLog> {
        Arc::new(DevLog {
            sh: Arc::new(Shared { on: AtomicBool::new(false), dropped: AtomicU64::new(0), written: AtomicU64::new(0), epoch: AtomicU64::new(0) }),
            dir,
            limits,
            sink: Mutex::new(sink),
            tx: OnceLock::new(),
        })
    }

    /// True when developer mode is on. This is the whole cost of the log while it is off.
    #[inline]
    pub fn enabled(&self) -> bool {
        self.sh.on.load(Ordering::Relaxed)
    }

    /// Switch logging on or off. Off takes effect for the very next [`DevLog::log`] call; lines
    /// already queued are still written. The writer thread starts on the first switch on.
    pub fn set_enabled(&self, on: bool) {
        if on {
            self.start_writer();
        }
        self.sh.on.store(on, Ordering::Relaxed);
    }

    fn start_writer(&self) {
        self.tx.get_or_init(|| {
            let (tx, rx) = sync_channel(self.limits.queue.max(1));
            let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner()).take();
            let sh = self.sh.clone();
            let _ = std::thread::Builder::new().name("devlog".into()).spawn(move || writer_loop(&rx, sink, &sh));
            tx
        });
    }

    /// Mask, format and queue one entry. Never blocks: a full queue drops the line and counts it.
    /// Does nothing while the log is off.
    pub fn log(&self, entry: &Entry) {
        if !self.enabled() {
            return;
        }
        let Some(tx) = self.tx.get() else { return };
        let line = entry.to_line();
        match tx.try_send(Msg::Line(self.sh.epoch.load(Ordering::Relaxed), line)) {
            Ok(()) => {}
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                self.sh.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Lines dropped because the writer could not keep up.
    pub fn dropped(&self) -> u64 {
        self.sh.dropped.load(Ordering::Relaxed)
    }

    /// Lines the writer has handed to its sink.
    pub fn written(&self) -> u64 {
        self.sh.written.load(Ordering::Relaxed)
    }

    /// The folder `trackers.log` lives in.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// The log file path, when there is one.
    pub fn path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(FILE_NAME))
    }

    /// Delete the log (the current file and the rotated ones). Lines still queued are discarded.
    pub fn clear(&self) {
        self.sh.epoch.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = &self.dir {
            for n in 0..self.limits.keep.max(1) + 2 {
                let p = if n == 0 { d.join(FILE_NAME) } else { d.join(format!("{FILE_NAME}.{n}")) };
                let _ = fs::remove_file(p);
            }
        }
    }

    /// Wait until everything queued so far is written (tests). Returns false if the writer is
    /// not running or does not answer within `ms`.
    pub fn flush_wait(&self, ms: u64) -> bool {
        let Some(tx) = self.tx.get() else { return false };
        let (a, b) = std::sync::mpsc::channel();
        let t0 = std::time::Instant::now();
        let mut msg = Msg::Flush(a);
        loop {
            match tx.try_send(msg) {
                Ok(()) => break,
                Err(TrySendError::Full(m)) => msg = m,
                Err(TrySendError::Disconnected(_)) => return false,
            }
            if t0.elapsed().as_millis() as u64 > ms {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        b.recv_timeout(std::time::Duration::from_millis(ms)).is_ok()
    }
}

fn writer_loop(rx: &Receiver<Msg>, sink: Option<Box<dyn LineSink>>, sh: &Shared) {
    let Some(mut sink) = sink else {
        while rx.recv().is_ok() {}
        return;
    };
    let mut seen_epoch = sh.epoch.load(Ordering::SeqCst);
    let mut handle = |m: Msg, sink: &mut Box<dyn LineSink>| match m {
        Msg::Line(epoch, line) => {
            let now = sh.epoch.load(Ordering::SeqCst);
            if now != seen_epoch {
                seen_epoch = now;
                sink.clear();
            }
            if epoch == now {
                sink.write_line(&line);
                sh.written.fetch_add(1, Ordering::Relaxed);
            }
        }
        Msg::Flush(ack) => {
            sink.flush();
            let _ = ack.send(());
        }
    };
    while let Ok(m) = rx.recv() {
        handle(m, &mut sink);
        while let Ok(m) = rx.try_recv() {
            handle(m, &mut sink);
        }
        sink.flush();
    }
}
