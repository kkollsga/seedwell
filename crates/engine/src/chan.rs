//! A small bounded MPSC queue for per-torrent and per-peer inboxes.
//!
//! `tokio::sync::mpsc` allocates its first 64-slot block when the channel is created, which costs
//! several KiB for every Warm torrent and every idle peer. This queue is a `VecDeque` behind a
//! mutex: an idle inbox owns no heap at all, and it grows only while messages are queued.
//! Backpressure is the same as tokio's: `send` waits while the queue is full, `try_send` fails.
//!
//! Wake-ups are plain wakers kept in the state (one for the receiver, a list for blocked senders)
//! instead of `Notify` objects: nothing is allocated per channel beyond the `Arc`, and the receiver
//! can be polled from a hand-written `poll_fn` ([`Receiver::poll_recv`]) without a `Notified`
//! future living in the caller's state machine (a peer task keeps that state for hours).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct State<T> {
    q: VecDeque<T>,
    senders: usize,
    rx_alive: bool,
    /// The receiver is parked here while the queue is empty.
    rx_waker: Option<Waker>,
    /// Senders parked while the queue is full (empty, no allocation, unless that happens).
    tx_wakers: Vec<Waker>,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    cap: usize,
}

/// Why a non-blocking send failed; carries the message back.
pub(crate) enum TrySendError<T> {
    /// The queue is full.
    Full(#[allow(dead_code)] T),
    /// The receiver is gone.
    Closed(#[allow(dead_code)] T),
}

impl<T> TrySendError<T> {
    /// The message that could not be sent.
    pub(crate) fn into_inner(self) -> T {
        match self {
            TrySendError::Full(m) | TrySendError::Closed(m) => m,
        }
    }
}

/// Sending half. Cloneable.
pub(crate) struct Sender<T>(Arc<Shared<T>>);

/// Receiving half. One consumer.
pub(crate) struct Receiver<T>(Arc<Shared<T>>);

/// A queue holding at most `cap` messages.
pub(crate) fn channel<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let s = Arc::new(Shared {
        state: Mutex::new(State {
            q: VecDeque::new(),
            senders: 1,
            rx_alive: true,
            rx_waker: None,
            tx_wakers: Vec::new(),
        }),
        cap: cap.max(1),
    });
    (Sender(s.clone()), Receiver(s))
}

fn lock<T>(s: &Shared<T>) -> std::sync::MutexGuard<'_, State<T>> {
    s.state.lock().unwrap_or_else(|p| p.into_inner())
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        lock(&self.0).senders += 1;
        Sender(self.0.clone())
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        let waker = {
            let mut g = lock(&self.0);
            g.senders -= 1;
            if g.senders == 0 { g.rx_waker.take() } else { None }
        };
        if let Some(w) = waker {
            w.wake();
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let wakers = {
            let mut g = lock(&self.0);
            g.rx_alive = false;
            std::mem::take(&mut g.tx_wakers)
        };
        for w in wakers {
            w.wake();
        }
    }
}

impl<T> Sender<T> {
    /// Queue a message without waiting.
    pub(crate) fn try_send(&self, v: T) -> Result<(), TrySendError<T>> {
        let waker = {
            let mut g = lock(&self.0);
            if !g.rx_alive {
                return Err(TrySendError::Closed(v));
            }
            if g.q.len() >= self.0.cap {
                return Err(TrySendError::Full(v));
            }
            g.q.push_back(v);
            g.rx_waker.take()
        };
        if let Some(w) = waker {
            w.wake();
        }
        Ok(())
    }

    /// Queue a message, waiting for room. `Err` returns the message if the receiver is gone.
    pub(crate) async fn send(&self, v: T) -> Result<(), T> {
        let mut v = Some(v);
        std::future::poll_fn(|cx| {
            let mut g = lock(&self.0);
            if !g.rx_alive {
                return Poll::Ready(Err(v.take().unwrap_or_else(|| unreachable!("message is present until sent"))));
            }
            if g.q.len() < self.0.cap {
                if let Some(m) = v.take() {
                    g.q.push_back(m);
                }
                let waker = g.rx_waker.take();
                drop(g);
                if let Some(w) = waker {
                    w.wake();
                }
                return Poll::Ready(Ok(()));
            }
            // Full. A sender that is polled again after a wake-up registers once more; remove
            // an earlier registration of the same task so the list cannot grow by re-polls.
            if !g.tx_wakers.iter().any(|w| w.will_wake(cx.waker())) {
                g.tx_wakers.push(cx.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

impl<T> Receiver<T> {
    /// Give the queue's allocation back when nothing is waiting: an idle inbox owns no heap.
    pub(crate) fn release_idle(&mut self) {
        let mut g = lock(&self.0);
        if g.q.is_empty() && g.q.capacity() > 0 {
            g.q = VecDeque::new();
        }
        if g.tx_wakers.is_empty() {
            g.tx_wakers = Vec::new();
        }
    }

    /// Next message (`Ready(None)` once every sender is gone and the queue is empty), or park the
    /// task's waker. For hand-written `poll_fn` loops; `recv` is the `async` form.
    pub(crate) fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let (m, wakers) = {
            let mut g = lock(&self.0);
            if let Some(m) = g.q.pop_front() {
                if g.q.is_empty() && g.q.capacity() > 64 {
                    // Give back the burst's memory.
                    g.q.shrink_to(8);
                }
                (m, std::mem::take(&mut g.tx_wakers))
            } else if g.senders == 0 {
                return Poll::Ready(None);
            } else {
                match &g.rx_waker {
                    Some(w) if w.will_wake(cx.waker()) => {}
                    _ => g.rx_waker = Some(cx.waker().clone()),
                }
                return Poll::Pending;
            }
        };
        for w in wakers {
            w.wake();
        }
        Poll::Ready(Some(m))
    }

    /// Next message, or `None` once every sender is gone and the queue is empty.
    pub(crate) async fn recv(&mut self) -> Option<T> {
        std::future::poll_fn(|cx| self.poll_recv(cx)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fifo_and_close() {
        let (tx, mut rx) = channel(4);
        for i in 0..4 {
            tx.try_send(i).ok().unwrap();
        }
        assert!(matches!(tx.try_send(9), Err(TrySendError::Full(9))));
        for i in 0..4 {
            assert_eq!(rx.recv().await, Some(i));
        }
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn send_waits_for_room() {
        let (tx, mut rx) = channel(1);
        tx.send(1).await.unwrap();
        let t2 = tx.clone();
        let h = tokio::spawn(async move { t2.send(2).await });
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!h.is_finished());
        assert_eq!(rx.recv().await, Some(1));
        h.await.unwrap().unwrap();
        assert_eq!(rx.recv().await, Some(2));
    }

    #[tokio::test]
    async fn dropped_receiver_fails_senders() {
        let (tx, rx) = channel::<u8>(1);
        drop(rx);
        assert!(tx.send(1).await.is_err());
        assert!(matches!(tx.try_send(1), Err(TrySendError::Closed(1))));
    }

    #[test]
    fn idle_queue_owns_no_heap() {
        let (tx, rx) = channel::<[u8; 64]>(4096);
        assert_eq!(lock(&tx.0).q.capacity(), 0);
        drop(rx);
    }
}
