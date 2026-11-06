//! The Mac's one Finder entry for every synced folder (SPEC §7G.11, *The Mac's host*; the user's decision,
//! 2026-11-05): a single File Provider domain, the hub, shown as *Seedwell* in the Finder's sidebar. Its root lists
//! each synced folder, in name order: an on-demand folder as a folder of placeholders (served as a domain of its own
//! was), a full folder as a link to its place, wherever that is (its files never move). Rust decides what the hub
//! holds; the Mac's host registers the one domain and draws it. Windows keeps its sync root per folder
//! (`sync_host_attach`): none of this reaches it.
//!
//! **Identifiers.** `root` is the hub's root. An item of on-demand folder `F` is `F:<the core's id>` (its folder is
//! `F:root`); a full folder's link is `F` alone. Folder ids never hold `:` (base32 and `-`), so a folder's ids can
//! never be another's, whatever the core's ids are.
//!
//! **The anchor.** A domain has one sync anchor of at most 500 bytes, so the system holds a short token
//! (`h2.<epoch>.<n>`) and the core keeps what it stands for ([`AnchorStore`]): the top level reached, every on-demand
//! folder's own anchor, the full folders shown as links. The changes since it are, for each on-demand folder, its own
//! feed from its own anchor (a folder new to the hub from the start), the top level again when it changed (a folder
//! added, renamed or switched), and the top item of a folder that left (its whole subtree goes with it; alone in its
//! page, so no top item collides with a name still there). A folder's anchor moves only by what was handed out, a page
//! cut by the limit says `more`, a folder that cannot be read keeps its anchor, and one held for a save by replace is
//! not read: no change of any folder is lost, and no temporary delete is handed out. One folder's anchor of an index
//! made anew expires the hub's (the system lists everything again), as does a token the core no longer has.

use std::collections::BTreeSet;

use super::*;

/// The hub's domain identifier: never a folder's (folder ids are upper-case base32).
pub const HUB_ID: &str = "seedwell";
const ROOT: &str = trs_sync::ondemand::ROOT_ID;
/// Between a folder's id and the core's id of an item in it.
const SEP: char = ':';

/// How the host shows on-demand folders (`Core::sync_host_start`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum SyncHostMode {
    /// A domain per on-demand folder (`SyncHostListener::domains` lists them).
    Folders,
    /// One domain for every synced folder (the Mac; `domains` lists the hub alone, while any folder is synced).
    Hub,
}

/// The hub's domain: *Seedwell*.
pub(crate) fn hub_domain() -> SyncHostDomain {
    SyncHostDomain { id: HUB_ID.into(), name: crate::menu::APP_NAME.into() }
}

/// A synced folder as the hub needs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HubFolder {
    pub id: String,
    pub name: String,
    /// Its place, absolute (a full folder's link points there).
    pub path: String,
    pub on_demand: bool,
    /// This device may change it (not a read-only member).
    pub writable: bool,
}

/// One item at the hub's top level.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Top {
    pub folder: String,
    /// The name shown: the folder's, made distinct (` 2`, ` 3`, … for a name another folder has already).
    pub name: String,
    pub path: String,
    pub on_demand: bool,
    pub writable: bool,
}

/// The hub's id of folder `folder`'s top item.
pub(crate) fn top_id(folder: &str, on_demand: bool) -> String {
    if on_demand { hub_id(folder, ROOT) } else { folder.to_string() }
}

impl Top {
    pub(crate) fn id(&self) -> String {
        top_id(&self.folder, self.on_demand)
    }
}

/// The top level: every folder once, in name order (without case, then by name and id, so the order never depends on
/// the configuration's), each with a name no other has (compared without case, as the Mac's disks do).
pub(crate) fn tops(mut folders: Vec<HubFolder>) -> Vec<Top> {
    folders.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then_with(|| a.name.cmp(&b.name)).then_with(|| a.id.cmp(&b.id)));
    let originals: HashSet<String> = folders.iter().map(|f| f.name.to_lowercase()).collect();
    let mut taken: HashSet<String> = HashSet::new();
    folders
        .into_iter()
        .map(|f| {
            let mut name = f.name.clone();
            if !taken.insert(name.to_lowercase()) {
                // Never a name another folder has as its own (it gets that one).
                for n in 2.. {
                    let c = format!("{} {n}", f.name);
                    if !originals.contains(&c.to_lowercase()) && taken.insert(c.to_lowercase()) {
                        name = c;
                        break;
                    }
                }
            }
            Top { folder: f.id, name, path: f.path, on_demand: f.on_demand, writable: f.writable }
        })
        .collect()
}

/// A digest of the top level (FNV-1a; never 0, which a first anchor carries): it changes when a folder is added, goes,
/// is renamed, moved, switched or changes role.
pub(crate) fn tops_hash(tops: &[Top]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for t in tops {
        for part in [t.folder.as_str(), t.name.as_str(), t.path.as_str(), if t.on_demand { "d" } else { "l" }, if t.writable { "w" } else { "r" }] {
            for b in part.bytes().chain([0]) {
                h ^= u64::from(b);
                h = h.wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    h | 1
}

/// What a hub identifier names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HubItem<'a> {
    Root,
    /// An item of an on-demand folder: the folder, and the core's id.
    In(&'a str, &'a str),
    /// A full folder's link.
    Link(&'a str),
}

pub(crate) fn parse_id(id: &str) -> HubItem<'_> {
    if id.is_empty() || id == ROOT {
        return HubItem::Root;
    }
    match id.split_once(SEP) {
        Some((f, i)) => HubItem::In(f, i),
        None => HubItem::Link(id),
    }
}

pub(crate) fn hub_id(folder: &str, core: &str) -> String {
    format!("{folder}{SEP}{core}")
}

/// What the hub's anchor stands for: the top level it reached, each on-demand folder's own anchor, and the full
/// folders shown as links. Kept on the core's side ([`AnchorStore`]); the system holds only a short token for it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HubAnchor {
    pub tops: u64,
    pub folders: BTreeMap<String, u64>,
    pub links: BTreeSet<String>,
}

/// The longest anchor the system takes (NSFileProviderEnumerating.h: "Sync anchor data is limited to 500 bytes",
/// and a page, which carries one in the working set's listing, likewise).
pub(crate) const ANCHOR_MAX: usize = 500;
/// How many anchors are kept: the system asks from its newest (or a page of one listing); an older one is expired
/// (the system lists everything again, nothing is lost).
const ANCHORS_KEPT: usize = 64;
/// The file they are kept in, in the core's state folder.
const ANCHORS_FILE: &str = "file-provider-hub-anchors.json";

/// The anchors handed to the system, by number: `h2.<epoch>.<seq>`, fixed in size whatever the number of folders.
/// The epoch is new whenever the store is (its file lost), so a token from another store never reads as one of this.
/// Written whole and put in place on every new anchor, so an app started again still knows the system's.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct AnchorStore {
    epoch: u64,
    next: u64,
    entries: VecDeque<(u64, HubAnchor)>,
    #[serde(skip)]
    path: Option<PathBuf>,
}

impl AnchorStore {
    /// The store kept at `state` (in memory only without one), or a new one. A store read back numbers on from far
    /// ahead of what it saved (2^32 further): a token handed out after its last save that did not reach the disk (a
    /// full disk, the app stopped) is never handed out again for something else.
    pub(crate) fn open(state: Option<&Path>) -> AnchorStore {
        let path = state.map(|s| s.join(ANCHORS_FILE));
        let mut store = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<AnchorStore>(&b).ok())
            .filter(|s| s.epoch != 0)
            .map(|mut s| {
                s.next = s.next.saturating_add(1 << 32);
                s
            })
            .unwrap_or_else(|| {
                let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
                AnchorStore { epoch: (t ^ (u64::from(std::process::id()) << 32)) | 1, next: 1, entries: VecDeque::new(), path: None }
            });
        store.path = path;
        store
    }

    /// The token for `a`: the newest one again when nothing moved, else a new number (kept, and saved).
    pub(crate) fn mint(&mut self, a: HubAnchor) -> String {
        if let Some((n, last)) = self.entries.back()
            && *last == a
        {
            return self.token(*n);
        }
        let n = self.next;
        self.next += 1;
        self.entries.push_back((n, a));
        while self.entries.len() > ANCHORS_KEPT {
            self.entries.pop_front();
        }
        if let Some(p) = &self.path
            && let Ok(b) = serde_json::to_vec(self)
        {
            let tmp = p.with_extension("tmp");
            if std::fs::write(&tmp, b).is_ok() {
                let _ = std::fs::rename(&tmp, p);
            }
        }
        self.token(n)
    }

    fn token(&self, n: u64) -> String {
        let t = format!("h2.{:x}.{n}", self.epoch);
        debug_assert!(t.len() <= ANCHOR_MAX);
        t
    }

    /// What a token stands for; none (or `0`, the system's start) is the start. One this store does not hold
    /// (another epoch, too old, not a token) is expired: the system lists everything again.
    pub(crate) fn resolve(&self, t: Option<&str>) -> Result<HubAnchor, HostError> {
        let expired = || HostError::new(ErrKind::AnchorExpired, "That sync anchor is not one Seedwell still knows.");
        let t = match t {
            None | Some("") | Some("0") => return Ok(HubAnchor::default()),
            Some(t) => t,
        };
        let rest = t.strip_prefix("h2.").ok_or_else(expired)?;
        let (epoch, n) = rest.split_once('.').ok_or_else(expired)?;
        if u64::from_str_radix(epoch, 16).ok() != Some(self.epoch) {
            return Err(expired());
        }
        let n: u64 = n.parse().map_err(|_| expired())?;
        self.entries.iter().find(|(k, _)| *k == n).map(|(_, a)| a.clone()).ok_or_else(expired)
    }
}

/// How much later than the hold the watch let through another may end and still be that one (the time between the
/// watch's look and the hub's read).
pub(crate) const PASS_SLACK: Duration = Duration::from_millis(100);

/// The words of the hub's own refusals.
const NOT_RUNNING: &str = "Folder sync isn't running yet.";
const TOP_LEVEL: &str = "Seedwell lists your synced folders here. Rename, move or remove a folder in Seedwell.";
const NEW_AT_TOP: &str = "New items go inside one of your folders in Seedwell.";
const ACROSS: &str = "Items can't be moved from one synced folder to another here. Copy them instead.";
const LINK_HERE: &str = "This folder's files are on this Mac; open the folder itself.";
const LETTING_GO: &str = "Seedwell is letting go of this folder.";

/// The identifiers in an answer of folder `f` made the hub's: items (`id`, `parent`), the deleted ids of a change
/// page. The folder's own item is its top item: the hub's root is its parent, its name the top level's, and it is
/// neither renamed, moved nor deleted from the Finder.
fn to_hub(v: &mut Value, f: &str, tops: &[Top]) {
    fn item(x: &mut Value, f: &str, tops: &[Top]) {
        let Some(o) = x.as_object_mut() else { return };
        let id = o.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let parent = o.get("parent").and_then(Value::as_str).unwrap_or_default().to_string();
        if id == ROOT {
            if let Some(t) = tops.iter().find(|t| t.folder == f) {
                o.insert("name".into(), json!(t.name));
                // Its versions are the top level's, the same in every answer (whether the core could be read then
                // changes nothing); a rename or a new role at the top level is a new version.
                o.insert("content_version".into(), json!("t"));
                o.insert("metadata_version".into(), json!(format!("t|{}|{}", if t.writable { "w" } else { "r" }, t.name)));
            }
            o.insert("parent".into(), json!(ROOT));
            if let Some(Value::Array(caps)) = o.get_mut("caps") {
                caps.retain(|c| matches!(c.as_str(), Some("read" | "write")));
            }
        } else {
            o.insert("parent".into(), json!(hub_id(f, &parent)));
        }
        o.insert("id".into(), json!(hub_id(f, &id)));
    }
    if v.get("id").is_some() && v.get("caps").is_some() {
        item(v, f, tops);
        return;
    }
    let Some(o) = v.as_object_mut() else { return };
    for k in ["item", "conflict_copy"] {
        if let Some(x) = o.get_mut(k)
            && x.is_object()
        {
            item(x, f, tops);
        }
    }
    if let Some(Value::Array(items)) = o.get_mut("items") {
        for x in items {
            item(x, f, tops);
        }
    }
    if let Some(Value::Array(gone)) = o.get_mut("deleted") {
        for x in gone {
            if let Some(s) = x.as_str() {
                *x = json!(hub_id(f, s));
            }
        }
    }
}

/// Whether the system marks this as a placeholder whose content is not on disk (`SF_DATALESS`): reading it would ask
/// the extension, and nothing of it is the user's here.
pub(crate) fn dataless(m: &std::fs::Metadata) -> bool {
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::MetadataExt;
        m.st_flags() & 0x4000_0000 != 0
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = m;
        false
    }
}

/// Whether `e`, met on `from`, is only `from` having gone meanwhile (a Word lock file `~$…`, a safe-save temporary
/// `.sb-…` deleted between the listing and the copy): nothing there to keep. Any other error is one.
fn vanished(e: &std::io::Error, from: &Path) -> bool {
    e.kind() == std::io::ErrorKind::NotFound && std::fs::symlink_metadata(from).is_err_and(|x| x.kind() == std::io::ErrorKind::NotFound)
}

/// Copies `from` to the new `to` (a folder with what is below it, a file, a link), never what is dataless; the source
/// is left as it is (it is the system's). How many files and links were copied (what was copied already as it is now
/// is not copied again). Something that vanished meanwhile is nothing to keep; every other error is returned.
fn copy_out(from: &Path, to: &Path) -> std::io::Result<u64> {
    match copy_one(from, to) {
        Err(e) if vanished(&e, from) => Ok(0),
        r => r,
    }
}

fn copy_one(from: &Path, to: &Path) -> std::io::Result<u64> {
    let m = std::fs::symlink_metadata(from)?;
    if dataless(&m) {
        return Ok(0);
    }
    if let Some(p) = to.parent() {
        std::fs::create_dir_all(p)?;
    }
    if m.file_type().is_symlink() {
        let target = std::fs::read_link(from)?;
        if std::fs::read_link(to).is_ok_and(|t| t == target) {
            return Ok(0);
        }
        let _ = std::fs::remove_file(to);
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, to)?;
        return Ok(1);
    }
    if m.is_dir() {
        std::fs::create_dir_all(to)?;
        let mut n = 0;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            n += copy_out(&e.path(), &to.join(e.file_name()))?;
        }
        return Ok(n);
    }
    if m.is_file() {
        // Copied by an earlier look as it is now: not again.
        if let Ok(t) = std::fs::symlink_metadata(to)
            && t.is_file()
            && t.len() == m.len()
            && mtime_ns(&t) == mtime_ns(&m)
        {
            return Ok(0);
        }
        // A clone on APFS (the time kept).
        std::fs::copy(from, to)?;
        if let Ok(t) = m.modified() {
            std::fs::File::options().write(true).open(to)?.set_modified(t)?;
        }
        return Ok(1);
    }
    Ok(0)
}

/// The system's own files in a folder it shows: never anyone's change.
fn systems_own(name: &str) -> bool {
    name == ".DS_Store"
}

impl Host {
    /// The hub's top level now, without the folders being let go of; `None` while folder sync is not running.
    pub(crate) fn hub_tops(&self) -> Option<Vec<Top>> {
        let mut all = self.core.hub_folders()?;
        {
            let releasing = lock(&self.releasing);
            all.retain(|f| !releasing.contains(&f.id));
        }
        Some(tops(all))
    }

    fn hub_root_wire() -> WireItem {
        WireItem {
            id: ROOT.into(),
            parent: ROOT.into(),
            name: String::new(),
            kind: "dir",
            size: 0,
            mtime_ns: 0,
            content_version: "hub".into(),
            metadata_version: "hub".into(),
            state: "downloaded",
            keep: false,
            pinned: false,
            exec: false,
            symlink_target: None,
            // Nothing is made at the top level, and the hub itself is the system's.
            caps: vec!["read"],
        }
    }

    /// A top item: an on-demand folder as the core has its root (or, when it cannot say, a plain folder), named and
    /// placed as the hub shows it; a full folder as a link to its absolute place, read only.
    pub(crate) fn top_wire(&self, t: &Top, tops: &[Top]) -> Value {
        if !t.on_demand {
            return json!(WireItem {
                id: t.id(),
                parent: ROOT.into(),
                name: t.name.clone(),
                kind: "symlink",
                size: 0,
                mtime_ns: 0,
                content_version: format!("l{}", t.path),
                metadata_version: format!("l{}|{}", t.path, t.name),
                state: "downloaded",
                keep: false,
                pinned: false,
                exec: false,
                symlink_target: Some(t.path.clone()),
                caps: vec!["read"],
            });
        }
        let w = match self.core.item(&t.folder, ROOT) {
            Ok(Some(i)) => self.wire(&t.folder, &i).ok(),
            _ => None,
        }
        .unwrap_or_else(|| {
            let mut root = Self::hub_root_wire();
            root.caps = if t.writable { vec!["read", "write"] } else { vec!["read"] };
            root
        });
        let mut w = json!(w);
        to_hub(&mut w, &t.folder, tops);
        w
    }

    /// The hub's anchor now: the top level, and each on-demand folder's own (one that cannot be read: from the
    /// start, so nothing of it is skipped).
    pub(crate) fn hub_anchor_now(&self, tops: &[Top]) -> HubAnchor {
        HubAnchor {
            tops: tops_hash(tops),
            folders: tops.iter().filter(|t| t.on_demand).map(|t| (t.folder.clone(), self.core.anchor(&t.folder).unwrap_or(0))).collect(),
            links: tops.iter().filter(|t| !t.on_demand).map(|t| t.folder.clone()).collect(),
        }
    }

    /// `f` with the anchor store, opened at first use.
    pub(crate) fn with_anchors<T>(&self, f: impl FnOnce(&mut AnchorStore) -> T) -> T {
        let mut g = lock(&self.hub_anchors);
        f(g.get_or_insert_with(|| AnchorStore::open(self.hub_state.as_deref())))
    }

    /// Answers a request for the hub.
    pub(crate) fn hub_answer(&self, r: &Request, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<Value, HostError> {
        if r.op == "hello" {
            return Ok(json!({ "app": format!("Seedwell {}", env!("CARGO_PKG_VERSION")) }));
        }
        let tops = self.hub_tops().ok_or_else(|| HostError::new(ErrKind::Unreachable, NOT_RUNNING))?;
        match r.op.as_str() {
            "anchor" => {
                let now = self.hub_anchor_now(&tops);
                return Ok(json!({ "anchor": self.with_anchors(|a| a.mint(now)) }));
            }
            "changes" => return self.hub_changes(&tops, r),
            "materialized" => return Ok(json!({ "released": self.hub_report(&tops, r) })),
            // Made inside a folder only (a create names its parent, not an item).
            "create" => {
                return match parse_id(r.parent.as_deref().unwrap_or(ROOT)) {
                    HubItem::In(f, p) => self.forward(&tops, f, r, None, Some(p), progress),
                    _ => Err(HostError::new(ErrKind::Failed, NEW_AT_TOP)),
                };
            }
            _ => {}
        }
        let id = r.item.as_deref().ok_or_else(|| HostError::new(ErrKind::BadRequest, "The request names no item."))?;
        let gone = || HostError::new(ErrKind::NoSuchItem, format!("{NO_ITEM}{id} in Seedwell."));
        let top = |f: &str, on_demand: bool| tops.iter().find(|t| t.folder == f && t.on_demand == on_demand).ok_or_else(gone);
        let target = parse_id(id);
        match (r.op.as_str(), target) {
            ("item", HubItem::Root) => Ok(json!(Self::hub_root_wire())),
            ("item", HubItem::Link(f)) => Ok(self.top_wire(top(f, false)?, &tops)),
            ("item", HubItem::In(f, i)) if i == ROOT => Ok(self.top_wire(top(f, true)?, &tops)),
            ("children", HubItem::Root) => {
                let start = r.cursor.as_deref().and_then(|c| c.parse::<usize>().ok()).unwrap_or(0).min(tops.len());
                let end = (start + r.limit.unwrap_or(PAGE_MAX).clamp(1, PAGE_MAX) as usize).min(tops.len());
                let items: Vec<Value> = tops[start..end].iter().map(|t| self.top_wire(t, &tops)).collect();
                Ok(json!({ "items": items, "next": (end < tops.len()).then(|| end.to_string()) }))
            }
            ("children", HubItem::Link(f)) => {
                top(f, false)?;
                Ok(json!({ "items": [], "next": Value::Null }))
            }
            ("modify", HubItem::Root | HubItem::Link(_)) => self.top_unchanged(r, target, &tops),
            ("modify", HubItem::In(_, ROOT)) => self.top_unchanged(r, target, &tops),
            ("modify", HubItem::In(f, i)) => {
                let parent = match r.parent.as_deref().map(parse_id) {
                    None => None,
                    Some(HubItem::In(g, p)) if g == f => Some(p),
                    Some(_) => return Err(HostError::new(ErrKind::Failed, ACROSS)),
                };
                self.forward(&tops, f, r, Some(i), parent, progress)
            }
            ("delete", HubItem::Root | HubItem::Link(_)) | ("delete", HubItem::In(_, ROOT)) => Err(HostError::new(ErrKind::DeletionRejected, TOP_LEVEL)),
            ("cancel", HubItem::Root | HubItem::Link(_)) => Ok(json!({})),
            ("pin", HubItem::Link(f)) => Ok(json!({ "item": self.top_wire(top(f, false)?, &tops) })),
            ("fetch", HubItem::Link(_)) => Err(HostError::new(ErrKind::Failed, LINK_HERE)),
            (_, HubItem::In(f, i)) => self.forward(&tops, f, r, Some(i), None, progress),
            ("fetch" | "pin", _) => Err(HostError::new(ErrKind::Failed, TOP_LEVEL)),
            (other, _) => Err(HostError::new(ErrKind::BadRequest, format!("Unknown request “{other}”."))),
        }
    }

    /// A change of a top item (or of the hub's root): only one that changes nothing Seedwell keeps (dates, tags) is
    /// taken, and the item stays as it is. A rename, a move or new content is refused: the folders are renamed and
    /// moved in Seedwell.
    fn top_unchanged(&self, r: &Request, target: HubItem<'_>, tops: &[Top]) -> Result<Value, HostError> {
        if r.parent.is_some() || r.name.is_some() || r.contents.is_some() {
            return Err(HostError::new(ErrKind::Failed, TOP_LEVEL));
        }
        let item = match target {
            HubItem::Root => json!(Self::hub_root_wire()),
            HubItem::Link(f) | HubItem::In(f, _) => {
                let t = tops.iter().find(|t| t.folder == f).ok_or_else(|| HostError::new(ErrKind::NoSuchItem, format!("{NO_ITEM}{f} in Seedwell.")))?;
                self.top_wire(t, tops)
            }
        };
        Ok(json!({ "item": item, "fetch_content": false, "conflict_copy": Value::Null }))
    }

    /// Request `r` asked of on-demand folder `f` (the core's ids `item` and `parent`), answered as a folder's own
    /// domain would be, its identifiers made the hub's.
    fn forward(&self, tops: &[Top], f: &str, r: &Request, item: Option<&str>, parent: Option<&str>, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<Value, HostError> {
        // Being let go of (listed still, or gone from the hub already): refused, and content the system sends is kept
        // first, so nothing it hands over after the last look is lost when it deletes its copy.
        if matches!(r.op.as_str(), "create" | "modify" | "delete") && lock(&self.frozen).contains(f) {
            self.keep_refused(f, r, item, parent);
            return Err(HostError::new(ErrKind::Unreachable, LETTING_GO));
        }
        if !tops.iter().any(|t| t.folder == f && t.on_demand) {
            return Err(HostError::new(ErrKind::NoSuchItem, format!("{NO_ITEM}{} in Seedwell.", r.item.as_deref().unwrap_or(f))));
        }
        let mut fr = r.clone();
        fr.folder = f.to_string();
        fr.item = item.map(str::to_string);
        fr.parent = parent.map(str::to_string);
        let mut v = self.answer(&fr, progress)?;
        to_hub(&mut v, f, tops);
        Ok(v)
    }

    /// What changed in the hub since the anchor in `r` (see the module's doc).
    fn hub_changes(&self, tops: &[Top], r: &Request) -> Result<Value, HostError> {
        let from = self.with_anchors(|a| a.resolve(r.anchor.as_deref()))?;
        let limit = r.limit.unwrap_or(PAGE_MAX).clamp(1, PAGE_MAX) as usize;
        let hash = tops_hash(tops);
        // A folder that left the top level (or changed kind): its top item goes, and the system deletes what is below.
        let mut gone: Vec<String> = Vec::new();
        for f in from.folders.keys() {
            if !tops.iter().any(|t| &t.folder == f && t.on_demand) {
                gone.push(top_id(f, true));
            }
        }
        for f in &from.links {
            if !tops.iter().any(|t| &t.folder == f && !t.on_demand) {
                gone.push(top_id(f, false));
            }
        }
        if !gone.is_empty() {
            // Alone in their page, before any top item comes (again): the extension reports a page's updates before
            // its deletes, and a top item named as one that goes (a folder switched, "X 2" now "X") would collide
            // with it, its rename refused and a wrong name kept. The next page has the top level as it is now.
            let mut next = from.clone();
            next.folders.retain(|f, _| tops.iter().any(|t| &t.folder == f && t.on_demand));
            next.links.retain(|f| tops.iter().any(|t| &t.folder == f && !t.on_demand));
            let token = self.with_anchors(|a| a.mint(next));
            return Ok(json!({ "items": [], "deleted": gone, "anchor": token, "more": true }));
        }
        let mut items: Vec<Value> = Vec::new();
        let mut deleted: Vec<String> = Vec::new();
        let mut next = HubAnchor { tops: hash, folders: BTreeMap::new(), links: tops.iter().filter(|t| !t.on_demand).map(|t| t.folder.clone()).collect() };
        // The top level again when it changed: before anything below, so the system meets each parent first.
        if from.tops != hash {
            items.extend(tops.iter().map(|t| self.top_wire(t, tops)));
        }
        let mut more = false;
        for t in tops.iter().filter(|t| t.on_demand) {
            // A folder new to the hub (or one that was a link): from the start.
            let start = from.folders.get(&t.folder).copied().unwrap_or(0);
            // A delete a moment ago in it may be a save by replace: its feed is not read until that is settled (or
            // the watch let it through after a window), whatever moved in another folder.
            if let Some(h) = self.core.replace_hold(&t.folder) {
                // Let through only for the hold the watch let through (it ends when that one would): a new delete
                // since starts a hold of its own, ending later.
                let passed = lock(&self.pass_hold).get(&t.folder).is_some_and(|end| Instant::now() + h.left <= *end + PASS_SLACK);
                if !passed {
                    next.folders.insert(t.folder.clone(), start);
                    continue;
                }
            }
            let left = limit.saturating_sub(items.len() + deleted.len());
            if more || left == 0 {
                more = true;
                next.folders.insert(t.folder.clone(), start);
                continue;
            }
            let fr = Request {
                v: PROTOCOL,
                op: "changes".into(),
                folder: t.folder.clone(),
                anchor: Some(anchor_text(start)),
                limit: Some(left as u32),
                ..Request::default()
            };
            match self.answer(&fr, &mut |_, _| true) {
                Ok(mut v) => {
                    to_hub(&mut v, &t.folder, tops);
                    let reached = anchor_of(v["anchor"].as_str())?;
                    if let Value::Array(a) = v["items"].take() {
                        items.extend(a);
                    }
                    if let Value::Array(d) = v["deleted"].take() {
                        deleted.extend(d.into_iter().filter_map(|x| x.as_str().map(str::to_string)));
                    }
                    more |= v["more"].as_bool().unwrap_or(false);
                    next.folders.insert(t.folder.clone(), reached);
                    lock(&self.pass_hold).remove(&t.folder);
                }
                // An index made anew: the system lists the hub again.
                Err(e) if e.kind == ErrKind::AnchorExpired => return Err(e),
                // Not readable now (paused, not open): it keeps its anchor, and is read from there next time.
                Err(_) => {
                    next.folders.insert(t.folder.clone(), start);
                }
            }
        }
        let token = self.with_anchors(|a| a.mint(next));
        Ok(json!({ "items": items, "deleted": deleted, "anchor": token, "more": more }))
    }

    /// A report of the system's set of items on disk in the hub: once whole, each on-demand folder's part is its own
    /// report (a folder with none in it: everything of it left the disk).
    fn hub_report(&self, tops: &[Top], r: &Request) -> u64 {
        let _one = lock(&self.report_lock);
        let Some(whole) = self.report_whole(HUB_ID, r) else { return 0 };
        let mut per: BTreeMap<String, Vec<String>> = tops.iter().filter(|t| t.on_demand).map(|t| (t.folder.clone(), Vec::new())).collect();
        for id in &whole {
            if let HubItem::In(f, i) = parse_id(id)
                && let Some(ids) = per.get_mut(f)
            {
                ids.push(i.to_string());
            }
        }
        per.into_iter().map(|(f, ids)| self.materialized(&f, ids)).sum()
    }

    /// `queue_materialize` for every on-demand folder in the hub, from the hub's set on disk. A folder that cannot be
    /// read now is skipped (the host asks again after a switch).
    pub(crate) fn hub_queue_materialize(&self, on_disk: Vec<String>) -> u64 {
        let Some(tops) = self.hub_tops() else { return 0 };
        let mut per: BTreeMap<String, Vec<String>> = tops.iter().filter(|t| t.on_demand).map(|t| (t.folder.clone(), Vec::new())).collect();
        for id in &on_disk {
            if let HubItem::In(f, i) = parse_id(id)
                && let Some(ids) = per.get_mut(f)
            {
                ids.push(i.to_string());
            }
        }
        per.into_iter().filter_map(|(f, ids)| self.queue_materialize(&f, ids).ok()).sum()
    }

    /// The hub's next batch: the next folder's, its identifiers the hub's.
    pub(crate) fn hub_next_batch(&self) -> Option<SyncHostBatch> {
        let mut folders: Vec<String> = lock(&self.to_materialize).keys().cloned().collect();
        folders.sort();
        for f in folders {
            if let Some(mut b) = self.next_batch(&f) {
                b.ids = b.ids.iter().map(|i| hub_id(&f, i)).collect();
                return Some(b);
            }
        }
        None
    }

    /// Lets go of on-demand folder `f` (see `Core::sync_host_let_go`): what the system sends for it is refused from
    /// now on (its content kept, [`Self::keep_refused`]); what it still has on disk under `visible` that the folder
    /// does not hold as it is there is copied next to the folder's place, look after look while the folder is still
    /// listed, until a look finds nothing new (a save made during a look is kept by the next); only then does the folder
    /// leave the hub. What to tell the user, if anything was kept. When anything found could not be copied, or the
    /// folder kept changing for [`LOOKS`] looks, the folder stays as it was (listed, changes taken again) and the error
    /// says why: the system would delete what was not kept.
    pub(crate) fn let_go(&self, f: &str, name: &str, place: Option<&Path>, visible: Option<&Path>) -> Result<Option<SyncHostNotice>, String> {
        lock(&self.pass_hold).remove(f);
        lock(&self.letting_go).insert(f.to_string(), KeptPlace { name: name.to_string(), place: place.map(Path::to_path_buf), dest: None });
        lock(&self.frozen).insert(f.to_string());
        if let Some(v) = visible {
            // Done when a look copies nothing and saw the place as the look before it did (a save made during a look,
            // in a folder that look had read already, shows in the next one).
            let mut quiet = false;
            let mut before: Option<u64> = None;
            for _ in 0..LOOKS {
                match self.rescue(f, v) {
                    Ok((0, seen)) if before == Some(seen) => {
                        quiet = true;
                        break;
                    }
                    Ok((_, seen)) => before = Some(seen),
                    Err(e) => {
                        self.let_go_undo(f);
                        return Err(e);
                    }
                }
            }
            if !quiet {
                self.let_go_undo(f);
                return Err(format!("“{name}” kept changing while Seedwell let go of it, so the folder stays as it is. Try again in a moment."));
            }
        }
        lock(&self.releasing).insert(f.to_string());
        Ok(lock(&self.letting_go).get(f).and_then(|k| k.dest.clone()).map(|d| kept_notice(name, &d)))
    }

    /// The let-go of `f` is undone, or over: it is back in the hub (unless the core forgot it), and its changes are
    /// taken again.
    pub(crate) fn let_go_undo(&self, f: &str) {
        lock(&self.frozen).remove(f);
        lock(&self.releasing).remove(f);
        lock(&self.pass_hold).remove(f);
        lock(&self.letting_go).remove(f);
        self.force_report.store(true, Ordering::SeqCst);
    }

    /// Content the system sends for a folder being let go of (after its last look, before it deletes its copy): kept
    /// next to the folder like what the looks found, at the item's path (numbered when something else is there),
    /// before the change is refused. `parent` and `item` are the core's ids.
    fn keep_refused(&self, f: &str, r: &Request, item: Option<&str>, parent: Option<&str>) {
        let Some(contents) = r.contents.as_deref() else { return };
        let path = || -> Option<String> {
            let base = match (r.op.as_str(), item) {
                ("modify", Some(id)) => self.core.item(f, id).ok().flatten()?.path,
                _ => String::new(),
            };
            let (dir, own) = match base.rsplit_once('/') {
                Some((d, n)) => (d.to_string(), n.to_string()),
                None => (String::new(), base.clone()),
            };
            let dir = match parent {
                Some(p) if p != ROOT => self.core.item(f, p).ok().flatten()?.path,
                Some(_) => String::new(),
                None => dir,
            };
            let name = r.name.clone().unwrap_or(own);
            (!name.is_empty()).then(|| if dir.is_empty() { name } else { format!("{dir}/{name}") })
        };
        let Some(rel) = path() else { return };
        let dest = {
            let mut all = lock(&self.letting_go);
            let Some(k) = all.get_mut(f) else { return };
            if k.dest.is_none() {
                k.dest = kept_dir(&k.name, k.place.as_deref());
            }
            k.dest.clone()
        };
        let Some(dest) = dest else { return };
        let kept = (|| -> std::io::Result<PathBuf> {
            let mut from = self.upload(contents).map_err(|e| std::io::Error::other(e.message))?;
            let mut to = dest.join(&rel);
            if let Some(d) = to.parent() {
                std::fs::create_dir_all(d)?;
            }
            let mut n = 2;
            while to.symlink_metadata().is_ok() {
                let p = Path::new(&rel);
                let stem = p.file_stem().map(|x| x.to_string_lossy().into_owned()).unwrap_or_default();
                let ext = p.extension().map(|x| format!(".{}", x.to_string_lossy())).unwrap_or_default();
                to = dest.join(p.with_file_name(format!("{stem} {n}{ext}")));
                n += 1;
            }
            let mut out = std::fs::File::options().write(true).create_new(true).open(&to)?;
            std::io::copy(&mut from, &mut out)?;
            out.sync_all()?;
            Ok(to)
        })();
        match kept {
            Ok(to) => eprintln!("seedwell: file provider: a change sent while letting go of a folder is kept at {}", to.display()),
            Err(e) => eprintln!("seedwell: file provider: a change sent while letting go of a folder could not be kept: {e}"),
        }
    }

    /// The children of `id` in folder `f`, by name (`None`: the core cannot say, so nothing there counts as held).
    fn children_by_name(&self, f: &str, id: &str) -> Option<HashMap<String, SyncItem>> {
        let mut out = HashMap::new();
        let mut cursor: Option<String> = None;
        loop {
            let page = self.core.children(f, id, cursor.as_deref(), PAGE_MAX).ok()?;
            out.extend(page.items.into_iter().filter(|i| i.id != ROOT).map(|i| (i.name.clone(), i)));
            match page.next {
                Some(n) => cursor = Some(n),
                None => return Some(out),
            }
        }
    }

    /// One look: copies what the system has on disk under `visible` (folder `f` as the Finder shows it) and the folder
    /// does not hold as it is there (a file or link it does not have, or has otherwise: another size, time or target;
    /// a folder it does not have, with what is below) to `<name> (changes not synced)` next to the folder's place,
    /// keeping the paths; what an earlier look copied, as it is now, is not copied again. How many were copied, and a
    /// digest of everything the look saw (each entry's path, kind, size and time). Never
    /// reads a placeholder (dataless: nothing of it is here), so it never asks the extension for content. What vanished
    /// meanwhile (a Word lock file, a safe-save temporary) is nothing to keep; any other failure is an error, in the
    /// user's words.
    pub(crate) fn rescue(&self, f: &str, visible: &Path) -> Result<(u64, u64), String> {
        let (name, place) = match lock(&self.letting_go).get(f) {
            Some(k) => (k.name.clone(), k.place.clone()),
            None => (f.to_string(), None),
        };
        let failed = |what: &Path, e: &dyn std::fmt::Display| format!("Seedwell could not keep “{}” before letting go of “{name}” ({e}), so the folder stays as it is.", what.display());
        let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut seen: u64 = 0;
        let mut stack: Vec<(PathBuf, String, PathBuf)> = vec![(visible.to_path_buf(), ROOT.to_string(), PathBuf::new())];
        while let Some((dir, id, rel)) = stack.pop() {
            // Nothing there (the system has no copy of the folder, or it went meanwhile): nothing to keep.
            let Ok(m) = std::fs::symlink_metadata(&dir) else { continue };
            if dataless(&m) || !m.is_dir() {
                continue;
            }
            let known = self.children_by_name(f, &id).unwrap_or_default();
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(e) if vanished(&e, &dir) => continue,
                Err(e) => return Err(failed(&dir, &e)),
            };
            for e in entries {
                let e = match e {
                    Ok(e) => e,
                    Err(x) if vanished(&x, &dir) => break,
                    Err(x) => return Err(failed(&dir, &x)),
                };
                let n = e.file_name().to_string_lossy().into_owned();
                if systems_own(&n) {
                    continue;
                }
                let m = match std::fs::symlink_metadata(e.path()) {
                    Ok(m) => m,
                    Err(x) if vanished(&x, &e.path()) => continue,
                    Err(x) => return Err(failed(&e.path(), &x)),
                };
                if dataless(&m) {
                    continue;
                }
                // (Summed: the order the disk lists them in does not matter.)
                seen = seen.wrapping_add(entry_digest(&rel.join(&n), &m));
                let there = known.get(&n);
                if m.is_dir() && !m.file_type().is_symlink() {
                    match there {
                        Some(i) if i.kind == SyncItemKind::Dir => stack.push((e.path(), i.id.clone(), rel.join(&n))),
                        _ => found.push((e.path(), rel.join(&n))),
                    }
                    continue;
                }
                let same = there.is_some_and(|i| {
                    if m.file_type().is_symlink() {
                        i.kind == SyncItemKind::Symlink && std::fs::read_link(e.path()).ok().map(|t| t.display().to_string()) == i.symlink_target
                    } else {
                        i.kind == SyncItemKind::File && i.size == m.len() && (mtime_ns(&m) - i.mtime_ns).abs() < 1_000_000_000
                    }
                });
                if !same {
                    found.push((e.path(), rel.join(&n)));
                }
            }
        }
        if found.is_empty() {
            return Ok((0, seen));
        }
        #[cfg(test)]
        if let Some(hook) = lock(&self.before_copy).take() {
            hook();
        }
        let dest = {
            let mut all = lock(&self.letting_go);
            let k = all.entry(f.to_string()).or_insert_with(|| KeptPlace { name: name.clone(), place: place.clone(), dest: None });
            if k.dest.is_none() {
                k.dest = kept_dir(&k.name, k.place.as_deref());
            }
            k.dest.clone()
        }
        .ok_or_else(|| failed(visible, &"there is no place to keep it"))?;
        let mut copied = 0;
        for (from, rel) in &found {
            copied += copy_out(from, &dest.join(rel)).map_err(|e| failed(from, &e))?;
        }
        Ok((copied, seen))
    }
}

/// One entry as a look saw it: its path, kind, size and modification time.
fn entry_digest(rel: &Path, m: &std::fs::Metadata) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let kind = if m.file_type().is_symlink() { 2u8 } else if m.is_dir() { 1 } else { 0 };
    let size = if m.is_dir() { 0 } else { m.len() };
    let bytes = rel.to_string_lossy().into_owned().into_bytes().into_iter().chain([0, kind]).chain(size.to_le_bytes()).chain(mtime_ns(m).to_le_bytes());
    for b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

/// How many looks a let-go takes at most before it gives up on a folder that keeps changing (and keeps it).
pub(crate) const LOOKS: usize = 5;

/// Where a let-go keeps what it finds: next to folder `name`'s place, made at the first thing found.
pub(crate) struct KeptPlace {
    pub name: String,
    pub place: Option<PathBuf>,
    pub dest: Option<PathBuf>,
}

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

/// The migration's mark in the core's state folder: `done` once every domain of the per-folder era is removed.
const MIGRATED: &str = "file-provider-hub";

/// The domains still to remove before the hub is registered: every registered one but the hub, until the migration
/// is done; then none, ever.
pub(crate) fn migration_plan(state: &Path, registered: Vec<SyncHostDomain>) -> Vec<SyncHostDomain> {
    if std::fs::read_to_string(state.join(MIGRATED)).is_ok_and(|s| s.trim() == "done") {
        return Vec::new();
    }
    registered.into_iter().filter(|d| d.id != HUB_ID).collect()
}

/// Notes the migration done (written whole, then put in place: a crash leaves it not done, and the next start runs
/// what is left).
pub(crate) fn migration_done(state: &Path) {
    let _ = std::fs::create_dir_all(state);
    let tmp = state.join(format!("{MIGRATED}.tmp"));
    if std::fs::write(&tmp, "done\n").is_ok() {
        let _ = std::fs::rename(&tmp, state.join(MIGRATED));
    }
}
