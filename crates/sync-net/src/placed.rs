//! On-demand content kept in the host's placeholders (SPEC §7G.11, "Where the content is", *in place*).
//!
//! Windows' Cloud Files hydrates a file into its placeholder in the sync root (the folder's place) itself, so a copy in
//! the core's cache as well would store every downloaded file twice. A folder whose host keeps content *in place*
//! stores it once:
//!
//! * The core fetches into its cache as always (the cache is the staging area: partial downloads, verification).
//! * The host transfers the complete file into the placeholder and says so ([`crate::SyncHandle::od_hydrated`]). The
//!   core checks the placeholder holds exactly the recorded content (every chunk's hash), notes it here, and only then
//!   drops its own copy.
//! * From then on the core reads that content where it is: [`PlaceFs`], the folder's filesystem, sends every read of
//!   such a file to the placeholder (serving chunks to other devices, answering whether this device holds it, scanning
//!   it), and presents it with the stamp the core recorded for its cache copy while the placeholder is unchanged since
//!   (size, time and file id). A placeholder changed since shows as it is, so the core sees the change, as it sees a
//!   change in its cache. A dehydrated or missing one is content not here (never a delete); a place that is not there
//!   at all (its disk not mounted) is an error, so nothing is taken for gone.
//! * Anything else the core does to such a file (archive it, make a conflict copy of it, write into it, rename it on
//!   another device's word) takes the content back into the cache first (a copy: the placeholder stays the host's, and
//!   the core never writes into the host's tree). Freeing it (evict, a delete) only lets go of it: the host dehydrates
//!   or deletes the placeholder when the feed says so. A rename the host reported (the user's) moves the note with it.
//!
//! Each placeholder the core put content in keeps a note after the content is back in the cache or freed (`held`
//! false): what was placed and how the file looked then, so [`crate::SyncHandle::od_local_change`] can tell the user's
//! edit from an earlier version the host may dehydrate. The notes are a log beside the folder's index
//! (`<sync state>/<id>.placed`), deleted with the folder.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use seedwell_sync::fs::{FileHandle, Fs, Listing, Meta, StdFs};
use seedwell_sync::index::Stamp;
use seedwell_sync_proto::model::{FileInfo, FileKind};

use crate::shared::lock;

/// A file's state as the notes compare it: size, time and file id (a rename keeps all three; a write changes one).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mark {
    /// Bytes.
    pub size: u64,
    /// Modification time, Unix ns.
    pub mtime_ns: i64,
    /// Status-change time, Unix ns (kept to present; not compared: a rename changes it).
    pub ctime_ns: i64,
    /// Inode (the NTFS file id).
    pub inode: u64,
}

impl Mark {
    /// The mark of `m`.
    pub fn of(m: &Meta) -> Mark {
        Mark { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode }
    }

    fn of_stamp(s: &Stamp) -> Mark {
        Mark { size: s.size, mtime_ns: s.mtime_ns, ctime_ns: s.ctime_ns, inode: s.inode }
    }

    /// The same file, unchanged: size, time and file id.
    pub fn same(&self, o: &Mark) -> bool {
        (self.size, self.mtime_ns, self.inode) == (o.size, o.mtime_ns, o.inode)
    }
}

/// What the core put in one placeholder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Note {
    /// Where the placeholder is, in the place: the item's own path, or the one it had before a rename the host
    /// reported is over.
    pub at: String,
    /// The placeholder as it was when the content was put there; `None` when that is not known (the host wrote into
    /// it before the core looked): only its content says then.
    pub real: Option<Mark>,
    /// The stamp the core recorded for its cache copy: presented while the placeholder is as `real` says.
    pub shown: Mark,
    /// The executable bit the cache copy had.
    #[serde(default)]
    pub exec: bool,
    /// The content version put there.
    pub content: String,
    /// The core's content is in the placeholder only. False: the core has its own copy again, or none.
    pub held: bool,
}

#[derive(Serialize, Deserialize)]
struct Line {
    p: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    n: Option<Note>,
}

struct Inner {
    notes: BTreeMap<String, Note>,
    log: Option<File>,
    lines: usize,
    /// A rename the host reported, under way on the folder's core: [`PlaceFs::rename`] moves the notes for it.
    host_rename: Option<(String, String)>,
}

/// The notes of an in-place folder, and where its place and cache are.
pub struct Placed {
    place: Mutex<PathBuf>,
    cache: PathBuf,
    log_path: PathBuf,
    inner: Mutex<Inner>,
    /// The versions a fetch handed the host (by path), so the placeholder it fills is checked against what it was
    /// given even when a newer version arrived meanwhile. In memory: after a restart that is not known.
    handed: Mutex<std::collections::HashMap<String, FileInfo>>,
}

fn under(p: &str, dir: &str) -> bool {
    dir.is_empty() || p == dir || p.strip_prefix(dir).is_some_and(|r| r.starts_with('/'))
}

fn parent_of(p: &str) -> &str {
    p.rsplit_once('/').map_or("", |(d, _)| d)
}

fn name_of(p: &str) -> &str {
    p.rsplit_once('/').map_or(p, |(_, n)| n)
}

impl Placed {
    /// The notes in `log` (made empty when missing), for content kept at `place` instead of in `cache`. Notes of
    /// content the cache holds again (a stop between noting and dropping the cache's copy) are no longer held.
    pub fn open(place: PathBuf, cache: PathBuf, log: PathBuf) -> io::Result<Placed> {
        let mut notes = BTreeMap::new();
        let mut lines = 0;
        match File::open(&log) {
            Ok(f) => {
                for l in io::BufReader::new(f).lines() {
                    let l = l?;
                    lines += 1;
                    // A line cut off by a crash is the last one: what it said never happened.
                    let Ok(line) = serde_json::from_str::<Line>(&l) else { continue };
                    match line.n {
                        Some(n) => notes.insert(line.p, n),
                        None => notes.remove(&line.p),
                    };
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        let cache_fs = StdFs::new(cache.clone());
        for (p, n) in notes.iter_mut() {
            if n.held && cache_fs.lstat(p).ok().flatten().is_some() {
                n.held = false;
            }
        }
        let p = Placed {
            place: Mutex::new(place),
            cache,
            log_path: log,
            inner: Mutex::new(Inner { notes, log: None, lines, host_rename: None }),
            handed: Mutex::new(std::collections::HashMap::new()),
        };
        p.compact()?;
        Ok(p)
    }

    /// Writes the log anew with the notes as they are.
    fn compact(&self) -> io::Result<()> {
        let mut inner = lock(&self.inner);
        let tmp = self.log_path.with_extension("placed-new");
        {
            let mut f = File::create(&tmp)?;
            for (p, n) in &inner.notes {
                writeln!(f, "{}", serde_json::to_string(&Line { p: p.clone(), n: Some(n.clone()) }).map_err(io::Error::other)?)?;
            }
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &self.log_path)?;
        inner.lines = inner.notes.len();
        inner.log = Some(OpenOptions::new().append(true).open(&self.log_path)?);
        Ok(())
    }

    /// Notes a change: written (and on the disk) before it is made in memory.
    fn write(&self, inner: &mut Inner, p: &str, n: Option<Note>) -> io::Result<()> {
        let line = serde_json::to_string(&Line { p: p.to_string(), n: n.clone() }).map_err(io::Error::other)?;
        let log = match inner.log.as_mut() {
            Some(l) => l,
            None => inner.log.insert(OpenOptions::new().create(true).append(true).open(&self.log_path)?),
        };
        writeln!(log, "{line}")?;
        log.sync_data()?;
        inner.lines += 1;
        match n {
            Some(n) => inner.notes.insert(p.to_string(), n),
            None => inner.notes.remove(p),
        };
        Ok(())
    }

    fn maybe_compact(&self) {
        let (lines, live) = {
            let i = lock(&self.inner);
            (i.lines, i.notes.len())
        };
        if lines > 2 * live + 256 {
            let _ = self.compact();
        }
    }

    /// The folder's place (the sync root).
    pub fn place(&self) -> PathBuf {
        lock(&self.place).clone()
    }

    /// The cache (where the core stages content).
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// A fetch handed the host `info`'s content for `p`.
    pub fn hand(&self, p: &str, info: FileInfo) {
        let mut h = lock(&self.handed);
        // A host fills a few placeholders at a time; this is no record of every fetch.
        if h.len() > 4096 {
            h.clear();
        }
        h.insert(p.to_string(), info);
    }

    /// The version with content version `content` a fetch handed the host for `p`, taken.
    pub fn handed(&self, p: &str, content: &str) -> Option<FileInfo> {
        let mut h = lock(&self.handed);
        let i = h.remove(p)?;
        (seedwell_sync::ondemand::content_version_of(&i) == content).then_some(i)
    }

    /// The note of `p`.
    pub fn note(&self, p: &str) -> Option<Note> {
        lock(&self.inner).notes.get(p).cloned()
    }

    /// The note of `p` while the core's content is in its placeholder.
    pub fn held(&self, p: &str) -> Option<Note> {
        lock(&self.inner).notes.get(p).filter(|n| n.held).cloned()
    }

    /// Whether the core's content of `p` is in its placeholder.
    pub fn is_held(&self, p: &str) -> bool {
        lock(&self.inner).notes.get(p).is_some_and(|n| n.held)
    }

    /// The notes at or below `dir`.
    pub fn under(&self, dir: &str) -> Vec<(String, Note)> {
        lock(&self.inner).notes.iter().filter(|(p, _)| under(p, dir)).map(|(p, n)| (p.clone(), n.clone())).collect()
    }

    /// How many files' content is in their placeholders.
    pub fn held_count(&self) -> usize {
        lock(&self.inner).notes.values().filter(|n| n.held).count()
    }

    /// Notes `n` for `p`.
    pub fn put(&self, p: &str, n: Note) -> io::Result<()> {
        let mut inner = lock(&self.inner);
        self.write(&mut inner, p, Some(n))?;
        drop(inner);
        self.maybe_compact();
        Ok(())
    }

    /// Forgets `p`'s note.
    pub fn forget(&self, p: &str) -> io::Result<()> {
        let mut inner = lock(&self.inner);
        if inner.notes.contains_key(p) {
            self.write(&mut inner, p, None)?;
        }
        Ok(())
    }

    /// The core no longer relies on `p`'s placeholder (its content is back in the cache, freed, or replaced): the note
    /// stays as a record of what was placed.
    pub fn let_go(&self, p: &str) -> io::Result<()> {
        let mut inner = lock(&self.inner);
        if let Some(mut n) = inner.notes.get(p).cloned().filter(|n| n.held) {
            n.held = false;
            self.write(&mut inner, p, Some(n))?;
        }
        Ok(())
    }

    /// The host is renaming `from` to `to` (the user's own rename) while `op` runs on the folder's core: the notes go
    /// with it.
    pub fn host_rename<T>(&self, from: &str, to: &str, op: impl FnOnce() -> T) -> T {
        lock(&self.inner).host_rename = Some((from.to_string(), to.to_string()));
        let r = op();
        lock(&self.inner).host_rename = None;
        r
    }

    /// Moves the notes at or below `from` to `to`, each keeping where its placeholder is now (`at`) until it is found
    /// at its new path.
    fn rekey(&self, from: &str, to: &str) -> io::Result<()> {
        let moved: Vec<(String, Note)> = self.under(from).into_iter().filter(|(p, _)| p == from || p.starts_with(&format!("{from}/"))).collect();
        let mut inner = lock(&self.inner);
        for (p, n) in moved {
            let new = format!("{to}{}", &p[from.len()..]);
            self.write(&mut inner, &p, None)?;
            self.write(&mut inner, &new, Some(n))?;
        }
        Ok(())
    }

    fn place_fs(&self) -> StdFs {
        StdFs::new(self.place())
    }

    /// The placeholder of `p` (noted `n`): where it is and how it is, when it holds content (a file, hydrated). Looked
    /// for at its path, then where the note says it was; found at its path, the note follows. An error when the place
    /// itself is not there (its disk not mounted), so content there is never taken for gone.
    pub fn locate(&self, p: &str, n: &Note) -> io::Result<Option<(String, Meta)>> {
        let place = self.place();
        if std::fs::symlink_metadata(&place).is_err() {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("the folder's place {} is not there", place.display())));
        }
        let fs = StdFs::new(place);
        for at in [p, n.at.as_str()] {
            if let Some(m) = fs.lstat(at)? {
                if m.kind != FileKind::File || m.dataless || m.other {
                    return Ok(None);
                }
                if at == p && n.at != p {
                    let mut inner = lock(&self.inner);
                    if let Some(mut now) = inner.notes.get(p).cloned() {
                        now.at = p.to_string();
                        let _ = self.write(&mut inner, p, Some(now));
                    }
                }
                return Ok(Some((at.to_string(), m)));
            }
        }
        Ok(None)
    }

    /// What the core sees of held `p`: its recorded stamp while the placeholder is unchanged, the placeholder as it is
    /// once changed, nothing when it holds no content.
    fn shown(&self, p: &str, n: &Note) -> io::Result<Option<Meta>> {
        let Some((_, m)) = self.locate(p, n)? else { return Ok(None) };
        if n.real.is_some_and(|r| r.same(&Mark::of(&m))) {
            let s = n.shown;
            return Ok(Some(Meta { kind: FileKind::File, size: s.size, mtime_ns: s.mtime_ns, ctime_ns: s.ctime_ns, inode: s.inode, exec: n.exec, dataless: false, other: false }));
        }
        Ok(Some(m))
    }

    /// Brings held `p`'s content back into the cache (a copy: the placeholder stays as it is), so the core can change
    /// it there. Written aside and renamed into place, the file's time kept.
    fn take_back(&self, p: &str) -> io::Result<()> {
        let Some(n) = self.held(p) else { return Ok(()) };
        let Some((at, _)) = self.locate(p, &n)? else {
            return Err(io::Error::new(io::ErrorKind::NotFound, format!("{p}: its content is no longer in its placeholder")));
        };
        let from = self.place_fs();
        let cache = StdFs::new(self.cache.clone());
        let tmp = seedwell_sync::pull::temp_path(&format!("{p}.placed"));
        copy_between(&from, &at, &cache, &tmp)?;
        cache.set_mtime(&tmp, n.shown.mtime_ns)?;
        cache.rename(&tmp, p)?;
        self.let_go(p)
    }

    /// Takes back every held file at or below `p`.
    fn take_back_under(&self, p: &str) -> io::Result<()> {
        for (q, n) in self.under(p) {
            if n.held {
                self.take_back(&q)?;
            }
        }
        Ok(())
    }

    /// Before the folder stops being kept in place (the host has let go of its placeholders: switched to full,
    /// removed): the content of every held file comes back into the cache, moved where the place and the cache are on
    /// one volume (copied otherwise, the placeholder then deleted), and the folders it leaves empty at the place go.
    /// Returns how many files came back. A placeholder that holds no content any more (missing, dehydrated) is
    /// forgotten: its file is online only from then on.
    pub fn take_all_back(&self) -> io::Result<u64> {
        let place = self.place();
        let cache = StdFs::new(self.cache.clone());
        let mut n = 0;
        let mut dirs = std::collections::BTreeSet::new();
        for (p, note) in self.under("") {
            if !note.held {
                self.forget(&p)?;
                continue;
            }
            let Some((at, _)) = self.locate(&p, &note)? else {
                self.forget(&p)?;
                continue;
            };
            let src = abs(&place, &at);
            let dst = abs(&self.cache, &p);
            if let Some(d) = dst.parent() {
                std::fs::create_dir_all(d)?;
            }
            if std::fs::rename(&src, &dst).is_err() {
                let tmp = seedwell_sync::pull::temp_path(&format!("{p}.placed"));
                copy_between(&self.place_fs(), &at, &cache, &tmp)?;
                cache.set_mtime(&tmp, note.shown.mtime_ns)?;
                cache.rename(&tmp, &p)?;
                std::fs::remove_file(&src)?;
            }
            self.forget(&p)?;
            let mut d = parent_of(&at);
            while !d.is_empty() {
                dirs.insert(d.to_string());
                d = parent_of(d);
            }
            n += 1;
        }
        // Deepest first; a folder with anything left in it stays.
        for d in dirs.iter().rev() {
            let _ = std::fs::remove_dir(abs(&place, d));
        }
        Ok(n)
    }

    /// Deletes the notes' log (the folder is no longer in place, or goes).
    pub fn remove_log(log: &Path) {
        let _ = std::fs::remove_file(log);
        let _ = std::fs::remove_file(log.with_extension("placed-new"));
    }
}

fn abs(root: &Path, p: &str) -> PathBuf {
    p.split('/').filter(|c| !c.is_empty()).fold(root.to_path_buf(), |r, c| r.join(c))
}

/// Copies `from_rel` of `from` into a new `to_rel` of `to`, in blocks.
fn copy_between(from: &StdFs, from_rel: &str, to: &StdFs, to_rel: &str) -> io::Result<()> {
    let src = from.open_read(from_rel)?;
    let _ = to.remove_file(to_rel);
    let dst = to.open_rw(to_rel)?;
    dst.set_len(0)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut off = 0u64;
    loop {
        let n = src.read_at(off, &mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_at(off, &buf[..n])?;
        off += n as u64;
    }
    dst.sync_all()
}

/// Whether file `ra` of `a` and file `rb` of `b` hold the same bytes (an unhashed version's placeholder against its
/// copy in the cache, which has no hashes to be checked by).
pub fn same_bytes(a: &dyn Fs, ra: &str, b: &dyn Fs, rb: &str) -> io::Result<bool> {
    let (fa, fb) = (a.open_read(ra)?, b.open_read(rb)?);
    let size = fa.len()?;
    if fb.len()? != size {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0u8; 1 << 20], vec![0u8; 1 << 20]);
    let mut off = 0;
    while off < size {
        let n = (size - off).min(1 << 20) as usize;
        for (f, buf) in [(&fa, &mut x), (&fb, &mut y)] {
            let mut got = 0;
            while got < n {
                match f.read_at(off + got as u64, &mut buf[got..n])? {
                    0 => return Ok(false),
                    k => got += k,
                }
            }
        }
        if x[..n] != y[..n] {
            return Ok(false);
        }
        off += n as u64;
    }
    Ok(true)
}

/// Whether the file `rel` of `fs` holds exactly `info`'s content: its size, and each chunk's hash. An unhashed version
/// (SPEC §7G.11) has no hashes to check by, so nothing holds it by this check: [`holds_version`] knows where this
/// device has its bytes.
pub fn holds_content(fs: &dyn Fs, rel: &str, info: &FileInfo) -> io::Result<bool> {
    if info.is_unhashed() {
        return Ok(false);
    }
    let fh = fs.open_read(rel)?;
    if fh.len()? != info.size {
        return Ok(false);
    }
    let mut buf = Vec::new();
    for (off, c) in seedwell_sync::index::offsets(&info.chunks) {
        buf.resize(c.len as usize, 0);
        let mut got = 0;
        while got < buf.len() {
            match fh.read_at(off + got as u64, &mut buf[got..])? {
                0 => return Ok(false),
                n => got += n,
            }
        }
        if ring::digest::digest(&ring::digest::SHA256, &buf).as_ref() != c.hash {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether file `rel` of `fs` holds the content of `rec`, a version the folder records here (SPEC §7G.11, *Unhashed
/// versions held here*). A hashed version by its hashes. An unhashed one has none, so by the bytes this device has of
/// it, in `core` (the folder's filesystem: the cache, or a held file's placeholder): that file unchanged since the
/// version was recorded from it (size, time and file id: the listing a scan records an unhashed version by) and the
/// same bytes as `rel`, or `rel` itself (`same_file`: the placeholder the core reads the version from). An unhashed
/// version whose bytes are not here (dataless, or its file changed since) is known to be in no file.
///
/// So this device's own edit, recorded first by its scan and reported by its host after, is the version recorded,
/// never a conflict with itself (Windows, master after build 69: a conflict copy, and the placeholder let go of as
/// stale while it held the only copy of the version at the name).
pub fn holds_version(core: &dyn Fs, rec: &seedwell_sync::index::LocalRecord, fs: &dyn Fs, rel: &str, same_file: bool) -> io::Result<bool> {
    let info = &rec.file.info;
    if !info.is_unhashed() {
        return holds_content(fs, rel, info);
    }
    if rec.dataless || info.deleted || info.kind != FileKind::File {
        return Ok(false);
    }
    let (Some(stamp), Some(m)) = (rec.stamp.as_ref(), core.lstat(&info.path)?) else { return Ok(false) };
    if m.kind != FileKind::File || m.dataless || m.other || m.size != info.size || !Mark::of_stamp(stamp).same(&Mark::of(&m)) {
        return Ok(false);
    }
    if same_file {
        return Ok(true);
    }
    same_bytes(core, &info.path, fs, rel)
}

type ReadHook = (PathBuf, Box<dyn FnMut() + Send>);
static READ_HOOKS: Mutex<Vec<ReadHook>> = Mutex::new(Vec::new());
static READ_HOOKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// **Tests:** `hook` runs each time [`crate::SyncHandle::od_local_change`] has read the placeholder at `at` to find
/// its content in a conflict copy, before it looks at the file again (as a write landing while it was read); `None`
/// takes `at`'s away.
#[doc(hidden)]
pub fn test_after_reading(at: &Path, hook: Option<Box<dyn FnMut() + Send>>) {
    let mut hooks = lock(&READ_HOOKS);
    hooks.retain(|(p, _)| p != at);
    hooks.extend(hook.map(|f| (at.to_path_buf(), f)));
    READ_HOOKED.store(!hooks.is_empty(), std::sync::atomic::Ordering::SeqCst);
}

/// Runs the test hook of `at`, if any ([`test_after_reading`]).
pub(crate) fn after_reading(at: &Path) {
    if !READ_HOOKED.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    if let Some((_, f)) = lock(&READ_HOOKS).iter_mut().find(|(p, _)| p == at) {
        f();
    }
}

/// The record's stamp as a mark.
pub fn mark_of_stamp(s: &Stamp) -> Mark {
    Mark::of_stamp(s)
}

/// The folder's filesystem when its content is kept in place: the cache, with each held file read from its
/// placeholder (see the module's text).
pub struct PlaceFs {
    cache: StdFs,
    placed: std::sync::Arc<Placed>,
}

impl PlaceFs {
    /// The cache at `placed`'s cache folder, its held files at their placeholders.
    pub fn new(placed: std::sync::Arc<Placed>) -> PlaceFs {
        PlaceFs { cache: StdFs::new(placed.cache.clone()), placed }
    }
}

impl Fs for PlaceFs {
    fn lstat(&self, rel: &str) -> io::Result<Option<Meta>> {
        match self.placed.held(rel) {
            Some(n) => self.placed.shown(rel, &n),
            None => self.cache.lstat(rel),
        }
    }

    fn read_dir(&self, rel: &str) -> io::Result<Listing> {
        let held: Vec<String> =
            self.placed.under(rel).into_iter().filter(|(p, n)| n.held && p != rel && parent_of(p) == rel).map(|(p, _)| name_of(&p).to_string()).collect();
        let mut out = match self.cache.read_dir(rel) {
            Ok(l) => l,
            Err(e) if e.kind() == io::ErrorKind::NotFound && !held.is_empty() => Listing::default(),
            Err(e) => return Err(e),
        };
        for name in held {
            if !out.entries.iter().any(|e| e.name == name) {
                out.entries.push(seedwell_sync::fs::DirEntry { name });
            }
        }
        out.entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn open_read(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        let Some(n) = self.placed.held(rel) else { return self.cache.open_read(rel) };
        // Never a dehydrated placeholder: reading it would ask the host (this process) to fetch it.
        match self.placed.locate(rel, &n)? {
            Some((at, _)) => self.placed.place_fs().open_read(&at),
            None => Err(io::Error::new(io::ErrorKind::NotFound, format!("{rel}: its content is not in its placeholder"))),
        }
    }

    fn open_rw(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        self.placed.take_back(rel)?;
        self.cache.open_rw(rel)
    }

    fn create_dir(&self, rel: &str) -> io::Result<()> {
        self.cache.create_dir(rel)
    }

    fn remove_dir(&self, rel: &str) -> io::Result<()> {
        if self.placed.under(rel).iter().any(|(p, n)| n.held && p != rel) {
            return Err(io::Error::other(format!("{rel}: not empty (files in it are kept in their placeholders)")));
        }
        self.cache.remove_dir(rel)
    }

    fn remove_file(&self, rel: &str) -> io::Result<()> {
        if self.placed.is_held(rel) {
            // Freed or deleted: the core lets go; the placeholder is the host's to dehydrate or delete.
            self.placed.let_go(rel)?;
            return match self.cache.remove_file(rel) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                r => r,
            };
        }
        self.cache.remove_file(rel)
    }

    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let host = lock(&self.placed.inner).host_rename.clone();
        if host.as_ref().is_some_and(|(f, t)| under(from, f) && from.len() >= f.len() && to == format!("{t}{}", &from[f.len()..])) {
            // The user's rename: the placeholders move with it, and so do the notes.
            let moves_held = self.placed.is_held(from);
            self.placed.rekey(from, to)?;
            return match self.cache.rename(from, to) {
                Err(e) if e.kind() == io::ErrorKind::NotFound && moves_held => Ok(()),
                r => r,
            };
        }
        // The core moves content on its own (an archive, a conflict copy, another device's rename): back into the cache
        // first. A held file it renames onto is replaced: no longer the core's. The notes stay with the placeholders
        // they describe (the host moves or refills those from the feed).
        self.placed.take_back_under(from)?;
        if self.placed.is_held(to) {
            self.placed.let_go(to)?;
        }
        self.cache.rename(from, to)
    }

    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        if self.placed.is_held(from) {
            return self.copy(from, to);
        }
        self.cache.hard_link(from, to)
    }

    fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        match self.placed.held(from) {
            Some(n) => match self.placed.locate(from, &n)? {
                Some((at, _)) => copy_between(&self.placed.place_fs(), &at, &self.cache, to),
                None => Err(io::Error::new(io::ErrorKind::NotFound, format!("{from}: its content is not in its placeholder"))),
            },
            None => self.cache.copy(from, to),
        }
    }

    fn set_mtime(&self, rel: &str, mtime_ns: i64) -> io::Result<()> {
        self.placed.take_back(rel)?;
        self.cache.set_mtime(rel, mtime_ns)
    }

    fn set_exec(&self, rel: &str, exec: bool) -> io::Result<()> {
        self.placed.take_back(rel)?;
        self.cache.set_exec(rel, exec)
    }

    fn symlink(&self, target: &str, rel: &str) -> io::Result<()> {
        self.cache.symlink(target, rel)
    }

    fn read_link(&self, rel: &str) -> io::Result<String> {
        self.cache.read_link(rel)
    }

    fn sync_dir(&self, rel: &str) -> io::Result<()> {
        self.cache.sync_dir(rel)
    }

    fn name_problem(&self, name: &str) -> Option<&'static str> {
        self.cache.name_problem(name)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    struct Setup {
        _dir: tempfile::TempDir,
        place: PathBuf,
        cache: PathBuf,
        log: PathBuf,
        placed: Arc<Placed>,
        fs: PlaceFs,
    }

    fn setup() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let (place, cache, log) = (dir.path().join("Place"), dir.path().join("cache"), dir.path().join("f.placed"));
        std::fs::create_dir_all(place.join("docs")).unwrap();
        std::fs::create_dir_all(cache.join("docs")).unwrap();
        let placed = Arc::new(Placed::open(place.clone(), cache.clone(), log.clone()).unwrap());
        let fs = PlaceFs::new(placed.clone());
        Setup { _dir: dir, place, cache, log, placed, fs }
    }

    /// `p` in the cache, as fetched, then moved into its placeholder as the host does: noted held.
    fn held(s: &Setup, p: &str, content: &[u8]) -> Mark {
        std::fs::write(s.cache.join(p), content).unwrap();
        let shown = Mark::of(&s.fs.lstat(p).unwrap().unwrap());
        std::fs::copy(s.cache.join(p), s.place.join(p)).unwrap();
        let real = Mark::of(&StdFs::new(s.place.clone()).lstat(p).unwrap().unwrap());
        s.placed.put(p, Note { at: p.into(), real: Some(real), shown, exec: false, content: "v1".into(), held: true }).unwrap();
        std::fs::remove_file(s.cache.join(p)).unwrap();
        shown
    }

    #[test]
    fn a_held_file_is_read_from_its_placeholder_with_the_stamp_the_core_recorded() {
        let s = setup();
        let shown = held(&s, "docs/a.txt", b"alpha");
        let m = s.fs.lstat("docs/a.txt").unwrap().unwrap();
        assert_eq!(Mark::of(&m), shown, "unchanged since: as the core recorded it");
        assert_eq!(seedwell_sync::fs::read_all(&s.fs, "docs/a.txt").unwrap(), b"alpha");
        let names: Vec<String> = s.fs.read_dir("docs").unwrap().entries.into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["a.txt"], "listed where the cache has no file");
        // Changed in the placeholder: the core sees the change.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(s.place.join("docs/a.txt"), b"alpha!").unwrap();
        assert_eq!(s.fs.lstat("docs/a.txt").unwrap().unwrap().size, 6);
        // Without content (gone): not here, never an error; the place itself gone: an error (nothing taken for gone).
        std::fs::remove_file(s.place.join("docs/a.txt")).unwrap();
        assert_eq!(s.fs.lstat("docs/a.txt").unwrap(), None);
        assert!(s.fs.open_read("docs/a.txt").is_err());
        std::fs::remove_dir_all(&s.place).unwrap();
        assert!(s.fs.lstat("docs/a.txt").is_err());
    }

    #[test]
    fn the_core_changes_a_held_file_only_after_taking_it_back() {
        let s = setup();
        held(&s, "docs/a.txt", b"alpha");
        held(&s, "docs/b.txt", b"bravo");
        // An archive or a conflict copy: the content comes back into the cache, the placeholder stays the host's.
        s.fs.rename("docs/a.txt", "docs/a (conflict).txt").unwrap();
        assert_eq!(std::fs::read(s.cache.join("docs/a (conflict).txt")).unwrap(), b"alpha");
        assert_eq!(std::fs::read(s.place.join("docs/a.txt")).unwrap(), b"alpha");
        assert!(!s.placed.is_held("docs/a.txt") && s.placed.note("docs/a.txt").is_some(), "the note stays with its placeholder");
        assert!(s.placed.note("docs/a (conflict).txt").is_none());
        // A copy for the archive reads the placeholder.
        s.fs.hard_link("docs/b.txt", "docs/b-archived").unwrap();
        assert_eq!(std::fs::read(s.cache.join("docs/b-archived")).unwrap(), b"bravo");
        assert!(s.placed.is_held("docs/b.txt"));
        // Written to: taken back first.
        drop(s.fs.open_rw("docs/b.txt").unwrap());
        assert!(!s.placed.is_held("docs/b.txt"));
        assert_eq!(std::fs::read(s.cache.join("docs/b.txt")).unwrap(), b"bravo");
    }

    #[test]
    fn the_users_rename_moves_the_note_and_a_free_lets_go() {
        let s = setup();
        held(&s, "docs/a.txt", b"alpha");
        // The host reports the rename before it happens on disk: the content is found at its old name meanwhile.
        s.placed.host_rename("docs/a.txt", "docs/z.txt", || s.fs.rename("docs/a.txt", "docs/z.txt")).unwrap();
        assert!(s.placed.is_held("docs/z.txt") && s.placed.note("docs/a.txt").is_none());
        assert_eq!(seedwell_sync::fs::read_all(&s.fs, "docs/z.txt").unwrap(), b"alpha");
        std::fs::rename(s.place.join("docs/a.txt"), s.place.join("docs/z.txt")).unwrap();
        assert_eq!(seedwell_sync::fs::read_all(&s.fs, "docs/z.txt").unwrap(), b"alpha");
        assert_eq!(s.placed.note("docs/z.txt").unwrap().at, "docs/z.txt", "found at its new name");
        // Freed: the core lets go, the placeholder is the host's to dehydrate.
        s.fs.remove_file("docs/z.txt").unwrap();
        assert!(!s.placed.is_held("docs/z.txt") && s.placed.note("docs/z.txt").is_some());
        assert!(s.place.join("docs/z.txt").exists());
        assert_eq!(s.fs.lstat("docs/z.txt").unwrap(), None, "the cache has none");
    }

    #[test]
    fn the_notes_survive_a_restart_and_a_copy_left_in_the_cache_wins() {
        let s = setup();
        held(&s, "docs/a.txt", b"alpha");
        held(&s, "docs/b.txt", b"bravo");
        // A stop between noting and dropping the cache's copy.
        std::fs::write(s.cache.join("docs/b.txt"), b"bravo").unwrap();
        let again = Placed::open(s.place.clone(), s.cache.clone(), s.log.clone()).unwrap();
        assert!(again.is_held("docs/a.txt"));
        assert!(!again.is_held("docs/b.txt"));
        assert_eq!(again.note("docs/b.txt").unwrap().content, "v1");
    }

    #[test]
    fn taking_everything_back_moves_the_content_and_leaves_the_place_empty() {
        let s = setup();
        std::fs::create_dir_all(s.place.join("docs/deep")).unwrap();
        std::fs::create_dir_all(s.cache.join("docs/deep")).unwrap();
        held(&s, "docs/a.txt", b"alpha");
        held(&s, "docs/deep/c.txt", b"charlie");
        assert_eq!(s.placed.take_all_back().unwrap(), 2);
        assert_eq!(std::fs::read(s.cache.join("docs/deep/c.txt")).unwrap(), b"charlie");
        assert_eq!(std::fs::read_dir(&s.place).unwrap().count(), 0, "the folders it left empty go");
        assert_eq!(s.placed.held_count(), 0);
        assert!(s.placed.under("").is_empty());
    }

    #[test]
    fn content_is_checked_chunk_by_chunk() {
        let s = setup();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(s.cache.join("docs/big"), &data).unwrap();
        let fs = StdFs::new(s.cache.clone());
        let chunks = seedwell_sync::chunk::chunk_bytes(&data, seedwell_sync::chunk::Tier::for_size(data.len() as u64));
        let info = FileInfo {
            path: "docs/big".into(),
            kind: FileKind::File,
            size: data.len() as u64,
            mtime_ns: 0,
            exec: false,
            deleted: false,
            version: seedwell_sync_proto::model::Version::new(),
            modified_by: seedwell_sync_proto::ids::ShortId(0),
            file_id: [0; 16],
            symlink_target: String::new(),
            conflict_of: None,
            chunks,
        };
        assert!(holds_content(&fs, "docs/big", &info).unwrap());
        let mut other = data.clone();
        other[150_000] ^= 1;
        std::fs::write(s.cache.join("docs/big"), &other).unwrap();
        assert!(!holds_content(&fs, "docs/big", &info).unwrap());
        std::fs::write(s.cache.join("docs/big"), &data[..1000]).unwrap();
        assert!(!holds_content(&fs, "docs/big", &info).unwrap());
    }
}
