//! Daily statistics buckets and the calendar maths behind the queries (SPEC §7E.2).
//!
//! # Day boundaries
//!
//! A [`Day`] is a *local civil day*: the number of days since 1970-01-01 in the user's time zone.
//! Callers convert a Unix timestamp with the zone's UTC offset **at that instant**
//! ([`Day::from_unix`]), so daylight-saving changes are handled by whoever knows the zone
//! (the engine, through the OS); this crate stays free of time-zone data and tests are
//! deterministic. Weeks start on **Monday**; "this month" is the calendar month of `today`.

use crate::codec::{Reader, Writer};
use crate::error::DecodeError;
use crate::fsutil::{civil_from_days, days_from_civil};

/// A local civil day (days since 1970-01-01, clamped at 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Day(pub u32);

impl Day {
    /// Day containing Unix time `secs` in a zone whose offset from UTC is `tz_offset_secs` then.
    pub fn from_unix(secs: i64, tz_offset_secs: i32) -> Day {
        let local = secs.saturating_add(i64::from(tz_offset_secs));
        Day(u32::try_from(local.div_euclid(86_400).max(0)).unwrap_or(u32::MAX))
    }

    /// Day for a calendar date (`None` before 1970 or for an invalid date).
    pub fn from_ymd(y: i64, m: u32, d: u32) -> Option<Day> {
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        let n = days_from_civil(y, m, d);
        if civil_from_days(n) != (y, m, d) {
            return None;
        }
        u32::try_from(n).ok().map(Day)
    }

    /// `(year, month, day)`.
    pub fn ymd(self) -> (i64, u32, u32) {
        civil_from_days(i64::from(self.0))
    }

    /// 0 = Monday .. 6 = Sunday. (1970-01-01 was a Thursday.)
    pub fn weekday(self) -> u8 {
        ((u64::from(self.0) + 3) % 7) as u8
    }

    /// The Monday of this day's week (clamped at the epoch).
    pub fn week_start(self) -> Day {
        Day(self.0.saturating_sub(u32::from(self.weekday())))
    }

    /// The first of this day's month.
    pub fn month_start(self) -> Day {
        let (y, m, _) = self.ymd();
        Day::from_ymd(y, m, 1).unwrap_or(Day(0))
    }

    /// `self - n` days, clamped at the epoch.
    pub fn minus(self, n: u32) -> Day {
        Day(self.0.saturating_sub(n))
    }

    /// `YYYY-MM-DD`.
    pub fn iso(self) -> String {
        let (y, m, d) = self.ymd();
        format!("{y:04}-{m:02}-{d:02}")
    }
}

/// Counters for one (bucket, day), or a sum of them. 8 x 8 bytes in memory.
///
/// All fields only ever grow. Payload bytes only, exactly what is reported to trackers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsBucket {
    /// Payload bytes downloaded.
    pub downloaded: u64,
    /// Payload bytes uploaded.
    pub uploaded: u64,
    /// Torrents added.
    pub torrents_added: u64,
    /// Torrents that finished downloading (not those added complete).
    pub torrents_completed: u64,
    /// Torrents removed. History is kept.
    pub torrents_removed: u64,
    /// Wanted files completed.
    pub files_completed: u64,
    /// Bytes of those files.
    pub files_bytes: u64,
    /// Seconds spent seeding, summed over torrents.
    pub seed_secs: u64,
}

const VERSION: u8 = 1;

impl StatsBucket {
    /// Saturating field-wise add.
    pub fn add(&mut self, o: &StatsBucket) {
        self.downloaded = self.downloaded.saturating_add(o.downloaded);
        self.uploaded = self.uploaded.saturating_add(o.uploaded);
        self.torrents_added = self.torrents_added.saturating_add(o.torrents_added);
        self.torrents_completed = self.torrents_completed.saturating_add(o.torrents_completed);
        self.torrents_removed = self.torrents_removed.saturating_add(o.torrents_removed);
        self.files_completed = self.files_completed.saturating_add(o.files_completed);
        self.files_bytes = self.files_bytes.saturating_add(o.files_bytes);
        self.seed_secs = self.seed_secs.saturating_add(o.seed_secs);
    }

    /// True when every field is zero.
    pub fn is_zero(&self) -> bool {
        *self == StatsBucket::default()
    }

    /// Upload divided by download; `None` when nothing was downloaded (ratio undefined).
    pub fn ratio(&self) -> Option<f64> {
        (self.downloaded > 0).then(|| self.uploaded as f64 / self.downloaded as f64)
    }

    /// Compact encoding: version byte plus eight varints (1 byte each while a field is zero;
    /// a busy day is about 20 bytes). The in-memory form is 64 bytes.
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.u8(VERSION);
        for v in self.fields() {
            w.varint(v);
        }
        w.0
    }

    pub(crate) fn decode(b: &[u8]) -> Result<StatsBucket, DecodeError> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(DecodeError("unknown stats bucket version"));
        }
        let mut v = [0u64; 8];
        for slot in &mut v {
            *slot = r.varint()?;
        }
        r.finish()?;
        Ok(StatsBucket {
            downloaded: v[0],
            uploaded: v[1],
            torrents_added: v[2],
            torrents_completed: v[3],
            torrents_removed: v[4],
            files_completed: v[5],
            files_bytes: v[6],
            seed_secs: v[7],
        })
    }

    fn fields(&self) -> [u64; 8] {
        [
            self.downloaded,
            self.uploaded,
            self.torrents_added,
            self.torrents_completed,
            self.torrents_removed,
            self.files_completed,
            self.files_bytes,
            self.seed_secs,
        ]
    }
}

/// Whose statistics to read or write.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StatsScope {
    /// One favorite's own early history (before statistics were keyed by domain); new traffic
    /// goes to [`StatsScope::Domain`].
    Profile(String),
    /// One automatically identified tracker: the registrable domain of its torrents' primary
    /// announce host (SPEC 7E.1, 7E.2). Every torrent counts here, favorite or not, so saving or
    /// removing a favorite never changes a tracker's history. Holds the storage key
    /// (`@d:<domain>`): build it with [`StatsScope::domain`].
    Domain(String),
    /// Torrents without a profile that are not private.
    Public,
    /// Torrents without a profile that are private.
    OtherPrivate,
    /// Everything (sum over all buckets, including profiles that were deleted).
    All,
}

impl StatsScope {
    /// The scope of an automatic tracker, by registrable domain (`example.org`).
    pub fn domain(domain: &str) -> StatsScope {
        StatsScope::Domain(format!("@d:{}", domain.to_ascii_lowercase()))
    }

    /// Storage key of a single bucket (`None` for [`StatsScope::All`], which is a sum).
    pub fn key(&self) -> Option<&str> {
        match self {
            StatsScope::Profile(id) | StatsScope::Domain(id) => Some(id),
            StatsScope::Public => Some("@public"),
            StatsScope::OtherPrivate => Some("@other-private"),
            StatsScope::All => None,
        }
    }
}

impl From<crate::trackers::Bucket<'_>> for StatsScope {
    fn from(b: crate::trackers::Bucket<'_>) -> Self {
        match b {
            crate::trackers::Bucket::Profile(id) => StatsScope::Profile(id.to_string()),
            crate::trackers::Bucket::Public => StatsScope::Public,
            crate::trackers::Bucket::OtherPrivate => StatsScope::OtherPrivate,
        }
    }
}

/// The scopes whose sum is one tracker's history, as the sidebar names it: an automatic tracker's
/// id is its domain; a favorite is its own early history plus the domain of each of its addresses
/// (several addresses on one domain count once); `public`, `other-private` and `all` as they are.
pub fn scopes_for(profiles: &crate::trackers::TrackerProfiles, id: &str) -> Vec<StatsScope> {
    match id {
        "public" => vec![StatsScope::Public],
        "other-private" => vec![StatsScope::OtherPrivate],
        "all" => vec![StatsScope::All],
        _ => match profiles.get(id) {
            Some(p) => {
                let mut v = vec![StatsScope::Profile(p.id.clone())];
                for d in p.urls.iter().filter_map(|u| u.domain()) {
                    let s = StatsScope::domain(&d);
                    if !v.contains(&s) {
                        v.push(s);
                    }
                }
                v
            }
            None => vec![StatsScope::domain(id)],
        },
    }
}

impl StatsSummary {
    /// Field-wise sum.
    pub fn add(&mut self, o: &StatsSummary) {
        self.today.add(&o.today);
        self.this_week.add(&o.this_week);
        self.this_month.add(&o.this_month);
        self.all_time.add(&o.all_time);
    }
}

/// "This week / this month / all time" for the tracker header (SPEC §7E.2).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StatsSummary {
    /// `today` only.
    pub today: StatsBucket,
    /// Monday through `today`.
    pub this_week: StatsBucket,
    /// The 1st through `today`.
    pub this_month: StatsBucket,
    /// Lifetime totals.
    pub all_time: StatsBucket,
}

/// Render a series as CSV (`date,downloaded,uploaded,...`), oldest first.
pub fn series_to_csv(series: &[(Day, StatsBucket)]) -> String {
    let mut s = String::from(
        "date,downloaded,uploaded,torrents_added,torrents_completed,torrents_removed,files_completed,files_bytes,seed_secs\n",
    );
    for (d, b) in series {
        s.push_str(&d.iso());
        for v in b.fields() {
            s.push(',');
            s.push_str(&v.to_string());
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_from_unix_respects_offset() {
        // 2026-10-11 23:30 UTC is already 2026-10-12 in UTC+2 and still the 11th in UTC-5.
        let t = 1_791_761_400;
        assert_eq!(Day::from_unix(t, 0).iso(), "2026-10-11");
        assert_eq!(Day::from_unix(t, 2 * 3600).iso(), "2026-10-12");
        assert_eq!(Day::from_unix(t, -5 * 3600).iso(), "2026-10-11");
        assert_eq!(Day::from_unix(-5, 0), Day(0), "pre-epoch clamps");
    }

    #[test]
    fn weekday_week_and_month_starts() {
        let d = Day::from_ymd(2026, 10, 11).unwrap(); // a Sunday
        assert_eq!(d.weekday(), 6);
        assert_eq!(d.week_start().iso(), "2026-10-05");
        assert_eq!(d.month_start().iso(), "2026-10-01");
        assert_eq!(Day(0).weekday(), 3, "1970-01-01 was a Thursday");
        assert_eq!(Day::from_ymd(2026, 2, 30), None);
        assert_eq!(Day::from_ymd(1969, 12, 31), None);
        assert_eq!(Day::from_ymd(2024, 2, 29).unwrap().iso(), "2024-02-29");
    }

    #[test]
    fn bucket_codec_roundtrip_and_size() {
        let b = StatsBucket { downloaded: 5_000_000_000, uploaded: 12_000_000_000, torrents_added: 3, torrents_completed: 2, torrents_removed: 1, files_completed: 40, files_bytes: 4_000_000_000, seed_secs: 86_400 };
        let enc = b.encode();
        assert!(enc.len() <= 40, "{}", enc.len());
        assert_eq!(StatsBucket::decode(&enc), Ok(b));
        assert_eq!(StatsBucket::default().encode().len(), 9);
        assert!(StatsBucket::decode(&enc[..enc.len() - 1]).is_err());
        let mut longer = enc.clone();
        longer.push(0);
        assert!(StatsBucket::decode(&longer).is_err());
    }

    #[test]
    fn ratio_is_none_without_downloads() {
        let mut b = StatsBucket { uploaded: 10, ..Default::default() };
        assert_eq!(b.ratio(), None);
        b.downloaded = 5;
        assert_eq!(b.ratio(), Some(2.0));
    }

    #[test]
    fn csv_has_header_and_rows() {
        let csv = series_to_csv(&[(Day(0), StatsBucket { downloaded: 1, ..Default::default() })]);
        assert_eq!(csv.lines().count(), 2);
        assert!(csv.contains("1970-01-01,1,0,"));
    }
}
