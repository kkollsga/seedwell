//! The rendezvous (SPEC §7G.3): an address book on the user's own Cloudflare account, where each device keeps a
//! sealed, signed record of where it can be reached ([`trs_sync_proto::rendezvous`]) and looks up the devices it
//! shares something with when they are not connected.
//!
//! * **Publishing** (only with a rendezvous set up, `settings.rendezvous`): at the start, on a network change, when
//!   this device's addresses change (looked at every [`LOOK`]), and every [`REFRESH`]. At most one write per
//!   [`MIN_GAP`] ([`MIN_GAP_CHANGE`] after a network change); a failed write is tried again with a growing wait.
//!   Writes are signed with this device's write key for that rendezvous (random, in the secret store; never the
//!   device key).
//! * **Looking up**: the dial loop asks for each member, channel member or conversation device it wants and has no
//!   session with ([`look_up`]), with a backoff per device ([`LOOKUP_STEPS`]; a network change clears it). It asks the
//!   rendezvous this device uses (the user's other devices publish there too) and the ones the device itself named:
//!   in its signed address record (an entry `rendezvous+https://…`, which older versions skip as an address they
//!   cannot parse) and in a link it made (the `a=` block's rendezvous field). The addresses found are dialed like any
//!   other; the session's TLS key still decides who is on the other end.
//! * Only addresses and a time go there: never file data, chat content, folder ids or names. When the rendezvous is
//!   down nothing else changes: LAN discovery, the DHT, relays and the addresses already known work as before.

use std::collections::{BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use trs_rendezvous::client::{ClientError, Rendezvous, WriteKey, normalize_url};
use trs_sync_proto::ids::DeviceId;
use trs_sync_proto::rendezvous::{Record, record_id_hex};

use crate::service::{SyncError, SyncHandle};
use crate::shared::lock;

/// A record is written again after this long even when nothing changed (the rendezvous keeps one 7 days).
pub const REFRESH: Duration = Duration::from_secs(30 * 60);
/// How often the publisher looks whether this device's addresses changed.
pub const LOOK: Duration = Duration::from_secs(30);
/// Between two writes at least.
pub const MIN_GAP: Duration = Duration::from_secs(60);
/// Between a write and the one a network change asks for, at least.
pub const MIN_GAP_CHANGE: Duration = Duration::from_secs(5);
/// The waits between lookups of one device that is not found, in seconds (then the last one).
pub const LOOKUP_STEPS: [u64; 7] = [0, 30, 60, 120, 300, 600, 1800];
/// The waits after failed writes, in seconds (then the last one).
const PUBLISH_STEPS: [u64; 5] = [30, 60, 120, 300, 900];
/// An address record's entry that names the device's rendezvous: `rendezvous+https://…`.
pub const RECORD_PREFIX: &str = "rendezvous+";
/// Records older than this are not used (the rendezvous drops them a week after their last write).
const MAX_AGE_MS: u64 = 7 * 24 * 3600 * 1000;

/// What the rendezvous is doing, for Settings, `seedwell sync settings` and agents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RendezvousStatus {
    /// Its address (empty: none set up).
    pub url: String,
    /// When this device's record was last written, Unix seconds (0: not yet since the start).
    pub last_publish_s: u64,
    /// What went wrong last (empty: nothing since the last write that worked).
    pub last_error: String,
    /// When that was, Unix seconds.
    pub last_error_s: u64,
    /// Devices found through a rendezvous since the start.
    pub found: u64,
    /// When the last one was found, Unix seconds (0: none yet).
    pub last_found_s: u64,
}

#[derive(Default)]
struct Published {
    url: String,
    addrs: Vec<SocketAddr>,
    at: Option<Instant>,
    fails: u32,
    retry_at: Option<Instant>,
}

/// The service's rendezvous state.
#[derive(Default)]
pub(crate) struct State {
    wake: Notify,
    /// A network change asked for a write.
    urgent: AtomicBool,
    status: Mutex<RendezvousStatus>,
    published: Mutex<Published>,
    /// Lookups per device: the last one and how many found nothing in a row.
    lookups: Mutex<HashMap<DeviceId, (Instant, u32)>>,
    /// Rendezvous devices named (their signed address records, the links they made), this run.
    urls: Mutex<HashMap<DeviceId, BTreeSet<String>>>,
    /// The newest record seen of each device (an older one served again changes nothing).
    newest: Mutex<HashMap<DeviceId, u64>>,
    /// The time of this device's last write, Unix milliseconds (each write is later than the one before).
    last_ms: AtomicU64,
    /// One write at a time.
    writing: tokio::sync::Mutex<()>,
}

fn now_s() -> u64 {
    trs_rendezvous::now_ms() / 1000
}

/// The secret-store name of this device's write key for `url`.
fn key_name(url: &str) -> String {
    let d = ring::digest::digest(&ring::digest::SHA256, url.as_bytes());
    format!("rendezvous-write-{}", &trs_rendezvous::hex(d.as_ref())[..16])
}

impl SyncHandle {
    /// The rendezvous this device uses (empty: none).
    pub(crate) fn rendezvous_url(&self) -> String {
        lock(&self.inner.cfg).settings.rendezvous.clone()
    }

    /// What the rendezvous is doing.
    pub fn rendezvous_status(&self) -> RendezvousStatus {
        let mut s = lock(&self.inner.rendezvous.status).clone();
        s.url = self.rendezvous_url();
        s
    }

    /// Uses the rendezvous at `url` (`https://…`, as the setup or the user gives it): saved, and this device's record is
    /// written there at once (the answer says whether that worked). The rendezvous it used before, if another, loses
    /// this device's record. Test it first (`trs_rendezvous::client::Rendezvous::self_test`): this does not.
    pub async fn set_rendezvous(&self, url: &str) -> Result<RendezvousStatus, SyncError> {
        let url = normalize_url(url).map_err(|e| SyncError::Invalid(e.to_string()))?;
        let old = self.rendezvous_url();
        if old != url {
            if !old.is_empty() {
                self.forget_rendezvous(&old).await;
            }
            lock(&self.inner.cfg).settings.rendezvous = url.clone();
            self.save_config()?;
            *lock(&self.inner.rendezvous.status) = RendezvousStatus::default();
            *lock(&self.inner.rendezvous.published) = Published::default();
        }
        if let Err(e) = publish_now(self).await {
            note_error(self, &e);
        }
        // The user's other devices learn it (and take it when they have none: `crate::devices`).
        self.inner.ctx.devices_wake.notify_one();
        Ok(self.rendezvous_status())
    }

    /// Stops using the rendezvous: this device's record there is deleted (when it answers within a few seconds), its
    /// write key forgotten, the setting cleared. Devices still find each other the other ways.
    pub async fn turn_off_rendezvous(&self) -> Result<(), SyncError> {
        let url = self.rendezvous_url();
        if url.is_empty() {
            return Ok(());
        }
        lock(&self.inner.cfg).settings.rendezvous.clear();
        self.save_config()?;
        self.forget_rendezvous(&url).await;
        *lock(&self.inner.rendezvous.status) = RendezvousStatus::default();
        *lock(&self.inner.rendezvous.published) = Published::default();
        // Said in the user's devices group (off here only; the others keep theirs).
        self.inner.ctx.devices_wake.notify_one();
        Ok(())
    }

    /// Deletes this device's record at `url` (best effort, a few seconds at most) and forgets the write key for it.
    async fn forget_rendezvous(&self, url: &str) {
        if let (Ok(r), Ok(Some(k))) = (Rendezvous::new(url), self.inner.secrets.get(&key_name(url)))
            && let Ok(key) = WriteKey::from_pkcs8(&k)
        {
            let t = next_ms(self);
            let id = record_id_hex(&self.inner.ctx.me);
            let _ = tokio::time::timeout(Duration::from_secs(5), r.delete(&id, &key, t)).await;
        }
        let _ = self.inner.secrets.delete(&key_name(url));
    }

    /// Tests the rendezvous in use: a test record written, read back and deleted, then this device's own record
    /// written and read back. The error is a sentence for the user.
    pub async fn test_rendezvous(&self) -> Result<RendezvousStatus, String> {
        let url = self.rendezvous_url();
        if url.is_empty() {
            return Err("No rendezvous is set up.".into());
        }
        let r = Rendezvous::new(&url).map_err(|e| e.to_string())?;
        r.self_test().await.map_err(|e| e.to_string())?;
        publish_now(self).await.map_err(|e| {
            note_error(self, &e);
            e.to_string()
        })?;
        let me = self.inner.ctx.me;
        let back = r.get(&record_id_hex(&me)).await.map_err(|e| e.to_string())?.ok_or("This device's record did not read back.")?;
        Record::open(&back, &me).map_err(|e| format!("This device's record did not read back ({e})."))?;
        Ok(self.rendezvous_status())
    }

    /// **Paired devices (hook):** the rendezvous to share with this user's other devices (docs/concepts/your-devices.md),
    /// said in the devices group (a rendezvous statement, kind 11, `crate::devices`). `None` when there is none.
    pub fn rendezvous_for_paired(&self) -> Option<String> {
        Some(self.rendezvous_url()).filter(|u| !u.is_empty())
    }

    /// **Paired devices (hook):** another of this user's devices uses `url`: this device takes it when it has none (a
    /// device the user set up stays as it is). Returns whether it took it. The caller (the paired group's settings) has
    /// already checked that the sender is one of the user's own devices.
    pub async fn adopt_rendezvous_from_paired(&self, url: &str) -> Result<bool, SyncError> {
        if !self.rendezvous_url().is_empty() {
            return Ok(false);
        }
        self.set_rendezvous(url).await?;
        Ok(true)
    }
}

/// The entry this device's signed address record carries for its rendezvous, if it has one.
pub(crate) fn record_entry(h: &SyncHandle) -> Option<String> {
    Some(h.rendezvous_url()).filter(|u| !u.is_empty() && u.len() + RECORD_PREFIX.len() <= trs_sync_proto::msg::MAX_ADDR).map(|u| format!("{RECORD_PREFIX}{u}"))
}

/// Whether an address record's entry is a rendezvous (not an address).
pub(crate) fn is_entry(a: &str) -> bool {
    a.starts_with(RECORD_PREFIX)
}

/// `device` named `url` as its rendezvous (its signed address record, a link it made): it is looked up there, and the
/// folders and channels it was met in remember it.
pub(crate) fn learn_url(h: &SyncHandle, device: DeviceId, url: &str) {
    if device == h.inner.ctx.me {
        return;
    }
    let Ok(url) = normalize_url(url) else { return };
    let new = lock(&h.inner.rendezvous.urls).entry(device).or_default().insert(url.clone());
    if !new {
        return;
    }
    let id = device.to_string();
    let changed = {
        let mut cfg = lock(&h.inner.cfg);
        let mut changed = false;
        for d in cfg.all_devices_mut().filter(|d| d.id == id && d.rendezvous != url) {
            d.rendezvous = url.clone();
            changed = true;
        }
        changed
    };
    if changed {
        let _ = h.save_config();
    }
}

/// The rendezvous entries of a device's verified address record.
pub(crate) fn learn_from_record(h: &SyncHandle, device: DeviceId, entries: &[String]) {
    for e in entries.iter().filter_map(|a| a.strip_prefix(RECORD_PREFIX)) {
        learn_url(h, device, e);
    }
}

/// Where to look `device` up: this device's rendezvous and the ones `device` named.
fn urls_for(h: &SyncHandle, device: DeviceId) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |u: String| {
        if !u.is_empty() && !out.contains(&u) {
            out.push(u);
        }
    };
    add(h.rendezvous_url());
    for u in lock(&h.inner.rendezvous.urls).get(&device).into_iter().flatten() {
        add(u.clone());
    }
    let id = device.to_string();
    let saved: Vec<String> = lock(&h.inner.cfg).all_devices_mut().filter(|d| d.id == id).map(|d| d.rendezvous.clone()).collect();
    for u in saved {
        add(u);
    }
    out
}

/// The dial loop wants `device` and has no session with it: look it up (unless its backoff holds it back).
pub(crate) fn look_up(h: &SyncHandle, device: DeviceId) {
    if device == h.inner.ctx.me || h.is_blocked(device) {
        return;
    }
    let urls = urls_for(h, device);
    if urls.is_empty() {
        return;
    }
    {
        let mut l = lock(&h.inner.rendezvous.lookups);
        let now = Instant::now();
        if let Some((last, n)) = l.get(&device).copied() {
            let wait = Duration::from_secs(LOOKUP_STEPS.get(n as usize).copied().unwrap_or(LOOKUP_STEPS[LOOKUP_STEPS.len() - 1]));
            if now < last + wait {
                return;
            }
        }
        let n = l.get(&device).map_or(0, |e| e.1);
        l.insert(device, (now, n + 1));
    }
    let h = h.clone();
    tokio::spawn(async move {
        for url in urls {
            if h.inner.ctx.registry.peer(&device).is_some() {
                break;
            }
            let Ok(r) = Rendezvous::new(&url) else { continue };
            let Ok(Some(sealed)) = r.get(&record_id_hex(&device)).await else { continue };
            let Ok(rec) = Record::open(&sealed, &device) else { continue };
            found(&h, device, &rec);
        }
    });
}

/// A record of `device` was found: its addresses are dialed (when it is newer than the last one seen).
fn found(h: &SyncHandle, device: DeviceId, rec: &Record) {
    if rec.issued_ms + MAX_AGE_MS < trs_rendezvous::now_ms() {
        return;
    }
    {
        let mut newest = lock(&h.inner.rendezvous.newest);
        if newest.get(&device).is_some_and(|&t| t >= rec.issued_ms) {
            return;
        }
        newest.insert(device, rec.issued_ms);
    }
    let addrs: Vec<SocketAddr> = rec.addresses().filter(|a| a.port() != 0 && !a.ip().is_unspecified()).collect();
    if addrs.is_empty() {
        return;
    }
    {
        let mut s = lock(&h.inner.rendezvous.status);
        s.found += 1;
        s.last_found_s = now_s();
    }
    for a in addrs {
        h.retry_now(a);
        h.learn_address(Some(device), None, a);
    }
}

/// The network changed: look devices up again at once, and write a new record soon.
pub(crate) fn network_changed(h: &SyncHandle) {
    lock(&h.inner.rendezvous.lookups).clear();
    let mut p = lock(&h.inner.rendezvous.published);
    p.fails = 0;
    p.retry_at = None;
    drop(p);
    h.inner.rendezvous.urgent.store(true, Ordering::SeqCst);
    h.inner.rendezvous.wake.notify_one();
}

/// The time of this device's next write: now, and later than the last one.
fn next_ms(h: &SyncHandle) -> u64 {
    let now = trs_rendezvous::now_ms();
    let last = &h.inner.rendezvous.last_ms;
    let mut prev = last.load(Ordering::SeqCst);
    loop {
        let t = now.max(prev + 1);
        match last.compare_exchange(prev, t, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return t,
            Err(p) => prev = p,
        }
    }
}

/// This device's write key for `url`, made and kept on first use.
fn write_key(h: &SyncHandle, url: &str) -> Result<WriteKey, ClientError> {
    let name = key_name(url);
    if let Some(k) = h.inner.secrets.get(&name).map_err(|e| ClientError::Key(e.to_string()))?
        && let Ok(k) = WriteKey::from_pkcs8(&k)
    {
        return Ok(k);
    }
    let k = WriteKey::generate()?;
    h.inner.secrets.put(&name, k.pkcs8()).map_err(|e| ClientError::Key(e.to_string()))?;
    Ok(k)
}

/// Writes this device's record now (one write at a time).
async fn publish_now(h: &SyncHandle) -> Result<(), ClientError> {
    let _one = h.inner.rendezvous.writing.lock().await;
    let url = h.rendezvous_url();
    if url.is_empty() {
        return Ok(());
    }
    let reach = h.own_reach();
    let addrs: Vec<SocketAddr> = reach.addresses().collect();
    let r = Rendezvous::new(&url)?;
    let key = write_key(h, &url)?;
    let t = next_ms(h);
    let rec = Record { device_pk: *h.inner.ctx.key.public_key(), issued_ms: t, lan: reach.lan, public: reach.public };
    let sealed = rec.seal(&h.inner.ctx.key, &mut ring::rand::SystemRandom::new()).map_err(|e| ClientError::Key(e.to_string()))?;
    let result = r.put(&record_id_hex(&h.inner.ctx.me), &sealed, &key, t).await;
    let mut p = lock(&h.inner.rendezvous.published);
    match &result {
        Ok(()) => {
            *p = Published { url, addrs, at: Some(Instant::now()), fails: 0, retry_at: None };
            drop(p);
            let mut s = lock(&h.inner.rendezvous.status);
            s.last_publish_s = now_s();
            s.last_error.clear();
            s.last_error_s = 0;
        }
        Err(_) => {
            let wait = PUBLISH_STEPS.get(p.fails as usize).copied().unwrap_or(PUBLISH_STEPS[PUBLISH_STEPS.len() - 1]);
            p.fails += 1;
            p.retry_at = Some(Instant::now() + Duration::from_secs(wait));
        }
    }
    result
}

fn note_error(h: &SyncHandle, e: &ClientError) {
    let mut s = lock(&h.inner.rendezvous.status);
    s.last_error = e.to_string();
    s.last_error_s = now_s();
}

/// Whether a write is due now, and if not, how long until the next look.
fn due(h: &SyncHandle, url: &str, addrs: &[SocketAddr], urgent: bool) -> Result<(), Duration> {
    let p = lock(&h.inner.rendezvous.published);
    let now = Instant::now();
    if let Some(at) = p.retry_at.filter(|&at| at > now) {
        return Err(at - now);
    }
    let Some(at) = p.at.filter(|_| p.url == url && p.fails == 0) else { return Ok(()) };
    let age = now.duration_since(at);
    let changed = urgent || p.addrs != addrs;
    if !changed && age < REFRESH {
        return Err(LOOK.min(REFRESH - age));
    }
    let gap = if urgent { MIN_GAP_CHANGE } else { MIN_GAP };
    if age < gap { Err(gap - age) } else { Ok(()) }
}

/// Publishes this device's record while the service runs (the task is aborted at the end).
pub(crate) async fn run(h: SyncHandle) {
    loop {
        if h.inner.stopping.load(Ordering::Relaxed) {
            return;
        }
        let url = h.rendezvous_url();
        let mut wait = LOOK;
        if !url.is_empty() {
            let urgent = h.inner.rendezvous.urgent.load(Ordering::SeqCst);
            let h2 = h.clone();
            // The interface list: off the runtime's threads.
            let addrs: Vec<SocketAddr> = tokio::task::spawn_blocking(move || h2.own_reach().addresses().collect()).await.unwrap_or_default();
            match due(&h, &url, &addrs, urgent) {
                Ok(()) => {
                    h.inner.rendezvous.urgent.store(false, Ordering::SeqCst);
                    if let Err(e) = publish_now(&h).await {
                        note_error(&h, &e);
                    }
                }
                Err(w) => wait = w.max(Duration::from_millis(200)),
            }
        }
        tokio::select! {
            _ = h.inner.rendezvous.wake.notified() => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_entry_is_not_an_address_to_older_versions() {
        let e = format!("{RECORD_PREFIX}https://seedwell-rendezvous.me.workers.dev");
        assert!(e.parse::<SocketAddr>().is_err(), "0.1.12 skips what it cannot parse as an address");
        assert!(is_entry(&e) && !is_entry("192.0.2.1:22000"));
        assert_ne!(key_name("https://a.workers.dev"), key_name("https://b.workers.dev"));
    }
}
