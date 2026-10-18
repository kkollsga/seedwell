//! Two-step removal tokens (SPEC 9A.2): `remove` stores a plan and returns an unguessable token;
//! `remove_confirm` redeems it once, within the TTL, by the same client.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::types::TransferId;

/// Most outstanding plans; the oldest is dropped beyond this.
pub const MAX_PENDING: usize = 16;

/// What a token stands for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// Transfers to remove.
    pub ids: Vec<TransferId>,
    /// Delete the files too.
    pub delete_files: bool,
    /// The client that asked.
    pub client: String,
}

/// Why a token was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenError {
    /// Never issued, already used, or issued to someone else.
    Unknown,
    /// Issued, but the time is up.
    Expired,
}

/// Outstanding plans.
#[derive(Debug)]
pub struct Confirmations {
    ttl: Duration,
    pending: HashMap<String, (Instant, Plan)>,
}

impl Confirmations {
    /// Tokens live `ttl` (60 s in the product).
    pub fn new(ttl: Duration) -> Self {
        Self { ttl, pending: HashMap::new() }
    }

    /// The time a token stays valid.
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Store a plan and return its token (128 random bits, hex).
    pub fn issue(&mut self, now: Instant, plan: Plan) -> String {
        // Forget what has been dead for a while; keep recent expired ones so the error can say
        // "expired" instead of "unknown".
        let keep = self.ttl * 5;
        self.pending.retain(|_, (t, _)| now.saturating_duration_since(*t) <= keep);
        while self.pending.len() >= MAX_PENDING {
            let oldest = self.pending.iter().min_by_key(|(_, (t, _))| *t).map(|(k, _)| k.clone());
            match oldest {
                Some(k) => {
                    self.pending.remove(&k);
                }
                None => break,
            }
        }
        let token = random_token();
        self.pending.insert(token.clone(), (now, plan));
        token
    }

    /// Redeem a token. It is consumed on success and when expired; a token presented by a
    /// different client is refused and stays valid for the right one.
    pub fn redeem(&mut self, now: Instant, token: &str, client: &str) -> Result<Plan, TokenError> {
        let Some((issued, plan)) = self.pending.get(token) else {
            return Err(TokenError::Unknown);
        };
        if plan.client != client {
            return Err(TokenError::Unknown);
        }
        let expired = now.saturating_duration_since(*issued) > self.ttl;
        let plan = plan.clone();
        self.pending.remove(token);
        if expired { Err(TokenError::Expired) } else { Ok(plan) }
    }

    /// Plans waiting (including expired ones not yet swept).
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Nothing waiting.
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

fn random_token() -> String {
    let mut b = [0u8; 16];
    // The OS RNG does not fail in practice. If it ever did, a timestamp-derived token is
    // still unique per call, and tokens only matter for a local, same-user confirmation step.
    if getrandom::fill(&mut b).is_err() {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        b = t.to_le_bytes();
    }
    const HEX: &[u8; 16] = b"0123456789abcdef";
    b.iter().flat_map(|x| [HEX[usize::from(x >> 4)] as char, HEX[usize::from(x & 15)] as char]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(client: &str) -> Plan {
        Plan { ids: vec![[1; 20]], delete_files: false, client: client.into() }
    }

    #[test]
    fn redeem_once() {
        let mut c = Confirmations::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let tok = c.issue(t0, plan("claude"));
        assert_eq!(tok.len(), 32);
        assert_eq!(c.redeem(t0 + Duration::from_secs(5), &tok, "claude"), Ok(plan("claude")));
        assert_eq!(c.redeem(t0 + Duration::from_secs(6), &tok, "claude"), Err(TokenError::Unknown), "single use");
    }

    #[test]
    fn expiry_and_wrong_token() {
        let mut c = Confirmations::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let tok = c.issue(t0, plan("a"));
        assert_eq!(c.redeem(t0, "00000000000000000000000000000000", "a"), Err(TokenError::Unknown));
        assert_eq!(c.redeem(t0 + Duration::from_secs(61), &tok, "a"), Err(TokenError::Expired));
        assert_eq!(c.redeem(t0 + Duration::from_secs(62), &tok, "a"), Err(TokenError::Unknown), "expired tokens are consumed");
        // Exactly at the TTL still works.
        let tok = c.issue(t0, plan("a"));
        assert!(c.redeem(t0 + Duration::from_secs(60), &tok, "a").is_ok());
    }

    #[test]
    fn other_clients_cannot_redeem() {
        let mut c = Confirmations::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let tok = c.issue(t0, plan("a"));
        assert_eq!(c.redeem(t0, &tok, "b"), Err(TokenError::Unknown));
        assert!(c.redeem(t0, &tok, "a").is_ok(), "the owner can still use it");
    }

    #[test]
    fn tokens_are_distinct_and_bounded() {
        let mut c = Confirmations::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let mut seen = std::collections::HashSet::new();
        for i in 0..(MAX_PENDING * 3) {
            assert!(seen.insert(c.issue(t0 + Duration::from_millis(i as u64), plan("a"))));
        }
        assert!(c.len() <= MAX_PENDING);
    }
}
