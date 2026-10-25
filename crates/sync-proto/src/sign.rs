//! Signed file versions: every version is signed by the writer device that made it, and every device checks every
//! entry before it believes it (SPEC §7G.1). A member that passes data on cannot change it.
//!
//! The signed bytes are a fixed encoding of their own ([`file_payload`]), not the wire encoding, so the wire format
//! can change without invalidating signatures.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use ring::digest::{Context, SHA256};

use crate::device::{self, DeviceKey};
use crate::ids::{DeviceId, FolderId, ShortId};
use crate::model::{ChunkRef, FileInfo, FileKind, SignedFile};

const FILE: &str = "seedwell-sync/v1/file";

/// The kind byte in [`file_payload`] (and on the wire).
pub(crate) fn kind_byte(kind: FileKind) -> u8 {
    match kind {
        FileKind::File => 0,
        FileKind::Dir => 1,
        FileKind::Symlink => 2,
    }
}

/// The flags byte: bit 0 executable, bit 1 deleted.
pub(crate) fn flags_byte(info: &FileInfo) -> u8 {
    u8::from(info.exec) | (u8::from(info.deleted) << 1)
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    // Paths and link targets are capped far below 4 GiB (MAX_PATH), so the length always fits.
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// `SHA-256(u32 n ‖ n × (u32 len ‖ hash32))`, every integer big-endian.
pub fn chunks_hash(chunks: &[ChunkRef]) -> [u8; 32] {
    let mut ctx = Context::new(&SHA256);
    ctx.update(&(chunks.len() as u32).to_be_bytes());
    for c in chunks {
        ctx.update(&c.len.to_be_bytes());
        ctx.update(&c.hash);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(ctx.finish().as_ref());
    out
}

/// The bytes a writer signs for one version (big-endian, `str` = u32 length ‖ UTF-8):
///
/// ```text
/// "seedwell-sync/v1/file" ‖ folder_id(32)
/// ‖ str(path) ‖ u8 kind ‖ u64 size ‖ i64 mtime_ns ‖ u8 flags(bit0 exec, bit1 deleted)
/// ‖ u16 n ‖ n × (u64 id, u64 value)          (version)
/// ‖ u64 modified_by ‖ file_id(16) ‖ str(symlink_target) ‖ u8 has_conflict_of ‖ [str(conflict_of)]
/// ‖ chunks_hash(32)
/// ```
///
/// The folder id is in it so a version cannot be replayed into another folder.
pub fn file_payload(folder_id: &FolderId, info: &FileInfo) -> Vec<u8> {
    let counters = info.version.counters();
    let mut out = Vec::with_capacity(FILE.len() + 32 + info.path.len() + 64 + counters.len() * 16 + 32 + 16);
    out.extend_from_slice(FILE.as_bytes());
    out.extend_from_slice(&folder_id.0);
    put_str(&mut out, &info.path);
    out.push(kind_byte(info.kind));
    out.extend_from_slice(&info.size.to_be_bytes());
    out.extend_from_slice(&info.mtime_ns.to_be_bytes());
    out.push(flags_byte(info));
    // A version holds at most MAX_COUNTERS (256) counters.
    out.extend_from_slice(&(counters.len() as u16).to_be_bytes());
    for c in counters {
        out.extend_from_slice(&c.id.0.to_be_bytes());
        out.extend_from_slice(&c.value.to_be_bytes());
    }
    out.extend_from_slice(&info.modified_by.0.to_be_bytes());
    out.extend_from_slice(&info.file_id);
    put_str(&mut out, &info.symlink_target);
    match &info.conflict_of {
        Some(p) => {
            out.push(1);
            put_str(&mut out, p);
        }
        None => out.push(0),
    }
    out.extend_from_slice(&chunks_hash(&info.chunks));
    out
}

/// Signs a version this device made. `info.modified_by` should be this device's short id: a version signed by
/// another device than the one it names never verifies.
pub fn sign_file(device_key: &DeviceKey, folder_id: &FolderId, info: FileInfo) -> SignedFile {
    debug_assert_eq!(info.modified_by, device_key.device_id().short(), "a device signs only its own versions");
    let sig = device_key.sign(&file_payload(folder_id, &info));
    SignedFile { info, sig }
}

/// Whether `file` carries `device_pk`'s signature for this folder.
pub fn verify_file(folder_id: &FolderId, device_pk: &[u8; 32], file: &SignedFile) -> bool {
    device::verify(device_pk, &file_payload(folder_id, &file.info), &file.sig)
}

/// A writer device of a folder: its public key. The folder's membership decides who writes
/// ([`crate::members::Membership::writers`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Writer {
    /// The device's Ed25519 public key.
    pub device_pk: [u8; 32],
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Writer({:?})", DeviceId::of_public_key(&self.device_pk))
    }
}

/// Why a writer was not added to a [`WriterTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadWriter {
    /// Another writer already has this short id (two keys whose SHA-256 share 64 bits: not seen in practice).
    ShortIdTaken,
}

impl std::fmt::Display for BadWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BadWriter::ShortIdTaken => f.write_str("another writer already has this short id"),
        }
    }
}

impl std::error::Error for BadWriter {}

/// The writers of one folder, by short id: what every entry is checked against. Checking an entry is one
/// signature check.
#[derive(Clone, Debug)]
pub struct WriterTable {
    folder_id: FolderId,
    // Ordered, so iteration is the same on every run.
    writers: BTreeMap<ShortId, Writer>,
}

impl WriterTable {
    /// An empty table for the folder with this signing public key.
    pub fn new(folder_pk: [u8; 32]) -> Self {
        WriterTable { folder_id: FolderId::of_public_key(&folder_pk), writers: BTreeMap::new() }
    }

    /// The folder this table is for.
    pub fn folder_id(&self) -> FolderId {
        self.folder_id
    }

    /// Adds a writer. `Ok(true)` when it is new, `Ok(false)` when it was known. The short id is computed here
    /// from the key, never taken from the sender.
    pub fn insert(&mut self, writer: Writer) -> Result<bool, BadWriter> {
        match self.writers.entry(DeviceId::of_public_key(&writer.device_pk).short()) {
            Entry::Vacant(v) => {
                v.insert(writer);
                Ok(true)
            }
            Entry::Occupied(o) if o.get().device_pk == writer.device_pk => Ok(false),
            Entry::Occupied(_) => Err(BadWriter::ShortIdTaken),
        }
    }

    /// Removes a writer: its versions are refused from now on.
    pub fn remove(&mut self, id: ShortId) -> bool {
        self.writers.remove(&id).is_some()
    }

    /// The writer with this short id.
    pub fn get(&self, id: ShortId) -> Option<&Writer> {
        self.writers.get(&id)
    }

    /// Every writer, by short id.
    pub fn iter(&self) -> impl Iterator<Item = &Writer> {
        self.writers.values()
    }

    /// Number of writers.
    pub fn len(&self) -> usize {
        self.writers.len()
    }

    /// No writers yet.
    pub fn is_empty(&self) -> bool {
        self.writers.is_empty()
    }

    /// Whether `file` was signed by the writer it names (`modified_by`), for this folder.
    pub fn verify(&self, file: &SignedFile) -> bool {
        self.get(file.info.modified_by).is_some_and(|w| verify_file(&self.folder_id, &w.device_pk, file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{FolderKey, FolderSecret};

    fn folder() -> FolderKey {
        FolderKey::Owner(FolderSecret::from_bytes([1; 32]))
    }
    use crate::model::{Counter, Version};


    fn info(by: &DeviceKey) -> FileInfo {
        let me = by.device_id().short();
        let other = ShortId(me.0 ^ 1);
        let (lo, hi) = if me < other { (me, other) } else { (other, me) };
        FileInfo {
            path: "Docs/report.txt".into(),
            kind: FileKind::File,
            size: 131_072,
            mtime_ns: 1_760_000_000_123_456_789,
            exec: false,
            deleted: false,
            version: Version::from_counters(vec![Counter { id: lo, value: 3 }, Counter { id: hi, value: 7 }]).unwrap(),
            modified_by: me,
            file_id: [4; 16],
            symlink_target: String::new(),
            conflict_of: None,
            chunks: vec![ChunkRef { len: 65_536, hash: [1; 32] }, ChunkRef { len: 65_536, hash: [2; 32] }],
        }
    }

    #[test]
    fn signed_files_verify() {
        let dev = DeviceKey::from_seed(&[9; 32]);
        let fid = folder().folder_id();
        let f = sign_file(&dev, &fid, info(&dev));
        assert!(verify_file(&fid, dev.public_key(), &f));
        assert!(!verify_file(&FolderId([0; 32]), dev.public_key(), &f), "another folder");
        assert!(!verify_file(&fid, DeviceKey::from_seed(&[8; 32]).public_key(), &f), "another device");
    }

    /// Every field of the payload, changed by one bit (or one step), must break the signature.
    #[test]
    fn a_one_bit_change_in_any_field_breaks_the_signature() {
        let dev = DeviceKey::from_seed(&[9; 32]);
        let fid = folder().folder_id();
        let good = sign_file(&dev, &fid, info(&dev));
        type Change = Box<dyn Fn(&mut FileInfo)>;
        let changes: Vec<(&str, Change)> = vec![
            ("path", Box::new(|i| i.path = "Docs/report.txu".into())),
            ("kind", Box::new(|i| i.kind = FileKind::Dir)),
            ("size", Box::new(|i| i.size ^= 1)),
            ("mtime", Box::new(|i| i.mtime_ns ^= 1)),
            ("exec", Box::new(|i| i.exec = true)),
            ("deleted", Box::new(|i| i.deleted = true)),
            ("version value", Box::new(|i| {
                let mut c = i.version.counters().to_vec();
                c[0].value ^= 1;
                i.version = Version::from_counters(c).unwrap();
            })),
            ("version id", Box::new(|i| {
                let mut c = i.version.counters().to_vec();
                c[0].id.0 ^= 1 << 63;
                c.sort_by_key(|c| c.id);
                i.version = Version::from_counters(c).unwrap();
            })),
            ("version length", Box::new(|i| i.version = Version::new())),
            ("modified_by", Box::new(|i| i.modified_by.0 ^= 1)),
            ("file_id", Box::new(|i| i.file_id[15] ^= 1)),
            ("symlink_target", Box::new(|i| i.symlink_target = "a".into())),
            ("conflict_of", Box::new(|i| i.conflict_of = Some(String::new()))),
            ("conflict_of text", Box::new(|i| i.conflict_of = Some("x".into()))),
            ("chunk hash", Box::new(|i| i.chunks[1].hash[31] ^= 1)),
            ("chunk len", Box::new(|i| i.chunks[0].len ^= 1)),
            ("chunk count", Box::new(|i| {
                i.chunks.pop();
            })),
        ];
        for (what, change) in changes {
            let mut bad = good.clone();
            change(&mut bad.info);
            assert_ne!(bad.info, good.info, "{what}: the change did nothing");
            assert!(!verify_file(&fid, dev.public_key(), &bad), "{what}");
        }
        for bit in 0..512 {
            let mut bad = good.clone();
            bad.sig[bit / 8] ^= 1 << (bit % 8);
            assert!(!verify_file(&fid, dev.public_key(), &bad), "signature bit {bit}");
        }
    }

    #[test]
    fn the_payload_is_the_documented_layout() {
        let dev = DeviceKey::from_seed(&[9; 32]);
        let fid = FolderId([0xAA; 32]);
        let mut i = info(&dev);
        i.conflict_of = Some("c".into());
        let p = file_payload(&fid, &i);
        let mut want = b"seedwell-sync/v1/file".to_vec();
        want.extend_from_slice(&[0xAA; 32]);
        want.extend_from_slice(&15u32.to_be_bytes());
        want.extend_from_slice(b"Docs/report.txt");
        want.push(0);
        want.extend_from_slice(&131_072u64.to_be_bytes());
        want.extend_from_slice(&1_760_000_000_123_456_789i64.to_be_bytes());
        want.push(0);
        want.extend_from_slice(&2u16.to_be_bytes());
        for c in i.version.counters() {
            want.extend_from_slice(&c.id.0.to_be_bytes());
            want.extend_from_slice(&c.value.to_be_bytes());
        }
        want.extend_from_slice(&i.modified_by.0.to_be_bytes());
        want.extend_from_slice(&[4; 16]);
        want.extend_from_slice(&0u32.to_be_bytes());
        want.push(1);
        want.extend_from_slice(&1u32.to_be_bytes());
        want.push(b'c');
        let mut ch = 2u32.to_be_bytes().to_vec();
        for (len, h) in [(65_536u32, [1u8; 32]), (65_536, [2; 32])] {
            ch.extend_from_slice(&len.to_be_bytes());
            ch.extend_from_slice(&h);
        }
        want.extend_from_slice(ring::digest::digest(&SHA256, &ch).as_ref());
        assert_eq!(p, want);
        assert_eq!(chunks_hash(&i.chunks).as_slice(), ring::digest::digest(&SHA256, &ch).as_ref());
    }

    #[test]
    fn the_writer_table_checks_signers() {
        let key = folder();
        let fid = key.folder_id();
        let (a, b) = (DeviceKey::from_seed(&[9; 32]), DeviceKey::from_seed(&[8; 32]));
        let mut t = WriterTable::new(*key.public_key());
        assert_eq!(t.folder_id(), fid);
        assert_eq!(t.insert(Writer { device_pk: *a.public_key() }), Ok(true));
        assert_eq!(t.insert(Writer { device_pk: *a.public_key() }), Ok(false));
        assert_eq!(t.len(), 1);

        let fa = sign_file(&a, &fid, info(&a));
        assert!(t.verify(&fa));
        let fb = sign_file(&b, &fid, info(&b));
        assert!(!t.verify(&fb), "b is not a writer yet");
        assert_eq!(t.insert(Writer { device_pk: *b.public_key() }), Ok(true));
        assert!(t.verify(&fb));

        // b signs a version that names a as its maker: checked against a's key, so it fails.
        let mut forged = info(&b);
        forged.modified_by = a.device_id().short();
        let forged = SignedFile { sig: b.sign(&file_payload(&fid, &forged)), info: forged };
        assert!(!t.verify(&forged));
        assert_eq!(t.iter().count(), 2);
        // Removed: its versions no longer count.
        assert!(t.remove(b.device_id().short()));
        assert!(!t.verify(&fb));
    }
}
