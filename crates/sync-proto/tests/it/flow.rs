//! The pieces together, as two devices use them: share a folder, prove access, exchange signed entries.

use trs_sync_proto::codec::Frame;
use trs_sync_proto::model::{ChunkRef, FileInfo, FileKind, IndexEntry, Version};
use trs_sync_proto::msg::{Index, Join, JoinFolder, Message, Writers};
use trs_sync_proto::sign::{Writer, WriterTable, sign_file};
use trs_sync_proto::{Access, DeviceKey, FolderKey, Link, Random, random::RandomFailed};

/// A deterministic random source, as simulations use.
struct Counter(u8);

impl Random for Counter {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), RandomFailed> {
        for b in out {
            self.0 = self.0.wrapping_mul(31).wrapping_add(7);
            *b = self.0;
        }
        Ok(())
    }
}

fn send(m: &Message) -> Message {
    let mut wire = Vec::new();
    m.encode(&mut wire);
    let (frame, _) = Frame::split(&wire).unwrap().unwrap();
    Message::decode(frame).unwrap()
}

#[test]
fn a_writer_shares_a_folder_with_a_reader() {
    let mut rng = Counter(1);
    let laptop = DeviceKey::generate(&mut rng).unwrap();
    let phone = DeviceKey::generate(&mut rng).unwrap();
    let folder = FolderKey::generate(&mut rng).unwrap();

    // The laptop shares a read-only link; the phone pastes it out of a mail.
    let link = Link { key: folder.read_only().unwrap(), name: Some("Photos".into()) };
    let pasted = Link::parse(&format!("<{}>\r\n", link.format())).unwrap();
    let phone_key = pasted.key;
    assert_eq!(phone_key.folder_id(), folder.folder_id());
    assert_eq!(pasted.name.as_deref(), Some("Photos"));

    // After TLS both sides see the same exporter value and each other's device key.
    let exporter = [42u8; 32];
    let join = Message::Join(Join {
        folders: vec![JoinFolder {
            folder_id: folder.folder_id(),
            proofs: folder.proofs(laptop.public_key(), &exporter),
            my_index_id: 1,
            your_index_id: 0,
            your_max_seq: 0,
        }],
    });
    let Message::Join(got) = send(&join) else { panic!() };
    let proofs = &got.folders[0].proofs;
    assert_eq!(phone_key.verify_proofs(laptop.public_key(), &exporter, proofs), Some(Access::Write));
    let phone_proofs = phone_key.proofs(phone.public_key(), &exporter);
    assert_eq!(folder.verify_proofs(phone.public_key(), &exporter, &phone_proofs), Some(Access::Read));

    // The laptop sends its writer certificate and a signed entry; the phone checks both.
    let cert = folder.writer_cert(laptop.public_key()).unwrap();
    let writers = Message::Writers(Writers {
        folder_id: folder.folder_id(),
        writers: vec![Writer { device_pk: *laptop.public_key(), cert }],
    });
    let Message::Writers(got) = send(&writers) else { panic!() };
    let mut table = WriterTable::new(*phone_key.public_key());
    for w in got.writers {
        assert_eq!(table.insert(w), Ok(true));
    }

    let me = laptop.device_id().short();
    let info = FileInfo {
        path: "2026/beach.jpg".into(),
        kind: FileKind::File,
        size: 3_000_000,
        mtime_ns: 1_760_000_000_000_000_000,
        exec: false,
        deleted: false,
        version: Version::new().bump(me, 1_760_000_000),
        modified_by: me,
        file_id: [7; 16],
        symlink_target: String::new(),
        conflict_of: None,
        chunks: vec![ChunkRef { len: 3_000_000, hash: [9; 32] }],
    };
    let signed = sign_file(&laptop, &folder.folder_id(), info);
    let index = Message::Index(Index {
        folder_id: folder.folder_id(),
        index_id: 1,
        full: true,
        entries: vec![IndexEntry { sequence: 1, invalid: false, file: signed.clone() }],
        done: true,
    });
    let Message::Index(got) = send(&index) else { panic!() };
    assert!(table.verify(&got.entries[0].file));

    // A reader that changed the file locally re-announces the version as invalid: the flag is not signed, so the
    // entry still verifies.
    let relayed = IndexEntry { sequence: 9, invalid: true, file: signed };
    let Message::Index(got) = send(&Message::Index(Index { entries: vec![relayed], ..got })) else { panic!() };
    assert!(got.entries[0].invalid);
    assert!(table.verify(&got.entries[0].file));

    // A member that tampers with the entry it passes on is caught.
    let mut tampered = got.entries[0].file.clone();
    tampered.info.chunks[0].hash[0] ^= 1;
    assert!(!table.verify(&tampered));
}
