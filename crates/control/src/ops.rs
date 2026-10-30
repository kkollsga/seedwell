//! Pure logic between the wire and the backend: id resolution, list filtering, counters,
//! input validation for `add`. No I/O, so it is easy to test exhaustively.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::proto::*;
use crate::redact::scrub_urls;
use crate::types::*;

/// A transfer picked by an id argument.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picked {
    /// Id.
    pub id: TransferId,
    /// Name, for answers and the activity log.
    pub name: String,
}

/// Shortest id prefix accepted.
pub const MIN_PREFIX: usize = 6;

/// Resolve user-supplied ids (full hex or unique prefixes) against the current rows. Order is
/// kept, duplicates are dropped. Unknown or ambiguous ids fail the whole call before anything
/// is changed.
pub fn resolve_ids(rows: &[TransferDto], wanted: &[String]) -> Result<Vec<Picked>, ApiError> {
    if wanted.is_empty() {
        return Err(ApiError::params("`ids` is empty"));
    }
    if wanted.len() > MAX_IDS {
        return Err(ApiError::params(format!("at most {MAX_IDS} ids per call")));
    }
    let mut out: Vec<Picked> = Vec::with_capacity(wanted.len());
    for w in wanted {
        let w = w.trim().to_ascii_lowercase();
        if w.len() < MIN_PREFIX || w.len() > 40 || !w.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ApiError::params(format!(
                "`{}` is not a transfer id (40 hex characters, or a unique prefix of at least {MIN_PREFIX})",
                clip(&w, 48)
            )));
        }
        let matches: Vec<&TransferDto> = rows.iter().filter(|r| r.id.starts_with(&w)).collect();
        match matches.as_slice() {
            [] => return Err(ApiError::not_found(format!("no transfer with id {}", clip(&w, 40)))),
            [one] => {
                let Some(id) = parse_id(&one.id) else {
                    return Err(ApiError::internal("backend returned a malformed id"));
                };
                if !out.iter().any(|p| p.id == id) {
                    out.push(Picked { id, name: clean_name(&one.name) });
                }
            }
            many => {
                let cands: Vec<_> = many.iter().take(8).map(|r| json!({"id": r.id, "name": r.name})).collect();
                return Err(ApiError::new(ErrorCode::Ambiguous, format!("id prefix `{w}` matches {} transfers; use more characters", many.len()))
                    .with_data(json!({ "candidates": cands })));
            }
        }
    }
    Ok(out)
}

/// Counts by state name, in [`STATE_ORDER`] (zero counts omitted).
pub fn counts_by_state(rows: &[TransferDto]) -> BTreeMap<String, u32> {
    let mut m = BTreeMap::new();
    for r in rows {
        *m.entry(r.state.clone()).or_insert(0) += 1;
    }
    m
}

/// `2 downloading, 1 queued, 40 sharing, 3 paused`.
pub fn state_summary(counts: &BTreeMap<String, u32>) -> String {
    let get = |k: &str| counts.get(k).copied().unwrap_or(0);
    let sharing = get("seeding") + get("low-power");
    let mut parts = Vec::new();
    for (n, label) in [
        (get("downloading"), "downloading"),
        (get("queued"), "queued"),
        (get("checking"), "checking"),
        (get("metadata"), "fetching metadata"),
        (sharing, "sharing"),
        (get("paused"), "paused"),
        (get("completed"), "completed"),
        (get("error") + get("missing-data"), "with errors"),
    ] {
        if n > 0 {
            parts.push(format!("{n} {label}"));
        }
    }
    if parts.is_empty() { "no transfers".into() } else { parts.join(", ") }
}

/// Whether a row passes a list request.
pub fn matches(t: &TransferDto, p: &ListParams) -> bool {
    let state_ok = match p.filter {
        ListFilter::All => true,
        ListFilter::Downloading => t.state == "downloading",
        ListFilter::Queued => t.state == "queued",
        ListFilter::Seeding => t.is_sharing(),
        ListFilter::LowPower => t.state == "low-power",
        ListFilter::Paused => t.state == "paused",
        ListFilter::Error => matches!(t.state.as_str(), "error" | "missing-data"),
        ListFilter::Finished | ListFilter::Recent => t.finished_at.is_some(),
        ListFilter::Checking => t.state == "checking",
        ListFilter::Metadata => t.state == "metadata",
        ListFilter::AwaitingChoice => t.state == "awaiting-choice",
        ListFilter::Pending => t.state == "pending",
    };
    if !state_ok {
        return false;
    }
    if let Some(k) = p.kind {
        let want = match k {
            KindFilter::Torrent => "torrent",
            KindFilter::Direct => "direct",
        };
        if t.kind != want {
            return false;
        }
    }
    if let Some(tr) = p.tracker.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let tr = tr.to_ascii_lowercase();
        let by_profile = t.tracker_profile.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(&tr));
        let by_host = t.tracker_host.to_ascii_lowercase().contains(&tr);
        let public = (tr == "public" && t.tracker_profile.is_none() && !t.private)
            || (tr == "other-private" && t.tracker_profile.is_none() && t.private);
        if !(by_profile || by_host || public) {
            return false;
        }
    }
    if let Some(c) = p.category.as_deref().map(str::trim).filter(|s| !s.is_empty())
        && !t.category.as_deref().is_some_and(|tc| tc.eq_ignore_ascii_case(c))
    {
        return false;
    }
    if let Some(x) = p.text.as_deref().map(str::trim).filter(|s| !s.is_empty())
        && !t.name.to_lowercase().contains(&x.to_lowercase())
    {
        return false;
    }
    true
}

/// Default and maximum page sizes of `list`.
pub const DEFAULT_PAGE: u32 = 50;
/// Largest page.
pub const MAX_PAGE: u32 = 500;

/// Filter, order and paginate.
pub fn list(rows: &[TransferDto], p: &ListParams) -> ListDto {
    let mut hits: Vec<&TransferDto> = rows.iter().filter(|t| matches(t, p)).collect();
    if p.filter == ListFilter::Recent {
        hits.sort_by_key(|t| std::cmp::Reverse(t.finished_at));
    }
    let total = hits.len() as u32;
    let offset = p.offset.unwrap_or(0).min(total);
    let limit = p.limit.unwrap_or(DEFAULT_PAGE).clamp(1, MAX_PAGE) as usize;
    ListDto {
        total,
        offset,
        transfers: hits.into_iter().skip(offset as usize).take(limit).cloned().collect(),
    }
}

/// Clean one outgoing row: no secret may leave through a URL or an error text.
pub fn sanitize_transfer(t: &mut TransferDto) {
    t.name = clean_name(&t.name);
    if let Some(u) = &t.url {
        t.url = Some(trs_state::url::redact_url(u));
    }
    if let Some(e) = &t.error {
        t.error = Some(clip(&scrub_urls(e), 500));
    }
    t.fill_human();
}

/// A name that is a magnet URI (before metadata arrives an engine may show the link) is cut down to
/// its identity, so a tracker passkey in `tr=` cannot reach an agent through a display name.
pub fn clean_name(name: &str) -> String {
    if name.get(..7).is_some_and(|h| h.eq_ignore_ascii_case("magnet:")) { crate::redact::redact_magnet(name) } else { name.to_owned() }
}

/// `rule_default` stands alone: nothing else may be set with it.
pub fn check_rule_default(p: &SetSeedGoalParams) -> Result<(), ApiError> {
    let alone = SetSeedGoalParams { ids: p.ids.clone(), rule_default: true, ..Default::default() };
    if *p != alone {
        return Err(ApiError::params("`rule_default` cannot be combined with a ratio, time, forever or any other goal option"));
    }
    Ok(())
}

/// Build the engine's seeding goal fields from the request.
pub fn build_goal(p: &SetSeedGoalParams) -> Result<SeedGoalDto, ApiError> {
    let bad = |m: &str| Err(ApiError::params(m.to_owned()));
    if p.forever && (p.ratio.is_some() || p.seed_secs.is_some() || p.stop_on_finish) {
        return bad("`forever` cannot be combined with a ratio, time or stop_on_finish");
    }
    if let Some(r) = p.ratio
        && (!r.is_finite() || !(0.0..=1000.0).contains(&r))
    {
        return bad("`ratio` must be between 0 and 1000");
    }
    if p.seed_secs.is_some_and(|s| s > 10 * 365 * 86_400) {
        return bad("`seed_secs` is longer than ten years");
    }
    let at_goal = match p.at_goal.unwrap_or(AtGoalParam::Stop) {
        AtGoalParam::Stop => "stop",
        AtGoalParam::Remove => "remove",
        AtGoalParam::RemoveAndDelete => "remove_and_delete",
    };
    Ok(if p.forever {
        SeedGoalDto { at_goal: "stop".into(), any: true, ..Default::default() }
    } else {
        SeedGoalDto {
            ratio: p.ratio,
            seed_secs: p.seed_secs,
            min_seed_secs: p.min_seed_secs.unwrap_or(0),
            any: p.any.unwrap_or(true),
            at_goal: at_goal.into(),
            stop_on_finish: p.stop_on_finish,
            idle_mb: 0,
            idle_hours: 0,
            text: String::new(),
        }
    })
}

/// Largest `.torrent` we read or decode.
pub const MAX_TORRENT_BYTES: usize = 16 << 20;

/// The source of an add, before files are read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// Magnet link.
    Magnet(String),
    /// Direct download URL.
    Url(String),
    /// `.torrent` path (absolute, `~` expanded).
    Path(PathBuf),
    /// `.torrent` bytes.
    Bytes(Vec<u8>),
}

fn count_sources(p: &AddParams) -> usize {
    [&p.source, &p.magnet, &p.torrent_path, &p.torrent_base64, &p.url].iter().filter(|o| o.is_some()).count()
}

/// Expand `~/x` and require an absolute path without `..`.
pub fn clean_path(raw: &str, what: &str) -> Result<PathBuf, ApiError> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 4096 || raw.chars().any(char::is_control) {
        return Err(ApiError::params(format!("`{what}` is empty, too long or has control characters")));
    }
    let p = if raw == "~" || raw.starts_with("~/") {
        let home = trs_platform::dirs::home().ok_or_else(|| ApiError::invalid("cannot expand `~`: HOME is not set"))?;
        home.join(raw.trim_start_matches('~').trim_start_matches('/'))
    } else {
        PathBuf::from(raw)
    };
    if !p.is_absolute() {
        return Err(ApiError::params(format!("`{what}` must be an absolute path or start with `~/`")));
    }
    if p.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(ApiError::params(format!("`{what}` must not contain `..`")));
    }
    Ok(p)
}

/// True for folders where a downloaded file could act as code or configuration: a disk's root, the
/// system's folders and the user's settings folder (`trs_platform::dirs::sensitive_dirs`), the
/// home folder itself, and hidden folders in it (`~/.ssh`, `~/.config`). The temporary folder
/// stays allowed (on Windows it is inside `AppData`).
pub fn is_sensitive_dir(p: &Path) -> bool {
    if p.parent().is_none() {
        return true;
    }
    if p.starts_with(std::env::temp_dir()) {
        return false;
    }
    if trs_platform::dirs::sensitive_dirs().iter().any(|d| p.starts_with(d)) {
        return true;
    }
    let Some(home) = trs_platform::dirs::home() else { return false };
    let Ok(rel) = p.strip_prefix(&home) else { return false };
    rel.as_os_str().is_empty() || rel.components().any(|c| matches!(c, std::path::Component::Normal(n) if n.to_string_lossy().starts_with('.')))
}

/// Decide which of the five source fields was used and validate its shape.
pub fn pick_source(p: &AddParams) -> Result<SourceKind, ApiError> {
    match count_sources(p) {
        0 => return Err(ApiError::params("give one of `source`, `magnet`, `torrent_path`, `torrent_base64`, `url`")),
        1 => {}
        _ => return Err(ApiError::params("give only one of `source`, `magnet`, `torrent_path`, `torrent_base64`, `url`")),
    }
    let magnet = |s: &str| -> Result<SourceKind, ApiError> {
        let s = s.trim();
        if s.len() > 16 * 1024 || !s.get(..7).is_some_and(|h| h.eq_ignore_ascii_case("magnet:")) {
            return Err(ApiError::params("not a magnet link (must start with `magnet:` and be under 16 KiB)"));
        }
        Ok(SourceKind::Magnet(s.to_owned()))
    };
    let url = |s: &str| -> Result<SourceKind, ApiError> {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        if s.len() > 8192 || s.chars().any(|c| c.is_control() || c.is_whitespace()) || !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(ApiError::params("`url` must be an http:// or https:// URL without spaces"));
        }
        Ok(SourceKind::Url(s.to_owned()))
    };
    if let Some(s) = &p.magnet {
        return magnet(s);
    }
    if let Some(s) = &p.url {
        return url(s);
    }
    if let Some(s) = &p.torrent_path {
        return Ok(SourceKind::Path(clean_path(s, "torrent_path")?));
    }
    if let Some(s) = &p.torrent_base64 {
        let bytes = crate::b64::decode(s, MAX_TORRENT_BYTES).ok_or_else(|| ApiError::params("`torrent_base64` is not valid base64 or is larger than 16 MiB"))?;
        if bytes.is_empty() {
            return Err(ApiError::params("`torrent_base64` is empty"));
        }
        return Ok(SourceKind::Bytes(bytes));
    }
    let s = p.source.as_deref().unwrap_or_default().trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("magnet:") {
        magnet(s)
    } else if lower.starts_with("http://") || lower.starts_with("https://") {
        url(s)
    } else if s.contains("://") {
        Err(ApiError::params("only magnet:, http:// and https:// sources and .torrent paths are supported"))
    } else {
        Ok(SourceKind::Path(clean_path(s, "source")?))
    }
}

/// Validate a category name; returns the canonical one.
pub fn parse_category(c: &str) -> Result<String, ApiError> {
    trs_classify::Category::parse(c).map(|c| c.as_str().to_owned()).ok_or_else(|| {
        ApiError::params(format!(
            "unknown category `{}` (one of: {})",
            clip(c, 32),
            trs_classify::Category::ALL.map(|c| c.as_str()).join(", ")
        ))
    })
}

/// Parse `peers`.
pub fn parse_peers(peers: &[String]) -> Result<Vec<SocketAddr>, ApiError> {
    if peers.len() > 50 {
        return Err(ApiError::params("at most 50 peers"));
    }
    peers
        .iter()
        .map(|p| p.parse::<SocketAddr>().map_err(|_| ApiError::params(format!("`{}` is not an ip:port", clip(p, 64)))))
        .collect()
}

/// Parse a 64-hex-digit SHA-256.
pub fn parse_sha256(s: &str) -> Result<[u8; 32], ApiError> {
    let b = s.trim().as_bytes();
    let bad = || ApiError::params("`sha256` must be 64 hex digits");
    if b.len() != 64 {
        return Err(bad());
    }
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        let hi = (b[2 * i] as char).to_digit(16).ok_or_else(bad)?;
        let lo = (b[2 * i + 1] as char).to_digit(16).ok_or_else(bad)?;
        *o = (hi * 16 + lo) as u8;
    }
    Ok(out)
}

/// Fill in `state` and `summary` of a [`ReachabilityDto`] from its facts. A peer that connected in
/// is proof and wins; otherwise the router's answer is the best sign there is.
pub fn reachability(mut r: ReachabilityDto) -> ReachabilityDto {
    let (state, summary) = if r.incoming > 0 {
        let peers = if r.incoming == 1 { "1 peer".to_owned() } else { format!("{} peers", r.incoming) };
        ("open", format!("Open: {peers} from the Internet connected in"))
    } else if !r.mapping {
        ("unknown", format!("Port mapping is off: forward port {} on your router, or switch port mapping on", r.port))
    } else if r.behind_cgnat {
        ("closed", "Closed: your provider shares one Internet address between customers (CGNAT); no router can open a port".to_owned())
    } else if r.router_unavailable {
        ("closed", format!("Closed: the router answered neither PCP, NAT-PMP nor UPnP; forward port {} on it by hand", r.port))
    } else if let Some(ext) = r.external_port {
        let via = match r.method.as_deref() {
            Some("pcp") => " with PCP",
            Some("nat-pmp") => " with NAT-PMP",
            Some("upnp") => " with UPnP",
            _ => "",
        };
        let port = if ext == r.port { String::new() } else { format!(" as port {ext}") };
        ("mapped", format!("Opened on the router{via}{port}; no peer from the Internet has connected in yet"))
    } else {
        ("checking", format!("Asking the router to open port {}", r.port))
    };
    r.state = state.to_owned();
    r.summary = summary;
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Unix-style absolute test path made absolute on this system: unchanged on Unix, a drive letter in front on Windows.
    fn abs(p: &str) -> String {
        if cfg!(windows) { format!("C:{p}") } else { p.to_string() }
    }

    #[test]
    fn roots_system_and_settings_folders_are_sensitive_but_the_temporary_folder_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_sensitive_dir(tmp.path()));
        assert!(!is_sensitive_dir(&tmp.path().join(".hidden")), "the temporary folder is allowed, even inside the home folder");
        let root = tmp.path().ancestors().last().unwrap();
        assert!(is_sensitive_dir(root), "{}", root.display());
        for d in trs_platform::dirs::sensitive_dirs() {
            assert!(is_sensitive_dir(&d.join("x")), "{}", d.display());
        }
        if let Some(home) = trs_platform::dirs::home() {
            assert!(is_sensitive_dir(&home));
            assert!(is_sensitive_dir(&home.join(".ssh")));
            assert!(!is_sensitive_dir(&home.join("Downloads")));
        }
    }

    fn row(id_byte: u8, name: &str, state: &str) -> TransferDto {
        TransferDto { id: id_hex(&[id_byte; 20]), kind: "torrent".into(), name: name.into(), state: state.into(), ..Default::default() }
    }

    #[test]
    fn rule_default_stands_alone() {
        let alone = SetSeedGoalParams { ids: vec!["a".into()], rule_default: true, ..Default::default() };
        assert!(check_rule_default(&alone).is_ok());
        for bad in [
            SetSeedGoalParams { ratio: Some(1.0), ..alone.clone() },
            SetSeedGoalParams { forever: true, ..alone.clone() },
            SetSeedGoalParams { at_goal: Some(AtGoalParam::Remove), ..alone.clone() },
            SetSeedGoalParams { stop_on_finish: true, ..alone.clone() },
        ] {
            assert!(check_rule_default(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn resolves_full_ids_and_prefixes() {
        let rows = vec![row(0xaa, "a", "seeding"), row(0xab, "b", "paused")];
        let full = rows[0].id.clone();
        assert_eq!(resolve_ids(&rows, &[full.clone(), full.to_uppercase()]).unwrap().len(), 1, "duplicates dropped");
        assert_eq!(resolve_ids(&rows, &["abab".repeat(2) + "ab"]).unwrap()[0].name, "b");
        let amb = resolve_ids(&rows, &["aaaaaa".into(), "ab".repeat(3)]).unwrap();
        assert_eq!(amb.len(), 2);
        let e = resolve_ids(&rows, &["cccccc".to_owned()]).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let rows2 = vec![row(0xaa, "a", "x"), {
            let mut r = row(0xaa, "a2", "x");
            r.id = format!("{}{}", &r.id[..39], "b");
            r
        }];
        let e = resolve_ids(&rows2, &["aaaaaa".into()]).unwrap_err();
        assert_eq!(e.code, ErrorCode::Ambiguous);
        assert!(e.data.unwrap().to_string().contains("candidates"));
        for bad in ["", "abc", "zzzzzzzz", "../../etc"] {
            assert_eq!(resolve_ids(&rows, &[bad.into()]).unwrap_err().code, ErrorCode::InvalidParams, "{bad}");
        }
        assert!(resolve_ids(&rows, &[]).is_err());
        assert!(resolve_ids(&rows, &vec!["aaaaaa".to_owned(); MAX_IDS + 1]).is_err());
    }

    #[test]
    fn filters() {
        let mut a = row(1, "Ubuntu ISO", "downloading");
        a.tracker_host = "tracker.ubuntu.com".into();
        a.category = Some("software".into());
        let mut b = row(2, "Song", "seeding");
        b.finished_at = Some(100);
        b.tracker_profile = Some("tl".into());
        b.private = true;
        let mut c = row(3, "Film", "low-power");
        c.finished_at = Some(200);
        let mut d = row(4, "Broken", "error");
        d.kind = "direct".into();
        let rows = vec![a, b, c, d];
        let l = |f: ListFilter| list(&rows, &ListParams { filter: f, ..Default::default() });
        assert_eq!(l(ListFilter::Downloading).total, 1);
        assert_eq!(l(ListFilter::Seeding).total, 2);
        assert_eq!(l(ListFilter::LowPower).total, 1);
        assert_eq!(l(ListFilter::Error).total, 1);
        let recent = l(ListFilter::Recent);
        assert_eq!(recent.transfers[0].name, "Film", "newest first");
        assert_eq!(list(&rows, &ListParams { tracker: Some("TL".into()), ..Default::default() }).total, 1);
        assert_eq!(list(&rows, &ListParams { tracker: Some("ubuntu".into()), ..Default::default() }).total, 1);
        assert_eq!(list(&rows, &ListParams { tracker: Some("other-private".into()), ..Default::default() }).total, 0);
        assert_eq!(list(&rows, &ListParams { tracker: Some("public".into()), ..Default::default() }).total, 3);
        assert_eq!(list(&rows, &ListParams { category: Some("Software".into()), ..Default::default() }).total, 1);
        assert_eq!(list(&rows, &ListParams { text: Some("sOnG".into()), ..Default::default() }).total, 1);
        assert_eq!(list(&rows, &ListParams { kind: Some(KindFilter::Direct), ..Default::default() }).total, 1);
        let page = list(&rows, &ListParams { limit: Some(2), offset: Some(3), ..Default::default() });
        assert_eq!((page.total, page.offset, page.transfers.len()), (4, 3, 1));
        let page = list(&rows, &ListParams { offset: Some(99), ..Default::default() });
        assert_eq!((page.offset, page.transfers.len()), (4, 0));
    }

    #[test]
    fn summaries() {
        let rows = vec![row(1, "a", "downloading"), row(2, "b", "seeding"), row(3, "c", "low-power"), row(4, "d", "error")];
        let c = counts_by_state(&rows);
        assert_eq!(state_summary(&c), "1 downloading, 2 sharing, 1 with errors");
        assert_eq!(state_summary(&BTreeMap::new()), "no transfers");
    }

    #[test]
    fn goals() {
        let g = build_goal(&SetSeedGoalParams { ids: vec![], ratio: Some(2.0), at_goal: Some(AtGoalParam::Remove), ..Default::default() }).unwrap();
        assert_eq!((g.ratio, g.at_goal.as_str(), g.any), (Some(2.0), "remove", true));
        assert!(build_goal(&SetSeedGoalParams { ratio: Some(f32::NAN), ..Default::default() }).is_err());
        assert!(build_goal(&SetSeedGoalParams { ratio: Some(-1.0), ..Default::default() }).is_err());
        assert!(build_goal(&SetSeedGoalParams { forever: true, ratio: Some(1.0), ..Default::default() }).is_err());
        assert_eq!(build_goal(&SetSeedGoalParams { forever: true, ..Default::default() }).unwrap().ratio, None);
    }

    #[test]
    fn add_sources() {
        let mk = |f: &dyn Fn(&mut AddParams)| {
            let mut p = AddParams::default();
            f(&mut p);
            pick_source(&p)
        };
        assert!(mk(&|_| {}).is_err());
        assert!(mk(&|p| {
            p.magnet = Some("magnet:?xt=urn:btih:ab".into());
            p.url = Some("https://x/y".into())
        })
        .is_err());
        assert!(matches!(mk(&|p| p.source = Some("magnet:?xt=urn:btih:ab".into())).unwrap(), SourceKind::Magnet(_)));
        assert!(matches!(mk(&|p| p.source = Some("https://dl.example.com/f.iso".into())).unwrap(), SourceKind::Url(_)));
        assert!(matches!(mk(&|p| p.source = Some(abs("/tmp/a.torrent"))).unwrap(), SourceKind::Path(_)));
        assert!(mk(&|p| p.source = Some("ftp://x/y".into())).is_err());
        assert!(mk(&|p| p.source = Some("relative/a.torrent".into())).is_err());
        assert!(mk(&|p| p.source = Some(abs("/tmp/../etc/passwd"))).is_err());
        assert!(mk(&|p| p.url = Some("https://a b/".into())).is_err());
        assert!(mk(&|p| p.url = Some("file:///etc/passwd".into())).is_err());
        assert!(mk(&|p| p.magnet = Some("http://x".into())).is_err());
        assert!(matches!(mk(&|p| p.torrent_base64 = Some("ZDM6Zm9vZQ==".into())).unwrap(), SourceKind::Bytes(_)));
        assert!(mk(&|p| p.torrent_base64 = Some("***".into())).is_err());
    }

    #[test]
    fn paths_and_values() {
        assert!(clean_path(&abs("/a/b"), "x").is_ok());
        assert!(clean_path("a/b", "x").is_err());
        assert!(clean_path(&abs("/a/../b"), "x").is_err());
        assert!(clean_path(&abs("/a\nb"), "x").is_err());
        assert!(clean_path("", "x").is_err());
        assert_eq!(parse_category("TV").unwrap(), "tv");
        assert!(parse_category("games").is_err());
        assert_eq!(parse_peers(&["127.0.0.1:6881".into()]).unwrap().len(), 1);
        assert!(parse_peers(&["nope".into()]).is_err());
        assert!(parse_sha256(&"ab".repeat(32)).is_ok());
        assert!(parse_sha256("ab").is_err());
        assert!(parse_sha256(&"zz".repeat(32)).is_err());
    }

    #[test]
    fn sanitize_hides_secrets() {
        let mut t = row(1, "x", "error");
        t.url = Some("https://dl.example.com/f.iso?token=SECRET".into());
        t.error = Some("tracker https://t.org/a/0123456789abcdef0123456789abcdef/announce?passkey=SECRET said no".into());
        sanitize_transfer(&mut t);
        let j = serde_json::to_string(&t).unwrap();
        assert!(!j.contains("SECRET") && !j.contains("0123456789abcdef"), "{j}");
    }

    #[test]
    fn reachability_prefers_proof_then_the_router() {
        let base = ReachabilityDto { port: 55252, mapping: true, ..Default::default() };
        let st = |r: ReachabilityDto| reachability(r).state;
        assert_eq!(st(base.clone()), "checking");
        assert_eq!(st(ReachabilityDto { method: Some("nat-pmp".into()), external_port: Some(55252), ..base.clone() }), "mapped");
        assert_eq!(st(ReachabilityDto { behind_cgnat: true, external_port: Some(55252), ..base.clone() }), "closed");
        assert_eq!(st(ReachabilityDto { router_unavailable: true, ..base.clone() }), "closed");
        assert_eq!(st(ReachabilityDto { mapping: false, ..base.clone() }), "unknown");
        // A peer that connected in is proof, whatever the router said (a port forwarded by hand).
        let open = reachability(ReachabilityDto { mapping: false, incoming: 3, last_incoming_at: Some(1), ..base.clone() });
        assert_eq!((open.state.as_str(), open.summary.as_str()), ("open", "Open: 3 peers from the Internet connected in"));
        let m = reachability(ReachabilityDto { method: Some("upnp".into()), external_port: Some(61000), ..base });
        assert_eq!(m.summary, "Opened on the router with UPnP as port 61000; no peer from the Internet has connected in yet");
    }
}
