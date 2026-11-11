//! Slow work off the caller's thread (docs/CROSS-PLATFORM.md §1: the host's UI thread never waits for the core).
//!
//! The FFI's `async` calls ([`crate::async_calls`]) hand their work to one of two places and return a future at once:
//!
//! - a [`Lane`]: one thread that runs its jobs one after another, in the order they came. The user's commands to the
//!   engine go there, so a Pause and the Resume right after it reach the engine in that order.
//! - [`apart`]: a thread of its own for each call (reads, folder sync, chat, updates), which may take as long as its
//!   disk or network takes without holding anything else up.
//!
//! The future only waits for a channel, so polling it is cheap on any thread and needs no runtime: UniFFI's foreign
//! executors (Swift's, C#'s) poll it from wherever they like.
//!
//! [`Polled`] serves the polls that stay synchronous (a model read the host makes every second): the caller waits at
//! most a few milliseconds for a fresh value and otherwise gets the last one, while the read goes on by itself.

use std::collections::HashMap;
use std::hash::Hash;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::oneshot;

type Job = Box<dyn FnOnce() + Send>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The result of `f`, or the panic it ended with (handed on to whoever awaits it).
type Outcome<T> = std::thread::Result<T>;

fn run_caught<T>(f: impl FnOnce() -> T) -> Outcome<T> {
    catch_unwind(AssertUnwindSafe(f))
}

/// Waits for the outcome of a job without blocking: a panic in the job goes on in the awaiting future (UniFFI reports
/// it to the host as it reports a panic of a synchronous call).
async fn outcome<T>(rx: oneshot::Receiver<Outcome<T>>) -> T {
    match rx.await {
        Ok(Ok(v)) => v,
        Ok(Err(panic)) => resume_unwind(panic),
        // The job was dropped unrun: its thread could not be started.
        Err(_) => panic!("seedwell: a call could not be run (no thread)"),
    }
}

/// One thread that runs jobs in the order they came, started with the first job. It ends when the lane is dropped
/// and its queue is empty.
pub(crate) struct Lane {
    name: &'static str,
    tx: Mutex<Option<mpsc::Sender<Job>>>,
}

impl Lane {
    pub(crate) const fn new(name: &'static str) -> Self {
        Lane { name, tx: Mutex::new(None) }
    }

    /// Queues `job`, starting the lane's thread when there is none (or it is gone).
    fn push(&self, job: Job) {
        let mut tx = lock(&self.tx);
        let job = match tx.as_ref() {
            Some(t) => match t.send(job) {
                Ok(()) => return,
                Err(mpsc::SendError(job)) => job,
            },
            None => job,
        };
        let (t, rx) = mpsc::channel::<Job>();
        let spawned = std::thread::Builder::new().name(self.name.into()).spawn(move || {
            // A job never takes the lane down with it: each one's panic is handed to its caller.
            while let Ok(job) = rx.recv() {
                job();
            }
        });
        if spawned.is_ok() {
            let _ = t.send(job);
            *tx = Some(t);
        }
    }

    /// Runs `f` on the lane after everything queued before it; the future resolves with its result. The job is queued
    /// now, at the call, not when the future is first polled.
    pub(crate) fn run<T: Send + 'static>(&self, f: impl FnOnce() -> T + Send + 'static) -> impl std::future::Future<Output = T> + Send + 'static {
        let (tx, rx) = oneshot::channel();
        self.push(Box::new(move || {
            let _ = tx.send(run_caught(f));
        }));
        outcome(rx)
    }
}

/// Runs `f` on a thread of its own; the future resolves with its result. The thread is started now, at the call.
pub(crate) fn apart<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> impl std::future::Future<Output = T> + Send + 'static {
    let (tx, rx) = oneshot::channel();
    let _ = std::thread::Builder::new().name("seedwell-call".into()).spawn(move || {
        let _ = tx.send(run_caught(f));
    });
    outcome(rx)
}

/// How long a synchronous read the host makes on its UI thread waits for the core before it shows the last value: a
/// busy engine (a commit, a stat of a slow disk) never holds the UI up.
pub(crate) const UI_WAIT: Duration = Duration::from_millis(40);

/// The longest single timed wait of a bounded wait ([`wait_until`]).
const WAIT_SLICE: Duration = Duration::from_millis(4);

/// Waits on `cv` until it is notified or `end` comes, whichever is first (a spurious wake-up returns early too: the
/// caller looks at its condition again). Never in one long timed wait: the system may wake a thread's timer late by
/// a share of its length (macOS coalesces the timers of a thread below the user-interactive QoS: on CI's runner a 40 ms
/// wait woke after 150 to 180 ms, as it does with `taskpolicy -c utility` on the tests), so the wait is made of short
/// slices, each late by a little at most. A notification still ends it at once; a wait that runs out costs a few
/// wake-ups.
pub(crate) fn wait_until<'a, T>(cv: &Condvar, guard: MutexGuard<'a, T>, end: Instant) -> MutexGuard<'a, T> {
    let slice = end.saturating_duration_since(Instant::now()).min(WAIT_SLICE);
    cv.wait_timeout(guard, slice).unwrap_or_else(PoisonError::into_inner).0
}

/// Runs `f` on a thread of its own and waits at most `wait` for it: `None` when it took longer (it goes on, and its
/// result is dropped).
pub(crate) fn within<T: Send + 'static>(wait: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("seedwell-call".into())
        .spawn(move || {
            let _ = tx.send(f());
        })
        .ok()?;
    // In short slices, as [`wait_until`] waits.
    let end = Instant::now() + wait;
    loop {
        match rx.recv_timeout(end.saturating_duration_since(Instant::now()).min(WAIT_SLICE)) {
            Ok(v) => return Some(v),
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() < end => {}
            Err(_) => return None,
        }
    }
}

// MARK: Polls that stay synchronous

/// The keys of a [`Polled`] and the signal a finished read gives.
type Shared<K, V> = (Mutex<HashMap<K, Entry<V>>>, Condvar);

/// The state of one key of a [`Polled`].
struct Entry<V> {
    /// The last value read, and when it was read.
    value: Option<(V, Instant)>,
    /// A read is under way.
    running: bool,
    /// Reads finished so far.
    done: u64,
}

impl<V> Default for Entry<V> {
    fn default() -> Self {
        Entry { value: None, running: false, done: 0 }
    }
}

/// Values read off the caller's thread, one per key: [`Polled::get`] waits at most a few milliseconds for a fresh read
/// and otherwise returns the last value (`None` before the first), while the read finishes by itself and its value is
/// kept for the next caller. One read per key runs at a time.
pub(crate) struct Polled<K, V> {
    inner: Arc<Shared<K, V>>,
    /// A value younger than this is returned without a read.
    fresh: Duration,
}

impl<K: Eq + Hash + Clone + Send + 'static, V: Clone + Send + 'static> Polled<K, V> {
    pub(crate) fn new(fresh: Duration) -> Self {
        Polled { inner: Arc::new((Mutex::new(HashMap::new()), Condvar::new())), fresh }
    }

    /// The value of `key`: a kept one younger than `fresh`, else a read made now on a thread of its own, waited for at
    /// most `wait`; when that runs out (or a read of `key` is under way already and does not finish in time), the last
    /// value read, or `None`.
    pub(crate) fn get(&self, key: K, wait: Duration, read: impl FnOnce() -> V + Send + 'static) -> Option<V> {
        self.get_or_wait(key, Some(wait), read)
    }

    /// The value of `key` as [`Polled::get`] has it, but waiting for the read however long it takes (off the host's UI
    /// thread only: an `async` call's work). The value is kept for the polls.
    pub(crate) fn wait_for(&self, key: K, read: impl FnOnce() -> V + Send + 'static) -> Option<V> {
        self.get_or_wait(key, None, read)
    }

    fn get_or_wait(&self, key: K, wait: Option<Duration>, read: impl FnOnce() -> V + Send + 'static) -> Option<V> {
        let (map, cv) = &*self.inner;
        let mut m = lock(map);
        let e = m.entry(key.clone()).or_default();
        if let Some((v, at)) = &e.value
            && at.elapsed() < self.fresh
        {
            return Some(v.clone());
        }
        let target = e.done + 1;
        // A read started before (by a caller that gave up on it) is still under way: the engine or the disk is busy,
        // and nobody waits for it again (a model that asks several times waits once).
        let wait = match wait {
            Some(_) if e.running => Some(Duration::ZERO),
            w => w,
        };
        if !e.running {
            e.running = true;
            let inner = self.inner.clone();
            let k = key.clone();
            let spawned = std::thread::Builder::new().name("seedwell-poll".into()).spawn(move || {
                let v = run_caught(read);
                let (map, cv) = &*inner;
                let mut m = lock(map);
                let e = m.entry(k).or_default();
                e.running = false;
                e.done += 1;
                if let Ok(v) = v {
                    e.value = Some((v, Instant::now()));
                }
                cv.notify_all();
            });
            if spawned.is_err() {
                let e = m.entry(key.clone()).or_default();
                e.running = false;
                return e.value.as_ref().map(|(v, _)| v.clone());
            }
        }
        let end = wait.map(|w| Instant::now() + w);
        loop {
            let e = m.entry(key.clone()).or_default();
            if e.done >= target {
                return e.value.as_ref().map(|(v, _)| v.clone());
            }
            match end {
                None => m = cv.wait(m).unwrap_or_else(PoisonError::into_inner),
                Some(end) => {
                    let now = Instant::now();
                    if now >= end {
                        return e.value.as_ref().map(|(v, _)| v.clone());
                    }
                    m = wait_until(cv, m, end);
                }
            }
        }
    }

    /// The value of `key` as kept, without a read.
    pub(crate) fn kept(&self, key: &K) -> Option<V> {
        lock(&self.inner.0).get(key).and_then(|e| e.value.as_ref().map(|(v, _)| v.clone()))
    }

    /// Forgets `key` (what it was read from changed): the next `get` reads it anew.
    pub(crate) fn forget(&self, key: &K) {
        let mut m = lock(&self.inner.0);
        if let Some(e) = m.get_mut(key) {
            e.value = None;
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Driving the FFI's futures in tests as a foreign executor does: poll once (it must say Pending at once while the
    //! work runs), then wait for the result.

    use std::future::Future;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    /// Polls `f` once: how long that took and whether it was ready.
    pub(crate) fn first_poll<F: Future + ?Sized>(f: Pin<&mut F>) -> (Duration, Poll<F::Output>) {
        let mut cx = Context::from_waker(Waker::noop());
        let at = Instant::now();
        let p = f.poll(&mut cx);
        (at.elapsed(), p)
    }

    /// Waits for `f` (a test's own executor).
    pub(crate) fn wait<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(f)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{first_poll, wait};
    use super::*;
    use std::pin::pin;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn a_lane_runs_its_jobs_in_the_order_they_came_off_the_callers_thread() {
        let lane = Lane::new("test-lane");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let caller = std::thread::current().id();
        let futs: Vec<_> = (0..20u32)
            .map(|i| {
                let seen = seen.clone();
                lane.run(move || {
                    // The first job is slow: the others still wait for it.
                    if i == 0 {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    assert_ne!(std::thread::current().id(), caller);
                    lock(&seen).push(i);
                    i * 2
                })
            })
            .collect();
        // Dropped futures do not stop their jobs: the order is the order of the calls.
        let last = futs.into_iter().map(wait).next_back();
        assert_eq!(last, Some(38));
        assert_eq!(*lock(&seen), (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn a_call_returns_at_once_and_its_result_comes_later() {
        let f = apart(|| {
            std::thread::sleep(Duration::from_millis(300));
            7
        });
        let mut f = pin!(f);
        let (took, p) = first_poll(f.as_mut());
        // A call run in place would take the job's 300 ms; the bound leaves room for a preempted test thread at utility
        // priority (it once took over 20 ms in the timing pass, 2026-11-11).
        assert!(p.is_pending() && took < Duration::from_millis(100), "{took:?}");
        assert_eq!(wait(f), 7);
    }

    #[test]
    fn a_panic_in_a_job_reaches_its_caller_and_the_lane_goes_on() {
        let lane = Lane::new("test-lane");
        let bad = lane.run(|| -> u32 { panic!("boom") });
        let r = std::thread::spawn(move || wait(bad)).join();
        assert!(r.is_err(), "the caller sees the panic");
        assert_eq!(wait(lane.run(|| 5)), 5, "the lane still runs");
    }

    #[test]
    fn within_gives_up_after_its_wait() {
        let at = Instant::now();
        assert_eq!(within(Duration::from_millis(30), || std::thread::sleep(Duration::from_secs(1))), None);
        assert!(at.elapsed() < Duration::from_millis(300));
        assert_eq!(within(Duration::from_secs(1), || 3), Some(3));
    }

    #[test]
    fn a_poll_waits_briefly_then_shows_the_last_value_and_reads_once_at_a_time() {
        let p: Polled<&str, u32> = Polled::new(Duration::ZERO);
        let reads = Arc::new(AtomicU32::new(0));
        let slow = |reads: &Arc<AtomicU32>, v: u32, d: u64| {
            let reads = reads.clone();
            move || {
                reads.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(d));
                v
            }
        };
        assert_eq!(p.get("a", Duration::from_secs(1), slow(&reads, 1, 0)), Some(1), "a quick read is waited for");
        let at = Instant::now();
        assert_eq!(p.get("a", Duration::from_millis(20), slow(&reads, 2, 400)), Some(1), "a slow one shows the last value");
        assert!(at.elapsed() < Duration::from_millis(200));
        // While that read runs, no other starts.
        assert_eq!(p.get("a", Duration::ZERO, slow(&reads, 3, 0)), Some(1));
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        // Kept once the slow read ends (waited for, not slept past: at utility priority its 400 ms run late).
        let end = Instant::now() + Duration::from_secs(10);
        while p.kept(&"a") != Some(2) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(p.kept(&"a"), Some(2), "the slow read's value is kept");
        assert_eq!(p.get("b", Duration::ZERO, slow(&reads, 9, 100)), None, "nothing to show yet");
        p.forget(&"a");
        assert_eq!(p.kept(&"a"), None);
        assert_eq!(p.wait_for("a", slow(&reads, 5, 200)), Some(5), "an async call's read is waited for");
        assert_eq!(p.kept(&"a"), Some(5));
    }

    #[test]
    fn a_fresh_value_is_returned_without_a_read() {
        let p: Polled<u8, u32> = Polled::new(Duration::from_secs(60));
        assert_eq!(p.get(0, Duration::from_secs(1), || 1), Some(1));
        assert_eq!(p.get(0, Duration::from_secs(1), || 2), Some(1));
    }
}
