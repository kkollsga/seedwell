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
