//! State shared between a task handle, its driver and its workers.

use std::sync::Arc;

use tokio::sync::{mpsc, watch};

use crate::gate::{RateGate, TokenBucket};
use crate::net::Net;
use crate::types::{DownloadRequest, DownloaderConfig, Event, Progress, State};

/// What a [`Downloader`](crate::Downloader) owns: the HTTP client with its
/// per-host keep-alive pool and connection caps, the injected rate gate.
pub(crate) struct Inner {
    pub net: Net,
    pub gate: Arc<dyn RateGate>,
    pub cfg: DownloaderConfig,
}

/// Publishes progress snapshots and discrete events.
pub(crate) struct Hub {
    tx: watch::Sender<Progress>,
    ev: mpsc::Sender<Event>,
}

impl Hub {
    pub fn new(initial: Progress, ev: mpsc::Sender<Event>) -> (Self, watch::Receiver<Progress>) {
        let (tx, rx) = watch::channel(initial);
        (Hub { tx, ev }, rx)
    }

    pub fn update(&self, f: impl FnOnce(&mut Progress)) {
        self.tx.send_modify(f);
    }

    /// Best effort: a full channel means nobody is listening closely.
    pub fn event(&self, e: Event) {
        let _ = self.ev.try_send(e);
    }

    pub fn set_state(&self, s: State) {
        let mut changed = false;
        self.tx.send_if_modified(|p| {
            changed = p.state != s;
            p.state = s;
            changed
        });
        if changed {
            self.event(Event::StateChanged(s));
        }
    }
}

/// One task's shared core.
pub(crate) struct Core {
    pub inner: Arc<Inner>,
    pub req: DownloadRequest,
    pub hub: Hub,
    /// Per-download limiter (rate 0 = off), applied before the global gate.
    pub limiter: TokenBucket,
}
