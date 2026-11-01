//! Integration tests of trs-rendezvous: the client against a loopback rendezvous, and the setup against a loopback
//! fake of the Cloudflare API with the request shapes of its reference. Nothing reaches the Internet.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use trs_rendezvous::client::{ClientError, Rendezvous, WriteKey};
use trs_rendezvous::cloudflare::{Api, DeployError, Stage, deploy};
use trs_rendezvous::testkit::{Account, fake_cloudflare, fake_rendezvous};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_record_is_written_read_and_kept_for_its_first_key() {
    let rv = fake_rendezvous().await;
    let r = Rendezvous::new(&rv.url()).unwrap();
    r.info().await.unwrap();
    let id = "ab".repeat(32);
    let (k, other) = (WriteKey::generate().unwrap(), WriteKey::generate().unwrap());
    assert_eq!(r.get(&id).await.unwrap(), None);
    r.put(&id, b"sealed", &k, 1000).await.unwrap();
    assert_eq!(r.get(&id).await.unwrap().as_deref(), Some(b"sealed".as_slice()));
    assert!(matches!(r.put(&id, b"x", &other, 2000).await, Err(ClientError::Refused { status: 403, .. })), "another key");
    assert!(matches!(r.put(&id, b"x", &k, 1000).await, Err(ClientError::Refused { status: 409, .. })), "a replay");
    r.delete(&id, &k, 3000).await.unwrap();
    assert_eq!(r.get(&id).await.unwrap(), None);
    r.self_test().await.unwrap();
    // Down: a network error, not a hang.
    rv.down.store(true, std::sync::atomic::Ordering::SeqCst);
    assert!(matches!(r.get(&id).await, Err(ClientError::Network(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_setup_uploads_the_worker_makes_the_subdomain_and_tests_it() {
    let cf = fake_cloudflare(Account { token: "tok-123".into(), subdomain: None, migration_tag: None }).await;
    let rv = fake_rendezvous().await;
    let mut api = Api::with_base(&cf.url(), " tok-123\n");
    api.test_url = Some(rv.url());
    api.settle = Duration::from_secs(5);
    let stages = Arc::new(Mutex::new(Vec::new()));
    let s2 = stages.clone();
    let d = deploy(api, &move |s| s2.lock().unwrap().push(s)).await.unwrap();
    assert_eq!(d.url, rv.url());
    assert_eq!(d.account, "Kristian's Account");
    let sub = d.new_subdomain.clone().expect("a subdomain was made");
    assert!(sub.starts_with("seedwell-") && sub.len() == 15, "{sub}");
    assert_eq!(*stages.lock().unwrap(), vec![Stage::Token, Stage::Account, Stage::Upload, Stage::Address, Stage::Test]);

    // The requests, in order and in Cloudflare's shapes.
    let reqs = cf.requests();
    let line: Vec<String> = reqs.iter().map(|r| format!("{} {}", r.method, r.path)).collect();
    assert_eq!(
        line,
        [
            "GET /user/tokens/verify",
            "GET /accounts?per_page=50",
            "GET /accounts/acc0123/workers/scripts",
            "PUT /accounts/acc0123/workers/scripts/seedwell-rendezvous",
            "GET /accounts/acc0123/workers/subdomain",
            "PUT /accounts/acc0123/workers/subdomain",
            "POST /accounts/acc0123/workers/scripts/seedwell-rendezvous/subdomain",
        ]
    );
    assert!(reqs.iter().all(|r| r.header("authorization") == Some("Bearer tok-123")), "the cleaned token on every call");
    let upload = &reqs[3];
    let body = String::from_utf8_lossy(&upload.body);
    let meta: Value = serde_json::from_str(body.split("\r\n\r\n").nth(1).unwrap().split("\r\n--").next().unwrap()).unwrap();
    assert_eq!(meta["migrations"]["new_sqlite_classes"][0], "Rendezvous");
    assert_eq!(meta["bindings"][0]["type"], "durable_object_namespace");
    assert!(body.contains(trs_rendezvous::WORKER_JS));
    let enable: Value = serde_json::from_slice(&reqs[6].body).unwrap();
    assert_eq!(enable, serde_json::json!({"enabled": true, "previews_enabled": false}));
    // The self-test wrote, read and deleted a record.
    assert_eq!((rv.count("PUT", "/v1/r/"), rv.count("GET", "/v1/r/"), rv.count("DELETE", "/v1/r/")), (1, 1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_keeps_the_migration_and_the_subdomain_and_a_bad_token_is_said_so() {
    let cf = fake_cloudflare(Account { token: "tok".into(), subdomain: Some("kristian".into()), migration_tag: Some("v1".into()) }).await;
    let rv = fake_rendezvous().await;
    let mut api = Api::with_base(&cf.url(), "tok");
    api.test_url = Some(rv.url());
    let d = deploy(api, &|_| {}).await.unwrap();
    assert_eq!(d.new_subdomain, None);
    let upload = cf.requests().into_iter().find(|r| r.method == "PUT").unwrap();
    assert!(!String::from_utf8_lossy(&upload.body).contains("\"migrations\""), "the class is there: no migration");
    assert!(!cf.requests().iter().any(|r| r.method == "PUT" && r.path.ends_with("/workers/subdomain")));

    // Without the test address the Worker's address is its workers.dev one (the API's subdomain).
    let api = Api::with_base(&cf.url(), "tok");
    let d = api.upload(&|_| {}).await.unwrap();
    assert_eq!(d.url, "https://seedwell-rendezvous.kristian.workers.dev");

    let e = deploy(Api::with_base(&cf.url(), "wrong"), &|_| {}).await.unwrap_err();
    assert!(matches!(e, DeployError::Token(_)), "{e:?}");
    assert!(e.to_string().starts_with("Cloudflare did not accept the token"), "{e}");
    assert!(!e.to_string().contains("wrong"), "the token is never in a message");
}
