//! On-demand folders (SPEC §7G.11): the items a host shows (File Provider on the Mac, a Cloud Files sync root on
//! Windows), with stable identifiers, and the change feed it follows.
//!
//! The local index is the whole tree: every file this device knows of has a local record, with its content
//! (downloaded) or without (dataless: online only). So the host's tree is read from local records alone: children
//! by path range, changes by our own sequence numbers (the anchor a host keeps), identifiers from the file id a
//! version carries across renames.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;

use sha2::{Digest, Sha256};
use trs_sync_proto::ids::ShortId;
use trs_sync_proto::model::{Causality, FileInfo, FileKind};
use trs_sync_proto::msg::HeldFile;
use trs_sync_proto::msg::encode_entry;

use crate::index::{IndexStore, LocalRecord};

/// The identifier of the folder's root item.
pub const ROOT_ID: &str = "root";

/// Identifiers of items without a file id of their own (a second live path with the same file id): this prefix and
/// the path.
const PATH_ID: &str = "p:";

/// Where an item's content is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemState {
    /// Not on this device; opening it fetches it.
    OnlineOnly,
    /// Asked for (or kept on this device) and on its way.
    Downloading,
    /// On this device, and on at least one other.
    Downloaded,
    /// On this device only: a change made here that no other device holds yet. It cannot be removed from this
    /// device until one does.
    ModifiedHere,
    /// Not on this device, and no device announces this version held (every device that held it was removed from the
    /// folder, or reset its index and has not announced it again): a fetch reports [`UNAVAILABLE`]. It can be deleted
    /// like any item, and becomes fetchable when a device announces it held. A report only: nothing acts on it.
    Unavailable,
}

/// What a fetch of an [`ItemState::Unavailable`] item reports. (It can be passing: a device that reset its index announces
/// it again; the item comes back when one announces it held.)
pub const UNAVAILABLE: &str = "No device has this version right now";

/// One item of an on-demand folder, as a host shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Stable across renames and moves ([`ROOT_ID`] for the folder itself).
    pub id: String,
    /// The parent's identifier ([`ROOT_ID`] for items at the top; the root's own parent is itself).
    pub parent_id: String,
    /// The path in the folder (wire form, `""` for the root).
    pub path: String,
    /// The name (the last path component; the folder's name is the host's).
    pub name: String,
    /// File, directory or link.
    pub kind: FileKind,
    /// Bytes (0 for directories and links).
    pub size: u64,
    /// Modification time, Unix nanoseconds.
    pub mtime_ns: i64,
    /// The executable bit.
    pub exec: bool,
    /// A link's target.
    pub symlink_target: String,
    /// Changes exactly when the content does (kind, size, chunks, the exec bit, a link's target).
    pub content_version: String,
    /// Changes whenever the version does (content, name, time or any device's change of it).
    pub metadata_version: String,
    /// Where the content is.
    pub state: ItemState,
    /// Below a folder kept on this device (pinned), or pinned itself.
    pub pinned: bool,
    /// A conflict copy: the path it is a conflict of.
    pub conflict_of: Option<String>,
}

/// A page of a directory's children, in path order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    /// The items.
    pub items: Vec<Item>,
    /// Pass this to get the next page; `None` when this was the last.
    pub next: Option<String>,
}

/// What changed since an anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangePage {
    /// Items new or changed (each as it is now).
    pub items: Vec<Item>,
    /// Identifiers of items deleted.
    pub deleted: Vec<String>,
    /// The anchor to ask from next time.
    pub anchor: u64,
    /// More changes follow (ask again from `anchor`).
    pub more: bool,
}

/// The anchor a host keeps: our index sequence in the low 48 bits and 16 bits of the index id above them, so an anchor
/// from before the index was reset (a new id) is recognised as expired, however far the new sequence has come. `0` is
/// "from the start".
pub fn anchor_of(index_id: u64, sequence: u64) -> u64 {
    ((index_id & 0xffff) << 48) | (sequence & ((1 << 48) - 1))
}

/// The index an anchor is of (16 bits of its id: an index made anew has another).
pub fn anchor_index(anchor: u64) -> u64 {
    anchor >> 48
}

/// The sequence an anchor carries (its low 48 bits), whatever index it is of.
pub fn anchor_sequence(anchor: u64) -> u64 {
    anchor & ((1 << 48) - 1)
}

/// The sequence an anchor stands for in this index, or `None` when it is from another (reset) index or from the future:
/// the host lists everything again (File Provider's "sync anchor expired").
pub fn sequence_of(anchor: u64, index_id: u64, sequence_now: u64) -> Option<u64> {
    if anchor == 0 {
        return Some(0);
    }
    let seq = anchor & ((1 << 48) - 1);
    (anchor >> 48 == index_id & 0xffff && seq <= sequence_now).then_some(seq)
}

/// What [`crate::folder::FolderCore::evict`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Evicted {
    /// Files whose content was removed from this device.
    pub files: u64,
    /// Their bytes.
    pub bytes: u64,
    /// Files kept: no other device that has them is connected, or they are pinned.
    pub kept: u64,
    /// The files kept, and why.
    pub kept_why: Vec<(String, String)>,
    /// Files whose content could not be removed (open elsewhere, say), and why: they stay downloaded.
    pub failed: Vec<(String, String)>,
}

/// What [`crate::folder::FolderCore::host_delete`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deleted {
    /// The items became deletes of this device, which the other devices take: this many. `failed` are the ones that
    /// could not be removed from the cache (open elsewhere, say), and why: they stay, and so does every folder above
    /// them.
    Deleted {
        /// Items deleted.
        items: u64,
        /// Items that stayed, and why.
        failed: Vec<(String, String)>,
    },
    /// A read-only member cannot delete for everyone: the delete freed the content instead (as
    /// [`crate::folder::FolderCore::evict`]); the items stay, online only, and nothing goes to the other devices. The
    /// host puts them back (Mac: `deletionRejected`; Windows: the delete fails and the files are dehydrated).
    Freed(Evicted),
}

/// File id → path of every live record, kept up to date from our sequence numbers.
#[derive(Default)]
pub(crate) struct Ids {
    seq: u64,
    by_id: HashMap<[u8; 16], String>,
    /// Paths that got a path identifier (a second live path with a file id another path holds), with that file id.
    /// Kept after they go, so their delete reports the identifier the host knows.
    dups: HashMap<String, [u8; 16]>,
    /// The file id each path had when last read, so a path that gets another one (deleted and made anew elsewhere,
    /// taken in one go) lets go of the old one. (The feed reports it from the store's record of it.)
    fid_of: HashMap<String, [u8; 16]>,
    /// Devices that are no members any more: a copy there is no copy ([`Items::set_not_members`]).
    not_members: BTreeSet<ShortId>,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex16(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A live item: a record of something this device shows (not a delete).
pub fn live(rec: &LocalRecord) -> bool {
    !rec.file.info.deleted && !(rec.invalid && rec.stamp.is_none() && !rec.dataless)
}

/// Of two live paths with one file id, `a` holds the file id's identifier over `b`: the newer version, or (neither
/// newer: two devices renamed it at once) the first path. Only what the records are decides, never the order they
/// were read in, so every device and every restart gives the same answer.
fn holds_id(a: &LocalRecord, b: &LocalRecord) -> bool {
    match a.file.info.version.compare(&b.file.info.version) {
        Causality::Greater => true,
        Causality::Lesser => false,
        _ => a.file.info.path < b.file.info.path,
    }
}

impl Ids {
    /// Reads every record changed since the last call.
    pub(crate) fn catch_up(&mut self, store: &dyn IndexStore) -> io::Result<()> {
        loop {
            let page = store.local_since(self.seq, 1000)?;
            let Some(last) = page.last().map(|r| r.sequence) else { return Ok(()) };
            let mut touched: HashMap<[u8; 16], BTreeSet<String>> = HashMap::new();
            for rec in page {
                let (fid, path) = (rec.file.info.file_id, rec.file.info.path);
                if let Some(old) = self.fid_of.insert(path.clone(), fid)
                    && old != fid
                {
                    touched.entry(old).or_default().insert(path.clone());
                }
                touched.entry(fid).or_default().insert(path);
            }
            for (fid, mut paths) in touched {
                // Every path that has (or had) this file id: the holder, the second paths, the ones just changed.
                paths.extend(self.by_id.get(&fid).cloned());
                paths.extend(self.dups.iter().filter(|(_, f)| **f == fid).map(|(p, _)| p.clone()));
                let mut alive = Vec::new();
                for p in paths {
                    if let Some(r) = store.local(&p)?.filter(|r| live(r) && r.file.info.file_id == fid) {
                        alive.push(r);
                    }
                }
                let Some(holder) = alive.iter().reduce(|a, b| if holds_id(a, b) { a } else { b }).map(|r| r.file.info.path.clone()) else {
                    self.by_id.remove(&fid);
                    continue;
                };
                for r in &alive {
                    if r.file.info.path != holder {
                        self.dups.insert(r.file.info.path.clone(), fid);
                    }
                }
                self.dups.remove(&holder);
                self.by_id.insert(fid, holder);
            }
            self.seq = last;
        }
    }

    /// The identifier `path` has now, if it is a holder or a second path.
    fn id_now(&self, path: &str) -> Option<String> {
        let fid = self.fid_of.get(path)?;
        if self.by_id.get(fid).is_some_and(|p| p == path) {
            Some(hex(fid))
        } else {
            self.dups.contains_key(path).then(|| format!("{PATH_ID}{path}"))
        }
    }

    /// The live second paths of `fid` (path identifiers), other than its holder.
    fn seconds(&self, fid: &[u8; 16]) -> Vec<String> {
        self.dups.iter().filter(|(_, f)| *f == fid).map(|(p, _)| p.clone()).collect()
    }

    /// The identifier of the item at `path` (its record `rec`).
    pub(crate) fn id_of(&self, path: &str, rec: &LocalRecord) -> String {
        if path.is_empty() {
            return ROOT_ID.into();
        }
        match self.by_id.get(&rec.file.info.file_id) {
            Some(p) if p == path => hex(&rec.file.info.file_id),
            _ => format!("{PATH_ID}{path}"),
        }
    }

    /// The identifier a delete at `path` (its tombstone `rec`) reports, if any: the path identifier of a second path
    /// with a file id, nothing when the file id lives on at another path (a rename: the item changed, it was not
    /// deleted), else the file id's.
    pub(crate) fn deleted_id(&self, path: &str, rec: &LocalRecord) -> Option<String> {
        if self.dups.get(path) == Some(&rec.file.info.file_id) {
            return Some(format!("{PATH_ID}{path}"));
        }
        match self.by_id.get(&rec.file.info.file_id) {
            Some(p) if p != path => None,
            _ => Some(hex(&rec.file.info.file_id)),
        }
    }

    /// The path of an identifier, if it names a live item.
    pub(crate) fn path_of(&self, id: &str) -> Option<String> {
        if id == ROOT_ID {
            return Some(String::new());
        }
        if let Some(p) = id.strip_prefix(PATH_ID) {
            return Some(p.to_string());
        }
        self.by_id.get(&unhex16(id)?).cloned()
    }
}

/// An item's content version (as [`Item::content_version`]), for a host's check of what it saw.
pub fn content_version_of(i: &FileInfo) -> String {
    content_version(i)
}

/// Changes exactly when the content does.
pub(crate) fn content_version(i: &FileInfo) -> String {
    let mut h = Sha256::new();
    h.update([i.kind as u8, u8::from(i.exec), u8::from(i.deleted)]);
    h.update(i.size.to_le_bytes());
    h.update(i.symlink_target.as_bytes());
    for c in &i.chunks {
        h.update(c.hash);
    }
    hex(&h.finalize()[..16])
}

/// Changes whenever the version does.
pub(crate) fn metadata_version(rec: &LocalRecord) -> String {
    let mut out = Vec::new();
    encode_entry(&mut out, 0, false, &rec.file);
    hex(&Sha256::digest(&out)[..16])
}

/// `path` is `pin` or below it (`""` pins the whole folder).
pub(crate) fn under(path: &str, pin: &str) -> bool {
    pin.is_empty() || path == pin || path.strip_prefix(pin).is_some_and(|r| r.starts_with('/'))
}

/// `path` is pinned (it or a folder above it).
pub(crate) fn pinned(pins: &BTreeSet<String>, path: &str) -> bool {
    pins.iter().any(|p| under(path, p))
}

/// The item for `rec` at `path`.
pub(crate) fn item(store: &dyn IndexStore, ids: &Ids, pins: &BTreeSet<String>, rec: &LocalRecord) -> io::Result<Item> {
    let i = &rec.file.info;
    let path = i.path.clone();
    let (parent, name) = match path.rfind('/') {
        Some(n) => (path[..n].to_string(), path[n + 1..].to_string()),
        None => (String::new(), path.clone()),
    };
    let parent_id = match store.local(&parent)? {
        Some(p) if !parent.is_empty() => ids.id_of(&parent, &p),
        _ => ROOT_ID.into(),
    };
    let state = if rec.dataless {
        if store.unavailable_at(&rec.file.info.path)?.is_some() {
            ItemState::Unavailable
        } else if rec.want {
            ItemState::Downloading
        } else {
            ItemState::OnlineOnly
        }
    } else if i.kind == FileKind::File && only_here(store, rec, &ids.not_members)? {
        ItemState::ModifiedHere
    } else {
        ItemState::Downloaded
    };
    Ok(Item {
        id: ids.id_of(&path, rec),
        parent_id,
        name,
        kind: i.kind,
        size: i.size,
        mtime_ns: i.mtime_ns,
        exec: i.exec,
        symlink_target: i.symlink_target.clone(),
        content_version: content_version(i),
        metadata_version: metadata_version(rec),
        state,
        pinned: pinned(pins, &path),
        conflict_of: i.conflict_of.clone(),
        path,
    })
}

/// This device holds the only copy of its version: no other member (none of `not_members`) announces it held (valid, with its content, the
/// same version). Where it came from says nothing: the device it was taken from may have let go of it since.
pub(crate) fn only_here(store: &dyn IndexStore, rec: &LocalRecord, not_members: &BTreeSet<ShortId>) -> io::Result<bool> {
    let i = &rec.file.info;
    Ok(!store.remote(&i.path)?.iter().any(|(d, e)| {
        !not_members.contains(d)
            && !e.invalid && !e.dataless && e.file.info.version.compare(&i.version) == Causality::Equal && e.file.info.same_content(i)
    }))
}

/// A device asked whether it holds files (SPEC §7G.11): what it answers from, read without waiting for its folder's
/// core (the read-only index handle, the folder's disk, the files it is freeing itself).
pub struct Holder<'a> {
    /// The index.
    pub store: &'a dyn IndexStore,
    /// The folder's disk (the cache, for an on-demand folder).
    pub fs: &'a dyn crate::fs::Fs,
    /// This device.
    pub me: ShortId,
    /// This device writes the folder (a read-only member's own change is never sent: its copy is no copy).
    pub writer: bool,
    /// The folder is on demand here.
    pub on_demand: bool,
    /// Files this device is freeing itself right now (from its first question to the end of its free).
    pub freeing: &'a dyn Fn(&str) -> bool,
    /// The folder's ignore patterns: an ignored file is not this device's to keep (it may go any time, unsynced).
    pub ignore: &'a crate::ignore::Ignore,
}

impl Holder<'_> {
    /// Yes for a file whose content this device holds (valid, not dataless, its disk as recorded) at this version or a
    /// strictly later one, and, when on demand, only if this device's id sorts above the asker's (so of two on-demand
    /// holders the higher never relies on the lower). Never from a read-only member, from a folder whose marker is gone
    /// (moved to the Trash, a disk unmounted), for a file this device is freeing or ignores. (The driver answers no for a
    /// folder that is not running.)
    pub fn answer(&self, asker: ShortId, files: &[HeldFile]) -> Vec<bool> {
        let rank = !self.on_demand || self.me.0 > asker.0;
        let here = self.writer && self.fs.lstat(trs_sync_proto::model::STATE_DIR).ok().flatten().is_some_and(|m| m.kind == FileKind::Dir);
        files
            .iter()
            .map(|h| {
                rank && here
                    && !(self.freeing)(&h.path)
                    && !self.ignore.is_ignored(&h.path, false)
                    && self.store.local(&h.path).ok().flatten().is_some_and(|r| {
                        let i = &r.file.info;
                        i.kind == FileKind::File
                            && crate::pull::holds(&r)
                            && !r.invalid
                            && match i.version.compare(&h.version) {
                                Causality::Equal => i.content_digest() == h.content,
                                Causality::Greater => true,
                                _ => false,
                            }
                            && self.fs.lstat(&h.path).ok().flatten().is_some_and(|m| {
                                r.stamp == Some(crate::index::Stamp { size: m.size, mtime_ns: m.mtime_ns, ctime_ns: m.ctime_ns, inode: m.inode })
                            })
                    })
            })
            .collect()
    }
}

/// Why a file is not freed when no device can stand in for it.
pub const NOT_HELD_ELSEWHERE: &str = "no other device that has it is connected";

/// The files this device holds that no other member (none of `not_members`) is known to hold (at this version or a
/// later one, with the content): if this device stops syncing the folder, the others lose them (SPEC §7G.11).
pub fn files_only_here(store: &dyn IndexStore, not_members: &BTreeSet<ShortId>) -> io::Result<u64> {
    let mut n = 0;
    let mut after: Option<String> = None;
    loop {
        let page = store.local_paths("", after.as_deref(), 1000)?;
        let Some(last) = page.last().cloned() else { return Ok(n) };
        for p in &page {
            if let Some(r) = store.local(p)?
                && r.file.info.kind == FileKind::File
                && !r.dataless
                && !r.invalid
                && !r.file.info.deleted
                && !held_at_path(store, &r.file.info, not_members)?
            {
                n += 1;
            }
        }
        after = Some(last);
    }
}

/// Some device (a member: none of `not_members`) announces this version (or a later one) held at its own path: it
/// can be fetched from there.
pub(crate) fn held_at_path(store: &dyn IndexStore, i: &FileInfo, not_members: &BTreeSet<ShortId>) -> io::Result<bool> {
    Ok(store.remote(&i.path)?.iter().any(|(d, e)| !not_members.contains(d) && !e.invalid && !e.dataless && !e.file.info.deleted && covers(&e.file.info, i)))
}

/// The same bytes (size and chunks: not the exec bit).
pub(crate) fn same_bytes(a: &FileInfo, b: &FileInfo) -> bool {
    a.size == b.size && a.chunks == b.chunks
}

/// This device holds `i`'s bytes in some file (the puller copies them from there).
pub(crate) fn bytes_here(store: &dyn IndexStore, i: &FileInfo) -> io::Result<bool> {
    let Some(first) = i.chunks.first() else { return Ok(true) };
    for (p, _) in store.chunk_sources(&first.hash, 64)? {
        if store.local(&p)?.is_some_and(|r| !r.dataless && !r.invalid && !r.file.info.deleted && same_bytes(&r.file.info, i)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `held` (another device's version) stands for `ours`: the same version and content, or a later version.
pub(crate) fn covers(held: &FileInfo, ours: &FileInfo) -> bool {
    match held.version.compare(&ours.version) {
        Causality::Equal => held.same_content(ours),
        Causality::Greater => true,
        _ => false,
    }
}

/// The items of an on-demand folder, read from its index: what a host lists, looks up and follows. Keeps the item
/// identifiers (file id → path), brought up to date from the index's sequence numbers at every call, so it works on
/// any handle on the index (the folder's own, or a read-only one that never waits for it).
#[derive(Default)]
pub struct Items {
    ids: Ids,
}

impl Items {
    /// The devices that are no members any more (a copy there is no copy: a file only they hold besides this device is
    /// shown as only here).
    pub fn set_not_members(&mut self, not_members: &BTreeSet<ShortId>) {
        self.ids.not_members.clone_from(not_members);
    }

    /// The path of an identifier, if it names a live item.
    pub fn path_of(&mut self, store: &dyn IndexStore, id: &str) -> io::Result<Option<String>> {
        self.ids.catch_up(store)?;
        Ok(match self.ids.path_of(id) {
            Some(p) if p.is_empty() => Some(p),
            Some(p) => store.local(&p)?.filter(live).map(|_| p),
            None => None,
        })
    }

    /// The item at `path` (`""` is the root, a directory).
    pub fn item_at(&mut self, store: &dyn IndexStore, pins: &BTreeSet<String>, path: &str) -> io::Result<Option<Item>> {
        if path.is_empty() {
            return Ok(Some(root(pins)));
        }
        self.ids.catch_up(store)?;
        match store.local(path)?.filter(live) {
            Some(rec) => Ok(Some(item(store, &self.ids, pins, &rec)?)),
            None => Ok(None),
        }
    }

    /// The item with this identifier.
    pub fn item(&mut self, store: &dyn IndexStore, pins: &BTreeSet<String>, id: &str) -> io::Result<Option<Item>> {
        match self.path_of(store, id)? {
            Some(p) => self.item_at(store, pins, &p),
            None => Ok(None),
        }
    }

    /// A page of the children of the directory item `id`, in path order, starting after `cursor` (the `next` of the
    /// page before). `None` when `id` names no directory.
    pub fn children(
        &mut self,
        store: &dyn IndexStore,
        pins: &BTreeSet<String>,
        id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> io::Result<Option<Page>> {
        let Some(dir) = self.path_of(store, id)? else { return Ok(None) };
        if !dir.is_empty() && store.local(&dir)?.is_none_or(|r| r.file.info.kind != FileKind::Dir) {
            return Ok(None);
        }
        let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
        let mut items = Vec::new();
        let mut after = cursor.map(str::to_string);
        let limit = limit.max(1);
        'pages: loop {
            let page = store.local_paths(&dir, after.as_deref(), 1000)?;
            if page.is_empty() {
                return Ok(Some(Page { items, next: None }));
            }
            for p in page {
                let Some(rest) = p.strip_prefix(&prefix).filter(|r| !r.is_empty()) else {
                    after = Some(p);
                    continue;
                };
                if let Some(slash) = rest.find('/') {
                    // A grandchild: everything below that child is skipped at once.
                    after = Some(format!("{}/\u{10FFFF}", &p[..prefix.len() + slash]));
                    continue 'pages;
                }
                if let Some(rec) = store.local(&p)?.filter(live) {
                    if items.len() == limit {
                        return Ok(Some(Page { next: after, items }));
                    }
                    items.push(item(store, &self.ids, pins, &rec)?);
                }
                after = Some(p);
            }
        }
    }

    /// What changed since `anchor` (`0` = everything there is; see [`anchor_of`]): the items as they are now and the
    /// identifiers of items deleted, at most `limit` records' worth. An anchor of another index (it was reset since) is
    /// an [`io::ErrorKind::NotFound`] error: list everything again.
    pub fn changes(&mut self, store: &dyn IndexStore, pins: &BTreeSet<String>, anchor: u64, limit: usize) -> io::Result<ChangePage> {
        let meta = store.meta()?;
        let floor = store.feed_floor()?;
        let Some(since) = sequence_of(anchor, meta.index_id, meta.sequence).filter(|s| *s == 0 || *s >= floor) else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "the anchor has expired (the folder's index was made anew): list the folder again"));
        };
        self.ids.catch_up(store)?;
        let limit = limit.max(1);
        // The records' sequences and the feed log's (what the host shows changed without a new version), one
        // counter: merged in order, each path at its latest.
        let recs = store.local_since(since, limit)?;
        let logged = store.shown_since(since, limit)?;
        let full = recs.len() == limit || logged.len() == limit;
        let mut events: Vec<(u64, String)> = recs.into_iter().map(|r| (r.sequence, r.file.info.path)).chain(logged).collect();
        events.sort();
        let more = full || events.len() > limit;
        events.truncate(limit);
        let upto = events.last().map_or(since, |e| e.0);
        let mut page = ChangePage { items: Vec::new(), deleted: Vec::new(), anchor: anchor_of(meta.index_id, since), more };
        let mut shown = HashSet::new();
        for (seq, path) in events {
            page.anchor = anchor_of(meta.index_id, seq);
            // Changed again since: the later sequence reports it (as it is now).
            let Some(now) = store.local(&path)? else { continue };
            if latest(store, &now)? != seq {
                continue;
            }
            let fid = now.file.info.file_id;
            if live(&now) {
                self.report(store, pins, &now, &mut page, &mut shown)?;
                // The holder of a file id: a path that held it before has a path identifier now.
                if self.ids.by_id.get(&fid) == Some(&now.file.info.path) {
                    for p in self.ids.seconds(&fid) {
                        if let Some(o) = store.local(&p)?.filter(live) {
                            self.report(store, pins, &o, &mut page, &mut shown)?;
                        }
                    }
                }
            } else {
                // What is in the folder above changed (an item deleted, or moved out of it): that folder is reported
                // too, so a host that versions a folder by what is below it sees it move.
                self.report_above(store, pins, &now.file.info.path, &mut page, &mut shown)?;
                if let Some(id) = self.ids.deleted_id(&now.file.info.path, &now) {
                    page.deleted.push(id);
                } else if let Some(p) = self.ids.by_id.get(&fid).cloned()
                    && !shown.contains(&p)
                    && let Some(o) = store.local(&p)?.filter(live)
                {
                    // The file id lives on at another path that this page has not reported (a rename's new name is
                    // stored with the old one's delete, so it comes just before; this is a second path that holds the
                    // file id now): that item is reported where it is, and the path identifier it had is gone.
                    self.report(store, pins, &o, &mut page, &mut shown)?;
                    page.deleted.push(format!("{PATH_ID}{p}"));
                }
            }
        }
        // File ids retired from their paths (made anew elsewhere, taken in one go): their identifiers name nothing
        // there any more.
        for (_, path, fid) in store.retired(since, upto)? {
            if self.ids.path_of(&hex(&fid)).is_none() {
                page.deleted.push(hex(&fid));
            }
            let second = format!("{PATH_ID}{path}");
            if self.ids.id_now(&path).as_deref() != Some(second.as_str()) && !page.deleted.contains(&second) {
                page.deleted.push(second);
            }
        }
        Ok(page)
    }

    /// Adds the live folder directly above `path` (not the folder itself) to the page, once.
    fn report_above(&self, store: &dyn IndexStore, pins: &BTreeSet<String>, path: &str, page: &mut ChangePage, shown: &mut HashSet<String>) -> io::Result<()> {
        let Some(n) = path.rfind('/') else { return Ok(()) };
        let parent = &path[..n];
        if shown.contains(parent) {
            return Ok(());
        }
        match store.local(parent)?.filter(live) {
            Some(d) if d.file.info.kind == FileKind::Dir => self.report(store, pins, &d, page, shown),
            _ => Ok(()),
        }
    }

    /// Adds `rec`'s item to the page, after any folder above it that the host has not heard of yet: one that changed
    /// after `rec` (a later sequence: the feed would report it only later). So a host meets every parent first.
    fn report(&self, store: &dyn IndexStore, pins: &BTreeSet<String>, rec: &LocalRecord, page: &mut ChangePage, shown: &mut HashSet<String>) -> io::Result<()> {
        let path = &rec.file.info.path;
        let mut above = Vec::new();
        let mut at = path.as_str();
        while let Some(n) = at.rfind('/') {
            at = &at[..n];
            if shown.contains(at) {
                break;
            }
            if let Some(d) = store.local(at)?.filter(live)
                && latest(store, &d)? > latest(store, rec)?
            {
                above.push(d);
            }
        }
        for d in above.into_iter().rev() {
            shown.insert(d.file.info.path.clone());
            page.items.push(item(store, &self.ids, pins, &d)?);
        }
        if shown.insert(path.clone()) {
            page.items.push(item(store, &self.ids, pins, rec)?);
        }
        Ok(())
    }
}

/// The sequence of `rec`'s latest event in the host's feed: its version, or a later change of what the host shows.
fn latest(store: &dyn IndexStore, rec: &LocalRecord) -> io::Result<u64> {
    Ok(store.shown_at(&rec.file.info.path)?.map_or(rec.sequence, |s| s.max(rec.sequence)))
}

/// The folder itself.
fn root(pins: &BTreeSet<String>) -> Item {
    Item {
        id: ROOT_ID.into(),
        parent_id: ROOT_ID.into(),
        path: String::new(),
        name: String::new(),
        kind: FileKind::Dir,
        size: 0,
        mtime_ns: 0,
        exec: false,
        symlink_target: String::new(),
        content_version: String::new(),
        metadata_version: String::new(),
        state: ItemState::Downloaded,
        pinned: pins.contains(""),
        conflict_of: None,
    }
}
