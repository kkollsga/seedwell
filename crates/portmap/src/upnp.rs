//! UPnP IGD: device description parsing and SOAP bodies/replies for
//! `WANIPConnection:1/2` and `WANPPPConnection:1`.

use std::net::Ipv4Addr;

use crate::Protocol;
use crate::xml::{self, Event};

/// Most services we remember from a description.
pub const MAX_SERVICES: usize = 32;

/// One `<service>` of a device description.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Service {
    /// `serviceType` URN.
    pub service_type: String,
    /// `controlURL`, as written (absolute or relative).
    pub control_url: String,
}

/// What we need from a device description.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Description {
    /// `URLBase`, if the device has one.
    pub url_base: Option<String>,
    /// All services found anywhere in the device tree.
    pub services: Vec<Service>,
}

/// Parse a device description.
pub fn parse_description(xml_text: &str) -> Result<Description, xml::XmlError> {
    let mut d = Description::default();
    let mut stack: Vec<&str> = Vec::new();
    let (mut st, mut cu): (Option<String>, Option<String>) = (None, None);
    xml::walk(xml_text, |e| match e {
        Event::Open(n) => {
            if n == "service" {
                st = None;
                cu = None;
            }
            stack.push(n);
        }
        Event::Close(n) => {
            if n == "service" {
                if let (Some(service_type), Some(control_url)) = (st.take(), cu.take()) {
                    if d.services.len() < MAX_SERVICES {
                        d.services.push(Service {
                            service_type,
                            control_url,
                        });
                    }
                }
            }
            stack.pop();
        }
        Event::Text(t) => {
            let n = stack.len();
            let top = stack.last().copied();
            let parent = n.checked_sub(2).and_then(|i| stack.get(i)).copied();
            match (parent, top) {
                (Some("service"), Some("serviceType")) => st = Some(t),
                (Some("service"), Some("controlURL")) => cu = Some(t),
                (Some(_), Some("URLBase")) if d.url_base.is_none() => d.url_base = Some(t),
                _ => {}
            }
        }
    })?;
    Ok(d)
}

/// The three WAN connection services we can drive.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ServiceKind {
    /// `WANPPPConnection:1`
    WanPpp1,
    /// `WANIPConnection:1`
    WanIp1,
    /// `WANIPConnection:2`
    WanIp2,
}

/// Classify a `serviceType`. Only exact, plain URNs are accepted, so the value can be
/// put in a header safely.
pub fn classify(service_type: &str) -> Option<ServiceKind> {
    match service_type {
        "urn:schemas-upnp-org:service:WANIPConnection:2" => Some(ServiceKind::WanIp2),
        "urn:schemas-upnp-org:service:WANIPConnection:1" => Some(ServiceKind::WanIp1),
        "urn:schemas-upnp-org:service:WANPPPConnection:1" => Some(ServiceKind::WanPpp1),
        _ => None,
    }
}

/// Best usable service: IP:2, then IP:1, then PPP:1.
pub fn pick_service(d: &Description) -> Option<(&Service, ServiceKind)> {
    d.services
        .iter()
        .filter_map(|s| classify(&s.service_type).map(|k| (s, k)))
        .max_by_key(|&(_, k)| k)
}

fn envelope(service_type: &str, action: &str, args: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\r\n<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{}\">{args}</u:{action}></s:Body></s:Envelope>\r\n",
        xml::escape(service_type)
    )
}

/// `GetExternalIPAddress` body.
pub fn external_ip_body(service_type: &str) -> String {
    envelope(service_type, "GetExternalIPAddress", "")
}

/// `AddPortMapping` body. `lease == 0` asks for a permanent mapping.
pub fn add_mapping_body(
    service_type: &str,
    protocol: Protocol,
    external_port: u16,
    internal_port: u16,
    internal_client: Ipv4Addr,
    description: &str,
    lease: u32,
) -> String {
    let args = format!(
        "<NewRemoteHost></NewRemoteHost><NewExternalPort>{external_port}</NewExternalPort><NewProtocol>{}</NewProtocol><NewInternalPort>{internal_port}</NewInternalPort><NewInternalClient>{internal_client}</NewInternalClient><NewEnabled>1</NewEnabled><NewPortMappingDescription>{}</NewPortMappingDescription><NewLeaseDuration>{lease}</NewLeaseDuration>",
        protocol.name(),
        xml::escape(description)
    );
    envelope(service_type, "AddPortMapping", &args)
}

/// `DeletePortMapping` body.
pub fn delete_mapping_body(service_type: &str, protocol: Protocol, external_port: u16) -> String {
    let args = format!(
        "<NewRemoteHost></NewRemoteHost><NewExternalPort>{external_port}</NewExternalPort><NewProtocol>{}</NewProtocol>",
        protocol.name()
    );
    envelope(service_type, "DeletePortMapping", &args)
}

/// Outcome of a SOAP call.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SoapReply {
    /// 2xx. `external_ip` is set for `GetExternalIPAddress`.
    Ok {
        /// `NewExternalIPAddress`, when present and a valid IPv4 address.
        external_ip: Option<Ipv4Addr>,
    },
    /// A SOAP fault or other HTTP failure.
    Fault {
        /// UPnP `errorCode` (for example 718, 725), when the device sent one.
        code: Option<u32>,
        /// HTTP status.
        status: u16,
    },
}

/// Interpret an HTTP reply to a SOAP request. Never fails: unparseable bodies of a
/// 2xx reply still count as success (some routers answer with an empty body).
pub fn parse_reply(status: u16, body: &[u8]) -> SoapReply {
    let text = String::from_utf8_lossy(body);
    let mut stack: Vec<&str> = Vec::new();
    let (mut ip, mut code, mut fault) = (None, None, false);
    // An XML error just ends the scan; what was gathered so far is still used.
    let _ = xml::walk(&text, |e| match e {
        Event::Open(n) => {
            fault |= n == "Fault" || n == "UPnPError";
            stack.push(n);
        }
        Event::Close(_) => {
            stack.pop();
        }
        Event::Text(t) => match stack.last().copied() {
            Some("NewExternalIPAddress") if ip.is_none() => ip = t.parse::<Ipv4Addr>().ok(),
            Some("errorCode") if code.is_none() => code = t.parse::<u32>().ok(),
            _ => {}
        },
    });
    if (200..300).contains(&status) && !fault {
        SoapReply::Ok { external_ip: ip }
    } else {
        SoapReply::Fault { code, status }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DESC: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><specVersion><major>1</major></specVersion>
<URLBase>http://192.168.1.1:5000/</URLBase>
<device><deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
 <serviceList><service><serviceType>urn:schemas-upnp-org:service:Layer3Forwarding:1</serviceType><controlURL>/ctl/L3F</controlURL></service></serviceList>
 <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANDevice:1</deviceType>
  <deviceList><device><deviceType>urn:schemas-upnp-org:device:WANConnectionDevice:1</deviceType>
   <serviceList>
    <service><serviceType>urn:schemas-upnp-org:service:WANPPPConnection:1</serviceType><controlURL>/ctl/PPP</controlURL></service>
    <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/ctl/IP1</controlURL></service>
    <service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:2</serviceType><controlURL>/ctl/IP2?a=1&amp;b=2</controlURL></service>
   </serviceList></device></deviceList></device></deviceList></device></root>"#;

    #[test]
    fn description_and_service_choice() {
        let d = parse_description(DESC).unwrap();
        assert_eq!(d.url_base.as_deref(), Some("http://192.168.1.1:5000/"));
        assert_eq!(d.services.len(), 4);
        let (s, k) = pick_service(&d).unwrap();
        assert_eq!(k, ServiceKind::WanIp2);
        assert_eq!(s.control_url, "/ctl/IP2?a=1&b=2");
        // without v2 the v1 IP service beats PPP
        let d1 = Description {
            url_base: None,
            services: d.services[..3].to_vec(),
        };
        assert_eq!(pick_service(&d1).unwrap().1, ServiceKind::WanIp1);
        assert!(pick_service(&Description::default()).is_none());
        // hostile service types are never used
        let evil = Description {
            url_base: None,
            services: vec![Service {
                service_type: "urn:schemas-upnp-org:service:WANIPConnection:1\r\nX: y".into(),
                control_url: "/c".into(),
            }],
        };
        assert!(pick_service(&evil).is_none());
    }

    #[test]
    fn bodies() {
        let b = add_mapping_body(
            "urn:schemas-upnp-org:service:WANIPConnection:1",
            Protocol::Tcp,
            40000,
            6881,
            Ipv4Addr::new(192, 168, 1, 20),
            "Torrent<RS>",
            7200,
        );
        assert!(b.contains(
            "<u:AddPortMapping xmlns:u=\"urn:schemas-upnp-org:service:WANIPConnection:1\">"
        ));
        assert!(b.contains("<NewExternalPort>40000</NewExternalPort><NewProtocol>TCP</NewProtocol><NewInternalPort>6881</NewInternalPort><NewInternalClient>192.168.1.20</NewInternalClient>"));
        assert!(b.contains("Torrent&lt;RS&gt;"));
        assert!(b.contains("<NewLeaseDuration>7200</NewLeaseDuration>"));
        // our own bodies tokenize cleanly
        assert!(crate::xml::walk(&b, |_| {}).is_ok());
        let d = delete_mapping_body("urn:x", Protocol::Udp, 51413);
        assert!(d.contains("<NewProtocol>UDP</NewProtocol>") && d.contains("DeletePortMapping"));
        assert!(
            external_ip_body("urn:x")
                .contains("<u:GetExternalIPAddress xmlns:u=\"urn:x\"></u:GetExternalIPAddress>")
        );
    }

    #[test]
    fn replies() {
        let ok = br#"<?xml version="1.0"?><s:Envelope xmlns:s="x"><s:Body><u:GetExternalIPAddressResponse xmlns:u="y"><NewExternalIPAddress>203.0.113.9</NewExternalIPAddress></u:GetExternalIPAddressResponse></s:Body></s:Envelope>"#;
        assert_eq!(
            parse_reply(200, ok),
            SoapReply::Ok {
                external_ip: Some(Ipv4Addr::new(203, 0, 113, 9))
            }
        );
        assert_eq!(parse_reply(200, b""), SoapReply::Ok { external_ip: None });
        assert_eq!(
            parse_reply(200, b"<<<garbage"),
            SoapReply::Ok { external_ip: None }
        );
        let fault = br#"<s:Envelope xmlns:s="x"><s:Body><s:Fault><faultcode>s:Client</faultcode><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>718</errorCode><errorDescription>ConflictInMappingEntry</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;
        assert_eq!(
            parse_reply(500, fault),
            SoapReply::Fault {
                code: Some(718),
                status: 500
            }
        );
        // a fault inside a 200 is still a fault
        assert_eq!(
            parse_reply(200, fault),
            SoapReply::Fault {
                code: Some(718),
                status: 200
            }
        );
        assert_eq!(
            parse_reply(404, b"<html>nope</html>"),
            SoapReply::Fault {
                code: None,
                status: 404
            }
        );
        assert_eq!(
            parse_reply(500, &[0xff, 0xfe]),
            SoapReply::Fault {
                code: None,
                status: 500
            }
        );
        // invalid ip text is not an address
        assert_eq!(
            parse_reply(
                200,
                b"<NewExternalIPAddress>not an ip</NewExternalIPAddress>"
            ),
            SoapReply::Ok { external_ip: None }
        );
    }

    #[test]
    fn caps_services() {
        let one = "<service><serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType><controlURL>/c</controlURL></service>";
        let d = parse_description(&format!("<root>{}</root>", one.repeat(100))).unwrap();
        assert_eq!(d.services.len(), MAX_SERVICES);
    }
}
