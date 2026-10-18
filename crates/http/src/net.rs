//! The HTTP side: one shared hyper client (keep-alive pool), a per-host
//! connection cap, manual redirect handling with the safety rules of SPEC 7D.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::body::Incoming;
use hyper::header::{ACCEPT, ACCEPT_ENCODING, IF_RANGE, LOCATION, RANGE, USER_AGENT};
use hyper::{Method, Request, Response, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::Error;

/// Networking limits, copied out of the public config.
#[derive(Clone, Debug)]
pub(crate) struct NetConfig {
    pub version: String,
    pub max_connections_per_host: usize,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub max_redirects: u32,
}

pub(crate) struct Net {
    client: Client<HttpsConnector<HttpConnector>, Empty<Bytes>>,
    user_agent: String,
    pub cfg: NetConfig,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
}

/// A response whose headers we have, plus the host permit that keeps the
/// per-host connection count honest until the body is dropped.
pub(crate) struct Opened {
    pub resp: Response<Incoming>,
    pub url: Uri,
    pub hops: u32,
    pub _permit: OwnedSemaphorePermit,
}

/// Where to read from: `[start, end)`; `end == None` means "to the end".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Piece {
    pub start: u64,
    pub end: Option<u64>,
}

impl Net {
    pub fn new(cfg: NetConfig) -> Self {
        let mut http = HttpConnector::new();
        http.enforce_http(false); // https is handled by the wrapping connector
        http.set_connect_timeout(Some(cfg.connect_timeout));
        http.set_nodelay(true);
        let https = HttpsConnectorBuilder::new()
            .with_webpki_roots()
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new())
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(cfg.max_connections_per_host.max(1))
            .build(https);
        // The version comes from the app; keep it header-safe.
        let version: String = cfg
            .version
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
            .collect();
        Net {
            client,
            user_agent: format!("Seedwell/{version}"),
            cfg,
            hosts: Mutex::new(HashMap::new()),
        }
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    fn host_semaphore(&self, uri: &Uri) -> Arc<Semaphore> {
        let key = host_key(uri);
        let mut map = match self.hosts.lock() {
            Ok(m) => m,
            Err(p) => p.into_inner(),
        };
        if map.len() > 256 {
            // Hosts nobody is using right now are cheap to forget.
            map.retain(|_, s| Arc::strong_count(s) > 1);
        }
        map.entry(key)
            .or_insert_with(|| Arc::new(Semaphore::new(self.cfg.max_connections_per_host.max(1))))
            .clone()
    }

    /// GET `start_url` (following redirects) with an optional byte range and
    /// `If-Range` validator. Returns the first non-redirect response.
    pub async fn open(
        &self,
        start_url: &str,
        range: Option<Piece>,
        if_range: Option<&str>,
    ) -> Result<Opened, Error> {
        let mut uri = parse_url(start_url)?;
        let mut visited: Vec<String> = vec![uri.to_string()];
        let mut hops = 0u32;
        loop {
            let sem = self.host_semaphore(&uri);
            let permit = sem
                .acquire_owned()
                .await
                .map_err(|_| Error::Network("connection gate closed".into()))?;
            let mut b = Request::builder()
                .method(Method::GET)
                .uri(uri.clone())
                .header(USER_AGENT, self.user_agent.as_str())
                .header(ACCEPT, "*/*")
                // Ranges address the stored representation; never let a
                // server compress behind our back.
                .header(ACCEPT_ENCODING, "identity");
            if let Some(p) = range {
                let v = match p.end {
                    Some(e) => format!("bytes={}-{}", p.start, e.saturating_sub(1)),
                    None => format!("bytes={}-", p.start),
                };
                b = b.header(RANGE, v);
                if let Some(ir) = if_range {
                    b = b.header(IF_RANGE, ir);
                }
            }
            let req = b
                .body(Empty::<Bytes>::new())
                .map_err(|e| Error::InvalidUrl(e.to_string()))?;
            let resp = match tokio::time::timeout(self.cfg.read_timeout, self.client.request(req)).await {
                Err(_) => return Err(Error::Timeout),
                Ok(Err(e)) => return Err(classify(&e)),
                Ok(Ok(r)) => r,
            };
            if resp.status().is_redirection() && resp.status().as_u16() != 304 {
                let loc = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                drop(resp);
                drop(permit);
                let Some(loc) = loc else {
                    return Err(Error::BadRedirect("redirect without Location".into()));
                };
                if hops >= self.cfg.max_redirects {
                    return Err(Error::TooManyRedirects);
                }
                hops += 1;
                let next = resolve_redirect(&uri, &loc)?;
                let key = next.to_string();
                if visited.contains(&key) {
                    return Err(Error::RedirectLoop);
                }
                visited.push(key);
                uri = next;
                continue;
            }
            return Ok(Opened {
                resp,
                url: uri,
                hops,
                _permit: permit,
            });
        }
    }
}

/// `scheme://host:port` lowercase, default ports filled in: the unit of
/// politeness.
fn host_key(uri: &Uri) -> String {
    let https = uri.scheme_str() == Some("https");
    let host = uri.host().unwrap_or("").to_ascii_lowercase();
    let port = uri.port_u16().unwrap_or(if https { 443 } else { 80 });
    format!("{host}:{port}")
}

/// Parse and vet a user-supplied URL: absolute http(s), a host, no
/// credentials (SPEC 12 #15: public links only in v1).
pub(crate) fn parse_url(s: &str) -> Result<Uri, Error> {
    let uri: Uri = s
        .trim()
        .parse()
        .map_err(|e: hyper::http::uri::InvalidUri| Error::InvalidUrl(e.to_string()))?;
    vet(&uri).map_err(Error::InvalidUrl)?;
    Ok(uri)
}

fn vet(uri: &Uri) -> Result<(), String> {
    match uri.scheme_str() {
        Some("http" | "https") => {}
        Some(other) => return Err(format!("unsupported scheme {other}")),
        None => return Err("missing scheme".into()),
    }
    let Some(auth) = uri.authority() else {
        return Err("missing host".into());
    };
    if auth.as_str().contains('@') {
        return Err("credentials in URLs are not supported".into());
    }
    if uri.host().is_none_or(str::is_empty) {
        return Err("missing host".into());
    }
    Ok(())
}

/// Resolve a `Location` against the URL that produced it, enforcing: only
/// http/https, no credentials, never https -> http.
pub(crate) fn resolve_redirect(base: &Uri, location: &str) -> Result<Uri, Error> {
    let loc = location.trim();
    let loc = loc.split('#').next().unwrap_or("");
    if loc.is_empty() {
        return Err(Error::BadRedirect("empty Location".into()));
    }
    let loc = loc.replace(' ', "%20");
    let scheme = base.scheme_str().unwrap_or("http");
    let authority = base.authority().map_or("", |a| a.as_str());
    let is_absolute = loc
        .split_once("://")
        .is_some_and(|(s, _)| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)));
    let full = if is_absolute {
        loc
    } else if let Some(rest) = loc.strip_prefix("//") {
        format!("{scheme}://{rest}")
    } else if loc.starts_with('/') {
        format!("{scheme}://{authority}{}", normalize_path(&loc))
    } else if loc.starts_with('?') {
        format!("{scheme}://{authority}{}{loc}", base.path())
    } else {
        let dir = base.path().rsplit_once('/').map_or("/", |(d, _)| d);
        format!("{scheme}://{authority}{}", normalize_path(&format!("{dir}/{loc}")))
    };
    let next: Uri = full
        .parse()
        .map_err(|e: hyper::http::uri::InvalidUri| Error::BadRedirect(e.to_string()))?;
    vet(&next).map_err(Error::BadRedirect)?;
    if base.scheme_str() == Some("https") && next.scheme_str() == Some("http") {
        return Err(Error::InsecureRedirect);
    }
    Ok(next)
}

/// Remove `.` and `..` segments from `path[?query]` (RFC 3986 5.2.4, simplified).
fn normalize_path(path_and_query: &str) -> String {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = path.split('/').collect();
    for (i, seg) in segs.iter().enumerate() {
        match *seg {
            "." => {}
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            s => {
                if s.is_empty() && i != 0 && i != segs.len() - 1 {
                    continue;
                }
                out.push(s);
            }
        }
    }
    let mut p = out.join("/");
    if matches!(segs.last(), Some(&"." | &"..")) && !p.ends_with('/') {
        p.push('/');
    }
    if !p.starts_with('/') {
        p.insert(0, '/');
    }
    match query {
        Some(q) => format!("{p}?{q}"),
        None => p,
    }
}

/// Map a hyper client error by walking its source chain (no brittle downcasts
/// to private types).
fn classify(e: &hyper_util::client::legacy::Error) -> Error {
    let mut text = String::new();
    let mut timed_out = false;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        text.push_str(&err.to_string().to_ascii_lowercase());
        text.push('|');
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::TimedOut
        {
            timed_out = true;
        }
        cur = err.source();
    }
    if timed_out || text.contains("timed out") {
        Error::Timeout
    } else if text.contains("certificate") || text.contains("invalidcert") || text.contains("tls") {
        // A handshake that was merely cut off is retryable; a verification
        // failure is not.
        if text.contains("certificate") || text.contains("invalidcert") {
            Error::Tls(text.trim_end_matches('|').to_owned())
        } else {
            Error::Network(text.trim_end_matches('|').to_owned())
        }
    } else {
        Error::Network(text.trim_end_matches('|').to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> Uri {
        s.parse().unwrap()
    }

    #[test]
    fn url_vetting() {
        assert!(parse_url("https://example.com/a").is_ok());
        assert!(parse_url("http://example.com:8080/a?b").is_ok());
        assert!(matches!(parse_url("ftp://example.com/a"), Err(Error::InvalidUrl(_))));
        assert!(matches!(parse_url("https://user:pw@example.com/a"), Err(Error::InvalidUrl(_))));
        assert!(matches!(parse_url("/relative"), Err(Error::InvalidUrl(_))));
        assert!(matches!(parse_url("not a url"), Err(Error::InvalidUrl(_))));
        assert!(matches!(parse_url(""), Err(Error::InvalidUrl(_))));
    }

    #[test]
    fn redirect_resolution() {
        let b = u("https://h.example/a/b/c.iso?x=1");
        let r = |l| resolve_redirect(&b, l).unwrap().to_string();
        assert_eq!(r("https://other/z"), "https://other/z");
        assert_eq!(r("//cdn/z"), "https://cdn/z");
        assert_eq!(r("/root/z?q"), "https://h.example/root/z?q");
        assert_eq!(r("d.iso"), "https://h.example/a/b/d.iso");
        assert_eq!(r("../d.iso"), "https://h.example/a/d.iso");
        assert_eq!(r("../../../../d.iso"), "https://h.example/d.iso");
        assert_eq!(r("?y=2"), "https://h.example/a/b/c.iso?y=2");
        assert_eq!(r("/p#frag"), "https://h.example/p");
        assert_eq!(r("/a b"), "https://h.example/a%20b");
    }

    #[test]
    fn redirect_safety() {
        let https = u("https://h.example/a");
        let http = u("http://h.example/a");
        assert_eq!(resolve_redirect(&https, "http://h.example/b"), Err(Error::InsecureRedirect));
        assert_eq!(resolve_redirect(&https, "http://evil/b"), Err(Error::InsecureRedirect));
        assert!(resolve_redirect(&http, "https://h.example/b").is_ok());
        assert!(resolve_redirect(&http, "http://other/b").is_ok());
        assert!(matches!(resolve_redirect(&https, "ftp://h/b"), Err(Error::BadRedirect(_))));
        assert!(matches!(resolve_redirect(&https, "file:///etc/passwd"), Err(Error::BadRedirect(_))));
        assert!(matches!(resolve_redirect(&https, "https://u:p@h/b"), Err(Error::BadRedirect(_))));
        assert!(matches!(resolve_redirect(&https, ""), Err(Error::BadRedirect(_))));
        assert!(matches!(resolve_redirect(&https, "https://"), Err(Error::BadRedirect(_))));
    }

    #[test]
    fn host_keys_normalize() {
        assert_eq!(host_key(&u("https://Example.COM/x")), "example.com:443");
        assert_eq!(host_key(&u("http://example.com/x")), "example.com:80");
        assert_eq!(host_key(&u("http://example.com:81/x")), "example.com:81");
    }
}
