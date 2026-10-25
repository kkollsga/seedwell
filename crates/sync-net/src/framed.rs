//! Messages over a byte stream: `trs-sync-proto` frames, with deflate for the big metadata messages.

use std::io::{self, Read, Write};

use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use trs_sync_proto::codec::{FLAG_COMPRESSED, FRAME_HEADER, Frame, MAX_FRAME};
use trs_sync_proto::msg::Message;

/// Payloads at least this big are compressed when the message is metadata (index, writers, addresses).
/// File data is not: most of it is compressed already, and the CPU is better spent elsewhere.
const COMPRESS_FROM: usize = 1024;

/// A decompressed payload may not be bigger than a frame could carry uncompressed.
const MAX_INFLATED: u64 = MAX_FRAME as u64;

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn compressible(m: &Message) -> bool {
    matches!(m, Message::Index(_) | Message::Members(_) | Message::Addresses(_) | Message::Join(_))
}

/// Encodes `m` into one frame, compressing it when that pays.
pub fn encode(m: &Message, out: &mut Vec<u8>) {
    let mut payload = Vec::new();
    m.encode_payload(&mut payload);
    if compressible(m) && payload.len() >= COMPRESS_FROM {
        let mut z = DeflateEncoder::new(Vec::with_capacity(payload.len() / 3), Compression::fast());
        if z.write_all(&payload).is_ok()
            && let Ok(small) = z.finish()
            && small.len() < payload.len()
        {
            Frame::write(m.kind(), FLAG_COMPRESSED, &small, out);
            return;
        }
    }
    Frame::write(m.kind(), 0, &payload, out);
}

/// Decodes one frame's message (inflating it first when it is flagged compressed).
pub fn decode(kind: u8, flags: u8, payload: &[u8]) -> io::Result<Message> {
    if flags & FLAG_COMPRESSED != 0 {
        let mut out = Vec::new();
        DeflateDecoder::new(payload).take(MAX_INFLATED + 1).read_to_end(&mut out)?;
        if out.len() as u64 > MAX_INFLATED {
            return Err(bad("a compressed message inflates past the frame limit"));
        }
        Message::decode(Frame { kind, flags: 0, payload: &out }).map_err(bad)
    } else {
        Message::decode(Frame { kind, flags, payload }).map_err(bad)
    }
}

/// Reads messages from a stream.
pub struct Reader<R> {
    r: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> Reader<R> {
    /// Reads from `r`.
    pub fn new(r: R) -> Self {
        Reader { r, buf: Vec::new() }
    }

    /// The next message; `Ok(None)` when the peer closed the stream cleanly between frames.
    pub async fn next(&mut self) -> io::Result<Option<Message>> {
        let mut head = [0u8; FRAME_HEADER];
        match self.r.read_exact(&mut head[..4]).await {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e),
        }
        let len = Frame::read_len(&[head[0], head[1], head[2], head[3]]).map_err(bad)? as usize;
        self.r.read_exact(&mut head[4..]).await?;
        let body = len - 2;
        self.buf.clear();
        self.buf.resize(body, 0);
        self.r.read_exact(&mut self.buf).await?;
        let m = decode(head[4], head[5], &self.buf)?;
        // A big buffer from one large chunk is not kept around for an idle connection.
        if self.buf.capacity() > 1 << 20 {
            self.buf = Vec::new();
        }
        Ok(Some(m))
    }
}

/// Writes messages to a stream.
pub struct Writer<W> {
    w: W,
    buf: Vec<u8>,
}

impl<W: AsyncWrite + Unpin> Writer<W> {
    /// Writes to `w`.
    pub fn new(w: W) -> Self {
        Writer { w, buf: Vec::new() }
    }

    /// Sends one message (and flushes it).
    pub async fn send(&mut self, m: &Message) -> io::Result<()> {
        self.buf.clear();
        encode(m, &mut self.buf);
        self.w.write_all(&self.buf).await?;
        self.w.flush().await?;
        if self.buf.capacity() > 1 << 20 {
            self.buf = Vec::new();
        }
        Ok(())
    }

    /// Closes the write side.
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.w.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use trs_sync_proto::msg::{Close, Hello};

    #[tokio::test]
    async fn messages_roundtrip_over_a_stream() {
        let (a, b) = tokio::io::duplex(1 << 16);
        let mut w = Writer::new(a);
        let mut r = Reader::new(b);
        let hello = Message::Hello(Hello::new("Seedwell 0.1.6".into(), "Mac".into()));
        let close = Message::Close(Close { reason: "x".repeat(200) });
        w.send(&hello).await.unwrap();
        w.send(&close).await.unwrap();
        w.shutdown().await.unwrap();
        assert_eq!(r.next().await.unwrap(), Some(hello));
        assert_eq!(r.next().await.unwrap(), Some(close));
        assert_eq!(r.next().await.unwrap(), None);
    }

    #[test]
    fn a_compression_bomb_is_refused() {
        let mut z = DeflateEncoder::new(Vec::new(), Compression::best());
        z.write_all(&vec![0u8; (MAX_FRAME as usize) + 10]).unwrap();
        let small = z.finish().unwrap();
        assert!(decode(4, FLAG_COMPRESSED, &small).is_err());
    }
}
