//! The dial loop's backoff against devices that answer: a refusal that no longer applies is not waited out for
//! minutes, and a session being set up is neither dialed again nor taken for a dial that fails. In-crate, to see and
//! set the address book's backoff.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use trs_sync_proto::ids::FolderId;
use trs_sync_proto::members::Role;

use crate::service::{Options, SyncHandle, held_until};
use crate::shared::lock;

async fn device(dir: &std::path::Path, name: &str) -> SyncHandle {
    let secrets = Arc::new(crate::secrets::FileSecrets::new(dir.join("secrets")));
    let mut o = Options::new(dir.to_path_buf(), "Seedwell test".into(), secrets);
    o.default_device_name = name.into();
    o.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
    o.lan_discovery = false;
    SyncHandle::start(o).await.unwrap()
}

async fn until(secs: u64, what: &str, mut ok: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for: {what}");
}

fn addr(h: &SyncHandle) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), h.port())
}

/// B joins A's folder with a link A revoked: B dials, A turns each session down. Returns once B was turned down
/// (its dial connected and ended, B not connected).
async fn refused_joiner(work: &std::path::Path) -> (SyncHandle, SyncHandle, FolderId) {
    let (a, b) = (device(&work.join("a"), "Alpha").await, device(&work.join("b"), "Beta").await);
    let docs = work.join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let id: FolderId = a.add_folder(docs).await.unwrap().id.parse().unwrap();
    let inv = a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap();
    a.revoke_invite(&id, &inv.id).unwrap();
    b.join_folder(&inv.link, work.join("B")).await.unwrap();
    b.add_address(&id, addr(&a)).unwrap();
    let at = addr(&a);
    // Noted as refused, not only dialed: a dial is counted and tried before it connects (in_setup comes later), so
    // "tried and not in setup" also holds while the first dial is still on its way.
    until(30, "B was turned down", || {
        let book = lock(&b.inner.book);
        b.test_dials_made() > 0 && book.tried.contains_key(&at) && book.refused.contains(&at) && !book.in_setup.contains(&at)
    })
    .await;
    assert!(b.inner.ctx.registry.peer(&a.device_id()).is_none());
    (a, b, id)
}

/// B's backoff for A as after a long run of refusals (two minutes to the next dial on the old steps).
fn held_back_long(b: &SyncHandle, at: SocketAddr, since: Duration) {
    let mut book = lock(&b.inner.book);
    book.tried.insert(at, (Instant::now() - since, 7));
}

/// Fails if a joiner that was turned down for a long time still waits out its backoff once the user gives it a new
/// link: the backoff (up to five minutes) is kept by address, across the folder's removal and a new join. The new
/// link and the address given with it are dialed at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_joiner_given_a_new_link_connects_at_once() {
    let work = tempfile::tempdir().unwrap();
    let (a, b, id) = refused_joiner(work.path()).await;
    held_back_long(&b, addr(&a), Duration::ZERO);
    b.remove_folder(&id).await.unwrap();
    let link = a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link;
    b.join_folder(&link, work.path().join("B2")).await.unwrap();
    b.add_address(&id, addr(&a)).unwrap();
    let ad = a.device_id();
    until(10, "B connected with the new link", || b.inner.ctx.registry.peer(&ad).is_some()).await;
    a.shutdown().await;
    b.shutdown().await;
}

/// Fails if a device that turned sessions down is waited for minutes: what made it refuse (a link used up, its
/// folder paused or still opening) may be over at any moment, and over the internet only the joiner dials. After a
/// long run of refusals the next dial is due within 30 s.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_is_waited_out_for_at_most_thirty_seconds() {
    let work = tempfile::tempdir().unwrap();
    let (a, b, _) = refused_joiner(work.path()).await;
    let at = addr(&a);
    {
        // (Under one lock: the dial loop does not change the entry in between.)
        let mut book = lock(&b.inner.book);
        let now = Instant::now();
        book.tried.insert(at, (now - Duration::from_secs(31), 7));
        assert_eq!(held_until(&book, at, now), None, "31 s after a refusal the dial goes");
        book.tried.insert(at, (now, 7));
        let due = held_until(&book, at, now).expect("held back just after a refusal");
        assert!(due <= now + Duration::from_secs(30), "due in {:?}", due - now);
    }
    a.shutdown().await;
    b.shutdown().await;
}

/// Fails if a refusal outlives a change of what this device offers: a folder opening here may be what the device
/// that turned the session down was missing, so its address is dialed again at once (an address that never
/// answered keeps its wait).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_folder_opening_here_forgets_the_refusals() {
    let work = tempfile::tempdir().unwrap();
    let (a, b, _) = refused_joiner(work.path()).await;
    let (at, silent) = (addr(&a), SocketAddr::from(([127, 0, 0, 1], 9)));
    b.test_dials_left(Some(0));
    held_back_long(&b, at, Duration::ZERO);
    lock(&b.inner.book).tried.insert(silent, (Instant::now(), 7));
    let other = work.path().join("Other");
    std::fs::create_dir_all(&other).unwrap();
    b.add_folder(other).await.unwrap();
    let (forgotten, kept) = {
        let book = lock(&b.inner.book);
        (!book.tried.contains_key(&at), book.tried.contains_key(&silent))
    };
    assert!(forgotten, "the refusal's backoff went");
    assert!(kept, "a dial that never connected keeps its wait");
    a.shutdown().await;
    b.shutdown().await;
}

/// Fails if a session being set up counts as a failing dial: while A takes long to set B's session up, B must not
/// dial A again (it did every backoff step, sessions crossing at A) nor take A as unreachable directly (the relay
/// and hole punch start 2 s after that).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_slow_setup_is_neither_dialed_again_nor_taken_for_a_failing_dial() {
    let work = tempfile::tempdir().unwrap();
    let (a, b) = (device(&work.path().join("a"), "Alpha").await, device(&work.path().join("b"), "Beta").await);
    let docs = work.path().join("Docs");
    std::fs::create_dir_all(&docs).unwrap();
    let id: FolderId = a.add_folder(docs).await.unwrap().id.parse().unwrap();
    let link = a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link;
    a.test_setup_wait(Duration::from_secs(8));
    b.join_folder(&link, work.path().join("B")).await.unwrap();
    b.add_address(&id, addr(&a)).unwrap();
    until(30, "B's session waits in its setup at A", || a.test_setups_waiting() > 0).await;
    let dials = b.test_dials_made();
    // Past the dial backoff's first steps (1 s, 2 s) and the relay's grace (2 s), the setup still running.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(a.test_setups_waiting() > 0, "the setup still runs");
    assert_eq!(b.test_dials_made(), dials, "no second dial during the setup");
    assert!(!b.direct_failing(a.device_id()), "a device setting a session up is not failing");
    a.test_setup_wait(Duration::ZERO);
    a.shutdown().await;
    b.shutdown().await;
}
