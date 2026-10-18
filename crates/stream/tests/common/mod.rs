#![allow(dead_code)]
pub mod client;
pub mod fake;
pub mod fixtures;

use std::sync::{Arc, Mutex};
use trs_stream::{Config, Event, StreamServer};

pub const MIB: u64 = 1024 * 1024;

pub type Events = Arc<Mutex<Vec<Event>>>;

/// A server whose events are recorded.
pub async fn server_with(mut cfg: Config) -> (StreamServer, Events) {
    let ev: Events = Arc::new(Mutex::new(Vec::new()));
    let e2 = ev.clone();
    cfg.on_event = Some(Arc::new(move |e| e2.lock().unwrap().push(e.clone())));
    (StreamServer::start(cfg).await.unwrap(), ev)
}

pub async fn server() -> (StreamServer, Events) {
    server_with(Config::default()).await
}
