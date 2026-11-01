//! Images and files in chat messages (SPEC §7G.7 *Attachments*): a text message names its attachments in its later
//! fields (`extra` of a channel message, signed with the rest; the sealed body's later fields of a private message);
//! the bytes are blobs that members fetch from each other by hash ([`crate::msg::BlobWant`],
//! [`crate::msg::BlobData`]).
//!
//! ```text
//! extra       = n × (tag varint ‖ varint length ‖ bytes)          (a tag this version does not know is skipped)
//! tag 1       = caption_len varint ‖ count varint (1..=10) ‖ count × (varint length ‖ attachment)
//! attachment  = sha256(32) ‖ kind u8 ‖ size varint ‖ width varint ‖ height varint ‖ name str ‖ thumb bytes
//!               [‖ pieces(32), kind 16 only] ‖ (later fields)
//!   kind      = 1 PNG | 2 JPEG | 3 GIF | 4 WebP | 5 SVG | 16 file | other: shown as a file of an unknown kind
//!   thumb     = a JPEG of at most 160 px a side and 12 KiB, or empty
//!   pieces    = SHA-256 of the file's piece table: n × SHA-256 of each PIECE (1 MiB) of the file, the last shorter
//! ```
//!
//! An image is fetched whole and checked against `sha256`. A file (kind 16, any kind and size, width and height 0) is
//! fetched only when its user asks: first its piece table (a blob named by `pieces`, checked against it), then each
//! piece, checked against its entry in the table, so a bad piece is fetched again alone and a download resumes from
//! the pieces it has. `sha256` names the whole file.
//!
//! The body stays a text message: `caption ‖ "\n" ‖ one line per attachment` (`[image: name.png, 240 KB]`,
//! `[file: report.pdf, 1.2 GB]`), and `caption_len` says where the caption ends. A device of 0.1.14 or earlier keeps,
//! passes on and shows the whole body, so its user reads what was sent; a device that knows tag 1 shows the caption
//! and the attachments.

use crate::codec::{DecodeError, Reader, put_bytes, put_str, put_varint};

/// The tag of the attachments record.
pub const TAG_ATTACHMENTS: u64 = 1;
/// The kind of a file attachment (anything but an image within its caps).
pub const KIND_FILE: u8 = 16;
/// Bytes of a file's piece: what one request asks for and one hash in the piece table checks.
pub const PIECE: u64 = 1 << 20;
/// Most attachments in one message.
pub const MAX_ATTACHMENTS: usize = 10;
/// Longest file name, characters.
pub const MAX_NAME: usize = 120;
/// Largest inline preview, bytes.
pub const MAX_THUMB: usize = 12 * 1024;
/// Most bytes of later fields a message may carry, so that its fields, with a full text, stay within
/// [`crate::chat::MAX_FIELDS`] for every version.
pub const MAX_EXTRA: usize = 48 * 1024;

/// The pieces of a file of `size` bytes (0 for an empty file).
pub fn piece_count(size: u64) -> u64 {
    size.div_ceil(PIECE)
}

/// Where piece `i` of a file of `size` bytes starts, and its length.
pub fn piece_span(size: u64, i: u64) -> (u64, u64) {
    let start = i.saturating_mul(PIECE).min(size);
    (start, (size - start).min(PIECE))
}

/// Bytes of the piece table of a file of `size` bytes.
pub fn table_len(size: u64) -> u64 {
    piece_count(size).saturating_mul(32)
}

/// The name a received file is saved under: the last part of the name its sender gave (`/`, `\\` and `:` end a part),
/// without control characters, `..`, leading dots or trailing dots and spaces, at most [`MAX_NAME`] characters (the
/// extension kept), and `file` when nothing is left. Never a path.
pub fn safe_name(name: &str) -> String {
    let last = name.rsplit(['/', '\\', ':']).next().unwrap_or_default();
    let clean: String = last.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim().trim_start_matches('.').trim_end_matches(['.', ' ']).trim();
    let clean = if clean.chars().all(|c| c == '.') { "" } else { clean };
    let mut out = if clean.chars().count() > MAX_NAME {
        match clean.rsplit_once('.') {
            Some((stem, ext)) if !stem.is_empty() && ext.chars().count() <= 16 => {
                let keep = MAX_NAME - ext.chars().count() - 1;
                format!("{}.{ext}", stem.chars().take(keep).collect::<String>().trim_end())
            }
            _ => clean.chars().take(MAX_NAME).collect(),
        }
    } else {
        clean.to_string()
    };
    if out.is_empty() {
        out = "file".into();
    }
    out
}

/// One image or file of a message.
#[derive(Clone, PartialEq, Eq)]
pub struct AttachmentRef {
    /// SHA-256 of its bytes: what members fetch it by (an image), or name it by (a file).
    pub hash: [u8; 32],
    /// Its format (1 PNG, 2 JPEG, 3 GIF, 4 WebP, 5 SVG) or [`KIND_FILE`].
    pub kind: u8,
    /// Its size, bytes.
    pub size: u64,
    /// Its width, pixels.
    pub width: u32,
    /// Its height, pixels.
    pub height: u32,
    /// Its file name.
    pub name: String,
    /// A small JPEG preview (empty: none).
    pub thumb: Vec<u8>,
    /// A file's piece table's SHA-256 (zero for an image).
    pub pieces: [u8; 32],
}

impl AttachmentRef {
    /// A file, not an image: fetched only when asked for, piece by piece.
    pub fn is_file(&self) -> bool {
        self.kind == KIND_FILE
    }
}

impl std::fmt::Debug for AttachmentRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AttachmentRef({}, kind {}, {} bytes, {}×{}, {:?})", crate::members::hex(&self.hash[..8]), self.kind, self.size, self.width, self.height, self.name)
    }
}

/// A message's images, and where its caption ends in its body.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Attachments {
    /// Bytes of the body that are the caption (the rest is the text for earlier versions).
    pub caption_len: usize,
    /// The images, in order.
    pub refs: Vec<AttachmentRef>,
}

impl Attachments {
    /// The record in a message's later fields.
    pub fn to_extra(&self) -> Vec<u8> {
        let mut rec = Vec::new();
        put_varint(&mut rec, self.caption_len as u64);
        put_varint(&mut rec, self.refs.len() as u64);
        for a in &self.refs {
            let mut one = a.hash.to_vec();
            one.push(a.kind);
            put_varint(&mut one, a.size);
            put_varint(&mut one, u64::from(a.width));
            put_varint(&mut one, u64::from(a.height));
            put_str(&mut one, &a.name);
            put_bytes(&mut one, &a.thumb);
            if a.is_file() {
                one.extend_from_slice(&a.pieces);
            }
            put_bytes(&mut rec, &one);
        }
        let mut out = Vec::with_capacity(rec.len() + 8);
        put_varint(&mut out, TAG_ATTACHMENTS);
        put_bytes(&mut out, &rec);
        out
    }

    /// The images a message's later fields name, for a body of `body_len` bytes: `None` when there are none, or the
    /// record is not one this version reads (the message then shows as plain text).
    pub fn from_extra(extra: &[u8], body_len: usize) -> Option<Attachments> {
        let r = &mut Reader::new(extra);
        while r.remaining() > 0 {
            let tag = r.varint("tag").ok()?;
            let rec = r.bytes(extra.len(), "record").ok()?;
            if tag == TAG_ATTACHMENTS {
                return Self::read(rec, body_len).ok();
            }
        }
        None
    }

    fn read(rec: &[u8], body_len: usize) -> Result<Attachments, DecodeError> {
        let r = &mut Reader::new(rec);
        let caption_len = usize::try_from(r.varint("caption length")?).map_err(|_| DecodeError::Invalid("caption length"))?;
        if caption_len > body_len {
            return Err(DecodeError::Invalid("caption length"));
        }
        let n = r.count(MAX_ATTACHMENTS, 40, "attachments")?;
        if n == 0 {
            return Err(DecodeError::Invalid("attachments"));
        }
        let mut refs = Vec::with_capacity(n);
        for _ in 0..n {
            let one = &mut Reader::new(r.bytes(rec.len(), "attachment")?);
            let hash = one.array("hash")?;
            let kind = one.u8("kind")?;
            let size = one.varint("size")?;
            let width = one.u32("width")?;
            let height = one.u32("height")?;
            let name = one.str(MAX_NAME * 4, "name")?;
            let thumb = one.bytes(MAX_THUMB, "thumb")?.to_vec();
            let pieces = if kind == KIND_FILE { one.array("pieces")? } else { [0; 32] };
            refs.push(AttachmentRef { hash, kind, size, width, height, name, thumb, pieces });
        }
        Ok(Attachments { caption_len, refs })
    }

    /// The caption of a message whose body is `body`.
    pub fn caption<'a>(&self, body: &'a str) -> &'a str {
        body.get(..self.caption_len).unwrap_or(body).trim_end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChatMessage, Draft, MessageKind};
    use crate::device::DeviceKey;
    use crate::ids::FolderId;

    fn one(n: u8, thumb: usize) -> AttachmentRef {
        AttachmentRef { hash: [n; 32], kind: 1, size: 240_000, width: 1280, height: 800, name: format!("shot {n}.png"), thumb: vec![n; thumb], pieces: [0; 32] }
    }

    fn file(n: u8, size: u64) -> AttachmentRef {
        AttachmentRef { hash: [n; 32], kind: KIND_FILE, size, width: 0, height: 0, name: format!("report {n}.pdf"), thumb: Vec::new(), pieces: [n + 1; 32] }
    }

    #[test]
    fn files_carry_their_piece_table_and_mix_with_images() {
        let a = Attachments { caption_len: 0, refs: vec![file(1, 5 * PIECE + 3), one(2, 10), file(3, 0)] };
        let back = Attachments::from_extra(&a.to_extra(), 40).unwrap();
        assert_eq!(back, a);
        assert!(back.refs[0].is_file() && !back.refs[1].is_file());
        assert_eq!(back.refs[0].pieces, [2; 32]);
        // A file record cut before its table hash is not read.
        let mut short = a.clone();
        short.refs.truncate(1);
        let rec = short.to_extra();
        assert!(Attachments::from_extra(&rec[..rec.len() - 1], 40).is_none());
        // Pieces: 1 MiB each, the last shorter; an empty file has none.
        assert_eq!((piece_count(0), piece_count(1), piece_count(PIECE), piece_count(PIECE + 1)), (0, 1, 1, 2));
        assert_eq!(piece_span(PIECE + 5, 1), (PIECE, 5));
        assert_eq!(piece_span(PIECE + 5, 0), (0, PIECE));
        assert_eq!(table_len(3 * PIECE), 96);
    }

    #[test]
    fn a_received_name_is_never_a_path() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("C:\\Users\\x\\evil.exe"), "evil.exe");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("..."), "file");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name(".hidden"), "hidden");
        assert_eq!(safe_name("a\u{0}b\nc.txt"), "abc.txt");
        assert_eq!(safe_name("report. "), "report");
        assert_eq!(safe_name("x:y"), "y");
        let long = format!("{}.tar.gz", "a".repeat(300));
        let s = safe_name(&long);
        assert!(s.chars().count() <= MAX_NAME && s.ends_with(".gz"), "{s}");
        assert_eq!(safe_name("Rapport 2026 – endelig.pdf"), "Rapport 2026 – endelig.pdf");
    }

    #[test]
    fn attachments_roundtrip_and_unknown_records_are_skipped() {
        let a = Attachments { caption_len: 5, refs: vec![one(1, 100), one(2, 0)] };
        let mut extra = Vec::new();
        // A record of a later version first: skipped.
        put_varint(&mut extra, 9);
        put_bytes(&mut extra, b"later");
        extra.extend(a.to_extra());
        assert_eq!(Attachments::from_extra(&extra, 30), Some(a.clone()));
        assert_eq!(Attachments::from_extra(&extra, 4), None, "a caption longer than the body");
        assert_eq!(Attachments::from_extra(b"", 10), None);
        assert_eq!(Attachments::from_extra(&[1, 200], 10), None, "cut off");
        assert_eq!(a.caption("look\n[image: shot 1.png, 240 KB]"), "look");
        let none = Attachments { caption_len: 0, refs: vec![] }.to_extra();
        assert_eq!(Attachments::from_extra(&none, 10), None, "no images");
    }

    /// A message with images, as a device of 0.1.14 reads it: the same signed text message, its body the caption and
    /// one line per image, the images record kept and passed on with the signature intact.
    #[test]
    fn a_device_of_0_1_14_reads_the_text_and_passes_the_images_on() {
        let key = DeviceKey::from_seed(&[3; 32]);
        let ch = FolderId([4; 32]);
        let body = "the new logo\n[image: logo.svg, 12 KB]\n[image: shot.png, 240 KB]";
        let att = Attachments { caption_len: "the new logo".len(), refs: vec![one(1, MAX_THUMB), one(2, MAX_THUMB)] };
        let draft = Draft { id: [1; 16], seq: 1, at_ms: 1, reply_to: None, topic: None, agent: None, kind: MessageKind::Text, body: body.as_bytes().to_vec() };
        let m = ChatMessage::sign_with_extra(&key, &ch, draft, att.to_extra()).unwrap();
        // The reader is 0.1.14's: kind, body, extra and the signature over them are unchanged since.
        let old = ChatMessage::from_bytes(&m.to_bytes()).unwrap();
        assert_eq!((old.kind, old.text(), old.verify(&ch)), (MessageKind::Text, Some(body), true));
        assert_eq!(old.to_bytes(), m.to_bytes(), "passed on as it came");
        assert_eq!(Attachments::from_extra(&old.extra, old.body.len()), Some(att));
        // Ten images with full previews and a full text still fit every version's limit on a message's fields.
        let ten = Attachments { caption_len: 0, refs: (0..MAX_ATTACHMENTS as u8).map(|n| one(n, 4000)).collect() };
        let full = Draft { id: [2; 16], seq: 2, at_ms: 1, reply_to: None, topic: None, agent: None, kind: MessageKind::Text, body: vec![b'a'; crate::chat::MAX_TEXT] };
        let big = ChatMessage::sign_with_extra(&key, &ch, full.clone(), ten.to_extra()).unwrap();
        assert!(ChatMessage::from_bytes(&big.to_bytes()).unwrap().verify(&ch));
        let too_much = Attachments { caption_len: 0, refs: (0..MAX_ATTACHMENTS as u8).map(|n| one(n, MAX_THUMB)).collect() };
        assert!(ChatMessage::sign_with_extra(&key, &ch, full, too_much.to_extra()).is_err(), "over MAX_EXTRA");
    }
}
