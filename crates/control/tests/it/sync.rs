//! The `sync.*` methods against a real folder sync service (loopback, temp dirs only): permissions,
//! folder selection, the two-step removal (and the Trash), settings, and that keys never reach the
//! activity log.

use crate::common;

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::Arc;

use common::*;
use serde_json::{Value, json};
use trs_control::client::ClientError;
use trs_control::proto::*;
use trs_control::types::*;
use trs_control::{Permission, ServerOptions};
use trs_sync_net::SyncHandle;
use trs_sync_net::secrets::FileSecrets;

fn api(e: ClientError) -> ApiError {
    match e {
        ClientError::Api(a) => a,
        other => panic!("expected an API error, got {other:?}"),
    }
}

async fn call(c: &mut trs_control::Client, method: &str, params: Value) -> Result<Value, ApiError> {
    let r = Request::decode(method, params).expect("params decode");
    c.call(&r).await.map_err(api)
}

struct SyncRig {
    rig: Rig,
    h: SyncHandle,
    root: tempfile::TempDir,
}

impl SyncRig {
    async fn new(level: Permission) -> SyncRig {
        let opts = ServerOptions { rate_per_sec: 500, burst: 500, ..ServerOptions::default() };
        let rig = Rig::with(level, opts, vec![]).await;
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let secrets = Arc::new(FileSecrets::new(state.join("secrets")));
        let h = trs_control::sync::start_sync_with(&state, "Seedwell test".into(), secrets, None, |o| {
            o.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
            o.lan_discovery = false;
            o.default_device_name = "Test Mac".into();
        })
        .await
        .unwrap();
        *rig.mock.sync.lock().unwrap() = Some(h.clone());
        *rig.mock.trash.lock().unwrap() = Some(root.path().join("Trash"));
        SyncRig { rig, h, root }
    }

    fn folder(&self, name: &str) -> std::path::PathBuf {
        let p = self.root.path().join(name);
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}

fn text(p: &Path) -> String {
    p.display().to_string()
}

async fn activity(rig: &Rig) -> String {
    // The log is read at `full`; the rig's level may be lower, so read it from the handle.
    serde_json::to_string(&rig.server.activity().recent(1000)).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_the_service_every_sync_method_says_so() {
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    for (m, p) in [("sync.list", json!({})), ("sync.status", json!({"folder": "x"})), ("sync.share", json!({"folder": "x"}))] {
        let e = call(&mut c, m, p).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Unsupported, "{m}");
        assert!(e.message.contains("folder sync is not running"), "{m}: {}", e.message);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn permission_levels() {
    // Read-only: look, nothing else.
    let s = SyncRig::new(Permission::ReadOnly).await;
    let mut c = s.rig.client("ro").await;
    let l = call(&mut c, "sync.list", json!({})).await.unwrap();
    assert_eq!(l["device_name"], "Test Mac");
    assert_eq!(l["port"], s.h.port());
    let docs = s.folder("Docs");
    let e = call(&mut c, "sync.add", json!({"path": text(&docs)})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);

    // Control: add and change, but no keys and no Trash.
    let s = SyncRig::new(Permission::Control).await;
    let mut c = s.rig.client("ctl").await;
    let docs = s.folder("Docs");
    let f = call(&mut c, "sync.add", json!({"path": text(&docs)})).await.unwrap();
    assert_eq!((f["name"].as_str(), f["access"].as_str()), (Some("Docs"), Some("read_write")));
    let e = call(&mut c, "sync.share", json!({"folder": "Docs"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(e.message.contains("full"), "{}", e.message);
    let e = call(&mut c, "sync.remove", json!({"folder": "Docs", "delete_files": true})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied, "moving to the Trash needs full");
    // Moving files on disk and changing settings need full too (SPEC 9A.3); reading settings does not.
    for (m, p) in [
        ("sync.rename", json!({"folder": "Docs", "name": "Papers"})),
        ("sync.move", json!({"folder": "Docs", "path": text(&s.root.path().join("Moved"))})),
        ("sync.set_settings", json!({"device_name": "Studio"})),
    ] {
        assert_eq!(call(&mut c, m, p).await.unwrap_err().code, ErrorCode::PermissionDenied, "{m}");
    }
    assert!(docs.exists() && !s.root.path().join("Papers").exists() && !s.root.path().join("Moved").exists());
    assert_eq!(call(&mut c, "sync.settings", json!({})).await.unwrap()["device_name"], "");
    assert!(call(&mut c, "sync.remove", json!({"folder": "Docs"})).await.is_ok(), "stopping without the files is control");
    // Even at control, no folder in a system, Library or hidden place.
    let e = call(&mut c, "sync.add", json!({"path": "/System/Library"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    let e = call(&mut c, "sync.add", json!({"path": "relative/path"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn folders_are_added_named_changed_and_listed() {
    let s = SyncRig::new(Permission::Full).await;
    let mut c = s.rig.client("claude").await;
    let docs = s.folder("Docs");
    std::fs::write(docs.join("a.txt"), b"hello").unwrap();
    let f: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.add", json!({"path": text(&docs)})).await.unwrap()).unwrap();
    let group = f.id.split('-').next().unwrap().to_owned();
    // By name (any case), by the first group, by the whole id.
    for q in ["docs", group.as_str(), f.id.as_str()] {
        let st: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.status", json!({"folder": q})).await.unwrap()).unwrap();
        assert_eq!(st.id, f.id, "{q}");
    }
    let e = call(&mut c, "sync.status", json!({"folder": "Nothing"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);

    let p: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.pause", json!({"folder": "Docs"})).await.unwrap()).unwrap();
    assert_eq!(p.state, "paused");
    let r: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.resume", json!({"folder": "Docs"})).await.unwrap()).unwrap();
    assert_ne!(r.state, "paused");
    call(&mut c, "sync.rescan", json!({"folder": "Docs"})).await.unwrap();
    let l: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.set_lan_only", json!({"folder": "Docs", "lan_only": true})).await.unwrap()).unwrap();
    assert!(l.lan_only);
    let l: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.set_lan_only", json!({"folder": "Docs", "lan_only": false})).await.unwrap()).unwrap();
    assert!(!l.lan_only);
    let k: SyncConflictsDto = serde_json::from_value(call(&mut c, "sync.conflicts", json!({"folder": "Docs"})).await.unwrap()).unwrap();
    assert_eq!((k.name.as_str(), k.conflicts.len()), ("Docs", 0));
    let e = call(&mut c, "sync.resolve", json!({"folder": "Docs", "copy": "a.txt", "keep": "neither"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    call(&mut c, "sync.add_address", json!({"folder": "Docs", "address": "127.0.0.1:9"})).await.unwrap();
    let e = call(&mut c, "sync.add_address", json!({"folder": "Docs", "address": "no port"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);

    let r: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.rename", json!({"folder": "Docs", "name": "Papers"})).await.unwrap()).unwrap();
    assert_eq!(r.path, text(&s.root.path().join("Papers")));
    assert!(s.root.path().join("Papers/a.txt").exists() && !docs.exists());
    let to = s.root.path().join("elsewhere").join("Papers");
    let m: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.move", json!({"folder": group, "path": text(&to)})).await.unwrap()).unwrap();
    assert_eq!(m.path, text(&to));
    assert_eq!(std::fs::read(to.join("a.txt")).unwrap(), b"hello");

    let l: SyncListDto = serde_json::from_value(call(&mut c, "sync.list", json!({})).await.unwrap()).unwrap();
    assert_eq!(l.folders.len(), 1);
    assert_eq!(l.folders[0].name, "Papers");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_are_read_and_changed() {
    let s = SyncRig::new(Permission::Full).await;
    let mut c = s.rig.client("claude").await;
    let st: SyncSettingsDto = serde_json::from_value(call(&mut c, "sync.settings", json!({})).await.unwrap()).unwrap();
    assert!(st.lan_discovery && st.dht_discovery && st.changed.is_empty());
    let loc = s.folder("Joined");
    let st: SyncSettingsDto = serde_json::from_value(
        call(&mut c, "sync.set_settings", json!({"device_name": "Studio", "up_limit": 2_048_000, "lan_discovery": false, "default_location": text(&loc)})).await.unwrap(),
    )
    .unwrap();
    assert_eq!((st.device_name.as_str(), st.up_limit, st.lan_discovery), ("Studio", Some(2_048_000), false));
    assert_eq!(st.default_location, text(&loc));
    assert_eq!(st.changed, vec!["device_name", "lan_discovery", "up_limit", "default_location"]);
    assert_eq!(s.h.settings().device_name, "Studio");
    let e = call(&mut c, "sync.set_settings", json!({"port": 80})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    let e = call(&mut c, "sync.set_settings", json!({})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "a set with nothing to set");
    assert_eq!(call(&mut c, "sync.settings", json!({})).await.unwrap()["device_name"], "Studio");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_are_shared_at_full_and_never_logged() {
    let s = SyncRig::new(Permission::Full).await;
    let mut c = s.rig.client("claude").await;
    let docs = s.folder("Docs");
    call(&mut c, "sync.add", json!({"path": text(&docs)})).await.unwrap();
    let sh: SyncSharesDto = serde_json::from_value(call(&mut c, "sync.share", json!({"folder": "Docs"})).await.unwrap()).unwrap();
    let rw = sh.read_write.clone().unwrap();
    let ro = sh.read_only.clone().unwrap();
    assert!(rw.starts_with("SWW-") && ro.starts_with("SWR-"), "{rw} {ro}");
    assert!(sh.read_only_link.as_deref().unwrap().starts_with("seedwell://sync/SWR-"));

    // Joining another folder by link, at the default location (set into the temp dir).
    let joined = s.folder("JoinedHere");
    call(&mut c, "sync.set_settings", json!({"default_location": text(&joined)})).await.unwrap();
    let other = trs_sync_proto::keys::FolderKey::ReadWrite(trs_sync_proto::keys::FolderSecret::from_bytes([9; 32]));
    let link = trs_sync_proto::keys::Link { key: other.clone(), name: Some("Shared".into()) }.format();
    let f: SyncFolderDto = serde_json::from_value(call(&mut c, "sync.join", json!({"key_or_link": link})).await.unwrap()).unwrap();
    assert_eq!(f.path, text(&joined.join("Shared")));
    assert_eq!(f.id, other.folder_id().to_string());
    // A bad key is refused without being quoted back.
    let e = call(&mut c, "sync.join", json!({"key_or_link": "SWW-NOTAKEYATALL", "path": text(&s.folder("x"))})).await.unwrap_err();
    assert!(!e.message.contains("NOTAKEYATALL"), "{}", e.message);

    let log = activity(&s.rig).await;
    for secret in [rw.as_str(), ro.as_str(), &other.to_text(), &link, &rw[4..20], &other.to_text()[4..20]] {
        assert!(!log.contains(secret), "a key reached the activity log: {log}");
    }
    assert!(log.contains("sync.share") && log.contains("keys not logged"), "share is always logged: {log}");
    assert!(log.contains("sync.join") && log.contains("key not logged"), "{log}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removal_takes_two_steps_and_the_trash_is_the_configured_one() {
    let s = SyncRig::new(Permission::Full).await;
    let mut c = s.rig.client("claude").await;
    let keep = s.folder("Keep");
    std::fs::write(keep.join("k.txt"), b"k").unwrap();
    let gone = s.folder("Gone");
    std::fs::write(gone.join("g.txt"), b"g").unwrap();
    call(&mut c, "sync.add", json!({"path": text(&keep)})).await.unwrap();
    call(&mut c, "sync.add", json!({"path": text(&gone)})).await.unwrap();

    // Step 1 changes nothing; another client cannot use the token; a token is used once.
    let plan: SyncRemovePlanDto = serde_json::from_value(call(&mut c, "sync.remove", json!({"folder": "Keep"})).await.unwrap()).unwrap();
    assert!(!plan.delete_files && plan.summary.contains("stay"), "{}", plan.summary);
    assert_eq!(s.h.snapshot().folders.len(), 2);
    let mut other = s.rig.client("someone-else").await;
    let e = call(&mut other, "sync.remove_confirm", json!({"token": plan.token})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidToken);
    // A torrent removal token is not a folder token.
    let e = call(&mut c, "remove_confirm", json!({"token": plan.token})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidToken);
    let r: SyncRemovedDto = serde_json::from_value(call(&mut c, "sync.remove_confirm", json!({"token": plan.token})).await.unwrap()).unwrap();
    assert_eq!((r.files.as_str(), r.trashed_to), ("kept", None));
    assert!(keep.join("k.txt").exists(), "the files stay");
    assert_eq!(call(&mut c, "sync.remove_confirm", json!({"token": plan.token})).await.unwrap_err().code, ErrorCode::InvalidToken);

    // With delete_files the folder goes to the Trash (the configured one, never the real one in tests).
    let plan: SyncRemovePlanDto = serde_json::from_value(call(&mut c, "sync.remove", json!({"folder": "Gone", "delete_files": true})).await.unwrap()).unwrap();
    assert!(plan.delete_files && plan.summary.contains("TRASH"), "{}", plan.summary);
    assert!(gone.exists(), "nothing happens at step 1");
    let r: SyncRemovedDto = serde_json::from_value(call(&mut c, "sync.remove_confirm", json!({"token": plan.token})).await.unwrap()).unwrap();
    assert_eq!(r.files, "trashed");
    let trashed = s.root.path().join("Trash").join("Gone");
    assert_eq!(r.trashed_to.as_deref(), Some(text(&trashed).as_str()));
    assert!(!gone.exists());
    assert_eq!(std::fs::read(trashed.join("g.txt")).unwrap(), b"g");
    assert!(s.h.snapshot().folders.is_empty());
}
