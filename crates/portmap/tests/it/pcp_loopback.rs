//! The tokio driver against a PCP server on real loopback UDP sockets.

use crate::common;

use std::time::Duration;

use common::net::*;
use common::*;
use trs_portmap::driver::spawn;
use trs_portmap::{Event, Method, Protocol};

#[tokio::test]
async fn pcp_maps_renews_and_deletes() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        natpmp: false,
        ..Default::default()
    })
    .await;
    let (h, mut ev) = spawn(driver_cfg(port, &[Method::Pcp, Method::NatPmp], 2));
    wait_for(5, "two mappings", || h.snapshot().mappings.len() == 2).await;
    let s = h.snapshot();
    assert_eq!(s.method, Some(Method::Pcp));
    assert_eq!(s.external_ip, Some(EXTERNAL_IP));
    assert_eq!(gw.lock().unwrap().has(Protocol::Udp, 51413), Some(51413));
    wait_for(6, "renewals", || {
        gw.lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.len() == 60 && r[36] == 6)
            .count()
            >= 3
    })
    .await;
    tokio::time::timeout(Duration::from_secs(5), h.shutdown())
        .await
        .unwrap();
    assert!(gw.lock().unwrap().table.is_empty());
    let events = drain(&mut ev);
    assert!(events.contains(&Event::MethodSelected(Method::Pcp)));
    assert!(events.contains(&Event::ShutdownComplete));
}

#[tokio::test]
async fn nat_pmp_only_gateway_is_found_without_waiting_for_pcp_timeouts() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    let t = std::time::Instant::now();
    let (h, _ev) = spawn(driver_cfg(port, &[Method::Pcp, Method::NatPmp], 7200));
    wait_for(5, "two mappings", || h.snapshot().mappings.len() == 2).await;
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(h.snapshot().method, Some(Method::NatPmp));
    assert_eq!(gw.lock().unwrap().table.len(), 2);
    h.shutdown().await;
}

#[tokio::test]
async fn network_change_with_the_same_gateway_sends_nothing() {
    let (port, gw) = udp_gateway(GatewayBehavior::default()).await;
    let (h, _ev) = spawn(driver_cfg(port, &[Method::Pcp], 7200));
    wait_for(5, "mapped", || h.snapshot().mappings.len() == 2).await;
    let n = gw.lock().unwrap().requests.len();
    h.network_changed().await;
    h.network_changed().await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(gw.lock().unwrap().requests.len(), n);
    h.shutdown().await;
}
