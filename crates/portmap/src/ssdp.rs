//! SSDP discovery messages (UPnP Device Architecture 1.1, section 1).

use std::net::{Ipv4Addr, SocketAddrV4};

/// The SSDP multicast group and port.
pub const MULTICAST: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);
/// Search targets, in the order we send them. One datagram each.
pub const SEARCH_TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:InternetGatewayDevice:1",
    "urn:schemas-upnp-org:device:InternetGatewayDevice:2",
];
/// `MX` we advertise: devices answer within this many seconds.
pub const MX: u8 = 2;
/// Datagrams longer than this are not SSDP replies we care about.
pub const MAX_DATAGRAM: usize = 4096;

/// `M-SEARCH` for one search target.
pub fn build_search(target: &str, user_agent: &str) -> Vec<u8> {
    format!(
        "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: {MX}\r\nST: {target}\r\nUSER-AGENT: {user_agent}\r\n\r\n"
    )
    .into_bytes()
}

/// The useful parts of a search reply.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SearchReply {
    /// `LOCATION` header: where the device description lives.
    pub location: String,
    /// `ST` header.
    pub target: String,
}

/// Parse a unicast reply to `M-SEARCH`. Only `200 OK` replies that name a gateway
/// device or a WAN connection service are accepted.
pub fn parse_reply(d: &[u8]) -> Option<SearchReply> {
    if d.len() > MAX_DATAGRAM {
        return None;
    }
    let text = std::str::from_utf8(d).ok()?;
    let mut lines = text.lines();
    let status = lines.next()?;
    let mut parts = status.split_ascii_whitespace();
    if !parts.next()?.to_ascii_uppercase().starts_with("HTTP/1.") || parts.next()? != "200" {
        return None;
    }
    let (mut location, mut st, mut usn) = (None, None, None);
    for line in lines.take(40) {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        match k.trim().to_ascii_lowercase().as_str() {
            "location" => location = Some(v.to_owned()),
            "st" => st = Some(v.to_owned()),
            "usn" => usn = Some(v.to_owned()),
            _ => {}
        }
    }
    let target = st.or(usn)?;
    let t = target.to_ascii_lowercase();
    if !(t.contains("internetgatewaydevice")
        || t.contains("wanipconnection")
        || t.contains("wanpppconnection"))
    {
        return None;
    }
    Some(SearchReply {
        location: location?,
        target,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_message_shape() {
        let m = String::from_utf8(build_search(SEARCH_TARGETS[0], "Seedwell/1 UPnP/1.1")).unwrap();
        assert!(m.starts_with("M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\n"));
        assert!(m.contains("MAN: \"ssdp:discover\"\r\n"));
        assert!(m.contains("ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n"));
        assert!(m.ends_with("\r\n\r\n"));
    }

    #[test]
    fn parses_replies() {
        let ok = b"HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age=120\r\nlocation: http://192.168.1.1:5000/rootDesc.xml\r\nST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\nUSN: uuid:x::urn:y\r\n\r\n";
        let r = parse_reply(ok).unwrap();
        assert_eq!(r.location, "http://192.168.1.1:5000/rootDesc.xml");
        // wrong status, unrelated devices, missing location, junk
        assert!(parse_reply(b"HTTP/1.1 404 Not Found\r\nLOCATION: http://a/\r\nST: InternetGatewayDevice\r\n\r\n").is_none());
        assert!(parse_reply(b"HTTP/1.1 200 OK\r\nLOCATION: http://a/\r\nST: urn:dial-multiscreen-org:service:dial:1\r\n\r\n").is_none());
        assert!(parse_reply(b"HTTP/1.1 200 OK\r\nST: InternetGatewayDevice\r\n\r\n").is_none());
        assert!(parse_reply(b"NOTIFY * HTTP/1.1\r\n\r\n").is_none());
        assert!(parse_reply(&[0xff; 100]).is_none());
        assert!(parse_reply(&vec![b'a'; MAX_DATAGRAM + 1]).is_none());
        assert!(parse_reply(b"").is_none());
    }
}
