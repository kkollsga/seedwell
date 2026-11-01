//! One small GET for a document, not a download (the app's update check reads `latest.json` with it): the same client,
//! User-Agent and redirect rules as the downloads, the whole body in memory, capped, within one time limit.

use std::time::Duration;

use http_body_util::BodyExt;

use crate::error::Error;
use crate::net::{Net, NetConfig};

/// What a [`get_small`] answered: the final status (after redirects) and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmallResponse {
    /// HTTP status of the last response.
    pub status: u16,
    /// The body; empty unless the status is 2xx.
    pub body: Vec<u8>,
}

/// GET `url`, following redirects (at most 5, never https to http), with `Seedwell/<version>` as the User-Agent.
/// The body of a 2xx answer is read whole, at most `max_bytes` (more is [`Error::BadResponse`]); everything,
/// redirects included, within `timeout` ([`Error::Timeout`]). No cookies, no credentials, no retries, no cache.
pub async fn get_small(url: &str, version: &str, max_bytes: usize, timeout: Duration) -> Result<SmallResponse, Error> {
    let net = Net::new(NetConfig {
        version: version.to_owned(),
        max_connections_per_host: 1,
        connect_timeout: timeout,
        read_timeout: timeout,
        max_redirects: 5,
    });
    let read = async {
        let opened = net.open(url, None, None).await?;
        let status = opened.resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Ok(SmallResponse { status, body: Vec::new() });
        }
        let mut body = opened.resp.into_body();
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| Error::Network(e.to_string()))?;
            if let Some(data) = frame.data_ref() {
                if out.len() + data.len() > max_bytes {
                    return Err(Error::BadResponse(format!("more than {max_bytes} bytes")));
                }
                out.extend_from_slice(data);
            }
        }
        Ok(SmallResponse { status, body: out })
    };
    tokio::time::timeout(timeout, read).await.map_err(|_| Error::Timeout)?
}

/// One small request with a body (an API call, not a download): [`request_small`]'s arguments.
#[derive(Clone, Debug, Default)]
pub struct SmallRequest {
    /// `GET`, `PUT`, `POST`, `DELETE`, …
    pub method: String,
    /// An absolute `http`/`https` URL without credentials.
    pub url: String,
    /// Extra headers (names and values as sent). `User-Agent` is always `Seedwell/<version>`.
    pub headers: Vec<(String, String)>,
    /// The body (empty: none).
    pub body: Vec<u8>,
}

/// Sends `req` once and reads the answer whole, at most `max_bytes`, within `timeout`: the body is kept for every
/// status (an API's error answer says what went wrong). Redirects are not followed (they come back as the answer). No
/// cookies, no retries, no cache. Nothing here logs, so credentials passed in `headers` stay out of logs.
pub async fn request_small(req: SmallRequest, version: &str, max_bytes: usize, timeout: Duration) -> Result<SmallResponse, Error> {
    use bytes::Bytes;
    use http_body_util::Full;
    use hyper::header::{HeaderName, HeaderValue, USER_AGENT};
    use hyper_util::client::legacy::Client;
    use hyper_util::rt::TokioExecutor;

    let uri = crate::net::parse_url(&req.url)?;
    let method = hyper::Method::from_bytes(req.method.as_bytes()).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(timeout));
    http.set_nodelay(true);
    let https = hyper_rustls::HttpsConnectorBuilder::new().with_webpki_roots().https_or_http().enable_http1().wrap_connector(http);
    let client: Client<_, Full<Bytes>> = Client::builder(TokioExecutor::new()).pool_max_idle_per_host(0).build(https);
    let version: String = version.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+')).collect();
    let mut b = hyper::Request::builder().method(method).uri(uri).header(USER_AGENT, format!("Seedwell/{version}"));
    for (k, v) in &req.headers {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|e| Error::InvalidUrl(e.to_string()))?;
        let value = HeaderValue::from_str(v).map_err(|_| Error::InvalidUrl(format!("header {k} is not valid")))?;
        b = b.header(name, value);
    }
    let r = b.body(Full::new(Bytes::from(req.body))).map_err(|e| Error::InvalidUrl(e.to_string()))?;
    let read = async {
        let resp = client.request(r).await.map_err(|e| crate::net::classify(&e))?;
        let status = resp.status().as_u16();
        let mut body = resp.into_body();
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| Error::Network(e.to_string()))?;
            if let Some(data) = frame.data_ref() {
                if out.len() + data.len() > max_bytes {
                    return Err(Error::BadResponse(format!("more than {max_bytes} bytes")));
                }
                out.extend_from_slice(data);
            }
        }
        Ok(SmallResponse { status, body: out })
    };
    tokio::time::timeout(timeout, read).await.map_err(|_| Error::Timeout)?
}
