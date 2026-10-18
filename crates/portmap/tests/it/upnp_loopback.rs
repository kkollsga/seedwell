//! The tokio driver against a mock IGD: SSDP responder plus SOAP over real TCP.

use crate::common;

use std::time::Duration;

use common::net::*;
use common::*;
use trs_portmap::driver::spawn;
use trs_portmap::{Event, Method};

fn upnp_cfg(ssdp: std::net::SocketAddrV4, lifetime: u32) -> trs_portmap::driver::DriverConfig {
    let mut c = driver_cfg(5351, &[Method::Upnp], lifetime);
    c.core.ssdp_addr = ssdp;
    c
}

#[tokio::test]
async fn upnp_discovers_maps_renews_and_deletes() {
    let (ssdp, igd) = igd(IgdBehavior::default()).await;
    let (h, mut ev) = spawn(upnp_cfg(ssdp, 2));
    wait_for(8, "two mappings", || h.snapshot().mappings.len() == 2).await;
    let s = h.snapshot();
    assert_eq!(s.method, Some(Method::Upnp));
    assert_eq!(s.external_ip, Some(EXTERNAL_IP));
    assert_eq!(igd.lock().unwrap().table.len(), 2);
    wait_for(8, "renewal", || {
        igd.lock()
            .unwrap()
            .actions
            .iter()
            .filter(|a| *a == "AddPortMapping")
            .count()
            >= 4
    })
    .await;
    tokio::time::timeout(Duration::from_secs(8), h.shutdown())
        .await
        .unwrap();
    assert!(igd.lock().unwrap().table.is_empty());
    let events = drain(&mut ev);
    assert!(events.contains(&Event::MethodSelected(Method::Upnp)));
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Deleted { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn upnp_with_permanent_leases_and_chunked_replies() {
    let (ssdp, igd) = igd(IgdBehavior {
        only_permanent: true,
        chunked: true,
        ..Default::default()
    })
    .await;
    let (h, _ev) = spawn(upnp_cfg(ssdp, 7200));
    wait_for(8, "two mappings", || h.snapshot().mappings.len() == 2).await;
    assert!(
        igd.lock()
            .unwrap()
            .table
            .values()
            .all(|(_, lease)| *lease == 0)
    );
    h.shutdown().await;
    assert!(igd.lock().unwrap().table.is_empty());
}

#[tokio::test]
async fn no_igd_on_the_network_gives_up_quietly() {
    // an SSDP address nobody listens on, and no UPnP traffic after the window
    let silent = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let std::net::SocketAddr::V4(addr) = silent.local_addr().unwrap() else {
        unreachable!()
    };
    let mut cfg = upnp_cfg(addr, 7200);
    cfg.core.ssdp_window = Duration::from_millis(2000);
    let (h, mut ev) = spawn(cfg);
    let mut got = Vec::new();
    wait_for(5, "NoMethod", || {
        got.extend(drain(&mut ev));
        got.contains(&Event::NoMethod)
    })
    .await;
    // the search went out as 2 rounds x 2 targets and then nothing
    let mut buf = [0u8; 2048];
    let mut seen = 0;
    while let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_millis(300), silent.recv_from(&mut buf)).await
    {
        seen += 1;
    }
    assert_eq!(seen, 4);
    assert!(h.snapshot().mappings.is_empty());
    h.shutdown().await;
}
