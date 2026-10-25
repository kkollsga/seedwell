//! Port mapping for the listen port (TCP and UDP) with `trs-portmap`, plus at most one extra port, also TCP
//! and UDP (folder sync's, SPEC 7G: the engine lends sync a port mapping).
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

fn status(s: &Snapshot, listen: u16, extra: Option<u16>, public: Option<bool>, unavailable: bool) -> PortMapStatus {
    let port = |p: Protocol, internal: u16| s.mappings.iter().find(|m| m.protocol == p && m.internal_port == internal).map(|m| m.external_port);
    PortMapStatus {
        method: s.method.map(method_name),
        external_ip: s.external_ip,
        behind_cgnat: public == Some(false),
        external_tcp_port: port(Protocol::Tcp, listen),
        external_udp_port: port(Protocol::Udp, listen),
        extra_external_tcp_port: extra.and_then(|e| port(Protocol::Tcp, e)),
        extra_external_udp_port: extra.and_then(|e| port(Protocol::Udp, e)),
        unavailable,
    }
}

/// The mappings to ask for: TCP and UDP for the listen port, and TCP and UDP for the extra port (when it is
/// another port; sync runs TCP and uTP on it).
fn wanted(listen: u16, extra: Option<u16>) -> Vec<Wanted> {
    let mut w = Wanted::tcp_and_udp(listen);
    if let Some(e) = extra.filter(|&e| e != 0 && e != listen) {
        w.extend(Wanted::tcp_and_udp(e));
    }
    w
}

impl PortMapRunner {
    /// Start mapping the current listen port (and `extra`), if the configuration asks for it.
    pub(crate) fn start(sh: &Arc<Shared>, serial: u64, extra: Option<u16>) -> Option<Self> {
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
        let mut core = Config { wanted: wanted(port, extra), methods, ..Config::default() };
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
                let now = status(&watch.borrow(), port, extra, public, unavailable);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extra_port_is_mapped_for_tcp_and_udp() {
        assert_eq!(wanted(6881, None), Wanted::tcp_and_udp(6881));
        let w = wanted(6881, Some(22000));
        assert_eq!(w.len(), 4);
        assert_eq!(w[2..], Wanted::tcp_and_udp(22000)[..]);
        assert_eq!(wanted(6881, Some(6881)).len(), 2, "the listen port itself is not mapped twice");
        assert_eq!(wanted(6881, Some(0)).len(), 2);
    }

    #[test]
    fn the_status_keeps_the_ports_apart() {
        use trs_portmap::Mapping;
        let m = |protocol, internal_port, external_port| Mapping { protocol, internal_port, external_port, external_ip: None, lifetime_secs: 7200, method: Method::NatPmp };
        let snap = Snapshot {
            method: Some(Method::NatPmp),
            external_ip: None,
            // The sync port's mapping comes first: the listen port's TCP port must not be taken from it.
            mappings: vec![m(Protocol::Udp, 22000, 23001), m(Protocol::Tcp, 22000, 23000), m(Protocol::Tcp, 6881, 6881), m(Protocol::Udp, 6881, 6882)],
        };
        let s = status(&snap, 6881, Some(22000), Some(true), false);
        assert_eq!((s.external_tcp_port, s.external_udp_port), (Some(6881), Some(6882)));
        assert_eq!((s.extra_external_tcp_port, s.extra_external_udp_port), (Some(23000), Some(23001)));
        let none = status(&snap, 6881, None, None, false);
        assert_eq!((none.extra_external_tcp_port, none.extra_external_udp_port), (None, None));
    }
}
