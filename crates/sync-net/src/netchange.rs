//! Network changes (SPEC §7G.3): a device that moves to another network finds its devices again at once, instead of
//! after its sessions time out (3 × 90 s of silence), the next DHT round (15 min) and the dial backoffs.
//!
//! The app tells the service when the operating system says the network changed (the Mac's path monitor, Windows'
//! address-change notification, waking from sleep: [`SyncHandle::network_changed`], through the FFI, together with
//! the torrent engine, which refreshes the port mapping on the same signal). Without that signal (a daemon, or a
//! change the OS reports late) the service looks itself every [`CHECK_EVERY`]: whether this device's addresses
//! changed, and whether a session's local address is gone. A check costs a few socket calls and nothing on the
//! network.
//!
//! On a change: sessions whose local address is gone are closed (they cannot carry anything again); the others get a
//! ping (a dead one fails sooner); LAN discovery announces now and the DHT round runs now; the dial backoffs are
//! cleared and every member that is not connected is dialed again; and the rendezvous gets a new record (the public
//! address from the port mapping follows a moment later, and is published when it arrives).

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
use seedwell_sync_proto::msg::Message;

use crate::service::SyncHandle;
use crate::session::Via;
use crate::shared::lock;

/// How often the service looks for a change itself.
pub const CHECK_EVERY: Duration = Duration::from_secs(15);

/// What the service knows of the network.
#[derive(Default)]
pub(crate) struct NetWatch {
    /// This device's addresses at the last check (`None` before the first).
    last: Mutex<Option<BTreeSet<IpAddr>>>,
    /// **Tests:** addresses that count as gone at the next check (then forgotten).
    test_gone: Mutex<BTreeSet<IpAddr>>,
    /// Changes handled since the start.
    pub(crate) changes: AtomicU64,
    /// The DHT round runs now.
    pub(crate) dht_wake: Notify,
}

/// Whether `ip` is still an address of this device: a socket can be bound to it. Only "not available" counts as gone
/// (a socket that cannot be made for another reason says nothing).
fn present(ip: IpAddr) -> bool {
    match UdpSocket::bind(SocketAddr::new(ip, 0)) {
        Ok(_) => true,
        Err(e) => e.kind() != std::io::ErrorKind::AddrNotAvailable,
    }
}

impl SyncHandle {
    /// The network changed (the operating system said so: another network, an address gone or new, a wake from
    /// sleep). Sessions whose local address is gone close; LAN discovery and the DHT announce now; every member not
    /// connected is dialed again without waiting for its backoff; the rendezvous gets a new record. Cheap; calling it
    /// for nothing costs a round of dials.
    pub fn network_changed(&self) {
        self.on_network_change("the system says the network changed");
    }

    pub(crate) fn on_network_change(&self, why: &str) {
        if self.inner.stopping.load(Ordering::Relaxed) {
            return;
        }
        let n = self.inner.netwatch.changes.fetch_add(1, Ordering::SeqCst) + 1;
        // Sessions come up again: a fetch waits for them a moment before it says no device that has the file is here.
        self.inner.ctx.settle();
        let dropped = self.drop_stranded_sessions();
        eprintln!("seedwell sync: network change {n} ({why}): {dropped} session(s) on a gone address closed; finding devices again");
        // The others: a ping now, so one the change broke fails sooner.
        for p in self.inner.ctx.registry.peers() {
            if p.via != Via::Relay {
                let _ = p.control.try_send(Message::Ping);
            }
        }
        {
            let mut book = lock(&self.inner.book);
            book.tried.clear();
            book.refused.clear();
        }
        self.inner.net.clear_backoffs();
        // Whether devices could reach this one was about the old network.
        self.inner.ctx.reach.forget();
        crate::rendezvous::network_changed(self);
        self.inner.lan.wake.notify_one();
        self.inner.netwatch.dht_wake.notify_one();
        self.inner.wake_dialer.notify_one();
    }

    /// Closes the direct sessions whose local address is no longer this device's; returns how many.
    fn drop_stranded_sessions(&self) -> usize {
        let gone_now: BTreeSet<IpAddr> = std::mem::take(&mut *lock(&self.inner.netwatch.test_gone));
        let mut dropped = 0;
        let mut checked: Vec<(IpAddr, bool)> = Vec::new();
        for p in self.inner.ctx.registry.peers() {
            if p.via == Via::Relay || p.local.ip().is_unspecified() {
                continue;
            }
            let ip = p.local.ip();
            let here = match checked.iter().find(|(a, _)| *a == ip) {
                Some((_, h)) => *h,
                None => {
                    let h = !gone_now.contains(&ip) && present(ip);
                    checked.push((ip, h));
                    h
                }
            };
            if !here {
                p.close();
                dropped += 1;
            }
        }
        dropped
    }

    /// This device's addresses as far as Folder Sync is concerned: those of its networks when the sync port listens on
    /// all of them, else the one it is bound to.
    fn own_ips(&self) -> BTreeSet<IpAddr> {
        let bind = self.inner.bind;
        if bind.is_unspecified() { seedwell_platform::net::lan_addresses().into_iter().collect() } else { [bind].into() }
    }

    /// One check without the operating system's signal: a change of this device's addresses, or a session on a gone
    /// address, is handled as a network change. Returns whether it was one.
    pub(crate) fn check_network(&self) -> bool {
        let now = self.own_ips();
        let test_gone = !lock(&self.inner.netwatch.test_gone).is_empty();
        let changed = {
            let mut last = lock(&self.inner.netwatch.last);
            let changed = last.as_ref().is_some_and(|l| *l != now);
            *last = Some(now);
            changed
        };
        if changed {
            self.on_network_change("this device's addresses changed");
            return true;
        }
        let stranded = test_gone || self.inner.ctx.registry.peers().iter().any(|p| p.via != Via::Relay && !p.local.ip().is_unspecified() && !present(p.local.ip()));
        if stranded {
            self.on_network_change("a session's local address is gone");
        }
        stranded
    }

    /// **Tests only:** `ips` count as gone at the next network change or check (then they are this device's again).
    #[doc(hidden)]
    pub fn test_addresses_gone(&self, ips: &[IpAddr]) {
        lock(&self.inner.netwatch.test_gone).extend(ips.iter().copied());
    }

    /// **Tests only:** network changes handled since the start.
    #[doc(hidden)]
    pub fn test_network_changes(&self) -> u64 {
        self.inner.netwatch.changes.load(Ordering::SeqCst)
    }
}

/// Looks for a change every [`CHECK_EVERY`] until the service stops (the task is aborted).
pub(crate) async fn run(h: SyncHandle) {
    let mut every = tokio::time::interval(CHECK_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        every.tick().await;
        if h.inner.stopping.load(Ordering::Relaxed) {
            return;
        }
        // getifaddrs and a socket per address: off the runtime's threads.
        let h2 = h.clone();
        let _ = tokio::task::spawn_blocking(move || h2.check_network()).await;
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::time::Instant;

    use seedwell_sync_proto::members::Role;

    use super::*;
    use crate::service::Options;

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

    #[test]
    fn an_address_of_this_device_is_present_and_another_is_not() {
        assert!(present(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        // TEST-NET-3: never an address of this machine.
        assert!(!present("203.0.113.77".parse().unwrap()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_network_change_drops_a_session_on_a_gone_address_and_redials_at_once_past_its_backoff() {
        let (da, db, df) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (device(da.path(), "Anna").await, device(db.path(), "Bo").await);
        let docs = df.path().join("Docs");
        std::fs::create_dir_all(&docs).unwrap();
        let id: seedwell_sync_proto::FolderId = a.add_folder(docs).await.unwrap().id.parse().unwrap();
        b.join_folder(&a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, df.path().join("B")).await.unwrap();
        let bob = b.device_id();
        until(20, "connected", || a.inner.ctx.registry.peer(&bob).is_some() && b.inner.ctx.registry.peer(&a.device_id()).is_some()).await;
        // Bo dialed: Anna knows where Bo listens only from the record Bo sends once the session is up (a busy machine
        // had not read it yet, so Anna had nobody to dial and the test waited for a dial that could not come).
        until(20, "Anna knows Bo's address", || lock(&a.inner.book).by_device.get(&bob).is_some_and(|s| !s.is_empty())).await;
        let first = a.inner.ctx.registry.peer(&bob).unwrap();
        // Bo's address is held back for minutes on both sides (as after failed dials), and Bo dials nobody: only a
        // dial by Anna past her backoff brings the session back soon.
        let hold = |h: &SyncHandle| {
            let mut book = lock(&h.inner.book);
            let addrs: Vec<SocketAddr> = book.by_device.values().flatten().copied().collect();
            for a in addrs {
                book.tried.insert(a, (std::time::Instant::now(), 8));
            }
        };
        b.test_dials_left(Some(0));
        hold(&a);
        let sessions = a.test_sessions();
        // Anna's local address goes (as when she leaves a network): the session on it closes.
        a.test_addresses_gone(&[IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        hold(&a);
        a.network_changed();
        assert!(first.is_closed(), "the session on the gone address closed at once");
        until(10, "Anna dialed Bo again at once, past the backoff", || {
            a.test_sessions() > sessions && a.inner.ctx.registry.peer(&bob).is_some_and(|p| !Arc::ptr_eq(&p, &first) && !p.is_closed())
        })
        .await;
        assert_eq!(a.test_network_changes(), 1);
        assert!(lock(&a.inner.book).tried.values().all(|t| t.1 <= 1), "the backoffs were cleared");
        a.shutdown().await;
        b.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_check_notices_a_gone_session_address_without_the_systems_signal() {
        let (da, db, df) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (device(da.path(), "Anna").await, device(db.path(), "Bo").await);
        let docs = df.path().join("Docs");
        std::fs::create_dir_all(&docs).unwrap();
        let id: seedwell_sync_proto::FolderId = a.add_folder(docs).await.unwrap().id.parse().unwrap();
        b.join_folder(&a.create_invite(&id, Role::ReadWrite, 1, 7).unwrap().link, df.path().join("B")).await.unwrap();
        let bob = b.device_id();
        until(20, "connected", || a.inner.ctx.registry.peer(&bob).is_some()).await;
        assert!(!a.check_network(), "nothing changed");
        let first = a.inner.ctx.registry.peer(&bob).unwrap();
        a.test_addresses_gone(&[IpAddr::V4(Ipv4Addr::LOCALHOST)]);
        assert!(a.check_network(), "a session's address is gone");
        assert!(first.is_closed());
        assert_eq!(a.test_network_changes(), 1);
        a.shutdown().await;
        b.shutdown().await;
    }
}
