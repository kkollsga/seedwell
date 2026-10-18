//! A taken listen port must not stop the engine.
use crate::common;
use std::time::Duration;

use common::*;
use trs_engine::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_taken_listen_port_falls_back_to_a_free_one_and_says_so() {
    // Another app on the port: a plain TCP listener.
    let squatter = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = squatter.local_addr().unwrap().port();

    let seeder = node(|c| c.listen_port = taken).await;
    let using = seeder.h.listen_port();
    assert_ne!(using, taken);
    assert_ne!(using, 0);
    let snap = seeder.h.snapshot().await.unwrap();
    assert_eq!(snap.listen_port, using);
    let f = snap.port_fallback.expect("the fallback is reported");
    assert_eq!((f.wanted, f.using), (taken, using));
    assert_eq!(f.message, format!("Port {taken} is in use by another app; using {using} for now"));
    assert_eq!(seeder.h.port_fallback(), Some(f));
    // The configuration keeps what the user chose: the next start tries the port again.
    assert_eq!(seeder.h.config().await.unwrap().listen_port, taken);

    // The engine really serves on the new port (TCP, and uTP/DHT share its number).
    let t = standard_torrent(500_000, None, false, 41);
    t.write_to(seeder.dir.path());
    seeder.h.add_torrent_file(t.bytes.clone(), AddOptions::default()).await.unwrap();
    wait_done(&seeder, &t.id, 30).await;
    let leech = node(|_| {}).await;
    leech.h.add_torrent_file(t.bytes.clone(), AddOptions { peers: vec![seeder.addr()], ..Default::default() }).await.unwrap();
    wait_done(&leech, &t.id, 60).await;
    t.verify(leech.dir.path(), &[]);
    assert!(leech.h.snapshot().await.unwrap().port_fallback.is_none(), "a free port is not a fallback");
    drop(squatter);
    let _ = Duration::from_secs(0);
}
