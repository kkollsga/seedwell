//! The public task handle and the driver that turns pause / resume / cancel
//! commands into session runs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

use crate::core::{Core, Hub, Inner};
use crate::error::Error;
use crate::gate::{RateGate, TokenBucket};
use crate::net::{Net, NetConfig, parse_url};
use crate::session::{self, SessionEnd, Stop};
use crate::types::{DownloadRequest, DownloaderConfig, Event, ProbeInfo, Progress, StagedInfo, State};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Cmd {
    Run,
    Pause,
    Cancel { keep_part: bool },
}

/// `epoch` makes "resume" observable even when the command value repeats
/// (retrying a `Failed` task).
#[derive(Clone, Copy, Debug)]
struct Control {
    cmd: Cmd,
    epoch: u64,
}

/// Shared HTTP client + injected rate gate. Cheap to clone.
#[derive(Clone)]
pub struct Downloader {
    inner: Arc<Inner>,
}

impl Downloader {
    /// `gate` is the engine's global download limiter: every few KiB this
    /// downloader accepts is first granted by it.
    pub fn new(cfg: DownloaderConfig, gate: Arc<dyn RateGate>) -> Self {
        let net = Net::new(NetConfig {
            version: cfg.version.clone(),
            max_connections_per_host: cfg.max_connections_per_host,
            connect_timeout: cfg.connect_timeout,
            read_timeout: cfg.read_timeout,
            max_redirects: cfg.max_redirects,
        });
        Downloader { inner: Arc::new(Inner { net, gate, cfg }) }
    }

    /// No global limit (per-download limits still apply).
    pub fn unlimited(cfg: DownloaderConfig) -> Self {
        Self::new(cfg, Arc::new(crate::gate::Unlimited))
    }

    /// The exact User-Agent sent, `Seedwell/<version>`.
    pub fn user_agent(&self) -> &str {
        self.inner.net.user_agent()
    }

    /// Start (or resume, if staged state for `req.id` exists) a download on
    /// the current tokio runtime.
    pub fn start(&self, req: DownloadRequest) -> Result<DownloadTask, Error> {
        parse_url(&req.url)?;
        let rt = tokio::runtime::Handle::try_current().map_err(|_| Error::Io("no tokio runtime".into()))?;
        let (ev_tx, ev_rx) = mpsc::channel(64);
        let paused = req.start_paused;
        let (hub, progress) = Hub::new(Progress::new(if paused { State::Paused } else { State::Connecting }), ev_tx);
        let limiter = TokenBucket::new(req.rate_limit.unwrap_or(0));
        let id = req.id.clone();
        let core = Arc::new(Core { inner: self.inner.clone(), req, hub, limiter });
        let (ctl, ctl_rx) = watch::channel(Control { cmd: if paused { Cmd::Pause } else { Cmd::Run }, epoch: 0 });
        let join = rt.spawn(drive(core.clone(), ctl_rx));
        Ok(DownloadTask { id, core, ctl, progress, events: Mutex::new(Some(ev_rx)), join })
    }

    /// Ask the server what `url` is (name, size, type, range support) with a
    /// one-byte range request, without downloading anything. Lets the app
    /// divert `.torrent` payloads (SPEC 7D) before creating a task.
    pub async fn probe(&self, url: &str) -> Result<ProbeInfo, Error> {
        parse_url(url)?;
        session::probe(&self.inner, url).await
    }
}

/// Resumable downloads found in `incomplete_dir` (engine start-up recovery).
pub fn list_staged(incomplete_dir: &std::path::Path) -> Vec<StagedInfo> {
    session::scan_sidecars(incomplete_dir)
        .into_iter()
        .filter_map(|(sc, path)| {
            let mut part = path.into_os_string().into_string().ok()?;
            part.truncate(part.len() - ".json".len());
            Some(StagedInfo {
                id: sc.id,
                url: sc.url,
                name: sc.name,
                total: sc.total,
                done: sc.ranges.total(),
                part_path: PathBuf::from(part),
            })
        })
        .collect()
}

/// Delete the staged `.part` and sidecar of `id` without starting a task.
pub fn discard_staged(incomplete_dir: &std::path::Path, id: &str) {
    session::remove_staged(incomplete_dir, id);
}

/// Handle to one running download. Dropping it aborts the download abruptly,
/// like a crash: progress up to the last checkpoint stays on disk. Use
/// [`pause`](Self::pause) first for a lossless stop.
pub struct DownloadTask {
    id: String,
    core: Arc<Core>,
    ctl: watch::Sender<Control>,
    progress: watch::Receiver<Progress>,
    events: Mutex<Option<mpsc::Receiver<Event>>>,
    join: JoinHandle<()>,
}

impl Drop for DownloadTask {
    fn drop(&mut self) {
        self.join.abort();
    }
}

impl DownloadTask {
    /// The id the task was created with.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Latest snapshot.
    pub fn progress(&self) -> Progress {
        self.progress.borrow().clone()
    }

    /// A receiver that wakes on every progress change (about 4 times a second).
    pub fn subscribe(&self) -> watch::Receiver<Progress> {
        self.progress.clone()
    }

    /// Take the event stream (once). Events are best effort and bounded.
    pub fn events(&self) -> Option<mpsc::Receiver<Event>> {
        match self.events.lock() {
            Ok(mut g) => g.take(),
            Err(p) => p.into_inner().take(),
        }
    }

    fn send(&self, cmd: Cmd) {
        self.ctl.send_modify(|c| {
            if !matches!(c.cmd, Cmd::Cancel { .. }) {
                c.cmd = cmd;
                c.epoch += 1;
            }
        });
    }

    /// Stop transferring, flush and checkpoint. Progress survives.
    pub fn pause(&self) {
        self.send(Cmd::Pause);
    }

    /// Continue after `pause()`, or retry after `Failed`.
    pub fn resume(&self) {
        // Reflect the transition immediately so `wait()` right after
        // `resume()` does not see the stale Paused/Failed state.
        self.core.hub.update(|p| {
            if matches!(p.state, State::Paused | State::Failed) {
                p.state = State::Connecting;
                p.error = None;
            }
        });
        self.send(Cmd::Run);
    }

    /// Stop for good. The `.part` and sidecar are deleted unless `keep_part`.
    pub fn cancel(&self, keep_part: bool) {
        self.send(Cmd::Cancel { keep_part });
    }

    /// Change the per-download limit in bytes/s (`None` = unlimited).
    pub fn set_rate_limit(&self, bytes_per_sec: Option<u64>) {
        self.core.limiter.set_rate(bytes_per_sec.unwrap_or(0));
    }

    /// Wait for `Completed` (returns the final path), `Failed` or `Cancelled`.
    pub async fn wait(&self) -> Result<PathBuf, Error> {
        let mut rx = self.progress.clone();
        loop {
            {
                let p = rx.borrow_and_update();
                match p.state {
                    State::Completed => return Ok(p.final_path.clone().unwrap_or_default()),
                    State::Failed => return Err(p.error.clone().unwrap_or(Error::Cancelled)),
                    State::Cancelled => return Err(Error::Cancelled),
                    _ => {}
                }
            }
            if rx.changed().await.is_err() {
                return Err(Error::Cancelled);
            }
        }
    }

    /// Wait until `state` is reached (or the task ends in another terminal state).
    pub async fn wait_for_state(&self, state: State) -> Progress {
        let mut rx = self.progress.clone();
        loop {
            {
                let p = rx.borrow_and_update();
                if p.state == state || matches!(p.state, State::Completed | State::Cancelled) {
                    return p.clone();
                }
            }
            if rx.changed().await.is_err() {
                return rx.borrow().clone();
            }
        }
    }
}

async fn drive(core: Arc<Core>, mut ctl: watch::Receiver<Control>) {
    loop {
        let c = *ctl.borrow_and_update();
        match c.cmd {
            Cmd::Cancel { keep_part } => {
                session::discard(&core, keep_part).await;
                core.hub.set_state(State::Cancelled);
                return;
            }
            Cmd::Pause => {
                core.hub.update(|p| {
                    p.rate = 0;
                    p.connections = 0;
                });
                core.hub.set_state(State::Paused);
                if ctl.changed().await.is_err() {
                    return;
                }
                continue;
            }
            Cmd::Run => {}
        }
        core.hub.set_state(State::Connecting);
        let (stop_tx, stop_rx) = watch::channel(false);
        let fut = session::run(core.clone(), Stop(stop_rx));
        tokio::pin!(fut);
        let end = loop {
            tokio::select! {
                r = &mut fut => break r,
                changed = ctl.changed() => {
                    if changed.is_err() {
                        let _ = stop_tx.send(true);
                        break (&mut fut).await;
                    }
                    if ctl.borrow_and_update().cmd != Cmd::Run {
                        // Let the session flush and checkpoint, then loop to
                        // act on the new command.
                        let _ = stop_tx.send(true);
                        break (&mut fut).await;
                    }
                }
            }
        };
        match end {
            SessionEnd::Completed(path) => {
                core.hub.set_state(State::Completed);
                core.hub.event(Event::Completed(path));
                return;
            }
            SessionEnd::Stopped => {}
            SessionEnd::Failed(e) => {
                core.hub.update(|p| {
                    p.error = Some(e.clone());
                    p.rate = 0;
                    p.connections = 0;
                });
                core.hub.set_state(State::Failed);
                core.hub.event(Event::Failed(e));
                if ctl.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}
