//! Filesystem helpers: atomic writes, timestamps, civil-date arithmetic.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Seconds since the Unix epoch (0 if the clock is before it).
pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// fsync a directory so a rename inside it survives power loss.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Write `data` to `path` atomically: temp file in the same directory, fsync,
/// rename over the target, fsync the directory. Readers see the old or the new
/// content, never a mix; a crash leaves at worst a stray `.tmp` file.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{name}.tmp.{}.{n}", std::process::id()));
    let result = (|| {
        let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        sync_dir(dir)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Inverse of [`days_from_civil`]: `(year, month 1-12, day 1-31)`.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Sortable UTC stamp `YYYYMMDDTHHMMSSZ` for file names.
pub fn utc_stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A path `dir/<prefix><stamp>[-NN]<suffix>` that does not exist and sorts after every existing
/// entry with the same prefix, so "newest = greatest name" holds even within one second.
pub(crate) fn unique_stamped(dir: &Path, prefix: &str, suffix: &str) -> PathBuf {
    let stamp = utc_stamp(unix_now());
    let mut best: Option<String> = None;
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(name) = e.file_name().into_string() else { continue };
            if let Some(rest) = name.strip_prefix(prefix) {
                let rest = rest.strip_suffix(suffix).unwrap_or(rest);
                if best.as_deref().is_none_or(|b| rest > b) {
                    best = Some(rest.to_string());
                }
            }
        }
    }
    let chosen = match best {
        Some(b) if b.as_str() >= stamp.as_str() => {
            // Same second (or a clock that went backwards): bump the counter after the greatest name.
            let (base, n) = match b.rsplit_once('-') {
                Some((base, n)) if n.len() == 2 && n.bytes().all(|c| c.is_ascii_digit()) => {
                    (base.to_string(), n.parse::<u32>().unwrap_or(0) + 1)
                }
                _ => (b.clone(), 1),
            };
            format!("{base}-{n:02}")
        }
        _ => stamp,
    };
    dir.join(format!("{prefix}{chosen}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_roundtrip_and_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(20_007), (2024, 10, 11));
        for z in (-800_000..800_000).step_by(997) {
            let (y, m, d) = civil_from_days(z);
            assert_eq!(days_from_civil(y, m, d), z);
        }
    }

    #[test]
    fn stamp_format() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(1_791_720_000), "20261011T120000Z");
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp() {
        let dir = crate::testutil::TempDir::new("fsutil");
        let p = dir.path().join("a.txt");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1);
    }
}
