//! 160-bit ids, XOR metric and BEP 42 secure node ids.

use crate::rng::Rng;
use std::cmp::Ordering;
use std::net::Ipv4Addr;

/// A 160-bit node id or info hash.
pub type Id = [u8; 20];

/// Bitwise XOR of two ids (the Kademlia distance).
pub fn xor(a: &Id, b: &Id) -> Id {
    let mut o = [0u8; 20];
    for i in 0..20 {
        o[i] = a[i] ^ b[i];
    }
    o
}

/// Number of leading bits shared by `a` and `b` (160 when equal).
pub fn common_prefix_len(a: &Id, b: &Id) -> usize {
    for i in 0..20 {
        let x = a[i] ^ b[i];
        if x != 0 {
            return i * 8 + x.leading_zeros() as usize;
        }
    }
    160
}

/// Orders `a` and `b` by their distance to `target` (closest first).
pub fn cmp_distance(target: &Id, a: &Id, b: &Id) -> Ordering {
    for i in 0..20 {
        let da = a[i] ^ target[i];
        let db = b[i] ^ target[i];
        match da.cmp(&db) {
            Ordering::Equal => {}
            o => return o,
        }
    }
    Ordering::Equal
}

/// Bit `i` (0 = most significant) of `id`.
pub fn bit(id: &Id, i: usize) -> bool {
    id[i / 8] & (0x80 >> (i % 8)) != 0
}

const fn crc_table() -> [u32; 256] {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0x82F6_3B78
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
}
static CRC_TABLE: [u32; 256] = crc_table();

/// CRC-32C (Castagnoli), as required by BEP 42.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c = CRC_TABLE[((c ^ b as u32) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// True for addresses exempt from BEP 42 (and never useful as DHT contacts):
/// loopback, private, link-local, unspecified, broadcast, multicast, CGNAT.
pub fn is_local_ip(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_multicast()
        || (o[0] == 100 && (o[1] & 0xc0) == 0x40)
        || o[0] == 0
        || o[0] >= 240
}

fn bep42_crc(ip: Ipv4Addr, r: u8) -> u32 {
    let mut b = ip.octets();
    let mask = [0x03u8, 0x0f, 0x3f, 0xff];
    for i in 0..4 {
        b[i] &= mask[i];
    }
    b[0] |= (r & 7) << 5;
    crc32c(&b)
}

/// Generates a BEP 42 node id for `ip`, using `r` (low 3 bits) and `rand3` (low 3 bits of byte 2)
/// and filling the remaining bytes from `rng`.
pub fn secure_id_with(ip: Ipv4Addr, r: u8, rng: &mut Rng) -> Id {
    let crc = bep42_crc(ip, r);
    let mut id = rng.id();
    id[0] = (crc >> 24) as u8;
    id[1] = (crc >> 16) as u8;
    id[2] = ((crc >> 8) as u8 & 0xf8) | (id[2] & 0x07);
    id[19] = r;
    id
}

/// Generates a BEP 42 node id for `ip` with a random `r`.
pub fn secure_id(ip: Ipv4Addr, rng: &mut Rng) -> Id {
    let r = (rng.next_u64() & 7) as u8;
    secure_id_with(ip, r, rng)
}

/// Checks whether `id` is a valid BEP 42 id for `ip`. Local addresses are always valid.
pub fn is_valid_for_ip(id: &Id, ip: Ipv4Addr) -> bool {
    if is_local_ip(ip) {
        return true;
    }
    let crc = bep42_crc(ip, id[19] & 7);
    id[0] == (crc >> 24) as u8
        && id[1] == (crc >> 16) as u8
        && (id[2] & 0xf8) == ((crc >> 8) as u8 & 0xf8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_check_value() {
        // Standard CRC-32C check value.
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn bep42_vectors() {
        // (ip, rand, expected first 21 bits as 3 bytes with the low 3 bits of byte 2 masked)
        let cases: [([u8; 4], u8, [u8; 3]); 5] = [
            ([124, 31, 75, 21], 1, [0x5f, 0xbf, 0xb8]),
            ([21, 75, 31, 124], 86, [0x5a, 0x3c, 0xe8]),
            ([65, 23, 51, 170], 22, [0xa5, 0xd4, 0x30]),
            ([84, 124, 73, 14], 65, [0x1b, 0x03, 0x20]),
            ([43, 213, 53, 83], 90, [0xe5, 0x6f, 0x68]),
        ];
        let mut rng = Rng::from_seed(3);
        for (ip, r, want) in cases {
            let ip = Ipv4Addr::from(ip);
            let id = secure_id_with(ip, r, &mut rng);
            assert_eq!(id[0], want[0], "{ip}");
            assert_eq!(id[1], want[1], "{ip}");
            assert_eq!(id[2] & 0xf8, want[2] & 0xf8, "{ip}");
            assert_eq!(id[19], r);
            assert!(is_valid_for_ip(&id, ip));
        }
    }

    #[test]
    fn validity_rejects_wrong_ip_and_exempts_local() {
        let mut rng = Rng::from_seed(9);
        let ip = Ipv4Addr::new(124, 31, 75, 21);
        let id = secure_id(ip, &mut rng);
        assert!(is_valid_for_ip(&id, ip));
        let mut bad = 0;
        for k in 1..50u8 {
            if !is_valid_for_ip(&id, Ipv4Addr::new(30 + k, 1, 2, 3)) {
                bad += 1;
            }
        }
        assert!(bad > 40);
        assert!(is_valid_for_ip(&[0u8; 20], Ipv4Addr::new(192, 168, 1, 5)));
    }

    #[test]
    fn metric() {
        let a = [0u8; 20];
        let mut b = [0u8; 20];
        b[0] = 0x80;
        assert_eq!(common_prefix_len(&a, &b), 0);
        b[0] = 0x01;
        assert_eq!(common_prefix_len(&a, &b), 7);
        assert_eq!(common_prefix_len(&a, &a), 160);
        assert!(bit(&b, 7) && !bit(&b, 0));
        let mut c = [0u8; 20];
        c[19] = 1;
        assert_eq!(cmp_distance(&a, &c, &b), Ordering::Less);
        assert_eq!(xor(&b, &b), [0u8; 20]);
    }
}
