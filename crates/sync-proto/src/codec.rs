//! Framing and the primitive encodings of the wire protocol.
//!
//! * Before TLS, the connecting side sends an 8-byte [`Magic`], so one port can serve sessions, the relay protocol
//!   and hole-punch probes.
//! * Inside TLS, everything is a frame: `u32 BE len` (of what follows) `‖ u8 type ‖ u8 flags ‖ payload`.
//! * In a payload, integers are LEB128 varints (signed ones zigzag), hashes, keys and signatures are fixed-size
//!   byte strings, and strings and lists carry a varint length or count first.
//!
//! The [`Reader`] checks every length against its cap and against the bytes left *before* allocating, so a hostile
//! peer cannot make a device allocate more than the frame it already sent (and frames are capped too).

use std::fmt;

/// Largest value of a frame's `len` field: a 16 MiB chunk plus room for its response header.
pub const MAX_FRAME: u32 = (16 << 20) + 64;

/// Bytes before the payload: `u32 len ‖ u8 type ‖ u8 flags`.
pub const FRAME_HEADER: usize = 6;

/// Flag bit 0: the payload is deflate-compressed. The net layer compresses and decompresses; this crate only
/// carries the bit.
pub const FLAG_COMPRESSED: u8 = 1;

/// What the connecting side sends before TLS.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Magic {
    /// `SWSYNC1\n`: a sync session (TLS follows).
    Session,
    /// `SWRELAY1`: the relay protocol.
    Relay,
    /// `SWPUNCH1`: a hole-punch probe.
    Punch,
}

impl Magic {
    /// Its 8 bytes.
    pub const fn bytes(self) -> [u8; 8] {
        match self {
            Magic::Session => *b"SWSYNC1\n",
            Magic::Relay => *b"SWRELAY1",
            Magic::Punch => *b"SWPUNCH1",
        }
    }

    /// The magic these 8 bytes are, if any.
    pub fn parse(b: &[u8; 8]) -> Option<Magic> {
        [Magic::Session, Magic::Relay, Magic::Punch].into_iter().find(|m| m.bytes() == *b)
    }
}

/// Why a frame or message was refused. Each says what was wrong, for the log line that drops the peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The input ended inside a field.
    Truncated(&'static str),
    /// A list or string longer than its cap.
    TooLong {
        /// The field.
        what: &'static str,
        /// Its cap.
        max: usize,
    },
    /// A varint longer than 10 bytes or over 64 bits, or an integer over its type's range.
    Varint(&'static str),
    /// A string that is not UTF-8.
    Utf8(&'static str),
    /// A value outside what the field allows.
    Invalid(&'static str),
    /// A frame whose `len` is over [`MAX_FRAME`].
    FrameTooLarge(u32),
    /// A frame whose `len` is under 2 (no room for type and flags).
    FrameTooShort,
    /// A frame still compressed: the net layer must inflate it first.
    Compressed,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Truncated(w) => write!(f, "message ends inside {w}"),
            DecodeError::TooLong { what, max } => write!(f, "{what} is over its limit of {max}"),
            DecodeError::Varint(w) => write!(f, "{w} is not a valid integer"),
            DecodeError::Utf8(w) => write!(f, "{w} is not UTF-8"),
            DecodeError::Invalid(w) => write!(f, "{w} has a value that is not allowed"),
            DecodeError::FrameTooLarge(n) => write!(f, "frame of {n} bytes is over the limit of {MAX_FRAME}"),
            DecodeError::FrameTooShort => f.write_str("frame too short for its header"),
            DecodeError::Compressed => f.write_str("frame is still compressed"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// One frame: its type, flags and payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The message type (see [`crate::msg::Message`]).
    pub kind: u8,
    /// Flag bits ([`FLAG_COMPRESSED`]; the others are reserved and ignored).
    pub flags: u8,
    /// The payload.
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// Reads the `len` field of a frame header: how many bytes follow it. Lets a reader know how much to wait for
    /// (and refuse an oversized frame) from the first 4 bytes.
    pub fn read_len(head: &[u8; 4]) -> Result<u32, DecodeError> {
        let len = u32::from_be_bytes(*head);
        if len > MAX_FRAME {
            return Err(DecodeError::FrameTooLarge(len));
        }
        if len < 2 {
            return Err(DecodeError::FrameTooShort);
        }
        Ok(len)
    }

    /// Splits one whole frame off the front of `buf`: `Ok(None)` when more bytes are needed, else the frame and
    /// how many bytes it used.
    pub fn split(buf: &'a [u8]) -> Result<Option<(Frame<'a>, usize)>, DecodeError> {
        let Some(head) = buf.first_chunk::<4>() else { return Ok(None) };
        let total = 4 + Self::read_len(head)? as usize;
        if buf.len() < total {
            return Ok(None);
        }
        Ok(Some((Frame { kind: buf[4], flags: buf[5], payload: &buf[FRAME_HEADER..total] }, total)))
    }

    /// Appends a frame with this type, flags and payload. `payload` must keep the frame under [`MAX_FRAME`]; the
    /// senders keep batches small enough (index batches about 1 MiB, data at most one chunk).
    pub fn write(kind: u8, flags: u8, payload: &[u8], out: &mut Vec<u8>) {
        debug_assert!(payload.len() + 2 <= MAX_FRAME as usize, "frame over MAX_FRAME");
        out.extend_from_slice(&((payload.len() + 2) as u32).to_be_bytes());
        out.push(kind);
        out.push(flags);
        out.extend_from_slice(payload);
    }
}

/// Appends `v` as a LEB128 varint.
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Appends a signed integer as a zigzag varint (small magnitudes stay short either side of 0).
pub fn put_ivarint(out: &mut Vec<u8>, v: i64) {
    put_varint(out, ((v << 1) ^ (v >> 63)) as u64);
}

/// Appends a length or count.
pub fn put_len(out: &mut Vec<u8>, n: usize) {
    put_varint(out, n as u64);
}

/// Appends a string: varint length ‖ UTF-8.
pub fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

/// Appends a byte string: varint length ‖ bytes.
pub fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_len(out, b.len());
    out.extend_from_slice(b);
}

/// Appends a bool as one byte, 0 or 1.
pub fn put_bool(out: &mut Vec<u8>, b: bool) {
    out.push(u8::from(b));
}

/// Reads a payload front to back. Every read names its field, for the error.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    /// A reader over `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf }
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> usize {
        self.buf.len()
    }

    /// The next `n` bytes.
    pub fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], DecodeError> {
        if self.buf.len() < n {
            return Err(DecodeError::Truncated(what));
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    /// One byte.
    pub fn u8(&mut self, what: &'static str) -> Result<u8, DecodeError> {
        Ok(self.take(1, what)?[0])
    }

    /// A fixed-size byte string (hash, key, signature).
    pub fn array<const N: usize>(&mut self, what: &'static str) -> Result<[u8; N], DecodeError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N, what)?);
        Ok(out)
    }

    /// A bool: exactly 0 or 1.
    pub fn bool(&mut self, what: &'static str) -> Result<bool, DecodeError> {
        match self.u8(what)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(DecodeError::Invalid(what)),
        }
    }

    /// A LEB128 varint of at most 64 bits.
    pub fn varint(&mut self, what: &'static str) -> Result<u64, DecodeError> {
        let mut v = 0u64;
        for i in 0..10 {
            let b = self.u8(what)?;
            // The 10th byte holds only bit 63.
            if i == 9 && b > 1 {
                return Err(DecodeError::Varint(what));
            }
            v |= u64::from(b & 0x7f) << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::Varint(what))
    }

    /// A varint that must fit in 32 bits.
    pub fn u32(&mut self, what: &'static str) -> Result<u32, DecodeError> {
        u32::try_from(self.varint(what)?).map_err(|_| DecodeError::Varint(what))
    }

    /// A zigzag varint.
    pub fn ivarint(&mut self, what: &'static str) -> Result<i64, DecodeError> {
        let v = self.varint(what)?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    /// A count of items each at least `min_item` bytes long, at most `max`. Checking against the bytes left means a
    /// claimed count can never ask for more memory than the frame itself holds.
    pub fn count(&mut self, max: usize, min_item: usize, what: &'static str) -> Result<usize, DecodeError> {
        let n = self.varint(what)?;
        if n > max as u64 {
            return Err(DecodeError::TooLong { what, max });
        }
        let n = n as usize;
        if n.saturating_mul(min_item.max(1)) > self.remaining() {
            return Err(DecodeError::Truncated(what));
        }
        Ok(n)
    }

    /// A byte string of at most `max` bytes.
    pub fn bytes(&mut self, max: usize, what: &'static str) -> Result<&'a [u8], DecodeError> {
        let n = self.count(max, 1, what)?;
        self.take(n, what)
    }

    /// A UTF-8 string of at most `max` bytes.
    pub fn str(&mut self, max: usize, what: &'static str) -> Result<String, DecodeError> {
        let b = self.bytes(max, what)?;
        std::str::from_utf8(b).map(str::to_owned).map_err(|_| DecodeError::Utf8(what))
    }

    /// A list of at most `max` items, each at least `min_item` bytes on the wire, read with `item`.
    pub fn list<T>(
        &mut self,
        max: usize,
        min_item: usize,
        what: &'static str,
        mut item: impl FnMut(&mut Self) -> Result<T, DecodeError>,
    ) -> Result<Vec<T>, DecodeError> {
        let n = self.count(max, min_item, what)?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(item(self)?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_roundtrip_at_the_edges() {
        for v in [0, 1, 127, 128, 16_383, 16_384, u32::MAX as u64, u64::MAX - 1, u64::MAX] {
            let mut out = Vec::new();
            put_varint(&mut out, v);
            assert_eq!(Reader::new(&out).varint("v"), Ok(v));
        }
        for v in [0, 1, -1, 63, -64, i64::MIN, i64::MAX] {
            let mut out = Vec::new();
            put_ivarint(&mut out, v);
            assert_eq!(Reader::new(&out).ivarint("v"), Ok(v));
        }
        let mut out = Vec::new();
        put_ivarint(&mut out, -1);
        assert_eq!(out, [1], "zigzag keeps small negatives short");
    }

    #[test]
    fn bad_varints_are_refused() {
        assert_eq!(Reader::new(&[0x80; 11]).varint("v"), Err(DecodeError::Varint("v")));
        let mut over = vec![0xff; 9];
        over.push(0x02);
        assert_eq!(Reader::new(&over).varint("v"), Err(DecodeError::Varint("v")));
        assert_eq!(Reader::new(&[0x80]).varint("v"), Err(DecodeError::Truncated("v")));
        let mut big = Vec::new();
        put_varint(&mut big, 1 << 32);
        assert_eq!(Reader::new(&big).u32("v"), Err(DecodeError::Varint("v")));
    }

    #[test]
    fn counts_are_capped_before_allocating() {
        let mut out = Vec::new();
        put_varint(&mut out, u64::MAX);
        assert_eq!(Reader::new(&out).count(10, 1, "l"), Err(DecodeError::TooLong { what: "l", max: 10 }));
        let mut out = Vec::new();
        put_varint(&mut out, 5);
        out.extend_from_slice(&[0; 9]);
        assert_eq!(Reader::new(&out).count(10, 2, "l"), Err(DecodeError::Truncated("l")), "5 × 2 > 9 bytes");
        assert_eq!(Reader::new(&out).count(10, 1, "l"), Ok(5));
    }

    #[test]
    fn strings_are_checked() {
        let mut out = Vec::new();
        put_str(&mut out, "héllo");
        assert_eq!(Reader::new(&out).str(16, "s").as_deref(), Ok("héllo"));
        assert_eq!(Reader::new(&out).str(5, "s"), Err(DecodeError::TooLong { what: "s", max: 5 }));
        assert_eq!(Reader::new(&[2, 0xc3, 0x28]).str(5, "s"), Err(DecodeError::Utf8("s")));
        assert_eq!(Reader::new(&[2, 0]).bool("b"), Err(DecodeError::Invalid("b")));
    }

    #[test]
    fn frames_split_and_refuse_bad_lengths() {
        let mut out = Vec::new();
        Frame::write(9, 0, b"", &mut out);
        Frame::write(4, FLAG_COMPRESSED, b"abc", &mut out);
        let (f, n) = Frame::split(&out).unwrap().unwrap();
        assert_eq!((f.kind, f.flags, f.payload, n), (9, 0, &b""[..], 6));
        let (f, m) = Frame::split(&out[n..]).unwrap().unwrap();
        assert_eq!((f.kind, f.flags, f.payload, m), (4, 1, &b"abc"[..], 9));
        assert_eq!(Frame::split(&out[n..n + 8]), Ok(None), "incomplete");
        assert_eq!(Frame::split(&out[..3]), Ok(None));
        assert_eq!(Frame::split(&(MAX_FRAME + 1).to_be_bytes()), Err(DecodeError::FrameTooLarge(MAX_FRAME + 1)));
        assert_eq!(Frame::split(&[0, 0, 0, 1, 9]), Err(DecodeError::FrameTooShort));
        assert_eq!(Frame::read_len(&MAX_FRAME.to_be_bytes()), Ok(MAX_FRAME));
    }

    #[test]
    fn magics() {
        for m in [Magic::Session, Magic::Relay, Magic::Punch] {
            assert_eq!(Magic::parse(&m.bytes()), Some(m));
        }
        assert_eq!(&Magic::Session.bytes(), b"SWSYNC1\n");
        assert_eq!(Magic::parse(b"GET / HT"), None);
    }
}
