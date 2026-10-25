//! The sending side of a watcher's channel. Bounded, and it never blocks: a backend callback that blocked
//! would make the OS drop events anyway. When the channel is full the event is dropped and the sink owes the
//! consumer a full rescan, which it sends before anything else once there is room.

use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use crate::{RawEvent, RescanReason};

/// Events buffered between a backend and a slow consumer. The aggregator collapses far fewer pending paths
/// than this into a rescan, so a consumer that keeps up never sees the limit.
pub(crate) const CAPACITY: usize = 8192;

pub(crate) struct Sink {
    tx: SyncSender<RawEvent>,
    owes_rescan: bool,
    gone: bool,
}

impl Sink {
    pub(crate) fn new() -> (Sink, Receiver<RawEvent>) {
        Self::with_capacity(CAPACITY)
    }

    pub(crate) fn with_capacity(cap: usize) -> (Sink, Receiver<RawEvent>) {
        let (tx, rx) = sync_channel(cap);
        (Sink { tx, owes_rescan: false, gone: false }, rx)
    }

    /// Sends `ev`, or drops it (and owes a rescan) if the channel is full. Returns `false` once the receiver
    /// is gone.
    pub(crate) fn send(&mut self, ev: RawEvent) -> bool {
        if !self.flush() {
            // Still full: this event is covered by the rescan that is owed.
            return !self.gone;
        }
        match self.tx.try_send(ev) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.owes_rescan = true;
                true
            }
            Err(TrySendError::Disconnected(_)) => {
                self.gone = true;
                false
            }
        }
    }

    /// Sends the rescan owed after an overflow, if any. `true` when nothing is owed any more and the receiver
    /// is still there. Backends call this from their timer so the rescan goes out even if no further event
    /// comes.
    pub(crate) fn flush(&mut self) -> bool {
        if !self.owes_rescan {
            return !self.gone;
        }
        match self.tx.try_send(RawEvent::Rescan { rel: None, reason: RescanReason::ChannelFull }) {
            Ok(()) => {
                self.owes_rescan = false;
                true
            }
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => {
                self.owes_rescan = false;
                self.gone = true;
                false
            }
        }
    }

    /// Whether a rescan is still owed (backends shorten their timer while it is).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn owes_rescan(&self) -> bool {
        self.owes_rescan
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hint;
    use std::path::PathBuf;

    fn ev(n: u32) -> RawEvent {
        RawEvent::Path { rel: PathBuf::from(n.to_string()), kind: Hint::Modified, is_dir: Some(false) }
    }

    #[test]
    fn overflow_becomes_one_rescan_before_anything_else() {
        let (mut s, rx) = Sink::with_capacity(2);
        assert!(s.send(ev(1)));
        assert!(s.send(ev(2)));
        assert!(s.send(ev(3))); // dropped
        assert!(s.send(ev(4))); // dropped
        assert_eq!(rx.try_recv().ok(), Some(ev(1)));
        assert!(s.send(ev(5))); // rescan takes the free slot, 5 is dropped (still owed nothing more)
        assert_eq!(rx.try_recv().ok(), Some(ev(2)));
        assert_eq!(rx.try_recv().ok(), Some(RawEvent::Rescan { rel: None, reason: RescanReason::ChannelFull }));
        // 5 was dropped after the rescan was queued: that drop owes another rescan.
        assert!(s.owes_rescan());
        assert!(s.flush());
        assert_eq!(rx.try_recv().ok(), Some(RawEvent::Rescan { rel: None, reason: RescanReason::ChannelFull }));
        assert!(s.send(ev(6)));
        assert_eq!(rx.try_recv().ok(), Some(ev(6)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn flush_sends_the_owed_rescan_without_new_events() {
        let (mut s, rx) = Sink::with_capacity(1);
        s.send(ev(1));
        s.send(ev(2));
        assert!(!s.flush());
        rx.try_recv().ok();
        assert!(s.flush());
        assert_eq!(rx.try_recv().ok(), Some(RawEvent::Rescan { rel: None, reason: RescanReason::ChannelFull }));
    }

    #[test]
    fn send_reports_a_gone_receiver() {
        let (mut s, rx) = Sink::with_capacity(1);
        drop(rx);
        assert!(!s.send(ev(1)));
    }
}
