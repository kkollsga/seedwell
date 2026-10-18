//! "The index first" (SPEC 7F.1): once per stream, fetch the head and tail, find `moov` /
//! `Cues`, read the duration and keep those ranges urgent until they are present.

use crate::conn::{Wait, wait_piece};
use crate::container::{Fetched, Probe, probe_for};
use crate::stream::Stream;
use std::sync::Arc;

const MAX_STEPS: usize = 64;
const READ_CHUNK: usize = 256 * 1024;

pub(crate) async fn run(stream: Arc<Stream>) {
    let mut rx = stream.source.subscribe();
    let mut cancel = stream.cancel.subscribe();
    if let Some(probe) = probe_for(&stream.file.name) {
        let mut fetched = Fetched::new();
        for _ in 0..MAX_STEPS {
            match probe(stream.file.len, &fetched) {
                Probe::Done(info) => {
                    if let Some(d) = info.duration {
                        stream.set_duration(d);
                    }
                    for r in info.index {
                        stream.add_index(r);
                    }
                    break;
                }
                Probe::Need { offset, len } => {
                    if len == 0 {
                        break;
                    }
                    stream.add_index(offset..offset + len as u64);
                    let Some(bytes) = fetch(&stream, &mut rx, &mut cancel, offset, len).await else { break };
                    fetched.add(offset, bytes);
                }
            }
        }
    }
    // Keep the index urgent until every piece of it is present, then let go.
    loop {
        let ranges: Vec<_> = {
            let ph = stream.index_ranges();
            ph.into_iter().filter(|r| !stream.range_present(r)).collect()
        };
        if ranges.is_empty() || stream.is_closed() {
            break;
        }
        let first = ranges[0].clone();
        let p = stream.geom.pieces_of(stream.file.offset + first.start..stream.file.offset + first.end);
        let mut missing = None;
        for q in p {
            if !stream.source.has(q) {
                missing = Some(q);
                break;
            }
        }
        let Some(q) = missing else { continue };
        // No stall limit here: this only keeps hints alive, it never blocks a response.
        match wait_piece(&*stream.source, &mut rx, q, std::time::Duration::from_secs(86_400), &mut cancel, None).await {
            Wait::Ready | Wait::Stalled => {}
            _ => return,
        }
    }
    stream.clear_index();
}

/// Read file bytes `offset..offset+len` once their pieces are verified.
async fn fetch(
    stream: &Arc<Stream>,
    rx: &mut tokio::sync::watch::Receiver<u64>,
    cancel: &mut tokio::sync::watch::Receiver<bool>,
    offset: u64,
    len: usize,
) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    let mut pos = offset;
    let end = offset + len as u64;
    while pos < end {
        let abs = stream.file.offset + pos;
        let piece = stream.geom.piece_of(abs);
        match wait_piece(&*stream.source, rx, piece, stream.cfg.stall_timeout, cancel, None).await {
            Wait::Ready => {}
            _ => return None,
        }
        let mut n = (READ_CHUNK as u64).min(end - pos);
        let last = stream.geom.piece_of(abs + n - 1);
        for q in piece + 1..=last {
            if !stream.source.has(q) {
                n = stream.geom.piece_start(q) - abs;
                break;
            }
        }
        let buf = stream.source.read(abs, n as usize, Vec::with_capacity(n as usize)).await.ok()?;
        if buf.len() != n as usize {
            return None;
        }
        out.extend_from_slice(&buf);
        pos += n;
    }
    Some(out)
}
