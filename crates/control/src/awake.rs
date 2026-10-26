//! Keeping the Mac awake while agents work (SPEC 9A.7): leases that agents take (`awake.set`) and give
//! back. The host that can hold a power assertion (the app) reads [`Awake::leases`] and keeps the Mac
//! from idle sleep while one is live; the display still sleeps and the screen still locks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::types::AwakeLeaseDto;

/// The longest lease one call can take.
pub const MAX: Duration = Duration::from_secs(24 * 3600);
/// The most leases held at once (the oldest goes first).
const CAP: usize = 64;

#[derive(Clone)]
struct Lease {
    who: String,
    client: String,
    since_ms: u64,
    until_ms: u64,
    until: Instant,
}

/// Shared, cloneable handle to the leases.
#[derive(Clone, Default)]
pub struct Awake {
    inner: Arc<Mutex<HashMap<String, Lease>>>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl Awake {
    /// No leases.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take or renew the lease `key` for `span` (at most [`MAX`]); `who` is what it says is working.
    pub fn set(&self, key: &str, who: &str, client: &str, span: Duration) {
        let span = span.min(MAX);
        let now = now_ms();
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.retain(|_, l| l.until > Instant::now());
        let since_ms = g.get(key).map_or(now, |l| l.since_ms);
        if !g.contains_key(key) && g.len() >= CAP
            && let Some(oldest) = g.iter().min_by_key(|(_, l)| l.until).map(|(k, _)| k.clone())
        {
            g.remove(&oldest);
        }
        g.insert(
            key.to_owned(),
            Lease { who: who.to_owned(), client: client.to_owned(), since_ms, until_ms: now + span.as_millis() as u64, until: Instant::now() + span },
        );
    }

    /// Give the lease `key` back. Whether there was one.
    pub fn release(&self, key: &str) -> bool {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.remove(key).is_some_and(|l| l.until > Instant::now())
    }

    /// The live leases, the one that lasts longest first.
    pub fn leases(&self) -> Vec<AwakeLeaseDto> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        g.retain(|_, l| l.until > now);
        let mut v: Vec<AwakeLeaseDto> = g
            .iter()
            .map(|(k, l)| AwakeLeaseDto { key: k.clone(), who: l.who.clone(), client: l.client.clone(), since_ms: l.since_ms, until_ms: l.until_ms })
            .collect();
        v.sort_by(|a, b| b.until_ms.cmp(&a.until_ms).then_with(|| a.key.cmp(&b.key)));
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lease_lasts_until_given_back_or_over() {
        let a = Awake::new();
        a.set("s1", "kglite", "seedwell-cli", Duration::from_secs(3600));
        a.set("s2", "web", "seedwell-cli", Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        let l = a.leases();
        assert_eq!(l.iter().map(|l| l.who.as_str()).collect::<Vec<_>>(), ["kglite"], "the short one is over");
        // Renewing keeps when it started.
        let since = l[0].since_ms;
        a.set("s1", "kglite", "seedwell-cli", Duration::from_secs(60));
        assert_eq!(a.leases()[0].since_ms, since);
        assert!(a.release("s1"));
        assert!(!a.release("s1"), "given back once");
        assert!(a.leases().is_empty());
    }

    #[test]
    fn no_lease_is_longer_than_a_day_and_there_are_at_most_64() {
        let a = Awake::new();
        a.set("long", "x", "c", Duration::from_secs(10 * 24 * 3600));
        assert!(a.leases()[0].until_ms <= now_ms() + MAX.as_millis() as u64);
        for i in 0..100 {
            a.set(&format!("k{i}"), "x", "c", Duration::from_secs(60 + i));
        }
        assert_eq!(a.leases().len(), CAP);
    }
}
