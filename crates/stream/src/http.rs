//! Minimal HTTP/1.1 request parsing, `Range` resolution and response heads.
//!
//! Hand-rolled on purpose: the server needs to watch the socket for a client hang-up while it is
//! parked waiting for a piece, which is awkward to do through a general HTTP stack, and the
//! protocol surface is tiny (GET/HEAD, keep-alive, one range).

pub const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_HEADERS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    /// Request target without the query string.
    pub path: String,
    pub http11: bool,
    pub range: Option<String>,
    pub if_range: Option<String>,
    pub host: Option<String>,
    /// `Connection: close` seen.
    pub close: bool,
    /// `Connection: keep-alive` seen (HTTP/1.0 clients).
    pub keep_alive: bool,
    /// Declared body length (we skip it; GET/HEAD should have none).
    pub content_length: u64,
}

impl Request {
    pub fn wants_keep_alive(&self) -> bool {
        !self.close && (self.http11 || self.keep_alive)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    Bad,
    TooLarge,
    Version,
}

/// Parse one request from the front of `buf`. `Ok(None)` = need more bytes. On success returns
/// the request and the number of bytes consumed (head only).
pub fn parse_request(buf: &[u8]) -> Result<Option<(Request, usize)>, ParseError> {
    let Some(end) = find_head_end(buf) else {
        return if buf.len() >= MAX_HEADER_BYTES { Err(ParseError::TooLarge) } else { Ok(None) };
    };
    if end > MAX_HEADER_BYTES {
        return Err(ParseError::TooLarge);
    }
    let head = std::str::from_utf8(&buf[..end - 4]).map_err(|_| ParseError::Bad)?;
    let mut lines = head.split("\r\n");
    let line = lines.next().ok_or(ParseError::Bad)?;
    let mut parts = line.split(' ');
    let (m, target, ver) = (parts.next(), parts.next(), parts.next());
    let (Some(m), Some(target), Some(ver), None) = (m, target, ver, parts.next()) else { return Err(ParseError::Bad) };
    let http11 = match ver {
        "HTTP/1.1" => true,
        "HTTP/1.0" => false,
        _ => return Err(ParseError::Version),
    };
    let method = match m {
        "GET" => Method::Get,
        "HEAD" => Method::Head,
        _ => Method::Other,
    };
    let path = target.split(['?', '#']).next().unwrap_or("").to_string();
    let mut r = Request {
        method,
        path,
        http11,
        range: None,
        if_range: None,
        host: None,
        close: false,
        keep_alive: false,
        content_length: 0,
    };
    for (n, l) in lines.enumerate() {
        if n >= MAX_HEADERS {
            return Err(ParseError::TooLarge);
        }
        let (name, value) = l.split_once(':').ok_or(ParseError::Bad)?;
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "range" => r.range = Some(value.to_string()),
            "if-range" => r.if_range = Some(value.to_string()),
            "host" => r.host = Some(value.to_string()),
            "connection" => {
                for t in value.split(',') {
                    match t.trim().to_ascii_lowercase().as_str() {
                        "close" => r.close = true,
                        "keep-alive" => r.keep_alive = true,
                        _ => {}
                    }
                }
            }
            "content-length" => r.content_length = value.parse().map_err(|_| ParseError::Bad)?,
            "transfer-encoding" => return Err(ParseError::Bad),
            _ => {}
        }
    }
    Ok(Some((r, end)))
}

fn find_head_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Is a `Host:` header value a loopback authority (`127.0.0.1[:p]`, `localhost[:p]`, `[::1][:p]`)?
/// Guards against DNS-rebinding pages talking to the server through a hostname.
pub fn host_is_loopback(host: &str) -> bool {
    let h = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.rsplit_once(':').map_or(host, |(a, _)| a)
    };
    h == "127.0.0.1" || h.eq_ignore_ascii_case("localhost") || h == "::1"
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeSpec {
    /// No usable `Range`: 200 with the whole file.
    Full,
    /// 206, `start..=end` (both inside the file).
    Partial { start: u64, end: u64 },
    /// 416.
    Unsatisfiable,
}

/// Resolve a `Range` header against a file of `len` bytes (RFC 9110 14.1.2):
/// `bytes=N-`, `bytes=N-M` (M clamped), `bytes=-N` (last N bytes; N > len gives the whole file).
/// With several ranges the first satisfiable one is served. Unknown units and syntactically
/// invalid specs are ignored (the whole file is sent).
pub fn resolve_range(header: Option<&str>, len: u64) -> RangeSpec {
    let Some(h) = header else { return RangeSpec::Full };
    let Some(specs) = h.trim().strip_prefix("bytes=").or_else(|| h.trim().strip_prefix("Bytes=")) else {
        return RangeSpec::Full;
    };
    let mut any_valid = false;
    for spec in specs.split(',') {
        let spec = spec.trim();
        let Some((a, b)) = spec.split_once('-') else { return RangeSpec::Full };
        let (a, b) = (a.trim(), b.trim());
        let one = match (a.is_empty(), b.is_empty()) {
            (true, true) => return RangeSpec::Full,
            (true, false) => {
                let Ok(n) = b.parse::<u64>() else { return RangeSpec::Full };
                any_valid = true;
                if n == 0 || len == 0 { None } else { Some((len.saturating_sub(n), len - 1)) }
            }
            (false, bempty) => {
                let Ok(s) = a.parse::<u64>() else { return RangeSpec::Full };
                let e = if bempty {
                    u64::MAX
                } else {
                    match b.parse::<u64>() {
                        Ok(e) if e >= s => e,
                        _ => return RangeSpec::Full, // last < first is invalid syntax: ignore header
                    }
                };
                any_valid = true;
                if s >= len { None } else { Some((s, e.min(len - 1))) }
            }
        };
        if let Some((start, end)) = one {
            return RangeSpec::Partial { start, end };
        }
    }
    if any_valid { RangeSpec::Unsatisfiable } else { RangeSpec::Full }
}

pub fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

/// Response head builder.
pub struct Head {
    buf: String,
}

impl Head {
    pub fn new(status: u16) -> Self {
        Self { buf: format!("HTTP/1.1 {status} {}\r\n", reason(status)) }
    }
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.buf.push_str(name);
        self.buf.push_str(": ");
        self.buf.push_str(value);
        self.buf.push_str("\r\n");
        self
    }
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.push_str("\r\n");
        self.buf.into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let r = |h: &str, len| resolve_range(Some(h), len);
        assert_eq!(resolve_range(None, 100), RangeSpec::Full);
        assert_eq!(r("bytes=0-", 100), RangeSpec::Partial { start: 0, end: 99 });
        assert_eq!(r("bytes=10-19", 100), RangeSpec::Partial { start: 10, end: 19 });
        assert_eq!(r("bytes=90-500", 100), RangeSpec::Partial { start: 90, end: 99 });
        assert_eq!(r("bytes=-10", 100), RangeSpec::Partial { start: 90, end: 99 });
        assert_eq!(r("bytes=-500", 100), RangeSpec::Partial { start: 0, end: 99 });
        assert_eq!(r("bytes=0-1", 100), RangeSpec::Partial { start: 0, end: 1 });
        assert_eq!(r("bytes=100-", 100), RangeSpec::Unsatisfiable);
        assert_eq!(r("bytes=100-200", 100), RangeSpec::Unsatisfiable);
        assert_eq!(r("bytes=-0", 100), RangeSpec::Unsatisfiable);
        assert_eq!(r("bytes=0-", 0), RangeSpec::Unsatisfiable);
        // multiple: first satisfiable wins
        assert_eq!(r("bytes=0-4, 10-14", 100), RangeSpec::Partial { start: 0, end: 4 });
        assert_eq!(r("bytes=200-300, 10-14", 100), RangeSpec::Partial { start: 10, end: 14 });
        assert_eq!(r("bytes=200-300, 400-", 100), RangeSpec::Unsatisfiable);
        // ignored
        assert_eq!(r("bytes=5-2", 100), RangeSpec::Full);
        assert_eq!(r("items=0-5", 100), RangeSpec::Full);
        assert_eq!(r("bytes=abc-", 100), RangeSpec::Full);
        assert_eq!(r("bytes=-", 100), RangeSpec::Full);
        assert_eq!(r("bytes=", 100), RangeSpec::Full);
        // > 4 GiB
        let big = 6 * (1u64 << 30);
        assert_eq!(r("bytes=5368709120-", big), RangeSpec::Partial { start: 5 * (1 << 30), end: big - 1 });
    }

    #[test]
    fn parse() {
        let raw = b"GET /tok/a%20b.mkv?x=1 HTTP/1.1\r\nHost: 127.0.0.1:1234\r\nRange: bytes=5-\r\nConnection: keep-alive\r\n\r\nGET /next";
        let (r, n) = parse_request(raw).unwrap().unwrap();
        assert_eq!(r.method, Method::Get);
        assert_eq!(r.path, "/tok/a%20b.mkv");
        assert_eq!(r.range.as_deref(), Some("bytes=5-"));
        assert!(r.wants_keep_alive());
        assert_eq!(&raw[n..], b"GET /next");
        assert!(parse_request(b"GET / HTTP/1.1\r\nHost: x").unwrap().is_none());
        assert_eq!(parse_request(b"GET /\r\n\r\n"), Err(ParseError::Bad));
        assert_eq!(parse_request(b"GET / HTTP/2\r\n\r\n"), Err(ParseError::Version));
        assert_eq!(parse_request(b"GET / HTTP/1.1\r\nbadheader\r\n\r\n"), Err(ParseError::Bad));
        let (r, _) = parse_request(b"HEAD / HTTP/1.0\r\n\r\n").unwrap().unwrap();
        assert_eq!(r.method, Method::Head);
        assert!(!r.wants_keep_alive());
        let (r, _) = parse_request(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap().unwrap();
        assert!(!r.wants_keep_alive());
        let huge = vec![b'a'; MAX_HEADER_BYTES + 10];
        assert_eq!(parse_request(&huge), Err(ParseError::TooLarge));
    }

    #[test]
    fn hosts() {
        assert!(host_is_loopback("127.0.0.1:5000"));
        assert!(host_is_loopback("localhost"));
        assert!(host_is_loopback("[::1]:80"));
        assert!(!host_is_loopback("evil.example.com:80"));
        assert!(!host_is_loopback("192.168.1.5"));
    }
}
