//! [`MemFs`]: an in-memory filesystem for tests and the simulator.
//!
//! Deterministic: inode numbers come from a counter, times from the injected [`Clock`], and every map
//! is a `BTreeMap`, so the same operations give the same results on every run.
//!
//! # Crashes
//!
//! The filesystem keeps a *durable* image next to the live one. [`MemFs::snapshot`] copies the live
//! tree into it (a checkpoint: everything up to here reached the disk), and [`MemFs::restore`]
//! simulates a crash and reboot: the live tree becomes the durable image again. In between,
//! [`FileHandle::sync_all`] on a file that exists in the durable image copies that file's content and
//! metadata into it, so data written and synced since the snapshot survives the crash, and unsynced
//! data is lost. [`FileHandle::sync_barrier`] is weaker: what it covers becomes durable only at the next full
//! flush (`sync_all` of any file, or `sync_dir`), as a write barrier on a real drive is ordered but not yet on
//! the medium; a crash before that loses it. Namespace changes (create, rename, link, remove) become durable at the next
//! snapshot, or when [`Fs::sync_dir`] syncs their directory, which is the pessimistic end of what
//! real filesystems do: a crash can undo a rename whose directory was not synced. A node a synced
//! directory names for the first time becomes durable with it (a directory with no entries yet). Handles opened before a restore are dead afterwards (their calls
//! fail). A new `MemFs` starts with an empty durable image.
//!
//! # Differences from a real filesystem
//!
//! * Symlinks are never followed, not even in the middle of a path (that component is "not a
//!   directory"), and opening or copying a symlink is refused.
//! * [`Fs::copy`] keeps the source's mtime and exec bit, as `std::fs::copy` does on macOS.
//! * There are no permissions besides the exec bit.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use trs_sync_proto::model::FileKind;

use super::{DirEntry, FileHandle, Fs, Listing, Meta, components};
use crate::path;

/// Where times come from: Unix nanoseconds, called once per operation that sets a time.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// A clock the test moves by hand.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    /// A clock showing `start_ns`.
    pub fn new(start_ns: i64) -> Self {
        ManualClock(Arc::new(AtomicI64::new(start_ns)))
    }

    /// The time it shows.
    pub fn now(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }

    /// Sets the time.
    pub fn set(&self, ns: i64) {
        self.0.store(ns, Ordering::SeqCst);
    }

    /// Moves the time forward by `ns`.
    pub fn advance(&self, ns: i64) {
        self.0.fetch_add(ns, Ordering::SeqCst);
    }

    /// This clock as a [`Clock`] for [`MemFsOptions`].
    pub fn clock(&self) -> Clock {
        let c = self.0.clone();
        Arc::new(move || c.load(Ordering::SeqCst))
    }
}

/// How a [`MemFs`] behaves.
#[derive(Clone)]
pub struct MemFsOptions {
    /// Lookups fold case (names keep the spelling they were created with), like APFS's default.
    pub case_insensitive: bool,
    /// Lookups compare NFC forms (names keep their own form), like APFS. Off: names are bytes, like ext4.
    pub normalization_insensitive: bool,
    /// The time source. The default ticks: it starts at 2026-01-01 and moves 1 µs on every call, so
    /// every change gets a new mtime and ctime.
    pub clock: Clock,
}

impl Default for MemFsOptions {
    fn default() -> Self {
        let t = Arc::new(AtomicI64::new(1_767_225_600_000_000_000));
        MemFsOptions {
            case_insensitive: false,
            normalization_insensitive: false,
            clock: Arc::new(move || t.fetch_add(1_000, Ordering::SeqCst)),
        }
    }
}

impl fmt::Debug for MemFsOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemFsOptions")
            .field("case_insensitive", &self.case_insensitive)
            .field("normalization_insensitive", &self.normalization_insensitive)
            .finish_non_exhaustive()
    }
}

/// The in-memory filesystem. Cloning gives another handle to the same tree.
#[derive(Clone)]
pub struct MemFs {
    inner: Arc<Inner>,
}

impl fmt::Debug for MemFs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemFs").field("options", &self.inner.opts).finish_non_exhaustive()
    }
}

struct Inner {
    opts: MemFsOptions,
    state: Mutex<State>,
}

struct State {
    live: Tree,
    durable: Tree,
    /// Bumped by every restore; handles of an older generation are dead.
    generation: u64,
    /// Path key → the failures still to come.
    faults: BTreeMap<String, Fault>,
    /// Files behind a write barrier since the last full flush: inode → content, mtime, ctime, exec bit.
    barriered: BTreeMap<u64, (Vec<u8>, i64, i64, bool)>,
}

impl State {
    /// A full flush: what is behind a barrier reaches the durable image (files it already holds).
    fn flush_barriers(&mut self) {
        for (ino, (data, mtime, ctime, exec)) in std::mem::take(&mut self.barriered) {
            if let Some(n) = self.durable.nodes.get_mut(&ino)
                && let Data::File(old) = &mut n.data
            {
                *old = data;
                n.mtime = mtime;
                n.ctime = ctime;
                n.exec = exec;
            }
        }
    }
}

struct Fault {
    left: u32,
    kind: io::ErrorKind,
}

#[derive(Clone)]
struct Tree {
    nodes: BTreeMap<u64, Node>,
    next_ino: u64,
}

#[derive(Clone)]
struct Node {
    data: Data,
    mtime: i64,
    ctime: i64,
    exec: bool,
    dataless: bool,
    /// Directory entries naming this node.
    nlink: u32,
    /// Open handles; an unlinked file lives on until the last one closes.
    open: u32,
}

#[derive(Clone)]
enum Data {
    File(Vec<u8>),
    /// Key → entry.
    Dir(BTreeMap<String, Entry>),
    Link(String),
}

#[derive(Clone)]
struct Entry {
    /// The spelling it was created with.
    name: String,
    ino: u64,
}

const ROOT: u64 = 1;

fn err(kind: io::ErrorKind, rel: &str, what: &str) -> io::Error {
    io::Error::new(kind, format!("{rel}: {what}"))
}

impl Tree {
    fn new(now: i64) -> Self {
        let root = Node::new(Data::Dir(BTreeMap::new()), now);
        Tree { nodes: BTreeMap::from([(ROOT, root)]), next_ino: ROOT + 1 }
    }

    fn node(&self, ino: u64) -> &Node {
        // Every entry points at a node, and handles keep theirs alive.
        &self.nodes[&ino]
    }

    fn node_mut(&mut self, ino: u64) -> &mut Node {
        self.nodes.get_mut(&ino).expect("entries and handles point at live nodes")
    }

    fn dir(&self, ino: u64) -> Option<&BTreeMap<String, Entry>> {
        match &self.node(ino).data {
            Data::Dir(d) => Some(d),
            _ => None,
        }
    }

    fn dir_mut(&mut self, ino: u64) -> &mut BTreeMap<String, Entry> {
        match &mut self.node_mut(ino).data {
            Data::Dir(d) => d,
            _ => unreachable!("callers check that the node is a directory"),
        }
    }

    /// Walks `keys` from the root: `Ok(None)` when something is missing or not a directory on the way.
    fn find(&self, keys: &[String]) -> Option<u64> {
        let mut ino = ROOT;
        for k in keys {
            ino = self.dir(ino)?.get(k)?.ino;
        }
        Some(ino)
    }

    /// The directory that holds the last component of `keys`.
    fn parent(&self, rel: &str, keys: &[String]) -> io::Result<u64> {
        let Some((_, up)) = keys.split_last() else {
            return Err(err(io::ErrorKind::InvalidInput, rel, "the folder root"));
        };
        let mut ino = ROOT;
        for k in up {
            let d = self.dir(ino).ok_or_else(|| err(io::ErrorKind::NotADirectory, rel, "a parent is not a directory"))?;
            ino = d.get(k).ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such directory"))?.ino;
        }
        if self.dir(ino).is_none() {
            return Err(err(io::ErrorKind::NotADirectory, rel, "the parent is not a directory"));
        }
        Ok(ino)
    }

    fn add(&mut self, node: Node) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        self.nodes.insert(ino, node);
        ino
    }

    /// One fewer name for `ino`; forgets it when nothing refers to it any more.
    fn unlink(&mut self, ino: u64, now: i64) {
        let n = self.node_mut(ino);
        n.nlink -= 1;
        n.ctime = now;
        if n.nlink == 0 && n.open == 0 {
            self.nodes.remove(&ino);
        }
    }

    fn touch(&mut self, ino: u64, now: i64) {
        let n = self.node_mut(ino);
        n.mtime = now;
        n.ctime = now;
    }
}

impl Node {
    fn new(data: Data, now: i64) -> Self {
        Node { data, mtime: now, ctime: now, exec: false, dataless: false, nlink: 1, open: 0 }
    }
}

impl MemFs {
    /// A case-sensitive, normalization-sensitive filesystem on the default ticking clock.
    pub fn new() -> Self {
        Self::with_options(MemFsOptions::default())
    }

    /// A filesystem with these options.
    pub fn with_options(opts: MemFsOptions) -> Self {
        let now = (opts.clock)();
        let state = State { live: Tree::new(now), durable: Tree::new(now), generation: 0, faults: BTreeMap::new(), barriered: BTreeMap::new() };
        MemFs { inner: Arc::new(Inner { opts, state: Mutex::new(state) }) }
    }

    /// Checkpoint: the current tree is on the disk (see the module docs).
    pub fn snapshot(&self) {
        let mut st = self.lock();
        st.durable = st.live.clone();
        st.barriered.clear();
    }

    /// Crash and reboot: back to the last snapshot, plus file data synced since (see the module docs).
    pub fn restore(&self) {
        let mut st = self.lock();
        let next = st.live.next_ino.max(st.durable.next_ino);
        let mut t = st.durable.clone();
        // Inode numbers are never reused, so a test can tell a file from the one before the crash.
        t.next_ino = next;
        t.nodes.retain(|_, n| n.nlink > 0);
        for n in t.nodes.values_mut() {
            n.open = 0;
        }
        st.live = t;
        st.generation += 1;
        st.barriered.clear();
    }

    /// Makes the next `n` operations that name `rel` fail with `kind` (before they change anything).
    /// Handle calls count as operations on the path the handle was opened with.
    pub fn fail_next(&self, rel: &str, n: u32, kind: io::ErrorKind) {
        let k = self.path_key(rel);
        let mut st = self.lock();
        if n == 0 {
            st.faults.remove(&k);
        } else {
            st.faults.insert(k, Fault { left: n, kind });
        }
    }

    /// Marks a file as a cloud placeholder: `lstat` says `dataless`, and opening or copying it fails
    /// (on a Mac that would download it).
    pub fn set_dataless(&self, rel: &str, dataless: bool) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let mut st = self.lock();
        let ino = st.live.find(&keys).ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such file"))?;
        st.live.node_mut(ino).dataless = dataless;
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now(&self) -> i64 {
        (self.inner.opts.clock)()
    }

    fn name_key(&self, name: &str) -> String {
        let o = &self.inner.opts;
        match (o.case_insensitive, o.normalization_insensitive) {
            (false, false) => name.to_owned(),
            (ci, true) => path::key(name, ci),
            (true, false) => {
                let mut s = String::with_capacity(name.len());
                for c in name.chars() {
                    path::fold_char(c, &mut s);
                }
                s
            }
        }
    }

    fn keys(&self, rel: &str) -> io::Result<Vec<String>> {
        Ok(components(rel)?.into_iter().map(|c| self.name_key(c)).collect())
    }

    fn path_key(&self, rel: &str) -> String {
        rel.split('/').map(|c| self.name_key(c)).collect::<Vec<_>>().join("/")
    }

    /// Locks, after checking every path and consuming any injected fault on them.
    fn begin(&self, op: &str, paths: &[&str]) -> io::Result<MutexGuard<'_, State>> {
        for p in paths {
            components(p)?;
        }
        let mut st = self.lock();
        for p in paths {
            st.fault(&self.path_key(p), op, p)?;
        }
        Ok(st)
    }

    fn open(&self, rel: &str, write: bool) -> io::Result<Box<dyn FileHandle>> {
        let keys = self.keys(rel)?;
        let now = self.now();
        let mut st = self.begin(if write { "open_rw" } else { "open_read" }, &[rel])?;
        let ino = match st.live.find(&keys) {
            Some(ino) => ino,
            None if write => {
                let parent = st.live.parent(rel, &keys)?;
                let ino = st.live.add(Node::new(Data::File(Vec::new()), now));
                let name = rel.rsplit('/').next().unwrap_or(rel).to_owned();
                st.live.dir_mut(parent).insert(keys[keys.len() - 1].clone(), Entry { name, ino });
                st.live.touch(parent, now);
                ino
            }
            None => return Err(err(io::ErrorKind::NotFound, rel, "no such file")),
        };
        let n = st.live.node_mut(ino);
        match n.data {
            Data::File(_) if n.dataless => {
                return Err(err(io::ErrorKind::Other, rel, "dataless: reading it would download it"));
            }
            Data::File(_) => {}
            Data::Dir(_) => return Err(err(io::ErrorKind::IsADirectory, rel, "is a directory")),
            Data::Link(_) => {
                return Err(err(io::ErrorKind::InvalidInput, rel, "is a symlink (MemFs does not follow links)"));
            }
        }
        n.open += 1;
        let generation = st.generation;
        Ok(Box::new(MemHandle { fs: self.clone(), ino, generation, path: rel.to_owned(), write }))
    }
}

impl Default for MemFs {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    fn fault(&mut self, key: &str, op: &str, rel: &str) -> io::Result<()> {
        let Some(f) = self.faults.get_mut(key) else { return Ok(()) };
        f.left -= 1;
        let kind = f.kind;
        if f.left == 0 {
            self.faults.remove(key);
        }
        Err(err(kind, rel, &format!("injected fault in {op}")))
    }
}

fn meta(ino: u64, n: &Node) -> Meta {
    let (kind, size) = match &n.data {
        Data::File(d) => (FileKind::File, d.len() as u64),
        Data::Dir(_) => (FileKind::Dir, 0),
        Data::Link(_) => (FileKind::Symlink, 0),
    };
    Meta {
        kind,
        size,
        mtime_ns: n.mtime,
        ctime_ns: n.ctime,
        inode: ino,
        exec: n.exec,
        dataless: n.dataless,
        other: false,
    }
}

impl Fs for MemFs {
    fn lstat(&self, rel: &str) -> io::Result<Option<Meta>> {
        let keys = self.keys(rel)?;
        let st = self.begin("lstat", &[rel])?;
        Ok(st.live.find(&keys).map(|ino| meta(ino, st.live.node(ino))))
    }

    fn read_dir(&self, rel: &str) -> io::Result<Listing> {
        let keys = self.keys(rel)?;
        let st = self.begin("read_dir", &[rel])?;
        let ino = st.live.find(&keys).ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such directory"))?;
        let d = st.live.dir(ino).ok_or_else(|| err(io::ErrorKind::NotADirectory, rel, "not a directory"))?;
        let mut entries: Vec<DirEntry> = d.values().map(|e| DirEntry { name: e.name.clone() }).collect();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Listing { entries, skipped: Vec::new() })
    }

    fn open_read(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        self.open(rel, false)
    }

    fn open_rw(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        self.open(rel, true)
    }

    fn create_dir(&self, rel: &str) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let now = self.now();
        let mut st = self.begin("create_dir", &[rel])?;
        if let Some(ino) = st.live.find(&keys) {
            return match st.live.dir(ino) {
                Some(_) => Ok(()),
                None => Err(err(io::ErrorKind::AlreadyExists, rel, "exists and is not a directory")),
            };
        }
        let parent = st.live.parent(rel, &keys)?;
        let ino = st.live.add(Node::new(Data::Dir(BTreeMap::new()), now));
        let name = rel.rsplit('/').next().unwrap_or(rel).to_owned();
        st.live.dir_mut(parent).insert(keys[keys.len() - 1].clone(), Entry { name, ino });
        st.live.touch(parent, now);
        Ok(())
    }

    fn remove_dir(&self, rel: &str) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let now = self.now();
        let mut st = self.begin("remove_dir", &[rel])?;
        let parent = st.live.parent(rel, &keys)?;
        let key = &keys[keys.len() - 1];
        let ino = st.live.dir(parent).and_then(|d| d.get(key)).map(|e| e.ino);
        let ino = ino.ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such directory"))?;
        match st.live.dir(ino) {
            None => return Err(err(io::ErrorKind::NotADirectory, rel, "not a directory")),
            Some(d) if !d.is_empty() => return Err(err(io::ErrorKind::DirectoryNotEmpty, rel, "directory not empty")),
            Some(_) => {}
        }
        st.live.dir_mut(parent).remove(key);
        st.live.touch(parent, now);
        st.live.unlink(ino, now);
        Ok(())
    }

    fn remove_file(&self, rel: &str) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let now = self.now();
        let mut st = self.begin("remove_file", &[rel])?;
        let parent = st.live.parent(rel, &keys)?;
        let key = &keys[keys.len() - 1];
        let ino = st.live.dir(parent).and_then(|d| d.get(key)).map(|e| e.ino);
        let ino = ino.ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such file"))?;
        if st.live.dir(ino).is_some() {
            return Err(err(io::ErrorKind::IsADirectory, rel, "is a directory"));
        }
        st.live.dir_mut(parent).remove(key);
        st.live.touch(parent, now);
        st.live.unlink(ino, now);
        Ok(())
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (fk, tk) = (self.keys(from)?, self.keys(to)?);
        let now = self.now();
        let mut st = self.begin("rename", &[from, to])?;
        let t = &mut st.live;
        let fp = t.parent(from, &fk)?;
        let tp = t.parent(to, &tk)?;
        let (fkey, tkey) = (&fk[fk.len() - 1], &tk[tk.len() - 1]);
        let ino = t.dir(fp).and_then(|d| d.get(fkey)).map(|e| e.ino);
        let ino = ino.ok_or_else(|| err(io::ErrorKind::NotFound, from, "no such file"))?;
        let to_name = to.rsplit('/').next().unwrap_or(to).to_owned();
        let is_dir = t.dir(ino).is_some();
        if is_dir && tk.len() > fk.len() && tk[..fk.len()] == fk[..] {
            return Err(err(io::ErrorKind::InvalidInput, to, "cannot move a directory into itself"));
        }
        if let Some(old) = t.dir(tp).and_then(|d| d.get(tkey)).map(|e| e.ino) {
            if old == ino {
                // The same entry under another spelling (`a` → `A` on a case-insensitive disk): only the
                // name changes. Two hard links to one file: POSIX says do nothing.
                if fp == tp && fkey == tkey {
                    t.dir_mut(tp).get_mut(tkey).expect("found above").name = to_name;
                    t.node_mut(ino).ctime = now;
                    t.touch(tp, now);
                }
                return Ok(());
            }
            match (is_dir, t.dir(old)) {
                (true, None) => return Err(err(io::ErrorKind::NotADirectory, to, "not a directory")),
                (true, Some(d)) if !d.is_empty() => {
                    return Err(err(io::ErrorKind::DirectoryNotEmpty, to, "directory not empty"));
                }
                (false, Some(_)) => return Err(err(io::ErrorKind::IsADirectory, to, "is a directory")),
                _ => {}
            }
            t.dir_mut(tp).remove(tkey);
            t.unlink(old, now);
        }
        t.dir_mut(fp).remove(fkey);
        t.dir_mut(tp).insert(tkey.clone(), Entry { name: to_name, ino });
        t.node_mut(ino).ctime = now;
        t.touch(fp, now);
        t.touch(tp, now);
        Ok(())
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        let (fk, tk) = (self.keys(from)?, self.keys(to)?);
        let now = self.now();
        let mut st = self.begin("hard_link", &[from, to])?;
        let t = &mut st.live;
        let ino = t.find(&fk).ok_or_else(|| err(io::ErrorKind::NotFound, from, "no such file"))?;
        if t.dir(ino).is_some() {
            return Err(err(io::ErrorKind::PermissionDenied, from, "cannot hard-link a directory"));
        }
        let tp = t.parent(to, &tk)?;
        let tkey = &tk[tk.len() - 1];
        if t.dir(tp).is_some_and(|d| d.contains_key(tkey)) {
            return Err(err(io::ErrorKind::AlreadyExists, to, "exists"));
        }
        let name = to.rsplit('/').next().unwrap_or(to).to_owned();
        t.dir_mut(tp).insert(tkey.clone(), Entry { name, ino });
        let n = t.node_mut(ino);
        n.nlink += 1;
        n.ctime = now;
        t.touch(tp, now);
        Ok(())
    }

    fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        let (fk, tk) = (self.keys(from)?, self.keys(to)?);
        let now = self.now();
        let mut st = self.begin("copy", &[from, to])?;
        let t = &mut st.live;
        let src = t.find(&fk).ok_or_else(|| err(io::ErrorKind::NotFound, from, "no such file"))?;
        let s = t.node(src);
        let content = match &s.data {
            Data::File(_) if s.dataless => {
                return Err(err(io::ErrorKind::Other, from, "dataless: reading it would download it"));
            }
            Data::File(d) => d.clone(),
            _ => return Err(err(io::ErrorKind::InvalidInput, from, "not a regular file")),
        };
        let (mtime, exec) = (s.mtime, s.exec);
        match t.find(&tk) {
            Some(dst) => {
                let n = t.node_mut(dst);
                match &mut n.data {
                    Data::File(d) => *d = content,
                    Data::Dir(_) => return Err(err(io::ErrorKind::IsADirectory, to, "is a directory")),
                    Data::Link(_) => return Err(err(io::ErrorKind::InvalidInput, to, "is a symlink")),
                }
                n.mtime = mtime;
                n.exec = exec;
                n.ctime = now;
            }
            None => {
                let tp = t.parent(to, &tk)?;
                let mut n = Node::new(Data::File(content), now);
                n.mtime = mtime;
                n.exec = exec;
                let ino = t.add(n);
                let name = to.rsplit('/').next().unwrap_or(to).to_owned();
                t.dir_mut(tp).insert(tk[tk.len() - 1].clone(), Entry { name, ino });
                t.touch(tp, now);
            }
        }
        Ok(())
    }

    fn set_mtime(&self, rel: &str, mtime_ns: i64) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let now = self.now();
        let mut st = self.begin("set_mtime", &[rel])?;
        let ino = st.live.find(&keys).ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such file"))?;
        let n = st.live.node_mut(ino);
        n.mtime = mtime_ns;
        n.ctime = now;
        Ok(())
    }

    fn set_exec(&self, rel: &str, exec: bool) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let now = self.now();
        let mut st = self.begin("set_exec", &[rel])?;
        let ino = st.live.find(&keys).ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such file"))?;
        let n = st.live.node_mut(ino);
        n.exec = exec;
        n.ctime = now;
        Ok(())
    }

    fn symlink(&self, target: &str, rel: &str) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let now = self.now();
        let mut st = self.begin("symlink", &[rel])?;
        let parent = st.live.parent(rel, &keys)?;
        let key = &keys[keys.len() - 1];
        if st.live.dir(parent).is_some_and(|d| d.contains_key(key)) {
            return Err(err(io::ErrorKind::AlreadyExists, rel, "exists"));
        }
        let ino = st.live.add(Node::new(Data::Link(target.to_owned()), now));
        let name = rel.rsplit('/').next().unwrap_or(rel).to_owned();
        st.live.dir_mut(parent).insert(key.clone(), Entry { name, ino });
        st.live.touch(parent, now);
        Ok(())
    }

    fn sync_dir(&self, rel: &str) -> io::Result<()> {
        let keys = self.keys(rel)?;
        let mut guard = self.begin("sync_dir", &[rel])?;
        let st = &mut *guard;
        st.flush_barriers();
        let ino = st.live.find(&keys).ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such directory"))?;
        let entries = st.live.dir(ino).ok_or_else(|| err(io::ErrorKind::NotADirectory, rel, "not a directory"))?.clone();
        for e in entries.values() {
            if !st.durable.nodes.contains_key(&e.ino) {
                let mut n = st.live.node(e.ino).clone();
                n.open = 0;
                if let Data::Dir(_) = n.data {
                    n.data = Data::Dir(BTreeMap::new());
                }
                st.durable.nodes.insert(e.ino, n);
            }
        }
        let live = st.live.node(ino).clone();
        match st.durable.nodes.get_mut(&ino) {
            Some(n) => {
                n.data = Data::Dir(entries);
                n.mtime = live.mtime;
                n.ctime = live.ctime;
            }
            None => {
                st.durable.nodes.insert(ino, Node { open: 0, ..live });
            }
        }
        Ok(())
    }

    fn read_link(&self, rel: &str) -> io::Result<String> {
        let keys = self.keys(rel)?;
        let st = self.begin("read_link", &[rel])?;
        let ino = st.live.find(&keys).ok_or_else(|| err(io::ErrorKind::NotFound, rel, "no such file"))?;
        match &st.live.node(ino).data {
            Data::Link(t) => Ok(t.clone()),
            _ => Err(err(io::ErrorKind::InvalidInput, rel, "not a symlink")),
        }
    }
}

struct MemHandle {
    fs: MemFs,
    ino: u64,
    generation: u64,
    path: String,
    write: bool,
}

impl MemHandle {
    /// Runs `f` on the file's content, after the generation and fault checks.
    fn with<T>(&self, op: &str, f: impl FnOnce(&mut State, &mut Vec<u8>) -> io::Result<T>) -> io::Result<T> {
        let mut st = self.fs.lock();
        if st.generation != self.generation {
            return Err(err(io::ErrorKind::Other, &self.path, "handle from before a crash"));
        }
        st.fault(&self.fs.path_key(&self.path), op, &self.path)?;
        // Moved out and back so `f` can also reach the durable tree.
        let n = st.live.node_mut(self.ino);
        let Data::File(d) = &mut n.data else { unreachable!("handles are opened on files only") };
        let mut data = std::mem::take(d);
        let r = f(&mut st, &mut data);
        if let Data::File(d) = &mut st.live.node_mut(self.ino).data {
            *d = data;
        }
        r
    }

    fn writable(&self) -> io::Result<()> {
        if self.write { Ok(()) } else { Err(err(io::ErrorKind::PermissionDenied, &self.path, "opened read-only")) }
    }
}

fn index(rel: &str, v: u64) -> io::Result<usize> {
    usize::try_from(v).map_err(|_| err(io::ErrorKind::InvalidInput, rel, "offset too large"))
}

impl FileHandle for MemHandle {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let off = index(&self.path, offset)?;
        self.with("read_at", |_, d| {
            let n = d.len().saturating_sub(off).min(buf.len());
            if n > 0 {
                buf[..n].copy_from_slice(&d[off..off + n]);
            }
            Ok(n)
        })
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.writable()?;
        let off = index(&self.path, offset)?;
        let now = self.fs.now();
        self.with("write_at", |st, d| {
            let end = off + buf.len();
            if d.len() < end {
                d.resize(end, 0);
            }
            d[off..end].copy_from_slice(buf);
            st.live.touch(self.ino, now);
            Ok(())
        })
    }

    fn set_len(&self, len: u64) -> io::Result<()> {
        self.writable()?;
        let len = index(&self.path, len)?;
        let now = self.fs.now();
        self.with("set_len", |st, d| {
            d.resize(len, 0);
            st.live.touch(self.ino, now);
            Ok(())
        })
    }

    fn len(&self) -> io::Result<u64> {
        self.with("len", |_, d| Ok(d.len() as u64))
    }

    fn sync_barrier(&self) -> io::Result<()> {
        self.with("sync_barrier", |st, d| {
            let live = st.live.node(self.ino);
            let entry = (d.clone(), live.mtime, live.ctime, live.exec);
            st.barriered.insert(self.ino, entry);
            Ok(())
        })
    }

    fn sync_all(&self) -> io::Result<()> {
        self.with("sync_all", |st, d| {
            st.flush_barriers();
            let live = st.live.node(self.ino);
            let (mtime, ctime, exec) = (live.mtime, live.ctime, live.exec);
            if let Some(n) = st.durable.nodes.get_mut(&self.ino)
                && let Data::File(old) = &mut n.data
            {
                old.clone_from(d);
                n.mtime = mtime;
                n.ctime = ctime;
                n.exec = exec;
            }
            Ok(())
        })
    }
}

impl Drop for MemHandle {
    fn drop(&mut self) {
        let mut st = self.fs.lock();
        if st.generation != self.generation {
            return;
        }
        if let Some(n) = st.live.nodes.get_mut(&self.ino) {
            n.open -= 1;
            if n.open == 0 && n.nlink == 0 {
                st.live.nodes.remove(&self.ino);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{read_all, write_all};

    fn ci() -> MemFs {
        MemFs::with_options(MemFsOptions { case_insensitive: true, ..MemFsOptions::default() })
    }

    #[test]
    fn a_synced_directory_keeps_its_renames_across_a_crash() {
        let fs = MemFs::new();
        fs.create_dir("d").unwrap();
        fs.snapshot();
        write_all(&fs, "d/.t", b"new").unwrap();
        fs.open_rw("d/.t").unwrap().sync_all().unwrap();
        fs.rename("d/.t", "d/f").unwrap();
        write_all(&fs, "lost", b"x").unwrap();
        fs.sync_dir("d").unwrap();
        fs.restore();
        assert_eq!(read_all(&fs, "d/f").unwrap(), b"new", "synced: the rename survives");
        assert!(fs.lstat("d/.t").unwrap().is_none());
        assert!(fs.lstat("lost").unwrap().is_none(), "the root was not synced");
        fs.create_dir("e").unwrap();
        fs.create_dir("e/sub").unwrap();
        fs.sync_dir("").unwrap();
        fs.restore();
        assert!(fs.lstat("e").unwrap().is_some_and(|m| m.kind == FileKind::Dir), "a new directory comes with its parent");
        assert!(fs.lstat("e/sub").unwrap().is_none(), "but not its own entries");
    }

    #[test]
    fn data_behind_a_barrier_is_durable_only_after_a_full_flush() {
        let fs = MemFs::new();
        write_all(&fs, "f", b"old").unwrap();
        fs.snapshot();
        let h = fs.open_rw("f").unwrap();
        h.write_at(0, b"new").unwrap();
        h.sync_barrier().unwrap();
        drop(h);
        fs.restore();
        assert_eq!(read_all(&fs, "f").unwrap(), b"old", "a barrier alone does not reach the medium");
        let h = fs.open_rw("f").unwrap();
        h.write_at(0, b"new").unwrap();
        h.sync_barrier().unwrap();
        drop(h);
        fs.create_dir("d").unwrap();
        fs.sync_dir("d").unwrap();
        fs.restore();
        assert_eq!(read_all(&fs, "f").unwrap(), b"new", "any full flush makes it durable");
    }

    #[test]
    fn files_dirs_and_metadata() {
        let fs = MemFs::new();
        fs.create_dir("d").unwrap();
        fs.create_dir("d").unwrap();
        write_all(&fs, "d/f", b"hello").unwrap();
        let m = fs.lstat("d/f").unwrap().unwrap();
        assert_eq!((m.kind, m.size, m.exec, m.dataless), (FileKind::File, 5, false, false));
        assert_eq!(fs.lstat("nope").unwrap(), None);
        assert_eq!(fs.lstat("d/f/x").unwrap(), None, "through a file: not there");
        assert_eq!(fs.read_dir("d").unwrap().entries, [DirEntry { name: "f".into() }]);
        assert_eq!(fs.remove_dir("d").unwrap_err().kind(), io::ErrorKind::DirectoryNotEmpty);
        assert_eq!(fs.open_rw("missing/f").err().unwrap().kind(), io::ErrorKind::NotFound);
        fs.set_exec("d/f", true).unwrap();
        fs.set_mtime("d/f", 42).unwrap();
        let m2 = fs.lstat("d/f").unwrap().unwrap();
        assert!(m2.exec && m2.mtime_ns == 42 && m2.ctime_ns > m.ctime_ns && m2.inode == m.inode);
        fs.symlink("../elsewhere", "d/l").unwrap();
        assert_eq!(fs.read_link("d/l").unwrap(), "../elsewhere");
        assert_eq!(fs.lstat("d/l").unwrap().unwrap().kind, FileKind::Symlink);
        fs.remove_file("d/l").unwrap();
        fs.remove_file("d/f").unwrap();
        fs.remove_dir("d").unwrap();
        assert!(fs.read_dir("").unwrap().entries.is_empty());
        assert!(fs.lstat("../x").is_err(), "escaping the root is refused");
    }

    #[test]
    fn hard_links_share_content_and_copies_do_not() {
        let fs = MemFs::new();
        write_all(&fs, "a", b"one").unwrap();
        fs.hard_link("a", "b").unwrap();
        fs.copy("a", "c").unwrap();
        write_all(&fs, "a", b"two").unwrap();
        assert_eq!(read_all(&fs, "b").unwrap(), b"two");
        assert_eq!(read_all(&fs, "c").unwrap(), b"one");
        let ino = |p| fs.lstat(p).unwrap().unwrap().inode;
        assert!(ino("a") == ino("b") && ino("a") != ino("c"));
        fs.remove_file("a").unwrap();
        assert_eq!(read_all(&fs, "b").unwrap(), b"two", "the other name keeps the file");
    }

    #[test]
    fn rename_replaces_and_follows_posix() {
        let fs = MemFs::new();
        write_all(&fs, "a", b"new").unwrap();
        write_all(&fs, "b", b"old").unwrap();
        let ino = fs.lstat("a").unwrap().unwrap().inode;
        fs.rename("a", "b").unwrap();
        assert_eq!(fs.lstat("a").unwrap(), None);
        assert_eq!(fs.lstat("b").unwrap().unwrap().inode, ino);
        assert_eq!(read_all(&fs, "b").unwrap(), b"new");
        fs.create_dir("d").unwrap();
        assert_eq!(fs.rename("b", "d").unwrap_err().kind(), io::ErrorKind::IsADirectory);
        assert_eq!(fs.rename("d", "d/e").unwrap_err().kind(), io::ErrorKind::InvalidInput);
        fs.rename("b", "d/b").unwrap();
        assert_eq!(read_all(&fs, "d/b").unwrap(), b"new");
    }

    #[test]
    fn case_insensitive_keeps_spelling() {
        let fs = ci();
        write_all(&fs, "Readme.TXT", b"x").unwrap();
        assert!(fs.lstat("README.txt").unwrap().is_some());
        assert_eq!(fs.read_dir("").unwrap().entries[0].name, "Readme.TXT");
        fs.rename("Readme.TXT", "README.TXT").unwrap();
        assert_eq!(fs.read_dir("").unwrap().entries[0].name, "README.TXT");
        assert_eq!(fs.symlink("t", "readme.txt").unwrap_err().kind(), io::ErrorKind::AlreadyExists);

        let cs = MemFs::new();
        write_all(&cs, "a", b"1").unwrap();
        write_all(&cs, "A", b"2").unwrap();
        assert_eq!(cs.read_dir("").unwrap().entries.len(), 2);
    }

    #[test]
    fn normalization_insensitive_like_apfs() {
        let fs = MemFs::with_options(MemFsOptions { normalization_insensitive: true, ..MemFsOptions::default() });
        write_all(&fs, "u\u{0308}ber", b"x").unwrap();
        assert!(fs.lstat("über").unwrap().is_some());
        assert_eq!(fs.read_dir("").unwrap().entries[0].name, "u\u{0308}ber", "the name stays as written");
        assert!(MemFs::new().lstat("über").unwrap().is_none());
    }

    #[test]
    fn crash_keeps_snapshot_and_synced_data_only() {
        let fs = MemFs::new();
        write_all(&fs, "kept", b"v1").unwrap();
        write_all(&fs, "lost", b"v1").unwrap();
        fs.snapshot();
        let h = fs.open_rw("kept").unwrap();
        h.write_at(0, b"v2").unwrap();
        h.sync_all().unwrap();
        write_all(&fs, "lost", b"v2").unwrap();
        write_all(&fs, "new", b"x").unwrap();
        fs.remove_file("kept").unwrap();
        fs.restore();
        assert_eq!(read_all(&fs, "kept").unwrap(), b"v2", "synced data survives");
        assert_eq!(read_all(&fs, "lost").unwrap(), b"v1", "unsynced data is lost");
        assert_eq!(fs.lstat("new").unwrap(), None, "namespace changes since the snapshot are lost");
        assert!(h.read_at(0, &mut [0; 2]).is_err(), "old handles are dead");
        write_all(&fs, "after", b"").unwrap();
        // 4 went to `new`, which the crash lost; it is not handed out again.
        assert_eq!(fs.lstat("after").unwrap().unwrap().inode, 5, "inodes are not reused");
    }

    #[test]
    fn open_unlinked_files_live_until_closed() {
        let fs = MemFs::new();
        write_all(&fs, "f", b"data").unwrap();
        let h = fs.open_read("f").unwrap();
        fs.remove_file("f").unwrap();
        let mut b = [0; 4];
        assert_eq!(h.read_at(0, &mut b).unwrap(), 4);
        assert_eq!(&b, b"data");
        assert!(h.write_at(0, b"x").is_err(), "read-only handle");
        drop(h);
        assert_eq!(fs.lock().live.nodes.len(), 1, "only the root is left");
    }

    #[test]
    fn faults_fail_the_next_n_operations() {
        let fs = ci();
        write_all(&fs, "f", b"data").unwrap();
        fs.fail_next("F", 2, io::ErrorKind::StorageFull);
        assert_eq!(fs.lstat("f").unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert_eq!(fs.rename("f", "g").unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert!(fs.lstat("f").unwrap().is_some(), "a failed rename changed nothing");
        let h = fs.open_rw("f").unwrap();
        fs.fail_next("f", 1, io::ErrorKind::Interrupted);
        assert_eq!(h.write_at(0, b"x").unwrap_err().kind(), io::ErrorKind::Interrupted);
        h.write_at(0, b"x").unwrap();
    }

    #[test]
    fn dataless_files_are_never_read() {
        let fs = MemFs::new();
        write_all(&fs, "cloud", b"far away").unwrap();
        fs.set_dataless("cloud", true).unwrap();
        assert!(fs.lstat("cloud").unwrap().unwrap().dataless);
        assert!(fs.open_read("cloud").is_err());
        assert!(fs.copy("cloud", "x").is_err());
    }

    #[test]
    fn manual_clock_sets_times() {
        let clock = ManualClock::new(1_000);
        let fs = MemFs::with_options(MemFsOptions { clock: clock.clock(), ..MemFsOptions::default() });
        write_all(&fs, "f", b"").unwrap();
        clock.advance(500);
        fs.set_exec("f", true).unwrap();
        let m = fs.lstat("f").unwrap().unwrap();
        assert_eq!((m.mtime_ns, m.ctime_ns), (1_000, 1_500));
    }
}
