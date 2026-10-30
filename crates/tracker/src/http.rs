//! HTTP(S) tracker transport: hyper 1.x + rustls (webpki roots, full
//! certificate verification, no insecure fallback), gzip, one shared
//! keep-alive pool, at most 3 redirects, 15 s connect and 30 s total timeouts.
//!
//! Destinations are vetted by [`DestinationPolicy`]: the name is resolved by our
//! own resolver, which drops forbidden addresses (loopback, link-local, cloud
//! metadata, private networks unless opted in), and the connector then connects
//! to exactly the addresses that passed, so a DNS answer cannot change between
//! check and connect. IP-literal hosts are checked before every request,
//! including each redirect hop.
//!
//! Dual-stack hosts: hyper's connector races the addresses of a name only for the
//! TCP connect. A host whose first address (usually IPv6) accepts the connection
//! and then resets it, or never answers, used to leave the announce stuck with
//! that family. Within ONE released request, a failure after connecting and
//! before any response (reset, EOF, no answer within `address_timeout`) is
//! retried once on an address of the other family, in the remaining time. That
//! hop is invisible to the gate (one announce for the token bucket and the
//! backoff). It is never taken for `completed` (the machine's at-most-once rule:
//! the first send may have been counted) and the family that worked is
//! remembered per host for `family_memory`. See [`HttpConfig`].
//!
//! It only accepts a `&GatedRequest`, so nothing but the `HostGate` can cause
//! a request. The User-Agent is `Seedwell/<version>` with the version passed
//! in by the caller; there is deliberately no way to set another value
//! (SPEC 7B.1: never spoof another client).

use std::collections::HashMap;
use std::future::Future;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, Limited};
use hyper::body::Incoming;
use hyper::header::{
    ACCEPT, ACCEPT_ENCODING, CONTENT_ENCODING, HOST, LOCATION, RETRY_AFTER, USER_AGENT,
};
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::client::legacy::connect::{
    CaptureConnection, HttpConnector, HttpInfo, capture_connection,
};
use hyper_util::rt::TokioExecutor;
use tower_service::Service;

use crate::gate::GatedRequest;
use crate::policy::{DestinationPolicy, Refusal};
use crate::response::{
    AnnounceError, AnnounceResponse, MAX_BODY, ParseError, TransportError, parse_announce_response,
};
use crate::retry_after::parse_retry_after;
use crate::scrape::{ScrapeEntry, parse_scrape_response, scrape_url};
use crate::types::Event;
use crate::types::InfoHash20;
use crate::url::{Scheme, TrackerUrl, build_announce_url};

/// Most redirects followed.
pub const MAX_REDIRECTS: u32 = 3;

/// Timeouts (SPEC 7C.1).
#[derive(Clone, Copy, Debug)]
pub struct HttpConfig {
    /// TCP connect timeout (15 s).
    pub connect_timeout: Duration,
    /// Whole request including redirects and body (30 s).
    pub total_timeout: Duration,
    /// Idle connections are dropped after this.
    pub pool_idle_timeout: Duration,
    /// Connect plus time to the response head for the FIRST address family of
    /// a dual-stack host (15 s). If it expires after the connection was made,
    /// or the connection is reset or closed without a response, the request is
    /// retried once on the other family in the rest of `total_timeout`. A host
    /// with one family gets the whole `total_timeout`.
    pub address_timeout: Duration,
    /// How long the family that worked after such a fallback is tried first
    /// for that host (1 h). It is not refreshed by later successes, so the
    /// natural order is probed again once an hour.
    pub family_memory: Duration,
    /// Where we may connect. The default refuses loopback, link-local, cloud
    /// metadata and private networks; the engine sets
    /// `policy.allow_lan_trackers` from the user's opt-in, tests use
    /// [`DestinationPolicy::LOCAL`].
    pub policy: DestinationPolicy,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            connect_timeout: Duration::from_secs(15),
            total_timeout: Duration::from_secs(30),
            pool_idle_timeout: Duration::from_secs(60),
            address_timeout: Duration::from_secs(15),
            family_memory: Duration::from_secs(3600),
            policy: DestinationPolicy::STRICT,
        }
    }
}

/// Why our resolver said no.
#[derive(Debug)]
enum ResolveError {
    /// The lookup failed or returned nothing.
    Dns(std::io::Error),
    /// The lookup took too long.
    Timeout,
    /// Every resolved address is forbidden by the policy.
    Forbidden(Refusal),
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::Dns(e) => write!(f, "dns error: {e}"),
            ResolveError::Timeout => f.write_str("dns error: lookup timed out"),
            ResolveError::Forbidden(r) => write!(f, "destination not allowed: {r}"),
        }
    }
}
impl std::error::Error for ResolveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ResolveError::Dns(e) => Some(e),
            _ => None,
        }
    }
}

/// `getaddrinfo` plus the destination policy. The connector connects to the
/// addresses returned here and to nothing else.
#[derive(Clone)]
struct VettingResolver {
    policy: DestinationPolicy,
    timeout: Duration,
    /// Fixed name table instead of DNS (tests, embedding); the policy still applies.
    hosts: Option<Arc<HashMap<String, Vec<IpAddr>>>>,
}

impl VettingResolver {
    async fn resolve(&self, host: &str) -> Result<Vec<SocketAddr>, ResolveError> {
        let addrs: Vec<SocketAddr> = match self
            .hosts
            .as_ref()
            .and_then(|h| h.get(&host.to_ascii_lowercase()))
        {
            Some(ips) => ips.iter().map(|ip| SocketAddr::new(*ip, 0)).collect(),
            None => {
                let lookup = tokio::net::lookup_host((host, 0));
                match tokio::time::timeout(self.timeout, lookup).await {
                    Err(_) => return Err(ResolveError::Timeout),
                    Ok(Err(e)) => return Err(ResolveError::Dns(e)),
                    Ok(Ok(it)) => it.collect(),
                }
            }
        };
        if addrs.is_empty() {
            return Err(ResolveError::Dns(std::io::Error::other("no addresses")));
        }
        self.policy.vet(addrs).map_err(ResolveError::Forbidden)
    }
}

impl Service<Name> for VettingResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = ResolveError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, ResolveError>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), ResolveError>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let this = self.clone();
        Box::pin(async move { this.resolve(name.as_str()).await.map(Vec::into_iter) })
    }
}

/// An address family.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Family {
    V4,
    V6,
}

impl Family {
    fn of(ip: IpAddr) -> Family {
        if ip.to_canonical().is_ipv4() {
            Family::V4
        } else {
            Family::V6
        }
    }

    fn other(self) -> Family {
        match self {
            Family::V4 => Family::V6,
            Family::V6 => Family::V4,
        }
    }
}

type Conn = HttpsConnector<HttpConnector<VettingResolver>>;
type HClient = Client<Conn, Empty<Bytes>>;

/// A client whose connector uses addresses of `family` only (`None`: both,
/// raced by hyper's connector). hyper-util restricts the candidates to the
/// family of the configured local address.
fn build_client(cfg: &HttpConfig, resolver: &VettingResolver, family: Option<Family>) -> HClient {
    let mut http = HttpConnector::new_with_resolver(resolver.clone());
    http.enforce_http(false); // https is handled by the wrapping connector
    http.set_connect_timeout(Some(cfg.connect_timeout));
    http.set_nodelay(true);
    match family {
        Some(Family::V4) => http.set_local_address(Some(IpAddr::V4(Ipv4Addr::UNSPECIFIED))),
        Some(Family::V6) => http.set_local_address(Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED))),
        None => {}
    }
    let https = HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .wrap_connector(http);
    Client::builder(TokioExecutor::new())
        .http1_title_case_headers(true)
        .pool_idle_timeout(cfg.pool_idle_timeout)
        .pool_max_idle_per_host(2)
        .build(https)
}

/// One try of a request: what came back, and what we know about the connection.
struct Attempt {
    outcome: Result<Response<Incoming>, AnnounceError>,
    /// Family of the address that was connected to, if a connection was made.
    connected: Option<Family>,
    /// The address that was connected to (for the developer log).
    ip: Option<IpAddr>,
    /// The failure happened on an established connection, before a response
    /// head: the request may have been written.
    after_connect: bool,
}

fn connected_ip(cap: &CaptureConnection) -> Option<IpAddr> {
    let meta = cap.connection_metadata();
    let conn = meta.as_ref()?;
    let mut ext = hyper::http::Extensions::new();
    conn.get_extras(&mut ext);
    ext.get::<HttpInfo>().map(|i| i.remote_addr().ip())
}

/// Patience of a best-effort request made while the process quits (a `stopped`): how long
/// to wait for the connection, and how long for the answer once the request has been written.
/// The answer to a `stopped` is irrelevant; the request being on the wire is what counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuitLimits {
    /// Connect (and, for a name, resolve) budget.
    pub connect: Duration,
    /// Time allowed for the response head after the connection is up (the request, a short GET,
    /// is written the moment the connection is handed to it).
    pub answer: Duration,
}

impl QuitLimits {
    /// Longest the whole exchange may take.
    pub fn total(&self) -> Duration {
        self.connect + self.answer
    }
}

/// Remembered families, per lower-cased host.
type Prefs = Arc<Mutex<HashMap<String, (Family, Instant)>>>;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What happened on the wire, for the developer log. A few `Copy` fields, filled either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HttpTrace {
    /// The address the connection went to (the last one tried).
    pub addr: Option<IpAddr>,
    /// Status of the last response head received.
    pub status: Option<u16>,
    /// The first address family failed and the other one was tried.
    pub fallback: bool,
}

/// Result of one released HTTP request.
#[derive(Debug)]
pub struct HttpOutcome<T> {
    /// Parsed answer or classified failure.
    pub result: Result<T, AnnounceError>,
    /// Address, status and fallback of the exchange.
    pub trace: HttpTrace,
    /// Redirect hops made on top of the released request. Report them with
    /// `HostGate::charge_extra` so the bucket pays for them.
    pub extra_hops: u32,
}

/// The HTTP(S) transport. Cheap to clone; clones share the connection pools and
/// the remembered address families.
#[derive(Clone)]
pub struct HttpTransport {
    /// Both families (hyper races the addresses of a name for the connect).
    client: HClient,
    /// One family only, for the fallback hop and the remembered preference.
    only_v4: HClient,
    only_v6: HClient,
    resolver: VettingResolver,
    prefs: Prefs,
    user_agent: String,
    cfg: HttpConfig,
}

impl HttpTransport {
    /// `version` is the application version, e.g. `"1.0.0"`; the User-Agent
    /// becomes `Seedwell/1.0.0`.
    pub fn new(version: &str) -> Self {
        Self::with_config(version, HttpConfig::default())
    }

    /// Same with explicit timeouts (tests).
    pub fn with_config(version: &str, cfg: HttpConfig) -> Self {
        Self::build(version, cfg, None)
    }

    /// Same, resolving the given names from a fixed table instead of DNS (tests
    /// of dual-stack behaviour, embedding). The destination policy still
    /// vets every address.
    #[doc(hidden)]
    pub fn with_static_hosts(
        version: &str,
        cfg: HttpConfig,
        hosts: &[(&str, Vec<IpAddr>)],
    ) -> Self {
        let table = hosts
            .iter()
            .map(|(h, ips)| (h.to_ascii_lowercase(), ips.clone()))
            .collect();
        Self::build(version, cfg, Some(Arc::new(table)))
    }

    fn build(
        version: &str,
        cfg: HttpConfig,
        hosts: Option<Arc<HashMap<String, Vec<IpAddr>>>>,
    ) -> Self {
        let resolver = VettingResolver {
            policy: cfg.policy,
            timeout: cfg.connect_timeout,
            hosts,
        };
        let version: String = version
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
            .collect();
        HttpTransport {
            client: build_client(&cfg, &resolver, None),
            only_v4: build_client(&cfg, &resolver, Some(Family::V4)),
            only_v6: build_client(&cfg, &resolver, Some(Family::V6)),
            resolver,
            prefs: Arc::new(Mutex::new(HashMap::new())),
            user_agent: format!("Seedwell/{version}"),
            cfg,
        }
    }

    /// The exact User-Agent that is sent.
    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// The request headers `announce` sends for `url`, as `Name: value` lines (developer log).
    pub fn request_headers(&self, url: &str) -> Vec<String> {
        let host = url.parse::<Uri>().map(|u| host_header(&u)).unwrap_or_default();
        vec![
            format!("Host: {host}"),
            format!("User-Agent: {}", self.user_agent),
            "Accept: */*".to_owned(),
            "Accept-Encoding: gzip".to_owned(),
        ]
    }

    /// Execute a released announce.
    pub async fn announce(&self, req: &GatedRequest) -> HttpOutcome<AnnounceResponse> {
        let Some(params) = req.announce_params() else {
            return HttpOutcome {
                result: Err(AnnounceError::Garbage(ParseError::NotDict)),
                trace: HttpTrace::default(),
                extra_hops: 0,
            };
        };
        let url = build_announce_url(req.url(), params);
        // `completed` is at-most-once (the machine keeps it set after an
        // ambiguous failure): never send it a second time on another address.
        let may_retry = params.event != Some(Event::Completed);
        self.fetch(req.url(), url, may_retry, None, parse_announce_response)
            .await
    }

    /// Like [`announce`](Self::announce) for a best-effort `stopped` at quit: one attempt on one
    /// address (no second family), the connection gets `limits.connect`, the answer
    /// `limits.answer` after the connection is up, and the whole thing never longer than
    /// `limits.total()`. A timeout is the expected end when a tracker is slow; the request was
    /// still sent. Over a pooled keep-alive connection where there is one.
    pub async fn announce_quit(&self, req: &GatedRequest, limits: QuitLimits) -> HttpOutcome<AnnounceResponse> {
        let Some(params) = req.announce_params() else {
            return HttpOutcome {
                result: Err(AnnounceError::Garbage(ParseError::NotDict)),
                trace: HttpTrace::default(),
                extra_hops: 0,
            };
        };
        let url = build_announce_url(req.url(), params);
        self.fetch(req.url(), url, false, Some(limits), parse_announce_response)
            .await
    }

    /// Execute a released scrape (BEP 48).
    pub async fn scrape(&self, req: &GatedRequest) -> HttpOutcome<Vec<(InfoHash20, ScrapeEntry)>> {
        let Some(hashes) = req.scrape_hashes() else {
            return HttpOutcome {
                result: Err(AnnounceError::Garbage(ParseError::NotDict)),
                trace: HttpTrace::default(),
                extra_hops: 0,
            };
        };
        let Some(url) = scrape_url(req.url(), hashes) else {
            return HttpOutcome {
                result: Err(AnnounceError::Garbage(ParseError::NotDict)),
                trace: HttpTrace::default(),
                extra_hops: 0,
            };
        };
        self.fetch(req.url(), url, true, None, parse_scrape_response)
            .await
    }

    async fn fetch<T>(
        &self,
        origin: &TrackerUrl,
        url: String,
        may_retry: bool,
        quit: Option<QuitLimits>,
        parse: impl Fn(&[u8]) -> Result<T, AnnounceError>,
    ) -> HttpOutcome<T> {
        let mut hops = 0;
        // Shared with the future so that a timeout still reports what was learned.
        let trace = Mutex::new(HttpTrace::default());
        let fut = self.fetch_inner(origin, url, may_retry, quit, &parse, &mut hops, &trace);
        let total = quit.map_or(self.cfg.total_timeout, |q| q.total().min(self.cfg.total_timeout));
        let result = match tokio::time::timeout(total, fut).await {
            Ok(r) => r,
            Err(_) => Err(AnnounceError::Transport(TransportError::Timeout)),
        };
        HttpOutcome {
            result,
            trace: *lock(&trace),
            extra_hops: hops,
        }
    }

    #[allow(clippy::too_many_arguments)] // one internal call site; a parameter struct would only move the list
    async fn fetch_inner<T>(
        &self,
        origin: &TrackerUrl,
        mut url: String,
        may_retry: bool,
        quit: Option<QuitLimits>,
        parse: &impl Fn(&[u8]) -> Result<T, AnnounceError>,
        hops: &mut u32,
        trace: &Mutex<HttpTrace>,
    ) -> Result<T, AnnounceError> {
        let mut current_scheme = origin.scheme();
        loop {
            let uri: Uri = pin_ip_literal(&url)
                .parse()
                .map_err(|_| AnnounceError::Transport(TransportError::Redirect))?;
            // Header set and order as sent by Transmission 4.x (captured
            // announces): Host, User-Agent, Accept, Accept-Encoding,
            // no `Connection` header (HTTP/1.1 keep-alive). `Host` is added
            // first by us so the pool does not append it after the others.
            // IP literals skip the resolver, so they are vetted here, on every
            // hop. (Names, including odd spellings such as `127.1`, go through
            // the vetting resolver at connect time.)
            if let Some(ip) = literal_ip(&uri)
                && let Err(r) = self.cfg.policy.check(ip) {
                    return Err(AnnounceError::Transport(TransportError::Forbidden(r)));
                }
            let response = self.send(&uri, may_retry, quit, trace).await?;
            let status = response.status();
            lock(trace).status = Some(status.as_u16());
            if status.is_redirection() {
                let Some(loc) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                else {
                    return Err(AnnounceError::Transport(TransportError::Redirect));
                };
                if *hops >= MAX_REDIRECTS {
                    return Err(AnnounceError::Transport(TransportError::Redirect));
                }
                let next = resolve_redirect(origin, current_scheme, &url, loc)
                    .ok_or(AnnounceError::Transport(TransportError::Redirect))?;
                *hops += 1;
                current_scheme = next.0;
                url = next.1;
                continue;
            }
            let retry_after = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| parse_retry_after(v, now_unix()));
            let gzip = response
                .headers()
                .get(CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_ascii_lowercase());
            let body = match Limited::new(response.into_body(), MAX_BODY).collect().await {
                Ok(c) => c.to_bytes(),
                Err(e) => {
                    let msg = e.to_string();
                    return Err(AnnounceError::Transport(if msg.contains("length limit") {
                        TransportError::TooLarge
                    } else {
                        TransportError::Io
                    }));
                }
            };
            let body = match gzip.as_deref() {
                None | Some("identity") | Some("") => body.to_vec(),
                Some("gzip") | Some("x-gzip") => {
                    gunzip(&body).ok_or(AnnounceError::Garbage(ParseError::NotBencode))?
                }
                Some(_) => return Err(AnnounceError::Garbage(ParseError::NotBencode)),
            };
            if body.len() > MAX_BODY {
                return Err(AnnounceError::Transport(TransportError::TooLarge));
            }
            if status.is_success() {
                return parse(&body);
            }
            return Err(error_status(status, retry_after, &body));
        }
    }

    fn client_for(&self, family: Option<Family>) -> &HClient {
        match family {
            None => &self.client,
            Some(Family::V4) => &self.only_v4,
            Some(Family::V6) => &self.only_v6,
        }
    }

    /// The remembered family for `host`, if it has not expired.
    fn preferred(&self, host: &str) -> Option<Family> {
        let mut prefs = lock(&self.prefs);
        match prefs.get(host) {
            Some((f, until)) if Instant::now() < *until => Some(*f),
            Some(_) => {
                prefs.remove(host);
                None
            }
            None => None,
        }
    }

    fn remember(&self, host: &str, family: Option<Family>) {
        let mut prefs = lock(&self.prefs);
        match family {
            Some(f) => {
                if prefs.len() >= 256 {
                    let now = Instant::now();
                    prefs.retain(|_, (_, until)| now < *until);
                }
                if prefs.len() < 256 {
                    prefs.insert(
                        host.to_owned(),
                        (f, Instant::now() + self.cfg.family_memory),
                    );
                }
            }
            None => {
                prefs.remove(host);
            }
        }
    }

    /// Send one request and return the response head.
    ///
    /// For a name that resolves to both families, a failure on an established
    /// connection before any response (reset, EOF, no answer within
    /// `address_timeout`) is retried ONCE on the other family; so is a failed
    /// connect to a remembered family. The retry happens inside this call, so
    /// the gate sees one request. When `may_retry` is false (`completed`) the
    /// first family gets the whole time and no second send is ever made.
    async fn send(
        &self,
        uri: &Uri,
        may_retry: bool,
        quit: Option<QuitLimits>,
        trace: &Mutex<HttpTrace>,
    ) -> Result<Response<Incoming>, AnnounceError> {
        let host = uri.host().unwrap_or_default().to_ascii_lowercase();
        let mut first: Option<Family> = None;
        let mut both_families = false;
        if literal_ip(uri).is_none() {
            let remembered = self.preferred(&host);
            if may_retry || remembered.is_some() {
                // A failed lookup is the answer (nothing was written): looking up a
                // second time through the connector would only double the wait.
                let addrs = self.resolver.resolve(&host).await.map_err(|e| {
                    AnnounceError::Transport(match e {
                        ResolveError::Forbidden(r) => TransportError::Forbidden(r),
                        ResolveError::Dns(_) | ResolveError::Timeout => TransportError::Dns,
                    })
                })?;
                let v4 = addrs.iter().any(|a| Family::of(a.ip()) == Family::V4);
                let v6 = addrs.iter().any(|a| Family::of(a.ip()) == Family::V6);
                if v4 && v6 {
                    both_families = true;
                    first = remembered;
                }
            }
        }
        // A second send after a failure on an established connection: not for `completed`.
        let can_fallback = both_families && may_retry;
        let limit = can_fallback.then_some(self.cfg.address_timeout);
        let a1 = self.attempt(first, uri, limit, quit).await;
        {
            let mut t = lock(trace);
            t.addr = a1.ip.or(t.addr);
        }
        let Err(first_err) = a1.outcome else {
            return a1.outcome;
        };
        let second = match (first, a1.after_connect, a1.connected) {
            // The remembered family failed: try the other. Even for `completed` if
            // the failure was in connecting (nothing written, as hyper's own
            // connector would have gone on to the next address).
            (Some(f), after, _) if both_families && (may_retry || !after) => Some(f.other()),
            (None, true, Some(c)) if can_fallback => Some(c.other()),
            _ => None,
        };
        let Some(second) = second else {
            return Err(first_err);
        };
        if first.is_some() {
            self.remember(&host, None);
        }
        let a2 = self.attempt(Some(second), uri, None, quit).await;
        {
            let mut t = lock(trace);
            t.fallback = true;
            t.addr = a2.ip.or(t.addr);
        }
        match a2.outcome {
            Ok(resp) => {
                self.remember(&host, Some(second));
                Ok(resp)
            }
            // The second try connected and failed: its error is the news.
            Err(e) if a2.after_connect => Err(e),
            // It never connected, so it sent nothing; the first error stands
            // (and, if that one was after a connect, it stays ambiguous).
            Err(_) => Err(first_err),
        }
    }

    /// One try on `family` (`None`: both), with an optional limit on connect
    /// plus time to the response head.
    async fn attempt(&self, family: Option<Family>, uri: &Uri, limit: Option<Duration>, quit: Option<QuitLimits>) -> Attempt {
        // Header set and order as sent by Transmission 4.x (captured
        // announces): Host, User-Agent, Accept, Accept-Encoding,
        // no `Connection` header (HTTP/1.1 keep-alive). `Host` is added
        // first by us so the pool does not append it after the others.
        let built = Request::builder()
            .method(Method::GET)
            .uri(uri.clone())
            .header(HOST, host_header(uri))
            .header(USER_AGENT, self.user_agent.as_str())
            .header(ACCEPT, "*/*")
            .header(ACCEPT_ENCODING, "gzip")
            .body(Empty::<Bytes>::new());
        let mut request = match built {
            Ok(r) => r,
            Err(_) => {
                return Attempt {
                    outcome: Err(AnnounceError::Transport(TransportError::Io)),
                    connected: None,
                    ip: None,
                    after_connect: false,
                };
            }
        };
        let mut cap = capture_connection(&mut request);
        let fut = self.client_for(family).request(request);
        let raced = match (quit, limit) {
            (Some(q), _) => race_quit(fut, &mut cap, q).await,
            (None, Some(d)) => tokio::time::timeout(d, fut).await.ok(),
            (None, None) => Some(fut.await),
        };
        let ip = connected_ip(&cap);
        let connected = ip.map(Family::of);
        match raced {
            Some(Ok(resp)) => Attempt {
                outcome: Ok(resp),
                connected,
                ip,
                after_connect: false,
            },
            Some(Err(e)) => Attempt {
                outcome: Err(AnnounceError::Transport(classify(&e))),
                connected,
                ip,
                after_connect: !e.is_connect(),
            },
            None if connected.is_some() => Attempt {
                outcome: Err(AnnounceError::Transport(TransportError::Timeout)),
                connected,
                ip,
                after_connect: true,
            },
            // Still connecting when the limit expired: nothing was written.
            None => Attempt {
                outcome: Err(AnnounceError::Transport(TransportError::ConnectTimeout)),
                connected: None,
                ip: None,
                after_connect: false,
            },
        }
    }
}

/// Wait for `fut` in two phases: `q.connect` for the connection to come up, then `q.answer` for
/// the response head. `None`: a phase ran out.
async fn race_quit<F, T>(fut: F, cap: &mut CaptureConnection, q: QuitLimits) -> Option<T>
where
    F: Future<Output = T>,
{
    enum Phase<T> {
        Done(T),
        Connected,
        NoConnection,
    }
    let mut fut = Box::pin(fut);
    let mut up = Box::pin(async {
        let _ = cap.wait_for_connection_metadata().await;
    });
    let mut give_up = Box::pin(tokio::time::sleep(q.connect));
    let phase = std::future::poll_fn(|cx| {
        if let Poll::Ready(r) = fut.as_mut().poll(cx) {
            return Poll::Ready(Phase::Done(r));
        }
        if up.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Phase::Connected);
        }
        if give_up.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Phase::NoConnection);
        }
        Poll::Pending
    })
    .await;
    match phase {
        Phase::Done(r) => Some(r),
        Phase::NoConnection => None,
        Phase::Connected => tokio::time::timeout(q.answer, fut).await.ok(),
    }
}

/// Rewrite a host that is an IP address in a legacy spelling (`127.1`,
/// `2130706433`, `0177.0.0.1`, `[::ffff:7f00:1]`) to its canonical form.
///
/// The spellings are not read the same everywhere (`0177.0.0.1` is 127.0.0.1
/// to glibc but 177.0.0.1 to macOS's `getaddrinfo`), and the policy check below
/// is only as good as our reading matching the one that connects. After this
/// rewrite the connector sees a plain dotted-quad / bracketed IPv6 literal and
/// never asks the OS to interpret an odd spelling.
fn pin_ip_literal(url: &str) -> String {
    let Ok(parsed) = TrackerUrl::parse(url) else {
        return url.to_owned();
    };
    let Some(ip) = parsed.host_ip() else {
        return url.to_owned();
    };
    let Some(scheme_end) = url.find("://") else {
        return url.to_owned();
    };
    let after = scheme_end + 3;
    let auth_end = url[after..]
        .find(['/', '?'])
        .map_or(url.len(), |i| after + i);
    let host = match ip {
        IpAddr::V4(a) => a.to_string(),
        IpAddr::V6(a) => format!("[{a}]"),
    };
    format!(
        "{}{}:{}{}",
        &url[..after],
        host,
        parsed.port(),
        &url[auth_end..]
    )
}

/// The IP address a URI names by literal, if it does.
fn literal_ip(uri: &Uri) -> Option<IpAddr> {
    let h = uri.host()?;
    let h = h
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(h);
    h.parse().ok()
}

/// `Host` header value: authority, with the port omitted when it is the
/// scheme's default (what curl and hyper do).
fn host_header(uri: &Uri) -> String {
    let host = uri.host().unwrap_or_default();
    let default = match uri.scheme_str() {
        Some("https") => 443,
        _ => 80,
    };
    match uri.port_u16() {
        Some(p) if p != default => format!("{host}:{p}"),
        _ => host.to_owned(),
    }
}

/// Map a non-success status to an error. A 4xx body that is a bencoded
/// `failure reason` is a genuine tracker rejection (some trackers answer 400
/// instead of 200); everything else is transient.
fn error_status(status: StatusCode, retry_after: Option<Duration>, body: &[u8]) -> AnnounceError {
    let code = status.as_u16();
    if (400..500).contains(&code) && code != 408 && code != 429
        && let Err(rej @ AnnounceError::Rejected { .. }) = parse_announce_response(body) {
            return rej;
        }
    AnnounceError::Http {
        status: code,
        retry_after,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn gunzip(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut dec = flate2::read::GzDecoder::new(data).take(MAX_BODY as u64 + 1);
    dec.read_to_end(&mut out).ok()?;
    (out.len() <= MAX_BODY).then_some(out)
}

/// Decide where a redirect leads, or `None` if we refuse to follow it:
/// same host only (a redirect must never move a request to a host the gate has
/// not budgeted, and private torrents must never leave their tracker), and the
/// scheme may only stay the same or upgrade http -> https.
pub fn resolve_redirect(
    origin: &TrackerUrl,
    current: Scheme,
    current_url: &str,
    location: &str,
) -> Option<(Scheme, String)> {
    let location = location.trim();
    let absolute = if location.contains("://") {
        location.to_owned()
    } else {
        // absolute-path reference on the current authority
        let path = location.strip_prefix('/')?;
        let after_scheme = current_url.find("://")? + 3;
        let authority_end = after_scheme
            + current_url[after_scheme..]
                .find('/')
                .unwrap_or(current_url.len() - after_scheme);
        format!("{}/{}", &current_url[..authority_end], path)
    };
    let target = TrackerUrl::parse(&absolute).ok()?;
    if target.host() != origin.host() {
        return None;
    }
    let ok = matches!(
        (current, target.scheme()),
        (Scheme::Http, Scheme::Http)
            | (Scheme::Https, Scheme::Https)
            | (Scheme::Http, Scheme::Https)
    );
    ok.then(|| (target.scheme(), absolute))
}

/// Classify a hyper client error by walking its source chain.
///
/// The classes double as the "was the request written?" flag (see
/// [`AnnounceError::never_reached_tracker`]): `Dns`, `Connect`, `ConnectTimeout`,
/// `Tls` and `Forbidden` are only returned for errors that happened while
/// *connecting* (`is_connect()`), i.e. before the request was written.
/// Everything that happens on an established connection is `Timeout` or `Io`,
/// which are ambiguous: the tracker may have received the announce.
fn classify(e: &hyper_util::client::legacy::Error) -> TransportError {
    let mut text = String::new();
    let mut kind_timeout = false;
    let mut resolve: Option<&ResolveError> = None;
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        text.push_str(&err.to_string().to_ascii_lowercase());
        text.push('|');
        if let Some(io) = err.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::TimedOut {
                kind_timeout = true;
            }
        if let Some(r) = err.downcast_ref::<ResolveError>() {
            resolve = Some(r);
        }
        cur = err.source();
    }
    if let Some(r) = resolve {
        return match r {
            ResolveError::Forbidden(r) => TransportError::Forbidden(*r),
            ResolveError::Dns(_) | ResolveError::Timeout => TransportError::Dns,
        };
    }
    let connecting = e.is_connect();
    if connecting {
        if kind_timeout || text.contains("timed out") {
            TransportError::ConnectTimeout
        } else if text.contains("dns")
            || text.contains("failed to lookup")
            || text.contains("nodename nor servname")
            || text.contains("no address associated")
        {
            TransportError::Dns
        } else if text.contains("certificate")
            || text.contains("tls")
            || text.contains("handshake")
            || text.contains("invalid peer")
            || text.contains("rustls")
        {
            TransportError::Tls
        } else {
            TransportError::Connect
        }
    } else if kind_timeout || text.contains("timed out") {
        TransportError::Timeout
    } else {
        TransportError::Io
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> TrackerUrl {
        TrackerUrl::parse(s).unwrap()
    }

    #[test]
    fn redirect_policy() {
        let o = u("http://t.example/announce?passkey=abc");
        let cur = "http://t.example/announce?passkey=abc&info_hash=x";
        // same host, same scheme
        assert_eq!(
            resolve_redirect(&o, Scheme::Http, cur, "http://t.example/new?a=b")
                .unwrap()
                .1,
            "http://t.example/new?a=b"
        );
        // absolute-path reference
        assert_eq!(
            resolve_redirect(&o, Scheme::Http, cur, "/moved/announce?x=1")
                .unwrap()
                .1,
            "http://t.example/moved/announce?x=1"
        );
        // http -> https upgrade (port changes)
        assert_eq!(
            resolve_redirect(
                &o,
                Scheme::Http,
                cur,
                "https://t.example/announce?passkey=abc"
            )
            .unwrap()
            .0,
            Scheme::Https
        );
        // downgrade refused
        assert!(
            resolve_redirect(
                &o,
                Scheme::Https,
                "https://t.example/a",
                "http://t.example/a"
            )
            .is_none()
        );
        // other host refused (also case-insensitively equal host accepted)
        assert!(resolve_redirect(&o, Scheme::Http, cur, "http://evil.example/announce").is_none());
        assert!(resolve_redirect(&o, Scheme::Http, cur, "http://T.EXAMPLE/x").is_some());
        // userinfo, relative paths, junk refused
        assert!(resolve_redirect(&o, Scheme::Http, cur, "http://user:pw@t.example/x").is_none());
        assert!(resolve_redirect(&o, Scheme::Http, cur, "relative/path").is_none());
        assert!(resolve_redirect(&o, Scheme::Http, cur, "ftp://t.example/x").is_none());
        assert!(resolve_redirect(&o, Scheme::Http, cur, "").is_none());
    }

    #[test]
    fn gzip_roundtrip_and_bomb() {
        use flate2::{Compression, write::GzEncoder};
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"d8:intervali900ee").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(gunzip(&gz).unwrap(), b"d8:intervali900ee");
        let mut enc = GzEncoder::new(Vec::new(), Compression::best());
        enc.write_all(&vec![b'a'; MAX_BODY + 10]).unwrap();
        assert!(
            gunzip(&enc.finish().unwrap()).is_none(),
            "decompression bomb refused"
        );
        assert!(gunzip(b"not gzip").is_none());
    }

    #[test]
    fn status_mapping() {
        let body = b"d14:failure reason9:not found e";
        let _ = body;
        let rej = error_status(StatusCode::BAD_REQUEST, None, b"d14:failure reason4:nopee");
        assert!(matches!(rej, AnnounceError::Rejected { .. }));
        assert!(matches!(
            error_status(
                StatusCode::TOO_MANY_REQUESTS,
                Some(Duration::from_secs(5)),
                b"d14:failure reason4:nopee"
            ),
            AnnounceError::Http { status: 429, .. }
        ));
        assert!(matches!(
            error_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                b"d14:failure reason4:nopee"
            ),
            AnnounceError::Http { status: 500, .. }
        ));
        assert!(matches!(
            error_status(StatusCode::NOT_FOUND, None, b"<html>"),
            AnnounceError::Http { status: 404, .. }
        ));
    }

    #[test]
    fn ip_literals_are_pinned_to_canonical_form() {
        let p = pin_ip_literal;
        assert_eq!(
            p("http://0177.0.0.1:8080/a?x=1"),
            "http://127.0.0.1:8080/a?x=1"
        );
        assert_eq!(p("http://127.1/a"), "http://127.0.0.1:80/a");
        assert_eq!(p("https://2130706433/a"), "https://127.0.0.1:443/a");
        assert_eq!(p("http://[::ffff:7f00:1]:9/a"), "http://127.0.0.1:9/a");
        assert_eq!(p("http://[0:0:0:0:0:0:0:1]:9/a"), "http://[::1]:9/a");
        // names and unparseable input are left alone
        assert_eq!(p("http://t.example/a?x=1"), "http://t.example/a?x=1");
        assert_eq!(p("not a url"), "not a url");
    }

    #[test]
    fn host_header_omits_default_ports() {
        let h = |s: &str| host_header(&s.parse::<Uri>().unwrap());
        assert_eq!(h("http://t.example/a"), "t.example");
        assert_eq!(h("http://t.example:80/a"), "t.example");
        assert_eq!(h("http://t.example:6969/a"), "t.example:6969");
        assert_eq!(h("https://t.example:443/a"), "t.example");
        assert_eq!(h("https://t.example:80/a"), "t.example:80");
        assert_eq!(h("http://[2001:db8::1]:6969/a"), "[2001:db8::1]:6969");
    }

    #[test]
    fn user_agent_is_fixed_shape() {
        // (constructing a transport needs a runtime-free connector only)
        let t = HttpTransport::new("1.2.3");
        assert_eq!(t.user_agent(), "Seedwell/1.2.3");
        // anything that is not a plain version string is sanitised away
        let t = HttpTransport::new("9.9\r\nX-Evil: 1 qBittorrent");
        assert!(t.user_agent().starts_with("Seedwell/"));
        assert!(!t.user_agent().contains(' ') && !t.user_agent().contains('\n'));
    }
}
