//! The tokio driver against a NAT-PMP server on real loopback UDP sockets.

use crate::common;

use std::time::Duration;

use common::net::*;
use common::*;
use trs_portmap::driver::spawn;
use trs_portmap::{Event, Failure, Method, Protocol};

#[tokio::test]
async fn maps_tcp_and_udp_and_reports_the_external_address() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    let (h, mut ev) = spawn(driver_cfg(port, &[Method::NatPmp], 7200));
    wait_for(5, "two mappings", || h.snapshot().mappings.len() == 2).await;
    let s = h.snapshot();
    assert_eq!(s.method, Some(Method::NatPmp));
    assert_eq!(s.external_ip, Some(EXTERNAL_IP));
    assert!(
        s.mappings
            .iter()
            .all(|m| m.external_port == 51413 && m.lifetime_secs == 7200)
    );
    assert_eq!(gw.lock().unwrap().has(Protocol::Tcp, 51413), Some(51413));
    assert_eq!(gw.lock().unwrap().has(Protocol::Udp, 51413), Some(51413));
    let events = drain(&mut ev);
    assert!(events.contains(&Event::MethodSelected(Method::NatPmp)));
    assert!(events.contains(&Event::ExternalIp {
        ip: EXTERNAL_IP,
        public: true
    }));
    h.shutdown().await;
}

#[tokio::test]
async fn error_codes_surface_as_events_and_no_mapping() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    gw.lock().unwrap().behavior.map_result = Some(2);
    let (h, mut ev) = spawn(driver_cfg(port, &[Method::NatPmp], 7200));
    let mut got = Vec::new();
    wait_for(5, "SlotFailed", || {
        got.extend(drain(&mut ev));
        got.iter().any(|e| {
            matches!(
                e,
                Event::SlotFailed {
                    reason: Failure::Refused,
                    ..
                }
            )
        })
    })
    .await;
    assert!(h.snapshot().mappings.is_empty());
    // exactly one probe + one attempt per protocol so far: not a storm
    let n = gw.lock().unwrap().requests.len();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(gw.lock().unwrap().requests.len(), n);
    h.shutdown().await;
}

#[tokio::test]
async fn short_leases_are_renewed_at_half_time() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    let (h, _ev) = spawn(driver_cfg(port, &[Method::NatPmp], 2));
    wait_for(5, "mapped", || h.snapshot().mappings.len() == 2).await;
    wait_for(6, "three TCP requests (initial + 2 renewals)", || {
        gw.lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r.len() == 12 && r[1] == 2)
            .count()
            >= 3
    })
    .await;
    // and the mapping never lapsed in between
    assert_eq!(h.snapshot().mappings.len(), 2);
    h.shutdown().await;
}

#[tokio::test]
async fn gateway_reboot_is_noticed_and_mappings_return() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    let (h, mut ev) = spawn(driver_cfg(port, &[Method::NatPmp], 2));
    wait_for(5, "mapped", || h.snapshot().mappings.len() == 2).await;
    tokio::time::sleep(Duration::from_millis(1300)).await; // epoch >= 1 recorded at a renewal
    // Reboot "in the future" of the mock's clock: its epoch saturates to 0 from now on,
    // which is lower than the epoch we recorded at the last renewal.
    gw.lock().unwrap().reboot(u64::MAX);
    let mut got = Vec::new();
    wait_for(8, "GatewayRestarted", || {
        got.extend(drain(&mut ev));
        got.contains(&Event::GatewayRestarted)
    })
    .await;
    wait_for(5, "mappings restored", || {
        gw.lock().unwrap().table.len() == 2
    })
    .await;
    h.shutdown().await;
}

#[tokio::test]
async fn shutdown_deletes_the_mappings_on_the_gateway() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    let (h, mut ev) = spawn(driver_cfg(port, &[Method::NatPmp], 7200));
    wait_for(5, "mapped", || h.snapshot().mappings.len() == 2).await;
    assert_eq!(gw.lock().unwrap().table.len(), 2);
    tokio::time::timeout(Duration::from_secs(5), h.shutdown())
        .await
        .unwrap();
    assert!(gw.lock().unwrap().table.is_empty());
    let events = drain(&mut ev);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Event::Deleted { .. }))
            .count(),
        2
    );
    assert!(events.contains(&Event::ShutdownComplete));
    // nothing happens after shutdown
    let n = gw.lock().unwrap().requests.len();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(gw.lock().unwrap().requests.len(), n);
}

#[tokio::test]
async fn shutdown_with_a_dead_gateway_returns_within_the_deadline() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    let (h, _ev) = spawn(driver_cfg(port, &[Method::NatPmp], 7200));
    wait_for(5, "mapped", || h.snapshot().mappings.len() == 2).await;
    gw.lock().unwrap().behavior.silent = true;
    let t = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(6), h.shutdown())
        .await
        .unwrap();
    assert!(
        t.elapsed() < Duration::from_millis(3500),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn dropping_every_handle_also_removes_the_mappings() {
    let (port, gw) = udp_gateway(GatewayBehavior {
        pcp: false,
        ..Default::default()
    })
    .await;
    let (h, _ev) = spawn(driver_cfg(port, &[Method::NatPmp], 7200));
    wait_for(5, "mapped", || h.snapshot().mappings.len() == 2).await;
    drop(h);
    wait_for(5, "deleted", || gw.lock().unwrap().table.is_empty()).await;
}
