//! Listed first, hashed when first fetched (SPEC §7G.11): an on-demand folder's scan reads no file; the first download
//! reads the file once on its maker, hashing it as it streams; the copy it leaves serves it by those hashes.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use trs_sync::chunk::{Tier, chunk_bytes};
use trs_sync::clock::Clock;
use trs_sync::folder::{DeviceSigner, FolderConfig, FolderCore, RemoteIndex, Requested};
use trs_sync::fs::{FileHandle, Fs, Listing, MemFs, Meta, read_all, write_all};
use trs_sync::ignore::Ignore;
use trs_sync::index::{LocalRecord, MemStore};
use trs_sync::lazy::{ChunksEnd, JobEnd, Stream, StreamEnd, StreamError, run_job};
use trs_sync::pull::{ChunkRequest, ChunkSource, FetchError, PullReport, StreamAsk, StreamFail, StreamGot};
use trs_sync::scan::Signer;
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{FileInfo, SignedFile};

use crate::flow::{Key, learn_writers};

struct Utc(AtomicU64);
impl Clock for Utc {
    fn now_ns(&self) -> i64 {
        (1_792_938_725 + self.0.fetch_add(1, Ordering::SeqCst) as i64) * 1_000_000_000
    }
}

/// A disk that counts the bytes read from it.
#[derive(Clone)]
struct Counted {
    fs: MemFs,
    read: Arc<AtomicU64>,
    /// Runs once a copy is made (a save to the source during the copy: what it does to both).
    on_copy: Arc<Mutex<Option<CopyHook>>>,
}

/// What a save during a copy does to its source and the copy ([`Counted::on_copy`]).
type CopyHook = Box<dyn Fn(&MemFs, &str, &str) + Send>;

struct CountedFile {
    f: Box<dyn FileHandle>,
    read: Arc<AtomicU64>,
}

impl FileHandle for CountedFile {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.f.read_at(offset, buf)?;
        self.read.fetch_add(n as u64, Ordering::SeqCst);
        Ok(n)
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> io::Result<()> {
        self.f.write_at(offset, buf)
    }
    fn set_len(&self, len: u64) -> io::Result<()> {
        self.f.set_len(len)
    }
    fn len(&self) -> io::Result<u64> {
        self.f.len()
    }
    fn sync_all(&self) -> io::Result<()> {
        self.f.sync_all()
    }
}

impl Fs for Counted {
    fn lstat(&self, rel: &str) -> io::Result<Option<Meta>> {
        self.fs.lstat(rel)
    }
    fn read_dir(&self, rel: &str) -> io::Result<Listing> {
        self.fs.read_dir(rel)
    }
    fn open_read(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        Ok(Box::new(CountedFile { f: self.fs.open_read(rel)?, read: self.read.clone() }))
    }
    fn open_rw(&self, rel: &str) -> io::Result<Box<dyn FileHandle>> {
        Ok(Box::new(CountedFile { f: self.fs.open_rw(rel)?, read: self.read.clone() }))
    }
    fn create_dir(&self, rel: &str) -> io::Result<()> {
        self.fs.create_dir(rel)
    }
    fn remove_dir(&self, rel: &str) -> io::Result<()> {
        self.fs.remove_dir(rel)
    }
    fn remove_file(&self, rel: &str) -> io::Result<()> {
        self.fs.remove_file(rel)
    }
    fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        self.fs.rename(from, to)
    }
    fn hard_link(&self, from: &str, to: &str) -> io::Result<()> {
        self.fs.hard_link(from, to)
    }
    fn copy(&self, from: &str, to: &str) -> io::Result<()> {
        // (A copy reads all of the source.)
        self.read.fetch_add(self.fs.lstat(from)?.map_or(0, |m| m.size), Ordering::SeqCst);
        self.fs.copy(from, to)?;
        if let Some(hook) = self.on_copy.lock().unwrap().take() {
            hook(&self.fs, from, to);
        }
        Ok(())
    }
    fn set_mtime(&self, rel: &str, mtime_ns: i64) -> io::Result<()> {
        self.fs.set_mtime(rel, mtime_ns)
    }
    fn set_exec(&self, rel: &str, exec: bool) -> io::Result<()> {
        self.fs.set_exec(rel, exec)
    }
    fn symlink(&self, target: &str, rel: &str) -> io::Result<()> {
        self.fs.symlink(target, rel)
    }
    fn read_link(&self, rel: &str) -> io::Result<String> {
        self.fs.read_link(rel)
    }
    fn sync_dir(&self, rel: &str) -> io::Result<()> {
        self.fs.sync_dir(rel)
    }
}

struct Dev {
    disk: Counted,
    core: FolderCore,
    seed: u8,
    /// The core's clock (seconds move on with each reading, and as a test moves them).
    clock: Arc<Utc>,
}

fn dev(seed: u8, key: &Key) -> Dev {
    dev_in(seed, key, true)
}

/// A device whose folder is on demand here (`on_demand`), or full here.
fn dev_in(seed: u8, key: &Key, on_demand: bool) -> Dev {
    let fs = MemFs::new();
    fs.create_dir(".seedwell").unwrap();
    let disk = Counted { fs, read: Arc::new(AtomicU64::new(0)), on_copy: Arc::new(Mutex::new(None)) };
    let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
    let cfg = FolderConfig {
        me: dk.device_id(),
        folder_pk: *key.public_key(),
        access: key.access(),
        case_insensitive: false,
        byte_exact_names: false,
        exec_bits: true,
        archive: true,
        on_demand,
    };
    let ids = Arc::new(AtomicU64::new(u64::from(seed) << 32));
    let new_id = Arc::new(move || {
        let mut id = [0u8; 16];
        id[..8].copy_from_slice(&ids.fetch_add(1, Ordering::SeqCst).to_be_bytes());
        id
    });
    let store = Box::new(MemStore::new(u64::from(seed)));
    let clock = Arc::new(Utc(AtomicU64::new(0)));
    let core = FolderCore::new(cfg, store, Arc::new(disk.clone()), Some(dk), clock.clone(), new_id, Ignore::defaults()).unwrap();
    Dev { disk, core, seed, clock }
}

fn send(from: &Dev, to: &mut Dev) {
    learn_writers(&mut to.core, &from.core.writers()).unwrap();
    let (index_id, _) = from.core.index_state().unwrap();
    let entries = from.core.announce_since(0, usize::MAX).unwrap();
    to.core.apply_remote(from.core.me(), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();
}

fn noise(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (s >> 33) as u8
        })
        .collect()
}

/// Signs a stream's end in a test ([`Maker::sign`]).
type EndSigner<'a> = &'a (dyn Fn(FileInfo) -> [u8; 64] + Sync);

/// The maker's side of the network, as `trs-sync-net` serves it: unhashed files streamed from its disk (one
/// [`Stream`] per file, hashed as read, the last piece signed), chunks read from its disk. `meddle` runs before each
/// piece after the first (a program writing the file meanwhile). Only the version's maker streams.
struct Maker<'a> {
    disk: &'a Counted,
    me: ShortId,
    records: HashMap<String, LocalRecord>,
    signer: DeviceSigner,
    streams: Mutex<HashMap<String, Stream>>,
    meddle: Option<&'a (dyn Fn() + Sync)>,
    /// Signs the end of a stream instead of the maker (what a wrong or lying end looks like).
    sign: Option<EndSigner<'a>>,
    /// The piece at this offset is served, and its answer lost on the way (a timeout), once.
    lose: Mutex<Option<u64>>,
    /// The offsets asked for, in order.
    asked: Mutex<Vec<u64>>,
}

impl<'a> Maker<'a> {
    fn of(d: &'a Dev, meddle: Option<&'a (dyn Fn() + Sync)>) -> Maker<'a> {
        let store = d.core.store();
        let mut records = HashMap::new();
        for p in store.local_paths("", None, 10_000).unwrap() {
            records.insert(p.clone(), store.local(&p).unwrap().unwrap());
        }
        let signer = DeviceSigner::new(Arc::new(DeviceKey::from_seed(&[d.seed; 32])), d.core.folder_id());
        Maker { disk: &d.disk, me: d.core.me(), records, signer, streams: Mutex::new(HashMap::new()), meddle, sign: None, lose: Mutex::new(None), asked: Mutex::new(Vec::new()) }
    }
}

impl ChunkSource for Maker<'_> {
    fn stream(&self, ask: &StreamAsk) -> Result<StreamGot, StreamFail> {
        if ask.maker != self.me {
            return Err(StreamFail::Failed("not its maker".into()));
        }
        self.asked.lock().unwrap().push(ask.offset);
        let mut all = self.streams.lock().unwrap();
        if ask.offset == 0 {
            let rec = self.records.get(&ask.path).ok_or(StreamFail::Unavailable)?;
            all.insert(ask.path.clone(), Stream::start(self.disk, rec).map_err(|e| StreamFail::Failed(format!("{e:?}")))?);
        } else if let Some(m) = self.meddle {
            m();
        }
        let s = all.get_mut(&ask.path).ok_or(StreamFail::Restart)?;
        match s.piece(self.disk, ask.offset, ask.len) {
            Ok(p) => {
                let mut lose = self.lose.lock().unwrap();
                if *lose == Some(ask.offset) {
                    *lose = None;
                    return Err(StreamFail::Unavailable);
                }
                let sig = p.done.map(|h| match self.sign {
                    Some(sign) => sign(h.info),
                    None => {
                        // (As `trs-sync-net` serves it: what was signed is noted.)
                        let signed = self.signer.sign(h.info);
                        trs_sync::lazy::note_hashed(self.disk, &signed).unwrap();
                        signed.sig
                    }
                });
                Ok(StreamGot { data: p.data, chunks: p.chunks, sig })
            }
            Err(StreamError::Changed) => Err(StreamFail::Changed),
            Err(StreamError::OutOfOrder) => Err(StreamFail::Restart),
            Err(e) => Err(StreamFail::Failed(format!("{e:?}"))),
        }
    }

    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
        reqs.iter()
            .map(|r| {
                let fh = self.disk.open_read(&r.path).map_err(|_| FetchError::Unavailable)?;
                let mut buf = vec![0; r.len as usize];
                (fh.read_at(r.offset, &mut buf).ok() == Some(buf.len())).then_some(buf).ok_or(FetchError::Unavailable)
            })
            .collect()
    }
}

/// Pull rounds until nothing more is done, running the streams the rounds want in between (as `trs-sync-net`'s
/// streamer does, with the folder's core let go).
fn pull(to: &mut Dev, source: &dyn ChunkSource) -> PullReport {
    let name = |_: ShortId| "device".to_string();
    let mut total = PullReport::default();
    loop {
        let r = to.core.pull(source, &name, 64 << 20).unwrap();
        total.done += r.done;
        total.fetched_bytes += r.fetched_bytes;
        total.failed.extend(r.failed.clone());
        let mut settled = false;
        for job in to.core.take_jobs() {
            let path = job.path().to_string();
            let end = run_job(&*to.core.fs(), source, to.core.store(), job, &|| false, 64 << 20, &mut total.fetched_bytes).unwrap();
            settled |= match &end {
                JobEnd::Stream(e) => !matches!(e, StreamEnd::Waiting(_)),
                JobEnd::Chunks(ChunksEnd::Fetched(c)) => c.remaining.is_empty(),
                JobEnd::Chunks(_) => true,
            };
            to.core.job_ended(&path, end);
        }
        if r.done == 0 && !settled {
            return total;
        }
    }
}

/// The failure: adding a folder on demand reads every byte of it before anything is listed (0.1.18: 28 minutes for
/// 914 GB). Its scan reads no file, and records each as an unhashed version; a scan of it unchanged records nothing.
/// Both ways a folder is on demand: its copies freed here (the cache), and a full folder here that is on demand for
/// the devices without its files (the app's *Files on demand*, SPEC §7G.11, *Modes*).
#[test]
fn an_on_demand_folders_scan_reads_no_file() {
    let key = Key::rw([9; 32]);
    for freed in [true, false] {
        let mut a = dev_in(1, &key, freed);
        if !freed {
            a.core.set_list_first(true);
        }
        a.disk.fs.create_dir("Films").unwrap();
        write_all(&a.disk.fs, "Films/a.mkv", &noise(9_000_000, 1)).unwrap();
        write_all(&a.disk.fs, "notes.txt", b"hello").unwrap();
        let r = a.core.scan("").unwrap();
        assert_eq!(a.disk.read.load(Ordering::SeqCst), 0, "nothing read (freed: {freed})");
        assert_eq!((r.unhashed, r.hashed_bytes), (2, 0));
        for p in ["Films/a.mkv", "notes.txt"] {
            let rec = a.core.store().local(p).unwrap().unwrap();
            assert!(rec.file.info.is_unhashed() && !rec.invalid && !rec.dataless, "{p}: listed, valid, not hashed");
        }
        let again = a.core.scan("").unwrap();
        assert_eq!((again.changed, a.disk.read.load(Ordering::SeqCst)), (0, 0));
        // Moved away and back (as moving a folder into the cache does to every file): another inode or change time,
        // the same size and time: the same version, no change for the other devices to take.
        let seq = a.core.index_state().unwrap().1;
        a.disk.fs.rename("Films/a.mkv", "moved.mkv").unwrap();
        a.disk.fs.rename("moved.mkv", "Films/a.mkv").unwrap();
        let moved = a.core.scan("").unwrap();
        assert_eq!((moved.changed, a.disk.read.load(Ordering::SeqCst)), (0, 0));
        assert_eq!(a.core.index_state().unwrap().1, seq, "nothing announced again");
        // Changed: listed again as a new unhashed version, still not read.
        write_all(&a.disk.fs, "notes.txt", b"hello again").unwrap();
        let before = a.disk.read.load(Ordering::SeqCst);
        assert_eq!(a.core.scan("").unwrap().changed, 1);
        assert_eq!(a.disk.read.load(Ordering::SeqCst), before, "a change is listed, not read");
        assert!(a.core.store().local("notes.txt").unwrap().unwrap().file.info.is_unhashed());
    }
}

/// The failure: the folder is marked on demand for the other devices while its first scan already walks (a folder
/// added, then marked); that scan hashes every big file it found (the whole folder, for a folder of films) before
/// anything of them is announced. The mark reaches the scan between two steps, and what it has not read yet is
/// listed instead.
#[test]
fn a_folder_marked_on_demand_while_its_first_scan_walks_reads_none_of_its_big_files() {
    let key = Key::rw([9; 32]);
    let mut a = dev_in(1, &key, false);
    for i in 0..3 {
        write_all(&a.disk.fs, &format!("Film {i}.mkv"), &noise(6_000_000, i)).unwrap();
    }
    write_all(&a.disk.fs, "notes.txt", b"hello").unwrap();
    let mut job = trs_sync::scan::ScanJob::stepped("", std::time::Duration::from_secs(60), 4 << 20);
    // The walk: small files read as met, the big ones left for after it.
    assert!(!a.core.scan_step(&mut job).unwrap());
    a.core.set_list_first(true);
    let mut steps = 0;
    while !a.core.scan_step(&mut job).unwrap() {
        assert!(job.take_to_hash().is_empty(), "no big file is read once the folder lists first");
        steps += 1;
        assert!(steps < 100);
    }
    assert_eq!(a.disk.read.load(Ordering::SeqCst), 5, "only the small file the walk met before the mark");
    for i in 0..3 {
        let rec = a.core.store().local(&format!("Film {i}.mkv")).unwrap().unwrap();
        assert!(rec.file.info.is_unhashed() && !rec.invalid, "Film {i}: listed");
    }
    // The failure: a file hashed before the mark whose change time or file id moves (a tag, a move away and back) is
    // listed again as a new unhashed version, which every device takes for a change. Its hashes say it is the same.
    let seq = a.core.index_state().unwrap().1;
    a.disk.fs.rename("notes.txt", "moved.txt").unwrap();
    a.disk.fs.rename("moved.txt", "notes.txt").unwrap();
    assert_eq!(a.core.scan("").unwrap().changed, 0);
    assert_eq!(a.core.index_state().unwrap().1, seq, "nothing announced again");
    assert!(!a.core.store().local("notes.txt").unwrap().unwrap().file.info.is_unhashed(), "still the hashed version");
}

/// The failures: the first download of an unhashed file reads it twice on its maker (once to hash, once to send) or
/// takes bytes nobody checked; and the copy it leaves cannot serve another device (no hashes recorded), so the maker
/// must be there for every download.
#[test]
fn the_first_download_reads_the_file_once_and_its_copy_serves_it_by_those_hashes() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b, mut c) = (dev(1, &key), dev(2, &key), dev(3, &key));
    let film = noise(20_000_000, 7);
    a.disk.fs.create_dir("Films").unwrap();
    write_all(&a.disk.fs, "Films/a.mkv", &film).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    assert!(b.core.store().local("Films/a.mkv").unwrap().unwrap().dataless, "listed, online only");
    assert!(matches!(b.core.request("Films/a.mkv").unwrap(), Requested::Fetching(_)));
    let r = pull(&mut b, &Maker::of(&a, None));
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(read_all(&b.disk.fs, "Films/a.mkv").unwrap(), film);
    assert_eq!(a.disk.read.load(Ordering::SeqCst), film.len() as u64, "the maker read every byte once");
    let rec = b.core.store().local("Films/a.mkv").unwrap().unwrap();
    assert_eq!(rec.file.info.chunks, chunk_bytes(&film, Tier::for_size(film.len() as u64)), "the hashed version, as a scan would hash it");
    assert!(!rec.dataless && !rec.invalid);

    // C fetches it from B's copy alone, chunk by chunk, each checked against the hashes B recorded.
    send(&a, &mut c);
    send(&b, &mut c);
    pull(&mut c, &Maker::of(&b, None));
    c.core.request("Films/a.mkv").unwrap();
    let before = b.disk.read.load(Ordering::SeqCst);
    let r = pull(&mut c, &Maker::of(&b, None));
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(read_all(&c.disk.fs, "Films/a.mkv").unwrap(), film);
    assert!(b.disk.read.load(Ordering::SeqCst) > before, "served from B's copy");
    assert_eq!(a.disk.read.load(Ordering::SeqCst), film.len() as u64, "the maker read nothing more");
}

/// The failure: a file written to on its maker while it streams is recorded on the receiving device with hashes of
/// mixed content (part old, part new), which every later download then checks against.
#[test]
fn a_file_changed_while_it_streams_is_never_recorded_mixed() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b) = (dev(1, &key), dev(2, &key));
    write_all(&a.disk.fs, "f.bin", &noise(20_000_000, 2)).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    b.core.request("f.bin").unwrap();
    let once = AtomicBool::new(false);
    let fs = a.disk.fs.clone();
    let meddle = move || {
        if !once.swap(true, Ordering::SeqCst) {
            fs.open_rw("f.bin").unwrap().write_at(15_000_000, &[7; 4096]).unwrap();
        }
    };
    let r = pull(&mut b, &Maker::of(&a, Some(&meddle)));
    assert!(!r.failed.is_empty(), "the stream fails");
    let rec = b.core.store().local("f.bin").unwrap().unwrap();
    assert!(rec.dataless && rec.file.info.is_unhashed(), "nothing recorded but the listing");
    assert!(b.disk.fs.lstat("f.bin").unwrap().is_none());
    // The maker's scan finds the change; the next fetch takes the file as it is now.
    a.core.scan("").unwrap();
    let now = read_all(&a.disk.fs, "f.bin").unwrap();
    send(&a, &mut b);
    let r = pull(&mut b, &Maker::of(&a, None));
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), now);
}

/// Every file in the folder (not the state folder), for a conflict copy made where none should be.
fn files(d: &Dev) -> Vec<String> {
    let mut out = Vec::new();
    let mut dirs = vec![String::new()];
    while let Some(dir) = dirs.pop() {
        for e in d.disk.fs.read_dir(&dir).unwrap().entries {
            let p = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
            if p == ".seedwell" {
                continue;
            }
            match d.disk.fs.lstat(&p).unwrap().unwrap().kind {
                trs_sync_proto::model::FileKind::Dir => dirs.push(p),
                _ => out.push(p),
            }
        }
    }
    out.sort();
    out
}

/// The maker `a` of an unhashed film, a device `c` that streamed it (its hashed form, as `c` recorded it), and a
/// device `b` whose pull placed the film and stopped before recording it: the film at its path with the version's
/// size and time, and the note the puller makes before the rename (`bytes` the file it left there).
fn placed_and_stopped(key: &Key, on_demand: bool, announced: bool, film: &[u8], bytes: &[u8]) -> (Dev, Dev, SignedFile) {
    let (mut a, mut c) = (dev_in(1, key, false), dev(3, key));
    a.core.set_list_first(true);
    write_all(&a.disk.fs, "f.bin", film).unwrap();
    a.core.scan("").unwrap();
    let v = a.core.store().local("f.bin").unwrap().unwrap().file;
    assert!(v.info.is_unhashed());
    send(&a, &mut c);
    pull(&mut c, &Maker::of(&a, None));
    c.core.request("f.bin").unwrap();
    assert!(pull(&mut c, &Maker::of(&a, None)).failed.is_empty());
    let h = c.core.store().local("f.bin").unwrap().unwrap().file;
    assert!(!h.info.is_unhashed() && h.info.version == v.info.version);
    let mut b = dev_in(2, key, on_demand);
    send(&a, &mut b);
    if announced {
        send(&c, &mut b);
    }
    if on_demand {
        pull(&mut b, &Maker::of(&a, None));
        b.core.request("f.bin").unwrap();
    }
    write_all(&b.disk.fs, "f.bin", bytes).unwrap();
    b.disk.fs.set_mtime("f.bin", v.info.mtime_ns).unwrap();
    trs_sync::lazy::note_hashed(&b.disk.fs, &h).unwrap();
    (a, b, h)
}

/// The failure (a stop between placing a streamed file and committing its record): the restarted device's scan finds
/// the file, needed as the maker's unhashed version, takes it for new content and signs a version of its own,
/// concurrent with the maker's: a conflict copy on every device (the maker's own file one, if it loses the tie), or on
/// an on-demand device the file kept aside as a stray. The scan takes it for that version's hashed form, by its bytes
/// (the puller's note, or a device's announcement): no new version, no conflict, and the hashes announced.
#[test]
fn a_streamed_file_placed_and_not_recorded_before_a_stop_is_that_version_and_no_conflict() {
    let key = Key::rw([9; 32]);
    let film = noise(9_000_000, 11);
    for (on_demand, announced) in [(false, false), (true, false), (false, true), (true, true)] {
        let (mut a, mut b, h) = placed_and_stopped(&key, on_demand, announced, &film, &film);
        b.core.scan("").unwrap();
        let rec = b.core.store().local("f.bin").unwrap().unwrap();
        let case = format!("on demand: {on_demand}, hashes announced: {announced}");
        assert_eq!(rec.file, h, "{case}: the maker's version, with its hashes");
        assert!(!rec.dataless && !rec.invalid, "{case}: held");
        assert_eq!(b.core.need_totals().unwrap().0, 0, "{case}");
        assert_eq!(files(&b), ["f.bin"], "{case}: nothing kept aside");
        send(&b, &mut a);
        assert_eq!(a.core.need_totals().unwrap().0, 0, "{case}: no conflict on the maker");
        pull(&mut a, &Maker::of(&b, None));
        assert_eq!(files(&a), ["f.bin"], "{case}");
        assert_eq!(read_all(&a.disk.fs, "f.bin").unwrap(), film);
    }
}

/// The failure: the rescue above goes by the listing alone (size, time and bits), so other bytes at that path with the
/// same listing are taken for the maker's version, and hashes recorded for them that they do not have. Other bytes
/// are what they always were: an on-demand device keeps them aside, a full one takes them for its own change.
#[test]
fn other_bytes_with_a_streamed_files_listing_are_not_taken_for_it() {
    let key = Key::rw([9; 32]);
    let film = noise(9_000_000, 11);
    let mut other = film.clone();
    other[4_000_000] ^= 1;
    for on_demand in [false, true] {
        let (_a, mut b, h) = placed_and_stopped(&key, on_demand, false, &film, &other);
        b.core.scan("").unwrap();
        let rec = b.core.store().local("f.bin").unwrap().unwrap();
        assert_ne!(rec.file, h, "on demand: {on_demand}: not the maker's version");
        if on_demand {
            assert!(rec.dataless, "still online only");
            assert!(b.disk.fs.lstat("f.bin").unwrap().is_none(), "kept aside (in the folder's archive): {:?}", files(&b));
        } else {
            assert_eq!(rec.file.info.modified_by, b.core.me(), "this device's own change");
        }
    }
}

/// The failures at the end of a stream: the last piece's signature is not the maker's (another writer signed it),
/// is over another chunk list than the one that came, or is no signature at all; the file is placed and recorded all
/// the same, with hashes nobody vouches for. It is never placed.
#[test]
fn a_stream_whose_end_does_not_check_is_never_placed() {
    let key = Key::rw([9; 32]);
    let folder = dev(3, &key).core.folder_id();
    let other = DeviceKey::from_seed(&[3; 32]);
    let by_another = |info: FileInfo| other.sign(&trs_sync_proto::sign::file_payload(&folder, &info));
    let maker = DeviceSigner::new(Arc::new(DeviceKey::from_seed(&[1; 32])), dev(1, &key).core.folder_id());
    let other_chunks = |mut info: FileInfo| {
        info.chunks[0].hash[0] ^= 1;
        maker.sign(info).sig
    };
    let garbage = |_: FileInfo| [7u8; 64];
    let ends: [(&str, EndSigner<'_>); 3] = [("another writer", &by_another), ("other chunks", &other_chunks), ("no signature", &garbage)];
    for (what, sign) in ends {
        let (mut a, mut b) = (dev(1, &key), dev(2, &key));
        learn_writers(&mut a.core, &dev(3, &key).core.writers()).unwrap();
        write_all(&a.disk.fs, "f.bin", &noise(5_000_000, 4)).unwrap();
        a.core.scan("").unwrap();
        send(&a, &mut b);
        pull(&mut b, &Maker::of(&a, None));
        b.core.request("f.bin").unwrap();
        let source = Maker { sign: Some(sign), ..Maker::of(&a, None) };
        let r = pull(&mut b, &source);
        assert!(!r.failed.is_empty(), "{what}: the fetch fails");
        let rec = b.core.store().local("f.bin").unwrap().unwrap();
        assert!(rec.dataless && rec.file.info.is_unhashed(), "{what}: nothing recorded");
        assert!(b.disk.fs.lstat("f.bin").unwrap().is_none(), "{what}: nothing placed");
    }
}

/// The failure: an answer that does not come in time (a slow link, the maker's upload limit) gives the whole stream up,
/// and the next round starts the file again from its first byte: over a slow link a big file never arrives. The
/// next round asks the same piece again, the maker serves it once more, and the stream goes on from there.
#[test]
fn a_piece_that_timed_out_is_asked_again_and_the_stream_goes_on() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b) = (dev(1, &key), dev(2, &key));
    let film = noise(20_000_000, 8);
    write_all(&a.disk.fs, "f.bin", &film).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    b.core.request("f.bin").unwrap();
    let source = Maker::of(&a, None);
    *source.lose.lock().unwrap() = Some(8 << 20);
    let first = pull(&mut b, &source);
    assert!(first.failed.is_empty(), "waiting, not failed: {:?}", first.failed);
    assert!(b.disk.fs.lstat("f.bin").unwrap().is_none());
    let r = pull(&mut b, &source);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), film);
    assert_eq!(*source.asked.lock().unwrap(), [0, 8 << 20, 8 << 20, 16 << 20], "the lost piece again, never the first");
    assert_eq!(a.disk.read.load(Ordering::SeqCst), film.len() as u64 + (8 << 20), "every byte read once, the lost piece twice");
}

/// The failure: a folder no longer marked on demand hashes a file it listed unhashed when its change time moves (a
/// tag, an attribute, Spotlight) and signs a new version of the same content, which every device takes for a change.
/// It records the version's hashed form instead: the same version, signed again with its chunks.
#[test]
fn unmarked_a_touched_file_listed_unhashed_keeps_its_version() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b) = (dev_in(1, &key, false), dev(2, &key));
    a.core.set_list_first(true);
    write_all(&a.disk.fs, "f.bin", &noise(3_000_000, 9)).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    let v = a.core.store().local("f.bin").unwrap().unwrap().file.info;
    a.core.set_list_first(false);
    a.disk.fs.rename("f.bin", "moved.bin").unwrap();
    a.disk.fs.rename("moved.bin", "f.bin").unwrap();
    a.core.scan("").unwrap();
    let now = a.core.store().local("f.bin").unwrap().unwrap().file.info;
    assert_eq!((now.version.clone(), now.modified_by), (v.version.clone(), v.modified_by), "the same version");
    assert!(!now.is_unhashed() && now.same_content(&v), "now with its hashes");
    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap().0, 0, "no change for the other devices");
}

/// The failure: a file the maker listed unhashed is changed in place with its size and time kept (an editor that
/// restores the time); a device already fetched it (its hashes are recorded), and the maker restamps the record as the
/// same version, so the next stream signs that version with other chunks: two hashed forms of one version, a
/// conflict of itself. Its hashes known, the file is read again: other chunks are a new version.
#[test]
fn a_streamed_file_changed_with_its_size_and_time_kept_is_a_new_version() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b) = (dev_in(1, &key, false), dev(2, &key));
    a.core.set_list_first(true);
    write_all(&a.disk.fs, "f.bin", &noise(3_000_000, 10)).unwrap();
    a.core.scan("").unwrap();
    let v = a.core.store().local("f.bin").unwrap().unwrap().file.info;
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    b.core.request("f.bin").unwrap();
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    send(&b, &mut a);
    // Unchanged, only touched: the same version (read once to confirm).
    a.disk.fs.rename("f.bin", "moved.bin").unwrap();
    a.disk.fs.rename("moved.bin", "f.bin").unwrap();
    a.core.scan("").unwrap();
    assert_eq!(a.core.store().local("f.bin").unwrap().unwrap().file.info.version, v.version);
    // Changed in place, the time put back.
    a.disk.fs.open_rw("f.bin").unwrap().write_at(1_000_000, &[1; 4096]).unwrap();
    a.disk.fs.set_mtime("f.bin", v.mtime_ns).unwrap();
    a.core.scan("").unwrap();
    let now = a.core.store().local("f.bin").unwrap().unwrap().file.info;
    assert!(now.version.dominates(&v.version) && now.version != v.version, "a new version");
    send(&a, &mut b);
    assert_eq!(b.core.need_totals().unwrap().0, 1, "B takes the change");
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), read_all(&a.disk.fs, "f.bin").unwrap());
}

/// The failure: a mark that comes while a scan walks makes the next step list what was left to read, but a big file
/// that must still be read (its hashes to confirm, a need there) is read in that step, the folder's core held for as
/// long as reading it takes. It is read apart, as any big file.
#[test]
fn a_file_still_read_after_a_mark_is_read_apart_with_the_core_let_go() {
    let key = Key::rw([9; 32]);
    let mut a = dev_in(1, &key, false);
    write_all(&a.disk.fs, "Film.mkv", &noise(6_000_000, 12)).unwrap();
    a.core.scan("").unwrap();
    let v = a.core.store().local("Film.mkv").unwrap().unwrap().file.info;
    a.disk.fs.rename("Film.mkv", "moved.mkv").unwrap();
    a.disk.fs.rename("moved.mkv", "Film.mkv").unwrap();
    let mut job = trs_sync::scan::ScanJob::stepped("", std::time::Duration::from_secs(60), 4 << 20);
    assert!(!a.core.scan_step(&mut job).unwrap());
    assert!(job.take_to_hash().is_empty(), "left for after the walk");
    a.core.set_list_first(true);
    let mut tasks = Vec::new();
    loop {
        let before = a.disk.read.load(Ordering::SeqCst);
        let done = a.core.scan_step(&mut job).unwrap();
        assert_eq!(a.disk.read.load(Ordering::SeqCst), before, "nothing read with the core held");
        if done {
            break;
        }
        let t = job.take_to_hash();
        let hashed = t.iter().map(|t| t.run(&a.disk)).collect();
        tasks.extend(t);
        job.hashed(hashed);
    }
    assert_eq!(tasks.len(), 1, "read apart");
    let now = a.core.store().local("Film.mkv").unwrap().unwrap().file.info;
    assert_eq!(now.version, v.version, "its hashes confirmed: the same version");
}

/// The failure: the maker records nothing of what it signs at the end of a stream, so its scans know a version's
/// chunks only from a receiver's announcement; a receiver that never announces them (it stopped before recording)
/// leaves the maker to sign the version again with other chunks after an edit that kept the size and time. What it
/// signed is noted: unmarked and touched, the file is recorded in that hashed form; edited with its size and time kept,
/// it is a new version.
#[test]
fn the_maker_knows_what_it_signed_without_the_receivers_announcement() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b) = (dev_in(1, &key, false), dev(2, &key));
    a.core.set_list_first(true);
    write_all(&a.disk.fs, "f.bin", &noise(3_000_000, 13)).unwrap();
    a.core.scan("").unwrap();
    let v = a.core.store().local("f.bin").unwrap().unwrap().file.info;
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    b.core.request("f.bin").unwrap();
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    let h = b.core.store().local("f.bin").unwrap().unwrap().file;
    // (B's index never reaches A.)
    a.core.set_list_first(false);
    a.disk.fs.rename("f.bin", "moved.bin").unwrap();
    a.disk.fs.rename("moved.bin", "f.bin").unwrap();
    a.core.scan("").unwrap();
    let now = a.core.store().local("f.bin").unwrap().unwrap().file;
    assert_eq!(now.info, h.info, "the hashed form it signed, the same version");
    a.core.set_list_first(true);
    // Edited, size and time kept: a hashed record now, read again whatever its listing says.
    a.disk.fs.open_rw("f.bin").unwrap().write_at(1_000_000, &[1; 4096]).unwrap();
    a.disk.fs.set_mtime("f.bin", v.mtime_ns).unwrap();
    a.core.scan("").unwrap();
    let edited = a.core.store().local("f.bin").unwrap().unwrap().file.info;
    assert!(edited.version.dominates(&v.version) && edited.version != v.version, "a new version");
    // A file listed unhashed again (marked), streamed to nobody since its note: an edit that keeps the size and time
    // is read again because the maker noted what it signed.
    let (mut a, mut b) = (dev_in(1, &key, false), dev(2, &key));
    a.core.set_list_first(true);
    write_all(&a.disk.fs, "g.bin", &noise(3_000_000, 14)).unwrap();
    a.core.scan("").unwrap();
    let v = a.core.store().local("g.bin").unwrap().unwrap().file.info;
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    b.core.request("g.bin").unwrap();
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    a.disk.fs.open_rw("g.bin").unwrap().write_at(1_000_000, &[1; 4096]).unwrap();
    a.disk.fs.set_mtime("g.bin", v.mtime_ns).unwrap();
    a.core.scan("").unwrap();
    let edited = a.core.store().local("g.bin").unwrap().unwrap().file.info;
    assert!(edited.version.dominates(&v.version) && edited.version != v.version, "a new version, not V signed again with other chunks");
}

/// A maker that loses the answer of the piece at `lose`, for a stream of `f.bin` (20 MB) from `a` to `b` that is cut
/// off there in its first pull.
fn cut_off(key: &Key) -> (Dev, Dev, Vec<u8>) {
    let (mut a, mut b) = (dev(1, key), dev(2, key));
    let film = noise(20_000_000, 15);
    write_all(&a.disk.fs, "f.bin", &film).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    pull(&mut b, &Maker::of(&a, None));
    b.core.request("f.bin").unwrap();
    let source = Maker::of(&a, None);
    *source.lose.lock().unwrap() = Some(8 << 20);
    assert!(pull(&mut b, &source).failed.is_empty());
    assert!(b.disk.fs.lstat("f.bin").unwrap().is_none(), "cut off");
    (a, b, film)
}

/// The failures of a stream cut off and gone on with: another version is wanted there by then (the maker changed the
/// file), or its temp file is gone (a scan, the user), or it was left too long (the maker has let its read go), and
/// the stream goes on from where it stopped all the same, its old bytes taken for the new ones. Each starts again from
/// the first byte.
#[test]
fn a_cut_off_stream_starts_again_when_its_version_or_temp_file_changed_or_it_was_left_too_long() {
    let key = Key::rw([9; 32]);
    // Another version.
    let (mut a, mut b, _) = cut_off(&key);
    let film2 = noise(20_000_000, 16);
    write_all(&a.disk.fs, "f.bin", &film2).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let source = Maker::of(&a, None);
    assert!(pull(&mut b, &source).failed.is_empty());
    assert_eq!(source.asked.lock().unwrap().first(), Some(&0), "from the first byte");
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), film2);
    // The temp file gone.
    let (a, mut b, film) = cut_off(&key);
    let tmp = trs_sync::pull::stream_temp_path("f.bin");
    assert!(b.disk.fs.lstat(&tmp).unwrap().is_some());
    b.disk.fs.remove_file(&tmp).unwrap();
    let source = Maker::of(&a, None);
    assert!(pull(&mut b, &source).failed.is_empty());
    assert_eq!(source.asked.lock().unwrap().first(), Some(&0));
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), film);
    // Left longer than the maker keeps its read.
    let (a, mut b, film) = cut_off(&key);
    b.clock.0.fetch_add(trs_sync::lazy::STREAM_IDLE.as_secs() + 1, Ordering::SeqCst);
    let source = Maker::of(&a, None);
    assert!(pull(&mut b, &source).failed.is_empty());
    assert_eq!(source.asked.lock().unwrap().first(), Some(&0));
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), film);
}

/// A source that answers the chunks at some offsets with "not here now" once (a device that went away, a request that
/// timed out), and counts what was asked.
struct Flaky<'a> {
    inner: Maker<'a>,
    lose: Mutex<std::collections::BTreeSet<u64>>,
    asked: Mutex<Vec<(String, u64)>>,
}

impl ChunkSource for Flaky<'_> {
    fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
        let got = self.inner.fetch(reqs);
        reqs.iter()
            .zip(got)
            .map(|(r, g)| {
                self.asked.lock().unwrap().push((r.path.clone(), r.offset));
                if self.lose.lock().unwrap().remove(&r.offset) { Err(FetchError::Unavailable) } else { g }
            })
            .collect()
    }
}

/// The failures of fetching a hashed file's chunks with the core let go (as `trs-sync-net` does: the round notes what
/// must come from peers, a job fetches it, a later round places the file): what is here already fetched again, a
/// chunk that did not come lost (or the file placed without it), a chunk fetched twice, or the file placed with the
/// round's checks skipped. The changed file is built from the old one plus the changed chunks, a chunk that did not
/// come is asked for again (and only it), and both are placed and recorded as a round in one go would.
#[test]
fn a_hashed_files_chunks_fetched_apart_are_placed_as_a_round_would() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b) = (dev_in(1, &key, false), dev_in(2, &key, false));
    b.core.set_fetch_apart(true);
    let mut doc = noise(6_000_000, 31);
    write_all(&a.disk.fs, "doc.bin", &doc).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    assert_eq!(read_all(&b.disk.fs, "doc.bin").unwrap(), doc);
    // A edits a part of it, and adds a film.
    doc[3_000_000..3_010_000].fill(7);
    write_all(&a.disk.fs, "doc.bin", &doc).unwrap();
    let film = noise(9_000_000, 32);
    write_all(&a.disk.fs, "film.bin", &film).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let film_chunks = chunk_bytes(&film, Tier::for_size(film.len() as u64));
    let lost = film_chunks[0].len as u64;
    let source = Flaky { inner: Maker::of(&a, None), lose: Mutex::new([lost].into()), asked: Mutex::new(Vec::new()) };
    // The rounds alone (the core held): nothing asked of a peer, nothing of the disk read; jobs noted for both files.
    let before = b.disk.read.load(Ordering::SeqCst);
    let name = |_: ShortId| "device".to_string();
    for _ in 0..2 {
        let r = b.core.pull(&source, &name, 64 << 20).unwrap();
        assert_eq!(r.done, 0, "nothing placed before a job built it");
    }
    assert!(source.asked.lock().unwrap().is_empty(), "the rounds asked a peer for nothing");
    assert_eq!(b.disk.read.load(Ordering::SeqCst), before, "the rounds read nothing of the files");
    let jobs = b.core.take_jobs();
    assert_eq!(jobs.iter().map(|j| j.path().to_string()).collect::<Vec<_>>(), ["doc.bin", "film.bin"]);
    for j in jobs {
        b.core.streams().lock().unwrap().cancelled(j.path());
    }
    // (The pull goes round by round, the jobs in between, until nothing more comes.)
    let r = pull(&mut b, &source);
    assert!(r.failed.is_empty(), "{:?}", r.failed);
    assert_eq!(read_all(&b.disk.fs, "doc.bin").unwrap(), doc, "the edit, placed");
    assert_eq!(read_all(&b.disk.fs, "film.bin").unwrap(), film, "whole, its lost chunk asked for again");
    let asked = source.asked.lock().unwrap().clone();
    let of = |p: &str| asked.iter().filter(|(q, _)| q == p).map(|(_, o)| *o).collect::<Vec<u64>>();
    assert!(of("doc.bin").len() <= 3, "only the edited chunks of the document: {:?}", of("doc.bin"));
    let film_asked = of("film.bin");
    assert_eq!(film_asked.len(), film_chunks.len() + 1, "each of the film's chunks once, the lost one twice");
    assert_eq!(film_asked.iter().filter(|o| **o == lost).count(), 2);
    for p in ["doc.bin", "film.bin"] {
        let (ours, theirs) = (b.core.store().local(p).unwrap().unwrap().file, a.core.store().local(p).unwrap().unwrap().file);
        assert_eq!(ours, theirs, "{p}: recorded as A's version");
    }
    assert_eq!(b.core.need_totals().unwrap().0, 0);
}

/// A device `b` with `doc.bin` of `a`'s, and `a`'s edit of it (one part changed) announced to `b`: its fetch builds on
/// `b`'s old version.
fn edited(key: &Key) -> (Dev, Dev, Vec<u8>, Vec<u8>) {
    let (mut a, mut b) = (dev_in(1, key, false), dev_in(2, key, false));
    b.core.set_fetch_apart(true);
    let old = noise(6_000_000, 51);
    write_all(&a.disk.fs, "doc.bin", &old).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    let mut new = old.clone();
    new[3_000_000..3_010_000].fill(7);
    write_all(&a.disk.fs, "doc.bin", &new).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    (a, b, old, new)
}

/// The failure (the review's): the user saves the old version while it is cloned into the temp file (a full copy on
/// NTFS or ext4 takes long), so the clone holds part of the save; the old version's hashes are trusted for it, only
/// the changed chunks come from peers, and the file is placed with the save's bytes mixed in, recorded under the new
/// version's hashes. A clone whose source changed during the copy is never trusted: what is placed, at the path or as
/// a conflict copy, is the remote version or the save, each whole.
#[test]
fn a_save_during_the_clone_is_never_placed_mixed_with_the_new_version() {
    let key = Key::rw([9; 32]);
    let (a, mut b, _old, new) = edited(&key);
    let saved = noise(6_000_000, 52);
    let s2 = saved.clone();
    // During the copy: the user's save reaches both the file and the part of the copy read after it.
    *b.disk.on_copy.lock().unwrap() = Some(Box::new(move |fs: &MemFs, from: &str, to: &str| {
        write_all(fs, from, &s2).unwrap();
        // (Older than the new version, so it loses the conflict and the new version is placed over it.)
        fs.set_mtime(from, 1_000_000_000).unwrap();
        fs.open_rw(to).unwrap().write_at(1_000_000, &s2[1_000_000..2_000_000]).unwrap();
    }));
    // A round notes the job; the job clones (the save comes in during the copy) and fetches; a scan finds the save
    // before a round places the file (the user's version, a conflict with the new one); then the rounds go on.
    let name = |_: ShortId| "device".to_string();
    b.core.pull(&Maker::of(&a, None), &name, 64 << 20).unwrap();
    for j in b.core.take_jobs() {
        let path = j.path().to_string();
        let end = run_job(&*b.core.fs(), &Maker::of(&a, None), b.core.store(), j, &|| false, 64 << 20, &mut 0).unwrap();
        b.core.job_ended(&path, end);
    }
    b.core.scan("").unwrap();
    for _ in 0..3 {
        pull(&mut b, &Maker::of(&a, None));
        b.core.scan("").unwrap();
    }
    for f in files(&b) {
        let bytes = read_all(&b.disk.fs, &f).unwrap();
        assert!(bytes == new || bytes == saved, "{f}: neither the new version nor the save, whole");
    }
    assert!(files(&b).iter().any(|f| read_all(&b.disk.fs, f).unwrap() == saved), "the save is kept");
}

/// The failure: a round checks a temp file left over (a stop, a restart) chunk by chunk, every byte of it read with
/// the core held (tens of seconds for a big file), and again every round while no device has the rest. The round
/// reads nothing; the job checks it, and the file is placed from what was there and what came.
#[test]
fn a_temp_file_left_over_is_checked_by_the_job_not_the_round() {
    let key = Key::rw([9; 32]);
    let (mut a, mut b) = (dev_in(1, &key, false), dev_in(2, &key, false));
    b.core.set_fetch_apart(true);
    let film = noise(9_000_000, 53);
    write_all(&a.disk.fs, "film.bin", &film).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    // Half of it in a temp file, as a stop leaves it.
    write_all(&b.disk.fs, &trs_sync::pull::temp_path("film.bin"), &film[..4_500_000]).unwrap();
    let before = b.disk.read.load(Ordering::SeqCst);
    let name = |_: ShortId| "device".to_string();
    let source = Maker::of(&a, None);
    for _ in 0..3 {
        b.core.pull(&source, &name, 64 << 20).unwrap();
    }
    assert_eq!(b.disk.read.load(Ordering::SeqCst), before, "the rounds read nothing of the temp file");
    let jobs = b.core.take_jobs();
    assert_eq!(jobs.len(), 1);
    for j in jobs {
        b.core.streams().lock().unwrap().cancelled(j.path());
    }
    let asked_before = a.disk.read.load(Ordering::SeqCst);
    assert!(pull(&mut b, &source).failed.is_empty());
    assert_eq!(read_all(&b.disk.fs, "film.bin").unwrap(), film);
    assert!(a.disk.read.load(Ordering::SeqCst) - asked_before < film.len() as u64, "what was in the temp file not fetched again");
}

/// The failures between a job and the round that places its file: another version is wanted by then (the job's file
/// placed for it), or the temp file was changed (its bytes placed unchecked). Another version is built anew; a changed
/// temp file is checked again chunk by chunk (by a job), and only its good chunks kept.
#[test]
fn a_job_for_an_old_version_or_a_changed_temp_file_is_never_placed_as_it_is() {
    let key = Key::rw([9; 32]);
    let name = |_: ShortId| "device".to_string();
    // Another version while the job runs.
    let (mut a, mut b) = (dev_in(1, &key, false), dev_in(2, &key, false));
    b.core.set_fetch_apart(true);
    write_all(&a.disk.fs, "f.bin", &noise(5_000_000, 54)).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    b.core.pull(&Maker::of(&a, None), &name, 64 << 20).unwrap();
    let jobs = b.core.take_jobs();
    let v2 = noise(5_000_000, 55);
    write_all(&a.disk.fs, "f.bin", &v2).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let old_source = Maker::of(&a, None);
    for j in jobs {
        let path = j.path().to_string();
        // (Built from what the maker has now: the job's chunks are of the old version, so they do not come.)
        let end = run_job(&*b.core.fs(), &old_source, b.core.store(), j, &|| false, 64 << 20, &mut 0).unwrap();
        b.core.job_ended(&path, end);
    }
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), v2, "the version wanted now");
    // A temp file changed after its job.
    let (mut a, mut b) = (dev_in(1, &key, false), dev_in(2, &key, false));
    b.core.set_fetch_apart(true);
    let film = noise(5_000_000, 56);
    write_all(&a.disk.fs, "f.bin", &film).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    b.core.pull(&Maker::of(&a, None), &name, 64 << 20).unwrap();
    for j in b.core.take_jobs() {
        let path = j.path().to_string();
        let end = run_job(&*b.core.fs(), &Maker::of(&a, None), b.core.store(), j, &|| false, 64 << 20, &mut 0).unwrap();
        b.core.job_ended(&path, end);
    }
    b.disk.fs.open_rw(&trs_sync::pull::temp_path("f.bin")).unwrap().write_at(2_000_000, &[0; 100]).unwrap();
    assert!(pull(&mut b, &Maker::of(&a, None)).failed.is_empty());
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), film, "the changed part fetched again");
}

/// The failure: a job's failure is kept by path alone, so the next round fails another version wanted there by then
/// for it (a version that never failed). It fails only the version it was of.
#[test]
fn a_failed_job_fails_only_its_own_version() {
    let key = Key::rw([9; 32]);
    let name = |_: ShortId| "device".to_string();
    let (mut a, mut b) = (dev_in(1, &key, false), dev_in(2, &key, false));
    b.core.set_fetch_apart(true);
    write_all(&a.disk.fs, "f.bin", &noise(5_000_000, 57)).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    b.core.pull(&Maker::of(&a, None), &name, 64 << 20).unwrap();
    let jobs = b.core.take_jobs();
    for j in jobs {
        let path = j.path().to_string();
        b.core.job_ended(&path, trs_sync::lazy::JobEnd::Chunks(trs_sync::lazy::ChunksEnd::Failed("a device sent nonsense".into())));
    }
    let v2 = noise(5_000_000, 58);
    write_all(&a.disk.fs, "f.bin", &v2).unwrap();
    a.core.scan("").unwrap();
    send(&a, &mut b);
    let r = pull(&mut b, &Maker::of(&a, None));
    assert!(r.failed.is_empty(), "not the old version's failure: {:?}", r.failed);
    assert_eq!(read_all(&b.disk.fs, "f.bin").unwrap(), v2);
}


/// While the folder is Read only (SPEC §7G.1) members pass the owner's versions on, but an unhashed one (no chunk hash
/// signed yet, §7G.11) is asked of its maker, the owner, alone: never of a member that holds or claims it, whose bytes
/// nothing could check. Fails when a device asks anyone but the owner for an unhashed owner's version, or takes its
/// bytes by chunk from anyone.
#[test]
fn under_read_only_an_unhashed_owners_version_is_asked_of_the_owner_alone() {
    /// Every device but the owner: notes whom each stream is asked of, and answers nothing.
    struct Others {
        asked: Mutex<Vec<ShortId>>,
        fetched: AtomicU64,
    }
    impl ChunkSource for Others {
        fn stream(&self, ask: &StreamAsk) -> Result<StreamGot, StreamFail> {
            self.asked.lock().unwrap().push(ask.maker);
            Err(StreamFail::Unavailable)
        }
        fn fetch(&self, reqs: &[ChunkRequest]) -> Vec<Result<Vec<u8>, FetchError>> {
            self.fetched.fetch_add(reqs.len() as u64, Ordering::SeqCst);
            reqs.iter().map(|r| Ok(vec![b'x'; r.len as usize])).collect()
        }
    }
    let key = Key::rw([9; 32]);
    let owners = [*DeviceKey::from_seed(&[1; 32]).public_key()];
    let mut a = dev(1, &key);
    let mut c = dev_in(3, &key, false);
    for d in [&mut a, &mut c] {
        d.core.set_read_only(Some(&owners[..])).unwrap();
    }
    let film = noise(300_000, 7);
    write_all(&a.disk.fs, "film.mkv", &film).unwrap();
    a.core.scan("").unwrap();
    assert!(a.core.store().local("film.mkv").unwrap().unwrap().file.info.is_unhashed());
    send(&a, &mut c);
    let others = Others { asked: Mutex::new(Vec::new()), fetched: AtomicU64::new(0) };
    pull(&mut c, &others);
    let asked = others.asked.lock().unwrap().clone();
    assert!(!asked.is_empty() && asked.iter().all(|m| *m == a.core.me()), "asked of another device: {asked:?}");
    assert_eq!(others.fetched.load(Ordering::SeqCst), 0, "an unhashed version's bytes taken by chunk");
    assert!(read_all(&c.disk.fs, "film.mkv").is_err(), "placed without its maker");
    pull(&mut c, &Maker::of(&a, None));
    assert_eq!(read_all(&c.disk.fs, "film.mkv").unwrap(), film);
}
