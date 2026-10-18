//! `trackers.toml`: tracker profiles (SPEC §7E.1).
//!
//! A profile is a named group of announce URL patterns plus a compliance flag and an optional
//! link to a rule. **Passkeys are never stored**: when a URL is added the query string is
//! dropped and passkey-looking path segments become `*`.
//!
//! # Matching precedence
//!
//! Every announce URL of a torrent is compared with every pattern of every profile. A pair has
//! a *tier*:
//!
//! 1. **Exact**: scheme, host, port (default ports ignored) and the whole path agree.
//! 2. **Host + path**: host and path agree, scheme and port are ignored.
//! 3. **Host**: only the host agrees (a bare host pattern, or a pattern whose path differs).
//!
//! The best pair wins, ordered by: lower tier; exact host over `*.suffix` glob; more literal
//! path segments; earlier URL in the torrent; earlier profile in the file; smaller id. The
//! result therefore never depends on hash order or on iteration accidents. No match is the
//! *Public* bucket (or *Other private* for private torrents). Profiles are the user's *favorites*;
//! every other torrent is grouped at run time by the registrable domain of its primary announce
//! host ([`group_of_host`]), nothing of which is stored.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::check_range;
use crate::doc::{Document, Parsed, parse_toml, render_table};
use crate::error::{DocError, FieldError, Warning};
use crate::ids::valid_id;
use crate::url::{AnnounceUrl, looks_secret, split, strip_default_port};

/// Highest `version` this build reads.
pub const TRACKERS_VERSION: u32 = 1;
/// Ids that stats and rules use for non-profile buckets.
pub const RESERVED_IDS: [&str; 3] = ["public", "other-private", "all"];
const MAX_PROFILES: usize = 256;
const MAX_URLS: usize = 32;
const HEADER: &str = "# Tracker profiles. Announce URLs here never contain passkeys: only the host and\n\
# path pattern are kept (`*` matches one path segment).\n\n";

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
enum HostPat {
    Exact(String),
    /// `*.example.org`: stored with the leading dot (`.example.org`).
    Suffix(String),
}

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
enum Seg {
    Lit(String),
    Any,
}

/// One announce URL pattern of a profile. Contains no query and no passkey, so `Debug` and
/// `Display` are safe to log.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct UrlPattern {
    scheme: Option<String>,
    host: HostPat,
    port: Option<u16>,
    path: Option<Vec<Seg>>,
    text: String,
}

/// How well a pattern matched a URL (lower is better).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MatchTier {
    /// Scheme, host, port and path equal.
    Exact = 1,
    /// Host and path equal.
    HostPath = 2,
    /// Host only.
    Host = 3,
}

impl UrlPattern {
    /// Parse a user- or file-supplied pattern: a full URL, `host/path`, a bare host, or
    /// `*.suffix`. The query is dropped; secret-looking path segments become `*`.
    pub fn parse(raw: &str) -> Result<UrlPattern, &'static str> {
        let p = split(raw, true)?;
        let host = match p.host.strip_prefix("*.") {
            Some(rest) => HostPat::Suffix(format!(".{rest}")),
            None => HostPat::Exact(p.host.clone()),
        };
        let scheme = p.scheme;
        let port = strip_default_port(scheme.as_deref(), p.port);
        let path = (p.has_path || !p.segments.is_empty()).then(|| {
            p.segments
                .iter()
                .map(|s| if *s == "*" || looks_secret(s) { Seg::Any } else { Seg::Lit((*s).to_string()) })
                .collect::<Vec<_>>()
        });
        let mut text = String::new();
        if let Some(s) = &scheme {
            text.push_str(s);
            text.push_str("://");
        }
        match &host {
            HostPat::Exact(h) if h.contains(':') => text.push_str(&format!("[{h}]")),
            HostPat::Exact(h) => text.push_str(h),
            HostPat::Suffix(s) => {
                text.push('*');
                text.push_str(s);
            }
        }
        if let Some(port) = port {
            text.push_str(&format!(":{port}"));
        }
        if let Some(segs) = &path {
            if segs.is_empty() {
                text.push('/');
            }
            for s in segs {
                text.push('/');
                match s {
                    Seg::Lit(l) => text.push_str(l),
                    Seg::Any => text.push('*'),
                }
            }
        }
        Ok(UrlPattern { scheme, host, port, path, text })
    }

    /// The registrable domain of the pattern's host (`tracker.example.org` and `*.example.org` -> `example.org`);
    /// the key its automatic tracker's statistics are recorded under.
    pub fn domain(&self) -> Option<String> {
        let h = match &self.host {
            HostPat::Exact(h) => h.as_str(),
            HostPat::Suffix(s) => s.trim_start_matches('.'),
        };
        (!h.is_empty()).then(|| registrable_domain(h))
    }

    /// Canonical text form (what `trackers.toml` stores).
    pub fn as_str(&self) -> &str {
        &self.text
    }

    fn host_matches(&self, host: &str) -> bool {
        match &self.host {
            HostPat::Exact(h) => h == host,
            HostPat::Suffix(s) => host.len() > s.len() && host.ends_with(s.as_str()),
        }
    }

    fn path_matches(&self, url: &AnnounceUrl) -> bool {
        match &self.path {
            None => false,
            Some(p) => {
                p.len() == url.path.len()
                    && p.iter().zip(&url.path).all(|(s, u)| match s {
                        Seg::Any => true,
                        Seg::Lit(l) => l == u,
                    })
            }
        }
    }

    /// Tier and tie-break score (`(exact host, literal segments)`) of this pattern against `url`.
    pub(crate) fn rank(&self, url: &AnnounceUrl) -> Option<(MatchTier, bool, usize)> {
        if !self.host_matches(&url.host) {
            return None;
        }
        let exact_host = matches!(self.host, HostPat::Exact(_));
        let path_ok = self.path_matches(url);
        // Literal segments only make a pattern more specific when the path actually matched.
        let lits = match (&self.path, path_ok) {
            (Some(p), true) => p.iter().filter(|s| matches!(s, Seg::Lit(_))).count(),
            _ => 0,
        };
        let tier = if path_ok {
            if exact_host
                && self.scheme.as_deref() == Some(url.scheme.as_str())
                && self.port == url.port
            {
                MatchTier::Exact
            } else {
                MatchTier::HostPath
            }
        } else {
            MatchTier::Host
        };
        Some((tier, exact_host, lits))
    }
}

impl fmt::Display for UrlPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}
impl fmt::Debug for UrlPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "UrlPattern({})", self.text)
    }
}

impl Serialize for UrlPattern {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text)
    }
}
impl<'de> Deserialize<'de> for UrlPattern {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Never put the offending text in the message: it may contain a passkey.
        UrlPattern::parse(&s).map_err(|e| serde::de::Error::custom(format!("invalid announce URL pattern: {e}")))
    }
}

/// One tracker profile.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Stable id (`a-z0-9-_`); used by rules, stats and torrent records.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Announce URL patterns that belong to this tracker.
    pub urls: Vec<UrlPattern>,
    /// Rule applied to this tracker's torrents (id in `rules.toml`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule: Option<String>,
    /// Sidebar colour, `#RRGGBB`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Sidebar icon (SF Symbol name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// One shared download cap for all torrents of this tracker together, bytes/s. Absent (or 0)
    /// = unlimited (SPEC §7E.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down_limit: Option<u64>,
    /// One shared upload cap for all torrents of this tracker together, bytes/s. Absent (or 0)
    /// = unlimited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub up_limit: Option<u64>,
}

impl Profile {
    /// The shared download cap, with `0` read as unlimited.
    pub fn down_cap(&self) -> Option<u64> {
        self.down_limit.filter(|v| *v != 0)
    }

    /// The shared upload cap, with `0` read as unlimited.
    pub fn up_cap(&self) -> Option<u64> {
        self.up_limit.filter(|v| *v != 0)
    }

    /// A profile named after the host of `announce_url`, ready for "Create tracker profile…".
    pub fn suggest_from_announce(announce_url: &str) -> Result<Profile, &'static str> {
        let pattern = UrlPattern::parse(announce_url)?;
        let host = match &pattern.host {
            HostPat::Exact(h) => h.clone(),
            HostPat::Suffix(s) => s.trim_start_matches('.').to_string(),
        };
        // `tracker.example.org` -> `example`; keep it readable, uniqueness is the caller's job.
        let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
        let core = if labels.len() >= 2 { labels[labels.len() - 2] } else { labels.first().copied().unwrap_or("tracker") };
        let mut id: String = core
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
            .collect();
        id.truncate(48);
        if !valid_id(&id) {
            id = "tracker".into();
        }
        Ok(Profile { name: host, id, urls: vec![pattern], ..Profile::default() })
    }
}

/// A host as a pattern text: IPv6 literals need brackets.
fn host_text(h: &str) -> String {
    if h.contains(':') { format!("[{h}]") } else { h.to_owned() }
}

/// Second-level labels that sit under a two-letter country code (`co.uk`, `com.au`, `ac.nz`).
fn is_sld(label: &str) -> bool {
    matches!(label, "co" | "com" | "org" | "net" | "ac" | "gov" | "edu")
}

/// The registrable domain of a tracker host: `tracker.example.org` -> `example.org`,
/// `a.b.example.co.uk` -> `example.co.uk`. An IP address, an IPv6 literal or a single-label host
/// (`localhost`) is its own domain. Lower case.
pub fn registrable_domain(host: &str) -> String {
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.parse::<std::net::IpAddr>().is_ok() || host.contains(':') {
        return host;
    }
    let labels: Vec<&str> = host.split('.').filter(|l| !l.is_empty()).collect();
    let n = labels.len();
    if n <= 2 {
        return labels.join(".");
    }
    let sld = is_sld(labels[n - 2]) && labels[n - 1].len() == 2;
    let keep = if sld { 3 } else { 2 };
    labels[n - keep..].join(".")
}

/// The label that names the tracker: the registrable domain without its TLD
/// (`tracker.example.org` -> `example`, `tracker.example.co.uk` -> `example`).
/// An IP address or a single-label host names itself.
fn core_label(host: &str) -> String {
    let dom = registrable_domain(host);
    if dom.parse::<std::net::IpAddr>().is_ok() || dom.contains(':') {
        return dom;
    }
    let labels: Vec<&str> = dom.split('.').filter(|l| !l.is_empty()).collect();
    match labels.len() {
        0 => "tracker".into(),
        1 => labels[0].to_owned(),
        n if n >= 3 => labels[n - 3].to_owned(),
        n => labels[n - 2].to_owned(),
    }
}

/// An automatically identified tracker (SPEC 7E.1): never stored, derived from the primary
/// announce host of a torrent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrackerGroup {
    /// The registrable domain (`example.org`, `127.0.0.1`, `localhost`).
    pub key: String,
    /// Title-cased name (`Example`).
    pub name: String,
}

/// The auto tracker a primary announce host belongs to; `None` for an empty host.
pub fn group_of_host(host: &str) -> Option<TrackerGroup> {
    let key = registrable_domain(host);
    (!key.is_empty()).then(|| TrackerGroup { name: auto_name(host), key })
}

/// The primary announce host of a torrent: the host of the first parseable URL (first tier,
/// first URL; the order the torrent lists them in).
pub fn primary_host<'u>(urls: impl IntoIterator<Item = &'u str>) -> Option<String> {
    urls.into_iter().take(256).find_map(|u| AnnounceUrl::parse(u).ok().map(|u| u.host))
}

/// `example` -> `Example`.
fn auto_name(host: &str) -> String {
    let core = core_label(host);
    let mut cs = core.chars();
    let name: String = match cs.next() {
        Some(c) => c.to_uppercase().chain(cs).collect(),
        None => "Tracker".into(),
    };
    name.chars().take(64).collect()
}

fn auto_id(host: &str) -> String {
    let mut id: String = core_label(host)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    id.truncate(48);
    if !valid_id(&id) {
        id = "tracker".into();
    }
    id
}

/// Result of matching a torrent against the profiles.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProfileMatch<'a> {
    /// The winning profile.
    pub profile: &'a Profile,
    /// How it matched.
    pub tier: MatchTier,
    /// Index (in the torrent's URL list) of the URL that matched.
    pub url_index: usize,
}

/// Statistics bucket a torrent belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bucket<'a> {
    /// A tracker profile id.
    Profile(&'a str),
    /// No profile, not private.
    Public,
    /// No profile, private.
    OtherPrivate,
}

impl Bucket<'_> {
    /// Key used in `stats_daily`: the profile id, or `@public` / `@other-private`.
    pub fn key(&self) -> &str {
        match self {
            Bucket::Profile(id) => id,
            Bucket::Public => "@public",
            Bucket::OtherPrivate => "@other-private",
        }
    }
}

/// The whole of `trackers.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackerProfiles {
    /// File format version.
    pub version: u32,
    /// Profiles in priority order (earlier wins ties).
    #[serde(rename = "tracker", skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<Profile>,
    /// Keys this build does not know, preserved when saving.
    #[serde(skip)]
    pub unknown: toml::Table,
}

impl Default for TrackerProfiles {
    fn default() -> Self {
        TrackerProfiles { version: TRACKERS_VERSION, profiles: Vec::new(), unknown: toml::Table::new() }
    }
}

fn validate_profile(p: &Profile, path: &str, errs: &mut Vec<FieldError>) {
    if !valid_id(&p.id) {
        errs.push(FieldError::new(format!("{path}.id"), "must be 1-64 characters of a-z, 0-9, `-`, `_`"));
    } else if RESERVED_IDS.contains(&p.id.as_str()) {
        errs.push(FieldError::new(format!("{path}.id"), format!("`{}` is reserved", p.id)));
    }
    let n = p.name.chars().count();
    if n == 0 || n > 64 || p.name.chars().any(char::is_control) || p.name.trim().is_empty() {
        errs.push(FieldError::new(format!("{path}.name"), "must be 1-64 printable characters"));
    }
    if p.urls.is_empty() {
        errs.push(FieldError::new(format!("{path}.urls"), "needs at least one announce URL or host"));
    }
    check_range(errs, &format!("{path}.urls (count)"), p.urls.len() as u64, 0, MAX_URLS as u64);
    if let Some(r) = &p.rule
        && !valid_id(r)
    {
        errs.push(FieldError::new(format!("{path}.rule"), "not a valid rule id"));
    }
    if let Some(c) = &p.color {
        let b = c.as_bytes();
        if !(b.len() == 7 && b[0] == b'#' && b[1..].iter().all(u8::is_ascii_hexdigit)) {
            errs.push(FieldError::new(format!("{path}.color"), "expected #RRGGBB"));
        }
    }
    for (name, v) in [("down_limit", p.down_limit), ("up_limit", p.up_limit)] {
        if let Some(v) = v {
            check_range(errs, &format!("{path}.{name}"), v, 0, crate::config::MAX_RATE);
        }
    }
    if let Some(i) = &p.icon
        && (i.is_empty() || i.len() > 64 || i.chars().any(char::is_control))
    {
        errs.push(FieldError::new(format!("{path}.icon"), "must be 1-64 printable characters"));
    }
}

impl TrackerProfiles {
    /// Every problem in the set.
    pub fn validate(&self) -> Vec<FieldError> {
        let mut e = Vec::new();
        if self.version == 0 || self.version > TRACKERS_VERSION {
            e.push(FieldError::new("version", format!("must be 1..={TRACKERS_VERSION}")));
        }
        if self.profiles.len() > MAX_PROFILES {
            e.push(FieldError::new("tracker", format!("more than {MAX_PROFILES} profiles")));
        }
        for (i, p) in self.profiles.iter().enumerate() {
            validate_profile(p, &format!("tracker[{i}]"), &mut e);
            if self.profiles[..i].iter().any(|o| o.id == p.id) {
                e.push(FieldError::new(format!("tracker[{i}].id"), format!("duplicate id `{}`", p.id)));
            }
        }
        e
    }

    /// Patterns claimed by two profiles at once (resolved by file order, but worth telling).
    fn overlap_warnings(&self) -> Vec<Warning> {
        let mut w = Vec::new();
        for (i, p) in self.profiles.iter().enumerate() {
            for u in &p.urls {
                if let Some(other) = self.profiles[..i].iter().find(|o| o.urls.contains(u)) {
                    w.push(Warning {
                        path: format!("tracker[{i}].urls"),
                        message: format!("`{u}` is also claimed by `{}`; the earlier profile wins", other.id),
                    });
                }
            }
        }
        w
    }

    /// Profile by id.
    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    fn checked(&mut self, f: impl FnOnce(&mut Self)) -> Result<(), Vec<FieldError>> {
        let backup = self.profiles.clone();
        f(self);
        let errs = self.validate();
        if errs.is_empty() {
            Ok(())
        } else {
            self.profiles = backup;
            Err(errs)
        }
    }

    /// Append a profile.
    pub fn add(&mut self, profile: Profile) -> Result<(), Vec<FieldError>> {
        self.checked(|s| s.profiles.push(profile))
    }

    /// Replace the profile with the same id.
    pub fn update(&mut self, profile: Profile) -> Result<(), Vec<FieldError>> {
        if self.get(&profile.id).is_none() {
            return Err(vec![FieldError::new("id", format!("no profile `{}`", profile.id))]);
        }
        self.checked(|s| {
            if let Some(slot) = s.profiles.iter_mut().find(|p| p.id == profile.id) {
                *slot = profile;
            }
        })
    }

    /// Remove a profile. Statistics history is kept by the database; rules that referenced it
    /// show up in [`crate::cross_check`].
    pub fn remove(&mut self, id: &str) -> Option<Profile> {
        let i = self.profiles.iter().position(|p| p.id == id)?;
        Some(self.profiles.remove(i))
    }

    /// Add an announce URL (or host) to a profile; the passkey is stripped. Returns the stored pattern.
    pub fn add_url(&mut self, id: &str, raw: &str) -> Result<UrlPattern, Vec<FieldError>> {
        let pat = UrlPattern::parse(raw).map_err(|e| vec![FieldError::new("url", e)])?;
        if self.get(id).is_none() {
            return Err(vec![FieldError::new("id", format!("no profile `{id}`"))]);
        }
        let p2 = pat.clone();
        self.checked(|s| {
            if let Some(p) = s.profiles.iter_mut().find(|p| p.id == id)
                && !p.urls.contains(&p2)
            {
                p.urls.push(p2);
            }
        })?;
        Ok(pat)
    }

    /// Remove the `index`-th URL of a profile (a profile keeps at least one).
    pub fn remove_url(&mut self, id: &str, index: usize) -> Result<(), Vec<FieldError>> {
        self.checked(|s| {
            if let Some(p) = s.profiles.iter_mut().find(|p| p.id == id)
                && index < p.urls.len()
            {
                p.urls.remove(index);
            }
        })
    }

    /// Set a favorite's shared speed caps (bytes/s; `None` or 0 = unlimited).
    pub fn set_limits(&mut self, id: &str, down: Option<u64>, up: Option<u64>) -> Result<(), Vec<FieldError>> {
        let Some(mut p) = self.get(id).cloned() else {
            return Err(vec![FieldError::new("id", format!("no profile `{id}`"))]);
        };
        p.down_limit = down.filter(|v| *v != 0);
        p.up_limit = up.filter(|v| *v != 0);
        self.update(p)
    }

    /// Link (or unlink) a rule.
    pub fn set_rule(&mut self, id: &str, rule: Option<String>) -> Result<(), Vec<FieldError>> {
        self.checked(|s| {
            if let Some(p) = s.profiles.iter_mut().find(|p| p.id == id) {
                p.rule = rule;
            }
        })
    }

    /// Match a torrent's announce URLs (in torrent order). Unparseable URLs are skipped.
    /// See the module docs for the precedence.
    pub fn match_announce<'a, 'u>(&'a self, urls: impl IntoIterator<Item = &'u str>) -> Option<ProfileMatch<'a>> {
        type Key = (MatchTier, usize, usize, usize, usize);
        let mut best: Option<(Key, &'a Profile, usize)> = None;
        for (ui, raw) in urls.into_iter().enumerate().take(256) {
            let Ok(url) = AnnounceUrl::parse(raw) else { continue };
            for (pi, prof) in self.profiles.iter().enumerate() {
                let Some(rank) = prof.urls.iter().filter_map(|pat| pat.rank(&url)).min_by_key(|(t, exact, lits)| {
                    (*t, !*exact, std::cmp::Reverse(*lits))
                }) else {
                    continue;
                };
                // Smaller key = better; encode "exact host" and "more literals" as smaller values.
                let key: Key = (rank.0, usize::from(!rank.1), usize::MAX - rank.2, ui, pi);
                if best.as_ref().is_none_or(|(b, bp, _)| key < *b || (key == *b && prof.id < bp.id)) {
                    best = Some((key, prof, ui));
                }
            }
        }
        best.map(|(k, profile, url_index)| ProfileMatch { profile, tier: k.0, url_index })
    }

    /// "Save as Favorite" (SPEC 7E.1): a profile that holds the bare `hosts` of an auto tracker.
    /// Only hosts are stored, never a path or a passkey. A profile that already claims one of the
    /// hosts is returned instead (saving twice changes nothing). `name` empty = named after the
    /// first host. Returns the profile id.
    pub fn add_favorite(&mut self, name: &str, hosts: &[String]) -> Result<String, Vec<FieldError>> {
        let mut uniq: Vec<String> = Vec::new();
        for h in hosts {
            let h = h.trim().to_ascii_lowercase();
            if !h.is_empty() && !uniq.contains(&h) {
                uniq.push(h);
            }
        }
        if uniq.is_empty() {
            return Err(vec![FieldError::new("urls", "needs at least one announce URL or host")]);
        }
        if let Some(p) = self.profiles.iter().find(|p| uniq.iter().any(|h| p.urls.iter().any(|pat| pat.host_matches(h)))) {
            return Ok(p.id.clone());
        }
        uniq.truncate(MAX_URLS);
        let mut id = auto_id(&uniq[0]);
        let base = id.clone();
        let mut n = 2;
        while self.get(&id).is_some() || RESERVED_IDS.contains(&id.as_str()) {
            id = format!("{base}-{n}");
            n += 1;
        }
        let urls: Vec<UrlPattern> = uniq
            .iter()
            .map(|h| UrlPattern::parse(&host_text(h)).map_err(|e| vec![FieldError::new("urls", e)]))
            .collect::<Result<_, _>>()?;
        let name = if name.trim().is_empty() { auto_name(&uniq[0]) } else { name.trim().chars().take(64).collect() };
        self.add(Profile { id: id.clone(), name, urls, ..Profile::default() })?;
        Ok(id)
    }

    /// Statistics bucket for a torrent.
    pub fn bucket_for<'a, 'u>(&'a self, urls: impl IntoIterator<Item = &'u str>, private: bool) -> Bucket<'a> {
        match self.match_announce(urls) {
            Some(m) => Bucket::Profile(&m.profile.id),
            None if private => Bucket::OtherPrivate,
            None => Bucket::Public,
        }
    }
}

impl Document for TrackerProfiles {
    const FILE: &'static str = "trackers.toml";

    fn defaults() -> Self {
        TrackerProfiles::default()
    }

    fn parse_str(text: &str) -> Result<Parsed<Self>, DocError> {
        let (mut v, unknown, mut warnings) = parse_toml::<TrackerProfiles>(Self::FILE, text, TRACKERS_VERSION)?;
        v.unknown = unknown;
        // `approved` was a retired per-tracker flag: old files may still carry it; ignore it silently.
        warnings.retain(|w| !w.path.ends_with(".approved"));
        let errors = v.validate();
        if !errors.is_empty() {
            return Err(DocError::Invalid { file: Self::FILE, errors });
        }
        warnings.extend(v.overlap_warnings());
        Ok(Parsed { value: v, warnings })
    }

    fn render(&self) -> Result<String, DocError> {
        let errors = self.validate();
        if !errors.is_empty() {
            return Err(DocError::Invalid { file: Self::FILE, errors });
        }
        let table = toml::Table::try_from(self).map_err(|e| DocError::Parse {
            file: Self::FILE,
            line: None,
            column: None,
            message: e.to_string(),
        })?;
        render_table(Self::FILE, table, &self.unknown, HEADER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(id: &str, urls: &[&str]) -> Profile {
        Profile {
            id: id.into(),
            name: id.to_uppercase(),
            urls: urls.iter().map(|u| UrlPattern::parse(u).unwrap()).collect(),
            ..Profile::default()
        }
    }

    fn set(profiles: Vec<Profile>) -> TrackerProfiles {
        let mut t = TrackerProfiles::default();
        for p in profiles {
            t.add(p).unwrap();
        }
        t
    }

    fn m<'a>(t: &'a TrackerProfiles, urls: &[&str]) -> Option<(&'a str, MatchTier)> {
        t.match_announce(urls.iter().copied()).map(|m| (m.profile.id.as_str(), m.tier))
    }

    #[test]
    fn passkeys_are_never_stored() {
        let p = UrlPattern::parse("https://tracker.example.org/a/0123456789abcdef0123456789abcdef/announce?passkey=ZZZ").unwrap();
        assert_eq!(p.as_str(), "https://tracker.example.org/a/*/announce");
        let q = UrlPattern::parse("http://t.org/announce?passkey=ZZZ").unwrap();
        assert_eq!(format!("{q} {q:?}"), "http://t.org/announce UrlPattern(http://t.org/announce)");
    }

    #[test]
    fn pattern_forms_canonicalise() {
        for (raw, want) in [
            ("Tracker.Example.ORG", "tracker.example.org"),
            ("tracker.example.org/announce", "tracker.example.org/announce"),
            ("*.example.org", "*.example.org"),
            ("https://t.org:443/announce", "https://t.org/announce"),
            ("udp://t.org:6969", "udp://t.org:6969"),
            ("udp://t.org:6969/", "udp://t.org:6969/"),
        ] {
            assert_eq!(UrlPattern::parse(raw).unwrap().as_str(), want, "{raw}");
        }
        assert!(UrlPattern::parse("*").is_err());
        assert!(UrlPattern::parse("a*b.org").is_err());
    }

    #[test]
    fn tier_order_exact_then_hostpath_then_host() {
        let t = set(vec![
            profile("hostonly", &["tracker.example.org"]),
            profile("hostpath", &["tracker.example.org/announce"]),
            profile("exact", &["https://tracker.example.org/announce"]),
        ]);
        assert_eq!(m(&t, &["https://tracker.example.org/announce?pk=1"]), Some(("exact", MatchTier::Exact)));
        assert_eq!(m(&t, &["http://tracker.example.org:8080/announce"]), Some(("hostpath", MatchTier::HostPath)));
        assert_eq!(m(&t, &["http://tracker.example.org/other"]), Some(("hostonly", MatchTier::Host)));
        assert_eq!(m(&t, &["http://elsewhere.org/announce"]), None);
    }

    #[test]
    fn passkey_in_path_matches_wildcard_pattern() {
        let t = set(vec![profile("tl", &["https://t.example.org/a/*/announce"])]);
        assert_eq!(
            m(&t, &["https://t.example.org/a/0123456789abcdef0123456789abcdef/announce"]),
            Some(("tl", MatchTier::Exact))
        );
        assert_eq!(m(&t, &["https://t.example.org/b/x/announce"]), Some(("tl", MatchTier::Host)));
    }

    #[test]
    fn ambiguity_is_deterministic() {
        // Same tier: exact host beats glob, then more literal segments, then torrent URL order, then file order.
        let t = set(vec![
            profile("glob", &["*.example.org/announce"]),
            profile("exact-host", &["tracker.example.org/announce"]),
            profile("deeper", &["tracker.example.org/announce"]),
        ]);
        assert_eq!(m(&t, &["http://tracker.example.org/announce"]), Some(("exact-host", MatchTier::HostPath)));
        let t2 = set(vec![profile("b", &["x.org"]), profile("a", &["y.org"])]);
        // URL order decides when both are tier 3.
        assert_eq!(m(&t2, &["http://y.org/announce", "http://x.org/announce"]), Some(("a", MatchTier::Host)));
        assert_eq!(m(&t2, &["http://x.org/announce", "http://y.org/announce"]), Some(("b", MatchTier::Host)));
        // A better tier on a later URL beats a worse tier on an earlier one.
        let t3 = set(vec![profile("p", &["x.org"]), profile("q", &["http://y.org/announce"])]);
        assert_eq!(m(&t3, &["http://x.org/zzz", "http://y.org/announce"]), Some(("q", MatchTier::Exact)));
    }

    #[test]
    fn glob_does_not_match_the_bare_domain() {
        let t = set(vec![profile("g", &["*.example.org"])]);
        assert_eq!(m(&t, &["http://example.org/announce"]), None);
        assert!(m(&t, &["http://a.example.org/announce"]).is_some());
    }

    #[test]
    fn registrable_domains() {
        for (h, want) in [
            ("tracker.example.org", "example.org"),
            ("example.org", "example.org"),
            ("a.b.Example.CO.UK", "example.co.uk"),
            ("tracker.debian.org", "debian.org"),
            ("127.0.0.1", "127.0.0.1"),
            ("::1", "::1"),
            ("localhost", "localhost"),
            ("tracker.example-a.test", "example-a.test"),
        ] {
            assert_eq!(registrable_domain(h), want, "{h}");
        }
        let g = group_of_host("tracker.example.org").unwrap();
        assert_eq!((g.key.as_str(), g.name.as_str()), ("example.org", "Example"));
        assert_eq!(group_of_host("bttracker.debian.org").unwrap().name, "Debian");
        assert_eq!(group_of_host("t.example.co.uk").unwrap().name, "Example");
        assert_eq!(group_of_host("127.0.0.1").unwrap().name, "127.0.0.1");
        assert_eq!(group_of_host("localhost").unwrap().name, "Localhost");
        assert_eq!(group_of_host(""), None);
    }

    #[test]
    fn primary_host_is_the_first_parseable_url() {
        assert_eq!(primary_host(["garbage", "http://a.org/announce", "http://b.org/announce"]).as_deref(), Some("a.org"));
        assert_eq!(primary_host(Vec::<&str>::new()), None);
        // a public multi-tracker magnet stays in its primary's group
        let g = primary_host(["udp://open.stealth.si:80/announce", "udp://tracker.opentrackr.org:1337/announce"]).unwrap();
        assert_eq!(group_of_host(&g).unwrap().key, "stealth.si");
    }

    #[test]
    fn favorite_round_trip_has_no_passkey_and_takes_precedence() {
        let pk = "0123456789abcdef0123456789abcdef";
        let url = format!("https://tracker.example.org/a/{pk}/announce");
        let mut t = TrackerProfiles::default();
        let host = primary_host([url.as_str()]).unwrap();
        let id = t.add_favorite("", &[host.clone(), host.clone(), "tracker.example-mirror.org".into()]).unwrap();
        assert_eq!(id, "example");
        let p = t.get(&id).unwrap();
        assert_eq!(p.name, "Example");
        assert_eq!(p.urls.len(), 2, "deduplicated");
        let text = t.render().unwrap();
        assert!(!text.contains(pk) && !text.contains("/a/"), "{text}");
        // the favorite claims the torrent (passkey path in the URL does not matter)
        assert_eq!(m(&t, &[url.as_str()]).map(|(i, _)| i), Some("example"));
        // saving again changes nothing
        assert_eq!(t.add_favorite("Other name", &[host]).unwrap(), "example");
        assert_eq!(t.profiles.len(), 1);
        // a clashing id gets a suffix
        let id2 = t.add_favorite("", &["tracker.example.net".into()]).unwrap();
        assert_eq!(id2, "example-2");
        assert!(t.add_favorite("", &[]).is_err());
        // remove from favorites: the torrents go back to their auto group
        t.remove("example");
        assert_eq!(m(&t, &[url.as_str()]), None);
    }

    #[test]
    fn auto_names() {
        assert_eq!(auto_name("tracker.example.co.uk"), "Example");
        assert_eq!(auto_name("localhost"), "Localhost");
        assert_eq!(auto_name("127.0.0.1"), "127.0.0.1");
        assert_eq!(auto_id("127.0.0.1"), "127-0-0-1");
    }

    #[test]
    fn bucket_for_public_and_private() {
        let t = set(vec![profile("tl", &["t.example.org"])]);
        assert_eq!(t.bucket_for(["http://t.example.org/announce"], true), Bucket::Profile("tl"));
        assert_eq!(t.bucket_for(["http://open.org/announce"], false), Bucket::Public);
        assert_eq!(t.bucket_for(["udp://x.org:1/announce"], true), Bucket::OtherPrivate);
        assert_eq!(t.bucket_for(["garbage", ""], true), Bucket::OtherPrivate);
        assert_eq!(Bucket::Public.key(), "@public");
    }

    #[test]
    fn crud_validates_and_rolls_back() {
        let mut t = TrackerProfiles::default();
        assert!(t.add(profile("tl", &["t.example.org"])).is_ok());
        assert!(t.add(profile("tl", &["x.org"])).is_err(), "duplicate id");
        assert_eq!(t.profiles.len(), 1);
        assert!(t.add(profile("public", &["x.org"])).is_err(), "reserved id");
        assert!(t.add(Profile { urls: vec![], ..profile("empty", &["x.org"]) }).is_err());
        let added = t.add_url("tl", "https://backup.example.org/a/0123456789abcdef0123456789abcdef/announce?k=v").unwrap();
        assert_eq!(added.as_str(), "https://backup.example.org/a/*/announce");
        assert_eq!(t.get("tl").unwrap().urls.len(), 2);
        t.add_url("tl", "https://backup.example.org/a/*/announce").unwrap();
        assert_eq!(t.get("tl").unwrap().urls.len(), 2, "no duplicates");
        assert!(t.add_url("nope", "x.org").is_err());
        assert!(t.add_url("tl", "bad url").is_err());
        t.remove_url("tl", 1).unwrap();
        assert!(t.remove_url("tl", 0).is_err(), "last URL cannot go");
        t.set_rule("tl", Some("tl-rule".into())).unwrap();
        assert!(t.set_rule("tl", Some("Bad Id".into())).is_err());
        assert_eq!(t.get("tl").unwrap().rule.as_deref(), Some("tl-rule"));
        assert!(t.remove("tl").is_some() && t.remove("tl").is_none());
    }

    #[test]
    fn retired_approved_key_still_loads_and_is_never_written() {
        let text = "version = 1\n\n[[tracker]]\nid = \"tl\"\nname = \"TL\"\nurls = [\"example.org\"]\napproved = true\n";
        let p = TrackerProfiles::parse_str(text).unwrap();
        assert!(p.warnings.is_empty(), "{:?}", p.warnings);
        assert_eq!(p.value.profiles.len(), 1);
        assert!(!p.value.render().unwrap().contains("approved"));
    }

    #[test]
    fn suggest_from_announce() {
        let p = Profile::suggest_from_announce("https://tracker.example.org/a/0123456789abcdef0123456789abcdef/announce").unwrap();
        assert_eq!((p.id.as_str(), p.name.as_str()), ("example", "tracker.example.org"));
        let mut t = TrackerProfiles::default();
        assert!(t.add(p).is_ok());
    }

    #[test]
    fn file_roundtrip_with_comments_and_unknown_keys() {
        let text = r##"
# my trackers
[[tracker]]
id = "tl"
name = "Example Tracker"
urls = ["https://tracker.example.org/a/*/announce", "backup.example.org"]
rule = "tl"
color = "#ff8800"
nickname = "tl!"
"##;
        let p = TrackerProfiles::parse_str(text).unwrap();
        assert_eq!(p.warnings.len(), 1);
        assert_eq!(p.value.profiles[0].urls.len(), 2);
        let out = p.value.render().unwrap();
        assert_eq!(TrackerProfiles::parse_str(&out).unwrap().value, p.value);
    }

    #[test]
    fn speed_caps_round_trip_and_zero_is_unlimited() {
        let text = "[[tracker]]\nid = \"tl\"\nname = \"TL\"\nurls = [\"example.org\"]\ndown_limit = 5000000\nup_limit = 0\n";
        let p = TrackerProfiles::parse_str(text).unwrap().value;
        let tl = p.get("tl").unwrap();
        assert_eq!((tl.down_cap(), tl.up_cap()), (Some(5_000_000), None));
        let out = p.render().unwrap();
        assert!(out.contains("down_limit = 5000000"), "{out}");
        assert_eq!(TrackerProfiles::parse_str(&out).unwrap().value, p);
        let mut t = p;
        t.set_limits("tl", None, Some(250_000)).unwrap();
        let out = t.render().unwrap();
        assert!(!out.contains("down_limit") && out.contains("up_limit = 250000"), "{out}");
        assert!(t.set_limits("tl", Some(u64::MAX), None).is_err(), "out of range is refused");
        assert_eq!(t.get("tl").unwrap().up_limit, Some(250_000), "a refused edit rolls back");
        assert!(t.set_limits("zz", None, None).is_err());
    }

    #[test]
    fn file_errors_have_paths() {
        let Err(DocError::Invalid { errors, .. }) = TrackerProfiles::parse_str("[[tracker]]\nid = \"A b\"\nname = \"\"\nurls = []\ncolor = \"red\"\n") else { panic!() };
        let paths: Vec<_> = errors.iter().map(|e| e.path.as_str()).collect();
        for want in ["tracker[0].id", "tracker[0].name", "tracker[0].urls", "tracker[0].color"] {
            assert!(paths.contains(&want), "{want} in {paths:?}");
        }
        let err = TrackerProfiles::parse_str("[[tracker]]\nid = \"x\"\nname = \"x\"\nurls = [\"http://a b/?passkey=SECRET\"]\n").unwrap_err();
        assert!(!err.to_string().contains("SECRET"), "{err}");
    }

    #[test]
    fn duplicate_pattern_across_profiles_warns() {
        let text = "[[tracker]]\nid=\"a\"\nname=\"A\"\nurls=[\"x.org\"]\n[[tracker]]\nid=\"b\"\nname=\"B\"\nurls=[\"x.org\"]\n";
        let p = TrackerProfiles::parse_str(text).unwrap();
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].message.contains("earlier profile wins"));
    }
}
