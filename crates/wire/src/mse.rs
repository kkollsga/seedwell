//! Message Stream Encryption ("protocol encryption", the obfuscated handshake that Transmission,
//! libtorrent, uTorrent and others open connections with): the primitives.
//!
//! The handshake is a Diffie-Hellman exchange in a fixed 768-bit group, then RC4 keyed from the
//! shared secret and the info-hash (`SKEY`). This module has the arithmetic and the cipher; the
//! engine runs the protocol on its sockets. Nothing here allocates per connection beyond a few
//! fixed arrays, and nothing panics on hostile input: a peer's public value is validated before it
//! is used.
//!
//! The big-number code is a plain Montgomery multiplier over twelve 64-bit limbs. It is not
//! constant-time: the exponents are fresh per connection and the protocol offers obfuscation, not
//! authentication.

use std::sync::OnceLock;

use sha1::{Digest, Sha1};

/// Bytes of a public key, a shared secret and the prime.
pub const KEY_LEN: usize = 96;
/// Bytes of a private exponent (160 bits, as the specification asks).
pub const SECRET_LEN: usize = 20;

const N: usize = 12;

/// The group prime, most significant limb first (the same safe prime every client uses).
const P_BE: [u64; N] = [
    0xFFFF_FFFF_FFFF_FFFF,
    0xC90F_DAA2_2168_C234,
    0xC4C6_628B_80DC_1CD1,
    0x2902_4E08_8A67_CC74,
    0x020B_BEA6_3B13_9B22,
    0x514A_0879_8E34_04DD,
    0xEF95_19B3_CD3A_431B,
    0x302B_0A6D_F25F_1437,
    0x4FE1_356D_6D51_C245,
    0xE485_B576_625E_7EC6,
    0xF44C_42E9_A63A_3621,
    0x0000_0000_0009_0563,
];

type Limbs = [u64; N];

fn prime() -> Limbs {
    let mut p = [0u64; N];
    for (i, w) in P_BE.iter().enumerate() {
        p[N - 1 - i] = *w;
    }
    p
}

fn from_be(bytes: &[u8; KEY_LEN]) -> Limbs {
    let mut l = [0u64; N];
    for (i, chunk) in bytes.as_chunks::<8>().0.iter().enumerate() {
        let mut w = [0u8; 8];
        w.copy_from_slice(chunk);
        l[N - 1 - i] = u64::from_be_bytes(w);
    }
    l
}

fn to_be(l: &Limbs) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    for i in 0..N {
        out[i * 8..i * 8 + 8].copy_from_slice(&l[N - 1 - i].to_be_bytes());
    }
    out
}

/// `a >= b`.
fn geq(a: &Limbs, b: &Limbs) -> bool {
    for i in (0..N).rev() {
        if a[i] != b[i] {
            return a[i] > b[i];
        }
    }
    true
}

/// `a -= b` (caller guarantees `a >= b`, or accepts the wrap); returns the borrow.
fn sub_in_place(a: &mut Limbs, b: &Limbs) -> bool {
    let mut borrow = false;
    for i in 0..N {
        let (d1, b1) = a[i].overflowing_sub(b[i]);
        let (d2, b2) = d1.overflowing_sub(u64::from(borrow));
        a[i] = d2;
        borrow = b1 || b2;
    }
    borrow
}

/// Montgomery constants for the group prime.
struct Mont {
    p: Limbs,
    /// `-p^-1 mod 2^64`.
    n0inv: u64,
    /// `R mod p` (the number 1 in Montgomery form), `R = 2^768`.
    one: Limbs,
    /// `R^2 mod p`.
    r2: Limbs,
}

fn mont() -> &'static Mont {
    static M: OnceLock<Mont> = OnceLock::new();
    M.get_or_init(|| {
        let p = prime();
        // Newton iteration for the inverse of the (odd) low limb modulo 2^64.
        let mut inv: u64 = p[0];
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(p[0].wrapping_mul(inv)));
        }
        let n0inv = inv.wrapping_neg();
        // R mod p = 2^768 - p, because p lies between R/2 and R.
        let mut one = [0u64; N];
        sub_in_place_from_zero(&mut one, &p);
        // R^2 mod p: double R mod p 768 times, reducing each time.
        let mut r2 = one;
        for _ in 0..(N * 64) {
            let mut carry = 0u64;
            for w in r2.iter_mut() {
                let next = *w >> 63;
                *w = (*w << 1) | carry;
                carry = next;
            }
            if carry == 1 || geq(&r2, &p) {
                sub_in_place(&mut r2, &p);
            }
        }
        Mont { p, n0inv, one, r2 }
    })
}

/// `out = 0 - p` modulo 2^768.
fn sub_in_place_from_zero(out: &mut Limbs, p: &Limbs) {
    *out = [0u64; N];
    sub_in_place(out, p);
}

impl Mont {
    /// Montgomery product `a * b / R mod p` (CIOS), for `a, b < p`.
    fn mul(&self, a: &Limbs, b: &Limbs) -> Limbs {
        let mut t = [0u64; N + 2];
        for &bi in b.iter() {
            let mut carry = 0u128;
            for j in 0..N {
                let v = u128::from(t[j]) + u128::from(a[j]) * u128::from(bi) + carry;
                t[j] = v as u64;
                carry = v >> 64;
            }
            let v = u128::from(t[N]) + carry;
            t[N] = v as u64;
            t[N + 1] = (v >> 64) as u64;

            let m = t[0].wrapping_mul(self.n0inv);
            let v = u128::from(t[0]) + u128::from(m) * u128::from(self.p[0]);
            let mut carry = v >> 64;
            for j in 1..N {
                let v = u128::from(t[j]) + u128::from(m) * u128::from(self.p[j]) + carry;
                t[j - 1] = v as u64;
                carry = v >> 64;
            }
            let v = u128::from(t[N]) + carry;
            t[N - 1] = v as u64;
            t[N] = t[N + 1] + (v >> 64) as u64;
            t[N + 1] = 0;
        }
        let mut r = [0u64; N];
        r.copy_from_slice(&t[..N]);
        if t[N] != 0 || geq(&r, &self.p) {
            sub_in_place(&mut r, &self.p);
        }
        r
    }

    /// `base^exp mod p` for `base < p`; `exp` is big-endian.
    fn pow(&self, base: &Limbs, exp: &[u8]) -> Limbs {
        let b = self.mul(base, &self.r2);
        let mut acc = self.one;
        for byte in exp {
            for bit in (0..8).rev() {
                acc = self.mul(&acc, &acc);
                if (byte >> bit) & 1 == 1 {
                    acc = self.mul(&acc, &b);
                }
            }
        }
        let mut unit = [0u64; N];
        unit[0] = 1;
        self.mul(&acc, &unit)
    }
}

/// `2^secret mod p`: the value we send.
pub fn public_key(secret: &[u8; SECRET_LEN]) -> [u8; KEY_LEN] {
    let m = mont();
    let mut two = [0u64; N];
    two[0] = 2;
    to_be(&m.pow(&two, secret))
}

/// `peer_public^secret mod p`, or `None` if the peer's value is not a valid group element
/// (anything outside 2..=p-2 would make the secret guessable).
pub fn shared_secret(secret: &[u8; SECRET_LEN], peer_public: &[u8; KEY_LEN]) -> Option<[u8; KEY_LEN]> {
    let m = mont();
    let y = from_be(peer_public);
    let mut one = [0u64; N];
    one[0] = 1;
    let mut pm1 = m.p;
    sub_in_place(&mut pm1, &one);
    // y <= 1, or y >= p - 1
    let small = y[1..].iter().all(|w| *w == 0) && y[0] <= 1;
    if small || geq(&y, &pm1) {
        return None;
    }
    Some(to_be(&m.pow(&y, secret)))
}

/// SHA-1 over the concatenation of `parts` (the handshake hashes a keyword, the secret and the
/// info-hash in various orders).
pub fn hash(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// RC4, as the handshake uses it: the first 1,024 output bytes are discarded.
// size: 258 bytes
#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// Key schedule, then drop the first 1,024 bytes of key stream.
    pub fn new(key: &[u8]) -> Self {
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            *v = i as u8;
        }
        let mut j = 0u8;
        if !key.is_empty() {
            for i in 0..256 {
                j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
                s.swap(i, usize::from(j));
            }
        }
        let mut c = Rc4 { s, i: 0, j: 0 };
        let mut skip = [0u8; 1024];
        c.apply(&mut skip);
        c
    }

    /// XOR the key stream into `data` (encrypt and decrypt are the same operation).
    pub fn apply(&mut self, data: &mut [u8]) {
        for b in data {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[usize::from(self.i)]);
            self.s.swap(usize::from(self.i), usize::from(self.j));
            let k = self.s[usize::from(self.s[usize::from(self.i)].wrapping_add(self.s[usize::from(self.j)]))];
            *b ^= k;
        }
    }
}

impl std::fmt::Debug for Rc4 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rc4(..)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex<const L: usize>(s: &str) -> [u8; L] {
        let mut out = [0u8; L];
        for (i, b) in out.iter_mut().enumerate() {
            *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap();
        }
        out
    }

    #[test]
    fn dh_matches_python_pow() {
        // Vectors from Python's pow(2, x, p) and pow(y, x, p) with the MSE prime.
        let x: [u8; SECRET_LEN] = unhex("0123456789abcdef0123456789abcdef01234567");
        let public: [u8; KEY_LEN] = unhex("6fd4bc7aa649593205ec30348a3ccc737b61fa01e9e1762c2c53eb69033afecbdf7c13b8ac3643af78d0760b0f42db009f2b96c970f009d060faf617f117d0f1c221cea0561b9a86e852fc70a6f09ad0f82378603aa5e56b811deb3f534bf276");
        assert_eq!(public_key(&x), public);
        let y = [0x5au8; KEY_LEN];
        let shared: [u8; KEY_LEN] = unhex("8e31703b09c2c83ae03fc3e00c3ce3c61892ca448a46dca40c930edf48179bc5cb45af6967134452f3c440474a69a02ba13514220fb7cc9df8a3cd69f694b028d23ae78d556c7835c928fa643c3286a2f2309c9df76659299eb984a0b27267b6");
        assert_eq!(shared_secret(&x, &y), Some(shared));
    }

    #[test]
    fn both_sides_agree() {
        let (a, b) = ([7u8; SECRET_LEN], [9u8; SECRET_LEN]);
        let (pa, pb) = (public_key(&a), public_key(&b));
        assert_eq!(shared_secret(&a, &pb), shared_secret(&b, &pa));
        assert!(shared_secret(&a, &pb).is_some());
    }

    #[test]
    fn degenerate_public_values_are_refused() {
        let x = [3u8; SECRET_LEN];
        assert_eq!(shared_secret(&x, &[0u8; KEY_LEN]), None);
        let mut one = [0u8; KEY_LEN];
        one[KEY_LEN - 1] = 1;
        assert_eq!(shared_secret(&x, &one), None);
        let p = to_be(&prime());
        assert_eq!(shared_secret(&x, &p), None);
        let mut pm1 = prime();
        let mut unit = [0u64; N];
        unit[0] = 1;
        sub_in_place(&mut pm1, &unit);
        assert_eq!(shared_secret(&x, &to_be(&pm1)), None);
        assert_eq!(shared_secret(&x, &[0xffu8; KEY_LEN]), None, "above the prime");
    }

    #[test]
    fn rc4_known_answer_without_the_discard() {
        // RFC 6229 style check, done with a plain RC4 so the discard does not matter: the cipher
        // here drops 1,024 bytes, so compare against the same key schedule run by hand.
        let mut c = Rc4::new(b"Key");
        let mut plain = Rc4 { s: [0; 256], i: 0, j: 0 };
        // Plain RC4("Key") encrypts "Plaintext" to BBF316E8D940AF0AD3.
        for (i, v) in plain.s.iter_mut().enumerate() {
            *v = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(plain.s[i]).wrapping_add(b"Key"[i % 3]);
            plain.s.swap(i, usize::from(j));
        }
        let mut data = *b"Plaintext";
        plain.apply(&mut data);
        assert_eq!(data, unhex::<9>("bbf316e8d940af0ad3"));
        // And the discarding cipher is a different, but self-inverse, stream.
        let mut a = *b"hello world";
        let mut d = Rc4::new(b"Key");
        c.apply(&mut a);
        assert_ne!(&a, b"hello world");
        d.apply(&mut a);
        assert_eq!(&a, b"hello world");
    }

    #[test]
    fn hash_concatenates() {
        assert_eq!(hash(&[b"ab", b"c"]), hash(&[b"abc"]));
    }
}
