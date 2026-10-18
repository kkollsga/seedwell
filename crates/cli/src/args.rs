//! A small argument scanner. Hand-rolled instead of a derive library: the grammar is a dozen
//! subcommands with a few flags each, and the binary stays small and fast to build.

/// A usage error (exit code 2).
#[derive(Debug, PartialEq, Eq)]
pub struct Usage(pub String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Remaining arguments of a subcommand; flags are taken out by name, what is left is positional.
#[derive(Debug)]
pub struct Opts {
    rest: Vec<String>,
}

impl Opts {
    /// Wrap the arguments after the subcommand name.
    pub fn new(rest: Vec<String>) -> Self {
        Self { rest }
    }

    /// Remove a boolean flag; true when it was present.
    pub fn flag(&mut self, names: &[&str]) -> bool {
        let before = self.rest.len();
        let mut stop = false;
        self.rest.retain(|a| {
            if a == "--" {
                stop = true;
            }
            stop || !names.contains(&a.as_str())
        });
        self.rest.len() != before
    }

    /// Remove `--name value` / `--name=value` (the last occurrence wins).
    pub fn value(&mut self, names: &[&str]) -> Result<Option<String>, Usage> {
        Ok(self.values(names)?.pop())
    }

    /// Remove every occurrence of `--name value`.
    pub fn values(&mut self, names: &[&str]) -> Result<Vec<String>, Usage> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < self.rest.len() {
            if self.rest[i] == "--" {
                break;
            }
            let a = self.rest[i].clone();
            if names.contains(&a.as_str()) {
                if i + 1 >= self.rest.len() {
                    return Err(Usage(format!("{a} needs a value")));
                }
                out.push(self.rest.remove(i + 1));
                self.rest.remove(i);
                continue;
            }
            if let Some((k, v)) = a.split_once('=')
                && names.contains(&k)
            {
                out.push(v.to_owned());
                self.rest.remove(i);
                continue;
            }
            i += 1;
        }
        Ok(out)
    }

    /// A numeric option.
    pub fn number<T: std::str::FromStr>(&mut self, names: &[&str]) -> Result<Option<T>, Usage> {
        match self.value(names)? {
            None => Ok(None),
            Some(v) => v.parse().map(Some).map_err(|_| Usage(format!("{} expects a number, got `{v}`", names[0]))),
        }
    }

    /// Everything that is left, unvalidated.
    pub fn into_rest(self) -> Vec<String> {
        self.rest
    }

    /// The positional arguments. Anything still starting with `-` is an unknown option.
    pub fn finish(self) -> Result<Vec<String>, Usage> {
        let mut out = Vec::new();
        let mut literal = false;
        for a in self.rest {
            if !literal && a == "--" {
                literal = true;
            } else if !literal && a.starts_with('-') && a.len() > 1 {
                return Err(Usage(format!("unknown option `{a}`")));
            } else {
                out.push(a);
            }
        }
        Ok(out)
    }
}

/// `5MB`, `500k`, `1.5M`, `2g`, `1048576`, `0`, `unlimited` (decimal units, bytes per second).
pub fn parse_rate(s: &str) -> Result<u64, Usage> {
    let t = s.trim().to_ascii_lowercase();
    if t == "unlimited" || t == "none" || t == "off" {
        return Ok(0);
    }
    let t = t.trim_end_matches("/s").trim_end_matches('b');
    let (num, mult) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 1e3),
        Some('m') => (&t[..t.len() - 1], 1e6),
        Some('g') => (&t[..t.len() - 1], 1e9),
        Some('t') => (&t[..t.len() - 1], 1e12),
        _ => (t, 1.0),
    };
    let v: f64 = num.trim().parse().map_err(|_| Usage(format!("`{s}` is not a rate (try 500k, 5MB, unlimited)")))?;
    if !v.is_finite() || v < 0.0 || v * mult > 1.0995e12 {
        return Err(Usage(format!("`{s}` is out of range")));
    }
    Ok((v * mult) as u64)
}

/// `1.5GB`, `700MB`, `64k`, `4096` (decimal units, bytes). `unlimited` is not a size.
pub fn parse_size(s: &str) -> Result<u64, Usage> {
    let t = s.trim().to_ascii_lowercase();
    let t = t.trim_end_matches('b');
    let (num, mult) = match t.chars().last() {
        Some('k') => (&t[..t.len() - 1], 1e3),
        Some('m') => (&t[..t.len() - 1], 1e6),
        Some('g') => (&t[..t.len() - 1], 1e9),
        Some('t') => (&t[..t.len() - 1], 1e12),
        _ => (t, 1.0),
    };
    let v: f64 = num.trim().parse().map_err(|_| Usage(format!("`{s}` is not a size (try 700MB, 1.5GB)")))?;
    if !v.is_finite() || v < 0.0 || v * mult > 1.0995e13 {
        return Err(Usage(format!("`{s}` is out of range")));
    }
    Ok((v * mult) as u64)
}

/// `90`, `90s`, `15m`, `12h`, `7d`, `2w` in seconds.
pub fn parse_duration(s: &str) -> Result<u64, Usage> {
    let t = s.trim().to_ascii_lowercase();
    let (num, mult) = match t.chars().last() {
        Some('s') => (&t[..t.len() - 1], 1),
        Some('m') => (&t[..t.len() - 1], 60),
        Some('h') => (&t[..t.len() - 1], 3600),
        Some('d') => (&t[..t.len() - 1], 86_400),
        Some('w') => (&t[..t.len() - 1], 604_800),
        _ => (t.as_str(), 1),
    };
    num.trim()
        .parse::<u64>()
        .ok()
        .and_then(|v| v.checked_mul(mult))
        .ok_or_else(|| Usage(format!("`{s}` is not a duration (try 90m, 12h, 7d)")))
}

/// `0,2,5-7` into indexes (at most 100 000 entries).
pub fn parse_index_list(s: &str) -> Result<Vec<u32>, Usage> {
    let bad = || Usage(format!("`{s}` is not a list of file numbers (try 0,2,5-7)"));
    let mut out = Vec::new();
    for part in s.split(',').filter(|p| !p.trim().is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                let (a, b): (u32, u32) = (a.trim().parse().map_err(|_| bad())?, b.trim().parse().map_err(|_| bad())?);
                if b < a || b - a > 100_000 {
                    return Err(bad());
                }
                out.extend(a..=b);
            }
            None => out.push(part.trim().parse().map_err(|_| bad())?),
        }
        if out.len() > 100_000 {
            return Err(bad());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(v: &[&str]) -> Opts {
        Opts::new(v.iter().map(|s| (*s).to_owned()).collect())
    }

    #[test]
    fn flags_and_values() {
        let mut a = o(&["x", "--json", "--limit", "5", "y", "--tracker=tl"]);
        assert!(a.flag(&["--json"]));
        assert!(!a.flag(&["--json"]));
        assert_eq!(a.number::<u32>(&["--limit"]).unwrap(), Some(5));
        assert_eq!(a.value(&["--tracker"]).unwrap().as_deref(), Some("tl"));
        assert_eq!(a.finish().unwrap(), vec!["x", "y"]);
    }

    #[test]
    fn unknown_and_missing() {
        assert!(o(&["--nope"]).finish().is_err());
        assert!(o(&["--limit"]).value(&["--limit"]).is_err());
        assert!(o(&["--limit", "x"]).number::<u32>(&["--limit"]).is_err());
        assert_eq!(o(&["--", "--weird"]).finish().unwrap(), vec!["--weird"]);
        assert_eq!(o(&["-"]).finish().unwrap(), vec!["-"]);
    }

    #[test]
    fn repeated_values() {
        let mut a = o(&["--peer", "a", "--peer", "b"]);
        assert_eq!(a.values(&["--peer"]).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn rates() {
        assert_eq!(parse_rate("5MB").unwrap(), 5_000_000);
        assert_eq!(parse_rate("500k").unwrap(), 500_000);
        assert_eq!(parse_rate("1.5m/s").unwrap(), 1_500_000);
        assert_eq!(parse_rate("unlimited").unwrap(), 0);
        assert_eq!(parse_rate("1234").unwrap(), 1234);
        assert!(parse_rate("fast").is_err());
        assert!(parse_rate("-5").is_err());
        assert!(parse_rate("999999999t").is_err());
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("1.5GB").unwrap(), 1_500_000_000);
        assert_eq!(parse_size("700mb").unwrap(), 700_000_000);
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert!(parse_size("big").is_err());
    }

    #[test]
    fn durations_and_indexes() {
        assert_eq!(parse_duration("7d").unwrap(), 604_800);
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert!(parse_duration("x").is_err());
        assert_eq!(parse_index_list("0,2,5-7").unwrap(), vec![0, 2, 5, 6, 7]);
        assert!(parse_index_list("5-2").is_err());
        assert!(parse_index_list("a").is_err());
    }
}
