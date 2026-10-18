//! Message model and encoding for BEP 3, BEP 6 and the BEP 10 envelope.

use bytes::BufMut;

/// Standard block size requested from peers (16 KiB).
pub const BLOCK_SIZE: u32 = 16 * 1024;

/// Wire message ids.
pub mod id {
    /// choke
    pub const CHOKE: u8 = 0;
    /// unchoke
    pub const UNCHOKE: u8 = 1;
    /// interested
    pub const INTERESTED: u8 = 2;
    /// not interested
    pub const NOT_INTERESTED: u8 = 3;
    /// have
    pub const HAVE: u8 = 4;
    /// bitfield
    pub const BITFIELD: u8 = 5;
    /// request
    pub const REQUEST: u8 = 6;
    /// piece
    pub const PIECE: u8 = 7;
    /// cancel
    pub const CANCEL: u8 = 8;
    /// port (BEP 5)
    pub const PORT: u8 = 9;
    /// suggest piece (BEP 6)
    pub const SUGGEST: u8 = 13;
    /// have all (BEP 6)
    pub const HAVE_ALL: u8 = 14;
    /// have none (BEP 6)
    pub const HAVE_NONE: u8 = 15;
    /// reject request (BEP 6)
    pub const REJECT: u8 = 16;
    /// allowed fast (BEP 6)
    pub const ALLOWED_FAST: u8 = 17;
    /// extended (BEP 10)
    pub const EXTENDED: u8 = 20;
}

/// A block address: piece index, byte offset in the piece, length.
// size: 12 bytes
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockRequest {
    /// Piece index.
    pub index: u32,
    /// Byte offset inside the piece.
    pub begin: u32,
    /// Block length in bytes.
    pub length: u32,
}

/// A peer wire message. `D` is the payload container: `&[u8]` for borrowed views
/// ([`MessageRef`]) or `bytes::Bytes` for owned zero-copy slices ([`OwnedMessage`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Message<D> {
    /// Zero-length frame.
    KeepAlive,
    /// Choke.
    Choke,
    /// Unchoke.
    Unchoke,
    /// Interested.
    Interested,
    /// Not interested.
    NotInterested,
    /// Have piece.
    Have(u32),
    /// Raw bitfield bytes (validate with [`crate::Bitfield::from_wire`]).
    Bitfield(D),
    /// Request a block.
    Request(BlockRequest),
    /// A block of data.
    Piece {
        /// Piece index.
        index: u32,
        /// Offset in the piece.
        begin: u32,
        /// Block payload.
        data: D,
    },
    /// Cancel a request.
    Cancel(BlockRequest),
    /// DHT listen port (BEP 5).
    Port(u16),
    /// Suggest a piece (BEP 6).
    Suggest(u32),
    /// Sender has every piece (BEP 6).
    HaveAll,
    /// Sender has no piece (BEP 6).
    HaveNone,
    /// Request rejected (BEP 6).
    RejectRequest(BlockRequest),
    /// Piece may be requested while choked (BEP 6).
    AllowedFast(u32),
    /// Extension message (BEP 10): extension id and raw payload.
    Extended {
        /// Extension id (0 = extended handshake).
        id: u8,
        /// Bencoded payload (plus raw trailer for `ut_metadata` data).
        payload: D,
    },
    /// A well-formed frame with an id we do not know; its body was skipped.
    Unknown(u8),
}

/// Borrowed view into a receive buffer.
pub type MessageRef<'a> = Message<&'a [u8]>;
/// Zero-copy owned message sharing the receive buffer's allocation.
pub type OwnedMessage = Message<bytes::Bytes>;

impl<D> Message<D> {
    /// Transform the payload container.
    pub fn map<E>(self, f: impl FnOnce(D) -> E) -> Message<E> {
        use Message::*;
        match self {
            KeepAlive => KeepAlive,
            Choke => Choke,
            Unchoke => Unchoke,
            Interested => Interested,
            NotInterested => NotInterested,
            Have(i) => Have(i),
            Bitfield(d) => Bitfield(f(d)),
            Request(r) => Request(r),
            Piece { index, begin, data } => Piece {
                index,
                begin,
                data: f(data),
            },
            Cancel(r) => Cancel(r),
            Port(p) => Port(p),
            Suggest(i) => Suggest(i),
            HaveAll => HaveAll,
            HaveNone => HaveNone,
            RejectRequest(r) => RejectRequest(r),
            AllowedFast(i) => AllowedFast(i),
            Extended { id, payload } => Extended {
                id,
                payload: f(payload),
            },
            Unknown(i) => Unknown(i),
        }
    }
}

impl<D: AsRef<[u8]>> Message<D> {
    /// Total encoded size including the 4-byte length prefix.
    pub fn encoded_len(&self) -> usize {
        use Message::*;
        4 + match self {
            KeepAlive => 0,
            Choke | Unchoke | Interested | NotInterested | HaveAll | HaveNone => 1,
            Have(_) | Suggest(_) | AllowedFast(_) => 5,
            Bitfield(d) => 1 + d.as_ref().len(),
            Request(_) | Cancel(_) | RejectRequest(_) => 13,
            Piece { data, .. } => 9 + data.as_ref().len(),
            Port(_) => 3,
            Extended { payload, .. } => 2 + payload.as_ref().len(),
            Unknown(_) => 1,
        }
    }

    /// Append the framed message to `out`.
    ///
    /// For uploads prefer [`piece_header`] plus a vectored write so the block is
    /// never copied into an intermediate buffer.
    pub fn encode<B: BufMut>(&self, out: &mut B) {
        use Message::*;
        let frame_len = (self.encoded_len() - 4) as u32;
        out.put_u32(frame_len);
        match self {
            KeepAlive => {}
            Choke => out.put_u8(id::CHOKE),
            Unchoke => out.put_u8(id::UNCHOKE),
            Interested => out.put_u8(id::INTERESTED),
            NotInterested => out.put_u8(id::NOT_INTERESTED),
            HaveAll => out.put_u8(id::HAVE_ALL),
            HaveNone => out.put_u8(id::HAVE_NONE),
            Unknown(i) => out.put_u8(*i),
            Have(i) => {
                out.put_u8(id::HAVE);
                out.put_u32(*i);
            }
            Suggest(i) => {
                out.put_u8(id::SUGGEST);
                out.put_u32(*i);
            }
            AllowedFast(i) => {
                out.put_u8(id::ALLOWED_FAST);
                out.put_u32(*i);
            }
            Bitfield(d) => {
                out.put_u8(id::BITFIELD);
                out.put_slice(d.as_ref());
            }
            Request(r) => put_block(out, id::REQUEST, r),
            Cancel(r) => put_block(out, id::CANCEL, r),
            RejectRequest(r) => put_block(out, id::REJECT, r),
            Piece { index, begin, data } => {
                out.put_u8(id::PIECE);
                out.put_u32(*index);
                out.put_u32(*begin);
                out.put_slice(data.as_ref());
            }
            Port(p) => {
                out.put_u8(id::PORT);
                out.put_u16(*p);
            }
            Extended { id: eid, payload } => {
                out.put_u8(id::EXTENDED);
                out.put_u8(*eid);
                out.put_slice(payload.as_ref());
            }
        }
    }
}

fn put_block<B: BufMut>(out: &mut B, mid: u8, r: &BlockRequest) {
    out.put_u8(mid);
    out.put_u32(r.index);
    out.put_u32(r.begin);
    out.put_u32(r.length);
}

/// The 13-byte header of a `piece` message carrying `data_len` bytes, so the sender
/// can write header and block separately (`writev`) without copying the block.
pub fn piece_header(index: u32, begin: u32, data_len: u32) -> [u8; 13] {
    let mut h = [0u8; 13];
    h[..4].copy_from_slice(&(9u32.wrapping_add(data_len)).to_be_bytes());
    h[4] = id::PIECE;
    h[5..9].copy_from_slice(&index.to_be_bytes());
    h[9..13].copy_from_slice(&begin.to_be_bytes());
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc(m: &Message<&[u8]>) -> Vec<u8> {
        let mut v = Vec::new();
        m.encode(&mut v);
        assert_eq!(v.len(), m.encoded_len());
        v
    }

    #[test]
    fn byte_exact_vectors() {
        let r = BlockRequest {
            index: 1,
            begin: 0x4000,
            length: 0x4000,
        };
        let req_tail = [0, 0, 0, 1, 0, 0, 0x40, 0, 0, 0, 0x40, 0];
        let mut exp_req = vec![0, 0, 0, 13, 6];
        exp_req.extend_from_slice(&req_tail);
        assert_eq!(enc(&Message::Request(r)), exp_req);
        exp_req[4] = 8;
        assert_eq!(enc(&Message::Cancel(r)), exp_req);
        exp_req[4] = 16;
        assert_eq!(enc(&Message::RejectRequest(r)), exp_req);

        assert_eq!(enc(&Message::KeepAlive), [0, 0, 0, 0]);
        assert_eq!(enc(&Message::Choke), [0, 0, 0, 1, 0]);
        assert_eq!(enc(&Message::Unchoke), [0, 0, 0, 1, 1]);
        assert_eq!(enc(&Message::Interested), [0, 0, 0, 1, 2]);
        assert_eq!(enc(&Message::NotInterested), [0, 0, 0, 1, 3]);
        assert_eq!(
            enc(&Message::Have(0x0102_0304)),
            [0, 0, 0, 5, 4, 1, 2, 3, 4]
        );
        assert_eq!(
            enc(&Message::Bitfield(&[0xA0, 0x80][..])),
            [0, 0, 0, 3, 5, 0xA0, 0x80]
        );
        assert_eq!(
            enc(&Message::Piece {
                index: 2,
                begin: 3,
                data: &b"xyz"[..]
            }),
            [0, 0, 0, 12, 7, 0, 0, 0, 2, 0, 0, 0, 3, b'x', b'y', b'z']
        );
        assert_eq!(enc(&Message::Port(6881)), [0, 0, 0, 3, 9, 0x1A, 0xE1]);
        assert_eq!(enc(&Message::Suggest(7)), [0, 0, 0, 5, 13, 0, 0, 0, 7]);
        assert_eq!(enc(&Message::HaveAll), [0, 0, 0, 1, 14]);
        assert_eq!(enc(&Message::HaveNone), [0, 0, 0, 1, 15]);
        assert_eq!(enc(&Message::AllowedFast(9)), [0, 0, 0, 5, 17, 0, 0, 0, 9]);
        assert_eq!(
            enc(&Message::Extended {
                id: 0,
                payload: &b"de"[..]
            }),
            [0, 0, 0, 4, 20, 0, b'd', b'e']
        );
    }

    #[test]
    fn piece_header_matches_encode() {
        let mut v = Vec::new();
        Message::Piece {
            index: 5,
            begin: 16384,
            data: &[1u8, 2, 3][..],
        }
        .encode(&mut v);
        assert_eq!(&v[..13], &piece_header(5, 16384, 3));
    }
}
