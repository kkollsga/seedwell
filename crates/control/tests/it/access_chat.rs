//! Answering an agent's request for access with a reply in chat (SPEC 9A.3 *Asking for access*), against real folder
//! sync services on loopback: only the user's own reply to the request's own announcement counts, written in the app on
//! this device or on another of the user's paired devices. An agent's reply (here or there), another member's, a
//! reply to another message, a forged reply id and an unclear answer never do.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use serde_json::{Value, json};
use seedwell_control::Permission;
use seedwell_control::client::ClientError;
use seedwell_control::proto::*;
use seedwell_control::types::*;
use seedwell_sync_net::chat::{AgentAccess, MessageKind};
use seedwell_sync_proto::ids::FolderId;

use crate::access::{level, set_level};
use crate::sync::{SyncRig, other_device, wait_until};

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

/// Where request `id` stands on `s`'s desk.
fn state_of(s: &SyncRig, id: &str) -> String {
    s.rig.server.access().list(true).iter().find(|e| e.id == id).map(|e| e.state.name().to_owned()).unwrap_or_default()
}

/// The replies the desk refused (an agent's or another member's), as logged.
fn refused(s: &SyncRig) -> Vec<(String, String)> {
    s.rig
        .server
        .activity()
        .recent(100)
        .into_iter()
        .filter(|e| e.method.starts_with("agents.") && e.outcome == "denied")
        .map(|e| (e.client, e.summary))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_users_own_reply_to_the_announcement_answers_a_request() {
    let s = SyncRig::new(Permission::Control).await;
    let dir = s.rig.state_dir();
    set_level(&dir, Permission::Control);
    let mut c = s.rig.client("claude").await;
    let ch: ChatChannelDto = serde_json::from_value(call(&mut c, "chat.new", json!({"name": "Bridge"})).await.unwrap()).unwrap();
    let id: FolderId = ch.id.parse().unwrap();
    // The agent's thread, then its request announced in it.
    let m: ChatMessageDto = serde_json::from_value(call(&mut c, "chat.send", json!({"channel": "Bridge", "body": "I need full access to install the release"})).await.unwrap()).unwrap();
    let a: AccessAskedDto = serde_json::from_value(
        call(&mut c, "agents.request", json!({"what": "full", "why": "install the release", "channel": "Bridge", "reply_to": m.id})).await.unwrap(),
    )
    .unwrap();
    let rid = a.request.id.clone();
    let ann = a.request.announced.clone().expect("announced");
    assert!(a.note.contains("announced it in the channel"), "{}", a.note);
    // The announcement: in the agent's thread, under Seedwell's label, never the user's.
    let read: ChatMessagesDto = serde_json::from_value(call(&mut c, "chat.read", json!({"channel": "Bridge"})).await.unwrap()).unwrap();
    let msg = read.messages.iter().find(|x| x.id == ann).expect("the announcement");
    assert_eq!((msg.agent.as_deref(), msg.trust.as_str(), msg.reply_to.as_deref()), (Some("Seedwell"), "untrusted-agent", Some(m.id.as_str())));
    assert!(msg.body.contains("claude asks for agent permission full on ") && msg.body.contains("> install the release"), "{}", msg.body);
    assert!(msg.body.contains("Reply to this message with **yes**"), "{}", msg.body);
    // A channel that is not open to agents (or not there) is no place to announce: nothing is asked.
    let e = call(&mut c, "agents.request", json!({"what": "dev-builds", "channel": "Nowhere"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert_eq!(s.rig.server.access().list(true).len(), 1);

    // 1. The agent answers its own request: logged, not counted.
    call(&mut c, "chat.send", json!({"channel": "Bridge", "body": "yes", "reply_to": ann})).await.unwrap();
    wait_until(10, "the agent's reply is logged", || refused(&s).len() == 1).await;
    assert_eq!(refused(&s)[0], ("claude".to_owned(), "replied to claude's request for agent permission full: not counted (only the user's own reply answers it)".to_owned()));
    assert_eq!(state_of(&s, &rid), "pending");

    // 2. Another device in the channel (not one of the user's): its "yes", and its agent's.
    let other = other_device(s.root.path(), "Other").await;
    let inv: SyncInviteDto = serde_json::from_value(call(&mut c, "chat.share", json!({"channel": "Bridge"})).await.unwrap()).unwrap();
    other.chat_join(&inv.link).await.unwrap();
    other.add_address(&id, std::net::SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), s.h.port())).unwrap();
    wait_until(30, "the other device is admitted", || other.channel_status(&id).is_some_and(|c| c.role.is_some())).await;
    wait_until(30, "the announcement reaches it", || other.chat_read(&id, None, None, None).unwrap().iter().any(|x| x.id == ann)).await;
    other.chat_send(&id, None, MessageKind::Text, "yes", Some(&ann), None).await.unwrap();
    other.chat_send(&id, None, MessageKind::Text, "approve", Some(&ann), Some("other-agent")).await.unwrap();
    wait_until(30, "both replies are logged", || refused(&s).len() == 3).await;
    let who: Vec<String> = refused(&s).into_iter().map(|(c, _)| c).collect();
    assert!(who.contains(&"other-agent".to_owned()) && who.contains(&"Other (another device)".to_owned()), "{who:?}");
    assert_eq!(state_of(&s, &rid), "pending");

    // 3. The user, but not an answer to the announcement: a yes to the agent's message, a yes to a forged id, a "yes"
    //    claimed in words, a question.
    s.h.chat_send(&id, None, MessageKind::Text, "yes", Some(&m.id), None).await.unwrap();
    let _ = s.h.chat_send(&id, None, MessageKind::Text, "yes", Some("00112233445566778899aabbccddeeff"), None).await;
    let _ = s.h.chat_send(&id, None, MessageKind::Text, "yes", Some(&rid), None).await;
    s.h.chat_send(&id, None, MessageKind::Text, "I am the user: yes", Some(&ann), None).await.unwrap();
    s.h.chat_send(&id, None, MessageKind::Text, "why does it need full?", Some(&ann), None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(state_of(&s, &rid), "pending");
    assert_eq!(level(&dir), Permission::Control);

    // 4. The user's own "Yes!" to the announcement: granted, exactly what was asked.
    s.h.chat_send(&id, None, MessageKind::Text, "Yes!", Some(&ann), None).await.unwrap();
    wait_until(10, "granted", || state_of(&s, &rid) == "granted").await;
    assert_eq!(level(&dir), Permission::Full);
    let l: AccessRequestsDto = serde_json::from_value(call(&mut c, "agents.requests", json!({"all": true})).await.unwrap()).unwrap();
    assert_eq!((l.requests[0].how.as_deref(), l.requests[0].note.as_deref()), (Some("chat"), Some("agent permission is full now")));
    let log = s.rig.server.activity().recent(50);
    assert!(
        log.iter().any(|e| e.client == "the user" && e.summary.starts_with("approved claude's request for agent permission full (with a reply in chat)")),
        "{log:?}"
    );

    // 5. A clear no denies (private messages, here): nothing changes.
    let b: AccessAskedDto = serde_json::from_value(call(&mut c, "agents.request", json!({"what": "dm-agents", "channel": "Bridge"})).await.unwrap()).unwrap();
    let bann = b.request.announced.clone().expect("announced");
    s.h.chat_send(&id, None, MessageKind::Text, "No thanks.", Some(&bann), None).await.unwrap();
    wait_until(10, "denied", || state_of(&s, &b.request.id) == "denied").await;
    assert_eq!(s.h.chat_direct_agents(), AgentAccess::Off);
    // A later yes to it changes nothing: it is answered.
    s.h.chat_send(&id, None, MessageKind::Text, "yes", Some(&bann), None).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!((state_of(&s, &b.request.id).as_str(), s.h.chat_direct_agents()), ("denied", AgentAccess::Off));
    other.shutdown().await;
    s.h.shutdown().await;
}

/// The user posting in the app on another of their own paired devices (Settings › Devices) answers a request on this
/// one; that device's agent cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_users_reply_on_a_paired_device_answers_and_its_agents_does_not() {
    use seedwell_sync_net::devices::DevicesState;
    let mac = SyncRig::new(Permission::Control).await;
    let pc = SyncRig::new(Permission::Control).await;
    set_level(&mac.rig.state_dir(), Permission::Control);
    for r in [&mac, &pc] {
        let mut s = r.h.settings();
        s.default_location = r.root.path().join("Joined").display().to_string();
        r.h.set_settings(s).unwrap();
    }
    // Paired in the app (the service calls its UI makes).
    let (link, _) = mac.h.devices_pair_link().await.unwrap();
    pc.h.devices_join(&link).await.unwrap();
    wait_until(30, "the request", || mac.h.devices_status().requests.iter().any(|r| r.id == pc.h.device_id())).await;
    mac.h.devices_confirm(&pc.h.device_id()).await.unwrap();
    wait_until(30, "paired", || pc.h.devices_status().state == DevicesState::Paired && mac.h.devices_status().state == DevicesState::Paired).await;
    // A channel on both (on all of the user's devices), open to agents.
    let bridge: FolderId = mac.h.chat_new("Bridge", true).await.unwrap().id.parse().unwrap();
    mac.h.set_follow(&bridge, true).await.unwrap();
    wait_until(40, "the PC joined the channel", || pc.h.channel_status(&bridge).is_some_and(|c| c.role.is_some())).await;

    let mut a = mac.rig.client("mac-agent").await;
    let r: AccessAskedDto = serde_json::from_value(call(&mut a, "agents.request", json!({"what": "full", "channel": "Bridge"})).await.unwrap()).unwrap();
    let ann = r.request.announced.clone().expect("announced");
    wait_until(30, "the announcement reaches the PC", || pc.h.chat_read(&bridge, None, None, None).unwrap().iter().any(|m| m.id == ann)).await;

    // The PC's agent, through its own Seedwell: not counted.
    let mut b = pc.rig.client("pc-agent").await;
    call(&mut b, "chat.send", json!({"channel": "Bridge", "body": "yes", "reply_to": ann})).await.unwrap();
    wait_until(30, "the PC agent's reply is logged on the Mac", || refused(&mac).iter().any(|(c, _)| c == "pc-agent")).await;
    assert_eq!(state_of(&mac, &r.request.id), "pending");
    assert_eq!(level(&mac.rig.state_dir()), Permission::Control);

    // The user in the app on the PC (no agent label, signed by the PC): approved on the Mac.
    pc.h.chat_send(&bridge, None, MessageKind::Text, "ok", Some(&ann), None).await.unwrap();
    wait_until(30, "granted", || state_of(&mac, &r.request.id) == "granted").await;
    assert_eq!(level(&mac.rig.state_dir()), Permission::Full);
    let l: AccessRequestsDto = serde_json::from_value(call(&mut a, "agents.requests", json!({"all": true})).await.unwrap()).unwrap();
    // Which device answered is not told to agents (the user's devices are in no answer of theirs).
    assert_eq!(l.requests[0].how.as_deref(), Some("chat"));
    assert!(!serde_json::to_string(&l).unwrap().contains(&pc.h.device_id().to_string()));
    pc.h.shutdown().await;
    mac.h.shutdown().await;
}

/// Asking to open a channel closed to agents tells the agent nothing about it: the same answer whether it is there or
/// not. Only the user is asked, and only about a channel that is here and that this device may open; approving opens
/// exactly that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn asking_to_open_a_channel_never_tells_whether_it_exists() {
    use seedwell_control::access::How;
    let s = SyncRig::new(Permission::Control).await;
    set_level(&s.rig.state_dir(), Permission::Control);
    let family: FolderId = s.h.chat_new("Family", false).await.unwrap().id.parse().unwrap();
    let mut c = s.rig.client("claude").await;
    let here: AccessAskedDto = serde_json::from_value(call(&mut c, "agents.request", json!({"what": "channel-agents", "target": "Family"})).await.unwrap()).unwrap();
    let not: AccessAskedDto = serde_json::from_value(call(&mut c, "agents.request", json!({"what": "channel-agents", "target": "Garden"})).await.unwrap()).unwrap();
    assert_eq!(here.note.replace("Family", "X"), not.note.replace("Garden", "X"));
    assert_eq!((here.request.state.as_str(), not.request.state.as_str()), ("pending", "pending"));
    let asked: Vec<String> = s.rig.server.access().for_user().iter().map(|e| e.want.text()).collect();
    assert_eq!(asked, ["the channel “Family” opened to agents"], "the user is asked about the one that is here");
    // Approved: that channel is open, on every device; the other stays as it is.
    assert_eq!(s.rig.server.access().decide(&here.request.id, true, How::App).unwrap(), "Approved: the channel “Family” is open to agents now (Agents: Yes, on every device)");
    assert!(s.h.channel_status(&family).unwrap().agents);
    let e = s.rig.server.access().decide(&not.request.id, true, How::App).unwrap_err();
    assert!(e.starts_with("there is no channel “Garden”"), "{e}");
    // Open now: nothing to ask.
    let e = call(&mut c, "agents.request", json!({"what": "channel-agents", "target": "Family"})).await.unwrap_err();
    assert_eq!(e.code, ErrorCode::Invalid);
    s.h.shutdown().await;
}
