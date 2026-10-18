//! 16-bit wrapping sequence-number helpers (BEP 29 `seq_nr` / `ack_nr`).

/// Signed distance `a - b` in the 16-bit circular space; positive when `a` is ahead of `b`.
#[inline]
pub fn diff(a: u16, b: u16) -> i16 {
    a.wrapping_sub(b) as i16
}

/// True when `a` is strictly before `b` in circular order.
#[inline]
pub fn lt(a: u16, b: u16) -> bool {
    diff(a, b) < 0
}

/// Wrap-aware comparison of two 32-bit delay values (clock offsets wrap).
#[inline]
pub fn lt32(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapping_order() {
        assert!(lt(65535, 0));
        assert!(lt(65530, 5));
        assert!(!lt(5, 65530));
        assert_eq!(diff(2, 65534), 4);
        assert_eq!(diff(65534, 2), -4);
        assert!(!lt(7, 7));
        assert!(lt32(u32::MAX, 3));
    }
}
