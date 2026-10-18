//! `announce_peer` write tokens (BEP 5): a keyed hash of the requester's IP, with the secret
//! rotated every 5 minutes. The previous secret is still accepted, so a token is valid for
//! 5-10 minutes. Stateless: nothing is stored per requester.

use crate::rng::Rng;
use std::net::Ipv4Addr;

/// Token length we hand out.
pub const TOKEN_LEN: usize = 8;
/// Secret rotation period.
pub const ROTATE_MS: u64 = 5 * 60 * 1000;

fn sip_round(v: &mut [u64; 4]) {
    v[0] = v[0].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(13) ^ v[0];
    v[0] = v[0].rotate_left(32);
    v[2] = v[2].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(16) ^ v[2];
    v[0] = v[0].wrapping_add(v[3]);
    v[3] = v[3].rotate_left(21) ^ v[0];
    v[2] = v[2].wrapping_add(v[1]);
    v[1] = v[1].rotate_left(17) ^ v[2];
    v[2] = v[2].rotate_left(32);
}

/// SipHash-2-4 of a short message with a 128-bit key.
pub fn siphash24(key: &[u8; 16], msg: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes(key[..8].try_into().unwrap_or([0; 8]));
    let k1 = u64::from_le_bytes(key[8..].try_into().unwrap_or([0; 8]));
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let (chunks, rem) = msg.as_chunks::<8>();
    for c in chunks {
        let m = u64::from_le_bytes(*c);
        v[3] ^= m;
        sip_round(&mut v);
        sip_round(&mut v);
        v[0] ^= m;
    }
    let mut last = [0u8; 8];
    last[..rem.len()].copy_from_slice(rem);
    last[7] = msg.len() as u8;
    let m = u64::from_le_bytes(last);
    v[3] ^= m;
    sip_round(&mut v);
    sip_round(&mut v);
    v[0] ^= m;
    v[2] ^= 0xff;
    for _ in 0..4 {
        sip_round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// Issues and validates tokens.
#[derive(Clone, Debug)]
pub struct Tokens {
    cur: [u8; 16],
    prev: [u8; 16],
    rotated_ms: u64,
}

impl Tokens {
    /// New secrets at time `now_ms`.
    pub fn new(now_ms: u64, rng: &mut Rng) -> Self {
        let mut cur = [0u8; 16];
        let mut prev = [0u8; 16];
        rng.fill(&mut cur);
        rng.fill(&mut prev);
        Self {
            cur,
            prev,
            rotated_ms: now_ms,
        }
    }

    /// Rotates secrets if due. Called lazily from packet handling, so an idle node never wakes for it.
    pub fn maybe_rotate(&mut self, now_ms: u64, rng: &mut Rng) {
        let elapsed = now_ms.saturating_sub(self.rotated_ms);
        if elapsed < ROTATE_MS {
            return;
        }
        if elapsed >= 2 * ROTATE_MS {
            rng.fill(&mut self.prev);
        } else {
            self.prev = self.cur;
        }
        rng.fill(&mut self.cur);
        self.rotated_ms = now_ms;
    }

    fn make(secret: &[u8; 16], ip: Ipv4Addr) -> [u8; TOKEN_LEN] {
        siphash24(secret, &ip.octets()).to_le_bytes()
    }

    /// Current token for `ip`.
    pub fn issue(&self, ip: Ipv4Addr) -> [u8; TOKEN_LEN] {
        Self::make(&self.cur, ip)
    }

    /// True if `token` was issued to `ip` under the current or previous secret.
    pub fn validate(&self, ip: Ipv4Addr, token: &[u8]) -> bool {
        token == Self::make(&self.cur, ip) || token == Self::make(&self.prev, ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siphash_reference_vector() {
        // Reference vector from the SipHash paper: key 00..0f, message 00..0e (15 bytes).
        let key: [u8; 16] = std::array::from_fn(|i| i as u8);
        let msg: Vec<u8> = (0..15u8).collect();
        assert_eq!(siphash24(&key, &msg), 0xa129_ca61_49be_45e5);
        // Empty message.
        assert_eq!(siphash24(&key, &[]), 0x726f_db47_dd0e_0e31);
    }

    #[test]
    fn issue_validate_and_ip_binding() {
        let mut rng = Rng::from_seed(1);
        let t = Tokens::new(0, &mut rng);
        let a = Ipv4Addr::new(1, 2, 3, 4);
        let b = Ipv4Addr::new(1, 2, 3, 5);
        let tok = t.issue(a);
        assert!(t.validate(a, &tok));
        assert!(!t.validate(b, &tok));
        assert!(!t.validate(a, &tok[..7]));
        assert!(!t.validate(a, b""));
    }

    #[test]
    fn rotation_accepts_previous_only() {
        let mut rng = Rng::from_seed(2);
        let mut t = Tokens::new(0, &mut rng);
        let ip = Ipv4Addr::new(9, 9, 9, 9);
        let tok = t.issue(ip);
        t.maybe_rotate(ROTATE_MS - 1, &mut rng);
        assert!(t.validate(ip, &tok));
        t.maybe_rotate(ROTATE_MS, &mut rng);
        assert!(t.validate(ip, &tok), "previous secret still accepted");
        assert_ne!(t.issue(ip), tok);
        t.maybe_rotate(2 * ROTATE_MS, &mut rng);
        assert!(!t.validate(ip, &tok), "two rotations later it is rejected");
    }

    #[test]
    fn long_gap_invalidates_everything() {
        let mut rng = Rng::from_seed(3);
        let mut t = Tokens::new(0, &mut rng);
        let ip = Ipv4Addr::new(9, 9, 9, 9);
        let tok = t.issue(ip);
        t.maybe_rotate(10 * ROTATE_MS, &mut rng);
        assert!(!t.validate(ip, &tok));
    }
}
