//! Talks to the real local router. `#[ignore]`d: run with
//! `cargo test -p trs-portmap --test real_router -- --ignored --nocapture`.
//!
//! It only *reads*: NAT-PMP external-address request, a PCP ANNOUNCE (opcode 0, creates
//! nothing) and UPnP `GetExternalIPAddress`. No mapping is ever created here.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UdpSocket};
use trs_portmap::{gateway, http, natpmp, pcp, ssdp, upnp};

async fn udp_exchange(
    sock: &UdpSocket,
    to: SocketAddr,
    packet: &[u8],
    tries: u32,
) -> Option<Vec<u8>> {
    let mut wait = 250u64;
    let mut buf = [0u8; 2048];
    for _ in 0..tries {
        sock.send_to(packet, to).await.ok()?;
        let deadline = tokio::time::Instant::now() + Duration::from_millis(wait);
        while let Ok(Ok((n, from))) =
            tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await
        {
            if from == to {
                return Some(buf[..n].to_vec());
            }
        }
        wait *= 2;
    }
    None
}

async fn tcp_exchange(to: SocketAddrV4, request: &[u8]) -> Option<Vec<u8>> {
    let run = async {
        let mut s = TcpStream::connect(to).await.ok()?;
        s.write_all(request).await.ok()?;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = s.read(&mut tmp).await.ok()?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if http::is_complete(&buf) || buf.len() > http::MAX_RESPONSE {
                break;
            }
        }
        Some(buf)
    };
    tokio::time::timeout(Duration::from_secs(5), run)
        .await
        .ok()?
}

#[tokio::test]
#[ignore = "talks to the real router on the local network"]
async fn query_the_real_router_without_creating_mappings() {
    let net = gateway::detect(natpmp::PORT)
        .await
        .expect("default gateway");
    println!(
        "default gateway {}, local address {}",
        net.gateway, net.local
    );
    let sock = UdpSocket::bind((net.local, 0)).await.unwrap();
    let gw = SocketAddr::from((net.gateway, natpmp::PORT));

    // --- NAT-PMP: external address request (read only)
    let natpmp_ip =
        match udp_exchange(&sock, gw, &natpmp::encode_external_address_request(), 4).await {
            None => {
                println!("NAT-PMP: no answer");
                None
            }
            Some(r) => {
                let d = natpmp::decode_response(&r);
                println!("NAT-PMP: {d:?}");
                match d {
                    Ok(natpmp::Response::ExternalAddress { address, .. }) => address,
                    _ => None,
                }
            }
        };

    // --- PCP: ANNOUNCE (opcode 0), read only
    let mut announce = [0u8; 24];
    announce[0] = 2;
    announce[18] = 0xff;
    announce[19] = 0xff;
    announce[20..24].copy_from_slice(&net.local.octets());
    match udp_exchange(&sock, gw, &announce, 3).await {
        None => println!("PCP: no answer"),
        Some(r) => match pcp::decode_response(&r) {
            Err(pcp::DecodeError::LegacyNatPmpServer) => {
                println!("PCP: not supported (the router answered in NAT-PMP format, version 0)")
            }
            other => println!(
                "PCP: version {} opcode {:#x} result {} ({} bytes) decode={other:?}",
                r[0],
                r.get(1).copied().unwrap_or(0),
                r.get(3).copied().unwrap_or(255),
                r.len()
            ),
        },
    }

    // --- UPnP IGD: SSDP search, description, GetExternalIPAddress (read only)
    let mut found = Vec::new();
    for st in ssdp::SEARCH_TARGETS {
        let _ = sock
            .send_to(
                &ssdp::build_search(st, "Seedwell-probe"),
                SocketAddr::V4(ssdp::MULTICAST),
            )
            .await;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(4);
    let mut buf = [0u8; 4096];
    while let Ok(Ok((n, from))) = tokio::time::timeout_at(deadline, sock.recv_from(&mut buf)).await
    {
        if let Some(r) = ssdp::parse_reply(&buf[..n]) {
            println!("SSDP reply from {from}: {}", r.location);
            found.push((from, r));
        }
    }
    let mut upnp_ip = None;
    if found.is_empty() {
        println!("UPnP: no IGD answered the SSDP search");
    }
    for (from, reply) in found.iter().take(1) {
        let Some(url) = http::parse_url(&reply.location) else {
            println!("UPnP: unusable LOCATION");
            continue;
        };
        println!(
            "UPnP: device at {from}, gateway is {} (same: {})",
            net.gateway,
            url.ip == net.gateway
        );
        let Some(raw) = tcp_exchange(url.addr(), &http::build_get(&url, "Seedwell-probe")).await
        else {
            println!("UPnP: description fetch failed");
            continue;
        };
        let Ok(resp) = http::parse_response(&raw, http::MAX_RESPONSE) else {
            println!("UPnP: description is not HTTP");
            continue;
        };
        let desc = upnp::parse_description(&String::from_utf8_lossy(&resp.body));
        let Ok(desc) = desc else {
            println!("UPnP: description XML rejected: {:?}", desc.err());
            continue;
        };
        for s in &desc.services {
            println!("  service {} -> {}", s.service_type, s.control_url);
        }
        let Some((svc, kind)) = upnp::pick_service(&desc) else {
            println!("UPnP: no WANIPConnection/WANPPPConnection service");
            continue;
        };
        println!("UPnP: using {kind:?}");
        let base = desc
            .url_base
            .as_deref()
            .and_then(http::parse_url)
            .filter(|b| b.ip == url.ip)
            .unwrap_or_else(|| url.clone());
        let control = http::resolve(&base, &svc.control_url).expect("control url");
        let body = upnp::external_ip_body(&svc.service_type);
        let req = http::build_soap_post(
            &control,
            "Seedwell-probe",
            &svc.service_type,
            "GetExternalIPAddress",
            &body,
        );
        match tcp_exchange(control.addr(), &req)
            .await
            .and_then(|r| http::parse_response(&r, http::MAX_RESPONSE).ok())
        {
            Some(r) => {
                let reply = upnp::parse_reply(r.status, &r.body);
                println!("UPnP GetExternalIPAddress: {reply:?}");
                if let upnp::SoapReply::Ok { external_ip } = reply {
                    upnp_ip = external_ip;
                }
            }
            None => println!("UPnP: SOAP call failed"),
        }
    }

    println!(
        "SUMMARY gateway={} natpmp_external={:?} upnp_external={:?}",
        net.gateway, natpmp_ip, upnp_ip
    );
    let _: Option<Ipv4Addr> = natpmp_ip.or(upnp_ip);
}
