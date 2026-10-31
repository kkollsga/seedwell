//! The pieces together, as two devices use them: invite, redeem, admit, exchange membership and signed entries.

use trs_sync_proto::codec::Frame;
use trs_sync_proto::model::{ChunkRef, FileInfo, FileKind, IndexEntry, Version};
use trs_sync_proto::members::{AdmitError, Body, Signer, Statement, check_redeem, new_invite, redeem_proof};
use trs_sync_proto::msg::{Index, Join, JoinFolder, Members, Message};
use trs_sync_proto::sign::{Writer, WriterTable, sign_file};
use trs_sync_proto::{Access, DeviceKey, FolderKey, InviteLink, Membership, Proof, Random, Role, random::RandomFailed};

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
fn an_owner_invites_a_reader() {
    let mut rng = Counter(1);
    // Fixed seeds: the counter's sequence repeats within 32 bytes, so two generated keys would be one.
    let laptop = DeviceKey::from_seed(&[1; 32]);
    let phone = DeviceKey::from_seed(&[2; 32]);
    let folder = FolderKey::generate(&mut rng).unwrap();
    let secret = folder.secret().unwrap().clone();
    let now = 1_800_000_000;

    // The laptop owns the folder, and makes a one-use Read only invitation; the phone pastes it out of a mail.
    let mut owner_view = Membership::new(*folder.public_key());
    owner_view.add([Statement::by_owner(&secret, Body::Grant { device: *laptop.public_key(), role: Role::Owner, serial: 1, at_s: now, invite: None })]);
    let invite = new_invite(*folder.public_key(), [5; 16], [6; 32], Role::ReadOnly, 1, now + 7 * 86_400, now, Signer::Folder, &|m| secret.sign(m));
    let link = InviteLink { invite, token_seed: [6; 32], name: Some("Photos".into()) };
    let pasted = InviteLink::parse(&format!("<{}>\r\n", link.format())).unwrap();
    assert_eq!(pasted.invite.folder_id(), folder.folder_id());
    assert_eq!(pasted.name.as_deref(), Some("Photos"));

    // After TLS both sides see the same exporter value and each other's device key. The laptop proves its grant;
    // the phone redeems the invitation, bound to the session.
    let exporter = [42u8; 32];
    let proofs = owner_view.proofs_for(laptop.public_key()).iter().map(|st| Proof::Grant(st.to_bytes())).collect();
    let join = |proofs| Message::Join(Join { folders: vec![JoinFolder { folder_id: folder.folder_id(), proofs, my_index_id: 1, your_index_id: 0, your_max_seq: 0 }] });
    let Message::Join(got) = send(&join(proofs)) else { panic!() };
    let mut phone_view = Membership::new(pasted.invite.folder_pk);
    for p in &got.folders[0].proofs {
        let Proof::Grant(b) = p else { panic!() };
        phone_view.add([Statement::from_bytes(b).unwrap()]);
    }
    assert_eq!(phone_view.role(laptop.public_key()), Some(Role::Owner));

    let redeem = vec![Proof::Invite(redeem_proof(&pasted.invite, &pasted.token_seed, phone.public_key(), &exporter))];
    let Message::Join(got) = send(&join(redeem)) else { panic!() };
    let Proof::Invite(b) = &got.folders[0].proofs[0] else { panic!() };
    let redeemed = check_redeem(b, phone.public_key(), &exporter).unwrap();
    assert_eq!(check_redeem(b, phone.public_key(), &[0; 32]), None, "another session");
    assert_eq!(owner_view.check_admit(laptop.public_key(), &redeemed, phone.public_key(), now), Ok(None));
    let grant = Statement::by_owner(&secret, Body::Grant { device: *phone.public_key(), role: redeemed.role, serial: owner_view.next_serial(), at_s: now, invite: Some(redeemed) });
    owner_view.add([grant]);
    assert_eq!(owner_view.check_admit(laptop.public_key(), &redeemed, laptop.public_key(), now), Ok(Some(Role::Owner)));
    assert_eq!(owner_view.check_admit(laptop.public_key(), &redeemed, &[3; 32], now), Err(AdmitError::UsedUp), "one use only");

    // The laptop sends the membership; the phone is a reader, and the laptop its only writer.
    let members = Message::Members(Members { folder_id: folder.folder_id(), statements: owner_view.statements().cloned().collect() });
    let Message::Members(got) = send(&members) else { panic!() };
    phone_view.add(got.statements);
    assert_eq!(phone_view.role(phone.public_key()), Some(Role::ReadOnly));
    assert_eq!(phone_view.role(phone.public_key()).map(Role::access), Some(Access::Read));
    let mut table = WriterTable::new(*folder.public_key());
    for pk in phone_view.writers() {
        assert_eq!(table.insert(Writer { device_pk: pk }), Ok(true));
    }
    assert_eq!(table.len(), 1);

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
        entries: vec![IndexEntry { sequence: 1, invalid: false, dataless: false, on_demand: false, file: signed.clone() }],
        done: true,
    });
    let Message::Index(got) = send(&index) else { panic!() };
    assert!(table.verify(&got.entries[0].file));

    // A reader that changed the file locally re-announces the version as invalid: the flag is not signed, so the
    // entry still verifies.
    let relayed = IndexEntry { sequence: 9, invalid: true, dataless: false, on_demand: false, file: signed };
    let Message::Index(got) = send(&Message::Index(Index { entries: vec![relayed], ..got })) else { panic!() };
    assert!(got.entries[0].invalid);
    assert!(table.verify(&got.entries[0].file));

    // A member that tampers with the entry it passes on is caught.
    let mut tampered = got.entries[0].file.clone();
    tampered.info.chunks[0].hash[0] ^= 1;
    assert!(!table.verify(&tampered));
}
