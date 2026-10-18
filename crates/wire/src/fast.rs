//! BEP 6 allowed-fast set generation.

use sha1::{Digest, Sha1};
use std::net::IpAddr;

/// Generate the canonical allowed-fast set for a peer (BEP 6).
///
/// Returns up to `k` distinct piece indices in generation order. `k` is clamped to
/// `num_pieces`; zero pieces yields an empty set. The BEP defines the algorithm for
/// IPv4 (mask to /24). For IPv6 we use the same construction over the /64 prefix:
/// the set is only ever *sent* to the peer in `allowed fast` messages, so it needs no
/// cross-client agreement.
pub fn allowed_fast_set(info_hash: &[u8; 20], ip: IpAddr, num_pieces: u32, k: usize) -> Vec<u32> {
    let k = k.min(num_pieces as usize);
    let mut out = Vec::with_capacity(k);
    if k == 0 {
        return out;
    }
    let mut x: Vec<u8> = match ip {
        IpAddr::V4(a) => {
            let mut o = a.octets();
            o[3] = 0;
            o.to_vec()
        }
        IpAddr::V6(a) => a.octets()[..8].to_vec(),
    };
    x.extend_from_slice(info_hash);
    let mut h: [u8; 20] = Sha1::digest(&x).into();
    while out.len() < k {
        for c in h.as_chunks::<4>().0 {
            if out.len() >= k {
                break;
            }
            let y = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
            let idx = y % num_pieces;
            if !out.contains(&idx) {
                out.push(idx);
            }
        }
        h = Sha1::digest(h).into();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Example vectors published in BEP 6: ip 80.4.4.200, infohash 0xaa*20, 1313 pieces.
    #[test]
    fn bep6_vectors() {
        let ip: IpAddr = "80.4.4.200".parse().unwrap();
        let ih = [0xAAu8; 20];
        assert_eq!(
            allowed_fast_set(&ih, ip, 1313, 7),
            [1059, 431, 808, 1217, 287, 376, 1188]
        );
        assert_eq!(
            allowed_fast_set(&ih, ip, 1313, 9),
            [1059, 431, 808, 1217, 287, 376, 1188, 353, 508]
        );
    }

    #[test]
    fn same_slash24_same_set() {
        let ih = [0x11u8; 20];
        let a = allowed_fast_set(&ih, "80.4.4.1".parse().unwrap(), 500, 10);
        let b = allowed_fast_set(&ih, "80.4.4.200".parse().unwrap(), 500, 10);
        let c = allowed_fast_set(&ih, "80.4.5.1".parse().unwrap(), 500, 10);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn distinct_and_in_range_and_clamped() {
        let ih = [7u8; 20];
        for n in [1u32, 2, 5, 10, 1000] {
            let s = allowed_fast_set(&ih, "1.2.3.4".parse().unwrap(), n, 20);
            assert_eq!(s.len(), (n as usize).min(20));
            let mut d = s.clone();
            d.sort_unstable();
            d.dedup();
            assert_eq!(d.len(), s.len());
            assert!(s.iter().all(|&i| i < n));
        }
        assert!(allowed_fast_set(&ih, "1.2.3.4".parse().unwrap(), 0, 10).is_empty());
        assert!(allowed_fast_set(&ih, "1.2.3.4".parse().unwrap(), 10, 0).is_empty());
        let v6 = allowed_fast_set(&ih, "2001:db8::1".parse().unwrap(), 100, 10);
        assert_eq!(v6.len(), 10);
        let v6b = allowed_fast_set(&ih, "2001:db8::ffff".parse().unwrap(), 100, 10);
        assert_eq!(v6, v6b);
    }
}
