//! The actor's side of play while downloading (SPEC 7F): opening a stream, applying the hints the
//! streams park, and the temporary bandwidth weight of a torrent somebody is watching.

use std::sync::atomic::Ordering;
use std::time::Instant;

use tokio::sync::oneshot;
use trs_stream::FileRange;

use super::Torrent;
use crate::api::Priority;
use crate::stream_source::{OpenError, StreamBus, StreamParts, TorrentSource};

impl Torrent {
    /// Priority used for bandwidth weights: High while a stream has live hints (SPEC 7F.2: a
    /// torrent being watched wins against background torrents), else the user's own.
    pub(super) fn effective_priority(&self) -> Priority {
        if self.stream_boost { Priority::High } else { self.priority }
    }

    /// Re-publish the effective priority after either input changed.
    pub(super) fn publish_priority(&self) {
        self.ctx.priority.store(self.effective_priority().to_u8(), Ordering::Relaxed);
    }

    pub(super) fn on_stream_open(&mut self, file: usize, reply: oneshot::Sender<Result<StreamParts, OpenError>>) {
        let _ = reply.send(self.open_stream_parts(file));
    }

    fn open_stream_parts(&mut self, file: usize) -> Result<StreamParts, OpenError> {
        // Still loading or checking: the session retries.
        let Some(m) = self.meta.as_ref() else {
            return Err(if self.meta_fetch.is_some() { OpenError::NoMetadata } else { OpenError::NotReady });
        };
        if self.ctx.disk.get().is_none() {
            return Err(OpenError::NotReady);
        }
        if file >= m.layout.file_count() || m.layout.file_is_padding(file) || m.layout.file_len(file) == 0 {
            return Err(OpenError::BadFile(format!("the torrent has no streamable file {file}")));
        }
        if m.picker.is_none() {
            return Err(OpenError::Complete);
        }
        let name = match m.layout.file_path(file) {
            trs_storage::PathRef::Relative(p) | trs_storage::PathRef::Absolute(p) => {
                let s = p.to_string_lossy();
                s.rsplit('/').next().unwrap_or(&s).to_owned()
            }
        };
        let range = FileRange { index: file, name, offset: m.layout.file_start(file), len: m.layout.file_len(file) };
        let count = m.piece_count;
        let tx = self.tx.clone();
        let bus = self.stream.get_or_insert_with(|| StreamBus::new(count, tx)).clone();
        Ok(StreamParts { source: std::sync::Arc::new(TorrentSource::new(self.ctx.clone(), bus)), file: range })
    }

    /// Streams parked new pictures: hand them to the picker and put requests out for the urgent pieces.
    pub(super) fn apply_stream_hints(&mut self) {
        let Some(bus) = self.stream.clone() else { return };
        let pending = bus.take_pending();
        let Some(pk) = self.meta.as_mut().and_then(|m| m.picker.as_mut()) else { return };
        for (id, h) in pending {
            match h.ranges {
                Some(r) => pk.set_stream_hints(id, &r, h.at),
                None => pk.clear_stream_hints(id),
            }
        }
        let live = pk.has_hints();
        if live != self.stream_boost {
            self.stream_boost = live;
            self.publish_priority();
            self.refresh_slots(Instant::now());
        }
        if live {
            self.last_demand = Instant::now();
            self.refill_all();
        }
    }
}
