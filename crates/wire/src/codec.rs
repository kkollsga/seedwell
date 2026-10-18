//! Incremental zero-copy decoder and reusable-buffer encoder.

use crate::error::{Error, Result};
use crate::handshake::Reserved;
use crate::message::{BLOCK_SIZE, BlockRequest, Message, MessageRef, OwnedMessage, id};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use core::ops::Range;

/// Absolute ceiling for any frame: 2^17 bytes.
pub const MAX_FRAME_LEN: u32 = 1 << 17;
/// Default cap for extended-message frames (ut_metadata data is 16 KiB + a small dict).
pub const DEFAULT_MAX_EXTENDED_LEN: u32 = 32 * 1024;

/// Length caps enforced before any buffering happens.
// size: 20 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted block in `piece`/`request`/`cancel`/`reject` (default 16 KiB).
    pub max_block_len: u32,
    /// Largest accepted extended frame, including the 2-byte header.
    pub max_extended_len: u32,
    /// Largest accepted bitfield (bytes) while the piece count is unknown (magnet links).
    pub max_bitfield_len: u32,
    /// Piece count once known; the bitfield must then be exactly `ceil(n / 8)` bytes.
    pub num_pieces: Option<u32>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_block_len: BLOCK_SIZE,
            max_extended_len: DEFAULT_MAX_EXTENDED_LEN,
            max_bitfield_len: MAX_FRAME_LEN - 1,
            num_pieces: None,
        }
    }
}

impl Limits {
    /// Defaults for a torrent with a known piece count.
    pub fn for_pieces(num_pieces: u32) -> Self {
        Self {
            num_pieces: Some(num_pieces),
            ..Self::default()
        }
    }
}

/// Stateless frame decoder. Tiny, `Copy`, shareable across connections.
///
/// Fast (BEP 6) and extended (BEP 10) frames are rejected with
/// [`Error::NotNegotiated`] unless enabled, so a peer cannot use features it did
/// not announce in the handshake.
// size: 24 bytes
#[derive(Clone, Copy, Debug)]
pub struct Decoder {
    limits: Limits,
    fast: bool,
    extended: bool,
}

type RangeMsg = Message<Range<usize>>;

impl Decoder {
    /// Decoder with the given limits and no optional features.
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            fast: false,
            extended: false,
        }
    }

    /// Enable features according to both sides' handshake reserved bytes.
    pub fn negotiated(mut self, ours: &Reserved, theirs: &Reserved) -> Self {
        self.fast = ours.both_fast(theirs);
        self.extended = ours.both_extension(theirs);
        self
    }

    /// Explicitly enable Fast / extended frames.
    pub fn with_features(mut self, fast: bool, extended: bool) -> Self {
        self.fast = fast;
        self.extended = extended;
        self
    }

    /// Update the piece count once metadata is known.
    pub fn set_num_pieces(&mut self, n: u32) {
        self.limits.num_pieces = Some(n);
    }

    /// Current limits.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Decode one frame from the front of `buf` as borrowed views.
    ///
    /// Returns the message and the number of bytes it occupies; the caller advances
    /// its buffer. `Ok(None)` means more bytes are needed. Never allocates, and
    /// rejects an oversized or malformed frame from its first 5 bytes, before the
    /// body arrives.
    pub fn decode_ref<'a>(&self, buf: &'a [u8]) -> Result<Option<(MessageRef<'a>, usize)>> {
        let Some((m, used)) = self.parse(buf)? else {
            return Ok(None);
        };
        let m = m.map(|r| buf.get(r).unwrap_or(&[]));
        Ok(Some((m, used)))
    }

    /// Decode and consume one frame from a `BytesMut`; payloads are `Bytes` slices
    /// sharing the buffer's allocation (no copy).
    pub fn decode(&self, buf: &mut BytesMut) -> Result<Option<OwnedMessage>> {
        let Some((m, used)) = self.parse(buf)? else {
            return Ok(None);
        };
        let frame: Bytes = buf.split_to(used).freeze();
        Ok(Some(m.map(|r| frame.slice(r))))
    }

    /// How many more bytes are needed before the next frame completes (0 if complete
    /// or if an error will be reported). Lets the caller reserve once for a 16 KiB block.
    pub fn bytes_needed(&self, buf: &[u8]) -> usize {
        let Some(h) = buf.get(..4) else {
            return 4 - buf.len();
        };
        let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
        if len > self.frame_cap() {
            return 0;
        }
        (4 + len as usize).saturating_sub(buf.len())
    }

    /// Ceiling for any frame: 2^17, raised only for a bitfield of a torrent whose
    /// (already verified) piece count needs more.
    fn frame_cap(&self) -> u32 {
        match self.limits.num_pieces {
            Some(n) => MAX_FRAME_LEN.max((crate::bitfield::byte_len(n) as u32).saturating_add(1)),
            None => MAX_FRAME_LEN,
        }
    }

    fn parse(&self, buf: &[u8]) -> Result<Option<(RangeMsg, usize)>> {
        let Some(h) = buf.get(..4) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes([h[0], h[1], h[2], h[3]]);
        let cap = self.frame_cap();
        if len > cap {
            return Err(Error::MessageTooLarge { len, max: cap });
        }
        if len == 0 {
            return Ok(Some((Message::KeepAlive, 4)));
        }
        let Some(&mid) = buf.get(4) else {
            return Ok(None);
        };
        // Validate the length for this id now, so a lying prefix never makes us buffer.
        self.check_len(mid, len)?;
        let total = 4 + len as usize;
        let Some(body) = buf.get(5..total) else {
            return Ok(None);
        };
        let at = |off: usize| -> Result<u32> {
            body.get(off..off + 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                .ok_or(Error::InvalidLength { id: mid, len })
        };
        // A block longer than the cap is a protocol violation; refusing it here
        // keeps the engine from ever reading an attacker-chosen amount from disk.
        let block = || -> Result<BlockRequest> {
            let r = BlockRequest {
                index: at(0)?,
                begin: at(4)?,
                length: at(8)?,
            };
            if r.length > self.limits.max_block_len {
                return Err(Error::InvalidField("length"));
            }
            Ok(r)
        };
        let m = match mid {
            id::CHOKE => Message::Choke,
            id::UNCHOKE => Message::Unchoke,
            id::INTERESTED => Message::Interested,
            id::NOT_INTERESTED => Message::NotInterested,
            id::HAVE => Message::Have(at(0)?),
            id::BITFIELD => Message::Bitfield(5..total),
            id::REQUEST => {
                let r = block()?;
                if r.length == 0 {
                    return Err(Error::InvalidField("length"));
                }
                Message::Request(r)
            }
            id::CANCEL => Message::Cancel(block()?),
            id::PIECE => Message::Piece {
                index: at(0)?,
                begin: at(4)?,
                data: 13..total,
            },
            id::PORT => {
                let p = body.get(..2).ok_or(Error::InvalidLength { id: mid, len })?;
                Message::Port(u16::from_be_bytes([p[0], p[1]]))
            }
            id::SUGGEST => Message::Suggest(at(0)?),
            id::HAVE_ALL => Message::HaveAll,
            id::HAVE_NONE => Message::HaveNone,
            id::REJECT => Message::RejectRequest(block()?),
            id::ALLOWED_FAST => Message::AllowedFast(at(0)?),
            id::EXTENDED => {
                let eid = *body.first().ok_or(Error::InvalidLength { id: mid, len })?;
                Message::Extended {
                    id: eid,
                    payload: 6..total,
                }
            }
            other => Message::Unknown(other),
        };
        Ok(Some((m, total)))
    }

    /// Per-id frame-length and feature validation (before the body is buffered).
    fn check_len(&self, mid: u8, len: u32) -> Result<()> {
        if mid != id::BITFIELD && len > MAX_FRAME_LEN {
            return Err(Error::MessageTooLarge {
                len,
                max: MAX_FRAME_LEN,
            });
        }
        let bad = Error::InvalidLength { id: mid, len };
        let exact = |n: u32| if len == n { Ok(()) } else { Err(bad.clone()) };
        match mid {
            id::CHOKE | id::UNCHOKE | id::INTERESTED | id::NOT_INTERESTED => exact(1),
            id::HAVE => exact(5),
            id::REQUEST | id::CANCEL => exact(13),
            id::PORT => exact(3),
            id::BITFIELD => {
                let body = len - 1;
                match self.limits.num_pieces {
                    Some(n) => {
                        if body as usize == crate::bitfield::byte_len(n) {
                            Ok(())
                        } else {
                            Err(Error::BadBitfield)
                        }
                    }
                    None if body <= self.limits.max_bitfield_len => Ok(()),
                    None => Err(Error::MessageTooLarge {
                        len,
                        max: self.limits.max_bitfield_len + 1,
                    }),
                }
            }
            id::PIECE => {
                let max = 9 + self.limits.max_block_len;
                if len > max {
                    Err(Error::MessageTooLarge { len, max })
                } else if len < 10 {
                    // An empty block carries no information and is never sent by real clients.
                    Err(bad)
                } else {
                    Ok(())
                }
            }
            id::SUGGEST | id::HAVE_ALL | id::HAVE_NONE | id::REJECT | id::ALLOWED_FAST => {
                if !self.fast {
                    return Err(Error::NotNegotiated(mid));
                }
                match mid {
                    id::HAVE_ALL | id::HAVE_NONE => exact(1),
                    id::REJECT => exact(13),
                    _ => exact(5),
                }
            }
            id::EXTENDED => {
                if !self.extended {
                    return Err(Error::NotNegotiated(mid));
                }
                if len > self.limits.max_extended_len {
                    Err(Error::MessageTooLarge {
                        len,
                        max: self.limits.max_extended_len,
                    })
                } else if len < 2 {
                    Err(bad)
                } else {
                    Ok(())
                }
            }
            // Unknown ids are skipped, but only up to the block-sized cap.
            _ => {
                let max = 9 + self.limits.max_block_len;
                if len > max {
                    Err(Error::MessageTooLarge { len, max })
                } else {
                    Ok(())
                }
            }
        }
    }
}

/// Writes messages into one reusable buffer.
///
/// Typical loop: `push(..)` several messages, write [`Encoder::pending`] to the
/// socket, [`Encoder::advance`] by the bytes written. When idle, call
/// [`Encoder::release_if_idle`] so a peer that once queued 16 KiB blocks does not pin them.
// size: 32 bytes (+ buffer capacity)
#[derive(Debug, Default)]
pub struct Encoder {
    buf: BytesMut,
}

impl Encoder {
    /// Empty encoder; allocates on first use.
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a message.
    pub fn push<D: AsRef<[u8]>>(&mut self, m: &Message<D>) {
        self.buf.reserve(m.encoded_len());
        m.encode(&mut self.buf);
    }

    /// Queue a `piece` message, copying the block into the buffer.
    pub fn push_piece(&mut self, index: u32, begin: u32, data: &[u8]) {
        self.buf.reserve(13 + data.len());
        self.buf.put_slice(&crate::message::piece_header(
            index,
            begin,
            data.len() as u32,
        ));
        self.buf.put_slice(data);
    }

    /// Queue an extended message (id 20) whose payload is produced by `f`.
    ///
    /// The frame length is back-filled after `f` runs, so payload encoders do not
    /// need to know their size in advance.
    pub fn push_extended(&mut self, ext_id: u8, f: impl FnOnce(&mut BytesMut)) {
        let start = self.buf.len();
        self.buf.put_u32(0);
        self.buf.put_u8(id::EXTENDED);
        self.buf.put_u8(ext_id);
        f(&mut self.buf);
        let frame_len = (self.buf.len() - start - 4) as u32;
        if let Some(slot) = self.buf.get_mut(start..start + 4) {
            slot.copy_from_slice(&frame_len.to_be_bytes());
        }
    }

    /// Queue a pipeline of `request` messages.
    pub fn push_requests(&mut self, reqs: impl IntoIterator<Item = BlockRequest>) {
        for r in reqs {
            self.push(&Message::<&[u8]>::Request(r));
        }
    }

    /// Bytes ready to be written to the socket.
    pub fn pending(&self) -> &[u8] {
        &self.buf
    }

    /// Mark `n` bytes as written.
    pub fn advance(&mut self, n: usize) {
        self.buf.advance(n.min(self.buf.len()));
    }

    /// Nothing queued.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Drop the allocation if nothing is queued.
    ///
    /// `BytesMut::advance` shrinks the visible capacity, so the real allocation
    /// cannot be measured; releasing it outright is the only reliable way for an
    /// idle peer to stop pinning a 16 KiB buffer.
    pub fn release_if_idle(&mut self) {
        if self.buf.is_empty() {
            self.buf = BytesMut::new();
        }
    }

    /// Current buffer capacity.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::piece_header;

    fn dec() -> Decoder {
        Decoder::new(Limits::default()).with_features(true, true)
    }

    fn sample_messages() -> Vec<Message<Vec<u8>>> {
        let r = BlockRequest {
            index: 3,
            begin: 16384,
            length: 16384,
        };
        vec![
            Message::KeepAlive,
            Message::Choke,
            Message::Unchoke,
            Message::Interested,
            Message::NotInterested,
            Message::Have(42),
            Message::Bitfield(vec![0xFF, 0x80]),
            Message::Request(r),
            Message::Piece {
                index: 1,
                begin: 0,
                data: vec![7; 100],
            },
            Message::Cancel(r),
            Message::Port(51413),
            Message::Suggest(5),
            Message::HaveAll,
            Message::HaveNone,
            Message::RejectRequest(r),
            Message::AllowedFast(77),
            Message::Extended {
                id: 1,
                payload: b"d1:ai1ee".to_vec(),
            },
        ]
    }

    #[test]
    fn roundtrip_every_message() {
        for m in sample_messages() {
            let mut v = Vec::new();
            m.encode(&mut v);
            let (d, used) = dec().decode_ref(&v).unwrap().unwrap();
            assert_eq!(used, v.len());
            assert_eq!(d.map(|s| s.to_vec()), m);
        }
    }

    #[test]
    fn roundtrip_owned_zero_copy() {
        let mut bm = BytesMut::new();
        let payload = vec![9u8; 5000];
        Message::Piece {
            index: 1,
            begin: 2,
            data: &payload[..],
        }
        .encode(&mut bm);
        let base = bm.as_ptr() as usize;
        let m = dec().decode(&mut bm).unwrap().unwrap();
        let Message::Piece { index, begin, data } = m else {
            panic!()
        };
        assert_eq!((index, begin), (1, 2));
        assert_eq!(&data[..], &payload[..]);
        // The block is a slice of the original allocation, 13 bytes in.
        assert_eq!(data.as_ptr() as usize, base + 13);
        assert!(bm.is_empty());
    }

    #[test]
    fn partial_reads_at_every_split_point() {
        let mut stream = Vec::new();
        let msgs = sample_messages();
        for m in &msgs {
            m.encode(&mut stream);
        }
        let d = dec();
        for split in 0..=stream.len() {
            // Feed the stream in two chunks, as a socket might.
            let mut buf = BytesMut::new();
            let mut got = Vec::new();
            for chunk in [&stream[..split], &stream[split..]] {
                buf.extend_from_slice(chunk);
                while let Some(m) = d.decode(&mut buf).unwrap() {
                    got.push(m.map(|b| b.to_vec()));
                }
            }
            assert_eq!(got, msgs, "split {split}");
            assert!(buf.is_empty());
        }
    }

    #[test]
    fn byte_at_a_time() {
        let mut stream = Vec::new();
        for m in sample_messages() {
            m.encode(&mut stream);
        }
        let d = dec();
        let mut buf = BytesMut::new();
        let mut n = 0;
        for b in stream {
            buf.put_u8(b);
            while d.decode(&mut buf).unwrap().is_some() {
                n += 1;
            }
        }
        assert_eq!(n, sample_messages().len());
    }

    #[test]
    fn oversized_rejected_without_body() {
        let d = dec();
        // 4-byte prefix only: huge length is refused immediately.
        let e = d
            .decode_ref(&(MAX_FRAME_LEN + 1).to_be_bytes())
            .unwrap_err();
        assert!(matches!(e, Error::MessageTooLarge { .. }));
        let e = d.decode_ref(&u32::MAX.to_be_bytes()).unwrap_err();
        assert!(matches!(e, Error::MessageTooLarge { .. }));
        // Piece larger than 16 KiB block + header.
        let mut v = (9 + BLOCK_SIZE + 1).to_be_bytes().to_vec();
        v.push(id::PIECE);
        assert!(matches!(
            d.decode_ref(&v),
            Err(Error::MessageTooLarge { .. })
        ));
        // Extended over the extended cap but under 2^17.
        let mut v = (DEFAULT_MAX_EXTENDED_LEN + 1).to_be_bytes().to_vec();
        v.push(id::EXTENDED);
        assert!(matches!(
            d.decode_ref(&v),
            Err(Error::MessageTooLarge { .. })
        ));
        // Unknown id with a large body.
        let mut v = (100_000u32).to_be_bytes().to_vec();
        v.push(99);
        assert!(matches!(
            d.decode_ref(&v),
            Err(Error::MessageTooLarge { .. })
        ));
    }

    #[test]
    fn bytes_needed_hint() {
        let d = dec();
        assert_eq!(d.bytes_needed(&[]), 4);
        assert_eq!(d.bytes_needed(&[0, 0]), 2);
        let mut v = Vec::new();
        v.extend_from_slice(&piece_header(0, 0, 100));
        assert_eq!(d.bytes_needed(&v), 100);
        assert_eq!(d.bytes_needed(&u32::MAX.to_be_bytes()), 0);
    }

    #[test]
    fn wrong_fixed_lengths_rejected() {
        let d = dec();
        for (id, len) in [
            (0u8, 2u32),
            (1, 3),
            (2, 5),
            (3, 9),
            (4, 4),
            (4, 6),
            (6, 12),
            (8, 14),
            (9, 4),
            (9, 2),
        ] {
            let mut v = len.to_be_bytes().to_vec();
            v.push(id);
            v.extend(core::iter::repeat_n(0u8, len as usize - 1));
            assert!(
                matches!(d.decode_ref(&v), Err(Error::InvalidLength { .. })),
                "id {id} len {len}"
            );
        }
        for (id, len) in [(13u8, 4u32), (14, 5), (15, 2), (16, 5), (17, 6)] {
            let mut v = len.to_be_bytes().to_vec();
            v.push(id);
            v.extend(core::iter::repeat_n(0u8, len as usize - 1));
            assert!(
                matches!(d.decode_ref(&v), Err(Error::InvalidLength { .. })),
                "id {id}"
            );
        }
        // Empty piece block and extended frame without an id.
        assert!(matches!(
            d.decode_ref(&[0, 0, 0, 9, 7, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(Error::InvalidLength { .. })
        ));
        assert!(matches!(
            d.decode_ref(&[0, 0, 0, 1, 20]),
            Err(Error::InvalidLength { .. })
        ));
    }

    #[test]
    fn invalid_length_detected_before_body_arrives() {
        // `choke` claiming 1000 bytes: refused from the first five bytes.
        let v = [0, 0, 3, 0xE8, 0];
        assert!(matches!(
            dec().decode_ref(&v),
            Err(Error::InvalidLength { .. })
        ));
    }

    #[test]
    fn feature_gating() {
        let plain = Decoder::new(Limits::default());
        assert_eq!(
            plain.decode_ref(&[0, 0, 0, 1, 14]).unwrap_err(),
            Error::NotNegotiated(14)
        );
        assert_eq!(
            plain.decode_ref(&[0, 0, 0, 2, 20, 0]).unwrap_err(),
            Error::NotNegotiated(20)
        );
        let neg = Decoder::new(Limits::default())
            .negotiated(&Reserved::ours(false), &Reserved::ours(true));
        assert!(neg.decode_ref(&[0, 0, 0, 1, 14]).unwrap().is_some());
        assert!(neg.decode_ref(&[0, 0, 0, 2, 20, 0]).unwrap().is_some());
        let one_sided =
            Decoder::new(Limits::default()).negotiated(&Reserved::ours(false), &Reserved::none());
        assert!(one_sided.decode_ref(&[0, 0, 0, 1, 14]).is_err());
    }

    #[test]
    fn bitfield_size_enforced_when_piece_count_known() {
        let d = Decoder::new(Limits::for_pieces(10));
        assert!(
            d.decode_ref(&[0, 0, 0, 3, 5, 0xFF, 0xC0])
                .unwrap()
                .is_some()
        );
        assert_eq!(
            d.decode_ref(&[0, 0, 0, 2, 5, 0xFF]).unwrap_err(),
            Error::BadBitfield
        );
        assert_eq!(
            d.decode_ref(&[0, 0, 0, 4, 5, 0, 0, 0]).unwrap_err(),
            Error::BadBitfield
        );
        // Rejected from the header alone; no need to wait for 100 KB.
        let mut big = 100_001u32.to_be_bytes().to_vec();
        big.push(5);
        assert_eq!(d.decode_ref(&big).unwrap_err(), Error::BadBitfield);
        // Zero pieces: empty bitfield only.
        let z = Decoder::new(Limits::for_pieces(0));
        assert!(z.decode_ref(&[0, 0, 0, 1, 5]).unwrap().is_some());
    }

    #[test]
    fn huge_torrent_bitfield_may_exceed_2_17_but_nothing_else_may() {
        // 2M pieces -> 250,000-byte bitfield: legal because the piece count is known.
        let n = 2_000_000u32;
        let d = Decoder::new(Limits::for_pieces(n));
        let blen = crate::bitfield::byte_len(n);
        let mut v = ((blen + 1) as u32).to_be_bytes().to_vec();
        v.push(5);
        v.extend(core::iter::repeat_n(0u8, blen));
        assert!(d.decode_ref(&v).unwrap().is_some());
        // A different message of the same size is still refused from its header.
        for id in [99u8, 7, 20] {
            let mut v = ((blen + 1) as u32).to_be_bytes().to_vec();
            v.push(id);
            let d = d.with_features(true, true);
            assert!(
                matches!(d.decode_ref(&v), Err(Error::MessageTooLarge { .. })),
                "id {id}"
            );
        }
    }

    #[test]
    fn bitfield_cap_when_piece_count_unknown() {
        let d = Decoder::new(Limits {
            max_bitfield_len: 10,
            ..Limits::default()
        });
        let mut ok = 11u32.to_be_bytes().to_vec();
        ok.push(5);
        ok.extend_from_slice(&[0; 10]);
        assert!(d.decode_ref(&ok).unwrap().is_some());
        let mut big = 12u32.to_be_bytes().to_vec();
        big.push(5);
        assert!(matches!(
            d.decode_ref(&big),
            Err(Error::MessageTooLarge { .. })
        ));
    }

    #[test]
    fn unknown_id_is_skipped() {
        let (m, used) = dec()
            .decode_ref(&[0, 0, 0, 3, 99, 1, 2, 0xAB])
            .unwrap()
            .unwrap();
        assert_eq!(m, Message::Unknown(99));
        assert_eq!(used, 7);
    }

    #[test]
    fn garbage_never_panics() {
        // Deterministic xorshift garbage through every entry point.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let d = dec();
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let n = (x % 64) as usize;
            let mut v: Vec<u8> = (0..n).map(|i| (x >> (i % 56)) as u8).collect();
            if n > 4 {
                v[0] = 0;
                v[1] = 0;
            }
            let _ = d.decode_ref(&v);
            let mut bm = BytesMut::from(&v[..]);
            let _ = d.decode(&mut bm);
        }
    }

    #[test]
    fn encoder_reuse_and_shrink() {
        let mut e = Encoder::new();
        e.push(&Message::<&[u8]>::Interested);
        e.push_piece(1, 0, &[5; 16384]);
        assert_eq!(e.pending().len(), 5 + 13 + 16384);
        let n = e.pending().len();
        e.advance(n + 10);
        assert!(e.is_empty());
        // Steady-state reuse must not keep growing the allocation.
        for _ in 0..200 {
            e.push_piece(1, 0, &[5; 16384]);
            let n = e.pending().len();
            e.advance(n);
        }
        assert!(e.capacity() <= 64 * 1024, "{}", e.capacity());
        e.push_piece(1, 0, &[5; 16384]);
        let n = e.pending().len();
        e.advance(n);
        e.release_if_idle();
        assert_eq!(e.capacity(), 0);
        e.push_requests((0..3).map(|i| BlockRequest {
            index: 0,
            begin: i * 16384,
            length: 16384,
        }));
        assert_eq!(e.pending().len(), 3 * 17);
    }

    #[test]
    fn push_extended_backfills_length() {
        let mut e = Encoder::new();
        e.push_extended(3, |b| b.put_slice(b"hello"));
        assert_eq!(
            e.pending(),
            [0, 0, 0, 7, 20, 3, b'h', b'e', b'l', b'l', b'o']
        );
    }
}
