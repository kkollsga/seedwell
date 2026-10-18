//! Minimal base64 (standard alphabet) decoder for `.torrent` payloads. Whitespace is ignored and
//! padding is optional. Hand-rolled to avoid a dependency for 30 lines; fuzzed by the proptest-free
//! unit tests below and by the request fuzz target (which reaches it through `add`).

/// Decode, refusing output larger than `max` bytes. `None` on any invalid character.
pub fn decode(input: &str, max: usize) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity((input.len() / 4 * 3).min(max));
    let mut acc: u32 = 0;
    let mut bits = 0u8;
    let mut padding = 0usize;
    for c in input.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => {
                padding += 1;
                continue;
            }
            b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return None,
        };
        if padding > 0 {
            return None; // data after padding
        }
        acc = acc << 6 | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
            if out.len() > max {
                return None;
            }
        }
    }
    if padding > 2 {
        return None;
    }
    // Leftover bits must be zero padding bits of a partial group; a lone 6-bit remainder is invalid.
    if bits >= 6 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_standard_vectors() {
        assert_eq!(decode("", 10).unwrap(), b"");
        assert_eq!(decode("Zg==", 10).unwrap(), b"f");
        assert_eq!(decode("Zm8=", 10).unwrap(), b"fo");
        assert_eq!(decode("Zm9v", 10).unwrap(), b"foo");
        assert_eq!(decode("Zm9vYg==", 10).unwrap(), b"foob");
        assert_eq!(decode("Zm9vYmE", 10).unwrap(), b"fooba", "padding optional");
        assert_eq!(decode("Zm9v\nYmFy", 10).unwrap(), b"foobar", "whitespace ignored");
        assert_eq!(decode("-_-_", 10).unwrap(), decode("+/+/", 10).unwrap(), "url-safe accepted");
    }

    #[test]
    fn rejects_garbage_and_oversize() {
        assert!(decode("Zm9v!", 10).is_none());
        assert!(decode("Zg==Zg", 10).is_none());
        assert!(decode("Z", 10).is_none());
        assert!(decode("Zm9vYmFy", 3).is_none());
        assert!(decode("é", 10).is_none());
    }

    #[test]
    fn roundtrips_all_bytes() {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let data: Vec<u8> = (0..=255u8).chain(0..=40u8).collect();
        let mut s = String::new();
        for ch in data.chunks(3) {
            let n = ch.iter().enumerate().fold(0u32, |a, (i, b)| a | u32::from(*b) << (16 - 8 * i));
            for k in 0..4 {
                if k <= ch.len() {
                    s.push(A[(n >> (18 - 6 * k) & 63) as usize] as char);
                } else {
                    s.push('=');
                }
            }
        }
        assert_eq!(decode(&s, 1000).unwrap(), data);
    }
}
