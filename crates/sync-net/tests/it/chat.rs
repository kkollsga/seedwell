//! Channels between services on loopback (SPEC §7G.7): chat both ways, catching up from another member, who may
//! post, removed members, devices without channels, the wake-up command, and topics.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use trs_sync_net::chat::{AgentAccess, MessageInfo, MessageKind};
use trs_sync_net::secrets::FileSecrets;
use trs_sync_net::{Options, SyncHandle};
use trs_sync_proto::chat::Draft;
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

use crate::utp::{Device, addr, device_with, read, until};

async fn device(name: &str) -> Device {
    device_with(name, |_| {}).await
}

/// A service on a state folder that outlives it.
pub(crate) async fn start_at(state: &Path, name: &str, tune: impl FnOnce(&mut Options)) -> SyncHandle {
    let secrets = Arc::new(FileSecrets::new(state.join("secrets")));
    let mut opts = Options::new(state.to_path_buf(), "Seedwell test".into(), secrets);
    opts.default_device_name = name.into();
    opts.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    opts.lan_discovery = false;
    tune(&mut opts);
    SyncHandle::start(opts).await.unwrap()
}

pub(crate) fn at(h: &SyncHandle) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), h.port())
}

fn texts(h: &SyncHandle, id: &FolderId) -> Vec<String> {
    h.chat_read(id, None, None, None).unwrap_or_default().into_iter().map(|m| m.body).collect()
}

pub(crate) fn role(h: &SyncHandle, id: &FolderId) -> Option<Role> {
    h.channel_status(id).and_then(|c| c.role)
}

/// `who` joins `owner`'s channel `id` at `role`, reaching it at `via`.
pub(crate) async fn join(who: &SyncHandle, owner: &SyncHandle, id: &FolderId, role_: Role, via: SocketAddr) {
    let link = owner.create_invite(id, role_, 1, 7).unwrap().link;
    assert!(link.starts_with("seedwell://chat/"), "{link}");
    let c = who.chat_join(&link).await.unwrap();
    assert!(c.joining && c.role.is_none());
    who.add_address(id, via).unwrap();
    until(30, "admitted", || role(who, id) == Some(role_)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_devices_chat_both_ways_and_a_read_only_member_reads() {
    let (a, b, c) = (device("Anna").await, device("Bo").await, device("Cy").await);
    let ch = a.h.chat_new("Plans").await.unwrap();
    let id: FolderId = ch.id.parse().unwrap();
    assert_eq!((ch.role, ch.members, ch.agents), (Some(Role::Owner), 1, AgentAccess::Off));
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    assert_eq!(b.h.channel_status(&id).unwrap().name, "Plans", "the link's name");

    let hello = a.h.chat_send(&id, None, MessageKind::Text, "hello Bo", None, None).await.unwrap();
    assert_eq!((hello.me, hello.trust.as_str(), hello.agent.as_deref()), (true, "this-device", None));
    until(20, "B has it", || texts(&b.h, &id) == ["hello Bo"]).await;
    let got: MessageInfo = b.h.chat_read(&id, None, None, None).unwrap().remove(0);
    assert_eq!((got.me, got.trust.as_str(), got.author_name.as_str(), got.id.as_str()), (false, "untrusted-peer", "Anna", hello.id.as_str()));
    assert_eq!(got.author_user, "", "no user name given");
    // The user names herself in Settings: every message of hers shows it, the earlier ones too.
    let mut s = a.h.settings();
    s.user_name = "Anna K".into();
    a.h.set_settings(s).unwrap();
    until(20, "B shows the user's name", || b.h.chat_read(&id, None, None, None).is_ok_and(|m| m[0].author_user == "Anna K")).await;
    assert_eq!(a.h.chat_read(&id, None, None, None).unwrap()[0].author_user, "Anna K");
    assert_eq!(b.h.channel_status(&id).unwrap().unread, 1);
    b.h.chat_mark_read(&id, None).unwrap();
    assert_eq!(b.h.channel_status(&id).unwrap().unread, 0);

    // An agent's reply: the label comes from the caller (the service's control socket), signed with it.
    let answer = b.h.chat_send(&id, None, MessageKind::Json, r#"{"ok":true}"#, Some(&hello.id), Some("claude")).await.unwrap();
    until(20, "A has the answer", || texts(&a.h, &id).len() == 2).await;
    let back = a.h.chat_read(&id, None, Some(&hello.id), None).unwrap().remove(0);
    assert_eq!((back.kind.as_str(), back.agent.as_deref(), back.reply_to.as_deref(), back.id.clone()), ("json", Some("claude"), Some(hello.id.as_str()), answer.id));
    assert!(b.h.chat_send(&id, None, MessageKind::Json, "not json", None, None).await.is_err());
    assert_eq!(a.h.chat_messages(&id, None, Some(&back.id), 10).unwrap().len(), 1, "a page before the answer");

    // Read only: reads, cannot post.
    join(&c.h, &a.h, &id, Role::ReadOnly, addr(&a)).await;
    until(20, "C caught up", || texts(&c.h, &id).len() == 2).await;
    let e = c.h.chat_send(&id, None, MessageKind::Text, "me too", None, None).await.unwrap_err().to_string();
    assert!(e.contains("Read only"), "{e}");
    // A message signed by it anyway is not taken.
    let forged = c.h.test_sign(&id, 1, "me too");
    a.h.test_receive(&id, c.h.device_id(), &[forged]).await.unwrap();
    assert_eq!(texts(&a.h, &id).len(), 2);
    let members = a.h.members(&id).unwrap();
    assert_eq!(members.len(), 3);
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_that_was_away_catches_up_from_another_member() {
    let (a, b, c) = (device("Anna").await, device("Bo").await, device("Cy").await);
    let id: FolderId = a.h.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    join(&c.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    // B is away from A: it only reaches C, later.
    b.h.test_refuse_direct(a.h.device_id());
    for n in 1..=3 {
        a.h.chat_send(&id, None, MessageKind::Text, &format!("news {n}"), None, None).await.unwrap();
    }
    until(20, "C has them", || texts(&c.h, &id).len() == 3).await;
    a.h.shutdown().await;
    b.h.add_address(&id, addr(&c)).unwrap();
    until(30, "B caught up from C", || texts(&b.h, &id) == ["news 1", "news 2", "news 3"]).await;
    assert_eq!(b.h.chat_read(&id, None, None, Some(2)).unwrap().len(), 2, "the last two");
    // And what B posts now reaches C, with the owner away.
    b.h.chat_send(&id, None, MessageKind::Text, "back", None, None).await.unwrap();
    until(20, "C has B's message", || texts(&c.h, &id).last().is_some_and(|t| t == "back")).await;
    for d in [&b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removed_members_later_messages_are_ignored_and_its_earlier_ones_stay() {
    let (a, b) = (device("Anna").await, device("Bo").await);
    let id: FolderId = a.h.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    b.h.chat_send(&id, None, MessageKind::Text, "while a member", None, None).await.unwrap();
    until(20, "A has it", || texts(&a.h, &id).len() == 1).await;
    a.h.remove_device(&id, &b.h.device_id()).unwrap();
    until(20, "B knows", || role(&b.h, &id).is_none()).await;
    let st = b.h.channel_status(&id).unwrap();
    assert!(st.error.as_deref().is_some_and(|e| e.contains("removed")), "{st:?}");
    assert!(b.h.chat_send(&id, None, MessageKind::Text, "after", None, None).await.is_err());
    // Signed anyway and passed on by someone: not taken.
    let late = b.h.test_sign(&id, 9, "after");
    a.h.test_receive(&id, b.h.device_id(), &[late]).await.unwrap();
    assert_eq!(texts(&a.h, &id), ["while a member"]);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_device_without_channels_gets_no_messages_but_still_syncs_folders() {
    let dirs: Vec<tempfile::TempDir> = (0..3).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let docs = dirs[2].path().join("A");
    std::fs::create_dir_all(&docs).unwrap();
    std::fs::write(docs.join("seed.txt"), b"x").unwrap();
    let folder: FolderId = a.add_folder(docs.clone()).await.unwrap().id.parse().unwrap();
    b.join_folder(&a.create_invite(&folder, Role::ReadWrite, 1, 7).unwrap().link, dirs[2].path().join("B")).await.unwrap();
    b.add_address(&folder, at(&a)).unwrap();
    let id: FolderId = a.chat_new("Team").await.unwrap().id.parse().unwrap();
    b.chat_join(&a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link).await.unwrap();
    until(30, "B is in both", || role(&b, &id).is_some() && read(&dirs[2].path().join("B/seed.txt")).is_some()).await;

    // B comes back as a Seedwell without the chat feature (0.1.6).
    b.shutdown().await;
    let b = start_at(dirs[1].path(), "Bo", |o| o.features = 0).await;
    assert_eq!((b.snapshot().folders.len(), b.chat_channels().len()), (1, 1), "everything opens again at once");
    b.add_address(&folder, at(&a)).unwrap();
    a.chat_send(&id, None, MessageKind::Text, "anyone?", None, None).await.unwrap();
    std::fs::write(docs.join("after.txt"), b"y").unwrap();
    until(60, "the folder still syncs", || read(&dirs[2].path().join("B/after.txt")).is_some()).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert!(texts(&b, &id).is_empty(), "no channel messages over a session without the feature");
    assert_eq!(a.channel_status(&id).unwrap().online, 1, "B is not online in the channel");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wake_up_command_gets_each_message_from_others_on_stdin() {
    let (a, b) = (device("Anna").await, device("Bo").await);
    let id: FolderId = a.h.chat_new("Agents").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("woken");
    // (cmd.exe on Windows: findstr copies standard input; a command there gets no arguments either.)
    #[cfg(unix)]
    let cmd = format!("cat >> '{}'; echo \"args=$#\" >> '{}'", out.display(), out.display());
    #[cfg(windows)]
    let cmd = format!("findstr \"^\" >> \"{}\" & >> \"{}\" echo args=0", out.display(), out.display());
    b.h.chat_set_wake_command(&id, Some(&cmd)).unwrap();
    // Agents off: nothing runs.
    a.h.chat_send(&id, None, MessageKind::Text, "first", None, None).await.unwrap();
    until(20, "B has it", || texts(&b.h, &id).len() == 1).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(!out.exists(), "agents may not read it: no wake-up");
    b.h.chat_set_agents(&id, AgentAccess::Read).unwrap();
    a.h.chat_send(&id, None, MessageKind::Text, "wake up", None, None).await.unwrap();
    until(20, "woken", || std::fs::read_to_string(&out).is_ok_and(|t| t.contains("args=0"))).await;
    let text = std::fs::read_to_string(&out).unwrap();
    let json: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!((json["body"].as_str(), json["trust"].as_str(), json["me"].as_bool()), (Some("wake up"), Some("untrusted-peer"), Some(false)), "{json}");
    // Its own messages do not wake it.
    b.h.chat_send(&id, None, MessageKind::Text, "mine", None, None).await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(std::fs::read_to_string(&out).unwrap().lines().count(), 2);
    let st = b.h.channel_status(&id).unwrap();
    assert_eq!((st.agents, st.wake_command.as_deref()), (AgentAccess::Read, Some(cmd.as_str())));
    b.h.chat_leave(&id).unwrap();
    assert!(b.h.channel_status(&id).is_none() && b.h.chat_channels().is_empty());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tag_seen_receipt_or_hook_notice_is_no_unread_message() {
    let (a, b) = (device("Anna").await, device("Bo").await);
    let id: FolderId = a.h.chat_new("Koding").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    let t = b.h.chat_new_topic(&id, "kglite", None).await.unwrap();
    for body in ["---\nmail: 1\ntype: seen\nre: 0392ba7c\n---\n", "---\nmail: 1\ntype: hook\nstate: on\n---\nhook on", "---\nmail: 1\ntype: tag\nre: 0392ba7c\ntags: backlog\n---\n", "---\nmail: 1\ntype: bug\n---\nreal work"] {
        b.h.chat_send(&id, Some(&t.id), MessageKind::Text, body, None, Some("kglite")).await.unwrap();
    }
    until(20, "A has all four", || a.h.chat_topics(&id).is_ok_and(|v| v.len() == 1 && v[0].messages == 4)).await;
    assert_eq!(a.h.chat_topics(&id).unwrap()[0].unread, 1, "only the bug is news");
    assert_eq!(a.h.channel_status(&id).unwrap().unread, 1);
    a.h.shutdown().await;
    b.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn topics_hold_their_own_messages_and_unread_counts() {
    let (a, b, c) = (device("Anna").await, device("Bo").await, device("Cy").await);
    let id: FolderId = a.h.chat_new("Family").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    let t = b.h.chat_new_topic(&id, "  Dinner on Saturday ", None).await.unwrap();
    assert_eq!((t.title.as_str(), t.messages, t.unread, t.author_name.as_str()), ("Dinner on Saturday", 0, 0, "Bo"));
    assert!(b.h.chat_new_topic(&id, "  ", None).await.is_err(), "a topic needs a title");
    b.h.chat_send(&id, Some(&t.id), MessageKind::Text, "18:00?", None, None).await.unwrap();
    b.h.chat_send(&id, Some("dinner on saturday"), MessageKind::Text, "I'll book", None, None).await.unwrap();
    b.h.chat_send(&id, None, MessageKind::Text, "unrelated", None, None).await.unwrap();
    until(20, "A has the topic and every message", || {
        a.h.chat_topics(&id).is_ok_and(|v| v.len() == 1 && v[0].messages == 2) && a.h.chat_messages(&id, None, None, 10).is_ok_and(|v| v.len() == 1)
    })
    .await;
    let at_a = a.h.chat_topics(&id).unwrap().remove(0);
    assert_eq!((at_a.id.as_str(), at_a.title.as_str(), at_a.unread, at_a.author_name.as_str()), (t.id.as_str(), "Dinner on Saturday", 2, "Bo"));
    let inside = a.h.chat_messages(&id, Some(&t.id[..6]), None, 10).unwrap();
    assert_eq!(inside.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["18:00?", "I'll book"]);
    assert!(inside.iter().all(|m| m.topic.as_deref() == Some(t.id.as_str()) && m.topic_title.as_deref() == Some("Dinner on Saturday")));
    assert_eq!(a.h.chat_messages(&id, None, None, 10).unwrap()[0].topic, None);
    assert_eq!(a.h.channel_status(&id).unwrap().unread, 3, "the channel's total: General's and the topic's");
    a.h.chat_mark_read(&id, Some(&t.id)).unwrap();
    assert_eq!(a.h.chat_topics(&id).unwrap()[0].unread, 0);
    assert_eq!(a.h.channel_status(&id).unwrap().unread, 1, "General's is left");
    a.h.chat_mark_read(&id, Some("general")).unwrap();
    assert_eq!(a.h.channel_status(&id).unwrap().unread, 0);
    // `chat_read`: every message (the new topic too), or one topic's.
    let all = a.h.chat_read(&id, None, None, None).unwrap();
    assert_eq!(all.iter().map(|m| m.kind.as_str()).collect::<Vec<_>>(), ["topic", "text", "text", "text"]);
    assert_eq!((all[0].id.as_str(), all[0].body.as_str()), (t.id.as_str(), "Dinner on Saturday"));
    assert_eq!(a.h.chat_read(&id, Some(None), None, None).unwrap().len(), 1);
    assert_eq!(a.h.chat_read(&id, Some(Some(&t.id)), None, None).unwrap().len(), 2);
    assert!(a.h.chat_messages(&id, Some("no such topic"), None, 10).is_err());
    assert!(a.h.chat_send(&id, Some("abcdef0123"), MessageKind::Text, "x", None, None).await.is_err());
    // Read only members read topics but cannot make one.
    join(&c.h, &a.h, &id, Role::ReadOnly, addr(&a)).await;
    until(20, "C has the topic", || c.h.chat_topics(&id).is_ok_and(|v| v.len() == 1 && v[0].messages == 2)).await;
    assert!(c.h.chat_new_topic(&id, "Mine", None).await.is_err());
    for d in [&a, &b, &c] {
        d.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_message_in_an_unknown_topic_shows_under_general_until_the_topic_arrives() {
    let (a, b) = (device("Anna").await, device("Bo").await);
    let id: FolderId = a.h.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let tid = [7u8; 16];
    let draft = |id: [u8; 16], seq: u64, topic: Option<[u8; 16]>, kind: MessageKind, body: &str| Draft { id, seq, at_ms: now + seq, reply_to: None, topic, agent: None, kind, body: body.as_bytes().to_vec() };
    let topic = b.h.test_sign_draft(&id, draft(tid, 1, None, MessageKind::Topic, "Later"));
    let early = b.h.test_sign_draft(&id, draft([8; 16], 2, Some(tid), MessageKind::Text, "early"));
    a.h.test_receive(&id, b.h.device_id(), &[early]).await.unwrap();
    let general = a.h.chat_messages(&id, None, None, 10).unwrap();
    assert_eq!((general.len(), general[0].body.as_str(), general[0].topic.as_deref()), (1, "early", None));
    assert!(a.h.chat_topics(&id).unwrap().is_empty());
    a.h.test_receive(&id, b.h.device_id(), &[topic]).await.unwrap();
    assert!(a.h.chat_messages(&id, None, None, 10).unwrap().is_empty(), "it moved into its topic");
    let t = a.h.chat_topics(&id).unwrap();
    assert_eq!((t.len(), t[0].title.as_str(), t[0].messages, t[0].unread), (1, "Later", 1, 1));
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// A watcher of the whole channel gets a new topic and the messages in it as they arrive from another device (an
/// agent that watches a channel must not miss its topics).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_watcher_gets_a_new_topic_and_its_messages_from_another_device() {
    let (a, b) = (device("Anna").await, device("Bo").await);
    let id: FolderId = a.h.chat_new("Dev").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    let mut events = a.h.chat_subscribe();
    let t = b.h.chat_new_topic(&id, "Seedwell", Some("Seedwell")).await.unwrap();
    b.h.chat_send(&id, Some(&t.id), MessageKind::Text, "in the thread", None, Some("Seedwell")).await.unwrap();
    b.h.chat_send(&id, None, MessageKind::Text, "in General", None, None).await.unwrap();
    let mut got = Vec::new();
    let end = tokio::time::Instant::now() + Duration::from_secs(20);
    while got.len() < 3 && tokio::time::Instant::now() < end {
        if let Ok(Ok(e)) = tokio::time::timeout(Duration::from_millis(500), events.recv()).await {
            got.push((e.message.kind.clone(), e.message.body.clone(), e.message.topic.clone()));
        }
    }
    assert_eq!(
        got,
        vec![
            ("topic".to_string(), "Seedwell".to_string(), None),
            ("text".to_string(), "in the thread".to_string(), Some(t.id.clone())),
            ("text".to_string(), "in General".to_string(), None),
        ]
    );
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// An agent's place (`chat_read_unread`): a first look gives the newest, then only what came after; never its own
/// posts; each agent and thread has its own place, and it is kept across a restart. Short ids answer messages.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_agent_reads_only_what_it_has_not_been_given() {
    let state = tempfile::tempdir().unwrap();
    let a = start_at(state.path(), "Anna", |_| {}).await;
    let id: FolderId = a.chat_new("Dev").await.unwrap().id.parse().unwrap();
    let body = |v: Vec<MessageInfo>| v.into_iter().map(|m| m.body).collect::<Vec<_>>();
    for n in 1..=3 {
        a.chat_send(&id, None, MessageKind::Text, &format!("old {n}"), None, Some("Docs")).await.unwrap();
    }
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 2, false).unwrap()), ["old 2", "old 3"], "a first look: the newest");
    assert!(a.chat_read_unread(&id, None, "Seedwell", 50, false).unwrap().is_empty(), "then nothing new");
    let q = a.chat_send(&id, None, MessageKind::Text, "a question", None, Some("Docs")).await.unwrap();
    a.chat_send(&id, None, MessageKind::Text, "my own post", None, Some("Seedwell")).await.unwrap();
    a.chat_send(&id, None, MessageKind::Text, "an answer", Some(&q.id[..8]), Some("Seedwell")).await.unwrap();
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 50, false).unwrap()), ["a question"], "its own posts are left out");
    assert_eq!(body(a.chat_read_unread(&id, None, "Docs", 50, false).unwrap()), ["my own post", "an answer"], "another agent's place");
    let answer = a.chat_read(&id, None, None, Some(1)).unwrap().remove(0);
    assert_eq!(answer.reply_to.as_deref(), Some(q.id.as_str()), "an 8-character id is enough to answer");
    assert!(a.chat_send(&id, None, MessageKind::Text, "x", Some("0123"), None).await.is_err(), "fewer than 8 is not");
    // More than asked for: the oldest first, and none skipped (a `watch --unread --count 1`, twice).
    for n in 1..=3 {
        a.chat_send(&id, None, MessageKind::Text, &format!("backlog {n}"), None, Some("Docs")).await.unwrap();
    }
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 1, false).unwrap()), ["backlog 1"]);
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 1, false).unwrap()), ["backlog 2"]);
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 5, false).unwrap()), ["backlog 3"]);
    assert!(a.chat_read_unread(&id, None, "Seedwell", 5, false).unwrap().is_empty());
    // A peek says what is unread and leaves the place.
    a.chat_send(&id, None, MessageKind::Text, "peeked", None, Some("Docs")).await.unwrap();
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 5, true).unwrap()), ["peeked"]);
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 5, true).unwrap()), ["peeked"], "still unread");
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 5, false).unwrap()), ["peeked"]);
    assert!(a.chat_read_unread(&id, None, "Seedwell", 5, true).unwrap().is_empty());
    // A thread has a place of its own.
    let t = a.chat_new_topic(&id, "Seedwell", Some("Seedwell")).await.unwrap();
    a.chat_send(&id, Some(&t.id), MessageKind::Text, "in the thread", None, Some("Docs")).await.unwrap();
    assert_eq!(body(a.chat_read_unread(&id, Some(Some(&t.id)), "Seedwell", 50, false).unwrap()), ["in the thread"]);
    assert!(a.chat_read_unread(&id, Some(Some(&t.id)), "Seedwell", 50, false).unwrap().is_empty());
    a.shutdown().await;
    // Kept across a restart.
    let a = start_at(state.path(), "Anna", |_| {}).await;
    a.chat_send(&id, None, MessageKind::Text, "after the restart", None, Some("Docs")).await.unwrap();
    // The whole channel is a place of its own: the thread's message is new there too.
    assert_eq!(body(a.chat_read_unread(&id, None, "Seedwell", 50, false).unwrap()), ["in the thread", "after the restart"], "the place survives");
    a.shutdown().await;
}

/// An agent's place follows arrival: a message from another device that comes in late, though written before
/// messages the agent was given already, is still given (the lost "please add eta_minutes" of the blind test).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_message_written_earlier_is_still_given_to_an_agent() {
    let (a, b) = (device("Anna").await, device("Bo").await);
    let id: FolderId = a.h.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let body = |v: Vec<MessageInfo>| v.into_iter().map(|m| m.body).collect::<Vec<_>>();
    a.h.chat_send(&id, None, MessageKind::Text, "first", None, Some("docs")).await.unwrap();
    assert_eq!(body(a.h.chat_read_unread(&id, None, "api", 50, false).unwrap()), ["first"]);
    let late = b.h.test_sign_draft(&id, Draft { id: [9; 16], seq: 500, at_ms: now - 60_000, reply_to: None, topic: None, agent: Some("web".into()), kind: MessageKind::Text, body: b"please add eta_minutes".to_vec() });
    a.h.test_receive(&id, b.h.device_id(), &[late]).await.unwrap();
    a.h.chat_send(&id, None, MessageKind::Text, "second", None, Some("docs")).await.unwrap();
    assert_eq!(body(a.h.chat_read_unread(&id, None, "api", 50, false).unwrap()), ["please add eta_minutes", "second"], "in the order they came in");
    assert!(a.h.chat_read_unread(&id, None, "api", 50, false).unwrap().is_empty());
    a.h.shutdown().await;
    b.h.shutdown().await;
}

/// What a device keeps of another lives in the folders and channels they share (SPEC §7G.1 *Devices*): leaving one
/// keeps it in the others, leaving the last forgets the other device in `sync.toml`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn leaving_the_last_shared_channel_forgets_the_other_device() {
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let mut ids = Vec::new();
    for name in ["Team", "Other"] {
        let id: FolderId = a.chat_new(name).await.unwrap().id.parse().unwrap();
        join(&b, &a, &id, Role::ReadWrite, at(&a)).await;
        ids.push(id);
    }
    let anna = a.device_id().to_string();
    let toml = || std::fs::read_to_string(dirs[1].path().join("sync.toml")).unwrap();
    until(20, "Anna is remembered in both channels", || toml().matches(&anna).count() == 2).await;
    assert!(!toml().contains("[[device]]"), "no list outside the channels:\n{}", toml());

    b.chat_leave(&ids[0]).unwrap();
    assert_eq!(toml().matches(&anna).count(), 1, "still met in Other");
    b.chat_leave(&ids[1]).unwrap();
    assert!(!toml().contains(&anna), "forgotten:\n{}", toml());
    b.shutdown().await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    assert!(b.chat_channels().is_empty() && !toml().contains(&anna));
    a.shutdown().await;
    b.shutdown().await;
}
