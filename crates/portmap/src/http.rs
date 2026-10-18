//! A deliberately tiny, lenient HTTP/1.x subset for talking to IGDs.
//!
//! Real routers send odd responses (bare `\n` line ends, missing or wrong
//! `Content-Length`, chunked bodies, trailing junk), so a strict general client
//! would fail on them. This module only builds requests and parses *responses*,
//! with hard size caps. The sockets live in the driver.

use std::net::{Ipv4Addr, SocketAddrV4};

/// Largest response (headers + body) the driver reads.
pub const MAX_RESPONSE: usize = 128 * 1024;
/// Longest URL we accept from a device.
pub const MAX_URL: usize = 512;

/// An `http://IPv4[:port]/path` URL. Hostnames are refused: resolving them would let
/// a LAN device steer us with DNS, and IGDs always advertise literal addresses.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Url {
    /// Host.
    pub ip: Ipv4Addr,
    /// Port (default 80).
    pub port: u16,
    /// Path and query, always starting with `/`.
    pub path: String,
}

impl Url {
    /// Socket address.
    pub fn addr(&self) -> SocketAddrV4 {
        SocketAddrV4::new(self.ip, self.port)
    }

    /// `Host:` header value.
    pub fn host_header(&self) -> String {
        if self.port == 80 {
            self.ip.to_string()
        } else {
            format!("{}:{}", self.ip, self.port)
        }
    }
}

fn clean(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_URL && s.bytes().all(|b| (0x21..0x7f).contains(&b))
}

/// Parse an absolute `http://a.b.c.d[:port][/path]` URL.
pub fn parse_url(s: &str) -> Option<Url> {
    let s = s.trim();
    if !clean(s) {
        return None;
    }
    let rest = s
        .get(..7)
        .filter(|p| p.eq_ignore_ascii_case("http://"))
        .and_then(|_| s.get(7..))?;
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_owned()),
        Some(i) => (&rest[..i], format!("/{}", &rest[i..])),
        None => (rest, "/".to_owned()),
    };
    if authority.contains('@') {
        return None;
    }
    let (host, port) = match authority.split_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().ok().filter(|&p| p != 0)?),
        None => (authority, 80),
    };
    let ip: Ipv4Addr = host.parse().ok()?;
    let path = path.split('#').next().unwrap_or("/").to_owned();
    Some(Url { ip, port, path })
}

/// Resolve a `controlURL`-style reference against the URL it came from (or a
/// `URLBase`): absolute URL, absolute path, or relative path.
pub fn resolve(base: &Url, reference: &str) -> Option<Url> {
    let r = reference.trim();
    if !clean(r) {
        return None;
    }
    if r.len() >= 7 && r[..7].eq_ignore_ascii_case("http://") {
        return parse_url(r);
    }
    if r.contains("://") || r.starts_with("//") {
        return None;
    }
    let path = if r.starts_with('/') {
        r.to_owned()
    } else {
        let dir = base.path.split(['?', '#']).next().unwrap_or("/");
        let dir = &dir[..dir.rfind('/').map_or(0, |i| i + 1)];
        format!("{}{}", if dir.is_empty() { "/" } else { dir }, r)
    };
    Some(Url {
        ip: base.ip,
        port: base.port,
        path,
    })
}

/// `GET` for a device description.
pub fn build_get(url: &Url, user_agent: &str) -> Vec<u8> {
    format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nAccept: text/xml, */*\r\nConnection: close\r\n\r\n",
        url.path,
        url.host_header(),
        user_agent
    )
    .into_bytes()
}

/// SOAP `POST`. `service_type` and `action` come from the device description and
/// from our own constants; the body is built by `upnp`.
pub fn build_soap_post(
    url: &Url,
    user_agent: &str,
    service_type: &str,
    action: &str,
    body: &str,
) -> Vec<u8> {
    format!(
        "POST {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {}\r\nContent-Type: text/xml; charset=\"utf-8\"\r\nSOAPAction: \"{}#{}\"\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        url.path,
        url.host_header(),
        user_agent,
        service_type,
        action,
        body.len(),
        body
    )
    .into_bytes()
}

/// A parsed response.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Response {
    /// Status code.
    pub status: u16,
    /// Decoded body.
    pub body: Vec<u8>,
}

/// Why a response could not be parsed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ParseError {
    /// No header terminator or no status line yet.
    Incomplete,
    /// Not HTTP.
    NotHttp,
    /// `Content-Length` promised more than arrived, or a chunk was cut short.
    Truncated,
    /// Body or headers exceed the caps.
    TooLarge,
    /// Bad chunk framing.
    BadChunk,
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Parse a complete (connection-closed) HTTP response.
pub fn parse_response(raw: &[u8], max_body: usize) -> Result<Response, ParseError> {
    if raw.len() > MAX_RESPONSE + 1024 {
        return Err(ParseError::TooLarge);
    }
    let (head_end, body_start) = match (find(raw, b"\r\n\r\n"), find(raw, b"\n\n")) {
        (Some(a), Some(b)) if b < a => (b, b + 2),
        (Some(a), _) => (a, a + 4),
        (None, Some(b)) => (b, b + 2),
        (None, None) => return Err(ParseError::Incomplete),
    };
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let mut lines = head.lines();
    let status_line = lines.next().ok_or(ParseError::NotHttp)?;
    let mut parts = status_line.split_ascii_whitespace();
    let version = parts.next().ok_or(ParseError::NotHttp)?;
    if !version.to_ascii_uppercase().starts_with("HTTP/1.") {
        return Err(ParseError::NotHttp);
    }
    let status: u16 = parts
        .next()
        .and_then(|s| s.parse().ok())
        .filter(|s| (100..600).contains(s))
        .ok_or(ParseError::NotHttp)?;
    let mut content_length: Option<usize> = None;
    let mut chunked = false;
    for line in lines.take(100) {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k.eq_ignore_ascii_case("content-length") {
            content_length = v.parse().ok();
        } else if k.eq_ignore_ascii_case("transfer-encoding")
            && v.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
    }
    let rest = &raw[body_start..];
    let body = if chunked {
        decode_chunked(rest, max_body)?
    } else if let Some(n) = content_length {
        if n > max_body {
            return Err(ParseError::TooLarge);
        }
        if rest.len() < n {
            return Err(ParseError::Truncated);
        }
        rest[..n].to_vec()
    } else {
        if rest.len() > max_body {
            return Err(ParseError::TooLarge);
        }
        rest.to_vec()
    };
    Ok(Response { status, body })
}

/// Has a complete response arrived already? Lets the driver stop reading without
/// waiting for EOF, since some routers ignore `Connection: close`.
pub fn is_complete(buf: &[u8]) -> bool {
    let body_start = match (find(buf, b"\r\n\r\n"), find(buf, b"\n\n")) {
        (Some(a), Some(b)) if b < a => b + 2,
        (Some(a), _) => a + 4,
        (None, Some(b)) => b + 2,
        (None, None) => return false,
    };
    let head = String::from_utf8_lossy(&buf[..body_start]).to_ascii_lowercase();
    let body = &buf[body_start..];
    if head.contains("transfer-encoding: chunked") {
        return body.ends_with(b"0\r\n\r\n") || body.ends_with(b"0\n\n");
    }
    head.lines()
        .find_map(|l| l.strip_prefix("content-length:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .is_some_and(|n| body.len() >= n)
}

fn decode_chunked(mut d: &[u8], max_body: usize) -> Result<Vec<u8>, ParseError> {
    let mut out = Vec::new();
    loop {
        let eol = find(d, b"\n").ok_or(ParseError::Truncated)?;
        let line = String::from_utf8_lossy(&d[..eol]);
        let size_str = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_str, 16).map_err(|_| ParseError::BadChunk)?;
        d = &d[eol + 1..];
        if size == 0 {
            return Ok(out);
        }
        if out.len().saturating_add(size) > max_body {
            return Err(ParseError::TooLarge);
        }
        if d.len() < size {
            return Err(ParseError::Truncated);
        }
        out.extend_from_slice(&d[..size]);
        d = &d[size..];
        // chunk terminator
        d = d
            .strip_prefix(b"\r\n")
            .or_else(|| d.strip_prefix(b"\n"))
            .unwrap_or(d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = parse_url("http://192.168.1.1:5000/rootDesc.xml").unwrap();
        assert_eq!(
            (u.ip, u.port, u.path.as_str()),
            (Ipv4Addr::new(192, 168, 1, 1), 5000, "/rootDesc.xml")
        );
        assert_eq!(u.host_header(), "192.168.1.1:5000");
        assert_eq!(parse_url("http://10.0.0.1").unwrap().path, "/");
        assert_eq!(parse_url("HTTP://10.0.0.1:80?x=1").unwrap().path, "/?x=1");
        assert_eq!(parse_url("http://10.0.0.1/a#frag").unwrap().path, "/a");
        for bad in [
            "",
            "https://10.0.0.1/",
            "http://router.local/",
            "http://u:p@10.0.0.1/",
            "http://10.0.0.1:0/",
            "http://10.0.0.1:99999/",
            "http://[::1]/",
            "ftp://10.0.0.1/",
            "http://10.0.0.1/a b",
            "http://10.0.0.1/\u{e9}",
        ] {
            assert!(parse_url(bad).is_none(), "{bad}");
        }
        assert!(parse_url(&format!("http://10.0.0.1/{}", "a".repeat(600))).is_none());
    }

    #[test]
    fn resolving_references() {
        let base = parse_url("http://192.168.1.1:5000/dev/desc.xml").unwrap();
        assert_eq!(resolve(&base, "/ctl/IPConn").unwrap().path, "/ctl/IPConn");
        assert_eq!(resolve(&base, "ctl").unwrap().path, "/dev/ctl");
        assert_eq!(
            resolve(&base, "http://192.168.1.1:49152/c").unwrap().port,
            49152
        );
        assert!(resolve(&base, "//evil/x").is_none());
        assert!(resolve(&base, "ftp://x/y").is_none());
        assert!(resolve(&base, "http://evil.example/x").is_none());
        assert!(resolve(&base, "").is_none());
    }

    #[test]
    fn request_shapes() {
        let u = parse_url("http://192.168.1.1:5000/c").unwrap();
        let get = String::from_utf8(build_get(&u, "UA")).unwrap();
        assert!(get.starts_with("GET /c HTTP/1.1\r\nHost: 192.168.1.1:5000\r\n"));
        assert!(get.ends_with("Connection: close\r\n\r\n"));
        let post = String::from_utf8(build_soap_post(&u, "UA", "urn:x:1", "Act", "<a/>")).unwrap();
        assert!(post.contains("SOAPAction: \"urn:x:1#Act\"\r\n"));
        assert!(post.contains("Content-Length: 4\r\n"));
        assert!(post.ends_with("\r\n\r\n<a/>"));
    }

    #[test]
    fn parses_common_and_sloppy_responses() {
        let r = parse_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA",
            100,
        )
        .unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"hello"[..]));
        let r = parse_response(b"HTTP/1.0 500 Err\r\n\r\nbody", 100).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (500, &b"body"[..]));
        // bare LF
        let r = parse_response(b"HTTP/1.1 200 OK\ncontent-length: 2\n\nhi", 100).unwrap();
        assert_eq!(r.body, b"hi");
        // chunked
        let r = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\n\r\n",
            100,
        )
        .unwrap();
        assert_eq!(r.body, b"abcde");
    }

    #[test]
    fn completeness() {
        assert!(!is_complete(b""));
        assert!(!is_complete(
            b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhe"
        ));
        assert!(is_complete(
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\n\r\nhello"
        ));
        assert!(!is_complete(
            b"HTTP/1.1 200 OK\r\n\r\nno length: wait for EOF"
        ));
        assert!(is_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n0\r\n\r\n"
        ));
        assert!(!is_complete(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n"
        ));
    }

    #[test]
    fn rejects_bad_responses_without_panicking() {
        assert_eq!(parse_response(b"", 10), Err(ParseError::Incomplete));
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\n", 10),
            Err(ParseError::Incomplete)
        );
        assert_eq!(
            parse_response(b"SSH-2.0\r\n\r\n", 10),
            Err(ParseError::NotHttp)
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 abc\r\n\r\n", 10),
            Err(ParseError::NotHttp)
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 99 x\r\n\r\n", 10),
            Err(ParseError::NotHttp)
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc", 100),
            Err(ParseError::Truncated)
        );
        assert_eq!(
            parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nabc", 100),
            Err(ParseError::TooLarge)
        );
        assert_eq!(
            parse_response(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n",
                100
            ),
            Err(ParseError::BadChunk)
        );
        assert_eq!(
            parse_response(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\n",
                100
            ),
            Err(ParseError::TooLarge)
        );
        assert_eq!(
            parse_response(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nab",
                100
            ),
            Err(ParseError::Truncated)
        );
        assert_eq!(
            parse_response(&vec![b'a'; MAX_RESPONSE + 2048], 10),
            Err(ParseError::TooLarge)
        );
        for len in 0..60 {
            let _ = parse_response(&vec![b'\n'; len], 10);
            let _ = parse_response(&b"HTTP/1.1 200 OK\r\n\r\n".repeat(len % 4 + 1), 10);
        }
    }
}
