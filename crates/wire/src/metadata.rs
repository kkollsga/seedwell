//! BEP 9 `ut_metadata`: message codec and verified metadata assembly.

use crate::bencode::{self, put_kv_int};
use crate::bitfield::Bitfield;
use crate::error::{Error, Result};
use bytes::BufMut;
use sha1::{Digest, Sha1};

/// Metadata is exchanged in 16 KiB pieces.
pub const METADATA_PIECE_LEN: usize = 16 * 1024;
/// Hard cap on advertised metadata size (16 MiB); larger values are refused.
pub const MAX_METADATA_SIZE: u32 = 16 * 1024 * 1024;

/// A `ut_metadata` message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetadataMessage<'a> {
    /// `msg_type` 0: ask for a piece.
    Request {
        /// Piece index.
        piece: u32,
    },
    /// `msg_type` 1: a piece, with the total size and the raw data after the dict.
    Data {
        /// Piece index.
        piece: u32,
        /// Total metadata size announced by the sender.
        total_size: u32,
        /// Up to 16 KiB of raw metadata.
        data: &'a [u8],
    },
    /// `msg_type` 2: the sender will not serve this piece.
    Reject {
        /// Piece index.
        piece: u32,
    },
    /// A `msg_type` this version does not know; BEP 9 says to ignore these.
    Unknown {
        /// The value of `msg_type`.
        msg_type: i64,
    },
}

impl<'a> MetadataMessage<'a> {
    /// Parse the payload of an extended message addressed to `ut_metadata`.
    pub fn parse(payload: &'a [u8]) -> Result<Self> {
        let (d, rest) = bencode::dict_prefix(payload)?;
        let msg_type = d
            .get(b"msg_type")
            .and_then(|v| v.as_int())
            .ok_or(Error::MissingField("msg_type"))?;
        let piece = || -> Result<u32> {
            d.get(b"piece")
                .ok_or(Error::MissingField("piece"))?
                .as_u32()
                .ok_or(Error::InvalidField("piece"))
        };
        match msg_type {
            0 => Ok(Self::Request { piece: piece()? }),
            1 => {
                let total_size = d
                    .get(b"total_size")
                    .ok_or(Error::MissingField("total_size"))?
                    .as_u32()
                    .ok_or(Error::InvalidField("total_size"))?;
                if rest.len() > METADATA_PIECE_LEN {
                    return Err(Error::MetadataPieceLength {
                        piece: piece()?,
                        got: rest.len(),
                    });
                }
                Ok(Self::Data {
                    piece: piece()?,
                    total_size,
                    data: rest,
                })
            }
            2 => Ok(Self::Reject { piece: piece()? }),
            other => Ok(Self::Unknown { msg_type: other }),
        }
    }

    /// Write the payload (dictionary, then raw data for `Data`).
    pub fn encode_payload<B: BufMut>(&self, out: &mut B) {
        out.put_u8(b'd');
        match self {
            Self::Request { piece } => {
                put_kv_int(out, "msg_type", 0);
                put_kv_int(out, "piece", i64::from(*piece));
            }
            Self::Data {
                piece,
                total_size,
                data,
            } => {
                put_kv_int(out, "msg_type", 1);
                put_kv_int(out, "piece", i64::from(*piece));
                put_kv_int(out, "total_size", i64::from(*total_size));
                out.put_u8(b'e');
                out.put_slice(data);
                return;
            }
            Self::Reject { piece } => {
                put_kv_int(out, "msg_type", 2);
                put_kv_int(out, "piece", i64::from(*piece));
            }
            Self::Unknown { msg_type } => put_kv_int(out, "msg_type", *msg_type),
        }
        out.put_u8(b'e');
    }
}

/// Number of 16 KiB pieces for `total_size` bytes of metadata.
pub fn piece_count(total_size: u32) -> u32 {
    (total_size as usize).div_ceil(METADATA_PIECE_LEN) as u32
}

/// The slice of `metadata` that makes up `piece`, for serving requests.
pub fn piece_slice(metadata: &[u8], piece: u32) -> Option<&[u8]> {
    let start = (piece as usize).checked_mul(METADATA_PIECE_LEN)?;
    if start >= metadata.len() {
        return None;
    }
    metadata.get(start..(start + METADATA_PIECE_LEN).min(metadata.len()))
}

/// Collects metadata pieces from peers and verifies the SHA-1 against the info hash.
///
/// Memory: the output buffer is zero-allocated (lazily committed by the OS) at the
/// advertised size, which is capped at [`MAX_METADATA_SIZE`]. An attacker claiming
/// 16 MiB costs address space but no resident memory until pieces actually arrive.
/// The engine should still prefer a size reported by several peers.
// size: 64 bytes (+ metadata_size heap + ceil(pieces / 8) bitmap)
#[derive(Debug)]
pub struct MetadataAssembler {
    info_hash: [u8; 20],
    buf: Box<[u8]>,
    have: Bitfield,
}

impl MetadataAssembler {
    /// Start assembling `total_size` bytes that must hash to `info_hash`.
    pub fn new(info_hash: [u8; 20], total_size: u32) -> Result<Self> {
        if total_size == 0 || total_size > MAX_METADATA_SIZE {
            return Err(Error::MetadataSize(total_size));
        }
        Ok(Self {
            info_hash,
            buf: vec![0u8; total_size as usize].into_boxed_slice(),
            have: Bitfield::new(piece_count(total_size)),
        })
    }

    /// Total metadata size.
    pub fn total_size(&self) -> u32 {
        self.buf.len() as u32
    }

    /// Number of pieces.
    pub fn num_pieces(&self) -> u32 {
        self.have.len()
    }

    /// Pieces received so far.
    pub fn received(&self) -> u32 {
        self.have.count_ones()
    }

    /// All pieces present (call [`MetadataAssembler::finish`] to verify).
    pub fn is_complete(&self) -> bool {
        self.have.is_complete()
    }

    /// Whether `piece` has arrived.
    pub fn has_piece(&self, piece: u32) -> bool {
        self.have.has(piece)
    }

    /// Indices still missing, ascending.
    pub fn missing(&self) -> impl Iterator<Item = u32> + '_ {
        self.have.iter_zeros()
    }

    /// Expected length of `piece`: 16 KiB, except the last.
    pub fn piece_len(&self, piece: u32) -> Option<usize> {
        if piece >= self.num_pieces() {
            return None;
        }
        let start = piece as usize * METADATA_PIECE_LEN;
        Some((self.buf.len() - start).min(METADATA_PIECE_LEN))
    }

    /// Store a received piece. Returns `Ok(true)` if it was new, `Ok(false)` for a duplicate.
    ///
    /// Wrong index or wrong length is an error and nothing is stored.
    pub fn insert(&mut self, piece: u32, data: &[u8]) -> Result<bool> {
        let want = self
            .piece_len(piece)
            .ok_or(Error::MetadataPieceIndex(piece))?;
        if data.len() != want {
            return Err(Error::MetadataPieceLength {
                piece,
                got: data.len(),
            });
        }
        if self.have.has(piece) {
            return Ok(false);
        }
        let start = piece as usize * METADATA_PIECE_LEN;
        let dst = self
            .buf
            .get_mut(start..start + want)
            .ok_or(Error::MetadataPieceIndex(piece))?;
        dst.copy_from_slice(data);
        self.have.set(piece);
        Ok(true)
    }

    /// Verify and take the metadata.
    ///
    /// On hash mismatch (one or more corrupt pieces; the culprit cannot be known) the
    /// state is reset so every piece is requested again, and
    /// [`Error::MetadataHashMismatch`] is returned.
    pub fn finish(&mut self) -> Result<Box<[u8]>> {
        if !self.have.is_complete() {
            return Err(Error::MetadataIncomplete);
        }
        let digest: [u8; 20] = Sha1::digest(&self.buf).into();
        if digest != self.info_hash {
            self.have.clear_all();
            self.buf.fill(0);
            return Err(Error::MetadataHashMismatch);
        }
        Ok(core::mem::take(&mut self.buf))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_vector() {
        let mut v = Vec::new();
        MetadataMessage::Request { piece: 3 }.encode_payload(&mut v);
        assert_eq!(v, b"d8:msg_typei0e5:piecei3ee");
        assert_eq!(
            MetadataMessage::parse(&v).unwrap(),
            MetadataMessage::Request { piece: 3 }
        );
    }

    #[test]
    fn reject_vector() {
        let mut v = Vec::new();
        MetadataMessage::Reject { piece: 0 }.encode_payload(&mut v);
        assert_eq!(v, b"d8:msg_typei2e5:piecei0ee");
        assert_eq!(
            MetadataMessage::parse(&v).unwrap(),
            MetadataMessage::Reject { piece: 0 }
        );
    }

    #[test]
    fn data_vector_matches_bep9_layout() {
        let mut v = Vec::new();
        MetadataMessage::Data {
            piece: 0,
            total_size: 34256,
            data: b"RAW",
        }
        .encode_payload(&mut v);
        assert_eq!(v, b"d8:msg_typei1e5:piecei0e10:total_sizei34256eeRAW");
        assert_eq!(
            MetadataMessage::parse(&v).unwrap(),
            MetadataMessage::Data {
                piece: 0,
                total_size: 34256,
                data: b"RAW"
            }
        );
    }

    #[test]
    fn parse_errors_and_unknown() {
        assert!(MetadataMessage::parse(b"d5:piecei0ee").is_err());
        assert!(MetadataMessage::parse(b"d8:msg_typei0ee").is_err());
        assert!(MetadataMessage::parse(b"d8:msg_typei0e5:piecei-1ee").is_err());
        assert!(MetadataMessage::parse(b"d8:msg_typei1e5:piecei0ee").is_err());
        assert!(MetadataMessage::parse(b"junk").is_err());
        assert_eq!(
            MetadataMessage::parse(b"d8:msg_typei9ee").unwrap(),
            MetadataMessage::Unknown { msg_type: 9 }
        );
        // Oversized data trailer.
        let mut v = b"d8:msg_typei1e5:piecei0e10:total_sizei100ee".to_vec();
        v.extend(vec![0u8; METADATA_PIECE_LEN + 1]);
        assert!(matches!(
            MetadataMessage::parse(&v),
            Err(Error::MetadataPieceLength { .. })
        ));
    }

    fn make(len: usize) -> (Vec<u8>, [u8; 20]) {
        let data: Vec<u8> = (0..len).map(|i| (i * 31 + 7) as u8).collect();
        let h: [u8; 20] = Sha1::digest(&data).into();
        (data, h)
    }

    #[test]
    fn assemble_and_verify() {
        let (data, hash) = make(40_000);
        let mut a = MetadataAssembler::new(hash, 40_000).unwrap();
        assert_eq!(a.num_pieces(), 3);
        assert_eq!(a.piece_len(2), Some(40_000 - 2 * 16384));
        assert_eq!(a.piece_len(3), None);
        assert_eq!(a.finish().unwrap_err(), Error::MetadataIncomplete);
        // Out of order, with a duplicate.
        for (p, expect_new) in [(2u32, true), (0, true), (0, false), (1, true)] {
            let s = piece_slice(&data, p).unwrap();
            assert_eq!(a.insert(p, s).unwrap(), expect_new);
        }
        assert!(a.is_complete());
        assert_eq!(&*a.finish().unwrap(), &data[..]);
    }

    #[test]
    fn corrupt_piece_detected_and_reset() {
        let (data, hash) = make(20_000);
        let mut a = MetadataAssembler::new(hash, 20_000).unwrap();
        a.insert(0, piece_slice(&data, 0).unwrap()).unwrap();
        let mut bad = piece_slice(&data, 1).unwrap().to_vec();
        bad[10] ^= 0xFF;
        a.insert(1, &bad).unwrap();
        assert_eq!(a.finish().unwrap_err(), Error::MetadataHashMismatch);
        // State was reset: everything is missing again, and a clean retry succeeds.
        assert_eq!(a.missing().collect::<Vec<_>>(), vec![0, 1]);
        for p in 0..2 {
            a.insert(p, piece_slice(&data, p).unwrap()).unwrap();
        }
        assert_eq!(&*a.finish().unwrap(), &data[..]);
    }

    #[test]
    fn bad_sizes_and_pieces() {
        let h = [0; 20];
        assert!(matches!(
            MetadataAssembler::new(h, 0),
            Err(Error::MetadataSize(0))
        ));
        assert!(MetadataAssembler::new(h, MAX_METADATA_SIZE).is_ok());
        assert!(matches!(
            MetadataAssembler::new(h, MAX_METADATA_SIZE + 1),
            Err(Error::MetadataSize(_))
        ));
        assert!(MetadataAssembler::new(h, u32::MAX).is_err());
        let mut a = MetadataAssembler::new(h, 20_000).unwrap();
        assert_eq!(a.insert(2, &[0; 10]), Err(Error::MetadataPieceIndex(2)));
        assert!(matches!(
            a.insert(0, &[0; 100]),
            Err(Error::MetadataPieceLength { .. })
        ));
        assert!(matches!(
            a.insert(1, &[0; 16384]),
            Err(Error::MetadataPieceLength { .. })
        ));
        assert_eq!(a.received(), 0);
    }

    #[test]
    fn exact_multiple_has_full_last_piece() {
        let h = [0; 20];
        let a = MetadataAssembler::new(h, 32768).unwrap();
        assert_eq!(a.num_pieces(), 2);
        assert_eq!(a.piece_len(1), Some(16384));
        assert_eq!(piece_slice(&[0u8; 32768], 2), None);
    }
}
