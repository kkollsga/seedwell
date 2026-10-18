//! Minimal oneshot channel whose receiver is both a `Future` and blockingly waitable.
//!
//! Hand-rolled (≈60 lines) instead of pulling in a futures crate: the disk threads only ever
//! need "complete once, wake the waiter", and tests/benches want a blocking `wait()`.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Waker};

use crate::error::{Result, StorageError};

struct State<T> {
    value: Option<Result<T>>,
    waker: Option<Waker>,
    sender_gone: bool,
}

struct Inner<T> {
    state: Mutex<State<T>>,
    cv: Condvar,
}

/// Completion side, held by a disk thread.
pub(crate) struct Sender<T>(Arc<Inner<T>>);

/// A request in flight. `.await` it, or call [`Pending::wait`] from a non-async thread.
///
/// Dropping it without awaiting is fine: the request still runs, the result is discarded.
/// If the disk threads shut down before answering, it resolves to [`StorageError::Shutdown`].
#[must_use = "a Pending does nothing unless awaited or waited on; dropping it discards the result"]
pub struct Pending<T>(Arc<Inner<T>>);

pub(crate) fn channel<T>() -> (Sender<T>, Pending<T>) {
    let inner = Arc::new(Inner {
        state: Mutex::new(State {
            value: None,
            waker: None,
            sender_gone: false,
        }),
        cv: Condvar::new(),
    });
    (Sender(inner.clone()), Pending(inner))
}

fn lock<T>(m: &Mutex<State<T>>) -> std::sync::MutexGuard<'_, State<T>> {
    // A poisoned lock only means another thread panicked while holding it; the state is a plain
    // Option and is still consistent.
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl<T> Sender<T> {
    pub(crate) fn send(self, v: Result<T>) {
        let waker = {
            let mut s = lock(&self.0.state);
            s.value = Some(v);
            s.waker.take()
        };
        self.0.cv.notify_all();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut s = lock(&self.0.state);
            s.sender_gone = true;
            if s.value.is_some() {
                None
            } else {
                s.waker.take()
            }
        };
        self.0.cv.notify_all();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> std::fmt::Debug for Pending<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pending")
    }
}

impl<T> Pending<T> {
    /// Blocks the current thread until the result is available.
    pub fn wait(self) -> Result<T> {
        let mut s = lock(&self.0.state);
        loop {
            if let Some(v) = s.value.take() {
                return v;
            }
            if s.sender_gone {
                return Err(StorageError::Shutdown);
            }
            s = self.0.cv.wait(s).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Like [`wait`](Self::wait) but gives up after `timeout`: `None` means the request is still
    /// in flight (the request keeps running; the result is discarded with this `Pending`).
    pub fn wait_timeout(self, timeout: std::time::Duration) -> Option<Result<T>> {
        let deadline = std::time::Instant::now() + timeout;
        let mut s = lock(&self.0.state);
        loop {
            if let Some(v) = s.value.take() {
                return Some(v);
            }
            if s.sender_gone {
                return Some(Err(StorageError::Shutdown));
            }
            let left = deadline.checked_duration_since(std::time::Instant::now())?;
            s = self
                .0
                .cv
                .wait_timeout(s, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
    }

    /// Non-blocking check; `None` if the request is still in flight.
    pub fn try_take(&self) -> Option<Result<T>> {
        let mut s = lock(&self.0.state);
        if let Some(v) = s.value.take() {
            return Some(v);
        }
        s.sender_gone.then_some(Err(StorageError::Shutdown))
    }
}

impl<T> Future for Pending<T> {
    type Output = Result<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut s = lock(&self.0.state);
        if let Some(v) = s.value.take() {
            return Poll::Ready(v);
        }
        if s.sender_gone {
            return Poll::Ready(Err(StorageError::Shutdown));
        }
        match &s.waker {
            Some(w) if w.will_wake(cx.waker()) => {}
            _ => s.waker = Some(cx.waker().clone()),
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_and_drop() {
        let (tx, rx) = channel::<u32>();
        std::thread::spawn(move || tx.send(Ok(7)));
        assert_eq!(rx.wait().unwrap(), 7);
        let (tx, rx) = channel::<u32>();
        drop(tx);
        assert!(matches!(rx.wait(), Err(StorageError::Shutdown)));
    }

    #[test]
    fn wait_timeout_gives_up_then_sees_result() {
        let (tx, rx) = channel::<u32>();
        assert!(
            rx.wait_timeout(std::time::Duration::from_millis(20))
                .is_none()
        );
        drop(tx);
        let (tx, rx) = channel::<u32>();
        std::thread::spawn(move || tx.send(Ok(3)));
        let v = rx.wait_timeout(std::time::Duration::from_secs(30));
        assert!(matches!(v, Some(Ok(3))));
        let (tx, rx) = channel::<u32>();
        drop(tx);
        let v = rx.wait_timeout(std::time::Duration::from_secs(30));
        assert!(matches!(v, Some(Err(StorageError::Shutdown))));
    }

    #[test]
    fn future_wakes() {
        struct Flag(std::sync::atomic::AtomicBool, std::thread::Thread);
        impl std::task::Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                self.1.unpark();
            }
        }
        let (tx, mut rx) = channel::<u32>();
        let flag = Arc::new(Flag(Default::default(), std::thread::current()));
        let waker = Waker::from(flag.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut rx).poll(&mut cx).is_pending());
        tx.send(Ok(1));
        assert!(flag.0.load(std::sync::atomic::Ordering::SeqCst));
        assert!(matches!(
            Pin::new(&mut rx).poll(&mut cx),
            Poll::Ready(Ok(1))
        ));
    }
}
