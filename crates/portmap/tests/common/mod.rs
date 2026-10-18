//! Pure mock gateways shared by the virtual-time tests and the loopback tests.
//!
//! Nothing here touches a socket: `MockGateway::handle` maps a request datagram to an
//! optional reply, `MockIgd::handle_http` maps a raw HTTP request to raw response bytes.
//! The loopback tests wrap them in real UDP/TCP servers.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::net::Ipv4Addr;

use trs_portmap::Protocol;

pub const EXTERNAL_IP: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 9);

/// What the mock gateway does with requests.
#[derive(Clone, Debug)]
pub struct GatewayBehavior {
    /// Answer NAT-PMP (version 0).
    pub natpmp: bool,
    /// Answer PCP (version 2). A NAT-PMP-only gateway answers version 2 with result 1.
    pub pcp: bool,
    /// Ignore every datagram.
    pub silent: bool,
    /// Force this result code on mapping requests (NAT-PMP code / PCP code).
    pub map_result: Option<u8>,
    /// Lifetime granted for mapping requests (None = what was asked).
    pub grant_lifetime: Option<u32>,
    /// PCP error responses carry this lifetime (retry hint).
    pub error_lifetime: u32,
    /// Assign external port = requested + this (to model a busy port).
    pub port_offset: u16,
}

impl Default for GatewayBehavior {
    fn default() -> Self {
        GatewayBehavior {
            natpmp: true,
            pcp: true,
            silent: false,
            map_result: None,
            grant_lifetime: None,
            error_lifetime: 0,
            port_offset: 0,
        }
    }
}

type Key = (u8, u16); // (protocol number, internal port)

#[derive(Debug)]
pub struct MockGateway {
    pub behavior: GatewayBehavior,
    boot_ms: u64,
    /// (proto, internal port) -> (external port, expiry in ms)
    pub table: BTreeMap<Key, (u16, u64)>,
    pub requests: Vec<Vec<u8>>,
    pub client_ip: Ipv4Addr,
}

impl MockGateway {
    pub fn new(behavior: GatewayBehavior) -> Self {
        MockGateway {
            behavior,
            boot_ms: 0,
            table: BTreeMap::new(),
            requests: Vec::new(),
            client_ip: Ipv4Addr::LOCALHOST,
        }
    }

    /// The gateway lost power: empty table, epoch restarts at `now_ms`.
    pub fn reboot(&mut self, now_ms: u64) {
        self.boot_ms = now_ms;
        self.table.clear();
    }

    fn epoch(&self, now_ms: u64) -> u32 {
        (now_ms.saturating_sub(self.boot_ms) / 1000) as u32
    }

    fn external_for(&self, proto: u8, internal: u16, suggested: u16) -> u16 {
        if let Some((e, _)) = self.table.get(&(proto, internal)) {
            return *e;
        }
        let want = if suggested == 0 { internal } else { suggested };
        want.wrapping_add(self.behavior.port_offset)
    }

    /// Process one request datagram at virtual time `now_ms`.
    pub fn handle(&mut self, data: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        self.requests.push(data.to_vec());
        if self.behavior.silent || data.len() < 2 {
            return None;
        }
        match data[0] {
            0 if self.behavior.natpmp => self.natpmp(data, now_ms),
            0 => None,
            2 if self.behavior.pcp => self.pcp(data, now_ms),
            2 if self.behavior.natpmp => {
                // RFC 6886: unsupported version -> result 1, our epoch.
                let mut r = vec![0, 0x80 | (data[1] & 0x7f), 0, 1];
                r.extend(self.epoch(now_ms).to_be_bytes());
                Some(r)
            }
            _ => None,
        }
    }

    fn natpmp(&mut self, d: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        let epoch = self.epoch(now_ms).to_be_bytes();
        match d[1] {
            0 => {
                let mut r = vec![0, 128, 0, 0];
                r.extend(epoch);
                r.extend(EXTERNAL_IP.octets());
                Some(r)
            }
            op @ (1 | 2) if d.len() >= 12 => {
                let proto = if op == 1 { 17 } else { 6 };
                let internal = u16::from_be_bytes([d[4], d[5]]);
                let suggested = u16::from_be_bytes([d[6], d[7]]);
                let lifetime = u32::from_be_bytes([d[8], d[9], d[10], d[11]]);
                if let Some(code) = self.behavior.map_result {
                    let mut r = vec![0, 128 + op, 0, code];
                    r.extend(epoch);
                    return Some(r);
                }
                let (ext, granted) = if lifetime == 0 {
                    self.table.remove(&(proto, internal));
                    (0, 0)
                } else {
                    let ext = self.external_for(proto, internal, suggested);
                    let granted = self
                        .behavior
                        .grant_lifetime
                        .unwrap_or(lifetime)
                        .min(lifetime);
                    self.table
                        .insert((proto, internal), (ext, now_ms + u64::from(granted) * 1000));
                    (ext, granted)
                };
                let mut r = vec![0, 128 + op, 0, 0];
                r.extend(epoch);
                r.extend(internal.to_be_bytes());
                r.extend(ext.to_be_bytes());
                r.extend(granted.to_be_bytes());
                Some(r)
            }
            _ => None,
        }
    }

    fn pcp(&mut self, d: &[u8], now_ms: u64) -> Option<Vec<u8>> {
        if d.len() < 60 || d[1] != 1 {
            return None;
        }
        let lifetime = u32::from_be_bytes([d[4], d[5], d[6], d[7]]);
        let proto = d[36];
        let internal = u16::from_be_bytes([d[40], d[41]]);
        let suggested = u16::from_be_bytes([d[42], d[43]]);
        let mut r = vec![0u8; 60];
        r[0] = 2;
        r[1] = 0x81;
        r[8..12].copy_from_slice(&self.epoch(now_ms).to_be_bytes());
        r[24..36].copy_from_slice(&d[24..36]);
        r[36] = proto;
        r[40..42].copy_from_slice(&internal.to_be_bytes());
        if let Some(code) = self.behavior.map_result {
            r[3] = code;
            r[4..8].copy_from_slice(&self.behavior.error_lifetime.to_be_bytes());
            return Some(r);
        }
        // the client address in the header must be our source address
        if d[8 + 12..8 + 16] != self.client_ip.octets() {
            r[3] = 12; // ADDRESS_MISMATCH
            return Some(r);
        }
        let (ext, granted) = if lifetime == 0 {
            self.table.remove(&(proto, internal));
            (0, 0)
        } else {
            let ext = self.external_for(proto, internal, suggested);
            let granted = self
                .behavior
                .grant_lifetime
                .unwrap_or(lifetime)
                .min(lifetime);
            self.table
                .insert((proto, internal), (ext, now_ms + u64::from(granted) * 1000));
            (ext, granted)
        };
        r[4..8].copy_from_slice(&granted.to_be_bytes());
        r[42..44].copy_from_slice(&ext.to_be_bytes());
        let ip = EXTERNAL_IP.octets();
        r[44..54].fill(0);
        r[54] = 0xff;
        r[55] = 0xff;
        r[56..60].copy_from_slice(&ip);
        Some(r)
    }

    pub fn has(&self, protocol: Protocol, internal: u16) -> Option<u16> {
        self.table
            .get(&(protocol.number(), internal))
            .map(|(e, _)| *e)
    }
}

// ---------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct IgdBehavior {
    /// Reject leases other than 0 with error 725.
    pub only_permanent: bool,
    /// External ports that are "taken" (error 718).
    pub taken_ports: Vec<u16>,
    /// Require external == internal (error 724).
    pub same_port_only: bool,
    /// Fail every AddPortMapping with this UPnP error.
    pub add_error: Option<u32>,
    /// Fail GetExternalIPAddress with this error.
    pub ip_error: Option<u32>,
    /// Use this controlURL in the description (default `/ctl/IPConn`).
    pub control_url: Option<String>,
    /// Describe only a PPP service.
    pub ppp_only: bool,
    /// Send a chunked SOAP reply.
    pub chunked: bool,
}

#[derive(Debug, Default)]
pub struct MockIgd {
    pub behavior: IgdBehavior,
    /// (protocol name, external port) -> (internal port, lease)
    pub table: BTreeMap<(String, u16), (u16, u32)>,
    pub actions: Vec<String>,
}

fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let i = s.find(a)? + a.len();
    let j = s[i..].find(b)? + i;
    Some(&s[i..j])
}

impl MockIgd {
    pub fn new(behavior: IgdBehavior) -> Self {
        MockIgd {
            behavior,
            ..Default::default()
        }
    }

    pub fn description(&self, host: &str) -> String {
        let ctl = self
            .behavior
            .control_url
            .clone()
            .unwrap_or_else(|| "/ctl/IPConn".to_owned());
        let svc = if self.behavior.ppp_only {
            "urn:schemas-upnp-org:service:WANPPPConnection:1"
        } else {
            "urn:schemas-upnp-org:service:WANIPConnection:1"
        };
        format!(
            r#"<?xml version="1.0"?><root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major><minor>0</minor></specVersion><device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType><deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType><deviceList><device><deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType><serviceList><service><serviceType>{svc}</serviceType><serviceId>urn:upnp-org:serviceId:WANIPConn1</serviceId><controlURL>{ctl}</controlURL><eventSubURL>/evt</eventSubURL><SCPDURL>/scpd.xml</SCPDURL></service></serviceList></device></deviceList></device></deviceList><presentationURL>http://{host}/</presentationURL></device></root>"#
        )
    }

    fn response(&self, status: &str, body: &str) -> Vec<u8> {
        if self.behavior.chunked && status.starts_with("200") {
            let (a, b) = body.split_at(body.len() / 2);
            return format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{a}\r\n{:x}\r\n{b}\r\n0\r\n\r\n",
                a.len(),
                b.len()
            )
            .into_bytes();
        }
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn fault(&self, code: u32) -> Vec<u8> {
        self.response(
            "500 Internal Server Error",
            &format!(
                "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>x</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"
            ),
        )
    }

    /// Process one raw HTTP request; `host` is what we advertise in the description.
    pub fn handle_http(&mut self, req: &[u8], host: &str) -> Vec<u8> {
        let text = String::from_utf8_lossy(req).into_owned();
        if text.starts_with("GET ") {
            self.actions.push("GET".into());
            return self.response("200 OK", &self.description(host));
        }
        let action = between(&text, "SOAPAction: \"", "\"")
            .and_then(|a| a.split('#').nth(1))
            .unwrap_or("")
            .to_owned();
        self.actions.push(action.clone());
        let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
        match action.as_str() {
            "GetExternalIPAddress" => {
                if let Some(c) = self.behavior.ip_error {
                    return self.fault(c);
                }
                self.response(
                    "200 OK",
                    &format!(
                        "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:GetExternalIPAddressResponse xmlns:u=\"x\"><NewExternalIPAddress>{EXTERNAL_IP}</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>"
                    ),
                )
            }
            "AddPortMapping" => {
                if let Some(c) = self.behavior.add_error {
                    return self.fault(c);
                }
                let ext: u16 = between(body, "<NewExternalPort>", "</NewExternalPort>")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let int: u16 = between(body, "<NewInternalPort>", "</NewInternalPort>")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let proto = between(body, "<NewProtocol>", "</NewProtocol>")
                    .unwrap_or("")
                    .to_owned();
                let lease: u32 = between(body, "<NewLeaseDuration>", "</NewLeaseDuration>")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if self.behavior.only_permanent && lease != 0 {
                    return self.fault(725);
                }
                if self.behavior.same_port_only && ext != int {
                    return self.fault(724);
                }
                if self.behavior.taken_ports.contains(&ext) {
                    return self.fault(718);
                }
                self.table.insert((proto, ext), (int, lease));
                self.response(
                    "200 OK",
                    "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:AddPortMappingResponse xmlns:u=\"x\"/></s:Body></s:Envelope>",
                )
            }
            "DeletePortMapping" => {
                let ext: u16 = between(body, "<NewExternalPort>", "</NewExternalPort>")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let proto = between(body, "<NewProtocol>", "</NewProtocol>")
                    .unwrap_or("")
                    .to_owned();
                if self.table.remove(&(proto, ext)).is_none() {
                    return self.fault(714);
                }
                self.response(
                    "200 OK",
                    "<?xml version=\"1.0\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:DeletePortMappingResponse xmlns:u=\"x\"/></s:Body></s:Envelope>",
                )
            }
            _ => self.response("404 Not Found", "nope"),
        }
    }
}

/// An SSDP reply for the mock IGD.
pub fn ssdp_reply(location: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: mock UPnP/1.1\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nUSN: uuid:mock::urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
    )
    .into_bytes()
}

// ---------------------------------------------------------------------------------
// Loopback servers (real sockets) around the pure mocks.

pub mod net {
    use std::net::{Ipv4Addr, SocketAddrV4};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, UdpSocket};
    use trs_portmap::driver::DriverConfig;
    use trs_portmap::{Config, Event, Method, NetInfo, Wanted};

    use super::*;

    pub const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;

    /// UDP NAT-PMP/PCP server on 127.0.0.1 backed by a [`MockGateway`].
    pub async fn udp_gateway(behavior: GatewayBehavior) -> (u16, Arc<Mutex<MockGateway>>) {
        let sock = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
        let port = sock.local_addr().unwrap().port();
        let gw = Arc::new(Mutex::new(MockGateway::new(behavior)));
        let (g, start) = (gw.clone(), Instant::now());
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = sock.recv_from(&mut buf).await {
                let reply = g
                    .lock()
                    .unwrap()
                    .handle(&buf[..n], start.elapsed().as_millis() as u64);
                if let Some(r) = reply {
                    let _ = sock.send_to(&r, from).await;
                }
            }
        });
        (port, gw)
    }

    /// SSDP responder + HTTP server on 127.0.0.1 backed by a [`MockIgd`].
    pub async fn igd(behavior: IgdBehavior) -> (SocketAddrV4, Arc<Mutex<MockIgd>>) {
        let ssdp = UdpSocket::bind((LOOPBACK, 0)).await.unwrap();
        let ssdp_addr = match ssdp.local_addr().unwrap() {
            std::net::SocketAddr::V4(a) => a,
            _ => unreachable!(),
        };
        let tcp = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
        let http_port = tcp.local_addr().unwrap().port();
        let igd = Arc::new(Mutex::new(MockIgd::new(behavior)));
        let location = format!("http://127.0.0.1:{http_port}/rootDesc.xml");
        tokio::spawn(async move {
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = ssdp.recv_from(&mut buf).await {
                if buf[..n].starts_with(b"M-SEARCH") {
                    let _ = ssdp.send_to(&ssdp_reply(&location), from).await;
                }
            }
        });
        let i = igd.clone();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = tcp.accept().await {
                let i = i.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut tmp = [0u8; 4096];
                    loop {
                        let Ok(n) = s.read(&mut tmp).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&tmp[..n]);
                        let text = String::from_utf8_lossy(&req).into_owned();
                        if let Some(h) = text.find("\r\n\r\n") {
                            let want = text[..h]
                                .lines()
                                .find_map(|l| l.strip_prefix("Content-Length: "))
                                .and_then(|v| v.trim().parse::<usize>().ok())
                                .unwrap_or(0);
                            if req.len() >= h + 4 + want {
                                break;
                            }
                        }
                    }
                    let resp = i
                        .lock()
                        .unwrap()
                        .handle_http(&req, &format!("127.0.0.1:{http_port}"));
                    let _ = s.write_all(&resp).await;
                    let _ = s.shutdown().await;
                });
            }
        });
        (ssdp_addr, igd)
    }

    /// Driver settings for a loopback gateway with short lifetimes.
    pub fn driver_cfg(gateway_port: u16, methods: &[Method], lifetime: u32) -> DriverConfig {
        DriverConfig {
            fixed_net: Some(NetInfo {
                gateway: LOOPBACK,
                local: LOOPBACK,
            }),
            core: Config {
                wanted: Wanted::tcp_and_udp(51413),
                methods: methods.to_vec(),
                gateway_port,
                lifetime_secs: lifetime,
                min_renew_gap: Duration::from_millis(50),
                seed: 7,
                ..Config::default()
            },
        }
    }

    /// Poll `f` until true or fail after `secs`.
    pub async fn wait_for(secs: u64, what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Drain events that are already queued.
    pub fn drain(rx: &mut tokio::sync::mpsc::Receiver<Event>) -> Vec<Event> {
        let mut v = Vec::new();
        while let Ok(e) = rx.try_recv() {
            v.push(e);
        }
        v
    }
}
