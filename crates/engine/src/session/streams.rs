//! Play while downloading, session side (SPEC 7F, `crates/stream/ENGINE_INTEGRATION.md` 5 and 7).
//!
//! `stream_url` selects the file if it was skipped, wakes the torrent (resume, bypass the queue),
//! asks the torrent actor for a `PieceSource` and opens a stream on the shared loopback server.
//! Closing mirrors it: removing, stopping or moving a torrent closes its streams first, so
//! responses end before storage goes away.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use trs_stream::StreamServer;

use super::cold::Want;
use super::{Cmd, Session};
use crate::api::*;
use crate::stream_source::OpenError;
use crate::torrent::{TorrentCmd, TorrentMsg};

/// The owner id a torrent's streams carry on the server.
pub(crate) fn owner_of(id: &TorrentId) -> u64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&id[..8]);
    u64::from_le_bytes(b)
}

/// What the session answers a stream request.
pub(crate) enum Opened {
    /// A stream is open.
    Stream { handle: trs_stream::StreamHandle, selected: bool },
    /// Nothing left to download for this torrent: open the file itself.
    Complete { selected: bool },
}

/// Why a stream request failed.
pub(crate) enum OpenFailure {
    Engine(EngineError),
    /// The torrent is still starting; the caller retries.
    NotReady,
}

pub(crate) type OpenReply = oneshot::Sender<Result<Opened, OpenFailure>>;

impl Session {
    pub(super) fn on_stream_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::StreamOpen { id, file, server, reply } => self.stream_open(id, file, server, reply),
            Cmd::StreamsClosed => {
                let before = self.streaming.len();
                let srv = self.sh.streams.get().cloned();
                self.streaming.retain(|id| srv.as_ref().is_some_and(|s| s.has_owner(owner_of(id))));
                if self.streaming.len() != before {
                    self.replan();
                }
            }
            _ => {}
        }
    }

    /// Close every stream of a torrent and forget that it is being watched. Call before the
    /// torrent's actor stops or its files change.
    pub(crate) fn close_streams(&mut self, id: &TorrentId) {
        if let Some(s) = self.sh.streams.get() {
            s.close_owner(owner_of(id));
        }
        self.streaming.remove(id);
    }

    fn stream_open(&mut self, id: TorrentId, file: usize, server: Arc<StreamServer>, reply: OpenReply) {
        match self.stream_prepare(id, file) {
            Err(e) => {
                let _ = reply.send(Err(OpenFailure::Engine(e)));
            }
            Ok(Prepared::Complete { selected }) => {
                let _ = reply.send(Ok(Opened::Complete { selected }));
            }
            Ok(Prepared::NotReady) => {
                let _ = reply.send(Err(OpenFailure::NotReady));
            }
            Ok(Prepared::Actor { tx, selected }) => {
                tokio::spawn(async move {
                    let (rtx, rrx) = oneshot::channel();
                    let sent = tx.send(TorrentMsg::Cmd(TorrentCmd::StreamOpen { file, reply: rtx })).await.is_ok();
                    // An actor that is still loading drops the request: the caller asks again.
                    let answer = if sent { tokio::time::timeout(Duration::from_secs(5), rrx).await.ok().and_then(Result::ok) } else { None };
                    let out = match answer {
                        None | Some(Err(OpenError::NotReady)) => Err(OpenFailure::NotReady),
                        Some(Err(OpenError::NoMetadata)) => Err(OpenFailure::Engine(EngineError::NoMetadata)),
                        Some(Err(OpenError::BadFile(m))) => Err(OpenFailure::Engine(EngineError::Invalid(m))),
                        Some(Err(OpenError::Complete)) => Ok(Opened::Complete { selected }),
                        Some(Ok(parts)) => server
                            .open(parts.source, parts.file, owner_of(&id))
                            .map(|handle| Opened::Stream { handle, selected })
                            .map_err(|e| OpenFailure::Engine(EngineError::Io(e.to_string()))),
                    };
                    let _ = reply.send(out);
                });
            }
        }
    }

    fn stream_prepare(&mut self, id: TorrentId, file: usize) -> Result<Prepared, EngineError> {
        if self.shutting_down {
            return Err(EngineError::ShuttingDown);
        }
        let e = self.table.get(&id).ok_or(EngineError::NotFound)?;
        if e.cold.awaiting() {
            return Err(EngineError::Invalid("choose what to download first".into()));
        }
        if !e.cold.has_metadata() {
            return Err(EngineError::NoMetadata);
        }
        // Selecting a file for streaming selects it for download.
        let mut selected = false;
        if let Some(p) = e.cold.file_prios.as_deref()
            && p.get(file) == Some(&0) {
                let mut raw = p.to_vec();
                raw[file] = FilePriority::Normal.to_u8();
                self.set_file_priorities(id, raw.into_iter().map(FilePriority::from_u8).collect())?;
                self.apply_edge_policy(id);
                self.sh.emit(EngineEvent::FilesSelected { id, files: vec![file as u32] });
                selected = true;
            }
        let e = self.table.get(&id).ok_or(EngineError::NotFound)?;
        if e.cold.complete() {
            return Ok(Prepared::Complete { selected });
        }
        // Streaming is an explicit user action: it resumes a paused torrent and jumps the queue.
        self.streaming.insert(id);
        if self.table.get(&id).is_some_and(|e| e.cold.want != Want::Active) {
            self.user_resume(id);
        } else {
            self.replan();
            self.reconcile(id);
        }
        match self.table.get(&id).and_then(|e| e.live.as_ref()) {
            Some(l) if !l.stopping => Ok(Prepared::Actor { tx: l.tx.clone(), selected }),
            _ if self.table.get(&id).is_some_and(|e| e.cold.want != Want::Active) => {
                self.streaming.remove(&id);
                Err(EngineError::Invalid("the torrent cannot run right now".into()))
            }
            // The actor is being (re)started: the caller asks again in a moment.
            _ => Ok(Prepared::NotReady),
        }
    }
}

enum Prepared {
    Complete { selected: bool },
    Actor { tx: crate::chan::Sender<TorrentMsg>, selected: bool },
    NotReady,
}
