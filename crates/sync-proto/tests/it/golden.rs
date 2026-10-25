//! The wire format as 0.1.6 writes it, frozen: every message type, statement kind, invitation link and proof, as
//! bytes (hex), channels included (types 17 to 22: channel messages and topics, a channel link, private messages
//! and the keys they are sealed to). Each decodes to its value and
//! encodes back to the same bytes, so a change to the format fails here first. 0.1.6 is the baseline every later
//! version must still read (`PROTOCOL.md`): these vectors only ever get new entries, never edits.
//!
//! `SEEDWELL_GOLDEN_PRINT=1 cargo test golden -- --nocapture` prints the table for a new entry.
//!
//! Then the rules for what later versions may send: appended fields, records that grew, kinds and values this
//! version does not know, and a peer with a newer protocol and features.

use trs_sync_proto::chat::{AuthorHave, ChatMessage, Draft, MessageKind};
use trs_sync_proto::direct::{DirectBody, DirectDraft, DirectKey, DirectMessage, secret_of};
use trs_sync_proto::codec::{DecodeError, Frame, Reader};
use trs_sync_proto::device::DeviceKey;
use trs_sync_proto::ids::{DeviceId, FolderId, ShortId};
use trs_sync_proto::keys::{FolderSecret, KeyError, Proof};
use trs_sync_proto::members::{
    Body, Invite, InviteLink, Membership, Role, Signer, Statement, check_redeem, new_invite, redeem_proof,
};
use trs_sync_proto::model::{ChunkRef, Counter, FileInfo, FileKind, IndexEntry, SignedFile, Version};
use trs_sync_proto::msg::*;

const T: u64 = 1_793_491_200; // 2026-11-01

fn secret() -> FolderSecret {
    FolderSecret::from_bytes([1; 32])
}

fn dev(n: u8) -> DeviceKey {
    DeviceKey::from_seed(&[n; 32])
}

fn fid() -> FolderId {
    FolderId::of_public_key(secret().public_key())
}

fn version(pairs: &[(u64, u64)]) -> Version {
    Version::from_counters(pairs.iter().map(|&(id, value)| Counter { id: ShortId(id), value }).collect()).unwrap()
}

fn invite(issuer: Option<&DeviceKey>, role: Role, n: u8) -> Invite {
    let s = secret();
    let who = issuer.map_or(Signer::Folder, |k| Signer::Device(*k.public_key()));
    let sign = |m: &[u8]| match issuer {
        None => s.sign(m),
        Some(k) => k.sign(m),
    };
    new_invite(*s.public_key(), [n; 16], [n; 32], role, 1, T + 7 * 86_400, T, who, &sign)
}

/// One of each statement kind, and an admin's grant.
fn statements() -> Vec<(&'static str, Statement)> {
    let s = secret();
    let admin = dev(2);
    vec![
        ("statement 1 grant", Statement::by_owner(&s, Body::Grant { device: *dev(3).public_key(), role: Role::Owner, serial: 1, at_s: T, invite: None })),
        (
            "statement 1 grant by an admin",
            Statement::by_device(&admin, &fid(), Body::Grant { device: *dev(4).public_key(), role: Role::ReadWrite, serial: 0, at_s: T, invite: Some(invite(Some(&admin), Role::ReadWrite, 5)) }),
        ),
        ("statement 2 remove", Statement::by_owner(&s, Body::Remove { device: *dev(4).public_key(), serial: 3, at_s: T + 1 })),
        ("statement 3 invite", Statement::by_owner(&s, Body::Invite(invite(None, Role::ReadOnly, 6)))),
        ("statement 4 revoke", Statement::by_device(&admin, &fid(), Body::Revoke { id: [6; 16], at_s: T + 2 })),
        ("statement 5 share until", Statement::by_owner(&s, Body::ShareUntil { until_s: T + 30 * 86_400, serial: 4, at_s: T })),
    ]
}

fn entry(seq: u64, path: &str, kind: FileKind, deleted: bool) -> IndexEntry {
    let file = kind == FileKind::File && !deleted;
    let info = FileInfo {
        path: path.into(),
        kind,
        size: if file { 5_000_000 } else { 0 },
        mtime_ns: 1_793_491_200_123_456_789,
        exec: path.ends_with(".sh"),
        deleted,
        version: version(&[(0x1122_3344_5566_7788, T), (0x99aa_bbcc_ddee_ff00, 3)]),
        modified_by: ShortId(0x1122_3344_5566_7788),
        file_id: [seq as u8; 16],
        symlink_target: if kind == FileKind::Symlink { "../target".into() } else { String::new() },
        conflict_of: path.contains("conflict").then(|| "Notes/plan.txt".into()),
        chunks: if file { vec![ChunkRef { len: 4_194_304, hash: [0xc1; 32] }, ChunkRef { len: 805_696, hash: [0xc2; 32] }] } else { vec![] },
    };
    IndexEntry { sequence: seq, invalid: seq == 4, file: SignedFile { info, sig: [seq as u8 + 0x40; 64] } }
}

/// One of each message type.
fn messages() -> Vec<(&'static str, Message)> {
    let a = dev(7);
    let st = statements().into_iter().map(|(_, s)| s).collect::<Vec<_>>();
    vec![
        ("message 1 hello", Message::Hello(Hello { protocol: 2, client: "Seedwell 0.1.6".into(), device_name: "Kari's MacBook".into(), min_protocol: 2, features: 0 })),
        (
            "message 2 join",
            Message::Join(Join {
                folders: vec![JoinFolder {
                    folder_id: fid(),
                    proofs: vec![Proof::Grant(st[0].to_bytes()), Proof::Invite(redeem()), Proof::Unknown { kind: 9, data: vec![1, 2] }],
                    my_index_id: 0x0102_0304_0506_0708,
                    your_index_id: 77,
                    your_max_seq: 1234,
                }],
            }),
        ),
        ("message 3 members", Message::Members(Members { folder_id: fid(), statements: st })),
        (
            "message 4 index",
            Message::Index(Index {
                folder_id: fid(),
                index_id: 0x0a0b_0c0d,
                full: true,
                entries: vec![
                    entry(1, "Notes/plan.txt", FileKind::File, false),
                    entry(2, "Notes", FileKind::Dir, false),
                    entry(3, "Notes/link", FileKind::Symlink, false),
                    entry(4, "Notes/old.sh", FileKind::File, true),
                    entry(5, "Notes/plan (conflict).txt", FileKind::File, false),
                ],
                done: true,
            }),
        ),
        (
            "message 5 request",
            Message::Request(Request { id: 9, folder_id: fid(), path: "Notes/plan.txt".into(), offset: 4_194_304, len: 805_696, hash: [0xc2; 32], from_temp: true }),
        ),
        ("message 6 response", Message::Response(Response { id: 9, code: ResponseCode::Ok, data: b"chunk bytes".to_vec() })),
        ("message 6 response busy", Message::Response(Response { id: 10, code: ResponseCode::Busy, data: vec![] })),
        (
            "message 7 progress",
            Message::Progress(Progress { folder_id: fid(), updates: vec![ProgressUpdate { path: "Big.mov".into(), version: version(&[(5, T)]), have: vec![0, 1, 7] }] }),
        ),
        (
            "message 8 addresses",
            Message::Addresses(Addresses { records: vec![AddressRecord::sign(&a, vec!["192.0.2.1:22100".into(), "utp://192.0.2.1:22100".into()], "relay://198.51.100.9:22067#ABC".into(), T)] }),
        ),
        ("message 9 ping", Message::Ping),
        ("message 10 close", Message::Close(Close { reason: "no folders in common".into() })),
        ("message 11 rendezvous", Message::Rendezvous(Rendezvous { target: DeviceId([0x21; 32]), candidates: vec!["203.0.113.5:40000".into()] })),
        ("message 12 introduce", Message::Introduce(Introduce { from: DeviceId([0x22; 32]), candidates: vec!["203.0.113.6:40001".into()], punch_at_ms: T * 1000 + 250 })),
        ("message 13 relay open", Message::RelayOpen(RelayOpen { target: DeviceId([0x23; 32]), session: 3 })),
        ("message 14 relay data", Message::RelayData(RelayData { session: 3, data: vec![0, 1, 2, 3] })),
        ("message 15 relay close", Message::RelayClose(RelayClose { session: 3 })),
        ("message 16 owner key", Message::OwnerKey(OwnerKey { folder_id: fid(), secret: [0x5e; 32] })),
        (
            "message 17 chat join",
            Message::ChatJoin(ChatJoin {
                channels: vec![JoinChannel { channel_id: fid(), proofs: vec![Proof::Grant(statements()[0].1.to_bytes()), Proof::Invite(redeem())] }],
            }),
        ),
        (
            "message 18 chat have",
            Message::ChatHave(ChatHave {
                channel_id: fid(),
                floor_at_ms: T * 1000 - 86_400_000,
                authors: vec![AuthorHave::of(*dev(3).public_key(), [1, 2, 3, 7]), AuthorHave::of(*a.public_key(), [5])],
            }),
        ),
        ("message 19 chat messages", Message::ChatMessages(ChatMessages { channel_id: fid(), messages: chat().into_iter().map(|(_, m)| m).collect() })),
        ("message 20 direct keys", Message::DirectKeys(DirectKeys { records: vec![direct_key(3), direct_key(7)] })),
        ("message 21 direct have", Message::DirectHave(DirectHave { authors: vec![AuthorHave::of(*dev(3).public_key(), [1, 2])] })),
        ("message 22 direct messages", Message::DirectMessages(DirectMessages { messages: direct().into_iter().map(|(_, m)| m).collect() })),
    ]
}

/// A device's key for private messages.
fn direct_key(n: u8) -> DirectKey {
    DirectKey::of(&dev(n), T * 1000 + n as u64)
}

/// A private message each way between dev(3) and dev(7): text, and an agent's JSON answer to it.
fn direct() -> Vec<(&'static str, DirectMessage)> {
    let (anna, agent) = (dev(3), dev(7));
    let seal = |from: &DeviceKey, to: u8, d: DirectDraft| DirectMessage::seal(from, dev(to).public_key(), &direct_key(to).x25519, d).unwrap();
    vec![
        (
            "direct 1 text",
            seal(
                &anna,
                7,
                DirectDraft {
                    id: [0x31; 16],
                    seq: 1,
                    at_ms: T * 1000 + 10,
                    body: DirectBody { reply_to: None, agent: None, kind: MessageKind::Text, body: "Bare oss to?".as_bytes().to_vec() },
                    ephemeral_secret: [0x41; 32],
                },
            ),
        ),
        (
            "direct 2 json from an agent",
            seal(
                &agent,
                3,
                DirectDraft {
                    id: [0x32; 16],
                    seq: 1,
                    at_ms: T * 1000 + 20,
                    body: DirectBody { reply_to: Some([0x31; 16]), agent: Some("claude-code".into()), kind: MessageKind::Json, body: br#"{"ok":true}"#.to_vec() },
                    ephemeral_secret: [0x42; 32],
                },
            ),
        ),
    ]
}

/// One message of each kind: text, an agent's JSON answer, a kind of a later version, a new topic and a message in
/// it.
fn chat() -> Vec<(&'static str, ChatMessage)> {
    let (anna, agent) = (dev(3), dev(7));
    let ch = fid();
    let sign = |k: &DeviceKey, d: Draft| ChatMessage::sign(k, &ch, d).unwrap();
    vec![
        (
            "chat 1 text",
            sign(&anna, Draft { id: [0x11; 16], seq: 1, at_ms: T * 1000 + 5, reply_to: None, topic: None, agent: None, kind: MessageKind::Text, body: "Middag kl. 18?".as_bytes().to_vec() }),
        ),
        (
            "chat 2 json from an agent",
            sign(
                &agent,
                Draft {
                    id: [0x12; 16],
                    seq: 4,
                    at_ms: T * 1000 + 900,
                    reply_to: Some([0x11; 16]),
                    topic: None,
                    agent: Some("claude-code".into()),
                    kind: MessageKind::Json,
                    body: br#"{"answer":"yes","confidence":0.9}"#.to_vec(),
                },
            ),
        ),
        (
            "chat 3 later kind",
            sign(&anna, Draft { id: [0x13; 16], seq: 2, at_ms: T * 1000 + 1200, reply_to: Some([0x12; 16]), topic: None, agent: None, kind: MessageKind::Unknown(9), body: vec![0x12, 0x01] }),
        ),
        (
            "chat 4 topic",
            sign(&anna, Draft { id: [0x14; 16], seq: 3, at_ms: T * 1000 + 1500, reply_to: None, topic: None, agent: None, kind: MessageKind::Topic, body: "Lørdag".as_bytes().to_vec() }),
        ),
        (
            "chat 5 in a topic",
            sign(&agent, Draft { id: [0x15; 16], seq: 5, at_ms: T * 1000 + 1800, reply_to: None, topic: Some([0x14; 16]), agent: None, kind: MessageKind::Text, body: "Bord for 4 kl. 18".as_bytes().to_vec() }),
        ),
    ]
}

fn chat_link() -> InviteLink {
    InviteLink { invite: invite(None, Role::ReadWrite, 9), token_seed: [9; 32], name: Some("Middag".into()) }
}

/// The channel a folder's link also invites to (`format_bundle`): another channel than [`chat_link`]'s folder key.
fn bundled_channel() -> InviteLink {
    let ch = FolderSecret::from_bytes([2; 32]);
    let invite = new_invite(*ch.public_key(), [10; 16], [10; 32], Role::ReadWrite, 1, T + 7 * 86_400, T, Signer::Folder, &|m: &[u8]| ch.sign(m));
    InviteLink { invite, token_seed: [10; 32], name: Some("Family Photos chat".into()) }
}

fn link() -> InviteLink {
    InviteLink { invite: invite(None, Role::ReadWrite, 8), token_seed: [8; 32], name: Some("Family Photos".into()) }
}

fn redeem() -> Vec<u8> {
    redeem_proof(&invite(None, Role::ReadWrite, 8), &[8; 32], dev(9).public_key(), &[0xee; 32])
}

fn encode(m: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    m.encode(&mut out);
    out
}

fn decode(b: &[u8]) -> Message {
    let (f, used) = Frame::split(b).unwrap().unwrap();
    assert_eq!(used, b.len());
    Message::decode(f).unwrap()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// Every vector as this build writes it.
fn written() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = messages().iter().map(|(n, m)| (n.to_string(), hex(&encode(m)))).collect();
    out.extend(statements().iter().map(|(n, s)| (n.to_string(), hex(&s.to_bytes()))));
    out.push(("link".into(), link().format()));
    out.push(("proof 5 redeem".into(), hex(&redeem())));
    out.extend(chat().iter().map(|(n, m)| (n.to_string(), hex(&m.to_bytes()))));
    out.push(("chat link".into(), chat_link().format_chat()));
    out.push(("folder and channel link".into(), link().format_bundle(&bundled_channel())));
    out.extend(direct().iter().map(|(n, m)| (n.to_string(), hex(&m.to_bytes()))));
    let mut key = Vec::new();
    direct_key(3).put(&mut key);
    out.push(("direct key".into(), hex(&key)));
    out
}

fn frozen(name: &str) -> &'static str {
    GOLDEN.iter().find(|(n, _)| *n == name).map(|(_, v)| *v).unwrap_or_else(|| panic!("no frozen vector for {name}"))
}

#[test]
fn golden_vectors_are_frozen() {
    if std::env::var_os("SEEDWELL_GOLDEN_PRINT").is_some() {
        // The entries not frozen yet (the frozen ones never change).
        for (n, v) in written().into_iter().filter(|(n, _)| !GOLDEN.iter().any(|(g, _)| g == n)) {
            println!("    (\"{n}\", \"{v}\"),");
        }
        return;
    }
    for (n, v) in written() {
        assert_eq!(v, frozen(&n), "{n} is not written as it was frozen");
    }
    assert_eq!(GOLDEN.len(), written().len(), "every frozen vector is still written");
    // And each reads back as its value.
    for (n, m) in messages() {
        assert_eq!(decode(&unhex(frozen(n))), m, "{n}");
    }
    for (n, s) in statements() {
        assert_eq!(Statement::from_bytes(&unhex(frozen(n))).unwrap(), s, "{n}");
        assert!(s.verify(secret().public_key()), "{n}");
    }
    assert_eq!(InviteLink::parse(frozen("link")).unwrap(), link());
    assert_eq!(InviteLink::parse_chat(frozen("chat link")).unwrap(), chat_link());
    assert!(InviteLink::parse(frozen("chat link")).is_err() && InviteLink::parse_chat(frozen("link")).is_err(), "one kind each");
    let bundle = frozen("folder and channel link");
    assert_eq!(InviteLink::parse_bundle(bundle).unwrap(), (link(), Some(bundled_channel())));
    assert_eq!(InviteLink::parse(bundle).unwrap(), link(), "a folder link to anything that reads only that");
    assert_eq!(InviteLink::parse_chat(bundle), Err(KeyError::FolderLink));
    assert_eq!(InviteLink::parse_bundle(frozen("link")).unwrap(), (link(), None));
    for (n, m) in chat() {
        let back = ChatMessage::from_bytes(&unhex(frozen(n))).unwrap();
        assert_eq!(back, m, "{n}");
        assert!(back.verify(&fid()), "{n}");
    }
    for (n, m) in direct() {
        let back = DirectMessage::from_bytes(&unhex(frozen(n))).unwrap();
        assert_eq!(back, m, "{n}");
        assert!(back.verify(), "{n}");
        let to = if back.recipient == *dev(7).public_key() { dev(7) } else { dev(3) };
        assert!(back.open(&secret_of(&to)).is_some_and(|b| b.kind != MessageKind::Topic), "{n} opens for its recipient");
        assert!(back.open(&secret_of(&dev(9))).is_none(), "{n} opens for no one else");
    }
    let key = DirectKey::read(&mut Reader::new(&unhex(frozen("direct key")))).unwrap();
    assert!(key == direct_key(3) && key.verify());
    let invite = check_redeem(&unhex(frozen("proof 5 redeem")), dev(9).public_key(), &[0xee; 32]).unwrap();
    assert_eq!(invite, link().invite);
}

#[test]
fn fields_appended_to_any_message_are_ignored() {
    for (n, m) in messages() {
        let mut payload = Vec::new();
        m.encode_payload(&mut payload);
        payload.extend_from_slice(&[0x01, 0x80, 0x01, 0xff, 0x00]);
        assert_eq!(Message::decode(Frame { kind: m.kind(), flags: 0, payload: &payload }), Ok(m), "{n}");
    }
}

/// `count ‖ records`, from records given as bytes.
fn records(recs: &[Vec<u8>]) -> Vec<u8> {
    let mut out = vec![recs.len() as u8];
    for r in recs {
        out.push(r.len() as u8);
        out.extend_from_slice(r);
    }
    out
}

#[test]
fn records_that_grew_are_read_and_ones_with_unknown_values_skipped() {
    // An address record with a field appended inside its own length.
    let rec = AddressRecord::sign(&dev(7), vec!["192.0.2.1:22100".into()], String::new(), T);
    let mut one = Vec::new();
    Message::Addresses(Addresses { records: vec![rec.clone()] }).encode_payload(&mut one);
    let body = one[2..].to_vec();
    let grown = [body.clone(), vec![0x07, 0x07]].concat();
    let Ok(Message::Addresses(a)) = Message::decode(Frame { kind: 8, flags: 0, payload: &records(&[grown, body]) }) else { panic!() };
    assert_eq!(a.records, vec![rec.clone(), rec]);

    // An index batch with an entry of a file kind this version does not know: that one is skipped.
    let mut idx = Vec::new();
    let e = |seq| entry(seq, "a.txt", FileKind::File, false);
    Message::Index(Index { folder_id: fid(), index_id: 1, full: false, entries: vec![e(1), e(2), e(3)], done: false }).encode_payload(&mut idx);
    let mut bare = Vec::new();
    trs_sync_proto::msg::encode_entry(&mut bare, 2, false, &e(2).file);
    let kind_at = 1 + 1 + 1 + "a.txt".len(); // sequence, flags, path length, path
    assert_eq!(bare[kind_at], 0, "the kind byte of a file");
    let at = idx.windows(bare.len()).position(|w| w == bare).unwrap();
    idx[at + kind_at] = 9;
    let Ok(Message::Index(i)) = Message::decode(Frame { kind: 4, flags: 0, payload: &idx }) else { panic!() };
    assert_eq!(i.entries.iter().map(|e| e.sequence).collect::<Vec<_>>(), vec![1, 3]);

    // A Join folder record with a field appended.
    let mut j = Vec::new();
    let jf = JoinFolder { folder_id: fid(), proofs: vec![], my_index_id: 1, your_index_id: 2, your_max_seq: 3 };
    Message::Join(Join { folders: vec![jf.clone()] }).encode_payload(&mut j);
    let rec = [j[2..].to_vec(), vec![0x2a]].concat();
    assert_eq!(Message::decode(Frame { kind: 2, flags: 0, payload: &records(&[rec]) }), Ok(Message::Join(Join { folders: vec![jf] })));

    // A cut-off record is still an error, not a skip.
    assert!(matches!(Message::decode(Frame { kind: 8, flags: 0, payload: &[1, 40, 1, 2] }), Err(DecodeError::Truncated(_))));
}

#[test]
fn statements_of_unknown_kinds_or_values_are_kept_passed_on_and_give_no_role() {
    let s = secret();
    let me = dev(3);
    let mut m = Membership::new(*s.public_key());
    m.add([Statement::by_owner(&s, Body::Grant { device: *me.public_key(), role: Role::Owner, serial: 1, at_s: T, invite: None })]);

    // A kind from a later version, signed by the owner: kept as it came, and verified.
    let later = Statement::by_owner(&s, Body::Unknown { kind: 42, data: vec![9, 9, 9] });
    let back = Statement::from_bytes(&later.to_bytes()).unwrap();
    assert_eq!((back.body.clone(), back.to_bytes()), (Body::Unknown { kind: 42, data: vec![9, 9, 9] }, later.to_bytes()));
    assert!(back.verify(s.public_key()));
    m.add([back]);
    assert!(m.statements().any(|st| *st == later));
    // From a device that is no member, it is not kept (as for any statement).
    m.add([Statement::by_device(&dev(5), &fid(), Body::Unknown { kind: 42, data: vec![1] })]);
    assert_eq!(m.statements().count(), 2);

    // A grant with a role this version does not know: kept, no role.
    let st = Statement::by_owner(&s, Body::Grant { device: *dev(4).public_key(), role: Role::ReadWrite, serial: 2, at_s: T, invite: None });
    let mut b = st.to_bytes();
    assert_eq!(b[2 + 32], 3, "the role byte of a Read & write grant");
    b[2 + 32] = 9;
    let odd = Statement::from_bytes(&b).unwrap();
    assert!(matches!(odd.body, Body::Unknown { kind: 1, .. }));
    assert_eq!(odd.to_bytes(), b, "passed on unchanged");

    // A known kind whose body grew: read, the new bytes kept, the signature still good.
    let remove = statements()[2].1.to_bytes();
    let body = remove[2..2 + remove[1] as usize].to_vec();
    let grown = Statement::by_owner(&s, Body::Unknown { kind: 2, data: [body, vec![0xab, 0xcd]].concat() });
    let read = Statement::from_bytes(&grown.to_bytes()).unwrap();
    assert!(matches!(read.body, Body::Remove { serial: 3, at_s, .. } if at_s == T + 1), "{:?}", read.body);
    assert_eq!((read.extra.as_slice(), read.to_bytes()), (&[0xab, 0xcd][..], grown.to_bytes()));
    assert!(read.verify(s.public_key()));
    m.add([read.clone()]);
    assert!(m.is_removed(dev(4).public_key()), "its known part counts");

    // A Members message keeps them all; a statement it cannot frame (an unknown signer kind) is left out.
    let mut bad = later.to_bytes();
    let signer_at = bad.len() - 65;
    bad[signer_at] = 7;
    let mut p = fid().0.to_vec();
    p.extend(records(&[later.to_bytes(), bad, odd.to_bytes()]));
    let Ok(Message::Members(ms)) = Message::decode(Frame { kind: 3, flags: 0, payload: &p }) else { panic!() };
    assert_eq!(ms.statements, vec![later, odd]);
}

#[test]
fn a_bundled_channel_part_that_is_damaged_or_the_folder_itself_spoils_the_link() {
    let good = link().format_bundle(&bundled_channel());
    let (head, chat) = good.split_once("&chat=").unwrap();
    let mut damaged = chat.to_string();
    damaged.replace_range(3..4, if &chat[3..4] == "A" { "B" } else { "A" });
    assert_eq!(InviteLink::parse_bundle(&format!("{head}&chat={damaged}")), Err(KeyError::Damaged));
    let itself = link().format_bundle(&link());
    assert_eq!(InviteLink::parse_bundle(&itself), Err(KeyError::Damaged));
    // A channel's link ignores a `chat=` field; a bundle is only ever a folder's.
    let chat_with = format!("{}&chat={}", chat_link().format_chat(), &chat[..chat.find('&').unwrap_or(chat.len())]);
    assert_eq!(InviteLink::parse_chat(&chat_with).unwrap(), chat_link());
    assert_eq!(InviteLink::parse_bundle(&chat_with), Err(KeyError::ChannelLink));
}

#[test]
fn links_and_invitations_of_a_later_version_say_update() {
    let text = link().format();
    let body = text.strip_prefix("seedwell://sync/").unwrap().split('?').next().unwrap();
    let mut bytes = trs_sync_proto::base32::decode(body).unwrap();
    bytes[0] = 2;
    // A good check over the new bytes, as a newer Seedwell would write it.
    let n = bytes.len() - 2;
    let d = ring::digest::digest(&ring::digest::SHA256, &[b"seedwell-sync/v2/link".as_slice(), &bytes[..n]].concat());
    bytes[n..].copy_from_slice(&d.as_ref()[..2]);
    let newer = format!("seedwell://sync/{}", trs_sync_proto::base32::encode(&bytes));
    assert_eq!(InviteLink::parse(&newer), Err(KeyError::Newer));
    assert!(KeyError::Newer.to_string().contains("newer Seedwell"));
    // An invitation of a later version inside a proof is not read.
    let mut proof = redeem();
    proof[0] = 2;
    assert_eq!(check_redeem(&proof, dev(9).public_key(), &[0xee; 32]), None);
    assert!(Invite::read(&mut Reader::new(&proof)).is_err());
}

#[test]
fn a_peer_with_a_newer_protocol_and_features_is_spoken_to_at_ours() {
    let theirs = Hello { protocol: PROTOCOL + 4, client: "Seedwell 9.0.0".into(), device_name: "Future Mac".into(), min_protocol: PROTOCOL, features: u64::MAX };
    let mut payload = Vec::new();
    Message::Hello(theirs.clone()).encode_payload(&mut payload);
    payload.extend_from_slice(b"\x05later fields");
    let Ok(Message::Hello(h)) = Message::decode(Frame { kind: 1, flags: 0, payload: &payload }) else { panic!() };
    assert_eq!(h, theirs);
    let mine = Hello::new("Seedwell 0.1.6".into(), "Mac".into());
    assert_eq!(agree(&mine, &h), Ok(Agreed { protocol: PROTOCOL, features: FEATURES }));
    // A message type from a later version is skipped.
    assert_eq!(Message::decode(Frame { kind: 99, flags: 0, payload: b"anything" }), Ok(Message::Unknown(99)));
}

/// The frozen table. Add entries for new types and kinds; never change one.
const GOLDEN: &[(&str, &str)] = &[
    ("message 1 hello", "000000230100020e5365656477656c6c20302e312e360e4b6172692773204d6163426f6f6b0200"),
    ("message 2 join", "00000184020001ff029e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c5603046b0128ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1010180829ad70600006e0fac6bc3157adbfa575e36b997a504aca780fd63baa2cfd3e3dfe8226c53a0c4dfb9e8356fa1463e3b6f3cefb254cfb512aefbdb4c250a5e9f72e6d73c4d0905de01019637b249c05e7c8e0191e443ba0d55bf87f417dd29ae535f51072e62af8ec13208080808080808080808080808080808030180f7bed70680829ad706001398f62c6d1a457c51ba6a4b5f3dbd2f69fca93216218dc8997e416bd17d93caadec86809f80941eb3d15ee572b6f1c98f498ec98279554f37107228c506d9c6d3ba625eedeb709fb1c7f99b0fa398e0741e1b7863f056ac5ba48d9549173406b4125bfe4ef842f5d4357d82bafc144a2e88a8dda4c437dd35886b034d025778159f67f11e7416e7c51c4edb90a87d6ba66f6ee7e802b07a152accae6b14380b09020102888e98a8c0e08081014dd209"),
    ("message 3 members", "000003f303009e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c56066b0128ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1010180829ad70600006e0fac6bc3157adbfa575e36b997a504aca780fd63baa2cfd3e3dfe8226c53a0c4dfb9e8356fa1463e3b6f3cefb254cfb512aefbdb4c250a5e9f72e6d73c4d09cc0201e801ca93ac1705187071d67b83c7ff0efe8108e8ec4530575d7726879333dbdabe7c030080829ad70601be01019637b249c05e7c8e0191e443ba0d55bf87f417dd29ae535f51072e62af8ec13205050505050505050505050505050505030180f7bed70680829ad706018139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b3946e7a1cdd29b0b78fd13af4c5598feff4ef2a97166e3ca6f2e4fbfccd80505bf1de648af7dcdfe6f14c003efa20454c99e14e119cf5e21d5233cfc508e37b17ddff1d0cc279e05c7b468c82425b74ebcef43f80d12f68c04dfd8d352bda73eb00018139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b3946cf84f5ebf3c2c63cdbf2e73419a49bd8d1f9a893e851bde21da10cf81bd4cacfac8196124d66225f2c6e9c827ae125c5d6a14f2ae9d22ee42c6d26a91d73f0b690226ca93ac1705187071d67b83c7ff0efe8108e8ec4530575d7726879333dbdabe7c0381829ad70600c99b94f82405dea25a37d8fa12d6700efa8845caaa10e589915368d6bf52ba498d036d2824b9aebeb13c55a8ae81ffef4cfb3eb71c63dc1e893b02457366410ee201039e01019637b249c05e7c8e0191e443ba0d55bf87f417dd29ae535f51072e62af8ec13206060606060606060606060606060606040180f7bed70680829ad706008a875fff1eb38451577acd5afee405456568dd7c89e090863a0557bc7af49f17c492db27f15677af52794012d53ee66cf9a5afe3f550ebb02e0164e0fc886360467a3c3bebf1a2524340d1db7c89e7a50ac87d33490e52ead9252f63b5e4480c006c7ddcd7be02e2c50cc57f50fc671528fcddc08dc0dee1a6b5ed5766fce80ae8e87ffc63b70db958ad58b8d07a222aa87c1c458dc12e33578c66da47c41df5027804150606060606060606060606060606060682829ad706018139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394e9c48e8815846956795e5944cb6a1f46d79fe309d75c82b9145d887f8f32ff0b0fa88ea3d2cf1c59f912621bf5c31adf6a18fce95354d9f1b26f237fbb1b2e0d4e050b809cb8d8060480829ad706008dca1af0b9d0ccdaef792d18a367ccbcd2c4cb5423a3f3f6534f9c6089cbf842b39342e798c8597b5856331d2c9a8bb6b17e3fd18fa24e60f39c92968590be0c"),
    ("message 4 index", "000003a604009e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c568d98ac500105d90101000e4e6f7465732f706c616e2e74787400c096b102aab4aeb2d0afe1e3310002112233445566778880829ad70699aabbccddeeff000311223344556677880101010101010101010101010101010100000280808002c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c09631c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c24141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414141414186010200054e6f7465730100aab4aeb2d0afe1e3310002112233445566778880829ad70699aabbccddeeff000311223344556677880202020202020202020202020202020200000042424242424242424242424242424242424242424242424242424242424242424242424242424242424242424242424242424242424242424242424242424242940103000a4e6f7465732f6c696e6b0200aab4aeb2d0afe1e3310002112233445566778880829ad70699aabbccddeeff0003112233445566778803030303030303030303030303030303092e2e2f7461726765740000434343434343434343434343434343434343434343434343434343434343434343434343434343434343434343434343434343434343434343434343434343438d0104010c4e6f7465732f6f6c642e73680000aab4aeb2d0afe1e3310302112233445566778880829ad70699aabbccddeeff000311223344556677880404040404040404040404040404040400000044444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444444f3010500194e6f7465732f706c616e2028636f6e666c696374292e74787400c096b102aab4aeb2d0afe1e3310002112233445566778880829ad70699aabbccddeeff000311223344556677880505050505050505050505050505050500010e4e6f7465732f706c616e2e7478740280808002c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c1c09631c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c24545454545454545454545454545454545454545454545454545454545454545454545454545454545454545454545454545454545454545454545454545454501"),
    ("message 5 request", "0000005a0500099e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c560e4e6f7465732f706c616e2e74787480808002c09631c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c201"),
    ("message 6 response", "00000010060009000b6368756e6b206279746573"),
    ("message 6 response busy", "0000000506000a0400"),
    ("message 7 progress", "0000003e07009e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c56011a074269672e6d6f7601000000000000000580829ad70603000107"),
    ("message 8 addresses", "000000b0080001ab01ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c020f3139322e302e322e313a3232313030157574703a2f2f3139322e302e322e313a32323130301e72656c61793a2f2f3139382e35312e3130302e393a32323036372341424380829ad706e0605a0e6ff801d19f367e05adae9906f9bc08fb4505620ecb7c7704d376e736e37f564fb3099be3752ae1fe09913140829451ac7342b8d007e6140b6341220e"),
    ("message 9 ping", "000000020900"),
    ("message 10 close", "000000170a00146e6f20666f6c6465727320696e20636f6d6d6f6e"),
    ("message 11 rendezvous", "000000350b00212121212121212121212121212121212121212121212121212121212121212101113230332e302e3131332e353a3430303030"),
    ("message 12 introduce", "0000003b0c00222222222222222222222222222222222222222222222222222222222222222201113230332e302e3131332e363a3430303031fad19fa39934"),
    ("message 13 relay open", "000000230d00232323232323232323232323232323232323232323232323232323232323232303"),
    ("message 14 relay data", "000000080e00030400010203"),
    ("message 15 relay close", "000000030f0003"),
    ("message 16 owner key", "0000004210009e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c565e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e5e"),
    ("statement 1 grant", "0128ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1010180829ad70600006e0fac6bc3157adbfa575e36b997a504aca780fd63baa2cfd3e3dfe8226c53a0c4dfb9e8356fa1463e3b6f3cefb254cfb512aefbdb4c250a5e9f72e6d73c4d09"),
    ("statement 1 grant by an admin", "01e801ca93ac1705187071d67b83c7ff0efe8108e8ec4530575d7726879333dbdabe7c030080829ad70601be01019637b249c05e7c8e0191e443ba0d55bf87f417dd29ae535f51072e62af8ec13205050505050505050505050505050505030180f7bed70680829ad706018139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b3946e7a1cdd29b0b78fd13af4c5598feff4ef2a97166e3ca6f2e4fbfccd80505bf1de648af7dcdfe6f14c003efa20454c99e14e119cf5e21d5233cfc508e37b17ddff1d0cc279e05c7b468c82425b74ebcef43f80d12f68c04dfd8d352bda73eb00018139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b3946cf84f5ebf3c2c63cdbf2e73419a49bd8d1f9a893e851bde21da10cf81bd4cacfac8196124d66225f2c6e9c827ae125c5d6a14f2ae9d22ee42c6d26a91d73f0b"),
    ("statement 2 remove", "0226ca93ac1705187071d67b83c7ff0efe8108e8ec4530575d7726879333dbdabe7c0381829ad70600c99b94f82405dea25a37d8fa12d6700efa8845caaa10e589915368d6bf52ba498d036d2824b9aebeb13c55a8ae81ffef4cfb3eb71c63dc1e893b02457366410e"),
    ("statement 3 invite", "039e01019637b249c05e7c8e0191e443ba0d55bf87f417dd29ae535f51072e62af8ec13206060606060606060606060606060606040180f7bed70680829ad706008a875fff1eb38451577acd5afee405456568dd7c89e090863a0557bc7af49f17c492db27f15677af52794012d53ee66cf9a5afe3f550ebb02e0164e0fc886360467a3c3bebf1a2524340d1db7c89e7a50ac87d33490e52ead9252f63b5e4480c006c7ddcd7be02e2c50cc57f50fc671528fcddc08dc0dee1a6b5ed5766fce80ae8e87ffc63b70db958ad58b8d07a222aa87c1c458dc12e33578c66da47c41df502"),
    ("statement 4 revoke", "04150606060606060606060606060606060682829ad706018139770ea87d175f56a35466c34c7ecccb8d8a91b4ee37a25df60f5b8fc9b394e9c48e8815846956795e5944cb6a1f46d79fe309d75c82b9145d887f8f32ff0b0fa88ea3d2cf1c59f912621bf5c31adf6a18fce95354d9f1b26f237fbb1b2e0d"),
    ("statement 5 share until", "050b809cb8d8060480829ad706008dca1af0b9d0ccdaef792d18a367ccbcd2c4cb5423a3f3f6534f9c6089cbf842b39342e798c8597b5856331d2c9a8bb6b17e3fd18fa24e60f39c92968590be0c"),
    ("link", "seedwell://sync/AGLDPMSJYBPHZDQBSHSEHOQNKW7YP5AX3UU24U27KEDS4YVPR3ATECAIBAEAQCAIBAEAQCAIBAEAQCADAGAPPPWXA2AIFGWXAYAK33EGQCPYBFA6WPIV5ZLSW3Y4TD2JR3EYE6KVJ43RA4RIYUDNTRWTXJRF53PLOCP3DR7ZTMH2HGHAOQPBW6DD6BLKYW5ERWKUSFZUAYEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQA7S?name=Family%20Photos"),
    ("proof 5 redeem", "019637b249c05e7c8e0191e443ba0d55bf87f417dd29ae535f51072e62af8ec13208080808080808080808080808080808030180f7bed70680829ad706001398f62c6d1a457c51ba6a4b5f3dbd2f69fca93216218dc8997e416bd17d93caadec86809f80941eb3d15ee572b6f1c98f498ec98279554f37107228c506d9c6d3ba625eedeb709fb1c7f99b0fa398e0741e1b7863f056ac5ba48d9549173406b4125bfe4ef842f5d4357d82bafc144a2e88a8dda4c437dd35886b034d025778159f67f11e7416e7c51c4edb90a87d6ba66f6ee7e802b07a152accae6b14380b"),
    // Channels (feature bit 0).
    ("message 17 chat join", "00000174110001ef029e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c5602046b0128ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1010180829ad70600006e0fac6bc3157adbfa575e36b997a504aca780fd63baa2cfd3e3dfe8226c53a0c4dfb9e8356fa1463e3b6f3cefb254cfb512aefbdb4c250a5e9f72e6d73c4d0905de01019637b249c05e7c8e0191e443ba0d55bf87f417dd29ae535f51072e62af8ec13208080808080808080808080808080808030180f7bed70680829ad706001398f62c6d1a457c51ba6a4b5f3dbd2f69fca93216218dc8997e416bd17d93caadec86809f80941eb3d15ee572b6f1c98f498ec98279554f37107228c506d9c6d3ba625eedeb709fb1c7f99b0fa398e0741e1b7863f056ac5ba48d9549173406b4125bfe4ef842f5d4357d82bafc144a2e88a8dda4c437dd35886b034d025778159f67f11e7416e7c51c4edb90a87d6ba66f6ee7e802b07a152accae6b14380b"),
    ("message 18 chat have", "0000007312009e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c56809886fa98340225ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1020103070123ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c010501"),
    ("message 19 chat messages", "0000032213009e64389090ed3bd0aecea44c8015b863a7eea036114e2aa1d76daa5ed1f14c56058b014a11111111111111111111111111111111ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d10185d09fa39934000000010e4d6964646167206b6c2e2031383fedba3f0c694cd649b510f16a07cfccda7fac2ca260f6c25e566e026b20c487e6c32c0f0473350806f96dc6574c296433eafaa70454272c219eb1e5e062737202b9017812121212121212121212121212121212ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c0484d79fa399340111111111111111111111111111111111000b636c617564652d636f646502217b22616e73776572223a22796573222c22636f6e666964656e6365223a302e397d2fb3819dd7205c335f24b5f925064d97f4c3f81dca1241b417f75dcfcba7764ff31a07a30db2fe96543ea608bf7c82c188d6aee022335570793a96a4f0b7480d8f014e13131313131313131313131313131313ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d102b0d99fa399340112121212121212121212121212121212000009021201e44a90951db802165f476de9cd2c2cf20b603d1e03462b790d653ce61ce9f0aa0955c0506dedbb32e1c4663c415881e89bde6022f9190c77ebe348d4aa4d800184014314141414141414141414141414141414ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d103dcdb9fa3993400000003074cc3b872646167be7a1e57f0c6f0ece1367904783f29f3869907a375f66490977974da39a82f9408bccb3be92f5b914a7c630e69822411bd51a684a0fa14ade932d1de3f3db90c9e015d15151515151515151515151515151515ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c0588de9fa39934000114141414141414141414141414141414000111426f726420666f722034206b6c2e20313831d3eca909e8cb31a70b8ce00b1e54c76945393e81a495a3773b07adfb3c95b941fa389fa2fbcb69a13d61a8e63d1e7bf9b445cc4c1b19400f90dbac5d78310a"),
    ("message 20 direct keys", "000001131400028601ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1d584d8f13a009997bc3a40d9ceb3ff937883ea52b9f3df921545eb1f4fd7c74583d09fa39934ddaa4b273fd0f10fb2854e587bb52ab7f6345b25c6d006f35c53b6a9d531f52ae51063d89cc58d9622ce55dd8f5917ce83bd4ea91b428c35f8b4bde34d689a0f8601ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c27917c7be2f2e90a3c73ee90dfb8079b6e7505021ac5eb9b0cc4071b8669361587d09fa39934832fd198b3bdc0c6bf76a736d8302ff4615d291afb3de0433c20e488397a825e52b3787e771f68d0d57951ac5ea1d21d46b9a95a855fbdba5a33400a7b8e9106"),
    ("message 21 direct have", "0000002715000123ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1010102"),
    ("message 22 direct messages", "000001d5160002da01980131313131313131313131313131313131ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c018ad09fa399347a1a4e709bf085ac494aba0469b9b1eda0ab1f78b16aabb79ffeda90623e852220d7c381f7888d09181b9a45b8b88bea9031c1ffba7d0ebc08d2f80a2afbef403961eb711b2a293e7b95baffa6d083620c9a5f439c48edaa248f25a74b66efcb7ffabdb236d49f34f3509f6dffbd9415ae4450171eca405ae599d6eff4caf5ad0cf401b20132323232323232323232323232323232ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22ced4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d10194d09fa39934132c442be010fbd57e72603328aa76e71fccc1503aae219327d14d9c9993f4723ad01b8d60854b1bef3dd8b14d4977d0a7fc20ab56685ff96d74aadbffbfef77d51a5dae49064736efb21f061c44861e31c452e8f2dc70a370e671ef2812d1b179bab2849c306cc293d92cbd6d7c2a239668bc9376af70f9eb5df90e751e5deff1696d0f1d62a706115c257161c346dc47832e252a5dac34d00901"),
    ("chat link", "seedwell://chat/AGLDPMSJYBPHZDQBSHSEHOQNKW7YP5AX3UU24U27KEDS4YVPR3ATECIJBEEQSCIJBEEQSCIJBEEQSCIDAGAPPPWXA2AIFGWXAYAETOGXKKOVBZZL32EIKJZMYZ7FRB34MUHHNKPNXPZD4LTP3Q3M7VNBL5ZVVH3A2OBPLDYOARUX4KIIQF75P7LKQK77YXO74N2OQSLPAYEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSCIJBEEQSJP3?name=Middag"),
    ("folder and channel link", "seedwell://sync/AGLDPMSJYBPHZDQBSHSEHOQNKW7YP5AX3UU24U27KEDS4YVPR3ATECAIBAEAQCAIBAEAQCAIBAEAQCADAGAPPPWXA2AIFGWXAYAK33EGQCPYBFA6WPIV5ZLSW3Y4TD2JR3EYE6KVJ43RA4RIYUDNTRWTXJRF53PLOCP3DR7ZTMH2HGHAOQPBW6DD6BLKYW5ERWKUSFZUAYEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQA7S?name=Family%20Photos&chat=AHJ2NEPDOIMGW22BOYRL7KSRFX3WCEVK4ZSNTOZJEBBZ6O7LY6PZKCQKBIFAUCQKBIFAUCQKBIFAUCQDAGAPPPWXA2AIFGWXAYAL7HHNX2BNDVW7V27HC3BOBZ5U43WQO3MDHBCH4U77UTZ5ZTQ56Y2GWPHDD3KYXGLK35XGLEGTPKRUXLZW3ZKCM3FCYYWXY7GFL37YAMFAUCQKBIFAUCQKBIFAUCQKBIFAUCQKBIFAUCQKBIFAUCQKBIFAVR6D&chat_name=Family%20Photos%20chat"),
    ("chat 1 text", "4a11111111111111111111111111111111ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d10185d09fa39934000000010e4d6964646167206b6c2e2031383fedba3f0c694cd649b510f16a07cfccda7fac2ca260f6c25e566e026b20c487e6c32c0f0473350806f96dc6574c296433eafaa70454272c219eb1e5e062737202"),
    ("chat 2 json from an agent", "7812121212121212121212121212121212ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c0484d79fa399340111111111111111111111111111111111000b636c617564652d636f646502217b22616e73776572223a22796573222c22636f6e666964656e6365223a302e397d2fb3819dd7205c335f24b5f925064d97f4c3f81dca1241b417f75dcfcba7764ff31a07a30db2fe96543ea608bf7c82c188d6aee022335570793a96a4f0b7480d"),
    ("chat 3 later kind", "4e13131313131313131313131313131313ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d102b0d99fa399340112121212121212121212121212121212000009021201e44a90951db802165f476de9cd2c2cf20b603d1e03462b790d653ce61ce9f0aa0955c0506dedbb32e1c4663c415881e89bde6022f9190c77ebe348d4aa4d8001"),
    ("chat 4 topic", "4314141414141414141414141414141414ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d103dcdb9fa3993400000003074cc3b872646167be7a1e57f0c6f0ece1367904783f29f3869907a375f66490977974da39a82f9408bccb3be92f5b914a7c630e69822411bd51a684a0fa14ade932d1de3f3db90c"),
    ("chat 5 in a topic", "5d15151515151515151515151515151515ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c0588de9fa39934000114141414141414141414141414141414000111426f726420666f722034206b6c2e20313831d3eca909e8cb31a70b8ce00b1e54c76945393e81a495a3773b07adfb3c95b941fa389fa2fbcb69a13d61a8e63d1e7bf9b445cc4c1b19400f90dbac5d78310a"),
    ("direct 1 text", "980131313131313131313131313131313131ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c018ad09fa399347a1a4e709bf085ac494aba0469b9b1eda0ab1f78b16aabb79ffeda90623e852220d7c381f7888d09181b9a45b8b88bea9031c1ffba7d0ebc08d2f80a2afbef403961eb711b2a293e7b95baffa6d083620c9a5f439c48edaa248f25a74b66efcb7ffabdb236d49f34f3509f6dffbd9415ae4450171eca405ae599d6eff4caf5ad0c"),
    ("direct 2 json from an agent", "b20132323232323232323232323232323232ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22ced4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d10194d09fa39934132c442be010fbd57e72603328aa76e71fccc1503aae219327d14d9c9993f4723ad01b8d60854b1bef3dd8b14d4977d0a7fc20ab56685ff96d74aadbffbfef77d51a5dae49064736efb21f061c44861e31c452e8f2dc70a370e671ef2812d1b179bab2849c306cc293d92cbd6d7c2a239668bc9376af70f9eb5df90e751e5deff1696d0f1d62a706115c257161c346dc47832e252a5dac34d00901"),
    ("direct key", "ed4928c628d1c2c6eae90338905995612959273a5c63f93636c14614ac8737d1d584d8f13a009997bc3a40d9ceb3ff937883ea52b9f3df921545eb1f4fd7c74583d09fa39934ddaa4b273fd0f10fb2854e587bb52ab7f6345b25c6d006f35c53b6a9d531f52ae51063d89cc58d9622ce55dd8f5917ce83bd4ea91b428c35f8b4bde34d689a0f"),
];
