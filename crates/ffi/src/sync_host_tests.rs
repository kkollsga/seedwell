//! The File Provider extension's server against a fake core (the protocol, the decisions), and once through the
//! real core and the socket.

use std::os::unix::net::UnixStream;
use std::sync::atomic::AtomicUsize;

use super::*;
use crate::sync::ondemand::SyncFailed;

/// A small on-demand folder in memory. Fetches complete after `fetch_rounds` calls (never, with `u32::MAX`).
#[derive(Default)]
struct Fake {
    items: Mutex<BTreeMap<String, SyncItem>>,
    anchor: AtomicU64,
    writable: AtomicBool,
    /// Where file contents are (`<cache>/<id>`).
    cache: PathBuf,
    fetch_rounds: AtomicU64,
    fetched: AtomicU64,
    cancelled: Mutex<Vec<String>>,
    evicted: Mutex<Vec<String>>,
    /// `modify` answers with a conflict copy.
    conflict: AtomicBool,
    /// `delete` frees instead (a read-only member).
    frees: AtomicBool,
    deleted: Mutex<Vec<String>>,
    /// The content each create and modify was handed.
    uploaded: Mutex<Vec<Vec<u8>>>,
    /// Each modify takes this long, and how many there were.
    modify_ms: AtomicU64,
    modifies: AtomicU64,
    /// Folder sync is not running (yet): no folders are known.
    stopped: AtomicBool,
    /// The newest sequence below any folder, for a delete's check (the core's).
    newest: AtomicU64,
    /// How many change pages were asked for.
    pages: AtomicU64,
    /// The anchor cannot be read.
    no_anchor: AtomicBool,
}

fn item(id: &str, parent: &str, name: &str, kind: SyncItemKind, state: SyncItemState) -> SyncItem {
    SyncItem {
        id: id.into(),
        parent_id: parent.into(),
        path: if parent == "root" { name.into() } else { format!("{parent}/{name}") },
        name: name.into(),
        kind,
        size: 5,
        mtime_ns: 1_792_000_000_000_000_000,
        state,
        pinned: false,
        content_version: format!("c-{id}"),
        metadata_version: format!("m-{id}"),
        exec: false,
        symlink_target: None,
        conflict_of: None,
    }
}

fn no_item(id: &str) -> OdError {
    OdError { kind: OdKind::NoItem, error: CoreError::Failed { message: format!("{NO_ITEM}{id} in this folder.") } }
}

impl Fake {
    fn new(cache: &Path) -> Arc<Fake> {
        let f = Fake { cache: cache.to_path_buf(), ..Fake::default() };
        f.writable.store(true, Ordering::Relaxed);
        f.fetch_rounds.store(2, Ordering::Relaxed);
        let mut items = BTreeMap::new();
        let mut root = item("root", "root", "", SyncItemKind::Dir, SyncItemState::Downloaded);
        root.path = String::new();
        items.insert("root".into(), root);
        items.insert("d".into(), item("d", "root", "Docs", SyncItemKind::Dir, SyncItemState::Downloaded));
        items.insert("a".into(), item("a", "d", "a.txt", SyncItemKind::File, SyncItemState::OnlineOnly));
        items.insert("b".into(), item("b", "root", "b.txt", SyncItemKind::File, SyncItemState::Downloaded));
        items.insert("u".into(), item("u", "root", "gone.txt", SyncItemKind::File, SyncItemState::Unavailable));
        std::fs::create_dir_all(cache).unwrap();
        std::fs::write(cache.join("a"), b"hello").unwrap();
        std::fs::write(cache.join("b"), b"bytes").unwrap();
        *f.items.lock().unwrap() = items;
        Arc::new(f)
    }

    fn get(&self, id: &str) -> Option<SyncItem> {
        self.items.lock().unwrap().get(id).cloned()
    }
}

impl HostCore for Fake {
    fn folders(&self) -> Option<Vec<HostFolder>> {
        (!self.stopped.load(Ordering::Relaxed)).then(|| vec![HostFolder { id: "F".into(), name: "Team".into(), writable: self.writable.load(Ordering::Relaxed) }])
    }
    fn item(&self, _: &str, id: &str) -> Result<Option<SyncItem>, OdError> {
        Ok(self.get(id))
    }
    fn children(&self, _: &str, id: &str, cursor: Option<&str>, limit: u32) -> Result<SyncItemPage, OdError> {
        let all: Vec<SyncItem> = self.items.lock().unwrap().values().filter(|i| i.parent_id == id && i.id != "root").cloned().collect();
        let start = cursor.map_or(0, |c| c.parse().unwrap());
        let end = (start + limit as usize).min(all.len());
        Ok(SyncItemPage { items: all[start..end].to_vec(), next: (end < all.len()).then(|| end.to_string()) })
    }
    fn changes(&self, _: &str, anchor: u64, _: u32) -> Result<SyncChangePage, OdError> {
        if anchor == 99 {
            return Err(OdError { kind: OdKind::AnchorExpired, error: CoreError::Failed { message: "Anchor expired.".into() } });
        }
        self.pages.fetch_add(1, Ordering::SeqCst);
        // From the start: every item; from 5 or 7, a (in d) changed; later, b changed. Up to the anchor now.
        let items = match anchor {
            0 => return Ok(SyncChangePage { items: self.items.lock().unwrap().values().cloned().collect(), deleted: vec!["x".into()], anchor: u64::MAX - 1, more: false }),
            5 | 7 => vec![self.get("a").unwrap()],
            _ => vec![self.get("b").unwrap()],
        };
        Ok(SyncChangePage { items, deleted: vec!["x".into()], anchor: self.anchor.load(Ordering::Relaxed), more: false })
    }
    fn anchor(&self, _: &str) -> Result<u64, OdError> {
        if self.no_anchor.load(Ordering::Relaxed) {
            return Err(CoreError::Failed { message: "no anchor".into() }.into());
        }
        Ok(self.anchor.load(Ordering::Relaxed))
    }
    fn fetch(&self, _: &str, id: &str, wait_ms: u64) -> Result<SyncFetch, OdError> {
        std::thread::sleep(Duration::from_millis(wait_ms.min(20)));
        let n = self.fetched.fetch_add(1, Ordering::Relaxed) + 1;
        let i = self.get(id).ok_or_else(|| no_item(id))?;
        let here = n >= self.fetch_rounds.load(Ordering::Relaxed);
        Ok(SyncFetch {
            local_path: here.then(|| self.cache.join(id).display().to_string()),
            done: if here { i.size } else { 0 },
            total: i.size,
            item: i,
            unavailable: Vec::new(),
        })
    }
    fn cancel(&self, _: &str, id: &str) -> Result<(), OdError> {
        self.cancelled.lock().unwrap().push(id.into());
        Ok(())
    }
    fn pin(&self, _: &str, id: &str, on: bool) -> Result<SyncItem, OdError> {
        let mut items = self.items.lock().unwrap();
        let i = items.get_mut(id).ok_or_else(|| no_item(id))?;
        i.pinned = on;
        Ok(i.clone())
    }
    fn evict(&self, _: &str, id: &str) -> Result<SyncEvicted, OdError> {
        self.evicted.lock().unwrap().push(id.into());
        Ok(SyncEvicted { files: 1, bytes: 5, ..SyncEvicted::default() })
    }
    fn create(&self, _: &str, parent: &str, name: &str, dir: bool, contents: Option<&str>) -> Result<SyncItem, OdError> {
        if !dir {
            self.uploaded.lock().unwrap().push(std::fs::read(contents.expect("a new file's content is handed over")).unwrap());
        }
        // Each create a new item (two folders of one name are two items).
        let n = self.modifies.fetch_add(0, Ordering::SeqCst) + self.items.lock().unwrap().len() as u64;
        let i = item(&format!("n-{name}-{n}"), parent, name, if dir { SyncItemKind::Dir } else { SyncItemKind::File }, SyncItemState::ModifiedHere);
        self.items.lock().unwrap().insert(i.id.clone(), i.clone());
        Ok(i)
    }
    fn modify(&self, _: &str, id: &str, parent: Option<&str>, name: Option<&str>, contents: Option<&str>, base: Option<&str>) -> Result<SyncItem, OdError> {
        if let Some(c) = contents {
            self.uploaded.lock().unwrap().push(std::fs::read(c).unwrap());
        }
        self.modifies.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(self.modify_ms.load(Ordering::Relaxed)));
        if self.conflict.load(Ordering::Relaxed) {
            assert_eq!(base, Some("c-b"), "the base is the item's content version");
            let copy = item("copy", "root", "b (conflict).txt", SyncItemKind::File, SyncItemState::ModifiedHere);
            self.items.lock().unwrap().insert("copy".into(), copy.clone());
            return Ok(copy);
        }
        if name == Some("busy") {
            return Err(CoreError::Failed { message: "Renaming needs a device that has these files to be connected.".into() }.into());
        }
        let mut items = self.items.lock().unwrap();
        let i = items.get_mut(id).ok_or_else(|| no_item(id))?;
        if let Some(n) = name {
            i.name = n.into();
        }
        if let Some(p) = parent {
            i.parent_id = p.into();
        }
        Ok(i.clone())
    }
    fn delete(&self, _: &str, id: &str, seen: Option<trs_sync_net::DeleteSeen>) -> Result<SyncDeleted, OdError> {
        let changed = match &seen {
            Some(trs_sync_net::DeleteSeen::Folder(s)) => *s < self.newest.load(Ordering::Relaxed),
            Some(trs_sync_net::DeleteSeen::File(v)) => self.get(id).is_some_and(|i| &i.content_version != v),
            None => false,
        };
        if changed {
            return Err(OdError { kind: OdKind::Changed, error: CoreError::Failed { message: "Something there changed.".into() } });
        }
        self.deleted.lock().unwrap().push(id.into());
        if self.frees.load(Ordering::Relaxed) {
            return Ok(SyncDeleted { deleted: 0, freed: Some(SyncEvicted { files: 1, ..SyncEvicted::default() }), failed: Vec::new() });
        }
        if id == "d" {
            return Ok(SyncDeleted { deleted: 1, freed: None, failed: vec![SyncFailed { path: "Docs/a.txt".into(), error: "it is open elsewhere".into() }] });
        }
        Ok(SyncDeleted { deleted: 1, ..SyncDeleted::default() })
    }
}

fn host(fake: &Arc<Fake>, dir: &Path) -> Host {
    Host::new(fake.clone(), dir)
}

/// One request over a connected pair of sockets, as the extension sends it: every line of the answer.
fn ask(h: &Host, req: Value) -> Vec<Value> {
    let (mut client, mut server) = UnixStream::pair().unwrap();
    // Written from a thread of its own: a long request does not fit the socket's buffer.
    let mut w = client.try_clone().unwrap();
    let line = format!("{req}\n");
    let writer = std::thread::spawn(move || {
        let _ = w.write_all(line.as_bytes());
    });
    serve(h, &mut server);
    drop(server);
    let mut out = String::new();
    client.read_to_string(&mut out).unwrap();
    writer.join().unwrap();
    out.lines().map(|l| serde_json::from_str(l).unwrap()).collect()
}

fn ok(h: &Host, req: Value) -> Value {
    let lines = ask(h, req);
    let last = lines.last().unwrap();
    assert_eq!(last["v"], 1);
    assert!(last.get("err").is_none(), "{last}");
    last["ok"].clone()
}

fn err(h: &Host, req: Value) -> (String, String) {
    let lines = ask(h, req);
    let e = &lines.last().unwrap()["err"];
    (e["kind"].as_str().unwrap().to_string(), e["message"].as_str().unwrap().to_string())
}

/// The item's wire form, as the Swift side's tests decode it (`FileProviderWireTests`): keep both in step.
#[test]
fn an_item_on_the_wire() {
    let mut i = item("a", "d", "a.txt", SyncItemKind::File, SyncItemState::OnlineOnly);
    i.pinned = true;
    let w = serde_json::to_string(&wire_item(&i, true, false)).unwrap();
    assert_eq!(
        w,
        r#"{"id":"a","parent":"d","name":"a.txt","kind":"file","size":5,"mtime_ns":1792000000000000000,"content_version":"c-a","metadata_version":"m-a","state":"online_only","keep":true,"pinned":true,"exec":false,"symlink_target":null,"caps":["read","write","rename","reparent","delete"]}"#
    );
}

#[test]
fn what_a_user_may_do_follows_the_role() {
    let file = item("a", "d", "a.txt", SyncItemKind::File, SyncItemState::OnlineOnly);
    let root = item("root", "root", "", SyncItemKind::Dir, SyncItemState::Downloaded);
    assert_eq!(caps_of(&file, true), ["read", "write", "rename", "reparent", "delete"]);
    // A read-only member reads, and its delete frees the content.
    assert_eq!(caps_of(&file, false), ["read", "delete"]);
    // The folder itself takes new items, and is never renamed, moved or deleted from the Finder.
    assert_eq!(caps_of(&root, true), ["read", "write"]);
    assert_eq!(caps_of(&root, false), ["read"]);
}

#[test]
fn core_errors_get_the_kind_the_system_needs() {
    use trs_sync_net::{OnDemandError as E, SyncError};
    let kind = |e: SyncError| HostError::from(crate::sync::ondemand::od_err(e)).kind;
    // From the service's typed errors, whatever their words.
    assert_eq!(kind(SyncError::OnDemand(E::NoItem, "Docs/a.txt: no such item in the folder".into())), ErrKind::NoSuchItem);
    assert_eq!(kind(SyncError::OnDemand(E::NotOnDemand, "the folder is not on demand".into())), ErrKind::Unreachable);
    assert_eq!(kind(SyncError::OnDemand(E::AnchorExpired, "anchor expired".into())), ErrKind::AnchorExpired);
    assert_eq!(kind(SyncError::OnDemand(E::Unavailable, "x".into())), ErrKind::Unreachable);
    assert_eq!(kind(SyncError::NotFound), ErrKind::Unreachable);
    // A rename that needs a device which holds the files, and one that answered busy: shown in the core's words.
    let e = HostError::from(crate::sync::ondemand::od_err(SyncError::Invalid("Renaming needs a device that has these files to be connected".into())));
    assert_eq!((e.kind, e.message.as_str()), (ErrKind::Failed, "Renaming needs a device that has these files to be connected."));
}

#[test]
fn items_children_changes_and_anchors() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    assert_eq!(ok(&h, json!({"v":1,"op":"hello"}))["app"], format!("Seedwell {}", env!("CARGO_PKG_VERSION")));
    assert_eq!(ok(&h, json!({"v":1,"op":"item","folder":"F","item":"a"}))["name"], "a.txt");
    assert_eq!(err(&h, json!({"v":1,"op":"item","folder":"F","item":"nope"})).0, "no_such_item");

    // Paged: the cursor is the core's.
    let p1 = ok(&h, json!({"v":1,"op":"children","folder":"F","item":"root","limit":2}));
    assert_eq!(p1["items"].as_array().unwrap().len(), 2);
    let p2 = ok(&h, json!({"v":1,"op":"children","folder":"F","item":"root","cursor":p1["next"],"limit":2}));
    assert_eq!((p2["items"].as_array().unwrap().len(), &p2["next"]), (1, &Value::Null));

    // Anchors use all 64 bits: they travel as text.
    let ch = ok(&h, json!({"v":1,"op":"changes","folder":"F","anchor":"0"}));
    assert_eq!((ch["anchor"].as_str().unwrap(), &ch["deleted"][0], &ch["more"]), ("18446744073709551614", &json!("x"), &json!(false)));
    assert_eq!(err(&h, json!({"v":1,"op":"changes","folder":"F","anchor":"99"})).0, "anchor_expired");
    fake.anchor.store(42, Ordering::Relaxed);
    assert_eq!(ok(&h, json!({"v":1,"op":"anchor","folder":"F"}))["anchor"], "42");

    // Another protocol version, or an op this server does not know.
    assert_eq!(err(&h, json!({"v":2,"op":"item","folder":"F","item":"a"})).0, "bad_request");
    assert_eq!(err(&h, json!({"v":1,"op":"what"})).0, "bad_request");
    assert_eq!(ask(&h, json!("not a request")).last().unwrap()["err"]["kind"], "bad_request");
}

#[test]
fn a_read_only_member_reads_and_deletes_only() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.writable.store(false, Ordering::Relaxed);
    let h = host(&fake, d.path());
    assert_eq!(ok(&h, json!({"v":1,"op":"item","folder":"F","item":"a"}))["caps"], json!(["read", "delete"]));
    // Its delete frees the content: the system is told it was rejected, so the item comes back online only.
    fake.frees.store(true, Ordering::Relaxed);
    let (kind, why) = err(&h, json!({"v":1,"op":"delete","folder":"F","item":"b","recursive":true}));
    assert_eq!(kind, "deletion_rejected");
    assert!(why.contains("online only"), "{why}");
}

#[test]
fn a_fetch_reports_progress_and_hands_over_a_copy() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.fetch_rounds.store(3, Ordering::Relaxed);
    let h = host(&fake, d.path());
    let lines = ask(&h, json!({"v":1,"op":"fetch","folder":"F","item":"a"}));
    assert_eq!(lines.len(), 3, "two progress lines, then the answer: {lines:?}");
    assert_eq!(lines[0]["progress"], json!({"done": 0, "total": 5}));
    let path = lines[2]["ok"]["path"].as_str().unwrap();
    assert!(Path::new(path).starts_with(d.path().join(CLONES)), "{path}");
    assert_eq!(std::fs::read(path).unwrap(), b"hello");
    assert_eq!(lines[2]["ok"]["item"]["id"], "a");
    // The cache's own file stays (the copy is the extension's).
    assert!(fake.cache.join("a").exists());

    // Folders have no content; a version no device can give says so at once.
    assert_eq!(err(&h, json!({"v":1,"op":"fetch","folder":"F","item":"d"})).0, "failed");
    let (kind, why) = err(&h, json!({"v":1,"op":"fetch","folder":"F","item":"u"}));
    assert_eq!((kind.as_str(), why.as_str()), ("unreachable", trs_sync::ondemand::UNAVAILABLE));
}

#[test]
fn a_fetch_that_gets_nowhere_gives_up_and_takes_its_request_back() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.fetch_rounds.store(u64::MAX, Ordering::Relaxed);
    let mut h = host(&fake, d.path());
    h.stall = Duration::from_millis(100);
    let (kind, _) = err(&h, json!({"v":1,"op":"fetch","folder":"F","item":"a"}));
    assert_eq!(kind, "unreachable");
    assert_eq!(*fake.cancelled.lock().unwrap(), ["a"]);
}

#[test]
fn closing_the_connection_cancels_the_fetch() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.fetch_rounds.store(u64::MAX, Ordering::Relaxed);
    let h = Arc::new(host(&fake, d.path()));
    let (mut client, mut server) = UnixStream::pair().unwrap();
    client.write_all(b"{\"v\":1,\"op\":\"fetch\",\"folder\":\"F\",\"item\":\"a\"}\n").unwrap();
    let h2 = h.clone();
    let t = std::thread::spawn(move || serve(&h2, &mut server));
    let mut first = [0u8; 8];
    client.read_exact(&mut first).unwrap();
    drop(client);
    t.join().unwrap();
    assert_eq!(*fake.cancelled.lock().unwrap(), ["a"]);
}

#[test]
fn new_and_changed_items() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let up = d.path().join(UPLOADS).join("x");
    std::fs::create_dir_all(up.parent().unwrap()).unwrap();
    std::fs::write(&up, b"new!!").unwrap();
    let made = ok(&h, json!({"v":1,"op":"create","folder":"F","parent":"d","name":"n.txt","kind":"file","contents":"x"}));
    assert_eq!((made["item"]["parent"].as_str(), made["item"]["state"].as_str()), (Some("d"), Some("modified_here")));
    assert_eq!(ok(&h, json!({"v":1,"op":"create","folder":"F","name":"New","kind":"dir"}))["item"]["kind"], "dir");
    // A link is not synced: excluded (the system keeps it here and stops asking), never retried as a failure.
    assert_eq!(err(&h, json!({"v":1,"op":"create","folder":"F","name":"l","kind":"symlink"})).0, "excluded");

    // The system not sure it made it already: what is there and the same is the item (nothing written twice).
    let before = fake.items.lock().unwrap().len();
    let again = ok(&h, json!({"v":1,"op":"create","folder":"F","parent":"root","name":"Docs","kind":"dir","may_exist":true}));
    assert_eq!((again["item"]["id"].as_str(), fake.items.lock().unwrap().len()), (Some("d"), before));

    // A rename.
    let renamed = ok(&h, json!({"v":1,"op":"modify","folder":"F","item":"a","name":"z.txt"}));
    assert_eq!((renamed["item"]["name"].as_str(), &renamed["fetch_content"]), (Some("z.txt"), &json!(false)));
    // Only fields Seedwell does not keep: the item as it is.
    assert_eq!(ok(&h, json!({"v":1,"op":"modify","folder":"F","item":"b"}))["item"]["id"], "b");
    // A rename the core refuses (no device that holds the files connected): the core's words.
    let (kind, why) = err(&h, json!({"v":1,"op":"modify","folder":"F","item":"a","name":"busy"}));
    assert_eq!((kind.as_str(), why.as_str()), ("failed", "Renaming needs a device that has these files to be connected."));

    // An edit of content that moved on: a conflict copy, and the item fetches the other version again.
    fake.conflict.store(true, Ordering::Relaxed);
    let c = ok(&h, json!({"v":1,"op":"modify","folder":"F","item":"b","contents":"x","base":"c-b"}));
    assert_eq!((c["item"]["id"].as_str(), &c["fetch_content"], c["conflict_copy"]["id"].as_str()), (Some("b"), &json!(true), Some("copy")));
}

#[test]
fn deletes() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    // A newer version than the user saw is not deleted unseen.
    assert_eq!(err(&h, json!({"v":1,"op":"delete","folder":"F","item":"b","base":"c-old"})).0, "deletion_rejected");
    assert!(fake.deleted.lock().unwrap().is_empty());
    // A folder with items, deleted without them.
    assert_eq!(err(&h, json!({"v":1,"op":"delete","folder":"F","item":"d","recursive":false})).0, "not_empty");
    // Something below could not go: the folder comes back.
    let (kind, why) = err(&h, json!({"v":1,"op":"delete","folder":"F","item":"d","recursive":true}));
    assert_eq!(kind, "deletion_rejected");
    assert!(why.contains("Docs/a.txt") && why.contains("open elsewhere"), "{why}");
    assert_eq!(ok(&h, json!({"v":1,"op":"delete","folder":"F","item":"b","base":"c-b","recursive":true}))["deleted"], 1);
}

#[test]
fn pins_show_as_kept_and_as_the_users_own() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let p = ok(&h, json!({"v":1,"op":"pin","folder":"F","item":"d","on":true}));
    assert_eq!((&p["item"]["keep"], &p["item"]["pinned"]), (&json!(true), &json!(true)));
    // Below a kept folder: kept, through the folder.
    fake.items.lock().unwrap().get_mut("a").unwrap().pinned = true;
    let a = ok(&h, json!({"v":1,"op":"item","folder":"F","item":"a"}));
    assert_eq!((&a["keep"], &a["pinned"]), (&json!(true), &json!(false)));
    let p = ok(&h, json!({"v":1,"op":"pin","folder":"F","item":"d","on":false}));
    assert_eq!(p["item"]["keep"], false);
}

#[test]
fn files_the_system_let_go_of_are_released_by_the_core() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    // The first report after a start is the baseline.
    assert_eq!(ok(&h, json!({"v":1,"op":"materialized","folder":"F","ids":["root","d","b"]}))["released"], 0);
    // b left the set (Remove Download, or disk pressure); d did too but is a folder (its files say what went).
    assert_eq!(ok(&h, json!({"v":1,"op":"materialized","folder":"F","ids":["root"]}))["released"], 1);
    let t = Instant::now();
    while fake.evicted.lock().unwrap().is_empty() {
        assert!(t.elapsed() < Duration::from_secs(5), "never evicted");
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(*fake.evicted.lock().unwrap(), ["b"]);
}

/// What the host was told.
#[derive(Default)]
struct Told {
    domains: Mutex<Vec<Vec<SyncHostDomain>>>,
    moved: Mutex<Vec<String>>,
    calls: AtomicUsize,
    /// Refuse every peer (any other process than the extension).
    refuse: AtomicBool,
    tokens: Mutex<Vec<Vec<u8>>>,
    /// Domains it was asked to remove; refuse to; notices.
    removed: Mutex<Vec<String>>,
    refuse_removal: AtomicBool,
    notices: Mutex<Vec<(String, String)>>,
}

impl SyncHostListener for Told {
    fn feed_moved(&self, folder_id: String) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.moved.lock().unwrap().push(folder_id);
    }
    fn domains(&self, domains: Vec<SyncHostDomain>) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.domains.lock().unwrap().push(domains);
    }
    fn verify_peer(&self, audit_token: Vec<u8>) -> bool {
        self.tokens.lock().unwrap().push(audit_token);
        !self.refuse.load(Ordering::Relaxed)
    }
    fn remove_domain(&self, folder_id: String) -> SyncHostRemoval {
        self.removed.lock().unwrap().push(folder_id);
        if self.refuse_removal.load(Ordering::Relaxed) {
            return SyncHostRemoval { error: Some("the system said no".into()) };
        }
        SyncHostRemoval { error: None }
    }
    fn notice(&self, title: String, text: String) {
        self.notices.lock().unwrap().push((title, text));
    }
}

#[test]
fn the_host_hears_of_domains_and_of_moved_feeds() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let told = Told::default();
    let (mut anchors, mut shown) = (HashMap::new(), None);
    h.watch_once(&told, &mut anchors, &mut shown);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(*told.domains.lock().unwrap(), [vec![SyncHostDomain { id: "F".into(), name: "Team".into() }]], "once, until it changes");
    assert_eq!(*told.moved.lock().unwrap(), ["F"], "signalled once at the start");
    fake.anchor.store(7, Ordering::Relaxed);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(*told.moved.lock().unwrap(), ["F", "F"]);
}

#[test]
fn leftovers_are_swept() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    std::fs::create_dir(d.path().join(CLONES)).unwrap();
    let left = d.path().join(CLONES).join(h.fresh_name());
    std::fs::write(&left, b"x").unwrap();
    h.sweep(Duration::from_secs(3600));
    assert!(left.exists(), "young files stay");
    h.sweep(Duration::ZERO);
    assert!(!left.exists());
}

/// The socket in a folder whose path is longer than a socket path may be: reached relative to the folder.
#[cfg(target_os = "macos")]
#[test]
fn a_socket_in_a_deep_folder() {
    let d = tempfile::tempdir().unwrap();
    let deep = d.path().join("a".repeat(60)).join("b".repeat(60));
    std::fs::create_dir_all(&deep).unwrap();
    assert!(deep.join(SOCKET_NAME).as_os_str().len() > 104);
    let fake = Fake::new(&d.path().join("cache"));
    let s = Server::start(fake, &deep, Arc::new(Told::default())).unwrap();
    let mut c = unix::connect(&deep, SOCKET_NAME).unwrap();
    c.write_all(b"{\"v\":1,\"op\":\"item\",\"folder\":\"F\",\"item\":\"b\"}\n").unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).unwrap();
    assert!(out.contains("\"b.txt\""), "{out}");
    drop(s);
    assert!(!deep.join(SOCKET_NAME).exists(), "the socket goes with the server");
}

/// The app's calls, through the real core: an on-demand folder listed and fetched over the socket.
#[test]
fn the_app_serves_the_extension_from_the_real_core() {
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

    let group = work.path().join("group");
    let told = Arc::new(Told::default());
    let socket = c.sync_host_start(group.display().to_string(), told.clone()).unwrap();
    assert_eq!(socket, group.join(SOCKET_NAME).display().to_string());
    let ask = |req: Value| -> Value {
        let mut s = UnixStream::connect(&socket).unwrap();
        s.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        serde_json::from_str(out.lines().last().unwrap()).unwrap()
    };
    let kids = ask(json!({"v":1,"op":"children","folder":f.id,"item":"root"}));
    let a = &kids["ok"]["items"][0];
    assert_eq!((a["name"].as_str(), a["state"].as_str()), (Some("a.txt"), Some("modified_here")));
    assert_eq!(a["caps"], json!(["read", "write", "rename", "reparent", "delete"]));
    let got = ask(json!({"v":1,"op":"fetch","folder":f.id,"item":a["id"]}));
    assert_eq!(std::fs::read(got["ok"]["path"].as_str().unwrap()).unwrap(), b"hello");
    assert!(ask(json!({"v":1,"op":"anchor","folder":f.id}))["ok"]["anchor"].as_str().unwrap().parse::<u64>().unwrap() > 0);
    let t0 = Instant::now();
    while told.domains.lock().unwrap().is_empty() {
        assert!(t0.elapsed().as_secs() < 5, "the host never heard of the domain");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(told.domains.lock().unwrap()[0], [SyncHostDomain { id: f.id.clone(), name: "Docs".into() }]);
    // A folder that is not on demand (typed, not read from the words): the system backs off.
    let docs2 = work.path().join("Full");
    std::fs::create_dir_all(&docs2).unwrap();
    let full = c.sync_add_folder(docs2.display().to_string()).unwrap();
    assert_eq!(ask(json!({"v":1,"op":"children","folder":full.id,"item":"root"}))["err"]["kind"], "unreachable");
    // Chats are not reachable: a channel is no on-demand folder, and no op reads or sends messages.
    let ch = c.chat_new("Team".into()).unwrap();
    for op in ["item", "children", "changes", "anchor", "fetch", "create", "modify", "delete", "pin"] {
        let r = ask(json!({"v":1,"op":op,"folder":ch.id,"item":"root","name":"x","kind":"dir"}));
        assert!(r.get("ok").is_none(), "{op}: {r}");
    }
    for op in ["chat", "messages", "send", "chat_send", "topics"] {
        assert_eq!(ask(json!({"v":1,"op":op,"folder":ch.id}))["err"]["kind"], "bad_request", "{op}");
    }
    c.sync_host_stop();
    assert!(!Path::new(&socket).exists());
}

/// Tests, QA and other isolated runs never serve the extension (nor register a domain).
#[test]
fn an_isolated_run_does_not_serve() {
    let d = tempfile::tempdir().unwrap();
    let c = crate::Core::new(crate::CoreOptions {
        config_dir: d.path().join("state").display().to_string(),
        mock: true,
        allow_loopback_trackers: false,
        forbidden_root: Some(d.path().display().to_string()),
    })
    .unwrap();
    assert!(c.sync_host_start(d.path().join("g").display().to_string(), Arc::new(Told::default())).is_err());
    assert!(!d.path().join("g").exists());
}

/// A folder whose files are held here (switched to on demand) shows them downloaded: the host asks the system for them
/// in batches, and their fetch hands over clones, never downloads, and never counts as the system letting go.
#[test]
fn files_held_here_are_shown_downloaded() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    {
        let mut items = fake.items.lock().unwrap();
        items.get_mut("a").unwrap().state = SyncItemState::ModifiedHere;
        for n in 0..(MATERIALIZE_BATCH + 3) {
            let i = item(&format!("f{n}"), "root", &format!("f{n}.txt"), SyncItemKind::File, SyncItemState::Downloaded);
            items.insert(i.id.clone(), i);
        }
        // Online only (not held) and unavailable files are not asked for.
        items.insert("o".into(), item("o", "root", "o.txt", SyncItemKind::File, SyncItemState::OnlineOnly));
    }
    let h = host(&fake, d.path());
    // b is on disk already: only the others are asked for.
    assert_eq!(h.queue_materialize("F", vec!["root".into(), "b".into()]).unwrap(), MATERIALIZE_BATCH as u64 + 4);
    let first = h.next_batch("F").unwrap();
    assert_eq!((first.ids.len(), first.left, first.pause_ms), (MATERIALIZE_BATCH, 4, MATERIALIZE_PAUSE.as_millis() as u64));
    assert!(!first.ids.iter().any(|i| i == "b" || i == "o" || i == "u" || i == "d" || i == "root"), "{:?}", first.ids);
    let second = h.next_batch("F").unwrap();
    assert_eq!((second.ids.len(), second.left), (4, 0));
    assert!(h.next_batch("F").is_none(), "done");

    // The system fetches one: a clone of what is here.
    let asked = first.ids[0].clone();
    std::fs::write(fake.cache.join(&asked), b"held!").unwrap();
    fake.fetch_rounds.store(1, Ordering::Relaxed);
    let lines = ask(&h, json!({"v":1,"op":"fetch","folder":"F","item":asked}));
    assert_eq!(std::fs::read(lines.last().unwrap()["ok"]["path"].as_str().unwrap()).unwrap(), b"held!");
    // One that went meanwhile (freed): not downloaded for this, the core is never asked.
    let gone = first.ids[1].clone();
    fake.items.lock().unwrap().get_mut(&gone).unwrap().state = SyncItemState::OnlineOnly;
    let before = fake.fetched.load(Ordering::Relaxed);
    assert_eq!(err(&h, json!({"v":1,"op":"fetch","folder":"F","item":gone})).0, "unreachable");
    assert_eq!(fake.fetched.load(Ordering::Relaxed), before);
    // Opened by the user later: an ordinary fetch.
    std::fs::write(fake.cache.join(&gone), b"fetched").unwrap();
    let lines = ask(&h, json!({"v":1,"op":"fetch","folder":"F","item":gone}));
    assert!(lines.last().unwrap().get("ok").is_some(), "{lines:?}");

    // The set the host read is the baseline: the files arriving on disk are no release, nothing is evicted.
    let mut now = vec!["root".to_string(), "b".into()];
    now.extend(first.ids.iter().cloned());
    assert_eq!(ok(&h, json!({"v":1,"op":"materialized","folder":"F","ids":now}))["released"], 0);
    std::thread::sleep(Duration::from_millis(50));
    assert!(fake.evicted.lock().unwrap().is_empty());
}

/// Content comes only from the extension's uploads: a plain name in `u/`, a regular file of this user's with one link,
/// read from the file opened then (never a path that could be swapped meanwhile). Anything else is refused and nothing
/// is written: no client makes the app copy a file of its choosing into a synced folder.
#[test]
fn content_comes_only_from_the_uploads_folder() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let u = d.path().join(UPLOADS);
    std::fs::create_dir_all(&u).unwrap();
    let secret = d.path().join("secret");
    std::fs::write(&secret, b"key").unwrap();
    std::os::unix::fs::symlink(&secret, u.join("link")).unwrap();
    std::fs::hard_link(&secret, u.join("hard")).unwrap();
    std::fs::create_dir(u.join("dir")).unwrap();
    let outside = secret.display().to_string();
    for bad in [outside.as_str(), "../secret", "u/x", ".", "..", "", "link", "hard", "dir", "missing"] {
        let req = json!({"v":1,"op":"create","folder":"F","parent":"root","name":"n.txt","kind":"file","contents":bad});
        assert_eq!(err(&h, req).0, "bad_request", "{bad}");
        let req = json!({"v":1,"op":"modify","folder":"F","item":"b","contents":bad,"base":"c-b"});
        assert_eq!(err(&h, req).0, "bad_request", "{bad}");
    }
    // A new file needs content.
    assert_eq!(err(&h, json!({"v":1,"op":"create","folder":"F","parent":"root","name":"n.txt","kind":"file"})).0, "bad_request");
    assert!(fake.uploaded.lock().unwrap().is_empty(), "nothing was copied");
    std::fs::write(u.join("good"), b"mine").unwrap();
    ok(&h, json!({"v":1,"op":"create","folder":"F","parent":"root","name":"n.txt","kind":"file","contents":"good"}));
    assert_eq!(*fake.uploaded.lock().unwrap(), [b"mine".to_vec()]);
}

/// Only the app's own extension is served: the host checks the peer's code signature (from its audit token) before
/// anything is read; any other process gets a refusal and the core is never asked.
#[cfg(target_os = "macos")]
#[test]
fn only_the_extension_is_served() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let told = Arc::new(Told::default());
    told.refuse.store(true, Ordering::Relaxed);
    let s = Server::start(fake.clone(), d.path(), told.clone()).unwrap();
    let send = |req: &str| {
        let mut c = unix::connect(d.path(), SOCKET_NAME).unwrap();
        let _ = c.write_all(req.as_bytes());
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out
    };
    let refused = send("{\"v\":1,\"op\":\"delete\",\"folder\":\"F\",\"item\":\"b\",\"recursive\":true}\n");
    assert!(refused.contains("bad_request") && refused.contains("own extension"), "{refused}");
    assert!(fake.deleted.lock().unwrap().is_empty(), "the core was never asked");
    assert_eq!(told.tokens.lock().unwrap()[0].len(), 32, "an audit token");
    told.refuse.store(false, Ordering::Relaxed);
    assert!(send("{\"v\":1,\"op\":\"item\",\"folder\":\"F\",\"item\":\"b\"}\n").contains("b.txt"));
    drop(s);
}

/// `t/` and `u/` are never followed through a link: the sweep deletes nothing outside the container, and a fetch's
/// copy lands in a real `t/` inside it. Inside them, only regular files are swept.
#[test]
fn the_containers_folders_are_never_followed_through_a_link() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let outside = d.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), b"mine").unwrap();
    std::os::unix::fs::symlink(&outside, d.path().join(CLONES)).unwrap();
    std::os::unix::fs::symlink(&outside, d.path().join(UPLOADS)).unwrap();
    h.sweep(Duration::ZERO);
    assert!(outside.join("keep").exists(), "nothing outside is deleted");
    fake.fetch_rounds.store(1, Ordering::Relaxed);
    let got = ask(&h, json!({"v":1,"op":"fetch","folder":"F","item":"b"}));
    let path = PathBuf::from(got.last().unwrap()["ok"]["path"].as_str().unwrap());
    assert!(std::fs::symlink_metadata(d.path().join(CLONES)).unwrap().is_dir(), "a real folder now");
    assert_eq!(path.parent().unwrap(), d.path().join(CLONES));
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 1, "nothing written outside");
    // A link inside t/ to a file outside: the sweep removes regular files only.
    std::os::unix::fs::symlink(outside.join("keep"), d.path().join(CLONES).join("l")).unwrap();
    h.sweep(Duration::ZERO);
    assert!(!path.exists() && outside.join("keep").exists());
}

/// A client that sends a request slowly, or never reads its answer, or one of too many at once, holds nothing for
/// long: the request has one deadline in all, writes time out (and the fetch's request is taken back), and connections
/// beyond the cap are turned away at once.
#[cfg(target_os = "macos")]
#[test]
fn slow_or_greedy_clients_hold_nothing() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.fetch_rounds.store(u64::MAX, Ordering::Relaxed);
    let mut h = Host::new(fake.clone(), d.path());
    h.request_wait = Duration::from_millis(300);
    h.send_wait = Duration::from_millis(300);
    h.max_connections = 2;
    let s = Server::start_host(h, d.path(), Arc::new(Told::default())).unwrap();
    // A request trickled byte by byte, never finished: closed after its deadline.
    let mut slow = unix::connect(d.path(), SOCKET_NAME).unwrap();
    slow.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let t0 = Instant::now();
    for b in b"{\"v\":1" {
        let _ = slow.write_all(&[*b]);
        std::thread::sleep(Duration::from_millis(60));
    }
    let mut out = String::new();
    let _ = slow.read_to_string(&mut out);
    assert!(out.contains("bad_request") && t0.elapsed() < Duration::from_secs(3), "{out} after {:?}", t0.elapsed());
    // A fetch whose client never reads: its writes time out and the request is taken back.
    let mut deaf = unix::connect(d.path(), SOCKET_NAME).unwrap();
    deaf.write_all(b"{\"v\":1,\"op\":\"fetch\",\"folder\":\"F\",\"item\":\"a\"}\n").unwrap();
    let t0 = Instant::now();
    while fake.cancelled.lock().unwrap().is_empty() {
        assert!(t0.elapsed() < Duration::from_secs(20), "the fetch never gave up");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(deaf);
    // Two connections held open: a third is turned away at once.
    let held: Vec<_> = (0..2).map(|_| unix::connect(d.path(), SOCKET_NAME).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(50));
    let mut third = unix::connect(d.path(), SOCKET_NAME).unwrap();
    third.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut out = String::new();
    let _ = third.read_to_string(&mut out);
    assert!(out.contains("unreachable"), "{out}");
    drop(held);
    drop(s);
}

/// Requests are small, except the system's set of items on disk, which comes in parts; a report older than the last
/// one taken is ignored.
#[test]
fn big_reports_come_in_parts_and_stale_ones_are_ignored() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let big = "x".repeat(REQUEST_MAX as usize + 10);
    assert_eq!(err(&h, json!({"v":1,"op":"item","folder":"F","item":big})).0, "bad_request");
    let part = |report: u64, part: u32, parts: u32, ids: Value| {
        ok(&h, json!({"v":1,"op":"materialized","folder":"F","report":report,"part":part,"parts":parts,"ids":ids}))["released"].as_u64().unwrap()
    };
    // Report 5 in two parts, the second first: taken once both are here.
    assert_eq!(part(5, 1, 2, json!(["b"])), 0);
    assert_eq!(part(5, 0, 2, json!(["root", "d"])), 0);
    // An older report arriving late is ignored (b would look released).
    assert_eq!(part(3, 0, 1, json!(["root"])), 0);
    // A report whose first part never came (the app started again in the middle) is never taken.
    assert_eq!(part(6, 1, 2, json!(["root"])), 0);
    // The next whole one: b left.
    assert_eq!(part(7, 0, 1, json!(["root", "d"])), 1);
    // A part of report 6 coming now is stale.
    assert_eq!(part(6, 0, 2, json!(["root"])), 0);
}

/// Long work keeps the connection alive: the extension waits for a line at most a minute, and a delegated rename may
/// take longer.
#[test]
fn long_work_keeps_alive() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.modify_ms.store(400, Ordering::Relaxed);
    let mut h = host(&fake, d.path());
    h.keepalive = Duration::from_millis(50);
    let lines = ask(&h, json!({"v":1,"op":"modify","folder":"F","item":"a","name":"z.txt"}));
    assert!(lines.iter().filter(|l| l.get("alive").is_some()).count() >= 3, "{lines:?}");
    assert_eq!(lines.last().unwrap()["ok"]["item"]["name"], "z.txt");
}

/// Changes that look alike are each made: two new folders of one name, a rename back and forth, a new file moved out
/// and back. (An identity the extension once sent with them, the same for each, is ignored: no answer is replayed.)
#[test]
fn changes_that_look_alike_are_each_made() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let new = json!({"v":1,"op":"create","folder":"F","parent":"root","name":"untitled folder","kind":"dir","request":"same"});
    let one = ok(&h, new.clone())["item"]["id"].as_str().unwrap().to_string();
    let two = ok(&h, new)["item"]["id"].as_str().unwrap().to_string();
    assert_ne!(one, two, "two folders");
    for (name, key) in [("B", "kB"), ("A", "kA"), ("B", "kB")] {
        ok(&h, json!({"v":1,"op":"modify","folder":"F","item":"a","name":name,"request":key}));
    }
    assert_eq!((fake.get("a").unwrap().name.as_str(), fake.modifies.load(Ordering::SeqCst)), ("B", 3));
    for (parent, key) in [("root", "kOut"), ("d", "kIn"), ("root", "kOut")] {
        ok(&h, json!({"v":1,"op":"modify","folder":"F","item":"a","parent":parent,"request":key}));
    }
    assert_eq!((fake.get("a").unwrap().parent_id.as_str(), fake.modifies.load(Ordering::SeqCst)), ("root", 6));
}

/// Nothing is reported while folder sync is not running (at a start, or while its port changes), and a folder being
/// let go of is not reported until that is over; the host never removes a domain because a list lacks it.
#[test]
fn nothing_is_reported_while_sync_is_not_running() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.stopped.store(true, Ordering::Relaxed);
    let h = host(&fake, d.path());
    let told = Told::default();
    let (mut anchors, mut shown) = (HashMap::new(), None);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(told.calls.load(Ordering::Relaxed), 0, "nothing while not running");
    fake.stopped.store(false, Ordering::Relaxed);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(told.domains.lock().unwrap().len(), 1);
    // Being let go of: left out until settled, then reported again.
    lock(&h.releasing).insert("F".into());
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(told.domains.lock().unwrap().last().unwrap(), &Vec::<SyncHostDomain>::new());
    lock(&h.releasing).remove("F");
    h.force_report.store(true, Ordering::Relaxed);
    h.watch_once(&told, &mut anchors, &mut shown);
    assert_eq!(told.domains.lock().unwrap().last().unwrap().len(), 1);
}

/// What the system kept of a removed domain (changes it had not handed over) is never thrown away: it goes next to
/// the folder's place under a name the user finds, and the user is told where.
#[test]
fn what_the_system_kept_goes_next_to_the_folder() {
    let d = tempfile::tempdir().unwrap();
    let kept = d.path().join("kept");
    std::fs::create_dir(&kept).unwrap();
    std::fs::write(kept.join("draft.txt"), b"unsynced").unwrap();
    std::fs::create_dir(d.path().join("Team (changes not synced)")).unwrap();
    let n = keep_preserved("Team", Some(&d.path().join("Team")), &kept).unwrap();
    let to = d.path().join("Team (changes not synced) 2");
    assert_eq!(std::fs::read(to.join("draft.txt")).unwrap(), b"unsynced");
    assert!(!kept.exists());
    assert!(n.title.contains("Team") && n.text.contains(&to.display().to_string()), "{n:?}");
    // Nothing kept, nothing to say.
    assert!(keep_preserved("Team", None, &d.path().join("none")).is_none());
}

/// Removing or switching back an on-demand folder through the real core asks the host to remove its domain first;
/// what the system kept is put next to the place; a host that cannot refuses it, and the folder stays.
#[test]
fn the_core_asks_the_host_to_let_go_first() {
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
    let told = Arc::new(Told::default());
    c.sync_host_start(work.path().join("group").display().to_string(), told.clone()).unwrap();
    // The host cannot: nothing changes.
    told.refuse_removal.store(true, Ordering::Relaxed);
    assert!(c.sync_remove(f.id.clone(), false).is_err());
    assert!(c.sync_snapshot().folders.iter().any(|x| x.id == f.id && x.on_demand));
    // What the system keeps of a domain is put next to the place (asked by the host, also late).
    let kept = work.path().join("kept");
    std::fs::create_dir(&kept).unwrap();
    std::fs::write(kept.join("b.txt"), b"unsynced").unwrap();
    let n = c.sync_host_keep_preserved(f.id.clone(), "Docs".into(), kept.display().to_string()).unwrap();
    assert_eq!(std::fs::read(work.path().join("Docs (changes not synced)/b.txt")).unwrap(), b"unsynced");
    assert!(n.text.contains("Docs (changes not synced)"));
    // It can: the switch goes on.
    told.refuse_removal.store(false, Ordering::Relaxed);
    c.sync_set_on_demand(f.id.clone(), false, None).unwrap();
    assert_eq!(*told.removed.lock().unwrap(), [f.id.clone(), f.id.clone()]);
    assert_eq!(std::fs::read(docs.join("a.txt")).unwrap(), b"hello", "full again, at its place");
    // A removal of a domain the app did not finish is told at the next start, once.
    c.sync_host_release_begin(f.id.clone(), "Docs".into());
    c.sync_host_release_begin("other".into(), "Other".into());
    c.sync_host_release_done("other".into());
    let unfinished = c.sync_host_unfinished_releases();
    assert_eq!(unfinished.len(), 1);
    assert!(unfinished[0].title.contains("Docs") && unfinished[0].text.contains("CloudStorage"), "{unfinished:?}");
    assert!(c.sync_host_unfinished_releases().is_empty());
    assert_eq!(c.sync_host_folder_state(f.id.clone()), SyncHostFolderState::Full);
    // A full folder's removal asks nothing.
    c.sync_remove(f.id.clone(), false).unwrap();
    assert_eq!(c.sync_host_folder_state(f.id.clone()), SyncHostFolderState::Removed);
    assert_eq!(told.removed.lock().unwrap().len(), 2);
    c.sync_host_stop();
}

/// A folder's version changes whenever anything below it does, kept up from the change feed (listing a folder
/// never walks what is below its folders); a change below reports the folders above it again, and a delete with an
/// older version is refused by the core (the folder comes back).
#[test]
fn a_folders_version_follows_what_is_below_and_guards_its_delete() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.anchor.store(5, Ordering::Relaxed);
    let h = host(&fake, d.path());
    assert_eq!(ok(&h, json!({"v":1,"op":"item","folder":"F","item":"d"}))["content_version"], "d5");
    // Listing again, nothing changed: no change page asked, nothing walked.
    let pages = fake.pages.load(Ordering::SeqCst);
    ok(&h, json!({"v":1,"op":"children","folder":"F","item":"root"}));
    ok(&h, json!({"v":1,"op":"children","folder":"F","item":"root"}));
    assert_eq!(fake.pages.load(Ordering::SeqCst), pages);
    // Something below d changed (a, at 7… up to 9): d's version moves with it, and only the feed is read.
    fake.anchor.store(9, Ordering::Relaxed);
    let pages = fake.pages.load(Ordering::SeqCst);
    let ch = ok(&h, json!({"v":1,"op":"changes","folder":"F","anchor":"7"}));
    let got: Vec<(&str, &str)> = ch["items"].as_array().unwrap().iter().map(|i| (i["id"].as_str().unwrap(), i["content_version"].as_str().unwrap())).collect();
    assert_eq!(got[0], ("d", "d9"), "the folder above first, with its new version: {got:?}");
    assert_eq!(got[1].0, "a");
    assert!(fake.pages.load(Ordering::SeqCst) <= pages + 2, "the system's page, and the catch-up from 5");
    fake.newest.store(9, Ordering::Relaxed);
    let (kind, why) = err(&h, json!({"v":1,"op":"delete","folder":"F","item":"d","base":"d5","recursive":true}));
    assert_eq!(kind, "deletion_rejected", "{why}");
    assert!(fake.deleted.lock().unwrap().is_empty(), "nothing deleted");
    ask(&h, json!({"v":1,"op":"delete","folder":"F","item":"d","base":"d9","recursive":true}));
    assert_eq!(*fake.deleted.lock().unwrap(), ["d"], "seen as it is now: the core deletes it");
}

/// Age is when a file came into `t/` or `u/` (its change time), not its modification time, which a clone or copy
/// keeps: a file just made from an old one stays.
#[test]
fn a_fresh_copy_of_an_old_file_is_not_swept() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    std::fs::create_dir(d.path().join(UPLOADS)).unwrap();
    let up = d.path().join(UPLOADS).join("x");
    let f = std::fs::File::create(&up).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(7200)).unwrap();
    drop(f);
    h.sweep(Duration::from_secs(3600));
    assert!(up.exists(), "just made");
}

/// A domain's folder is judged by the configuration: one that could not open (missing from the running folders) is
/// still on demand, and only a folder the configuration lacks is removed.
#[test]
fn a_folder_is_judged_by_the_configuration() {
    assert_eq!(folder_state(None), SyncHostFolderState::NotRunning);
    assert_eq!(folder_state(Some(Some(true))), SyncHostFolderState::OnDemand);
    assert_eq!(folder_state(Some(Some(false))), SyncHostFolderState::Full);
    assert_eq!(folder_state(Some(None)), SyncHostFolderState::Removed);
}

/// Through the real core: deleting a file in a folder moves the folder's version (the system is told); deleting the
/// folder after it is done with that version, and with the one from before too: this Mac's own change below (here
/// the delete of a file first, as `rm -rf` does) refuses nothing.
#[test]
fn a_folder_emptied_here_is_deleted_with_its_new_version() {
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
    std::fs::create_dir_all(docs.join("D")).unwrap();
    std::fs::write(docs.join("D/x.txt"), b"x").unwrap();
    std::fs::write(docs.join("D/y.txt"), b"y").unwrap();
    let f = c.sync_add_folder_on_demand(docs.display().to_string()).unwrap();
    let h = Host::new(Arc::new(BackendCore(c.backend.sync())), &work.path().join("group"));
    let t0 = Instant::now();
    let dir = loop {
        let root = ok(&h, json!({"v":1,"op":"children","folder":f.id,"item":"root"}));
        let kids = ok(&h, json!({"v":1,"op":"children","folder":f.id,"item":root["items"].get(0).map_or(json!("root"), |i| i["id"].clone())}));
        if let Some(d) = root["items"].as_array().unwrap().iter().find(|i| i["name"] == "D")
            && kids["items"].as_array().is_some_and(|k| k.len() == 2)
        {
            break d.clone();
        }
        assert!(t0.elapsed().as_secs() < 10, "the folder's files are listed");
        std::thread::sleep(Duration::from_millis(50));
    };
    let before = dir["content_version"].as_str().unwrap().to_string();
    let kids = ok(&h, json!({"v":1,"op":"children","folder":f.id,"item":dir["id"]}));
    let x = kids["items"].as_array().unwrap().iter().find(|i| i["name"] == "x.txt").unwrap().clone();
    let anchor = ok(&h, json!({"v":1,"op":"anchor","folder":f.id}))["anchor"].clone();
    ok(&h, json!({"v":1,"op":"delete","folder":f.id,"item":x["id"],"base":x["content_version"],"recursive":true}));
    // What the system reads next: D again, with a new version.
    let ch = ok(&h, json!({"v":1,"op":"changes","folder":f.id,"anchor":anchor}));
    let now = ch["items"].as_array().unwrap().iter().find(|i| i["id"] == dir["id"]).expect("D reported").clone();
    assert_ne!(now["content_version"].as_str().unwrap(), before);
    ok(&h, json!({"v":1,"op":"delete","folder":f.id,"item":dir["id"],"base":before,"recursive":true}));
}

/// A named pipe in `u/` is no upload: refused at once (opening it never waits for a writer).
#[test]
fn a_pipe_in_the_uploads_is_refused_at_once() {
    unsafe extern "C" {
        fn mkfifo(path: *const std::ffi::c_char, mode: u16) -> i32;
    }
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    std::fs::create_dir(d.path().join(UPLOADS)).unwrap();
    let p = std::ffi::CString::new(d.path().join(UPLOADS).join("pipe").display().to_string()).unwrap();
    // SAFETY: a NUL-terminated path.
    assert_eq!(unsafe { mkfifo(p.as_ptr(), 0o600) }, 0);
    let t0 = Instant::now();
    let req = json!({"v":1,"op":"create","folder":"F","parent":"root","name":"n.txt","kind":"file","contents":"pipe"});
    assert_eq!(err(&h, req).0, "bad_request");
    assert!(t0.elapsed() < Duration::from_secs(2));
}

/// Only a report of the system's set may be longer than an ordinary request, and the server knows it is one before
/// it reads further (its op comes first, as the extension writes it).
#[test]
fn only_a_report_may_be_long() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    let h = host(&fake, d.path());
    let ids: Vec<String> = (0..5000).map(|n| format!("item-{n:020}")).collect();
    let line = format!(r#"{{"v":1,"op":"materialized","folder":"F","report":1,"part":0,"parts":1,"ids":{}}}"#, serde_json::to_string(&ids).unwrap());
    assert!(line.len() as u64 > REQUEST_MAX);
    let (mut client, mut server) = UnixStream::pair().unwrap();
    let mut w = client.try_clone().unwrap();
    let writer = std::thread::spawn(move || {
        let _ = w.write_all(format!("{line}\n").as_bytes());
    });
    serve(&h, &mut server);
    drop(server);
    let mut out = String::new();
    client.read_to_string(&mut out).unwrap();
    writer.join().unwrap();
    assert!(out.contains("\"released\""), "{out}");
    // Another op as long is refused once it is past the limit.
    let big = "x".repeat(REQUEST_MAX as usize + 10);
    assert_eq!(err(&h, json!({"v":1,"op":"item","folder":"F","item":big})).0, "bad_request");
}

/// What the system kept goes across volumes too: copied, checked, then removed (never removed unless whole).
#[test]
fn what_was_kept_is_copied_when_it_cannot_be_renamed() {
    let d = tempfile::tempdir().unwrap();
    let from = d.path().join("kept");
    std::fs::create_dir_all(from.join("sub")).unwrap();
    std::fs::write(from.join("sub/a.txt"), b"alpha").unwrap();
    std::fs::write(from.join("b.txt"), b"bravo").unwrap();
    std::os::unix::fs::symlink("b.txt", from.join("link")).unwrap();
    let to = d.path().join("there");
    copy_verified(&from, &to).unwrap();
    assert!(!from.exists(), "removed once the copy is whole");
    assert_eq!(std::fs::read(to.join("sub/a.txt")).unwrap(), b"alpha");
    assert_eq!(std::fs::read_link(to.join("link")).unwrap(), PathBuf::from("b.txt"));
}

/// A folder's delete needs a version of this index: none, one that does not read, or one from before the index was
/// made anew, is refused (the folder comes back with a fresh version), never a delete unchecked.
#[test]
fn a_folder_delete_without_a_version_of_this_index_is_refused() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.anchor.store(5, Ordering::Relaxed);
    fake.newest.store(1, Ordering::Relaxed);
    let h = host(&fake, d.path());
    let v = ok(&h, json!({"v":1,"op":"item","folder":"F","item":"d"}))["content_version"].as_str().unwrap().to_string();
    let other_index = format!("d{}", (7u64 << 48) | 5);
    for base in [json!(null), json!("x"), json!("d"), json!(other_index)] {
        let (kind, _) = err(&h, json!({"v":1,"op":"delete","folder":"F","item":"d","base":base,"recursive":true}));
        assert_eq!(kind, "deletion_rejected", "{base}");
    }
    assert!(fake.deleted.lock().unwrap().is_empty());
    ask(&h, json!({"v":1,"op":"delete","folder":"F","item":"d","base":v,"recursive":true}));
    assert_eq!(*fake.deleted.lock().unwrap(), ["d"]);
}

/// A folder is never served without a version, also when the feed could not be read at the first look.
#[test]
fn a_folder_always_has_a_version() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    fake.no_anchor.store(true, Ordering::Relaxed);
    let h = host(&fake, d.path());
    let v = ok(&h, json!({"v":1,"op":"item","folder":"F","item":"d"}))["content_version"].as_str().unwrap().to_string();
    assert!(v.starts_with('d'), "{v}");
}

/// Two whole reports on two threads at once: the older one is never taken after the newer (it would make the newer's
/// files look let go of, and free them).
#[test]
fn an_older_report_never_overtakes_a_newer_one() {
    let d = tempfile::tempdir().unwrap();
    let fake = Fake::new(&d.path().join("cache"));
    for _ in 0..300 {
        let h = Arc::new(host(&fake, d.path()));
        let report = |n: u64, ids: &[&str]| Request {
            v: 1,
            op: "materialized".into(),
            folder: "F".into(),
            ids: Some(ids.iter().map(|s| s.to_string()).collect()),
            report: Some(n),
            part: Some(0),
            parts: Some(1),
            ..Request::default()
        };
        h.report("F", &report(1, &["root", "d"]));
        let go = Arc::new(std::sync::Barrier::new(2));
        let (h2, g2) = (h.clone(), go.clone());
        let newer = std::thread::spawn(move || {
            g2.wait();
            h2.report("F", &report(3, &["root", "d", "b"]));
        });
        go.wait();
        h.report("F", &report(2, &["root"]));
        newer.join().unwrap();
        let now: std::collections::BTreeSet<String> = lock(&h.materialized)["F"].iter().cloned().collect();
        assert_eq!(now, ["b", "d", "root"].iter().map(|s| s.to_string()).collect::<std::collections::BTreeSet<_>>(), "the newest report's set");
    }
}

/// Kept changes copied across volumes keep their extended attributes (Finder tags), and once the copy is whole it is
/// the one reported, even when the original cannot be removed all the way.
#[cfg(target_os = "macos")]
#[test]
fn what_was_kept_keeps_its_tags_and_the_whole_copy_is_reported() {
    use std::os::unix::fs::PermissionsExt;
    unsafe extern "C" {
        fn setxattr(path: *const std::ffi::c_char, name: *const std::ffi::c_char, value: *const std::ffi::c_void, size: usize, position: u32, options: i32) -> i32;
        fn getxattr(path: *const std::ffi::c_char, name: *const std::ffi::c_char, value: *mut std::ffi::c_void, size: usize, position: u32, options: i32) -> isize;
    }
    let d = tempfile::tempdir().unwrap();
    let from = d.path().join("kept");
    std::fs::create_dir_all(from.join("locked")).unwrap();
    std::fs::write(from.join("tagged.txt"), b"t").unwrap();
    std::fs::write(from.join("locked/x.txt"), b"x").unwrap();
    let name = std::ffi::CString::new("com.apple.metadata:_kMDItemUserTags").unwrap();
    let file = std::ffi::CString::new(from.join("tagged.txt").display().to_string()).unwrap();
    let tag = b"red";
    // SAFETY: NUL-terminated strings and a buffer of the size given.
    assert_eq!(unsafe { setxattr(file.as_ptr(), name.as_ptr(), tag.as_ptr().cast(), tag.len(), 0, 0) }, 0);
    // The original cannot be removed all the way (a folder in it is read-only).
    std::fs::set_permissions(from.join("locked"), std::fs::Permissions::from_mode(0o555)).unwrap();
    let to = d.path().join("there");
    let r = copy_verified(&from, &to);
    std::fs::set_permissions(from.join("locked"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(r.is_ok(), "the copy is whole: {r:?}");
    let copied = std::ffi::CString::new(to.join("tagged.txt").display().to_string()).unwrap();
    let mut buf = [0u8; 16];
    // SAFETY: as above.
    let n = unsafe { getxattr(copied.as_ptr(), name.as_ptr(), buf.as_mut_ptr().cast(), buf.len(), 0, 0) };
    assert_eq!(&buf[..usize::try_from(n).unwrap_or(0)], tag, "the tag came along");
    assert_eq!(std::fs::read(to.join("locked/x.txt")).unwrap(), b"x");
}

/// `t/` and `u/` are the user's alone: one there already, made with looser permissions, is tightened to 0700.
#[test]
fn the_containers_folders_are_the_users_alone() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let u = d.path().join(UPLOADS);
    std::fs::create_dir(&u).unwrap();
    std::fs::set_permissions(&u, std::fs::Permissions::from_mode(0o755)).unwrap();
    fsx::own_dir(&u).unwrap();
    assert_eq!(std::fs::metadata(&u).unwrap().permissions().mode() & 0o777, 0o700);
}
