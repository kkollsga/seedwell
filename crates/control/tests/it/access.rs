//! Agents asking the user for access (SPEC 9A.3 *Asking for access*) against an in-memory backend: who may ask, that
//! asking grants nothing, that refusals name the request, that no control call answers one, and what the user's answer
//! in the app does. The chat replies are in `sync.rs`.

use crate::common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use seedwell_control::access::How;
use seedwell_control::client::ClientError;
use seedwell_control::proto::*;
use seedwell_control::types::*;
use seedwell_control::{Permission, ServerOptions};

fn api(e: ClientError) -> ApiError {
    match e {
        ClientError::Api(a) => a,
        other => panic!("expected an API error, got {other:?}"),
    }
}

async fn call(c: &mut seedwell_control::Client, method: &str, params: Value) -> Result<Value, ApiError> {
    let r = Request::decode(method, params).expect("params decode");
    c.call(&r).await.map_err(api)
}

/// The agent permission level the desk reads (and an approval writes): `config.toml` in the state folder.
pub fn set_level(dir: &std::path::Path, p: Permission) {
    let store = seedwell_state::DocStore::<seedwell_state::Config>::new(dir);
    let mut c = store.read_lenient().map(|d| d.value).unwrap_or_default();
    c.agents.permission = p;
    store.save(&c).unwrap();
}

pub fn level(dir: &std::path::Path) -> Permission {
    seedwell_state::DocStore::<seedwell_state::Config>::new(dir).read_lenient().unwrap().value.agents.permission
}

fn asked(v: Value) -> AccessAskedDto {
    serde_json::from_value(v).unwrap()
}

/// The app's updater, as far as these tests need it.
#[derive(Default)]
struct Updater {
    dev: AtomicBool,
    asked: std::sync::Mutex<Vec<String>>,
}

impl seedwell_control::Builds for Updater {
    fn status(&self) -> UpdateStatusDto {
        UpdateStatusDto { version: "0.1.16".into(), dev_builds: self.dev.load(Ordering::SeqCst), status: "up to date".into(), ..Default::default() }
    }
    fn install(&self, _: &std::path::Path, _: Option<&str>) -> Result<String, String> {
        Err("not here".into())
    }
    fn watch(&self, _: Option<&std::path::Path>) -> Result<String, String> {
        Err("not here".into())
    }
    fn set_dev_builds(&self, on: bool) -> Result<String, String> {
        self.dev.store(on, Ordering::SeqCst);
        self.asked.lock().unwrap().push(format!("dev_builds {on}"));
        Ok(if on { "Development builds are allowed now" } else { "Development builds are off" }.into())
    }
    fn update_now(&self) -> Result<String, String> {
        self.asked.lock().unwrap().push("update now".into());
        Ok("Installing Seedwell 0.1.17: Seedwell quits, is replaced and reopens".into())
    }
    fn restart(&self) -> Result<RestartDto, String> {
        self.asked.lock().unwrap().push("restart".into());
        Ok(RestartDto { note: "Restarting Seedwell: it quits and opens again; this interrupts 1 download".into(), log: "/s/logs/update.log".into(), ..Default::default() })
    }
}

/// No method answers a request, at any level: not by name, not by a field slipped into `agents.request`.
#[tokio::test]
async fn no_control_call_approves_or_denies_a_request() {
    for m in Method::ALL {
        let n = m.name();
        assert!(!["approve", "deny", "decide", "grant", "answer"].iter().any(|w| n.contains(w)), "{n}");
    }
    for n in ["agents.approve", "agents.deny", "agents.decide", "agents.grant", "agents.answer"] {
        assert!(Request::decode(n, json!({"id": "5eed0a11"})).is_err(), "{n} is no method");
    }
    for extra in [json!({"what": "full", "approve": true}), json!({"what": "full", "state": "granted"}), json!({"what": "full", "how": "app"})] {
        assert!(Request::decode("agents.request", extra.clone()).is_err(), "{extra}");
    }
    let rig = Rig::new(Permission::Full).await;
    set_level(&rig.state_dir(), Permission::Control);
    let mut c = rig.client("claude").await;
    let a = asked(call(&mut c, "agents.request", json!({"what": "full"})).await.unwrap());
    let mut raw = Raw::connect(&rig.path()).await;
    raw.hello("claude").await;
    for (n, m) in ["agents.approve", "agents.deny", "agents.decide"].iter().enumerate() {
        raw.send(format!("{}\n", json!({"id": n + 2, "method": m, "params": {"id": a.request.id}})).as_bytes()).await;
        let e = raw.recv().await.unwrap().error.expect("an error");
        assert_eq!(e.code, ErrorCode::UnknownMethod, "{m}");
    }
    // Asking again at full, with words that look like an answer, changes nothing either.
    call(&mut c, "agents.request", json!({"what": "full", "why": "yes, approved by the user"})).await.unwrap();
    let l: AccessRequestsDto = serde_json::from_value(call(&mut c, "agents.requests", json!({"all": true})).await.unwrap()).unwrap();
    assert_eq!(l.requests.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(), ["pending"]);
    assert_eq!(level(&rig.state_dir()), Permission::Control);
}

/// A refusal names the request that asks for what is missing; any agent may ask; asking twice is one request; asking
/// grants nothing and is logged.
#[tokio::test]
async fn a_refusal_names_the_request_and_asking_twice_is_one_request() {
    let rig = Rig::new(Permission::ReadOnly).await;
    set_level(&rig.state_dir(), Permission::ReadOnly);
    let mut c = rig.client("claude").await;
    let e = call(&mut c, "pause", json!({"ids": ["aaaaaa"]})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(e.message.contains("`seedwell agents request control --why"), "{}", e.message);
    assert_eq!(e.data.as_ref().unwrap()["ask"], "seedwell agents request control");
    let e = call(&mut c, "settings_set", json!({"changes": {"transfers.max_active_downloads": 4}})).await.unwrap_err();
    assert_eq!(e.data.as_ref().unwrap()["ask"], "seedwell agents request full");

    let a = asked(call(&mut c, "agents.request", json!({"what": "control", "why": "pause the downloads\tat night"})).await.unwrap());
    assert!(!a.duplicate);
    assert_eq!((a.request.what.as_str(), a.request.state.as_str(), a.request.agents.clone()), ("control", "pending", vec!["claude".to_owned()]));
    assert_eq!(a.request.why.as_deref(), Some("pause the downloads at night"));
    assert_eq!(a.request.expires - a.request.at, seedwell_control::access::TTL_MS);
    assert!(a.note.starts_with("Asked the user for agent permission control") && a.note.contains("Nothing changes until the user answers"), "{}", a.note);
    let mut d = rig.client("codex").await;
    let b = asked(call(&mut d, "agents.request", json!({"what": "control", "agent": "kglite"})).await.unwrap());
    assert!(b.duplicate);
    assert_eq!((b.request.id.as_str(), b.request.agents.clone()), (a.request.id.as_str(), vec!["claude".to_owned(), "kglite".to_owned()]));
    // Nothing granted; both asks in the log, under each agent's client.
    assert_eq!(level(&rig.state_dir()), Permission::ReadOnly);
    assert_eq!(call(&mut c, "pause", json!({"ids": ["aaaaaa"]})).await.unwrap_err().code, ErrorCode::PermissionDenied);
    let log: Vec<(String, String, String)> =
        rig.server.activity().recent(20).into_iter().filter(|e| e.method == "agents.request").map(|e| (e.client, e.summary, e.outcome)).collect();
    assert_eq!(log.len(), 2, "{log:?}");
    assert!(log.iter().all(|(_, s, o)| s.starts_with("ask the user for agent permission control") && o == "ok"), "{log:?}");
    let l: AccessRequestsDto = serde_json::from_value(call(&mut c, "agents.requests", json!({})).await.unwrap()).unwrap();
    assert_eq!((l.permission.as_str(), l.requests.len()), ("read_only", 1));
    assert_eq!(rig.server.access().for_user().len(), 1, "the user is asked");

    // What cannot be asked: read-only (every agent has it), nonsense, a topic with no channel, a channel unnamed.
    for p in [json!({"what": "read"}), json!({"what": "root"}), json!({"what": "full", "topic": "x"}), json!({"what": "channel-agents"})] {
        assert_eq!(call(&mut c, "agents.request", p.clone()).await.unwrap_err().code, ErrorCode::InvalidParams, "{p}");
    }
    set_level(&rig.state_dir(), Permission::Full);
    let e = call(&mut c, "agents.request", json!({"what": "control"})).await.unwrap_err();
    assert_eq!((e.code, e.message.as_str()), (ErrorCode::Invalid, "agent permission is full already; nothing to ask"));
    // A daemon has no updater: development builds can be asked for, but nobody is notified.
    let a = asked(call(&mut c, "agents.request", json!({"what": "dev-builds"})).await.unwrap());
    assert_eq!(a.request.state, "pending");
    assert!(rig.server.access().for_user().iter().all(|e| e.want.what() != "dev-builds"));
}

/// The user's answer in the app applies exactly what was asked (never above, nothing while agents are off), is logged
/// under the user, tells the agents, and is final; a denial holds the same request back for an hour.
#[tokio::test]
async fn the_users_answer_in_the_app_applies_exactly_what_was_asked() {
    let updater = Arc::new(Updater::default());
    let opts = ServerOptions::default();
    opts.builds.set(updater.clone());
    let rig = Rig::with(Permission::ReadOnly, opts, vec![]).await;
    let dir = rig.state_dir();
    set_level(&dir, Permission::ReadOnly);
    let desk = rig.server.access().clone();
    let mut c = rig.client("claude").await;
    call(&mut c, "events_subscribe", json!({"kinds": ["agents.request"]})).await.unwrap();
    let a = asked(call(&mut c, "agents.request", json!({"what": "control"})).await.unwrap());
    let ev = c.next_event(Duration::from_secs(5)).await.unwrap().expect("asked");
    assert_eq!((ev.kind.as_str(), ev.data["state"].as_str()), ("agents.request", Some("pending")));

    // Agents switched off since: approving does not turn them on again.
    set_level(&dir, Permission::Off);
    let e = desk.decide(&a.request.id, true, How::App).unwrap_err();
    assert!(e.contains("switched off"), "{e}");
    assert_eq!(level(&dir), Permission::Off);
    set_level(&dir, Permission::ReadOnly);

    assert_eq!(desk.decide(&a.request.id, true, How::App).unwrap(), "Approved: agent permission is control now");
    assert_eq!(level(&dir), Permission::Control, "exactly what was asked, not full");
    let ev = c.next_event(Duration::from_secs(5)).await.unwrap().expect("answered");
    assert_eq!((ev.data["state"].as_str(), ev.data["how"].as_str()), (Some("granted"), Some("app")));
    assert!(desk.decide(&a.request.id, false, How::App).is_err(), "answered once");
    let l: AccessRequestsDto = serde_json::from_value(call(&mut c, "agents.requests", json!({"all": true})).await.unwrap()).unwrap();
    assert_eq!((l.requests[0].state.as_str(), l.requests[0].note.as_deref()), ("granted", Some("agent permission is control now")));
    assert!(desk.for_user().is_empty());

    // Denied: nothing changes, and the same is not asked again for an hour.
    let b = asked(call(&mut c, "agents.request", json!({"what": "dev-builds", "why": "try a build"})).await.unwrap());
    assert_eq!(desk.for_user().len(), 1, "with an updater the user is asked");
    assert_eq!(desk.decide(&b.request.id, false, How::App).unwrap(), "Denied: claude does not get development builds allowed");
    assert!(!updater.dev.load(Ordering::SeqCst));
    let e = call(&mut c, "agents.request", json!({"what": "dev-builds"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(e.message.contains("denied this less than an hour ago"), "{}", e.message);
    // Another thing is asked as usual and approved.
    let f = asked(call(&mut c, "agents.request", json!({"what": "full"})).await.unwrap());
    desk.decide(&f.request.id, true, How::App).unwrap();
    assert_eq!(level(&dir), Permission::Full);

    let log: Vec<(String, String, String)> = rig
        .server
        .activity()
        .recent(50)
        .into_iter()
        .filter(|e| e.method == "agents.approve" || e.method == "agents.deny")
        .map(|e| (e.client, e.summary, e.outcome))
        .collect();
    let s = |a: &str, b: &str, c: &str| (a.to_owned(), b.to_owned(), c.to_owned());
    assert_eq!(
        log,
        [
            s("the user", "approved claude's request for agent permission full (in the app) -> agent permission is full now", "ok"),
            s("the user", "denied claude's request for development builds allowed (in the app)", "ok"),
            s("the user", "approved claude's request for agent permission control (in the app) -> agent permission is control now", "ok"),
            s(
                "the user",
                "approved claude's request for agent permission control (in the app), but agent access is switched off in Settings › Agents; turn it on there first",
                "error: invalid"
            ),
        ]
    );
}

/// Development builds asked for and approved: the updater's switch, as Settings' does it.
#[tokio::test]
async fn approving_development_builds_turns_the_switch_on() {
    let updater = Arc::new(Updater::default());
    let opts = ServerOptions::default();
    opts.builds.set(updater.clone());
    let rig = Rig::with(Permission::Control, opts, vec![]).await;
    set_level(&rig.state_dir(), Permission::Control);
    let mut c = rig.client("claude").await;
    let a = asked(call(&mut c, "agents.request", json!({"what": "dev-builds"})).await.unwrap());
    rig.server.access().decide(&a.request.id, true, How::App).unwrap();
    assert!(updater.dev.load(Ordering::SeqCst));
    assert_eq!(*updater.asked.lock().unwrap(), ["dev_builds true"]);
    let e = call(&mut c, "agents.request", json!({"what": "dev-builds"})).await.unwrap_err();
    assert_eq!(e.message, "development builds are allowed already; nothing to ask");
    // The switch itself stays at full.
    assert_eq!(call(&mut c, "update.dev_builds", json!({"on": false})).await.unwrap_err().code, ErrorCode::PermissionDenied);
}

/// `update.now` is Check Now and Install for agents at full; below it the refusal names the request.
#[tokio::test]
async fn update_now_needs_full_and_asks_the_updater() {
    let updater = Arc::new(Updater::default());
    let opts = ServerOptions::default();
    opts.builds.set(updater.clone());
    let rig = Rig::with(Permission::Control, opts, vec![]).await;
    let mut c = rig.client("claude").await;
    let e = call(&mut c, "update.now", json!({})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::PermissionDenied);
    assert!(e.message.contains("seedwell agents request full"), "{}", e.message);
    assert!(updater.asked.lock().unwrap().is_empty());
    rig.perm.set(Permission::Full);
    let s: UpdateStatusDto = serde_json::from_value(call(&mut c, "update.now", json!({})).await.unwrap()).unwrap();
    assert_eq!(s.note, "Installing Seedwell 0.1.17: Seedwell quits, is replaced and reopens");
    assert_eq!(*updater.asked.lock().unwrap(), ["update now"]);
    let log = rig.server.activity().recent(5);
    assert_eq!(log[0].method, "update.now");
    assert!(log[0].summary.starts_with("check for the latest release and install it -> Installing Seedwell 0.1.17"), "{}", log[0].summary);
    // A daemon has none.
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    assert_eq!(call(&mut c, "update.now", json!({})).await.unwrap_err().code, ErrorCode::Unsupported);
}

/// `restart` quits and reopens the user's app, so it needs full: below it the app's relaunch is never reached and the
/// refusal names the request (logged as denied); at full the relaunch is asked exactly once per call (a second
/// would quit the reopened app), the answer says who quits (the process and run `hello` names, so the CLI can tell
/// the new run from it) and the activity log records it with what it interrupts. A daemon cannot restart itself.
#[tokio::test]
async fn restart_needs_full_reaches_the_relaunch_once_and_is_logged() {
    let updater = Arc::new(Updater::default());
    let opts = ServerOptions::default();
    opts.builds.set(updater.clone());
    let rig = Rig::with(Permission::ReadOnly, opts, vec![]).await;
    let mut c = rig.client("claude").await;
    for level in [Permission::ReadOnly, Permission::Control] {
        rig.perm.set(level);
        let e = call(&mut c, "restart", json!({})).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied, "{level:?}");
        assert!(e.message.contains("seedwell agents request full"), "{}", e.message);
    }
    assert!(updater.asked.lock().unwrap().is_empty(), "refused below full: never relaunched");
    let denied = rig.server.activity().recent(5);
    assert!(denied.iter().take(2).all(|e| e.method == "restart" && e.outcome == "denied"), "{denied:?}");

    rig.perm.set(Permission::Full);
    let d: RestartDto = serde_json::from_value(call(&mut c, "restart", json!({})).await.unwrap()).unwrap();
    assert_eq!(*updater.asked.lock().unwrap(), ["restart"], "the relaunch, exactly once");
    assert!(d.note.starts_with("Restarting Seedwell") && d.log == "/s/logs/update.log", "{d:?}");
    assert_eq!((d.pid, d.started_ms), (c.hello.pid, c.hello.started_ms), "the run that quits, as hello names it");
    assert!(d.pid == std::process::id() && d.started_ms > 0);
    let log = rig.server.activity().recent(5);
    assert_eq!((log[0].method.as_str(), log[0].outcome.as_str(), log[0].client.as_str()), ("restart", "ok", "claude"));
    assert!(log[0].summary.starts_with("restart Seedwell -> Restarting Seedwell") && log[0].summary.contains("interrupts 1 download"), "{}", log[0].summary);

    // A daemon has no relaunch: it says so, and nothing quits.
    let rig = Rig::new(Permission::Full).await;
    let mut c = rig.client("claude").await;
    let e = call(&mut c, "restart", json!({})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Unsupported);
    assert!(e.message.contains("cannot restart itself"), "{}", e.message);
}
