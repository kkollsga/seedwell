//! Setting up the rendezvous on the user's Cloudflare account with an API token, through the Cloudflare REST API:
//! no Node, no wrangler, no Cloudflare credentials kept anywhere (the token lives only in this call).
//!
//! The steps and the endpoints, as Cloudflare's API reference has them (checked 2026-10-31; base
//! `https://api.cloudflare.com/client/v4`, `Authorization: Bearer <token>`, every answer the envelope
//! `{"success", "errors": [{"code", "message"}], "messages", "result"}`):
//!
//! 1. The token works: `GET /user/tokens/verify` → `result.status == "active"`
//!    (developers.cloudflare.com/api/resources/user/subresources/tokens/methods/verify/). An account-owned token
//!    (`cfat_…`) is checked by the next step instead (its own verify endpoint needs the account id).
//! 2. The account: `GET /accounts` → `result[].id`, `.name` (…/api/resources/accounts/methods/list/). The "Edit
//!    Cloudflare Workers" token template includes Account Settings Read (…/fundamentals/api/reference/template/).
//! 3. Whether the Worker is there, and its Durable Object migration: `GET /accounts/{id}/workers/scripts` →
//!    `result[].id`, `.migration_tag` (…/api/resources/workers/subresources/scripts/methods/list/).
//! 4. The upload: `PUT /accounts/{id}/workers/scripts/seedwell-rendezvous`, `multipart/form-data` with a part
//!    `metadata` (JSON: `main_module`, `compatibility_date`, `bindings: [{"type": "durable_object_namespace",
//!    "name", "class_name"}]`, and on the first upload `migrations: {"new_tag": "v1", "new_sqlite_classes":
//!    ["Rendezvous"]}`) and the module as a part named after it, `worker.js`, of type
//!    `application/javascript+module` (…/api/resources/workers/subresources/scripts/methods/update/,
//!    …/workers/configuration/multipart-upload-metadata/; wrangler builds the same form). When the script already
//!    has the migration, the upload leaves `migrations` out (as wrangler does). The legacy migrations are used, not
//!    the newer `exports` map, because `wrangler.jsonc` (the developer's way) uses them and a Worker deployed with
//!    `exports` cannot go back.
//! 5. The account's workers.dev subdomain: `GET /accounts/{id}/workers/subdomain` → `result.subdomain`; when the
//!    account has none (error 10007, or no result), `PUT` the same path with `{"subdomain": "seedwell-<random>"}`
//!    (…/api/resources/workers/subresources/subdomains/methods/get/ and …/update/).
//! 6. The Worker on workers.dev: `POST /accounts/{id}/workers/scripts/seedwell-rendezvous/subdomain` with
//!    `{"enabled": true, "previews_enabled": false}` (…/api/resources/workers/subresources/scripts/subresources/
//!    subdomain/methods/create/).
//! 7. The address: `https://seedwell-rendezvous.<subdomain>.workers.dev`, from the subdomain the API answered in 5
//!    (…/workers/configuration/routing/workers-dev/: `<worker name>.<subdomain>.workers.dev`), never from the login
//!    email. Then a self-test (`GET /v1`, a test record written, read back and deleted), retried for a while: a new
//!    workers.dev address can take a minute to answer.

use std::time::Duration;

use serde_json::{Value, json};
use seedwell_http::{SmallRequest, request_small};

use crate::client::{ClientError, Rendezvous};
use crate::{BINDING, CLASS_NAME, COMPATIBILITY_DATE, MIGRATION_TAG, SCRIPT_NAME, WORKER_JS, hex};

/// Cloudflare's API.
pub const API_BASE: &str = "https://api.cloudflare.com/client/v4";
/// Where the user makes the API token.
pub const TOKEN_PAGE: &str = "https://dash.cloudflare.com/profile/api-tokens";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const TIMEOUT: Duration = Duration::from_secs(60);

/// Where the setup is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Checking the token.
    Token,
    /// Finding the account.
    Account,
    /// Uploading the Worker.
    Upload,
    /// Putting it on workers.dev.
    Address,
    /// Testing it.
    Test,
}

/// What the setup made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployed {
    /// The rendezvous's address (`https://seedwell-rendezvous.<subdomain>.workers.dev`).
    pub url: String,
    /// The Cloudflare account it is on (its name).
    pub account: String,
    /// The account had no workers.dev subdomain: this one was made.
    pub new_subdomain: Option<String>,
}

/// Why the setup stopped, in a sentence for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeployError {
    /// The token was refused.
    Token(String),
    /// The token reaches no account.
    NoAccount,
    /// The token lacks a permission.
    Permission(String),
    /// The account has no workers.dev subdomain and none could be made.
    Subdomain(String),
    /// Cloudflare could not be reached.
    Network(String),
    /// Cloudflare refused a step.
    Api(String),
    /// The Worker is up but did not pass the self-test.
    Test(String),
}

impl std::fmt::Display for DeployError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeployError::Token(why) => write!(
                f,
                "Cloudflare did not accept the token ({why}). Make a new one from the \u{201c}Edit Cloudflare Workers\u{201d} template and paste all of it."
            ),
            DeployError::NoAccount => f.write_str(
                "The token does not reach a Cloudflare account. Make it again and, under Account Resources, include your account.",
            ),
            DeployError::Permission(why) => write!(
                f,
                "The token may not do this ({why}). Make it from the \u{201c}Edit Cloudflare Workers\u{201d} template, which has the permissions Seedwell needs."
            ),
            DeployError::Subdomain(why) => write!(
                f,
                "Your Cloudflare account has no workers.dev address yet, and Seedwell could not make one ({why}). In the Cloudflare dashboard, open Workers & Pages once (it asks you to pick a workers.dev subdomain), then press Set Up again."
            ),
            DeployError::Network(why) => write!(f, "Cloudflare cannot be reached ({why}). Check the Internet connection and try again."),
            DeployError::Api(why) => write!(f, "Cloudflare refused a step: {why}."),
            DeployError::Test(why) => write!(f, "The Worker is on Cloudflare, but it did not answer as it should yet: {why} Press Test in a minute."),
        }
    }
}

impl std::error::Error for DeployError {}

/// A session with the Cloudflare API: the token lives only here and is wiped when it is dropped.
pub struct Api {
    base: String,
    token: Vec<u8>,
    /// **Tests:** the address the deployed Worker answers at (a loopback fake), instead of its workers.dev address.
    pub test_url: Option<String>,
    /// How long the self-test keeps trying while a new address starts to answer.
    pub settle: Duration,
}

impl Drop for Api {
    fn drop(&mut self) {
        self.token.fill(0);
        std::hint::black_box(&self.token);
    }
}

impl std::fmt::Debug for Api {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Api({})", self.base)
    }
}

/// The token as typed: whitespace and a `Bearer ` the user may have copied along are dropped.
fn clean_token(t: &str) -> String {
    let t = t.trim();
    let t = t.strip_prefix("Bearer ").unwrap_or(t).trim();
    t.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The workers.dev address of `script` on an account whose subdomain is `subdomain`.
pub fn workers_dev_url(script: &str, subdomain: &str) -> String {
    format!("https://{script}.{}.workers.dev", subdomain.trim().trim_end_matches(".workers.dev").to_ascii_lowercase())
}

struct Answer {
    status: u16,
    v: Value,
}

impl Answer {
    fn ok(&self) -> bool {
        (200..300).contains(&self.status) && self.v.get("success").and_then(Value::as_bool).unwrap_or(false)
    }

    fn codes(&self) -> Vec<i64> {
        self.v.get("errors").and_then(Value::as_array).into_iter().flatten().filter_map(|e| e.get("code").and_then(Value::as_i64)).collect()
    }

    /// The errors Cloudflare named, `message (code N)`, joined.
    fn why(&self) -> String {
        let list: Vec<String> = self
            .v
            .get("errors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|e| {
                let m = e.get("message").and_then(Value::as_str).unwrap_or("error");
                match e.get("code").and_then(Value::as_i64) {
                    Some(c) => format!("{m} (code {c})"),
                    None => m.to_string(),
                }
            })
            .collect();
        if list.is_empty() { format!("HTTP {}", self.status) } else { list.join("; ") }
    }

    /// The error for a refused step: the token's problem, a missing permission, or the API's words.
    fn error(&self, what: &str) -> DeployError {
        let why = format!("{what}: {}", self.why());
        let codes = self.codes();
        if self.status == 401 || codes.contains(&10000) && self.status != 403 {
            DeployError::Token(why)
        } else if self.status == 403 || codes.contains(&10000) {
            DeployError::Permission(why)
        } else {
            DeployError::Api(why)
        }
    }

    fn result(&self) -> &Value {
        self.v.get("result").unwrap_or(&Value::Null)
    }
}

impl Api {
    /// A session with `token` against Cloudflare's API.
    pub fn new(token: &str) -> Api {
        Api::with_base(API_BASE, token)
    }

    /// The same against another base URL (tests: a loopback fake).
    pub fn with_base(base: &str, token: &str) -> Api {
        Api { base: base.trim_end_matches('/').to_string(), token: clean_token(token).into_bytes(), test_url: None, settle: Duration::from_secs(90) }
    }

    fn auth(&self) -> (String, String) {
        ("Authorization".into(), format!("Bearer {}", String::from_utf8_lossy(&self.token)))
    }

    async fn call(&self, method: &str, path: &str, content_type: Option<&str>, body: Vec<u8>) -> Result<Answer, DeployError> {
        let mut headers = vec![self.auth()];
        if let Some(c) = content_type {
            headers.push(("Content-Type".into(), c.into()));
        }
        let req = SmallRequest { method: method.into(), url: format!("{}{path}", self.base), headers, body };
        let r = request_small(req, VERSION, 4 << 20, TIMEOUT).await.map_err(|e| DeployError::Network(e.to_string()))?;
        let v = serde_json::from_slice(&r.body).unwrap_or(Value::Null);
        Ok(Answer { status: r.status, v })
    }

    async fn json(&self, method: &str, path: &str, body: Value) -> Result<Answer, DeployError> {
        self.call(method, path, Some("application/json"), body.to_string().into_bytes()).await
    }

    /// Uploads the Worker (or a newer version of it) and puts it on workers.dev; returns its address and account.
    /// The self-test follows ([`deploy`] runs both).
    pub async fn upload(&self, progress: &(dyn Fn(Stage) + Send + Sync)) -> Result<Deployed, DeployError> {
        if self.token.is_empty() {
            return Err(DeployError::Token("no token was given".into()));
        }
        progress(Stage::Token);
        if !self.token.starts_with(b"cfat_") {
            let a = self.call("GET", "/user/tokens/verify", None, Vec::new()).await?;
            if !a.ok() {
                return Err(DeployError::Token(a.why()));
            }
            let status = a.result().get("status").and_then(Value::as_str).unwrap_or("");
            if status != "active" {
                return Err(DeployError::Token(format!("it is {}", if status.is_empty() { "not active" } else { status })));
            }
        }

        progress(Stage::Account);
        let a = self.call("GET", "/accounts?per_page=50", None, Vec::new()).await?;
        if !a.ok() {
            return Err(a.error("listing the accounts"));
        }
        let first = a.result().as_array().and_then(|l| l.first()).ok_or(DeployError::NoAccount)?;
        let account = first.get("id").and_then(Value::as_str).filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric())).ok_or(DeployError::NoAccount)?.to_string();
        let account_name = first.get("name").and_then(Value::as_str).unwrap_or("").to_string();

        progress(Stage::Upload);
        let a = self.call("GET", &format!("/accounts/{account}/workers/scripts"), None, Vec::new()).await?;
        if !a.ok() {
            return Err(a.error("listing the Workers"));
        }
        let tag = a
            .result()
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s.get("id").and_then(Value::as_str) == Some(SCRIPT_NAME))
            .and_then(|s| s.get("migration_tag").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        if !tag.is_empty() && tag != MIGRATION_TAG {
            return Err(DeployError::Api(format!(
                "a Worker called {SCRIPT_NAME} is already on the account, with a Durable Object migration Seedwell does not know ({tag}); delete it in the dashboard (Workers & Pages) and set up again"
            )));
        }
        let (content_type, body) = upload_form(&tag, &random_boundary());
        let a = self.call("PUT", &format!("/accounts/{account}/workers/scripts/{SCRIPT_NAME}"), Some(&content_type), body).await?;
        if !a.ok() {
            return Err(a.error("uploading the Worker"));
        }

        progress(Stage::Address);
        let path = format!("/accounts/{account}/workers/subdomain");
        let a = self.call("GET", &path, None, Vec::new()).await?;
        let mut new_subdomain = None;
        let subdomain = match a.result().get("subdomain").and_then(Value::as_str).filter(|s| !s.is_empty() && a.ok()) {
            Some(s) => s.to_string(),
            None if a.ok() || a.codes().contains(&10007) || a.status == 404 => {
                let want = format!("seedwell-{}", &hex(&random_bytes::<4>())[..6]);
                let made = self.json("PUT", &path, json!({ "subdomain": want })).await?;
                let got = made.result().get("subdomain").and_then(Value::as_str).filter(|s| !s.is_empty() && made.ok()).map(str::to_owned);
                match got {
                    Some(s) => {
                        new_subdomain = Some(s.clone());
                        s
                    }
                    None => return Err(DeployError::Subdomain(made.why())),
                }
            }
            None => return Err(a.error("reading the workers.dev subdomain")),
        };
        let a = self.json("POST", &format!("/accounts/{account}/workers/scripts/{SCRIPT_NAME}/subdomain"), json!({ "enabled": true, "previews_enabled": false })).await?;
        if !a.ok() {
            return Err(a.error("putting the Worker on workers.dev"));
        }
        let url = self.test_url.clone().unwrap_or_else(|| workers_dev_url(SCRIPT_NAME, &subdomain));
        Ok(Deployed { url, account: account_name, new_subdomain })
    }
}

/// Uploads, puts on workers.dev and tests the rendezvous with the token in `api` (dropped, and wiped, when this
/// returns). `progress` hears each stage.
pub async fn deploy(api: Api, progress: &(dyn Fn(Stage) + Send + Sync)) -> Result<Deployed, DeployError> {
    let d = api.upload(progress).await?;
    let settle = api.settle;
    drop(api);
    progress(Stage::Test);
    let r = Rendezvous::new(&d.url).map_err(|e| DeployError::Test(e.to_string()))?;
    test_until(&r, settle).await.map_err(|e| DeployError::Test(e.to_string()))?;
    Ok(d)
}

/// Runs the self-test until it passes or `within` is over (a new workers.dev address can take a while to answer).
pub async fn test_until(r: &Rendezvous, within: Duration) -> Result<(), ClientError> {
    let end = tokio::time::Instant::now() + within;
    loop {
        match r.self_test().await {
            Ok(()) => return Ok(()),
            Err(e) if tokio::time::Instant::now() + Duration::from_secs(3) >= end => return Err(e),
            // Not answering yet, or not yet the new script: try again.
            Err(ClientError::Network(_) | ClientError::NotRendezvous(_) | ClientError::Refused { status: 404 | 500..=599, .. }) => {
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut b);
    b
}

fn random_boundary() -> String {
    format!("seedwell-{}", hex(&random_bytes::<12>()))
}

/// The upload's metadata: the module, the Durable Object binding, and the migration that makes its class (SQLite
/// storage) unless the script has it already (`tag` is the script's migration tag, empty for a new script).
pub fn metadata(tag: &str) -> Value {
    let mut m = json!({
        "main_module": "worker.js",
        "compatibility_date": COMPATIBILITY_DATE,
        "bindings": [{ "type": "durable_object_namespace", "name": BINDING, "class_name": CLASS_NAME }],
    });
    if tag.is_empty() {
        m["migrations"] = json!({ "new_tag": MIGRATION_TAG, "new_sqlite_classes": [CLASS_NAME] });
    }
    m
}

/// The multipart form of the upload: its content type and body.
pub fn upload_form(tag: &str, boundary: &str) -> (String, Vec<u8>) {
    let mut b = Vec::with_capacity(WORKER_JS.len() + 1024);
    let part = |b: &mut Vec<u8>, disposition: &str, ty: &str, body: &[u8]| {
        b.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; {disposition}\r\nContent-Type: {ty}\r\n\r\n").as_bytes());
        b.extend_from_slice(body);
        b.extend_from_slice(b"\r\n");
    };
    part(&mut b, "name=\"metadata\"", "application/json", metadata(tag).to_string().as_bytes());
    part(&mut b, "name=\"worker.js\"; filename=\"worker.js\"", "application/javascript+module", WORKER_JS.as_bytes());
    b.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_upload_makes_the_sqlite_class_and_a_later_one_leaves_the_migration_out() {
        let first = metadata("");
        assert_eq!(first["main_module"], "worker.js");
        assert_eq!(first["bindings"][0], json!({"type": "durable_object_namespace", "name": "RENDEZVOUS", "class_name": "Rendezvous"}));
        assert_eq!(first["migrations"], json!({"new_tag": "v1", "new_sqlite_classes": ["Rendezvous"]}));
        assert!(metadata("v1").get("migrations").is_none());
        let (ty, body) = upload_form("", "B");
        assert_eq!(ty, "multipart/form-data; boundary=B");
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with("--B\r\nContent-Disposition: form-data; name=\"metadata\"\r\nContent-Type: application/json\r\n\r\n{"));
        assert!(text.contains("--B\r\nContent-Disposition: form-data; name=\"worker.js\"; filename=\"worker.js\"\r\nContent-Type: application/javascript+module\r\n\r\n// Seedwell rendezvous"));
        assert!(text.ends_with("\r\n--B--\r\n"));
    }

    #[test]
    fn the_address_is_the_scripts_name_on_the_accounts_subdomain() {
        assert_eq!(workers_dev_url("seedwell-rendezvous", "kristian"), "https://seedwell-rendezvous.kristian.workers.dev");
        assert_eq!(workers_dev_url("seedwell-rendezvous", "Kristian.workers.dev"), "https://seedwell-rendezvous.kristian.workers.dev");
    }

    #[test]
    fn a_pasted_token_is_cleaned_and_never_printed() {
        assert_eq!(clean_token("  Bearer abc def\n"), "abcdef");
        let api = Api::new("secret-token-123");
        assert!(!format!("{api:?}").contains("secret"));
    }
}
