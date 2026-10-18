//! Shared machinery for the three human-editable TOML documents:
//! strict parsing with unknown-key detection, safe loading with fallback to the
//! last good copy, atomic saving with rotating backups, and mtime polling.

use std::fs;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::error::{DocError, Warning};
use crate::fsutil::{atomic_write, unique_stamped};

/// Hard cap on a document read from disk. Real files are a few KiB.
pub const MAX_DOC_BYTES: u64 = 1 << 20;
/// Timestamped backups kept per document in `backups/` (plus the single `.bak`).
pub const BACKUPS_KEEP: usize = 5;
/// Default polling interval for hot reload.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// A successfully parsed and validated document plus non-fatal findings.
#[derive(Clone, Debug, PartialEq)]
pub struct Parsed<T> {
    /// The validated value.
    pub value: T,
    /// Unknown keys and similar.
    pub warnings: Vec<Warning>,
}

/// A document stored as TOML on disk.
pub trait Document: Sized + Clone + PartialEq + Send + 'static {
    /// File name inside the data directory.
    const FILE: &'static str;
    /// Built-in defaults (used when the file is missing and nothing can be restored).
    fn defaults() -> Self;
    /// Parse and fully validate. Never panics, whatever the text is.
    fn parse_str(text: &str) -> Result<Parsed<Self>, DocError>;
    /// Validate and render to TOML text (unknown keys from the last load are merged back in).
    fn render(&self) -> Result<String, DocError>;
    /// True when `text` is a valid file of an older format version. [`DocStore::load`] then
    /// rewrites it in the current format right away, after backing up the old file (the usual
    /// `.bak` plus a timestamped copy) like any other save.
    fn file_is_outdated(_text: &str) -> bool {
        false
    }
}

/// Where a loaded document came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadSource {
    /// The file on disk, valid.
    File,
    /// The file did not exist; defaults were written to it.
    Created,
    /// The file was invalid; this last-known-good backup was used instead.
    Backup(PathBuf),
    /// The file was invalid and no backup could be used; built-in defaults.
    Defaults,
}

/// Result of [`DocStore::load`]. Loading never fails: there is always a usable value.
#[derive(Clone, Debug)]
pub struct Loaded<T> {
    /// The document to run with.
    pub value: T,
    /// Where it came from.
    pub source: LoadSource,
    /// Unknown keys etc. in the file that was used.
    pub warnings: Vec<Warning>,
    /// Why the primary file could not be used, if it could not.
    pub error: Option<DocError>,
}

enum ReadFail {
    NotFound,
    Other(DocError),
}

fn read_text(path: &Path) -> Result<String, ReadFail> {
    let meta = fs::metadata(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound { ReadFail::NotFound } else { ReadFail::Other(DocError::io(path, &e)) }
    })?;
    if meta.len() > MAX_DOC_BYTES {
        return Err(ReadFail::Other(DocError::TooLarge {
            path: path.display().to_string(),
            size: meta.len(),
            max: MAX_DOC_BYTES,
        }));
    }
    let bytes = fs::read(path).map_err(|e| {
        if e.kind() == io::ErrorKind::NotFound { ReadFail::NotFound } else { ReadFail::Other(DocError::io(path, &e)) }
    })?;
    // Re-check: the file may have grown between stat and read.
    if bytes.len() as u64 > MAX_DOC_BYTES {
        return Err(ReadFail::Other(DocError::TooLarge {
            path: path.display().to_string(),
            size: bytes.len() as u64,
            max: MAX_DOC_BYTES,
        }));
    }
    String::from_utf8(bytes).map_err(|e| {
        ReadFail::Other(DocError::Parse {
            file: "",
            line: None,
            column: None,
            message: format!("{}: not valid UTF-8 (byte {})", path.display(), e.utf8_error().valid_up_to()),
        })
    })
}

/// Storage for one document in a data directory.
#[derive(Clone, Debug)]
pub struct DocStore<T> {
    path: PathBuf,
    backups: PathBuf,
    _t: PhantomData<fn() -> T>,
}

impl<T: Document> DocStore<T> {
    /// Store for `dir/<T::FILE>`; backups go to `dir/backups/`.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        let dir = dir.as_ref();
        DocStore { path: dir.join(T::FILE), backups: dir.join("backups"), _t: PhantomData }
    }

    /// Path of the document.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn bak_path(&self) -> PathBuf {
        let mut n = self.path.file_name().unwrap_or_default().to_os_string();
        n.push(".bak");
        self.path.with_file_name(n)
    }

    fn rejected_path(&self) -> PathBuf {
        let mut n = self.path.file_name().unwrap_or_default().to_os_string();
        n.push(".rejected");
        self.path.with_file_name(n)
    }

    fn try_read(&self) -> Result<(Parsed<T>, bool), ReadFail> {
        let text = read_text(&self.path).map_err(|e| match e {
            ReadFail::Other(e) => ReadFail::Other(fix_file_name::<T>(e)),
            nf => nf,
        })?;
        let parsed = T::parse_str(&text).map_err(ReadFail::Other)?;
        let outdated = T::file_is_outdated(&text);
        Ok((parsed, outdated))
    }

    /// Read and validate the file without any fallback.
    pub fn read_strict(&self) -> Result<Parsed<T>, DocError> {
        self.try_read().map(|(p, _)| p).map_err(|e| match e {
            ReadFail::NotFound => DocError::Io { path: self.path.display().to_string(), message: "No such file".into() },
            ReadFail::Other(e) => e,
        })
    }

    fn candidates(&self) -> Vec<PathBuf> {
        let mut v = vec![self.bak_path()];
        let prefix = format!("{}.", T::FILE);
        if let Ok(rd) = fs::read_dir(&self.backups) {
            let mut names: Vec<String> = rd
                .filter_map(Result::ok)
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.starts_with(&prefix))
                .collect();
            names.sort_unstable_by(|a, b| b.cmp(a));
            v.extend(names.into_iter().map(|n| self.backups.join(n)));
        }
        v
    }

    /// Load the document, always producing a usable value.
    ///
    /// * valid file: used as is;
    /// * missing file: defaults are written and used;
    /// * invalid file: the last-known-good `.bak`, then the newest valid file in `backups/`,
    ///   then defaults; the reason is reported in [`Loaded::error`] and the broken file is left
    ///   alone for the user to fix.
    pub fn load(&self) -> Loaded<T> {
        match self.try_read() {
            Ok((p, outdated)) => {
                let mut error = None;
                if outdated {
                    // Migration: back up the old file (done by `save`) and write the new format.
                    error = self.save(&p.value).err();
                }
                Loaded { value: p.value, source: LoadSource::File, warnings: p.warnings, error }
            }
            Err(ReadFail::NotFound) => {
                let value = T::defaults();
                let error = match value.render() {
                    Ok(text) => atomic_write(&self.path, text.as_bytes()).err().map(|e| DocError::io(&self.path, &e)),
                    Err(e) => Some(e),
                };
                Loaded { value, source: LoadSource::Created, warnings: Vec::new(), error }
            }
            Err(ReadFail::Other(primary)) => {
                for cand in self.candidates() {
                    let Ok(text) = read_text(&cand) else { continue };
                    if let Ok(p) = T::parse_str(&text) {
                        return Loaded {
                            value: p.value,
                            source: LoadSource::Backup(cand),
                            warnings: p.warnings,
                            error: Some(primary),
                        };
                    }
                }
                Loaded { value: T::defaults(), source: LoadSource::Defaults, warnings: Vec::new(), error: Some(primary) }
            }
        }
    }

    /// Validate and write `value` atomically.
    ///
    /// The previous file is first preserved (only if it was itself valid, so a hand-broken file
    /// can never push a good backup out): as `<file>.bak` and as a timestamped copy in
    /// `backups/`, of which the newest [`BACKUPS_KEEP`] survive. Saving identical content is a no-op.
    pub fn save(&self, value: &T) -> Result<(), DocError> {
        let text = value.render()?;
        let io_err = |e: io::Error| DocError::io(&self.path, &e);
        match fs::read(&self.path) {
            Ok(old) => {
                if old == text.as_bytes() {
                    return Ok(());
                }
                let old_valid = old.len() as u64 <= MAX_DOC_BYTES
                    && std::str::from_utf8(&old).ok().is_some_and(|s| T::parse_str(s).is_ok());
                if old_valid {
                    atomic_write(&self.bak_path(), &old).map_err(io_err)?;
                    fs::create_dir_all(&self.backups).map_err(io_err)?;
                    let name = format!("{}.", T::FILE);
                    atomic_write(&unique_stamped(&self.backups, &name, ""), &old).map_err(io_err)?;
                    self.prune();
                } else {
                    atomic_write(&self.rejected_path(), &old).map_err(io_err)?;
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(e)),
        }
        atomic_write(&self.path, text.as_bytes()).map_err(io_err)
    }

    fn prune(&self) {
        let prefix = format!("{}.", T::FILE);
        let Ok(rd) = fs::read_dir(&self.backups) else { return };
        let mut names: Vec<String> = rd
            .filter_map(Result::ok)
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.starts_with(&prefix))
            .collect();
        names.sort_unstable_by(|a, b| b.cmp(a));
        for n in names.into_iter().skip(BACKUPS_KEEP) {
            let _ = fs::remove_file(self.backups.join(n));
        }
    }

    /// Timestamped backups currently on disk, newest first.
    pub fn backups(&self) -> Vec<PathBuf> {
        self.candidates().into_iter().skip(1).collect()
    }
}

fn fix_file_name<T: Document>(e: DocError) -> DocError {
    match e {
        DocError::Parse { file: "", line, column, message } => DocError::Parse { file: T::FILE, line, column, message },
        other => other,
    }
}

// ---------------------------------------------------------------- parsing helpers

/// Parse `text` into `T`, check the `version` key, and compute unknown keys.
///
/// `supported` is the highest `version` this build understands.
pub(crate) fn parse_toml<T>(
    file: &'static str,
    text: &str,
    supported: u32,
) -> Result<(T, toml::Table, Vec<Warning>), DocError>
where
    T: DeserializeOwned + Serialize,
{
    let table: toml::Table = toml::from_str(text).map_err(|e| to_parse_error(file, text, &e))?;
    if let Some(v) = table.get("version") {
        match v.as_integer() {
            Some(n) if n > i64::from(supported) => {
                return Err(DocError::Unsupported { file, found: u32::try_from(n).unwrap_or(u32::MAX), supported });
            }
            Some(n) if n < 0 => {
                return Err(DocError::Invalid {
                    file,
                    errors: vec![crate::error::FieldError::new("version", "must not be negative")],
                });
            }
            _ => {}
        }
    }
    let value: T = toml::from_str(text).map_err(|e| to_parse_error(file, text, &e))?;
    let canonical = toml::Table::try_from(&value).map_err(|e| DocError::Parse {
        file,
        line: None,
        column: None,
        message: format!("internal: cannot re-serialise: {e}"),
    })?;
    let mut unknown = toml::Table::new();
    let mut warnings = Vec::new();
    split_unknown(&table, &canonical, "", &mut unknown, &mut warnings);
    Ok((value, unknown, warnings))
}

fn to_parse_error(file: &'static str, text: &str, e: &toml::de::Error) -> DocError {
    let (line, column) = match e.span() {
        Some(span) => {
            let upto = &text.as_bytes()[..span.start.min(text.len())];
            let line = upto.iter().filter(|b| **b == b'\n').count() + 1;
            let col = upto.iter().rev().take_while(|b| **b != b'\n').count() + 1;
            (Some(line), Some(col))
        }
        None => (None, None),
    };
    DocError::Parse { file, line, column, message: e.message().to_string() }
}

fn join(prefix: &str, key: &str) -> String {
    if prefix.is_empty() { key.to_string() } else { format!("{prefix}.{key}") }
}

/// Walk `input` against what we serialise back. Keys we would not write are unknown: they are
/// warned about and (for tables) kept in `unknown` so saving does not drop them.
fn split_unknown(
    input: &toml::Table,
    canonical: &toml::Table,
    path: &str,
    unknown: &mut toml::Table,
    warnings: &mut Vec<Warning>,
) {
    use toml::Value;
    for (k, v) in input {
        let here = join(path, k);
        match canonical.get(k) {
            None => {
                warnings.push(Warning { path: here, message: "unknown key (kept in the file, ignored by the app)".into() });
                unknown.insert(k.clone(), v.clone());
            }
            Some(Value::Table(ct)) => {
                if let Value::Table(it) = v {
                    let mut sub = toml::Table::new();
                    split_unknown(it, ct, &here, &mut sub, warnings);
                    if !sub.is_empty() {
                        unknown.insert(k.clone(), Value::Table(sub));
                    }
                }
            }
            Some(Value::Array(ca)) => {
                if let Value::Array(ia) = v {
                    for (i, (iv, cv)) in ia.iter().zip(ca.iter()).enumerate() {
                        if let (Value::Table(it), Value::Table(ct)) = (iv, cv) {
                            // Unknown keys inside array elements are reported but not preserved:
                            // elements are re-ordered and edited through the API.
                            let mut sub = toml::Table::new();
                            let mut w = Vec::new();
                            split_unknown(it, ct, &format!("{here}[{i}]"), &mut sub, &mut w);
                            for mut x in w {
                                x.message = "unknown key (ignored; will be dropped when the app saves)".into();
                                warnings.push(x);
                            }
                        }
                    }
                }
            }
            Some(_) => {}
        }
    }
}

/// Merge preserved unknown entries into a freshly serialised table.
pub(crate) fn merge_unknown(target: &mut toml::Table, unknown: &toml::Table) {
    for (k, v) in unknown {
        match (target.get_mut(k), v) {
            (Some(toml::Value::Table(t)), toml::Value::Table(u)) => merge_unknown(t, u),
            (Some(_), _) => {} // a known key now owns this name; known data wins
            (None, _) => {
                target.insert(k.clone(), v.clone());
            }
        }
    }
}

pub(crate) fn render_table(file: &'static str, mut table: toml::Table, unknown: &toml::Table, header: &str) -> Result<String, DocError> {
    merge_unknown(&mut table, unknown);
    let body = toml::to_string(&table).map_err(|e| DocError::Parse {
        file,
        line: None,
        column: None,
        message: format!("cannot render: {e}"),
    })?;
    Ok(format!("{header}{body}"))
}

// ---------------------------------------------------------------- hot reload

/// Cheap change fingerprint: one `stat`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    mtime: Option<SystemTime>,
    len: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let m = fs::metadata(path).ok()?;
    Some(Stamp { mtime: m.modified().ok(), len: m.len() })
}

/// What a poll found.
#[derive(Clone, Debug, PartialEq)]
pub enum PollEvent<T> {
    /// The file changed and the new content is valid and different from the running one.
    Reloaded(Parsed<T>),
    /// The file changed but is invalid; the running value stays in force.
    Rejected(DocError),
}

/// Detects edits of one document by polling its mtime and size.
///
/// A poll costs a single `stat`. An invalid file is only reported once it has stayed
/// unchanged for a full interval, so an editor that is mid-write does not raise a false alarm.
/// A change that leaves the semantic content equal (including our own saves) yields no event.
pub struct Poller<T: Document> {
    store: DocStore<T>,
    seen: Option<Stamp>,
    suspect: Option<Stamp>,
    running: T,
}

impl<T: Document> Poller<T> {
    /// Start watching `store`; `running` is the value currently in force.
    pub fn new(store: DocStore<T>, running: T) -> Self {
        let seen = stamp(store.path());
        Poller { store, seen, suspect: None, running }
    }

    /// The value last accepted.
    pub fn running(&self) -> &T {
        &self.running
    }

    /// Tell the poller the app itself replaced the running value (after a UI save).
    pub fn set_running(&mut self, value: T) {
        self.running = value;
        self.seen = stamp(self.store.path());
        self.suspect = None;
    }

    /// One poll.
    pub fn poll(&mut self) -> Option<PollEvent<T>> {
        let now = stamp(self.store.path());
        if now == self.seen {
            return None;
        }
        let Some(now_stamp) = now else {
            self.seen = None;
            return Some(PollEvent::Rejected(DocError::Io {
                path: self.store.path().display().to_string(),
                message: "file was removed; keeping the running settings".into(),
            }));
        };
        match self.store.read_strict() {
            Ok(p) => {
                self.seen = Some(now_stamp);
                self.suspect = None;
                if p.value == self.running {
                    return None;
                }
                self.running = p.value.clone();
                Some(PollEvent::Reloaded(p))
            }
            Err(e) => {
                if self.suspect == Some(now_stamp) {
                    self.seen = Some(now_stamp);
                    self.suspect = None;
                    Some(PollEvent::Rejected(e))
                } else {
                    self.suspect = Some(now_stamp);
                    None
                }
            }
        }
    }
}

/// Handle to a background poll; dropping it stops the poll.
pub struct WatchHandle {
    id: u64,
    poll: Arc<Mutex<Option<PollFn>>>,
}

type PollFn = Box<dyn FnMut() + Send>;

impl WatchHandle {
    /// Stop the poll; when this returns the callback is not running and never runs again.
    pub fn stop(mut self) {
        self.shutdown();
    }
    fn shutdown(&mut self) {
        let hub = hub();
        if let Ok(mut st) = hub.state.lock() {
            st.entries.retain(|e| e.id != self.id);
        }
        hub.wake.notify_all();
        // Waits for a call in progress on the hub thread, then forgets the closure.
        if let Ok(mut f) = self.poll.lock() {
            *f = None;
        }
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One thread polls every document of the process (config, rules, trackers): three threads
/// and three wake-ups per interval became one thread and one wake-up. It starts with the first
/// watcher and ends with the last.
struct Hub {
    state: Mutex<HubState>,
    wake: Condvar,
}

struct HubState {
    entries: Vec<Entry>,
    running: bool,
    next_id: u64,
}

struct Entry {
    id: u64,
    interval: Duration,
    due: Instant,
    poll: Arc<Mutex<Option<PollFn>>>,
}

fn hub() -> &'static Hub {
    static HUB: OnceLock<Hub> = OnceLock::new();
    HUB.get_or_init(|| Hub {
        state: Mutex::new(HubState { entries: Vec::new(), running: false, next_id: 1 }),
        wake: Condvar::new(),
    })
}

fn hub_loop() {
    let hub = hub();
    let Ok(mut st) = hub.state.lock() else { return };
    loop {
        let now = Instant::now();
        let mut due_now = Vec::new();
        let mut next: Option<Instant> = None;
        for e in &mut st.entries {
            if e.due <= now {
                due_now.push(Arc::clone(&e.poll));
                e.due = now + e.interval;
            }
            next = Some(next.map_or(e.due, |n| n.min(e.due)));
        }
        if !due_now.is_empty() {
            drop(st);
            for poll in due_now {
                if let Ok(mut f) = poll.lock() {
                    if let Some(f) = f.as_mut() {
                        f();
                    }
                }
            }
            let Ok(g) = hub.state.lock() else { return };
            st = g;
            continue;
        }
        let Some(next) = next else {
            st.running = false;
            return;
        };
        let Ok((g, _)) = hub.wake.wait_timeout(st, next.saturating_duration_since(now)) else { return };
        st = g;
    }
}

/// Poll every `interval` (use [`POLL_INTERVAL`]) on the shared watcher thread and hand events to `on_event`.
pub fn spawn_watcher<T: Document>(
    mut poller: Poller<T>,
    interval: Duration,
    mut on_event: impl FnMut(PollEvent<T>) + Send + 'static,
) -> io::Result<WatchHandle> {
    let poll: Arc<Mutex<Option<PollFn>>> = Arc::new(Mutex::new(Some(Box::new(move || {
        if let Some(ev) = poller.poll() {
            on_event(ev);
        }
    }))));
    let hub = hub();
    let mut st = hub.state.lock().map_err(|_| io::Error::other("watcher hub poisoned"))?;
    let id = st.next_id;
    st.next_id += 1;
    st.entries.push(Entry { id, interval, due: Instant::now() + interval, poll: Arc::clone(&poll) });
    if !st.running {
        st.running = true;
        if let Err(e) = std::thread::Builder::new().name("trs-watch".into()).spawn(hub_loop) {
            st.running = false;
            st.entries.pop();
            return Err(e);
        }
    }
    drop(st);
    hub.wake.notify_all();
    Ok(WatchHandle { id, poll })
}
