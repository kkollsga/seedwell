//! Private messages between services on loopback (SPEC §7G.7): only between the two devices, over a direct session
//! between them; never carried or relayed by another; tampering; restarts; a conversation that outlives the channel
//! it started in, and one deleted.

use trs_sync_net::chat::{AgentAccess, MessageKind};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

use crate::chat::{at, join, role, start_at};
use crate::utp::{addr, device_with, until};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_message_goes_only_to_its_recipient_and_no_other_member_gets_it() {
    let (a, b, c) = (device_with("Anna", |_| {}).await, device_with("Bo", |_| {}).await, device_with("Cy", |_| {}).await);
    let (ad, bd) = (a.h.device_id(), b.h.device_id());
    let id: FolderId = a.h.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    join(&c.h, &a.h, &id, Role::ReadOnly, addr(&a)).await;
    b.h.add_address(&id, addr(&c)).unwrap();
    // Each has the key of the devices it is connected to directly, from them.
    until(20, "the keys", || a.h.test_direct_knows(&bd) && b.h.test_direct_knows(&ad) && c.h.test_direct_knows(&bd)).await;

    let m = a.h.chat_send_direct(&bd, MessageKind::Text, "just us", None, None).await.unwrap();
    assert_eq!((m.direct, m.me, m.channel_id.as_str(), m.peer_device.clone()), (true, true, "", Some(bd.to_string())));
    until(20, "B has it", || b.h.chat_directs().first().is_some_and(|d| d.unread == 1)).await;
    let got = b.h.chat_direct_messages(&ad, None, 10).unwrap().remove(0);
    assert_eq!((got.body.as_str(), got.trust.as_str(), got.author_name.as_str(), got.id.as_str()), ("just us", "untrusted-peer", "Anna", m.id.as_str()));
    assert_eq!(got.peer_device, Some(ad.to_string()));
    assert!(b.h.chat_directs()[0].online, "connected directly");
    // C, connected to both, gets nothing: no device carries another's private messages.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(c.h.test_direct_held(), 0);
    assert!(c.h.chat_directs().is_empty());

    // An answer, with its agent label; read marks.
    let back = b.h.chat_send_direct(&ad, MessageKind::Json, r#"{"ok":true}"#, Some(&m.id), Some("claude")).await.unwrap();
    until(20, "A has the answer", || a.h.chat_direct_messages(&bd, None, 10).is_ok_and(|v| v.len() == 2)).await;
    let r = a.h.chat_direct_messages(&bd, None, 10).unwrap().remove(1);
    assert_eq!((r.id.as_str(), r.kind.as_str(), r.agent.as_deref(), r.reply_to.as_deref()), (back.id.as_str(), "json", Some("claude"), Some(m.id.as_str())));
    assert_eq!(a.h.chat_direct_messages(&bd, Some(&back.id), 10).unwrap().len(), 1, "a page before the answer");
    b.h.chat_mark_direct_read(&ad).unwrap();
    assert_eq!(b.h.chat_directs()[0].unread, 0);
    assert_eq!(a.h.chat_directs()[0].unread, 1);
    assert!(b.h.chat_send_direct(&ad, MessageKind::Json, "not json", None, None).await.is_err());
    assert!(b.h.chat_send_direct(&bd, MessageKind::Text, "me", None, None).await.is_err(), "not to itself");

    // Not connected directly: refused, and nothing is kept to send later.
    b.h.test_refuse_direct(ad);
    until(20, "A and B apart", || !a.h.chat_directs()[0].online).await;
    let e = a.h.chat_send_direct(&bd, MessageKind::Text, "later", None, None).await.unwrap_err().to_string();
    assert!(e.contains("not connected to this Mac directly"), "{e}");
    assert_eq!(a.h.chat_direct_messages(&bd, None, 10).unwrap().len(), 2);

    // A device that shares no channel and no conversation with this one is refused, with a reason.
    let d = device_with("Dee", |_| {}).await;
    let e = a.h.chat_send_direct(&d.h.device_id(), MessageKind::Text, "hi", None, None).await.unwrap_err().to_string();
    assert!(e.contains("no conversation with that device"), "{e}");
    // The Mac-wide agent switch: off by default.
    assert_eq!(b.h.chat_direct_agents(), AgentAccess::Off);
    b.h.chat_set_direct_agents(AgentAccess::ReadPost).unwrap();
    assert_eq!(b.h.chat_direct_agents(), AgentAccess::ReadPost);
    for x in [&a, &b, &c, &d] {
        x.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tampered_or_forwarded_private_message_is_dropped_and_kept_ones_survive_a_restart() {
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let (ad, bd) = (a.device_id(), b.device_id());
    let id: FolderId = a.chat_new("Team").await.unwrap().id.parse().unwrap();
    b.chat_join(&a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link).await.unwrap();
    b.add_address(&id, at(&a)).unwrap();
    until(30, "B is in", || role(&b, &id).is_some() && a.test_direct_knows(&bd)).await;
    let wire = a.test_direct_seal(&bd, 40, "sealed for Bo").unwrap();
    for at_byte in [40, wire.len() / 2, wire.len() - 1] {
        let mut bad = wire.clone();
        bad[at_byte] ^= 1;
        b.test_direct_receive(ad, &[bad]).await;
    }
    assert_eq!(b.test_direct_held(), 0, "none of the tampered copies is taken");
    b.test_direct_receive(ad, std::slice::from_ref(&wire)).await;
    assert_eq!(b.chat_direct_messages(&ad, None, 10).unwrap()[0].body, "sealed for Bo");
    a.chat_send_direct(&bd, MessageKind::Text, "the plans for friday", None, None).await.unwrap();
    until(20, "B has the second", || b.test_direct_held() == 2).await;

    // Both keep them across a restart: A its own copy, B those to it (opened again).
    a.shutdown().await;
    b.shutdown().await;
    // On disk, no device's files hold the text (SPEC §7G.7 *On disk*).
    for d in &dirs {
        for f in std::fs::read_dir(d.path().join("sync")).unwrap() {
            let bytes = std::fs::read(f.unwrap().path()).unwrap();
            for text in [&b"sealed for Bo"[..], b"plans for friday"] {
                assert!(!bytes.windows(text.len()).any(|w| w == text), "{} in the clear", String::from_utf8_lossy(text));
            }
        }
    }
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    assert_eq!(a.chat_direct_messages(&bd, None, 10).unwrap()[0].body, "the plans for friday", "A keeps only what it sent");
    let mine: Vec<String> = b.chat_direct_messages(&ad, None, 10).unwrap().into_iter().map(|m| m.body).collect();
    assert_eq!(mine, ["sealed for Bo", "the plans for friday"]);
    assert_eq!(b.chat_directs()[0].unread, 2);
    a.shutdown().await;
    b.shutdown().await;
}

/// A conversation is a container of its own: once started in a channel it outlives leaving the channel (the two
/// reach each other for it alone), and deleting it forgets the other device.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conversation_outlives_its_channel_until_it_is_deleted() {
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let (ad, bd) = (a.device_id(), b.device_id());
    let id: FolderId = a.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b, &a, &id, Role::ReadWrite, at(&a)).await;
    until(20, "B has A's key", || b.test_direct_knows(&ad)).await;
    b.chat_send_direct(&ad, MessageKind::Text, "hi Anna", None, None).await.unwrap();
    until(20, "A has it", || a.chat_direct_messages(&bd, None, 10).is_ok_and(|v| v.len() == 1)).await;

    // B leaves the channel and starts again: the conversation alone brings them together (B dials A where it
    // last reached it).
    b.chat_leave(&id).unwrap();
    b.shutdown().await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    assert!(b.chat_channels().is_empty());
    until(30, "connected for the conversation", || b.chat_directs().first().is_some_and(|d| d.online) && b.test_direct_knows(&ad)).await;
    b.chat_send_direct(&ad, MessageKind::Text, "still here", None, None).await.unwrap();
    until(20, "A has the second", || a.chat_direct_messages(&bd, None, 10).is_ok_and(|v| v.len() == 2)).await;
    let toml = || std::fs::read_to_string(dirs[1].path().join("sync.toml")).unwrap();
    assert!(toml().contains("[[conversation]]") && toml().contains(&ad.to_string()));

    // Deleted on B: its messages, their key and all B knew of A through it go, and the session ends.
    b.chat_delete_direct(&ad).unwrap();
    assert!(b.chat_directs().is_empty());
    assert!(!toml().contains(&ad.to_string()), "forgotten:\n{}", toml());
    assert!(!dirs[1].path().join("sync").join(format!("dm-{ad}.chat")).exists());
    assert!(!std::fs::read_to_string(dirs[1].path().join("secrets")).unwrap().contains(&format!("store-dm-{ad}")));
    until(20, "apart", || !a.chat_directs()[0].online).await;
    let e = a.chat_send_direct(&bd, MessageKind::Text, "hello?", None, None).await.unwrap_err().to_string();
    assert!(e.contains("not connected to this Mac directly"), "{e}");
    assert_eq!(a.chat_direct_messages(&bd, None, 10).unwrap().len(), 2, "A keeps its copy");
    a.shutdown().await;
    b.shutdown().await;
}
