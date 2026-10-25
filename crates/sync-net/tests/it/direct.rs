//! Private messages between services on loopback (SPEC §7G.7): read only by the recipient, carried sealed by
//! another member, delivered later through it, tampering, restarts, and devices that share no channel.

use trs_sync_net::chat::{AgentAccess, MessageKind};
use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

use crate::chat::{at, join, role, start_at};
use crate::utp::{addr, device_with, until};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_private_message_is_read_only_by_its_recipient_and_carried_sealed_by_others() {
    let (a, b, c) = (device_with("Anna", |_| {}).await, device_with("Bo", |_| {}).await, device_with("Cy", |_| {}).await);
    let (ad, bd) = (a.h.device_id(), b.h.device_id());
    let id: FolderId = a.h.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    join(&c.h, &a.h, &id, Role::ReadOnly, addr(&a)).await;
    // Each learns the others' keys, C's and B's through A.
    until(20, "the keys", || a.h.test_direct_knows(&bd) && b.h.test_direct_knows(&ad) && c.h.test_direct_knows(&bd) && b.h.test_direct_knows(&c.h.device_id())).await;

    let m = a.h.chat_send_direct(&bd, MessageKind::Text, "just us", None, None).await.unwrap();
    assert_eq!((m.direct, m.me, m.channel_id.as_str(), m.peer_device.clone()), (true, true, "", Some(bd.to_string())));
    until(20, "B has it", || b.h.chat_directs().first().is_some_and(|d| d.unread == 1)).await;
    let got = b.h.chat_direct_messages(&ad, None, 10).unwrap().remove(0);
    assert_eq!((got.body.as_str(), got.trust.as_str(), got.author_name.as_str(), got.id.as_str()), ("just us", "untrusted-peer", "Anna", m.id.as_str()));
    assert_eq!(got.peer_device, Some(ad.to_string()));
    // C carries it, and cannot read it.
    until(20, "C carries it", || c.h.test_direct_held() == 1).await;
    assert!(c.h.chat_directs().is_empty(), "nothing C can show");
    assert!(c.h.test_direct_wire()[0].windows(7).all(|w| w != b"just us"), "sealed");

    // An answer, with its agent label; read marks.
    let back = b.h.chat_send_direct(&ad, MessageKind::Json, r#"{"ok":true}"#, Some(&m.id), Some("claude")).await.unwrap();
    until(20, "A has the answer", || a.h.chat_direct_messages(&bd, None, 10).is_ok_and(|v| v.len() == 2)).await;
    let r = a.h.chat_direct_messages(&bd, None, 10).unwrap().remove(1);
    assert_eq!((r.id.as_str(), r.kind.as_str(), r.agent.as_deref(), r.reply_to.as_deref()), (back.id.as_str(), "json", Some("claude"), Some(m.id.as_str())));
    assert_eq!(a.h.chat_direct_messages(&bd, None, 1).unwrap().len(), 1);
    assert_eq!(a.h.chat_direct_messages(&bd, Some(&back.id), 10).unwrap().len(), 1, "a page before the answer");
    b.h.chat_mark_direct_read(&ad).unwrap();
    assert_eq!(b.h.chat_directs()[0].unread, 0);
    assert_eq!(a.h.chat_directs()[0].unread, 1);
    assert!(b.h.chat_send_direct(&ad, MessageKind::Json, "not json", None, None).await.is_err());
    assert!(b.h.chat_send_direct(&bd, MessageKind::Text, "me", None, None).await.is_err(), "not to itself");

    // A device that shares no channel with this one is refused, with a reason.
    let d = device_with("Dee", |_| {}).await;
    let e = a.h.chat_send_direct(&d.h.device_id(), MessageKind::Text, "hi", None, None).await.unwrap_err().to_string();
    assert!(e.contains("shares no channel"), "{e}");
    // The Mac-wide agent switch: off by default.
    assert_eq!(b.h.chat_direct_agents(), AgentAccess::Off);
    b.h.chat_set_direct_agents(AgentAccess::ReadPost).unwrap();
    assert_eq!(b.h.chat_direct_agents(), AgentAccess::ReadPost);
    for x in [&a, &b, &c, &d] {
        x.h.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recipient_that_was_away_gets_its_private_message_through_another_member() {
    let (a, b, c) = (device_with("Anna", |_| {}).await, device_with("Bo", |_| {}).await, device_with("Cy", |_| {}).await);
    let (ad, bd) = (a.h.device_id(), b.h.device_id());
    let id: FolderId = a.h.chat_new("Team").await.unwrap().id.parse().unwrap();
    join(&b.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    join(&c.h, &a.h, &id, Role::ReadWrite, addr(&a)).await;
    until(20, "A has B's key", || a.h.test_direct_knows(&bd)).await;
    // B is away from A; C is online.
    b.h.test_refuse_direct(ad);
    a.h.chat_send_direct(&bd, MessageKind::Text, "while you were out", None, None).await.unwrap();
    until(20, "C carries it", || c.h.test_direct_held() == 1).await;
    a.h.shutdown().await;
    b.h.add_address(&id, addr(&c)).unwrap();
    until(30, "B has it from C", || b.h.chat_direct_messages(&ad, None, 10).is_ok_and(|v| v.len() == 1 && v[0].body == "while you were out")).await;
    assert!(c.h.chat_directs().is_empty());
    b.h.shutdown().await;
    c.h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_tampered_private_message_is_dropped_and_kept_ones_survive_a_restart() {
    let dirs: Vec<tempfile::TempDir> = (0..2).map(|_| tempfile::tempdir().unwrap()).collect();
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    let (ad, bd) = (a.device_id(), b.device_id());
    let id: FolderId = a.chat_new("Team").await.unwrap().id.parse().unwrap();
    b.chat_join(&a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link).await.unwrap();
    b.add_address(&id, at(&a)).unwrap();
    until(30, "B is in", || role(&b, &id).is_some() && a.test_direct_knows(&bd)).await;
    b.test_refuse_direct(ad);
    a.chat_send_direct(&bd, MessageKind::Text, "sealed for Bo", None, None).await.unwrap();
    let wire = a.test_direct_wire().remove(0);
    for at_byte in [40, wire.len() / 2, wire.len() - 1] {
        let mut bad = wire.clone();
        bad[at_byte] ^= 1;
        b.test_direct_receive(ad, &[bad]).await;
    }
    assert_eq!(b.test_direct_held(), 0, "none of the tampered copies is taken");
    b.test_direct_receive(ad, &[wire]).await;
    assert_eq!(b.chat_direct_messages(&ad, None, 10).unwrap()[0].body, "sealed for Bo");

    // Both keep them across a restart: A its own copy, B the one to it (opened again).
    a.shutdown().await;
    b.shutdown().await;
    let a = start_at(dirs[0].path(), "Anna", |_| {}).await;
    let b = start_at(dirs[1].path(), "Bo", |_| {}).await;
    assert_eq!(a.chat_direct_messages(&bd, None, 10).unwrap()[0].body, "sealed for Bo");
    assert_eq!(b.chat_direct_messages(&ad, None, 10).unwrap()[0].body, "sealed for Bo");
    assert_eq!(b.chat_directs()[0].unread, 1);
    assert!(b.test_direct_knows(&ad) && a.test_direct_knows(&bd), "the keys are kept too");
    a.shutdown().await;
    b.shutdown().await;
}
