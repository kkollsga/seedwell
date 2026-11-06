//! The Mac's hub (one File Provider domain for every synced folder) against a fake core of several folders, as the
//! system and the host use it, and once through the real core.

use std::os::unix::net::UnixStream;
use std::sync::OnceLock;

use super::hub::*;
use super::*;

/// A folder of the fake core. Every change is one entry of its log, so its feed from any anchor is exact.
struct Folder {
    name: String,
    path: String,
    on_demand: bool,
    writable: bool,
    items: BTreeMap<String, SyncItem>,
    /// (sequence, item id, deleted)
    log: Vec<(u64, String, bool)>,
    seq: u64,
    /// The folder cannot be read now (paused, not open).
    broken: bool,
    /// A delete a moment ago may be a save by replace (`replace_hold`).
    hold: bool,
    /// How long until the hold's last delete stops waiting (a new delete: longer).
    hold_left: Duration,
    /// Its index was made anew: every anchor but 0 is expired.
    expired: bool,
    /// The index id the anchors carry (high bits), so two folders' anchors differ.
    base: u64,
}

impl Folder {
    fn new(name: &str, path: &str, on_demand: bool, base: u64) -> Folder {
        let mut f = Folder {
            name: name.into(),
            path: path.into(),
            on_demand,
            writable: true,
            items: BTreeMap::new(),
            log: Vec::new(),
            seq: 0,
            broken: false,
            hold: false,
            hold_left: Duration::from_secs(1),
            expired: false,
            base,
        };
        let mut root = file("root", "root", "", SyncItemKind::Dir);
        root.path = String::new();
        f.items.insert("root".into(), root);
        f
    }

    fn put(&mut self, i: SyncItem) {
        self.seq += 1;
        self.log.push((self.seq, i.id.clone(), false));
        self.items.insert(i.id.clone(), i);
    }

    fn remove(&mut self, id: &str) {
        self.seq += 1;
        self.items.remove(id);
        self.log.push((self.seq, id.into(), true));
    }

    fn anchor(&self, seq: u64) -> u64 {
        trs_sync::ondemand::anchor_of(self.base, seq)
    }
}

fn file(id: &str, parent: &str, name: &str, kind: SyncItemKind) -> SyncItem {
    SyncItem {
        id: id.into(),
        parent_id: parent.into(),
        path: name.into(),
        name: name.into(),
        kind,
        size: 5,
        mtime_ns: 1_792_000_000_000_000_000,
        state: SyncItemState::Downloaded,
        pinned: false,
        content_version: format!("c-{id}"),
        metadata_version: format!("m-{id}-0"),
        exec: false,
        symlink_target: None,
        conflict_of: None,
        in_place: false,
    }
}

/// Something done at a moment of the test's choosing.
type Hook = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct Fakes {
    folders: Mutex<BTreeMap<String, Folder>>,
    /// Calls that changed something: (op, folder, item or name).
    calls: Mutex<Vec<(String, String, String)>>,
    /// Run at every listing of an item's children, with its id (something the user does meanwhile).
    on_children: Mutex<Option<Hook>>,
}

fn od_err(kind: OdKind, m: &str) -> OdError {
    OdError { kind, error: CoreError::Failed { message: m.into() } }
}

impl Fakes {
    fn with<T>(&self, f: &str, g: impl FnOnce(&mut Folder) -> Result<T, OdError>) -> Result<T, OdError> {
        let mut all = self.folders.lock().unwrap();
        let x = all.get_mut(f).ok_or_else(|| od_err(OdKind::Gone, "gone"))?;
        if !x.on_demand {
            return Err(od_err(OdKind::NotOnDemand, NOT_ON_DEMAND));
        }
        if x.broken {
            return Err(od_err(OdKind::NotRunning, "not running"));
        }
        g(x)
    }

    fn add(&self, id: &str, f: Folder) {
        self.folders.lock().unwrap().insert(id.into(), f);
    }

    fn edit(&self, id: &str, g: impl FnOnce(&mut Folder)) {
        g(self.folders.lock().unwrap().get_mut(id).unwrap());
    }
}

impl HostCore for Fakes {
    fn folders(&self) -> Option<Vec<HostFolder>> {
        Some(
            self.folders
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, f)| f.on_demand)
                .map(|(id, f)| HostFolder { id: id.clone(), name: f.name.clone(), writable: f.writable })
                .collect(),
        )
    }
    fn hub_folders(&self) -> Option<Vec<HubFolder>> {
        Some(
            self.folders
                .lock()
                .unwrap()
                .iter()
                .map(|(id, f)| HubFolder { id: id.clone(), name: f.name.clone(), path: f.path.clone(), on_demand: f.on_demand, writable: f.writable })
                .collect(),
        )
    }
    fn item(&self, f: &str, id: &str) -> Result<Option<SyncItem>, OdError> {
        self.with(f, |x| Ok(x.items.get(id).cloned()))
    }
    fn children(&self, f: &str, id: &str, cursor: Option<&str>, limit: u32) -> Result<SyncItemPage, OdError> {
        let hook = self.on_children.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(id);
        }
        self.with(f, |x| {
            let all: Vec<SyncItem> = x.items.values().filter(|i| i.parent_id == id && i.id != "root").cloned().collect();
            let start = cursor.map_or(0, |c| c.parse().unwrap());
            let end = (start + limit as usize).min(all.len());
            Ok(SyncItemPage { items: all[start..end].to_vec(), next: (end < all.len()).then(|| end.to_string()) })
        })
    }
    fn changes(&self, f: &str, anchor: u64, limit: u32) -> Result<SyncChangePage, OdError> {
        self.with(f, |x| {
            let from = if anchor == 0 {
                0
            } else if x.expired || trs_sync::ondemand::anchor_index(anchor) != x.base || trs_sync::ondemand::anchor_sequence(anchor) > x.seq {
                // Another index's anchor, or one from the future (a folder joined again since): as the core's.
                return Err(od_err(OdKind::AnchorExpired, "expired"));
            } else {
                trs_sync::ondemand::anchor_sequence(anchor)
            };
            let pending: Vec<&(u64, String, bool)> = x.log.iter().filter(|(s, _, _)| *s > from).collect();
            let taken = &pending[..pending.len().min(limit as usize)];
            let mut items = Vec::new();
            let mut deleted = Vec::new();
            for (_, id, gone) in taken {
                match (gone, x.items.get(id)) {
                    (false, Some(i)) => items.push(i.clone()),
                    _ => deleted.push(id.clone()),
                }
            }
            let reached = taken.last().map_or(x.seq, |(s, _, _)| *s);
            Ok(SyncChangePage { items, deleted, anchor: x.anchor(reached), more: taken.len() < pending.len() })
        })
    }
    fn anchor(&self, f: &str) -> Result<u64, OdError> {
        self.with(f, |x| Ok(x.anchor(x.seq)))
    }
    fn fetch(&self, f: &str, id: &str, _: u64) -> Result<SyncFetch, OdError> {
        self.with(f, |x| {
            let i = x.items.get(id).cloned().ok_or_else(|| od_err(OdKind::NoItem, "no item"))?;
            Ok(SyncFetch { local_path: None, done: 0, total: i.size, item: i, unavailable: Vec::new(), waiting_for: None })
        })
    }
    fn cancel(&self, _: &str, _: &str) -> Result<(), OdError> {
        Ok(())
    }
    fn pin(&self, f: &str, id: &str, on: bool) -> Result<SyncItem, OdError> {
        self.with(f, |x| {
            let i = x.items.get_mut(id).ok_or_else(|| od_err(OdKind::NoItem, "no item"))?;
            i.pinned = on;
            Ok(i.clone())
        })
    }
    fn evict(&self, _: &str, _: &str) -> Result<SyncEvicted, OdError> {
        Ok(SyncEvicted::default())
    }
    fn create(&self, f: &str, parent: &str, name: &str, dir: bool, _: Option<&str>) -> Result<SyncItem, OdError> {
        self.calls.lock().unwrap().push(("create".into(), f.into(), name.into()));
        self.with(f, |x| {
            let i = file(&format!("n-{name}"), parent, name, if dir { SyncItemKind::Dir } else { SyncItemKind::File });
            x.put(i.clone());
            Ok(i)
        })
    }
    fn modify(&self, f: &str, id: &str, parent: Option<&str>, name: Option<&str>, _: Option<&str>, _: Option<&str>) -> Result<SyncItem, OdError> {
        self.calls.lock().unwrap().push(("modify".into(), f.into(), format!("{id}->{}", parent.unwrap_or("-"))));
        self.with(f, |x| {
            let mut i = x.items.get(id).cloned().ok_or_else(|| od_err(OdKind::NoItem, "no item"))?;
            if let Some(p) = parent {
                i.parent_id = p.into();
            }
            if let Some(n) = name {
                i.name = n.into();
            }
            x.put(i.clone());
            Ok(i)
        })
    }
    fn replace_hold(&self, f: &str) -> Option<trs_sync::ondemand::Hold> {
        self.folders.lock().unwrap().get(f).filter(|x| x.hold).map(|x| trs_sync::ondemand::Hold { left: x.hold_left, writing: false })
    }
    fn delete(&self, f: &str, id: &str, _: Option<trs_sync_net::DeleteSeen>) -> Result<SyncDeleted, OdError> {
        self.calls.lock().unwrap().push(("delete".into(), f.into(), id.into()));
        self.with(f, |x| {
            x.remove(id);
            Ok(SyncDeleted { deleted: 1, ..SyncDeleted::default() })
        })
    }
}

/// Folders `F` (Team, on demand: a folder `d` "Docs" with `a` "a.txt", and `b` "b.txt" at the top), `G` (Notes, on
/// demand: the same core ids as F's) and `L` (Music, full, at an absolute place).
fn three() -> Arc<Fakes> {
    let fakes = Arc::new(Fakes::default());
    let mut f = Folder::new("Team", "/Users/me/Seedwell/Team", true, 1);
    f.put(file("d", "root", "Docs", SyncItemKind::Dir));
    f.put(file("a", "d", "a.txt", SyncItemKind::File));
    f.put(file("b", "root", "b.txt", SyncItemKind::File));
    fakes.add("F", f);
    let mut g = Folder::new("Notes", "/Users/me/Seedwell/Notes", true, 2);
    g.put(file("d", "root", "Inbox", SyncItemKind::Dir));
    g.put(file("a", "d", "idea.md", SyncItemKind::File));
    fakes.add("G", g);
    fakes.add("L", Folder::new("Music", "/Volumes/Disk/Music", false, 3));
    fakes
}

fn hub_host(fakes: &Arc<Fakes>, dir: &Path) -> Arc<Host> {
    let mut h = Host::new(fakes.clone(), dir);
    h.hub = true;
    Arc::new(h)
}

fn req(v: Value) -> Request {
    let mut v = v;
    v["v"] = json!(PROTOCOL);
    v["folder"] = json!(HUB_ID);
    serde_json::from_value(v).unwrap()
}

fn ask(h: &Host, v: Value) -> Result<Value, HostError> {
    h.answer(&req(v), &mut |_, _| true)
}

fn ok(h: &Host, v: Value) -> Value {
    ask(h, v.clone()).unwrap_or_else(|e| panic!("{v}: {e:?}"))
}

fn kids(h: &Host, id: &str) -> Vec<Value> {
    ok(h, json!({"op":"children","item":id}))["items"].as_array().unwrap().clone()
}

/// Fails if a synced folder is missing from the Finder's one entry, or shown as the wrong kind (a full folder as a
/// folder of placeholders would mean its files are served from the cache instead of where they are), or if the order
/// depends on the configuration's. The hub's domain is the only one the host hears of.
#[test]
fn the_hub_lists_every_folder_with_its_kind_in_name_order() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    fakes.add("Z", Folder::new("alpha", "/Users/me/alpha", false, 4));
    let h = hub_host(&fakes, d.path());
    let top = kids(&h, "root");
    let shown: Vec<(&str, &str, &str)> = top.iter().map(|i| (i["name"].as_str().unwrap(), i["kind"].as_str().unwrap(), i["id"].as_str().unwrap())).collect();
    assert_eq!(shown, [("alpha", "symlink", "Z"), ("Music", "symlink", "L"), ("Notes", "dir", "G:root"), ("Team", "dir", "F:root")]);
    assert!(top.iter().all(|i| i["parent"] == "root"));
    // The hub's root itself: nothing is made at the top level.
    assert_eq!(ok(&h, json!({"op":"item","item":"root"}))["caps"], json!(["read"]));
    let told = Told::default();
    let (mut anchors, mut shown) = (HashMap::new(), None);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(*told.domains.lock().unwrap(), [vec![SyncHostDomain { id: HUB_ID.into(), name: "Seedwell".into() }]]);
    assert_eq!(*told.moved.lock().unwrap(), [HUB_ID], "one signal for the whole hub");
}

/// Fails if a full folder's item is anything but a link to its absolute place (a relative target would point inside
/// the hub; content served for it would copy files that are on this Mac already), or if the Finder may change it.
#[test]
fn a_full_folder_is_a_link_to_its_absolute_place() {
    let d = tempfile::tempdir().unwrap();
    let h = hub_host(&three(), d.path());
    let l = ok(&h, json!({"op":"item","item":"L"}));
    assert_eq!((l["kind"].as_str(), l["symlink_target"].as_str()), (Some("symlink"), Some("/Volumes/Disk/Music")));
    assert!(Path::new(l["symlink_target"].as_str().unwrap()).is_absolute());
    assert_eq!(l["caps"], json!(["read"]));
    assert_eq!(kids(&h, "L"), Vec::<Value>::new());
    assert_eq!(ask(&h, json!({"op":"fetch","item":"L"})).unwrap_err().kind, ErrKind::Failed, "nothing is copied for it");
    // Keeping it is no change: its files are here.
    assert_eq!(ok(&h, json!({"op":"pin","item":"L","on":true}))["item"]["id"], "L");
}

/// Fails if an item of one folder could be taken for another's (the same core ids in two folders), or if two folders
/// of the same name show under one name (the system would merge or drop one).
#[test]
fn ids_never_collide_between_folders_and_duplicate_names_stay_distinct() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, d.path());
    assert_eq!(ok(&h, json!({"op":"item","item":"F:a"}))["name"], "a.txt");
    assert_eq!(ok(&h, json!({"op":"item","item":"G:a"}))["name"], "idea.md");
    let f = kids(&h, "F:root");
    assert!(f.iter().all(|i| i["parent"] == "F:root" && i["id"].as_str().unwrap().starts_with("F:")), "{f:?}");
    assert_eq!(kids(&h, "G:d")[0]["parent"], "G:d");
    // Changes in one folder name its own items only.
    fakes.edit("G", |g| g.remove("a"));
    let all = ok(&h, json!({"op":"changes","anchor":"0"}));
    assert!(all["deleted"].as_array().unwrap().contains(&json!("G:a")));
    assert!(!all["deleted"].as_array().unwrap().contains(&json!("F:a")));
    // Three of one name (and one that already has the name a second would get), and one differing only in case.
    for (id, name) in [("H", "Team"), ("I", "Team 2"), ("J", "team")] {
        fakes.add(id, Folder::new(name, &format!("/Users/me/{id}"), true, 9));
    }
    let names: Vec<String> = kids(&h, "root").iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
    let lower: HashSet<String> = names.iter().map(|n| n.to_lowercase()).collect();
    assert_eq!(lower.len(), names.len(), "distinct, without case: {names:?}");
    assert!(names.contains(&"Team 2".to_string()) && names.contains(&"Team".to_string()), "{names:?}");
}

/// A little generator, so the run is the same every time.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// The system's copy of the hub: what it has been told, applied as File Provider applies a change page.
#[derive(Default)]
struct System {
    items: BTreeMap<String, Value>,
    anchor: Option<String>,
    /// The longest anchor it was handed.
    longest_anchor: usize,
}

impl System {
    /// Reads every change page from its anchor, `limit` items at most each.
    fn catch_up(&mut self, h: &Host, limit: u32) {
        for _ in 0..1000 {
            let p = match ask(h, json!({"op":"changes","anchor":self.anchor.clone().unwrap_or_default(),"limit":limit})) {
                Ok(p) => p,
                Err(e) if e.kind == ErrKind::AnchorExpired => {
                    // Listed again from the start.
                    self.items.clear();
                    self.anchor = None;
                    continue;
                }
                Err(e) => panic!("{e:?}"),
            };
            for i in p["items"].as_array().unwrap() {
                self.items.insert(i["id"].as_str().unwrap().into(), i.clone());
            }
            for gone in p["deleted"].as_array().unwrap() {
                let gone = gone.as_str().unwrap();
                self.items.remove(gone);
                // A folder's top item goes with everything below it.
                if let Some(f) = gone.strip_suffix(":root") {
                    self.items.retain(|id, _| !id.starts_with(&format!("{f}:")));
                }
            }
            self.anchor = Some(p["anchor"].as_str().unwrap().into());
            self.longest_anchor = self.longest_anchor.max(p["anchor"].as_str().unwrap().len());
            // The extension reports a page's updates before its deletes: a top-level name must never be taken by
            // an item while another that goes in the same page still has it.
            let goes: Vec<&str> = p["deleted"].as_array().unwrap().iter().filter_map(|d| d.as_str()).filter(|d| !d.contains(':') || d.ends_with(":root")).collect();
            let comes = p["items"].as_array().unwrap().iter().any(|i| i["parent"] == "root");
            assert!(goes.is_empty() || !comes, "top items come in the page where {goes:?} go");
            if !p["more"].as_bool().unwrap() {
                return;
            }
        }
        panic!("the feed never ended");
    }
}

/// What the hub holds now, as the system should have it: every top item, and every item of every on-demand folder.
fn truth(h: &Host, fakes: &Fakes) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for t in kids(h, "root") {
        out.insert(t["id"].as_str().unwrap().to_string(), (t["name"].as_str().unwrap().to_string(), t["metadata_version"].as_str().unwrap().to_string()));
    }
    for (f, x) in fakes.folders.lock().unwrap().iter() {
        if !x.on_demand {
            continue;
        }
        for i in x.items.values().filter(|i| i.id != "root") {
            out.insert(hub_id(f, &i.id), (i.name.clone(), i.metadata_version.clone()));
        }
    }
    out
}

/// Fails if the one anchor of the hub loses a change of any folder: a page cut by the limit in the middle of a folder,
/// a folder that cannot be read for a while, a folder added (listed from the start), switched to full or removed (its
/// top item and everything below go), a top-level rename, or one folder's index made anew. After every read the
/// system's copy is exactly what the hub holds.
#[test]
fn the_combined_anchor_never_loses_a_change_from_any_folder() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, d.path());
    let mut sys = System::default();
    let mut rng = Lcg(7);
    let mut n = 0;
    let ids = ["F", "G", "K"];
    for step in 0..600 {
        match rng.next(10) {
            0..=4 => {
                // A change in a folder: new, renamed or deleted items.
                let f = ids[rng.next(3) as usize];
                if !fakes.folders.lock().unwrap().get(f).is_some_and(|x| x.on_demand) {
                    continue;
                }
                n += 1;
                fakes.edit(f, |x| match rng.next(3) {
                    0 => x.put(file(&format!("i{n}"), "root", &format!("i{n}.txt"), SyncItemKind::File)),
                    1 => {
                        if let Some(id) = x.items.keys().find(|k| *k != "root").cloned() {
                            let mut i = x.items[&id].clone();
                            i.name = format!("r{n}-{}", i.name);
                            i.metadata_version = format!("m-{id}-{n}");
                            x.put(i);
                        }
                    }
                    _ => {
                        if let Some(id) = x.items.keys().rfind(|k| *k != "root" && !x.items.values().any(|c| &c.parent_id == *k)).cloned() {
                            x.remove(&id);
                        }
                    }
                });
            }
            5..=7 => {
                sys.catch_up(&h, 1 + rng.next(4) as u32);
                // (A folder that cannot be read now is behind by design: compared once it can.)
                if fakes.folders.lock().unwrap().values().any(|x| x.broken) {
                    continue;
                }
                assert_eq!(
                    sys.items.iter().map(|(k, v)| (k.clone(), (v["name"].as_str().unwrap().to_string(), v["metadata_version"].as_str().unwrap().to_string()))).collect::<BTreeMap<_, _>>(),
                    truth(&h, &fakes),
                    "step {step}"
                );
            }
            8 => {
                // A folder unreadable for a while, or readable again.
                let f = ids[rng.next(2) as usize];
                fakes.edit(f, |x| x.broken = !x.broken);
            }
            _ => match rng.next(5) {
                // A folder joins (from the start), goes, switches to full and back, or is renamed.
                0 => {
                    if !fakes.folders.lock().unwrap().contains_key("K") {
                        let mut k = Folder::new("Kites", "/Users/me/Kites", true, 5);
                        k.put(file("k", "root", "kite.txt", SyncItemKind::File));
                        fakes.add("K", k);
                    }
                }
                1 => {
                    fakes.folders.lock().unwrap().remove("K");
                }
                2 => fakes.edit("G", |g| g.on_demand = !g.on_demand),
                3 => fakes.edit("F", |f| f.name = format!("Team {}", step)),
                _ => {}
            },
        }
        // A broken folder is not read, so the comparison waits until it is readable: repaired before each check.
        if step % 50 == 49 {
            for f in ids {
                if fakes.folders.lock().unwrap().contains_key(f) {
                    fakes.edit(f, |x| x.broken = false);
                }
            }
        }
    }
    for f in ids {
        if fakes.folders.lock().unwrap().contains_key(f) {
            fakes.edit(f, |x| x.broken = false);
        }
    }
    sys.catch_up(&h, 3);
    let got: BTreeMap<String, (String, String)> =
        sys.items.iter().map(|(k, v)| (k.clone(), (v["name"].as_str().unwrap().to_string(), v["metadata_version"].as_str().unwrap().to_string()))).collect();
    assert_eq!(got, truth(&h, &fakes));
    // One folder's index made anew: the hub's anchor expires (the system lists everything again).
    fakes.edit("F", |f| f.expired = true);
    let e = ask(&h, json!({"op":"changes","anchor":sys.anchor.clone().unwrap()})).unwrap_err();
    assert_eq!(e.kind, ErrKind::AnchorExpired);
    // An anchor that is not the hub's: expired too, never read as "nothing changed".
    assert_eq!(ask(&h, json!({"op":"changes","anchor":"12345"})).unwrap_err().kind, ErrKind::AnchorExpired);
    // However many folders took part, every anchor handed out fit the system's limit.
    assert!(sys.longest_anchor <= 40, "{}", sys.longest_anchor);
}

/// The broken-folder rule of [`the_combined_anchor_never_loses_a_change_from_any_folder`], alone: a folder that
/// cannot be read keeps its anchor, so what changed in it meanwhile comes once it can.
#[test]
fn a_folder_that_cannot_be_read_keeps_its_place_in_the_feed() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, d.path());
    let mut sys = System::default();
    sys.catch_up(&h, 100);
    fakes.edit("F", |f| {
        f.put(file("late", "root", "late.txt", SyncItemKind::File));
        f.broken = true;
    });
    fakes.edit("G", |g| g.put(file("g2", "root", "two.txt", SyncItemKind::File)));
    sys.catch_up(&h, 100);
    assert!(sys.items.contains_key("G:g2") && !sys.items.contains_key("F:late"));
    fakes.edit("F", |f| f.broken = false);
    sys.catch_up(&h, 100);
    assert!(sys.items.contains_key("F:late"), "the change made while it could not be read came");
}

/// Fails if the Finder could rename, move, delete or trash a top-level item (a synced folder is renamed and removed in
/// Seedwell; a delete there would delete the folder's files on every device), make something at the top level, or
/// move an item from one folder into another (each folder is its own index: the move would be a delete in one and a
/// file without its content in the other). Nothing reaches the core for any of them.
#[test]
fn top_level_items_refuse_rename_move_and_delete() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, d.path());
    for t in kids(&h, "root") {
        let caps: Vec<&str> = t["caps"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        assert!(!caps.iter().any(|c| matches!(*c, "rename" | "reparent" | "delete" | "trash")), "{t}");
    }
    for id in ["F:root", "L", "root"] {
        assert_eq!(ask(&h, json!({"op":"modify","item":id,"name":"New"})).unwrap_err().kind, ErrKind::Failed, "{id}");
        assert_eq!(ask(&h, json!({"op":"modify","item":id,"parent":"G:d"})).unwrap_err().kind, ErrKind::Failed, "{id}");
        assert_eq!(ask(&h, json!({"op":"delete","item":id,"recursive":true})).unwrap_err().kind, ErrKind::DeletionRejected, "{id}");
        // A change of nothing Seedwell keeps (a tag): the item as it is.
        assert!(ask(&h, json!({"op":"modify","item":id})).is_ok(), "{id}");
    }
    assert_eq!(ask(&h, json!({"op":"create","parent":"root","name":"x","kind":"dir"})).unwrap_err().kind, ErrKind::Failed);
    assert_eq!(ask(&h, json!({"op":"create","parent":"L","name":"x","kind":"dir"})).unwrap_err().kind, ErrKind::Failed);
    // Across folders: refused.
    assert_eq!(ask(&h, json!({"op":"modify","item":"F:a","parent":"G:d"})).unwrap_err().kind, ErrKind::Failed);
    assert_eq!(ask(&h, json!({"op":"modify","item":"F:a","parent":"root"})).unwrap_err().kind, ErrKind::Failed);
    assert!(fakes.calls.lock().unwrap().is_empty(), "nothing reached the core: {:?}", fakes.calls.lock().unwrap());
    // Within a folder: the core moves it, with its own ids.
    let moved = ok(&h, json!({"op":"modify","item":"F:b","parent":"F:d"}));
    assert_eq!((moved["item"]["id"].as_str(), moved["item"]["parent"].as_str()), (Some("F:b"), Some("F:d")));
    assert_eq!(fakes.calls.lock().unwrap()[0], ("modify".to_string(), "F".to_string(), "b->d".to_string()));
    // Made inside a folder: the core makes it there.
    let made = ok(&h, json!({"op":"create","parent":"G:d","name":"New","kind":"dir"}));
    assert_eq!((made["item"]["id"].as_str(), made["item"]["parent"].as_str()), (Some("G:n-New"), Some("G:d")));
}

/// Fails if a read-only member's folder lets the Finder write in the hub (a change it makes is never sent: the
/// version would be held nowhere), at its top or below.
#[test]
fn read_only_roles_carry_through() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    fakes.edit("G", |g| g.writable = false);
    let h = hub_host(&fakes, d.path());
    let g = kids(&h, "root").into_iter().find(|t| t["id"] == "G:root").unwrap();
    assert_eq!(g["caps"], json!(["read"]));
    let f = kids(&h, "root").into_iter().find(|t| t["id"] == "F:root").unwrap();
    assert_eq!(f["caps"], json!(["read", "write"]));
    for i in kids(&h, "G:root").into_iter().chain(kids(&h, "G:d")) {
        let caps: Vec<&str> = i["caps"].as_array().unwrap().iter().map(|c| c.as_str().unwrap()).collect();
        assert!(!caps.contains(&"write") && !caps.contains(&"rename"), "{i}");
    }
}

#[derive(Default)]
struct Told {
    domains: Mutex<Vec<Vec<SyncHostDomain>>>,
    moved: Mutex<Vec<String>>,
}

impl SyncHostListener for Told {
    fn feed_moved(&self, folder_id: String) {
        self.moved.lock().unwrap().push(folder_id);
    }
    fn domains(&self, domains: Vec<SyncHostDomain>) {
        self.domains.lock().unwrap().push(domains);
    }
    fn remove_domain(&self, _: String) -> SyncHostRemoval {
        SyncHostRemoval { error: None }
    }
    fn notice(&self, _: String, _: String) {}
    fn nav_pane(&self, _: SyncNavPane) {}
    fn verify_peer(&self, _: Vec<u8>) -> bool {
        true
    }
}

/// The Mac's host, as `FileProviderHost.releaseDomain` acts for the hub: the system hands over what it has pending
/// below the folder (here: a folder made in the Finder), then the host reads the folder's place in the Finder and
/// lets go (`sync_host_let_go`).
struct MacHost {
    host: OnceLock<Arc<Host>>,
    visible: PathBuf,
    place: PathBuf,
    /// Whether the hand-over was still served (not refused) while the system sent it.
    handed_over: Mutex<Option<bool>>,
    notices: Mutex<Vec<SyncHostNotice>>,
}

impl SyncHostListener for MacHost {
    fn feed_moved(&self, _: String) {}
    fn domains(&self, _: Vec<SyncHostDomain>) {}
    fn remove_domain(&self, folder_id: String) -> SyncHostRemoval {
        let h = self.host.get().unwrap();
        // waitForChanges(below: F:root): the system sends what it has pending there.
        let sent = h.answer(&req(json!({"op":"create","parent":"F:root","name":"Pending","kind":"dir"})), &mut |_, _| true);
        *self.handed_over.lock().unwrap() = Some(sent.is_ok());
        match h.let_go(&folder_id, "Team", Some(&self.place), Some(&self.visible)) {
            Ok(Some(n)) => self.notices.lock().unwrap().push(n),
            Ok(None) => {}
            // The host refuses the removal with the core's words.
            Err(e) => return SyncHostRemoval { error: Some(e) },
        }
        SyncHostRemoval { error: None }
    }
    fn notice(&self, _: String, _: String) {}
    fn nav_pane(&self, _: SyncNavPane) {}
    fn verify_peer(&self, _: Vec<u8>) -> bool {
        true
    }
}

fn touch(p: &Path, bytes: &[u8], mtime_ns: Option<i64>) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
    if let Some(ns) = mtime_ns {
        let t = SystemTime::UNIX_EPOCH + Duration::from_nanos(ns as u64);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }
}

/// Removing (or switching) one on-demand folder of the hub: fails if the system's pending changes for it are refused
/// before they are handed over, if anything the system still has on disk that the folder does not hold is lost or
/// moved out of the system's tree (it is copied next to the folder, as `<name> (changes not synced)`, and the user is
/// told), if what the folder holds as it is gets copied too, if the system may still send changes for it once it was
/// let go of, or if another folder's items or anchor change. The folder's top item goes from the hub.
#[test]
fn removing_one_folder_hands_over_its_changes_and_leaves_the_others() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, &d.path().join("group"));
    let mut sys = System::default();
    sys.catch_up(&h, 100);
    let before = h.with_anchors(|a| a.resolve(sys.anchor.as_deref())).unwrap();
    let g_before: BTreeMap<String, Value> = sys.items.iter().filter(|(k, _)| k.starts_with("G:")).map(|(k, v)| (k.clone(), v.clone())).collect();

    // The folder as the Finder has it on disk.
    let visible = d.path().join("CloudStorage/Seedwell/Team");
    let mt = Some(1_792_000_000_000_000_000);
    touch(&visible.join("b.txt"), b"other content", mt); // held otherwise: another size
    touch(&visible.join("Docs/a.txt"), b"12345", mt); // held as it is
    touch(&visible.join("Docs/new.md"), b"written after the hand-over", None);
    touch(&visible.join("late.txt"), b"late", None);
    touch(&visible.join(".DS_Store"), b"finder", None);
    let place = d.path().join("Seedwell/Team");
    let mac = Arc::new(MacHost { host: OnceLock::new(), visible: visible.clone(), place: place.clone(), handed_over: Mutex::new(None), notices: Mutex::new(Vec::new()) });
    let _ = mac.host.set(h.clone());
    let release = Release { host: h.clone(), listener: mac.clone() };
    trs_sync_net::OnDemandHost::release(&release, "F", "Team", &place).unwrap();

    assert_eq!(*mac.handed_over.lock().unwrap(), Some(true), "the hand-over was served");
    assert!(fakes.calls.lock().unwrap().iter().any(|c| c.0 == "create" && c.1 == "F" && c.2 == "Pending"));
    let kept = d.path().join("Seedwell/Team (changes not synced)");
    assert_eq!(std::fs::read(kept.join("late.txt")).unwrap(), b"late");
    assert_eq!(std::fs::read(kept.join("b.txt")).unwrap(), b"other content");
    assert_eq!(std::fs::read(kept.join("Docs/new.md")).unwrap(), b"written after the hand-over");
    assert!(!kept.join("Docs/a.txt").exists(), "held as it is: not copied");
    assert!(!kept.join(".DS_Store").exists());
    assert!(visible.join("late.txt").exists(), "the system's tree is never changed");
    let notices = mac.notices.lock().unwrap();
    assert!(notices.len() == 1 && notices[0].text.contains("Team (changes not synced)"), "{notices:?}");

    // Let go of: what the system sends for it now is refused (until the core's removal ends), and it left the hub.
    assert_eq!(ask(&h, json!({"op":"create","parent":"F:root","name":"x","kind":"dir"})).unwrap_err().kind, ErrKind::Unreachable);
    assert!(!kids(&h, "root").iter().any(|t| t["id"] == "F:root"));
    sys.catch_up(&h, 100);
    assert!(!sys.items.keys().any(|k| k.starts_with("F:")), "its subtree went");
    let after = h.with_anchors(|a| a.resolve(sys.anchor.as_deref())).unwrap();
    assert_eq!(after.folders.get("G"), before.folders.get("G"), "the other folder's anchor did not move");
    let g_after: BTreeMap<String, Value> = sys.items.iter().filter(|(k, _)| k.starts_with("G:")).map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(g_after, g_before);

    // The core refused after all (`settled`, as it says when the removal ends): the folder is back, listed again.
    trs_sync_net::OnDemandHost::settled(&release, "F");
    sys.catch_up(&h, 100);
    assert!(sys.items.contains_key("F:a"), "back, from the start");
}

/// The let-go of a folder while its changes are still pending: fails if the system's late change is taken once the
/// host let go (it would land in a folder already on its way out) instead of being left on disk, where the copy finds
/// it.
#[test]
fn a_frozen_folder_refuses_changes_until_it_is_back() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, d.path());
    lock(&h.frozen).insert("F".into());
    assert_eq!(ask(&h, json!({"op":"create","parent":"F:root","name":"x","kind":"dir"})).unwrap_err().kind, ErrKind::Unreachable);
    assert_eq!(ask(&h, json!({"op":"delete","item":"F:b"})).unwrap_err().kind, ErrKind::Unreachable);
    // Reading goes on, and other folders are not held.
    assert!(ask(&h, json!({"op":"item","item":"F:b"})).is_ok());
    assert!(ask(&h, json!({"op":"create","parent":"G:root","name":"x","kind":"dir"})).is_ok());
    h.let_go_undo("F");
    assert!(ask(&h, json!({"op":"create","parent":"F:root","name":"x","kind":"dir"})).is_ok());
}

/// The system's set of items on disk in the hub: fails if a folder's files are taken for another's, or if the files
/// a folder let go of are not freed by its core.
#[test]
fn the_hubs_report_of_what_is_on_disk_is_each_folders_own() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, d.path());
    // The baseline, then F's a leaves the disk (Remove Download); G's a stays.
    assert_eq!(ok(&h, json!({"op":"materialized","ids":["F:a","F:b","G:a"]}))["released"], 0);
    assert_eq!(lock(&h.materialized).get("F").unwrap().len(), 2);
    assert_eq!(lock(&h.materialized).get("G").unwrap().iter().collect::<Vec<_>>(), ["a"]);
    assert_eq!(ok(&h, json!({"op":"materialized","ids":["F:b","G:a"]}))["released"], 1);
    // What is held is asked for under the hub's ids.
    assert_eq!(h.hub_queue_materialize(vec!["F:b".into()]), 2, "F's a, G's a");
    let mut asked = Vec::new();
    while let Some(b) = h.hub_next_batch() {
        asked.extend(b.ids);
    }
    asked.sort();
    assert_eq!(asked, ["F:a", "G:a"]);
}

/// A host-like run of the migration from the domains of the per-folder era, as `FileProviderHost.migrate` does it:
/// for each old domain the release note, the system's pending changes handed over (served for the folder's own
/// domain), the removal keeping what is unsynced (put next to the folder and told), the note done; then the mark.
struct OldSystem {
    /// Registered domains: id, name, a pending change to hand over, what the removal keeps.
    domains: Vec<(String, String, Option<&'static str>, Option<PathBuf>)>,
}

/// One run of the migration; `crash_in`: the domain during whose removal the app stops (after the note and the
/// hand-over, before the domain went). Returns the notices.
fn migrate(state: &Path, h: &Host, sys: &mut OldSystem, places: &HashMap<String, PathBuf>, crash_in: Option<&str>) -> Vec<SyncHostNotice> {
    let mut told = unfinished_releases(state);
    let registered: Vec<SyncHostDomain> = sys.domains.iter().map(|d| SyncHostDomain { id: d.0.clone(), name: d.1.clone() }).collect();
    let plan = migration_plan(state, registered);
    for d in &plan {
        let note = release_note(state, &d.id);
        std::fs::create_dir_all(note.parent().unwrap()).unwrap();
        std::fs::write(&note, &d.name).unwrap();
        let i = sys.domains.iter().position(|x| x.0 == d.id).unwrap();
        if let Some(name) = sys.domains[i].2.take() {
            // The old domain's extension asks with the folder's own id: served as before.
            let r: Request = serde_json::from_value(json!({"v":PROTOCOL,"op":"create","folder":d.id,"parent":"root","name":name,"kind":"dir"})).unwrap();
            h.answer(&r, &mut |_, _| true).unwrap();
        }
        if crash_in == Some(d.id.as_str()) {
            return told;
        }
        let (_, name, _, preserved) = sys.domains.remove(i);
        if let Some(p) = preserved
            && let Some(n) = keep_preserved(&name, places.get(&d.id).map(PathBuf::as_path), &p)
        {
            told.push(n);
        }
        std::fs::remove_file(&note).unwrap();
    }
    migration_done(state);
    told
}

/// Fails if the migration from per-folder domains loses a pending change (it must reach the core before the domain
/// goes), deletes what macOS kept of a domain (it goes next to the folder, and the user is told), does not resume
/// after the app stopped in the middle (the rest is removed at the next start, and the user hears that the stopped
/// removal may have left changes with macOS), or runs again once done (a domain registered later stays).
#[test]
fn the_migration_runs_once_and_resumes_after_an_interruption() {
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("state");
    let fakes = three();
    let h = hub_host(&fakes, &d.path().join("group"));
    let kept_f = d.path().join("kept-F");
    std::fs::create_dir_all(&kept_f).unwrap();
    std::fs::write(kept_f.join("unsynced.txt"), b"mine").unwrap();
    let mut sys = OldSystem {
        domains: vec![
            ("F".into(), "Team".into(), Some("Pending F"), Some(kept_f.clone())),
            ("G".into(), "Notes".into(), Some("Pending G"), None),
        ],
    };
    let places: HashMap<String, PathBuf> = [("F".to_string(), d.path().join("Seedwell/Team")), ("G".to_string(), d.path().join("Seedwell/Notes"))].into();

    // First start: F goes; the app stops while G is removed.
    let told = migrate(&state, &h, &mut sys, &places, Some("G"));
    assert!(fakes.calls.lock().unwrap().iter().any(|c| c.1 == "F" && c.2 == "Pending F"), "F's pending change reached the core");
    assert_eq!(std::fs::read(d.path().join("Seedwell/Team (changes not synced)/unsynced.txt")).unwrap(), b"mine");
    assert!(told.iter().any(|n| n.title.contains("Team")), "{told:?}");
    assert_eq!(sys.domains.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["G"]);
    assert!(std::fs::read_to_string(state.join("file-provider-hub")).is_err(), "not done");

    // Next start: the user hears of the stopped removal; G (with its change, handed over already) goes; done.
    let told = migrate(&state, &h, &mut sys, &places, None);
    assert!(told.iter().any(|n| n.title.contains("Notes") && n.text.contains("CloudStorage")), "{told:?}");
    assert!(sys.domains.is_empty());
    assert!(fakes.calls.lock().unwrap().iter().any(|c| c.1 == "G" && c.2 == "Pending G"));

    // Done: never again, also for a domain registered later.
    sys.domains.push(("X".into(), "Later".into(), None, None));
    assert!(migrate(&state, &h, &mut sys, &places, None).is_empty());
    assert_eq!(sys.domains.len(), 1, "left alone");
    // The hub itself is never in a plan.
    let fresh = d.path().join("fresh");
    assert_eq!(migration_plan(&fresh, vec![hub_domain(), SyncHostDomain { id: "F".into(), name: "Team".into() }]).len(), 1);
}

/// The hub through the real core and the socket, as the extension asks: fails if a full folder is not a link to its
/// real place, an on-demand folder's items do not come from its own index under the hub's ids, or the host is told of
/// more than the one domain; and if removing or switching a folder in hub mode does not ask the host first.
#[test]
fn the_hub_is_served_from_the_real_core() {
    let home = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    // SAFETY: set before the core starts any thread; nothing else here reads the environment.
    unsafe { std::env::set_var("HOME", home.path()) };
    let state = work.path().join("state");
    let store = crate::SettingsStore::new(&state);
    let mut st = store.load_settings();
    st.folders.download_dir = work.path().join("done").display().to_string();
    st.folders.incomplete_dir = work.path().join("incomplete").display().to_string();
    st.folders.watch.clear();
    st.network.port_mapping = false;
    st.network.dht = false;
    store.save_settings(&st).unwrap();
    let c = crate::Core::new(crate::CoreOptions { config_dir: state.display().to_string(), mock: false, allow_loopback_trackers: false, forbidden_root: None }).unwrap();
    let t0 = Instant::now();
    while !c.sync_snapshot().running {
        assert!(t0.elapsed().as_secs() < 20, "sync did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    let docs = work.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("a.txt"), b"hello").unwrap();
    let f = c.sync_add_folder(docs.display().to_string()).unwrap();
    c.sync_set_on_demand(f.id.clone(), true, None).unwrap();
    let full_dir = work.path().join("Full");
    std::fs::create_dir_all(&full_dir).unwrap();
    let full = c.sync_add_folder(full_dir.display().to_string()).unwrap();

    let group = work.path().join("group");
    let told = Arc::new(Told::default());
    let socket = c.sync_host_start(group.display().to_string(), told.clone(), SyncHostMode::Hub).unwrap();
    let ask = |req: Value| -> Value {
        let mut s = UnixStream::connect(&socket).unwrap();
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        serde_json::from_str(out.lines().last().unwrap()).unwrap()
    };
    let top = ask(json!({"v":1,"op":"children","folder":HUB_ID,"item":"root"}));
    let top = top["ok"]["items"].as_array().unwrap().clone();
    let docs_top = top.iter().find(|t| t["name"] == "Docs").unwrap();
    assert_eq!((docs_top["kind"].as_str(), docs_top["id"].as_str()), (Some("dir"), Some(c.sync_host_hub_item(f.id.clone()).as_str())));
    let full_top = top.iter().find(|t| t["name"] == "Full").unwrap();
    assert_eq!(full_top["kind"], "symlink");
    assert_eq!(full_top["symlink_target"].as_str(), Some(full.path.as_str()));
    assert!(Path::new(&full.path).is_absolute());
    assert_eq!(full_top["id"].as_str(), Some(c.sync_host_hub_item(full.id.clone()).as_str()));
    let kids = ask(json!({"v":1,"op":"children","folder":HUB_ID,"item":docs_top["id"]}));
    let a = &kids["ok"]["items"][0];
    assert_eq!(a["name"], "a.txt");
    assert!(a["id"].as_str().unwrap().starts_with(&format!("{}:", f.id)));
    let got = ask(json!({"v":1,"op":"fetch","folder":HUB_ID,"item":a["id"]}));
    assert_eq!(std::fs::read(got["ok"]["path"].as_str().unwrap()).unwrap(), b"hello");
    assert!(ask(json!({"v":1,"op":"anchor","folder":HUB_ID}))["ok"]["anchor"].as_str().unwrap().starts_with("h2."));
    let t0 = Instant::now();
    while told.domains.lock().unwrap().is_empty() {
        assert!(t0.elapsed().as_secs() < 5, "the host never heard of the hub");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(told.domains.lock().unwrap()[0], [c.sync_host_hub_domain()]);
    // Switched to full in hub mode: the host is asked first (Told lets go at once), and the folder is a link after.
    c.sync_set_on_demand(f.id.clone(), false, None).unwrap();
    assert_eq!(std::fs::read(docs.join("a.txt")).unwrap(), b"hello", "full again, at its place");
    let top = ask(json!({"v":1,"op":"children","folder":HUB_ID,"item":"root"}));
    let docs_top = top["ok"]["items"].as_array().unwrap().iter().find(|t| t["name"] == "Docs").unwrap().clone();
    assert_eq!(docs_top["kind"], "symlink");
    c.sync_host_stop();
}

/// Fails if a let-go drops a folder from the hub (and the system then deletes its files) when something found on
/// disk could not be copied: a full disk, a file that cannot be read, no writable place. The removal is refused with
/// the core's words, the folder stays in the hub and its changes are taken again.
#[test]
fn a_let_go_that_cannot_keep_a_file_keeps_the_folder() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, &d.path().join("group"));
    let visible = d.path().join("CloudStorage/Seedwell/Team");
    touch(&visible.join("late.txt"), b"never synced", None);
    // The place where kept files go cannot be written.
    let parent = d.path().join("Seedwell");
    std::fs::create_dir_all(&parent).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
    let place = parent.join("Team");
    let mac = Arc::new(MacHost { host: OnceLock::new(), visible: visible.clone(), place: place.clone(), handed_over: Mutex::new(None), notices: Mutex::new(Vec::new()) });
    let _ = mac.host.set(h.clone());
    let release = Release { host: h.clone(), listener: mac.clone() };
    let refused = trs_sync_net::OnDemandHost::release(&release, "F", "Team", &place);
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let e = refused.unwrap_err();
    assert!(e.contains("late.txt") && e.contains("stays"), "{e}");
    assert!(kids(&h, "root").iter().any(|t| t["id"] == "F:root"), "still in the hub");
    assert!(ask(&h, json!({"op":"create","parent":"F:root","name":"again","kind":"dir"})).is_ok(), "its changes are taken again");
    assert!(visible.join("late.txt").exists());
    // A folder of it that cannot be read: refused too (what is in it could not be looked at).
    let locked = visible.join("Locked");
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = h.let_go("F", "Team", Some(&d.path().join("Elsewhere/Team")), Some(&visible));
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.is_err());
    assert!(kids(&h, "root").iter().any(|t| t["id"] == "F:root"));
}

/// Fails if a look of the let-go runs after the folder left the hub: the watch then signals the system, which deletes
/// its copy, and a file saved between two looks is lost. Every look runs while the folder is still listed, the looks go
/// on until one finds nothing new (a save during a look is kept by the next), and only then does the folder go.
#[test]
fn every_look_runs_while_the_folder_is_listed_and_a_save_between_looks_is_kept() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, &d.path().join("group"));
    let visible = d.path().join("CloudStorage/Seedwell/Team");
    touch(&visible.join("late.txt"), b"late", None);
    std::fs::create_dir_all(visible.join("Docs")).unwrap();
    let host: Arc<OnceLock<Arc<Host>>> = Arc::new(OnceLock::new());
    let _ = host.set(h.clone());
    let (looks, gone_while_looking) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicBool::new(false)));
    let at = visible.join("saved meanwhile.txt");
    {
        let (host, looks, gone) = (host.clone(), looks.clone(), gone_while_looking.clone());
        *fakes.on_children.lock().unwrap() = Some(Arc::new(move |id: &str| {
            let h = host.get().unwrap();
            // Each look reads Docs once: while it runs, the system must still see the folder.
            if id == "d" {
                if !h.hub_tops().unwrap().iter().any(|t| t.folder == "F") {
                    gone.store(true, Ordering::SeqCst);
                }
                // During the first look, the user saves into the top folder, looked at already.
                if looks.fetch_add(1, Ordering::SeqCst) == 0 {
                    touch(&at, b"saved", None);
                }
            }
        }));
    }
    let place = d.path().join("Seedwell/Team");
    let n = h.let_go("F", "Team", Some(&place), Some(&visible)).unwrap();
    *fakes.on_children.lock().unwrap() = None;
    assert!(n.is_some());
    assert!(!gone_while_looking.load(Ordering::SeqCst), "the folder left the hub while a look ran");
    assert!(looks.load(Ordering::SeqCst) >= 3, "looked until a look found nothing new");
    let kept = d.path().join("Seedwell/Team (changes not synced)");
    assert_eq!(std::fs::read(kept.join("saved meanwhile.txt")).unwrap(), b"saved");
    assert_eq!(std::fs::read(kept.join("late.txt")).unwrap(), b"late");
    assert!(!h.hub_tops().unwrap().iter().any(|t| t.folder == "F"), "gone once the looks are over");
}

/// Fails if a folder that never settles (something writing into it all the time) is let go of anyway after the last
/// look: the let-go gives up after its looks and keeps the folder, in plain words.
#[test]
fn a_folder_that_keeps_changing_is_kept() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, &d.path().join("group"));
    let visible = d.path().join("CloudStorage/Seedwell/Team");
    std::fs::create_dir_all(&visible).unwrap();
    let n = Arc::new(AtomicU64::new(0));
    {
        let (visible, n) = (visible.clone(), n.clone());
        *fakes.on_children.lock().unwrap() = Some(Arc::new(move |id: &str| {
            if id == "root" {
                touch(&visible.join(format!("new {}.txt", n.fetch_add(1, Ordering::SeqCst))), b"x", None);
            }
        }));
    }
    let e = h.let_go("F", "Team", Some(&d.path().join("Seedwell/Team")), Some(&visible)).unwrap_err();
    *fakes.on_children.lock().unwrap() = None;
    assert!(e.contains("kept changing"), "{e}");
    assert!(h.hub_tops().unwrap().iter().any(|t| t.folder == "F"));
    assert!(ask(&h, json!({"op":"create","parent":"F:root","name":"x","kind":"dir"})).is_ok(), "taken again");
}

/// Fails if content the system sends for a folder after the let-go's last look (before it deletes its copy) is only
/// refused: the system would then delete it. It is kept next to the folder at the item's path, and still refused.
#[test]
fn what_the_system_sends_after_the_last_look_is_kept() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let group = d.path().join("group");
    let h = hub_host(&fakes, &group);
    let place = d.path().join("Seedwell/Team");
    assert!(h.let_go("F", "Team", Some(&place), None).unwrap().is_none());
    let u = group.join(UPLOADS);
    std::fs::create_dir_all(&u).unwrap();
    std::fs::write(u.join("one"), b"new file").unwrap();
    std::fs::write(u.join("two"), b"edited").unwrap();
    let e = ask(&h, json!({"op":"create","parent":"F:d","name":"late.txt","kind":"file","contents":"one"})).unwrap_err();
    assert_eq!(e.kind, ErrKind::Unreachable);
    let e = ask(&h, json!({"op":"modify","item":"F:b","contents":"two","base":"c-b"})).unwrap_err();
    assert_eq!(e.kind, ErrKind::Unreachable);
    let kept = d.path().join("Seedwell/Team (changes not synced)");
    assert_eq!(std::fs::read(kept.join("Docs/late.txt")).unwrap(), b"new file");
    assert_eq!(std::fs::read(kept.join("b.txt")).unwrap(), b"edited");
    assert!(!fakes.calls.lock().unwrap().iter().any(|c| c.1 == "F"), "nothing reached the folder's core");
}

/// Fails if a file that vanishes between the listing and its copy (a Word lock file `~$…`, a safe-save `.sb-…`)
/// refuses the removal: it is nothing to keep. What is still there is kept.
#[test]
fn a_file_that_vanishes_before_its_copy_is_nothing_to_keep() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, &d.path().join("group"));
    let visible = d.path().join("CloudStorage/Seedwell/Team");
    touch(&visible.join("~$report.docx"), b"lock", None);
    touch(&visible.join(".sb-1234-x/part"), b"temp", None);
    touch(&visible.join("real.txt"), b"keep me", None);
    let (lock_file, temp) = (visible.join("~$report.docx"), visible.join(".sb-1234-x"));
    *lock(&h.before_copy) = Some(Box::new(move || {
        std::fs::remove_file(&lock_file).unwrap();
        std::fs::remove_dir_all(&temp).unwrap();
    }));
    let n = h.let_go("F", "Team", Some(&d.path().join("Seedwell/Team")), Some(&visible));
    let kept = d.path().join("Seedwell/Team (changes not synced)");
    assert!(n.is_ok(), "{n:?}");
    assert_eq!(std::fs::read(kept.join("real.txt")).unwrap(), b"keep me");
    assert!(!kept.join("~$report.docx").exists());
}

/// Fails if a token number handed out after the store's last save that did not reach the disk is handed out again
/// after a start, for another anchor: the system would read it as the other one and skip changes.
#[test]
fn a_token_handed_out_after_a_lost_save_is_never_reused() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let state = d.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let mut store = AnchorStore::open(Some(&state));
    let one = HubAnchor { tops: 1, ..HubAnchor::default() };
    let _ = store.mint(one);
    // The next save fails (the folder cannot be written).
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o555)).unwrap();
    let lost = store.mint(HubAnchor { tops: 2, ..HubAnchor::default() });
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut again = AnchorStore::open(Some(&state));
    let other = again.mint(HubAnchor { tops: 3, ..HubAnchor::default() });
    assert_ne!(other, lost, "a number handed out once is never another anchor's");
    assert_eq!(again.resolve(Some(&lost)).unwrap_err().kind, ErrKind::AnchorExpired);
}

/// Fails if the pass the watch gives a held folder carries over to a new hold (a new delete started since, a save by
/// replace of its own), or outlives the folder or its let-go: the hub would hand out that save's temporary delete.
#[test]
fn a_pass_is_for_the_hold_it_was_given_for() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let mut h = Host::new(fakes.clone(), d.path());
    h.hub = true;
    h.replace_cap = Duration::from_millis(30);
    let h = Arc::new(h);
    let mut sys = System::default();
    sys.catch_up(&h, 100);
    fakes.edit("F", |f| f.hold = true);
    let told = Told::default();
    let (mut anchors, mut shown) = (HashMap::new(), None);
    h.watch_once(&told, &mut anchors, &mut shown);
    std::thread::sleep(Duration::from_millis(40));
    h.watch_once(&told, &mut anchors, &mut shown);
    assert!(lock(&h.pass_hold).contains_key("F"), "let through");
    // Before the hub reads it, a new delete starts a hold of its own, ending later.
    fakes.edit("F", |f| {
        f.remove("b");
        f.hold_left = Duration::from_secs(5);
    });
    sys.catch_up(&h, 100);
    assert!(sys.items.contains_key("F:b"), "the new hold's temporary delete was not handed out");
    // A let-go, its undo, and the folder's going clear the pass.
    lock(&h.pass_hold).insert("F".into(), Instant::now() + Duration::from_secs(9));
    let _ = h.let_go("F", "Team", None, None);
    assert!(!lock(&h.pass_hold).contains_key("F"));
    h.let_go_undo("F");
    lock(&h.pass_hold).insert("G".into(), Instant::now() + Duration::from_secs(9));
    fakes.folders.lock().unwrap().remove("G");
    h.watch_once(&told, &mut anchors, &mut shown);
    assert!(!lock(&h.pass_hold).contains_key("G"));
}

/// Fails if the hub's anchor grows with the number of folders: the system takes 500 bytes at most (a page that
/// carries one, too), and a longer one breaks its listing. It is a short token; what it stands for is the core's,
/// kept across a start, and a token of another store (its file lost) is expired, never read as another anchor.
#[test]
fn the_anchor_is_short_whatever_the_folders_and_survives_a_start() {
    let d = tempfile::tempdir().unwrap();
    let fakes = Arc::new(Fakes::default());
    for n in 0..50 {
        let mut f = Folder::new(&format!("Folder {n:02} with a rather long name"), &format!("/Users/me/Seedwell/F{n}"), n % 5 != 0, 10 + n);
        f.put(file("a", "root", "a.txt", SyncItemKind::File));
        fakes.add(&format!("{:0>63}", format!("ABCDEFG-{n}")), f);
    }
    let state = d.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    let mut first = Host::new(fakes.clone(), &d.path().join("group"));
    first.hub = true;
    first.hub_state = Some(state.clone());
    let mut sys = System::default();
    sys.catch_up(&first, 7);
    let a = sys.anchor.clone().unwrap();
    assert!(a.len() <= hub::ANCHOR_MAX && sys.longest_anchor <= 40, "{a}");
    let now = ok(&first, json!({"op":"anchor"}))["anchor"].as_str().unwrap().to_string();
    assert!(now.len() <= 40);
    // The app starts again: the system's anchor still reads, and what changed since comes.
    fakes.edit(&format!("{:0>63}", "ABCDEFG-1"), |f| f.put(file("b", "root", "b.txt", SyncItemKind::File)));
    let mut again = Host::new(fakes.clone(), &d.path().join("group"));
    again.hub = true;
    again.hub_state = Some(state.clone());
    let p = ok(&again, json!({"op":"changes","anchor":a}));
    assert_eq!(p["items"].as_array().unwrap().len(), 1, "{p}");
    // Its file lost: another store, so the token is expired (the system lists everything again).
    std::fs::remove_file(state.join("file-provider-hub-anchors.json")).unwrap();
    let mut fresh = Host::new(fakes.clone(), &d.path().join("group"));
    fresh.hub = true;
    fresh.hub_state = Some(state);
    assert_eq!(ask(&fresh, json!({"op":"changes","anchor":a})).unwrap_err().kind, ErrKind::AnchorExpired);
}

/// Fails if the hub hands out a folder's temporary delete while a save by replace runs in it (Word: the document
/// renamed aside, the new one written at its name) because another folder's feed moved: the system would delete the
/// item and download it again. Once the hold ends, or the watch lets it through after a window, the change comes.
#[test]
fn another_folders_change_does_not_hand_out_a_save_by_replace() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let mut h = Host::new(fakes.clone(), d.path());
    h.hub = true;
    h.replace_cap = Duration::from_millis(30);
    let h = Arc::new(h);
    let mut sys = System::default();
    sys.catch_up(&h, 100);
    fakes.edit("F", |f| {
        f.remove("b");
        f.hold = true;
    });
    fakes.edit("G", |g| g.put(file("g2", "root", "two.txt", SyncItemKind::File)));
    sys.catch_up(&h, 100);
    assert!(sys.items.contains_key("G:g2"));
    assert!(sys.items.contains_key("F:b"), "the temporary delete was not handed out");
    // A run of holds never keeps it back for more than a window: the watch lets the folder through once.
    let told = Told::default();
    let (mut anchors, mut shown) = (HashMap::new(), None);
    h.watch_once(&told, &mut anchors, &mut shown);
    std::thread::sleep(Duration::from_millis(40));
    h.watch_once(&told, &mut anchors, &mut shown);
    sys.catch_up(&h, 100);
    assert!(!sys.items.contains_key("F:b"), "let through after the window");
    // And held again after that read, while the hold lasts.
    fakes.edit("F", |f| f.remove("a"));
    sys.catch_up(&h, 100);
    assert!(sys.items.contains_key("F:a"));
    fakes.edit("F", |f| f.hold = false);
    sys.catch_up(&h, 100);
    assert!(!sys.items.contains_key("F:a"), "the hold ended: it comes");
}

/// Fails if a folder made on demand elsewhere (the command line, an agent) is not told to the host: the hub's domain
/// list is the same, so without that the host never shows the new folder's held files downloaded.
#[test]
fn the_host_hears_again_when_a_folder_becomes_on_demand() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    let h = hub_host(&fakes, d.path());
    let told = Told::default();
    let (mut anchors, mut shown) = (HashMap::new(), None);
    h.watch_once(&told, &mut anchors, &mut shown);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(told.domains.lock().unwrap().len(), 1);
    fakes.edit("L", |l| l.on_demand = true);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(told.domains.lock().unwrap().len(), 2, "told again: one more on-demand folder");
    fakes.edit("L", |l| l.on_demand = false);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(told.domains.lock().unwrap().len(), 2, "one fewer: nothing to show");
}

/// Fails if a top item comes in the same page as a top item that goes (a folder switched to full: its folder goes and
/// its link comes under the same name; "Team 2" becoming "Team"): the extension reports the page's updates first, so
/// the new one would collide with the old name. The goes come alone, the top level in the next page.
#[test]
fn top_items_that_go_come_alone_in_their_page() {
    let d = tempfile::tempdir().unwrap();
    let fakes = three();
    fakes.add("H", Folder::new("Team", "/Users/me/Other/Team", true, 9));
    let h = hub_host(&fakes, d.path());
    let mut sys = System::default();
    sys.catch_up(&h, 100);
    assert_eq!(sys.items["H:root"]["name"], "Team 2");
    // F goes (H is "Team" now), and G is switched to full (its folder goes, its link comes as "Notes").
    fakes.folders.lock().unwrap().remove("F");
    fakes.edit("G", |g| g.on_demand = false);
    let p = ok(&h, json!({"op":"changes","anchor":sys.anchor.clone().unwrap()}));
    let mut gone: Vec<&str> = p["deleted"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
    gone.sort();
    assert_eq!(gone, ["F:root", "G:root"]);
    assert!(p["items"].as_array().unwrap().is_empty() && p["more"] == true, "{p}");
    sys.catch_up(&h, 100);
    assert_eq!(sys.items["H:root"]["name"], "Team");
    assert_eq!(sys.items["G"]["kind"], "symlink");
}
