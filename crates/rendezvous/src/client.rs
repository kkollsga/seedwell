//! Talking to a rendezvous Worker (API v1, `tools/rendezvous-worker/worker.js`).
//!
//! Reads need nothing (records are sealed). A write (`PUT`, `DELETE`) carries the device's write key for this
//! rendezvous (a random Ed25519 key, never the device key: Cloudflare never sees a device key), the time, and the
//! write key's signature over `"seedwell-rendezvous/v1\n" ‖ method ‖ "\n" ‖ id ‖ "\n" ‖ time ‖ "\n" ‖
//! hex(SHA-256(body))`. The Worker keeps the first write key of a record and refuses others, and refuses a time
//! that is more than 5 minutes off or not later than the last write's.

use std::time::Duration;

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};
use trs_http::{SmallRequest, request_small};

use crate::{MAX_RECORD, hex, now_ms};

/// How long one request may take.
pub const TIMEOUT: Duration = Duration::from_secs(15);
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Why a rendezvous request did not work, in a sentence for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientError {
    /// The URL is not a rendezvous address (`https://…`).
    Url(String),
    /// It could not be reached.
    Network(String),
    /// It answered, but not as a Seedwell rendezvous does.
    NotRendezvous(String),
    /// It refused the request.
    Refused {
        /// The HTTP status.
        status: u16,
        /// What it said.
        message: String,
    },
    /// The system random source or the key failed.
    Key(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Url(why) => write!(f, "That is not a rendezvous address: {why}."),
            ClientError::Network(why) => write!(f, "The rendezvous cannot be reached ({why})."),
            ClientError::NotRendezvous(why) => write!(f, "That address answers, but not as a Seedwell rendezvous ({why})."),
            ClientError::Refused { status: 409, message: m } if m.contains("minutes off") => {
                f.write_str("The rendezvous refused the record: this device's clock is more than 5 minutes off.")
            }
            ClientError::Refused { status: 403, .. } => {
                f.write_str("The rendezvous keeps this device's record for another key (set up again from this device, or wait a week for it to expire).")
            }
            ClientError::Refused { status: 429, .. } => f.write_str("The rendezvous is busy with this record; it is tried again later."),
            ClientError::Refused { status, message } => write!(f, "The rendezvous refused the request ({status}: {message})."),
            ClientError::Key(why) => write!(f, "The write key could not be made ({why})."),
        }
    }
}

impl std::error::Error for ClientError {}

/// A device's write key for one rendezvous: a random Ed25519 key, kept in the secret store.
pub struct WriteKey {
    pair: Ed25519KeyPair,
    pkcs8: Vec<u8>,
}

impl std::fmt::Debug for WriteKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WriteKey({})", &self.public_hex()[..8])
    }
}

impl WriteKey {
    /// A new random key.
    pub fn generate() -> Result<WriteKey, ClientError> {
        let doc = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).map_err(|_| ClientError::Key("no random bytes".into()))?;
        WriteKey::from_pkcs8(doc.as_ref())
    }

    /// The key from its PKCS#8 form (as [`WriteKey::pkcs8`] gives it).
    pub fn from_pkcs8(pkcs8: &[u8]) -> Result<WriteKey, ClientError> {
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8).map_err(|e| ClientError::Key(e.to_string()))?;
        Ok(WriteKey { pair, pkcs8: pkcs8.to_vec() })
    }

    /// The PKCS#8 form, for the secret store.
    pub fn pkcs8(&self) -> &[u8] {
        &self.pkcs8
    }

    /// The public key, as the `Seedwell-Key` header sends it.
    pub fn public_hex(&self) -> String {
        hex(self.pair.public_key().as_ref())
    }

    /// The signature of a write.
    fn sign(&self, method: &str, id: &str, time_ms: u64, body: &[u8]) -> String {
        hex(self.pair.sign(signed_text(method, id, time_ms, body).as_bytes()).as_ref())
    }
}

/// What the write key signs.
pub fn signed_text(method: &str, id: &str, time_ms: u64, body: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, body);
    format!("seedwell-rendezvous/v1\n{method}\n{id}\n{time_ms}\n{}", hex(digest.as_ref()))
}

/// The rendezvous's address as Seedwell keeps it: `https://host` (a port when it has one), without a path. A pasted
/// address may end in `/` or `/v1`; plain `http` is taken only for this computer (tests, a local Worker).
pub fn normalize_url(text: &str) -> Result<String, ClientError> {
    let t = text.trim().trim_end_matches('/');
    let t = t.strip_suffix("/v1").unwrap_or(t).trim_end_matches('/');
    let (scheme, rest) = t.split_once("://").ok_or_else(|| ClientError::Url("it starts with https://".into()))?;
    let scheme = scheme.to_ascii_lowercase();
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ']) {
        return Err(ClientError::Url("give the Worker's address alone, like https://seedwell-rendezvous.example.workers.dev".into()));
    }
    let host = rest.rsplit_once(':').filter(|(_, p)| p.parse::<u16>().is_ok()).map_or(rest, |(h, _)| h);
    let local = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
    match scheme.as_str() {
        "https" => {}
        "http" if local => {}
        _ => return Err(ClientError::Url("it starts with https://".into())),
    }
    Ok(format!("{scheme}://{}", rest.to_ascii_lowercase()))
}

/// One rendezvous.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rendezvous {
    base: String,
}

fn network(e: trs_http::Error) -> ClientError {
    ClientError::Network(match e {
        trs_http::Error::Timeout => "no answer".into(),
        trs_http::Error::Tls(_) => "its certificate is not valid".into(),
        trs_http::Error::Network(m) | trs_http::Error::InvalidUrl(m) | trs_http::Error::BadResponse(m) => m,
        other => other.to_string(),
    })
}

fn refused(status: u16, body: &[u8]) -> ClientError {
    let message = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
        .unwrap_or_else(|| format!("HTTP {status}"));
    ClientError::Refused { status, message }
}

impl Rendezvous {
    /// The rendezvous at `url` ([`normalize_url`]).
    pub fn new(url: &str) -> Result<Rendezvous, ClientError> {
        Ok(Rendezvous { base: normalize_url(url)? })
    }

    /// Its address.
    pub fn url(&self) -> &str {
        &self.base
    }

    async fn send(&self, method: &str, path: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> Result<trs_http::SmallResponse, ClientError> {
        let req = SmallRequest { method: method.into(), url: format!("{}{path}", self.base), headers, body };
        request_small(req, VERSION, MAX_RECORD + 4096, TIMEOUT).await.map_err(network)
    }

    /// Checks that it is a Seedwell rendezvous of an API this client speaks.
    pub async fn info(&self) -> Result<(), ClientError> {
        let r = self.send("GET", "/v1", Vec::new(), Vec::new()).await?;
        if r.status != 200 {
            return Err(ClientError::NotRendezvous(format!("HTTP {}", r.status)));
        }
        let v: serde_json::Value = serde_json::from_slice(&r.body).map_err(|_| ClientError::NotRendezvous("not its answer".into()))?;
        if v.get("service").and_then(|s| s.as_str()) != Some("seedwell-rendezvous") {
            return Err(ClientError::NotRendezvous("not its answer".into()));
        }
        if v.get("api").and_then(serde_json::Value::as_u64) != Some(u64::from(crate::API)) {
            return Err(ClientError::NotRendezvous("another version: update the Worker from Settings".into()));
        }
        Ok(())
    }

    /// The record under `id` (64 hex digits), if there is one.
    pub async fn get(&self, id: &str) -> Result<Option<Vec<u8>>, ClientError> {
        let r = self.send("GET", &format!("/v1/r/{id}"), Vec::new(), Vec::new()).await?;
        match r.status {
            200 => Ok(Some(r.body)),
            404 => Ok(None),
            s => Err(refused(s, &r.body)),
        }
    }

    fn signed_headers(key: &WriteKey, method: &str, id: &str, time_ms: u64, body: &[u8]) -> Vec<(String, String)> {
        vec![
            ("Seedwell-Key".into(), key.public_hex()),
            ("Seedwell-Time".into(), time_ms.to_string()),
            ("Seedwell-Signature".into(), key.sign(method, id, time_ms, body)),
            ("Content-Type".into(), "application/octet-stream".into()),
        ]
    }

    /// Writes the record under `id`, signed with `key` at `time_ms`.
    pub async fn put(&self, id: &str, body: &[u8], key: &WriteKey, time_ms: u64) -> Result<(), ClientError> {
        let headers = Self::signed_headers(key, "PUT", id, time_ms, body);
        let r = self.send("PUT", &format!("/v1/r/{id}"), headers, body.to_vec()).await?;
        if (200..300).contains(&r.status) { Ok(()) } else { Err(refused(r.status, &r.body)) }
    }

    /// Deletes the record under `id` (it is gone at once; the write key keeps the id until it expires).
    pub async fn delete(&self, id: &str, key: &WriteKey, time_ms: u64) -> Result<(), ClientError> {
        let headers = Self::signed_headers(key, "DELETE", id, time_ms, &[]);
        let r = self.send("DELETE", &format!("/v1/r/{id}"), headers, Vec::new()).await?;
        if (200..300).contains(&r.status) { Ok(()) } else { Err(refused(r.status, &r.body)) }
    }

    /// Writes a test record under a random id with a new key, reads it back and deletes it.
    pub async fn self_test(&self) -> Result<(), ClientError> {
        self.info().await?;
        let key = WriteKey::generate()?;
        let mut raw = [0u8; 32];
        ring::rand::SecureRandom::fill(&SystemRandom::new(), &mut raw).map_err(|_| ClientError::Key("no random bytes".into()))?;
        let id = hex(&raw);
        let body = format!("seedwell self-test {}", hex(&raw[..4])).into_bytes();
        let t = now_ms();
        self.put(&id, &body, &key, t).await?;
        let back = self.get(&id).await?;
        if back.as_deref() != Some(body.as_slice()) {
            return Err(ClientError::NotRendezvous("a test record did not read back".into()));
        }
        self.delete(&id, &key, t + 1).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_normalized_and_only_https_is_taken() {
        let ok = |s: &str| normalize_url(s).unwrap();
        assert_eq!(ok("https://seedwell-rendezvous.Me.workers.dev/"), "https://seedwell-rendezvous.me.workers.dev");
        assert_eq!(ok(" https://x.workers.dev/v1/ "), "https://x.workers.dev");
        assert_eq!(ok("https://x.example:8443"), "https://x.example:8443");
        assert_eq!(ok("http://127.0.0.1:5000"), "http://127.0.0.1:5000");
        for bad in ["http://x.workers.dev", "x.workers.dev", "https://", "https://x.workers.dev/other", "ftp://x", "https://u@x.dev"] {
            assert!(normalize_url(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_write_is_signed_over_the_documented_text() {
        let k = WriteKey::generate().unwrap();
        let text = signed_text("PUT", &"ab".repeat(32), 1_800_000_000_000, b"abc");
        assert_eq!(text, format!("seedwell-rendezvous/v1\nPUT\n{}\n1800000000000\nba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", "ab".repeat(32)));
        let sig = crate::unhex(&k.sign("PUT", &"ab".repeat(32), 1_800_000_000_000, b"abc")).unwrap();
        let pk = crate::unhex(&k.public_hex()).unwrap();
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, pk).verify(text.as_bytes(), &sig).unwrap();
        let again = WriteKey::from_pkcs8(k.pkcs8()).unwrap();
        assert_eq!(again.public_hex(), k.public_hex());
    }
}
