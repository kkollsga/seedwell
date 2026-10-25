//! The messages two devices exchange inside TLS, one per frame ([`crate::codec`]).
//!
//! The decoder checks structure and caps: lengths, counts, UTF-8, enum values, sorted version vectors. Policy is
//! the receiver's: paths ([`crate::model::validate_path`], so one bad entry can be skipped without dropping its
//! whole batch), signatures ([`crate::sign::WriterTable`]) and proofs ([`crate::keys::FolderKey::verify_proofs`]).
//!
//! Bytes after the last field of a message are ignored, so a later version can append fields that older devices
//! skip. A frame of an unknown type decodes to [`Message::Unknown`], which the session ignores.

use crate::codec::{
    DecodeError, FLAG_COMPRESSED, Frame, Reader, put_bool, put_bytes, put_ivarint, put_len, put_str, put_varint,
};
use crate::device::{self, DeviceKey};
use crate::ids::{DeviceId, FolderId, ShortId};
use crate::keys::Proof;
use crate::model::{
    ChunkRef, Counter, FileInfo, FileKind, IndexEntry, MAX_CHUNK, MAX_COUNTERS, MAX_PATH, SignedFile, Version,
};
use crate::sign::{Writer, flags_byte, kind_byte};

/// The protocol version in [`Hello`].
pub const PROTOCOL: u32 = 1;

/// Longest client string (`Seedwell <version>`), bytes.
pub const MAX_CLIENT: usize = 128;
/// Longest device name, in characters.
pub const MAX_DEVICE_NAME: usize = 64;
/// Folders in one [`Join`].
pub const MAX_JOIN_FOLDERS: usize = 1024;
/// Proofs per folder in a [`Join`] (a writer sends 3).
pub const MAX_PROOFS: usize = 8;
/// Bytes of one proof of a kind this version does not know.
pub const MAX_PROOF_BYTES: usize = 256;
/// Writers in one [`Writers`].
pub const MAX_WRITERS: usize = 1024;
/// Entries in one [`Index`] batch.
pub const MAX_INDEX_ENTRIES: usize = 1000;
/// Chunks of one file. At most 36 bytes each on the wire, so one entry with this many still fits a frame; that is
/// about 1.5 TiB at the 4 MiB average chunk.
pub const MAX_CHUNKS: usize = 400_000;
/// Bytes of data in one [`Response`] or [`RelayData`]: one chunk.
pub const MAX_DATA: usize = MAX_CHUNK as usize;
/// Updates in one [`Progress`].
pub const MAX_PROGRESS_UPDATES: usize = 1000;
/// Address records in one [`Addresses`].
pub const MAX_RECORDS: usize = 256;
/// Addresses (or hole-punch candidates) in one record or message.
pub const MAX_ADDRS: usize = 32;
/// Longest address or relay string, bytes.
pub const MAX_ADDR: usize = 256;
/// Longest [`Close`] reason, bytes.
pub const MAX_REASON: usize = 256;

/// Type 1, first in each direction: who is talking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// [`PROTOCOL`].
    pub protocol: u32,
    /// `Seedwell <version>`.
    pub client: String,
    /// The device's name as its owner set it (at most [`MAX_DEVICE_NAME`] characters).
    pub device_name: String,
}

/// One folder in a [`Join`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinFolder {
    /// The folder.
    pub folder_id: FolderId,
    /// Every proof the sender can make ([`crate::keys::FolderKey::proofs`]).
    pub proofs: Vec<Proof>,
    /// The sender's index id for this folder (a new one means "my sequence numbers started again").
    pub my_index_id: u64,
    /// The receiver's index id as the sender last saw it.
    pub your_index_id: u64,
    /// The receiver's highest sequence the sender has, under `your_index_id`.
    pub your_max_seq: u64,
}

/// Type 2: the folders the sender shares with the receiver, and its proof for each.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Join {
    /// The folders.
    pub folders: Vec<JoinFolder>,
}

/// Type 3: writers of a folder the sender knows (each checked on arrival, [`crate::sign::WriterTable::insert`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Writers {
    /// The folder.
    pub folder_id: FolderId,
    /// The writers.
    pub writers: Vec<Writer>,
}

/// Type 4: a batch of the sender's index (at most 1000 entries or about 1 MiB).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Index {
    /// The folder.
    pub folder_id: FolderId,
    /// The sender's index id the sequences belong to.
    pub index_id: u64,
    /// Part of a full send (the receiver replaces what it had) rather than an update.
    pub full: bool,
    /// The entries.
    pub entries: Vec<IndexEntry>,
    /// The last batch of a full send.
    pub done: bool,
}

/// Type 5: a request for bytes of a file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// Matches the [`Response`].
    pub id: u32,
    /// The folder.
    pub folder_id: FolderId,
    /// The file.
    pub path: String,
    /// First byte.
    pub offset: u64,
    /// Bytes wanted (one chunk).
    pub len: u32,
    /// SHA-256 the bytes must have (the chunk's hash in a signed entry).
    pub hash: [u8; 32],
    /// Read from the sender's temporary file of a download in progress.
    pub from_temp: bool,
}

/// How a [`Request`] went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponseCode {
    /// 0: here is the data.
    Ok,
    /// 1: failed (also any code this version does not know).
    Generic,
    /// 2: no such file.
    NoSuchFile,
    /// 3: the request was invalid.
    Invalid,
    /// 4: busy, ask someone else or later.
    Busy,
}

impl ResponseCode {
    fn byte(self) -> u8 {
        match self {
            ResponseCode::Ok => 0,
            ResponseCode::Generic => 1,
            ResponseCode::NoSuchFile => 2,
            ResponseCode::Invalid => 3,
            ResponseCode::Busy => 4,
        }
    }

    fn from_byte(b: u8) -> Self {
        match b {
            0 => ResponseCode::Ok,
            2 => ResponseCode::NoSuchFile,
            3 => ResponseCode::Invalid,
            4 => ResponseCode::Busy,
            // A newer peer's code still means the request failed.
            _ => ResponseCode::Generic,
        }
    }
}

/// Type 6: the answer to a [`Request`].
#[derive(Clone, PartialEq, Eq)]
pub struct Response {
    /// The request's id.
    pub id: u32,
    /// How it went.
    pub code: ResponseCode,
    /// The bytes (empty unless `code` is `Ok`).
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Response {{ id: {}, code: {:?}, data: {} bytes }}", self.id, self.code, self.data.len())
    }
}

/// One file in a [`Progress`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressUpdate {
    /// The file.
    pub path: String,
    /// The version being downloaded.
    pub version: Version,
    /// Chunk indexes the sender has of it (in its temporary file). Empty: forget this file.
    pub have: Vec<u32>,
}

/// Type 7: which chunks of files in progress the sender already has, so others can fetch them from it early.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    /// The folder.
    pub folder_id: FolderId,
    /// The files.
    pub updates: Vec<ProgressUpdate>,
}

/// Where a device can be reached, signed by that device.
#[derive(Clone, PartialEq, Eq)]
pub struct AddressRecord {
    /// The device's public key (and signer).
    pub device_pk: [u8; 32],
    /// Addresses (`ip:port`, `utp://…`; the net layer's syntax).
    pub addrs: Vec<String>,
    /// A relay it can be reached through, or empty.
    pub relay: String,
    /// When it was issued, Unix seconds (a newer record replaces an older one).
    pub issued_s: u64,
    /// Ed25519 by `device_pk` over [`AddressRecord::signed_payload`].
    pub sig: [u8; 64],
}

const ADDRESSES: &str = "seedwell-sync/v1/addresses";

impl AddressRecord {
    /// A record signed by `key`.
    pub fn sign(key: &DeviceKey, addrs: Vec<String>, relay: String, issued_s: u64) -> Self {
        let mut r = AddressRecord { device_pk: *key.public_key(), addrs, relay, issued_s, sig: [0; 64] };
        r.sig = key.sign(&r.signed_payload());
        r
    }

    /// The signed bytes, in the same style as [`crate::sign::file_payload`] (big-endian, `str` = u32 length ‖
    /// UTF-8): `"seedwell-sync/v1/addresses" ‖ device_pk ‖ u32 n ‖ n × str(addr) ‖ str(relay) ‖ u64 issued_s`.
    pub fn signed_payload(&self) -> Vec<u8> {
        let mut out = ADDRESSES.as_bytes().to_vec();
        out.extend_from_slice(&self.device_pk);
        out.extend_from_slice(&(self.addrs.len() as u32).to_be_bytes());
        for a in self.addrs.iter().chain(std::iter::once(&self.relay)) {
            out.extend_from_slice(&(a.len() as u32).to_be_bytes());
            out.extend_from_slice(a.as_bytes());
        }
        out.extend_from_slice(&self.issued_s.to_be_bytes());
        out
    }

    /// Whether `device_pk` signed this record.
    pub fn verify(&self) -> bool {
        device::verify(&self.device_pk, &self.signed_payload(), &self.sig)
    }
}

impl std::fmt::Debug for AddressRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddressRecord")
            .field("device", &DeviceId::of_public_key(&self.device_pk))
            .field("addrs", &self.addrs)
            .field("relay", &self.relay)
            .field("issued_s", &self.issued_s)
            .finish_non_exhaustive()
    }
}

/// Type 8: where devices can be reached (peer exchange).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addresses {
    /// The records.
    pub records: Vec<AddressRecord>,
}

/// Type 10: the sender is closing the session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Close {
    /// Why, for the other side's log.
    pub reason: String,
}

/// Type 11: asks a common peer to introduce the sender to `target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendezvous {
    /// The device to reach.
    pub target: DeviceId,
    /// The sender's candidate addresses.
    pub candidates: Vec<String>,
}

/// Type 12: sent by the common peer to both sides: punch towards these candidates at this time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Introduce {
    /// The other device.
    pub from: DeviceId,
    /// Its candidate addresses.
    pub candidates: Vec<String>,
    /// When both sides start punching, Unix milliseconds.
    pub punch_at_ms: u64,
}

/// Type 13: member relay, open a byte pipe to `target`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayOpen {
    /// The device at the other end.
    pub target: DeviceId,
    /// The pipe's id in this session.
    pub session: u64,
}

/// Type 14: bytes for a relay pipe.
#[derive(Clone, PartialEq, Eq)]
pub struct RelayData {
    /// The pipe.
    pub session: u64,
    /// The bytes.
    pub data: Vec<u8>,
}

impl std::fmt::Debug for RelayData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RelayData {{ session: {}, data: {} bytes }}", self.session, self.data.len())
    }
}

/// Type 15: closes a relay pipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelayClose {
    /// The pipe.
    pub session: u64,
}

/// Every message, by frame type.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message {
    /// 1.
    Hello(Hello),
    /// 2.
    Join(Join),
    /// 3.
    Writers(Writers),
    /// 4.
    Index(Index),
    /// 5.
    Request(Request),
    /// 6.
    Response(Response),
    /// 7.
    Progress(Progress),
    /// 8.
    Addresses(Addresses),
    /// 9: keeps the session alive (empty).
    Ping,
    /// 10.
    Close(Close),
    /// 11.
    Rendezvous(Rendezvous),
    /// 12.
    Introduce(Introduce),
    /// 13.
    RelayOpen(RelayOpen),
    /// 14.
    RelayData(RelayData),
    /// 15.
    RelayClose(RelayClose),
    /// A type this version does not know: skipped, not an error. Encodes as an empty frame of that type.
    Unknown(u8),
}

impl Message {
    /// The frame type.
    pub fn kind(&self) -> u8 {
        match self {
            Message::Hello(_) => 1,
            Message::Join(_) => 2,
            Message::Writers(_) => 3,
            Message::Index(_) => 4,
            Message::Request(_) => 5,
            Message::Response(_) => 6,
            Message::Progress(_) => 7,
            Message::Addresses(_) => 8,
            Message::Ping => 9,
            Message::Close(_) => 10,
            Message::Rendezvous(_) => 11,
            Message::Introduce(_) => 12,
            Message::RelayOpen(_) => 13,
            Message::RelayData(_) => 14,
            Message::RelayClose(_) => 15,
            Message::Unknown(k) => *k,
        }
    }

    /// Appends the whole frame (header with no flags, then the payload).
    pub fn encode(&self, out: &mut Vec<u8>) {
        let start = out.len();
        out.extend_from_slice(&[0; 4]);
        out.push(self.kind());
        out.push(0);
        self.encode_payload(out);
        let len = (out.len() - start - 4) as u32;
        out[start..start + 4].copy_from_slice(&len.to_be_bytes());
    }

    /// Appends the payload only, for a sender that compresses it before framing ([`Frame::write`]).
    pub fn encode_payload(&self, out: &mut Vec<u8>) {
        match self {
            Message::Hello(m) => {
                put_varint(out, u64::from(m.protocol));
                put_str(out, &m.client);
                put_str(out, &m.device_name);
            }
            Message::Join(m) => {
                put_len(out, m.folders.len());
                for f in &m.folders {
                    out.extend_from_slice(&f.folder_id.0);
                    put_len(out, f.proofs.len());
                    f.proofs.iter().for_each(|p| put_proof(out, p));
                    put_varint(out, f.my_index_id);
                    put_varint(out, f.your_index_id);
                    put_varint(out, f.your_max_seq);
                }
            }
            Message::Writers(m) => {
                out.extend_from_slice(&m.folder_id.0);
                put_len(out, m.writers.len());
                for w in &m.writers {
                    out.extend_from_slice(&w.device_pk);
                    out.extend_from_slice(&w.cert);
                }
            }
            Message::Index(m) => {
                out.extend_from_slice(&m.folder_id.0);
                put_varint(out, m.index_id);
                put_bool(out, m.full);
                put_len(out, m.entries.len());
                m.entries.iter().for_each(|e| put_entry(out, e));
                put_bool(out, m.done);
            }
            Message::Request(m) => {
                put_varint(out, u64::from(m.id));
                out.extend_from_slice(&m.folder_id.0);
                put_str(out, &m.path);
                put_varint(out, m.offset);
                put_varint(out, u64::from(m.len));
                out.extend_from_slice(&m.hash);
                put_bool(out, m.from_temp);
            }
            Message::Response(m) => {
                put_varint(out, u64::from(m.id));
                out.push(m.code.byte());
                put_bytes(out, &m.data);
            }
            Message::Progress(m) => {
                out.extend_from_slice(&m.folder_id.0);
                put_len(out, m.updates.len());
                for u in &m.updates {
                    put_str(out, &u.path);
                    put_version(out, &u.version);
                    put_len(out, u.have.len());
                    u.have.iter().for_each(|&i| put_varint(out, u64::from(i)));
                }
            }
            Message::Addresses(m) => {
                put_len(out, m.records.len());
                for r in &m.records {
                    out.extend_from_slice(&r.device_pk);
                    put_strs(out, &r.addrs);
                    put_str(out, &r.relay);
                    put_varint(out, r.issued_s);
                    out.extend_from_slice(&r.sig);
                }
            }
            Message::Ping | Message::Unknown(_) => {}
            Message::Close(m) => put_str(out, &m.reason),
            Message::Rendezvous(m) => {
                out.extend_from_slice(&m.target.0);
                put_strs(out, &m.candidates);
            }
            Message::Introduce(m) => {
                out.extend_from_slice(&m.from.0);
                put_strs(out, &m.candidates);
                put_varint(out, m.punch_at_ms);
            }
            Message::RelayOpen(m) => {
                out.extend_from_slice(&m.target.0);
                put_varint(out, m.session);
            }
            Message::RelayData(m) => {
                put_varint(out, m.session);
                put_bytes(out, &m.data);
            }
            Message::RelayClose(m) => put_varint(out, m.session),
        }
    }

    /// Decodes one frame. Never panics, whatever the bytes.
    pub fn decode(frame: Frame<'_>) -> Result<Message, DecodeError> {
        if frame.flags & FLAG_COMPRESSED != 0 {
            return Err(DecodeError::Compressed);
        }
        let r = &mut Reader::new(frame.payload);
        Ok(match frame.kind {
            1 => Message::Hello(Hello {
                protocol: r.u32("protocol")?,
                client: r.str(MAX_CLIENT, "client")?,
                device_name: device_name(r)?,
            }),
            2 => Message::Join(Join { folders: r.list(MAX_JOIN_FOLDERS, 36, "folders", join_folder)? }),
            3 => Message::Writers(Writers {
                folder_id: FolderId(r.array("folder id")?),
                writers: r.list(MAX_WRITERS, 96, "writers", |r| {
                    Ok(Writer { device_pk: r.array("writer key")?, cert: r.array("writer certificate")? })
                })?,
            }),
            4 => Message::Index(Index {
                folder_id: FolderId(r.array("folder id")?),
                index_id: r.varint("index id")?,
                full: r.bool("full")?,
                entries: r.list(MAX_INDEX_ENTRIES, 90, "index entries", entry)?,
                done: r.bool("done")?,
            }),
            5 => Message::Request(Request {
                id: r.u32("request id")?,
                folder_id: FolderId(r.array("folder id")?),
                path: r.str(MAX_PATH, "path")?,
                offset: r.varint("offset")?,
                len: r.u32("length")?,
                hash: r.array("hash")?,
                from_temp: r.bool("from temp")?,
            }),
            6 => Message::Response(Response {
                id: r.u32("request id")?,
                code: ResponseCode::from_byte(r.u8("code")?),
                data: r.bytes(MAX_DATA, "data")?.to_vec(),
            }),
            7 => Message::Progress(Progress {
                folder_id: FolderId(r.array("folder id")?),
                updates: r.list(MAX_PROGRESS_UPDATES, 3, "progress updates", progress_update)?,
            }),
            8 => Message::Addresses(Addresses { records: r.list(MAX_RECORDS, 99, "address records", record)? }),
            9 => Message::Ping,
            10 => Message::Close(Close { reason: r.str(MAX_REASON, "reason")? }),
            11 => Message::Rendezvous(Rendezvous {
                target: DeviceId(r.array("target")?),
                candidates: strs(r, "candidates")?,
            }),
            12 => Message::Introduce(Introduce {
                from: DeviceId(r.array("from")?),
                candidates: strs(r, "candidates")?,
                punch_at_ms: r.varint("punch time")?,
            }),
            13 => Message::RelayOpen(RelayOpen {
                target: DeviceId(r.array("target")?),
                session: r.varint("session")?,
            }),
            14 => Message::RelayData(RelayData {
                session: r.varint("session")?,
                data: r.bytes(MAX_DATA, "data")?.to_vec(),
            }),
            15 => Message::RelayClose(RelayClose { session: r.varint("session")? }),
            k => Message::Unknown(k),
        })
    }
}

fn device_name(r: &mut Reader<'_>) -> Result<String, DecodeError> {
    // 64 characters of up to 4 bytes each; the character count is the real limit.
    let name = r.str(MAX_DEVICE_NAME * 4, "device name")?;
    if name.chars().count() > MAX_DEVICE_NAME {
        return Err(DecodeError::TooLong { what: "device name", max: MAX_DEVICE_NAME });
    }
    Ok(name)
}

fn put_strs(out: &mut Vec<u8>, list: &[String]) {
    put_len(out, list.len());
    list.iter().for_each(|s| put_str(out, s));
}

fn strs(r: &mut Reader<'_>, what: &'static str) -> Result<Vec<String>, DecodeError> {
    r.list(MAX_ADDRS, 1, what, |r| r.str(MAX_ADDR, "address"))
}

const PROOF_WRITER: u8 = 1;
const PROOF_READ: u8 = 2;
const PROOF_STORAGE: u8 = 3;

/// A proof is `u8 kind ‖ bytes`, so a device can carry past a kind from a newer version.
fn put_proof(out: &mut Vec<u8>, p: &Proof) {
    let (kind, data): (u8, &[u8]) = match p {
        Proof::Writer(c) => (PROOF_WRITER, c),
        Proof::Read(t) => (PROOF_READ, t),
        Proof::Storage(t) => (PROOF_STORAGE, t),
        Proof::Unknown { kind, data } => (*kind, data),
    };
    out.push(kind);
    put_bytes(out, data);
}

fn proof(r: &mut Reader<'_>) -> Result<Proof, DecodeError> {
    let kind = r.u8("proof kind")?;
    let data = r.bytes(MAX_PROOF_BYTES, "proof")?;
    let bad = |_| DecodeError::Invalid("proof length");
    Ok(match kind {
        PROOF_WRITER => Proof::Writer(data.try_into().map_err(bad)?),
        PROOF_READ => Proof::Read(data.try_into().map_err(bad)?),
        PROOF_STORAGE => Proof::Storage(data.try_into().map_err(bad)?),
        kind => Proof::Unknown { kind, data: data.to_vec() },
    })
}

fn join_folder(r: &mut Reader<'_>) -> Result<JoinFolder, DecodeError> {
    Ok(JoinFolder {
        folder_id: FolderId(r.array("folder id")?),
        proofs: r.list(MAX_PROOFS, 2, "proofs", proof)?,
        my_index_id: r.varint("index id")?,
        your_index_id: r.varint("index id")?,
        your_max_seq: r.varint("max sequence")?,
    })
}

/// A version: count ‖ n × (short id, fixed 8 bytes BE ‖ value varint). Short ids are hash prefixes, so a varint
/// would only make them longer.
fn put_version(out: &mut Vec<u8>, v: &Version) {
    put_len(out, v.counters().len());
    for c in v.counters() {
        out.extend_from_slice(&c.id.0.to_be_bytes());
        put_varint(out, c.value);
    }
}

fn version(r: &mut Reader<'_>) -> Result<Version, DecodeError> {
    let counters = r.list(MAX_COUNTERS, 9, "version", |r| {
        Ok(Counter { id: ShortId(u64::from_be_bytes(r.array("device")?)), value: r.varint("counter")? })
    })?;
    Version::from_counters(counters).map_err(|_| DecodeError::Invalid("version"))
}

/// Entry flags (the announcer's, not signed): bit 0 invalid. Other bits are reserved and ignored.
const ENTRY_INVALID: u8 = 1;
/// File flags (signed): bit 0 executable, bit 1 deleted. Other bits are refused, since the signature cannot cover
/// them.
const FILE_FLAGS: u8 = 0b11;

fn put_entry(out: &mut Vec<u8>, e: &IndexEntry) {
    encode_entry(out, e.sequence, e.invalid, &e.file);
}

/// Appends one index entry in its wire form (as in `Index` and `IndexUpdate`), from its parts so a caller
/// holding a [`SignedFile`] elsewhere need not build an [`IndexEntry`]. Stores reuse it for their records.
pub fn encode_entry(out: &mut Vec<u8>, sequence: u64, invalid: bool, file: &SignedFile) {
    put_varint(out, sequence);
    out.push(if invalid { ENTRY_INVALID } else { 0 });
    let f = &file.info;
    put_str(out, &f.path);
    out.push(kind_byte(f.kind));
    put_varint(out, f.size);
    put_ivarint(out, f.mtime_ns);
    out.push(flags_byte(f));
    put_version(out, &f.version);
    out.extend_from_slice(&f.modified_by.0.to_be_bytes());
    out.extend_from_slice(&f.file_id);
    put_str(out, &f.symlink_target);
    match &f.conflict_of {
        Some(p) => {
            put_bool(out, true);
            put_str(out, p);
        }
        None => put_bool(out, false),
    }
    put_len(out, f.chunks.len());
    for c in &f.chunks {
        put_varint(out, u64::from(c.len));
        out.extend_from_slice(&c.hash);
    }
    out.extend_from_slice(&file.sig);
}

/// Reads one index entry written by [`encode_entry`], with every wire check (caps, kinds, flags, versions).
pub fn decode_entry(r: &mut Reader<'_>) -> Result<IndexEntry, DecodeError> {
    entry(r)
}

fn entry(r: &mut Reader<'_>) -> Result<IndexEntry, DecodeError> {
    let sequence = r.varint("sequence")?;
    let invalid = r.u8("entry flags")? & ENTRY_INVALID != 0;
    let path = r.str(MAX_PATH, "path")?;
    let kind = match r.u8("kind")? {
        0 => FileKind::File,
        1 => FileKind::Dir,
        2 => FileKind::Symlink,
        _ => return Err(DecodeError::Invalid("kind")),
    };
    let size = r.varint("size")?;
    let mtime_ns = r.ivarint("mtime")?;
    let flags = r.u8("file flags")?;
    if flags & !FILE_FLAGS != 0 {
        return Err(DecodeError::Invalid("file flags"));
    }
    let info = FileInfo {
        path,
        kind,
        size,
        mtime_ns,
        exec: flags & 1 != 0,
        deleted: flags & 2 != 0,
        version: version(r)?,
        modified_by: ShortId(u64::from_be_bytes(r.array("modified by")?)),
        file_id: r.array("file id")?,
        symlink_target: r.str(MAX_PATH, "symlink target")?,
        conflict_of: if r.bool("has conflict of")? { Some(r.str(MAX_PATH, "conflict of")?) } else { None },
        chunks: r.list(MAX_CHUNKS, 33, "chunks", chunk)?,
    };
    Ok(IndexEntry { sequence, invalid, file: SignedFile { info, sig: r.array("signature")? } })
}

fn chunk(r: &mut Reader<'_>) -> Result<ChunkRef, DecodeError> {
    let len = r.u32("chunk length")?;
    if len > MAX_CHUNK {
        return Err(DecodeError::Invalid("chunk length"));
    }
    Ok(ChunkRef { len, hash: r.array("chunk hash")? })
}

fn progress_update(r: &mut Reader<'_>) -> Result<ProgressUpdate, DecodeError> {
    Ok(ProgressUpdate {
        path: r.str(MAX_PATH, "path")?,
        version: version(r)?,
        have: r.list(MAX_CHUNKS, 1, "chunk indexes", |r| r.u32("chunk index"))?,
    })
}

fn record(r: &mut Reader<'_>) -> Result<AddressRecord, DecodeError> {
    Ok(AddressRecord {
        device_pk: r.array("device key")?,
        addrs: strs(r, "addresses")?,
        relay: r.str(MAX_ADDR, "relay")?,
        issued_s: r.varint("issued")?,
        sig: r.array("signature")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(m: &Message) -> Message {
        let mut out = Vec::new();
        m.encode(&mut out);
        let (frame, used) = Frame::split(&out).unwrap().unwrap();
        assert_eq!(used, out.len());
        Message::decode(frame).unwrap()
    }

    #[test]
    fn hello_is_byte_exact() {
        let m = Message::Hello(Hello { protocol: 1, client: "Seedwell 0.1.5".into(), device_name: "Mac".into() });
        let mut out = Vec::new();
        m.encode(&mut out);
        let mut want = vec![0, 0, 0, 22, 1, 0, 1, 14];
        want.extend_from_slice(b"Seedwell 0.1.5");
        want.push(3);
        want.extend_from_slice(b"Mac");
        assert_eq!(out, want);
        assert_eq!(roundtrip(&m), m);
    }

    #[test]
    fn unknown_types_are_skipped_and_trailing_bytes_ignored() {
        let f = Frame { kind: 200, flags: 0, payload: b"whatever" };
        assert_eq!(Message::decode(f), Ok(Message::Unknown(200)));
        let f = Frame { kind: 15, flags: 0, payload: &[5, 0xff, 0xff] };
        assert_eq!(Message::decode(f), Ok(Message::RelayClose(RelayClose { session: 5 })));
        let f = Frame { kind: 9, flags: FLAG_COMPRESSED, payload: b"" };
        assert_eq!(Message::decode(f), Err(DecodeError::Compressed));
        let f = Frame { kind: 9, flags: 0x80, payload: b"" };
        assert_eq!(Message::decode(f), Ok(Message::Ping), "reserved flags are ignored");
    }

    #[test]
    fn device_names_are_capped_in_characters() {
        let name =
            |n: usize| Message::Hello(Hello { protocol: 1, client: String::new(), device_name: "ø".repeat(n) });
        assert_eq!(roundtrip(&name(64)), name(64));
        let mut out = Vec::new();
        name(65).encode(&mut out);
        let (frame, _) = Frame::split(&out).unwrap().unwrap();
        assert_eq!(Message::decode(frame), Err(DecodeError::TooLong { what: "device name", max: 64 }));
    }

    #[test]
    fn proofs_of_a_wrong_length_are_refused() {
        let mut p = vec![1];
        p.extend_from_slice(&[0; 32]); // folder id
        p.extend_from_slice(&[1, PROOF_READ, 31]);
        p.extend_from_slice(&[0; 31]);
        p.extend_from_slice(&[0, 0, 0]);
        let f = Frame { kind: 2, flags: 0, payload: &p };
        assert_eq!(Message::decode(f), Err(DecodeError::Invalid("proof length")));
    }

    #[test]
    fn address_records_are_signed() {
        let key = DeviceKey::from_seed(&[3; 32]);
        let r = AddressRecord::sign(&key, vec!["192.0.2.1:22000".into()], String::new(), 1_760_000_000);
        assert!(r.verify());
        let mut moved = r.clone();
        moved.addrs[0] = "192.0.2.2:22000".into();
        assert!(!moved.verify());
        let mut relayed = r.clone();
        relayed.relay = "x".into();
        assert!(!relayed.verify());
        let mut later = r.clone();
        later.issued_s += 1;
        assert!(!later.verify());
        // An address moved into the relay field changes the signed bytes too.
        let a = AddressRecord::sign(&key, vec!["a".into()], String::new(), 1);
        let b = AddressRecord { addrs: vec![], relay: "a".into(), ..a.clone() };
        assert!(!b.verify());
    }

    #[test]
    fn response_codes_from_newer_peers_mean_failure() {
        let f = Frame { kind: 6, flags: 0, payload: &[7, 99, 0] };
        let Ok(Message::Response(r)) = Message::decode(f) else { panic!() };
        assert_eq!((r.id, r.code, r.data.len()), (7, ResponseCode::Generic, 0));
    }
}
