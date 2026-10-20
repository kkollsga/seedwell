//! Port mapping for the listen port (TCP and UDP) with `trs-portmap`.
//!
//! Started when `network.upnp` or `network.natpmp` is on, stopped (mappings deleted) when they go
//! off and at shutdown. The driver is polite by construction (one exchange at a time, back-off,
//! one discovery per network change); the engine only starts it, forwards `network_changed` and
//! publishes a [`PortMapStatus`].

use std::net::Ipv4Addr;
use std::sync::Arc;

use tokio::task::JoinHandle;
use trs_portmap::driver::{DriverConfig, PortMapHandle, Snapshot, spawn};
use trs_portmap::{Config, Event, Method, Protocol, Wanted};

use crate::api::PortMapStatus;
use crate::session::SessionMsg;
use crate::shared::Shared;

/// A running mapper plus the task that turns its events into engine events.
pub(crate) struct PortMapRunner {
    handle: PortMapHandle,
    task: JoinHandle<()>,
}

fn method_name(m: Method) -> &'static str {
    match m {
        Method::Pcp => "pcp",
        Method::NatPmp => "nat-pmp",
        Method::Upnp => "upnp",
    }
}

fn status(s: &Snapshot, public: Option<bool>, unavailable: bool) -> PortMapStatus {
    let port = |p: Protocol| s.mappings.iter().find(|m| m.protocol == p).map(|m| m.external_port);
    PortMapStatus {
        method: s.method.map(method_name),
        external_ip: s.external_ip,
        behind_cgnat: public == Some(false),
        external_tcp_port: port(Protocol::Tcp),
        external_udp_port: port(Protocol::Udp),
        unavailable,
    }
}

impl PortMapRunner {
    /// Start mapping the current listen port, if the configuration asks for it.
    pub(crate) fn start(sh: &Arc<Shared>, serial: u64) -> Option<Self> {
        let cfg = sh.cfg();
        let pc = cfg.portmap;
        let port = sh.port();
        if !pc.enabled || (!pc.upnp && !pc.natpmp) || port == 0 {
            return None;
        }
        let mut methods = Vec::new();
        if pc.natpmp {
            methods.extend([Method::Pcp, Method::NatPmp]);
        }
        if pc.upnp {
            methods.push(Method::Upnp);
        }
        let mut core = Config { wanted: Wanted::tcp_and_udp(port), methods, ..Config::default() };
        let mut fixed_net = None;
        if let Some(gw) = pc.test_gateway {
            core.gateway_port = gw.port();
            fixed_net = Some(trs_portmap::NetInfo { gateway: *gw.ip(), local: Ipv4Addr::LOCALHOST });
        }
        let (handle, mut events) = spawn(DriverConfig { core, fixed_net });
        let mut watch = handle.watch();
        let tx = sh.session.clone();
        let task = tokio::spawn(async move {
            let (mut public, mut unavailable, mut last) = (None, false, PortMapStatus::default());
            loop {
                tokio::select! {
                    ev = events.recv() => match ev {
                        None => break,
                        Some(Event::ExternalIp { public: p, .. }) => public = Some(p),
                        Some(Event::NoMethod) => unavailable = true,
                        Some(Event::Mapped(_) | Event::MethodSelected(_)) => unavailable = false,
                        Some(_) => {}
                    },
                    r = watch.changed() => if r.is_err() { break },
                }
                let now = status(&watch.borrow(), public, unavailable);
                if now != last {
                    last = now.clone();
                    if tx.send(SessionMsg::PortMap { serial, status: now }).await.is_err() {
                        break;
                    }
                }
            }
        });
        Some(Self { handle, task })
    }

    /// The network changed (wake, new interface): the driver re-reads the gateway.
    pub(crate) fn network_changed(&self) {
        let h = self.handle.clone();
        tokio::spawn(async move { h.network_changed().await });
    }

    /// Delete the mappings (at most the driver's own 3 s) and stop.
    pub(crate) async fn shutdown(self) {
        self.handle.shutdown().await;
        self.task.abort();
    }
}
