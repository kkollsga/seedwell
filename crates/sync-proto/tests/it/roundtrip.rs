//! Every message survives encode → frame → decode unchanged, on random contents.

use proptest::collection::vec;
use proptest::prelude::*;
use trs_sync_proto::chat::{AuthorHave, ChatMessage, MessageKind};
use trs_sync_proto::direct::{DirectKey, DirectMessage};
use trs_sync_proto::codec::Frame;
use trs_sync_proto::ids::{DeviceId, FolderId, ShortId};
use trs_sync_proto::keys::Proof;
use trs_sync_proto::model::{ChunkRef, Counter, FileInfo, FileKind, IndexEntry, MAX_CHUNK, SignedFile, Version};
use trs_sync_proto::msg::*;
use trs_sync_proto::keys::FolderSecret;
use trs_sync_proto::members::{Body, Role, Statement};

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES").ok().and_then(|v| v.parse().ok()).unwrap_or(256)
}

fn bytes<const N: usize>() -> impl Strategy<Value = [u8; N]> {
    vec(any::<u8>(), N).prop_map(|v| <[u8; N]>::try_from(v).unwrap_or([0; N]))
}

fn text(max: usize) -> impl Strategy<Value = String> {
    vec(any::<char>(), 0..=max).prop_map(String::from_iter)
}

fn folder_id() -> impl Strategy<Value = FolderId> {
    bytes::<32>().prop_map(FolderId)
}

fn device_id() -> impl Strategy<Value = DeviceId> {
    bytes::<32>().prop_map(DeviceId)
}

fn version() -> impl Strategy<Value = Version> {
    proptest::collection::btree_map(any::<u64>(), 1..=u64::MAX, 0..6).prop_map(|m| {
        let counters = m.into_iter().map(|(id, value)| Counter { id: ShortId(id), value }).collect();
        Version::from_counters(counters).unwrap_or_default()
    })
}

fn proof() -> impl Strategy<Value = Proof> {
    prop_oneof![
        vec(any::<u8>(), 0..300).prop_map(Proof::Grant),
        vec(any::<u8>(), 0..300).prop_map(Proof::Invite),
        (6..=255u8, vec(any::<u8>(), 0..40)).prop_map(|(kind, data)| Proof::Unknown { kind, data }),
    ]
}

fn kind() -> impl Strategy<Value = FileKind> {
    prop_oneof![Just(FileKind::File), Just(FileKind::Dir), Just(FileKind::Symlink)]
}

prop_compose! {
    fn file_info()(
        path in text(40), kind in kind(), size in any::<u64>(), mtime_ns in any::<i64>(), exec in any::<bool>(),
        deleted in any::<bool>(), version in version(), modified_by in any::<u64>(), file_id in bytes::<16>(),
        symlink_target in text(20), conflict_of in proptest::option::of(text(20)),
        chunks in vec((0..=MAX_CHUNK, bytes::<32>()).prop_map(|(len, hash)| ChunkRef { len, hash }), 0..8),
    ) -> FileInfo {
        FileInfo {
            path, kind, size, mtime_ns, exec, deleted, version, modified_by: ShortId(modified_by), file_id,
            symlink_target, conflict_of, chunks,
        }
    }
}

prop_compose! {
    fn index_entry()(sequence in any::<u64>(), invalid in any::<bool>(), dataless in any::<bool>(), on_demand in any::<bool>(), info in file_info(), sig in bytes::<64>())
        -> IndexEntry {
        IndexEntry { sequence, invalid, dataless, on_demand, file: SignedFile { info, sig } }
    }
}

fn hello() -> impl Strategy<Value = Message> {
    (any::<u32>(), text(30), text(MAX_DEVICE_NAME), any::<u32>(), any::<u64>()).prop_map(
        |(protocol, client, device_name, min_protocol, features)| {
            Message::Hello(Hello { protocol, client, device_name, min_protocol, features })
        },
    )
}

fn join() -> impl Strategy<Value = Message> {
    let folder = (folder_id(), vec(proof(), 0..=MAX_PROOFS), any::<u64>(), any::<u64>(), any::<u64>()).prop_map(
        |(folder_id, proofs, my_index_id, your_index_id, your_max_seq)| JoinFolder {
            folder_id,
            proofs,
            my_index_id,
            your_index_id,
            your_max_seq,
        },
    );
    vec(folder, 0..5).prop_map(|folders| Message::Join(Join { folders }))
}

fn members() -> impl Strategy<Value = Message> {
    let statement = (bytes::<32>(), 0..4usize, any::<u64>(), any::<u64>(), 0..3u8).prop_map(|(device, r, serial, at_s, kind)| {
        let secret = FolderSecret::from_bytes([1; 32]);
        let role = [Role::Owner, Role::Admin, Role::ReadWrite, Role::ReadOnly][r];
        let body = match kind {
            0 => Body::Remove { device, serial, at_s },
            1 => Body::ShareUntil { until_s: at_s / 2, serial, at_s },
            _ => Body::Grant { device, role, serial, at_s, invite: None },
        };
        Statement::by_owner(&secret, body)
    });
    (folder_id(), vec(statement, 0..5)).prop_map(|(folder_id, statements)| Message::Members(Members { folder_id, statements }))
}

fn index() -> impl Strategy<Value = Message> {
    (folder_id(), any::<u64>(), any::<bool>(), vec(index_entry(), 0..5), any::<bool>()).prop_map(
        |(folder_id, index_id, full, entries, done)| Message::Index(Index { folder_id, index_id, full, entries, done }),
    )
}

fn request() -> impl Strategy<Value = Message> {
    (any::<u32>(), folder_id(), text(40), any::<u64>(), any::<u32>(), bytes::<32>(), any::<bool>()).prop_map(
        |(id, folder_id, path, offset, len, hash, from_temp)| {
            Message::Request(Request { id, folder_id, path, offset, len, hash, from_temp })
        },
    )
}

fn response() -> impl Strategy<Value = Message> {
    let code = prop_oneof![
        Just(ResponseCode::Ok),
        Just(ResponseCode::Generic),
        Just(ResponseCode::NoSuchFile),
        Just(ResponseCode::Invalid),
        Just(ResponseCode::Busy),
    ];
    (any::<u32>(), code, vec(any::<u8>(), 0..300))
        .prop_map(|(id, code, data)| Message::Response(Response { id, code, data }))
}

fn progress() -> impl Strategy<Value = Message> {
    let update = (text(30), version(), vec(any::<u32>(), 0..10))
        .prop_map(|(path, version, have)| ProgressUpdate { path, version, have });
    (folder_id(), vec(update, 0..5)).prop_map(|(folder_id, updates)| Message::Progress(Progress { folder_id, updates }))
}

fn addresses() -> impl Strategy<Value = Message> {
    let record = (bytes::<32>(), vec(text(30), 0..4), text(30), any::<u64>(), bytes::<64>()).prop_map(
        |(device_pk, addrs, relay, issued_s, sig)| AddressRecord { device_pk, addrs, relay, issued_s, sig },
    );
    vec(record, 0..4).prop_map(|records| Message::Addresses(Addresses { records }))
}

fn chat() -> impl Strategy<Value = Message> {
    let kind = prop_oneof![Just(MessageKind::Text), Just(MessageKind::Json), Just(MessageKind::Topic), (4..255u8).prop_map(MessageKind::Unknown)];
    let agent = proptest::option::of("[a-z][a-z-]{0,20}");
    let ids = (proptest::option::of(bytes::<16>()), proptest::option::of(bytes::<16>()));
    let msg = (bytes::<16>(), bytes::<32>(), any::<u64>(), any::<u64>(), ids, agent, kind, text(40), bytes::<64>())
        .prop_map(|(id, author, seq, at_ms, (reply_to, topic), agent, kind, body, sig)| ChatMessage {
            id,
            author,
            seq,
            at_ms,
            reply_to,
            topic,
            agent,
            kind,
            // A topic's title is never blank (one that is does not decode).
            body: if kind == MessageKind::Topic { format!("T{body}") } else { body }.into_bytes(),
            extra: Vec::new(),
            sig,
        });
    let ranges = vec((0..1u64 << 40, 1..1000u64), 0..5).prop_map(|r| {
        let mut start = 0;
        r.into_iter().map(|(gap, len)| { start += gap; let s = start; start += len; (s, len) }).collect::<Vec<_>>()
    });
    let author = (bytes::<32>(), ranges).prop_map(|(author, ranges)| AuthorHave { author, ranges });
    prop_oneof![
        vec((folder_id(), vec(proof(), 0..=MAX_PROOFS)), 0..4)
            .prop_map(|c| Message::ChatJoin(ChatJoin { channels: c.into_iter().map(|(channel_id, proofs)| JoinChannel { channel_id, proofs }).collect() })),
        (folder_id(), any::<u64>(), vec(author, 0..4)).prop_map(|(channel_id, floor_at_ms, authors)| Message::ChatHave(ChatHave { channel_id, floor_at_ms, authors })),
        (folder_id(), vec(msg, 0..4)).prop_map(|(channel_id, messages)| Message::ChatMessages(ChatMessages { channel_id, messages })),
    ]
}

fn direct() -> impl Strategy<Value = Message> {
    let key = (bytes::<32>(), bytes::<32>(), any::<u64>(), bytes::<64>()).prop_map(|(device, x25519, at_ms, sig)| DirectKey { device, x25519, at_ms, sig });
    let ranges = vec((0..1u64 << 40, 1..1000u64), 0..5).prop_map(|r| {
        let mut start = 0;
        r.into_iter().map(|(gap, len)| { start += gap; let s = start; start += len; (s, len) }).collect::<Vec<_>>()
    });
    let author = (bytes::<32>(), ranges).prop_map(|(author, ranges)| AuthorHave { author, ranges });
    let msg = (bytes::<16>(), bytes::<32>(), bytes::<32>(), any::<u64>(), any::<u64>(), bytes::<32>(), vec(any::<u8>(), 16..200), bytes::<64>()).prop_map(
        |(id, author, recipient, seq, at_ms, ephemeral, sealed, sig)| DirectMessage { id, author, recipient, seq, at_ms, ephemeral, sealed, extra: Vec::new(), sig },
    );
    prop_oneof![
        vec(key, 0..4).prop_map(|records| Message::DirectKeys(DirectKeys { records })),
        vec(author, 0..4).prop_map(|authors| Message::DirectHave(DirectHave { authors })),
        vec(msg, 0..4).prop_map(|messages| Message::DirectMessages(DirectMessages { messages })),
    ]
}

fn the_rest() -> impl Strategy<Value = Message> {
    prop_oneof![
        Just(Message::Ping),
        text(60).prop_map(|reason| Message::Close(Close { reason })),
        (device_id(), vec(text(30), 0..5))
            .prop_map(|(target, candidates)| Message::Rendezvous(Rendezvous { target, candidates })),
        (device_id(), vec(text(30), 0..5), any::<u64>()).prop_map(|(from, candidates, punch_at_ms)| {
            Message::Introduce(Introduce { from, candidates, punch_at_ms })
        }),
        (device_id(), any::<u64>()).prop_map(|(target, session)| Message::RelayOpen(RelayOpen { target, session })),
        (any::<u64>(), vec(any::<u8>(), 0..300))
            .prop_map(|(session, data)| Message::RelayData(RelayData { session, data })),
        any::<u64>().prop_map(|session| Message::RelayClose(RelayClose { session })),
    ]
}

/// Every message type, for the properties below and for the garbage tests.
pub fn message() -> impl Strategy<Value = Message> {
    prop_oneof![hello(), join(), members(), index(), request(), response(), progress(), addresses(), chat(), direct(), the_rest()]
}

fn roundtrip(m: &Message) -> Message {
    let mut out = Vec::new();
    m.encode(&mut out);
    let (frame, used) = Frame::split(&out).unwrap().unwrap();
    assert_eq!(used, out.len(), "one frame, all of it");
    assert_eq!(frame.kind, m.kind());
    Message::decode(frame).unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(cases()))]

    #[test]
    fn hello_roundtrips(m in hello()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn chat_roundtrips(m in chat()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn direct_roundtrips(m in direct()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn join_roundtrips(m in join()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn members_roundtrip(m in members()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn index_roundtrips(m in index()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn request_roundtrips(m in request()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn response_roundtrips(m in response()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn progress_roundtrips(m in progress()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn addresses_roundtrip(m in addresses()) { prop_assert_eq!(roundtrip(&m), m); }

    #[test]
    fn the_small_messages_roundtrip(m in the_rest()) { prop_assert_eq!(roundtrip(&m), m); }

    /// The payload-only path (for compression) gives the same bytes as the framed one.
    #[test]
    fn payload_and_frame_agree(m in message()) {
        let (mut framed, mut payload, mut manual) = (Vec::new(), Vec::new(), Vec::new());
        m.encode(&mut framed);
        m.encode_payload(&mut payload);
        Frame::write(m.kind(), 0, &payload, &mut manual);
        prop_assert_eq!(framed, manual);
    }

    /// Frames back to back split exactly where they were joined.
    #[test]
    fn a_stream_of_frames_splits_back(ms in vec(message(), 1..6)) {
        let mut stream = Vec::new();
        ms.iter().for_each(|m| m.encode(&mut stream));
        let mut rest = &stream[..];
        for m in &ms {
            let (frame, used) = Frame::split(rest).unwrap().unwrap();
            prop_assert_eq!(&Message::decode(frame).unwrap(), m);
            rest = &rest[used..];
        }
        prop_assert!(rest.is_empty());
    }
}
