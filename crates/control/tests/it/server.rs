//! The control server against an in-memory backend: handshake, permissions, remove tokens,
//! limits, abuse, concurrency, events, redaction, settings and statistics.

use crate::common;

use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use trs_control::client::ClientError;
use trs_control::proto::*;
use trs_control::types::*;
use trs_control::{Permission, ServerOptions};

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

#[tokio::test]
async fn handshake_status_list_info() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    assert_eq!(c.hello.protocol, PROTOCOL_VERSION);
    assert_eq!(c.hello.permission, "control");
    assert!(c.hello.methods.contains(&"add".to_owned()) && !c.hello.methods.contains(&"settings_set".to_owned()));

    let s: StatusDto = serde_json::from_value(call(&mut c, "status", json!({})).await.unwrap()).unwrap();
    assert_eq!(s.total, 2);
    assert_eq!(s.counts["downloading"], 1);
    assert_eq!(s.totals.down_rate, 1_200_000);
    assert!(s.summary.contains("1.2 MB/s"), "{}", s.summary);

    let l: ListDto = serde_json::from_value(call(&mut c, "list", json!({"filter": "seeding"})).await.unwrap()).unwrap();
    assert_eq!(l.total, 1);
    assert_eq!(l.transfers[0].name, "Beta");
    assert_eq!(l.transfers[0].human.progress, "100.0%");

    // Prefix ids work.
    let d: DetailDto = serde_json::from_value(call(&mut c, "info", json!({"id": "aaaaaaaa", "max_peers": 10})).await.unwrap()).unwrap();
    assert_eq!(d.transfer.name, "Alpha");
    assert_eq!(d.files.len(), 3);
    assert_eq!(d.peers.len(), 10);
    assert_eq!(d.peers_truncated, 140);
    let e = call(&mut c, "info", json!({"id": "dddddddd"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
}

#[tokio::test]
async fn secrets_never_leave() {
    let mut rows = vec![row(0xaa, "Alpha", "error")];
    rows[0].error = Some("announce https://t.example.org/a/0123456789abcdef0123456789abcdef/announce?passkey=SECRETKEY failed".into());
    rows[0].kind = "direct".into();
    rows[0].url = Some("https://dl.example.com/f.iso?token=SECRETKEY".into());
    let rig = Rig::with(Permission::Full, ServerOptions::default(), rows).await;
    let mut c = rig.client("claude").await;
    for (m, p) in [("list", json!({})), ("info", json!({"id": "aaaaaa"})), ("status", json!({})), ("reannounce", json!({"ids": ["aaaaaa"]}))] {
        let v = call(&mut c, m, p).await.unwrap().to_string();
        assert!(!v.contains("SECRETKEY") && !v.contains("0123456789abcdef"), "{m}: {v}");
    }
}

#[tokio::test]
async fn permission_matrix_every_method_every_level() {
    // Valid params for every method, so only the permission decides.
    let params = |m: Method| -> Value {
        match m {
            Method::Hello => json!({"client": "x"}),
            Method::Info => json!({"id": "aaaaaa"}),
            Method::Add => json!({"magnet": "magnet:?xt=urn:btih:abcdef"}),
            Method::Pause | Method::Resume => json!({"ids": ["aaaaaa"]}),
            Method::MoveToTop | Method::Reannounce | Method::Recheck => json!({"ids": ["aaaaaa"]}),
            Method::SetPriority => json!({"ids": ["aaaaaa"], "priority": "low"}),
            Method::SetFilePriorities => json!({"id": "aaaaaa", "set": [{"index": 0, "priority": "skip"}]}),
            Method::SetCategory => json!({"ids": ["aaaaaa"], "category": "tv"}),
            Method::SetSeedGoal => json!({"ids": ["aaaaaa"], "ratio": 2.0}),
            Method::SetLimits => json!({"down_limit": 1000}),
            Method::SetTrackerLimits => json!({"id": "nope", "down_limit": 1000}),
            Method::Remove => json!({"ids": ["aaaaaa"]}),
            Method::RemoveConfirm => json!({"token": "nope"}),
            Method::TrackerStats => json!({"tracker": "all"}),
            Method::SimAllow => json!({"ids": ["aa".repeat(20)], "allow": false}),
            Method::Choice | Method::CancelChoice | Method::DebugPeers => json!({"id": "aaaaaa"}),
            Method::ChooseFiles => json!({"id": "aaaaaa", "accept_default": true}),
            Method::Move => json!({"ids": ["aaaaaa"], "path": "/tmp/seedwell-test-move"}),
            Method::CancelMove => json!({"ids": ["aaaaaa"]}),
            Method::StreamUrl => json!({"id": "aaaaaa"}),
            Method::StopStream => json!({"token": "nope"}),
            Method::SettingsSet => json!({"changes": {"transfers.max_active_downloads": 4}}),
            Method::WaitFor => json!({"ids": ["bbbbbb"], "timeout_secs": 1}),
            _ => json!({}),
        }
    };
    for level in [Permission::ReadOnly, Permission::Control, Permission::Full] {
        // Every method in a row: more calls than the default burst allows.
        let opts = ServerOptions { rate_per_sec: 500, burst: 500, ..ServerOptions::default() };
        let rig = Rig::with(level, opts, vec![row(0xaa, "Alpha", "downloading"), row(0xbb, "Beta", "seeding")]).await;
        let mut c = rig.client("matrix").await;
        for m in Method::ALL {
            if *m == Method::Hello {
                continue;
            }
            let r = call(&mut c, m.name(), params(*m)).await;
            let allowed = level >= m.required();
            match (&r, allowed) {
                (Err(e), false) => assert_eq!(e.code, ErrorCode::PermissionDenied, "{} at {level:?}", m.name()),
                (Err(e), true) => assert_ne!(e.code, ErrorCode::PermissionDenied, "{} at {level:?} must be allowed: {e}", m.name()),
                (Ok(_), false) => panic!("{} succeeded at {level:?} but needs {:?}", m.name(), m.required()),
                (Ok(_), true) => {}
            }
        }
    }
}

#[tokio::test]
async fn permission_is_checked_before_params() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let mut r = Raw::connect(&rig.path()).await;
    assert!(r.hello("x").await.error.is_none());
    // Garbage params for a method the level may not use: denied, not "invalid params".
    r.send(b"{\"id\":2,\"method\":\"pause\",\"params\":{\"zzz\":1}}\n").await;
    assert_eq!(r.recv().await.unwrap().error.unwrap().code, ErrorCode::PermissionDenied);
    r.send(b"{\"id\":3,\"method\":\"settings_set\",\"params\":42}\n").await;
    assert_eq!(r.recv().await.unwrap().error.unwrap().code, ErrorCode::PermissionDenied);
    // And for a permitted method they are reported.
    r.send(b"{\"id\":4,\"method\":\"list\",\"params\":{\"nope\":1}}\n").await;
    assert_eq!(r.recv().await.unwrap().error.unwrap().code, ErrorCode::InvalidParams);
    // The permission error says what is needed.
    r.send(b"{\"id\":5,\"method\":\"pause\",\"params\":{\"ids\":[\"aaaaaa\"]}}\n").await;
    let e = r.recv().await.unwrap().error.unwrap();
    assert_eq!(e.data.unwrap()["required"], "control");
    assert!(rig.mock.calls().is_empty(), "nothing reached the backend");
}

#[tokio::test]
async fn off_refuses_connections_and_cuts_open_ones() {
    let rig = Rig::new(Permission::Off).await;
    let e = trs_control::Client::connect(rig.path(), "x").await.err().unwrap();
    assert_eq!(api(e).code, ErrorCode::AgentAccessOff);

    // A level lowered to off while connected: the next call is refused and the connection closes.
    rig.perm.set(Permission::Control);
    let mut c = rig.client("x").await;
    assert!(call(&mut c, "status", json!({})).await.is_ok());
    rig.perm.set(Permission::Off);
    assert_eq!(call(&mut c, "status", json!({})).await.unwrap_err().code, ErrorCode::AgentAccessOff);
    assert!(matches!(c.call(&Request::Ping(Empty {})).await, Err(ClientError::Io(_))));
}

#[tokio::test]
async fn level_changes_apply_to_open_connections() {
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("x").await;
    assert!(call(&mut c, "pause", json!({"ids": ["aaaaaa"]})).await.is_ok());
    rig.perm.set(Permission::ReadOnly);
    assert_eq!(call(&mut c, "resume", json!({"ids": ["aaaaaa"]})).await.unwrap_err().code, ErrorCode::PermissionDenied);
    assert!(call(&mut c, "list", json!({})).await.is_ok());
}

#[tokio::test]
async fn hello_is_required_first() {
    let rig = Rig::new(Permission::Full).await;
    let mut r = Raw::connect(&rig.path()).await;
    r.send(b"{\"id\":1,\"method\":\"list\"}\n").await;
    assert_eq!(r.recv().await.unwrap().error.unwrap().code, ErrorCode::HelloRequired);
    r.send(b"{\"id\":2,\"method\":\"hello\",\"params\":{\"client\":\"\"}}\n").await;
    assert_eq!(r.recv().await.unwrap().error.unwrap().code, ErrorCode::InvalidParams);
    assert!(r.hello("ok").await.error.is_none());
    r.send(b"{\"id\":3,\"method\":\"list\"}\n").await;
    assert!(r.recv().await.unwrap().result.is_some());
}

#[tokio::test]
async fn two_step_remove_happy_path_and_reuse() {
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    let plan: RemovePlanDto = serde_json::from_value(call(&mut c, "remove", json!({"ids": ["bbbbbb"], "delete_files": true})).await.unwrap()).unwrap();
    assert!(plan.delete_files && plan.items.len() == 1 && plan.items[0].name == "Beta");
    assert_eq!(plan.token.len(), 32);
    assert!(plan.summary.contains("DELETE"), "{}", plan.summary);
    assert!(!rig.mock.calls().iter().any(|c| c.starts_with("remove")), "step 1 removes nothing");
    assert_eq!(rig.mock.inner.lock().unwrap().rows.len(), 2);

    let done: RemovedDto = serde_json::from_value(call(&mut c, "remove_confirm", json!({"token": plan.token})).await.unwrap()).unwrap();
    assert!(done.files_deleted && done.removed.len() == 1);
    assert!(rig.mock.calls().contains(&"remove 1 delete=true".to_owned()));
    assert_eq!(rig.mock.inner.lock().unwrap().rows.len(), 1);

    // Single use.
    let e = call(&mut c, "remove_confirm", json!({"token": plan.token})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidToken);
}

#[tokio::test]
async fn remove_token_wrong_expired_and_foreign() {
    let opts = ServerOptions { confirm_ttl: Duration::from_millis(300), ..Default::default() };
    let rig = Rig::with(Permission::Full, opts, vec![row(0xaa, "Alpha", "downloading"), row(0xbb, "Beta", "seeding")]).await;
    let mut c = rig.client("claude").await;
    let plan: RemovePlanDto = serde_json::from_value(call(&mut c, "remove", json!({"ids": ["aaaaaa"]})).await.unwrap()).unwrap();
    assert!(!plan.delete_files);

    // Wrong token.
    assert_eq!(call(&mut c, "remove_confirm", json!({"token": "0".repeat(32)})).await.unwrap_err().code, ErrorCode::InvalidToken);
    // Another client name cannot use it.
    let mut other = rig.client("someone-else").await;
    assert_eq!(call(&mut other, "remove_confirm", json!({"token": plan.token})).await.unwrap_err().code, ErrorCode::InvalidToken);
    // Expiry.
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(call(&mut c, "remove_confirm", json!({"token": plan.token})).await.unwrap_err().code, ErrorCode::TokenExpired);
    assert!(rig.mock.calls().is_empty(), "nothing was removed");
    // The same client on another connection can redeem its own token.
    let plan2: RemovePlanDto = serde_json::from_value(call(&mut c, "remove", json!({"ids": ["aaaaaa"]})).await.unwrap()).unwrap();
    let mut c2 = rig.client("claude").await;
    assert!(call(&mut c2, "remove_confirm", json!({"token": plan2.token})).await.is_ok());
    assert!(rig.mock.calls().contains(&"remove 1 delete=false".to_owned()));
}

#[tokio::test]
async fn file_deletion_needs_full_at_both_steps() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    let e = call(&mut c, "remove", json!({"ids": ["aaaaaa"], "delete_files": true})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert_eq!(e.data.unwrap()["required"], "full");
    // Keep-files removal is fine at control.
    assert!(call(&mut c, "remove", json!({"ids": ["aaaaaa"]})).await.is_ok());

    // Planned at full, level dropped before confirming: refused, and the token is spent.
    rig.perm.set(Permission::Full);
    let plan: RemovePlanDto = serde_json::from_value(call(&mut c, "remove", json!({"ids": ["aaaaaa"], "delete_files": true})).await.unwrap()).unwrap();
    rig.perm.set(Permission::Control);
    let e = call(&mut c, "remove_confirm", json!({"token": plan.token})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(rig.mock.calls().is_empty());
    // Deleting files at the seeding goal is file deletion as well.
    let e = call(&mut c, "set_seed_goal", json!({"ids": ["aaaaaa"], "ratio": 1.0, "at_goal": "remove_and_delete"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(call(&mut c, "set_seed_goal", json!({"ids": ["aaaaaa"], "ratio": 1.0, "at_goal": "remove"})).await.is_ok());
    // Back to the rule's goal: its own call, alone.
    rig.mock.clear_calls();
    assert!(call(&mut c, "set_seed_goal", json!({"ids": ["aaaaaa"], "rule_default": true})).await.is_ok());
    assert_eq!(rig.mock.calls(), ["goal-clear 1"]);
    let e = call(&mut c, "set_seed_goal", json!({"ids": ["aaaaaa"], "rule_default": true, "ratio": 2.0})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
}

#[tokio::test]
async fn controls_reach_the_backend() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    let a: ActionDto = serde_json::from_value(call(&mut c, "pause", json!({"ids": ["aaaaaa", "bbbbbb"]})).await.unwrap()).unwrap();
    assert_eq!(a.count, 2);
    assert_eq!(call(&mut c, "pause", json!({"ids": ["aaaaaa"], "all": true})).await.unwrap_err().code, ErrorCode::InvalidParams);
    assert_eq!(call(&mut c, "pause", json!({})).await.unwrap_err().code, ErrorCode::InvalidParams);
    call(&mut c, "resume", json!({"all": true})).await.unwrap();
    call(&mut c, "set_priority", json!({"ids": ["aaaaaa"], "priority": "high"})).await.unwrap();
    call(&mut c, "set_category", json!({"ids": ["aaaaaa"], "category": "TV"})).await.unwrap();
    assert_eq!(call(&mut c, "set_category", json!({"ids": ["aaaaaa"], "category": "games"})).await.unwrap_err().code, ErrorCode::InvalidParams);
    call(&mut c, "set_file_priorities", json!({"id": "aaaaaa", "set": [{"index": 1, "priority": "skip"}]})).await.unwrap();
    assert_eq!(call(&mut c, "set_file_priorities", json!({"id": "aaaaaa", "set": [{"index": 9, "priority": "skip"}]})).await.unwrap_err().code, ErrorCode::InvalidParams);
    assert_eq!(call(&mut c, "set_file_priorities", json!({"id": "aaaaaa", "priorities": ["skip"]})).await.unwrap_err().code, ErrorCode::InvalidParams);
    call(&mut c, "recheck", json!({"ids": ["aaaaaa"]})).await.unwrap();
    let calls = rig.mock.calls();
    for want in ["pause 2", "resume_all", "priority 1 high", "category 1 Some(\"tv\")", "files normal,skip,normal", "recheck 1"] {
        assert!(calls.contains(&want.to_owned()), "{want} not in {calls:?}");
    }
    // Reannounce surfaces "not before".
    let r = call(&mut c, "reannounce", json!({"ids": ["aaaaaa"]})).await.unwrap();
    assert_eq!(r["transfers"][0]["trackers"][0]["not_before"], 2_000_000_000u64);
}

#[tokio::test]
async fn add_validation_and_sources() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    // A custom save folder needs `full`; at `control` downloads go where settings and rules say.
    let e = call(&mut c, "add", json!({"source": "magnet:?xt=urn:btih:abcdef", "save_path": "/tmp/x"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied, "{e}");
    let a: AddedDto = serde_json::from_value(call(&mut c, "add", json!({"source": "magnet:?xt=urn:btih:abcdef", "paused": true})).await.unwrap()).unwrap();
    assert_eq!(a.kind, "torrent");
    let a: AddedDto = serde_json::from_value(call(&mut c, "add", json!({"source": "https://dl.example.com/a.iso"})).await.unwrap()).unwrap();
    assert_eq!(a.kind, "direct");
    // base64 .torrent bytes ("d3:foo3:bare" is not a valid torrent but the mock accepts any bytes)
    let a: AddedDto = serde_json::from_value(call(&mut c, "add", json!({"torrent_base64": "ZDM6Zm9vMzpiYXJl"})).await.unwrap()).unwrap();
    assert!(a.name.contains("file-12-bytes"), "{}", a.name);
    // A .torrent path is read by the server.
    let f = rig.dir.path().join("a.torrent");
    std::fs::write(&f, b"0123456789").unwrap();
    let a: AddedDto = serde_json::from_value(call(&mut c, "add", json!({"torrent_path": f.to_str().unwrap()})).await.unwrap()).unwrap();
    assert!(a.name.contains("file-10-bytes"));
    for bad in [
        json!({}),
        json!({"magnet": "magnet:?xt=a", "url": "https://x/y"}),
        json!({"source": "relative.torrent"}),
        json!({"source": "ftp://x/y"}),
        json!({"torrent_path": "/nonexistent/file.torrent"}),
        json!({"torrent_path": "/tmp"}),
        json!({"magnet": "magnet:?xt=a", "category": "nope"}),
        json!({"magnet": "magnet:?xt=a", "sha256": "ab"}),
        json!({"magnet": "magnet:?xt=a", "rule": "no-such-rule"}),
        json!({"magnet": "magnet:?xt=a", "peers": ["not-an-addr"]}),
    ] {
        let e = call(&mut c, "add", bad.clone()).await.unwrap_err();
        assert!(matches!(e.code, ErrorCode::InvalidParams | ErrorCode::Invalid), "{bad}: {e}");
    }
}

#[tokio::test]
async fn a_favorites_shared_caps_are_set_and_saved_in_trackers_toml() {
    let rig = Rig::new(Permission::Control).await;
    std::fs::write(rig.state_dir().join("trackers.toml"), "version = 1\n\n[[tracker]]\nid = \"tl\"\nname = \"TL\"\nurls = [\"tracker.example.org\"]\n").unwrap();
    let mut c = rig.client("claude").await;
    let v = call(&mut c, "set_tracker_limits", json!({"id": "tl", "down_limit": 5_000_000, "up_limit": 250_000})).await.unwrap();
    assert_eq!((v["down_limit"].as_u64(), v["up_limit"].as_u64()), (Some(5_000_000), Some(250_000)));
    // Omitted fields stay, 0 clears.
    let v = call(&mut c, "set_tracker_limits", json!({"id": "tl", "down_limit": 0})).await.unwrap();
    assert_eq!((v["down_limit"].as_u64(), v["up_limit"].as_u64()), (None, Some(250_000)));
    let text = std::fs::read_to_string(rig.state_dir().join("trackers.toml")).unwrap();
    assert!(text.contains("up_limit = 250000") && !text.contains("down_limit"), "{text}");
    let t: TrackersDto = serde_json::from_value(call(&mut c, "trackers", json!({})).await.unwrap()).unwrap();
    assert_eq!((t.profiles[0].down_limit, t.profiles[0].up_limit), (None, Some(250_000)));
    // Only favorites have caps; nothing to change is an error; so is a limit out of range.
    assert_eq!(call(&mut c, "set_tracker_limits", json!({"id": "nope", "up_limit": 1})).await.unwrap_err().code, ErrorCode::InvalidParams);
    assert_eq!(call(&mut c, "set_tracker_limits", json!({"id": "tl"})).await.unwrap_err().code, ErrorCode::InvalidParams);
    assert_eq!(call(&mut c, "set_tracker_limits", json!({"id": "tl", "up_limit": u64::MAX})).await.unwrap_err().code, ErrorCode::InvalidParams);
}

#[tokio::test]
async fn limits_are_applied_and_persisted() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    let l: LimitsDto = serde_json::from_value(call(&mut c, "set_limits", json!({"down_limit": 5_000_000, "up_limit": 1_000_000, "alt_speed": true})).await.unwrap()).unwrap();
    assert_eq!((l.down_limit, l.up_limit, l.alt_active), (Some(5_000_000), Some(1_000_000), true));
    assert_eq!(call(&mut c, "set_limits", json!({})).await.unwrap_err().code, ErrorCode::InvalidParams);
    assert_eq!(call(&mut c, "set_limits", json!({"down_limit": u64::MAX})).await.unwrap_err().code, ErrorCode::InvalidParams);
    let cfg = trs_state::DocStore::<trs_state::Config>::new(rig.state_dir()).load().value;
    assert_eq!(cfg.transfers.download_limit, 5_000_000);
    assert!(cfg.transfers.alt.enabled);
    // Status shows them.
    let s: StatusDto = serde_json::from_value(call(&mut c, "status", json!({})).await.unwrap()).unwrap();
    assert_eq!(s.limits.down_limit, Some(5_000_000));
}

#[tokio::test]
async fn settings_get_set_full_only_and_validated() {
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    let g: SettingsDto = serde_json::from_value(call(&mut c, "settings_get", json!({"keys": ["transfers.max_active_downloads"]})).await.unwrap()).unwrap();
    assert_eq!(g.settings["transfers.max_active_downloads"], 3);
    let s: SettingsDto = serde_json::from_value(call(&mut c, "settings_set", json!({"changes": {"transfers.max_active_downloads": 7}})).await.unwrap()).unwrap();
    assert_eq!(s.changed, vec!["transfers.max_active_downloads"]);
    let cfg = trs_state::DocStore::<trs_state::Config>::new(rig.state_dir()).load().value;
    assert_eq!(cfg.transfers.max_active_downloads, 7);
    let e = call(&mut c, "settings_set", json!({"changes": {"transfers.max_active_downloads": 0}})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    assert!(e.message.contains("max_active_downloads"));
    let e = call(&mut c, "settings_set", json!({"changes": {"agents.permission": "full"}})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams);
    let cfg = trs_state::DocStore::<trs_state::Config>::new(rig.state_dir()).load().value;
    assert_eq!(cfg.agents.permission, trs_state::config::Permission::Off, "an agent cannot change its own permission");
}

#[tokio::test]
async fn activity_log_records_who_and_hides_secrets() {
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    call(&mut c, "list", json!({})).await.unwrap();
    call(&mut c, "add", json!({"magnet": "magnet:?xt=urn:btih:abcdef&dn=Thing&tr=https%3A%2F%2Ft.org%2Fannounce%3Fpasskey%3DSECRETKEY"})).await.unwrap();
    call(&mut c, "pause", json!({"ids": ["aaaaaa", "bbbbbb"]})).await.unwrap();
    let _ = call(&mut c, "set_category", json!({"ids": ["aaaaaa"], "category": "nope"})).await;
    rig.perm.set(Permission::ReadOnly);
    let _ = call(&mut c, "resume", json!({"ids": ["aaaaaa"]})).await;

    let log = rig.server.activity().recent(50);
    assert!(log.iter().all(|e| e.client == "claude"));
    assert!(!log.iter().any(|e| e.method == "list" || e.method == "hello"), "reads are not logged: {log:?}");
    let methods: Vec<_> = log.iter().rev().map(|e| (e.method.as_str(), e.outcome.as_str())).collect();
    assert_eq!(methods, vec![("add", "ok"), ("pause", "ok"), ("set_category", "error: invalid_params"), ("resume", "denied")]);
    let text = serde_json::to_string(&log).unwrap();
    assert!(!text.contains("SECRETKEY") && !text.contains("passkey"), "{text}");
    assert!(text.contains("pause 2 transfers") || text.contains("paused Alpha"), "{text}");

    rig.perm.set(Permission::Full);
    let via: Value = call(&mut c, "activity", json!({"limit": 2})).await.unwrap();
    assert_eq!(via["entries"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn events_push_poll_and_filter() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let mut c = rig.client("watcher").await;
    let sub = call(&mut c, "events_subscribe", json!({"kinds": ["finished", "error"]})).await.unwrap();
    assert_eq!(sub["subscribed"], true);
    rig.mock.emit("added", "ignored");
    rig.mock.emit("finished", "Alpha");
    let e = c.next_event(Duration::from_secs(3)).await.unwrap().expect("an event");
    assert_eq!((e.kind.as_str(), e.name.as_deref()), ("finished", Some("Alpha")));
    assert!(e.seq >= 2 && e.at > 0);
    assert!(c.next_event(Duration::from_millis(200)).await.unwrap().is_none(), "filtered kinds are not pushed");

    // Polling sees everything, in order, with a cursor.
    let mut p = rig.client("poller").await;
    let all: EventsDto = serde_json::from_value(call(&mut p, "events", json!({})).await.unwrap()).unwrap();
    assert_eq!(all.events.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(), vec!["added", "finished"]);
    let again: EventsDto = serde_json::from_value(call(&mut p, "events", json!({"since": all.cursor})).await.unwrap()).unwrap();
    assert!(again.events.is_empty() && again.cursor == all.cursor);
    rig.mock.emit("error", "X");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let newer: EventsDto = serde_json::from_value(call(&mut p, "events", json!({"since": all.cursor, "kinds": ["error"]})).await.unwrap()).unwrap();
    assert_eq!(newer.events.len(), 1);

    let pushed = c.next_event(Duration::from_secs(2)).await.unwrap().expect("the error event was pushed too");
    assert_eq!(pushed.kind, "error");
    call(&mut c, "events_unsubscribe", json!({})).await.unwrap();
    rig.mock.emit("finished", "Beta");
    assert!(c.next_event(Duration::from_millis(300)).await.unwrap().is_none());
}

#[tokio::test]
async fn wait_for_returns_when_done_or_times_out() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let mut c = rig.client("waiter").await;
    // Already seeding: immediate.
    let w: WaitDto = serde_json::from_value(call(&mut c, "wait_for", json!({"ids": ["bbbbbb"]})).await.unwrap()).unwrap();
    assert!(w.done && !w.timed_out);
    // Alpha finishes after a moment.
    let mock = rig.mock.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(700)).await;
        mock.set_state(0xaa, "seeding", Some(1_700_000_100));
    });
    let t = std::time::Instant::now();
    let w: WaitDto = serde_json::from_value(call(&mut c, "wait_for", json!({"ids": ["aaaaaa"], "timeout_secs": 10})).await.unwrap()).unwrap();
    assert!(w.done && t.elapsed() < Duration::from_secs(5));
    // A transfer that never finishes times out.
    let rig2 = Rig::new(Permission::ReadOnly).await;
    let mut c2 = rig2.client("waiter").await;
    let w: WaitDto = serde_json::from_value(call(&mut c2, "wait_for", json!({"ids": ["aaaaaa"], "timeout_secs": 1})).await.unwrap()).unwrap();
    assert!(!w.done && w.timed_out);
}

#[tokio::test]
async fn rate_limit_per_connection() {
    let opts = ServerOptions { rate_per_sec: 5, burst: 5, ..Default::default() };
    let rig = Rig::with(Permission::ReadOnly, opts, vec![]).await;
    let mut c = rig.client("fast").await;
    let mut limited = None;
    for _ in 0..20 {
        if let Err(e) = call(&mut c, "ping", json!({})).await {
            limited = Some(e);
            break;
        }
    }
    let e = limited.expect("rate limit reached");
    assert_eq!(e.code, ErrorCode::RateLimited);
    assert!(e.data.unwrap()["retry_after_ms"].as_u64().unwrap() > 0);
    // A second connection has its own bucket.
    let mut other = rig.client("other").await;
    assert!(call(&mut other, "ping", json!({})).await.is_ok());
    // And the first recovers.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(call(&mut c, "ping", json!({})).await.is_ok());
}

#[tokio::test]
async fn max_connections_is_enforced() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let mut held = Vec::new();
    for i in 0..8 {
        held.push(rig.client(&format!("c{i}")).await);
    }
    let e = trs_control::Client::connect(rig.path(), "ninth").await.err().unwrap();
    assert_eq!(api(e).code, ErrorCode::TooManyConnections);
    assert!(rig.server.connections() <= 8);
    drop(held.pop());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(trs_control::Client::connect(rig.path(), "ninth").await.is_ok());
}

#[tokio::test]
async fn size_caps_and_garbage() {
    let opts = ServerOptions { max_line_bytes: 64 * 1024, prehello_line_bytes: 512, ..Default::default() };
    let rig = Rig::with(Permission::Full, opts, vec![row(0xaa, "Alpha", "downloading")]).await;

    // Before hello only tiny lines are accepted.
    let mut r = Raw::connect(&rig.path()).await;
    r.send(&vec![b'x'; 2000]).await;
    assert_eq!(r.recv().await.unwrap().error.unwrap().code, ErrorCode::RequestTooLarge);
    assert!(r.recv().await.is_none(), "connection closed");

    // After hello, up to the cap; beyond it the connection is closed with an error.
    let mut r = Raw::connect(&rig.path()).await;
    assert!(r.hello("big").await.error.is_none());
    let pad = "a".repeat(30_000);
    r.send(format!("{{\"id\":2,\"method\":\"list\",\"params\":{{\"text\":\"{pad}\"}}}}\n").as_bytes()).await;
    assert!(r.recv().await.unwrap().result.is_some(), "a 30 KB request is fine");
    let mut huge = vec![b'y'; 200 * 1024];
    huge.push(b'\n');
    r.send(&huge).await;
    assert_eq!(r.recv().await.unwrap().error.unwrap().code, ErrorCode::RequestTooLarge);

    // Garbage: errors, not crashes, and the connection survives a few bad lines.
    let mut r = Raw::connect(&rig.path()).await;
    assert!(r.hello("fuzz").await.error.is_none());
    for junk in [&b"not json\n"[..], b"{\n", b"[1,2,3]\n", b"\xff\xfe\xfd\n", b"{\"id\":9}\n", b"null\n", b"{\"id\":1,\"method\":42}\n", b"\n"] {
        r.send(junk).await;
        if junk == b"\n" {
            continue; // blank lines are ignored
        }
        let e = r.recv().await.unwrap().error.expect("an error");
        assert!(matches!(e.code, ErrorCode::ParseError | ErrorCode::InvalidRequest), "{junk:?} -> {e}");
    }
    r.send(b"{\"id\":10,\"method\":\"ping\"}\n").await;
    assert!(r.recv().await.unwrap().result.is_some(), "still alive after garbage");
    // Eight bad lines in a row end the connection.
    for _ in 0..8 {
        r.send(b"garbage\n").await;
    }
    let mut last_closed = false;
    for _ in 0..9 {
        if r.recv().await.is_none() {
            last_closed = true;
            break;
        }
    }
    assert!(last_closed, "connection dropped after repeated garbage");
    // The server is fine.
    assert!(rig.client("after").await.call(&Request::Ping(Empty {})).await.is_ok());
}

#[tokio::test]
async fn many_concurrent_clients() {
    let rows: Vec<_> = (1..=20u8).map(|i| row(i, &format!("T{i}"), if i % 2 == 0 { "seeding" } else { "downloading" })).collect();
    let opts = ServerOptions { max_connections: 8, rate_per_sec: 1000, burst: 1000, ..Default::default() };
    let rig = Rig::with(Permission::Control, opts, rows).await;
    let path = rig.path();
    let mut tasks = Vec::new();
    for n in 0..8 {
        let path = path.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = trs_control::Client::connect(&path, &format!("worker-{n}")).await.unwrap();
            for i in 0..60u32 {
                match i % 4 {
                    0 => {
                        let v = c.call(&Request::List(ListParams { filter: ListFilter::Seeding, ..Default::default() })).await.unwrap();
                        assert_eq!(v["total"], 10);
                    }
                    1 => {
                        let v = c.call(&Request::Info(InfoParams { id: id_hex(&[(i % 20 + 1) as u8; 20]), files: Some(false), trackers: Some(false), peers: Some(false), max_files: None, max_peers: None })).await.unwrap();
                        assert_eq!(v["transfer"]["name"], format!("T{}", i % 20 + 1));
                    }
                    2 => {
                        c.call(&Request::SetPriority(SetPriorityParams { ids: vec![id_hex(&[((i + n) % 20 + 1) as u8; 20])], priority: PriorityParam::Low })).await.unwrap();
                    }
                    _ => {
                        let v = c.call(&Request::Status(Empty {})).await.unwrap();
                        assert_eq!(v["total"], 20);
                    }
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    assert!(rig.server.calls() >= 8 * 60);
}

#[tokio::test]
async fn tracker_profiles_and_statistics() {
    use trs_state::{Day, StatsBucket, StatsScope};
    let rig = Rig::new(Permission::ReadOnly).await;
    // A real database and a profile file in the state dir.
    let dir = rig.state_dir();
    std::fs::write(dir.join("trackers.toml"), "version = 1\n\n[[tracker]]\nid = \"tl\"\nname = \"Example Tracker\"\nurls = [\"https://tracker.example.org/a/*/announce\"]\n").unwrap();
    let opened = trs_state::Store::open(&dir).unwrap();
    let today = Day::from_unix(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64, 0);
    opened.store.record_stats(&StatsScope::Profile("tl".into()), today, &StatsBucket { uploaded: 4_000_000, downloaded: 1_000_000, torrents_completed: 1, ..Default::default() }).unwrap();
    opened.store.record_stats(&StatsScope::Profile("tl".into()), today.minus(3), &StatsBucket { uploaded: 2_000_000, ..Default::default() }).unwrap();
    opened.store.flush_now().unwrap();
    let store: std::sync::Arc<dyn trs_control::state::StoreRef> = std::sync::Arc::new(opened.store);
    let rig2 = {
        // A second server on the same state dir, with the store attached.
        let mut other = row(3, "c", "seeding");
        other.tracker_host = "bttracker.debian.org".into();
        let mock = Mock::new(vec![row(1, "a", "seeding"), row(2, "b", "downloading"), other]);
        let (src, perm) = trs_control::PermissionSource::shared(Permission::ReadOnly);
        let state = trs_control::StateAccess::new(&dir, Some(store));
        let server = trs_control::serve_backend(mock.clone(), state, rig.dir.path().join("s2.sock"), src, ServerOptions::default()).await.unwrap();
        (mock, server, perm)
    };
    let mut c = trs_control::Client::connect(rig2.1.path(), "stats").await.unwrap();

    let t: TrackersDto = serde_json::from_value(call(&mut c, "trackers", json!({})).await.unwrap()).unwrap();
    assert_eq!(t.profiles.len(), 1);
    assert_eq!(t.profiles[0].id, "tl");
    assert!(t.profiles[0].favorite);
    // Everything no favorite claims is grouped by the registrable domain of its primary host.
    let g: Vec<_> = t.groups.iter().map(|g| (g.id.as_str(), g.name.as_str(), g.transfers, g.favorite)).collect();
    assert_eq!(g, vec![("tracker.test", "Tracker", 2, false), ("debian.org", "Debian", 1, false)]);
    assert_eq!(t.groups[1].urls, vec!["bttracker.debian.org".to_string()]);

    let s: TrackerStatsDto = serde_json::from_value(call(&mut c, "tracker_stats", json!({"tracker": "tl", "days": 7})).await.unwrap()).unwrap();
    assert_eq!(s.series.len(), 7);
    assert_eq!(s.today.uploaded, 4_000_000);
    assert_eq!(s.today.ratio, Some(4.0));
    assert_eq!(s.all_time.uploaded, 6_000_000);
    assert_eq!(s.series.last().unwrap().bucket.uploaded, 4_000_000);
    let all: TrackerStatsDto = serde_json::from_value(call(&mut c, "tracker_stats", json!({})).await.unwrap()).unwrap();
    assert_eq!(all.tracker, "all");
    assert_eq!(all.all_time.uploaded, 6_000_000);
    assert_eq!(call(&mut c, "tracker_stats", json!({"tracker": "nope"})).await.unwrap_err().code, ErrorCode::NotFound);
    // Without a database the answer says so instead of failing oddly.
    let mut c0 = rig.client("nodb").await;
    assert_eq!(call(&mut c0, "tracker_stats", json!({})).await.unwrap_err().code, ErrorCode::Unsupported);
}

#[tokio::test]
async fn tracker_log_is_read_only_and_serves_the_masked_lines() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let logs = rig.state_dir().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let line = |ts: u64, ev: &str, err: bool| {
        let resp = if err { r#","response":{"fields":{},"error":"timeout"}"# } else { r#","response":{"status":200,"fields":{"interval":"1800"}}"# };
        format!(r#"{{"v":1,"ts_ms":{ts},"kind":"announce","torrent":"aabbccddeeff00112233445566778899aabbccdd","id":"aabbccdd","title":"Ubuntu","tracker":"https://t.example/announce?passkey=••••••","host":"t.example","transport":"https","event":"{ev}"{resp}}}"#)
    };
    std::fs::write(logs.join("trackers.log"), format!("{}\n{}\n{}\n", line(1000, "started", false), line(2000, "regular", true), line(3000, "regular", false))).unwrap();
    let mut c = rig.client("log").await;
    let v = call(&mut c, "tracker_log", json!({})).await.unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 3);
    assert_eq!(v["enabled"], false, "the setting, not the file");
    assert!(v["path"].as_str().unwrap().ends_with("logs/trackers.log"));
    let e = call(&mut c, "tracker_log", json!({"errors_only": true})).await.unwrap();
    assert_eq!(e["entries"].as_array().unwrap().len(), 1);
    let f = call(&mut c, "tracker_log", json!({"since_ms": 2000, "tracker": "t.example", "torrent": "aabb", "limit": 5})).await.unwrap();
    assert_eq!(f["entries"].as_array().unwrap().len(), 1);
    assert_eq!(f["entries"][0]["ts_ms"], 3000);
    let n = call(&mut c, "tracker_log", json!({"event": "started", "limit": 1})).await.unwrap();
    assert_eq!(n["entries"].as_array().unwrap().len(), 1);
    // no secret can come out of the log, because none is in it; and no log is just an empty answer
    std::fs::remove_dir_all(&logs).unwrap();
    assert_eq!(call(&mut c, "tracker_log", json!({})).await.unwrap()["entries"].as_array().unwrap().len(), 0);
    // the method is allowed at read_only (the permission table test covers the other levels)
    assert!(trs_control::proto::Method::from_name("tracker_log").is_some());
}

#[tokio::test]
async fn rules_are_listed() {
    let rig = Rig::new(Permission::ReadOnly).await;
    std::fs::write(rig.state_dir().join("rules.toml"), "version = 1\n\n[default]\npriority = \"normal\"\n\n[[rule]]\nid = \"tz\"\nname = \"Z\"\nmatch = { private = true }\npriority = \"high\"\n").unwrap();
    let mut c = rig.client("r").await;
    let v = call(&mut c, "rules", json!({})).await.unwrap();
    assert_eq!(v["rule"][0]["id"], "tz");
    assert!(v.get("default").is_some());
}

#[tokio::test]
async fn socket_file_goes_away_with_the_server() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let path = rig.path();
    assert!(path.exists());
    let Rig { server, dir, .. } = rig;
    server.shutdown().await;
    assert!(!path.exists());
    assert!(trs_control::Client::connect(&path, "late").await.is_err());
    drop(dir);
}

#[tokio::test]
async fn save_path_needs_full_and_never_targets_sensitive_folders() {
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    let ok = call(&mut c, "add", json!({"source": "magnet:?xt=urn:btih:abcdef", "save_path": "/tmp/x"})).await;
    assert!(ok.is_ok(), "{ok:?}");
    let e = call(&mut c, "add", json!({"source": "magnet:?xt=urn:btih:abcdef", "save_path": "relative/dir"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidParams, "{e}");
    for bad in ["~/.ssh", "~/Library/LaunchAgents", "~", "/Library/LaunchDaemons", "/usr/local/bin", "/etc"] {
        let e = call(&mut c, "add", json!({"source": "magnet:?xt=urn:btih:abcdef", "save_path": bad})).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid, "{bad}: {e}");
    }
}


// ---- smart add and moving (SPEC 7A.5, 7A.6) ----------------------------------------------------

#[tokio::test]
async fn a_held_add_carries_needs_choice_and_the_structure() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    let v = call(&mut c, "add", json!({"magnet": "magnet:?xt=urn:btih:abcdef&dn=held"})).await.unwrap();
    let a: AddedDto = serde_json::from_value(v).unwrap();
    assert!(a.needs_choice);
    let ch = a.choice.expect("structure");
    assert_eq!(ch.groups[0].items.len(), 2);
    assert!(!ch.groups[0].label.contains('\u{202e}'), "labels come from the torrent and are cleaned: {:?}", ch.groups[0].label);
    // A normal add says nothing about choices.
    let v = call(&mut c, "add", json!({"magnet": "magnet:?xt=urn:btih:abcdef&dn=plain"})).await.unwrap();
    assert!(v.get("needs_choice").is_none() && v.get("choice").is_none(), "{v}");
    // The held transfer is listed under its own state and filter.
    let l: ListDto = serde_json::from_value(call(&mut c, "list", json!({"filter": "awaiting-choice"})).await.unwrap()).unwrap();
    assert_eq!(l.total, 1);
    assert_eq!(l.transfers[0].state, "awaiting-choice");
}

#[tokio::test]
async fn an_add_reports_the_grace_period_and_now_skips_it() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    let v = call(&mut c, "add", json!({"magnet": "magnet:?xt=urn:btih:abcdef&dn=graceful"})).await.unwrap();
    let a: AddedDto = serde_json::from_value(v).unwrap();
    let p = a.pending.expect("pending with its deadline");
    assert_eq!((p.starts_in_secs, p.starts_at_ms, p.paused), (10, 1_700_000_010_000, false));
    assert!(!a.needs_choice);
    // The row says so too, and `list pending` finds it.
    let l: ListDto = serde_json::from_value(call(&mut c, "list", json!({"filter": "pending"})).await.unwrap()).unwrap();
    assert_eq!(l.total, 1);
    assert_eq!(l.transfers[0].state, "pending");
    assert_eq!(l.transfers[0].pending.map(|p| p.starts_in_secs), Some(10));
    // `now` skips the wait; a plain add carries no pending at all.
    let v = call(&mut c, "add", json!({"magnet": "magnet:?xt=urn:btih:abcdee&dn=graceful-now", "now": true})).await.unwrap();
    assert!(v.get("pending").is_none(), "{v}");
    let v = call(&mut c, "add", json!({"magnet": "magnet:?xt=urn:btih:abcdef&dn=plain"})).await.unwrap();
    assert!(v.get("pending").is_none(), "{v}");
}

#[tokio::test]
async fn choose_files_validates_and_maps_to_priorities() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    // The mock has 3 files.
    call(&mut c, "choose_files", json!({"id": "aaaaaa", "files": [0, 2]})).await.unwrap();
    call(&mut c, "choose_files", json!({"id": "aaaaaa", "skip": [1]})).await.unwrap();
    call(&mut c, "choose_files", json!({"id": "aaaaaa", "accept_default": true})).await.unwrap();
    let calls = rig.mock.calls();
    assert_eq!(calls[0], "choose normal,skip,normal");
    assert_eq!(calls[1], "choose normal,skip,normal");
    assert_eq!(calls[2], "choose default");
    for bad in [json!({"id": "aaaaaa"}), json!({"id": "aaaaaa", "files": [0], "skip": [1]}), json!({"id": "aaaaaa", "files": [9]})] {
        assert_eq!(call(&mut c, "choose_files", bad).await.unwrap_err().code, ErrorCode::InvalidParams);
    }
    call(&mut c, "cancel_choice", json!({"id": "aaaaaa"})).await.unwrap();
    assert!(rig.mock.calls().last().unwrap().starts_with("cancel_choice"));
    let ch: ChoiceDto = serde_json::from_value(call(&mut c, "choice", json!({"id": "aaaaaa"})).await.unwrap()).unwrap();
    assert!(ch.awaiting);
}

#[tokio::test]
async fn move_needs_full_and_refuses_sensitive_folders() {
    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    let e = call(&mut c, "move", json!({"ids": ["aaaaaa"], "path": "/Volumes/Media/Done"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert_eq!(e.data.unwrap()["required"], "full");
    // Reading what is out of place and cancelling moves are not that sensitive.
    let m: MoveProposalDto = serde_json::from_value(call(&mut c, "location_moves", json!({})).await.unwrap()).unwrap();
    assert_eq!(m.items.len(), 1);
    call(&mut c, "cancel_move", json!({"ids": ["aaaaaa"]})).await.unwrap();

    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    for bad in ["/System/x", "/Library/y", "/etc", "relative/dir", "/a/../b"] {
        let e = call(&mut c, "move", json!({"ids": ["aaaaaa"], "path": bad})).await.unwrap_err();
        assert!(matches!(e.code, ErrorCode::Invalid | ErrorCode::InvalidParams), "{bad}: {e}");
    }
    if let Some(home) = std::env::var_os("HOME") {
        let hidden = std::path::Path::new(&home).join(".ssh");
        let e = call(&mut c, "move", json!({"ids": ["aaaaaa"], "path": hidden.display().to_string()})).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::Invalid);
    }
    assert!(rig.mock.calls().is_empty(), "nothing reached the backend");
    let r: MoveReportDto = serde_json::from_value(call(&mut c, "move", json!({"ids": ["aaaaaa"], "path": "/Volumes/Media/Done"})).await.unwrap()).unwrap();
    assert_eq!(r.queued.len(), 1);
    assert_eq!(rig.mock.calls(), ["move 1 to Some(\"/Volumes/Media/Done\")"]);
    // Without a path: where the settings say.
    call(&mut c, "move", json!({"ids": ["aaaaaa"]})).await.unwrap();
    assert_eq!(rig.mock.calls()[1], "move 1 to None");
}

#[tokio::test]
async fn stream_url_and_stop_need_control_and_are_logged() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let mut c = rig.client("ro").await;
    let e = call(&mut c, "stream_url", json!({"id": "aaaaaa"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);

    let rig = Rig::new(Permission::Control).await;
    let mut c = rig.client("claude").await;
    let v = call(&mut c, "stream_url", json!({"id": "aaaaaa", "file": 3})).await.unwrap();
    let i: StreamInfoDto = serde_json::from_value(v).unwrap();
    assert_eq!(i.url.as_deref(), Some("http://127.0.0.1:5/tok/movie.mkv"));
    assert_eq!(i.file, 3);
    assert!(i.url.unwrap().starts_with("http://127.0.0.1:"), "loopback only");
    let v = call(&mut c, "stop_stream", json!({"token": "tok"})).await.unwrap();
    assert_eq!(v["closed"], true);
    let v = call(&mut c, "stop_stream", json!({"token": "other"})).await.unwrap();
    assert_eq!(v["closed"], false);
    // Unknown fields are refused like everywhere else.
    assert!(Request::decode("stream_url", json!({"id": "aaaaaa", "url": "x"})).is_err());
}

#[tokio::test]
async fn debug_methods_answer_at_read_only_and_never_leak_a_secret() {
    let rig = Rig::new(Permission::ReadOnly).await;
    let mut c = rig.client("claude").await;
    let t: DebugTrackersDto = serde_json::from_value(call(&mut c, "debug_trackers", json!({})).await.unwrap()).unwrap();
    assert!(t.online && !t.transfers.is_empty());
    let tr = &t.transfers[0].trackers[0];
    assert!(tr.url.contains("t.example.org") && !tr.url.contains("0123456789abcdef"), "{}", tr.url);
    assert!(!tr.next_reason.is_empty());
    let one = call(&mut c, "debug_trackers", json!({"id": "aaaaaa", "tracker": "example.org"})).await.unwrap();
    assert_eq!(one["transfers"].as_array().unwrap().len(), 1);
    let none = call(&mut c, "debug_trackers", json!({"id": "aaaaaa", "tracker": "nowhere.test"})).await.unwrap();
    assert!(none["transfers"].as_array().unwrap().is_empty());
    let p: DebugPeersDto = serde_json::from_value(call(&mut c, "debug_peers", json!({"id": "aaaaaa", "redact": true})).await.unwrap()).unwrap();
    assert_eq!(p.peers.len(), 150);
    assert!(p.peers[0].addr.ends_with(".x:6881"), "{}", p.peers[0].addr);
    let plain: DebugPeersDto = serde_json::from_value(call(&mut c, "debug_peers", json!({"id": "aaaaaa"})).await.unwrap()).unwrap();
    assert_eq!(plain.peers[0].addr, "10.0.0.0:6881");
    let perf: DebugPerfDto = serde_json::from_value(call(&mut c, "debug_perf", json!({})).await.unwrap()).unwrap();
    assert_eq!(perf.torrent_count, 2);
    assert!(perf.process.pid > 0);
    let h: DebugHealthDto = serde_json::from_value(call(&mut c, "debug_health", json!({})).await.unwrap()).unwrap();
    assert!(h.summary.len() > 3);
    for (m, p) in [("debug_trackers", json!({})), ("debug_peers", json!({"id": "aaaaaa"})), ("debug_perf", json!({})), ("debug_health", json!({}))] {
        let v = call(&mut c, m, p).await.unwrap().to_string();
        assert!(!v.contains("SECRETKEY") && !v.contains("0123456789abcdef"), "{m}: {v}");
    }
    assert_eq!(call(&mut c, "debug_peers", json!({"id": "dddddd"})).await.unwrap_err().code, ErrorCode::NotFound);
}
