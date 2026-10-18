//! Human-friendly durations for hand-edited TOML: `"72h"`, `"7d"`, `"1d12h"`, or a plain integer of seconds.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Upper bound accepted from files: 100 years. Keeps arithmetic far from overflow.
pub const MAX_SECS: u64 = 100 * 365 * 86_400;

/// A duration in whole seconds that serialises as `"7d"`, `"72h"`, `"90m"`, `"45s"`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Secs(pub u64);

impl Secs {
    /// `n` seconds.
    pub const fn from_secs(n: u64) -> Secs {
        Secs(n)
    }
    /// `n` minutes.
    pub const fn minutes(n: u64) -> Secs {
        Secs(n * 60)
    }
    /// `n` hours.
    pub const fn hours(n: u64) -> Secs {
        Secs(n * 3600)
    }
    /// `n` days.
    pub const fn days(n: u64) -> Secs {
        Secs(n * 86_400)
    }
    /// As a std duration.
    pub const fn as_duration(self) -> std::time::Duration {
        std::time::Duration::from_secs(self.0)
    }

    /// Parse `"1d12h30m5s"`-style text (units `w d h m s`, case-insensitive) or digits (seconds).
    pub fn parse(s: &str) -> Result<Secs, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty duration".into());
        }
        let mut total: u64 = 0;
        let mut rest = s;
        let mut last_rank = u8::MAX;
        while !rest.is_empty() {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return Err(format!("expected a number in `{s}`"));
            }
            let n: u64 = rest[..digits].parse().map_err(|_| format!("number too large in `{s}`"))?;
            rest = &rest[digits..];
            let (mult, rank, adv) = match rest.chars().next() {
                None => {
                    if total == 0 && digits == s.len() {
                        (1, 0, 0) // bare integer: seconds
                    } else {
                        return Err(format!("missing unit at the end of `{s}`"));
                    }
                }
                Some(c) => match c.to_ascii_lowercase() {
                    'w' => (604_800, 5, 1),
                    'd' => (86_400, 4, 1),
                    'h' => (3600, 3, 1),
                    'm' => (60, 2, 1),
                    's' => (1, 1, 1),
                    other => return Err(format!("unknown unit `{other}` in `{s}` (use w, d, h, m, s)")),
                },
            };
            if rank >= last_rank && adv != 0 {
                return Err(format!("units in `{s}` must go from large to small, once each"));
            }
            last_rank = rank;
            rest = &rest[adv..];
            total = n
                .checked_mul(mult)
                .and_then(|v| total.checked_add(v))
                .filter(|v| *v <= MAX_SECS)
                .ok_or_else(|| format!("`{s}` is longer than 100 years"))?;
        }
        Ok(Secs(total))
    }
}

impl fmt::Display for Secs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.0;
        if s == 0 {
            return f.write_str("0s");
        }
        // One unit when it divides evenly (the common hand-written case), else a compound.
        for (unit, ch) in [(86_400, 'd'), (3600, 'h'), (60, 'm')] {
            if s.is_multiple_of(unit) && s / unit < 10_000 {
                return write!(f, "{}{ch}", s / unit);
            }
        }
        let (d, r) = (s / 86_400, s % 86_400);
        let (h, r) = (r / 3600, r % 3600);
        let (m, sec) = (r / 60, r % 60);
        for (n, ch) in [(d, 'd'), (h, 'h'), (m, 'm'), (sec, 's')] {
            if n > 0 {
                write!(f, "{n}{ch}")?;
            }
        }
        Ok(())
    }
}

impl Serialize for Secs {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Secs {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = Secs;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration like \"72h\" or a number of seconds")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Secs, E> {
                Secs::parse(v).map_err(E::custom)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Secs, E> {
                u64::try_from(v)
                    .ok()
                    .filter(|v| *v <= MAX_SECS)
                    .map(Secs)
                    .ok_or_else(|| E::custom("duration out of range"))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Secs, E> {
                if v <= MAX_SECS { Ok(Secs(v)) } else { Err(E::custom("duration out of range")) }
            }
        }
        d.deserialize_any(V)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_units_and_compounds() {
        assert_eq!(Secs::parse("72h"), Ok(Secs(259_200)));
        assert_eq!(Secs::parse("7d"), Ok(Secs::days(7)));
        assert_eq!(Secs::parse("1d12h"), Ok(Secs(129_600)));
        assert_eq!(Secs::parse(" 90M "), Ok(Secs(5400)));
        assert_eq!(Secs::parse("45"), Ok(Secs(45)));
        assert_eq!(Secs::parse("2w"), Ok(Secs::days(14)));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "d", "-5s", "5x", "1h1d", "1h1h", "5s5", "99999999999999999999s", "1.5h", "200y", "36500d1s"] {
            assert!(Secs::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn display_roundtrips() {
        for s in [0u64, 1, 59, 60, 61, 3600, 3661, 86_400, 90_000, 604_800, 1_234_567, MAX_SECS] {
            assert_eq!(Secs::parse(&Secs(s).to_string()), Ok(Secs(s)), "{s}");
        }
        assert_eq!(Secs::days(7).to_string(), "7d");
        assert_eq!(Secs::hours(72).to_string(), "3d");
        assert_eq!(Secs::hours(36).to_string(), "36h");
        assert_eq!(Secs(90_000).to_string(), "25h");
        assert_eq!(Secs(90_061).to_string(), "1d1h1m1s");
    }
}
