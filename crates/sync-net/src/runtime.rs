//! One folder's loop: the watcher feeds scans, peers' changes start pulls, and every local change wakes
//! the sessions that announce it.
//!
//! At start the watcher is started *before* the full scan, so nothing changes unseen between the two.
//! The full scan is a stat of every file (unchanged files are not read), so starting with it costs little
//! and needs no saved watcher state. A jittered full scan every hour is the safety net under the watcher.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use trs_fswatch::aggregate::{Aggregator, Batch};
use trs_fswatch::{RawEvent, WatchOptions};
use trs_sync::clock::SystemClock;
use trs_sync::folder::FolderCore;
use trs_sync::fs::{Fs, StdFs};
use trs_sync::global::Need;
use trs_sync::pull::PullReport;
use trs_sync::scan::{ScanError, ScanReport};
use trs_sync_proto::ids::ShortId;

use crate::shared::{Ctx, FolderShared, PENDING_SAMPLE, lock};
use crate::source::NetSource;
use crate::status::FolderState;

/// Full scan this often (± a quarter, so many folders do not scan at once).
const FULL_SCAN: Duration = Duration::from_secs(3600);
/// How often a folder looks for changes itself while its watcher is still starting (FSEvents can take many seconds
/// when many streams start at once).
const WATCHER_POLL: Duration = Duration::from_secs(3);
/// After a pull that is waiting for data or failed, try again after this (doubling to 5 minutes).
const RETRY_FIRST: Duration = Duration::from_secs(10);
const RETRY_MAX: Duration = Duration::from_secs(300);
/// Bytes asked of peers in one call by the puller.
const PULL_WINDOW: u64 = 64 << 20;
/// Pull rounds in one go (each round re-plans; a round that does nothing ends it).
const MAX_ROUNDS: usize = 64;

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn jitter(base: Duration, seed: u64) -> Duration {
    // A spread of ±25 % from the folder id: stable per folder, different between folders.
    let quarter = base.as_secs() / 4;
    let off = if quarter == 0 { 0 } else { seed % (2 * quarter) };
    Duration::from_secs(base.as_secs() - quarter + off)
}

/// Turns a watcher path (relative, as on disk) into the wire path the index uses.
fn wire(rel: &Path) -> Option<String> {
    let s = rel.to_str()?;
    let parts: Vec<String> = s.split('/').filter(|c| !c.is_empty()).map(trs_sync::path::wire_name).collect();
    Some(parts.join("/"))
}

/// Runs until `stop` turns true.
pub async fn run(ctx: Arc<Ctx>, f: Arc<FolderShared>, mut stop: watch::Receiver<bool>) {
    let (tx, rx) = mpsc::channel::<RawEvent>(1024);
    let mut events = Some(rx);
    // Starting FSEvents takes a second, and far longer when many streams start at once (a Mac with many folders, at
    // login): it starts on its own thread (holding only the path) while the folder scans and syncs, and once it runs
    // one more scan catches what changed before it was watching. Not on the blocking pool: shutting the runtime down
    // (quitting) waits for that pool, and must not wait for FSEvents.
    let root = f.root.clone();
    let (started, watcher_rx) = tokio::sync::oneshot::channel();
    let _ = std::thread::Builder::new().name("seedwell-sync-watch-start".into()).spawn(move || {
        let _ = started.send(start_watcher(&root, tx));
    });
    let mut watcher = Some(watcher_rx);
    let mut catch_up = false;
    let mut agg = Aggregator::new();
    let started = Instant::now();
    let ms = |at: Instant| at.duration_since(started).as_millis() as u64;
    let seed = u64::from_be_bytes(f.id.0[..8].try_into().unwrap_or([0; 8]));
    let mut next_full = Instant::now() + jitter(FULL_SCAN, seed);
    let mut retry: Option<(Instant, Duration)> = None;

    // A stop while starting (a folder renamed right after it was added) is noticed between the steps.
    scan(&ctx, &f, "").await;
    if *stop.borrow() {
        return;
    }
    pull(&ctx, &f, &mut retry).await;
    if *stop.borrow() {
        return;
    }
    housekeeping(&f).await;

    loop {
        if *stop.borrow() {
            break;
        }
        let deadline = agg.next_deadline().map(|d| started + Duration::from_millis(d));
        // Until the watcher runs, the folder looks for changes itself every few seconds.
        let poll = watcher.is_some().then(|| Instant::now() + WATCHER_POLL);
        let wake = [Some(next_full), deadline, retry.map(|(t, _)| t), poll].into_iter().flatten().min().unwrap_or(next_full);
        tokio::select! {
            _ = stop.changed() => {}
            w = async { watcher.as_mut().expect("guarded").await }, if watcher.is_some() => {
                watcher = None;
                if w.unwrap_or(false) {
                    catch_up = true;
                } else {
                    lock(&f.live).issues.push((String::new(), "changes are noticed only by the hourly scan (the watcher did not start)".into()));
                }
            }
            ev = next_event(&mut events), if events.is_some() => match ev {
                Some(ev) => agg.push(ev, ms(Instant::now())),
                // The watcher thread ended (an error): the hourly scan still keeps the folder right.
                None => events = None,
            },
            _ = f.remote_changed.notified() => {
                retry = None;
                pull(&ctx, &f, &mut retry).await;
            }
            _ = f.rescan.notified() => {
                scan(&ctx, &f, "").await;
                pull(&ctx, &f, &mut retry).await;
            }
            _ = tokio::time::sleep_until(wake.into()) => {}
        }
        if f.paused.load(Ordering::Relaxed) {
            set_state(&f, FolderState::Paused);
            continue;
        }
        let now = Instant::now();
        let mut changed = false;
        if std::mem::take(&mut catch_up) || watcher.is_some() {
            changed |= scan(&ctx, &f, "").await;
        }
        for batch in agg.poll(ms(now)) {
            let scopes: Vec<String> = match batch {
                Batch::All => vec![String::new()],
                Batch::Dir(d) => wire(&d).into_iter().collect(),
                Batch::Paths(ps) => ps.iter().filter_map(|p| wire(p)).collect(),
            };
            for s in scopes {
                changed |= scan(&ctx, &f, &s).await;
            }
        }
        if now >= next_full {
            next_full = now + jitter(FULL_SCAN, seed ^ now_s());
            changed |= scan(&ctx, &f, "").await;
            housekeeping(&f).await;
        }
        if changed || retry.is_some_and(|(t, _)| now >= t) {
            pull(&ctx, &f, &mut retry).await;
        }
    }
}

async fn next_event(rx: &mut Option<mpsc::Receiver<RawEvent>>) -> Option<RawEvent> {
    rx.as_mut()?.recv().await
}

/// Starts the watcher on a thread that moves its events onto the async channel. The thread owns the
/// watcher and stops it when the runtime drops the receiving end. False when the watcher could not start
/// (the hourly scan then keeps the folder right on its own).
fn start_watcher(root: &Path, tx: mpsc::Sender<RawEvent>) -> bool {
    let Ok(w) = trs_fswatch::watch(root, WatchOptions::default()) else { return false };
    std::thread::Builder::new()
        .name("seedwell-sync-watch".into())
        .spawn(move || {
            while !tx.is_closed() {
                match w.events().recv_timeout(Duration::from_millis(500)) {
                    Ok(ev) => {
                        if tx.blocking_send(ev).is_err() {
                            break;
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            w.stop();
        })
        .is_ok()
}

/// Scans `scope` on a blocking thread. True when the index changed (peers are then told).
async fn scan(ctx: &Arc<Ctx>, f: &Arc<FolderShared>, scope: &str) -> bool {
    if f.paused.load(Ordering::Relaxed) {
        return false;
    }
    set_state(f, FolderState::Scanning);
    let (f2, scope) = (f.clone(), scope.to_string());
    let res = tokio::task::spawn_blocking(move || {
        let mut core = lock(&f2.core);
        let r = core.scan(&scope);
        let seq = core.index_state().map(|(_, s)| s).unwrap_or(0);
        (r, seq)
    })
    .await;
    let _ = ctx;
    match res {
        Ok((Ok(report), seq)) => {
            record_issues(f, &report);
            let changed = report.changed + report.deleted > 0;
            if changed {
                f.local_seq.send_replace(seq);
            }
            settle(f).await;
            changed
        }
        Ok((Err(ScanError::FolderMissing), _)) => {
            set_state(f, FolderState::Missing);
            false
        }
        Ok((Err(ScanError::Io(e)), _)) => {
            set_error(f, format!("scan: {e}"));
            false
        }
        Err(e) => {
            set_error(f, format!("scan: {e}"));
            false
        }
    }
}

/// Pull rounds until nothing more can be done now; paths that changed under the puller are scanned and
/// pulled again.
async fn pull(ctx: &Arc<Ctx>, f: &Arc<FolderShared>, retry: &mut Option<(Instant, Duration)>) {
    if f.paused.load(Ordering::Relaxed) || lock(&f.live).state == FolderState::Missing {
        return;
    }
    for _ in 0..3 {
        set_state(f, FolderState::Syncing);
        let (ctx2, f2) = (ctx.clone(), f.clone());
        let rt = tokio::runtime::Handle::current();
        let res = tokio::task::spawn_blocking(move || {
            let source = NetSource::new(ctx2.clone(), f2.id, rt);
            // The losing device's own signed name in this folder first, so every member names a conflict copy the same.
            let profiles = ctx2.registry.space(&f2.id).map(|s| s.profile_names()).unwrap_or_default();
            let names = |id: ShortId| {
                profiles
                    .iter()
                    .find(|(d, _)| d.short() == id)
                    .map(|(_, n)| n.device.clone())
                    .or_else(|| ctx2.registry.peers().into_iter().find(|p| p.device.short() == id).map(|p| p.name.clone()))
                    .unwrap_or_else(|| if id == ctx2.me.short() { lock(&ctx2.device_name).clone() } else { "another device".into() })
            };
            let mut core = lock(&f2.core);
            lock(&f2.live).pending = sample_pending(&core);
            let mut total = PullReport::default();
            for _ in 0..MAX_ROUNDS {
                let r = core.pull(&source, &names, PULL_WINDOW)?;
                let done = r.done;
                merge(&mut total, r);
                // After each round, so the other devices hear "receiving" while big files still come.
                if done > 0 {
                    pass_status(&ctx2, &f2.root, &core);
                }
                lock(&f2.live).pending = sample_pending(&core);
                if done == 0 {
                    break;
                }
            }
            let seq = core.index_state().map(|(_, s)| s).unwrap_or(0);
            Ok::<_, std::io::Error>((total, seq))
        })
        .await;
        let (report, seq) = match res {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => return set_error(f, format!("pull: {e}")),
            Err(e) => return set_error(f, format!("pull: {e}")),
        };
        if report.done > 0 {
            f.local_seq.send_replace(seq);
        }
        {
            let mut live = lock(&f.live);
            live.fetched += report.fetched_bytes;
            for copy in &report.conflicts {
                if let Some(c) = trs_sync::conflict::parse_conflict(copy) {
                    live.conflicts.insert(copy.clone(), c.original);
                }
            }
            live.issues.retain(|(p, _)| !report.failed.iter().any(|(q, _)| q == p));
            live.issues.extend(report.failed.iter().cloned());
        }
        let stuck = report.waiting > 0 || !report.failed.is_empty();
        *retry = if stuck {
            let next = retry.map_or(RETRY_FIRST, |(_, d)| (d * 2).min(RETRY_MAX));
            Some((Instant::now() + next, next))
        } else {
            None
        };
        if report.rescan.is_empty() {
            break;
        }
        for p in &report.rescan {
            scan(ctx, f, p).await;
        }
    }
    settle(f).await;
}

/// This device's status in each pass folder (SPEC §7G.10), `.status-<this device>`: `receiving` while something
/// below it is still to fetch, `arrived` when nothing is. Each device writes only its own file, so any number of
/// them report without ever writing the same one; it syncs, so the sender (whose CLI wrote `state: sent`) and every
/// other device see how far each one is.
fn pass_status(ctx: &Ctx, root: &Path, core: &FolderCore) {
    let me = ctx.me.to_string();
    let me = me.split('-').next().unwrap_or_default();
    let Ok(projects) = std::fs::read_dir(root.join(trs_sync::PASS_THROUGH)) else { return };
    for to in projects.flatten().filter(|e| e.path().is_dir()) {
        for pass in std::fs::read_dir(to.path()).into_iter().flatten().flatten().filter(|e| e.path().is_dir()) {
            let file = pass.path().join(format!(".status-{me}"));
            let now = std::fs::read_to_string(&file).unwrap_or_default();
            let state = now.lines().find_map(|l| l.strip_prefix("state: ")).unwrap_or_default().trim().to_string();
            if state == "sent" || state == "arrived" {
                continue;
            }
            // Below the pass's own folder (`after` it with the slash, so a sibling such as "… 2/" never counts).
            let rel = format!("{}/{}/{}/", trs_sync::PASS_THROUGH, to.file_name().to_string_lossy(), pass.file_name().to_string_lossy());
            let missing = core.store().needs(Some(&rel), 1).map_or(true, |n| n.first().is_some_and(|(p, _)| p.starts_with(&rel)));
            let want = if missing { "receiving" } else { "arrived" };
            if want != state {
                let text = format!("computer: {}\nstate: {want}\nat: {}\n", lock(&ctx.device_name), crate::roles::now_s());
                let _ = std::fs::write(&file, text);
            }
        }
    }
}

fn merge(total: &mut PullReport, r: PullReport) {
    total.done += r.done;
    total.fetched_bytes += r.fetched_bytes;
    total.reused_bytes += r.reused_bytes;
    total.rescan = r.rescan;
    total.waiting = r.waiting;
    total.failed = r.failed;
    total.conflicts.extend(r.conflicts);
}

fn record_issues(f: &FolderShared, r: &ScanReport) {
    let mut live = lock(&f.live);
    live.issues.retain(|(p, _)| !r.issues.iter().any(|i| &i.path == p));
    for i in &r.issues {
        live.issues.push((i.path.clone(), crate::describe::issue(&i.kind)));
    }
    live.issues.truncate(1000);
}

/// After a scan or a pull: up to date, or waiting for devices, from what is still needed.
async fn settle(f: &Arc<FolderShared>) {
    let f2 = f.clone();
    let copies: Vec<String> = lock(&f.live).conflicts.keys().cloned().collect();
    let totals = tokio::task::spawn_blocking(move || {
        // Copies resolved since (here, or on another device and synced): a few lstats.
        let fs = StdFs::new(f2.root.clone());
        let gone: Vec<String> = copies.into_iter().filter(|c| matches!(fs.lstat(c), Ok(None))).collect();
        let core = lock(&f2.core);
        (core.need_totals().unwrap_or((0, 0)), core.local_totals().unwrap_or((0, 0)), gone, sample_pending(&core))
    })
    .await;
    let Ok((need, local, gone, pending)) = totals else { return };
    let mut live = lock(&f.live);
    live.need = need;
    live.pending = pending;
    live.local = local;
    for c in &gone {
        live.conflicts.remove(c);
    }
    if matches!(live.state, FolderState::Missing | FolderState::Paused) {
        return;
    }
    live.error = None;
    if need.0 == 0 {
        live.state = FolderState::UpToDate;
        live.last_synced_s = Some(now_s());
    } else {
        live.state = FolderState::WaitingForDevices;
    }
}

/// The first needed paths, for [`crate::shared::Live::pending`]. Tombstones of paths this device never
/// had are needs too and are skipped; a few pages at most are read.
fn sample_pending(core: &FolderCore) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..4 {
        let Ok(page) = core.store().needs(after.as_deref(), PENDING_SAMPLE) else { return (out, true) };
        let full = page.len() == PENDING_SAMPLE;
        after = page.last().map(|(p, _)| p.clone());
        for (path, n) in page {
            if n.need == Need::Nothing {
                continue;
            }
            if out.len() == PENDING_SAMPLE {
                return (out, true);
            }
            out.push(path);
        }
        if !full {
            return (out, false);
        }
    }
    (out, true)
}

/// At start and with every full scan: old archived versions go, and the conflict list is read anew.
async fn housekeeping(f: &Arc<FolderShared>) {
    if lock(&f.live).state == FolderState::Missing {
        return;
    }
    let f2 = f.clone();
    let pruned = tokio::task::spawn_blocking(move || {
        lock(&f2.core).prune_archive(trs_sync::clock::Clock::now_ns(&SystemClock), f2.retention)
    })
    .await;
    if let Ok(Err(e)) = pruned {
        lock(&f.live).issues.push((String::new(), format!("the archive could not be cleaned up: {e}")));
    }
    let _ = refresh_conflicts(f).await;
}

/// Reads the conflict copies from the index, keeps those made here that are not recorded yet (a
/// read-only device's own losing change), drops those no longer on disk, and stores the list in
/// [`crate::shared::Live`]. Pages through the whole index: not for every status update.
pub(crate) async fn refresh_conflicts(f: &Arc<FolderShared>) -> std::io::Result<Vec<(String, String)>> {
    let f2 = f.clone();
    let made_here = lock(&f.live).conflicts.clone();
    let seen: std::collections::BTreeSet<String> = made_here.keys().cloned().collect();
    let list = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<(String, String)>> {
        let mut all: std::collections::BTreeMap<String, String> = lock(&f2.core).conflicts()?.into_iter().collect();
        all.extend(made_here);
        let fs = StdFs::new(f2.root.clone());
        let mut out = Vec::with_capacity(all.len());
        for (copy, original) in all {
            if fs.lstat(&copy)?.is_some() {
                out.push((copy, original));
            }
        }
        Ok(out)
    })
    .await
    .map_err(std::io::Error::other)??;
    // Copies a pull made meanwhile stay.
    let mut live = lock(&f.live);
    live.conflicts.retain(|copy, _| !seen.contains(copy));
    live.conflicts.extend(list.iter().cloned());
    Ok(list)
}

fn set_state(f: &FolderShared, s: FolderState) {
    lock(&f.live).state = s;
}

fn set_error(f: &FolderShared, e: String) {
    let mut live = lock(&f.live);
    live.state = FolderState::Error;
    live.error = Some(e);
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use trs_sync::folder::{FolderConfig, FolderCore, RemoteIndex};
    use trs_sync::fs::{Fs, MemFs, write_all};
    use trs_sync::ignore::Ignore;
    use trs_sync::index::MemStore;
    use trs_sync_proto::device::DeviceKey;
    use trs_sync_proto::keys::{Access, FolderSecret};

    use super::{PENDING_SAMPLE, sample_pending};

    fn core(seed: u8, key: &FolderSecret) -> (MemFs, FolderCore) {
        let fs = MemFs::new();
        fs.create_dir(".seedwell").unwrap();
        let dk = Arc::new(DeviceKey::from_seed(&[seed; 32]));
        let cfg = FolderConfig {
            me: dk.device_id(),
            folder_pk: *key.public_key(),
            access: Access::Write,
            case_insensitive: false,
            byte_exact_names: false,
            exec_bits: true,
            archive: true,
        };
        let ids = Arc::new(AtomicU64::new(u64::from(seed) << 32));
        let new_id = Arc::new(move || {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&ids.fetch_add(1, Ordering::SeqCst).to_be_bytes());
            id
        });
        let core = FolderCore::new(
            cfg,
            Box::new(MemStore::new(u64::from(seed))),
            Arc::new(fs.clone()),
            Some(dk),
            Arc::new(trs_sync::clock::SystemClock),
            new_id,
            Ignore::defaults(),
        )
        .unwrap();
        (fs, core)
    }

    /// `n` files on A, then A's index on B: what B samples as pending.
    fn pending_after(n: usize) -> (Vec<String>, bool) {
        let key = FolderSecret::from_bytes([7; 32]);
        let ((afs, mut a), (_, mut b)) = (core(1, &key), core(2, &key));
        for i in 0..n {
            write_all(&afs, &format!("f{i:05}"), b"x").unwrap();
        }
        a.scan("").unwrap();
        let writers: Vec<[u8; 32]> = a.writers().iter().chain(b.writers().iter()).map(|w| w.device_pk).collect();
        b.set_membership(&writers, Access::Write).unwrap();
        let (index_id, _) = a.index_state().unwrap();
        let entries = a.announce_since(0, usize::MAX).unwrap();
        b.apply_remote(a.me(), RemoteIndex { index_id, full: true, done: true, entries }).unwrap();
        sample_pending(&b)
    }

    #[test]
    fn pending_lists_the_first_needed_paths_and_says_when_there_are_more() {
        assert_eq!(pending_after(3), (vec!["f00000".to_string(), "f00001".into(), "f00002".into()], false));
        let (paths, more) = pending_after(PENDING_SAMPLE + 5);
        assert_eq!((paths.len(), more), (PENDING_SAMPLE, true));
        assert_eq!(paths.first().map(String::as_str), Some("f00000"));
        // Nothing needed: nothing listed.
        let key = FolderSecret::from_bytes([7; 32]);
        assert_eq!(sample_pending(&core(3, &key).1), (Vec::new(), false));
    }
}
