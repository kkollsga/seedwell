//! The tokio driver: one UDP socket (gateway + SSDP), short-lived TCP exchanges for
//! UPnP, one timer. All decisions are made by [`PortMapper`]; this file only moves
//! bytes and watches the clock.

use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

use crate::core::{
    Config, Event, HttpFailure, HttpRequest, Input, Mapping, NetInfo, Output, PortMapper,
};
use crate::{Method, Millis, gateway, http};

/// Driver settings.
#[derive(Clone, Debug, Default)]
pub struct DriverConfig {
    /// Protocol machine settings.
    pub core: Config,
    /// Skip gateway detection and use this network (tests, or an engine that already
    /// knows the gateway).
    pub fixed_net: Option<NetInfo>,
}

/// What is mapped right now.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Snapshot {
    /// Method in use.
    pub method: Option<Method>,
    /// Gateway's external address.
    pub external_ip: Option<Ipv4Addr>,
    /// Held mappings.
    pub mappings: Vec<Mapping>,
}

enum Cmd {
    NetworkChanged,
    Retry,
    Shutdown(oneshot::Sender<()>),
}

/// Handle to the running driver. Cheap to clone.
#[derive(Clone)]
pub struct PortMapHandle {
    cmd: mpsc::Sender<Cmd>,
    snap: watch::Receiver<Snapshot>,
}

impl PortMapHandle {
    /// Tell the driver the network changed (interface up/down, wake from sleep, new
    /// Wi-Fi). It re-reads the gateway; if nothing changed it does nothing.
    pub async fn network_changed(&self) {
        let _ = self.cmd.send(Cmd::NetworkChanged).await;
    }

    /// User-initiated retry after every method failed.
    pub async fn retry(&self) {
        let _ = self.cmd.send(Cmd::Retry).await;
    }

    /// Current state.
    pub fn snapshot(&self) -> Snapshot {
        self.snap.borrow().clone()
    }

    /// A receiver that wakes on every change of the snapshot.
    pub fn watch(&self) -> watch::Receiver<Snapshot> {
        self.snap.clone()
    }

    /// Delete the mappings (bounded by `Config::shutdown_deadline`) and stop the task.
    pub async fn shutdown(&self) {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(Cmd::Shutdown(tx)).await.is_ok() {
            let _ = rx.await;
        }
    }
}

/// Start the driver on the current tokio runtime. Events (rare) arrive on the
/// returned receiver; a full channel drops the newest event, the snapshot stays
/// authoritative.
pub fn spawn(cfg: DriverConfig) -> (PortMapHandle, mpsc::Receiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::channel(16);
    let (ev_tx, ev_rx) = mpsc::channel(256);
    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    tokio::spawn(run(cfg, cmd_rx, ev_tx, snap_tx));
    (
        PortMapHandle {
            cmd: cmd_tx,
            snap: snap_rx,
        },
        ev_rx,
    )
}

struct State {
    core: PortMapper,
    start: Instant,
    sock: Option<UdpSocket>,
    bound_to: Option<Ipv4Addr>,
    http_tx: mpsc::UnboundedSender<(u64, Result<Vec<u8>, HttpFailure>)>,
    events: mpsc::Sender<Event>,
    snap: watch::Sender<Snapshot>,
}

impl State {
    fn now(&self) -> Millis {
        Millis(u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    async fn feed(&mut self, input: Input<'_>) {
        let now = self.now();
        let outputs = self.core.handle(input, now);
        for o in outputs {
            match o {
                Output::Udp { to, data } => {
                    if let Some(s) = &self.sock {
                        // Best effort: a lost datagram is handled by retransmission.
                        let _ = s.send_to(&data, to).await;
                    }
                }
                Output::Http(req) => {
                    let tx = self.http_tx.clone();
                    tokio::spawn(async move {
                        let id = req.id;
                        let _ = tx.send((id, exchange(req).await));
                    });
                }
                Output::Event(e) => {
                    let _ = self.events.try_send(e);
                }
            }
        }
        let snapshot = Snapshot {
            method: self.core.method(),
            external_ip: self.core.external_ip(),
            mappings: self.core.mappings(),
        };
        self.snap.send_if_modified(|s| {
            let changed = *s != snapshot;
            if changed {
                *s = snapshot;
            }
            changed
        });
    }

    async fn network(&mut self, fixed: Option<NetInfo>, probe_port: u16) {
        let net = match fixed {
            Some(n) => Some(n),
            None => gateway::detect(probe_port).await.ok(),
        };
        let net = match net {
            Some(n) => {
                if self.bound_to != Some(n.local) {
                    match UdpSocket::bind((n.local, 0)).await {
                        Ok(s) => {
                            self.sock = Some(s);
                            self.bound_to = Some(n.local);
                            Some(n)
                        }
                        Err(_) => None,
                    }
                } else {
                    Some(n)
                }
            }
            None => None,
        };
        if net.is_none() {
            self.sock = None;
            self.bound_to = None;
        }
        self.feed(Input::Network(net)).await;
    }
}

async fn run(
    cfg: DriverConfig,
    mut cmd_rx: mpsc::Receiver<Cmd>,
    events: mpsc::Sender<Event>,
    snap: watch::Sender<Snapshot>,
) {
    let (http_tx, mut http_rx) = mpsc::unbounded_channel();
    let probe_port = cfg.core.gateway_port;
    let mut st = State {
        core: PortMapper::new(cfg.core.clone()),
        start: Instant::now(),
        sock: None,
        bound_to: None,
        http_tx,
        events,
        snap,
    };
    st.network(cfg.fixed_net, probe_port).await;
    let mut buf = vec![0u8; 4096];
    let mut ack: Option<oneshot::Sender<()>> = None;
    // false once every handle is gone: that counts as a shutdown request
    let mut handles_alive = true;
    loop {
        if st.core.is_done() {
            break;
        }
        let wake = st
            .core
            .next_wakeup()
            .map(|m| st.start + Duration::from_millis(m.0));
        tokio::select! {
            cmd = cmd_rx.recv(), if handles_alive && ack.is_none() => match cmd {
                Some(Cmd::NetworkChanged) => st.network(cfg.fixed_net, probe_port).await,
                Some(Cmd::Retry) => st.feed(Input::Retry).await,
                Some(Cmd::Shutdown(a)) => { ack = Some(a); st.feed(Input::Shutdown).await; }
                None => {
                    handles_alive = false;
                    st.feed(Input::Shutdown).await;
                }
            },
            r = recv(st.sock.as_ref(), &mut buf) => {
                if let Ok((n, from)) = r {
                    let data = buf[..n].to_vec();
                    st.feed(Input::Udp { from, data: &data }).await;
                } else {
                    // a failed receive (for example ICMP unreachable): do not spin
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            },
            Some((id, outcome)) = http_rx.recv() => st.feed(Input::HttpDone { id, outcome }).await,
            _ = sleep_until(wake) => st.feed(Input::Tick).await,
        }
    }
    if let Some(a) = ack {
        let _ = a.send(());
    }
}

async fn recv(sock: Option<&UdpSocket>, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
    match sock {
        Some(s) => s.recv_from(buf).await,
        None => std::future::pending().await,
    }
}

async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// One TCP exchange: connect, send, read until the response is complete, EOF or the cap.
async fn exchange(req: HttpRequest) -> Result<Vec<u8>, HttpFailure> {
    let run = async {
        let mut s = TcpStream::connect(req.to)
            .await
            .map_err(|_| HttpFailure::Connect)?;
        s.write_all(&req.request)
            .await
            .map_err(|_| HttpFailure::Io)?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            match s.read(&mut tmp).await {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    if buf.len() > req.max_response {
                        return Err(HttpFailure::TooLarge);
                    }
                    if http::is_complete(&buf) {
                        break;
                    }
                }
                Err(_) if buf.is_empty() => return Err(HttpFailure::Io),
                Err(_) => break,
            }
        }
        if buf.is_empty() {
            Err(HttpFailure::Io)
        } else {
            Ok(buf)
        }
    };
    tokio::time::timeout(req.timeout, run)
        .await
        .map_err(|_| HttpFailure::Timeout)?
}
